//! Minimal public surface for command-layer integration tests.
//! Not a stable application API — prefer `#[tauri::command]` wrappers in production.
//! Gated behind the `test-support` cargo feature (enabled in CI test/clippy).

pub use crate::commands::{
    add_file_tag, clear_error_logs, clear_file_tags, enqueue_pipeline_jobs, get_all_files,
    get_error_logs, get_file_tags, get_or_create_tag, get_settings, insert_error_log,
    resolve_suspected_duplicate, save_settings, scan_directory, scan_multiple_directories,
    EnqueueResult, PipelineOptionsInput, ResolveSuspectedOutcome, ScanResult,
};
pub use crate::database::{
    current_schema_user_version, Database, ErrorLogEntry, FileWithMetadata, JobRow, Metadata,
    NewJobRow, Tag,
};
pub use crate::file_stability::StabilityOptions;
pub use crate::pipeline::PipelineOptions;
pub use crate::settings::{AppSettings, SettingsStore};
pub use crate::task_queue::{max_terminal_jobs, JobKind, TaskQueue};
pub use crate::watcher::{WatcherManager, WatcherOptions, WatcherStatus};
