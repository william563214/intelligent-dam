//! Command-layer integration tests against temporary SQLite + settings.
//!
//! Uses injected State-style handles (`Arc<Mutex<Database>>`, etc.) rather than
//! a full UI. tauri-driver WebDriver E2E is intentionally deferred (see #35).
//!
//! Windows: `build.rs` embeds Common-Controls v6 so this harness can load
//! (avoids STATUS_ENTRYPOINT_NOT_FOUND via TaskDialogIndirect / comctl32).

use intelligent_dam_lib::test_support::{
    add_file_tag, clear_error_logs, clear_file_tags, current_schema_user_version,
    discard_overwrite_backup, enqueue_pipeline_jobs, get_all_files, get_error_logs, get_file_tags,
    get_or_create_tag, get_settings, heal_identity_override, insert_error_log,
    is_same_file_identity, list_duplicate_tombstones, move_file_overwrite_via_copy, overwrite_fault,
    path_collision_tx_fault, preview_output_template, remove_duplicate_tombstone,
    resolve_path_collision, resolve_suspected_duplicate, resolve_suspected_duplicate_with_enqueue,
    save_settings, scan_directory, scan_multiple_directories, AppSettings, Database, Metadata,
    PipelineOptions, ResolvePathCollisionOutcome, ResolveSuspectedOutcome, SettingsStore,
    TaskQueue,
};
use std::fs;
use std::path::Path;
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
    assert!(current_schema_user_version() >= 6);
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

#[test]
fn preview_output_template_renders_sample_with_helpers() {
    let preview = preview_output_template(
        "{{studio}}/{{format_date date \"YYYY\"}}_{{truncate title 6}}.{{ext}}",
        Some("/library"),
    )
    .expect("preview");
    assert!(
        preview.rendered_path.ends_with("StudioX/2024_Sam....mp4")
            || preview.rendered_path.replace('\\', "/").ends_with("StudioX/2024_Sam....mp4"),
        "got {}",
        preview.rendered_path
    );
    assert_eq!(preview.base_dir, "/library");
}

