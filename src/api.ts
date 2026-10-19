import { invoke } from "@tauri-apps/api/core";
import type {
  AppSettings,
  ConnectivityReport,
  EnqueueResult,
  ErrorLogEntry,
  FileWithMetadata,
  PipelineOptionsInput,
  ResolveSuspectedOutcome,
  ScanResult,
  ScrapeResult,
  SettingsResponse,
  Metadata,
  WatcherStatus,
  TaskQueueStatus,
} from "./types";

export async function scanDirectory(path: string): Promise<ScanResult> {
  return await invoke("scan_directory_command", { path });
}

export async function scanMultipleDirectories(paths: string[]): Promise<ScanResult> {
  return await invoke("scan_multiple_directories_command", { paths });
}

export async function getAllFiles(): Promise<FileWithMetadata[]> {
  return await invoke("get_all_files_command");
}

/** Confirm or ignore a near-phash suspected duplicate. */
export async function resolveSuspectedDuplicate(
  fileId: number,
  action: "confirm" | "ignore"
): Promise<ResolveSuspectedOutcome> {
  return await invoke("resolve_suspected_duplicate_command", { fileId, action });
}

export async function scrapeFile(
  fileId: number,
  avdcPath?: string,
  stashdbUrl?: string
): Promise<ScrapeResult> {
  return await invoke("scrape_file_command", {
    fileId,
    avdcPath,
    stashdbUrl,
  });
}

export async function updateFileMetadata(
  fileId: number,
  metadata: Metadata
): Promise<void> {
  return await invoke("update_file_metadata_command", { fileId, metadata });
}

export async function generatePreview(fileId: number): Promise<string> {
  return await invoke("generate_preview_command", { fileId });
}

export async function generateThumbnail(
  fileId: number,
  timestamp?: number
): Promise<string> {
  return await invoke("generate_thumbnail_command", { fileId, timestamp });
}

export async function checkFFmpegAvailable(): Promise<boolean> {
  return await invoke("check_ffmpeg_available_command");
}

export async function getSettings(): Promise<SettingsResponse> {
  return await invoke("get_settings_command");
}

export async function saveSettings(settings: AppSettings): Promise<void> {
  return await invoke("save_settings_command", { settings });
}

export async function startWatcher(): Promise<WatcherStatus> {
  return await invoke("start_watcher_command");
}

export async function stopWatcher(): Promise<WatcherStatus> {
  return await invoke("stop_watcher_command");
}

export async function getWatcherStatus(): Promise<WatcherStatus> {
  return await invoke("get_watcher_status_command");
}

export async function getTaskQueueStatus(): Promise<TaskQueueStatus> {
  return await invoke("get_task_queue_status_command");
}

export async function retryTask(jobId: number): Promise<number> {
  return await invoke("retry_task_command", { jobId });
}

export async function testConnectivity(): Promise<ConnectivityReport> {
  return await invoke("test_connectivity_command");
}

export async function getErrorLogs(limit?: number): Promise<ErrorLogEntry[]> {
  return await invoke("get_error_logs_command", { limit });
}

export async function clearErrorLogs(): Promise<number> {
  return await invoke("clear_error_logs_command");
}

export async function enqueuePipelineJobs(
  fileIds: number[],
  options?: PipelineOptionsInput
): Promise<EnqueueResult> {
  return await invoke("enqueue_pipeline_jobs_command", { fileIds, options });
}

export async function organizeEligibleFiles(): Promise<EnqueueResult> {
  return await invoke("organize_eligible_files_command");
}

export async function canonicalizePath(path: string): Promise<string> {
  return await invoke("canonicalize_path_command", { path });
}
