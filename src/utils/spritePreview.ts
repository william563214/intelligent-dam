import type { CSSProperties } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";

export interface sprite_cue {
  start: number;
  end: number;
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface sprite_preview {
  sprite_path: string;
  cols: number;
  rows: number;
  cell_width: number;
  cell_height: number;
  sheet_width: number;
  sheet_height: number;
  cues: sprite_cue[];
}

const default_cols = 10;
const default_rows = 10;
const default_cell_width = 160;
const default_cell_height = 90;

export function dirname_path(file_path: string): string {
  const idx = Math.max(file_path.lastIndexOf("/"), file_path.lastIndexOf("\\"));
  return idx >= 0 ? file_path.slice(0, idx) : ".";
}

export function join_path(dir: string, name: string): string {
  if (!name) return dir;
  if (/^[a-zA-Z]:[\\/]/.test(name) || name.startsWith("/") || name.startsWith("\\")) {
    return name;
  }
  const sep = dir.includes("\\") ? "\\" : "/";
  const trimmed = dir.endsWith("/") || dir.endsWith("\\") ? dir.slice(0, -1) : dir;
  return `${trimmed}${sep}${name}`;
}

export function sprite_path_from_stored(stored_path: string): string {
  if (/\.vtt$/i.test(stored_path)) {
    return stored_path.replace(/\.vtt$/i, ".jpg");
  }
  return stored_path;
}

export function parse_vtt_timestamp(value: string): number {
  const parts = value.trim().split(":");
  if (parts.length < 3) return 0;
  const hours = Number(parts[0]) || 0;
  const minutes = Number(parts[1]) || 0;
  const sec_parts = parts[2].split(".");
  const seconds = Number(sec_parts[0]) || 0;
  const millis = Number((sec_parts[1] || "0").padEnd(3, "0").slice(0, 3)) || 0;
  return hours * 3600 + minutes * 60 + seconds + millis / 1000;
}

export function parse_vtt_content(content: string, vtt_path: string): sprite_preview | null {
  const lines = content.replace(/^\uFEFF/, "").split(/\r?\n/);
  const cues: sprite_cue[] = [];
  let sprite_name: string | null = null;
  let i = 0;

  while (i < lines.length) {
    const line = lines[i].trim();
    i += 1;
    if (!line || line === "WEBVTT" || line.startsWith("NOTE")) continue;

    const arrow = line.indexOf("-->");
    if (arrow < 0) continue;

    const start = parse_vtt_timestamp(line.slice(0, arrow));
    const end = parse_vtt_timestamp(line.slice(arrow + 3));
    const payload = (lines[i] || "").trim();
    i += 1;
    if (!payload) continue;

    const hash = payload.indexOf("#xywh=");
    if (hash < 0) continue;

    const file_part = payload.slice(0, hash);
    const xywh = payload.slice(hash + 6).split(",").map((n) => Number(n));
    if (xywh.length < 4 || xywh.some((n) => Number.isNaN(n))) continue;

    if (!sprite_name && file_part) sprite_name = file_part;
    cues.push({
      start,
      end,
      x: xywh[0],
      y: xywh[1],
      width: xywh[2],
      height: xywh[3],
    });
  }

  if (cues.length === 0) return null;

  const cell_width = cues[0].width || default_cell_width;
  const cell_height = cues[0].height || default_cell_height;
  const max_x = Math.max(...cues.map((c) => c.x + c.width));
  const max_y = Math.max(...cues.map((c) => c.y + c.height));
  const cols = Math.max(1, Math.round(max_x / cell_width));
  const rows = Math.max(1, Math.round(max_y / cell_height));
  const base_dir = dirname_path(vtt_path);
  const sprite_path = sprite_name
    ? join_path(base_dir, sprite_name)
    : sprite_path_from_stored(vtt_path);

  return {
    sprite_path,
    cols,
    rows,
    cell_width,
    cell_height,
    sheet_width: max_x,
    sheet_height: max_y,
    cues,
  };
}

export function build_default_preview(
  sprite_path: string,
  cols = default_cols,
  rows = default_rows,
  cell_width = default_cell_width,
  cell_height = default_cell_height
): sprite_preview {
  const cues: sprite_cue[] = [];
  const total = cols * rows;
  for (let index = 0; index < total; index += 1) {
    const col = index % cols;
    const row = Math.floor(index / cols);
    cues.push({
      start: index,
      end: index + 1,
      x: col * cell_width,
      y: row * cell_height,
      width: cell_width,
      height: cell_height,
    });
  }
  return {
    sprite_path,
    cols,
    rows,
    cell_width,
    cell_height,
    sheet_width: cols * cell_width,
    sheet_height: rows * cell_height,
    cues,
  };
}

export function cue_at_progress(preview: sprite_preview, progress: number): sprite_cue {
  const clamped = Math.min(1, Math.max(0, progress));
  if (preview.cues.length === 0) {
    return {
      start: 0,
      end: 1,
      x: 0,
      y: 0,
      width: preview.cell_width,
      height: preview.cell_height,
    };
  }

  const duration = preview.cues[preview.cues.length - 1].end;
  if (duration > 0 && preview.cues[0].start === 0 && duration !== preview.cues.length) {
    const time = clamped * duration;
    if (time >= duration) return preview.cues[preview.cues.length - 1];
    for (const cue of preview.cues) {
      if (time >= cue.start && time < cue.end) return cue;
    }
    return preview.cues[preview.cues.length - 1];
  }

  const index = Math.min(
    preview.cues.length - 1,
    Math.floor(clamped * preview.cues.length)
  );
  return preview.cues[index];
}

export async function load_sprite_preview(stored_path: string): Promise<sprite_preview | null> {
  if (!stored_path) return null;

  if (/\.vtt$/i.test(stored_path)) {
    try {
      const url = await path_to_asset_url(stored_path);
      const response = await fetch(url);
      if (response.ok) {
        const text = await response.text();
        const parsed = parse_vtt_content(text, stored_path);
        if (parsed) return parsed;
      }
    } catch {
      // fall through to path convention
    }
    return build_default_preview(sprite_path_from_stored(stored_path));
  }

  return build_default_preview(stored_path);
}

export async function path_to_asset_url(file_path: string): Promise<string> {
  if (!file_path) return "";
  if (
    file_path.startsWith("http://") ||
    file_path.startsWith("https://") ||
    file_path.startsWith("asset:") ||
    file_path.startsWith("data:") ||
    file_path.startsWith("blob:")
  ) {
    return file_path;
  }

  try {
    return convertFileSrc(file_path);
  } catch {
    return file_path;
  }
}

export function scrub_background_style(
  preview: sprite_preview,
  cue: sprite_cue,
  sprite_url: string
): CSSProperties {
  const col = preview.cell_width > 0 ? Math.round(cue.x / preview.cell_width) : 0;
  const row = preview.cell_height > 0 ? Math.round(cue.y / preview.cell_height) : 0;
  return {
    backgroundImage: `url("${sprite_url}")`,
    backgroundRepeat: "no-repeat",
    backgroundSize: `${preview.cols * 100}% ${preview.rows * 100}%`,
    backgroundPosition: `-${col * 100}% -${row * 100}%`,
  };
}
