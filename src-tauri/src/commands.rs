use crate::connectivity::{self, ConnectivityReport};
use crate::database::{Database, ErrorLogEntry, FileWithMetadata, Metadata, Tag};
use crate::ffmpeg;
use crate::file_status;
use crate::path_template::{self, TemplateContext};
use crate::pipeline::{self, PipelineOptions};
use crate::scanner;
use crate::scraper::{scrape_metadata, to_database_metadata};
use crate::settings::{AppSettings, SettingsStore};
use crate::task_queue::{TaskQueue, TaskQueueStatus};
use crate::watcher::{self, WatcherManager, WatcherStatus};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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
    Ignored {
        file: Box<FileWithMetadata>,
        pipeline_enqueued: bool,
    },
}

/// Confirm or ignore a near-phash suspected duplicate.
///
/// - `confirm`: atomic tombstone + resolve + delete; PathSuppress only after COMMIT
/// - `ignore`: mark ignored, set ready; optionally enqueue pipeline when args provided
pub fn resolve_suspected_duplicate_with_enqueue(
    db: &Arc<Mutex<Database>>,
    queue: &Arc<TaskQueue>,
    file_id: i64,
    action: &str,
    app: Option<&AppHandle>,
    settings: Option<&AppSettings>,
    pipeline: Option<PipelineOptions>,
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
            guard
                .confirm_suspected_duplicate(file_id)
                .map_err(|e| format!("Failed to confirm duplicate: {}", e))?
        };
        // PathSuppress only after COMMIT succeeded
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

    let mut pipeline_enqueued = false;
    if let (Some(settings), Some(opts)) = (settings, pipeline) {
        if !opts.is_noop() {
            match enqueue_pipeline_jobs(app, db, queue, settings, vec![file_id], opts) {
                Ok(result) => {
                    pipeline_enqueued = result.enqueued > 0;
                }
                Err(e) => {
                    eprintln!(
                        "resolve_suspected: ignore enqueue pipeline failed for {}: {}",
                        file_id, e
                    );
                }
            }
        }
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
        pipeline_enqueued,
    })
}

/// Settings template preview result (#40)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewTemplateResult {
    pub rendered_path: String,
    pub template: String,
    pub base_dir: String,
}

/// Render `output_template` with sample (or provided) context for Settings preview.
pub fn preview_output_template(
    template: &str,
    base_dir: Option<&str>,
) -> Result<PreviewTemplateResult, String> {
    let trimmed = template.trim();
    if trimmed.is_empty() {
        return Err("output_template is empty".to_string());
    }
    let base = base_dir
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("/library");
    let ctx = TemplateContext::sample();
    let dest = path_template::render_output_path(trimmed, &ctx, Path::new(base))
        .map_err(|e| format!("Failed to preview template: {e:#}"))?;
    let rendered_path = dest.to_string_lossy().to_string();
    Ok(PreviewTemplateResult {
        rendered_path,
        template: trimmed.to_string(),
        base_dir: base.to_string(),
    })
}

/// Outcome of resolving a template path collision (#40).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ResolvePathCollisionOutcome {
    Suffix {
        file: Box<FileWithMetadata>,
        new_path: String,
    },
    Skip {
        file: Box<FileWithMetadata>,
    },
    Overwrite {
        file: Box<FileWithMetadata>,
        new_path: String,
    },
}

