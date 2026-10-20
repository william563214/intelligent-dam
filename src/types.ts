export interface FileRecord {
  id: number;
  path: string;
  size: number;
  created_at: string;
  phash: string | null;
  /** processing | ready | scrape_failed | missing */
  status: string;
}

export interface Metadata {
  file_id: number;
  title: string | null;
  studio: string | null;
  release_date: string | null;
  summary: string | null;
  cover_image: string | null;
  sprite_sheet_path: string | null;
  source_type: string | null;
}

export interface Tag {
  id: number;
  name: string;
  category: string | null;
}

export interface SuspectedDuplicateInfo {
  keeperId: number;
  keeperPath: string;
  distance: number;
}

export interface PathCollisionInfo {
  desiredPath: string;
  existingPath: string;
  occupantFileId?: number | null;
}

export interface FileWithMetadata {
  file: FileRecord;
  metadata: Metadata | null;
  tags: Tag[];
  suspectedDuplicate?: SuspectedDuplicateInfo | null;
  pathCollision?: PathCollisionInfo | null;
}

export interface ScanResult {
  imported_count: number;
  file_ids: number[];
}

export interface ScrapeResult {
  success: boolean;
  metadata: Metadata | null;
  tags: Tag[] | null;
  error: string | null;
}

export interface AppSettings {
  scanDirectories: string[];
  avdcPath: string;
  stashdbUrl: string;
  outputTemplate: string;
  /** Max Hamming distance for near-phash suspected matches (default 4) */
  nearPhashMaxDistance: number;
}

/** get_settings_command response (flatten AppSettings + env override metadata) */
export interface SettingsResponse extends AppSettings {
  nearPhashEnvOverride: number | null;
  effectiveNearPhashMaxDistance: number;
}

export type ResolveSuspectedOutcome =
  | { kind: "confirmed"; fileId: number }
  | {
      kind: "ignored";
      file: FileWithMetadata;
      pipelineEnqueued: boolean;
    };

export type ResolvePathCollisionOutcome =
  | { kind: "suffix"; file: FileWithMetadata; newPath: string }
  | { kind: "skip"; file: FileWithMetadata }
  | { kind: "overwrite"; file: FileWithMetadata; newPath: string };

export interface PreviewTemplateResult {
  renderedPath: string;
  template: string;
  baseDir: string;
}

export interface DuplicateTombstone {
  id: number;
  path: string;
  size: number | null;
  phash: string | null;
  keeperId: number | null;
  createdAt: string;
}

export interface WatcherStatus {
  running: boolean;
  watchedPaths: string[];
}

export interface WatcherFileEvent {
  path: string;
  kind: string;
  fileId: number | null;
  error: string | null;
}

/** Progress from automatic scrape → move → preview pipeline */
export interface PipelineProgressEvent {
  fileId: number;
  path: string;
  /** scrape | move | preview | done | pipeline */
  step: string;
  /** started | ok | failed | skipped | completed_with_errors */
  status: string;
  message: string | null;
  newPath: string | null;
}

/** Progress from background task queue (phash / pipeline jobs) */
export interface TaskQueueProgressEvent {
  jobId: number;
  fileId: number;
  /** pipeline | phash */
  kind: string;
  /** queued | running | succeeded | failed | cancelled */
  state: string;
  attempt: number;
  message: string | null;
  retryable: boolean;
}

export interface TaskQueueJobView {
  jobId: number;
  fileId: number;
  kind: string;
  state: string;
  attempt: number;
  maxAttempts: number;
  error: string | null;
  retryable: boolean;
}

export interface TaskQueueStatus {
  concurrency: number;
  pending: number;
  running: number;
  failed: number;
  succeeded: number;
  jobs: TaskQueueJobView[];
}

export interface PipelineOptionsInput {
  enableScrape?: boolean;
  enableMove?: boolean;
  enablePreview?: boolean;
}

export interface EnqueueResult {
  jobIds: number[];
  enqueued: number;
  skipped: number;
}

export interface ConnectivityCheck {
  ok: boolean;
  message: string;
}

export interface ConnectivityReport {
  ffmpeg: ConnectivityCheck;
  avdc: ConnectivityCheck;
  stashdb: ConnectivityCheck;
}

export interface ErrorLogEntry {
  id: number;
  createdAt: string;
  category: string;
  fileId: number | null;
  path: string | null;
  message: string;
  detail: string | null;
}

