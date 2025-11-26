import { useState } from "react";
import type { FileWithMetadata } from "../types";

interface VideoCardProps {
  fileWithMetadata: FileWithMetadata;
}

export default function VideoCard({ fileWithMetadata }: VideoCardProps) {
  const { file, metadata, tags } = fileWithMetadata;
  const [isHovering, setIsHovering] = useState(false);
  const [spriteImage, setSpriteImage] = useState<string | null>(null);

  const title = metadata?.title || file.path.split(/[/\\]/).pop() || "未知標題";
  const coverImage = metadata?.cover_image;

  const handleMouseEnter = () => {
    setIsHovering(true);
    // 這裡可以載入 Sprite Sheet 進行預覽
    // 目前先顯示封面圖
    if (coverImage) {
      setSpriteImage(coverImage);
    }
  };

  const handleMouseLeave = () => {
    setIsHovering(false);
  };

  const formatFileSize = (bytes: number): string => {
    if (bytes < 1024) return bytes + " B";
    if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(2) + " KB";
    if (bytes < 1024 * 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(2) + " MB";
    return (bytes / (1024 * 1024 * 1024)).toFixed(2) + " GB";
  };

  return (
    <div
      className="bg-white rounded-lg shadow-md overflow-hidden hover:shadow-lg transition-shadow cursor-pointer"
      onMouseEnter={handleMouseEnter}
      onMouseLeave={handleMouseLeave}
    >
      <div className="relative aspect-video bg-gray-200">
        {spriteImage ? (
          <img
            src={spriteImage}
            alt={title}
            className="w-full h-full object-cover"
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
        {isHovering && metadata?.sprite_sheet_path && (
          <div className="absolute inset-0 bg-black bg-opacity-50 flex items-center justify-center">
            <span className="text-white text-sm">預覽中...</span>
          </div>
        )}
      </div>
      <div className="p-4">
        <h3 className="font-semibold text-gray-900 truncate mb-1">{title}</h3>
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
        <p className="text-xs text-gray-500">{formatFileSize(file.size)}</p>
      </div>
    </div>
  );
}