/// Resolve a pending path collision: `suffix` | `skip` | `overwrite`.
///
/// Overwrite requires `confirmed == true` (frontend confirm dialog).
pub fn resolve_path_collision(
    db: &Arc<Mutex<Database>>,
    queue: &Arc<TaskQueue>,
    file_id: i64,
    action: &str,
    confirmed: bool,
) -> Result<ResolvePathCollisionOutcome, String> {
    let action = action.trim().to_lowercase();
    if action != "suffix" && action != "skip" && action != "overwrite" {
        return Err(format!(
            "invalid action '{action}' (expected suffix, skip, or overwrite)"
        ));
    }
    if action == "overwrite" && !confirmed {
        return Err("overwrite requires confirmed=true".to_string());
    }

    let suppressions = queue.path_suppressions().ok();

    let (source, desired, source_path_str, mover_size, mover_phash) = {
        let guard = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        let file = guard
            .get_file_by_id(file_id)
            .map_err(|e| format!("File not found: {}", e))?;
        if file.status != file_status::path_collision {
            return Err(format!(
                "file {} is not path_collision (status={})",
                file_id, file.status
            ));
        }
        let pending = guard
            .get_pending_path_collision(file_id)
            .map_err(|e| format!("Failed to read path collision: {}", e))?
            .ok_or_else(|| format!("no pending path collision for file {file_id}"))?;
        let source = PathBuf::from(&file.path);
        (
            source,
            PathBuf::from(&pending.desired_path),
            file.path.clone(),
            file.size,
            file.phash.clone(),
        )
    };

    match action.as_str() {
        "skip" => {
            if !source.is_file() {
                let guard = db
                    .lock()
                    .map_err(|e| format!("Database lock error: {}", e))?;
                let _ = guard.update_file_status(file_id, file_status::missing);
                return Err(format!("File missing on disk: {}", source_path_str));
            }
            {
                let guard = db
                    .lock()
                    .map_err(|e| format!("Database lock error: {}", e))?;
                guard
                    .resolve_path_collision(file_id, "skip")
                    .map_err(|e| format!("Failed to resolve path collision: {}", e))?;
            }
            let file = reload_file_with_metadata(db, file_id)?;
            Ok(ResolvePathCollisionOutcome::Skip {
                file: Box::new(file),
            })
        }
        "suffix" => {
            if !source.is_file() {
                let guard = db
                    .lock()
                    .map_err(|e| format!("Database lock error: {}", e))?;
                let _ = guard.update_file_status(file_id, file_status::missing);
                return Err(format!("File missing on disk: {}", source_path_str));
            }
            // #59: suffix must also avoid paths already claimed in Files
            let dest = {
                let guard = db
                    .lock()
                    .map_err(|e| format!("Database lock error: {}", e))?;
                path_template::unique_destination_avoiding(&desired, |candidate| {
                    if candidate.exists() {
                        return true;
                    }
                    let Some(s) = candidate.to_str() else {
                        return true;
                    };
                    guard
                        .find_file_occupying_path(s, file_id)
                        .ok()
                        .flatten()
                        .is_some()
                })
            };
            pipeline::finalize_move(db, file_id, &source, &dest, suppressions.as_deref())
                .map_err(|e| format!("Failed to move with suffix: {e:#}"))?;
            {
                let guard = db
                    .lock()
                    .map_err(|e| format!("Database lock error: {}", e))?;
                guard
                    .resolve_path_collision(file_id, "suffix")
                    .map_err(|e| format!("Failed to resolve path collision: {}", e))?;
            }
            let new_path = dest.to_string_lossy().to_string();
            let file = reload_file_with_metadata(db, file_id)?;
            Ok(ResolvePathCollisionOutcome::Suffix {
                file: Box::new(file),
                new_path,
            })
        }
        "overwrite" => {
            if let Some(supp) = suppressions.as_ref() {
                supp.suppress(&desired);
            }

            // N4 / #59 / #66: crash after FS overwrite before DB commit leaves source gone,
            // desired holding mover bytes, and occupant often in `.idam-bak`. On restart,
            // finish the DB commit from pending_move_to (size/phash backup); keep bak.
            if !source.is_file() {
                return complete_crashed_overwrite(
                    db,
                    file_id,
                    &source_path_str,
                    &desired,
                    mover_size,
                    mover_phash.as_deref(),
                );
            }

            let desired_str = desired
                .to_str()
                .ok_or_else(|| "Destination path is not valid UTF-8".to_string())?
                .to_string();
            // #66: durable intent + source fs id before overwrite FS ops
            let source_fs_id = path_template::read_fs_file_id(&source)
                .map_err(|e| format!("Failed to read source fs id: {e}"))?
                .ok_or_else(|| format!("Source is not a file: {source_path_str}"))?;
            {
                let guard = db
                    .lock()
                    .map_err(|e| format!("Database lock error: {}", e))?;
                guard
                    .set_pending_move_to(file_id, &desired_str, Some(source_fs_id))
                    .map_err(|e| format!("Failed to set pending_move_to: {e}"))?;
            }

            // Filesystem next (backup occupant); commit clears pending_move_to
            let outcome = match path_template::move_file_overwrite(&source, &desired) {
                Ok(o) => o,
                Err(e) => {
                    if let Ok(guard) = db.lock() {
                        let _ = guard.clear_pending_move_to_logged(
                            file_id,
                            source.to_str(),
                        );
                    }
                    let detail = format!("{e:#}");
                    let message = format!("Failed to overwrite move: {detail}");
                    // Occupant may sit in .idam-bak — surface bak path + ErrorLog
                    if detail.contains("restore also failed") {
                        let path_for_log = desired.to_str().or_else(|| source.to_str());
                        if let Ok(guard) = db.lock() {
                            if let Err(log_err) = guard.insert_error_log(
                                "path_collision",
                                Some(file_id),
                                path_for_log,
                                &message,
                                Some(&detail),
                            ) {
                                eprintln!("commands: failed to write ErrorLog: {log_err}");
                            }
                        }
                    }
                    return Err(message);
                }
            };
            if let Some(supp) = suppressions.as_ref() {
                supp.suppress(&desired);
            }

            let new_path = desired_str;

            let commit = {
                let guard = db
                    .lock()
                    .map_err(|e| format!("Database lock error: {}", e))?;
                guard.commit_path_collision_overwrite(
                    file_id,
                    &source_path_str,
                    &new_path,
                    outcome.case_only,
                )
            };

            if let Err(e) = commit {
                if let Err(rb) = path_template::restore_overwrite_files(
                    &source,
                    &desired,
                    outcome.backup.as_deref(),
                ) {
                    // Leave pending_move_to so startup can finish the commit
                    let bak_hint = outcome
                        .backup
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(no bak)".to_string());
                    let message = format!(
                        "overwrite commit failed and restore also failed; occupant may remain at {bak_hint}"
                    );
                    let detail = format!("{rb:#}");
                    if let Ok(guard) = db.lock() {
                        let path_for_log = outcome
                            .backup
                            .as_ref()
                            .and_then(|p| p.to_str())
                            .or_else(|| desired.to_str());
                        if let Err(log_err) = guard.insert_error_log(
                            "path_collision",
                            Some(file_id),
                            path_for_log,
                            &message,
                            Some(&detail),
                        ) {
                            eprintln!("commands: failed to write ErrorLog: {log_err}");
                        }
                    }
                    return Err(format!("{message}; commit: {e}; restore: {detail}"));
                }
                // FS restored to pre-overwrite — drop stale intent
                if let Ok(guard) = db.lock() {
                    let _ = guard.clear_pending_move_to_logged(
                        file_id,
                        source.to_str(),
                    );
                }
                return Err(format!("Failed to commit overwrite: {e}"));
            }

            path_template::discard_overwrite_backup(outcome.backup.as_deref());
            let file = reload_file_with_metadata(db, file_id)?;
            Ok(ResolvePathCollisionOutcome::Overwrite {
                file: Box::new(file),
                new_path,
            })
        }
        _ => unreachable!(),
    }
}