#[test]
fn path_collision_suffix_skip_overwrite_flow() {
    let fx = Fixture::new();
    let media = fx.temp().join("lib");
    fs::create_dir_all(media.join("StudioA")).expect("mkdir");
    let occupied = media.join("StudioA").join("Demo.mp4");
    let incoming = media.join("incoming.mp4");
    fs::write(&occupied, b"old-bytes").expect("write occupied");
    fs::write(&incoming, b"new-bytes").expect("write incoming");

    let (file_id, occupant_id) = {
        let db = fx.db.lock().expect("lock");
        let (occupant, _) = db
            .upsert_file(occupied.to_str().expect("utf8"), 3, None)
            .expect("occupant");
        let (id, _) = db
            .upsert_file(incoming.to_str().expect("utf8"), 4, None)
            .expect("incoming");
        db.update_metadata(&Metadata {
            file_id: id,
            title: Some("Demo".to_string()),
            studio: Some("StudioA".to_string()),
            release_date: None,
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".to_string()),
        })
        .expect("meta");
        db.mark_path_collision_atomic(
            id,
            occupied.to_str().expect("utf8"),
            Some(occupant),
        )
        .expect("mark collision");
        (id, occupant)
    };

    let denied = resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", false);
    assert!(denied.is_err(), "overwrite needs confirm");

    let skip = resolve_path_collision(&fx.db, &fx.queue, file_id, "skip", false).expect("skip");
    match skip {
        ResolvePathCollisionOutcome::Skip { file } => {
            assert_eq!(file.file.id, file_id);
            assert_eq!(file.file.status, "ready");
            assert!(file.path_collision.is_none());
        }
        other => panic!("expected Skip, got {other:?}"),
    }
    assert!(incoming.exists());
    assert_eq!(fs::read(&occupied).unwrap(), b"old-bytes");

    {
        let db = fx.db.lock().expect("lock");
        db.mark_path_collision_atomic(
            file_id,
            occupied.to_str().expect("utf8"),
            Some(occupant_id),
        )
        .expect("re-mark");
    }
    let suffix =
        resolve_path_collision(&fx.db, &fx.queue, file_id, "suffix", false).expect("suffix");
    match suffix {
        ResolvePathCollisionOutcome::Suffix { file, new_path } => {
            assert_eq!(file.file.status, "ready");
            assert!(Path::new(&new_path).exists());
            assert!(
                new_path.contains("Demo_2"),
                "expected suffix path, got {new_path}"
            );
            assert!(!incoming.exists());
            assert_eq!(fs::read(&occupied).unwrap(), b"old-bytes");
            assert_eq!(fs::read(&new_path).unwrap(), b"new-bytes");
        }
        other => panic!("expected Suffix, got {other:?}"),
    }

    let incoming2 = media.join("incoming2.mp4");
    fs::write(&incoming2, b"overwrite-me").expect("write");
    let file2 = {
        let db = fx.db.lock().expect("lock");
        let (id, _) = db
            .upsert_file(incoming2.to_str().expect("utf8"), 5, None)
            .expect("incoming2");
        db.mark_path_collision_atomic(
            id,
            occupied.to_str().expect("utf8"),
            Some(occupant_id),
        )
        .expect("mark2");
        id
    };
    let overwrite =
        resolve_path_collision(&fx.db, &fx.queue, file2, "overwrite", true).expect("overwrite");
    match overwrite {
        ResolvePathCollisionOutcome::Overwrite { file, new_path } => {
            assert_eq!(file.file.status, "ready");
            assert_eq!(Path::new(&new_path), occupied.as_path());
            assert_eq!(fs::read(&occupied).unwrap(), b"overwrite-me");
            assert!(!incoming2.exists());
        }
        other => panic!("expected Overwrite, got {other:?}"),
    }
    let (occupant_status, logs) = {
        let db = fx.db.lock().expect("lock");
        (
            db.get_file_by_id(occupant_id).expect("occupant").status,
            db.list_error_logs(50).expect("logs"),
        )
    };
    assert_eq!(occupant_status, "missing");
    assert!(
        logs.iter().any(|e| {
            e.category == "path_collision"
                && e.message.contains("occupant row overwritten")
                && e.file_id == Some(occupant_id)
        }),
        "expected ErrorLog when occupant overwritten, got {logs:?}"
    );
}

#[test]
fn path_collision_overwrite_fs_failure_leaves_pending_and_occupant() {
    let fx = Fixture::new();
    let media = fx.temp().join("fsfail");
    fs::create_dir_all(&media).expect("mkdir");
    let occupied = media.join("Demo.mp4");
    let incoming = media.join("incoming.mp4");
    fs::write(&occupied, b"old-bytes").expect("write occupied");
    fs::write(&incoming, b"new-bytes").expect("write incoming");

    let (file_id, occupant_id, occupant_path) = {
        let db = fx.db.lock().expect("lock");
        let (occupant, _) = db
            .upsert_file(occupied.to_str().expect("utf8"), 3, None)
            .expect("occupant");
        let (id, _) = db
            .upsert_file(incoming.to_str().expect("utf8"), 4, None)
            .expect("incoming");
        db.mark_path_collision_atomic(
            id,
            occupied.to_str().expect("utf8"),
            Some(occupant),
        )
        .expect("mark");
        let occ_path = db.get_file_by_id(occupant).expect("occ").path;
        (id, occupant, occ_path)
    };

    overwrite_fault::arm_fail_after_backup();
    let err = resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true)
        .expect_err("fs fail");
    overwrite_fault::clear();
    assert!(err.contains("injected") || err.contains("overwrite"), "{err}");

    assert!(occupied.exists());
    assert_eq!(fs::read(&occupied).unwrap(), b"old-bytes");
    assert!(incoming.exists());
    assert_eq!(fs::read(&incoming).unwrap(), b"new-bytes");

    let db = fx.db.lock().expect("lock");
    let mover = db.get_file_by_id(file_id).expect("mover");
    assert_eq!(mover.status, "path_collision");
    assert!(db.get_pending_path_collision(file_id).unwrap().is_some());
    let occupant = db.get_file_by_id(occupant_id).expect("occupant");
    assert_eq!(occupant.path, occupant_path);
    assert_ne!(occupant.status, "missing");
}

