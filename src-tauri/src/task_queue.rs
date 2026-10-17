//! Background task queue with bounded concurrency and per-file serial ordering.
//!
//! # Policy (documented choice)
//! - **Concurrency**: default `2` workers (env `IDAM_QUEUE_CONCURRENCY`, clamped ≥1).
//!   Small parallel pool keeps scrape/ffmpeg from stampeding while still overlapping
//!   I/O across different files.
//! - **Same-file serial**: at most one running job per `file_id`. Pending jobs for
//!   that file stay queued until the active one finishes (FIFO). This preserves
//!   phash → pipeline ordering when both are enqueued for the same ingest.
//! - **Dispatch**: each worker wake takes **exactly one** job via [`Scheduler::dispatch_one`].
//!   Jobs are marked `Running` only when a worker actually claims them.
//! - **Failures**: jobs are marked `Failed` with an error string and `retryable`
//!   flag; errors are never swallowed. Auto-retry requeues while attempts remain
//!   then marks failed + ErrorLog. Manual [`TaskQueue::retry`] resets the attempt
//!   count so operators can try again after exhaustion. Abort / stop_flag →
//!   `Cancelled`, not retry.
//! - Pipeline still honors scrape_failed / PathSuppression / stop_flag from #19/#23.
//!
//! # `completed_with_errors` vs queue Failed / retry
//! Pipeline step events may end with UI status `completed_with_errors` (chain ran
//! to the end but some steps pushed into `PipelineResult.errors`, e.g. scrape
//! failed then move/preview still attempted). The queue maps that to job state
//! **`Failed` + `retryable`** so the operator can retry without inventing success.
//! On each Pipeline run (including retry) options are re-softened from DB via
//! [`pipeline::options_for_existing_file`] so already-scraped / already-previewed
//! work is skipped (avoids wiping scrape_failed or re-running a finished scrape).
//!
//! # Durability (#34)
//! Jobs are persisted in the SQLite `Jobs` table. Enqueue and every status
//! change are written to DB **before** the matching `task-queue-progress` event
//! is emitted. On startup recovery (after DB init, before workers start), rows
//! with status `running` are reset to `queued` and reloaded. Issue wording
//! "pending" maps to the in-memory / DB status `queued` (no translation layer).
//! Terminal history is pruned in both memory and DB (`max_terminal_jobs`).
//! Malformed payloads load as `failed` + ErrorLog rather than panicking.

use crate::database::Database;
use crate::path_suppression::PathSuppression;
use crate::pipeline::{self, PipelineOptions};
use crate::scanner::{self, PhashJob};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

#[allow(non_upper_case_globals)]
const queue_event_name: &str = "task-queue-progress";

#[allow(non_upper_case_globals)]
const env_concurrency: &str = "IDAM_QUEUE_CONCURRENCY";

#[allow(non_upper_case_globals)]
const env_max_attempts: &str = "IDAM_QUEUE_MAX_ATTEMPTS";

#[allow(non_upper_case_globals)]
const default_concurrency: usize = 2;

#[allow(non_upper_case_globals)]
const default_max_attempts: u32 = 3;

#[allow(non_upper_case_globals)]
const error_log_category_queue: &str = "queue";

/// Cap retained terminal jobs (succeeded / failed / cancelled) to bound memory
#[allow(non_upper_case_globals)]
pub const max_terminal_jobs: usize = 200;

/// What the queue can run
#[derive(Debug, Clone)]
pub enum JobKind {
    /// Full scrape → move → preview chain
    Pipeline {
        file_id: i64,
        options: PipelineOptions,
    },
    /// Compute and write perceptual hash
    Phash { file_id: i64, path: PathBuf },
}

impl JobKind {
    pub fn file_id(&self) -> i64 {
        match self {
            JobKind::Pipeline { file_id, .. } => *file_id,
            JobKind::Phash { file_id, .. } => *file_id,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            JobKind::Pipeline { .. } => "pipeline",
            JobKind::Phash { .. } => "phash",
        }
    }

    fn to_payload_json(&self) -> Result<String, String> {
        match self {
            JobKind::Pipeline { file_id, options } => {
                let body = PipelineJobPayload {
                    file_id: *file_id,
                    options: options.clone(),
                };
                serde_json::to_string(&body).map_err(|e| format!("serialize pipeline payload: {e}"))
            }
            JobKind::Phash { file_id, path } => {
                let body = PhashJobPayload {
                    file_id: *file_id,
                    path: path.to_string_lossy().into_owned(),
                };
                serde_json::to_string(&body).map_err(|e| format!("serialize phash payload: {e}"))
            }
        }
    }

