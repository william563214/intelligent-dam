//! Minimal public surface for command-layer integration tests.
//! Not a stable application API — prefer `#[tauri::command]` wrappers in production.

pub use crate::commands::{
    add_file_tag, clear_error_logs, clear_file_tags, enqueue_pipeline_jobs, get_all_files,
    get_error_logs, get_file_tags, get_or_create_tag, get_settings, insert_error_log,
    save_settings, scan_directory, scan_multiple_directories, EnqueueResult, PipelineOptionsInput,
    ScanResult,
};
pub use crate::database::{
    current_schema_user_version, Database, ErrorLogEntry, FileWithMetadata, JobRow, Metadata,
    NewJobRow, Tag,
};
pub use crate::pipeline::PipelineOptions;
pub use crate::settings::{AppSettings, SettingsStore};
pub use crate::task_queue::{
    arm_execute_fault_for_test, clear_execute_fault_for_test, env_max_attempts, max_terminal_jobs,
    JobKind, TaskQueue,
};