#[test]
fn path_collision_overwrite_restore_failure_writes_error_log() {
    let fx = Fixture::new();
    let media = fx.temp().join("rbfail");
    fs::create_dir_all(&media).expect("mkdir");
    let occupied = media.join("Demo.mp4");
    let incoming = media.join("incoming.mp4");
    fs::write(&occupied, b"old-bytes").expect("write occupied");
    fs::write(&incoming, b"new-bytes").expect("write incoming");

    let file_id = {
        let db = fx.db.lock().expect("lock");
        let (occupant, _) = db
            .upsert_file(occupied.to_str().expect("utf8"), 3, None)
            .expect("occupant");
        let (id, _) = db
            .upsert_file(incoming.to_str().expect("utf8"), 4, None)
            .expect("incoming");
        db.mark_path_collision_atomic(
            id,
            occupied.to_str().expect("utf8"),
            Some(occupant),
        )
        .expect("mark");
        id
    };

    overwrite_fault::arm_fail_after_backup();
    overwrite_fault::arm_fail_restore();
    let err = resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true)
        .expect_err("restore fail");
    overwrite_fault::clear();
    assert!(
        err.contains("restore also failed") || err.contains("idam-bak"),
        "{err}"
    );
    let bak = media.join("Demo.mp4.idam-bak");
    assert!(bak.exists(), "occupant must remain in .idam-bak");
    assert_eq!(fs::read(&bak).unwrap(), b"old-bytes");

    let logs = get_error_logs(&fx.db, Some(20)).expect("logs");
    assert!(
        logs.iter().any(|e| {
            e.category == "path_collision"
                && (e.message.contains("restore also failed")
                    || e.message.contains("idam-bak")
                    || e.detail.as_deref().unwrap_or("").contains("idam-bak"))
        }),
        "expected ErrorLog for restore failure, got {logs:?}"
    );
}

#[test]
fn path_collision_overwrite_db_failure_restores_files() {
    let fx = Fixture::new();
    let media = fx.temp().join("dbfail");
    fs::create_dir_all(&media).expect("mkdir");
    let occupied = media.join("Demo.mp4");
    let incoming = media.join("incoming.mp4");
    fs::write(&occupied, b"old-bytes").expect("write occupied");
    fs::write(&incoming, b"new-bytes").expect("write incoming");

    let (file_id, occupant_id, occupant_path, mover_path) = {
        let db = fx.db.lock().expect("lock");
        let (occupant, _) = db
            .upsert_file(occupied.to_str().expect("utf8"), 3, None)
            .expect("occupant");
        let (id, _) = db
            .upsert_file(incoming.to_str().expect("utf8"), 4, None)
            .expect("incoming");
        db.mark_path_collision_atomic(
            id,
            occupied.to_str().expect("utf8"),
            Some(occupant),
        )
        .expect("mark");
        (
            id,
            occupant,
            db.get_file_by_id(occupant).unwrap().path,
            db.get_file_by_id(id).unwrap().path,
        )
    };

    path_collision_tx_fault::arm_fail_commit();
    let err = resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true)
        .expect_err("db fail");
    path_collision_tx_fault::clear();
    assert!(
        err.contains("injected") || err.contains("commit"),
        "{err}"
    );

    assert!(occupied.exists(), "occupant file restored");
    assert_eq!(fs::read(&occupied).unwrap(), b"old-bytes");
    assert!(incoming.exists(), "source restored");
    assert_eq!(fs::read(&incoming).unwrap(), b"new-bytes");

    let db = fx.db.lock().expect("lock");
    let mover = db.get_file_by_id(file_id).expect("mover");
    assert_eq!(mover.status, "path_collision");
    assert_eq!(mover.path, mover_path);
    assert!(db.get_pending_path_collision(file_id).unwrap().is_some());
    let occupant = db.get_file_by_id(occupant_id).expect("occupant");
    assert_eq!(occupant.path, occupant_path);
    assert_ne!(occupant.status, "missing");
}

