import {
  listDuplicateTombstones,
  removeDuplicateTombstone,
} from "../api";
import type { DuplicateTombstone } from "../types";

export type tombstone_remove_result =
  | { ok: true; tombstones: DuplicateTombstone[] }
  | { ok: false; error: string; tombstones: DuplicateTombstone[] };

export async function refresh_tombstone_list(): Promise<DuplicateTombstone[]> {
  return listDuplicateTombstones();
}

/** Remove a tombstone; always re-lists. If remove returned false, notify via message. */
export async function remove_tombstone_and_refresh(
  id: number,
  confirmFn: (message: string) => boolean = (msg) =>
    typeof window !== "undefined" ? window.confirm(msg) : true
): Promise<tombstone_remove_result> {
  if (
    !confirmFn("移除此確認重複墓碑？之後同路徑／phash 可能再次被匯入。")
  ) {
    const tombstones = await refresh_tombstone_list().catch(() => []);
    return { ok: false, error: "已取消", tombstones };
  }
  try {
    const removed = await removeDuplicateTombstone(id);
    const tombstones = await refresh_tombstone_list();
    if (!removed) {
      return {
        ok: false,
        error: "墓碑不存在或已被移除，列表已重新整理",
        tombstones,
      };
    }
    return { ok: true, tombstones };
  } catch (err) {
    const tombstones = await refresh_tombstone_list().catch(() => []);
    return {
      ok: false,
      error: err instanceof Error ? err.message : String(err),
      tombstones,
    };
  }
}
