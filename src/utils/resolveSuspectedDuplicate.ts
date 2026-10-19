import { resolveSuspectedDuplicate } from "../api";
import type { ResolveSuspectedOutcome } from "../types";

export type resolve_action = "confirm" | "ignore";

export function confirm_duplicate_message(): string {
  return (
    "確認此檔為重複？將刪除資料庫中的檔案紀錄與中繼資料／標籤（不可復原），" +
    "磁碟上的影片檔不會被刪除。確認後此路徑／phash 會永久略過再匯入。"
  );
}

export function resolve_success_message(
  action: resolve_action,
  fileId: number
): string {
  if (action === "confirm") {
    return `已確認重複：已移除資料庫紀錄 #${fileId}（磁碟檔案仍保留）`;
  }
  return `已忽略疑似重複，檔案 #${fileId} 恢復就緒並排入流水線`;
}

export async function handle_resolve_suspected(
  fileId: number,
  action: resolve_action,
  confirmFn: (message: string) => boolean = (msg) =>
    typeof window !== "undefined" ? window.confirm(msg) : true
): Promise<{ ok: true; outcome: ResolveSuspectedOutcome } | { ok: false; error: string }> {
  if (action === "confirm" && !confirmFn(confirm_duplicate_message())) {
    return { ok: false, error: "已取消確認" };
  }
  try {
    const outcome = await resolveSuspectedDuplicate(fileId, action);
    return { ok: true, outcome };
  } catch (err) {
    return {
      ok: false,
      error: err instanceof Error ? err.message : String(err),
    };
  }
}