#[test]
fn path_collision_overwrite_requeries_when_occupant_already_moved() {
    let fx = Fixture::new();
    let media = fx.temp().join("stale_occ");
    fs::create_dir_all(&media).expect("mkdir");
    let occupied = media.join("Demo.mp4");
    let incoming = media.join("incoming.mp4");
    let moved_away = media.join("elsewhere.mp4");
    fs::write(&occupied, b"orphan-on-disk").expect("write occupied");
    fs::write(&incoming, b"new-bytes").expect("write incoming");
    // Keep a real file at the occupant row's new path so reconcile does not mark missing
    fs::write(&moved_away, b"occupant-elsewhere").expect("write elsewhere");

    let (file_id, stale_occupant) = {
        let db = fx.db.lock().expect("lock");
        let (occupant, _) = db
            .upsert_file(occupied.to_str().expect("utf8"), 3, None)
            .expect("occupant");
        let (id, _) = db
            .upsert_file(incoming.to_str().expect("utf8"), 4, None)
            .expect("incoming");
        db.mark_path_collision_atomic(
            id,
            occupied.to_str().expect("utf8"),
            Some(occupant),
        )
        .expect("mark");
        // Occupant row moved away; disk file at desired is now untracked
        db.update_file_path(occupant, moved_away.to_str().expect("utf8"))
            .expect("move occupant path");
        (id, occupant)
    };

    let overwrite =
        resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true).expect("overwrite");
    match overwrite {
        ResolvePathCollisionOutcome::Overwrite { file, new_path } => {
            assert_eq!(file.file.status, "ready");
            assert_eq!(Path::new(&new_path), occupied.as_path());
            assert_eq!(fs::read(&occupied).unwrap(), b"new-bytes");
        }
        other => panic!("expected Overwrite, got {other:?}"),
    }
    let stale = {
        let db = fx.db.lock().expect("lock");
        db.get_file_by_id(stale_occupant).expect("stale")
    };
    assert_eq!(stale.path, moved_away.to_string_lossy());
    assert_ne!(stale.status, "missing", "stale occupant must not be marked missing");
}

#[test]
fn path_collision_overwrite_via_copy_helper_roundtrip() {
    // Unit-level cross-volume path exercised through public test helper
    let dir = tempfile::tempdir().expect("temp");
    let src = dir.path().join("new.mp4");
    let dest = dir.path().join("old.mp4");
    fs::write(&src, b"new-bytes").expect("src");
    fs::write(&dest, b"old-bytes").expect("dest");
    let outcome = move_file_overwrite_via_copy(&src, &dest).expect("copy ovw");
    assert!(!outcome.case_only);
    assert!(!src.exists());
    assert_eq!(fs::read(&dest).unwrap(), b"new-bytes");
    discard_overwrite_backup(outcome.backup.as_deref());
}

#[test]
fn path_collision_overwrite_crash_before_commit_recovers_on_restart() {
    // #59 N4 / B1: FS overwrite done, DB not committed → restart finishes commit
    // and keeps `.idam-bak` for manual cleanup
    let fx = Fixture::new();
    let media = fx.temp().join("n4_crash");
    fs::create_dir_all(&media).expect("mkdir");
    let occupied = media.join("Demo.mp4");
    let incoming = media.join("incoming.mp4");
    fs::write(&occupied, b"old-bytes").expect("write occupied");
    fs::write(&incoming, b"new-bytes").expect("write incoming");

    let (file_id, occupant_id, mover_path) = {
        let db = fx.db.lock().expect("lock");
        let (occupant, _) = db
            .upsert_file(occupied.to_str().expect("utf8"), 9, None)
            .expect("occupant");
        let (id, _) = db
            .upsert_file(incoming.to_str().expect("utf8"), 9, None)
            .expect("incoming");
        db.mark_path_collision_atomic(id, occupied.to_str().expect("utf8"), Some(occupant))
            .expect("mark");
        (id, occupant, db.get_file_by_id(id).unwrap().path)
    };

    // Simulate crash after FS success / before commit: mover at dest, occupant in bak
    let bak = media.join("Demo.mp4.idam-bak");
    fs::rename(&occupied, &bak).expect("bak");
    fs::rename(&incoming, &occupied).expect("mover to dest");
    assert!(!Path::new(&mover_path).exists());
    assert_eq!(fs::read(&occupied).unwrap(), b"new-bytes");
    assert!(bak.exists());

    let outcome =
        resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true).expect("recover");
    match outcome {
        ResolvePathCollisionOutcome::Overwrite { file, new_path } => {
            assert_eq!(file.file.status, "ready");
            assert_eq!(Path::new(&new_path), occupied.as_path());
            assert_eq!(fs::read(&occupied).unwrap(), b"new-bytes");
        }
        other => panic!("expected Overwrite recovery, got {other:?}"),
    }
    assert!(bak.exists(), "bak must be kept for manual cleanup (B1)");
    assert_eq!(fs::read(&bak).unwrap(), b"old-bytes");
    let db = fx.db.lock().expect("lock");
    let mover = db.get_file_by_id(file_id).expect("mover");
    assert_eq!(Path::new(&mover.path), occupied.as_path());
    let occupant = db.get_file_by_id(occupant_id).expect("occupant");
    assert_eq!(occupant.status, "missing");
    assert!(occupant.path.contains("#displaced-"));
    assert!(
        db.list_error_logs(20).unwrap().iter().any(|e| {
            e.category == "path_collision"
                && e.message.contains("completed overwrite after crash")
                && e.detail.as_deref().unwrap_or("").contains("Demo.mp4.idam-bak")
        }),
        "recovery should log bak path for cleanup"
    );
}

