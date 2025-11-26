import { invoke } from "@tauri-apps/api/core";
import type {
  FileWithMetadata,
  ScanResult,
  ScrapeResult,
  Metadata,
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

