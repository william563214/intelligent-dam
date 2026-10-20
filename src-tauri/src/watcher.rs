//! Background directory Watcher using the `notify` crate.
//!
//! # Events handled
//! - **Created** (`EventKind::Create`): new file appears under a watched tree
//! - **Moved / renamed in** (`EventKind::Modify(Name(To|Both|Any))`): file
//!   lands in a watched tree via rename or move
//!
//! # Events ignored (by design for this issue)
//! - `Remove` / rename-from: left for later missing reconciliation
//! - `Modify(Data)` / `Access`: size growth is handled by the stability gate
//!   after Create/Moved, not as a separate ingest trigger
//! - Non-video extensions: filtered before ingest
//!
//! New video paths are enqueued on an internal channel. The worker waits for
//! write stability (no DB lock), then registers the file (short lock), emits
//! `watcher-file-detected`, and submits phash / pipeline work to the shared
//! [`TaskQueue`] (bounded concurrency, same-file serial). Destinations produced
//! by the pipeline are suppressed so recursive watch trees do not re-ingest them;
//! already-scraped library rows skip scrape on any residual events.
//! Path UNIQUE upsert + phash near-dedup (issue #11) prevent duplicate Files rows.

use crate::database::Database;
use crate::file_stability::{self, StabilityError, StabilityOptions};
use crate::path_suppression::PathSuppression;
use crate::pipeline::{self, PipelineOptions};
use crate::scanner::{self, ScannedFile};
use crate::task_queue::TaskQueue;
use anyhow::{Context, Result};
use notify::event::{CreateKind, Event, EventKind, ModifyKind, RenameMode};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

#[allow(non_upper_case_globals)]
const watcher_event_name: &str = "watcher-file-detected";

#[allow(non_upper_case_globals)]
const default_debounce_window: Duration = Duration::from_millis(750);

#[allow(non_upper_case_globals)]
const debounce_capacity: usize = 256;

/// Tunables for ingest (stability gate + path debounce). Production uses [`Default`].
#[derive(Debug, Clone)]
pub struct WatcherOptions {
    pub stability: StabilityOptions,
    pub debounce_window: Duration,
    /// When set skips `PipelineOptions::from_env` and AppHandle settings load
    pub pipeline_override: Option<PipelineOptions>,
}

impl Default for WatcherOptions {
    fn default() -> Self {
        Self {
            // Env-overridable stability tunables (IDAM_STABILITY_*)
            stability: StabilityOptions::from_env(),
            debounce_window: default_debounce_window,
            pipeline_override: None,
        }
    }
}

impl WatcherOptions {
    /// Fast debounce + short stability for notify e2e tests
    #[cfg(feature = "test-support")]
    pub fn for_tests() -> Self {
        Self {
            stability: StabilityOptions::for_watcher_e2e(),
            debounce_window: Duration::from_millis(80),
            pipeline_override: Some(PipelineOptions {
                enable_scrape: false,
                enable_move: false,
                enable_preview: false,
            }),
        }
    }
}

/// Payload emitted to the frontend when a new video is detected and registered.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatcherFileEvent {
    pub path: String,
    pub kind: String,
    pub file_id: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatcherStatus {
    pub running: bool,
    pub watched_paths: Vec<String>,
}

#[derive(Debug, Clone)]
struct DetectedPath {
    path: PathBuf,
    kind: &'static str,
}

/// Resolve configured scan directories into watchable absolute directory paths.
/// Empty strings are dropped; missing paths are skipped; duplicates collapsed.
pub fn resolve_watch_paths(raw: &[String]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = PathBuf::from(trimmed);
        if !path.is_dir() {
            eprintln!(
                "watcher: skip non-directory or missing path {}",
                path.display()
            );
            continue;
        }
        let key = canonicalize_key(&path);
        if !seen.insert(key) {
            continue;
        }
        out.push(path);
    }

    out
}

