# 專案規格書：智慧型本地影音資產管理系統 (Intelligent DAM)

## 1. 專案概述 (Project Overview)

本專案旨在開發一款 Windows 平台下的高效能桌面應用程式，用於管理本地端的大量影片資產。系統透過「邏輯層」與「實體層」分離的架構，解決傳統檔案總管無法處理元資料（Metadata）的痛點。

### 核心價值主張

- **自動化攝取 (Auto-Ingestion)**: 監控下載目錄，自動完成「識別、刮削、改名、搬運」全流程。
- **混合智慧 (Hybrid Intelligence)**: 優先使用網路刮削 (Web Scraping) 獲取精確資料，並以本地 AI (Local AI) 填補未知檔案的分類空缺。
- **極致體驗**: 採用 Rust 打造零延遲後端，並實現類似 Stash 的滑鼠懸停預覽 (Video Scrubbing)。

## 2. 系統架構設計 (System Architecture)

採用 Tauri v2 架構，確保前端的現代化開發體驗與後端的原生效能。

### 2.1 技術堆疊 (Tech Stack)

| 層級 | 技術選型 | 關鍵套件/庫 | 選擇理由 |
|------|---------|------------|---------|
| 表現層 (Frontend) | React + TypeScript | Vite, TailwindCSS | 現代化 UI，開發效率高，組件豐富。 |
| 應用層 (Host) | Tauri (Rust) | tauri, tauri-plugin-window | 極致輕量 (相較 Electron)，安全性高，原生整合。 |
| 邏輯核心 (Backend) | Rust | tokio (非同步), notify (監控) | 記憶體安全，適合處理高併發 I/O 與運算。 |
| 資料層 (Data) | SQLite | rusqlite, sqlite-vss | 單一檔案資料庫，支援向量搜尋 (Vector Search)。 |
| 媒體處理 | FFmpeg | std::process::Command | 業界標準，用於轉碼、截圖、生成 Sprite Sheet。 |
| AI 推論 | ONNX Runtime | ort | 支援跨平台加速，可運行 CLIP/BLIP 模型。 |

## 3. 功能模組規格 (Functional Specifications)

### 3.1 自動攝取與整理引擎 (Ingestion Engine)

系統將在背景運行一個 Watcher Service。

#### 監控
- 監聽指定資料夾 (如 Downloads) 的 Created 與 Moved 事件。
- 穩定性檢測: 使用檔案鎖 (File Lock) 機制，確保檔案下載/寫入完成後才開始處理。

#### 刮削 (Scraping)
- 基於檔名或 Hash 查詢外部 API (如 StashDB, TMDB)。
- 若成功：獲取標題、封面、標籤、演員。
- 若失敗：標記為 Scrape_Failed，觸發 AI 分析流程。

#### 搬運 (Organization)
- 支援 Handlebars 模板路徑，例如：`D:/Library/{{studio}}/{{date}}_{{title}}.mp4`。
- 自動建立目錄並移動檔案。

### 3.2 媒體體驗與預覽 (Media Experience)

#### Sprite Sheet 生成
- 針對每個影片，背景調用 FFmpeg 生成 10x10 的拼接圖 (VTT Sprites)。
- 互動: 滑鼠懸停於進度條時，即時顯示該時間點的預覽畫面 (仿 Stash/YouTube 體驗)。

#### 即時轉碼
- 若瀏覽器不支援該編碼 (如 H.265)，即時透過 Rust 串流轉碼為 H.264。

### 3.3 AI 智慧分析 (Local AI Analysis)

當網路刮削失敗，或使用者主動要求分析時觸發。

#### 內容識別
- 使用 CLIP 模型分析關鍵影格，生成標籤 (如：Outdoors, Beach, Car)。

#### 語意搜尋
- 將影片內容轉為向量 (Vector) 存入資料庫，支援自然語言搜尋 (如搜尋「穿紅衣服的人」)。

## 4. 資料庫設計 (Database Schema)

使用 SQLite，設計需兼顧「外部資料」與「AI 資料」。

```sql
-- 1. 檔案索引表 (物理層)
CREATE TABLE Files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,  -- 當前實際儲存路徑
    size INTEGER,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    phash TEXT,                 -- 感知雜湊 (用於偵測重複影片)
    status TEXT DEFAULT 'ready'  -- 'processing', 'ready', 'missing'
);

-- 2. 元資料表 (邏輯層)
CREATE TABLE Metadata (
    file_id INTEGER PRIMARY KEY,
    title TEXT,
    studio TEXT,
    release_date TEXT,
    summary TEXT,
    cover_image TEXT,
    sprite_sheet_path TEXT,     -- 預覽拼圖路徑
    source_type TEXT,           -- 'scraped' (網路) 或 'ai_generated' (AI)
    FOREIGN KEY(file_id) REFERENCES Files(id)
);

-- 3. 標籤與分類
CREATE TABLE Tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT UNIQUE NOT NULL,
    category TEXT               -- 'Actor', 'Genre', 'Studio', 'Tag'
);

CREATE TABLE FileTags (
    file_id INTEGER,
    tag_id INTEGER,
    confidence FLOAT DEFAULT 1.0, -- 網路資料為 1.0，AI 根據推論分數
    PRIMARY KEY (file_id, tag_id),
    FOREIGN KEY(file_id) REFERENCES Files(id),
    FOREIGN KEY(tag_id) REFERENCES Tags(id)
);

-- 4. 向量搜尋索引 (AI 層) - 第三階段實作
CREATE VIRTUAL TABLE Embeddings USING vss0(
    video_vector(512)           -- 儲存 CLIP 模型產出的向量
);
```

