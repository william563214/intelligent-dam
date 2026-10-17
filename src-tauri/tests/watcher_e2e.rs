//! Watcher end-to-end: notify → stability → ingest → queue (#36).
//!
//! Lives under `tests/` so Windows harnesses pick up Common-Controls v6 via
//! `cargo:rustc-link-arg-tests` (see build.rs). Requires `--features test-support`.

use intelligent_dam_lib::test_support::{
    get_all_files, Database, TaskQueue, WatcherManager, WatcherOptions,
};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[allow(non_upper_case_globals)]
const poll_interval: Duration = Duration::from_millis(25);

#[allow(non_upper_case_globals)]
const ingest_timeout: Duration = Duration::from_secs(20);

struct Fixture {
    temp: tempfile::TempDir,
    watch_dir: PathBuf,
    db: Arc<Mutex<Database>>,
    /// Kept alive so bind_database / enqueue share one queue for the session
    _queue: Arc<TaskQueue>,
    watcher: WatcherManager,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let watch_dir = temp.path().join("watch");
        fs::create_dir_all(&watch_dir).expect("mkdir watch");
        let db_path = temp.path().join(format!("{label}.db"));
        let db = Database::new(db_path.to_str().expect("utf8")).expect("db");
        let db = Arc::new(Mutex::new(db));
        let queue = Arc::new(TaskQueue::with_concurrency(1));
        let watcher = WatcherManager::new();
        let status = watcher
            .start_for_test(
                vec![watch_dir.clone()],
                Arc::clone(&db),
                Arc::clone(&queue),
                WatcherOptions::for_tests(),
            )
            .expect("start watcher");
        assert!(status.running);
        assert_eq!(status.watched_paths.len(), 1);
        // Event-driven ready: watcher reports running (no fixed sleep as assertion)
        assert!(wait_until(Duration::from_secs(2), || watcher.is_running()));
        Self {
            temp,
            watch_dir,
            db,
            _queue: queue,
            watcher,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.watch_dir.join(name)
    }

    fn file_count(&self) -> usize {
        get_all_files(&self.db).expect("list").len()
    }

    fn queued_job_count(&self) -> usize {
        self.db
            .lock()
            .expect("lock")
            .list_jobs_with_status(&["queued", "running"])
            .expect("jobs")
            .len()
    }

    fn has_file_named(&self, name: &str) -> bool {
        get_all_files(&self.db).expect("list").iter().any(|f| {
            f.file.path.ends_with(name)
                || Path::new(&f.file.path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    == Some(name)
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.watcher.stop();
    }
}

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if pred() {
            return true;
        }
        thread::sleep(poll_interval);
    }
    false
}

fn write_chunked(path: &Path, chunks: usize, chunk_bytes: usize, pause: Duration) {
    let mut file = File::create(path).expect("create");
    for i in 0..chunks {
        let byte = (i as u8).wrapping_add(1);
        file.write_all(&vec![byte; chunk_bytes]).expect("write");
        file.flush().expect("flush");
        if i + 1 < chunks {
            thread::sleep(pause);
        }
    }
}

#[test]
fn chunked_write_waits_for_stability_then_ingests_and_enqueues() {
    let fx = Fixture::new("chunked");
    let dest = fx.path("growing.mp4");
    let writer_done = Arc::new(Mutex::new(false));
    let done_flag = Arc::clone(&writer_done);
    let dest_writer = dest.clone();

    let writer = thread::spawn(move || {
        // Slow growth so stability cannot pass mid-write
        write_chunked(
            &dest_writer,
            10,
            128,
            Duration::from_millis(80),
        );
        *done_flag.lock().expect("done") = true;
    });

    // While still growing assert no ingest yet (event-driven samples not fixed sleeps)
    let growth_deadline = Instant::now() + Duration::from_millis(500);
    let mut saw_empty_during_growth = false;
    while Instant::now() < growth_deadline {
        if !*writer_done.lock().expect("flag") {
            if fx.file_count() == 0 {
                saw_empty_during_growth = true;
            } else {
                panic!("ingested before write finished / stable");
            }
        }
        thread::sleep(poll_interval);
    }
    assert!(
        saw_empty_during_growth,
        "expected at least one empty-DB sample during growth"
    );

    writer.join().expect("writer");
    assert!(
        wait_until(ingest_timeout, || fx.has_file_named("growing.mp4")),
        "file should ingest after stability"
    );
    assert!(
        wait_until(ingest_timeout, || fx.queued_job_count() >= 1),
        "phash job should be enqueued after ingest"
    );
    let files = get_all_files(&fx.db).expect("list");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file.size, 10 * 128);
}

