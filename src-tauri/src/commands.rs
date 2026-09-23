use crate::database::{Database, FileWithMetadata, Metadata, Tag};
use crate::ffmpeg;
use crate::file_status;
use crate::scanner;
use crate::scraper::{scrape_metadata, to_database_metadata};
use crate::settings::{AppSettings, SettingsStore};
use crate::watcher::{self, WatcherManager, WatcherStatus};
use crate::task_queue::{TaskQueue, TaskQueueStatus};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, State};

#[derive(Debug, Serialize, Deserialize)]
pub struct ScanResult {
    pub imported_count: usize,
    pub file_ids: Vec<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ScrapeResult {
    pub success: bool,
    pub metadata: Option<Metadata>,
    pub tags: Option<Vec<Tag>>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn scan_directory_command(
    path: String,
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<ScanResult, String> {
    // Directory walk off the async runtime; no DB lock held.
    let scanned = tokio::task::spawn_blocking(move || scanner::scan_directory(path))
        .await
        .map_err(|e| format!("Scan task join failed: {}", e))?
        .map_err(|e| format!("Scan failed: {}", e))?;

    // Short lock: insert rows (null phash ok; COALESCE keeps existing).
    let (imported, jobs) = {
        let db = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        scanner::import_files_pending_phash(&db, &scanned)
            .map_err(|e| format!("Import failed: {}", e))?
    };

    // ffmpeg phash outside the mutex (~25 frames per file).
    if !jobs.is_empty() {
        let results = tokio::task::spawn_blocking(move || scanner::compute_phash_jobs(jobs))
            .await
            .map_err(|e| format!("Phash task join failed: {}", e))?;

        let db = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        scanner::write_phashes(&db, &results)
            .map_err(|e| format!("Failed to write phash: {}", e))?;
    }

    // Drop ids removed by phash near-dedup; unique by path upsert
    let file_ids: Vec<i64> = {
        let db = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        let mut seen = std::collections::HashSet::new();
        imported
            .into_iter()
            .map(|e| e.file_id)
            .filter(|id| seen.insert(*id) && db.get_file_by_id(*id).is_ok())
            .collect()
    };

    Ok(ScanResult {
        imported_count: file_ids.len(),
        file_ids,
    })
}

#[tauri::command]
pub async fn scan_multiple_directories_command(
    paths: Vec<String>,
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<ScanResult, String> {
    if paths.is_empty() {
        return Err("No directories provided".to_string());
    }

    let scanned = tokio::task::spawn_blocking(move || scanner::scan_directories(&paths))
        .await
        .map_err(|e| format!("Scan task join failed: {}", e))?;

    let (imported, jobs) = {
        let db = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        scanner::import_files_pending_phash(&db, &scanned)
            .map_err(|e| format!("Import failed: {}", e))?
    };

    if !jobs.is_empty() {
        let results = tokio::task::spawn_blocking(move || scanner::compute_phash_jobs(jobs))
            .await
            .map_err(|e| format!("Phash task join failed: {}", e))?;

        let db = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        scanner::write_phashes(&db, &results)
            .map_err(|e| format!("Failed to write phash: {}", e))?;
    }

    let file_ids: Vec<i64> = {
        let db = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        let mut seen = std::collections::HashSet::new();
        imported
            .into_iter()
            .map(|e| e.file_id)
            .filter(|id| seen.insert(*id) && db.get_file_by_id(*id).is_ok())
            .collect()
    };

    Ok(ScanResult {
        imported_count: file_ids.len(),
        file_ids,
    })
}

#[tauri::command]
pub async fn get_all_files_command(
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<Vec<FileWithMetadata>, String> {
    let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
    
    db.get_files_with_metadata()
        .map_err(|e| format!("Failed to get files: {}", e))
}

#[tauri::command]
pub fn get_settings_command(
    settings_store: State<'_, Mutex<SettingsStore>>,
) -> Result<AppSettings, String> {
    let store = settings_store
        .lock()
        .map_err(|e| format!("Settings lock error: {}", e))?;
    store
        .load()
        .map_err(|e| format!("Failed to load settings: {}", e))
}

#[tauri::command]
pub async fn save_settings_command(
    settings: AppSettings,
    settings_store: State<'_, Mutex<SettingsStore>>,
    watcher: State<'_, WatcherManager>,
    queue: State<'_, Arc<TaskQueue>>,
    app: AppHandle,
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<(), String> {
    {
        let store = settings_store
            .lock()
            .map_err(|e| format!("Settings lock error: {}", e))?;
        store
            .save(&settings)
            .map_err(|e| format!("Failed to save settings: {}", e))?;
    }

    // Running watcher picks up new scan directories; join off the async runtime
    if watcher.is_running() {
        let paths = watcher::resolve_watch_paths(&settings.scan_directories);
        join_detached_worker(watcher.request_stop()).await?;
        if !paths.is_empty() {
            watcher
                .start(paths, app, Arc::clone(&db), Arc::clone(&queue))
                .map_err(|e| format!("Failed to reload watcher paths: {:#}", e))?;
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn start_watcher_command(
    app: AppHandle,
    db: State<'_, Arc<Mutex<Database>>>,
    settings_store: State<'_, Mutex<SettingsStore>>,
    watcher: State<'_, WatcherManager>,
    queue: State<'_, Arc<TaskQueue>>,
) -> Result<WatcherStatus, String> {
    let settings = {
        let store = settings_store
            .lock()
            .map_err(|e| format!("Settings lock error: {}", e))?;
        store
            .load()
            .map_err(|e| format!("Failed to load settings: {}", e))?
    };

    let paths = watcher::resolve_watch_paths(&settings.scan_directories);
    if paths.is_empty() {
        return Err(
            "No valid watch directories. Add existing scan directories in Settings first.".to_string(),
        );
    }

    // Stop prior session without blocking the async runtime on a long join
    join_detached_worker(watcher.request_stop()).await?;

    watcher
        .start(paths, app, Arc::clone(&db), Arc::clone(&queue))
        .map_err(|e| format!("Failed to start watcher: {:#}", e))
}

#[tauri::command]
pub async fn stop_watcher_command(
    watcher: State<'_, WatcherManager>,
) -> Result<WatcherStatus, String> {
    join_detached_worker(watcher.request_stop()).await?;
    Ok(watcher.status())
}

#[tauri::command]
pub fn get_watcher_status_command(watcher: State<'_, WatcherManager>) -> WatcherStatus {
    watcher.status()
}

#[tauri::command]
pub fn get_task_queue_status_command(
    queue: State<'_, Arc<TaskQueue>>,
) -> Result<TaskQueueStatus, String> {
    queue.status()
}

#[tauri::command]
pub fn retry_task_command(
    job_id: u64,
    queue: State<'_, Arc<TaskQueue>>,
) -> Result<u64, String> {
    queue.retry(job_id)
}

async fn join_detached_worker(
    result: anyhow::Result<Option<std::thread::JoinHandle<()>>>,
) -> Result<(), String> {
    let handle = result.map_err(|e| format!("Failed to stop watcher: {:#}", e))?;
    if let Some(handle) = handle {
        tokio::task::spawn_blocking(move || {
            WatcherManager::join_worker(handle, std::time::Duration::from_secs(3));
        })
        .await
        .map_err(|e| format!("Watcher stop task join failed: {}", e))?;
    }
    Ok(())
}

#[tauri::command]
pub async fn scrape_file_command(
    file_id: i64,
    avdc_path: Option<String>,
    stashdb_url: Option<String>,
    db: State<'_, Arc<Mutex<Database>>>,
    settings_store: State<'_, Mutex<SettingsStore>>,
) -> Result<ScrapeResult, String> {
    // 未傳入時改讀持久化設定 讓 Rust 端可取得完整設定
    let persisted = {
        let store = settings_store
            .lock()
            .map_err(|e| format!("Settings lock error: {}", e))?;
        store
            .load()
            .map_err(|e| format!("Failed to load settings: {}", e))?
    };
    let resolved_avdc = avdc_path
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let path = persisted.avdc_path.trim();
            if path.is_empty() {
                None
            } else {
                Some(path.to_string())
            }
        });
    let resolved_stashdb = stashdb_url
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let url = persisted.stashdb_url.trim();
            if url.is_empty() {
                None
            } else {
                Some(url.to_string())
            }
        });

    // 獲取檔案資訊並標為 processing（在 await 之前完成）
    let (file_path, phash, existing_sprite) = {
        let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
        let file = db.get_file_by_id(file_id)
            .map_err(|e| format!("File not found: {}", e))?;
        if !std::path::Path::new(&file.path).exists() {
            db.update_file_status(file_id, file_status::missing)
                .map_err(|e| format!("Failed to mark missing: {}", e))?;
            return Ok(ScrapeResult {
                success: false,
                metadata: None,
                tags: None,
                error: Some(format!("File missing on disk: {}", file.path)),
            });
        }
        db.update_file_status(file_id, file_status::processing)
            .map_err(|e| format!("Failed to mark processing: {}", e))?;
        let sprite = db
            .get_metadata(file_id)
            .map_err(|e| format!("Failed to read metadata: {}", e))?
            .and_then(|m| m.sprite_sheet_path);
        (PathBuf::from(&file.path), file.phash.clone(), sprite)
    };
    
    // 執行刮削（不需要持有鎖）
    match scrape_metadata(
        &file_path,
        phash,
        resolved_avdc.as_deref(),
        resolved_stashdb.as_deref(),
    ).await {
        Ok((scraped_meta, source_type)) => {
            let db_metadata = to_database_metadata(
                &scraped_meta,
                file_id,
                source_type,
                existing_sprite,
            );

            // metadata + tags + status 同一交易寫入
            let saved_tags = {
                let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
                db.persist_scrape_success(
                    &db_metadata,
                    &scraped_meta.tags,
                    &scraped_meta.actors,
                    file_status::ready,
                )
                .map_err(|e| format!("Failed to persist scrape result: {}", e))?
            };

            Ok(ScrapeResult {
                success: true,
                metadata: Some(db_metadata),
                tags: Some(saved_tags),
                error: None,
            })
        }
        Err(e) => {
            // AVDC 與 StashDB 皆失敗時標記 scrape_failed 錯誤不可吞掉
            let scrape_error = e.to_string();
            {
                let db = db.lock().map_err(|lock_err| {
                    format!(
                        "Scrape failed ({}); also failed to lock DB for scrape_failed: {}",
                        scrape_error, lock_err
                    )
                })?;
                db.update_file_status(file_id, file_status::scrape_failed)
                    .map_err(|status_err| {
                        format!(
                            "Scrape failed ({}); also failed to mark scrape_failed: {}",
                            scrape_error, status_err
                        )
                    })?;
            }

            Ok(ScrapeResult {
                success: false,
                metadata: None,
                tags: None,
                error: Some(scrape_error),
            })
        }
    }
}

#[tauri::command]
pub async fn update_file_metadata_command(
    file_id: i64,
    metadata: Metadata,
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<(), String> {
    let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
    
    db.update_metadata(&metadata)
        .map_err(|e| format!("Failed to update metadata: {}", e))
}

#[tauri::command]
pub async fn generate_preview_command(
    file_id: i64,
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<String, String> {
    // 獲取檔案資訊並標為 processing（在 await 之前完成）
    let (file_path, previous_status) = {
        let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
        let file = db.get_file_by_id(file_id)
            .map_err(|e| format!("File not found: {}", e))?;
        if !std::path::Path::new(&file.path).exists() {
            db.update_file_status(file_id, file_status::missing)
                .map_err(|e| format!("Failed to mark missing: {}", e))?;
            return Err(format!("File missing on disk: {}", file.path));
        }
        let previous = file.status.clone();
        db.update_file_status(file_id, file_status::processing)
            .map_err(|e| format!("Failed to mark processing: {}", e))?;
        (PathBuf::from(&file.path), previous)
    };
    
    // 生成輸出路徑
    let output_dir = std::env::temp_dir().join("intelligent_dam");
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("Failed to create output directory: {}", e))?;
    
    let sprite_path = output_dir.join(format!("sprite_{}.jpg", file_id));
    
    // 生成 Sprite Sheet（不需要持有鎖）
    let vtt_path_str = match ffmpeg::generate_sprite_sheet(&file_path, &sprite_path, (10, 10)).await
    {
        Ok(path) => path,
        Err(e) => {
            let db = db.lock().map_err(|lock_err| {
                format!(
                    "Preview failed ({}); also failed to lock DB: {}",
                    e, lock_err
                )
            })?;
            db.update_file_status(file_id, &previous_status)
                .map_err(|status_err| {
                    format!(
                        "Preview failed ({}); also failed to restore status: {}",
                        e, status_err
                    )
                })?;
            return Err(format!("Failed to generate sprite sheet: {}", e));
        }
    };
    
    // 更新資料庫中的 sprite_sheet_path 並結束 processing
    {
        let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
        if let Some(mut metadata) = db.get_metadata(file_id)
            .map_err(|e| format!("Failed to get metadata: {}", e))? {
            metadata.sprite_sheet_path = Some(vtt_path_str.clone());
            db.update_metadata(&metadata)
                .map_err(|e| format!("Failed to update metadata: {}", e))?;
        } else {
            // 創建新的 metadata
            let new_metadata = Metadata {
                file_id,
                title: None,
                studio: None,
                release_date: None,
                summary: None,
                cover_image: None,
                sprite_sheet_path: Some(vtt_path_str.clone()),
                source_type: None,
            };
            db.update_metadata(&new_metadata)
                .map_err(|e| format!("Failed to create metadata: {}", e))?;
        }
        let next = file_status::after_transient_work(&previous_status);
        db.update_file_status(file_id, next)
            .map_err(|e| format!("Failed to update status after preview: {}", e))?;
    }
    
    Ok(vtt_path_str)
}

#[tauri::command]
pub async fn generate_thumbnail_command(
    file_id: i64,
    timestamp: Option<f64>,
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<String, String> {
    // 獲取檔案資訊（在 await 之前完成）
    let file_path = {
        let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
        let file = db.get_file_by_id(file_id)
            .map_err(|e| format!("File not found: {}", e))?;
        PathBuf::from(&file.path)
    };
    
    // 生成輸出路徑
    let output_dir = std::env::temp_dir().join("intelligent_dam");
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("Failed to create output directory: {}", e))?;
    
    let thumbnail_path = output_dir.join(format!("thumb_{}.jpg", file_id));
    
    // 生成縮圖（不需要持有鎖）
    ffmpeg::generate_thumbnail(&file_path, &thumbnail_path, timestamp)
        .await
        .map_err(|e| format!("Failed to generate thumbnail: {}", e))?;
    
    Ok(thumbnail_path.to_str().unwrap().to_string())
}

#[tauri::command]
pub fn check_ffmpeg_available_command() -> bool {
    ffmpeg::check_ffmpeg_available()
}

