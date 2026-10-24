//! Watcher end-to-end: notify → stability → ingest → queue (#36).
//!
//! Lives under `tests/` so Windows harnesses pick up Common-Controls v6 via
//! `cargo:rustc-link-arg-tests` (see build.rs). Requires `--features test-support`.

#[cfg(windows)]
use fs4::FileExt;
use intelligent_dam_lib::test_support::{
    get_all_files, Database, PipelineOptions, TaskQueue, WatcherManager, WatcherOptions,
};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    queue: Arc<TaskQueue>,
    watcher: WatcherManager,
}

impl Fixture {
    fn new(label: &str) -> Self {
        Self::with_options(label, WatcherOptions::for_tests())
    }

    /// Auto-pipeline on (non-noop) so watcher enqueues pipeline after phash.
    fn with_auto_pipeline(label: &str) -> Self {
        let mut opts = WatcherOptions::for_tests();
        opts.pipeline_override = Some(PipelineOptions {
            enable_scrape: true,
            enable_move: false,
            enable_preview: false,
        });
        Self::with_options(label, opts)
    }

    fn with_options(label: &str, options: WatcherOptions) -> Self {
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
                options,
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
            queue,
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
            Path::new(&f.file.path)
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
fn paused_then_resumed_write_ingests_only_after_completion() {
    // Pause longer than for_watcher_e2e stability (~500ms) while a *child*
    // process holds the write handle (+ advisory lock). Same-process flock on
    // macOS is process-scoped so a same-thread writer made the probe Free and
    // ingested size 512 mid-pause (#93 / #68). Cross-process lock keeps Held.
    let fx = Fixture::new("pause_resume");
    let dest = fx.path("pause-resume.mp4");
    let half = 512usize;
    let total = half * 2;
    let mid = dest.with_extension("mid");
    let _ = fs::remove_file(&mid);

    let pause_ms = 1800u64;
    let mut writer = Command::new(env!("CARGO_BIN_EXE_idam_pause_writer"))
        .args([
            dest.to_str().expect("utf8 dest"),
            &half.to_string(),
            &pause_ms.to_string(),
        ])
        .spawn()
        .expect("spawn cross-process pause writer");

    assert!(
        wait_until(ingest_timeout, || mid.exists()),
        "writer should reach mid-pause"
    );
    let pause_started = Instant::now();
    // Deadline anchored to mid signal (not when main noticed the flag)
    let pause_deadline = pause_started + Duration::from_millis(1400);

    let mut samples = 0u32;
    while Instant::now() < pause_deadline {
        assert!(
            writer.try_wait().expect("writer poll").is_none(),
            "writer finished earlier than expected"
        );
        assert_eq!(
            fx.file_count(),
            0,
            "must not ingest during mid-write pause (sample {samples})"
        );
        samples += 1;
        thread::sleep(poll_interval);
    }
    assert!(samples >= 10, "need enough pause samples, got {samples}");

    let status = writer.wait().expect("writer wait");
    assert!(status.success(), "pause writer exit {status}");

    assert!(
        wait_until(ingest_timeout, || fx.has_file_named("pause-resume.mp4")),
        "ingest only after true write completion"
    );
    let files = get_all_files(&fx.db).expect("list");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file.size, total as u64);
    assert!(
        wait_until(ingest_timeout, || fx.queued_job_count() >= 1),
        "phash job after final ingest"
    );
}

/// Windows: only an open write handle (no LockFileEx) must block mid-pause ingest.
#[cfg(windows)]
#[test]
fn windows_paused_write_handle_only_blocks_until_complete() {
    let fx = Fixture::new("win_write_only");
    let dest = fx.path("win-write-only.mp4");
    let half = 512usize;
    let total = half * 2;
    let writer_done = Arc::new(Mutex::new(false));
    let mid_at = Arc::new(Mutex::new(None::<Instant>));
    let done_flag = Arc::clone(&writer_done);
    let mid_flag = Arc::clone(&mid_at);
    let dest_writer = dest.clone();

    let writer = thread::spawn(move || {
        let mut file = File::create(&dest_writer).expect("create");
        file.write_all(&vec![0x33u8; half]).expect("first half");
        file.flush().expect("flush half");
        // No try_lock — share probe alone must keep the gate waiting
        *mid_flag.lock().expect("mid") = Some(Instant::now());
        thread::sleep(Duration::from_millis(1800));
        file.write_all(&vec![0x44u8; half]).expect("second half");
        file.flush().expect("flush full");
        drop(file);
        *done_flag.lock().expect("done") = true;
    });

    assert!(
        wait_until(ingest_timeout, || mid_at.lock().expect("mid").is_some()),
        "writer mid-pause"
    );
    let pause_started = mid_at.lock().expect("mid").expect("writer mid_at");
    let pause_deadline = pause_started + Duration::from_millis(1400);
    while Instant::now() < pause_deadline {
        assert_eq!(fx.file_count(), 0, "must not ingest with write handle open");
        thread::sleep(poll_interval);
    }

    writer.join().expect("writer");
    assert!(
        wait_until(ingest_timeout, || fx.has_file_named("win-write-only.mp4")),
        "ingest after write handle released"
    );
    let files = get_all_files(&fx.db).expect("list");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file.size, total as u64);
}