fn canonicalize_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Map a notify event kind to an ingest label, if we should act on it.
pub fn classify_event_kind(kind: &EventKind) -> Option<&'static str> {
    match kind {
        EventKind::Create(CreateKind::File) | EventKind::Create(CreateKind::Any) => {
            Some("created")
        }
        // Some backends report directory create when a file lands; still inspect paths
        EventKind::Create(CreateKind::Folder) => None,
        EventKind::Create(_) => Some("created"),
        EventKind::Modify(ModifyKind::Name(RenameMode::To))
        | EventKind::Modify(ModifyKind::Name(RenameMode::Both))
        | EventKind::Modify(ModifyKind::Name(RenameMode::Any)) => Some("moved"),
        // Rename from / other modify kinds are not ingest triggers here
        _ => None,
    }
}

/// Extract video paths we care about from a notify event.
pub fn extract_detected_paths(event: &Event) -> Vec<(PathBuf, &'static str)> {
    let Some(kind_label) = classify_event_kind(&event.kind) else {
        return Vec::new();
    };

    let paths: Vec<&PathBuf> = match &event.kind {
        // Both: notify typically puts [from, to]; prefer the destination
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if event.paths.len() >= 2 => {
            vec![event.paths.last().unwrap()]
        }
        _ => event.paths.iter().collect(),
    };

    paths
        .into_iter()
        .filter(|p| scanner::is_video_path(p))
        // Keep if not yet visible or is a regular file (drop directories)
        .filter(|p| !p.exists() || p.is_file())
        .map(|p| (p.clone(), kind_label))
        .collect()
}