    fn from_db(job_type: &str, payload: &str) -> Result<Self, String> {
        match job_type {
            "pipeline" => {
                let body: PipelineJobPayload = serde_json::from_str(payload)
                    .map_err(|e| format!("invalid pipeline payload: {e}"))?;
                Ok(JobKind::Pipeline {
                    file_id: body.file_id,
                    options: body.options,
                })
            }
            "phash" => {
                let body: PhashJobPayload = serde_json::from_str(payload)
                    .map_err(|e| format!("invalid phash payload: {e}"))?;
                Ok(JobKind::Phash {
                    file_id: body.file_id,
                    path: PathBuf::from(body.path),
                })
            }
            other => Err(format!("unknown job type '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PipelineJobPayload {
    file_id: i64,
    options: PipelineOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PhashJobPayload {
    file_id: i64,
    path: String,
}

fn parse_job_state(status: &str) -> Result<JobState, String> {
    match status {
        "queued" => Ok(JobState::Queued),
        "running" => Ok(JobState::Running),
        "succeeded" => Ok(JobState::Succeeded),
        "failed" => Ok(JobState::Failed),
        "cancelled" => Ok(JobState::Cancelled),
        other => Err(format!("unknown job status '{other}'")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobRecord {
    pub id: u64,
    pub kind: JobKind,
    pub state: JobState,
    pub error: Option<String>,
    pub retryable: bool,
    pub attempts: u32,
    pub max_attempts: u32,
    /// Watcher session stop; when set, pipeline/phash abort cooperatively
    pub stop_flag: Option<Arc<AtomicBool>>,
}

/// Frontend / observer payload (extends progress surface alongside pipeline events)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskQueueProgressEvent {
    pub job_id: u64,
    pub file_id: i64,
    pub kind: String,
    pub state: String,
    pub attempt: u32,
    pub message: Option<String>,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskQueueStatus {
    pub concurrency: usize,
    pub pending: usize,
    pub running: usize,
    pub failed: usize,
    pub succeeded: usize,
    pub jobs: Vec<TaskQueueJobView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskQueueJobView {
    pub job_id: u64,
    pub file_id: i64,
    pub kind: String,
    pub state: String,
    pub attempt: u32,
    pub max_attempts: u32,
    pub error: Option<String>,
    pub retryable: bool,
}

/// Pure scheduler: concurrency limit + same-file serial + failure bookkeeping
#[derive(Debug)]
pub struct Scheduler {
    concurrency: usize,
    next_id: u64,
    /// FIFO of job ids waiting to run
    pending: VecDeque<u64>,
    jobs: HashMap<u64, JobRecord>,
    running: HashSet<u64>,
    /// file_ids that currently have a running job
    running_files: HashSet<i64>,
}

impl Scheduler {
    pub fn new(concurrency: usize) -> Self {
        Self {
            concurrency: concurrency.max(1),
            next_id: 1,
            pending: VecDeque::new(),
            jobs: HashMap::new(),
            running: HashSet::new(),
            running_files: HashSet::new(),
        }
    }

    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    #[cfg(test)]
    pub fn enqueue(
        &mut self,
        kind: JobKind,
        stop_flag: Option<Arc<AtomicBool>>,
        max_attempts: u32,
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.ingest_record(JobRecord {
            id,
            kind,
            state: JobState::Queued,
            error: None,
            retryable: true,
            attempts: 0,
            max_attempts: max_attempts.max(1),
            stop_flag,
        });
        id
    }

    /// Load or adopt a job with a durable id (from SQLite `Jobs.id`)
    pub fn ingest_record(&mut self, record: JobRecord) {
        let id = record.id;
        self.next_id = self.next_id.max(id.saturating_add(1));
        if record.state == JobState::Queued {
            if !self.pending.iter().any(|p| *p == id) {
                self.pending.push_back(id);
            }
        } else if record.state == JobState::Running {
            self.running.insert(id);
            self.running_files.insert(record.kind.file_id());
        }
        self.jobs.insert(id, record);
    }

    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    #[cfg(test)]
    pub fn running_len(&self) -> usize {
        self.running.len()
    }

    pub fn get(&self, id: u64) -> Option<&JobRecord> {
        self.jobs.get(&id)
    }

    #[cfg(test)]
    pub fn failed_ids(&self) -> Vec<u64> {
        self.jobs
            .values()
            .filter(|j| j.state == JobState::Failed)
            .map(|j| j.id)
            .collect()
    }

    /// True when a pipeline job for this file is already Queued or Running
    pub fn has_active_pipeline(&self, file_id: i64) -> bool {
        self.jobs.values().any(|j| {
            matches!(
                &j.kind,
                JobKind::Pipeline {
                    file_id: fid,
                    ..
                } if *fid == file_id
            ) && (j.state == JobState::Queued || j.state == JobState::Running)
        })
    }

    /// Claim **exactly one** eligible job for a worker.
    ///
    /// Marks that job `Running` only when returned. Same-file-blocked jobs stay
    /// `Queued` at the front of the pending list (never orphaned as Running).
    pub fn dispatch_one(&mut self) -> Option<JobRecord> {
        if self.running.len() >= self.concurrency {
            return None;
        }

        let mut skipped = VecDeque::new();
        let mut chosen: Option<JobRecord> = None;

        while let Some(id) = self.pending.pop_front() {
            let Some(job) = self.jobs.get(&id) else {
                continue;
            };
            if job.state != JobState::Queued {
                continue;
            }
            let file_id = job.kind.file_id();
            if self.running_files.contains(&file_id) {
                skipped.push_back(id);
                continue;
            }

            let mut job = self.jobs.remove(&id).expect("job exists");
            job.state = JobState::Running;
            job.attempts = job.attempts.saturating_add(1);
            self.running.insert(id);
            self.running_files.insert(file_id);
            chosen = Some(job.clone());
            self.jobs.insert(id, job);
            break;
        }

        // Preserve relative order of skipped (same-file blocked) at the front
        while let Some(id) = skipped.pop_back() {
            self.pending.push_front(id);
        }

        chosen
    }

    pub fn complete_success(&mut self, id: u64) -> Option<(JobRecord, Vec<u64>)> {
        self.finish(id, JobState::Succeeded, None, false)
    }

    pub fn complete_failed(
        &mut self,
        id: u64,
        error: String,
        retryable: bool,
    ) -> Option<(JobRecord, Vec<u64>)> {
        self.finish(id, JobState::Failed, Some(error), retryable)
    }

    pub fn complete_cancelled(
        &mut self,
        id: u64,
        message: Option<String>,
    ) -> Option<(JobRecord, Vec<u64>)> {
        self.finish(id, JobState::Cancelled, message, false)
    }

    /// After a retryable failure with attempts remaining, return to `queued`
    pub fn requeue_after_failure(&mut self, id: u64, error: String) -> Option<JobRecord> {
        let job = self.jobs.get_mut(&id)?;
        if job.state != JobState::Running {
            return None;
        }
        let file_id = job.kind.file_id();
        job.state = JobState::Queued;
        job.error = Some(error);
        job.retryable = true;
        self.running.remove(&id);
        self.running_files.remove(&file_id);
        self.pending.push_back(id);
        Some(job.clone())
    }

    fn finish(
        &mut self,
        id: u64,
        state: JobState,
        error: Option<String>,
        retryable: bool,
    ) -> Option<(JobRecord, Vec<u64>)> {
        let job = self.jobs.get_mut(&id)?;
        let file_id = job.kind.file_id();
        job.state = state;
        job.error = error;
        job.retryable = retryable;
        self.running.remove(&id);
        self.running_files.remove(&file_id);
        let out = job.clone();
        let pruned = self.prune_terminal_history();
        Some((out, pruned))
    }

    /// Drop oldest terminal jobs when over [`max_terminal_jobs`]; returns removed ids
    fn prune_terminal_history(&mut self) -> Vec<u64> {
        let mut terminal: Vec<u64> = self
            .jobs
            .values()
            .filter(|j| {
                matches!(
                    j.state,
                    JobState::Succeeded | JobState::Failed | JobState::Cancelled
                )
            })
            .map(|j| j.id)
            .collect();
        if terminal.len() <= max_terminal_jobs {
            return Vec::new();
        }
        terminal.sort_unstable();
        let remove_count = terminal.len() - max_terminal_jobs;
        let mut removed = Vec::with_capacity(remove_count);
        for id in terminal.into_iter().take(remove_count) {
            self.jobs.remove(&id);
            removed.push(id);
        }
        removed
    }

    /// Re-queue a failed retryable job, resetting the attempt count for a fresh run
    pub fn retry(&mut self, id: u64) -> Result<&JobRecord, String> {
        let job = self
            .jobs
            .get_mut(&id)
            .ok_or_else(|| format!("Unknown job id {}", id))?;
        if job.state != JobState::Failed {
            return Err(format!(
                "Job {} is {:?} (only Failed can retry)",
                id,
                job.state.as_str()
            ));
        }
        if !job.retryable {
            return Err(format!("Job {} is not retryable", id));
        }
        // Manual retry always gets a fresh attempt budget (auto-retry may have exhausted)
        job.attempts = 0;
        job.state = JobState::Queued;
        job.error = None;
        self.pending.push_back(id);
        Ok(self.jobs.get(&id).expect("job"))
    }

    pub fn status_snapshot(&self) -> TaskQueueStatus {
        let mut pending = 0usize;
        let mut running = 0usize;
        let mut failed = 0usize;
        let mut succeeded = 0usize;
        let mut jobs: Vec<TaskQueueJobView> = self
            .jobs
            .values()
            .map(|j| {
                match j.state {
                    JobState::Queued => pending += 1,
                    JobState::Running => running += 1,
                    JobState::Failed => failed += 1,
                    JobState::Succeeded => succeeded += 1,
                    JobState::Cancelled => {}
                }
                TaskQueueJobView {
                    job_id: j.id,
                    file_id: j.kind.file_id(),
                    kind: j.kind.label().to_string(),
                    state: j.state.as_str().to_string(),
                    attempt: j.attempts,
                    max_attempts: j.max_attempts,
                    error: j.error.clone(),
                    retryable: j.retryable,
                }
            })
            .collect();
        jobs.sort_by_key(|j| j.job_id);
        TaskQueueStatus {
            concurrency: self.concurrency,
            pending,
            running,
            failed,
            succeeded,
            jobs,
        }
    }
}

fn concurrency_from_env() -> usize {
    match std::env::var(env_concurrency) {
        Ok(v) => v
            .trim()
            .parse::<usize>()
            .unwrap_or(default_concurrency)
            .max(1),
        Err(_) => default_concurrency,
    }
}

fn max_attempts_from_env() -> u32 {
    match std::env::var(env_max_attempts) {
        Ok(v) => v
            .trim()
            .parse::<u32>()
            .unwrap_or(default_max_attempts)
            .max(1),
        Err(_) => default_max_attempts,
    }
}

struct QueueInner {
    scheduler: Scheduler,
}

/// App-managed queue with background workers executing jobs
pub struct TaskQueue {
    inner: Arc<Mutex<QueueInner>>,
    wake: Arc<(Mutex<()>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    /// Shared so re-bind (watcher restart) updates workers without respawning
    suppressions: Arc<Mutex<Arc<PathSuppression>>>,
    app: Mutex<Option<AppHandle>>,
    db: Mutex<Option<Arc<Mutex<Database>>>>,
    /// Startup recovery runs once after DB bind, before workers start
    recovered: AtomicBool,
    /// Default max_attempts for newly enqueued jobs (per-queue, not process env in tests)
    max_attempts: u32,
    /// Injected execute failures for tests (`test-support` / arm_fault)
    execute_fault: Arc<AtomicBool>,
    /// Injected scheduler-retry failure after DB write (`test-support`)
    #[cfg(feature = "test-support")]
    retry_apply_fault: Arc<AtomicBool>,
}

impl TaskQueue {
    pub fn new() -> Self {
        Self::with_options(concurrency_from_env(), max_attempts_from_env())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_concurrency(concurrency: usize) -> Self {
        Self::with_options(concurrency, default_max_attempts)
    }

    pub fn with_options(concurrency: usize, max_attempts: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(QueueInner {
                scheduler: Scheduler::new(concurrency),
            })),
            wake: Arc::new((Mutex::new(()), Condvar::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            workers: Mutex::new(Vec::new()),
            suppressions: Arc::new(Mutex::new(Arc::new(PathSuppression::default()))),
            app: Mutex::new(None),
            db: Mutex::new(None),
            recovered: AtomicBool::new(false),
            max_attempts: max_attempts.max(1),
            execute_fault: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "test-support")]
            retry_apply_fault: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Count jobs currently in `queued` state (memory scheduler)
    pub fn queued_job_count(&self) -> Result<usize, String> {
        Ok(self.status()?.pending)
    }

    /// Arm injected execute failures (per-queue; safe for parallel tests)
    #[cfg(feature = "test-support")]
    pub fn arm_fault(&self) {
        self.execute_fault.store(true, Ordering::SeqCst);
    }

    /// Clear injected execute failures
    #[cfg(feature = "test-support")]
    pub fn clear_fault(&self) {
        self.execute_fault.store(false, Ordering::SeqCst);
    }

    /// Arm injected scheduler-retry failure after a successful DB write
    #[cfg(feature = "test-support")]
    pub fn arm_retry_apply_fault(&self) {
        self.retry_apply_fault.store(true, Ordering::SeqCst);
    }

    /// Shared PathSuppression used by Watcher moves and phash near-dedup orphans.
    pub fn path_suppressions(&self) -> Result<Arc<PathSuppression>, String> {
        self.suppressions
            .lock()
            .map(|g| Arc::clone(&*g))
            .map_err(|e| format!("queue suppressions lock: {}", e))
    }

    /// Bind the shared DB without starting workers (startup recovery / integration tests).
    pub fn bind_database(&self, db: Arc<Mutex<Database>>) -> Result<(), String> {
        *self
            .db
            .lock()
            .map_err(|e| format!("queue db lock: {}", e))? = Some(Arc::clone(&db));
        self.recover_from_database_once(&db)
    }

    /// After recovery, start workers when any jobs remain queued.
    ///
    /// Suppressions lookup failures are logged and treated as a soft skip
    /// (same as the former inline setup branch). `bind_and_start` errors
    /// propagate to the caller for ErrorLog handling.
    pub fn resume_if_pending(
        &self,
        app: AppHandle,
        db: Arc<Mutex<Database>>,
    ) -> Result<(), String> {
        if self.queued_job_count().unwrap_or(0) == 0 {
            return Ok(());
        }
        match self.path_suppressions() {
            Ok(suppressions) => self.bind_and_start(app, db, suppressions),
            Err(e) => {
                eprintln!("warn: task queue suppressions for resume: {e}");
                if let Ok(guard) = db.lock() {
                    if let Err(log_err) = guard.insert_error_log(
                        error_log_category_queue,
                        None,
                        None,
                        &format!("task queue suppressions for resume: {e}"),
                        None,
                    ) {
                        eprintln!("warn: ErrorLog after suppressions failure: {log_err}");
                    }
                }
                Ok(())
            }
        }
    }

    /// Bind runtime deps and ensure worker threads are running.
    /// `suppressions` should be the same Arc the Watcher uses so moves stay suppressed.
    /// Re-binding updates the shared suppressions pointer used by existing workers.
    /// Recovery runs once after DB bind and **before** workers start.
    pub fn bind_and_start(
        &self,
        app: AppHandle,
        db: Arc<Mutex<Database>>,
        suppressions: Arc<PathSuppression>,
    ) -> Result<(), String> {
        {
            *self
                .app
                .lock()
                .map_err(|e| format!("queue app lock: {}", e))? = Some(app.clone());
            *self
                .db
                .lock()
                .map_err(|e| format!("queue db lock: {}", e))? = Some(Arc::clone(&db));
            *self
                .suppressions
                .lock()
                .map_err(|e| format!("queue suppressions lock: {}", e))? =
                Arc::clone(&suppressions);
        }
        self.recover_from_database_once(&db)?;
        self.ensure_workers(app, db)
    }

    /// Reset `running`→`queued`, load durable jobs into the in-memory scheduler once.
    fn recover_from_database_once(&self, db: &Arc<Mutex<Database>>) -> Result<(), String> {
        if self.recovered.load(Ordering::SeqCst) {
            return Ok(());
        }

        let result = self.recover_from_database_inner(db);
        match &result {
            Ok(()) => {
                self.recovered.store(true, Ordering::SeqCst);
            }
            Err(e) => {
                if let Ok(guard) = db.lock() {
                    if let Err(log_err) = guard.insert_error_log(
                        error_log_category_queue,
                        None,
                        None,
                        &format!("task queue recovery failed: {e}"),
                        None,
                    ) {
                        eprintln!("task_queue: ErrorLog after recovery failure: {log_err}");
                    }
                } else {
                    eprintln!("task_queue: recovery failed (db lock): {e}");
                }
            }
        }
        result
    }

    fn recover_from_database_inner(&self, db: &Arc<Mutex<Database>>) -> Result<(), String> {
        let rows = {
            let guard = db
                .lock()
                .map_err(|e| format!("Database lock error: {}", e))?;
            let reset = guard
                .reset_running_jobs_to_queued()
                .map_err(|e| format!("reset running jobs: {}", e))?;
            if reset > 0 {
                eprintln!("task_queue: reset {reset} running job(s) to queued");
            }
            let _ = guard.prune_terminal_jobs(max_terminal_jobs);
            guard
                .list_jobs_with_status(&[
                    "queued",
                    "running",
                    "succeeded",
                    "failed",
                    "cancelled",
                ])
                .map_err(|e| format!("list jobs: {}", e))?
        };

        let mut loaded = Vec::new();
        for row in rows {
            match Self::job_record_from_row(&row) {
                Ok(record) => loaded.push(record),
                Err(err_msg) => {
                    let detail = format!(
                        "type={} payload={}",
                        row.job_type,
                        row.payload.chars().take(200).collect::<String>()
                    );
                    {
                        let guard = db
                            .lock()
                            .map_err(|e| format!("Database lock error: {}", e))?;
                        if let Err(e) = guard.mark_job_failed(
                            row.id,
                            row.attempts,
                            &err_msg,
                            false,
                        ) {
                            eprintln!(
                                "task_queue: mark_job_failed for bad payload id={}: {e}",
                                row.id
                            );
                            return Err(format!(
                                "mark_job_failed for job {}: {e}",
                                row.id
                            ));
                        }
                        if let Err(e) = guard.insert_error_log(
                            error_log_category_queue,
                            None,
                            None,
                            &format!("job {} load failed: {err_msg}", row.id),
                            Some(&detail),
                        ) {
                            eprintln!("task_queue: ErrorLog after bad job payload: {e}");
                        }
                    }
                    loaded.push(JobRecord {
                        id: row.id as u64,
                        kind: JobKind::Pipeline {
                            file_id: 0,
                            options: PipelineOptions {
                                enable_scrape: false,
                                enable_move: false,
                                enable_preview: false,
                            },
                        },
                        state: JobState::Failed,
                        error: Some(err_msg),
                        retryable: false,
                        attempts: row.attempts,
                        max_attempts: row.max_attempts.max(1),
                        stop_flag: None,
                    });
                }
            }
        }

        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|e| format!("queue lock: {}", e))?;
            for record in loaded {
                inner.scheduler.ingest_record(record);
            }
            let _ = inner.scheduler.prune_terminal_history();
        }

        // Align DB prune with in-memory prune after load
        if let Ok(guard) = db.lock() {
            let _ = guard.prune_terminal_jobs(max_terminal_jobs);
        }
        Ok(())
    }

    fn job_record_from_row(row: &crate::database::JobRow) -> Result<JobRecord, String> {
        let state = parse_job_state(&row.status)?;
        // After reset, running should not appear; treat stray running as queued
        let state = if state == JobState::Running {
            JobState::Queued
        } else {
            state
        };
        let kind = JobKind::from_db(&row.job_type, &row.payload)?;
        Ok(JobRecord {
            id: row.id as u64,
            kind,
            state,
            error: row.error.clone(),
            retryable: row.retryable,
            attempts: row.attempts,
            max_attempts: row.max_attempts.max(1),
            stop_flag: None,
        })
    }

    fn with_db<F, T>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(&Database) -> Result<T, String>,
    {
        let slot = self
            .db
            .lock()
            .map_err(|e| format!("queue db lock: {}", e))?;
        let db = slot
            .as_ref()
            .ok_or_else(|| "task queue database not bound".to_string())?;
        let guard = db
            .lock()
            .map_err(|e| format!("Database lock error: {}", e))?;
        f(&guard)
    }

    #[cfg(feature = "test-support")]
    fn delete_pruned_jobs(&self, pruned: &[u64]) -> Result<(), String> {
        let db = {
            let slot = self
                .db
                .lock()
                .map_err(|e| format!("queue db lock: {}", e))?;
            slot.as_ref()
                .ok_or_else(|| "task queue database not bound".to_string())?
                .clone()
        };
        delete_pruned_job_ids(&db, pruned)
    }

    fn ensure_workers(
        &self,
        app: AppHandle,
        db: Arc<Mutex<Database>>,
    ) -> Result<(), String> {
        let mut workers = self
            .workers
            .lock()
            .map_err(|e| format!("queue workers lock: {}", e))?;
        if !workers.is_empty() {
            return Ok(());
        }

        self.shutdown.store(false, Ordering::SeqCst);
        let concurrency = {
            let guard = self
                .inner
                .lock()
                .map_err(|e| format!("queue lock: {}", e))?;
            guard.scheduler.concurrency()
        };

        for i in 0..concurrency {
            let inner = Arc::clone(&self.inner);
            let wake = Arc::clone(&self.wake);
            let shutdown = Arc::clone(&self.shutdown);
            let app = app.clone();
            let db = Arc::clone(&db);
            let suppressions = Arc::clone(&self.suppressions);
            let execute_fault = Arc::clone(&self.execute_fault);
            let handle = thread::Builder::new()
                .name(format!("dam-task-queue-{}", i))
                .spawn(move || {
                    worker_loop(inner, wake, shutdown, app, db, suppressions, execute_fault);
                })
                .map_err(|e| format!("Failed to spawn queue worker: {}", e))?;
            workers.push(handle);
        }
        Ok(())
    }

    /// Enqueue a job; returns job id. Persists then emits. Wakes a worker.
    pub fn enqueue(
        &self,
        kind: JobKind,
        stop_flag: Option<Arc<AtomicBool>>,
    ) -> Result<u64, String> {
        let max_attempts = self.max_attempts;
        let payload = kind.to_payload_json()?;
        let id = self.with_db(|db| {
            db.insert_job(&crate::database::NewJobRow {
                status: JobState::Queued.as_str(),
                job_type: kind.label(),
                payload: &payload,
                attempts: 0,
                max_attempts,
                error: None,
                retryable: true,
            })
            .map(|id| id as u64)
            .map_err(|e| format!("insert job: {}", e))
        })?;

        {
            let mut guard = self
                .inner
                .lock()
                .map_err(|e| format!("queue lock: {}", e))?;
            guard.scheduler.ingest_record(JobRecord {
                id,
                kind: kind.clone(),
                state: JobState::Queued,
                error: None,
                retryable: true,
                attempts: 0,
                max_attempts,
                stop_flag,
            });
        }
        self.emit_queue_event(&kind, id, JobState::Queued, 0, None, true);
        self.wake_workers();
        Ok(id)
    }

    pub fn enqueue_pipeline(
        &self,
        file_id: i64,
        options: PipelineOptions,
        stop_flag: Option<Arc<AtomicBool>>,
    ) -> Result<u64, String> {
        self.enqueue(JobKind::Pipeline { file_id, options }, stop_flag)
    }

    /// Skip enqueue when the same file already has a Queued/Running pipeline job
    pub fn has_active_pipeline(&self, file_id: i64) -> Result<bool, String> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| format!("queue lock: {}", e))?;
        Ok(guard.scheduler.has_active_pipeline(file_id))
    }

    pub fn enqueue_phash(
        &self,
        job: PhashJob,
        stop_flag: Option<Arc<AtomicBool>>,
    ) -> Result<u64, String> {
        self.enqueue(
            JobKind::Phash {
                file_id: job.file_id,
                path: job.path,
            },
            stop_flag,
        )
    }

    /// Apply in-memory scheduler retry (optional test fault runs first).
    fn apply_scheduler_retry(&self, job_id: u64) -> Result<JobRecord, String> {
        #[cfg(feature = "test-support")]
        if self.retry_apply_fault.swap(false, Ordering::SeqCst) {
            return Err("injected scheduler retry failure".to_string());
        }
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| format!("queue lock: {}", e))?;
        guard
            .scheduler
            .retry(job_id)
            .map(|job| (*job).clone())
    }

    pub fn retry(&self, job_id: u64) -> Result<u64, String> {
        // Validate and snapshot failed fields for possible DB rollback
        let previous = {
            let guard = self
                .inner
                .lock()
                .map_err(|e| format!("queue lock: {}", e))?;
            let job = guard
                .scheduler
                .get(job_id)
                .ok_or_else(|| format!("Unknown job id {}", job_id))?;
            if job.state != JobState::Failed {
                return Err(format!(
                    "Job {} is {:?} (only Failed can retry)",
                    job_id,
                    job.state.as_str()
                ));
            }
            if !job.retryable {
                return Err(format!("Job {} is not retryable", job_id));
            }
            (job.attempts, job.error.clone(), job.retryable)
        };

        // Persist queued + reset attempts before touching in-memory scheduler
        self.with_db(|db| {
            db.update_job_state(
                job_id as i64,
                JobState::Queued.as_str(),
                0,
                None,
                true,
            )
            .map_err(|e| format!("persist retry for job {}: {}", job_id, e))
        })?;

        let apply_result = self.apply_scheduler_retry(job_id);

        let job = match apply_result {
            Ok(job) => job,
            Err(e) => {
                if let Err(rb) = self.with_db(|db| {
                    db.update_job_state(
                        job_id as i64,
                        JobState::Failed.as_str(),
                        previous.0,
                        previous.1.as_deref(),
                        previous.2,
                    )
                    .map_err(|rb| format!("rollback retry for job {}: {}", job_id, rb))
                }) {
                    eprintln!(
                        "warn: retry scheduler failed for job {job_id}: {e}; DB rollback also failed: {rb}"
                    );
                } else {
                    eprintln!(
                        "warn: retry scheduler failed for job {job_id}: {e}; rolled DB row back to failed"
                    );
                }
                return Err(e);
            }
        };
        self.emit_queue_event(
            &job.kind,
            job_id,
            JobState::Queued,
            job.attempts,
            Some("retry queued".to_string()),
            true,
        );
        self.wake_workers();
        Ok(job_id)
    }

    pub fn status(&self) -> Result<TaskQueueStatus, String> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| format!("queue lock: {}", e))?;
        Ok(guard.scheduler.status_snapshot())
    }

    fn wake_workers(&self) {
        let (lock, cvar) = &*self.wake;
        if let Ok(guard) = lock.lock() {
            cvar.notify_all();
            drop(guard);
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake_workers();
        if let Ok(mut workers) = self.workers.lock() {
            for handle in workers.drain(..) {
                let _ = handle.join();
            }
        }
    }

    fn emit_queue_event(
        &self,
        kind: &JobKind,
        job_id: u64,
        state: JobState,
        attempt: u32,
        message: Option<String>,
        retryable: bool,
    ) {
        let payload = TaskQueueProgressEvent {
            job_id,
            file_id: kind.file_id(),
            kind: kind.label().to_string(),
            state: state.as_str().to_string(),
            attempt,
            message,
            retryable,
        };
        if let Ok(guard) = self.app.lock() {
            if let Some(app) = guard.as_ref() {
                let _ = app.emit(queue_event_name, payload);
            }
        }
    }
}

impl Default for TaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TaskQueue {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Apply an execute outcome to the scheduler (shared by workers and test drain).
fn apply_exec_outcome(
    scheduler: &mut Scheduler,
    job: &JobRecord,
    outcome: ExecOutcome,
) -> (Option<JobRecord>, Vec<u64>, bool) {
    match outcome {
        ExecOutcome::Success => match scheduler.complete_success(job.id) {
            Some((rec, pruned)) => (Some(rec), pruned, false),
            None => (None, Vec::new(), false),
        },
        ExecOutcome::Failed { error, retryable } => {
            if retryable && job.attempts < job.max_attempts {
                (
                    scheduler.requeue_after_failure(job.id, error),
                    Vec::new(),
                    false,
                )
            } else {
                match scheduler.complete_failed(job.id, error, retryable) {
                    Some((rec, pruned)) => (Some(rec), pruned, true),
                    None => (None, Vec::new(), false),
                }
            }
        }
        ExecOutcome::Cancelled { message } => match scheduler.complete_cancelled(job.id, message)
        {
            Some((rec, pruned)) => (Some(rec), pruned, false),
            None => (None, Vec::new(), false),
        },
    }
}

fn write_exhaustion_error_log(db: &Database, rec: &JobRecord) {
    let msg = format!(
        "job {} exhausted retries ({}/{}): {}",
        rec.id,
        rec.attempts,
        rec.max_attempts,
        rec.error.as_deref().unwrap_or("unknown")
    );
    if let Err(e) = db.insert_error_log(
        error_log_category_queue,
        Some(rec.kind.file_id()),
        None,
        &msg,
        Some(rec.kind.label()),
    ) {
        eprintln!("task_queue: ErrorLog on retry exhaustion: {e}");
    }
}

fn worker_loop(
    inner: Arc<Mutex<QueueInner>>,
    wake: Arc<(Mutex<()>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    app: AppHandle,
    db: Arc<Mutex<Database>>,
    suppressions: Arc<Mutex<Arc<PathSuppression>>>,
    execute_fault: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        // Claim exactly one job — never mark more Running than this worker will run
        let job = {
            let mut guard = match inner.lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            guard.scheduler.dispatch_one()
        };

        let Some(job) = job else {
            let (lock, cvar) = &*wake;
            let Ok(guard) = lock.lock() else {
                break;
            };
            let _ = cvar.wait_timeout(guard, Duration::from_millis(200)).ok();
            continue;
        };

        if let Err(e) = persist_job_state(&db, &job) {
            eprintln!("task_queue: persist running failed for {}: {}", job.id, e);
        }
        emit_progress(&app, &job, JobState::Running, None, job.retryable);

        let supp = suppressions
            .lock()
            .ok()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|| Arc::new(PathSuppression::default()));
        let outcome = execute_job(
            &job,
            Some(&app),
            &db,
            supp.as_ref(),
            execute_fault.as_ref(),
        );

        let (finished, pruned, write_exhaustion_log) = {
            let mut guard = match inner.lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            apply_exec_outcome(&mut guard.scheduler, &job, outcome)
        };

        if let Some(ref rec) = finished {
            if let Err(e) = persist_job_state(&db, rec) {
                eprintln!("task_queue: persist finish failed for {}: {}", rec.id, e);
            }
            if write_exhaustion_log {
                if let Ok(guard) = db.lock() {
                    write_exhaustion_error_log(&guard, rec);
                }
            }
            if let Err(e) = delete_pruned_job_ids(&db, &pruned) {
                eprintln!("task_queue: prune delete failed: {e}");
            }
            let message = if rec.state == JobState::Queued {
                Some("auto-retry queued".to_string())
            } else {
                rec.error.clone()
            };
            emit_progress(&app, rec, rec.state, message, rec.retryable);
        }

        wake.1.notify_all();
    }
}

fn persist_job_state(db: &Arc<Mutex<Database>>, job: &JobRecord) -> Result<(), String> {
    let guard = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    guard
        .update_job_state(
            job.id as i64,
            job.state.as_str(),
            job.attempts,
            job.error.as_deref(),
            job.retryable,
        )
        .map_err(|e| format!("update job {}: {}", job.id, e))
}

fn delete_pruned_job_ids(db: &Arc<Mutex<Database>>, pruned: &[u64]) -> Result<(), String> {
    if pruned.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = pruned.iter().map(|id| *id as i64).collect();
    let guard = db
        .lock()
        .map_err(|e| format!("Database lock error: {}", e))?;
    guard
        .delete_jobs_by_ids(&ids)
        .map_err(|e| format!("delete pruned jobs: {}", e))?;
    let _ = guard.prune_terminal_jobs(max_terminal_jobs);
    Ok(())
}

enum ExecOutcome {
    Success,
    Failed { error: String, retryable: bool },
    Cancelled { message: Option<String> },
}

fn execute_job(
    job: &JobRecord,
    app: Option<&AppHandle>,
    db: &Arc<Mutex<Database>>,
    suppressions: &PathSuppression,
    execute_fault: &AtomicBool,
) -> ExecOutcome {
    if job
        .stop_flag
        .as_ref()
        .map(|f| f.load(Ordering::SeqCst))
        .unwrap_or(false)
    {
        return ExecOutcome::Cancelled {
            message: Some("stopped before start".to_string()),
        };
    }

    if execute_fault.load(Ordering::SeqCst) {
        return ExecOutcome::Failed {
            error: "injected execute fault".to_string(),
            retryable: true,
        };
    }

    match &job.kind {
        JobKind::Pipeline { file_id, options } => {
            // Re-soften from DB on every run (including retry) so scrape_failed /
            // already-moved / already-previewed rows do not re-run the full chain
            let effective = {
                let settings = match app {
                    Some(app) => pipeline::load_settings(app),
                    None => crate::settings::AppSettings::default(),
                };
                match db.lock() {
                    Ok(guard) => pipeline::options_for_existing_file(
                        &guard,
                        *file_id,
                        options,
                        Some(&settings),
                    )
                    .unwrap_or_else(|_| options.clone()),
                    Err(_) => options.clone(),
                }
            };
            if effective.is_noop() {
                return ExecOutcome::Success;
            }

            let Some(app) = app else {
                return ExecOutcome::Failed {
                    error: "app handle required for non-noop pipeline".to_string(),
                    retryable: true,
                };
            };

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pipeline::run_for_file(
                    app,
                    db,
                    *file_id,
                    &effective,
                    job.stop_flag.as_deref(),
                    Some(suppressions),
                )
            }));
            match result {
                Ok(r) => {
                    if r.aborted {
                        ExecOutcome::Cancelled {
                            message: Some("aborted by stop_flag".to_string()),
                        }
                    } else if r.errors.is_empty() {
                        // Pipeline UI may still have emitted step skips; empty errors = ok
                        ExecOutcome::Success
                    } else {
                        // Maps pipeline `completed_with_errors` onto Failed+retryable
                        ExecOutcome::Failed {
                            error: r.errors.join("; "),
                            retryable: true,
                        }
                    }
                }
                Err(payload) => ExecOutcome::Failed {
                    error: format!("pipeline panic: {:?}", payload),
                    retryable: true,
                },
            }
        }
        JobKind::Phash { file_id, path } => {
            if job
                .stop_flag
                .as_ref()
                .map(|f| f.load(Ordering::SeqCst))
                .unwrap_or(false)
            {
                return ExecOutcome::Cancelled {
                    message: Some("stopped before phash".to_string()),
                };
            }
            let phash_job = PhashJob {
                file_id: *file_id,
                path: path.clone(),
            };
            let results = scanner::compute_phash_jobs(vec![phash_job]);
            match db.lock() {
                Ok(guard) => match scanner::write_phashes(&guard, &results, Some(suppressions)) {
                    Ok(outcomes) => {
                        let skipped = outcomes.iter().any(|(_, o)| {
                            matches!(o, scanner::PhashApplyOutcome::SkippedDuplicate { .. })
                        });
                        if skipped {
                            eprintln!(
                                "task_queue: phash job for file {} skipped as near-duplicate (path suppressed)",
                                file_id
                            );
                        }
                        ExecOutcome::Success
                    }
                    Err(e) => ExecOutcome::Failed {
                        error: e.to_string(),
                        retryable: true,
                    },
                },
                Err(e) => ExecOutcome::Failed {
                    error: format!("db lock: {}", e),
                    retryable: true,
                },
            }
        }
    }
}

fn emit_progress(
    app: &AppHandle,
    job: &JobRecord,
    state: JobState,
    message: Option<String>,
    retryable: bool,
) {
    let payload = TaskQueueProgressEvent {
        job_id: job.id,
        file_id: job.kind.file_id(),
        kind: job.kind.label().to_string(),
        state: state.as_str().to_string(),
        attempt: job.attempts,
        message,
        retryable,
    };
    let _ = app.emit(queue_event_name, payload);
}

#[cfg(feature = "test-support")]
impl TaskQueue {
    fn persist_record(&self, job: &JobRecord) -> Result<(), String> {
        self.with_db(|db| {
            db.update_job_state(
                job.id as i64,
                job.state.as_str(),
                job.attempts,
                job.error.as_deref(),
                job.retryable,
            )
            .map_err(|e| format!("persist job {}: {}", job.id, e))
        })
    }

    /// Synchronously claim and finish pending jobs without worker threads (tests).
    pub fn drain_for_test(&self) -> Result<usize, String> {
        let db = {
            let slot = self
                .db
                .lock()
                .map_err(|e| format!("queue db lock: {}", e))?;
            slot.as_ref()
                .ok_or_else(|| "task queue database not bound".to_string())?
                .clone()
        };
        let suppressions = self.path_suppressions()?;
        let mut finished_count = 0usize;
        loop {
            let job = {
                let mut guard = self
                    .inner
                    .lock()
                    .map_err(|e| format!("queue lock: {}", e))?;
                guard.scheduler.dispatch_one()
            };
            let Some(job) = job else {
                break;
            };
            self.persist_record(&job)?;
            let outcome = execute_job(
                &job,
                None,
                &db,
                suppressions.as_ref(),
                self.execute_fault.as_ref(),
            );
            let (finished, pruned, write_exhaustion_log) = {
                let mut guard = self
                    .inner
                    .lock()
                    .map_err(|e| format!("queue lock: {}", e))?;
                apply_exec_outcome(&mut guard.scheduler, &job, outcome)
            };
            if let Some(ref rec) = finished {
                self.persist_record(rec)?;
                if write_exhaustion_log {
                    self.with_db(|db| {
                        write_exhaustion_error_log(db, rec);
                        Ok(())
                    })?;
                }
                self.delete_pruned_jobs(&pruned)?;
                finished_count = finished_count.saturating_add(1);
                // Auto-retry leaves job queued — continue draining
                if rec.state == JobState::Queued {
                    continue;
                }
            }
        }
        Ok(finished_count)
    }

    /// Recover from DB then run any queued jobs (no further caller action needed).
    /// Uses the same pending gate as [`Self::resume_if_pending`].
    pub fn recover_and_resume_for_test(
        &self,
        db: Arc<Mutex<Database>>,
    ) -> Result<usize, String> {
        self.bind_database(db)?;
        // Same gate as resume_if_pending (production starts workers; tests drain)
        if self.queued_job_count()? == 0 {
            return Ok(0);
        }
        self.drain_for_test()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    fn pipeline_kind(file_id: i64) -> JobKind {
        JobKind::Pipeline {
            file_id,
            options: PipelineOptions {
                enable_scrape: false,
                enable_move: false,
                enable_preview: false,
            },
        }
    }

    #[test]
    fn enqueue_assigns_monotonic_ids() {
        let mut s = Scheduler::new(2);
        let a = s.enqueue(pipeline_kind(1), None, 3);
        let b = s.enqueue(pipeline_kind(2), None, 3);
        assert_eq!(a, 1);
        assert_eq!(b, 2);
    }

    #[test]
    fn has_active_pipeline_detects_queued_and_running() {
        let mut s = Scheduler::new(2);
        assert!(!s.has_active_pipeline(7));
        let id = s.enqueue(pipeline_kind(7), None, 3);
        assert!(s.has_active_pipeline(7));
        assert!(!s.has_active_pipeline(8));
        let job = s.dispatch_one().expect("run");
        assert_eq!(job.id, id);
        assert!(s.has_active_pipeline(7));
        s.complete_success(id);
        assert!(!s.has_active_pipeline(7));
    }

    #[test]
    fn concurrency_limit_caps_dispatch_one() {
        let mut s = Scheduler::new(2);
        s.enqueue(pipeline_kind(10), None, 3);
        s.enqueue(pipeline_kind(20), None, 3);
        s.enqueue(pipeline_kind(30), None, 3);

        let a = s.dispatch_one().expect("first");
        let b = s.dispatch_one().expect("second");
        assert_eq!(s.running_len(), 2);
        assert_eq!(s.pending_len(), 1);
        assert!(s.dispatch_one().is_none());

        s.complete_success(a.id);
        let next = s.dispatch_one().expect("third");
        assert_eq!(next.kind.file_id(), 30);
        let _ = b;
    }

    #[test]
    fn dispatch_one_marks_only_the_claimed_job_running() {
        // Regression: old dispatch_ready marked concurrency jobs Running while a
        // worker only executed one — orphans stayed Running forever.
        let mut s = Scheduler::new(2);
        let a = s.enqueue(pipeline_kind(1), None, 3);
        let b = s.enqueue(pipeline_kind(2), None, 3);
        let c = s.enqueue(pipeline_kind(3), None, 3);

        let taken = s.dispatch_one().expect("claim one");
        assert_eq!(taken.id, a);
        assert_eq!(s.running_len(), 1);
        assert_eq!(s.get(b).unwrap().state, JobState::Queued);
        assert_eq!(s.get(c).unwrap().state, JobState::Queued);

        let taken2 = s.dispatch_one().expect("second worker");
        assert_eq!(taken2.id, b);
        assert_eq!(s.running_len(), 2);
        assert_eq!(s.get(c).unwrap().state, JobState::Queued);
    }

    #[test]
    fn same_file_blocked_job_stays_queued_not_running() {
        let mut s = Scheduler::new(4);
        let j1 = s.enqueue(
            JobKind::Phash {
                file_id: 7,
                path: PathBuf::from("/tmp/a.mp4"),
            },
            None,
            3,
        );
        let j2 = s.enqueue(pipeline_kind(7), None, 3);
        let j3 = s.enqueue(pipeline_kind(8), None, 3);

        let t1 = s.dispatch_one().expect("phash 7");
        let t2 = s.dispatch_one().expect("pipeline 8");
        assert_eq!(t1.id, j1);
        assert_eq!(t2.id, j3);
        // Sibling for file 7 must remain Queued (not orphan Running holding the lock)
        assert_eq!(s.get(j2).unwrap().state, JobState::Queued);
        assert_eq!(s.pending_len(), 1);
        assert_eq!(s.running_len(), 2);

        s.complete_success(j1);
        let t3 = s.dispatch_one().expect("pipeline 7 after unlock");
        assert_eq!(t3.id, j2);
        assert_eq!(t3.kind.file_id(), 7);
    }

    #[test]
    fn failed_jobs_are_marked_and_retryable() {
        let mut s = Scheduler::new(1);
        let id = s.enqueue(pipeline_kind(1), None, 3);
        let started = s.dispatch_one().expect("run");
        assert_eq!(started.id, id);
        assert_eq!(started.attempts, 1);

        s.complete_failed(id, "scrape boom".to_string(), true);
        let job = s.get(id).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.error.as_deref(), Some("scrape boom"));
        assert!(job.retryable);
        assert!(s.failed_ids().contains(&id));

        s.retry(id).expect("retry");
        assert_eq!(s.get(id).unwrap().state, JobState::Queued);
        assert_eq!(s.get(id).unwrap().attempts, 0);
        assert_eq!(s.pending_len(), 1);

        let again = s.dispatch_one().expect("retry run");
        assert_eq!(again.attempts, 1);
    }

    #[test]
    fn manual_retry_resets_attempts_after_exhaustion() {
        let mut s = Scheduler::new(1);
        let id = s.enqueue(pipeline_kind(1), None, 1);
        let _ = s.dispatch_one();
        s.complete_failed(id, "once".to_string(), true);
        // Auto-budget exhausted (1/1) but manual retry still works and resets attempts
        s.retry(id).expect("manual retry after exhaustion");
        assert_eq!(s.get(id).unwrap().state, JobState::Queued);
        assert_eq!(s.get(id).unwrap().attempts, 0);

        let id2 = s.enqueue(pipeline_kind(2), None, 3);
        let _ = s.dispatch_one();
        s.complete_failed(id2, "fatal".to_string(), false);
        let err = s.retry(id2).unwrap_err();
        assert!(err.contains("not retryable"), "{}", err);
    }

    #[test]
    fn cancelled_jobs_are_not_failed() {
        let mut s = Scheduler::new(1);
        let id = s.enqueue(pipeline_kind(1), None, 3);
        let _ = s.dispatch_one();
        s.complete_cancelled(id, Some("stop".into()));
        assert_eq!(s.get(id).unwrap().state, JobState::Cancelled);
        assert!(s.failed_ids().is_empty());
        assert!(s.retry(id).is_err());
    }

    #[test]
    fn status_snapshot_counts() {
        let mut s = Scheduler::new(2);
        let a = s.enqueue(pipeline_kind(1), None, 3);
        let b = s.enqueue(pipeline_kind(2), None, 3);
        let _ = s.dispatch_one();
        let _ = s.dispatch_one();
        s.complete_failed(a, "x".into(), true);
        s.complete_success(b);
        let snap = s.status_snapshot();
        assert_eq!(snap.concurrency, 2);
        assert_eq!(snap.failed, 1);
        assert_eq!(snap.succeeded, 1);
        assert_eq!(snap.running, 0);
        assert_eq!(snap.jobs.len(), 2);
    }

    #[test]
    fn terminal_history_is_pruned_to_bound() {
        let mut s = Scheduler::new(1);
        for i in 0..(max_terminal_jobs as i64 + 25) {
            let id = s.enqueue(pipeline_kind(i + 1), None, 3);
            let _ = s.dispatch_one();
            s.complete_success(id);
        }
        let terminal = s
            .jobs
            .values()
            .filter(|j| j.state == JobState::Succeeded)
            .count();
        assert_eq!(terminal, max_terminal_jobs);
        // Oldest ids pruned
        assert!(s.get(1).is_none());
        assert!(s.get(25).is_none());
        assert!(s.get(26).is_some());
    }

    /// Integration-style: concurrency≥2 workers each claim via dispatch_one
    /// (mirrors worker_loop). Proves no job stuck Running and same-file lock
    /// is released so siblings eventually run.
    #[test]
    fn concurrent_workers_never_orphan_running_or_hold_file_lock() {
        let sched = Arc::new(Mutex::new(Scheduler::new(2)));
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();

        {
            let mut s = sched.lock().unwrap();
            for i in 0..8 {
                s.enqueue(pipeline_kind(200 + i), None, 3);
            }
            // Same-file pair: second must wait for first (serial), not stick Running
            s.enqueue(
                JobKind::Phash {
                    file_id: 999,
                    path: PathBuf::from("/tmp/x.mp4"),
                },
                None,
                3,
            );
            s.enqueue(pipeline_kind(999), None, 3);
        }

        for _ in 0..2 {
            let sched = Arc::clone(&sched);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                loop {
                    let job = {
                        let mut s = sched.lock().unwrap();
                        // Invariant while claiming: every Running job is owned
                        // by some worker that will complete it (dispatch_one only)
                        let running_before = s.running_len();
                        assert!(running_before <= 2);
                        s.dispatch_one()
                    };
                    let Some(job) = job else {
                        // Drain check: if pending remains but we got None, either
                        // at concurrency or same-file blocked — wait briefly
                        let (pending, running) = {
                            let s = sched.lock().unwrap();
                            (s.pending_len(), s.running_len())
                        };
                        if pending == 0 && running == 0 {
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    };
                    thread::sleep(Duration::from_millis(3));
                    let mut s = sched.lock().unwrap();
                    s.complete_success(job.id);
                }
            }));
        }

        barrier.wait();
        for h in handles {
            h.join().unwrap();
        }

        let s = sched.lock().unwrap();
        assert_eq!(s.running_len(), 0, "no orphan Running jobs");
        assert_eq!(
            s.pending_len(),
            0,
            "all work drained including same-file sibling"
        );
        for job in s.jobs.values() {
            assert_ne!(
                job.state,
                JobState::Running,
                "job {} stuck Running",
                job.id
            );
            assert!(
                matches!(job.state, JobState::Succeeded),
                "job {} unexpected {:?}",
                job.id,
                job.state
            );
        }
        // Both jobs for file 999 completed (lock was released)
        let file_999: Vec<_> = s
            .jobs
            .values()
            .filter(|j| j.kind.file_id() == 999)
            .collect();
        assert_eq!(file_999.len(), 2);
    }

    #[test]
    fn concurrent_dispatch_never_exceeds_limit() {
        let sched = Arc::new(Mutex::new(Scheduler::new(2)));
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();

        {
            let mut s = sched.lock().unwrap();
            for i in 0..20 {
                s.enqueue(pipeline_kind(100 + i), None, 3);
            }
        }

        for _ in 0..4 {
            let sched = Arc::clone(&sched);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..10 {
                    let job = {
                        let mut s = sched.lock().unwrap();
                        s.dispatch_one()
                    };
                    if let Some(job) = job {
                        thread::sleep(Duration::from_millis(1));
                        let mut s = sched.lock().unwrap();
                        assert!(s.running_len() <= 2);
                        s.complete_success(job.id);
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Drain any leftover under the mutex with sequential takes
        loop {
            let job = {
                let mut s = sched.lock().unwrap();
                s.dispatch_one()
            };
            let Some(job) = job else {
                break;
            };
            let mut s = sched.lock().unwrap();
            s.complete_success(job.id);
        }

        let s = sched.lock().unwrap();
        assert_eq!(s.running_len(), 0);
        let snap = s.status_snapshot();
        assert_eq!(snap.succeeded, 20);
        assert_eq!(snap.pending, 0);
    }
}
