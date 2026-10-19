import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api", () => ({
  resolveSuspectedDuplicate: vi.fn(),
}));

import { resolveSuspectedDuplicate } from "../api";
import {
  confirm_duplicate_message,
  handle_resolve_suspected,
  resolve_success_message,
} from "./resolveSuspectedDuplicate";

describe("resolveSuspectedDuplicate helpers", () => {
  beforeEach(() => {
    vi.mocked(resolveSuspectedDuplicate).mockReset();
  });

  it("resolve_success_message distinguishes confirm vs ignore enqueue", () => {
    expect(resolve_success_message("confirm", 7)).toContain("資料庫紀錄 #7");
    expect(resolve_success_message("confirm", 7)).toContain("磁碟檔案仍保留");
    expect(resolve_success_message("ignore", 9, true)).toContain("已排入流水線");
    expect(resolve_success_message("ignore", 9, false)).not.toContain(
      "已排入流水線"
    );
  });

  it("confirm_duplicate_message warns about DB not disk", () => {
    const msg = confirm_duplicate_message();
    expect(msg).toContain("資料庫");
    expect(msg).toContain("磁碟上的影片檔不會被刪除");
  });

  it("handle_resolve_suspected success on ignore", async () => {
    vi.mocked(resolveSuspectedDuplicate).mockResolvedValue({
      kind: "ignored",
      file: {
        file: {
          id: 3,
          path: "/a.mp4",
          size: 1,
          created_at: "",
          phash: null,
          status: "ready",
        },
        metadata: null,
        tags: [],
      },
      pipelineEnqueued: true,
    } as never);
    const result = await handle_resolve_suspected(3, "ignore", () => true);
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.outcome.kind).toBe("ignored");
    }
    expect(resolveSuspectedDuplicate).toHaveBeenCalledWith(3, "ignore");
  });

  it("handle_resolve_suspected cancels confirm when dialog declines", async () => {
    const result = await handle_resolve_suspected(1, "confirm", () => false);
    expect(result).toEqual({ ok: false, error: "已取消確認" });
    expect(resolveSuspectedDuplicate).not.toHaveBeenCalled();
  });

  it("handle_resolve_suspected surfaces API failure", async () => {
    vi.mocked(resolveSuspectedDuplicate).mockRejectedValue(new Error("boom"));
    const result = await handle_resolve_suspected(2, "ignore", () => true);
    expect(result).toEqual({ ok: false, error: "boom" });
  });

  it("handle_resolve_suspected success on confirm", async () => {
    vi.mocked(resolveSuspectedDuplicate).mockResolvedValue({
      kind: "confirmed",
      fileId: 5,
    });
    const result = await handle_resolve_suspected(5, "confirm", () => true);
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.outcome).toEqual({ kind: "confirmed", fileId: 5 });
    }
  });
});
