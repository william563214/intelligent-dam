//! Command-layer integration tests against temporary SQLite + settings.
//!
//! Uses injected State-style handles (`Arc<Mutex<Database>>`, etc.) rather than
//! a full UI. tauri-driver WebDriver E2E is intentionally deferred (see #35).
//!
//! Windows: `build.rs` embeds Common-Controls v6 so this harness can load
//! (avoids STATUS_ENTRYPOINT_NOT_FOUND via TaskDialogIndirect / comctl32).

use intelligent_dam_lib::test_support::{
    add_file_tag, clear_error_logs, clear_file_tags, current_schema_user_version,
    enqueue_pipeline_jobs, get_all_files, get_error_logs, get_file_tags, get_or_create_tag,
    get_settings, insert_error_log, list_duplicate_tombstones, remove_duplicate_tombstone,
    resolve_suspected_duplicate, resolve_suspected_duplicate_with_enqueue, save_settings,
    scan_directory, scan_multiple_directories, AppSettings, Database, PipelineOptions,
    ResolveSuspectedOutcome, SettingsStore, TaskQueue,
};
use std::fs;
use std::sync::{Arc, Mutex};

struct Fixture {
    db: Arc<Mutex<Database>>,
    settings: Mutex<SettingsStore>,
    queue: Arc<TaskQueue>,
    _temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("intelligent_dam.db");
        let db = Database::new(db_path.to_str().expect("utf8")).expect("open db");
        let settings = SettingsStore::new(temp.path());
        let queue = TaskQueue::with_concurrency(1);
        Self {
            db: Arc::new(Mutex::new(db)),
            settings: Mutex::new(settings),
            queue: Arc::new(queue),
            _temp: temp,
        }
    }

    fn temp(&self) -> &std::path::Path {
        self._temp.path()
    }
}

#[test]
fn fresh_temp_db_applies_user_version_migrations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fresh.db");
    let db = Database::new(path.to_str().expect("utf8")).expect("open");
    let version = db.read_user_version().expect("user_version");
    assert_eq!(version, current_schema_user_version());
    assert!(current_schema_user_version() >= 3);
}

#[test]
fn settings_roundtrip_and_load_defaults() {
    let fx = Fixture::new();
    let loaded = get_settings(&fx.settings).expect("default load");
    assert!(loaded.scan_directories.is_empty());

    let scan_dir = fx.temp().join("library");
    fs::create_dir_all(&scan_dir).expect("mkdir");
    let mut settings = AppSettings::default();
    settings.scan_directories = vec![scan_dir.to_string_lossy().to_string()];
    settings.stashdb_url = "https://example.test".to_string();

    let saved = save_settings(&fx.settings, settings).expect("save");
    assert_eq!(saved.stashdb_url, "https://example.test");
    assert_eq!(saved.scan_directories.len(), 1);

    let again = get_settings(&fx.settings).expect("reload");
    assert_eq!(again.stashdb_url, "https://example.test");
    assert_eq!(again.scan_directories, saved.scan_directories);
}

#[tokio::test]
async fn scan_ingest_lists_files_and_rejects_empty_multi_scan() {
    let fx = Fixture::new();
    let media = fx.temp().join("media");
    fs::create_dir_all(&media).expect("mkdir");
    // Non-video files are ignored; phash may fail on a tiny stub and that is fine.
    let video = media.join("clip.mp4");
    fs::write(&video, b"not a real video").expect("write video stub");
    let ignored = media.join("notes.txt");
    fs::write(&ignored, b"skip me").expect("write txt");

    let settings = AppSettings::default();
    let err = scan_multiple_directories(Vec::new(), &fx.db, &fx.queue, &settings)
        .await
        .expect_err("empty paths");
    assert!(err.contains("No directories provided"), "{err}");

    let result = scan_directory(
        media.to_string_lossy().to_string(),
        &fx.db,
        &fx.queue,
        &settings,
    )
    .await
    .expect("scan");
    assert_eq!(result.imported_count, 1);
    assert_eq!(result.file_ids.len(), 1);

    let files = get_all_files(&fx.db).expect("list");
    assert_eq!(files.len(), 1);
    assert!(
        std::path::Path::new(&files[0].file.path).ends_with("clip.mp4"),
        "path={}",
        files[0].file.path
    );
}