/// Windows: a read-only holder (preview / seeder style) must not block ingest.
#[cfg(windows)]
#[test]
fn windows_readonly_holder_still_ingests() {
    let fx = Fixture::new("win_ro_ingest");
    let dest = fx.path("win-ro.mp4");
    fs::write(&dest, b"readonly-holder-e2e-payload-bytes").expect("write");
    let reader = File::open(&dest).expect("read-only hold");

    assert!(
        wait_until(ingest_timeout, || fx.has_file_named("win-ro.mp4")),
        "read-only holder must not block ingest"
    );
    drop(reader);
    assert!(
        wait_until(ingest_timeout, || fx.queued_job_count() >= 1),
        "enqueue after readonly ingest"
    );
}

#[test]
fn chunked_write_waits_for_stability_then_ingests_and_enqueues() {
    let fx = Fixture::new("chunked");
    let dest = fx.path("growing.mp4");
    let writer_done = Arc::new(Mutex::new(false));
    let done_flag = Arc::clone(&writer_done);
    let dest_writer = dest.clone();

    let writer = thread::spawn(move || {
        // 25ms between chunks vs for_watcher_e2e 100ms x5 (~500ms) — clear margin
        write_chunked(
            &dest_writer,
            10,
            128,
            Duration::from_millis(25),
        );
        *done_flag.lock().expect("done") = true;
    });

    // While still growing assert no ingest yet (event-driven samples not fixed sleeps)
    let growth_deadline = Instant::now() + Duration::from_millis(200);
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
    // Wait for enqueue to finish before snapshotting (DB row can land first)
    assert!(
        wait_until(ingest_timeout, || fx.queued_job_count() >= 1),
        "precondition enqueue"
    );
    let count_before = fx.file_count();
    let jobs_before = fx.queued_job_count();

    fs::remove_file(&path).expect("delete");

    // Remove is ignored by design — no new rows / no panic; poll for quiescence
    let quiet_deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < quiet_deadline {
        assert!(fx.watcher.is_running(), "watcher must stay up after delete");
        assert_eq!(fx.file_count(), count_before, "delete must not add Files rows");
        assert_eq!(
            fx.queued_job_count(),
            jobs_before,
            "delete must not enqueue additional jobs"
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

    // Drain may auto-retry; wait until the job is terminal (not queued/running)
    assert!(
        wait_until(ingest_timeout, || {
            let status = fx
                .db
                .lock()
                .expect("lock")
                .get_job(job_id)
                .expect("row")
                .status;
            status == "succeeded" || status == "failed"
        }),
        "after recovery job must end succeeded or failed"
    );
    let row = fx.db.lock().expect("lock").get_job(job_id).expect("row");
    assert!(
        row.status == "succeeded" || row.status == "failed",
        "unexpected terminal status {}",
        row.status
    );
}

/// B1 + THE RULE: organized keeper K; drop exact + near copies with auto pipeline on.
/// New rows are deleted/flagged; K unchanged; queued pipelines soft-skip (succeeded, not Failed).
#[test]
fn auto_pipeline_exact_deleted_near_flagged_keeper_untouched() {
    let fx = Fixture::with_auto_pipeline("dedup_rule");
    let keeper_phash = "0000000000000000";
    let near_phash = "000000000000000f"; // hamming distance 4

    // Organized keeper outside the watch tree (not re-ingested)
    let lib = fx.temp.path().join("library");
    fs::create_dir_all(&lib).expect("lib");
    let keeper_path = lib.join("keeper.mp4");
    fs::write(&keeper_path, b"organized-keeper-bytes").expect("write k");
    let keeper_id = {
        let guard = fx.db.lock().expect("lock");
        let (id, _) = guard
            .upsert_file(
                keeper_path.to_str().expect("utf8"),
                22,
                Some(keeper_phash.into()),
            )
            .expect("insert k");
        guard.mark_file_organized(id).expect("organized_at");
        id
    };

    let exact_path = fx.path("exact-copy.mp4");
    let near_path = fx.path("near-copy.mp4");
    fs::write(&exact_path, b"exact-copy-stub-bytes!!").expect("exact");
    fs::write(
        PathBuf::from(format!("{}.phash", exact_path.display())),
        keeper_phash,
    )
    .expect("exact sidecar");
    fs::write(&near_path, b"near-copy-stub-bytes!!!").expect("near");
    fs::write(
        PathBuf::from(format!("{}.phash", near_path.display())),
        near_phash,
    )
    .expect("near sidecar");

    assert!(
        wait_until(ingest_timeout, || {
            fx.has_file_named("exact-copy.mp4") && fx.has_file_named("near-copy.mp4")
        }),
        "both copies should ingest before phash drain"
    );

    let (exact_id, near_id) = {
        let files = get_all_files(&fx.db).expect("list");
        let exact = files
            .iter()
            .find(|f| {
                Path::new(&f.file.path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    == Some("exact-copy.mp4")
            })
            .expect("exact row")
            .file
            .id;
        let near = files
            .iter()
            .find(|f| {
                Path::new(&f.file.path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    == Some("near-copy.mp4")
            })
            .expect("near row")
            .file
            .id;
        (exact, near)
    };
    assert_ne!(exact_id, keeper_id);
    assert_ne!(near_id, keeper_id);

    // Wait until watcher has enqueued phash+pipeline for both (4 jobs) or at least
    // one pipeline each — then drain so phash runs before pipeline (same-file serial).
    assert!(
        wait_until(ingest_timeout, || fx.queued_job_count() >= 4),
        "expected phash+pipeline for each copy, got {}",
        fx.queued_job_count()
    );

    let finished = fx.queue.drain_for_test().expect("drain");
    assert!(finished >= 4, "drain finished {finished}");

    // Exact deleted; near flagged; keeper untouched
    assert!(
        fx.db.lock().expect("lock").get_file_by_id(exact_id).is_err(),
        "exact copy must be auto-deleted"
    );
    let near_row = fx
        .db
        .lock()
        .expect("lock")
        .get_file_by_id(near_id)
        .expect("near survives as suspected");
    assert_eq!(near_row.status, "suspected_duplicate");
    let keeper = fx
        .db
        .lock()
        .expect("lock")
        .get_file_by_id(keeper_id)
        .expect("keeper");
    assert_ne!(keeper.status, "suspected_duplicate");
    assert!(
        fx.db
            .lock()
            .expect("lock")
            .is_organized_file(keeper_id)
            .expect("org"),
        "keeper remains organized"
    );
    assert!(
        fx.db
            .lock()
            .expect("lock")
            .get_pending_suspected_duplicate(keeper_id)
            .expect("q")
            .is_none(),
        "keeper must never be auto-flagged"
    );

    // Pipeline jobs for both copies: succeeded soft-skip (not Failed / not retried)
    let jobs = fx
        .db
        .lock()
        .expect("lock")
        .list_jobs_with_status(&["succeeded", "failed", "queued", "running"])
        .expect("jobs");
    let pipeline_for = |fid: i64| {
        jobs.iter()
            .filter(|j| {
                j.job_type == "pipeline"
                    && j.payload.contains(&format!("\"file_id\":{fid}"))
            })
            .collect::<Vec<_>>()
    };
    let exact_pipes = pipeline_for(exact_id);
    let near_pipes = pipeline_for(near_id);
    assert!(
        !exact_pipes.is_empty(),
        "exact must have had a pipeline job enqueued"
    );
    assert!(
        !near_pipes.is_empty(),
        "near must have had a pipeline job enqueued"
    );
    for j in exact_pipes.iter().chain(near_pipes.iter()) {
        assert_eq!(
            j.status, "succeeded",
            "pipeline for file_id payload {} must soft-skip as succeeded, got {} err={:?}",
            j.payload, j.status, j.error
        );
        assert!(
            j.error.is_none() || j.error.as_deref() == Some(""),
            "soft-skip must not record a failure error: {:?}",
            j.error
        );
    }
}
