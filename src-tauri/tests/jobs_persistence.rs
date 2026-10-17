//! Jobs table persistence / restart recovery (#34).
//!
//! Lives under `tests/` so Windows harnesses pick up Common-Controls v6 via
//! `cargo:rustc-link-arg-tests` (see build.rs / #48). Do not move into `--lib`.
//! Requires `--features test-support`.

use intelligent_dam_lib::test_support::{
    max_terminal_jobs, Database, JobKind, NewJobRow, PipelineOptions, TaskQueue,
};
use std::sync::{Arc, Mutex};

fn temp_queue_db(label: &str) -> (tempfile::TempDir, Arc<Mutex<Database>>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{label}.db"));
    let db = Database::new(path.to_str().expect("utf8")).expect("db");
    (dir, Arc::new(Mutex::new(db)))
}

fn noop_pipeline(file_id: i64) -> JobKind {
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
fn enqueue_persists_queued_row_before_status_visible() {
    let (_dir, db) = temp_queue_db("enqueue-persist");
    let queue = TaskQueue::with_concurrency(1);
    queue.bind_database(Arc::clone(&db)).expect("bind");
    let id = queue.enqueue(noop_pipeline(9), None).expect("enqueue");
    let row = db.lock().unwrap().get_job(id as i64).expect("row");
    assert_eq!(row.status, "queued");
    assert_eq!(row.job_type, "pipeline");
    assert!(row.payload.contains("\"file_id\":9"));
    let status = queue.status().expect("status");
    assert_eq!(status.pending, 1);
    assert_eq!(status.jobs[0].job_id, id);
}

#[test]
fn mid_run_restart_resumes_without_further_calls() {
    let (_dir, db) = temp_queue_db("restart-complete");
    let queue = TaskQueue::with_concurrency(1);
    queue.bind_database(Arc::clone(&db)).expect("bind");
    let id = queue.enqueue(noop_pipeline(3), None).expect("enqueue");

    db.lock()
        .unwrap()
        .update_job_state(id as i64, "running", 1, None, true)
        .expect("mark running");

    // Same pending gate as TaskQueue::resume_if_pending; tests drain instead of workers
    let queue2 = TaskQueue::with_concurrency(1);
    let n = queue2
        .recover_and_resume_for_test(Arc::clone(&db))
        .expect("recover+resume");
    assert!(n >= 1, "recovered jobs must start executing");

    let row = db.lock().unwrap().get_job(id as i64).expect("done");
    assert_eq!(row.status, "succeeded");
    let snap = queue2.status().expect("snap");
    assert_eq!(snap.succeeded, 1);
    assert_eq!(snap.pending, 0);
}

#[test]
fn resume_if_pending_gate_skips_when_no_queued_jobs() {
    let (_dir, db) = temp_queue_db("resume-empty");
    let queue = TaskQueue::with_concurrency(1);
    queue.bind_database(Arc::clone(&db)).expect("bind");
    assert_eq!(queue.queued_job_count().expect("count"), 0);
    // Mirrors resume_if_pending early return (no AppHandle needed when empty)
    let n = queue
        .recover_and_resume_for_test(Arc::clone(&db))
        .expect("noop resume");
    assert_eq!(n, 0);
}

#[test]
fn retry_rolls_db_back_when_scheduler_apply_fails() {
    let (_dir, db) = temp_queue_db("retry-rollback");
    let queue = TaskQueue::with_options(1, 1);
    queue.bind_database(Arc::clone(&db)).expect("bind");
    let id = queue.enqueue(noop_pipeline(5), None).expect("enqueue");
    queue.arm_fault();
    let _ = queue.drain_for_test();
    queue.clear_fault();

    let before = db.lock().unwrap().get_job(id as i64).expect("failed row");
    assert_eq!(before.status, "failed");
    let prev_attempts = before.attempts;
    let prev_error = before.error.clone();
    let prev_retryable = before.retryable;

    queue.arm_retry_apply_fault();
    let err = queue.retry(id).expect_err("scheduler apply fault");
    assert!(err.contains("injected"), "{err}");

    let after = db.lock().unwrap().get_job(id as i64).expect("rolled back");
    assert_eq!(after.status, "failed");
    assert_eq!(after.attempts, prev_attempts);
    assert_eq!(after.error, prev_error);
    assert_eq!(after.retryable, prev_retryable);
    let snap = queue.status().expect("snap");
    assert_eq!(snap.failed, 1);
    assert_eq!(snap.pending, 0);
}

#[test]
fn retry_exhaustion_marks_failed_and_writes_error_log() {
    let (_dir, db) = temp_queue_db("retry-exhaust");
    let queue = TaskQueue::with_options(1, 1);
    queue.bind_database(Arc::clone(&db)).expect("bind");
    let id = queue.enqueue(noop_pipeline(4), None).expect("enqueue");
    queue.arm_fault();
    let _ = queue.drain_for_test();
    queue.clear_fault();

    let row = db.lock().unwrap().get_job(id as i64).expect("row");
    assert_eq!(row.status, "failed");
    assert!(row.error.as_deref().unwrap_or("").contains("injected"));
    let logs = db.lock().unwrap().list_error_logs(20).expect("logs");
    assert!(
        logs.iter()
            .any(|e| e.category == "queue" && e.message.contains("exhausted")),
        "logs={logs:?}"
    );

    // Manual retry after auto-exhaustion must reset attempts and re-queue
    queue.retry(id).expect("manual retry after exhaustion");
    let row = db.lock().unwrap().get_job(id as i64).expect("retried");
    assert_eq!(row.status, "queued");
    assert_eq!(row.attempts, 0);
}

#[test]
fn malformed_payload_on_load_becomes_failed_with_error_log() {
    let (_dir, db) = temp_queue_db("bad-payload");
    let id = {
        let guard = db.lock().unwrap();
        guard
            .insert_job(&NewJobRow {
                status: "queued",
                job_type: "pipeline",
                payload: "not-json",
                attempts: 0,
                max_attempts: 3,
                error: None,
                retryable: true,
            })
            .expect("insert")
    };
    let queue = TaskQueue::with_concurrency(1);
    queue.bind_database(Arc::clone(&db)).expect("recover");
    let row = db.lock().unwrap().get_job(id).expect("row");
    assert_eq!(row.status, "failed");
    assert!(row.error.as_deref().unwrap_or("").contains("invalid"));
    let logs = db.lock().unwrap().list_error_logs(10).expect("logs");
    assert!(
        logs.iter()
            .any(|e| e.category == "queue" && e.message.contains("load failed")),
        "logs={logs:?}"
    );
    let snap = queue.status().expect("snap");
    assert_eq!(snap.failed, 1);
    assert_eq!(snap.pending, 0);
}

#[test]
fn terminal_prune_deletes_from_db() {
    let (_dir, db) = temp_queue_db("prune-db");
    let queue = TaskQueue::with_concurrency(1);
    queue.bind_database(Arc::clone(&db)).expect("bind");
    let mut first_id = None;
    for i in 0..(max_terminal_jobs as i64 + 5) {
        let id = queue.enqueue(noop_pipeline(100 + i), None).expect("enq");
        if first_id.is_none() {
            first_id = Some(id);
        }
        queue.drain_for_test().expect("drain");
    }
    let oldest = first_id.expect("id");
    let missing = db.lock().unwrap().get_job(oldest as i64);
    assert!(
        missing.is_err(),
        "oldest terminal job should be pruned from DB"
    );
    let remaining = db
        .lock()
        .unwrap()
        .list_jobs_with_status(&["succeeded"])
        .expect("list")
        .len();
    assert!(remaining <= max_terminal_jobs);
}

#[test]
fn active_pipeline_dedupe_skips_second_enqueue_same_file() {
    let (_dir, db) = temp_queue_db("dedupe");
    let queue = TaskQueue::with_concurrency(1);
    queue.bind_database(Arc::clone(&db)).expect("bind");
    let a = queue.enqueue(noop_pipeline(7), None).expect("a");
    assert!(queue.has_active_pipeline(7).expect("active"));
    // Caller-layer dedupe: skip when the same file is already queued/running
    let enqueued_again = if queue.has_active_pipeline(7).expect("check") {
        None
    } else {
        Some(queue.enqueue(noop_pipeline(7), None).expect("b"))
    };
    assert!(enqueued_again.is_none(), "second enqueue of file 7 must skip");
    let queued = db
        .lock()
        .unwrap()
        .list_jobs_with_status(&["queued"])
        .expect("list");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].id, a as i64);
}
