use crate::connectivity::{self, ConnectivityReport};
use crate::database::{Database, ErrorLogEntry, FileWithMetadata, Metadata, Tag};
use crate::ffmpeg;
use crate::file_status;
use crate::pipeline::PipelineOptions;
use crate::scanner;
use crate::scraper::{scrape_metadata, to_database_metadata};
use crate::settings::{AppSettings, SettingsStore};
use crate::task_queue::{TaskQueue, TaskQueueStatus};
use crate::watcher::{self, WatcherManager, WatcherStatus};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineOptionsInput {
    pub enable_scrape: Option<bool>,
    pub enable_move: Option<bool>,
    pub enable_preview: Option<bool>,
}

impl PipelineOptionsInput {
    pub fn into_options(self) -> PipelineOptions {
        PipelineOptions {
            enable_scrape: self.enable_scrape.unwrap_or(true),
            enable_move: self.enable_move.unwrap_or(true),
            enable_preview: self.enable_preview.unwrap_or(true),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueResult {
    pub job_ids: Vec<u64>,
    pub enqueued: usize,
    pub skipped: usize,
}

/// Cap one-click / batch enqueue size to avoid unbounded queue growth
#[allow(non_upper_case_globals)]
const max_batch_enqueue: usize = 500;

#[allow(non_upper_case_globals)]
const env_max_batch_enqueue: &str = "IDAM_MAX_BATCH_ENQUEUE";

fn batch_enqueue_limit() -> usize {
    match std::env::var(env_max_batch_enqueue) {
        Ok(v) => v.trim().parse::<usize>().unwrap_or(max_batch_enqueue).max(1),
        Err(_) => max_batch_enqueue,
    }
}

/// Resolve a user-picked path to an absolute canonical form when possible
pub fn canonicalize_user_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match std::path::Path::new(trimmed).canonicalize() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => trimmed.to_string(),
    }
}

fn normalize_scan_directories(dirs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for dir in dirs {
        let canon = canonicalize_user_path(dir);
        if canon.is_empty() {
            continue;
        }
        if out.iter().any(|existing: &String| {
            existing == &canon
                || std::path::Path::new(existing) == std::path::Path::new(&canon)
        }) {
            continue;
        }
        out.push(canon);
    }
    out
}

fn ensure_queue_ready(
    queue: &TaskQueue,
    app: Option<&AppHandle>,
    db: &Arc<Mutex<Database>>,
) -> Result<(), String> {
    match app {
        Some(app) => {
            let suppressions = queue.path_suppressions()?;
            queue.bind_and_start(app.clone(), Arc::clone(db), suppressions)
        }
        // Integration tests: bind DB only so enqueue accounting works without workers
        None => queue.bind_database(Arc::clone(db)),
    }
}

fn log_command_error(
    db: &Arc<Mutex<Database>>,
    category: &str,
    file_id: Option<i64>,
    path: Option<&str>,
    message: &str,
) {
    if let Ok(guard) = db.lock() {
        if let Err(e) = guard.insert_error_log(category, file_id, path, message, None) {
            eprintln!("commands: failed to write ErrorLog: {}", e);
        }
    }
}

/// Scan one directory and ingest into the shared DB (phash best-effort).
pub async fn scan_directory(
    path: String,
    db: &Arc<Mutex<Database>>,
    queue: &TaskQueue,
    settings: &AppSettings,
) -> Result<ScanResult, String> {
    // Directory walk off the async runtime; no DB lock held.
    let scanned = tokio::task::spawn_blocking(move || scanner::scan_directory(path))
        .await
        .map_err(|e| format!("Scan task join failed: {}", e))?
        .map_err(|e| format!("Scan failed: {}", e))?;

    finish_scan_import(db, queue, scanned, settings).await
}

/// Scan multiple directories and ingest. Empty `paths` is an error.
pub async fn scan_multiple_directories(
    paths: Vec<String>,
    db: &Arc<Mutex<Database>>,
    queue: &TaskQueue,
    settings: &AppSettings,
) -> Result<ScanResult, String> {
    if paths.is_empty() {
        return Err("No directories provided".to_string());
    }

    let scanned = tokio::task::spawn_blocking(move || scanner::scan_directories(&paths))
        .await
        .map_err(|e| format!("Scan task join failed: {}", e))?;

    finish_scan_import(db, queue, scanned, settings).await
}

async fn finish_scan_import(
    db: &Arc<Mutex<Database>>,
    queue: &TaskQueue,
    scanned: Vec<scanner::ScannedFile>,
    settings: &AppSettings,
) -> Result<ScanResult, String> {
    let suppressions = queue.path_suppressions().ok();
    let supp_ref = suppressions.as_deref();

    let max_distance = settings.effective_near_phash_max_distance();

    // Short lock: insert rows (null phash ok; COALESCE keeps existing).
    let (imported, jobs) = {
        let db = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        scanner::import_files_pending_phash(&db, &scanned, supp_ref, max_distance)
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
        scanner::write_phashes(&db, &results, supp_ref, max_distance)
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

/// List library files with metadata and tags.
pub fn get_all_files(db: &Arc<Mutex<Database>>) -> Result<Vec<FileWithMetadata>, String> {
    let db = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    db.get_files_with_metadata()
        .map_err(|e| format!("Failed to get files: {}", e))
}

/// Outcome of confirming or ignoring a near-phash suspicion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ResolveSuspectedOutcome {
    /// DB record removed; disk file left in place; permanent tombstone written.
    Confirmed { file_id: i64 },
    /// Suspicion cleared; file restored to ready (pipeline may be enqueued).
    Ignored { file: Box<FileWithMetadata> },
}

/// Confirm or ignore a near-phash suspected duplicate.
///
/// - `confirm`: write DuplicateTombstone, delete DB row (not disk) + PathSuppress
/// - `ignore`: mark ignored, set ready; caller should enqueue pipeline
pub fn resolve_suspected_duplicate(
    db: &Arc<Mutex<Database>>,
    queue: &TaskQueue,
    file_id: i64,
    action: &str,
) -> Result<ResolveSuspectedOutcome, String> {
    let action = action.trim().to_lowercase();
    if action != "confirm" && action != "ignore" {
        return Err(format!(
            "invalid action '{action}' (expected confirm or ignore)"
        ));
    }

    let suppressions = queue.path_suppressions().ok();

    if action == "confirm" {
        let orphan_path = {
            let guard = db
                .lock()
                .map_err(|e| format!("Database lock error: {}", e))?;
            let file = guard
                .get_file_by_id(file_id)
                .map_err(|e| format!("File not found: {}", e))?;
            if file.status != file_status::suspected_duplicate {
                return Err(format!(
                    "file {} is not suspected_duplicate (status={})",
                    file_id, file.status
                ));
            }
            let pending = guard
                .get_pending_suspected_duplicate(file_id)
                .map_err(|e| format!("Failed to read suspicion: {}", e))?
                .ok_or_else(|| format!("no pending suspicion for file {file_id}"))?;
            let keeper_phash = guard
                .get_file_by_id(pending.keeper_id)
                .ok()
                .and_then(|k| k.phash);
            guard
                .insert_duplicate_tombstone(
                    &file.path,
                    file.phash.as_deref(),
                    Some(pending.keeper_id),
                    keeper_phash.as_deref(),
                )
                .map_err(|e| format!("Failed to write duplicate tombstone: {}", e))?;
            // Persist confirmed resolution while the Files row still exists
            guard
                .resolve_suspected_duplicate(file_id, "confirmed")
                .map_err(|e| format!("Failed to mark confirmed: {}", e))?;
            // delete_file_record removes SuspectedDuplicates; tombstone remains
            guard
                .delete_file_record(file_id)
                .map_err(|e| format!("Failed to delete confirmed duplicate DB row: {}", e))?;
            file.path.clone()
        };
        if let Some(supp) = suppressions {
            supp.suppress(std::path::Path::new(&orphan_path));
        }
        return Ok(ResolveSuspectedOutcome::Confirmed { file_id });
    }

    // ignore
    {
        let guard = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        let file = guard
            .get_file_by_id(file_id)
            .map_err(|e| format!("File not found: {}", e))?;
        if file.status != file_status::suspected_duplicate {
            return Err(format!(
                "file {} is not suspected_duplicate (status={})",
                file_id, file.status
            ));
        }
        if guard
            .get_pending_suspected_duplicate(file_id)
            .map_err(|e| format!("Failed to read suspicion: {}", e))?
            .is_none()
        {
            return Err(format!("no pending suspicion for file {file_id}"));
        }
        guard
            .resolve_suspected_duplicate(file_id, "ignored")
            .map_err(|e| format!("Failed to resolve suspicion: {}", e))?;
    }

    let guard = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    let files = guard
        .get_files_with_metadata()
        .map_err(|e| format!("Failed to reload files: {}", e))?;
    let file = files
        .into_iter()
        .find(|f| f.file.id == file_id)
        .ok_or_else(|| format!("file {} missing after ignore", file_id))?;
    Ok(ResolveSuspectedOutcome::Ignored {
        file: Box::new(file),
    })
}

/// Load settings from the store (defaults when missing).
pub fn get_settings(settings_store: &Mutex<SettingsStore>) -> Result<AppSettings, String> {
    let store = settings_store
        .lock()
        .map_err(|e| format!("Settings lock error: {}", e))?;
    store
        .load()
        .map_err(|e| format!("Failed to load settings: {}", e))
}

/// Normalize and persist settings. Returns the stored snapshot.
pub fn save_settings(
    settings_store: &Mutex<SettingsStore>,
    mut settings: AppSettings,
) -> Result<AppSettings, String> {
    settings.scan_directories = normalize_scan_directories(&settings.scan_directories);
    if !settings.avdc_path.trim().is_empty() {
        settings.avdc_path = canonicalize_user_path(&settings.avdc_path);
    }
    settings.normalize_for_save();

    {
        let store = settings_store
            .lock()
            .map_err(|e| format!("Settings lock error: {}", e))?;
        store
            .save(&settings)
            .map_err(|e| format!("Failed to save settings: {}", e))?;
    }

    Ok(settings)
}

/// Create or fetch a tag id (command-layer wrapper around DB tag CRUD).
#[cfg(feature = "test-support")]
pub fn get_or_create_tag(
    db: &Arc<Mutex<Database>>,
    name: &str,
    category: Option<String>,
) -> Result<i64, String> {
    let db = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    db.get_or_create_tag(name, category)
        .map_err(|e| format!("Failed to get or create tag: {}", e))
}

/// Attach an existing tag to a file.
#[cfg(feature = "test-support")]
pub fn add_file_tag(
    db: &Arc<Mutex<Database>>,
    file_id: i64,
    tag_id: i64,
    confidence: f64,
) -> Result<(), String> {
    let db = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    db.add_file_tag(file_id, tag_id, confidence)
        .map_err(|e| format!("Failed to add file tag: {}", e))
}

/// List tags for a file.
#[cfg(feature = "test-support")]
pub fn get_file_tags(db: &Arc<Mutex<Database>>, file_id: i64) -> Result<Vec<Tag>, String> {
    let db = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    db.get_file_tags(file_id)
        .map_err(|e| format!("Failed to list file tags: {}", e))
}

/// Clear all tags for a file.
#[cfg(feature = "test-support")]
pub fn clear_file_tags(db: &Arc<Mutex<Database>>, file_id: i64) -> Result<(), String> {
    let db = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    db.clear_file_tags(file_id)
        .map_err(|e| format!("Failed to clear file tags: {}", e))
}

/// Insert an error log row (same path command failures use).
#[cfg(feature = "test-support")]
pub fn insert_error_log(
    db: &Arc<Mutex<Database>>,
    category: &str,
    file_id: Option<i64>,
    path: Option<&str>,
    message: &str,
    detail: Option<&str>,
) -> Result<i64, String> {
    let db = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    db.insert_error_log(category, file_id, path, message, detail)
        .map_err(|e| format!("Failed to insert error log: {}", e))
}

/// List recent error log entries.
pub fn get_error_logs(
    db: &Arc<Mutex<Database>>,
    limit: Option<i64>,
) -> Result<Vec<ErrorLogEntry>, String> {
    let db = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    db.list_error_logs(limit.unwrap_or(100))
        .map_err(|e| format!("Failed to list error logs: {}", e))
}

/// Clear all error log rows; returns deleted count.
pub fn clear_error_logs(db: &Arc<Mutex<Database>>) -> Result<usize, String> {
    let db = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    db.clear_error_logs()
        .map_err(|e| format!("Failed to clear error logs: {}", e))
}

#[tauri::command]
pub async fn scan_directory_command(
    path: String,
    db: State<'_, Arc<Mutex<Database>>>,
    queue: State<'_, Arc<TaskQueue>>,
    settings_store: State<'_, Mutex<SettingsStore>>,
) -> Result<ScanResult, String> {
    let settings = get_settings(&settings_store)?;
    scan_directory(path, &db, &queue, &settings).await
}

#[tauri::command]
pub async fn scan_multiple_directories_command(
    paths: Vec<String>,
    db: State<'_, Arc<Mutex<Database>>>,
    queue: State<'_, Arc<TaskQueue>>,
    settings_store: State<'_, Mutex<SettingsStore>>,
) -> Result<ScanResult, String> {
    let settings = get_settings(&settings_store)?;
    scan_multiple_directories(paths, &db, &queue, &settings).await
}

#[tauri::command]
pub async fn get_all_files_command(
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<Vec<FileWithMetadata>, String> {
    get_all_files(&db)
}

#[tauri::command]
pub async fn resolve_suspected_duplicate_command(
    file_id: i64,
    action: String,
    db: State<'_, Arc<Mutex<Database>>>,
    queue: State<'_, Arc<TaskQueue>>,
    settings_store: State<'_, Mutex<SettingsStore>>,
    app: AppHandle,
) -> Result<ResolveSuspectedOutcome, String> {
    let outcome = resolve_suspected_duplicate(&db, &queue, file_id, &action)?;
    if matches!(outcome, ResolveSuspectedOutcome::Ignored { .. }) {
        // After ignore, enqueue pipeline so the file does not stay unscanned
        let settings = get_settings(&settings_store).unwrap_or_default();
        let base = PipelineOptions::from_env();
        if !base.is_noop() {
            let enqueue = enqueue_pipeline_jobs(
                Some(&app),
                &db,
                &queue,
                &settings,
                vec![file_id],
                base,
            );
            if let Err(e) = enqueue {
                eprintln!(
                    "resolve_suspected: ignore enqueue pipeline failed for {}: {}",
                    file_id, e
                );
            }
        }
    }
    Ok(outcome)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsResponse {
    #[serde(flatten)]
    pub settings: AppSettings,
    /// Present when `IDAM_NEAR_PHASH_MAX_DISTANCE` is set and valid.
    pub near_phash_env_override: Option<u32>,
    pub effective_near_phash_max_distance: u32,
}

#[tauri::command]
pub fn get_settings_command(
    settings_store: State<'_, Mutex<SettingsStore>>,
) -> Result<SettingsResponse, String> {
    let settings = get_settings(&settings_store)?;
    let env_override = crate::phash::parse_env_near_phash_max_distance();
    let effective = settings.effective_near_phash_max_distance();
    Ok(SettingsResponse {
        settings,
        near_phash_env_override: env_override,
        effective_near_phash_max_distance: effective,
    })
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
    let settings = save_settings(&settings_store, settings)?;

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
                let _ = db.insert_error_log(
                    "scrape",
                    Some(file_id),
                    Some(&file_path.to_string_lossy()),
                    &scrape_error,
                    None,
                );
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
    _file_id: i64,
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
            let err_msg = format!("Failed to generate sprite sheet: {}", e);
            {
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
                let _ = db.insert_error_log(
                    "ffmpeg",
                    Some(file_id),
                    Some(&file_path.to_string_lossy()),
                    &err_msg,
                    None,
                );
            }
            return Err(err_msg);
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

#[tauri::command]
pub async fn test_connectivity_command(
    settings_store: State<'_, Mutex<SettingsStore>>,
) -> Result<ConnectivityReport, String> {
    let settings = {
        let store = settings_store
            .lock()
            .map_err(|e| format!("Settings lock error: {}", e))?;
        store
            .load()
            .map_err(|e| format!("Failed to load settings: {}", e))?
    };
    Ok(connectivity::check_all(&settings).await)
}

#[tauri::command]
pub fn canonicalize_path_command(path: String) -> String {
    canonicalize_user_path(&path)
}

#[tauri::command]
pub fn get_error_logs_command(
    limit: Option<i64>,
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<Vec<ErrorLogEntry>, String> {
    get_error_logs(&db, limit)
}

#[tauri::command]
pub fn clear_error_logs_command(
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<usize, String> {
    clear_error_logs(&db)
}

/// Enqueue pipeline jobs for selected files through the existing task queue
#[tauri::command]
pub fn enqueue_pipeline_jobs_command(
    file_ids: Vec<i64>,
    options: Option<PipelineOptionsInput>,
    app: AppHandle,
    db: State<'_, Arc<Mutex<Database>>>,
    queue: State<'_, Arc<TaskQueue>>,
    settings_store: State<'_, Mutex<SettingsStore>>,
) -> Result<EnqueueResult, String> {
    let base = options
        .unwrap_or(PipelineOptionsInput {
            enable_scrape: Some(true),
            enable_move: Some(true),
            enable_preview: Some(true),
        })
        .into_options();
    let settings = get_settings(&settings_store)?;
    enqueue_pipeline_jobs(Some(&app), &db, &queue, &settings, file_ids, base)
}

/// One-click organize: enqueue full pipeline for all non-missing library files
#[tauri::command]
pub fn organize_eligible_files_command(
    app: AppHandle,
    db: State<'_, Arc<Mutex<Database>>>,
    queue: State<'_, Arc<TaskQueue>>,
    settings_store: State<'_, Mutex<SettingsStore>>,
) -> Result<EnqueueResult, String> {
    let settings = get_settings(&settings_store)?;
    let file_ids = {
        let guard = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        guard
            .list_organizable_file_ids()
            .map_err(|e| format!("Failed to list files: {}", e))?
    };

    enqueue_pipeline_jobs(
        Some(&app),
        &db,
        &queue,
        &settings,
        file_ids,
        PipelineOptions::default(),
    )
}

/// Enqueue pipeline jobs for selected files through the task queue.
///
/// Pass `app: None` to bind the DB without starting workers (integration tests).
pub fn enqueue_pipeline_jobs(
    app: Option<&AppHandle>,
    db: &Arc<Mutex<Database>>,
    queue: &Arc<TaskQueue>,
    settings: &AppSettings,
    file_ids: Vec<i64>,
    base: PipelineOptions,
) -> Result<EnqueueResult, String> {
    if file_ids.is_empty() {
        return Ok(EnqueueResult {
            job_ids: Vec::new(),
            enqueued: 0,
            skipped: 0,
        });
    }

    ensure_queue_ready(queue, app, db)?;

    if base.is_noop() {
        return Err("Pipeline options disable all steps".to_string());
    }

    let limit = batch_enqueue_limit();
    let mut job_ids = Vec::new();
    let mut skipped = 0usize;

    for file_id in file_ids {
        if job_ids.len() >= limit {
            skipped += 1;
            continue;
        }

        let options = {
            let guard = db
                .lock()
                .map_err(|e| format!("Database lock error: {}", e))?;
            match guard.get_file_by_id(file_id) {
                Ok(file) => {
                    if file_status::blocks_auto_pipeline(&file.status) {
                        skipped += 1;
                        continue;
                    }
                    if !std::path::Path::new(&file.path).exists() {
                        let _ = guard.update_file_status(file_id, file_status::missing);
                        skipped += 1;
                        continue;
                    }
                    crate::pipeline::options_for_existing_file(
                        &guard,
                        file_id,
                        &base,
                        Some(settings),
                    )
                    .unwrap_or_else(|_| base.clone())
                }
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            }
        };

        if options.is_noop() {
            skipped += 1;
            continue;
        }

        match queue.has_active_pipeline(file_id) {
            Ok(true) => {
                skipped += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                log_command_error(
                    db,
                    "queue",
                    Some(file_id),
                    None,
                    &format!("active pipeline check failed: {}", e),
                );
                skipped += 1;
                continue;
            }
        }

        match queue.enqueue_pipeline(file_id, options, None) {
            Ok(id) => job_ids.push(id),
            Err(e) => {
                log_command_error(
                    db,
                    "queue",
                    Some(file_id),
                    None,
                    &format!("enqueue failed: {}", e),
                );
                skipped += 1;
            }
        }
    }

    Ok(EnqueueResult {
        enqueued: job_ids.len(),
        skipped,
        job_ids,
    })
}

#[cfg(test)]
mod path_tests {
    use super::{canonicalize_user_path, normalize_scan_directories};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn normalize_dedupes_canonical_paths() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "idam_canon_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        let canon = canonicalize_user_path(dir.to_str().unwrap());
        assert!(!canon.is_empty());
        let dirs = vec![
            dir.to_string_lossy().to_string(),
            canon.clone(),
            format!("{}/.", dir.display()),
        ];
        let normalized = normalize_scan_directories(&dirs);
        assert_eq!(normalized.len(), 1, "{:?}", normalized);
        assert_eq!(normalized[0], canon);
        let _ = fs::remove_dir_all(&dir);
    }
}