/// Finish an overwrite whose filesystem step already succeeded before a crash (#59 N4 / B1 / #66)
///
/// Never auto-deletes `.idam-bak` — that file is the occupant's only copy until the
/// user cleans it up. Recovery accepts only the primary bak name.
///
/// Commit when pending matches desired AND size/phash match (and same-volume fs id
/// when recorded). Without pending, size/phash alone (legacy). Disagree → refuse
/// and keep pending.
fn complete_crashed_overwrite(
    db: &Arc<Mutex<Database>>,
    file_id: i64,
    source_path_str: &str,
    desired: &Path,
    mover_size: u64,
    mover_phash: Option<&str>,
) -> Result<ResolvePathCollisionOutcome, String> {
    let source = Path::new(source_path_str);
    // Offline volume: do not mark missing (same rule as #60 S4)
    if !pipeline::source_parent_reachable(source) {
        return Err(format!(
            "overwrite crash recovery deferred: source parent unreachable (retryable): {source_path_str}"
        ));
    }
    // S1: dest volume offline → keep pending
    if !pipeline::source_parent_reachable(desired) {
        return Err(format!(
            "overwrite crash recovery deferred: dest parent unreachable (retryable): {}",
            desired.display()
        ));
    }

    let bak = match path_template::lookup_occupant_bak(desired) {
        path_template::OccupantBakLookup::Ambiguous { extras } => {
            let detail = format!(
                "source={source_path_str} dest={} extras={extras:?}",
                desired.display()
            );
            if let Ok(guard) = db.lock() {
                let _ = guard.insert_error_log(
                    "path_collision",
                    Some(file_id),
                    desired.to_str(),
                    "overwrite crash recovery refused: multiple idam-bak candidates",
                    Some(&detail),
                );
            }
            return Err(
                "overwrite crash recovery refused: multiple idam-bak candidates".into(),
            );
        }
        path_template::OccupantBakLookup::Primary(p) => Some(p),
        path_template::OccupantBakLookup::Missing => None,
    };

    if !desired.is_file() {
        if let Ok(guard) = db.lock() {
            let _ = guard.clear_pending_move_to_logged(file_id, Some(source_path_str));
            let _ = guard.update_file_status(file_id, file_status::missing);
        }
        return Err(format!(
            "File missing on disk and overwrite crash recovery not possible: {source_path_str}"
        ));
    }

    let new_path = desired
        .to_str()
        .ok_or_else(|| "Destination path is not valid UTF-8".to_string())?
        .to_string();
    let pending_row = {
        let guard = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        guard
            .get_pending_move_row(file_id)
            .map_err(|e| format!("Failed to read pending_move_to: {e}"))?
    };
    let pending_matches = pending_row
        .as_ref()
        .map(|r| {
            path_template::normalize_db_path(&r.pending_move_to)
                == path_template::normalize_db_path(&new_path)
        })
        .unwrap_or(false);
    let identity_matches = pipeline::dest_matches_mover_identity(desired, mover_size, mover_phash)
        .map_err(|e| format!("overwrite crash identity check failed: {e:#}"))?;

    // B1: pending alone is never enough; identity alone only when no pending row
    if pending_matches && !identity_matches {
        let dest_size = std::fs::metadata(desired)
            .map(|m| m.len())
            .unwrap_or(0);
        let bak_hint = bak
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no bak)".to_string());
        let detail = format!(
            "source={source_path_str} dest={} db_size={mover_size} dest_size={dest_size} phash={:?} bak={bak_hint}",
            desired.display(),
            mover_phash
        );
        if let Ok(guard) = db.lock() {
            let _ = guard.insert_error_log(
                "path_collision",
                Some(file_id),
                desired.to_str(),
                "overwrite crash recovery refused: pending set but destination does not match size/phash; keeping pending",
                Some(&detail),
            );
        }
        return Err(
            "overwrite crash recovery refused: pending set but destination does not match size/phash".into(),
        );
    }

    let allow = (pending_row.is_none() || pending_matches) && identity_matches;
    if !allow {
        let dest_size = std::fs::metadata(desired)
            .map(|m| m.len())
            .unwrap_or(0);
        let bak_hint = bak
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no bak)".to_string());
        let detail = format!(
            "source={source_path_str} dest={} db_size={mover_size} dest_size={dest_size} phash={:?} pending={:?} bak={bak_hint}",
            desired.display(),
            mover_phash,
            pending_row.as_ref().map(|r| r.pending_move_to.clone())
        );
        if let Ok(guard) = db.lock() {
            let _ = guard.insert_error_log(
                "path_collision",
                Some(file_id),
                desired.to_str(),
                "overwrite crash recovery refused: destination does not match recorded file",
                Some(&detail),
            );
        }
        return Err(
            "overwrite crash recovery refused: destination does not match recorded file".into(),
        );
    }

    // S3: same-volume fs id must match when recorded
    if let Some(ref row) = pending_row {
        if let Some(stored) = row.pending_source_fs_id() {
            match path_template::read_fs_file_id(desired) {
                Ok(Some(dest_id))
                    if path_template::fs_file_id_same_volume(stored, dest_id)
                        && stored != dest_id =>
                {
                    let detail = format!(
                        "stored={stored:?} dest={dest_id:?} path={}",
                        desired.display()
                    );
                    if let Ok(guard) = db.lock() {
                        let _ = guard.insert_error_log(
                            "path_collision",
                            Some(file_id),
                            desired.to_str(),
                            "overwrite crash recovery refused: destination fs id does not match pending source; keeping pending",
                            Some(&detail),
                        );
                    }
                    return Err(
                        "overwrite crash recovery refused: destination fs id does not match pending source".into(),
                    );
                }
                _ => {}
            }
        }
    }

    let phash_note = mover_phash
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("(none)");
    {
        let guard = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        guard
            .commit_path_collision_overwrite(file_id, source_path_str, &new_path, false)
            .map_err(|e| format!("Failed to commit crashed overwrite: {e}"))?;
        // B1: never auto-delete bak; surface path for manual cleanup (N2)
        let (message, bak_detail) = match bak.as_ref() {
            Some(p) => (
                "completed overwrite after crash before DB commit; occupant bak kept for manual cleanup",
                format!("bak={}", p.display()),
            ),
            None => (
                "completed overwrite after crash before DB commit; no bak (occupant already gone)",
                "bak=(none)".to_string(),
            ),
        };
        let pending_note = if pending_matches {
            "pending=matched+identity"
        } else {
            "pending=(none-identity-backup)"
        };
        let _ = guard.insert_error_log(
            "path_collision",
            Some(file_id),
            Some(&new_path),
            message,
            Some(&format!(
                "{bak_detail} {pending_note} size={mover_size} phash={phash_note}"
            )),
        );
    }
    let file = reload_file_with_metadata(db, file_id)?;
    Ok(ResolvePathCollisionOutcome::Overwrite {
        file: Box::new(file),
        new_path,
    })
}

