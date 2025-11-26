import { useState, useEffect } from "react";
import { scanDirectory, checkFFmpegAvailable } from "../api";

interface SettingsData {
  scanDirectory: string;
  avdcPath: string;
  stashdbUrl: string;
  outputTemplate: string;
}

export default function Settings() {
  const [settings, setSettings] = useState<SettingsData>({
    scanDirectory: "",
    avdcPath: "",
    stashdbUrl: "https://stashdb.org",
    outputTemplate: "{{studio}}/{{date}}_{{title}}.mp4",
  });
  const [scanning, setScanning] = useState(false);
  const [scanResult, setScanResult] = useState<string | null>(null);
  const [ffmpegAvailable, setFFmpegAvailable] = useState<boolean | null>(null);

  useEffect(() => {
    loadSettings();
    checkFFmpeg();
  }, []);

  const loadSettings = () => {
    // 從 localStorage 載入設定
    const saved = localStorage.getItem("intelligent_dam_settings");
    if (saved) {
      try {
        const parsed = JSON.parse(saved);
        setSettings({ ...settings, ...parsed });
      } catch (e) {
        console.error("Failed to parse settings:", e);
      }
    }
  };

  const saveSettings = () => {
    localStorage.setItem("intelligent_dam_settings", JSON.stringify(settings));
    alert("設定已儲存！");
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

  const handleScan = async () => {
    if (!settings.scanDirectory) {
      alert("請先設定掃描目錄");
      return;
    }

    try {
      setScanning(true);
      setScanResult(null);
      const result = await scanDirectory(settings.scanDirectory);
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

  return (
    <div className="p-6">
      <h2 className="text-2xl font-bold text-gray-900 mb-4">設定</h2>
      <div className="bg-white rounded-lg shadow p-6 space-y-6">
        <div>
          <label className="block text-sm font-medium text-gray-700 mb-2">
            掃描目錄
          </label>
          <div className="flex space-x-2">
            <input
              type="text"
              value={settings.scanDirectory}
              onChange={(e) =>
                setSettings({ ...settings, scanDirectory: e.target.value })
              }
              placeholder="例如: C:/Videos 或 /Users/username/Videos"
              className="flex-1 px-3 py-2 border border-gray-300 rounded-md"
            />
            <button
              onClick={handleScan}
              disabled={scanning || !settings.scanDirectory}
              className="px-4 py-2 bg-blue-500 text-white rounded hover:bg-blue-600 disabled:bg-gray-400"
            >
              {scanning ? "掃描中..." : "掃描目錄"}
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
            onClick={saveSettings}
            className="px-6 py-2 bg-blue-500 text-white rounded hover:bg-blue-600"
          >
            儲存設定
          </button>
        </div>
      </div>
    </div>
  );
}

