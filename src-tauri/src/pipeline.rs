//! Automatic post-ingest pipeline: scrape → template move → preview
//!
//! Runs after Watcher registers a file Step failures are isolated so the
//! watcher worker keeps running Status follows #19 rules (scrape_failed
//! preserved across transient preview work)

use crate::database::{Database, FileRecord, Metadata};
use crate::ffmpeg;
use crate::file_status;
use crate::path_suppression::PathSuppression;
use crate::path_template::{
    self, destination_collides, move_file, render_output_path, rollback_move, TemplateContext,
};
use crate::scraper::{scrape_metadata, to_database_metadata};
use crate::settings::{AppSettings, SettingsStore};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

#[allow(non_upper_case_globals)]
const pipeline_event_name: &str = "watcher-pipeline-progress";

#[allow(non_upper_case_globals)]
const env_disable: &str = "IDAM_PIPELINE_DISABLE";

#[allow(non_upper_case_globals)]
const env_skip_scrape: &str = "IDAM_PIPELINE_SKIP_SCRAPE";

#[allow(non_upper_case_globals)]
const env_skip_move: &str = "IDAM_PIPELINE_SKIP_MOVE";

#[allow(non_upper_case_globals)]
const env_skip_preview: &str = "IDAM_PIPELINE_SKIP_PREVIEW";

/// Toggle individual steps (env overrides applied via [`PipelineOptions::from_env`])
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineOptions {
    pub enable_scrape: bool,
    pub enable_move: bool,
    pub enable_preview: bool,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        Self {
            enable_scrape: true,
            enable_move: true,
            enable_preview: true,
        }
    }
}

impl PipelineOptions {
    /// Read skip flags from the environment for testing
    pub fn from_env() -> Self {
        let mut opts = Self::default();
        if env_flag_truthy(env_disable) {
            opts.enable_scrape = false;
            opts.enable_move = false;
            opts.enable_preview = false;
            return opts;
        }
        if env_flag_truthy(env_skip_scrape) {
            opts.enable_scrape = false;
        }
        if env_flag_truthy(env_skip_move) {
            opts.enable_move = false;
        }
        if env_flag_truthy(env_skip_preview) {
            opts.enable_preview = false;
        }
        opts
    }

    pub fn is_noop(&self) -> bool {
        !self.enable_scrape && !self.enable_move && !self.enable_preview
    }
}

fn env_flag_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            t == "1" || t == "true" || t == "yes" || t == "on"
        }
        Err(_) => false,
    }
}

