import type { OrganizePreviewItem } from "../types";

export function confirm_batch_organize_message(moveCount: number): string {
  return `確認執行 ${moveCount} 筆模板搬移？完成後可用「還原上一批」復原。`;
}

export function confirm_batch_undo_message(batchId?: number | null): string {
  if (batchId != null) {
    return `還原批次 #${batchId}？狀態不符的檔案會拒絕並寫入錯誤日誌；被拒絕者可再次重試。`;
  }
  return "還原上一批整理？";
}

export function plans_from_preview_items(
  items: OrganizePreviewItem[]
): { fileId: number; dstPath: string }[] {
  return items
    .filter((i) => i.outcome === "move")
    .map((i) => ({ fileId: i.fileId, dstPath: i.dstPath }));
}

export async function handle_confirm_batch_organize(
  moveCount: number,
  confirmFn: (message: string) => boolean = (msg) => window.confirm(msg)
): Promise<boolean> {
  if (moveCount <= 0) {
    return false;
  }
  return confirmFn(confirm_batch_organize_message(moveCount));
}

export async function handle_confirm_batch_undo(
  batchId: number | null | undefined,
  confirmFn: (message: string) => boolean = (msg) => window.confirm(msg)
): Promise<boolean> {
  return confirmFn(confirm_batch_undo_message(batchId));
}
