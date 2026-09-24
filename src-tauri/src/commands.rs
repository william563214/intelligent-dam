use crate::database::{Database, FileWithMetadata, Metadata, Tag};
use crate::ffmpeg;
use crate::scanner;
use crate::scraper::{scrape_metadata, to_database_metadata};
use crate::settings::{AppSettings, SettingsStore};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::State;

#[allow(non_upper_case_globals)]
const status_ready: &str = "ready";
#[allow(non_upper_case_globals)]
const status_scrape_failed: &str = "scrape_failed";

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
    db: State<'_, Mutex<Database>>,
) -> Result<ScanResult, String> {
    let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
    
    let imported_ids = scanner::scan_and_import(&db, &path)
        .map_err(|e| format!("Scan failed: {}", e))?;

    Ok(ScanResult {
        imported_count: imported_ids.len(),
        file_ids: imported_ids,
    })
}

#[tauri::command]
pub async fn scan_multiple_directories_command(
    paths: Vec<String>,
    db: State<'_, Mutex<Database>>,
) -> Result<ScanResult, String> {
    if paths.is_empty() {
        return Err("No directories provided".to_string());
    }

    let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
    
    let imported_ids = scanner::scan_multiple_directories(&db, paths)
        .map_err(|e| format!("Scan failed: {}", e))?;

    Ok(ScanResult {
        imported_count: imported_ids.len(),
        file_ids: imported_ids,
    })
}

#[tauri::command]
pub async fn get_all_files_command(
    db: State<'_, Mutex<Database>>,
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
pub fn save_settings_command(
    settings: AppSettings,
    settings_store: State<'_, Mutex<SettingsStore>>,
) -> Result<(), String> {
    let store = settings_store
        .lock()
        .map_err(|e| format!("Settings lock error: {}", e))?;
    store
        .save(&settings)
        .map_err(|e| format!("Failed to save settings: {}", e))
}

#[tauri::command]
pub async fn scrape_file_command(
    file_id: i64,
    avdc_path: Option<String>,
    stashdb_url: Option<String>,
    db: State<'_, Mutex<Database>>,
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

    // 獲取檔案資訊（在 await 之前完成）
    let (file_path, phash, existing_sprite) = {
        let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
        let file = db.get_file_by_id(file_id)
            .map_err(|e| format!("File not found: {}", e))?;
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
                    status_ready,
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
                db.update_file_status(file_id, status_scrape_failed)
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
    db: State<'_, Mutex<Database>>,
) -> Result<(), String> {
    let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
    
    db.update_metadata(&metadata)
        .map_err(|e| format!("Failed to update metadata: {}", e))
}

#[tauri::command]
pub async fn generate_preview_command(
    file_id: i64,
    db: State<'_, Mutex<Database>>,
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
    
    let sprite_path = output_dir.join(format!("sprite_{}.jpg", file_id));
    
    // 生成 Sprite Sheet（不需要持有鎖）
    let vtt_path_str = ffmpeg::generate_sprite_sheet(&file_path, &sprite_path, (10, 10))
        .await
        .map_err(|e| format!("Failed to generate sprite sheet: {}", e))?;
    
    // 更新資料庫中的 sprite_sheet_path（重新獲取鎖）
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
    }
    
    Ok(vtt_path_str)
}

#[tauri::command]
pub async fn generate_thumbnail_command(
    file_id: i64,
    timestamp: Option<f64>,
    db: State<'_, Mutex<Database>>,
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

