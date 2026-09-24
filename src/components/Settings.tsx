import { useState, useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  scanMultipleDirectories,
  checkFFmpegAvailable,
  getSettings,
  saveSettings,
  startWatcher,
  stopWatcher,
  getWatcherStatus,
} from "../api";
import type { AppSettings, WatcherFileEvent, WatcherStatus } from "../types";

const default_settings: AppSettings = {
  scanDirectories: [],
  avdcPath: "",
  stashdbUrl: "https://stashdb.org",
  outputTemplate: "{{studio}}/{{date}}_{{title}}.mp4",
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

  useEffect(() => {
    loadSettings();
    checkFFmpeg();
    refreshWatcherStatus();
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<WatcherFileEvent>("watcher-file-detected", (event) => {
      const payload = event.payload;
      if (payload.error) {
        setWatcherMessage(`偵測失敗: ${payload.path} (${payload.error})`);
        return;
      }
      setWatcherMessage(
        `偵測到新檔 (${payload.kind}): ${payload.path}` +
          (payload.fileId != null ? ` #${payload.fileId}` : "")
      );
    })
      .then((fn) => {
        unlisten = fn;
      })
      .catch((err) => {
        console.error("Failed to listen watcher events:", err);
      });
    return () => {
      if (unlisten) {
        unlisten();
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

  const handleAddDirectory = () => {
    if (newDirectory.trim() && !settings.scanDirectories.includes(newDirectory.trim())) {
      setSettings({
        ...settings,
        scanDirectories: [...settings.scanDirectories, newDirectory.trim()],
      });
      setNewDirectory("");
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
                    handleAddDirectory();
                  }
                }}
                placeholder="例如: C:/Videos 或 /Users/username/Videos"
                className="flex-1 px-3 py-2 border border-gray-300 rounded-md"
              />
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
          <p className="mt-1 text-xs text-gray-500">
            使用設定中的掃描目錄；偵測 Created / Moved 的影片後會入庫並送出
            watcher-file-detected 事件（完整刮削搬運流程另案實作）
          </p>
        </div>

        <div>
          <label className="block text-sm font-medium text-gray-700 mb-2">
            AV_Data_Capture 路徑
          </label>
          <input
            type="text"
            value={settings.avdcPath}
            onChange={(e) =>
              setSettings({ ...settings, avdcPath: e.target.value })
            }
            placeholder="例如: C:/AV_Data_Capture/AV_Data_Capture.py"
            className="w-full px-3 py-2 border border-gray-300 rounded-md"
          />
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
            placeholder="{{studio}}/{{date}}_{{title}}.mp4"
            className="w-full px-3 py-2 border border-gray-300 rounded-md"
          />
          <p className="mt-1 text-xs text-gray-500">
            使用 Handlebars 語法，可用變數: studio, date, title, number 等
          </p>
        </div>

        <div>
          <label className="block text-sm font-medium text-gray-700 mb-2">
            FFmpeg 狀態
          </label>
          <div className="flex items-center space-x-2">
            {ffmpegAvailable === null ? (
              <span className="text-gray-500">檢查中...</span>
            ) : ffmpegAvailable ? (
              <span className="text-green-500">✓ FFmpeg 已安裝並可用</span>
            ) : (
              <span className="text-red-500">
                ✗ FFmpeg 未安裝或不可用
              </span>
            )}
            <button
              onClick={checkFFmpeg}
              className="px-3 py-1 text-sm bg-gray-200 rounded hover:bg-gray-300"
            >
              重新檢查
            </button>
          </div>
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
