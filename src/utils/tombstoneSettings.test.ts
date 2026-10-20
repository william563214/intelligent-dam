import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api", () => ({
  listDuplicateTombstones: vi.fn(),
  removeDuplicateTombstone: vi.fn(),
}));

import {
  listDuplicateTombstones,
  removeDuplicateTombstone,
} from "../api";
import {
  refresh_tombstone_list,
  remove_tombstone_and_refresh,
} from "./tombstoneSettings";

const sample = [
  {
    id: 1,
    path: "/a.mp4",
    size: 10,
    phash: "aa",
    keeperId: 2,
    createdAt: "t",
  },
];

describe("tombstoneSettings", () => {
  beforeEach(() => {
    vi.mocked(listDuplicateTombstones).mockReset();
    vi.mocked(removeDuplicateTombstone).mockReset();
  });

  it("refresh_tombstone_list returns API list", async () => {
    vi.mocked(listDuplicateTombstones).mockResolvedValue(sample);
    await expect(refresh_tombstone_list()).resolves.toEqual(sample);
  });

  it("remove success refreshes list", async () => {
    vi.mocked(removeDuplicateTombstone).mockResolvedValue(true);
    vi.mocked(listDuplicateTombstones).mockResolvedValue([]);
    const result = await remove_tombstone_and_refresh(1, () => true);
    expect(result).toEqual({ ok: true, tombstones: [] });
  });

  it("remove false refreshes and notifies", async () => {
    vi.mocked(removeDuplicateTombstone).mockResolvedValue(false);
    vi.mocked(listDuplicateTombstones).mockResolvedValue(sample);
    const result = await remove_tombstone_and_refresh(99, () => true);
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error).toContain("不存在");
      expect(result.tombstones).toEqual(sample);
    }
  });

  it("cancel confirm skips API remove", async () => {
    const result = await remove_tombstone_and_refresh(1, () => false);
    expect(result.ok).toBe(false);
    expect(removeDuplicateTombstone).not.toHaveBeenCalled();
  });
});
