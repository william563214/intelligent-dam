import { describe, expect, it, vi } from "vitest";
import {
  confirm_batch_organize_message,
  confirm_batch_undo_message,
  handle_confirm_batch_organize,
  handle_confirm_batch_undo,
  plans_from_preview_items,
} from "./batchOrganizeConfirm";
import type { OrganizePreviewItem } from "../types";

describe("batchOrganizeConfirm", () => {
  it("confirm_batch_organize_message includes count", () => {
    expect(confirm_batch_organize_message(3)).toContain("3");
    expect(confirm_batch_organize_message(3)).toContain("還原上一批");
  });

  it("confirm_batch_undo_message includes batch id and retry hint", () => {
    expect(confirm_batch_undo_message(12)).toContain("#12");
    expect(confirm_batch_undo_message(12)).toContain("重試");
    expect(confirm_batch_undo_message(null)).toContain("還原上一批");
  });

  it("plans_from_preview_items keeps only move outcomes with dst", () => {
    const items: OrganizePreviewItem[] = [
      {
        fileId: 1,
        srcPath: "/a",
        dstPath: "/b",
        size: 1,
        phash: null,
        outcome: "move",
        detail: null,
        occupantFileId: null,
      },
      {
        fileId: 2,
        srcPath: "/c",
        dstPath: "/d",
        size: 1,
        phash: null,
        outcome: "collision",
        detail: null,
        occupantFileId: null,
      },
    ];
    expect(plans_from_preview_items(items)).toEqual([
      { fileId: 1, dstPath: "/b" },
    ]);
  });

  it("handle_confirm_batch_organize cancels when dialog declines", async () => {
    const confirmFn = vi.fn(() => false);
    await expect(handle_confirm_batch_organize(2, confirmFn)).resolves.toBe(
      false
    );
    expect(confirmFn).toHaveBeenCalledOnce();
  });

  it("handle_confirm_batch_organize accepts when dialog confirms", async () => {
    const confirmFn = vi.fn(() => true);
    await expect(handle_confirm_batch_organize(2, confirmFn)).resolves.toBe(
      true
    );
  });

  it("handle_confirm_batch_organize skips dialog when moveCount is 0", async () => {
    const confirmFn = vi.fn(() => true);
    await expect(handle_confirm_batch_organize(0, confirmFn)).resolves.toBe(
      false
    );
    expect(confirmFn).not.toHaveBeenCalled();
  });

  it("handle_confirm_batch_undo uses batch id in dialog", async () => {
    const confirmFn = vi.fn(() => true);
    await expect(handle_confirm_batch_undo(9, confirmFn)).resolves.toBe(true);
    expect(confirmFn).toHaveBeenCalledWith(expect.stringContaining("#9"));
  });
});
