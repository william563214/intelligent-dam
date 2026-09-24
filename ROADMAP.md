# Intelligent DAM — Roadmap

本文件整理 **第一階段 MVP** 完成後的缺口與建議實作順序。規格細節見 [SPECIFICATION.md](./SPECIFICATION.md)；安裝與使用見 [README.md](./README.md)。

> 實作變更請透過 Pull Request 送審，**請勿直接 merge 到 main**（合併請威廉明確指示）。細項以 GitHub Issues 為準。

---

## 階段編號對照 (Phase numbering)

SPECIFICATION / README 使用「第 N 階段」；本檔的 wrap-up / automation checklist 為 MVP 之後的工作包。對照如下：

| SPEC / README | 本檔 | 說明 |
|---------------|------|------|
| **第一階段** MVP 基礎建設 ✅ | （已完成）+ 下方 **MVP wrap-up**（items #1–#5） | Foundation 已落地；wrap-up 補齊與 SPEC 的落差，對應 Issues #1–#5 |
| **第二階段** 自動化與刮削 | **Automation core**（items #6–#11） | Watcher、穩定度檢測、pipeline、Handlebars、佇列、去重 |
| （README／SPEC 未單列） | **UX polish** | 一鍵整理、批次、錯誤日誌、Settings 選擇器／連線測試 |
| **第三階段** AI 整合 | **Deferred / Later** | ONNX / CLIP、語意搜尋、即時轉碼 — **現階段不要開工** |

---

## 專案現況 (Status)

- **第一階段 MVP 已完成**（見 README / SPECIFICATION）：Tauri v2 + React/TS、SQLite Schema、手動掃描入庫、FFmpeg 縮圖 / Sprite Sheet、AV_Data_Capture + StashDB 刮削命令、手動整理與設定 UI。
- **進行中（MVP wrap-up）**：實作 PR 已堆疊、多份 DAM APPROVE，**尚未合入 main**（見下方 Active PRs）。
- **下一步（第二階段）**：Watcher、模板搬運、任務佇列與完整狀態機，再談 UX polish。
- **明確延後（第三階段）**：ONNX / CLIP、語意搜尋、即時轉碼。

### Active PRs（堆疊，待威廉 merge）

合併順序：**#15 → #16 → #17 → #18** → main。皆為 draft／stacked；**勿跳過 base 單獨合入**。

