import { useEffect, useState } from "react";
import { getAllFiles } from "../api";
import type { FileWithMetadata } from "../types";
import VideoCard from "./VideoCard";

export default function FileBrowser() {
  const [files, setFiles] = useState<FileWithMetadata[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    loadFiles();
  }, []);

  const loadFiles = async () => {
    try {
      setLoading(true);
      setError(null);
      const data = await getAllFiles();
      setFiles(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load files");
    } finally {
      setLoading(false);
    }
  };

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="text-gray-500">載入中...</div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="text-red-500">錯誤: {error}</div>
      </div>
    );
  }

  if (files.length === 0) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="text-gray-500">尚無檔案，請先掃描目錄</div>
      </div>
    );
  }

  return (
    <div className="p-6">
      <div className="mb-4 flex justify-between items-center">
        <h2 className="text-2xl font-bold text-gray-900">影片庫</h2>
        <button
          onClick={loadFiles}
          className="px-4 py-2 bg-blue-500 text-white rounded hover:bg-blue-600"
        >
          重新整理
        </button>
      </div>
      <div className="grid grid-cols-1 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5 gap-4">
        {files.map((item) => (
          <VideoCard key={item.file.id} fileWithMetadata={item} />
        ))}
      </div>
    </div>
  );
}

