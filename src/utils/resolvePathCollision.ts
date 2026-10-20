import { resolvePathCollision } from "../api";
import type { ResolvePathCollisionOutcome } from "../types";

export type path_collision_action = "suffix" | "skip" | "overwrite";

export function confirm_overwrite_message(desiredPath: string): string {
  return (
    `確定要覆蓋既有檔案嗎？\n${desiredPath}\n` +
    "若該路徑屬於媒體庫中的另一筆紀錄，該紀錄會標為缺失（不可復原覆寫）。"
  );
}

export function collision_success_message(
  action: path_collision_action,
  fileId: number,
  newPath?: string
): string {
  if (action === "skip") {
    return `已跳過搬運：檔案 #${fileId} 維持原路徑`;
  }
  if (action === "suffix") {
    return `已加後綴搬運：檔案 #${fileId} → ${newPath ?? ""}`;
  }
  return `已覆蓋目的地：檔案 #${fileId} → ${newPath ?? ""}`;
}

export async function handle_resolve_path_collision(
  fileId: number,
  action: path_collision_action,
  desiredPath: string,
  confirmFn: (message: string) => boolean = (msg) =>
    typeof window !== "undefined" ? window.confirm(msg) : true
): Promise<
  | { ok: true; outcome: ResolvePathCollisionOutcome }
  | { ok: false; error: string }
> {
  let confirmed = false;
  if (action === "overwrite") {
    if (!confirmFn(confirm_overwrite_message(desiredPath))) {
      return { ok: false, error: "已取消覆蓋" };
    }
    confirmed = true;
  }
  try {
    const outcome = await resolvePathCollision(fileId, action, confirmed);
    return { ok: true as const, outcome };
  } catch (err) {
    return {
      ok: false,
      error: err instanceof Error ? err.message : String(err),
    };
  }
}