/// Payload emitted for UI progress observation
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineProgressEvent {
    pub file_id: i64,
    pub path: String,
    pub step: String,
    pub status: String,
    pub message: Option<String>,
    pub new_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PipelineResult {
    pub scrape_ok: Option<bool>,
    pub moved_to: Option<PathBuf>,
    pub preview_path: Option<String>,
    pub final_status: String,
    pub errors: Vec<String>,
    pub aborted: bool,
}

fn stopped(stop_flag: Option<&AtomicBool>) -> bool {
    stop_flag
        .map(|f| f.load(Ordering::SeqCst))
        .unwrap_or(false)
}

fn persist_step_error(
    db: &Arc<Mutex<Database>>,
    category: &str,
    file_id: i64,
    path: &str,
    message: &str,
) {
    if let Ok(guard) = db.lock() {
        if let Err(e) = guard.insert_error_log(
            category,
            Some(file_id),
            if path.is_empty() { None } else { Some(path) },
            message,
            None,
        ) {
            eprintln!("pipeline: failed to write ErrorLog: {}", e);
        }
    }
}

/// True when scrape already ran (success with source_type or scrape_failed)
/// Used to skip re-scrape when a move lands back under the watch tree
pub fn is_already_scraped(db: &Database, file_id: i64) -> Result<bool> {
    let file = db.get_file_by_id(file_id)?;
    if file.status == file_status::scrape_failed {
        return Ok(true);
    }
    if let Some(meta) = db.get_metadata(file_id)? {
        if meta
            .source_type
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_sprite(db: &Database, file_id: i64) -> Result<bool> {
    Ok(db
        .get_metadata(file_id)?
        .and_then(|m| m.sprite_sheet_path)
        .map(|p| !p.trim().is_empty())
        .unwrap_or(false))
}

/// True when template render already equals the file's current path (move no-op)
///
/// Absolute templates compare full paths Relative templates also treat a path
/// that already ends with the rendered `studio/title.ext` layout as organized
/// so soft re-enqueue does not nest another studio folder
pub fn destination_equals_source(
    db: &Database,
    file_id: i64,
    settings: &AppSettings,
) -> Result<bool> {
    let template = settings.output_template.trim();
    if template.is_empty() {
        return Ok(false);
    }

    let file = db.get_file_by_id(file_id)?;
    let source = PathBuf::from(&file.path);
    if !source.is_file() {
        return Ok(false);
    }

    let meta = db.get_metadata(file_id)?;
    let tags = db.get_file_tags(file_id).unwrap_or_default();
    let first_actor = tags
        .iter()
        .find(|t| t.category.as_deref() == Some("Actor"))
        .map(|t| t.name.as_str());
    let ctx = TemplateContext::from_metadata(
        &source,
        meta.as_ref().and_then(|m| m.studio.as_deref()),
        meta.as_ref().and_then(|m| m.release_date.as_deref()),
        meta.as_ref().and_then(|m| m.title.as_deref()),
        first_actor,
    );
    let base_dir = source
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let desired = render_output_path(template, &ctx, &base_dir)?;
    if path_template::paths_equal(&source, &desired) {
        return Ok(true);
    }

    // Relative template: file may already sit at .../<rendered relative>
    // after a prior organize; re-render from parent would nest another level
    if !path_template::is_absolute_template(template) {
        let relative = render_output_path(template, &ctx, Path::new(""))?;
        if !relative.as_os_str().is_empty() && source.ends_with(&relative) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Soften options for a file that is already organized under the watch tree
///
/// When `settings` is provided also disables move if the template destination
/// already equals the current path so fully-done files become noop
pub fn options_for_existing_file(
    db: &Database,
    file_id: i64,
    base: &PipelineOptions,
    settings: Option<&AppSettings>,
) -> Result<PipelineOptions> {
    let mut opts = base.clone();
    if is_already_scraped(db, file_id)? {
        opts.enable_scrape = false;
    }
    if has_sprite(db, file_id)? {
        opts.enable_preview = false;
    }
    if opts.enable_move {
        if let Some(settings) = settings {
            if destination_equals_source(db, file_id, settings)? {
                opts.enable_move = false;
            }
        }
    }
    Ok(opts)
}

/// Run the full chain for one file Failure in one step does not abort the
/// watcher caller should catch and log
pub fn run_for_file(
    app: &AppHandle,
    db: &Arc<Mutex<Database>>,
    file_id: i64,
    options: &PipelineOptions,
    stop_flag: Option<&AtomicBool>,
    suppressions: Option<&PathSuppression>,
) -> PipelineResult {
    if options.is_noop() {
        return PipelineResult {
            scrape_ok: None,
            moved_to: None,
            preview_path: None,
            final_status: String::new(),
            errors: Vec::new(),
            aborted: false,
        };
    }

    let settings = load_settings(app);
    let mut result = PipelineResult {
        scrape_ok: None,
        moved_to: None,
        preview_path: None,
        final_status: String::new(),
        errors: Vec::new(),
        aborted: false,
    };

    // Near-phash / path-collision must not auto-run scrape/move/preview
    if let Ok(status) = current_status(db, file_id) {
        if status == file_status::suspected_duplicate {
            eprintln!(
                "pipeline: soft-skip suspected_duplicate file_id {}",
                file_id
            );
            let path = current_path(db, file_id).unwrap_or_default();
            emit_progress(
                app,
                PipelineProgressEvent {
                    file_id,
                    path,
                    step: "pipeline".to_string(),
                    status: "skipped".to_string(),
                    message: Some("skipped: suspected_duplicate (awaiting confirm/ignore)".to_string()),
                    new_path: None,
                },
            );
            return result;
        }
        if status == file_status::path_collision {
            eprintln!(
                "pipeline: soft-skip path_collision file_id {}",
                file_id
            );
            let path = current_path(db, file_id).unwrap_or_default();
            emit_progress(
                app,
                PipelineProgressEvent {
                    file_id,
                    path,
                    step: "pipeline".to_string(),
                    status: "skipped".to_string(),
                    message: Some(
                        "skipped: path_collision (awaiting suffix/skip/overwrite)".to_string(),
                    ),
                    new_path: None,
                },
            );
            return result;
        }
    }

    let initial_path = match current_path(db, file_id) {
        Ok(p) => p,
        Err(e) => {
            // Exact phash dedup may have removed the provisional row before pipeline runs
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("QueryReturnedNoRows") {
                eprintln!(
                    "pipeline: soft-skip missing file_id {} (likely exact phash dedup): {}",
                    file_id, msg
                );
                emit_progress(
                    app,
                    PipelineProgressEvent {
                        file_id,
                        path: String::new(),
                        step: "pipeline".to_string(),
                        status: "skipped".to_string(),
                        message: Some("skipped: file removed (exact phash duplicate)".to_string()),
                        new_path: None,
                    },
                );
                return result;
            }
            result.errors.push(msg);
            emit_progress(
                app,
                PipelineProgressEvent {
                    file_id,
                    path: String::new(),
                    step: "pipeline".to_string(),
                    status: "failed".to_string(),
                    message: Some(e.to_string()),
                    new_path: None,
                },
            );
            return result;
        }
    };

    if stopped(stop_flag) {
        result.aborted = true;
        return result;
    }

    // --- scrape ---
    if options.enable_scrape {
        emit_progress(
            app,
            PipelineProgressEvent {
                file_id,
                path: initial_path.clone(),
                step: "scrape".to_string(),
                status: "started".to_string(),
                message: None,
                new_path: None,
            },
        );
        match step_scrape(db, file_id, &settings) {
            Ok(true) => {
                result.scrape_ok = Some(true);
                emit_progress(
                    app,
                    PipelineProgressEvent {
                        file_id,
                        path: initial_path.clone(),
                        step: "scrape".to_string(),
                        status: "ok".to_string(),
                        message: None,
                        new_path: None,
                    },
                );
            }
            Ok(false) => {
                result.scrape_ok = Some(false);
                let msg = "scrape returned no result".to_string();
                result.errors.push(msg.clone());
                persist_step_error(db, "scrape", file_id, &initial_path, &msg);
                emit_progress(
                    app,
                    PipelineProgressEvent {
                        file_id,
                        path: initial_path.clone(),
                        step: "scrape".to_string(),
                        status: "failed".to_string(),
                        message: Some(msg),
                        new_path: None,
                    },
                );
            }
            Err(e) => {
                result.scrape_ok = Some(false);
                let msg = format!("scrape: {:#}", e);
                result.errors.push(msg.clone());
                persist_step_error(db, "scrape", file_id, &initial_path, &msg);
                emit_progress(
                    app,
                    PipelineProgressEvent {
                        file_id,
                        path: initial_path.clone(),
                        step: "scrape".to_string(),
                        status: "failed".to_string(),
                        message: Some(e.to_string()),
                        new_path: None,
                    },
                );
            }
        }
    } else {
        emit_progress(
            app,
            PipelineProgressEvent {
                file_id,
                path: initial_path.clone(),
                step: "scrape".to_string(),
                status: "skipped".to_string(),
                message: None,
                new_path: None,
            },
        );
    }

    if stopped(stop_flag) {
        result.aborted = true;
        result.final_status = current_status(db, file_id).unwrap_or_default();
        return result;
    }

    // --- move (uses metadata when available even after scrape_failed) ---
    if options.enable_move {
        emit_progress(
            app,
            PipelineProgressEvent {
                file_id,
                path: initial_path.clone(),
                step: "move".to_string(),
                status: "started".to_string(),
                message: None,
                new_path: None,
            },
        );
        match step_move(db, file_id, &settings, suppressions) {
            Ok(MoveStepOutcome::Moved(new_path)) => {
                let new_str = new_path.to_string_lossy().to_string();
                result.moved_to = Some(new_path);
                emit_progress(
                    app,
                    PipelineProgressEvent {
                        file_id,
                        path: initial_path.clone(),
                        step: "move".to_string(),
                        status: "ok".to_string(),
                        message: None,
                        new_path: Some(new_str),
                    },
                );
            }
            Ok(MoveStepOutcome::Unchanged) => {
                emit_progress(
                    app,
                    PipelineProgressEvent {
                        file_id,
                        path: initial_path.clone(),
                        step: "move".to_string(),
                        status: "skipped".to_string(),
                        message: Some("destination equals source".to_string()),
                        new_path: None,
                    },
                );
            }
            Ok(MoveStepOutcome::CollisionPending { desired }) => {
                let desired_str = desired.to_string_lossy().to_string();
                emit_progress(
                    app,
                    PipelineProgressEvent {
                        file_id,
                        path: initial_path.clone(),
                        step: "move".to_string(),
                        status: "skipped".to_string(),
                        message: Some(format!(
                            "path collision at {} (awaiting suffix/skip/overwrite)",
                            desired_str
                        )),
                        new_path: Some(desired_str),
                    },
                );
            }
            Err(e) => {
                let msg = format!("move: {:#}", e);
                result.errors.push(msg.clone());
                persist_step_error(db, "move", file_id, &initial_path, &msg);
                emit_progress(
                    app,
                    PipelineProgressEvent {
                        file_id,
                        path: initial_path.clone(),
                        step: "move".to_string(),
                        status: "failed".to_string(),
                        message: Some(e.to_string()),
                        new_path: None,
                    },
                );
            }
        }
    } else {
        emit_progress(
            app,
            PipelineProgressEvent {
                file_id,
                path: initial_path.clone(),
                step: "move".to_string(),
                status: "skipped".to_string(),
                message: None,
                new_path: None,
            },
        );
    }

    if stopped(stop_flag) {
        result.aborted = true;
        result.final_status = current_status(db, file_id).unwrap_or_default();
        return result;
    }

    // --- preview ---
    if options.enable_preview {
        let path_now = current_path(db, file_id).unwrap_or(initial_path.clone());
        emit_progress(
            app,
            PipelineProgressEvent {
                file_id,
                path: path_now.clone(),
                step: "preview".to_string(),
                status: "started".to_string(),
                message: None,
                new_path: None,
            },
        );
        match step_preview(db, file_id) {
            Ok(vtt) => {
                result.preview_path = Some(vtt.clone());
                emit_progress(
                    app,
                    PipelineProgressEvent {
                        file_id,
                        path: path_now,
                        step: "preview".to_string(),
                        status: "ok".to_string(),
                        message: None,
                        new_path: Some(vtt),
                    },
                );
            }
            Err(e) => {
                let msg = format!("preview: {:#}", e);
                result.errors.push(msg.clone());
                persist_step_error(db, "ffmpeg", file_id, &path_now, &msg);
                emit_progress(
                    app,
                    PipelineProgressEvent {
                        file_id,
                        path: path_now,
                        step: "preview".to_string(),
                        status: "failed".to_string(),
                        message: Some(e.to_string()),
                        new_path: None,
                    },
                );
            }
        }
    } else {
        emit_progress(
            app,
            PipelineProgressEvent {
                file_id,
                path: initial_path.clone(),
                step: "preview".to_string(),
                status: "skipped".to_string(),
                message: None,
                new_path: None,
            },
        );
    }

    result.final_status = current_status(db, file_id).unwrap_or_default();
    emit_progress(
        app,
        PipelineProgressEvent {
            file_id,
            path: current_path(db, file_id).unwrap_or(initial_path),
            step: "done".to_string(),
            status: if result.errors.is_empty() {
                "ok".to_string()
            } else {
                "completed_with_errors".to_string()
            },
            message: if result.errors.is_empty() {
                None
            } else {
                Some(result.errors.join("; "))
            },
            new_path: result
                .moved_to
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
        },
    );

    result
}

pub(crate) fn load_settings(app: &AppHandle) -> AppSettings {
    match app.try_state::<Mutex<SettingsStore>>() {
        Some(store) => store
            .lock()
            .ok()
            .and_then(|s| s.load().ok())
            .unwrap_or_default(),
        None => AppSettings::default(),
    }
}

fn current_path(db: &Arc<Mutex<Database>>, file_id: i64) -> Result<String> {
    let guard = db
        .lock()
        .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
    match guard.get_file_by_id(file_id) {
        Ok(file) => Ok(file.path),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            anyhow::bail!("File id {} not found", file_id)
        }
        Err(e) => Err(e).with_context(|| format!("File id {} lookup failed", file_id)),
    }
}

fn current_status(db: &Arc<Mutex<Database>>, file_id: i64) -> Result<String> {
    let guard = db
        .lock()
        .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
    let file = guard.get_file_by_id(file_id)?;
    Ok(file.status)
}

fn emit_progress(app: &AppHandle, payload: PipelineProgressEvent) {
    let _ = app.emit(pipeline_event_name, payload);
}

/// Scrape using persisted settings Persist success or mark scrape_failed
/// Returns Ok(true) on metadata write Ok(false) should not happen; Err on I/O
fn step_scrape(
    db: &Arc<Mutex<Database>>,
    file_id: i64,
    settings: &AppSettings,
) -> Result<bool> {
    let (file_path, phash, existing_sprite) = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let file = guard.get_file_by_id(file_id)?;
        if !Path::new(&file.path).exists() {
            guard.update_file_status(file_id, file_status::missing)?;
            anyhow::bail!("File missing on disk: {}", file.path);
        }
        guard.update_file_status(file_id, file_status::processing)?;
        let sprite = guard
            .get_metadata(file_id)?
            .and_then(|m| m.sprite_sheet_path);
        (PathBuf::from(&file.path), file.phash.clone(), sprite)
    };

    let avdc = {
        let t = settings.avdc_path.trim();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    };
    let stashdb = {
        let t = settings.stashdb_url.trim();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to create tokio runtime for scrape")?;

    let scrape_outcome =
        runtime.block_on(scrape_metadata(&file_path, phash, avdc, stashdb));

    match scrape_outcome {
        Ok((scraped_meta, source_type)) => {
            let db_metadata =
                to_database_metadata(&scraped_meta, file_id, source_type, existing_sprite);
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            guard.persist_scrape_success(
                &db_metadata,
                &scraped_meta.tags,
                &scraped_meta.actors,
                file_status::ready,
            )?;
            Ok(true)
        }
        Err(e) => {
            let guard = db
                .lock()
                .map_err(|lock_err| anyhow::anyhow!("scrape failed ({}); lock: {}", e, lock_err))?;
            guard.update_file_status(file_id, file_status::scrape_failed)?;
            Err(e)
        }
    }
}

/// Result of the template move step (#40 surfaces collisions instead of auto-suffix)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveStepOutcome {
    Unchanged,
    Moved(PathBuf),
    CollisionPending { desired: PathBuf },
}

/// True when the source parent directory is reachable (drive not offline)
pub fn source_parent_reachable(source: &Path) -> bool {
    match source.parent() {
        None => true,
        Some(parent) if parent.as_os_str().is_empty() => true,
        Some(parent) => parent.exists(),
    }
}

/// Test hook: inject dest phash for heal / overwrite-crash identity without ffmpeg
#[cfg(any(test, feature = "test-support"))]
pub mod heal_identity_override {
    use std::cell::RefCell;

    thread_local! {
        #[allow(non_upper_case_globals)]
        static dest_phash: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub fn arm_dest_phash(hex: &str) {
        dest_phash.with(|c| *c.borrow_mut() = Some(hex.to_string()));
    }

    pub fn clear() {
        dest_phash.with(|c| *c.borrow_mut() = None);
    }

    pub fn take_dest_phash() -> Option<String> {
        dest_phash.with(|c| c.borrow_mut().take())
    }
}

/// Test hook: after FS rename succeeds, fail before heal and leave `pending_move_to` set
#[cfg(any(test, feature = "test-support"))]
pub mod finalize_move_fault {
    use std::cell::Cell;

    thread_local! {
        static LEAVE_PENDING: Cell<bool> = const { Cell::new(false) };
    }

    pub fn arm_fail_after_fs_leave_pending() {
        LEAVE_PENDING.with(|f| f.set(true));
    }

    pub fn clear() {
        LEAVE_PENDING.with(|f| f.set(false));
    }

    pub fn should_fail_after_fs_leave_pending() -> bool {
        LEAVE_PENDING.with(|f| {
            if f.get() {
                f.set(false);
                true
            } else {
                false
            }
        })
    }
}

fn resolve_dest_phash_for_heal(dest: &Path) -> Option<String> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(p) = heal_identity_override::take_dest_phash() {
        return Some(p);
    }
    crate::phash::calculate_video_phash(dest).ok()
}

/// Shared identity check for move heal and overwrite-crash recovery (#50 / #59 B1)
pub fn dest_matches_mover_identity(
    dest: &Path,
    mover_size: u64,
    mover_phash: Option<&str>,
) -> Result<bool> {
    if path_template::is_idam_transient_path(dest) {
        return Ok(false);
    }
    let meta = match std::fs::metadata(dest) {
        Ok(m) if m.is_file() => m,
        _ => return Ok(false),
    };
    if meta.len() != mover_size {
        return Ok(false);
    }
    let db_phash = mover_phash.map(str::trim).filter(|s| !s.is_empty());
    let Some(db_phash) = db_phash else {
        return Ok(true);
    };
    let Some(dest_phash) = resolve_dest_phash_for_heal(dest) else {
        return Ok(false);
    };
    let Some(a) = crate::phash::parse_phash_hex(db_phash) else {
        return Ok(false);
    };
    let Some(b) = crate::phash::parse_phash_hex(&dest_phash) else {
        return Ok(false);
    };
    Ok(a == b)
}

/// Confirm dest is the same file we moved
/// Size always required; when DB has phash both size and phash must match (exact)
/// Intentionally ignores `.idam-bak` / `.idam-tmp-*` sidecars from #58 overwrite
fn dest_matches_recorded_file(file: &FileRecord, dest: &Path) -> Result<bool> {
    dest_matches_mover_identity(dest, file.size, file.phash.as_deref())
}

/// #50 / #66: source gone after crash mid-move; dest present → heal Files.path
///
/// Commit only when (pending matches desired AND size/phash match), or when
/// there is no pending and size/phash alone match (legacy). Pending without
/// size/phash match → refuse and keep pending (intent ≠ success).
fn heal_move_after_crash(
    db: &Arc<Mutex<Database>>,
    file_id: i64,
    file: &FileRecord,
    source: &Path,
    desired: &Path,
) -> Result<MoveStepOutcome> {
    // Never heal onto overwrite leftovers (#59)
    if path_template::is_idam_transient_path(desired) {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.update_file_status(file_id, file_status::missing)?;
        let _ = guard.clear_pending_move_to_logged(file_id, source.to_str());
        let _ = guard.insert_error_log(
            "move",
            Some(file_id),
            source.to_str(),
            "move heal refused: desired path is an idam sidecar",
            Some(&desired.display().to_string()),
        );
        anyhow::bail!(
            "File missing on disk and desired path is idam sidecar: {}",
            source.display()
        );
    }

    if !desired.is_file() {
        // S4 / S1: parent gone (offline volume) → retryable, keep pending
        if !source_parent_reachable(source) || !source_parent_reachable(desired) {
            anyhow::bail!(
                "move heal deferred: source or dest parent unreachable (retryable): {}",
                source.display()
            );
        }
        // Source and template dest both gone (bak leftovers are not a substitute)
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let _ = guard.clear_pending_move_to_logged(file_id, source.to_str());
        guard.update_file_status(file_id, file_status::missing)?;
        anyhow::bail!("File missing on disk: {}", source.display());
    }

    let desired_str = desired
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Desired path is not valid UTF-8"))?
        .to_string();
    let pending_row = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.get_pending_move_row(file_id)?
    };
    let pending_matches = pending_row
        .as_ref()
        .map(|r| {
            path_template::normalize_db_path(&r.pending_move_to)
                == path_template::normalize_db_path(&desired_str)
        })
        .unwrap_or(false);
    let identity_matches = dest_matches_recorded_file(file, desired)?;

    // B1: pending matches → also require size/phash; no pending → size/phash alone
    // Pending alone is never enough; identity alone only when no pending row
    let allow = (pending_row.is_none() || pending_matches) && identity_matches;

    if pending_matches && !identity_matches {
        let dest_size = std::fs::metadata(desired)
            .map(|m| m.len())
            .unwrap_or(0);
        let detail = format!(
            "source={} dest={} db_size={} dest_size={} phash={:?} pending={:?} bak_leftover={}",
            source.display(),
            desired.display(),
            file.size,
            dest_size,
            file.phash,
            pending_row.as_ref().map(|r| r.pending_move_to.as_str()),
            path_template::primary_idam_bak_path(desired).exists()
        );
        {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            // Keep pending: intent recorded but dest does not match recorded file
            let _ = guard.insert_error_log(
                "move",
                Some(file_id),
                source.to_str(),
                "move heal refused: pending_move_to set but destination does not match size/phash",
                Some(&detail),
            );
        }
        anyhow::bail!(
            "move heal refused: pending_move_to set but destination does not match size/phash ({})",
            detail
        );
    }

    if !allow {
        let dest_size = std::fs::metadata(desired)
            .map(|m| m.len())
            .unwrap_or(0);
        let detail = format!(
            "source={} dest={} db_size={} dest_size={} phash={:?} pending={:?} bak_leftover={}",
            source.display(),
            desired.display(),
            file.size,
            dest_size,
            file.phash,
            pending_row.as_ref().map(|r| r.pending_move_to.as_str()),
            path_template::primary_idam_bak_path(desired).exists()
        );
        {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            let _ = guard.insert_error_log(
                "move",
                Some(file_id),
                source.to_str(),
                "move heal refused: destination exists but does not match recorded file",
                Some(&detail),
            );
        }
        anyhow::bail!(
            "move heal refused: destination exists but does not match recorded file ({})",
            detail
        );
    }

    // S3: same-volume fs id must match when recorded (exact-dup without bak)
    if let Some(ref row) = pending_row {
        if let Err(msg) = pending_fs_id_allows_commit(row, desired) {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            let _ = guard.insert_error_log(
                "move",
                Some(file_id),
                source.to_str(),
                "move heal refused: destination fs id does not match pending source",
                Some(&msg),
            );
            anyhow::bail!("{msg}");
        }
    }

    {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        // Leave any sibling .idam-bak in place (#59); successful heal only repairs DB path
        if let Err(e) = guard.heal_moved_path(file_id, &file.path, &desired_str) {
            let detail = format!(
                "source={} dest={} err={} pending={:?}",
                source.display(),
                desired.display(),
                e,
                pending_row.as_ref().map(|r| r.pending_move_to.as_str())
            );
            let message = if detail.contains("owned by another") {
                "move heal refused: destination path owned by another Files row"
            } else {
                "move heal refused: failed to commit path repair"
            };
            let _ = guard.insert_error_log(
                "move",
                Some(file_id),
                source.to_str(),
                message,
                Some(&detail),
            );
            anyhow::bail!("{message} ({detail})");
        }
    }

    Ok(MoveStepOutcome::Moved(desired.to_path_buf()))
}

/// S3: when pending recorded a source fs id on the same volume, dest must match it.
/// Unreadable / missing dest id → refuse (do not silently skip).
pub(crate) fn pending_fs_id_allows_commit(
    row: &crate::database::PendingMoveRow,
    dest: &Path,
) -> std::result::Result<(), String> {
    let Some(stored) = row.pending_source_fs_id() else {
        return Ok(());
    };
    let dest_id = match path_template::read_fs_file_id(dest) {
        Ok(Some(id)) => id,
        Ok(None) => {
            return Err(format!(
                "pending fs id check: dest is not a file ({})",
                dest.display()
            ));
        }
        Err(e) => {
            return Err(format!(
                "pending fs id check: failed to read dest {}: {e}",
                dest.display()
            ));
        }
    };
    if !path_template::fs_file_id_same_volume(stored, dest_id) {
        // Cross-volume: index is not comparable; size/phash already passed
        return Ok(());
    }
    if stored != dest_id {
        return Err(format!(
            "destination fs id does not match pending source (stored={stored:?} dest={dest_id:?} path={})",
            dest.display()
        ));
    }
    Ok(())
}

/// Startup refusal ErrorLog — dedupe by category+path+message (same as leftover scan)
fn insert_pending_recovery_refusal_log(
    guard: &Database,
    row: &crate::database::PendingMoveRow,
    message: &str,
    detail: &str,
) {
    match guard.has_error_log_entry("move", Some(&row.path), message) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            eprintln!(
                "warn: ErrorLog dedupe check failed for pending recovery file_id={}: {e}",
                row.file_id
            );
            return;
        }
    }
    if let Err(log_err) = guard.insert_error_log(
        "move",
        Some(row.file_id),
        Some(&row.path),
        message,
        Some(detail),
    ) {
        eprintln!(
            "warn: ErrorLog for pending recovery refusal file_id={}: {log_err}",
            row.file_id
        );
    }
}