#[test]
fn tag_crud_via_command_wrappers() {
    let fx = Fixture::new();
    let media = fx.temp().join("tagged.mp4");
    fs::write(&media, b"stub").expect("write");

    let file_id = {
        let db = fx.db.lock().expect("lock");
        let (id, _) = db
            .upsert_file(media.to_str().expect("utf8"), 4, None)
            .expect("upsert");
        id
    };

    let tag_id = get_or_create_tag(&fx.db, "outdoor", Some("Tag".to_string())).expect("create");
    let again = get_or_create_tag(&fx.db, "outdoor", Some("Tag".to_string())).expect("idempotent");
    assert_eq!(tag_id, again);

    add_file_tag(&fx.db, file_id, tag_id, 1.0).expect("attach");
    let tags = get_file_tags(&fx.db, file_id).expect("list");
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].name, "outdoor");

    clear_file_tags(&fx.db, file_id).expect("clear");
    let empty = get_file_tags(&fx.db, file_id).expect("list empty");
    assert!(empty.is_empty());

    // Error path: attach to missing file should fail under FK
    let missing = add_file_tag(&fx.db, 9_999_999, tag_id, 1.0);
    assert!(missing.is_err(), "expected FK failure, got {missing:?}");
}

#[test]
fn error_log_insert_list_clear() {
    let fx = Fixture::new();
    let id = insert_error_log(
        &fx.db,
        "scrape",
        None,
        Some("/tmp/missing.mp4"),
        "scrape failed",
        Some("no network in test"),
    )
    .expect("insert");
    assert!(id > 0);

    let logs = get_error_logs(&fx.db, Some(10)).expect("list");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].category, "scrape");
    assert!(logs[0].message.contains("scrape failed"));

    let cleared = clear_error_logs(&fx.db).expect("clear");
    assert_eq!(cleared, 1);
    assert!(get_error_logs(&fx.db, None).expect("list").is_empty());
}

#[test]
fn enqueue_pipeline_empty_noop_and_skips_unknown_file_ids() {
    let fx = Fixture::new();
    let settings = AppSettings::default();

    // app: None binds DB without starting workers (no network / ffmpeg side effects)
    let empty = enqueue_pipeline_jobs(
        None,
        &fx.db,
        &fx.queue,
        &settings,
        Vec::new(),
        PipelineOptions::default(),
    )
    .expect("empty ok");
    assert_eq!(empty.enqueued, 0);
    assert!(empty.job_ids.is_empty());

    let noop = enqueue_pipeline_jobs(
        None,
        &fx.db,
        &fx.queue,
        &settings,
        vec![1],
        PipelineOptions {
            enable_scrape: false,
            enable_move: false,
            enable_preview: false,
        },
    )
    .expect_err("noop");
    assert!(noop.contains("disable all steps"), "{noop}");

    let skipped = enqueue_pipeline_jobs(
        None,
        &fx.db,
        &fx.queue,
        &settings,
        vec![42],
        PipelineOptions {
            enable_scrape: false,
            enable_move: false,
            enable_preview: true,
        },
    )
    .expect("missing id skipped");
    assert_eq!(skipped.enqueued, 0);
    assert_eq!(skipped.skipped, 1);
}

#[test]
fn enqueue_pipeline_queues_existing_on_disk_file() {
    let fx = Fixture::new();

    let media = fx.temp().join("queue_me.mp4");
    fs::write(&media, b"stub").expect("write");
    let file_id = {
        let db = fx.db.lock().expect("lock");
        let (id, _) = db
            .upsert_file(media.to_str().expect("utf8"), 4, None)
            .expect("upsert");
        id
    };

    // Preview-only avoids scrape/network; workers are not started (app: None).
    let result = enqueue_pipeline_jobs(
        None,
        &fx.db,
        &fx.queue,
        &AppSettings::default(),
        vec![file_id],
        PipelineOptions {
            enable_scrape: false,
            enable_move: false,
            enable_preview: true,
        },
    )
    .expect("enqueue");
    assert_eq!(result.enqueued, 1);
    assert_eq!(result.job_ids.len(), 1);

    // Second enqueue of same file should skip while still queued.
    let again = enqueue_pipeline_jobs(
        None,
        &fx.db,
        &fx.queue,
        &AppSettings::default(),
        vec![file_id],
        PipelineOptions {
            enable_scrape: false,
            enable_move: false,
            enable_preview: true,
        },
    )
    .expect("skip active");
    assert_eq!(again.enqueued, 0);
    assert_eq!(again.skipped, 1);
}

