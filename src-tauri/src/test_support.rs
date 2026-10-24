//! Minimal public surface for command-layer integration tests.
//! Not a stable application API — prefer `#[tauri::command]` wrappers in production.
//! Gated behind the `test-support` cargo feature (enabled in CI test/clippy).

pub use crate::commands::{
    add_file_tag, clear_error_logs, clear_file_tags, enqueue_pipeline_jobs, execute_batch_organize,
    get_all_files, get_error_logs, get_file_tags, get_last_undoable_organize_batch,
    get_or_create_tag, get_settings, insert_error_log, list_duplicate_tombstones,
    preview_batch_organize, preview_output_template, remove_duplicate_tombstone,
    resolve_path_collision, resolve_suspected_duplicate_with_enqueue, save_settings,
    scan_directory, scan_multiple_directories, undo_last_organize_batch, EnqueueResult,
    PipelineOptionsInput, PreviewTemplateResult, ResolvePathCollisionOutcome,
    ResolveSuspectedOutcome, ScanResult,
};
pub use crate::batch_organize::{
    OrganizeExecutePlanItem, OrganizeExecuteResult, OrganizePreviewItem, OrganizePreviewResult,
    OrganizeUndoResult,
};

/// Convenience wrapper without pipeline enqueue (integration tests).
pub fn resolve_suspected_duplicate(
    db: &std::sync::Arc<std::sync::Mutex<Database>>,
    queue: &std::sync::Arc<TaskQueue>,
    file_id: i64,
    action: &str,
) -> Result<ResolveSuspectedOutcome, String> {
    resolve_suspected_duplicate_with_enqueue(db, queue, file_id, action, None, None, None)
}
pub use crate::database::{
    current_schema_user_version, path_collision_tx_fault, Database, DuplicateTombstone,
    ErrorLogEntry, FileWithMetadata, JobRow, Metadata, NewJobRow, OrganizeBatch,
    OrganizeBatchItem, PathTombstoneAction, Tag,
};
pub use crate::file_stability::StabilityOptions;
pub use crate::path_template::{
    discard_overwrite_backup, is_same_file_identity, move_file_overwrite_via_copy, overwrite_fault,
    read_fs_file_id, FsFileId,
};
pub use crate::pipeline::{
    finalize_move_fault, heal_identity_override, recover_pending_move_for, recover_pending_moves,
    PipelineOptions,
};
pub use crate::settings::{AppSettings, SettingsStore};
pub use crate::task_queue::{max_terminal_jobs, JobKind, TaskQueue};
pub use crate::watcher::{WatcherManager, WatcherOptions, WatcherStatus};