fn reload_file_with_metadata(
    db: &Arc<Mutex<Database>>,
    file_id: i64,
) -> Result<FileWithMetadata, String> {
    let guard = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    let files = guard
        .get_files_with_metadata()
        .map_err(|e| format!("Failed to reload files: {}", e))?;
    files
        .into_iter()
        .find(|f| f.file.id == file_id)
        .ok_or_else(|| format!("file {file_id} missing after resolve"))
}

/// List confirmed-duplicate tombstones (Settings UI).
pub fn list_duplicate_tombstones(
    db: &Arc<Mutex<Database>>,
) -> Result<Vec<crate::database::DuplicateTombstone>, String> {
    let guard = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    guard
        .list_duplicate_tombstones()
        .map_err(|e| format!("Failed to list tombstones: {}", e))
}

/// Remove one tombstone by id (Settings UI).
pub fn remove_duplicate_tombstone(
    db: &Arc<Mutex<Database>>,
    id: i64,
) -> Result<bool, String> {
    let guard = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    guard
        .remove_duplicate_tombstone(id)
        .map_err(|e| format!("Failed to remove tombstone: {}", e))
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
    let settings = get_settings(&settings_store).unwrap_or_default();
    let base = PipelineOptions::from_env();
    resolve_suspected_duplicate_with_enqueue(
        &db,
        &queue,
        file_id,
        &action,
        Some(&app),
        Some(&settings),
        Some(base),
    )
}

#[tauri::command]
pub fn list_duplicate_tombstones_command(
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<Vec<crate::database::DuplicateTombstone>, String> {
    list_duplicate_tombstones(&db)
}

#[tauri::command]
pub fn remove_duplicate_tombstone_command(
    id: i64,
    db: State<'_, Arc<Mutex<Database>>>,
) -> Result<bool, String> {
    remove_duplicate_tombstone(&db, id)
}

#[tauri::command]
pub fn preview_output_template_command(
    template: String,
    base_dir: Option<String>,
) -> Result<PreviewTemplateResult, String> {
    preview_output_template(&template, base_dir.as_deref())
}

#[tauri::command]
pub fn resolve_path_collision_command(
    file_id: i64,
    action: String,
    confirmed: Option<bool>,
    db: State<'_, Arc<Mutex<Database>>>,
    queue: State<'_, Arc<TaskQueue>>,
) -> Result<ResolvePathCollisionOutcome, String> {
    resolve_path_collision(
        &db,
        &queue,
        file_id,
        &action,
        confirmed.unwrap_or(false),
    )
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
            "資料夾不存在：請先在設定中加入有效的掃描目錄".to_string(),
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

