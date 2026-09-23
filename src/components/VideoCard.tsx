import { useEffect, useRef, useState } from "react";
import type { MouseEvent } from "react";
import type { FileWithMetadata } from "../types";
import { status_badge_class, status_label } from "../utils/fileStatus";
import {
  cue_at_progress,
  load_sprite_preview,
  path_to_asset_url,
  scrub_background_style,
  type sprite_cue,
  type sprite_preview,
} from "../utils/spritePreview";

interface VideoCardProps {
  fileWithMetadata: FileWithMetadata;
}

export default function VideoCard({ fileWithMetadata }: VideoCardProps) {
  const { file, metadata, tags } = fileWithMetadata;
  const media_ref = useRef<HTMLDivElement>(null);
  const [is_hovering, set_is_hovering] = useState(false);
  const [cover_url, set_cover_url] = useState<string | null>(null);
  const [preview, set_preview] = useState<sprite_preview | null>(null);
  const [sprite_url, set_sprite_url] = useState<string | null>(null);
  const [active_cue, set_active_cue] = useState<sprite_cue | null>(null);
  const [scrub_progress, set_scrub_progress] = useState(0);
  const [preview_failed, set_preview_failed] = useState(false);

  const title = metadata?.title || file.path.split(/[/\\]/).pop() || "未知標題";
  const cover_image = metadata?.cover_image;
  const sprite_sheet_path = metadata?.sprite_sheet_path;

  useEffect(() => {
    let cancelled = false;
    set_cover_url(null);
    if (!cover_image) return;

    path_to_asset_url(cover_image)
      .then((url) => {
        if (!cancelled) set_cover_url(url);
      })
      .catch(() => {
        if (!cancelled) set_cover_url(cover_image);
      });

    return () => {
      cancelled = true;
    };
  }, [cover_image]);

  useEffect(() => {
    let cancelled = false;
    set_preview(null);
    set_sprite_url(null);
    set_active_cue(null);
    set_preview_failed(false);

    if (!sprite_sheet_path) return;

    (async () => {
      try {
        const loaded = await load_sprite_preview(sprite_sheet_path);
        if (cancelled || !loaded) {
          if (!cancelled) set_preview_failed(true);
          return;
        }
        const url = await path_to_asset_url(loaded.sprite_path);
        if (cancelled) return;
        set_preview(loaded);
        set_sprite_url(url);
        set_active_cue(loaded.cues[0] || null);
      } catch {
        if (!cancelled) set_preview_failed(true);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [sprite_sheet_path]);

  const update_scrub = (client_x: number) => {
    const el = media_ref.current;
    if (!el || !preview) return;
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0) return;
    const progress = Math.min(1, Math.max(0, (client_x - rect.left) / rect.width));
    set_scrub_progress(progress);
    set_active_cue(cue_at_progress(preview, progress));
  };

  const handle_mouse_enter = (event: MouseEvent<HTMLDivElement>) => {
    set_is_hovering(true);
    if (preview && sprite_url && !preview_failed) {
      update_scrub(event.clientX);
    }
  };

  const handle_mouse_move = (event: MouseEvent<HTMLDivElement>) => {
    if (!is_hovering) return;
    if (preview && sprite_url && !preview_failed) {
      update_scrub(event.clientX);
    }
  };

  const handle_mouse_leave = () => {
    set_is_hovering(false);
    set_scrub_progress(0);
    if (preview?.cues[0]) set_active_cue(preview.cues[0]);
  };

  const format_file_size = (bytes: number): string => {
    if (bytes < 1024) return bytes + " B";
    if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(2) + " KB";
    if (bytes < 1024 * 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(2) + " MB";
    return (bytes / (1024 * 1024 * 1024)).toFixed(2) + " GB";
  };

  const show_sprite =
    is_hovering && !!preview && !!sprite_url && !!active_cue && !preview_failed;

  return (
    <div
      className="bg-white rounded-lg shadow-md overflow-hidden hover:shadow-lg transition-shadow cursor-pointer"
      onMouseEnter={handle_mouse_enter}
      onMouseMove={handle_mouse_move}
      onMouseLeave={handle_mouse_leave}
    >
      <div ref={media_ref} className="relative aspect-video bg-gray-200 overflow-hidden">
        {sprite_url && (
          <img
            src={sprite_url}
            alt=""
            className="hidden"
            onError={() => set_preview_failed(true)}
          />
        )}
        {show_sprite && preview && active_cue && sprite_url ? (
          <div
            className="absolute inset-0"
            style={scrub_background_style(preview, active_cue, sprite_url)}
            role="img"
            aria-label={`${title} 預覽`}
          />
        ) : cover_url ? (
          <img
            src={cover_url}
            alt={title}
            className="w-full h-full object-cover"
            onError={() => set_cover_url(null)}
          />
        ) : (
          <div className="w-full h-full flex items-center justify-center text-gray-400">
            <svg
              className="w-16 h-16"
              fill="none"
              stroke="currentColor"
              viewBox="0 0 24 24"
            >
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                strokeWidth={2}
                d="M15 10l4.553-2.276A1 1 0 0121 8.618v6.764a1 1 0 01-1.447.894L15 14M5 18h8a2 2 0 002-2V8a2 2 0 00-2-2H5a2 2 0 00-2 2v8a2 2 0 002 2z"
              />
            </svg>
          </div>
        )}
        {show_sprite && (
          <div className="absolute left-0 right-0 bottom-0 h-1 bg-black/30">
            <div
              className="h-full bg-white/90"
              style={{ width: `${scrub_progress * 100}%` }}
            />
          </div>
        )}
      </div>
      <div className="p-4">
        <div className="flex items-start justify-between gap-2 mb-1">
          <h3 className="font-semibold text-gray-900 truncate">{title}</h3>
          <span
            className={`shrink-0 px-2 py-0.5 text-xs rounded ${status_badge_class(file.status)}`}
            title={file.status}
          >
            {status_label(file.status)}
          </span>
        </div>
        {metadata?.studio && (
          <p className="text-sm text-gray-600 truncate mb-1">
            {metadata.studio}
          </p>
        )}
        {metadata?.release_date && (
          <p className="text-xs text-gray-500 mb-2">
            {metadata.release_date}
          </p>
        )}
        {tags.length > 0 && (
          <div className="flex flex-wrap gap-1 mb-2">
            {tags.slice(0, 3).map((tag) => (
              <span
                key={tag.id}
                className="px-2 py-1 bg-blue-100 text-blue-800 text-xs rounded"
              >
                {tag.name}
              </span>
            ))}
            {tags.length > 3 && (
              <span className="px-2 py-1 bg-gray-100 text-gray-600 text-xs rounded">
                +{tags.length - 3}
              </span>
            )}
          </div>
        )}
        <p className="text-xs text-gray-500">{format_file_size(file.size)}</p>
      </div>
    </div>
  );
}
