//! Minimal public surface for command-layer integration tests.
//! Not a stable application API — prefer `#[tauri::command]` wrappers in production.

pub use crate::commands::{
    add_file_tag, clear_error_logs, clear_file_tags, enqueue_pipeline_jobs, get_all_files,
    get_error_logs, get_file_tags, get_or_create_tag, get_settings, insert_error_log,
    save_settings, scan_directory, scan_multiple_directories, EnqueueResult, PipelineOptionsInput,
    ScanResult,
};
pub use crate::database::{
    current_schema_user_version, Database, ErrorLogEntry, FileWithMetadata, Metadata, Tag,
};
pub use crate::pipeline::PipelineOptions;
pub use crate::settings::{AppSettings, SettingsStore};
pub use crate::task_queue::TaskQueue;
