/** Files.status 狀態機（對齊 SPEC / Rust file_status） */
export type FileStatus =
  | "processing"
  | "ready"
  | "scrape_failed"
  | "missing"
  | "suspected_duplicate";

export const file_status = {
  processing: "processing",
  ready: "ready",
  scrape_failed: "scrape_failed",
  missing: "missing",
  suspected_duplicate: "suspected_duplicate",
} as const;

const status_labels: Record<string, string> = {
  processing: "處理中",
  ready: "就緒",
  scrape_failed: "刮削失敗",
  missing: "檔案缺失",
  suspected_duplicate: "疑似重複",
};

const status_class_names: Record<string, string> = {
  processing: "bg-amber-100 text-amber-800",
  ready: "bg-emerald-100 text-emerald-800",
  scrape_failed: "bg-red-100 text-red-800",
  missing: "bg-gray-200 text-gray-700",
  suspected_duplicate: "bg-orange-100 text-orange-900",
};

export function status_label(status: string): string {
  return status_labels[status] || status;
}

export function status_badge_class(status: string): string {
  return status_class_names[status] || "bg-gray-100 text-gray-700";
}