#[test]
fn resolve_suspected_confirm_ignore_and_errors() {
    let fx = Fixture::new();
    let keeper_path = fx.temp().join("cmd-keeper.mp4");
    let near_path = fx.temp().join("cmd-near.mp4");
    fs::write(&keeper_path, b"k").expect("write keeper");
    fs::write(&near_path, b"n").expect("write near");
    let keeper_path_str = keeper_path.to_string_lossy().to_string();
    let near_path_str = near_path.to_string_lossy().to_string();
    let (keeper, near) = {
        let db = fx.db.lock().expect("lock");
        let (keeper, _) = db
            .upsert_file(
                &keeper_path_str,
                1,
                Some("0000000000000000".into()),
            )
            .expect("keeper");
        let (near, _) = db
            .upsert_file(
                &near_path_str,
                2,
                Some("000000000000000f".into()),
            )
            .expect("near");
        db.mark_suspected_duplicate_atomic(near, keeper, 4, None)
            .expect("mark");
        (keeper, near)
    };

    let invalid = resolve_suspected_duplicate(&fx.db, &fx.queue, near, "nope");
    assert!(invalid.unwrap_err().contains("invalid action"));

    let not_suspected = resolve_suspected_duplicate(&fx.db, &fx.queue, keeper, "ignore");
    assert!(not_suspected
        .unwrap_err()
        .contains("not suspected_duplicate"));

    // Confirm path
    let confirmed =
        resolve_suspected_duplicate(&fx.db, &fx.queue, near, "confirm").expect("confirm");
    assert!(matches!(
        confirmed,
        ResolveSuspectedOutcome::Confirmed { file_id } if file_id == near
    ));
    {
        let db = fx.db.lock().expect("lock");
        assert!(db.get_file_by_id(near).is_err());
        assert!(db
            .get_duplicate_tombstone_by_path(&near_path_str)
            .expect("tombstone")
            .is_some());
    }

    // Fresh pair for ignore + no-pending
    let keeper2_path = fx.temp().join("cmd-keeper2.mp4");
    let near2_path = fx.temp().join("cmd-near2.mp4");
    fs::write(&keeper2_path, b"k2").expect("write k2");
    fs::write(&near2_path, b"n2").expect("write n2");
    let keeper2_path_str = keeper2_path.to_string_lossy().to_string();
    let near2_path_str = near2_path.to_string_lossy().to_string();
    let (keeper2, near2) = {
        let db = fx.db.lock().expect("lock");
        let (keeper2, _) = db
            .upsert_file(
                &keeper2_path_str,
                3,
                Some("ffffffffffffffff".into()),
            )
            .expect("k2");
        let (near2, _) = db
            .upsert_file(
                &near2_path_str,
                4,
                Some("fffffffffffffffc".into()),
            )
            .expect("n2");
        db.mark_suspected_duplicate_atomic(near2, keeper2, 2, None)
            .expect("mark2");
        (keeper2, near2)
    };

    let ignored =
        resolve_suspected_duplicate(&fx.db, &fx.queue, near2, "ignore").expect("ignore");
    match ignored {
        ResolveSuspectedOutcome::Ignored {
            file,
            pipeline_enqueued,
        } => {
            assert_eq!(file.file.id, near2);
            assert_eq!(file.file.status, "ready");
            assert!(!pipeline_enqueued);
        }
        other => panic!("expected Ignored, got {other:?}"),
    }

    {
        let db = fx.db.lock().expect("lock");
        db.update_file_status(near2, "suspected_duplicate")
            .expect("force status");
    }
    let no_pending = resolve_suspected_duplicate(&fx.db, &fx.queue, near2, "ignore");
    assert!(
        no_pending.unwrap_err().contains("no pending suspicion"),
        "expected no pending error"
    );
    let _ = keeper2;
}