struct ActiveWatch {
    _watcher: RecommendedWatcher,
    watched_paths: Vec<String>,
    stop_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

#[allow(non_upper_case_globals)]
const worker_join_timeout: Duration = Duration::from_secs(3);

/// Process-wide watcher controller managed as Tauri state.
pub struct WatcherManager {
    inner: Mutex<Option<ActiveWatch>>,
}

impl Default for WatcherManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WatcherManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    pub fn status(&self) -> WatcherStatus {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(active) => WatcherStatus {
                running: true,
                watched_paths: active.watched_paths.clone(),
            },
            None => WatcherStatus {
                running: false,
                watched_paths: Vec::new(),
            },
        }
    }

    pub fn is_running(&self) -> bool {
        self.status().running
    }

    /// Signal stop drop the notify watcher and detach the ingest worker.
    /// Does not join — call [`join_worker`] off the UI thread.
    pub fn request_stop(&self) -> Result<Option<JoinHandle<()>>> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| anyhow::anyhow!("Watcher lock poisoned: {}", e))?;
        if let Some(mut active) = guard.take() {
            active.stop_flag.store(true, Ordering::SeqCst);
            // Dropping RecommendedWatcher ends the notify callback and closes tx
            drop(active._watcher);
            Ok(active.worker.take())
        } else {
            Ok(None)
        }
    }

    /// Join a detached ingest worker with a timeout (never blocks forever).
    pub fn join_worker(handle: JoinHandle<()>, timeout: Duration) {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = handle.join();
            let _ = done_tx.send(());
        });
        match done_rx.recv_timeout(timeout) {
            Ok(()) => {}
            Err(_) => {
                eprintln!(
                    "watcher: ingest worker join timed out after {:?}; left detached",
                    timeout
                );
            }
        }
    }

    /// Request stop then join with timeout. Prefer async commands + spawn_blocking
    /// for UI paths; this is for Drop and tests.
    pub fn stop_with_timeout(&self, timeout: Duration) -> Result<()> {
        if let Some(handle) = self.request_stop()? {
            Self::join_worker(handle, timeout);
        }
        Ok(())
    }

    /// Stop monitoring if active. Idempotent. Uses [`worker_join_timeout`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn stop(&self) -> Result<()> {
        self.stop_with_timeout(worker_join_timeout)
    }

    /// Start monitoring `paths`. Caller should have already stopped any prior
    /// session via [`request_stop`] + [`join_worker`] when on the UI path.
    /// If still running this will stop with timeout first.
    pub fn start(
        &self,
        paths: Vec<PathBuf>,
        app: AppHandle,
        db: Arc<Mutex<Database>>,
        queue: Arc<TaskQueue>,
    ) -> Result<WatcherStatus> {
        self.start_with_options(paths, app, db, queue, WatcherOptions::default())
    }

    /// Like [`start`] with injectable stability / debounce (tests keep production defaults via [`Default`]).
    pub fn start_with_options(
        &self,
        paths: Vec<PathBuf>,
        app: AppHandle,
        db: Arc<Mutex<Database>>,
        queue: Arc<TaskQueue>,
        options: WatcherOptions,
    ) -> Result<WatcherStatus> {
        self.spawn_watch(
            paths,
            Some(app),
            db,
            queue,
            options,
            true,
        )
    }

    /// Headless notify→ingest→enqueue for integration tests (no AppHandle / workers).
    #[cfg(feature = "test-support")]
    pub fn start_for_test(
        &self,
        paths: Vec<PathBuf>,
        db: Arc<Mutex<Database>>,
        queue: Arc<TaskQueue>,
        options: WatcherOptions,
    ) -> Result<WatcherStatus> {
        self.spawn_watch(paths, None, db, queue, options, false)
    }

    fn spawn_watch(
        &self,
        paths: Vec<PathBuf>,
        app: Option<AppHandle>,
        db: Arc<Mutex<Database>>,
        queue: Arc<TaskQueue>,
        options: WatcherOptions,
        start_workers: bool,
    ) -> Result<WatcherStatus> {
        if paths.is_empty() {
            anyhow::bail!("No valid watch directories (configure scan directories in settings)");
        }

        self.stop_with_timeout(worker_join_timeout)?;

        let (tx, rx): (Sender<DetectedPath>, Receiver<DetectedPath>) = mpsc::channel();
        let stop_flag = Arc::new(AtomicBool::new(false));

        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                match res {
                    Ok(event) => {
                        for (path, kind) in extract_detected_paths(&event) {
                            let _ = tx.send(DetectedPath { path, kind });
                        }
                    }
                    Err(e) => eprintln!("watcher: notify error: {:#}", e),
                }
            },
            Config::default(),
        )
        .context("Failed to create filesystem watcher")?;

        let mut watched_paths = Vec::new();
        for path in &paths {
            watcher
                .watch(path, RecursiveMode::Recursive)
                .with_context(|| format!("Failed to watch {}", path.display()))?;
            watched_paths.push(path.to_string_lossy().to_string());
        }

        let worker_stop = Arc::clone(&stop_flag);
        let suppressions = Arc::new(PathSuppression::default());
        if start_workers {
            let app_handle = app
                .clone()
                .ok_or_else(|| anyhow::anyhow!("AppHandle required when starting queue workers"))?;
            queue
                .bind_and_start(app_handle, Arc::clone(&db), Arc::clone(&suppressions))
                .map_err(|e| anyhow::anyhow!("Failed to bind task queue: {}", e))?;
        } else {
            queue
                .bind_database(Arc::clone(&db))
                .map_err(|e| anyhow::anyhow!("Failed to bind task queue: {}", e))?;
        }

        let worker_suppressions = Arc::clone(&suppressions);
        let worker_queue = Arc::clone(&queue);
        let worker = thread::Builder::new()
            .name("dam-watcher-ingest".into())
            .spawn(move || {
                ingest_worker(
                    rx,
                    worker_stop,
                    app,
                    db,
                    worker_suppressions,
                    worker_queue,
                    options,
                )
            })
            .context("Failed to spawn watcher ingest worker")?;

        let mut guard = self
            .inner
            .lock()
            .map_err(|e| anyhow::anyhow!("Watcher lock poisoned: {}", e))?;
        *guard = Some(ActiveWatch {
            _watcher: watcher,
            watched_paths: watched_paths.clone(),
            stop_flag,
            worker: Some(worker),
        });

        Ok(WatcherStatus {
            running: true,
            watched_paths,
        })
    }
}

impl Drop for WatcherManager {
    fn drop(&mut self) {
        if let Ok(Some(handle)) = self.request_stop() {
            Self::join_worker(handle, Duration::from_secs(2));
        }
    }
}

