# Intelligent DAM — Roadmap

本文件整理 **第一階段 MVP** 完成後的缺口與建議實作順序，並反映 **`main` @ `f40cd0b`** 與 **v0.1.0 已發布** 現況。規格細節見 [SPECIFICATION.md](./SPECIFICATION.md)；安裝與使用見 [README.md](./README.md)。

> 實作變更請透過 Pull Request 送審，**請勿直接 merge 到 main**（合併請威廉明確指示）。細項以 GitHub Issues 為準。

---

## 階段編號對照 (Phase numbering)

SPECIFICATION / README 使用「第 N 階段」；本檔的 wrap-up / automation checklist 為 MVP 之後的工作包。對照如下：

| SPEC / README | 本檔 | 說明 |
|---------------|------|------|
| **第一階段** MVP 基礎建設 ✅ | （已完成）+ 下方 **MVP wrap-up**（items #1–#5）✅ | Foundation 已落地；wrap-up 已合入 main |
| **第二階段** 自動化與刮削 ✅ | **Automation core**（items #6–#11）✅ | Watcher、穩定度檢測、pipeline、Handlebars、佇列、去重 |
| （README／SPEC 未單列） | **UX polish**（#12）✅ | 一鍵整理、批次、錯誤日誌、Settings 選擇器／連線測試 |
| （發行） | **v0.1.0** ✅ | tag `v0.1.0` @ `6d19ef9`（[#29](https://github.com/william563214/intelligent-dam/pull/29) merge commit）；Linux + Windows 安裝包已上架（見下方） |
| （README／SPEC 未單列） | **v0.2**（Issues #31–#40） | CI、DB／佇列硬化、整合測試、Watcher／Windows／phash 強化、macOS workflow、Handlebars／碰撞 UI |
| **第三階段** AI 整合 | **Deferred / Later**（#13） | ONNX / CLIP、語意搜尋、即時轉碼 — **現階段不要開工** |

---

## 專案現況 (Status)

基準：**`main` @ `f40cd0b`**（含 PR [#30](https://github.com/william563214/intelligent-dam/pull/30) Windows release workflow；以及 PR [#29](https://github.com/william563214/intelligent-dam/pull/29) v0.1.0 release prep）。

- **第一階段 MVP + wrap-up：已完成**（Issues #1–#5 對應 PR #15–#19）。
- **第二階段 Automation core：已完成**（Issues #6–#11 對應 PR #21–#25）。
- **UX polish（#12）：已完成**（PR [#26](https://github.com/william563214/intelligent-dam/pull/26)，`a2c96e6`）。
- **Tag uniqueness（#20）：已合入** — `(name, category)` 唯一約束 + 原子、可恢復遷移（`c433334`）。
- **DB hardening（#28）：已合入** — 正式連線 `PRAGMA foreign_keys = ON`、啟動 orphan 清理、Tags 遷移硬化、DB init 錯誤對話框（`21245f7`）。
- **v0.1.0：已發布** — tag [`v0.1.0`](https://github.com/william563214/intelligent-dam/releases/tag/v0.1.0) 指向 `6d19ef9`（PR [#29](https://github.com/william563214/intelligent-dam/pull/29) 的 merge commit）。
  - **Linux**：`.deb`／`.rpm`／`.AppImage`（校驗：`SHA256SUMS`）
  - **Windows**：MSI／NSIS（經 PR [#30](https://github.com/william563214/intelligent-dam/pull/30) `release-windows.yml` 上傳至既有 Release；**未簽章**，安裝時可能觸發 SmartScreen；校驗：`SHA256SUMS-windows.txt`）
  - **macOS**：`release-macos.yml`（Issue [#39](https://github.com/william563214/intelligent-dam/issues/39)）**workflow 已就緒，待首次發佈**（universal `.dmg` + `SHA256SUMS-macos.txt`）。**未簽章／未公證**；若系統擋下 App，請至 **系統設定 → 隱私權與安全性 → 強制打開**，或執行 `xattr -dr com.apple.quarantine "/Applications/Intelligent DAM.app"`（macOS 15+ 右鍵「打開」已無效）
- **明確延後（第三階段 / #13）**：ONNX / CLIP、語意搜尋、即時轉碼。

---

## 與 SPECIFICATION 的已知缺口 (Known Gaps)

以下僅列已於 `src-tauri/src` / `src/`（及 repo 根目錄）**實際確認**的剩餘缺口；已合入功能不再列為缺口。v0.2 追蹤 Issue 見備註欄。

| 缺口 | `main` @ `f40cd0b` 現況 | 備註 |
|------|-------------------------|------|
| Task queue 非持久化 | `task_queue.rs` 為進程內 `VecDeque` / `HashMap`；註明 in-memory、重啟遺失、非 crash-safe | Issue [#34](https://github.com/william563214/intelligent-dam/issues/34)；Issue #10 已交付佇列與進度事件 |
| Near-phash 閾值 ≤8 可能誤判 | `phash.rs` 預設 `near_phash_max_distance = 8`（可用 `IDAM_NEAR_PHASH_MAX_DISTANCE` 覆寫）；文件註明視覺近似但不同片段可能 false-positive | Issue [#38](https://github.com/william563214/intelligent-dam/issues/38)；行為符合 Stash 風格；非阻塞 |
| 無 CI workflow | 僅有 `release-windows.yml`；尚無 `cargo test`／`clippy`／`vitest` CI | Issue [#31](https://github.com/william563214/intelligent-dam/issues/31) |
| 無 Tauri invoke／整合測試 | 後端多為 `#[cfg(test)]` 單元測試；前端僅 `src/utils/*.test.ts`；無 invoke／tauri-driver E2E | Issue [#35](https://github.com/william563214/intelligent-dam/issues/35)（tauri-driver E2E 延後，見該 Issue） |
| Watcher 無 E2E | `watcher.rs` 測試僅涵蓋路徑／事件分類／debounce 等 helper，無 notify→穩定度→ingest→queue 端到端 | Issue [#36](https://github.com/william563214/intelligent-dam/issues/36) |
| Windows 鎖偵測偏重 size stability | `file_stability.rs`：Windows 上多數複製工具不鎖目的檔，**size-stable 為主要可靠訊號**；`try_lock` 回傳 `WouldBlock` 時繼續等待，**僅其他錯誤**才退回 size-stable | Issue [#37](https://github.com/william563214/intelligent-dam/issues/37)；Issue #8 已交付穩定度閘門 |
| ~~進階 Handlebars helpers／碰撞 UI 缺席~~ | ~~已由 Issue #40 交付~~：helpers（`format_date`／大小寫／`truncate`／`default`／`sanitize`）、Settings 模板預覽、碰撞時 UI 選加後綴／跳過／覆蓋 | Issue [#40](https://github.com/william563214/intelligent-dam/issues/40) |
| 無 `PRAGMA user_version` schema 版本 | schema 以 `CREATE TABLE IF NOT EXISTS` + 特製遷移為主；未使用 `user_version` 做版本追蹤 | Issue [#32](https://github.com/william563214/intelligent-dam/issues/32) |
| `get_or_create_tag` 非 `ON CONFLICT` | `database.rs` 為 select-then-insert（`SELECT` 後無列再 `INSERT`），非 `INSERT … ON CONFLICT` | Issue [#33](https://github.com/william563214/intelligent-dam/issues/33)；併發下理論上可能撞 UNIQUE |

> 已由 PR [#28](https://github.com/william563214/intelligent-dam/pull/28) 修復、不再列為缺口：正式連線 `PRAGMA foreign_keys = ON`＋啟動 orphan 清理；#20 nits（碰撞斷言、`BEGIN` 失敗仍還原 FK、`Tags_mig` 備份寫入 ErrorLog）。
>
> Issue [#39](https://github.com/william563214/intelligent-dam/issues/39)：`release-macos.yml` **workflow 已就緒，待首次發佈**（universal `.dmg` + `SHA256SUMS-macos.txt`）。未簽章時若被擋：**系統設定 → 隱私權與安全性 → 強制打開**，或 `xattr -dr com.apple.quarantine "/Applications/Intelligent DAM.app"`。

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

### v0.1.0 — RELEASED

- [x] **Release prep** — PR [#29](https://github.com/william563214/intelligent-dam/pull/29) merge commit `6d19ef9`；tag [`v0.1.0`](https://github.com/william563214/intelligent-dam/releases/tag/v0.1.0)
- [x] **Linux 安裝包** — `.deb`／`.rpm`／`.AppImage`（校驗：`SHA256SUMS`）
- [x] **Windows 安裝包** — MSI／NSIS；PR [#30](https://github.com/william563214/intelligent-dam/pull/30)（`f40cd0b`，`release-windows.yml`）；**未簽章**／可能觸發 SmartScreen（校驗：`SHA256SUMS-windows.txt`）
- [ ] **macOS 安裝包** — `release-macos.yml`（Issue [#39](https://github.com/william563214/intelligent-dam/issues/39)）**workflow 已就緒，待首次發佈**；universal `.dmg`／`SHA256SUMS-macos.txt`；未簽章時：**系統設定 → 隱私權與安全性 → 強制打開**，或 `xattr -dr com.apple.quarantine "/Applications/Intelligent DAM.app"`

### v0.2（Issues #31–#40）

建議 PR 順序（可依風險／依賴微調；**#39 可插隊**）：

> [#42](https://github.com/william563214/intelligent-dam/pull/42) → [#31](https://github.com/william563214/intelligent-dam/issues/31) → [#33](https://github.com/william563214/intelligent-dam/issues/33) → [#32](https://github.com/william563214/intelligent-dam/issues/32) → [#35](https://github.com/william563214/intelligent-dam/issues/35) → [#34](https://github.com/william563214/intelligent-dam/issues/34) → [#36](https://github.com/william563214/intelligent-dam/issues/36) → [#37](https://github.com/william563214/intelligent-dam/issues/37) → [#38](https://github.com/william563214/intelligent-dam/issues/38) → [#39](https://github.com/william563214/intelligent-dam/issues/39) → [#40](https://github.com/william563214/intelligent-dam/issues/40)

#### 必做

- [ ] **31. CI** — Issue [#31](https://github.com/william563214/intelligent-dam/issues/31)：`cargo test`／`clippy` + `vitest`（Linux／Windows，`--locked`）
- [ ] **33. ON CONFLICT** — Issue [#33](https://github.com/william563214/intelligent-dam/issues/33)：`get_or_create_tag` 改用 `INSERT … ON CONFLICT`
- [ ] **32. user_version** — Issue [#32](https://github.com/william563214/intelligent-dam/issues/32)：導入 `PRAGMA user_version` 遷移框架
- [ ] **35. invoke 整合測試** — Issue [#35](https://github.com/william563214/intelligent-dam/issues/35)：臨時 DB 的 Tauri invoke 整合測試（**tauri-driver E2E 延後**，見該 Issue）
- [ ] **34. 持久化 task queue** — Issue [#34](https://github.com/william563214/intelligent-dam/issues/34)：佇列持久化與重啟恢復（需 #32）

#### 建議

- [ ] **36. Watcher E2E** — Issue [#36](https://github.com/william563214/intelligent-dam/issues/36)：notify→穩定度→ingest→queue（需 #35）
- [ ] **37. Windows 寫入完成偵測** — Issue [#37](https://github.com/william563214/intelligent-dam/issues/37)（需 #36）
- [ ] **38. Near-phash 誤判** — Issue [#38](https://github.com/william563214/intelligent-dam/issues/38)：閾值調整＋疑似重複確認
- [ ] **39. macOS workflow** — Issue [#39](https://github.com/william563214/intelligent-dam/issues/39)：`release-macos.yml` **workflow 已就緒，待首次發佈**（未簽章；擋下時用系統設定「強制打開」或 `xattr -dr com.apple.quarantine`）

#### 可延後

- [x] **40. Handlebars helpers + 碰撞 UI** — Issue [#40](https://github.com/william563214/intelligent-dam/issues/40)（需 #35）

#### #34 / #40 後續

- [x] **50. 搬檔中途當機後補寫 DB 路徑** — Issue [#50](https://github.com/william563214/intelligent-dam/issues/50)：來源不在、目的地在且可確認同一檔時 heal `Files.path`（schema 維持 v6；與 #59 的 `.idam-bak`／`.idam-tmp-*` 殘檔互不混淆）
- [x] **59. #58 follow-ups** — Issue [#59](https://github.com/william563214/intelligent-dam/issues/59)：grapheme truncate、段長 255 bytes、skip 不重進碰撞、suffix 查 DB、去掉 API 冗餘 `existing_path`、occupant 覆寫 ErrorLog、startup 掃描 sidecars、hardlink occupant、N4 覆寫 crash 重啟完成 commit（NOCASE／NFD／尾端分隔／Volume 前綴見 [#63](https://github.com/william563214/intelligent-dam/issues/63)）


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
