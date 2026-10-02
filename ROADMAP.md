# Intelligent DAM — Roadmap

本文件整理 **第一階段 MVP** 完成後的缺口與建議實作順序，並反映 **`main` @ `c433334`** 現況。規格細節見 [SPECIFICATION.md](./SPECIFICATION.md)；安裝與使用見 [README.md](./README.md)。

> 實作變更請透過 Pull Request 送審，**請勿直接 merge 到 main**（合併請威廉明確指示）。細項以 GitHub Issues 為準。

---

## 階段編號對照 (Phase numbering)

SPECIFICATION / README 使用「第 N 階段」；本檔的 wrap-up / automation checklist 為 MVP 之後的工作包。對照如下：

| SPEC / README | 本檔 | 說明 |
|---------------|------|------|
| **第一階段** MVP 基礎建設 ✅ | （已完成）+ 下方 **MVP wrap-up**（items #1–#5）✅ | Foundation 已落地；wrap-up 已合入 main |
| **第二階段** 自動化與刮削 ✅ | **Automation core**（items #6–#11）✅ | Watcher、穩定度檢測、pipeline、Handlebars、佇列、去重 |
| （README／SPEC 未單列） | **UX polish**（#12）✅ | 一鍵整理、批次、錯誤日誌、Settings 選擇器／連線測試 |
| **第三階段** AI 整合 | **Deferred / Later**（#13） | ONNX / CLIP、語意搜尋、即時轉碼 — **現階段不要開工** |

---

## 專案現況 (Status)

