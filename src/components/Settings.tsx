import { useState, useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import {
  scanMultipleDirectories,
  checkFFmpegAvailable,
  getSettings,
  saveSettings,
  startWatcher,
  stopWatcher,
  getWatcherStatus,
  getTaskQueueStatus,
  retryTask,
  testConnectivity,
  canonicalizePath,
} from "../api";
import type {
  AppSettings,
  ConnectivityReport,
  PipelineProgressEvent,
  TaskQueueProgressEvent,
  TaskQueueStatus,
  WatcherFileEvent,
  WatcherStatus,
} from "../types";

const default_settings: AppSettings = {
  scanDirectories: [],
  avdcPath: "",
  stashdbUrl: "https://stashdb.org",
  outputTemplate: "{{studio}}/{{date}}_{{title}}.{{ext}}",
};

const legacy_settings_key = "intelligent_dam_settings";

export default function Settings() {
  const [settings, setSettings] = useState<AppSettings>(default_settings);
  const [newDirectory, setNewDirectory] = useState("");
  const [scanning, setScanning] = useState(false);
  const [scanResult, setScanResult] = useState<string | null>(null);
  const [ffmpegAvailable, setFFmpegAvailable] = useState<boolean | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [watcherStatus, setWatcherStatus] = useState<WatcherStatus>({
    running: false,
    watchedPaths: [],
  });
  const [watcherBusy, setWatcherBusy] = useState(false);
  const [watcherMessage, setWatcherMessage] = useState<string | null>(null);
  const [queueStatus, setQueueStatus] = useState<TaskQueueStatus | null>(null);
  const [queueMessage, setQueueMessage] = useState<string | null>(null);
  const [connectivity, setConnectivity] = useState<ConnectivityReport | null>(
    null
  );
  const [connectivityBusy, setConnectivityBusy] = useState(false);

  useEffect(() => {
    loadSettings();
    checkFFmpeg();
    refreshWatcherStatus();
    refreshQueueStatus();
  }, []);

  useEffect(() => {
    let unlistenFile: (() => void) | undefined;
    let unlistenPipeline: (() => void) | undefined;
    let unlistenQueue: (() => void) | undefined;

    listen<WatcherFileEvent>("watcher-file-detected", (event) => {
      const payload = event.payload;
      if (payload.error) {
        setWatcherMessage(`偵測失敗: ${payload.path} (${payload.error})`);
        return;
      }
      setWatcherMessage(
        `偵測到新檔 (${payload.kind}): ${payload.path}` +
          (payload.fileId != null ? ` #${payload.fileId}` : "") +
          " → 已送入任務佇列"
      );
    })
      .then((fn) => {
        unlistenFile = fn;
      })
      .catch((err) => {
        console.error("Failed to listen watcher events:", err);
      });

    listen<PipelineProgressEvent>("watcher-pipeline-progress", (event) => {
      const p = event.payload;
      const detail = p.message ? ` (${p.message})` : "";
      const relocated = p.newPath ? ` → ${p.newPath}` : "";
      setWatcherMessage(
        `流水線 #${p.fileId} [${p.step}/${p.status}]${relocated}${detail}`
      );
    })
      .then((fn) => {
        unlistenPipeline = fn;
      })
      .catch((err) => {
        console.error("Failed to listen pipeline events:", err);
      });

    listen<TaskQueueProgressEvent>("task-queue-progress", (event) => {
      const p = event.payload;
      const detail = p.message ? ` (${p.message})` : "";
      const failHint =
        p.state === "failed"
          ? p.retryable
            ? " — 可重試"
            : " — 不可重試"
          : "";
      setQueueMessage(
        `佇列 job #${p.jobId} file #${p.fileId} [${p.kind}/${p.state}] attempt ${p.attempt}${detail}${failHint}`
      );
      void refreshQueueStatus();
    })
      .then((fn) => {
        unlistenQueue = fn;
      })
      .catch((err) => {
        console.error("Failed to listen task-queue events:", err);
      });

    return () => {
      if (unlistenFile) {
        unlistenFile();
      }
      if (unlistenPipeline) {
        unlistenPipeline();
      }
      if (unlistenQueue) {
        unlistenQueue();
      }
    };
  }, []);

  const migrateLegacySettings = async (
    current: AppSettings
  ): Promise<AppSettings> => {
    const saved = localStorage.getItem(legacy_settings_key);
    if (!saved) {
      return current;
    }

    try {
      const parsed = JSON.parse(saved) as Partial<AppSettings>;
      const hasPersistedData =
        current.scanDirectories.length > 0 ||
        current.avdcPath.trim() !== "" ||
        (current.stashdbUrl.trim() !== "" &&
          current.stashdbUrl !== default_settings.stashdbUrl) ||
        (current.outputTemplate.trim() !== "" &&
          current.outputTemplate !== default_settings.outputTemplate);

      if (hasPersistedData) {
        localStorage.removeItem(legacy_settings_key);
        return current;
      }

      const migrated: AppSettings = {
        ...default_settings,
        ...current,
        ...parsed,
        scanDirectories:
          parsed.scanDirectories ??
          current.scanDirectories ??
          default_settings.scanDirectories,
      };
      await saveSettings(migrated);
      localStorage.removeItem(legacy_settings_key);
      return migrated;
    } catch (e) {
      console.error("Failed to migrate legacy settings:", e);
      return current;
    }
  };

  const loadSettings = async () => {
    try {
      setLoading(true);
      setLoadError(null);
      const loaded = await getSettings();
      const resolved = await migrateLegacySettings(loaded);
      setSettings({ ...default_settings, ...resolved });
    } catch (e) {
      console.error("Failed to load settings:", e);
      const message =
        e instanceof Error ? e.message : typeof e === "string" ? e : "未知錯誤";
      setLoadError(message);
      // 不把預設值當成已載入成功 避免用空預設覆寫磁碟上的設定
    } finally {
      setLoading(false);
    }
  };

  const handleSaveSettings = async () => {
    if (loadError) {
      return;
    }
    try {
      setSaving(true);
      await saveSettings(settings);
      localStorage.removeItem(legacy_settings_key);
      await refreshWatcherStatus();
      alert("設定已儲存！");
    } catch (e) {
      alert(`儲存失敗: ${e instanceof Error ? e.message : "未知錯誤"}`);
    } finally {
      setSaving(false);
    }
  };

  const checkFFmpeg = async () => {
    try {
      const available = await checkFFmpegAvailable();
      setFFmpegAvailable(available);
    } catch (err) {
      console.error("Failed to check FFmpeg:", err);
      setFFmpegAvailable(false);
    }
  };

  const refreshWatcherStatus = async () => {
    try {
      const status = await getWatcherStatus();
      setWatcherStatus(status);
    } catch (err) {
      console.error("Failed to get watcher status:", err);
    }
  };

  const refreshQueueStatus = async () => {
    try {
      const status = await getTaskQueueStatus();
      setQueueStatus(status);
    } catch (err) {
      console.error("Failed to get task queue status:", err);
    }
  };

  const handleRetryFailed = async () => {
    if (!queueStatus) {
      return;
    }
    const failed = queueStatus.jobs.filter(
      (j) => j.state === "failed" && j.retryable
    );
    for (const job of failed) {
      try {
        await retryTask(job.jobId);
      } catch (err) {
        console.error("Failed to retry job", job.jobId, err);
        setQueueMessage(
          `重試 job #${job.jobId} 失敗: ${
            err instanceof Error ? err.message : String(err)
          }`
        );
      }
    }
    await refreshQueueStatus();
  };

  const handleStartWatcher = async () => {
    if (loadError) {
      alert("設定載入失敗 請先重新載入");
      return;
    }
    if (settings.scanDirectories.length === 0) {
      alert("請先添加至少一個掃描目錄");
      return;
    }
    try {
      setWatcherBusy(true);
      setWatcherMessage(null);
      await saveSettings(settings);
      const status = await startWatcher();
      setWatcherStatus(status);
      setWatcherMessage(
        `監控已啟動 (${status.watchedPaths.length} 個目錄)`
      );
    } catch (err) {
      setWatcherMessage(
        `啟動失敗: ${err instanceof Error ? err.message : String(err)}`
      );
    } finally {
      setWatcherBusy(false);
    }
  };

  const handleStopWatcher = async () => {
    try {
      setWatcherBusy(true);
      const status = await stopWatcher();
      setWatcherStatus(status);
      setWatcherMessage("監控已停止");
    } catch (err) {
      setWatcherMessage(
        `停止失敗: ${err instanceof Error ? err.message : String(err)}`
      );
    } finally {
      setWatcherBusy(false);
    }
  };

  const handleAddDirectory = async () => {
    const raw = newDirectory.trim();
    if (!raw) {
      return;
    }
    let path = raw;
    try {
      path = await canonicalizePath(raw);
    } catch (err) {
      console.error("canonicalize failed", err);
    }
    if (!path) {
      return;
    }
    if (settings.scanDirectories.includes(path)) {
      alert("此目錄已在清單中");
      setNewDirectory("");
      return;
    }
    setSettings({
      ...settings,
      scanDirectories: [...settings.scanDirectories, path],
    });
    setNewDirectory("");
  };

  const handlePickDirectory = async () => {
    try {
      const selected = await open({
        directory: true,
        multiple: false,
        title: "選擇掃描／監控目錄",
      });
      if (selected == null) {
        return;
      }
      const raw = typeof selected === "string" ? selected : String(selected);
      if (!raw.trim()) {
        return;
      }
      let path = raw;
      try {
        path = await canonicalizePath(raw);
      } catch (err) {
        console.error("canonicalize failed", err);
      }
      if (settings.scanDirectories.includes(path)) {
        alert("此目錄已在清單中");
        return;
      }
      setSettings({
        ...settings,
        scanDirectories: [...settings.scanDirectories, path],
      });
    } catch (err) {
      alert(
        `選擇資料夾失敗: ${err instanceof Error ? err.message : String(err)}`
      );
    }
  };

  const handlePickAvdc = async () => {
    try {
      const selected = await open({
        multiple: false,
        title: "選擇 AV_Data_Capture 腳本",
        filters: [
          { name: "Python", extensions: ["py"] },
          { name: "All", extensions: ["*"] },
        ],
      });
      if (selected == null) {
        return;
      }
      const raw = typeof selected === "string" ? selected : String(selected);
      let path = raw;
      try {
        path = await canonicalizePath(raw);
      } catch (err) {
        console.error("canonicalize failed", err);
      }
      setSettings({ ...settings, avdcPath: path });
    } catch (err) {
      alert(
        `選擇檔案失敗: ${err instanceof Error ? err.message : String(err)}`
      );
    }
  };

  const handleTestConnectivity = async () => {
    try {
      setConnectivityBusy(true);
      // 先把目前表單寫入 讓測試讀到最新路徑／URL
      if (!loadError) {
        await saveSettings(settings);
      }
      const report = await testConnectivity();
      setConnectivity(report);
      setFFmpegAvailable(report.ffmpeg.ok);
    } catch (err) {
      alert(
        `連線測試失敗: ${err instanceof Error ? err.message : String(err)}`
      );
    } finally {
      setConnectivityBusy(false);
    }
  };

  const handleRemoveDirectory = (index: number) => {
    setSettings({
      ...settings,
      scanDirectories: settings.scanDirectories.filter((_, i) => i !== index),
    });
  };

  const handleScan = async () => {
    if (loadError) {
      alert("設定載入失敗 無法儲存或掃描 請先重新載入");
      return;
    }
    if (settings.scanDirectories.length === 0) {
      alert("請先添加至少一個掃描目錄");
      return;
    }

    try {
      setScanning(true);
      setScanResult(null);
      await saveSettings(settings);
      const result = await scanMultipleDirectories(settings.scanDirectories);
      setScanResult(
        `掃描完成！成功匯入 ${result.imported_count} 個檔案。`
      );
    } catch (err) {
      setScanResult(
        `掃描失敗: ${err instanceof Error ? err.message : "未知錯誤"}`
      );
    } finally {
      setScanning(false);
    }
  };

  if (loading) {
    return (
      <div className="p-6">
        <h2 className="text-2xl font-bold text-gray-900 mb-4">設定</h2>
        <p className="text-gray-500">載入設定中...</p>
      </div>
    );
  }

  return (
    <div className="p-6">
      <h2 className="text-2xl font-bold text-gray-900 mb-4">設定</h2>
      {loadError && (
        <div className="mb-4 rounded-md border border-red-200 bg-red-50 p-4 text-sm text-red-700">
          <p className="font-medium">載入設定失敗</p>
          <p className="mt-1">{loadError}</p>
          <p className="mt-2 text-red-600">
            已停用儲存 避免用空白預設覆寫磁碟上的設定。
          </p>
          <button
            type="button"
            onClick={loadSettings}
            className="mt-3 px-3 py-1 text-sm bg-red-100 rounded hover:bg-red-200"
          >
            重新載入
          </button>
        </div>
      )}
      <div className="bg-white rounded-lg shadow p-6 space-y-6">
        <div>
          <label className="block text-sm font-medium text-gray-700 mb-2">
            掃描目錄（支援多個目錄，會遞迴掃描所有子資料夾）
          </label>
          <div className="space-y-2">
            <div className="flex space-x-2">
              <input
                type="text"
                value={newDirectory}
                onChange={(e) => setNewDirectory(e.target.value)}
                onKeyPress={(e) => {
                  if (e.key === "Enter") {
                    void handleAddDirectory();
                  }
                }}
                placeholder="例如: C:/Videos 或 /Users/username/Videos"
                className="flex-1 px-3 py-2 border border-gray-300 rounded-md"
              />
              <button
                type="button"
                onClick={() => void handlePickDirectory()}
                className="px-4 py-2 bg-slate-600 text-white rounded hover:bg-slate-700"
              >
                瀏覽…
              </button>
              <button
                onClick={handleAddDirectory}
                disabled={!newDirectory.trim()}
                className="px-4 py-2 bg-green-500 text-white rounded hover:bg-green-600 disabled:bg-gray-400"
              >
                添加
              </button>
            </div>
            {settings.scanDirectories.length > 0 && (
              <div className="space-y-1">
                {settings.scanDirectories.map((dir, index) => (
                  <div
                    key={index}
                    className="flex items-center justify-between px-3 py-2 bg-gray-50 rounded-md"
                  >
                    <span className="text-sm text-gray-700 flex-1 truncate mr-2">
                      {dir}
                    </span>
                    <button
                      onClick={() => handleRemoveDirectory(index)}
                      className="px-2 py-1 text-sm text-red-600 hover:text-red-800"
                    >
                      移除
                    </button>
                  </div>
                ))}
              </div>
            )}
            <button
              onClick={handleScan}
              disabled={
                scanning ||
                !!loadError ||
                settings.scanDirectories.length === 0
              }
              className="w-full px-4 py-2 bg-blue-500 text-white rounded hover:bg-blue-600 disabled:bg-gray-400"
            >
              {scanning ? "掃描中..." : `掃描 ${settings.scanDirectories.length} 個目錄`}
            </button>
          </div>
          {scanResult && (
            <p
              className={`mt-2 text-sm ${
                scanResult.includes("失敗") ? "text-red-500" : "text-green-500"
              }`}
            >
              {scanResult}
            </p>
          )}
          <p className="mt-1 text-xs text-gray-500">
            可以添加多個目錄，系統會遞迴掃描每個目錄及其所有子資料夾
          </p>
        </div>

        <div>
          <label className="block text-sm font-medium text-gray-700 mb-2">
            目錄監控 (Watcher)
          </label>
          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              onClick={handleStartWatcher}
              disabled={
                watcherBusy ||
                !!loadError ||
                watcherStatus.running ||
                settings.scanDirectories.length === 0
              }
              className="px-4 py-2 bg-emerald-600 text-white rounded hover:bg-emerald-700 disabled:bg-gray-400"
            >
              {watcherBusy && !watcherStatus.running
                ? "啟動中..."
                : "開始監控"}
            </button>
            <button
              type="button"
              onClick={handleStopWatcher}
              disabled={watcherBusy || !watcherStatus.running}
              className="px-4 py-2 bg-gray-700 text-white rounded hover:bg-gray-800 disabled:bg-gray-400"
            >
              {watcherBusy && watcherStatus.running ? "停止中..." : "停止監控"}
            </button>
            <button
              type="button"
              onClick={refreshWatcherStatus}
              className="px-3 py-2 text-sm bg-gray-200 rounded hover:bg-gray-300"
            >
              重新整理狀態
            </button>
            <span
              className={`text-sm ${
                watcherStatus.running ? "text-emerald-600" : "text-gray-500"
              }`}
            >
              {watcherStatus.running
                ? `監控中 (${watcherStatus.watchedPaths.length})`
                : "未監控"}
            </span>
          </div>
          {watcherStatus.running && watcherStatus.watchedPaths.length > 0 && (
            <ul className="mt-2 space-y-1 text-xs text-gray-600">
              {watcherStatus.watchedPaths.map((dir) => (
                <li key={dir} className="truncate">
                  {dir}
                </li>
              ))}
            </ul>
          )}
          {watcherMessage && (
            <p className="mt-2 text-sm text-gray-700">{watcherMessage}</p>
          )}
          {queueMessage && (
            <p className="mt-1 text-sm text-gray-700">{queueMessage}</p>
          )}
          {queueStatus && (
            <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-gray-600">
              <span>
                佇列 concurrency={queueStatus.concurrency} pending=
                {queueStatus.pending} running={queueStatus.running} failed=
                {queueStatus.failed} succeeded={queueStatus.succeeded}
              </span>
              <button
                type="button"
                onClick={refreshQueueStatus}
                className="px-2 py-1 bg-gray-200 rounded hover:bg-gray-300"
              >
                重新整理佇列
              </button>
              {queueStatus.failed > 0 && (
                <button
                  type="button"
                  onClick={handleRetryFailed}
                  className="px-2 py-1 bg-amber-100 text-amber-900 rounded hover:bg-amber-200"
                >
                  重試失敗任務
                </button>
              )}
            </div>
          )}
          <p className="mt-1 text-xs text-gray-500">
            偵測 Created / Moved 影片後會入庫，並將 phash／刮削→模板搬運→預覽
            送入背景任務佇列（事件：watcher-file-detected /
            watcher-pipeline-progress / task-queue-progress）。預設並發 2、同檔
            序列；可用 IDAM_QUEUE_CONCURRENCY 調整。測試可用
            IDAM_PIPELINE_DISABLE、IDAM_PIPELINE_SKIP_SCRAPE、
            IDAM_PIPELINE_SKIP_MOVE、IDAM_PIPELINE_SKIP_PREVIEW 跳過步驟。
          </p>
        </div>

        <div>
          <label className="block text-sm font-medium text-gray-700 mb-2">
            AV_Data_Capture 路徑
          </label>
          <div className="flex space-x-2">
            <input
              type="text"
              value={settings.avdcPath}
              onChange={(e) =>
                setSettings({ ...settings, avdcPath: e.target.value })
              }
              placeholder="例如: C:/AV_Data_Capture/AV_Data_Capture.py"
              className="flex-1 px-3 py-2 border border-gray-300 rounded-md"
            />
            <button
              type="button"
              onClick={() => void handlePickAvdc()}
              className="px-4 py-2 bg-slate-600 text-white rounded hover:bg-slate-700"
            >
              瀏覽…
            </button>
          </div>
          <p className="mt-1 text-xs text-gray-500">
            設定 AV_Data_Capture Python 腳本的完整路徑
          </p>
        </div>

        <div>
          <label className="block text-sm font-medium text-gray-700 mb-2">
            StashDB API URL
          </label>
          <input
            type="text"
            value={settings.stashdbUrl}
            onChange={(e) =>
              setSettings({ ...settings, stashdbUrl: e.target.value })
            }
            placeholder="https://stashdb.org"
            className="w-full px-3 py-2 border border-gray-300 rounded-md"
          />
          <p className="mt-1 text-xs text-gray-500">
            StashDB API 的基礎 URL
          </p>
        </div>

        <div>
          <label className="block text-sm font-medium text-gray-700 mb-2">
            輸出模板 (Handlebars)
          </label>
          <input
            type="text"
            value={settings.outputTemplate}
            onChange={(e) =>
              setSettings({ ...settings, outputTemplate: e.target.value })
            }
            placeholder="{{studio}}/{{date}}_{{title}}.{{ext}}"
            className="w-full px-3 py-2 border border-gray-300 rounded-md"
          />
          <p className="mt-1 text-xs text-gray-500">
            Handlebars 變數: studio date title number filename ext actor。
            相對路徑以來源檔所在目錄為基準；搬回監控樹內時會抑制 notify
            重入並略過已刮削檔。建議設定絕對圖書館根目錄以免與下載目錄重疊。
          </p>
        </div>

        <div>
          <label className="block text-sm font-medium text-gray-700 mb-2">
            連線／可用性測試
          </label>
          <div className="flex flex-wrap items-center gap-2 mb-3">
            <button
              type="button"
              onClick={() => void handleTestConnectivity()}
              disabled={connectivityBusy}
              className="px-4 py-2 bg-teal-600 text-white rounded hover:bg-teal-700 disabled:bg-gray-400"
            >
              {connectivityBusy ? "測試中..." : "測試 FFmpeg／AVDC／StashDB"}
            </button>
            <button
              type="button"
              onClick={checkFFmpeg}
              className="px-3 py-1 text-sm bg-gray-200 rounded hover:bg-gray-300"
            >
              僅檢查 FFmpeg
            </button>
          </div>
          <ul className="space-y-2 text-sm">
            <li className="flex items-start gap-2">
              <span
                className={
                  ffmpegAvailable === null
                    ? "text-gray-500"
                    : ffmpegAvailable
                      ? "text-green-600"
                      : "text-red-600"
                }
              >
                {ffmpegAvailable === null
                  ? "FFmpeg: 檢查中..."
                  : ffmpegAvailable
                    ? "✓ FFmpeg"
                    : "✗ FFmpeg"}
              </span>
              <span className="text-gray-600">
                {connectivity?.ffmpeg.message ||
                  (ffmpegAvailable
                    ? "已安裝並可用"
                    : ffmpegAvailable === false
                      ? "未安裝或不可用"
                      : "")}
              </span>
            </li>
            <li className="flex items-start gap-2">
              <span
                className={
                  connectivity == null
                    ? "text-gray-500"
                    : connectivity.avdc.ok
                      ? "text-green-600"
                      : "text-red-600"
                }
              >
                {connectivity == null
                  ? "AVDC: 尚未測試"
                  : connectivity.avdc.ok
                    ? "✓ AVDC"
                    : "✗ AVDC"}
              </span>
              <span className="text-gray-600">
                {connectivity?.avdc.message || "按上方按鈕測試腳本路徑"}
              </span>
            </li>
            <li className="flex items-start gap-2">
              <span
                className={
                  connectivity == null
                    ? "text-gray-500"
                    : connectivity.stashdb.ok
                      ? "text-green-600"
                      : "text-red-600"
                }
              >
                {connectivity == null
                  ? "StashDB: 尚未測試"
                  : connectivity.stashdb.ok
                    ? "✓ StashDB"
                    : "✗ StashDB"}
              </span>
              <span className="text-gray-600">
                {connectivity?.stashdb.message || "按上方按鈕測試 URL 可達性"}
              </span>
            </li>
          </ul>
        </div>

        <div className="pt-4 border-t">
          <button
            onClick={handleSaveSettings}
            disabled={saving || !!loadError}
            className="px-6 py-2 bg-blue-500 text-white rounded hover:bg-blue-600 disabled:bg-gray-400"
          >
            {saving ? "儲存中..." : "儲存設定"}
          </button>
        </div>
      </div>
    </div>
  );
}