/// #66: recover every Files row with `pending_move_to` on startup
///
/// Crash points:
/// - before FS: source still present → clear pending (retry will re-set)
/// - after FS / before DB commit: source gone, dest present → complete when
///   size/phash (and same-volume fs id) match; else refuse and keep pending
/// - after DB commit: pending already null (no-op)
pub fn recover_pending_moves(db: &Arc<Mutex<Database>>) -> Result<usize> {
    let pending_rows = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.list_pending_moves()?
    };
    let mut recovered = 0usize;
    for row in pending_rows {
        match recover_one_pending_move(db, &row) {
            Ok(true) => recovered += 1,
            Ok(false) => {}
            Err(e) => {
                eprintln!(
                    "warn: pending_move recovery failed for file_id={}: {:#}",
                    row.file_id, e
                );
                if let Ok(guard) = db.lock() {
                    let _ = guard.insert_error_log(
                        "move",
                        Some(row.file_id),
                        Some(&row.path),
                        "pending_move recovery failed",
                        Some(&format!("{e:#}")),
                    );
                }
            }
        }
    }
    Ok(recovered)
}

/// #66 S2: recover only one file's pending intent (step_move / overwrite)
///
/// Avoids racing other workers' in-flight moves by not scanning the whole table.
pub fn recover_pending_move_for(db: &Arc<Mutex<Database>>, file_id: i64) -> Result<bool> {
    let row = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.get_pending_move_row(file_id)?
    };
    let Some(row) = row else {
        return Ok(false);
    };
    recover_one_pending_move(db, &row)
}