| PR | 對應 | DAM 審核（截至文件更新時） | 狀態 |
|----|------|---------------------------|------|
| [#15](https://github.com/william563214/intelligent-dam/pull/15) | Issue #1 Settings 持久化 | **APPROVE**（複審通過） | 未 merge；實作在分支，非 main |
| [#16](https://github.com/william563214/intelligent-dam/pull/16) | Issue #2 Scrape E2E | **APPROVE**（複審通過） | base = #15；未 merge |
| [#17](https://github.com/william563214/intelligent-dam/pull/17) | Issue #3 phash on scan | **APPROVE**（複審通過） | base = #16；未 merge |
| [#18](https://github.com/william563214/intelligent-dam/pull/18) | Issue #4 Hover Sprite / VTT | 仍有審核意見／未標 APPROVE | base = #17；未 merge |

`main` 仍反映 MVP 缺口，直到上述 PR 依序合入。

---

## 與 SPECIFICATION 的已知缺口 (Known Gaps)

以下描述以 **`main` 現況** 為準；括號註記堆疊 PR 中的進展（**尚未 merge**）。

| 缺口 | `main` 現況 | SPEC / 預期 | 追蹤 |
|------|-------------|-------------|------|
| Settings 持久化 | 設定只在前端 `localStorage`；後端無 JSON / store | SPEC §8.3：後端持久化掃描目錄、AVDC、StashDB URL、輸出模板 | **進行中** — [#15](https://github.com/william563214/intelligent-dam/pull/15)（DAM APPROVE，**待 merge**；完成後後端 `settings.json` 為真相來源） |
| Scraper 設定未完整到達後端 | 刮削依前端傳參；後端不持有設定 | 後端讀持久化設定；AV_Data_Capture → StashDB fallback | 隨 #15 解決 fallback；E2E 見 #16 |
| Tags / Actors 入庫不完整 | `to_database_metadata` 未寫 Tags／Actors | 刮削成功寫入 Metadata + Tags / Actors | **進行中** — [#16](https://github.com/william563214/intelligent-dam/pull/16)（APPROVE，待 merge） |
| `phash` | import 使用 size + mtime stub，非感知雜湊 | 掃描產出可供 StashDB / 去重的 PHash | **進行中** — [#17](https://github.com/william563214/intelligent-dam/pull/17)（APPROVE，待 merge） |
| Sprite / VTT ↔ hover preview | 後端可生成 Sprite／VTT；`VideoCard` 僅封面／「預覽中…」占位，**未接 VTT scrub** | SPEC §3.2：懸停依時間點顯示 Sprite | **進行中** — [#18](https://github.com/william563214/intelligent-dam/pull/18)（堆疊中，待 merge） |
| `notify` 無 Watcher | `Cargo.toml` 有 `notify`，無 Watcher 模組 | SPEC §3.1 / §5：監控 Created / Moved | 第二階段 Automation |
| Output path template | Settings UI 有 Handlebars 欄位；後端無 rename／move | 依模板搬運並更新 `Files.path` | 第二階段 Automation |
| 檔案狀態機 | 有 `status` 與 `update_file_status`；未系統性使用 `processing` / `scrape_failed` / `missing` | SPEC §7.2 定義 `scrape_failed`；需與 schema 狀態對齊並涵蓋 processing／missing | Issue #5；#16 已涵蓋部分 `scrape_failed`（待 merge） |

---

## 分階段 Checklist（建議順序）

### MVP wrap-up（對應 Issues #1–#5；屬第一階段收尾）

補齊 MVP 與 SPEC 的落差，再進入第二階段自動化。

- [ ] **1. Persist settings on Tauri side** — 後端可讀寫掃描目錄、AVDC、StashDB URL、輸出模板 → [#15](https://github.com/william563214/intelligent-dam/pull/15)（**進行中／DAM APPROVE，待威廉 merge**；勿在 `main` 上勾成已完成）
- [ ] **2. End-to-end scraping** — tags／actors 入 DB；AV_Data_Capture → StashDB；失敗可標狀態 → [#16](https://github.com/william563214/intelligent-dam/pull/16)（APPROVE，待 merge）
- [ ] **3. Compute phash during scan** — 入庫產出可用感知雜湊 → [#17](https://github.com/william563214/intelligent-dam/pull/17)（APPROVE，待 merge）
- [ ] **4. Wire hover preview to Sprite / VTT** — `VideoCard` 依 VTT 做懸停 scrubbing → [#18](https://github.com/william563214/intelligent-dam/pull/18)（堆疊中，待 merge）
- [ ] **5. File status machine** — 維護 `processing`／`scrape_failed`（SPEC §7.2）／`missing`／`ready`（部分隨 #16；完整狀態機仍開放）

### Automation core（第二階段）

- [ ] **6. notify Watcher** — 監聽設定中的攝取目錄（Created / Moved）
- [ ] **7. Wait until file write completes** — 寫入完成（file lock／穩定度檢測）後才處理
- [ ] **8. Auto pipeline** — ingest → scrape → template move → preview
- [ ] **9. Handlebars path template rename / move** — 依輸出模板搬運並更新 DB path
- [ ] **10. Task queue + progress events** — 背景佇列與進度回報給前端
- [ ] **11. Dedup by phash / path** — 依 phash 與 path 避免重複入庫或重複處理

### UX polish

- [x] **一鍵整理** — 影片庫「一鍵整理」對可整理檔 enqueue 既有 scrape→move→preview 佇列（`organize_eligible_files_command`）
- [x] **批次操作** — 影片庫多選 + 批量刮削／預覽／整理（`enqueue_pipeline_jobs_command`）
- [x] **錯誤日誌** — SQLite `ErrorLog` +「錯誤日誌」分頁；pipeline／手動刮削／FFmpeg 失敗入庫
- [x] **Settings：資料夾選擇器 + 連線／可用性測試** — `tauri-plugin-dialog` 選掃描／AVDC；FFmpeg／AVDC／StashDB 測試（`test_connectivity_command`）

追蹤：Issue #12。

### Deferred / Later（第三階段 — 明確延後，現階段不要開工）

- [ ] ONNX Runtime + CLIP
- [ ] 語意搜尋／向量（sqlite-vss 等）
- [ ] 即時轉碼（例如 H.265 → H.264 串流）

---

## 相關文件

- [SPECIFICATION.md](./SPECIFICATION.md) — 架構、Schema、自動化流程與刮削規格
- [README.md](./README.md) — 安裝、開發、MVP 已知限制

---

## 協作備註

- 實作請開 PR；本 roadmap 僅文件與優先序。
- **合併請威廉明確指示**；堆疊 PR 請依 #15 → #16 → #17 → #18 順序。
- 細項追蹤以 GitHub Issues 為準；PR 合入後請同步勾選本檔 checklist 並改寫 Known Gaps。
