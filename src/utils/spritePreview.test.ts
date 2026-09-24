import { describe, expect, it } from "vitest";
import {
  build_default_preview,
  cue_at_progress,
  parse_vtt_content,
  parse_vtt_timestamp,
  scrub_background_style,
  sprite_path_from_stored,
} from "./spritePreview";

describe("spritePreview", () => {
  it("parses vtt timestamps", () => {
    expect(parse_vtt_timestamp("00:01:02.500")).toBe(62.5);
  });

  it("maps stored vtt path to sibling sprite jpg", () => {
    expect(sprite_path_from_stored("/tmp/intelligent_dam/sprite_1.vtt")).toBe(
      "/tmp/intelligent_dam/sprite_1.jpg"
    );
  });

  it("parses vtt cues and sprite path", () => {
    const sample_vtt = `WEBVTT

00:00:00.000 --> 00:00:01.000
sprite_9.jpg#xywh=0,0,160,90

00:00:01.000 --> 00:00:02.000
sprite_9.jpg#xywh=160,0,160,90

00:00:02.000 --> 00:00:03.000
sprite_9.jpg#xywh=320,0,160,90
`;

    const parsed = parse_vtt_content(
      sample_vtt,
      "/tmp/intelligent_dam/sprite_9.vtt"
    );
    expect(parsed).not.toBeNull();
    expect(parsed!.sprite_path).toBe("/tmp/intelligent_dam/sprite_9.jpg");
    expect(parsed!.cols).toBe(3);
    expect(parsed!.rows).toBe(1);
    expect(parsed!.cues).toHaveLength(3);
    expect(cue_at_progress(parsed!, 0.5).x).toBe(160);
    expect(cue_at_progress(parsed!, 1).x).toBe(320);
  });

  it("builds default 10x10 preview and scrubs by progress", () => {
    const fallback = build_default_preview("/tmp/intelligent_dam/sprite_2.jpg");
    expect(fallback.cues).toHaveLength(100);
    expect(cue_at_progress(fallback, 0).x).toBe(0);
    expect(cue_at_progress(fallback, 0.99).x).toBe(1440);

    const style = scrub_background_style(
      fallback,
      cue_at_progress(fallback, 0.15),
      "asset://sprite"
    );
    expect(String(style.backgroundSize)).toMatch(/1000%/);
    expect(String(style.backgroundPosition)).toMatch(/-/);
  });
});
