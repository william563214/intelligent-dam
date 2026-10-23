import { useEffect, useState } from "react";
import {
  enqueuePipelineJobs,
  executeBatchOrganize,
  getAllFiles,
  getLastUndoableOrganizeBatch,
  organizeEligibleFiles,
  previewBatchOrganize,
  undoLastOrganizeBatch,
} from "../api";
import type {
  FileWithMetadata,
  OrganizeBatch,
  OrganizePreviewResult,
} from "../types";
import {
  handle_resolve_suspected,
  resolve_success_message,
} from "../utils/resolveSuspectedDuplicate";
import {
  collision_success_message,
  handle_resolve_path_collision,
} from "../utils/resolvePathCollision";
import VideoCard from "./VideoCard";

export default function FileBrowser() {
  const [files, setFiles] = useState<FileWithMetadata[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [batchBusy, setBatchBusy] = useState(false);
  const [batchMessage, setBatchMessage] = useState<string | null>(null);
  const [resolveBusyId, setResolveBusyId] = useState<number | null>(null);
  const [organizePreview, setOrganizePreview] =
    useState<OrganizePreviewResult | null>(null);
  const [undoableBatch, setUndoableBatch] = useState<OrganizeBatch | null>(
    null
  );

  useEffect(() => {
    loadFiles();
    void refreshUndoable();
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
    setResolveBusyId(fileId);
    setBatchMessage(null);
    const result = await handle_resolve_suspected(fileId, action);
    if (!result.ok) {
      if (result.error !== "已取消確認") {
        setBatchMessage(`疑似重複處理失敗: ${result.error}`);
      }
      setResolveBusyId(null);
      return;
    }
    await loadFiles();
    const enqueued =
      result.outcome.kind === "ignored"
        ? result.outcome.pipelineEnqueued
        : false;
    setBatchMessage(resolve_success_message(action, fileId, enqueued));
    setResolveBusyId(null);
  };

  const handleResolvePathCollision = async (
    fileId: number,
    action: "suffix" | "skip" | "overwrite",
    desiredPath: string
  ) => {
    setResolveBusyId(fileId);
    setBatchMessage(null);
    const result = await handle_resolve_path_collision(
      fileId,
      action,
      desiredPath
    );
    if (!result.ok) {
      if (result.error !== "已取消覆蓋") {
        setBatchMessage(`路徑碰撞處理失敗: ${result.error}`);
      }
      setResolveBusyId(null);
      return;
    }
    await loadFiles();
    const newPath =
      result.outcome.kind === "suffix" || result.outcome.kind === "overwrite"
        ? result.outcome.newPath
        : undefined;
    setBatchMessage(collision_success_message(action, fileId, newPath));
    setResolveBusyId(null);
  };

  const refreshUndoable = async () => {
    try {
      setUndoableBatch(await getLastUndoableOrganizeBatch());
    } catch {
      setUndoableBatch(null);
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

  const handlePreviewOrganize = async () => {
    try {
      setBatchBusy(true);
      setBatchMessage(null);
      const ids =
        selected.size > 0 ? [...selected] : undefined;
      const preview = await previewBatchOrganize(ids);
      setOrganizePreview(preview);
      setBatchMessage(
        `整理預覽：可搬移 ${preview.moveCount}，略過 ${preview.skipCount}（尚未寫入檔案系統）`
      );
    } catch (err) {
      setBatchMessage(
        `整理預覽失敗: ${err instanceof Error ? err.message : String(err)}`
      );
    } finally {
      setBatchBusy(false);
    }
  };

  const handleConfirmOrganize = async () => {
    if (!organizePreview) {
      return;
    }
    const moveIds = organizePreview.items
      .filter((i) => i.outcome === "move")
      .map((i) => i.fileId);
    if (moveIds.length === 0) {
      setBatchMessage("沒有可執行的搬移項目");
      return;
    }
    if (
      !confirm(
        `確認執行 ${moveIds.length} 筆模板搬移？完成後可用「還原上一批」復原。`
      )
    ) {
      return;
    }
    try {
      setBatchBusy(true);
      setBatchMessage(null);
      const result = await executeBatchOrganize(moveIds);
      setOrganizePreview(null);
      setBatchMessage(
        `批次整理完成：成功 ${result.moved}，略過 ${result.skipped}，失敗 ${result.failed}（batch #${result.batchId}）`
      );
      await loadFiles();
      await refreshUndoable();
    } catch (err) {
      setBatchMessage(
        `批次整理失敗: ${err instanceof Error ? err.message : String(err)}`
      );
    } finally {
      setBatchBusy(false);
    }
  };

  const handleUndoLastBatch = async () => {
    if (
      !confirm(
        undoableBatch
          ? `還原批次 #${undoableBatch.id}？狀態不符的檔案會拒絕並寫入錯誤日誌。`
          : "還原上一批整理？"
      )
    ) {
      return;
    }
    try {
      setBatchBusy(true);
      setBatchMessage(null);
      const result = await undoLastOrganizeBatch();
      setBatchMessage(
        `還原完成：成功 ${result.undone}，拒絕 ${result.refused}（batch #${result.batchId}，狀態 ${result.batch.status}）`
      );
      await loadFiles();
      await refreshUndoable();
    } catch (err) {
      setBatchMessage(
        `還原失敗: ${err instanceof Error ? err.message : String(err)}`
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
            onClick={() => void handlePreviewOrganize()}
            disabled={batchBusy}
            className="px-4 py-2 bg-teal-600 text-white rounded hover:bg-teal-700 disabled:bg-gray-400"
          >
            整理預覽
          </button>
          <button
            type="button"
            onClick={() => void handleConfirmOrganize()}
            disabled={batchBusy || !organizePreview || organizePreview.moveCount === 0}
            className="px-4 py-2 bg-teal-800 text-white rounded hover:bg-teal-900 disabled:bg-gray-400"
          >
            確認執行
          </button>
          <button
            type="button"
            onClick={() => void handleUndoLastBatch()}
            disabled={batchBusy || !undoableBatch}
            className="px-4 py-2 bg-rose-600 text-white rounded hover:bg-rose-700 disabled:bg-gray-400"
            title={
              undoableBatch
                ? `還原批次 #${undoableBatch.id}`
                : "尚無可還原批次"
            }
          >
            還原上一批
          </button>
          <button
            type="button"
            onClick={() => void handleOneClickOrganize()}
            disabled={batchBusy}
            className="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700 disabled:bg-gray-400"
          >
            一鍵整理
          </button>
          <button
            onClick={() => {
              void loadFiles();
              void refreshUndoable();
            }}
            className="px-4 py-2 bg-blue-500 text-white rounded hover:bg-blue-600"
          >
            重新整理
          </button>
        </div>
      </div>

      {organizePreview && (
        <div className="mb-4 max-h-56 overflow-y-auto rounded-lg bg-white p-3 shadow text-sm">
          <div className="mb-2 font-medium text-gray-800">
            整理預覽（唯讀）— 可搬 {organizePreview.moveCount}／略過{" "}
            {organizePreview.skipCount}
          </div>
          <ul className="space-y-1 font-mono text-xs text-gray-700">
            {organizePreview.items.map((item) => (
              <li key={item.fileId}>
                <span className="text-gray-500">[{item.outcome}]</span>{" "}
                {item.srcPath || "(無來源)"} → {item.dstPath || "(無目的)"}
                {item.detail ? ` — ${item.detail}` : ""}
              </li>
            ))}
          </ul>
        </div>
      )}

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
                  onResolvePathCollision={handleResolvePathCollision}
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
