//! Background task queue with bounded concurrency and per-file serial ordering.
//!
//! # Policy (documented choice)
//! - **Concurrency**: default `2` workers (env `IDAM_QUEUE_CONCURRENCY`, clamped ≥1).
//!   Small parallel pool keeps scrape/ffmpeg from stampeding while still overlapping
//!   I/O across different files.
//! - **Same-file serial**: at most one running job per `file_id`. Pending jobs for
//!   that file stay queued until the active one finishes (FIFO). This preserves
//!   phash → pipeline ordering when both are enqueued for the same ingest.
//! - **Failures**: jobs are marked `Failed` with an error string and `retryable`
//!   flag; errors are never swallowed. Callers may `retry` a failed job (new
//!   attempt) without inventing success. Abort / stop_flag → `Cancelled`, not retry.
//! - Pipeline still honors scrape_failed / PathSuppression / stop_flag from #19/#23.

use crate::database::Database;
use crate::path_suppression::PathSuppression;
use crate::pipeline::{self, PipelineOptions};
use crate::scanner::{self, PhashJob};
use serde::Serialize;
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
const default_concurrency: usize = 2;

#[allow(non_upper_case_globals)]
const default_max_attempts: u32 = 3;

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

    pub fn enqueue(
        &mut self,
        kind: JobKind,
        stop_flag: Option<Arc<AtomicBool>>,
        max_attempts: u32,
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let record = JobRecord {
            id,
            kind,
            state: JobState::Queued,
            error: None,
            retryable: true,
            attempts: 0,
            max_attempts: max_attempts.max(1),
            stop_flag,
        };
        self.jobs.insert(id, record);
        self.pending.push_back(id);
        id
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn running_len(&self) -> usize {
        self.running.len()
    }

    pub fn get(&self, id: u64) -> Option<&JobRecord> {
        self.jobs.get(&id)
    }

    pub fn failed_ids(&self) -> Vec<u64> {
        self.jobs
            .values()
            .filter(|j| j.state == JobState::Failed)
            .map(|j| j.id)
            .collect()
    }

    /// Mark up to `concurrency` eligible jobs as Running and return them.
    /// Eligible = first pending whose file_id is not already running.
    pub fn dispatch_ready(&mut self) -> Vec<JobRecord> {
        let mut started = Vec::new();
        let mut skipped = VecDeque::new();

        while self.running.len() < self.concurrency {
            let Some(id) = self.pending.pop_front() else {
                break;
            };
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
            started.push(job.clone());
            self.jobs.insert(id, job);
        }

        // Preserve relative order of skipped (same-file blocked) at the front
        while let Some(id) = skipped.pop_back() {
            self.pending.push_front(id);
        }

        started
    }

    pub fn complete_success(&mut self, id: u64) -> Option<JobRecord> {
        self.finish(id, JobState::Succeeded, None, false)
    }

    pub fn complete_failed(&mut self, id: u64, error: String, retryable: bool) -> Option<JobRecord> {
        self.finish(id, JobState::Failed, Some(error), retryable)
    }

    pub fn complete_cancelled(&mut self, id: u64, message: Option<String>) -> Option<JobRecord> {
        self.finish(id, JobState::Cancelled, message, false)
    }

    fn finish(
        &mut self,
        id: u64,
        state: JobState,
        error: Option<String>,
        retryable: bool,
    ) -> Option<JobRecord> {
        let job = self.jobs.get_mut(&id)?;
        let file_id = job.kind.file_id();
        job.state = state;
        job.error = error;
        job.retryable = retryable;
        self.running.remove(&id);
        self.running_files.remove(&file_id);
        Some(job.clone())
    }

    /// Re-queue a failed job if attempts remain
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
        if job.attempts >= job.max_attempts {
            return Err(format!(
                "Job {} exhausted attempts ({}/{})",
                id, job.attempts, job.max_attempts
            ));
        }
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
}

impl TaskQueue {
    pub fn new() -> Self {
        Self::with_concurrency(concurrency_from_env())
    }

    pub fn with_concurrency(concurrency: usize) -> Self {
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
        }
    }

    /// Bind runtime deps and ensure worker threads are running.
    /// `suppressions` should be the same Arc the Watcher uses so moves stay suppressed.
    /// Re-binding updates the shared suppressions pointer used by existing workers.
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
        self.ensure_workers(app, db)
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
            let handle = thread::Builder::new()
                .name(format!("dam-task-queue-{}", i))
                .spawn(move || {
                    worker_loop(inner, wake, shutdown, app, db, suppressions);
                })
                .map_err(|e| format!("Failed to spawn queue worker: {}", e))?;
            workers.push(handle);
        }
        Ok(())
    }

    /// Enqueue a job; returns job id. Wakes a worker.
    pub fn enqueue(
        &self,
        kind: JobKind,
        stop_flag: Option<Arc<AtomicBool>>,
    ) -> Result<u64, String> {
        let (id, attempt) = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|e| format!("queue lock: {}", e))?;
            let id = guard
                .scheduler
                .enqueue(kind.clone(), stop_flag, default_max_attempts);
            (id, 0u32)
        };
        self.emit_queue_event(&kind, id, JobState::Queued, attempt, None, true);
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

    pub fn retry(&self, job_id: u64) -> Result<u64, String> {
        let (kind, attempt) = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|e| format!("queue lock: {}", e))?;
            guard.scheduler.retry(job_id)?;
            let job = guard
                .scheduler
                .get(job_id)
                .ok_or_else(|| format!("Unknown job {}", job_id))?;
            (job.kind.clone(), job.attempts)
        };
        self.emit_queue_event(
            &kind,
            job_id,
            JobState::Queued,
            attempt,
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

fn worker_loop(
    inner: Arc<Mutex<QueueInner>>,
    wake: Arc<(Mutex<()>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    app: AppHandle,
    db: Arc<Mutex<Database>>,
    suppressions: Arc<Mutex<Arc<PathSuppression>>>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        let job = {
            let mut guard = match inner.lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            let mut started = guard.scheduler.dispatch_ready();
            started.pop()
        };

        let Some(job) = job else {
            let (lock, cvar) = &*wake;
            let Ok(guard) = lock.lock() else {
                break;
            };
            let _ = cvar.wait_timeout(guard, Duration::from_millis(200)).ok();
            continue;
        };

        emit_progress(&app, &job, JobState::Running, None, job.retryable);

        let supp = suppressions
            .lock()
            .ok()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|| Arc::new(PathSuppression::default()));
        let outcome = execute_job(&job, &app, &db, supp.as_ref());

        let finished = {
            let mut guard = match inner.lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            match outcome {
                ExecOutcome::Success => guard.scheduler.complete_success(job.id),
                ExecOutcome::Failed { error, retryable } => {
                    guard.scheduler.complete_failed(job.id, error, retryable)
                }
                ExecOutcome::Cancelled { message } => {
                    guard.scheduler.complete_cancelled(job.id, message)
                }
            }
        };

        if let Some(rec) = finished {
            emit_progress(&app, &rec, rec.state, rec.error.clone(), rec.retryable);
        }

        wake.1.notify_all();
    }
}