fn ingest_worker(
    rx: Receiver<DetectedPath>,
    stop_flag: Arc<AtomicBool>,
    app: Option<AppHandle>,
    db: Arc<Mutex<Database>>,
    suppressions: Arc<PathSuppression>,
    queue: Arc<TaskQueue>,
    options: WatcherOptions,
) {
    let mut recent: VecDeque<(String, Instant)> = VecDeque::new();

    while !stop_flag.load(Ordering::SeqCst) {
        let detected = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(d) => d,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        let path_key = detected.path.to_string_lossy().to_string();
        if is_duplicate(&mut recent, &path_key, options.debounce_window) {
            continue;
        }

        // Ignore destinations we just moved/created (recursive watch re-entry)
        if suppressions.is_suppressed(&detected.path) {
            eprintln!(
                "watcher: skip self-generated path {}",
                detected.path.display()
            );
            continue;
        }

        // I/O outside DB lock: wait for write stability then confirm size
        let stop_for_wait = Arc::clone(&stop_flag);
        let stability = options.stability.clone();
        let scanned = match prepare_scanned_file(&detected.path, &stability, move || {
            stop_for_wait.load(Ordering::SeqCst)
        }) {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(e) => {
                eprintln!(
                    "watcher: skip ingest for {}: {:#}",
                    detected.path.display(),
                    e
                );
                let _ = emit_event(
                    app.as_ref(),
                    WatcherFileEvent {
                        path: path_key,
                        kind: detected.kind.to_string(),
                        file_id: None,
                        error: Some(e.to_string()),
                    },
                );
                continue;
            }
        };

        // Re-check after stability wait (move may have suppressed meanwhile)
        if suppressions.is_suppressed(&scanned.path) {
            eprintln!(
                "watcher: skip self-generated path after stability {}",
                scanned.path.display()
            );
            continue;
        }

        let path_str = match scanned.path.to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        // Short lock: register only
        let (imported, jobs) = {
            let db_guard = match db.lock() {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("watcher: db lock error: {}", e);
                    continue;
                }
            };
            let max_distance = match app.as_ref() {
                Some(handle) => pipeline::load_settings(handle).effective_near_phash_max_distance(),
                None => crate::phash::resolve_near_phash_max_distance(None),
            };
            match scanner::import_files_pending_phash(
                &db_guard,
                &[scanned],
                Some(&suppressions),
                max_distance,
            ) {
                Ok((entries, jobs)) => (entries.into_iter().next(), jobs),
                Err(e) => {
                    eprintln!("watcher: import failed for {}: {:#}", path_str, e);
                    let _ = emit_event(
                        app.as_ref(),
                        WatcherFileEvent {
                            path: path_str.clone(),
                            kind: detected.kind.to_string(),
                            file_id: None,
                            error: Some(e.to_string()),
                        },
                    );
                    continue;
                }
            }
        };

        // Exact phash dedup may have removed the provisional row during import
        let Some(entry) = imported else {
            eprintln!(
                "watcher: skip ingest for {} (exact phash duplicate of existing row)",
                path_str
            );
            let _ = emit_event(
                app.as_ref(),
                WatcherFileEvent {
                    path: path_str,
                    kind: detected.kind.to_string(),
                    file_id: None,
                    error: Some("skipped: exact phash duplicate".to_string()),
                },
            );
            continue;
        };

        // Suspected near-phash / path collision: keep row but do not auto-enqueue pipeline
        let blocks_pipeline = {
            match db.lock() {
                Ok(g) => g
                    .get_file_by_id(entry.file_id)
                    .map(|f| crate::file_status::blocks_auto_pipeline(&f.status))
                    .unwrap_or(false),
                Err(_) => false,
            }
        };

        let file_id = entry.file_id;
        let is_new = entry.is_new;

        let _ = emit_event(
            app.as_ref(),
            WatcherFileEvent {
                path: path_str,
                kind: detected.kind.to_string(),
                file_id: Some(file_id),
                error: None,
            },
        );

        // Path already known and no phash work: do not re-enqueue pipeline
        // (documented: known path with no phash job -> don't enqueue)
        if !is_new && jobs.is_empty() {
            eprintln!(
                "watcher: path dedup skip queue for existing file_id {}",
                file_id
            );
            continue;
        }

        // Submit phash then pipeline to the shared queue (same-file serial FIFO)
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        for job in jobs {
            match queue.enqueue_phash(job, Some(Arc::clone(&stop_flag))) {
                Ok(job_id) => {
                    eprintln!("watcher: enqueued phash job {} for file {}", job_id, file_id);
                }
                Err(e) => {
                    eprintln!("watcher: failed to enqueue phash for file {}: {}", file_id, e);
                }
            }
        }

        if blocks_pipeline {
            eprintln!(
                "watcher: skip pipeline enqueue for blocked status file_id {}",
                file_id
            );
            continue;
        }

        let base_options = options
            .pipeline_override
            .clone()
            .unwrap_or_else(PipelineOptions::from_env);
        if base_options.is_noop() {
            continue;
        }

        // Soften options when this path is an already-organized library row
        // (e.g. move landed under watch and suppress window missed)
        let Some(app_handle) = app.as_ref() else {
            // Headless tests: enqueue pipeline with override as-is (no settings soften)
            match queue.enqueue_pipeline(file_id, base_options, Some(Arc::clone(&stop_flag))) {
                Ok(job_id) => {
                    eprintln!(
                        "watcher: enqueued pipeline job {} for file {} (headless)",
                        job_id, file_id
                    );
                }
                Err(e) => {
                    eprintln!(
                        "watcher: failed to enqueue pipeline for file {}: {}",
                        file_id, e
                    );
                }
            }
            continue;
        };
        let settings = pipeline::load_settings(app_handle);
        let pipeline_opts = {
            let db_guard = match db.lock() {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("watcher: db lock error before pipeline enqueue: {}", e);
                    continue;
                }
            };
            match pipeline::options_for_existing_file(
                &db_guard,
                file_id,
                &base_options,
                Some(&settings),
            ) {
                Ok(opts) => opts,
                Err(e) => {
                    eprintln!(
                        "watcher: options_for_existing_file failed for {}: {:#}",
                        file_id, e
                    );
                    base_options
                }
            }
        };

        if pipeline_opts.is_noop() {
            eprintln!(
                "watcher: skip pipeline enqueue for already-organized file {}",
                file_id
            );
            continue;
        }

        match queue.has_active_pipeline(file_id) {
            Ok(true) => {
                eprintln!(
                    "watcher: skip pipeline enqueue file {} already queued/running",
                    file_id
                );
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("watcher: has_active_pipeline check failed: {}", e);
            }
        }

        match queue.enqueue_pipeline(file_id, pipeline_opts, Some(Arc::clone(&stop_flag))) {
            Ok(job_id) => {
                eprintln!(
                    "watcher: enqueued pipeline job {} for file {}",
                    job_id, file_id
                );
            }
            Err(e) => {
                eprintln!(
                    "watcher: failed to enqueue pipeline for file {}: {}",
                    file_id, e
                );
            }
        }
    }
}