fn recover_one_pending_move(
    db: &Arc<Mutex<Database>>,
    row: &crate::database::PendingMoveRow,
) -> Result<bool> {
    let source = Path::new(&row.path);
    let dest = Path::new(&row.pending_move_to);

    if !source_parent_reachable(source) {
        // Offline source volume — leave pending for a later retry
        return Ok(false);
    }

    // Pre-FS crash: source still here → abandon intent
    if source.is_file() {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.clear_pending_move_to_logged(row.file_id, Some(&row.path))?;
        return Ok(true);
    }

    // S1: dest volume offline (absolute template on another disk) → keep pending
    if !source_parent_reachable(dest) {
        return Ok(false);
    }

    if path_template::is_idam_transient_path(dest) {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.clear_pending_move_to_logged(row.file_id, Some(&row.path))?;
        guard.update_file_status(row.file_id, file_status::missing)?;
        return Ok(true);
    }

    if !dest.is_file() {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.clear_pending_move_to_logged(row.file_id, Some(&row.path))?;
        guard.update_file_status(row.file_id, file_status::missing)?;
        return Ok(true);
    }

    // B1: pending means intent — also require size/phash before commit
    let identity_ok = dest_matches_mover_identity(dest, row.size, row.phash.as_deref())?;
    if !identity_ok {
        let dest_size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        let detail = format!(
            "source={} dest={} db_size={} dest_size={} phash={:?}",
            row.path, row.pending_move_to, row.size, dest_size, row.phash
        );
        let message =
            "pending_move recovery refused: destination does not match size/phash; keeping pending";
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        insert_pending_recovery_refusal_log(&guard, row, message, &detail);
        return Ok(false);
    }

    // S3: same-volume fs id must match when recorded
    if let Err(msg) = pending_fs_id_allows_commit(row, dest) {
        let message =
            "pending_move recovery refused: destination fs id does not match pending source; keeping pending";
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        insert_pending_recovery_refusal_log(&guard, row, message, &msg);
        return Ok(false);
    }

    let pending_collision = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.get_pending_path_collision(row.file_id)?
    };
    let is_overwrite = pending_collision
        .as_ref()
        .map(|c| {
            path_template::normalize_db_path(&c.desired_path)
                == path_template::normalize_db_path(&row.pending_move_to)
                && row.status == file_status::path_collision
        })
        .unwrap_or(false);

    if is_overwrite {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.commit_path_collision_overwrite(
            row.file_id,
            &row.path,
            &row.pending_move_to,
            false,
        )?;
        let _ = guard.insert_error_log(
            "path_collision",
            Some(row.file_id),
            Some(&row.pending_move_to),
            "completed overwrite from pending_move_to on startup recovery",
            Some(&format!("source={} dest={}", row.path, row.pending_move_to)),
        );
        return Ok(true);
    }

    let guard = db
        .lock()
        .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
    guard.heal_moved_path(row.file_id, &row.path, &row.pending_move_to)?;
    Ok(true)
}

/// Render template move on disk then update Files.path
/// On collision records PathCollisions and awaits UI (suffix / skip / overwrite)
/// On DB update failure rolls the file back so DB never points at a wrong path
/// #50: when source is gone but template dest matches recorded size heal DB path
fn step_move(
    db: &Arc<Mutex<Database>>,
    file_id: i64,
    settings: &AppSettings,
    suppressions: Option<&PathSuppression>,
) -> Result<MoveStepOutcome> {
    let template = settings.output_template.trim();
    if template.is_empty() {
        anyhow::bail!("output_template is empty");
    }

    let (source, ctx, base_dir) = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let file = guard.get_file_by_id(file_id)?;
        let source = PathBuf::from(&file.path);
        let meta = guard.get_metadata(file_id)?;
        let tags = guard.get_file_tags(file_id).unwrap_or_default();
        let first_actor = tags
            .iter()
            .find(|t| t.category.as_deref() == Some("Actor"))
            .map(|t| t.name.as_str());

        let ctx = TemplateContext::from_metadata(
            &source,
            meta.as_ref().and_then(|m| m.studio.as_deref()),
            meta.as_ref().and_then(|m| m.release_date.as_deref()),
            meta.as_ref().and_then(|m| m.title.as_deref()),
            first_actor,
        );
        let base_dir = source
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        (source, ctx, base_dir)
    };

    let desired = render_output_path(template, &ctx, &base_dir)?;
    // If rendered path is the source itself skip (no collision / no move)
    if path_template::paths_equal(&source, &desired) {
        if source.is_file() {
            return Ok(MoveStepOutcome::Unchanged);
        }
        // Path equal but file gone → true missing
        {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            guard.update_file_status(file_id, file_status::missing)?;
        }
        anyhow::bail!("File missing on disk: {}", source.display());
    }

    // #66 S2: finish this file's pending intent only (do not scan the whole table)
    {
        let pending = {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            guard.get_pending_move_to(file_id)?
        };
        if pending.is_some() {
            let _ = recover_pending_move_for(db, file_id)?;
            // Reload source after recovery may have updated Files.path
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            let refreshed = guard.get_file_by_id(file_id)?;
            let desired_str = desired
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Desired path is not valid UTF-8"))?;
            if path_template::normalize_db_path(&refreshed.path)
                == path_template::normalize_db_path(desired_str)
            {
                return Ok(MoveStepOutcome::Moved(desired));
            }
        }
    }

    let (source, file_snapshot) = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let file = guard.get_file_by_id(file_id)?;
        (PathBuf::from(&file.path), file)
    };

    // #50 crash recovery before collision / finalize
    if !source.is_file() {
        // S4: offline volume looks like a missing source — do not heal or mark missing
        if !source_parent_reachable(&source) {
            anyhow::bail!(
                "move heal deferred: source parent unreachable (retryable): {}",
                source.display()
            );
        }
        return heal_move_after_crash(db, file_id, &file_snapshot, &source, &desired);
    }

    // #40: do not auto-suffix; pause for UI when destination already exists
    if destination_collides(&source, &desired) {
        let desired_str = desired
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Desired path is not valid UTF-8"))?
            .to_string();
        // #59: after skip, same desired path must not re-enter path_collision
        {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            if guard.has_skipped_same_path_collision(file_id, &desired_str)? {
                return Ok(MoveStepOutcome::Unchanged);
            }
        }
        let occupant_id = {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            guard
                .find_file_occupying_path(&desired_str, file_id)?
                .map(|f| f.id)
        };
        {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            guard
                .mark_path_collision_atomic(file_id, &desired_str, occupant_id)
                .map_err(|e| anyhow::anyhow!("Failed to mark path collision: {}", e))?;
        }
        return Ok(MoveStepOutcome::CollisionPending { desired });
    }

    finalize_move(db, file_id, &source, &desired, suppressions)?;
    Ok(MoveStepOutcome::Moved(desired))
}

/// Perform filesystem move + Files.path update (shared by pipeline and collision resolve)
pub fn finalize_move(
    db: &Arc<Mutex<Database>>,
    file_id: i64,
    source: &Path,
    dest: &Path,
    suppressions: Option<&PathSuppression>,
) -> Result<()> {
    finalize_move_inner(
        db,
        file_id,
        source,
        dest,
        suppressions,
        crate::database::OrganizedAtWrite::MarkNow,
    )
}