enum ExecOutcome {
    Success,
    Failed { error: String, retryable: bool },
    Cancelled { message: Option<String> },
}

fn execute_job(
    job: &JobRecord,
    app: &AppHandle,
    db: &Arc<Mutex<Database>>,
    suppressions: &PathSuppression,
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

    match &job.kind {
        JobKind::Pipeline { file_id, options } => {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pipeline::run_for_file(
                    app,
                    db,
                    *file_id,
                    options,
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
                        ExecOutcome::Success
                    } else {
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
                Ok(guard) => match scanner::write_phashes(&guard, &results) {
                    Ok(()) => ExecOutcome::Success,
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
    fn concurrency_limit_caps_dispatch() {
        let mut s = Scheduler::new(2);
        s.enqueue(pipeline_kind(10), None, 3);
        s.enqueue(pipeline_kind(20), None, 3);
        s.enqueue(pipeline_kind(30), None, 3);

        let started = s.dispatch_ready();
        assert_eq!(started.len(), 2);
        assert_eq!(s.running_len(), 2);
        assert_eq!(s.pending_len(), 1);

        let more = s.dispatch_ready();
        assert!(more.is_empty());

        s.complete_success(started[0].id);
        let next = s.dispatch_ready();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].kind.file_id(), 30);
    }

    #[test]
    fn same_file_jobs_run_serially_in_fifo_order() {
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

        let started = s.dispatch_ready();
        assert_eq!(started.len(), 2);
        let ids: Vec<u64> = started.iter().map(|j| j.id).collect();
        assert!(ids.contains(&j1));
        assert!(ids.contains(&j3));
        assert!(!ids.contains(&j2));
        assert_eq!(s.pending_len(), 1);

        s.complete_success(j1);
        let next = s.dispatch_ready();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].id, j2);
        assert_eq!(next[0].kind.file_id(), 7);
    }

    #[test]
    fn failed_jobs_are_marked_and_retryable() {
        let mut s = Scheduler::new(1);
        let id = s.enqueue(pipeline_kind(1), None, 3);
        let started = s.dispatch_ready();
        assert_eq!(started[0].id, id);
        assert_eq!(started[0].attempts, 1);

        s.complete_failed(id, "scrape boom".to_string(), true);
        let job = s.get(id).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.error.as_deref(), Some("scrape boom"));
        assert!(job.retryable);
        assert!(s.failed_ids().contains(&id));

        s.retry(id).expect("retry");
        assert_eq!(s.get(id).unwrap().state, JobState::Queued);
        assert_eq!(s.pending_len(), 1);

        let again = s.dispatch_ready();
        assert_eq!(again[0].attempts, 2);
    }

    #[test]
    fn retry_respects_max_attempts_and_non_retryable() {
        let mut s = Scheduler::new(1);
        let id = s.enqueue(pipeline_kind(1), None, 1);
        let _ = s.dispatch_ready();
        s.complete_failed(id, "once".to_string(), true);
        let err = s.retry(id).unwrap_err();
        assert!(err.contains("exhausted"), "{}", err);

        let id2 = s.enqueue(pipeline_kind(2), None, 3);
        let _ = s.dispatch_ready();
        s.complete_failed(id2, "fatal".to_string(), false);
        let err = s.retry(id2).unwrap_err();
        assert!(err.contains("not retryable"), "{}", err);
    }

    #[test]
    fn cancelled_jobs_are_not_failed() {
        let mut s = Scheduler::new(1);
        let id = s.enqueue(pipeline_kind(1), None, 3);
        let _ = s.dispatch_ready();
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
        let _ = s.dispatch_ready();
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
                    let started = {
                        let mut s = sched.lock().unwrap();
                        s.dispatch_ready()
                    };
                    for job in started {
                        thread::sleep(Duration::from_millis(1));
                        let mut s = sched.lock().unwrap();
                        s.complete_success(job.id);
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let s = sched.lock().unwrap();
        assert_eq!(s.running_len(), 0);
        let snap = s.status_snapshot();
        assert_eq!(snap.succeeded, 20);
        assert_eq!(snap.pending, 0);
    }
}
