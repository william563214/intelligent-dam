import { useState } from "react";
import FileBrowser from "./components/FileBrowser";
import ManualOrganize from "./components/ManualOrganize";
import Settings from "./components/Settings";

type Tab = "browser" | "organize" | "settings";

function App() {
  const [activeTab, setActiveTab] = useState<Tab>("browser");

  return (
    <div className="min-h-screen bg-gray-100">
      <header className="bg-white shadow">
        <div className="max-w-7xl mx-auto py-6 px-4 sm:px-6 lg:px-8">
          <h1 className="text-3xl font-bold text-gray-900">
            Intelligent DAM
          </h1>
        </div>
      </header>
      <nav className="bg-white border-b">
        <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
          <div className="flex space-x-8">
            <button
              onClick={() => setActiveTab("browser")}
              className={`py-4 px-1 border-b-2 font-medium text-sm ${
                activeTab === "browser"
                  ? "border-blue-500 text-blue-600"
                  : "border-transparent text-gray-500 hover:text-gray-700 hover:border-gray-300"
              }`}
            >
              檔案瀏覽器
            </button>
            <button
              onClick={() => setActiveTab("organize")}
              className={`py-4 px-1 border-b-2 font-medium text-sm ${
                activeTab === "organize"
                  ? "border-blue-500 text-blue-600"
                  : "border-transparent text-gray-500 hover:text-gray-700 hover:border-gray-300"
              }`}
            >
              手動整理
            </button>
            <button
              onClick={() => setActiveTab("settings")}
              className={`py-4 px-1 border-b-2 font-medium text-sm ${
                activeTab === "settings"
                  ? "border-blue-500 text-blue-600"
                  : "border-transparent text-gray-500 hover:text-gray-700 hover:border-gray-300"
              }`}
            >
              設定
            </button>
          </div>
        </div>
      </nav>
      <main className="max-w-7xl mx-auto py-6 sm:px-6 lg:px-8">
        {activeTab === "browser" && <FileBrowser />}
        {activeTab === "organize" && <ManualOrganize />}
        {activeTab === "settings" && <Settings />}
      </main>
    </div>
  );
}

export default App;