fn setup_crashed_overwrite(
    fx: &Fixture,
    media_name: &str,
    mover_bytes: &[u8],
    bak_bytes: &[u8],
    mover_phash: Option<&str>,
) -> (i64, std::path::PathBuf, std::path::PathBuf, String) {
    let media = fx.temp().join(media_name);
    fs::create_dir_all(&media).expect("mkdir");
    let occupied = media.join("Demo.mp4");
    let incoming = media.join("incoming.mp4");
    fs::write(&occupied, bak_bytes).expect("occupied");
    fs::write(&incoming, mover_bytes).expect("incoming");
    let file_id = {
        let db = fx.db.lock().expect("lock");
        let (occupant, _) = db
            .upsert_file(
                occupied.to_str().expect("utf8"),
                bak_bytes.len() as u64,
                None,
            )
            .expect("occupant");
        let (id, _) = db
            .upsert_file(
                incoming.to_str().expect("utf8"),
                mover_bytes.len() as u64,
                mover_phash.map(|s| s.to_string()),
            )
            .expect("incoming");
        db.mark_path_collision_atomic(id, occupied.to_str().expect("utf8"), Some(occupant))
            .expect("mark");
        id
    };
    let bak = media.join("Demo.mp4.idam-bak");
    fs::rename(&occupied, &bak).expect("bak");
    fs::rename(&incoming, &occupied).expect("mover to dest");
    let mover_path = {
        let db = fx.db.lock().expect("lock");
        db.get_file_by_id(file_id).unwrap().path
    };
    (file_id, occupied, bak, mover_path)
}

#[test]
fn path_collision_overwrite_crash_defers_when_source_parent_unreachable() {
    // S-A: offline volume → retryable; mover stays path_collision; bak kept
    let fx = Fixture::new();
    let (file_id, _occupied, bak, _mover_path) = setup_crashed_overwrite(
        &fx,
        "n4_offline",
        b"new-bytes",
        b"old-bytes",
        None,
    );
    let offline_path = "/idam-offline-vol-xyz/media/incoming.mp4";
    {
        let db = fx.db.lock().expect("lock");
        db.update_file_path(file_id, offline_path)
            .expect("point mover at offline parent");
    }
    let err = resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true)
        .expect_err("offline");
    assert!(
        err.to_lowercase().contains("retryable")
            || err.to_lowercase().contains("unreachable"),
        "expected retryable offline error got {err}"
    );
    assert!(bak.exists(), "bak kept while offline");
    let db = fx.db.lock().expect("lock");
    let mover = db.get_file_by_id(file_id).expect("mover");
    assert_eq!(mover.status, "path_collision");
    assert_eq!(mover.path, offline_path);
}