fn prepare_scanned_file<F>(
    path: &Path,
    stability: &StabilityOptions,
    should_abort: F,
) -> Result<Option<ScannedFile>>
where
    F: FnMut() -> bool,
{
    if !scanner::is_video_path(path) {
        return Ok(None);
    }

    let size = match file_stability::wait_until_stable(path, stability, should_abort) {
        Ok(size) => size,
        Err(StabilityError::Aborted { .. }) => return Ok(None),
        Err(StabilityError::Missing { .. }) => return Ok(None),
        Err(e) => {
            // TimedOut / Io: surface to caller for UI error emit; do not touch status
            return Err(e.into());
        }
    };

    if size == 0 {
        return Ok(None);
    }

    Ok(Some(ScannedFile {
        path: path.to_path_buf(),
        size,
        phash: None,
    }))
}

fn is_duplicate(
    recent: &mut VecDeque<(String, Instant)>,
    path_key: &str,
    debounce_window: Duration,
) -> bool {
    let now = Instant::now();
    while let Some((_, at)) = recent.front() {
        if now.duration_since(*at) > debounce_window {
            recent.pop_front();
        } else {
            break;
        }
    }
    if recent.iter().any(|(p, _)| p == path_key) {
        return true;
    }
    recent.push_back((path_key.to_string(), now));
    while recent.len() > debounce_capacity {
        recent.pop_front();
    }
    false
}

