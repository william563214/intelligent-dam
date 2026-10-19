import { useEffect, useState } from "react";
import {
  enqueuePipelineJobs,
  getAllFiles,
  organizeEligibleFiles,
  resolveSuspectedDuplicate,
} from "../api";
import type { FileWithMetadata } from "../types";
import VideoCard from "./VideoCard";

export default function FileBrowser() {
  const [files, setFiles] = useState<FileWithMetadata[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [batchBusy, setBatchBusy] = useState(false);
  const [batchMessage, setBatchMessage] = useState<string | null>(null);
  const [resolveBusyId, setResolveBusyId] = useState<number | null>(null);

  useEffect(() => {
    loadFiles();
  }, []);

  const loadFiles = async () => {
    try {
      setLoading(true);
      setError(null);
      const data = await getAllFiles();
      setFiles(data);
      setSelected((prev) => {
        const valid = new Set(data.map((f) => f.file.id));
        return new Set([...prev].filter((id) => valid.has(id)));
      });
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load files");
    } finally {
      setLoading(false);
    }
  };

  const toggleSelect = (fileId: number) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(fileId)) {
        next.delete(fileId);
      } else {
        next.add(fileId);
      }
      return next;
    });
  };

  const selectAll = () => {
    setSelected(new Set(files.map((f) => f.file.id)));
  };

  const clearSelection = () => {
    setSelected(new Set());
  };

  const runBatch = async (
    label: string,
    options: {
      enableScrape?: boolean;
      enableMove?: boolean;
      enablePreview?: boolean;
    }
  ) => {
    if (selected.size === 0) {
      alert("請先勾選至少一個檔案");
      return;
    }
    try {
      setBatchBusy(true);
      setBatchMessage(null);
      const result = await enqueuePipelineJobs([...selected], options);
      setBatchMessage(
        `${label}：已排入 ${result.enqueued} 個任務（略過 ${result.skipped}）`
      );
      clearSelection();
    } catch (err) {
      setBatchMessage(
        `${label}失敗: ${err instanceof Error ? err.message : String(err)}`
      );
    } finally {
      setBatchBusy(false);
    }
  };

  const handleResolveSuspected = async (
    fileId: number,
    action: "confirm" | "ignore"
  ) => {
    try {
      setResolveBusyId(fileId);
      setBatchMessage(null);
      await resolveSuspectedDuplicate(fileId, action);
      await loadFiles();
      setBatchMessage(
        action === "confirm"
          ? `已確認重複並移除檔案 #${fileId}`
          : `已忽略疑似重複，檔案 #${fileId} 恢復就緒`
      );
    } catch (err) {
      setBatchMessage(
        `疑似重複處理失敗: ${err instanceof Error ? err.message : String(err)}`
      );
    } finally {
      setResolveBusyId(null);
    }
  };

  const handleOneClickOrganize = async () => {
    if (
      !confirm(
        "對媒體庫中所有可整理檔案執行刮削 → 模板搬運 → 預覽？已完成步驟會自動略過。"
      )
    ) {
      return;
    }
    try {
      setBatchBusy(true);
      setBatchMessage(null);
      const result = await organizeEligibleFiles();
      setBatchMessage(
        `一鍵整理：已排入 ${result.enqueued} 個任務（略過 ${result.skipped}）`
      );
    } catch (err) {
      setBatchMessage(
        `一鍵整理失敗: ${err instanceof Error ? err.message : String(err)}`
      );
    } finally {
      setBatchBusy(false);
    }
  };

  return (
    <div className="p-6">
      <div className="mb-4 flex flex-wrap justify-between items-center gap-2">
        <h2 className="text-2xl font-bold text-gray-900">影片庫</h2>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            onClick={() => void handleOneClickOrganize()}
            disabled={batchBusy}
            className="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700 disabled:bg-gray-400"
          >
            一鍵整理
          </button>
          <button
            onClick={loadFiles}
            className="px-4 py-2 bg-blue-500 text-white rounded hover:bg-blue-600"
          >
            重新整理
          </button>
        </div>
      </div>

      <div className="mb-4 flex flex-wrap items-center gap-2 rounded-lg bg-white p-3 shadow">
        <span className="text-sm text-gray-600">
          已選 {selected.size} / {files.length}
        </span>
        <button
          type="button"
          onClick={selectAll}
          disabled={files.length === 0}
          className="px-3 py-1 text-sm bg-gray-100 rounded hover:bg-gray-200 disabled:opacity-50"
        >
          全選
        </button>
        <button
          type="button"
          onClick={clearSelection}
          className="px-3 py-1 text-sm bg-gray-100 rounded hover:bg-gray-200"
        >
          清除選取
        </button>
        <span className="mx-1 h-4 w-px bg-gray-200" />
        <button
          type="button"
          disabled={batchBusy || selected.size === 0}
          onClick={() =>
            void runBatch("批量刮削", {
              enableScrape: true,
              enableMove: false,
              enablePreview: false,
            })
          }
          className="px-3 py-1 text-sm bg-green-600 text-white rounded hover:bg-green-700 disabled:bg-gray-400"
        >
          批量刮削
        </button>
        <button
          type="button"
          disabled={batchBusy || selected.size === 0}
          onClick={() =>
            void runBatch("批量預覽", {
              enableScrape: false,
              enableMove: false,
              enablePreview: true,
            })
          }
          className="px-3 py-1 text-sm bg-purple-600 text-white rounded hover:bg-purple-700 disabled:bg-gray-400"
        >
          批量預覽
        </button>
        <button
          type="button"
          disabled={batchBusy || selected.size === 0}
          onClick={() =>
            void runBatch("批量整理", {
              enableScrape: true,
              enableMove: true,
              enablePreview: true,
            })
          }
          className="px-3 py-1 text-sm bg-amber-600 text-white rounded hover:bg-amber-700 disabled:bg-gray-400"
        >
          批量整理／搬運
        </button>
        {batchBusy && <span className="text-sm text-gray-500">排程中...</span>}
      </div>

      {batchMessage && (
        <p className="mb-4 text-sm text-gray-700">{batchMessage}</p>
      )}

      {loading && (
        <div className="flex items-center justify-center h-64">
          <div className="text-gray-500">載入中...</div>
        </div>
      )}

      {!loading && error && (
        <div className="flex items-center justify-center h-64">
          <div className="text-red-500">錯誤: {error}</div>
        </div>
      )}

      {!loading && !error && files.length === 0 && (
        <div className="flex items-center justify-center h-64">
          <div className="text-gray-500">尚無檔案，請先掃描目錄</div>
        </div>
      )}

      {!loading && !error && files.length > 0 && (
        <div className="grid grid-cols-1 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5 gap-4">
          {files.map((item) => {
            const checked = selected.has(item.file.id);
            return (
              <div key={item.file.id} className="relative">
                <label className="absolute left-2 top-2 z-10 flex items-center gap-1 rounded bg-white/90 px-2 py-1 text-xs shadow">
                  <input
                    type="checkbox"
                    checked={checked}
                    onChange={() => toggleSelect(item.file.id)}
                  />
                  選取
                </label>
                <VideoCard
                  fileWithMetadata={item}
                  onResolveSuspected={handleResolveSuspected}
                  resolveBusy={resolveBusyId === item.file.id}
                />
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
