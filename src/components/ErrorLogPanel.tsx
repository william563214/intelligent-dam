import { useEffect, useState } from "react";
import { clearErrorLogs, getErrorLogs } from "../api";
import type { ErrorLogEntry } from "../types";

export default function ErrorLogPanel() {
  const [logs, setLogs] = useState<ErrorLogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const loadLogs = async () => {
    try {
      setLoading(true);
      setError(null);
      const data = await getErrorLogs(100);
      setLogs(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void loadLogs();
  }, []);

  const handleClear = async () => {
    if (!confirm("確定清除全部錯誤日誌？")) {
      return;
    }
    try {
      setBusy(true);
      await clearErrorLogs();
      await loadLogs();
    } catch (err) {
      alert(`清除失敗: ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="p-6">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
        <h2 className="text-2xl font-bold text-gray-900">錯誤日誌</h2>
        <div className="flex gap-2">
          <button
            type="button"
            onClick={() => void loadLogs()}
            className="px-4 py-2 bg-blue-500 text-white rounded hover:bg-blue-600"
          >
            重新整理
          </button>
          <button
            type="button"
            onClick={() => void handleClear()}
            disabled={busy || logs.length === 0}
            className="px-4 py-2 bg-gray-600 text-white rounded hover:bg-gray-700 disabled:bg-gray-400"
          >
            清除全部
          </button>
        </div>
      </div>

      <p className="mb-4 text-sm text-gray-600">
        記錄刮削、FFmpeg 預覽與模板搬運失敗，方便排查。
      </p>

      {loading && <p className="text-gray-500">載入中...</p>}
      {error && <p className="text-red-500">錯誤: {error}</p>}

      {!loading && !error && logs.length === 0 && (
        <p className="text-gray-500">尚無錯誤紀錄</p>
      )}

      {!loading && logs.length > 0 && (
        <div className="overflow-x-auto bg-white rounded-lg shadow">
          <table className="min-w-full text-sm">
            <thead className="bg-gray-50 text-left text-gray-600">
              <tr>
                <th className="px-3 py-2 font-medium">時間</th>
                <th className="px-3 py-2 font-medium">類別</th>
                <th className="px-3 py-2 font-medium">檔案</th>
                <th className="px-3 py-2 font-medium">訊息</th>
              </tr>
            </thead>
            <tbody>
              {logs.map((row) => (
                <tr key={row.id} className="border-t align-top">
                  <td className="px-3 py-2 whitespace-nowrap text-gray-500">
                    {row.createdAt}
                  </td>
                  <td className="px-3 py-2">
                    <span className="inline-block rounded bg-amber-50 px-2 py-0.5 text-amber-900">
                      {row.category}
                    </span>
                  </td>
                  <td className="px-3 py-2 text-gray-700 max-w-xs truncate" title={row.path || ""}>
                    {row.fileId != null ? `#${row.fileId} ` : ""}
                    {row.path || "—"}
                  </td>
                  <td className="px-3 py-2 text-gray-900">
                    <div className="whitespace-pre-wrap break-words">{row.message}</div>
                    {row.detail && (
                      <div className="mt-1 text-xs text-gray-500 whitespace-pre-wrap">
                        {row.detail}
                      </div>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