fn emit_event(app: Option<&AppHandle>, payload: WatcherFileEvent) -> Result<()> {
    let Some(app) = app else {
        return Ok(());
    };
    app.emit(watcher_event_name, payload)
        .context("Failed to emit watcher-file-detected")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::CreateKind;
    use std::fs;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "idam_watcher_{}_{}_{}",
            label,
            std::process::id(),
            nanos
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_watch_paths_filters_empty_missing_and_dupes() {
        let dir_a = unique_temp_dir("a");
        let dir_b = unique_temp_dir("b");
        let missing = dir_a.join("nope-missing");

        let raw = vec![
            String::new(),
            "   ".to_string(),
            dir_a.to_string_lossy().to_string(),
            dir_a.to_string_lossy().to_string(),
            missing.to_string_lossy().to_string(),
            dir_b.to_string_lossy().to_string(),
            "/tmp/this-path-should-not-exist-idam-watcher-xyz".to_string(),
        ];

        let resolved = resolve_watch_paths(&raw);
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().any(|p| p == &dir_a));
        assert!(resolved.iter().any(|p| p == &dir_b));

        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn classify_handles_create_and_moved() {
        assert_eq!(
            classify_event_kind(&EventKind::Create(CreateKind::File)),
            Some("created")
        );
        assert_eq!(
            classify_event_kind(&EventKind::Modify(ModifyKind::Name(RenameMode::To))),
            Some("moved")
        );
        assert_eq!(
            classify_event_kind(&EventKind::Modify(ModifyKind::Name(RenameMode::Both))),
            Some("moved")
        );
        assert_eq!(
            classify_event_kind(&EventKind::Remove(notify::event::RemoveKind::File)),
            None
        );
        assert_eq!(
            classify_event_kind(&EventKind::Modify(ModifyKind::Data(
                notify::event::DataChange::Any
            ))),
            None
        );
    }

    #[test]
    fn extract_detected_paths_filters_non_video() {
        let dir = unique_temp_dir("extract");
        let video = dir.join("clip.mp4");
        let text = dir.join("notes.txt");
        fs::write(&video, b"fake").unwrap();
        fs::write(&text, b"x").unwrap();

        let event = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![video.clone(), text.clone()],
            attrs: Default::default(),
        };
        let found = extract_detected_paths(&event);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, video);
        assert_eq!(found[0].1, "created");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_rename_both_uses_destination() {
        let dir = unique_temp_dir("rename");
        let from = dir.join("old.mp4");
        let to = dir.join("new.mp4");
        fs::write(&to, b"fake").unwrap();

        let event = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![from, to.clone()],
            attrs: Default::default(),
        };
        let found = extract_detected_paths(&event);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, to);
        assert_eq!(found[0].1, "moved");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn watcher_manager_stop_when_idle_is_ok() {
        let mgr = WatcherManager::new();
        assert!(!mgr.is_running());
        mgr.stop().expect("stop idle");
        assert!(!mgr.status().running);
        assert!(mgr.status().watched_paths.is_empty());
    }

    #[test]
    fn request_stop_then_join_with_timeout_is_fast_when_idle() {
        let mgr = WatcherManager::new();
        let handle = mgr.request_stop().expect("request");
        assert!(handle.is_none());
        let started = Instant::now();
        mgr.stop_with_timeout(Duration::from_millis(500))
            .expect("stop");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn is_duplicate_debounces_same_path() {
        let mut recent = VecDeque::new();
        let window = default_debounce_window;
        assert!(!is_duplicate(&mut recent, "/a/b.mp4", window));
        assert!(is_duplicate(&mut recent, "/a/b.mp4", window));
        assert!(!is_duplicate(&mut recent, "/a/c.mp4", window));
    }
}
