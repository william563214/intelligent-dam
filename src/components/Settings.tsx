import { useState, useEffect } from "react";
import { scanMultipleDirectories, checkFFmpegAvailable } from "../api";

interface SettingsData {
  scanDirectories: string[];
  avdcPath: string;
  stashdbUrl: string;
  outputTemplate: string;
}

export default function Settings() {
  const [settings, setSettings] = useState<SettingsData>({
    scanDirectories: [],
    avdcPath: "",
    stashdbUrl: "https://stashdb.org",
    outputTemplate: "{{studio}}/{{date}}_{{title}}.mp4",
  });
  const [newDirectory, setNewDirectory] = useState("");
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
    if (settings.scanDirectories.length === 0) {
      alert("請先添加至少一個掃描目錄");
      return;
    }

    try {
      setScanning(true);
      setScanResult(null);
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

  return (
    <div className="p-6">
      <h2 className="text-2xl font-bold text-gray-900 mb-4">設定</h2>
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
              disabled={scanning || settings.scanDirectories.length === 0}
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