基準：**`main` @ `c433334`**（含 PR [#20](https://github.com/william563214/intelligent-dam/pull/20) tag uniqueness）。

- **第一階段 MVP + wrap-up：已完成**（Issues #1–#5 對應 PR #15–#19）。
- **第二階段 Automation core：已完成**（Issues #6–#11 對應 PR #21–#25）。
- **UX polish（#12）：已完成**（PR [#26](https://github.com/william563214/intelligent-dam/pull/26)，`a2c96e6`）。
- **Tag uniqueness（#20）：已合入** — `(name, category)` 唯一約束 + 原子、可恢復遷移（`c433334`）。
- **明確延後（第三階段 / #13）**：ONNX / CLIP、語意搜尋、即時轉碼。

---

## 與 SPECIFICATION 的已知缺口 (Known Gaps)

以下僅列已於 `src-tauri/src` / `src/`（及 repo 根目錄）**實際確認**的剩餘缺口；已合入功能不再列為缺口。

| 缺口 | `main` @ `c433334` 現況 | 備註 |
|------|-------------------------|------|
| Task queue 非持久化 | `task_queue.rs` 為進程內 `VecDeque` / `HashMap`；註明 in-memory、重啟遺失、非 crash-safe | Issue #10 已交付佇列與進度事件；持久化／重啟恢復仍缺 |
| Near-phash 閾值 ≤8 可能誤判 | `phash.rs` 預設 `near_phash_max_distance = 8`（可用 `IDAM_NEAR_PHASH_MAX_DISTANCE` 覆寫）；文件註明視覺近似但不同片段可能 false-positive | 行為符合 Stash 風格；非阻塞 |
| 無 CI workflow | 無 `.github/workflows`；僅有本機 `cargo test` / `vitest` | — |
| 無 Tauri invoke／整合測試 | 後端多為 `#[cfg(test)]` 單元測試；前端僅 `src/utils/*.test.ts`；無 invoke／tauri-driver E2E | — |
| Watcher 無 E2E | `watcher.rs` 測試僅涵蓋路徑／事件分類／debounce 等 helper，無 notify→穩定度→ingest→queue 端到端 | — |
| Windows 鎖偵測偏重 size stability | `file_stability.rs`：Windows 上多數複製工具不鎖目的檔，**size-stable 為主要可靠訊號**；`try_lock` 回傳 `WouldBlock` 時繼續等待，**僅其他錯誤**才退回 size-stable | Issue #8 已交付穩定度閘門 |
| 進階 Handlebars helpers／碰撞 UI 缺席 | `path_template.rs` 僅註冊變數渲染、無 `register_helper`；衝突時後端 `unique_destination` 自動加後綴，前端無碰撞／衝突 UI | Issue #9 已交付基本模板搬運 |
| 無 `PRAGMA user_version` schema 版本 | schema 以 `CREATE TABLE IF NOT EXISTS` + 特製遷移為主；未使用 `user_version` 做版本追蹤 | — |
| `get_or_create_tag` 非 `ON CONFLICT` | `database.rs` 為 select-then-insert（`SELECT` 後無列再 `INSERT`），非 `INSERT … ON CONFLICT` | 併發下理論上可能撞 UNIQUE |

> 已由此 PR（[#28](https://github.com/william563214/intelligent-dam/pull/28)）修復、不再列為缺口：正式連線 `PRAGMA foreign_keys = ON`＋啟動 orphan 清理；#20 nits（碰撞斷言、`BEGIN` 失敗仍還原 FK、`Tags_mig` 備份寫入 ErrorLog）。

---

## 分階段 Checklist

### MVP wrap-up（Issues #1–#5）— DONE

補齊 MVP 與 SPEC 的落差；已全部合入 `main`。

- [x] **1. Persist settings on Tauri side** — Issue [#1](https://github.com/william563214/intelligent-dam/issues/1) → PR [#15](https://github.com/william563214/intelligent-dam/pull/15)（`f7496aa`）
- [x] **2. End-to-end scraping** — Issue [#2](https://github.com/william563214/intelligent-dam/issues/2) → PR [#16](https://github.com/william563214/intelligent-dam/pull/16)（`9524fba`）；含 `scrape_failed`
- [x] **3. Compute phash during scan** — Issue [#3](https://github.com/william563214/intelligent-dam/issues/3) → PR [#17](https://github.com/william563214/intelligent-dam/pull/17)（`75a885e`）
- [x] **4. Wire hover preview to Sprite / VTT** — Issue [#4](https://github.com/william563214/intelligent-dam/issues/4) → PR [#18](https://github.com/william563214/intelligent-dam/pull/18)（`d362440`）
- [x] **5. File status machine** — Issue [#5](https://github.com/william563214/intelligent-dam/issues/5) → PR [#19](https://github.com/william563214/intelligent-dam/pull/19)（`f34f66c`）；`scrape_failed` 來自 #16

### Automation core（Issues #6–#11）— DONE

Checklist **項目編號 = Issue 編號**（修正舊稿 item 7/8 與 Issue #7/#8 對調）。

- [x] **6. notify Watcher** — Issue [#6](https://github.com/william563214/intelligent-dam/issues/6) → PR [#21](https://github.com/william563214/intelligent-dam/pull/21)（`ca2600d`）
- [x] **7. Auto pipeline** — Issue [#7](https://github.com/william563214/intelligent-dam/issues/7) → PR [#23](https://github.com/william563214/intelligent-dam/pull/23)（`c23f4d8`）；經 #24 佇列排程
- [x] **8. Wait until file write completes** — Issue [#8](https://github.com/william563214/intelligent-dam/issues/8) → PR [#22](https://github.com/william563214/intelligent-dam/pull/22)（`a48b1d7`）
- [x] **9. Handlebars path template rename / move** — Issue [#9](https://github.com/william563214/intelligent-dam/issues/9) → 實作於 PR [#23](https://github.com/william563214/intelligent-dam/pull/23)（`c23f4d8`）
- [x] **10. Task queue + progress events** — Issue [#10](https://github.com/william563214/intelligent-dam/issues/10) → PR [#24](https://github.com/william563214/intelligent-dam/pull/24)（`cff2456`）
- [x] **11. Dedup by phash / path** — Issue [#11](https://github.com/william563214/intelligent-dam/issues/11) → PR [#25](https://github.com/william563214/intelligent-dam/pull/25)（`bda2eaf`）

### UX polish（Issue #12）— DONE

- [x] **一鍵整理** — 影片庫「一鍵整理」對可整理檔 enqueue 既有 scrape→move→preview 佇列（`organize_eligible_files_command`）
- [x] **批次操作** — 影片庫多選 + 批量刮削／預覽／整理（`enqueue_pipeline_jobs_command`）
- [x] **錯誤日誌** — SQLite `ErrorLog` +「錯誤日誌」分頁；pipeline／手動刮削／FFmpeg 失敗入庫
- [x] **Settings：資料夾選擇器 + 連線／可用性測試** — `tauri-plugin-dialog` 選掃描／AVDC；FFmpeg／AVDC／StashDB 測試（`test_connectivity_command`）

追蹤：Issue [#12](https://github.com/william563214/intelligent-dam/issues/12) → PR [#26](https://github.com/william563214/intelligent-dam/pull/26)（`a2c96e6`）。

### Deferred / Later（第三階段 / Issue #13 — 明確延後，現階段不要開工）

- [ ] ONNX Runtime + CLIP
- [ ] 語意搜尋／向量（sqlite-vss 等）
- [ ] 即時轉碼（例如 H.265 → H.264 串流）

追蹤：Issue [#13](https://github.com/william563214/intelligent-dam/issues/13)。

---

## 相關文件

- [SPECIFICATION.md](./SPECIFICATION.md) — 架構、Schema、自動化流程與刮削規格
- [README.md](./README.md) — 安裝、開發、MVP 已知限制

---

## 協作備註

- 實作請開 PR；本 roadmap 僅文件與優先序。
- **合併請威廉明確指示**。
- 細項追蹤以 GitHub Issues 為準；合入後請同步勾選本檔 checklist 並更新 Known Gaps。