#[test]
fn path_collision_overwrite_crash_recovers_without_bak_when_identity_matches() {
    // N1: bak already gone but dest matches mover → complete commit (like #60 heal)
    let fx = Fixture::new();
    let (file_id, occupied, bak, _mover_path) = setup_crashed_overwrite(
        &fx,
        "n4_no_bak",
        b"new-bytes",
        b"old-bytes",
        None,
    );
    fs::remove_file(&bak).expect("drop bak");
    let outcome =
        resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true).expect("recover");
    match outcome {
        ResolvePathCollisionOutcome::Overwrite { file, new_path } => {
            assert_eq!(file.file.status, "ready");
            assert_eq!(Path::new(&new_path), occupied.as_path());
        }
        other => panic!("expected Overwrite got {other:?}"),
    }
    let db = fx.db.lock().expect("lock");
    let mover = db.get_file_by_id(file_id).expect("mover");
    assert_eq!(Path::new(&mover.path), occupied.as_path());
    assert_ne!(mover.status, "missing");
}

#[test]
fn path_collision_overwrite_crash_refuses_size_mismatch_keeps_bak() {
    let fx = Fixture::new();
    let (file_id, occupied, bak, mover_path) = setup_crashed_overwrite(
        &fx,
        "n4_size",
        b"new-bytes",
        b"old-bytes",
        None,
    );
    // Dest no longer matches DB size recorded for the mover
    fs::write(&occupied, b"different-longer-payload").expect("resize dest");
    let err = resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true)
        .expect_err("size mismatch");
    assert!(
        err.to_lowercase().contains("does not match") || err.to_lowercase().contains("refused"),
        "{err}"
    );
    assert!(bak.exists(), "bak kept");
    assert!(occupied.exists(), "dest kept");
    let db = fx.db.lock().expect("lock");
    let mover = db.get_file_by_id(file_id).expect("mover");
    assert_eq!(mover.status, "path_collision", "mover must not be marked missing");
    assert_eq!(mover.path, mover_path);
    assert!(
        db.list_error_logs(20).unwrap().iter().any(|e| {
            e.category == "path_collision" && e.message.contains("does not match")
        }),
        "ErrorLog on size mismatch"
    );
}

#[test]
fn path_collision_overwrite_crash_refuses_phash_mismatch_keeps_bak() {
    let fx = Fixture::new();
    let (file_id, occupied, bak, mover_path) = setup_crashed_overwrite(
        &fx,
        "n4_phash",
        b"new-bytes",
        b"old-bytes",
        Some("0123456789abcdef"),
    );
    heal_identity_override::arm_dest_phash("fedcba9876543210");
    let err = resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true)
        .expect_err("phash mismatch");
    heal_identity_override::clear();
    assert!(
        err.to_lowercase().contains("does not match") || err.to_lowercase().contains("refused"),
        "{err}"
    );
    assert!(bak.exists(), "bak kept");
    assert!(occupied.exists(), "dest kept");
    let db = fx.db.lock().expect("lock");
    let mover = db.get_file_by_id(file_id).expect("mover");
    assert_eq!(mover.status, "path_collision");
    assert_eq!(mover.path, mover_path);
    assert!(
        db.list_error_logs(20).unwrap().iter().any(|e| {
            e.category == "path_collision" && e.message.contains("does not match")
        }),
        "ErrorLog on phash mismatch"
    );
}

#[test]
fn path_collision_overwrite_crash_refuses_multiple_baks_keeps_them() {
    let fx = Fixture::new();
    let (file_id, occupied, bak, mover_path) = setup_crashed_overwrite(
        &fx,
        "n4_multi_bak",
        b"new-bytes",
        b"old-bytes",
        None,
    );
    let bak2 = occupied.parent().unwrap().join("Demo.mp4.idam-bak-2");
    fs::write(&bak2, b"other-old").expect("bak2");
    let err = resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true)
        .expect_err("ambiguous bak");
    assert!(
        err.to_lowercase().contains("multiple") || err.to_lowercase().contains("refused"),
        "{err}"
    );
    assert!(bak.exists(), "primary bak kept");
    assert!(bak2.exists(), "numbered bak kept");
    assert!(occupied.exists());
    let db = fx.db.lock().expect("lock");
    let mover = db.get_file_by_id(file_id).expect("mover");
    assert_eq!(mover.status, "path_collision");
    assert_eq!(mover.path, mover_path);
    assert!(
        db.list_error_logs(20).unwrap().iter().any(|e| {
            e.category == "path_collision" && e.message.contains("multiple idam-bak")
        }),
        "ErrorLog on ambiguous bak"
    );
}

