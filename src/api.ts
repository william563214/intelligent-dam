import { invoke } from "@tauri-apps/api/core";
import type {
  AppSettings,
  FileWithMetadata,
  ScanResult,
  ScrapeResult,
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

export async function getSettings(): Promise<AppSettings> {
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

