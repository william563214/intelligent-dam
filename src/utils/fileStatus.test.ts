import { describe, expect, it } from "vitest";
import { status_badge_class, status_label } from "./fileStatus";

describe("fileStatus", () => {
  it("maps known statuses to zh labels", () => {
    expect(status_label("processing")).toBe("處理中");
    expect(status_label("ready")).toBe("就緒");
    expect(status_label("scrape_failed")).toBe("刮削失敗");
    expect(status_label("missing")).toBe("檔案缺失");
    expect(status_label("suspected_duplicate")).toBe("疑似重複");
  });

  it("falls back for unknown status", () => {
    expect(status_label("custom")).toBe("custom");
    expect(status_badge_class("custom")).toContain("gray");
  });

  it("uses distinct badge classes for scrape_failed and missing", () => {
    expect(status_badge_class("scrape_failed")).toContain("red");
    expect(status_badge_class("missing")).toContain("gray");
    expect(status_badge_class("processing")).toContain("amber");
    expect(status_badge_class("suspected_duplicate")).toContain("orange");
  });
});
