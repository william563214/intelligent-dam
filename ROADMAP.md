# Intelligent DAM — Roadmap

本文件整理 Phase 1 MVP 完成後的缺口與建議實作順序。規格細節見 [SPECIFICATION.md](./SPECIFICATION.md)；安裝與使用見 [README.md](./README.md)。

> 實作變更請透過 Pull Request 送審，**請勿直接 merge 到 main**。相關工作項會另開 GitHub Issues。

---

## 專案現況 (Status)

- **Phase 1 MVP 已完成**（見 README / SPECIFICATION）：Tauri v2 + React/TS 框架、SQLite Schema、手動掃描入庫、FFmpeg 縮圖 / Sprite Sheet、AV_Data_Capture + StashDB 刮削命令、手動整理與設定 UI。
- **下一步重點是自動化**：設定持久化、刮削端到端可靠、Watcher、模板搬運、任務佇列與狀態機，再談 UX 打磨。
- **明確延後**：ONNX / CLIP、語意搜尋、即時轉碼 — 現階段不要開工。

---

## 與 SPECIFICATION 的已知缺口 (Known Gaps)

以下依目前程式碼與文件對照，具體描述尚未對齊之處：

| 缺口 | 現況 | SPEC / 預期 |
|------|------|-------------|
| Settings 持久化 | 設定只存在前端 `localStorage`（`intelligent_dam_settings`）；後端無 `tauri-plugin-store` / JSON 設定檔 | SPEC §8.3：後端持久化掃描目錄、AVDC 路徑、StashDB URL、輸出模板 |
| Scraper 設定未完整到達後端 | 刮削時由前端從 `localStorage` 讀取後以參數傳入 `scrape_file_command`；後端本身不持有設定。若 UI 未帶參數，後端無法自行解析路徑 / URL | 後端應能讀取已持久化設定，並完成 AV_Data_Capture → StashDB fallback |
| Tags / Actors 入庫不完整 | `ScrapedMetadata` 有 `tags` / `actors`，但 `to_database_metadata` 未寫入；`scrape_file_command` 未呼叫 `FileTags` 寫入 | 刮削成功應寫入 Metadata + Tags / Actors |
| `phash` | `Files.phash` 欄位存在；掃描時先設 `None`，import 時用 size + mtime 的 stub hash，**非**感知雜湊，不利 StashDB `?phash=` | SPEC：掃描 / 指紋步驟計算可用的 PHash |
| Sprite / VTT ↔ hover preview | 後端可生成 Sprite Sheet 與 VTT，並寫入 `sprite_sheet_path`；`VideoCard` hover 仍以封面圖為主，未依 VTT 做 scrubbing | SPEC §3.2：懸停依時間點顯示 Sprite 預覽 |
| `notify` 無 Watcher | `Cargo.toml` 已依賴 `notify`，`src-tauri/src` 無 Watcher 模組 | SPEC §3.1 / §5：Downloads 監控 Created / Moved |
| Output path template | Settings UI 有 Handlebars 模板欄位；後端無 rename / move，亦無 Handlebars 依賴 | SPEC：依 `Output_Template` 建立目錄並 `fs::rename`，更新 `Files.path` |
| 檔案狀態機 | Schema 有 `status`（預設 `ready`）與 `update_file_status`；刮削失敗多回傳 error，未系統性標記 `processing` / `scrape_failed` / `missing` | SPEC：處理中、刮削失敗、檔案遺失等狀態可追蹤 |

---

## 分階段 Checklist（建議順序）

### Phase 0 — MVP wrap-up（優先完成）

補齊 MVP 與 SPEC 的落差，再進入自動化。

- [ ] **1. Persist settings on Tauri side** — 不以僅 `localStorage` 為準；後端可讀寫掃描目錄、AVDC 路徑、StashDB URL、輸出模板
- [ ] **2. End-to-end scraping** — tags / actors 寫入 DB；策略固定為 AV_Data_Capture → StashDB fallback；失敗可標記狀態
- [ ] **3. Compute phash during scan** — 入庫時產出可供 StashDB / 去重使用的 hash（必要時替換現有 stub）
- [ ] **4. Wire hover preview to Sprite / VTT** — `VideoCard`（或等效 UI）依 `sprite_sheet_path` / VTT 做懸停 scrubbing
- [ ] **5. File status machine** — 使用並維護 `processing` / `scrape_failed` / `missing`（及既有 `ready`）

### Phase 1 — Automation core

- [ ] **6. notify Watcher** — 監聽設定中的攝取目錄（Created / Moved）
- [ ] **7. Wait until file write completes** — 寫入完成（例如 file lock / 穩定度檢測）後才處理
- [ ] **8. Auto pipeline** — ingest → scrape → template move → preview
- [ ] **9. Handlebars path template rename / move** — 依輸出模板搬運並更新 DB path
- [ ] **10. Task queue + progress events** — 背景佇列與進度回報給前端
- [ ] **11. Dedup by phash / path** — 依 phash 與 path 避免重複入庫或重複處理

### Phase 2 — UX polish

- [ ] 一鍵整理（對齊手動整理流程的批次入口）
- [ ] 批次操作（多選刮削 / 預覽 / 搬運等）
- [ ] 錯誤日誌（刮削 / FFmpeg / 搬運失敗可查）
- [ ] Settings：資料夾選擇器 + 連線／可用性測試（FFmpeg、AVDC、StashDB 等）

### Phase 3 — Later（明確延後，現階段不要開工）

- [ ] ONNX Runtime + CLIP
- [ ] 語意搜尋 / 向量（sqlite-vss 等）
- [ ] 即時轉碼（例如 H.265 → H.264 串流）

---

## 相關文件

- [SPECIFICATION.md](./SPECIFICATION.md) — 架構、Schema、自動化流程與刮削規格
- [README.md](./README.md) — 安裝、開發、MVP 已知限制

---

## 協作備註

- 實作請開 PR；本 roadmap 僅文件與優先序，**不 merge 實作分支進 main 除非經審查**。
- 細項追蹤以 GitHub Issues 為準；本檔為總覽與建議順序。