/// Like [`finalize_move`] but controls `organized_at` in the path-commit transaction (#73 undo)
pub fn finalize_move_with_organized(
    db: &Arc<Mutex<Database>>,
    file_id: i64,
    source: &Path,
    dest: &Path,
    suppressions: Option<&PathSuppression>,
    organized: crate::database::OrganizedAtWrite,
) -> Result<()> {
    finalize_move_inner(db, file_id, source, dest, suppressions, organized)
}

fn finalize_move_inner(
    db: &Arc<Mutex<Database>>,
    file_id: i64,
    source: &Path,
    dest: &Path,
    suppressions: Option<&PathSuppression>,
    organized: crate::database::OrganizedAtWrite,
) -> Result<()> {
    let new_path_str = dest
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Destination path is not valid UTF-8"))?
        .to_string();

    // #66: durable intent + optional source fs id before any filesystem rename
    if !source.is_file() {
        anyhow::bail!("Source is not a file: {}", source.display());
    }
    // Windows: missing index → None (size/phash only); other IO errors degrade too
    let source_fs_id = path_template::read_fs_file_id(source).unwrap_or_else(|e| {
        eprintln!(
            "warn: source fs id unavailable ({}): {e}; pending without fs id",
            source.display()
        );
        None
    });
    {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard
            .set_pending_move_to(file_id, &new_path_str, source_fs_id)
            .map_err(|e| anyhow::anyhow!("Failed to set pending_move_to: {}", e))?;
    }

    // Suppress before the rename so recursive watch events are ignored
    if let Some(supp) = suppressions {
        supp.suppress(dest);
    }

    if let Err(e) = move_file(source, dest).with_context(|| {
        format!(
            "Failed to move {} → {}",
            source.display(),
            dest.display()
        )
    }) {
        // Pre-FS failure: drop intent so startup does not try to complete a no-op
        if let Ok(guard) = db.lock() {
            let _ = guard.clear_pending_move_to_logged(file_id, source.to_str());
        }
        return Err(e);
    }

    // Test hook: FS moved + pending set, heal never runs (simulates crash mid-finalize)
    #[cfg(any(test, feature = "test-support"))]
    if finalize_move_fault::should_fail_after_fs_leave_pending() {
        return Err(anyhow::anyhow!(
            "injected finalize failure after FS move (pending left set)"
        ));
    }

    if let Some(supp) = suppressions {
        supp.suppress(dest);
    }

    // Short lock: path update + clear pending when DB still references the old source
    let update_result = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let file = guard.get_file_by_id(file_id)?;
        if Path::new(&file.path) != source {
            Err(anyhow::anyhow!(
                "Files.path changed during move (expected {} got {})",
                source.display(),
                file.path
            ))
        } else {
            // heal clears pending_move_to and applies organized_at in the same transaction
            guard
                .heal_moved_path_with_organized(
                    file_id,
                    &file.path,
                    &new_path_str,
                    organized,
                )
                .map_err(|e| anyhow::anyhow!("Failed to commit moved path: {}", e))
        }
    };

    if let Err(e) = update_result {
        if let Err(rb) = rollback_move(dest, source) {
            // Leave pending_move_to set so startup / step_move can finish the commit
            return Err(e).context(format!(
                "DB path update failed and rollback also failed: {:#}",
                rb
            ));
        }
        // FS rolled back to source — clear stale intent
        if let Ok(guard) = db.lock() {
            let _ = guard.clear_pending_move_to_logged(file_id, source.to_str());
        }
        return Err(e).context("DB path update failed file rolled back to original location");
    }

    Ok(())
}

fn step_preview(db: &Arc<Mutex<Database>>, file_id: i64) -> Result<String> {
    let (file_path, previous_status) = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let file = guard.get_file_by_id(file_id)?;
        if !Path::new(&file.path).exists() {
            guard.update_file_status(file_id, file_status::missing)?;
            anyhow::bail!("File missing on disk: {}", file.path);
        }
        let previous = file.status.clone();
        // Only mark processing when allowed (preserve scrape_failed until work ends)
        if file_status::allows_transient_processing(&previous)
            || previous == file_status::scrape_failed
        {
            // Preview is transient for ready; for scrape_failed we still flip to
            // processing then restore via after_transient_work
            guard.update_file_status(file_id, file_status::processing)?;
        }
        (PathBuf::from(&file.path), previous)
    };

    let output_dir = std::env::temp_dir().join("intelligent_dam");
    fs_create_dir_all(&output_dir)?;
    let sprite_path = output_dir.join(format!("sprite_{}.jpg", file_id));

    let vtt_path_str = match ffmpeg::generate_sprite_sheet_blocking(&file_path, &sprite_path, (10, 10))
    {
        Ok(path) => path,
        Err(e) => {
            let guard = db
                .lock()
                .map_err(|lock_err| anyhow::anyhow!("preview failed ({}); lock: {}", e, lock_err))?;
            let _ = guard.update_file_status(file_id, &previous_status);
            return Err(e);
        }
    };

    {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        if let Some(mut metadata) = guard.get_metadata(file_id)? {
            metadata.sprite_sheet_path = Some(vtt_path_str.clone());
            guard.update_metadata(&metadata)?;
        } else {
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
            guard.update_metadata(&new_metadata)?;
        }
        let next = file_status::after_transient_work(&previous_status);
        guard.update_file_status(file_id, next)?;
    }

    Ok(vtt_path_str)
}

fn fs_create_dir_all(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("Failed to create directory {}", path.display()))
}

