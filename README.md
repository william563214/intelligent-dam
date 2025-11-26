# Intelligent DAM - 智慧型本地影音資產管理系統

## 專案概述

Intelligent DAM 是一款 Windows 平台下的高效能桌面應用程式，用於管理本地端的大量影片資產。系統透過「邏輯層」與「實體層」分離的架構，解決傳統檔案總管無法處理元資料（Metadata）的痛點。

## 核心功能

- **自動化攝取**: 監控下載目錄，自動完成「識別、刮削、改名、搬運」全流程
- **混合智慧**: 優先使用網路刮削（AV_Data_Capture + StashDB）獲取精確資料
- **極致體驗**: 採用 Rust 打造零延遲後端，並實現類似 Stash 的滑鼠懸停預覽（Video Scrubbing）

## 技術堆疊

- **前端**: React + TypeScript + Vite + TailwindCSS
- **後端**: Rust + Tauri v2
- **資料庫**: SQLite
- **媒體處理**: FFmpeg
- **網路刮削**: AV_Data_Capture (Python) + StashDB API

## 系統需求

- Windows 10/11 (或 macOS/Linux，Tauri 支援跨平台)
- Node.js 18+ 和 npm
- Rust 1.70+
- Python 3.x (用於 AV_Data_Capture)
- FFmpeg (用於媒體處理)

## 安裝步驟

### 1. 安裝依賴

```bash
# 安裝前端依賴
npm install

# Rust 依賴會在首次編譯時自動下載
```

### 2. 安裝 FFmpeg

**Windows:**
- 下載 FFmpeg 並解壓到系統路徑，或使用 Chocolatey: `choco install ffmpeg`

**macOS:**
```bash
brew install ffmpeg
```

**Linux:**
```bash
sudo apt-get install ffmpeg  # Debian/Ubuntu
sudo yum install ffmpeg      # CentOS/RHEL
```

### 3. 設定 AV_Data_Capture (可選)

1. 下載 [AV_Data_Capture](https://github.com/YEreed/AV_Data_Capture)
2. 在應用程式的「設定」頁面中配置 AV_Data_Capture 的路徑

## 開發模式

```bash
# 啟動開發伺服器
npm run dev

# 或使用 Tauri 開發模式（會同時啟動前端和後端）
npm run tauri dev
```

## 建置

```bash
# 建置前端
npm run build

# 建置 Tauri 應用程式
npm run tauri build
```

## 使用說明

### 1. 掃描目錄

1. 開啟應用程式
2. 進入「設定」頁面
3. 設定要掃描的目錄路徑
4. 點擊「掃描目錄」按鈕
5. 系統會自動掃描目錄中的所有影片檔案並入庫

### 2. 瀏覽檔案

- 在「檔案瀏覽器」頁面查看所有已掃描的影片
- 滑鼠懸停在影片卡片上可預覽（需要先生成預覽）

### 3. 手動整理

1. 進入「手動整理」頁面
2. 選擇要整理的檔案
3. 點擊「網路刮削」自動獲取元資料
4. 或手動編輯元資料後點擊「儲存」

### 4. 生成預覽

在手動整理頁面中，選擇檔案後點擊「生成預覽」按鈕，系統會使用 FFmpeg 生成 Sprite Sheet 用於懸停預覽。

## 專案結構

```
william_video/
├── src/                    # 前端 React 應用
│   ├── components/        # React 組件
│   ├── api.ts             # Tauri API 調用
│   └── types.ts           # TypeScript 類型定義
├── src-tauri/             # Tauri 後端
│   ├── src/
│   │   ├── database.rs    # 資料庫模組
│   │   ├── scanner.rs      # 檔案掃描器
│   │   ├── scraper.rs      # 網路刮削整合
│   │   ├── ffmpeg.rs       # FFmpeg 整合
│   │   ├── commands.rs     # Tauri 命令
│   │   └── lib.rs          # 主程式
│   └── Cargo.toml          # Rust 依賴
└── package.json           # Node.js 依賴
```

## 資料庫結構

系統使用 SQLite 儲存資料，包含以下表：

- `Files`: 檔案索引表（物理層）
- `Metadata`: 元資料表（邏輯層）
- `Tags`: 標籤表
- `FileTags`: 檔案標籤關聯表

資料庫檔案位置：應用程式資料目錄下的 `intelligent_dam.db`

## 已知限制（第一階段 MVP）

- 不包含自動監控（Watcher），需手動觸發掃描
- 不包含 AI 分析（第三階段功能）
- 不包含向量搜尋（第三階段功能）
- 檔案搬運功能為手動觸發，不自動執行

## 開發路線圖

- **第一階段（當前）**: MVP 基礎建設 ✅
- **第二階段**: 自動化與監控（Watcher Service）
- **第三階段**: AI 整合（ONNX Runtime + CLIP 模型）

## 授權

本專案為個人專案，僅供學習和研究使用。

## 貢獻

歡迎提交 Issue 和 Pull Request！
# intelligent-dam