#[test]
fn multi_file_copy_ingests_all() {
    let fx = Fixture::new("multi");
    let names = ["a.mp4", "b.mp4", "c.mp4"];
    for (i, name) in names.iter().enumerate() {
        let path = fx.path(name);
        fs::write(&path, vec![0xAAu8; 64 + i * 16]).expect("copy stub");
    }

    assert!(
        wait_until(ingest_timeout, || fx.file_count() == names.len()),
        "all copied videos should ingest, got {}",
        fx.file_count()
    );
    for name in names {
        assert!(fx.has_file_named(name), "missing {name}");
    }
    assert!(
        wait_until(ingest_timeout, || fx.queued_job_count() >= names.len()),
        "each ingest should enqueue work"
    );
}

#[test]
fn rename_into_watch_dir_ingests() {
    let fx = Fixture::new("rename");
    let outside = fx.temp.path().join("staging");
    fs::create_dir_all(&outside).expect("staging");
    let src = outside.join("clip.mp4");
    fs::write(&src, b"rename-payload-bytes-0123456789").expect("write outside");

    let dest = fx.path("renamed-in.mp4");
    fs::rename(&src, &dest).expect("rename into watch");

    assert!(
        wait_until(ingest_timeout, || fx.has_file_named("renamed-in.mp4")),
        "rename into watch dir should ingest"
    );
    assert!(
        wait_until(ingest_timeout, || fx.queued_job_count() >= 1),
        "rename ingest should enqueue"
    );
}

#[test]
fn delete_does_not_ingest_or_crash() {
    let fx = Fixture::new("delete");
    let path = fx.path("soon-gone.mp4");
    fs::write(&path, b"delete-me-video-stub-bytes").expect("write");
    assert!(
        wait_until(ingest_timeout, || fx.has_file_named("soon-gone.mp4")),
        "precondition ingest"
    );
    let count_before = fx.file_count();
    let jobs_before = fx.queued_job_count();

    fs::remove_file(&path).expect("delete");

    // Remove is ignored by design — no new rows / no panic; poll for quiescence
    let quiet_deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < quiet_deadline {
        assert!(fx.watcher.is_running(), "watcher must stay up after delete");
        assert_eq!(fx.file_count(), count_before, "delete must not add Files rows");
        assert!(
            fx.queued_job_count() <= jobs_before + 1,
            "delete must not flood the queue"
        );
        thread::sleep(poll_interval);
    }
}

#[test]
fn queue_recovers_after_restart_following_watcher_enqueue() {
    let fx = Fixture::new("recover");
    let path = fx.path("recover-me.mp4");
    fs::write(&path, b"recover-enqueue-stub-payload").expect("write");
    assert!(
        wait_until(ingest_timeout, || fx.has_file_named("recover-me.mp4")),
        "ingest before recovery"
    );
    assert!(
        wait_until(ingest_timeout, || fx.queued_job_count() >= 1),
        "job enqueued by watcher"
    );

    let job_id = {
        let guard = fx.db.lock().expect("lock");
        let jobs = guard
            .list_jobs_with_status(&["queued"])
            .expect("queued jobs");
        assert!(!jobs.is_empty(), "expected persisted queued job");
        jobs[0].id
    };

    // Simulate crash mid-run then process restart via #34 recovery API
    fx.db
        .lock()
        .expect("lock")
        .update_job_state(job_id, "running", 1, None, true)
        .expect("mark running");

    fx.watcher.stop().expect("stop watcher before restart");

    let queue2 = TaskQueue::with_concurrency(1);
    let n = queue2
        .recover_and_resume_for_test(Arc::clone(&fx.db))
        .expect("recover+resume");
    assert!(n >= 1, "recovered jobs must execute, n={n}");

    let row = fx.db.lock().expect("lock").get_job(job_id).expect("row");
    assert!(
        row.status == "succeeded" || row.status == "failed" || row.status == "queued",
        "after recovery job must leave bare running state, got {}",
        row.status
    );
    // Mid-run reset → drain should have attempted the job (terminal or re-queued retry)
    assert_ne!(row.status, "running", "running must not stick after recover");
}
