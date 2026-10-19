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
    get_settings, insert_error_log, save_settings, scan_directory, scan_multiple_directories,
    AppSettings, Database, PipelineOptions, SettingsStore, TaskQueue,
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
