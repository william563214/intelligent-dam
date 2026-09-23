import { useState, useEffect } from "react";
import { getAllFiles, scrapeFile, updateFileMetadata, generatePreview } from "../api";
import type { FileWithMetadata, Metadata, Tag } from "../types";

export default function ManualOrganize() {
  const [files, setFiles] = useState<FileWithMetadata[]>([]);
  const [selectedFile, setSelectedFile] = useState<FileWithMetadata | null>(null);
  const [loading, setLoading] = useState(false);
  const [scraping, setScraping] = useState(false);
  const [editing, setEditing] = useState(false);
  const [formData, setFormData] = useState<Partial<Metadata>>({});

  useEffect(() => {
    loadFiles();
  }, []);

  useEffect(() => {
    if (selectedFile?.metadata) {
      setFormData(selectedFile.metadata);
    } else if (selectedFile) {
      setFormData({
        file_id: selectedFile.file.id,
        title: null,
        studio: null,
        release_date: null,
        summary: null,
        cover_image: null,
        sprite_sheet_path: null,
        source_type: null,
      });
    }
  }, [selectedFile]);

  const loadFiles = async (keepSelectedId?: number) => {
    try {
      setLoading(true);
      const data = await getAllFiles();
      setFiles(data);
      if (keepSelectedId !== undefined) {
        const refreshed = data.find((f) => f.file.id === keepSelectedId) || null;
        setSelectedFile(refreshed);
      }
    } catch (err) {
      console.error("Failed to load files:", err);
    } finally {
      setLoading(false);
    }
  };

  const handleScrape = async () => {
    if (!selectedFile) return;

    try {
      setScraping(true);
      // 由 Rust 端讀取持久化設定（AVDC / StashDB）
      const result = await scrapeFile(selectedFile.file.id);
      if (result.success && result.metadata) {
        setFormData(result.metadata);
        await loadFiles(selectedFile.file.id);
        alert("刮削成功！");
      } else {
        await loadFiles(selectedFile.file.id);
        alert(`刮削失敗: ${result.error || "未知錯誤"}`);
      }
    } catch (err) {
      alert(`刮削錯誤: ${err instanceof Error ? err.message : "未知錯誤"}`);
    } finally {
      setScraping(false);
    }
  };

  const handleGeneratePreview = async () => {
    if (!selectedFile) return;

    try {
      setScraping(true);
      await generatePreview(selectedFile.file.id);
      await loadFiles(selectedFile.file.id);
      alert("預覽生成成功！");
    } catch (err) {
      alert(`生成預覽錯誤: ${err instanceof Error ? err.message : "未知錯誤"}`);
    } finally {
      setScraping(false);
    }
  };

  const handleSave = async () => {
    if (!selectedFile) return;

    try {
      setScraping(true);
      const metadata: Metadata = {
        file_id: selectedFile.file.id,
        title: formData.title || null,
        studio: formData.studio || null,
        release_date: formData.release_date || null,
        summary: formData.summary || null,
        cover_image: formData.cover_image || null,
        sprite_sheet_path: formData.sprite_sheet_path || null,
        source_type: formData.source_type || null,
      };
      await updateFileMetadata(selectedFile.file.id, metadata);
      await loadFiles(selectedFile.file.id);
      setEditing(false);
      alert("儲存成功！");
    } catch (err) {
      alert(`儲存錯誤: ${err instanceof Error ? err.message : "未知錯誤"}`);
    } finally {
      setScraping(false);
    }
  };

  const actorTags = (selectedFile?.tags || []).filter(
    (t) => (t.category || "").toLowerCase() === "actor"
  );
  const otherTags = (selectedFile?.tags || []).filter(
    (t) => (t.category || "").toLowerCase() !== "actor"
  );

  const renderTagList = (items: Tag[], emptyLabel: string) => {
    if (items.length === 0) {
      return <span className="text-sm text-gray-400">{emptyLabel}</span>;
    }
    return (
      <div className="flex flex-wrap gap-1">
        {items.map((tag) => (
          <span
            key={tag.id}
            className="px-2 py-1 bg-blue-100 text-blue-800 text-xs rounded"
          >
            {tag.name}
          </span>
        ))}
      </div>
    );
  };

  if (loading && files.length === 0) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="text-gray-500">載入中...</div>
      </div>
    );
  }

  return (
    <div className="p-6">
      <h2 className="text-2xl font-bold text-gray-900 mb-4">手動整理</h2>
      <div className="grid grid-cols-2 gap-6">
        <div>
          <h3 className="text-lg font-semibold mb-2">選擇檔案</h3>
          <div className="bg-white rounded-lg shadow max-h-96 overflow-y-auto">
            {files.map((file) => (
              <div
                key={file.file.id}
                onClick={() => setSelectedFile(file)}
                className={`p-3 border-b cursor-pointer hover:bg-gray-50 ${
                  selectedFile?.file.id === file.file.id
                    ? "bg-blue-50 border-blue-500"
                    : ""
                }`}
              >
                <div className="font-medium truncate">
                  {file.metadata?.title || file.file.path.split(/[/\\]/).pop()}
                </div>
                <div className="text-sm text-gray-500 truncate">
                  {file.file.path}
                </div>
                <div className="text-xs text-gray-400 mt-1">
                  狀態: {file.file.status}
                  {file.tags.length > 0 ? ` · 標籤 ${file.tags.length}` : ""}
                </div>
              </div>
            ))}
          </div>
        </div>
        <div>
          {selectedFile ? (
            <div>
              <h3 className="text-lg font-semibold mb-2">編輯元資料</h3>
              <div className="bg-white rounded-lg shadow p-4 space-y-4">
                <div className="text-sm text-gray-600">
                  狀態:{" "}
                  <span
                    className={
                      selectedFile.file.status === "scrape_failed"
                        ? "text-red-600"
                        : "text-gray-800"
                    }
                  >
                    {selectedFile.file.status}
                  </span>
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">
                    標題
                  </label>
                  <input
                    type="text"
                    value={formData.title || ""}
                    onChange={(e) =>
                      setFormData({ ...formData, title: e.target.value })
                    }
                    disabled={!editing}
                    className="w-full px-3 py-2 border border-gray-300 rounded-md disabled:bg-gray-100"
                  />
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">
                    工作室
                  </label>
                  <input
                    type="text"
                    value={formData.studio || ""}
                    onChange={(e) =>
                      setFormData({ ...formData, studio: e.target.value })
                    }
                    disabled={!editing}
                    className="w-full px-3 py-2 border border-gray-300 rounded-md disabled:bg-gray-100"
                  />
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">
                    發行日期
                  </label>
                  <input
                    type="text"
                    value={formData.release_date || ""}
                    onChange={(e) =>
                      setFormData({ ...formData, release_date: e.target.value })
                    }
                    disabled={!editing}
                    className="w-full px-3 py-2 border border-gray-300 rounded-md disabled:bg-gray-100"
                  />
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">
                    簡介
                  </label>
                  <textarea
                    value={formData.summary || ""}
                    onChange={(e) =>
                      setFormData({ ...formData, summary: e.target.value })
                    }
                    disabled={!editing}
                    rows={4}
                    className="w-full px-3 py-2 border border-gray-300 rounded-md disabled:bg-gray-100"
                  />
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">
                    封面圖片 URL
                  </label>
                  <input
                    type="text"
                    value={formData.cover_image || ""}
                    onChange={(e) =>
                      setFormData({ ...formData, cover_image: e.target.value })
                    }
                    disabled={!editing}
                    className="w-full px-3 py-2 border border-gray-300 rounded-md disabled:bg-gray-100"
                  />
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">
                    演員
                  </label>
                  {renderTagList(actorTags, "尚無演員")}
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">
                    標籤
                  </label>
                  {renderTagList(otherTags, "尚無標籤")}
                </div>
                <div className="flex space-x-2">
                  {!editing ? (
                    <>
                      <button
                        onClick={() => setEditing(true)}
                        className="px-4 py-2 bg-blue-500 text-white rounded hover:bg-blue-600"
                      >
                        編輯
                      </button>
                      <button
                        onClick={handleScrape}
                        disabled={scraping}
                        className="px-4 py-2 bg-green-500 text-white rounded hover:bg-green-600 disabled:bg-gray-400"
                      >
                        {scraping ? "刮削中..." : "網路刮削"}
                      </button>
                      <button
                        onClick={handleGeneratePreview}
                        disabled={scraping}
                        className="px-4 py-2 bg-purple-500 text-white rounded hover:bg-purple-600 disabled:bg-gray-400"
                      >
                        {scraping ? "生成中..." : "生成預覽"}
                      </button>
                    </>
                  ) : (
                    <>
                      <button
                        onClick={handleSave}
                        disabled={scraping}
                        className="px-4 py-2 bg-blue-500 text-white rounded hover:bg-blue-600 disabled:bg-gray-400"
                      >
                        {scraping ? "儲存中..." : "儲存"}
                      </button>
                      <button
                        onClick={() => {
                          setEditing(false);
                          if (selectedFile?.metadata) {
                            setFormData(selectedFile.metadata);
                          }
                        }}
                        className="px-4 py-2 bg-gray-500 text-white rounded hover:bg-gray-600"
                      >
                        取消
                      </button>
                    </>
                  )}
                </div>
              </div>
            </div>
          ) : (
            <div className="bg-white rounded-lg shadow p-4 text-center text-gray-500">
              請選擇一個檔案進行編輯
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