/// Pure step-order helper used by unit tests
#[cfg(test)]
pub fn planned_steps(options: &PipelineOptions) -> Vec<&'static str> {
    let mut steps = Vec::new();
    if options.enable_scrape {
        steps.push("scrape");
    }
    if options.enable_move {
        steps.push("move");
    }
    if options.enable_preview {
        steps.push("preview");
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_template::TemplateContext;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "idam_pipe_{}_{}_{}",
            label,
            std::process::id(),
            nanos
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_db() -> Database {
        Database::new(":memory:").expect("db")
    }

    #[test]
    fn planned_steps_respect_flags() {
        let full = PipelineOptions::default();
        assert_eq!(planned_steps(&full), vec!["scrape", "move", "preview"]);

        let mut skip_scrape = PipelineOptions::default();
        skip_scrape.enable_scrape = false;
        assert_eq!(planned_steps(&skip_scrape), vec!["move", "preview"]);

        let noop = PipelineOptions {
            enable_scrape: false,
            enable_move: false,
            enable_preview: false,
        };
        assert!(noop.is_noop());
        assert!(planned_steps(&noop).is_empty());
    }

    #[test]
    fn step_move_updates_path_after_success() {
        let dir = unique_temp_dir("move_ok");
        let src = dir.join("ABC-100.mp4");
        fs::write(&src, b"payload").unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), 7, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let arc = Arc::new(Mutex::new(db));
        let outcome = step_move(&arc, file_id, &settings, None).expect("move ok");
        let MoveStepOutcome::Moved(moved) = outcome else {
            panic!("expected Moved got {outcome:?}");
        };

        assert!(moved.exists());
        assert!(!src.exists());
        let guard = arc.lock().unwrap();
        let file = guard.get_file_by_id(file_id).unwrap();
        assert_eq!(Path::new(&file.path), moved.as_path());
        assert!(file.path.contains("StudioA"));
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_records_path_collision_instead_of_auto_suffix() {
        let dir = unique_temp_dir("collide_ui");
        let src = dir.join("incoming.mp4");
        let occupied = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(occupied.parent().unwrap()).unwrap();
        fs::write(&src, b"new").unwrap();
        fs::write(&occupied, b"old").unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), 7, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let arc = Arc::new(Mutex::new(db));
        let outcome = step_move(&arc, file_id, &settings, None).expect("collision ok");
        match outcome {
            MoveStepOutcome::CollisionPending { desired } => {
                assert_eq!(desired, occupied);
            }
            other => panic!("expected CollisionPending got {other:?}"),
        }
        assert!(src.exists(), "source must stay until user resolves");
        assert_eq!(fs::read(&occupied).unwrap(), b"old");
        let guard = arc.lock().unwrap();
        let file = guard.get_file_by_id(file_id).unwrap();
        assert_eq!(file.status, file_status::path_collision);
        assert!(guard
            .get_pending_path_collision(file_id)
            .unwrap()
            .is_some());
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_skips_same_desired_after_user_skip() {
        // #59: after skip, re-running move must not re-enter path_collision
        let dir = unique_temp_dir("collide_skip_once");
        let src = dir.join("incoming.mp4");
        let occupied = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(occupied.parent().unwrap()).unwrap();
        fs::write(&src, b"new").unwrap();
        fs::write(&occupied, b"old").unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), 7, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let arc = Arc::new(Mutex::new(db));
        let first = step_move(&arc, file_id, &settings, None).expect("collision");
        assert!(matches!(first, MoveStepOutcome::CollisionPending { .. }));
        {
            let guard = arc.lock().unwrap();
            guard
                .resolve_path_collision(file_id, "skip")
                .expect("skip");
            assert_eq!(
                guard.get_file_by_id(file_id).unwrap().status,
                file_status::ready
            );
        }
        let second = step_move(&arc, file_id, &settings, None).expect("second");
        assert_eq!(second, MoveStepOutcome::Unchanged);
        let guard = arc.lock().unwrap();
        assert_eq!(
            guard.get_file_by_id(file_id).unwrap().status,
            file_status::ready
        );
        assert!(guard
            .get_pending_path_collision(file_id)
            .unwrap()
            .is_none());
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    /// #61 / #58: occupant row inserted with native `dir.join(..).join(..)` must
    /// still be found by find_file_occupying_path after template render.
    /// Before the fix Windows CI failed here (mixed `/` vs `\`).
    #[test]
    fn step_move_collision_finds_occupant_inserted_with_native_path() {
        let dir = unique_temp_dir("collide_native_occ");
        let src = dir.join("incoming.mp4");
        // Native OS separators — must match render_output_path after #61
        let occupied = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(occupied.parent().unwrap()).unwrap();
        fs::write(&src, b"new").unwrap();
        fs::write(&occupied, b"old").unwrap();

        let db = open_db();
        let occupant_id = db
            .insert_file(occupied.to_str().unwrap(), 3, None)
            .expect("occupant");
        let file_id = db
            .insert_file(src.to_str().unwrap(), 7, None)
            .expect("mover");
        assert_ne!(occupant_id, file_id);
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };

        #[cfg(windows)]
        {
            let rendered = render_output_path(
                &settings.output_template,
                &TemplateContext::from_metadata(
                    &src,
                    Some("StudioA"),
                    Some("2024-06-01"),
                    Some("Demo"),
                    None,
                ),
                src.parent().unwrap(),
            )
            .expect("render");
            let rendered_s = rendered.to_string_lossy();
            assert!(
                !rendered_s.contains('/'),
                "precondition for #61: rendered path has no '/': {rendered_s}"
            );
            assert_eq!(rendered, occupied);
        }

        let arc = Arc::new(Mutex::new(db));
        let outcome = step_move(&arc, file_id, &settings, None).expect("collision ok");
        match outcome {
            MoveStepOutcome::CollisionPending { desired } => {
                assert_eq!(desired, occupied);
            }
            other => panic!("expected CollisionPending got {other:?}"),
        }

        let guard = arc.lock().unwrap();
        let pending = guard
            .get_pending_path_collision(file_id)
            .unwrap()
            .expect("pending collision");
        assert_eq!(
            pending.occupant_file_id,
            Some(occupant_id),
            "occupant inserted via native Path::join must be found (#58/#61)"
        );
        assert_eq!(
            guard
                .find_file_occupying_path(occupied.to_str().unwrap(), file_id)
                .unwrap()
                .map(|f| f.id),
            Some(occupant_id)
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_leaves_db_unchanged_when_source_missing() {
        let db = open_db();
        let file_id = db
            .insert_file("/tmp/idam-missing-pipeline-xyz.mp4", 1, None)
            .unwrap();
        let settings = AppSettings {
            output_template: "{{title}}.mp4".to_string(),
            ..AppSettings::default()
        };
        let arc = Arc::new(Mutex::new(db));
        let err = step_move(&arc, file_id, &settings, None).expect_err("should fail");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("missing"),
            "expected missing-file error got {}",
            err
        );
        let guard = arc.lock().unwrap();
        let file = guard.get_file_by_id(file_id).unwrap();
        assert_eq!(
            file.path,
            path_template::normalize_db_path("/tmp/idam-missing-pipeline-xyz.mp4")
        );
        assert_eq!(file.status, file_status::missing);
    }

    #[test]
    fn step_move_heals_db_path_after_crash_mid_move() {
        // Simulate FS move succeeded then crash before Files.path update (#50)
        let dir = unique_temp_dir("heal_ok");
        let src = dir.join("ABC-100.mp4");
        let payload = b"heal-payload-bytes";
        fs::write(&src, payload).unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), payload.len() as u64, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(desired.parent().unwrap()).unwrap();
        // Crash mid-move: bytes already at dest, DB still points at source
        fs::rename(&src, &desired).unwrap();
        assert!(!src.exists());
        assert!(desired.exists());

        let arc = Arc::new(Mutex::new(db));
        let outcome = step_move(&arc, file_id, &settings, None).expect("heal ok");
        let MoveStepOutcome::Moved(moved) = outcome else {
            panic!("expected Moved got {outcome:?}");
        };
        assert_eq!(moved, desired);
        assert!(desired.exists(), "must not re-move or delete dest");
        assert!(!src.exists());
        assert_eq!(fs::read(&desired).unwrap(), payload);

        let guard = arc.lock().unwrap();
        let file = guard.get_file_by_id(file_id).unwrap();
        assert_eq!(Path::new(&file.path), desired.as_path());
        assert_ne!(file.status, file_status::missing);
        assert!(
            guard.list_error_logs(20).unwrap().is_empty(),
            "successful heal must not write ErrorLog"
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_heal_refuses_size_mismatch_and_logs_error() {
        let dir = unique_temp_dir("heal_bad");
        let src = dir.join("ABC-100.mp4");
        fs::write(&src, b"original-seven").unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), 14, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(desired.parent().unwrap()).unwrap();
        // Different file already at dest (wrong size vs DB)
        fs::write(&desired, b"OTHER").unwrap();
        fs::remove_file(&src).unwrap();

        let old_path = src.to_str().unwrap().to_string();
        let arc = Arc::new(Mutex::new(db));
        let err = step_move(&arc, file_id, &settings, None).expect_err("heal must refuse");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("heal refused") || msg.contains("does not match"),
            "expected heal-refuse error got {err}"
        );

        let guard = arc.lock().unwrap();
        let file = guard.get_file_by_id(file_id).unwrap();
        assert_eq!(file.path, old_path, "must not overwrite DB path");
        assert_ne!(file.status, file_status::missing);
        let logs = guard.list_error_logs(20).unwrap();
        assert!(
            logs.iter().any(|e| {
                e.category == "move"
                    && e.message.contains("heal refused")
                    && e.detail
                        .as_deref()
                        .unwrap_or("")
                        .contains("db_size=")
            }),
            "expected ErrorLog on heal refuse got {logs:?}"
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_heal_refuses_same_size_different_phash() {
        let dir = unique_temp_dir("heal_phash");
        let src = dir.join("ABC-100.mp4");
        let payload = b"same-size-payload!";
        fs::write(&src, payload).unwrap();

        let db_phash = "0123456789abcdef";
        let db = open_db();
        let file_id = db
            .insert_file(
                src.to_str().unwrap(),
                payload.len() as u64,
                Some(db_phash.into()),
            )
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(desired.parent().unwrap()).unwrap();
        fs::rename(&src, &desired).unwrap();

        heal_identity_override::arm_dest_phash("fedcba9876543210");
        let old_path = src.to_str().unwrap().to_string();
        let arc = Arc::new(Mutex::new(db));
        let err = step_move(&arc, file_id, &settings, None).expect_err("phash mismatch");
        heal_identity_override::clear();
        assert!(
            err.to_string().contains("heal refused"),
            "expected heal refuse got {err}"
        );

        let guard = arc.lock().unwrap();
        let file = guard.get_file_by_id(file_id).unwrap();
        assert_eq!(file.path, old_path);
        assert!(
            guard.list_error_logs(20).unwrap().iter().any(|e| {
                e.category == "move" && e.message.contains("heal refused")
            }),
            "ErrorLog required on phash mismatch"
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_heals_same_size_when_no_phash() {
        // S3: without recorded phash, size alone is enough
        let dir = unique_temp_dir("heal_nophash");
        let src = dir.join("ABC-100.mp4");
        let payload = b"size-only-identity";
        fs::write(&src, payload).unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), payload.len() as u64, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(desired.parent().unwrap()).unwrap();
        fs::rename(&src, &desired).unwrap();

        let arc = Arc::new(Mutex::new(db));
        let outcome = step_move(&arc, file_id, &settings, None).expect("heal");
        assert!(matches!(outcome, MoveStepOutcome::Moved(_)));
        let guard = arc.lock().unwrap();
        assert_eq!(
            Path::new(&guard.get_file_by_id(file_id).unwrap().path),
            desired.as_path()
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_heal_after_reconcile_restores_ready() {
        // S2: reconcile marks missing while DB still points at old path; heal → ready
        let dir = unique_temp_dir("heal_recon");
        let src = dir.join("ABC-100.mp4");
        let payload = b"reconcile-then-heal";
        fs::write(&src, payload).unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), payload.len() as u64, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(desired.parent().unwrap()).unwrap();
        fs::rename(&src, &desired).unwrap();

        let n = db.reconcile_missing_files().expect("reconcile");
        assert!(n >= 1);
        assert_eq!(
            db.get_file_by_id(file_id).unwrap().status,
            file_status::missing
        );

        let arc = Arc::new(Mutex::new(db));
        let outcome = step_move(&arc, file_id, &settings, None).expect("heal");
        assert!(matches!(outcome, MoveStepOutcome::Moved(_)));
        let guard = arc.lock().unwrap();
        let file = guard.get_file_by_id(file_id).unwrap();
        assert_eq!(Path::new(&file.path), desired.as_path());
        assert_eq!(file.status, file_status::ready);
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_defers_when_source_parent_unreachable() {
        // S4: offline volume — parent missing → retryable, no heal, not missing
        let db = open_db();
        let src = PathBuf::from("/idam-offline-vol-xyz/media/ABC-100.mp4");
        assert!(
            !src.parent().unwrap().exists(),
            "precondition: parent must not exist"
        );
        let file_id = db
            .insert_file(src.to_str().unwrap(), 8, None)
            .expect("insert");
        // #61: insert_file stores normalize_db_path (Windows / → \)
        let stored_path = db.get_file_by_id(file_id).unwrap().path;
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let arc = Arc::new(Mutex::new(db));
        let err = step_move(&arc, file_id, &settings, None).expect_err("deferred");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("retryable") && msg.contains("unreachable"),
            "expected retryable offline error got {err}"
        );
        let guard = arc.lock().unwrap();
        let file = guard.get_file_by_id(file_id).unwrap();
        assert_eq!(file.path, stored_path);
        assert_ne!(file.status, file_status::missing);
    }

    #[test]
    fn step_move_heal_ignores_idam_bak_leftover_as_destination() {
        // N2: template itself renders onto a .idam-bak sidecar path
        let dir = unique_temp_dir("heal_bak");
        let src = dir.join("incoming.mp4");
        let payload = b"mover-bytes";
        fs::write(&src, payload).unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), payload.len() as u64, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}.idam-bak".to_string(),
            ..AppSettings::default()
        };
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4.idam-bak");
        assert!(
            path_template::is_idam_transient_path(&desired),
            "template must resolve to sidecar"
        );
        fs::create_dir_all(desired.parent().unwrap()).unwrap();
        fs::write(&desired, payload).unwrap();
        fs::remove_file(&src).unwrap();

        let old_path = src.to_str().unwrap().to_string();
        let arc = Arc::new(Mutex::new(db));
        let err = step_move(&arc, file_id, &settings, None).expect_err("sidecar refused");
        assert!(
            err.to_string().to_lowercase().contains("sidecar")
                || err.to_string().to_lowercase().contains("idam"),
            "expected sidecar refuse got {err}"
        );

        let guard = arc.lock().unwrap();
        let file = guard.get_file_by_id(file_id).unwrap();
        assert_eq!(file.path, old_path);
        assert!(
            guard.list_error_logs(20).unwrap().iter().any(|e| {
                e.message.contains("sidecar")
            }),
            "ErrorLog for sidecar heal refuse"
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_heal_refuses_occupant_row() {
        let dir = unique_temp_dir("heal_occ");
        let src = dir.join("incoming.mp4");
        let payload = b"mover-payload-xx";
        fs::write(&src, payload).unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), payload.len() as u64, None)
            .expect("insert");
        let old_path = db.get_file_by_id(file_id).unwrap().path;
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        // Native OS separators (#61 / revert 2705a3d rendered-path workaround)
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(desired.parent().unwrap()).unwrap();
        fs::rename(&src, &desired).unwrap();

        let occupant = db
            .insert_file(desired.to_str().unwrap(), payload.len() as u64, None)
            .expect("occupant");
        assert_ne!(occupant, file_id);

        let arc = Arc::new(Mutex::new(db));
        let err = step_move(&arc, file_id, &settings, None).expect_err("occupant");
        assert!(
            err.to_string().contains("owned by another"),
            "expected occupant refuse got {err}"
        );
        let guard = arc.lock().unwrap();
        assert_eq!(guard.get_file_by_id(file_id).unwrap().path, old_path);
        assert!(
            guard.list_error_logs(20).unwrap().iter().any(|e| {
                e.category == "move" && e.message.contains("owned by another")
            }),
            "ErrorLog on occupant refuse must use category move"
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn step_move_heal_refuses_occupant_nocase() {
        // N3: find_file_occupying_path uses COLLATE NOCASE on Windows/macOS
        let dir = unique_temp_dir("heal_nocase");
        let src = dir.join("incoming.mp4");
        let payload = b"nocase-payload!!!";
        fs::write(&src, payload).unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), payload.len() as u64, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        // Native OS separators — same as #61 collision occupant tests
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(desired.parent().unwrap()).unwrap();
        fs::rename(&src, &desired).unwrap();

        // Occupant under differently-cased path string (same FS file on Windows)
        let alt = desired.to_string_lossy().to_uppercase();
        assert_ne!(
            alt,
            desired.to_string_lossy(),
            "precondition: case must differ"
        );
        let occ = db
            .insert_file(&alt, payload.len() as u64, None)
            .expect("occupant nocase");
        assert_ne!(occ, file_id);

        let arc = Arc::new(Mutex::new(db));
        let err = step_move(&arc, file_id, &settings, None).expect_err("nocase occupant");
        assert!(
            err.to_string().contains("owned by another"),
            "expected NOCASE occupant refuse got {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_heal_with_matching_dest_and_bak_leftover_still_heals() {
        // Overwrite left a bak; dest holds our bytes → heal DB only leave bak alone
        let dir = unique_temp_dir("heal_bak_ok");
        let src = dir.join("incoming.mp4");
        let payload = b"our-moved-bytes";
        fs::write(&src, payload).unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), payload.len() as u64, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(desired.parent().unwrap()).unwrap();
        fs::rename(&src, &desired).unwrap();
        let bak = path_template::primary_idam_bak_path(&desired);
        fs::write(&bak, b"old-occupant").unwrap();

        let arc = Arc::new(Mutex::new(db));
        let outcome = step_move(&arc, file_id, &settings, None).expect("heal");
        assert!(matches!(outcome, MoveStepOutcome::Moved(_)));

        let guard = arc.lock().unwrap();
        let file = guard.get_file_by_id(file_id).unwrap();
        assert_eq!(Path::new(&file.path), desired.as_path());
        drop(guard);
        assert_eq!(fs::read(&desired).unwrap(), payload);
        assert!(bak.exists(), "bak leftover must remain (#59)");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_move_cleared_after_successful_finalize() {
        let dir = unique_temp_dir("pending_ok");
        let src = dir.join("ABC-100.mp4");
        fs::write(&src, b"payload").unwrap();
        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), 7, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();
        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        let arc = Arc::new(Mutex::new(db));
        let outcome = step_move(&arc, file_id, &settings, None).expect("move");
        assert!(matches!(outcome, MoveStepOutcome::Moved(_)));
        let guard = arc.lock().unwrap();
        assert_eq!(guard.get_pending_move_to(file_id).unwrap(), None);
        assert_eq!(Path::new(&guard.get_file_by_id(file_id).unwrap().path), desired.as_path());
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_move_crash_before_fs_clears_on_startup_recovery() {
        // Crash point 1: pending written, FS not started — source still present
        let dir = unique_temp_dir("pending_pre_fs");
        let src = dir.join("ABC-100.mp4");
        fs::write(&src, b"still-here").unwrap();
        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), 10, None)
            .expect("insert");
        let dest = dir.join("StudioA").join("out.mp4");
        let src_id = path_template::read_fs_file_id(&src).unwrap().expect("src id");
        db.set_pending_move_to(file_id, dest.to_str().unwrap(), Some(src_id))
            .expect("pending");
        let arc = Arc::new(Mutex::new(db));
        let n = recover_pending_moves(&arc).expect("recover");
        assert_eq!(n, 1);
        let guard = arc.lock().unwrap();
        assert_eq!(guard.get_pending_move_to(file_id).unwrap(), None);
        assert_eq!(Path::new(&guard.get_file_by_id(file_id).unwrap().path), src.as_path());
        assert!(src.exists());
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_move_crash_after_fs_completes_on_startup_recovery() {
        // Crash point 2: FS done, DB path not updated — pending drives completion
        let dir = unique_temp_dir("pending_post_fs");
        let src = dir.join("ABC-100.mp4");
        let payload = b"moved-via-pending";
        fs::write(&src, payload).unwrap();
        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), payload.len() as u64, None)
            .expect("insert");
        let dest = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::rename(&src, &dest).unwrap();
        let dest_id = path_template::read_fs_file_id(&dest).unwrap().expect("dest id");
        db.set_pending_move_to(file_id, dest.to_str().unwrap(), Some(dest_id))
            .expect("pending");
        // Pending + size/phash both required (B1); fs id matches after same-volume rename
        let arc = Arc::new(Mutex::new(db));
        let n = recover_pending_moves(&arc).expect("recover");
        assert_eq!(n, 1);
        let guard = arc.lock().unwrap();
        assert_eq!(guard.get_pending_move_to(file_id).unwrap(), None);
        assert_eq!(Path::new(&guard.get_file_by_id(file_id).unwrap().path), dest.as_path());
        assert_eq!(fs::read(&dest).unwrap(), payload);
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_move_absent_after_db_commit_is_noop() {
        // Crash point 3: commit already cleared pending — recovery finds nothing
        let dir = unique_temp_dir("pending_post_commit");
        let dest = dir.join("done.mp4");
        fs::write(&dest, b"x").unwrap();
        let db = open_db();
        let file_id = db
            .insert_file(dest.to_str().unwrap(), 1, None)
            .expect("insert");
        assert_eq!(db.get_pending_move_to(file_id).unwrap(), None);
        let arc = Arc::new(Mutex::new(db));
        let n = recover_pending_moves(&arc).expect("recover");
        assert_eq!(n, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_move_recovery_refuses_size_mismatch() {
        // B1 deterministic: recover_pending_moves must refuse when dest size ≠ DB size.
        // (Unlocked growing_file alone does not prove this recovery path.)
        let dir = unique_temp_dir("pending_size_refuse");
        let src = dir.join("incoming.mp4");
        let dest = dir.join("StudioA").join("Demo.mp4");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&src, b"recorded").unwrap(); // 8 bytes in DB
        fs::write(&dest, b"DIFFERENT-SIZE-PAYLOAD").unwrap();
        let dest_id = path_template::read_fs_file_id(&dest).unwrap().expect("dest id");
        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), 8, None)
            .expect("insert");
        fs::remove_file(&src).unwrap();
        db.set_pending_move_to(file_id, dest.to_str().unwrap(), Some(dest_id))
            .expect("pending");
        let arc = Arc::new(Mutex::new(db));
        let n = recover_pending_moves(&arc).expect("recover");
        assert_eq!(n, 0, "must not commit when size mismatches");
        let guard = arc.lock().unwrap();
        assert!(
            guard.get_pending_move_to(file_id).unwrap().is_some(),
            "pending kept on size refuse"
        );
        assert_eq!(
            Path::new(&guard.get_file_by_id(file_id).unwrap().path),
            src.as_path(),
            "path unchanged on refuse"
        );
        assert!(
            guard.list_error_logs(20).unwrap().iter().any(|e| {
                e.message.contains("size/phash") || e.message.contains("keeping pending")
            }),
            "ErrorLog on size mismatch refuse"
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_heals_via_pending_when_identity_would_fail() {
        // B1: pending matches but size differs → refuse and keep pending
        let dir = unique_temp_dir("pending_over_identity");
        let src = dir.join("ABC-100.mp4");
        fs::write(&src, b"original").unwrap();
        let db = open_db();
        let file_id = db.insert_file(src.to_str().unwrap(), 8, None).expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: Some("2024-06-01".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .unwrap();
        let settings = AppSettings {
            output_template: "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let desired = dir.join("StudioA").join("2024-06-01_Demo.mp4");
        fs::create_dir_all(desired.parent().unwrap()).unwrap();
        // Dest size != DB size
        fs::write(&desired, b"DIFFERENT-SIZE-PAYLOAD").unwrap();
        let desired_id = path_template::read_fs_file_id(&desired).unwrap().expect("id");
        fs::remove_file(&src).unwrap();
        db.set_pending_move_to(file_id, desired.to_str().unwrap(), Some(desired_id))
            .expect("pending");
        let arc = Arc::new(Mutex::new(db));
        let err = step_move(&arc, file_id, &settings, None).expect_err("must refuse");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("pending") || msg.contains("does not match") || msg.contains("refused"),
            "expected pending+identity refuse got {err}"
        );
        let guard = arc.lock().unwrap();
        assert!(
            guard.get_pending_move_to(file_id).unwrap().is_some(),
            "pending must be kept when size/phash disagree"
        );
        assert_eq!(
            Path::new(&guard.get_file_by_id(file_id).unwrap().path),
            src.as_path(),
            "path must not be updated on refuse"
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_move_refuses_exact_duplicate_different_fs_id() {
        // S3: identical content at dest but different inode → refuse, keep pending
        let dir = unique_temp_dir("pending_exact_dup_fs");
        let src = dir.join("incoming.mp4");
        let dest = dir.join("StudioA").join("Demo.mp4");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let same = b"identical-payload-bytes";
        fs::write(&src, same).unwrap();
        fs::write(&dest, same).unwrap(); // different file, same bytes
        let src_id = path_template::read_fs_file_id(&src).unwrap().expect("src");
        let dest_id = path_template::read_fs_file_id(&dest).unwrap().expect("dest");
        assert_ne!(src_id, dest_id, "precondition: distinct files");
        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), same.len() as u64, None)
            .expect("insert");
        fs::remove_file(&src).unwrap(); // simulate source gone without move
        db.set_pending_move_to(file_id, dest.to_str().unwrap(), Some(src_id))
            .expect("pending");
        let arc = Arc::new(Mutex::new(db));
        let n = recover_pending_moves(&arc).expect("recover");
        assert_eq!(n, 0, "must not complete onto a different file");
        let guard = arc.lock().unwrap();
        assert!(guard.get_pending_move_to(file_id).unwrap().is_some());
        assert!(
            guard.list_error_logs(20).unwrap().iter().any(|e| {
                e.message.contains("fs id") || e.message.contains("keeping pending")
            }),
            "ErrorLog on fs id mismatch"
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failure_isolation_scrape_failure_does_not_block_move_attempt() {
        // Documented ordering scrape then move then preview even when scrape fails
        // Move still runs with filename fallbacks
        let dir = unique_temp_dir("iso");
        let src = dir.join("XYZ-777.mp4");
        fs::write(&src, b"x").unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), 1, None)
            .unwrap();
        // Simulate scrape_failed with no metadata
        db.update_file_status(file_id, file_status::scrape_failed)
            .unwrap();

        let settings = AppSettings {
            output_template: "organized/{{number}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let arc = Arc::new(Mutex::new(db));
        let outcome = step_move(&arc, file_id, &settings, None).expect("move with fallbacks");
        let MoveStepOutcome::Moved(moved) = outcome else {
            panic!("expected Moved got {outcome:?}");
        };
        assert!(moved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("XYZ-777"));
        assert!(moved.to_string_lossy().contains("organized"));
        let guard = arc.lock().unwrap();
        assert_eq!(
            guard.get_file_by_id(file_id).unwrap().status,
            file_status::scrape_failed
        );
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn template_context_drives_render_used_by_move() {
        let ctx = TemplateContext::from_metadata(
            Path::new("/watch/ABC-1.mp4"),
            Some("Stu"),
            Some("2021-01-01"),
            Some("Title"),
            Some("Actor"),
        );
        let dest = render_output_path(
            "{{studio}}/{{actor}}/{{number}}_{{title}}.{{ext}}",
            &ctx,
            Path::new("/library"),
        )
        .unwrap();
        assert_eq!(
            dest,
            PathBuf::from("/library/Stu/Actor/ABC-1_Title.mp4")
        );
    }

    #[test]
    fn move_under_watch_suppresses_destination_and_skips_rescrape() {
        // Regression: relative template landing inside the watch tree must not
        // re-run scrape (which could wipe ready → scrape_failed)
        let watch = unique_temp_dir("watch_loop");
        let src = watch.join("ABC-200.mp4");
        fs::write(&src, b"video").unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(src.to_str().unwrap(), 5, None)
            .unwrap();
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Kept".to_string()),
            studio: Some("StudioLoop".to_string()),
            release_date: Some("2024-02-02".to_string()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: Some("/tmp/sprite.vtt".to_string()),
            source_type: Some("scraped_avdc".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let suppressions = PathSuppression::new(std::time::Duration::from_secs(60));
        let arc = Arc::new(Mutex::new(db));

        let outcome = step_move(&arc, file_id, &settings, Some(&suppressions)).expect("move");
        let MoveStepOutcome::Moved(moved) = outcome else {
            panic!("expected Moved got {outcome:?}");
        };
        assert!(moved.starts_with(&watch));
        assert!(suppressions.is_suppressed(&moved));

        let guard = arc.lock().unwrap();
        assert!(is_already_scraped(&guard, file_id).unwrap());
        let softened = options_for_existing_file(
            &guard,
            file_id,
            &PipelineOptions::default(),
            Some(&settings),
        )
        .unwrap();
        assert!(
            !softened.enable_scrape,
            "re-ingest must not re-scrape organized file"
        );
        assert!(
            !softened.enable_preview,
            "sprite already present so preview skipped"
        );
        assert!(
            !softened.enable_move,
            "already at template destination so move skipped"
        );
        assert_eq!(
            guard.get_file_by_id(file_id).unwrap().status,
            file_status::ready
        );
        drop(guard);
        let _ = fs::remove_dir_all(&watch);
    }

    #[test]
    fn options_for_existing_file_keeps_scrape_failed_from_rescrape() {
        let db = open_db();
        let file_id = db
            .insert_file("/tmp/idam-already-failed.mp4", 1, None)
            .unwrap();
        db.update_file_status(file_id, file_status::scrape_failed)
            .unwrap();
        let softened =
            options_for_existing_file(&db, file_id, &PipelineOptions::default(), None).unwrap();
        assert!(!softened.enable_scrape);
        assert_eq!(
            db.get_file_by_id(file_id).unwrap().status,
            file_status::scrape_failed
        );
    }

    #[test]
    fn options_disables_move_when_already_at_destination() {
        let dir = unique_temp_dir("already_dest");
        let studio = dir.join("StudioX");
        fs::create_dir_all(&studio).unwrap();
        let dest = studio.join("Title.mp4");
        fs::write(&dest, b"data").unwrap();

        let db = open_db();
        let file_id = db
            .insert_file(dest.to_str().unwrap(), 4, None)
            .unwrap();
        db.update_metadata(&Metadata {
            file_id,
            title: Some("Title".to_string()),
            studio: Some("StudioX".to_string()),
            release_date: None,
            summary: None,
            cover_image: None,
            sprite_sheet_path: Some("/tmp/s.vtt".to_string()),
            source_type: Some("scraped_avdc".to_string()),
        })
        .unwrap();

        let settings = AppSettings {
            output_template: "{{studio}}/{{title}}.{{ext}}".to_string(),
            ..AppSettings::default()
        };
        let softened =
            options_for_existing_file(&db, file_id, &PipelineOptions::default(), Some(&settings))
                .unwrap();
        assert!(softened.is_noop(), "fully organized file must be noop");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_move_db_update_failure_rolls_file_back() {
        let dir = unique_temp_dir("db_rb");
        let src = dir.join("keep.mp4");
        fs::write(&src, b"data").unwrap();
        let dest = dir.join("StudioX").join("Title.mp4");

        // Perform move then rollback as step_move does on DB failure
        path_template::move_file(&src, &dest).unwrap();
        assert!(!src.exists());
        rollback_move(&dest, &src).unwrap();
        assert!(src.exists());
        assert!(!dest.exists());
        assert_eq!(fs::read(&src).unwrap(), b"data");
        let _ = fs::remove_dir_all(&dir);
    }
}
