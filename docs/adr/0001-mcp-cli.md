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
3. **權限分級**：
   - 唯讀：預設開放。
   - 寫入／搬檔：先回傳預覽（diff），使用者確認後才套用，且可還原——沿用 v0.2 整批預覽與還原（#73，schema v8 批次 log、#69 `pending_move_to`）。
   - 刪除：另需明確確認（獨立 confirm token，不能與一般寫入共用）。

## 並行方案（**待威廉決定**）
App 與 CLI／MCP 可能同時存取同一個 SQLite。

| 面向 | (A) WAL + `busy_timeout` 重試 | (B) 透過 app 內本機 socket 轉送 |
|---|---|---|
| 做法 | CLI／MCP 直接開 DB，WAL 模式，寫入撞鎖時 `busy_timeout`＋有限重試 | CLI／MCP 連 app 的本機 socket（Unix domain socket／Windows named pipe），由 app 單一寫入者執行 |
| app 未開時 | 照常可用 | 無法寫入；需退回 (A) 唯讀模式或自動啟動 app（headless） |
| 鎖 | SQLite 檔案鎖；長交易（搬檔）可能讓對方等待或逾時 | 單一行程內排隊，無跨行程寫鎖競爭 |
| 一致性 | 兩邊各自持有記憶體狀態（佇列、watcher），需靠 DB 重讀同步；GUI 可能顯示過期資料 | 只有一份狀態；GUI 可即時收到事件 |
| 複雜度 | 低，改動少 | 中高：IPC 協定、驗證、跨平台 socket |
| 安全 | 只依賴檔案權限 | 需確保 socket 僅本機、限目前使用者 |

**建議**：先採 (A) 讓唯讀工具與小型寫入可用；搬檔／整批套用這類長操作在 app 開著時優先走 (B)，未開時用 (A) 並持有 #73 的批次 log。**此建議尚待威廉決定。**

## 工具清單（初版）

| 工具 | 等級 |
|---|---|
| 搜尋（`search`） | 唯讀 |
| 取得詳情（`get_details`） | 唯讀 |
| 加標籤（`add_tags`） | 寫入 |
| 整理預覽（`organize_preview`） | 唯讀 |
| 整理套用（`organize_apply`） | 寫入·需確認·可還原 |
| 追更清單（`follow_list` 讀／`follow_add` 寫，#78） | 唯讀／寫入 |
| 想要清單（`wishlist_list` 讀／`wishlist_add` 寫，#78） | 唯讀／寫入 |
| 觸發掃描／刮削（`scan`／`scrape`） | 寫入 |
| 刪除（未列入初版） | 刪除·另需明確確認 |

## 後果
- 需先重構出 `idam-core`，會動到大量 command 檔；應分多個 PR。
- MCP 不開網路埠，遠端使用不在範圍內（符合「明確不做：手機與 Web 存取」）。
