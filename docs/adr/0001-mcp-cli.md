# ADR 0001：MCP 與 CLI 操控（讓 AI 能接管媒體庫）

- 狀態：**Proposed**
- 日期：2026-10-23
- 追蹤：Issue [#87](https://github.com/william563214/intelligent-dam/issues/87)
- 授權限制：參考 peach-mitao/peach（AGPL-3.0）——**只參考概念，不得複製其程式碼，以 Rust 自行重寫**。

## 背景
目前所有操作都只能透過 Tauri GUI 的 commands 觸發。希望讓 AI 助手（MCP client）與腳本（CLI）也能查詢與操控媒體庫，同時不犧牲安全與資料一致性。

## 決策
1. **共用核心**：把現有 Tauri commands 的業務邏輯抽成 `idam-core` crate（DB、掃描、刮削、整理、標籤）。Tauri app、`idam` CLI、MCP server 三者都只是 `idam-core` 的薄包裝。
2. **MCP 傳輸**：只走 **stdio**、僅本機；**不開任何網路埠**。
3. **權限分級**（每個寫入工具必須歸入下列其中一層，不可未分類）：
   - **唯讀**：預設開放；不改 DB、不動檔案。
   - **低風險寫入**：僅改 DB metadata（標籤、追更／想要清單等），**不搬檔、不刪檔、不覆寫刮削欄位**；變更應可經 #73 schema v8 批次 log 還原。此層不需預覽→確認流程，但仍記錄批次 log。
   - **高風險寫入**：會搬檔／觸碰檔案系統，或覆寫既有 metadata（例如 `organize_apply`、會寫入檔案或覆寫欄位的 `scan`／`scrape`）。必須走 **預覽 → 確認 → 可還原**（沿用 #73 批次 log、#69 `pending_move_to`）。
   - **刪除**：未列入初版工具；若日後加入，另需獨立 confirm token（不可與一般寫入共用）。
4. **確認必須由人類完成**：高風險寫入與刪除的確認，只能經 app GUI 確認、MCP elicitation，或 CLI 互動式提示。**AI client 不得自行持有或代簽 confirm token**（刪除 token 尤其禁止）。

## 並行方案（**建議採用：預設 WAL+busy_timeout，長操作走 app 本機 socket（待威廉確認）**）
App 與 CLI／MCP 可能同時存取同一個 SQLite。

| 面向 | (A) WAL + `busy_timeout` 重試 | (B) 透過 app 內本機 socket 轉送 |
|---|---|---|
| 做法 | CLI／MCP 直接開 DB，WAL 模式，寫入撞鎖時 `busy_timeout`＋有限重試 | CLI／MCP 連 app 的本機 socket（Unix domain socket／Windows named pipe），由 app 單一寫入者執行 |
| app 未開時 | 照常可用 | 無法寫入；需退回 (A) 唯讀模式或自動啟動 app（headless） |
| 鎖 | SQLite 檔案鎖；長交易（搬檔）可能讓對方等待或逾時 | 單一行程內排隊，無跨行程寫鎖競爭 |
| 一致性 | 兩邊各自持有記憶體狀態（佇列、watcher），需靠 DB 重讀同步；GUI 可能顯示過期資料 | 只有一份狀態；GUI 可即時收到事件 |
| 複雜度 | 低，改動少 | 中高：IPC 協定、驗證、跨平台 socket |
| 安全 | 只依賴檔案權限 | 需確保 socket 僅本機、限目前使用者 |

**建議採用**：預設 (A) WAL + `busy_timeout`，讓唯讀與低風險寫入可用；搬檔／整批套用等高風險長操作在 app 開著時走 (B) 本機 socket，未開時用 (A) 並持有 #73 的批次 log。**待威廉確認**（非已決定）。

### (A) 下 app 如何察覺 CLI／MCP 寫入並刷新
在採 (A) 時，GUI 行程需偵測他方對同一 DB 的寫入並重讀狀態，避免顯示過期資料。建議以週期 `PRAGMA data_version`（或等效的 schema／user_version 輪詢）比對上次快取值；若版本前進則使相關查詢／列表失效並從 DB 重載。細部間隔與失效範圍實作時再定，本 ADR 只要求有明確刷新機制。

## 工具清單（初版）

| 工具 | 等級 |
|---|---|
| 搜尋（`search`） | 唯讀 |
| 取得詳情（`get_details`） | 唯讀 |
| 整理預覽（`organize_preview`） | 唯讀 |
| 追更清單讀（`follow_list`，#78） | 唯讀 |
| 想要清單讀（`wishlist_list`，#78） | 唯讀 |
| 加標籤（`add_tags`） | 低風險寫入 |
| 追更清單加（`follow_add`，#78） | 低風險寫入 |
| 想要清單加（`wishlist_add`，#78） | 低風險寫入 |
| 整理套用（`organize_apply`） | 高風險寫入（預覽→人類確認→可還原） |
| 觸發掃描（`scan`） | 高風險寫入（預覽→人類確認→可還原；可能觸碰檔案／寫入 DB） |
| 觸發刮削（`scrape`） | 高風險寫入（預覽→人類確認→可還原；可能覆寫 metadata） |
| 刪除（未列入初版） | 刪除·另需人類確認與獨立 token（禁止 AI 自簽） |

## 後果
- 需先重構出 `idam-core`，會動到大量 command 檔；應分多個 PR。
- MCP 不開網路埠，遠端使用不在範圍內（符合「明確不做：手機與 Web 存取」）。
