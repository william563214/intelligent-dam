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

export interface FileWithMetadata {
  file: FileRecord;
  metadata: Metadata | null;
  tags: Tag[];
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
}