## 5. 自動化工作流程邏輯 (Automation Workflow)

這是 Rust 後端的核心邏輯流程圖：

1. **觸發 (Trigger)**: notify 偵測到 Downloads/new_video.mp4。
2. **等待 (Wait)**: 迴圈檢查 `File::open().try_lock_exclusive()` 直到成功 (代表下載軟體已釋放檔案)。
3. **指紋 (Hash)**: 計算部分 Hash (快速) 與 PHash。
4. **決策分支 (Decision)**:
   - **路徑 A (Scrape)**: 呼叫 API 搜尋。
     - 成功: 寫入 Metadata 表 (source='scraped')。
     - 失敗: 進入路徑 B。
   - **路徑 B (AI Analysis)**: 
     - FFmpeg 截取 5 張關鍵圖。
     - ONNX Runtime (CLIP) 分析圖片。
     - 生成標籤與向量，寫入 Metadata (source='ai')。
5. **執行搬運 (Action)**: 
   - 讀取設定檔中的 Output_Template。
   - `fs::rename()` 移動檔案。
   - 更新 Files 表中的 path 欄位。
6. **後處理 (Post-Processing)**: 
   - 生成 Sprite Sheet (用於 UI 預覽)。
   - 前端接收 WebSocket 通知：「新影片已整理完畢」。

## 6. 開發階段規劃 (Roadmap)

### 第一階段：MVP 基礎建設 (Foundation) ✅

**目標**: 可用的檔案瀏覽器，手動整理功能。

**工作**:
- ✅ 搭建 Tauri + React 框架。
- ✅ Rust 實作 scanner (掃描現有資料夾入庫)。
- ✅ 整合 rusqlite 與基礎 Schema。
- ✅ FFmpeg 生成縮圖與 Sprite Sheet (最優先實作，確保體驗)。
- ✅ 整合 AV_Data_Capture 作為主要刮削工具。
- ✅ 整合 StashDB 作為備選刮削方案。

### 第二階段：自動化與刮削 (Automation & Scraping)

**目標**: 實現「監控 -> 刮削 -> 搬運」流程。

**工作**:
- 實作 Watcher 監控模組。
- 實作 Scraper Trait (對接 1-2 個外部網站)。
- 實作 Handlebars 路徑重命名邏輯。

### 第三階段：AI 整合 (AI Integration)

**目標**: 實現本地 AI 補全與語意搜尋。

**工作**:
- 整合 ort (ONNX Runtime)。
- 下載並載入量化版 CLIP 模型。
- 實作向量資料庫寫入與搜尋介面。

## 7. 網路刮削整合

### 7.1 AV_Data_Capture 整合

本專案整合了 [AV_Data_Capture](https://github.com/YEreed/AV_Data_Capture) 作為主要刮削工具。

**實作方式**: 透過 `std::process::Command` 調用 Python 腳本

**前置需求**:
- 系統需安裝 Python 3.x
- AV_Data_Capture 需下載至本地或透過 pip 安裝

**輸出解析**:
- AV_Data_Capture 會在輸出目錄生成 NFO 檔案
- 解析 NFO 取得：title, actor, studio, release_date, cover_image, tags

### 7.2 StashDB API 整合（備選）

**實作方式**: HTTP API 調用

**API 端點**: `GET /api/v1/scenes?phash={phash}`

**優先順序**: AV_Data_Capture → StashDB → 標記為 `scrape_failed`

## 8. 技術細節

### 8.1 FFmpeg 整合

- **縮圖生成**: `ffmpeg -i input.mp4 -ss 00:00:01 -vframes 1 output.jpg`
- **Sprite Sheet**: `ffmpeg -i input.mp4 -vf "fps=1/10,scale=160:90,tile=10x10" output.jpg`
- **VTT 檔案**: 自動生成對應的 WebVTT 檔案用於前端預覽

### 8.2 錯誤處理

- AV_Data_Capture 執行失敗 → 嘗試 StashDB
- StashDB 失敗 → 標記為 `scrape_failed`，等待 AI 分析（第三階段）
- FFmpeg 失敗 → 記錄錯誤，不阻塞主流程

### 8.3 配置管理

- 使用 Tauri 的 `tauri-plugin-store` 或簡單的 JSON 檔案
- 儲存：掃描目錄、AV_Data_Capture 路徑、StashDB API URL、輸出模板

## 9. 已知限制（第一階段 MVP）

- 不包含自動監控（Watcher），需手動觸發掃描
- 不包含 AI 分析（第三階段）
- 不包含向量搜尋（第三階段）
- 檔案搬運功能為手動觸發，不自動執行

## 10. 授權與貢獻

本專案為個人專案，僅供學習和研究使用。

歡迎提交 Issue 和 Pull Request！

