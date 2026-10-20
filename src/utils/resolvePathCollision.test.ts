import { beforeEach, describe, expect, it, vi } from "vitest";

const resolve_mock = vi.fn();

vi.mock("../api", () => ({
  resolvePathCollision: (...args: unknown[]) => resolve_mock(...args),
}));

import {
  collision_success_message,
  confirm_overwrite_message,
  handle_resolve_path_collision,
} from "./resolvePathCollision";

describe("resolvePathCollision helpers", () => {
  beforeEach(() => {
    resolve_mock.mockReset();
  });

  it("confirm_overwrite_message includes path", () => {
    expect(confirm_overwrite_message("/lib/a.mp4")).toContain("/lib/a.mp4");
  });

  it("collision_success_message covers actions", () => {
    expect(collision_success_message("skip", 1)).toContain("跳過");
    expect(collision_success_message("suffix", 2, "/x_2.mp4")).toContain(
      "/x_2.mp4"
    );
    expect(collision_success_message("overwrite", 3, "/x.mp4")).toContain(
      "覆蓋"
    );
  });

  it("handle_resolve_path_collision cancels overwrite when dialog declines", async () => {
    const result = await handle_resolve_path_collision(
      1,
      "overwrite",
      "/lib/a.mp4",
      () => false
    );
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error).toBe("已取消覆蓋");
    }
    expect(resolve_mock).not.toHaveBeenCalled();
  });

  it("handle_resolve_path_collision passes confirmed on overwrite", async () => {
    resolve_mock.mockResolvedValue({
      kind: "overwrite",
      file: { file: { id: 5 }, metadata: null, tags: [] },
      newPath: "/lib/a.mp4",
    });
    const result = await handle_resolve_path_collision(
      5,
      "overwrite",
      "/lib/a.mp4",
      () => true
    );
    expect(result.ok).toBe(true);
    expect(resolve_mock).toHaveBeenCalledWith(5, "overwrite", true);
  });

  it("handle_resolve_path_collision suffix without confirm flag", async () => {
    resolve_mock.mockResolvedValue({
      kind: "suffix",
      file: { file: { id: 2 }, metadata: null, tags: [] },
      newPath: "/lib/a_2.mp4",
    });
    const result = await handle_resolve_path_collision(
      2,
      "suffix",
      "/lib/a.mp4",
      () => true
    );
    expect(result.ok).toBe(true);
    expect(resolve_mock).toHaveBeenCalledWith(2, "suffix", false);
  });

  it("handle_resolve_path_collision surfaces API failure", async () => {
    resolve_mock.mockRejectedValue(new Error("boom"));
    const result = await handle_resolve_path_collision(
      3,
      "skip",
      "/lib/a.mp4",
      () => true
    );
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error).toBe("boom");
    }
  });
});