#[test]
fn path_collision_suffix_avoids_db_owned_path() {
    // #59: suffix must skip candidates that exist only as Files.path rows
    let fx = Fixture::new();
    let media = fx.temp().join("suffix_db");
    fs::create_dir_all(&media).expect("mkdir");
    let occupied = media.join("Demo.mp4");
    let incoming = media.join("incoming.mp4");
    let db_taken = media.join("Demo_2.mp4");
    fs::write(&occupied, b"old").expect("occupied");
    fs::write(&incoming, b"new").expect("incoming");
    // Demo_2 exists only in DB (no file on disk)
    let file_id = {
        let db = fx.db.lock().expect("lock");
        let (occupant, _) = db
            .upsert_file(occupied.to_str().expect("utf8"), 3, None)
            .expect("occupant");
        let (_ghost, _) = db
            .upsert_file(db_taken.to_str().expect("utf8"), 1, None)
            .expect("ghost");
        let (id, _) = db
            .upsert_file(incoming.to_str().expect("utf8"), 3, None)
            .expect("incoming");
        db.mark_path_collision_atomic(id, occupied.to_str().expect("utf8"), Some(occupant))
            .expect("mark");
        id
    };
    let suffix =
        resolve_path_collision(&fx.db, &fx.queue, file_id, "suffix", false).expect("suffix");
    match suffix {
        ResolvePathCollisionOutcome::Suffix { new_path, .. } => {
            assert!(
                new_path.contains("Demo_3"),
                "expected Demo_3 after DB-owned Demo_2, got {new_path}"
            );
            assert!(Path::new(&new_path).exists());
            assert!(!db_taken.exists(), "must not clobber DB-only path on disk");
        }
        other => panic!("expected Suffix, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn path_collision_overwrite_hardlink_occupant_displaced_missing() {
    // #59 S1: same-inode overwrite displaces + marks missing (keeps Metadata/Tags)
    // Nit: on Linux rename of a hard link may leave the original path present
    let fx = Fixture::new();
    let media = fx.temp().join("hardlink_occ");
    fs::create_dir_all(&media).expect("mkdir");
    let occupied = media.join("Demo.mp4");
    let incoming = media.join("incoming.mp4");
    fs::write(&incoming, b"same-bytes").expect("write");
    fs::hard_link(&incoming, &occupied).expect("hardlink");

    let (file_id, occupant_id) = {
        let db = fx.db.lock().expect("lock");
        let (occupant, _) = db
            .upsert_file(occupied.to_str().expect("utf8"), 10, None)
            .expect("occupant");
        let (id, _) = db
            .upsert_file(incoming.to_str().expect("utf8"), 10, None)
            .expect("incoming");
        db.mark_path_collision_atomic(id, occupied.to_str().expect("utf8"), Some(occupant))
            .expect("mark");
        (id, occupant)
    };

    assert!(
        is_same_file_identity(&incoming, &occupied),
        "precondition: hardlink identity"
    );
    let outcome =
        resolve_path_collision(&fx.db, &fx.queue, file_id, "overwrite", true).expect("ovw");
    match outcome {
        ResolvePathCollisionOutcome::Overwrite { file, .. } => {
            assert_eq!(file.file.status, "ready");
        }
        other => panic!("expected Overwrite, got {other:?}"),
    }
    let db = fx.db.lock().expect("lock");
    let occupant = db.get_file_by_id(occupant_id).expect("occupant kept");
    assert_eq!(occupant.status, "missing");
    assert!(occupant.path.contains("#displaced-"));
    assert!(
        db.list_error_logs(20).unwrap().iter().any(|e| {
            e.category == "path_collision"
                && e.message.contains("same-file")
                && e.file_id == Some(occupant_id)
        }),
        "expected same-file overwrite ErrorLog"
    );
}