#[test]
#[test]
fn confirm_failure_rolls_back_to_recoverable_state() {
    let fx = Fixture::new();
    let near_path = fx.temp().join("fail-near.mp4");
    fs::write(&near_path, b"n").expect("write");
    let keeper_path = fx.temp().join("fail-keeper.mp4");
    fs::write(&keeper_path, b"k").expect("write");
    let (keeper, near) = {
        let db = fx.db.lock().expect("lock");
        let (keeper, _) = db
            .upsert_file(
                keeper_path.to_str().unwrap(),
                1,
                Some("0000000000000000".into()),
            )
            .expect("k");
        let (near, _) = db
            .upsert_file(
                near_path.to_str().unwrap(),
                1,
                Some("000000000000000f".into()),
            )
            .expect("n");
        db.mark_suspected_duplicate_atomic(near, keeper, 4, None)
            .expect("mark");
        (keeper, near)
    };
    {
        let db = fx.db.lock().expect("lock");
        let err = db
            .confirm_suspected_duplicate_fail_after_resolve(near)
            .expect_err("injected fail");
        assert!(err.to_string().contains("injected"));
        assert!(db.get_file_by_id(near).is_ok(), "row must remain");
        assert_eq!(
            db.get_file_by_id(near).unwrap().status,
            "suspected_duplicate"
        );
        assert!(db
            .get_pending_suspected_duplicate(near)
            .unwrap()
            .is_some());
        assert!(db
            .get_duplicate_tombstone_by_path(near_path.to_str().unwrap())
            .unwrap()
            .is_none());
        // Recoverable: confirm can succeed after rollback
        db.confirm_suspected_duplicate(near).expect("retry confirm");
        assert!(db.get_file_by_id(near).is_err());
        assert!(db
            .get_duplicate_tombstone_by_path(near_path.to_str().unwrap())
            .unwrap()
            .is_some());
    }
    let _ = keeper;
}

#[test]
fn list_and_remove_duplicate_tombstones_commands() {
    let fx = Fixture::new();
    {
        let db = fx.db.lock().expect("lock");
        db.insert_duplicate_tombstone("/tmp/list-a.mp4", 10, Some("aa"), None)
            .expect("a");
        db.insert_duplicate_tombstone("/tmp/list-b.mp4", 20, None, None)
            .expect("b");
    }
    let list = list_duplicate_tombstones(&fx.db).expect("list");
    assert_eq!(list.len(), 2);
    let id = list[0].id;
    assert!(remove_duplicate_tombstone(&fx.db, id).expect("remove"));
    assert_eq!(list_duplicate_tombstones(&fx.db).expect("list2").len(), 1);
    assert!(!remove_duplicate_tombstone(&fx.db, id).expect("gone"));
}

#[test]
fn ignore_enqueues_pipeline_when_options_enabled() {
    let fx = Fixture::new();
    let near_path = fx.temp().join("enq-near.mp4");
    let keeper_path = fx.temp().join("enq-keeper.mp4");
    fs::write(&near_path, b"n").expect("write");
    fs::write(&keeper_path, b"k").expect("write");
    let near = {
        let db = fx.db.lock().expect("lock");
        let (keeper, _) = db
            .upsert_file(
                keeper_path.to_str().unwrap(),
                1,
                Some("ffffffffffffffff".into()),
            )
            .expect("k");
        let (near, _) = db
            .upsert_file(
                near_path.to_str().unwrap(),
                1,
                Some("fffffffffffffffc".into()),
            )
            .expect("n");
        db.mark_suspected_duplicate_atomic(near, keeper, 2, None)
            .expect("mark");
        near
    };
    let settings = AppSettings::default();
    let outcome = resolve_suspected_duplicate_with_enqueue(
        &fx.db,
        &fx.queue,
        near,
        "ignore",
        None,
        Some(&settings),
        Some(PipelineOptions {
            enable_scrape: false,
            enable_move: false,
            enable_preview: true,
        }),
    )
    .expect("ignore+enqueue");
    match outcome {
        ResolveSuspectedOutcome::Ignored {
            pipeline_enqueued, ..
        } => assert!(pipeline_enqueued),
        other => panic!("expected Ignored, got {other:?}"),
    }
}

#[test]
fn deleting_keeper_restores_flagged_file_to_ready() {
    let fx = Fixture::new();
    let keeper_path = fx.temp().join("del-keeper.mp4");
    let near_path = fx.temp().join("del-near.mp4");
    fs::write(&keeper_path, b"k").expect("write");
    fs::write(&near_path, b"n").expect("write");
    let db = fx.db.lock().expect("lock");
    let (keeper, _) = db
        .upsert_file(
            keeper_path.to_str().expect("utf8"),
            1,
            Some("0000000000000000".into()),
        )
        .expect("keeper");
    let (near, _) = db
        .upsert_file(
            near_path.to_str().expect("utf8"),
            2,
            Some("000000000000000f".into()),
        )
        .expect("near");
    db.mark_suspected_duplicate_atomic(near, keeper, 4, None)
        .expect("mark");
    assert_eq!(
        db.get_file_by_id(near).unwrap().status,
        "suspected_duplicate"
    );
    db.delete_file_record(keeper).expect("delete keeper");
    assert_eq!(db.get_file_by_id(near).unwrap().status, "ready");
    assert!(db
        .get_pending_suspected_duplicate(near)
        .unwrap()
        .is_none());
}
