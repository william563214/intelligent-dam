use crate::file_status;
use crate::path_template::{normalize_db_path, FsFileId};
use crate::phash;
use rusqlite::{params, Connection, OptionalExtension, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub size: u64,
    pub created_at: String,
    pub phash: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub file_id: i64,
    pub title: Option<String>,
    pub studio: Option<String>,
    pub release_date: Option<String>,
    pub summary: Option<String>,
    pub cover_image: Option<String>,
    pub sprite_sheet_path: Option<String>,
    pub source_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuspectedDuplicateInfo {
    pub keeper_id: i64,
    pub keeper_path: String,
    pub distance: u32,
}

/// Pending template destination collision (#40)
/// `existing_path` was redundant with `desired_path` (#59) and is omitted from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathCollisionInfo {
    pub desired_path: String,
    pub occupant_file_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileWithMetadata {
    // 保持巢狀 shape 與前端 FileWithMetadata.file / metadata / tags 對齊
    // 勿使用 flatten 否則 invoke JSON 會少掉 file 物件
    pub file: FileRecord,
    pub metadata: Option<Metadata>,
    pub tags: Vec<Tag>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "suspectedDuplicate")]
    pub suspected_duplicate: Option<SuspectedDuplicateInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "pathCollision")]
    pub path_collision: Option<PathCollisionInfo>,
}

/// Pending near-phash suspicion row (SuspectedDuplicates table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspectedDuplicateRow {
    pub file_id: i64,
    pub keeper_id: i64,
    pub distance: u32,
    pub resolution: String,
}

/// Pending path-collision row (PathCollisions table / #40).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCollisionRow {
    pub file_id: i64,
    pub desired_path: String,
    pub existing_path: String,
    pub occupant_file_id: Option<i64>,
    pub resolution: String,
}

/// Files row with a non-null `pending_move_to` (#66).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMoveRow {
    pub file_id: i64,
    pub path: String,
    pub pending_move_to: String,
    /// Source volume id at pending write time (Unix st_dev / Win volume serial)
    pub pending_source_vol: Option<i64>,
    /// Source file index at pending write time (Unix st_ino / Win file index)
    pub pending_source_idx: Option<i64>,
    pub size: u64,
    pub phash: Option<String>,
    pub status: String,
}

impl PendingMoveRow {
    /// Recorded source filesystem identity when both halves were stored
    pub fn pending_source_fs_id(&self) -> Option<FsFileId> {
        match (self.pending_source_vol, self.pending_source_idx) {
            (Some(volume), Some(index)) => Some(FsFileId {
                volume: volume as u64,
                index: index as u64,
            }),
            _ => None,
        }
    }
}

/// Confirmed-duplicate tombstone (DuplicateTombstones table).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateTombstone {
    pub id: i64,
    pub path: String,
    pub size: Option<i64>,
    pub phash: Option<String>,
    pub keeper_id: Option<i64>,
    pub created_at: String,
}

/// Result of checking a path against DuplicateTombstones at import time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathTombstoneAction {
    /// Same path + same size — skip re-ingest.
    Block,
    /// Same path but different size — tombstone removed; proceed as new file.
    Cleared,
    /// No tombstone for this path.
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorLogEntry {
    pub id: i64,
    pub created_at: String,
    pub category: String,
    pub file_id: Option<i64>,
    pub path: Option<String>,
    pub message: String,
    pub detail: Option<String>,
}

/// Header row for a batch organize / undo log (#73)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrganizeBatch {
    pub id: i64,
    pub created_at: String,
    pub status: String,
    pub note: Option<String>,
}

/// One planned / executed move inside a batch (#73)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrganizeBatchItem {
    pub id: i64,
    pub batch_id: i64,
    pub file_id: i64,
    pub src_path: String,
    pub dst_path: String,
    pub size: u64,
    pub phash: Option<String>,
    pub status: String,
    pub collision: Option<String>,
    pub error: Option<String>,
}

/// Fields for inserting a batch item before / during execute
#[derive(Debug, Clone)]
pub struct NewOrganizeBatchItem<'a> {
    pub file_id: i64,
    pub src_path: &'a str,
    pub dst_path: &'a str,
    pub size: u64,
    pub phash: Option<&'a str>,
    pub status: &'a str,
    pub collision: Option<&'a str>,
}

/// Durable task-queue row (`Jobs` table / #34)
#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: i64,
    pub status: String,
    pub job_type: String,
    pub payload: String,
    pub attempts: u32,
    pub max_attempts: u32,
    pub error: Option<String>,
    pub retryable: bool,
    // Populated from DB; reserved for future UI / diagnostics
    #[allow(dead_code)]
    pub created_at: String,
    #[allow(dead_code)]
    pub updated_at: String,
}

/// Fields for inserting a new `Jobs` row
#[derive(Debug, Clone)]
pub struct NewJobRow<'a> {
    pub status: &'a str,
    pub job_type: &'a str,
    pub payload: &'a str,
    pub attempts: u32,
    pub max_attempts: u32,
    pub error: Option<&'a str>,
    pub retryable: bool,
}

pub struct Database {
    conn: Connection,
}

/// 目前程式支援的 schema user_version
/// 1 = v0.1.0 baseline（含 #20 Tags / #28 FK 硬化）
/// 2 = #45 NULL-category 唯一索引
/// 3 = #34 Jobs 佇列持久化
/// 4 = #38 SuspectedDuplicates（near-phash 疑似重複）
/// 5 = #38 DuplicateTombstones + Files.organized_at（durable organized）
/// 6 = #40 PathCollisions（模板目的地碰撞）
/// 7 = #66 Files.pending_move_to + pending_source_vol/idx（搬檔前意圖／來源 fs id）
/// 8 = #73 OrganizeBatches / OrganizeBatchItems（整批整理 log 與一鍵還原）
const SCHEMA_USER_VERSION: i32 = 8;

/// 對外查詢目前程式支援的 schema user_version（整合測試用）
#[cfg(any(test, feature = "test-support"))]
pub fn current_schema_user_version() -> i32 {
    SCHEMA_USER_VERSION
}

/// 既有 v0.1.0 DB（user_version 0）升版後的 baseline
const SCHEMA_BASELINE_VERSION: i32 = 1;

const TAGS_NULLSAFE_UNIQUE_INDEX: &str = "idx_tags_name_ifnull_category";

impl Database {
    pub fn new(db_path: &str) -> SqliteResult<Self> {
        let conn = Connection::open(db_path)?;
        let db = Database { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> SqliteResult<()> {
        let found = self.get_user_version()?;
        if found > SCHEMA_USER_VERSION {
            return Err(Self::db_version_too_new_error(found));
        }

        // baseline 表與 #20/#28 安全遷移（冪等 含 Tags_mig 救援）
        self.ensure_baseline_schema()?;

        // 舊 DB 或新建 DB 的 user_version 為 0 無損標成 baseline 再跑後續遷移
        let mut version = self.get_user_version()?;
        if version == 0 {
            self.set_user_version(SCHEMA_BASELINE_VERSION)?;
            version = SCHEMA_BASELINE_VERSION;
        }

        self.apply_pending_migrations(version)?;
        // Dev DBs that reached user_version=5 before organized_at landed still need repair.
        let version_after = self.get_user_version()?;
        if version_after >= 5 {
            self.ensure_v5_columns()?;
        }
        // Mid-flight 7996aae stamped user_version=7 with only pending_move_to;
        // later builds added pending_source_vol/idx — repair without re-running mig7.
        if version_after >= 7 {
            self.ensure_v7_columns()?;
        }
        // Mid-flight v8 may stamp user_version without both batch tables — repair.
        if version_after >= 8 {
            self.ensure_v8_tables()?;
        }
        // #61 / #63: idempotent repair of legacy path strings (no schema bump).
        // Trailing seps + Windows prefixes/separators; NFC rewrite only on macOS.
        self.repair_mixed_separator_paths()?;
        Ok(())
    }

    /// Normalize existing Files / PathCollisions / DuplicateTombstones path strings.
    ///
    /// `needs_path_separator_repair` confirms with `normalize_db_path` via plain reads
    /// before any write lock (important for large libraries). All three tables update
    /// in one `BEGIN IMMEDIATE`. If normalizing a Files or tombstone path would
    /// violate UNIQUE leave the row unchanged and write an ErrorLog entry (no
    /// auto-merge); skip if the same category+path is already logged.
    fn repair_mixed_separator_paths(&self) -> SqliteResult<()> {
        if !self.needs_path_separator_repair()? {
            return Ok(());
        }
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.repair_files_path_separators()?;
            self.repair_path_collisions_separators()?;
            self.repair_tombstone_path_separators()?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();
        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!("warn: ROLLBACK failed after path separator repair: {rb}");
                }
                Err(e)
            }
        }
    }

    /// True when any stored path differs from `normalize_db_path`.
    ///
    /// Cheap SQL lists *candidates* only; each hit is confirmed with
    /// `normalize_db_path(p) != p` as a plain read. Never returns true (and thus
    /// never leads to `BEGIN IMMEDIATE`) on an unconfirmed SQL hit. If no candidate
    /// confirms dirty, falls back to a full definitive scan (macOS NFC, subtle
    /// Windows drive-letter cases, etc.). Read-only — no write lock.
    fn needs_path_separator_repair(&self) -> SqliteResult<bool> {
        if self.any_confirmed_cheap_candidate()? {
            return Ok(true);
        }
        self.any_path_table_needs_normalize()
    }

    /// Confirm cheap-probe candidate rows with `normalize_db_path` (plain reads).
    fn any_confirmed_cheap_candidate(&self) -> SqliteResult<bool> {
        if self.path_rows_need_normalize(&self.cheap_files_candidate_sql())? {
            return Ok(true);
        }
        if self.table_exists("PathCollisions")?
            && self.path_rows_need_normalize(&self.cheap_path_collisions_candidate_sql())?
        {
            return Ok(true);
        }
        if self.table_exists("DuplicateTombstones")?
            && self.path_rows_need_normalize(&self.cheap_tombstones_candidate_sql())?
        {
            return Ok(true);
        }
        Ok(false)
    }

    /// SQL predicate for paths that *might* need repair (candidates only).
    ///
    /// Tightened (#63 S-1): exclude roots (`/`, `X:\`, volume root); trailing `\`
    /// only on Windows; never treat `\\?\Volume{…}` or `\\.\PhysicalDrive…` as
    /// dirty just for their prefix. Uses `char(92)` for `\`.
    fn cheap_path_candidate_predicate(column: &str) -> String {
        // Trailing `/` except lone root `/`
        #[cfg(not(windows))]
        {
            format!("(substr({column}, -1) = '/' AND {column} != '/')")
        }
        #[cfg(windows)]
        {
            let mut pred = format!("(substr({column}, -1) = '/' AND {column} != '/')");
            // `\\?\Volume` prefix (10 chars) — kept by normalize, not a dirty signal
            let volume_prefix = format!(
                "lower(substr({column}, 1, 10)) = lower(char(92) || char(92) || '?' || char(92) || 'Volume')"
            );
            // Volume root `\\?\Volume{…}\` — `}` followed only by trailing `\`
            let volume_root = format!(
                "({volume_prefix}
                  AND instr({column}, '}}') > 0
                  AND substr({column}, instr({column}, '}}')) = '}}' || char(92))"
            );
            // Drive root `X:\`
            let drive_root = format!(
                "(length({column}) = 3 AND substr({column}, 2, 1) = ':')"
            );
            // Trailing `\` except drive / volume roots
            pred.push_str(&format!(
                " OR (
                    substr({column}, -1) = char(92)
                    AND NOT {drive_root}
                    AND NOT {volume_root}
                )"
            ));
            // Mixed `/`; non-Volume `\\?\`; drive-form `\\.\X:`; lowercase drive
            pred.push_str(&format!(
                " OR instr({column}, '/') > 0
                  OR (
                        {column} LIKE char(92) || char(92) || '?%'
                        AND NOT ({volume_prefix})
                     )
                  OR (
                        {column} LIKE char(92) || char(92) || '.%'
                        AND length({column}) >= 6
                        AND substr({column}, 5, 1) GLOB '[A-Za-z]'
                        AND substr({column}, 6, 1) = ':'
                     )
                  OR (
                        length({column}) >= 2
                        AND substr({column}, 2, 1) = ':'
                        AND substr({column}, 1, 1) GLOB '[a-z]'
                     )"
            ));
            pred
        }
    }

    fn cheap_files_candidate_sql(&self) -> String {
        format!(
            "SELECT path FROM Files WHERE {}",
            Self::cheap_path_candidate_predicate("path")
        )
    }

    fn cheap_path_collisions_candidate_sql(&self) -> String {
        let d = Self::cheap_path_candidate_predicate("desired_path");
        let e = Self::cheap_path_candidate_predicate("existing_path");
        format!(
            "SELECT desired_path FROM PathCollisions WHERE {d}
             UNION ALL
             SELECT existing_path FROM PathCollisions WHERE {e}"
        )
    }

    fn cheap_tombstones_candidate_sql(&self) -> String {
        format!(
            "SELECT path FROM DuplicateTombstones WHERE {}",
            Self::cheap_path_candidate_predicate("path")
        )
    }

    fn any_path_table_needs_normalize(&self) -> SqliteResult<bool> {
        if self.path_rows_need_normalize("SELECT path FROM Files")? {
            return Ok(true);
        }
        if self.table_exists("PathCollisions")?
            && self.path_rows_need_normalize(
                "SELECT desired_path FROM PathCollisions
                 UNION ALL
                 SELECT existing_path FROM PathCollisions",
            )?
        {
            return Ok(true);
        }
        if self.table_exists("DuplicateTombstones")?
            && self.path_rows_need_normalize("SELECT path FROM DuplicateTombstones")?
        {
            return Ok(true);
        }
        Ok(false)
    }

    /// Definitive probe: any row where normalize changes the stored string.
    fn path_rows_need_normalize(&self, sql: &str) -> SqliteResult<bool> {
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            if normalize_db_path(&path) != path {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_path_normalize_error_log(&self, path: &str) -> SqliteResult<bool> {
        self.select_exists_params(
            "SELECT EXISTS(
                SELECT 1 FROM ErrorLog
                WHERE category = 'path_normalize' AND path = ?1
            )",
            params![path],
        )
    }

    fn select_exists_params(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> SqliteResult<bool> {
        let flag: i64 = self.conn.query_row(sql, params, |row| row.get(0))?;
        Ok(flag != 0)
    }

    fn repair_files_path_separators(&self) -> SqliteResult<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path FROM Files ORDER BY id ASC")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<SqliteResult<Vec<_>>>()?;

        for (id, path) in rows {
            let normalized = normalize_db_path(&path);
            if normalized == path {
                continue;
            }
            let conflict: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM Files WHERE path = ?1 AND id != ?2 LIMIT 1",
                    params![normalized, id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(other_id) = conflict {
                if !self.has_path_normalize_error_log(&path)? {
                    let _ = self.insert_error_log(
                        "path_normalize",
                        Some(id),
                        Some(&path),
                        "無法正規化路徑：正規化後會與既有 Files 列撞 UNIQUE 已保留原狀",
                        Some(&format!(
                            "normalized={normalized} conflicting_id={other_id}"
                        )),
                    );
                }
                continue;
            }
            self.conn.execute(
                "UPDATE Files SET path = ?1 WHERE id = ?2",
                params![normalized, id],
            )?;
        }
        Ok(())
    }

    fn repair_path_collisions_separators(&self) -> SqliteResult<()> {
        if !self.table_exists("PathCollisions")? {
            return Ok(());
        }

        let mut stmt = self.conn.prepare(
            "SELECT file_id, desired_path, existing_path FROM PathCollisions",
        )?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<SqliteResult<Vec<_>>>()?;

        for (file_id, desired, existing) in rows {
            let desired_n = normalize_db_path(&desired);
            let existing_n = normalize_db_path(&existing);
            if desired_n == desired && existing_n == existing {
                continue;
            }
            // Re-query who currently occupies the normalized desired path
            let occupant_id = self
                .find_file_occupying_path(&desired_n, file_id)?
                .map(|f| f.id);
            // existing_path mirrors desired_path (#59); keep column for schema v6
            self.conn.execute(
                "UPDATE PathCollisions
                 SET desired_path = ?1, existing_path = ?1, occupant_file_id = ?2
                 WHERE file_id = ?3",
                params![desired_n, occupant_id, file_id],
            )?;
        }
        Ok(())
    }

    fn repair_tombstone_path_separators(&self) -> SqliteResult<()> {
        if !self.table_exists("DuplicateTombstones")? {
            return Ok(());
        }

        let mut stmt = self
            .conn
            .prepare("SELECT id, path FROM DuplicateTombstones ORDER BY id ASC")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<SqliteResult<Vec<_>>>()?;

        for (id, path) in rows {
            let normalized = normalize_db_path(&path);
            if normalized == path {
                continue;
            }
            let conflict: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM DuplicateTombstones WHERE path = ?1 AND id != ?2 LIMIT 1",
                    params![normalized, id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(other_id) = conflict {
                if !self.has_path_normalize_error_log(&path)? {
                    let _ = self.insert_error_log(
                        "path_normalize",
                        None,
                        Some(&path),
                        "無法正規化墓碑路徑：正規化後會與既有 DuplicateTombstones 列撞 UNIQUE 已保留原狀",
                        Some(&format!(
                            "tombstone_id={id} normalized={normalized} conflicting_id={other_id}"
                        )),
                    );
                }
                continue;
            }
            self.conn.execute(
                "UPDATE DuplicateTombstones SET path = ?1 WHERE id = ?2",
                params![normalized, id],
            )?;
        }
        Ok(())
    }

    /// Idempotent v5 column repair: `Files.organized_at` (+ backfill).
    ///
    /// Mid-flight PR commits stamped `user_version=5` without this column; opening those
    /// DBs must ALTER+backfill or scrape/move/dedup break. Shared by migration_5 and
    /// post-migration init so there is one code path.
    fn ensure_v5_columns(&self) -> SqliteResult<()> {
        let has_organized_at: bool = {
            let mut stmt = self.conn.prepare("PRAGMA table_info(Files)")?;
            let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
            let mut found = false;
            for c in cols {
                if c? == "organized_at" {
                    found = true;
                    break;
                }
            }
            found
        };
        if !has_organized_at {
            self.conn.execute(
                "ALTER TABLE Files ADD COLUMN organized_at DATETIME",
                [],
            )?;
        }
        // Backfill semantics unchanged (#54 tracks rule changes separately).
        self.conn.execute_batch(
            "
            UPDATE Files SET organized_at = CURRENT_TIMESTAMP
            WHERE organized_at IS NULL
              AND (
                id IN (SELECT file_id FROM Metadata)
                OR id IN (SELECT file_id FROM FileTags)
                OR id IN (
                    SELECT CAST(
                        CASE WHEN json_valid(payload)
                             THEN json_extract(payload, '$.file_id')
                             ELSE NULL END
                        AS INTEGER
                    )
                    FROM Jobs
                    WHERE type = 'pipeline' AND status = 'succeeded'
                )
              );
            ",
        )?;
        Ok(())
    }

    fn ensure_baseline_schema(&self) -> SqliteResult<()> {
        // 檔案索引表（物理層）
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT DEFAULT 'ready'  -- processing ready scrape_failed missing
            )",
            [],
        )?;

        // 元資料表（邏輯層）
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS Metadata (
                file_id INTEGER PRIMARY KEY,
                title TEXT,
                studio TEXT,
                release_date TEXT,
                summary TEXT,
                cover_image TEXT,
                sprite_sheet_path TEXT,
                source_type TEXT,
                FOREIGN KEY(file_id) REFERENCES Files(id)
            )",
            [],
        )?;

        // 若上次在 DROP Tags 後中斷 資料可能只在 Tags_mig 必須在建 Tags 之前救回
        self.recover_orphan_tags_mig_before_create()?;

        // 標籤表 同名可存在於不同 category
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT,
                UNIQUE(name, category)
            )",
            [],
        )?;

        // 檔案標籤關聯表
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS FileTags (
                file_id INTEGER,
                tag_id INTEGER,
                confidence FLOAT DEFAULT 1.0,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            )",
            [],
        )?;

        // 錯誤日誌 scrape ffmpeg move 等失敗可查
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS ErrorLog (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                category TEXT NOT NULL,
                file_id INTEGER,
                path TEXT,
                message TEXT NOT NULL,
                detail TEXT
            )",
            [],
        )?;

        // 遷移前先清 orphan 但 Tags 空且 Tags_mig 有資料時暫留 FileTags→Tags
        self.cleanup_fk_orphans_for_startup()?;
        self.migrate_tags_unique_to_name_category()?;
        // Tags 定案後再清一次並啟用 FK
        self.cleanup_fk_orphans_for_startup()?;
        self.enable_foreign_keys()?;

        Ok(())
    }

    fn get_user_version(&self) -> SqliteResult<i32> {
        self.conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
    }

    /// 讀取 PRAGMA user_version（整合測試與診斷用）
    #[cfg(any(test, feature = "test-support"))]
    pub fn read_user_version(&self) -> SqliteResult<i32> {
        self.get_user_version()
    }

    fn set_user_version(&self, version: i32) -> SqliteResult<()> {
        // version 僅來自內部常數 非使用者輸入
        self.conn
            .execute(&format!("PRAGMA user_version = {version}"), [])?;
        Ok(())
    }

    fn db_version_too_new_error(found: i32) -> rusqlite::Error {
        // 用 SqliteFailure 而非 InvalidParameterName
        // 避免對話框顯示 Invalid parameter name 前綴
        let msg = format!(
            "此資料庫由較新版應用程式建立（user_version={found}）目前程式僅支援至 {SCHEMA_USER_VERSION} 請升級應用程式後再開啟"
        );
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(msg),
        )
    }

    fn apply_pending_migrations(&self, from_version: i32) -> SqliteResult<()> {
        let mut version = from_version;
        while version < SCHEMA_USER_VERSION {
            let next = version + 1;
            match self.run_migration(next) {
                Ok(()) => version = next,
                Err(e) => {
                    let message = format!("schema migration to v{next} failed: {e}");
                    if let Err(log_err) = self.insert_error_log(
                        "database",
                        None,
                        None,
                        &message,
                        Some("user_version migration"),
                    ) {
                        eprintln!(
                            "warn: failed to write ErrorLog after migration failure: {log_err}"
                        );
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn run_migration(&self, target: i32) -> SqliteResult<()> {
        match target {
            2 => self.migration_2_tags_nullsafe_unique(),
            3 => self.migration_3_jobs_table(),
            4 => self.migration_4_suspected_duplicates(),
            5 => self.migration_5_duplicate_tombstones(),
            6 => self.migration_6_path_collisions(),
            7 => self.migration_7_pending_move_to(),
            8 => self.migration_8_organize_batches(),
            other => {
                // 用 SqliteFailure 而非 InvalidParameterName
                // 避免顯示 Invalid parameter name 前綴
                let msg = format!(
                    "未知的 schema 遷移目標 user_version={other}（程式僅實作至 {SCHEMA_USER_VERSION}）"
                );
                Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISUSE),
                    Some(msg),
                ))
            }
        }
    }

    /// #38 SuspectedDuplicates（near-phash 疑似重複確認）
    fn migration_4_suspected_duplicates(&self) -> SqliteResult<()> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS SuspectedDuplicates (
                    file_id INTEGER PRIMARY KEY,
                    keeper_id INTEGER NOT NULL,
                    distance INTEGER NOT NULL
                        CHECK (distance BETWEEN 0 AND 64),
                    resolution TEXT NOT NULL DEFAULT 'pending'
                        CHECK (resolution IN ('pending', 'confirmed', 'ignored')),
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    FOREIGN KEY(file_id) REFERENCES Files(id),
                    FOREIGN KEY(keeper_id) REFERENCES Files(id)
                );
                CREATE INDEX IF NOT EXISTS idx_suspected_duplicates_keeper
                    ON SuspectedDuplicates(keeper_id);
                CREATE INDEX IF NOT EXISTS idx_suspected_duplicates_resolution
                    ON SuspectedDuplicates(resolution);
                ",
            )?;
            self.set_user_version(4)?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!(
                        "warn: ROLLBACK failed after SuspectedDuplicates migration error: {rb}"
                    );
                }
                Err(e)
            }
        }
    }

    /// #40 PathCollisions（模板目的地碰撞：加後綴 / 跳過 / 覆蓋）
    fn migration_6_path_collisions(&self) -> SqliteResult<()> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS PathCollisions (
                    file_id INTEGER PRIMARY KEY,
                    desired_path TEXT NOT NULL,
                    existing_path TEXT NOT NULL,
                    occupant_file_id INTEGER,
                    resolution TEXT NOT NULL DEFAULT 'pending'
                        CHECK (resolution IN ('pending', 'suffix', 'skip', 'overwrite')),
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    FOREIGN KEY(file_id) REFERENCES Files(id),
                    FOREIGN KEY(occupant_file_id) REFERENCES Files(id)
                );
                CREATE INDEX IF NOT EXISTS idx_path_collisions_resolution
                    ON PathCollisions(resolution);
                CREATE INDEX IF NOT EXISTS idx_path_collisions_desired
                    ON PathCollisions(desired_path);
                ",
            )?;
            self.set_user_version(6)?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!(
                        "warn: ROLLBACK failed after PathCollisions migration error: {rb}"
                    );
                }
                Err(e)
            }
        }
    }

    /// Idempotent v7 column repair: `pending_source_vol` / `pending_source_idx`.
    ///
    /// Same class of bug as [`Self::ensure_v5_columns`] / #55: mid-flight 7996aae
    /// stamped `user_version=7` with only `pending_move_to`. Opening those DBs must
    /// ALTER the fs-id columns or `set_pending_move_to` fails. Shared helper below
    /// is also used by migration_7 so there is one ADD COLUMN path.
    ///
    /// Skips `BEGIN IMMEDIATE` when both columns already exist (common open path).
    fn ensure_v7_columns(&self) -> SqliteResult<()> {
        let need_vol = !self.files_column_exists("pending_source_vol")?;
        let need_idx = !self.files_column_exists("pending_source_idx")?;
        if !need_vol && !need_idx {
            return Ok(());
        }
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            // Re-check inside the tx (another connection may have raced)
            self.ensure_pending_source_fs_id_columns()?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();
        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!("warn: ROLLBACK failed after ensure_v7_columns error: {rb}");
                }
                Err(e)
            }
        }
    }

    /// ADD `pending_source_vol` / `pending_source_idx` when missing (no transaction).
    fn ensure_pending_source_fs_id_columns(&self) -> SqliteResult<()> {
        if !self.files_column_exists("pending_source_vol")? {
            self.conn.execute(
                "ALTER TABLE Files ADD COLUMN pending_source_vol INTEGER",
                [],
            )?;
        }
        if !self.files_column_exists("pending_source_idx")? {
            self.conn.execute(
                "ALTER TABLE Files ADD COLUMN pending_source_idx INTEGER",
                [],
            )?;
        }
        Ok(())
    }

    /// #73 OrganizeBatches + OrganizeBatchItems — durable batch move log for undo
    ///
    /// Idempotent via CREATE TABLE IF NOT EXISTS / CREATE INDEX IF NOT EXISTS.
    fn migration_8_organize_batches(&self) -> SqliteResult<()> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.ensure_organize_batch_schema()?;
            self.set_user_version(8)?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!(
                        "warn: ROLLBACK failed after organize_batches migration error: {rb}"
                    );
                }
                Err(e)
            }
        }
    }

    /// Idempotent v8 table repair when user_version >= 8 but tables/indexes missing.
    ///
    /// Skips `BEGIN IMMEDIATE` when both tables already exist (common open path).
    fn ensure_v8_tables(&self) -> SqliteResult<()> {
        let need_batches = !self.table_exists("OrganizeBatches")?;
        let need_items = !self.table_exists("OrganizeBatchItems")?;
        if !need_batches && !need_items {
            // Still ensure indexes (IF NOT EXISTS is cheap)
            self.conn.execute_batch(
                "
                CREATE INDEX IF NOT EXISTS idx_organize_batch_items_batch
                    ON OrganizeBatchItems(batch_id);
                CREATE INDEX IF NOT EXISTS idx_organize_batches_created
                    ON OrganizeBatches(created_at DESC);
                ",
            )?;
            return Ok(());
        }
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.ensure_organize_batch_schema()?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();
        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!("warn: ROLLBACK failed after ensure_v8_tables error: {rb}");
                }
                Err(e)
            }
        }
    }

    /// CREATE TABLE / INDEX for batch organize log (no transaction wrapper).
    ///
    /// CHECK + FK apply to newly created tables (CREATE IF NOT EXISTS). Mid-flight
    /// DBs that already have the tables keep their original DDL; behavior is enforced
    /// in application code as well.
    fn ensure_organize_batch_schema(&self) -> SqliteResult<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS OrganizeBatches (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                status TEXT NOT NULL DEFAULT 'executing'
                    CHECK (status IN (
                        'executing', 'undoing', 'completed', 'undone', 'partially_undone'
                    )),
                note TEXT
            );
            CREATE TABLE IF NOT EXISTS OrganizeBatchItems (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                batch_id INTEGER NOT NULL,
                file_id INTEGER NOT NULL,
                src_path TEXT NOT NULL,
                dst_path TEXT NOT NULL,
                size INTEGER NOT NULL,
                phash TEXT,
                status TEXT NOT NULL DEFAULT 'planned'
                    CHECK (status IN (
                        'planned', 'moving', 'moved', 'skipped', 'failed',
                        'undoing', 'undone', 'undo_refused', 'plan_changed'
                    )),
                collision TEXT,
                error TEXT,
                FOREIGN KEY (batch_id) REFERENCES OrganizeBatches(id) ON DELETE CASCADE,
                FOREIGN KEY (file_id) REFERENCES Files(id)
            );
            CREATE INDEX IF NOT EXISTS idx_organize_batch_items_batch
                ON OrganizeBatchItems(batch_id);
            CREATE INDEX IF NOT EXISTS idx_organize_batches_created
                ON OrganizeBatches(created_at DESC);
            ",
        )?;
        Ok(())
    }

    /// #66 Files.pending_move_to + source fs id — durable move intent for crash recovery
    ///
    /// Idempotent: skips ADD COLUMN when the column already exists (re-open / partial).
    fn migration_7_pending_move_to(&self) -> SqliteResult<()> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            if !self.files_column_exists("pending_move_to")? {
                self.conn.execute(
                    "ALTER TABLE Files ADD COLUMN pending_move_to TEXT",
                    [],
                )?;
            }
            self.ensure_pending_source_fs_id_columns()?;
            self.conn.execute_batch(
                "
                CREATE INDEX IF NOT EXISTS idx_files_pending_move_to
                    ON Files(pending_move_to)
                    WHERE pending_move_to IS NOT NULL;
                ",
            )?;
            self.set_user_version(7)?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!(
                        "warn: ROLLBACK failed after pending_move_to migration error: {rb}"
                    );
                }
                Err(e)
            }
        }
    }

    fn files_column_exists(&self, name: &str) -> SqliteResult<bool> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(Files)")?;
        let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for c in cols {
            if c? == name {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// #38 DuplicateTombstones + Files.organized_at（確認重複墓碑；durable organized）
    fn migration_5_duplicate_tombstones(&self) -> SqliteResult<()> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS DuplicateTombstones (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    path TEXT NOT NULL UNIQUE,
                    size INTEGER,
                    phash TEXT,
                    keeper_id INTEGER,
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
                );
                CREATE INDEX IF NOT EXISTS idx_duplicate_tombstones_phash
                    ON DuplicateTombstones(phash);
                ",
            )?;
            // Dev DBs that already created DuplicateTombstones under a premature v4
            // may lack `size` — add it if missing.
            let has_size: bool = {
                let mut stmt = self.conn.prepare("PRAGMA table_info(DuplicateTombstones)")?;
                let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
                let mut found = false;
                for c in cols {
                    if c? == "size" {
                        found = true;
                        break;
                    }
                }
                found
            };
            if !has_size {
                self.conn
                    .execute("ALTER TABLE DuplicateTombstones ADD COLUMN size INTEGER", [])?;
            }
            // Drop unused keeper_phash if a premature v4 schema introduced it
            let has_keeper_phash: bool = {
                let mut stmt = self.conn.prepare("PRAGMA table_info(DuplicateTombstones)")?;
                let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
                let mut found = false;
                for c in cols {
                    if c? == "keeper_phash" {
                        found = true;
                        break;
                    }
                }
                found
            };
            if has_keeper_phash {
                // SQLite < 3.35 cannot DROP COLUMN; leave column unused if present.
                eprintln!(
                    "warn: DuplicateTombstones.keeper_phash present (legacy); unused by app"
                );
            }
            // Durable organized marker — survives Jobs terminal prune (max 200).
            self.ensure_v5_columns()?;
            self.set_user_version(5)?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!(
                        "warn: ROLLBACK failed after DuplicateTombstones migration error: {rb}"
                    );
                }
                Err(e)
            }
        }
    }

    /// #34 建立 Jobs 表（queued/running/succeeded/failed/cancelled）
    fn migration_3_jobs_table(&self) -> SqliteResult<()> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS Jobs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    status TEXT NOT NULL CHECK (
                        status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')
                    ),
                    type TEXT NOT NULL CHECK (type IN ('pipeline', 'phash')),
                    payload TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    max_attempts INTEGER NOT NULL DEFAULT 3,
                    error TEXT,
                    retryable INTEGER NOT NULL DEFAULT 1 CHECK (retryable IN (0, 1)),
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
                );
                CREATE INDEX IF NOT EXISTS idx_jobs_status ON Jobs(status);
                ",
            )?;
            self.set_user_version(3)?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!("warn: ROLLBACK failed after Jobs migration error: {rb}");
                }
                Err(e)
            }
        }
    }

    /// #45 合併重複 NULL-category tags 後建立 IFNULL 唯一索引
    fn migration_2_tags_nullsafe_unique(&self) -> SqliteResult<()> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.merge_duplicate_null_category_tags()?;
            self.filetags_tags_foreign_key_check_ok()?;

            #[cfg(test)]
            if migrate_null_tags_fault::should_fail_after_merge() {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ErrorCode::ConstraintViolation,
                        extended_code: 19,
                    },
                    Some("injected migrate fault after null-tag merge".to_string()),
                ));
            }

            self.conn.execute(
                &format!(
                    "CREATE UNIQUE INDEX IF NOT EXISTS {TAGS_NULLSAFE_UNIQUE_INDEX}
                     ON Tags (name, IFNULL(category, ''))"
                ),
                [],
            )?;
            self.set_user_version(2)?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!(
                        "warn: ROLLBACK failed after null-tag migration error: {rb}"
                    );
                }
                Err(e)
            }
        }
    }

    fn merge_duplicate_null_category_tags(&self) -> SqliteResult<()> {
        // IFNULL(category,'') 會把 '' 與 NULL 視為同一鍵 先正規化再合併
        self.conn
            .execute("UPDATE Tags SET category = NULL WHERE category = ''", [])?;

        let mut name_stmt = self.conn.prepare(
            "SELECT name FROM Tags
             WHERE category IS NULL
             GROUP BY name
             HAVING COUNT(*) > 1",
        )?;
        let names = name_stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<SqliteResult<Vec<_>>>()?;
        drop(name_stmt);

        for name in names {
            let mut id_stmt = self.conn.prepare(
                "SELECT id FROM Tags
                 WHERE name = ?1 AND category IS NULL
                 ORDER BY id ASC",
            )?;
            let ids = id_stmt
                .query_map(params![name], |row| row.get::<_, i64>(0))?
                .collect::<SqliteResult<Vec<_>>>()?;
            drop(id_stmt);

            if ids.len() < 2 {
                continue;
            }
            let keeper = ids[0];
            for &dup in &ids[1..] {
                self.conn.execute(
                    "INSERT INTO FileTags (file_id, tag_id, confidence)
                     SELECT file_id, ?1, confidence FROM FileTags WHERE tag_id = ?2
                     ON CONFLICT(file_id, tag_id) DO UPDATE SET
                       confidence = max(FileTags.confidence, excluded.confidence)",
                    params![keeper, dup],
                )?;
                self.conn
                    .execute("DELETE FROM FileTags WHERE tag_id = ?1", params![dup])?;
                self.conn
                    .execute("DELETE FROM Tags WHERE id = ?1", params![dup])?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn tags_nullsafe_index_exists(&self) -> SqliteResult<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = ?1",
            params![TAGS_NULLSAFE_UNIQUE_INDEX],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn table_exists(&self, name: &str) -> SqliteResult<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn table_row_count(&self, name: &str) -> SqliteResult<i64> {
        // 表名來自內部固定字串 非使用者輸入
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |row| {
                row.get(0)
            })
    }

    fn tags_table_sql(&self) -> SqliteResult<Option<String>> {
        match self.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'Tags'",
            [],
            |row| row.get(0),
        ) {
            Ok(sql) => Ok(Some(sql)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn tags_schema_is_name_category_unique(table_sql: &str) -> bool {
        let normalized: String = table_sql
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        normalized.contains("unique(name,category)")
    }

    /// DROP Tags 後 RENAME 前中斷時 Tags_mig 有資料且 Tags 不存在
    fn recover_orphan_tags_mig_before_create(&self) -> SqliteResult<()> {
        if !self.table_exists("Tags")? && self.table_exists("Tags_mig")? {
            self.conn
                .execute("ALTER TABLE Tags_mig RENAME TO Tags", [])?;
        }
        Ok(())
    }

    /// 已是新 schema 時處理殘留 Tags_mig
    /// Tags 空且 Tags_mig 有資料則交易內改用 Tags_mig
    /// 兩邊都非空則備份 Tags_mig 不 DROP 以免丟掉 abb2fd3 殘留資料
    fn reconcile_leftover_tags_mig(&self) -> SqliteResult<()> {
        if !self.table_exists("Tags_mig")? {
            return Ok(());
        }

        let tags_count = self.table_row_count("Tags")?;
        let mig_count = self.table_row_count("Tags_mig")?;

        if tags_count == 0 && mig_count > 0 {
            return self.swap_empty_tags_with_tags_mig();
        }

        if mig_count == 0 {
            self.conn.execute("DROP TABLE Tags_mig", [])?;
            return Ok(());
        }

        // Tags 與 Tags_mig 都非空 保留殘表以免永久丟 tag
        self.backup_nonempty_tags_mig()?;
        Ok(())
    }

    fn swap_empty_tags_with_tags_mig(&self) -> SqliteResult<()> {
        let foreign_keys_on: i64 = self
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        // FK 必須在交易外關閉
        self.conn.execute("PRAGMA foreign_keys = OFF", [])?;
        let begin_result = (|| -> SqliteResult<()> {
            #[cfg(test)]
            if swap_tags_fault::should_fail_begin() {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ErrorCode::DatabaseBusy,
                        extended_code: 5,
                    },
                    Some("injected BEGIN IMMEDIATE failure".to_string()),
                ));
            }
            self.conn.execute("BEGIN IMMEDIATE", [])?;
            Ok(())
        })();
        if let Err(e) = begin_result {
            // BEGIN 失敗時仍須還原 FK
            if let Err(restore) = self.restore_foreign_keys_state(foreign_keys_on) {
                eprintln!(
                    "warn: restore foreign_keys after Tags_mig BEGIN failure: {}",
                    restore
                );
            }
            return Err(e);
        }
        let swap_result = (|| -> SqliteResult<()> {
            self.conn.execute("DROP TABLE Tags", [])?;
            self.conn
                .execute("ALTER TABLE Tags_mig RENAME TO Tags", [])?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();
        if let Err(e) = swap_result {
            if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                eprintln!(
                    "warn: ROLLBACK failed after Tags_mig swap error: {}",
                    rb
                );
            }
            if let Err(restore) = self.restore_foreign_keys_state(foreign_keys_on) {
                eprintln!(
                    "warn: restore foreign_keys after Tags_mig swap error: {}",
                    restore
                );
            }
            return Err(e);
        }
        self.restore_foreign_keys_state(foreign_keys_on)?;
        Ok(())
    }

    /// 將非空 Tags_mig 改名備份 處理同一秒碰撞
    fn backup_nonempty_tags_mig(&self) -> SqliteResult<String> {
        let base_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        for suffix in 0u32..1000 {
            let backup_name = if suffix == 0 {
                format!("Tags_mig_backup_{base_secs}")
            } else {
                format!("Tags_mig_backup_{base_secs}_{suffix}")
            };
            if self.table_exists(&backup_name)? {
                continue;
            }
            // 名稱僅含固定前綴與數字
            self.conn.execute(
                &format!("ALTER TABLE Tags_mig RENAME TO {backup_name}"),
                [],
            )?;
            let message = format!(
                "preserved nonempty Tags_mig as {backup_name} because Tags already has rows"
            );
            eprintln!("warn: {message}");
            if let Err(log_err) = self.insert_error_log(
                "migration",
                None,
                None,
                &message,
                Some("Tags_mig backup during reconcile"),
            ) {
                eprintln!("warn: failed to write ErrorLog for Tags_mig backup: {log_err}");
            }
            return Ok(backup_name);
        }

        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                extended_code: 1,
            },
            Some(
                "unable to allocate Tags_mig_backup_* name after collisions".to_string(),
            ),
        ))
    }

    /// 只檢查 FileTags 對 Tags 的 FK 避免無關 orphan 阻擋遷移
    fn filetags_tags_foreign_key_check_ok(&self) -> SqliteResult<()> {
        let mut stmt = self.conn.prepare("PRAGMA foreign_key_check(FileTags)")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let parent: String = row.get(2)?;
            if parent.eq_ignore_ascii_case("Tags") {
                let table: String = row.get(0)?;
                let rowid: i64 = row.get(1)?;
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ErrorCode::ConstraintViolation,
                        extended_code: 787,
                    },
                    Some(format!(
                        "foreign_key_check FileTags->Tags failed table={table} rowid={rowid}"
                    )),
                ));
            }
        }
        Ok(())
    }

    fn restore_foreign_keys_state(&self, was_on: i64) -> SqliteResult<()> {
        if was_on == 0 {
            return Ok(());
        }
        self.conn.execute("PRAGMA foreign_keys = ON", [])?;
        let now: i64 = self
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        if now == 0 {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::ApiMisuse,
                    extended_code: 21,
                },
                Some(
                    "failed to restore foreign_keys=ON after Tags migration".to_string(),
                ),
            ));
        }
        Ok(())
    }

    /// 啟用連線級 FK 並以 foreign_key_check 確認無殘留違反
    fn enable_foreign_keys(&self) -> SqliteResult<()> {
        self.conn.execute("PRAGMA foreign_keys = ON", [])?;
        let enabled: i64 = self
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        if enabled == 0 {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::ApiMisuse,
                    extended_code: 21,
                },
                Some("failed to enable PRAGMA foreign_keys=ON".to_string()),
            ));
        }
        let mut stmt = self.conn.prepare("PRAGMA foreign_key_check")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let table: String = row.get(0)?;
            let rowid: i64 = row.get(1)?;
            let parent: String = row.get(2)?;
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::ConstraintViolation,
                    extended_code: 787,
                },
                Some(format!(
                    "foreign_key_check failed after orphan cleanup table={table} rowid={rowid} parent={parent}"
                )),
            ));
        }
        Ok(())
    }

    fn select_exists(&self, sql: &str) -> SqliteResult<bool> {
        let flag: i64 = self.conn.query_row(sql, [], |row| row.get(0))?;
        Ok(flag != 0)
    }

    /// 單一交易 bulk 清除 orphan 若有刪除則寫一筆 ErrorLog 摘要
    /// 先以 cheap EXISTS 探測 無 orphan 時不取 BEGIN IMMEDIATE 寫鎖
    /// Tags 空且 Tags_mig 有資料時跳過 FileTags→Tags 以免交換前誤刪
    fn cleanup_fk_orphans_for_startup(&self) -> SqliteResult<()> {
        let has_filetags = self.table_exists("FileTags")?;
        let has_files = self.table_exists("Files")?;
        let has_metadata = self.table_exists("Metadata")?;
        let has_tags = self.table_exists("Tags")?;

        let clean_filetags_tags = if has_filetags && has_tags {
            let tags_count = self.table_row_count("Tags")?;
            let mig_count = if self.table_exists("Tags_mig")? {
                self.table_row_count("Tags_mig")?
            } else {
                0
            };
            tags_count > 0 || mig_count == 0
        } else {
            false
        };

        let has_ft_files_orphans = if has_filetags && has_files {
            self.select_exists(
                "SELECT EXISTS(
                    SELECT 1 FROM FileTags
                    WHERE file_id NOT IN (SELECT id FROM Files)
                )",
            )?
        } else {
            false
        };
        let has_meta_orphans = if has_metadata && has_files {
            self.select_exists(
                "SELECT EXISTS(
                    SELECT 1 FROM Metadata
                    WHERE file_id NOT IN (SELECT id FROM Files)
                )",
            )?
        } else {
            false
        };
        let has_ft_tags_orphans = if clean_filetags_tags {
            self.select_exists(
                "SELECT EXISTS(
                    SELECT 1 FROM FileTags
                    WHERE tag_id NOT IN (SELECT id FROM Tags)
                )",
            )?
        } else {
            false
        };

        if !has_ft_files_orphans && !has_meta_orphans && !has_ft_tags_orphans {
            return Ok(());
        }

        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let result = (|| -> SqliteResult<()> {
            let filetags_missing_files = if has_ft_files_orphans {
                self.conn.execute(
                    "DELETE FROM FileTags WHERE file_id NOT IN (SELECT id FROM Files)",
                    [],
                )?
            } else {
                0
            };
            let metadata_missing_files = if has_meta_orphans {
                self.conn.execute(
                    "DELETE FROM Metadata WHERE file_id NOT IN (SELECT id FROM Files)",
                    [],
                )?
            } else {
                0
            };
            let filetags_missing_tags = if has_ft_tags_orphans {
                self.conn.execute(
                    "DELETE FROM FileTags WHERE tag_id NOT IN (SELECT id FROM Tags)",
                    [],
                )?
            } else {
                0
            };

            let total =
                filetags_missing_files + metadata_missing_files + filetags_missing_tags;
            if total > 0 {
                let message = format!(
                    "startup orphan cleanup removed FileTags(missing Files)={filetags_missing_files} FileTags(missing Tags)={filetags_missing_tags} Metadata(missing Files)={metadata_missing_files}"
                );
                eprintln!("warn: {message}");
                self.insert_error_log(
                    "database",
                    None,
                    None,
                    &message,
                    Some("startup foreign_key orphan cleanup summary"),
                )?;
            }
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        if let Err(e) = result {
            if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                eprintln!("warn: ROLLBACK failed after orphan cleanup error: {rb}");
            }
            return Err(e);
        }
        Ok(())
    }

    /// 既有 DB 若仍為 name 唯一 則重建為 (name, category) 唯一
    /// 整段在單一交易內完成 失敗則 ROLLBACK 並保留原 Tags
    fn migrate_tags_unique_to_name_category(&self) -> SqliteResult<()> {
        let table_sql = match self.tags_table_sql()? {
            Some(sql) => sql,
            None => return Ok(()),
        };

        if Self::tags_schema_is_name_category_unique(&table_sql) {
            self.reconcile_leftover_tags_mig()?;
            return Ok(());
        }

        // PRAGMA foreign_keys 在交易內無效 必須在 BEGIN 之前關閉
        let foreign_keys_on: i64 = self
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        self.conn.execute("PRAGMA foreign_keys = OFF", [])?;

        let migrate_result = self.run_tags_unique_migration_tx();
        match migrate_result {
            Ok(()) => self.restore_foreign_keys_state(foreign_keys_on),
            Err(e) => {
                // 保留遷移主錯誤 仍嘗試恢復 FK
                if let Err(restore) = self.restore_foreign_keys_state(foreign_keys_on) {
                    eprintln!(
                        "warn: restore foreign_keys after Tags migrate error: {}",
                        restore
                    );
                }
                Err(e)
            }
        }
    }

    fn run_tags_unique_migration_tx(&self) -> SqliteResult<()> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.conn.execute("DROP TABLE IF EXISTS Tags_mig", [])?;
            self.conn.execute(
                "CREATE TABLE Tags_mig (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    category TEXT,
                    UNIQUE(name, category)
                )",
                [],
            )?;
            self.conn.execute(
                "INSERT INTO Tags_mig (id, name, category)
                 SELECT id, name, category FROM Tags",
                [],
            )?;

            #[cfg(test)]
            if migrate_tags_fault::should_fail_after_insert() {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ErrorCode::ConstraintViolation,
                        extended_code: 19,
                    },
                    Some("injected migrate fault after insert".to_string()),
                ));
            }

            self.conn.execute("DROP TABLE Tags", [])?;
            self.conn
                .execute("ALTER TABLE Tags_mig RENAME TO Tags", [])?;
            self.filetags_tags_foreign_key_check_ok()?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                // 保留主錯誤 ROLLBACK 失敗只記 warn
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!(
                        "warn: ROLLBACK failed after Tags migrate error: {}",
                        rb
                    );
                }
                Err(e)
            }
        }
    }

    /// Upsert by path. Returns `(id, is_new)`.
    /// Same path never creates a second Files row (UNIQUE + ON CONFLICT).
    pub fn upsert_file(
        &self,
        path: &str,
        size: u64,
        phash: Option<String>,
    ) -> SqliteResult<(i64, bool)> {
        let path = normalize_db_path(path);
        let existed = self.get_file_by_path(&path)?.is_some();
        // 掃描再次找到檔案時 若先前為 missing 則恢復 ready
        self.conn.execute(
            "INSERT INTO Files (path, size, phash, status) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               phash = COALESCE(excluded.phash, Files.phash),
               status = CASE
                 WHEN Files.status = ?5 THEN ?4
                 ELSE Files.status
               END",
            params![path, size as i64, phash, file_status::ready, file_status::missing],
        )?;
        let id = match self.get_file_by_path(&path)? {
            Some(f) => f.id,
            None => self.conn.last_insert_rowid(),
        };
        Ok((id, !existed))
    }

    /// Insert or update by path; returns the stable row id (path-unique).
    #[cfg(test)]
    pub fn insert_file(
        &self,
        path: &str,
        size: u64,
        phash: Option<String>,
    ) -> SqliteResult<i64> {
        let (id, _) = self.upsert_file(path, size, phash)?;
        Ok(id)
    }

    /// Count Files rows (for path-dedup assertions).
    #[cfg(test)]
    pub fn count_files(&self) -> SqliteResult<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM Files", [], |row| row.get(0))
    }

    /// Find another Files row whose phash is within `max_distance` of `phash_hex`.
    /// Considers any `id != exclude_file_id` (order-independent with multi-worker phash).
    /// Skips candidates already suspected-of this file (prevents reverse re-flag).
    /// Caller may only auto-act on the just-phashed `exclude_file_id` (never on the match).
    pub fn find_near_phash_duplicate(
        &self,
        exclude_file_id: i64,
        phash_hex: &str,
        max_distance: u32,
    ) -> SqliteResult<Option<(FileRecord, u32)>> {
        let probe = match phash::parse_phash_hex(phash_hex) {
            Some(v) => v,
            None => return Ok(None),
        };

        let mut stmt = self.conn.prepare(
            "SELECT id, path, size, created_at, phash, status FROM Files
             WHERE id != ?1 AND phash IS NOT NULL AND TRIM(phash) != ''
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![exclude_file_id], |row| {
            Ok(FileRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                size: row.get::<_, i64>(2)? as u64,
                created_at: row.get(3)?,
                phash: row.get(4)?,
                status: row.get(5)?,
            })
        })?;

        let mut best: Option<(FileRecord, u32)> = None;
        for row in rows {
            let file = row?;
            // Skip rows already suspected-of this file (reverse link)
            if self.is_suspected_of(file.id, exclude_file_id)? {
                continue;
            }
            let Some(ref hex) = file.phash else {
                continue;
            };
            let Some(other) = phash::parse_phash_hex(hex) else {
                continue;
            };
            let dist = phash::hamming_distance(probe, other);
            if dist > max_distance {
                continue;
            }
            let replace = match &best {
                None => true,
                Some((keeper, best_dist)) => {
                    dist < *best_dist || (dist == *best_dist && file.id < keeper.id)
                }
            };
            if replace {
                best = Some((file, dist));
            }
        }
        Ok(best)
    }

    /// Organized = Files.organized_at set, or Metadata / FileTags still present.
    ///
    /// `organized_at` is the durable signal (survives Jobs prune). Metadata/FileTags
    /// remain as live signals for rows organized before the column existed.
    pub fn is_organized_file(&self, file_id: i64) -> SqliteResult<bool> {
        if self.file_has_organized_at(file_id)? {
            return Ok(true);
        }
        if self.has_metadata_row(file_id)? {
            return Ok(true);
        }
        if self.has_file_tags(file_id)? {
            return Ok(true);
        }
        Ok(false)
    }

    fn file_has_organized_at(&self, file_id: i64) -> SqliteResult<bool> {
        let v: Option<String> = self.conn.query_row(
            "SELECT organized_at FROM Files WHERE id = ?1",
            params![file_id],
            |row| row.get(0),
        )?;
        Ok(v.filter(|s| !s.trim().is_empty()).is_some())
    }

    /// Test helper: true when `Files.organized_at` column is non-null (#73 undo)
    #[cfg(any(test, feature = "test-support"))]
    pub fn organized_at_is_set(&self, file_id: i64) -> SqliteResult<bool> {
        self.file_has_organized_at(file_id)
    }

    /// Persist organized marker (scrape/move success). Idempotent.
    pub fn mark_file_organized(&self, file_id: i64) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE Files SET organized_at = COALESCE(organized_at, CURRENT_TIMESTAMP)
             WHERE id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    /// Clear organized marker after batch undo returns a file to its pre-batch path (#73)
    pub fn clear_file_organized_at(&self, file_id: i64) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE Files SET organized_at = NULL WHERE id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    pub fn has_file_tags(&self, file_id: i64) -> SqliteResult<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM FileTags WHERE file_id = ?1",
            params![file_id],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    fn count_pipeline_jobs_with_file_id(
        &self,
        file_id: i64,
        statuses: &str,
    ) -> SqliteResult<i64> {
        if !self.table_exists("Jobs")? {
            return Ok(0);
        }
        // Exact match via json_extract — do not use instr('"file_id":1') (matches 12, 123…).
        // json_valid guards so one corrupt payload cannot break all dedup lookups.
        self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM Jobs
                 WHERE type = 'pipeline'
                   AND status IN ({statuses})
                   AND json_valid(payload)
                   AND CAST(json_extract(payload, '$.file_id') AS INTEGER) = ?1"
            ),
            params![file_id],
            |row| row.get(0),
        )
    }

    /// Pipeline job currently executing (`status='running'`).
    ///
    /// Own `queued` pipeline does **not** count — watcher enqueues pipeline before
    /// phash completes, so treating queued as busy would invert #38 victims.
    pub fn has_running_pipeline_job(&self, file_id: i64) -> SqliteResult<bool> {
        Ok(self.count_pipeline_jobs_with_file_id(file_id, "'running'")? > 0)
    }

    /// THE RULE: auto delete/flag may only target this just-phashed row when it is
    /// not organized and does not have a running pipeline job.
    pub fn may_auto_act_on_self(&self, file_id: i64) -> SqliteResult<bool> {
        if self.is_organized_file(file_id)? {
            return Ok(false);
        }
        if self.has_running_pipeline_job(file_id)? {
            return Ok(false);
        }
        Ok(true)
    }

    /// True when `file_id` has any SuspectedDuplicates row pointing at `keeper_id`.
    pub fn is_suspected_of(&self, file_id: i64, keeper_id: i64) -> SqliteResult<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM SuspectedDuplicates
             WHERE file_id = ?1 AND keeper_id = ?2",
            params![file_id, keeper_id],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// True when this (file_id, keeper_id) pair was already ignored.
    pub fn is_ignored_suspected_pair(
        &self,
        file_id: i64,
        keeper_id: i64,
    ) -> SqliteResult<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM SuspectedDuplicates
             WHERE file_id = ?1 AND keeper_id = ?2 AND resolution = 'ignored'",
            params![file_id, keeper_id],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// Upsert pending suspicion; never resets ignored/confirmed back to pending.
    pub fn upsert_suspected_duplicate(
        &self,
        file_id: i64,
        keeper_id: i64,
        distance: u32,
    ) -> SqliteResult<()> {
        self.conn.execute(
            "INSERT INTO SuspectedDuplicates (file_id, keeper_id, distance, resolution)
             VALUES (?1, ?2, ?3, 'pending')
             ON CONFLICT(file_id) DO UPDATE SET
               keeper_id = excluded.keeper_id,
               distance = excluded.distance,
               resolution = 'pending'
             WHERE SuspectedDuplicates.resolution = 'pending'",
            params![file_id, keeper_id, distance as i64],
        )?;
        Ok(())
    }

    /// Atomically write phash (optional), upsert pending suspicion, set status.
    ///
    /// Decision: `scrape_failed` / `missing` are **not** transitioned to
    /// `suspected_duplicate` (keep failure / missing semantics). Near-phash
    /// marking is skipped for those statuses (caller should treat as Distinct).
    pub fn mark_suspected_duplicate_atomic(
        &self,
        file_id: i64,
        keeper_id: i64,
        distance: u32,
        phash_hex: Option<&str>,
    ) -> SqliteResult<()> {
        let current = self.get_file_by_id(file_id)?.status;
        if current == file_status::scrape_failed || current == file_status::missing {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "refusing suspected mark for status={current}"
            )));
        }
        if self.is_ignored_suspected_pair(file_id, keeper_id)? {
            return Ok(());
        }

        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            if let Some(hash) = phash_hex {
                self.conn.execute(
                    "UPDATE Files SET phash = ?1 WHERE id = ?2",
                    params![hash, file_id],
                )?;
            }
            self.upsert_suspected_duplicate(file_id, keeper_id, distance)?;
            // If row is ignored/confirmed, upsert did not reset it — do not flip status
            let resolution: Option<String> = self
                .conn
                .query_row(
                    "SELECT resolution FROM SuspectedDuplicates WHERE file_id = ?1",
                    params![file_id],
                    |row| row.get(0),
                )
                .optional()?;
            if resolution.as_deref() != Some("pending") {
                self.conn.execute("COMMIT", [])?;
                return Ok(());
            }
            let status: String = self.conn.query_row(
                "SELECT status FROM Files WHERE id = ?1",
                params![file_id],
                |row| row.get(0),
            )?;
            if status != file_status::suspected_duplicate {
                if !file_status::can_transition(&status, file_status::suspected_duplicate) {
                    return Err(rusqlite::Error::InvalidParameterName(format!(
                        "illegal status transition: {status} -> suspected_duplicate"
                    )));
                }
                self.conn.execute(
                    "UPDATE Files SET status = ?1 WHERE id = ?2",
                    params![file_status::suspected_duplicate, file_id],
                )?;
            }
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!("warn: ROLLBACK failed after mark_suspected: {rb}");
                }
                Err(e)
            }
        }
    }

    /// Pending suspicion for a file, if any.
    pub fn get_pending_suspected_duplicate(
        &self,
        file_id: i64,
    ) -> SqliteResult<Option<SuspectedDuplicateRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_id, keeper_id, distance, resolution FROM SuspectedDuplicates
             WHERE file_id = ?1 AND resolution = 'pending'",
        )?;
        let mut rows = stmt.query(params![file_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(SuspectedDuplicateRow {
                file_id: row.get(0)?,
                keeper_id: row.get(1)?,
                distance: row.get::<_, i64>(2)? as u32,
                resolution: row.get(3)?,
            })),
            None => Ok(None),
        }
    }

    /// Any SuspectedDuplicates row for file_id (any resolution).
    #[cfg(any(test, feature = "test-support"))]
    pub fn get_suspected_duplicate_row(
        &self,
        file_id: i64,
    ) -> SqliteResult<Option<SuspectedDuplicateRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_id, keeper_id, distance, resolution FROM SuspectedDuplicates
             WHERE file_id = ?1",
        )?;
        let mut rows = stmt.query(params![file_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(SuspectedDuplicateRow {
                file_id: row.get(0)?,
                keeper_id: row.get(1)?,
                distance: row.get::<_, i64>(2)? as u32,
                resolution: row.get(3)?,
            })),
            None => Ok(None),
        }
    }

    /// Mark suspicion resolved and clear Files.status when ignoring.
    pub fn resolve_suspected_duplicate(
        &self,
        file_id: i64,
        resolution: &str,
    ) -> SqliteResult<SuspectedDuplicateRow> {
        if resolution != "confirmed" && resolution != "ignored" {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "invalid suspected-duplicate resolution: {resolution}"
            )));
        }
        let pending = self
            .get_pending_suspected_duplicate(file_id)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;

        self.conn.execute(
            "UPDATE SuspectedDuplicates SET resolution = ?1 WHERE file_id = ?2",
            params![resolution, file_id],
        )?;

        if resolution == "ignored" {
            let status = self.get_file_by_id(file_id)?.status;
            if status == file_status::suspected_duplicate {
                self.update_file_status(file_id, file_status::ready)?;
            }
        }

        Ok(SuspectedDuplicateRow {
            resolution: resolution.to_string(),
            ..pending
        })
    }

    /// Upsert pending path collision; refreshes paths while still pending.
    /// DB column `existing_path` is kept for schema v6 but always mirrors desired (#59)
    pub fn upsert_path_collision(
        &self,
        file_id: i64,
        desired_path: &str,
        occupant_file_id: Option<i64>,
    ) -> SqliteResult<()> {
        let desired_path = normalize_db_path(desired_path);
        // Re-open as pending when desired changes or prior resolution was not skip
        // Same desired after skip stays skipped (caller should avoid calling)
        self.conn.execute(
            "INSERT INTO PathCollisions
                (file_id, desired_path, existing_path, occupant_file_id, resolution)
             VALUES (?1, ?2, ?2, ?3, 'pending')
             ON CONFLICT(file_id) DO UPDATE SET
               desired_path = excluded.desired_path,
               existing_path = excluded.existing_path,
               occupant_file_id = excluded.occupant_file_id,
               resolution = 'pending'",
            params![file_id, desired_path, occupant_file_id],
        )?;
        Ok(())
    }

    /// True when this file already skipped the same desired path (#59)
    pub fn has_skipped_same_path_collision(
        &self,
        file_id: i64,
        desired_path: &str,
    ) -> SqliteResult<bool> {
        let desired_path = normalize_db_path(desired_path);
        let mut stmt = self.conn.prepare(
            "SELECT 1 FROM PathCollisions
             WHERE file_id = ?1 AND resolution = 'skip' AND desired_path = ?2
             LIMIT 1",
        )?;
        let found = stmt
            .query_row(params![file_id, desired_path], |_| Ok(()))
            .optional()?;
        Ok(found.is_some())
    }

    /// Atomically record a template destination collision and set status.
    pub fn mark_path_collision_atomic(
        &self,
        file_id: i64,
        desired_path: &str,
        occupant_file_id: Option<i64>,
    ) -> SqliteResult<()> {
        let current = self.get_file_by_id(file_id)?.status;
        if current == file_status::missing || current == file_status::suspected_duplicate {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "refusing path collision mark for status={current}"
            )));
        }

        let desired_path = normalize_db_path(desired_path);

        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.upsert_path_collision(file_id, &desired_path, occupant_file_id)?;
            let resolution: Option<String> = self
                .conn
                .query_row(
                    "SELECT resolution FROM PathCollisions WHERE file_id = ?1",
                    params![file_id],
                    |row| row.get(0),
                )
                .optional()?;
            if resolution.as_deref() != Some("pending") {
                self.conn.execute("COMMIT", [])?;
                return Ok(());
            }
            let status: String = self.conn.query_row(
                "SELECT status FROM Files WHERE id = ?1",
                params![file_id],
                |row| row.get(0),
            )?;
            if status != file_status::path_collision {
                if !file_status::can_transition(&status, file_status::path_collision) {
                    return Err(rusqlite::Error::InvalidParameterName(format!(
                        "illegal status transition: {status} -> path_collision"
                    )));
                }
                self.conn.execute(
                    "UPDATE Files SET status = ?1 WHERE id = ?2",
                    params![file_status::path_collision, file_id],
                )?;
            }
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!("warn: ROLLBACK failed after mark_path_collision: {rb}");
                }
                Err(e)
            }
        }
    }

    pub fn get_pending_path_collision(
        &self,
        file_id: i64,
    ) -> SqliteResult<Option<PathCollisionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_id, desired_path, existing_path, occupant_file_id, resolution
             FROM PathCollisions
             WHERE file_id = ?1 AND resolution = 'pending'",
        )?;
        let mut rows = stmt.query(params![file_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(PathCollisionRow {
                file_id: row.get(0)?,
                desired_path: row.get(1)?,
                existing_path: row.get(2)?,
                occupant_file_id: row.get(3)?,
                resolution: row.get(4)?,
            })),
            None => Ok(None),
        }
    }

    /// Mark path collision resolved and restore ready when appropriate.
    pub fn resolve_path_collision(
        &self,
        file_id: i64,
        resolution: &str,
    ) -> SqliteResult<PathCollisionRow> {
        if resolution != "suffix" && resolution != "skip" && resolution != "overwrite" {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "invalid path-collision resolution: {resolution}"
            )));
        }
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<PathCollisionRow> {
            let row = self.resolve_path_collision_in_tx(file_id, resolution)?;
            self.conn.execute("COMMIT", [])?;
            Ok(row)
        })();
        match tx_result {
            Ok(row) => Ok(row),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!("warn: ROLLBACK failed after resolve_path_collision: {rb}");
                }
                Err(e)
            }
        }
    }

    fn resolve_path_collision_in_tx(
        &self,
        file_id: i64,
        resolution: &str,
    ) -> SqliteResult<PathCollisionRow> {
        let pending = self
            .get_pending_path_collision(file_id)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;

        self.conn.execute(
            "UPDATE PathCollisions SET resolution = ?1 WHERE file_id = ?2",
            params![resolution, file_id],
        )?;

        let status = self.get_file_by_id(file_id)?.status;
        if status == file_status::path_collision {
            self.update_file_status(file_id, file_status::ready)?;
        }

        Ok(PathCollisionRow {
            resolution: resolution.to_string(),
            ..pending
        })
    }

    /// Find a library row currently occupying `path` (excludes `exclude_id`).
    /// Windows/macOS use COLLATE NOCASE; Linux uses exact match.
    pub fn find_file_occupying_path(
        &self,
        path: &str,
        exclude_id: i64,
    ) -> SqliteResult<Option<FileRecord>> {
        let path = normalize_db_path(path);
        #[cfg(any(windows, target_os = "macos"))]
        let sql = "SELECT id, path, size, created_at, phash, status FROM Files
                   WHERE path = ?1 COLLATE NOCASE AND id != ?2 LIMIT 1";
        #[cfg(not(any(windows, target_os = "macos")))]
        let sql = "SELECT id, path, size, created_at, phash, status FROM Files
                   WHERE path = ?1 AND id != ?2 LIMIT 1";

        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(params![path, exclude_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(FileRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                size: row.get::<_, i64>(2)? as u64,
                created_at: row.get(3)?,
                phash: row.get(4)?,
                status: row.get(5)?,
            })),
            None => Ok(None),
        }
    }

    /// #50 / #66: after crash mid-move, atomically repair Files.path when FS already moved
    /// Re-checks occupancy (NOCASE where applicable), expected old path, path update,
    /// clears `pending_move_to`, organized_at, and missing→ready in one BEGIN IMMEDIATE
    pub fn heal_moved_path(
        &self,
        file_id: i64,
        expected_old_path: &str,
        new_path: &str,
    ) -> SqliteResult<()> {
        // #61: all path comparisons go through normalize_db_path (same as overwrite)
        let expected_old_path = normalize_db_path(expected_old_path);
        let new_path = normalize_db_path(new_path);
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            if let Some(occupant) = self.find_file_occupying_path(&new_path, file_id)? {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "destination path owned by another Files row id={} path={}",
                    occupant.id, occupant.path
                )));
            }

            let current = self.get_file_by_id(file_id)?;
            if normalize_db_path(&current.path) != expected_old_path {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "Files.path changed during move heal (expected {expected_old_path} got {})",
                    current.path
                )));
            }

            self.conn.execute(
                "UPDATE Files SET path = ?1,
                     pending_move_to = NULL,
                     pending_source_vol = NULL,
                     pending_source_idx = NULL
                 WHERE id = ?2",
                params![new_path, file_id],
            )?;
            self.mark_file_organized(file_id)?;

            // S2: reconcile may have marked missing while source path was gone
            let after = self.get_file_by_id(file_id)?;
            if after.status == file_status::missing {
                if !file_status::can_transition(file_status::missing, file_status::ready) {
                    return Err(rusqlite::Error::InvalidParameterName(format!(
                        "illegal status transition: {} -> ready",
                        after.status
                    )));
                }
                self.conn.execute(
                    "UPDATE Files SET status = ?1 WHERE id = ?2",
                    params![file_status::ready, file_id],
                )?;
            }

            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!("warn: ROLLBACK failed after heal_moved_path: {rb}");
                }
                Err(e)
            }
        }
    }

    /// #66: record move intent + source fs id before any filesystem rename / overwrite
    pub fn set_pending_move_to(
        &self,
        file_id: i64,
        dest: &str,
        source_fs_id: Option<FsFileId>,
    ) -> SqliteResult<()> {
        let dest = normalize_db_path(dest);
        let (vol, idx) = match source_fs_id {
            Some(id) => (Some(id.volume as i64), Some(id.index as i64)),
            None => (None, None),
        };
        self.conn.execute(
            "UPDATE Files SET pending_move_to = ?1,
                 pending_source_vol = ?2,
                 pending_source_idx = ?3
             WHERE id = ?4",
            params![dest, vol, idx, file_id],
        )?;
        Ok(())
    }

    /// #66: clear move intent (pre-FS abort or successful commit)
    pub fn clear_pending_move_to(&self, file_id: i64) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE Files SET pending_move_to = NULL,
                 pending_source_vol = NULL,
                 pending_source_idx = NULL
             WHERE id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    /// Clear pending intent; on failure write ErrorLog (do not silently drop)
    pub fn clear_pending_move_to_logged(
        &self,
        file_id: i64,
        path_for_log: Option<&str>,
    ) -> SqliteResult<()> {
        match self.clear_pending_move_to(file_id) {
            Ok(()) => Ok(()),
            Err(e) => {
                let detail = e.to_string();
                if let Err(log_err) = self.insert_error_log(
                    "move",
                    Some(file_id),
                    path_for_log,
                    "failed to clear pending_move_to",
                    Some(&detail),
                ) {
                    eprintln!(
                        "warn: clear pending_move_to failed ({detail}) and ErrorLog failed: {log_err}"
                    );
                }
                Err(e)
            }
        }
    }

    /// #66: read `pending_move_to` for one file (normalized when present)
    pub fn get_pending_move_to(&self, file_id: i64) -> SqliteResult<Option<String>> {
        Ok(self.get_pending_move_row(file_id)?.map(|r| r.pending_move_to))
    }

    /// #66: one pending-move row (path + source fs id) or None
    pub fn get_pending_move_row(&self, file_id: i64) -> SqliteResult<Option<PendingMoveRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, pending_move_to, pending_source_vol, pending_source_idx,
                    size, phash, status
             FROM Files WHERE id = ?1
               AND pending_move_to IS NOT NULL AND TRIM(pending_move_to) != ''",
        )?;
        let mut rows = stmt.query(params![file_id])?;
        match rows.next()? {
            Some(row) => {
                let pending: String = row.get(2)?;
                Ok(Some(PendingMoveRow {
                    file_id: row.get(0)?,
                    path: row.get(1)?,
                    pending_move_to: normalize_db_path(&pending),
                    pending_source_vol: row.get(3)?,
                    pending_source_idx: row.get(4)?,
                    size: row.get::<_, i64>(5)? as u64,
                    phash: row.get(6)?,
                    status: row.get(7)?,
                }))
            }
            None => Ok(None),
        }
    }

    /// #66: all rows with a non-null pending destination (startup recovery)
    pub fn list_pending_moves(&self) -> SqliteResult<Vec<PendingMoveRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, pending_move_to, pending_source_vol, pending_source_idx,
                    size, phash, status
             FROM Files
             WHERE pending_move_to IS NOT NULL AND TRIM(pending_move_to) != ''
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let pending: String = row.get(2)?;
            Ok(PendingMoveRow {
                file_id: row.get(0)?,
                path: row.get(1)?,
                pending_move_to: normalize_db_path(&pending),
                pending_source_vol: row.get(3)?,
                pending_source_idx: row.get(4)?,
                size: row.get::<_, i64>(5)? as u64,
                phash: row.get(6)?,
                status: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// After a successful overwrite filesystem move: one transaction updates the
    /// live occupant (re-queried), the mover path/organized/ready, and collision.
    ///
    /// `same_file_overwrite` is true for case-only / hard-link renames (#59): the
    /// occupant row is displaced but not marked missing (same inode as the mover).
    pub fn commit_path_collision_overwrite(
        &self,
        mover_id: i64,
        expected_old_path: &str,
        new_path: &str,
        same_file_overwrite: bool,
    ) -> SqliteResult<()> {
        let expected_old_path = normalize_db_path(expected_old_path);
        let new_path = normalize_db_path(new_path);
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            // S1: re-query who currently owns desired_path (stale occupant_file_id ignored)
            // Same-file / hardlink: displace + missing (do not delete Metadata/FileTags).
            // Clear occupant_file_id FKs first so a pending PathCollisions row that
            // referenced this occupant cannot block the transaction after FS move.
            if let Some(occupant) = self.find_file_occupying_path(&new_path, mover_id)? {
                self.conn.execute(
                    "UPDATE PathCollisions SET occupant_file_id = NULL
                     WHERE occupant_file_id = ?1",
                    params![occupant.id],
                )?;
                let displaced = normalize_db_path(&format!(
                    "{}#displaced-{}",
                    occupant.path, occupant.id
                ));
                self.conn.execute(
                    "UPDATE Files SET path = ?1 WHERE id = ?2",
                    params![displaced, occupant.id],
                )?;
                if occupant.status != file_status::missing {
                    if !file_status::can_transition(&occupant.status, file_status::missing) {
                        return Err(rusqlite::Error::InvalidParameterName(format!(
                            "illegal status transition: {} -> missing",
                            occupant.status
                        )));
                    }
                    self.conn.execute(
                        "UPDATE Files SET status = ?1 WHERE id = ?2",
                        params![file_status::missing, occupant.id],
                    )?;
                }
                let message = if same_file_overwrite {
                    // Nit: on Linux hardlink rename may leave the original path
                    // still present so a later rescan can re-import that path
                    "occupant row overwritten (same-file hardlink or case-only)"
                } else {
                    "occupant row overwritten by path collision resolve"
                };
                let _ = self.insert_error_log(
                    "path_collision",
                    Some(occupant.id),
                    Some(&occupant.path),
                    message,
                    Some(&format!(
                        "mover_id={mover_id} displaced_to={displaced} same_file={same_file_overwrite}"
                    )),
                );
            }

            let mover = self.get_file_by_id(mover_id)?;
            if normalize_db_path(&mover.path) != expected_old_path {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "Files.path changed during overwrite (expected {expected_old_path} got {})",
                    mover.path
                )));
            }
            self.conn.execute(
                "UPDATE Files SET path = ?1,
                     pending_move_to = NULL,
                     pending_source_vol = NULL,
                     pending_source_idx = NULL
                 WHERE id = ?2",
                params![new_path, mover_id],
            )?;
            self.mark_file_organized(mover_id)?;
            self.resolve_path_collision_in_tx(mover_id, "overwrite")?;

            #[cfg(any(test, feature = "test-support"))]
            if path_collision_tx_fault::should_fail_commit() {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                    Some("injected path-collision overwrite commit failure".into()),
                ));
            }

            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!(
                        "warn: ROLLBACK failed after commit_path_collision_overwrite: {rb}"
                    );
                }
                Err(e)
            }
        }
    }

    /// Permanent tombstone so confirmed duplicates are not re-ingested after PathSuppress TTL.
    /// Stores `size` so a later different file at the same path is not silently blocked.
    pub fn insert_duplicate_tombstone(
        &self,
        path: &str,
        size: u64,
        phash: Option<&str>,
        keeper_id: Option<i64>,
    ) -> SqliteResult<()> {
        self.insert_duplicate_tombstone_inner(path, size, phash, keeper_id)
    }

    fn insert_duplicate_tombstone_inner(
        &self,
        path: &str,
        size: u64,
        phash: Option<&str>,
        keeper_id: Option<i64>,
    ) -> SqliteResult<()> {
        let path = normalize_db_path(path);
        self.conn.execute(
            "INSERT INTO DuplicateTombstones (path, size, phash, keeper_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               phash = COALESCE(excluded.phash, DuplicateTombstones.phash),
               keeper_id = COALESCE(excluded.keeper_id, DuplicateTombstones.keeper_id)",
            params![path, size as i64, phash, keeper_id],
        )?;
        Ok(())
    }

    pub fn get_duplicate_tombstone_by_path(
        &self,
        path: &str,
    ) -> SqliteResult<Option<DuplicateTombstone>> {
        let path = normalize_db_path(path);
        let mut stmt = self.conn.prepare(
            "SELECT id, path, size, phash, keeper_id, created_at
             FROM DuplicateTombstones WHERE path = ?1",
        )?;
        let mut rows = stmt.query(params![path])?;
        match rows.next()? {
            Some(row) => Ok(Some(DuplicateTombstone {
                id: row.get(0)?,
                path: row.get(1)?,
                size: row.get(2)?,
                phash: row.get(3)?,
                keeper_id: row.get(4)?,
                created_at: row.get(5)?,
            })),
            None => Ok(None),
        }
    }

    /// Path tombstone gate for import: same size → block; different size → clear tombstone.
    pub fn check_path_tombstone(
        &self,
        path: &str,
        size: u64,
    ) -> SqliteResult<PathTombstoneAction> {
        let path = normalize_db_path(path);
        let Some(ts) = self.get_duplicate_tombstone_by_path(&path)? else {
            return Ok(PathTombstoneAction::None);
        };
        match ts.size {
            Some(stored) if stored as u64 != size => {
                self.remove_duplicate_tombstone(ts.id)?;
                Ok(PathTombstoneAction::Cleared)
            }
            _ => Ok(PathTombstoneAction::Block),
        }
    }

    pub fn get_duplicate_tombstone_by_phash(
        &self,
        phash_hex: &str,
    ) -> SqliteResult<Option<DuplicateTombstone>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, size, phash, keeper_id, created_at
             FROM DuplicateTombstones
             WHERE phash IS NOT NULL AND lower(phash) = lower(?1)
             LIMIT 1",
        )?;
        let mut rows = stmt.query(params![phash_hex])?;
        match rows.next()? {
            Some(row) => Ok(Some(DuplicateTombstone {
                id: row.get(0)?,
                path: row.get(1)?,
                size: row.get(2)?,
                phash: row.get(3)?,
                keeper_id: row.get(4)?,
                created_at: row.get(5)?,
            })),
            None => Ok(None),
        }
    }

    pub fn list_duplicate_tombstones(&self) -> SqliteResult<Vec<DuplicateTombstone>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, size, phash, keeper_id, created_at
             FROM DuplicateTombstones ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DuplicateTombstone {
                id: row.get(0)?,
                path: row.get(1)?,
                size: row.get(2)?,
                phash: row.get(3)?,
                keeper_id: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn remove_duplicate_tombstone(&self, id: i64) -> SqliteResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM DuplicateTombstones WHERE id = ?1",
            params![id],
        )?;
        Ok(n > 0)
    }

    pub fn has_metadata_row(&self, file_id: i64) -> SqliteResult<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM Metadata WHERE file_id = ?1",
            params![file_id],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// Confirm suspected duplicate atomically: tombstone + resolve + delete in one tx.
    /// Returns orphan path for PathSuppress **after** COMMIT.
    pub fn confirm_suspected_duplicate(&self, file_id: i64) -> SqliteResult<String> {
        self.confirm_suspected_duplicate_impl(file_id, false)
    }

    /// Test hook: fail after resolve (before delete) to verify ROLLBACK recoverability.
    #[cfg(any(test, feature = "test-support"))]
    pub fn confirm_suspected_duplicate_fail_after_resolve(
        &self,
        file_id: i64,
    ) -> SqliteResult<String> {
        self.confirm_suspected_duplicate_impl(file_id, true)
    }

    fn confirm_suspected_duplicate_impl(
        &self,
        file_id: i64,
        fail_after_resolve: bool,
    ) -> SqliteResult<String> {
        let file = self.get_file_by_id(file_id)?;
        if file.status != file_status::suspected_duplicate {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "file {file_id} is not suspected_duplicate"
            )));
        }
        let pending = self
            .get_pending_suspected_duplicate(file_id)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;

        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<String> {
            self.insert_duplicate_tombstone(
                &file.path,
                file.size,
                file.phash.as_deref(),
                Some(pending.keeper_id),
            )?;
            self.conn.execute(
                "UPDATE SuspectedDuplicates SET resolution = 'confirmed' WHERE file_id = ?1",
                params![file_id],
            )?;
            if fail_after_resolve {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                    Some("injected confirm failure after resolve".into()),
                ));
            }
            self.delete_file_record_inner(file_id)?;
            self.conn.execute("COMMIT", [])?;
            Ok(file.path.clone())
        })();

        match tx_result {
            Ok(path) => Ok(path),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!("warn: ROLLBACK failed after confirm_suspected: {rb}");
                }
                Err(e)
            }
        }
    }

    /// Remove a Files row and children; restore pending orphans that pointed at it as keeper.
    pub fn delete_file_record(&self, file_id: i64) -> SqliteResult<()> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<()> {
            self.delete_file_record_inner(file_id)?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!("warn: ROLLBACK failed after delete_file_record: {rb}");
                }
                Err(e)
            }
        }
    }

    /// Delete file row + children. Caller must hold an open transaction when nesting.
    fn delete_file_record_inner(&self, file_id: i64) -> SqliteResult<()> {
        let mut stmt = self.conn.prepare(
            "SELECT file_id FROM SuspectedDuplicates
             WHERE keeper_id = ?1 AND resolution = 'pending'",
        )?;
        let orphan_ids: Vec<i64> = stmt
            .query_map(params![file_id], |row| row.get(0))?
            .collect::<SqliteResult<Vec<_>>>()?;
        drop(stmt);

        self.conn.execute(
            "DELETE FROM SuspectedDuplicates WHERE file_id = ?1 OR keeper_id = ?1",
            params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM FileTags WHERE file_id = ?1",
            params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM Metadata WHERE file_id = ?1",
            params![file_id],
        )?;
        self.conn
            .execute("DELETE FROM Files WHERE id = ?1", params![file_id])?;

        for orphan_id in orphan_ids {
            let status: String = self.conn.query_row(
                "SELECT status FROM Files WHERE id = ?1",
                params![orphan_id],
                |row| row.get(0),
            )?;
            if status == file_status::suspected_duplicate {
                if !file_status::can_transition(
                    file_status::suspected_duplicate,
                    file_status::ready,
                ) {
                    return Err(rusqlite::Error::InvalidParameterName(
                        "illegal status transition restoring orphan".into(),
                    ));
                }
                self.conn.execute(
                    "UPDATE Files SET status = ?1 WHERE id = ?2",
                    params![file_status::ready, orphan_id],
                )?;
            }
        }
        Ok(())
    }

    pub fn get_file_by_id(&self, id: i64) -> SqliteResult<FileRecord> {
        self.conn.query_row(
            "SELECT id, path, size, created_at, phash, status FROM Files WHERE id = ?1",
            params![id],
            |row| {
                Ok(FileRecord {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    size: row.get::<_, i64>(2)? as u64,
                    created_at: row.get(3)?,
                    phash: row.get(4)?,
                    status: row.get(5)?,
                })
            },
        )
    }

    pub fn get_file_by_path(&self, path: &str) -> SqliteResult<Option<FileRecord>> {
        let path = normalize_db_path(path);
        match self.conn.query_row(
            "SELECT id, path, size, created_at, phash, status FROM Files WHERE path = ?1",
            params![path],
            |row| {
                Ok(FileRecord {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    size: row.get::<_, i64>(2)? as u64,
                    created_at: row.get(3)?,
                    phash: row.get(4)?,
                    status: row.get(5)?,
                })
            },
        ) {
            Ok(file) => Ok(Some(file)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn update_metadata(&self, metadata: &Metadata) -> SqliteResult<()> {
        self.conn.execute(
            "INSERT INTO Metadata (file_id, title, studio, release_date, summary, cover_image, sprite_sheet_path, source_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(file_id) DO UPDATE SET
                title = ?2,
                studio = ?3,
                release_date = ?4,
                summary = ?5,
                cover_image = ?6,
                sprite_sheet_path = ?7,
                source_type = ?8",
            params![
                metadata.file_id,
                metadata.title,
                metadata.studio,
                metadata.release_date,
                metadata.summary,
                metadata.cover_image,
                metadata.sprite_sheet_path,
                metadata.source_type
            ],
        )?;
        Ok(())
    }

    pub fn get_metadata(&self, file_id: i64) -> SqliteResult<Option<Metadata>> {
        match self.conn.query_row(
            "SELECT file_id, title, studio, release_date, summary, cover_image, sprite_sheet_path, source_type
             FROM Metadata WHERE file_id = ?1",
            params![file_id],
            |row| {
                Ok(Metadata {
                    file_id: row.get(0)?,
                    title: row.get(1)?,
                    studio: row.get(2)?,
                    release_date: row.get(3)?,
                    summary: row.get(4)?,
                    cover_image: row.get(5)?,
                    sprite_sheet_path: row.get(6)?,
                    source_type: row.get(7)?,
                })
            },
        ) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn get_or_create_tag(&self, name: &str, category: Option<String>) -> SqliteResult<i64> {
        // 與 idx_tags_name_ifnull_category 同一 conflict target
        // SELECT → INSERT ON CONFLICT DO NOTHING RETURNING → 無列再 SELECT
        // 避免每次衝突都寫入並消耗 AUTOINCREMENT
        // Some("") 正規成 None 與遷移時 ''→NULL 一致
        let category = match category {
            Some(ref s) if s.is_empty() => None,
            other => other,
        };
        match self.conn.query_row(
            "SELECT id FROM Tags
             WHERE name = ?1 AND IFNULL(category, '') = IFNULL(?2, '')",
            params![name, category],
            |row| row.get(0),
        ) {
            Ok(id) => return Ok(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => return Err(e),
        }

        match self.conn.query_row(
            "INSERT INTO Tags (name, category) VALUES (?1, ?2)
             ON CONFLICT(name, IFNULL(category, '')) DO NOTHING
             RETURNING id",
            params![name, category],
            |row| row.get(0),
        ) {
            Ok(id) => Ok(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => self.conn.query_row(
                "SELECT id FROM Tags
                 WHERE name = ?1 AND IFNULL(category, '') = IFNULL(?2, '')",
                params![name, category],
                |row| row.get(0),
            ),
            Err(e) => Err(e),
        }
    }

    pub fn add_file_tag(&self, file_id: i64, tag_id: i64, confidence: f64) -> SqliteResult<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO FileTags (file_id, tag_id, confidence) VALUES (?1, ?2, ?3)",
            params![file_id, tag_id, confidence],
        )?;
        Ok(())
    }

    pub fn clear_file_tags(&self, file_id: i64) -> SqliteResult<()> {
        self.conn.execute(
            "DELETE FROM FileTags WHERE file_id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    /// 將刮削得到的 tags / actors 寫入 Tags + FileTags
    /// actors 使用 category Actor tags 使用 category Tag
    pub fn apply_scraped_tags(
        &self,
        file_id: i64,
        tags: &[String],
        actors: &[String],
    ) -> SqliteResult<()> {
        #[allow(non_upper_case_globals)]
        const tag_category: &str = "Tag";
        #[allow(non_upper_case_globals)]
        const actor_category: &str = "Actor";

        self.clear_file_tags(file_id)?;

        for name in tags {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                continue;
            }
            let tag_id = self.get_or_create_tag(trimmed, Some(tag_category.to_string()))?;
            self.add_file_tag(file_id, tag_id, 1.0)?;
        }

        for name in actors {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                continue;
            }
            let tag_id = self.get_or_create_tag(trimmed, Some(actor_category.to_string()))?;
            self.add_file_tag(file_id, tag_id, 1.0)?;
        }

        Ok(())
    }

    pub fn get_file_tags(&self, file_id: i64) -> SqliteResult<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.name, t.category
             FROM Tags t
             INNER JOIN FileTags ft ON t.id = ft.tag_id
             WHERE ft.file_id = ?1",
        )?;

        let tags = stmt.query_map(params![file_id], |row| {
            Ok(Tag {
                id: row.get(0)?,
                name: row.get(1)?,
                category: row.get(2)?,
            })
        })?;

        let mut result = Vec::new();
        for tag in tags {
            result.push(tag?);
        }
        Ok(result)
    }

    pub fn get_files_with_metadata(&self) -> SqliteResult<Vec<FileWithMetadata>> {
        // 列表時輕量核對磁碟路徑 缺檔 → missing 復現 → ready
        self.reconcile_missing_files()?;

        // One query for all pending suspicions / collisions (avoids N+1)
        let suspected_map = self.load_pending_suspected_map()?;
        let collision_map = self.load_pending_path_collision_map()?;

        let mut stmt = self.conn.prepare(
            "SELECT id, path, size, created_at, phash, status FROM Files ORDER BY created_at DESC",
        )?;

        let files = stmt.query_map([], |row| {
            Ok(FileRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                size: row.get::<_, i64>(2)? as u64,
                created_at: row.get(3)?,
                phash: row.get(4)?,
                status: row.get(5)?,
            })
        })?;

        let mut result = Vec::new();
        for file in files {
            let file = file?;
            let metadata = self.get_metadata(file.id)?;
            let tags = self.get_file_tags(file.id)?;
            let suspected_duplicate = suspected_map.get(&file.id).cloned();
            let path_collision = collision_map.get(&file.id).cloned();
            result.push(FileWithMetadata {
                file,
                metadata,
                tags,
                suspected_duplicate,
                path_collision,
            });
        }
        Ok(result)
    }

    fn load_pending_suspected_map(
        &self,
    ) -> SqliteResult<std::collections::HashMap<i64, SuspectedDuplicateInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.file_id, s.keeper_id, s.distance, k.path
             FROM SuspectedDuplicates s
             INNER JOIN Files k ON k.id = s.keeper_id
             WHERE s.resolution = 'pending'",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                SuspectedDuplicateInfo {
                    keeper_id: row.get(1)?,
                    distance: row.get::<_, i64>(2)? as u32,
                    keeper_path: row.get(3)?,
                },
            ))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (file_id, info) = row?;
            map.insert(file_id, info);
        }
        Ok(map)
    }

    fn load_pending_path_collision_map(
        &self,
    ) -> SqliteResult<std::collections::HashMap<i64, PathCollisionInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_id, desired_path, existing_path, occupant_file_id
             FROM PathCollisions
             WHERE resolution = 'pending'",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                PathCollisionInfo {
                    desired_path: row.get(1)?,
                    occupant_file_id: row.get(3)?,
                },
            ))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (file_id, info) = row?;
            map.insert(file_id, info);
        }
        Ok(map)
    }

    /// Persist a scrape / ffmpeg / move failure for the error log UI
    pub fn insert_error_log(
        &self,
        category: &str,
        file_id: Option<i64>,
        path: Option<&str>,
        message: &str,
        detail: Option<&str>,
    ) -> SqliteResult<i64> {
        self.conn.execute(
            "INSERT INTO ErrorLog (category, file_id, path, message, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![category, file_id, path, message, detail],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// True when an ErrorLog row already exists for this category+path+message (S2)
    pub fn has_error_log_entry(
        &self,
        category: &str,
        path: Option<&str>,
        message: &str,
    ) -> SqliteResult<bool> {
        let flag: i64 = self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM ErrorLog
                WHERE category = ?1
                  AND IFNULL(path, '') = IFNULL(?2, '')
                  AND message = ?3
            )",
            params![category, path, message],
            |row| row.get(0),
        )?;
        Ok(flag != 0)
    }

    /// Insert a queued job row; returns new `Jobs.id`
    pub fn insert_job(&self, row: &NewJobRow<'_>) -> SqliteResult<i64> {
        self.conn.execute(
            "INSERT INTO Jobs (
                status, type, payload, attempts, max_attempts, error, retryable
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.status,
                row.job_type,
                row.payload,
                row.attempts as i64,
                row.max_attempts as i64,
                row.error,
                if row.retryable { 1i64 } else { 0i64 },
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Persist status / attempts / error for an existing job
    pub fn update_job_state(
        &self,
        id: i64,
        status: &str,
        attempts: u32,
        error: Option<&str>,
        retryable: bool,
    ) -> SqliteResult<()> {
        let n = self.conn.execute(
            "UPDATE Jobs
             SET status = ?1,
                 attempts = ?2,
                 error = ?3,
                 retryable = ?4,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?5",
            params![
                status,
                attempts as i64,
                error,
                if retryable { 1i64 } else { 0i64 },
                id,
            ],
        )?;
        if n == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    /// Crash recovery: any `running` row becomes `queued` again
    pub fn reset_running_jobs_to_queued(&self) -> SqliteResult<usize> {
        let n = self.conn.execute(
            "UPDATE Jobs
             SET status = 'queued',
                 updated_at = CURRENT_TIMESTAMP
             WHERE status = 'running'",
            [],
        )?;
        Ok(n)
    }

    /// Mark a job failed (malformed payload / exhausted retries)
    pub fn mark_job_failed(
        &self,
        id: i64,
        attempts: u32,
        error: &str,
        retryable: bool,
    ) -> SqliteResult<()> {
        self.update_job_state(id, "failed", attempts, Some(error), retryable)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn get_job(&self, id: i64) -> SqliteResult<JobRow> {
        self.conn.query_row(
            "SELECT id, status, type, payload, attempts, max_attempts,
                    error, retryable, created_at, updated_at
             FROM Jobs WHERE id = ?1",
            params![id],
            Self::map_job_row,
        )
    }

    pub fn list_jobs_with_status(&self, statuses: &[&str]) -> SqliteResult<Vec<JobRow>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = statuses
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, status, type, payload, attempts, max_attempts,
                    error, retryable, created_at, updated_at
             FROM Jobs
             WHERE status IN ({placeholders})
             ORDER BY id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows =
            stmt.query_map(rusqlite::params_from_iter(statuses.iter()), Self::map_job_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn delete_jobs_by_ids(&self, ids: &[i64]) -> SqliteResult<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let placeholders = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("DELETE FROM Jobs WHERE id IN ({placeholders})");
        let mut stmt = self.conn.prepare(&sql)?;
        stmt.execute(rusqlite::params_from_iter(ids.iter()))
    }

    /// Keep newest terminal jobs; delete older succeeded/failed/cancelled rows
    pub fn prune_terminal_jobs(&self, keep: usize) -> SqliteResult<usize> {
        let keep = keep.max(1) as i64;
        let n = self.conn.execute(
            "DELETE FROM Jobs
             WHERE status IN ('succeeded', 'failed', 'cancelled')
               AND id NOT IN (
                   SELECT id FROM Jobs
                   WHERE status IN ('succeeded', 'failed', 'cancelled')
                   ORDER BY id DESC
                   LIMIT ?1
               )",
            params![keep],
        )?;
        Ok(n)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn jobs_table_exists(&self) -> SqliteResult<bool> {
        self.table_exists("Jobs")
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn suspected_duplicates_table_exists(&self) -> SqliteResult<bool> {
        self.table_exists("SuspectedDuplicates")
    }

    fn map_job_row(row: &rusqlite::Row<'_>) -> SqliteResult<JobRow> {
        let retryable_i: i64 = row.get(7)?;
        Ok(JobRow {
            id: row.get(0)?,
            status: row.get(1)?,
            job_type: row.get(2)?,
            payload: row.get(3)?,
            attempts: row.get::<_, i64>(4)? as u32,
            max_attempts: row.get::<_, i64>(5)? as u32,
            error: row.get(6)?,
            retryable: retryable_i != 0,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
        })
    }

    pub fn list_error_logs(&self, limit: i64) -> SqliteResult<Vec<ErrorLogEntry>> {
        let limit = limit.clamp(1, 500);
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, category, file_id, path, message, detail
             FROM ErrorLog
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(ErrorLogEntry {
                id: row.get(0)?,
                created_at: row.get(1)?,
                category: row.get(2)?,
                file_id: row.get(3)?,
                path: row.get(4)?,
                message: row.get(5)?,
                detail: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn clear_error_logs(&self) -> SqliteResult<usize> {
        let n = self.conn.execute("DELETE FROM ErrorLog", [])?;
        Ok(n)
    }

    /// File ids eligible for one-click / batch organize (not missing / suspected / collision)
    pub fn list_organizable_file_ids(&self) -> SqliteResult<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM Files
             WHERE status != ?1 AND status != ?2 AND status != ?3
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(
            params![
                file_status::missing,
                file_status::suspected_duplicate,
                file_status::path_collision
            ],
            |row| row.get(0),
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// #73: create a batch header (status typically `executing`)
    ///
    /// Production execute path uses [`Self::create_organize_batch_with_items`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_organize_batch(&self, status: &str, note: Option<&str>) -> SqliteResult<i64> {
        self.conn.execute(
            "INSERT INTO OrganizeBatches (status, note) VALUES (?1, ?2)",
            params![status, note],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// #73: insert batch header + all items in one BEGIN IMMEDIATE transaction
    pub fn create_organize_batch_with_items(
        &self,
        status: &str,
        note: Option<&str>,
        items: &[NewOrganizeBatchItem<'_>],
    ) -> SqliteResult<i64> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let tx_result = (|| -> SqliteResult<i64> {
            self.conn.execute(
                "INSERT INTO OrganizeBatches (status, note) VALUES (?1, ?2)",
                params![status, note],
            )?;
            let batch_id = self.conn.last_insert_rowid();
            for item in items {
                self.insert_organize_batch_item(batch_id, item)?;
            }
            self.conn.execute("COMMIT", [])?;
            Ok(batch_id)
        })();
        match tx_result {
            Ok(id) => Ok(id),
            Err(e) => {
                if let Err(rb) = self.conn.execute("ROLLBACK", []) {
                    eprintln!(
                        "warn: ROLLBACK failed after create_organize_batch_with_items: {rb}"
                    );
                }
                Err(e)
            }
        }
    }

    pub fn set_organize_batch_status(&self, batch_id: i64, status: &str) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE OrganizeBatches SET status = ?1 WHERE id = ?2",
            params![status, batch_id],
        )?;
        Ok(())
    }

    pub fn get_organize_batch(&self, batch_id: i64) -> SqliteResult<OrganizeBatch> {
        self.conn.query_row(
            "SELECT id, created_at, status, note FROM OrganizeBatches WHERE id = ?1",
            params![batch_id],
            |row| {
                Ok(OrganizeBatch {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    status: row.get(2)?,
                    note: row.get(3)?,
                })
            },
        )
    }

    /// Latest batch that still has `moved` or retryable `undo_refused` items
    pub fn latest_undoable_organize_batch(&self) -> SqliteResult<Option<OrganizeBatch>> {
        let mut stmt = self.conn.prepare(
            "SELECT b.id, b.created_at, b.status, b.note
             FROM OrganizeBatches b
             WHERE EXISTS (
                 SELECT 1 FROM OrganizeBatchItems i
                 WHERE i.batch_id = b.id
                   AND i.status IN ('moved', 'undo_refused')
             )
             ORDER BY b.id DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(Some(OrganizeBatch {
                id: row.get(0)?,
                created_at: row.get(1)?,
                status: row.get(2)?,
                note: row.get(3)?,
            })),
            None => Ok(None),
        }
    }

    pub fn insert_organize_batch_item(
        &self,
        batch_id: i64,
        item: &NewOrganizeBatchItem<'_>,
    ) -> SqliteResult<i64> {
        let src = if item.src_path.trim().is_empty() {
            format!("(unknown file_id={})", item.file_id)
        } else {
            normalize_db_path(item.src_path)
        };
        let dst = if item.dst_path.trim().is_empty() {
            src.clone()
        } else {
            normalize_db_path(item.dst_path)
        };
        self.conn.execute(
            "INSERT INTO OrganizeBatchItems (
                batch_id, file_id, src_path, dst_path, size, phash, status, collision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                batch_id,
                item.file_id,
                src,
                dst,
                item.size as i64,
                item.phash,
                item.status,
                item.collision
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_organize_batch_item_status(
        &self,
        item_id: i64,
        status: &str,
        error: Option<&str>,
    ) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE OrganizeBatchItems SET status = ?1, error = ?2 WHERE id = ?3",
            params![status, error, item_id],
        )?;
        Ok(())
    }

    pub fn list_organize_batch_items(
        &self,
        batch_id: i64,
    ) -> SqliteResult<Vec<OrganizeBatchItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, batch_id, file_id, src_path, dst_path, size, phash,
                    status, collision, error
             FROM OrganizeBatchItems
             WHERE batch_id = ?1
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![batch_id], |row| {
            Ok(OrganizeBatchItem {
                id: row.get(0)?,
                batch_id: row.get(1)?,
                file_id: row.get(2)?,
                src_path: row.get(3)?,
                dst_path: row.get(4)?,
                size: row.get::<_, i64>(5)? as u64,
                phash: row.get(6)?,
                status: row.get(7)?,
                collision: row.get(8)?,
                error: row.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// After pending_move recovery: promote `moving` items whose Files.path already
    /// matches dst, or mark failed when still at src with no pending intent.
    pub fn reconcile_organize_batch_items_after_recovery(&self) -> SqliteResult<usize> {
        let mut stmt = self.conn.prepare(
            "SELECT i.id, i.file_id, i.src_path, i.dst_path, i.status,
                    f.path, f.pending_move_to
             FROM OrganizeBatchItems i
             JOIN Files f ON f.id = i.file_id
             WHERE i.status IN ('moving', 'undoing')",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut updated = 0usize;
        let collected: Vec<_> = rows.collect::<SqliteResult<Vec<_>>>()?;
        for (item_id, _file_id, src, dst, item_status, file_path, pending) in collected {
            let path_n = normalize_db_path(&file_path);
            let src_n = normalize_db_path(&src);
            let dst_n = normalize_db_path(&dst);
            let pending_set = pending
                .as_ref()
                .map(|p| !p.trim().is_empty())
                .unwrap_or(false);
            if item_status == "moving" {
                if path_n == dst_n {
                    self.update_organize_batch_item_status(item_id, "moved", None)?;
                    updated += 1;
                } else if path_n == src_n && !pending_set {
                    self.update_organize_batch_item_status(
                        item_id,
                        "failed",
                        Some("interrupted before filesystem move completed"),
                    )?;
                    updated += 1;
                }
            } else if item_status == "undoing" {
                if path_n == src_n {
                    self.update_organize_batch_item_status(item_id, "undone", None)?;
                    updated += 1;
                } else if path_n == dst_n && !pending_set {
                    self.update_organize_batch_item_status(
                        item_id,
                        "undo_refused",
                        Some("interrupted before undo filesystem move completed"),
                    )?;
                    updated += 1;
                }
            }
        }
        self.refresh_organize_batch_statuses()?;
        Ok(updated)
    }

    /// Recompute batch header status from item rows
    pub fn refresh_organize_batch_statuses(&self) -> SqliteResult<()> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM OrganizeBatches
             WHERE status IN ('executing', 'undoing', 'completed', 'partially_undone')",
        )?;
        let ids: Vec<i64> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<SqliteResult<Vec<_>>>()?;
        for batch_id in ids {
            let items = self.list_organize_batch_items(batch_id)?;
            if items.is_empty() {
                continue;
            }
            let any_moving = items.iter().any(|i| i.status == "moving");
            let any_undoing = items.iter().any(|i| i.status == "undoing");
            let moved = items.iter().filter(|i| i.status == "moved").count();
            let undone = items.iter().filter(|i| i.status == "undone").count();
            let undo_refused = items.iter().filter(|i| i.status == "undo_refused").count();
            let next = if any_moving {
                "executing"
            } else if any_undoing {
                "undoing"
            } else if moved > 0 && (undone > 0 || undo_refused > 0) {
                "partially_undone"
            } else if moved == 0 && undo_refused > 0 {
                // All remaining targets refused — still retryable via undo
                "partially_undone"
            } else if moved == 0 && undone > 0 {
                "undone"
            } else {
                "completed"
            };
            self.set_organize_batch_status(batch_id, next)?;
        }
        Ok(())
    }

    pub fn update_file_status(&self, file_id: i64, status: &str) -> SqliteResult<()> {
        if !file_status::is_known(status) {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "unknown file status: {}",
                status
            )));
        }
        let current = self.get_file_by_id(file_id)?.status;
        if !file_status::can_transition(&current, status) {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "illegal status transition: {} -> {}",
                current, status
            )));
        }
        self.conn.execute(
            "UPDATE Files SET status = ?1 WHERE id = ?2",
            params![status, file_id],
        )?;
        Ok(())
    }

    /// 依磁碟路徑存在與否調整 missing / ready
    /// 不覆寫 processing 或 scrape_failed 除非路徑已消失
    pub fn reconcile_missing_files(&self) -> SqliteResult<usize> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, status FROM Files",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        let mut changed = 0usize;
        for row in rows {
            let (id, path, status) = row?;
            let exists = Path::new(&path).exists();
            if !exists && status != file_status::missing {
                self.update_file_status(id, file_status::missing)?;
                changed += 1;
            } else if exists && status == file_status::missing {
                self.update_file_status(id, file_status::ready)?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    pub fn update_file_phash(&self, file_id: i64, phash: &str) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE Files SET phash = ?1 WHERE id = ?2",
            params![phash, file_id],
        )?;
        Ok(())
    }

    /// Test / recovery helper: rewrite `Files.path` and clear `pending_move_to`.
    /// Production moves use `heal_moved_path` / `commit_path_collision_overwrite`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn update_file_path(&self, file_id: i64, new_path: &str) -> SqliteResult<()> {
        let new_path = normalize_db_path(new_path);
        // Path rewrite outside move commit also drops stale pending intent (#66)
        self.conn.execute(
            "UPDATE Files SET path = ?1,
                 pending_move_to = NULL,
                 pending_source_vol = NULL,
                 pending_source_idx = NULL
             WHERE id = ?2",
            params![new_path, file_id],
        )?;
        Ok(())
    }

    /// 刮削成功後在同一交易寫入 metadata tags、status 與 organized_at
    pub fn persist_scrape_success(
        &self,
        metadata: &Metadata,
        tags: &[String],
        actors: &[String],
        status: &str,
    ) -> SqliteResult<Vec<Tag>> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let outcome = (|| -> SqliteResult<Vec<Tag>> {
            self.update_metadata(metadata)?;
            self.apply_scraped_tags(metadata.file_id, tags, actors)?;
            self.update_file_status(metadata.file_id, status)?;
            self.mark_file_organized(metadata.file_id)?;
            self.get_file_tags(metadata.file_id)
        })();
        match outcome {
            Ok(saved_tags) => {
                self.conn.execute("COMMIT", [])?;
                Ok(saved_tags)
            }
            Err(e) => {
                let _ = self.conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }
}

/// Test-only fault injection after overwrite FS success / before COMMIT
#[cfg(any(test, feature = "test-support"))]
pub mod path_collision_tx_fault {
    use std::cell::Cell;

    thread_local! {
        #[allow(non_upper_case_globals)]
        static fail_commit: Cell<bool> = const { Cell::new(false) };
    }

    pub fn arm_fail_commit() {
        fail_commit.with(|f| f.set(true));
    }

    pub fn clear() {
        fail_commit.with(|f| f.set(false));
    }

    pub fn should_fail_commit() -> bool {
        fail_commit.with(|f| {
            if f.get() {
                f.set(false);
                true
            } else {
                false
            }
        })
    }
}

#[cfg(test)]
mod migrate_tags_fault {
    use std::cell::Cell;

    thread_local! {
        static FAULT_AFTER_INSERT: Cell<bool> = const { Cell::new(false) };
    }

    pub fn arm_after_insert() {
        FAULT_AFTER_INSERT.with(|f| f.set(true));
    }

    pub fn clear() {
        FAULT_AFTER_INSERT.with(|f| f.set(false));
    }

    pub fn should_fail_after_insert() -> bool {
        FAULT_AFTER_INSERT.with(|f| {
            if f.get() {
                f.set(false);
                true
            } else {
                false
            }
        })
    }
}

#[cfg(test)]
mod migrate_null_tags_fault {
    use std::cell::Cell;

    thread_local! {
        static FAULT_AFTER_MERGE: Cell<bool> = const { Cell::new(false) };
    }

    pub fn arm_after_merge() {
        FAULT_AFTER_MERGE.with(|f| f.set(true));
    }

    pub fn clear() {
        FAULT_AFTER_MERGE.with(|f| f.set(false));
    }

    pub fn should_fail_after_merge() -> bool {
        FAULT_AFTER_MERGE.with(|f| {
            if f.get() {
                f.set(false);
                true
            } else {
                false
            }
        })
    }
}

#[cfg(test)]
mod swap_tags_fault {
    use std::cell::Cell;

    thread_local! {
        static FAULT_BEGIN: Cell<bool> = const { Cell::new(false) };
    }

    pub fn arm_begin() {
        FAULT_BEGIN.with(|f| f.set(true));
    }

    pub fn clear() {
        FAULT_BEGIN.with(|f| f.set(false));
    }

    pub fn should_fail_begin() -> bool {
        FAULT_BEGIN.with(|f| {
            if f.get() {
                f.set(false);
                true
            } else {
                false
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn open_memory_db() -> Database {
        Database::new(":memory:").expect("open memory db")
    }

    fn foreign_keys_enabled(db: &Database) -> i64 {
        db.conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("pragma foreign_keys")
    }

    fn ensure_temp_tests_dir() -> PathBuf {
        // Join components so Windows does not keep literal `/` in the path string
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(".temp")
            .join("tests");
        std::fs::create_dir_all(&dir).expect("mkdir .temp/tests");
        dir
    }

    fn temp_db_path(label: &str) -> PathBuf {
        ensure_temp_tests_dir().join(format!(
            "tags-mig-{label}-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    fn seed_legacy_tags_schema(db: &Database, tag_id: i64, name: &str, category: &str) {
        db.conn
            .execute_batch(&format!(
                "
                DROP TABLE IF EXISTS Tags;
                CREATE TABLE Tags (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT UNIQUE NOT NULL,
                    category TEXT
                );
                INSERT INTO Tags (id, name, category) VALUES ({tag_id}, '{name}', '{category}');
                "
            ))
            .expect("seed legacy schema");
    }

    /// 不經 Database::new 寫入舊 schema 檔案 供真實啟動測試
    fn write_legacy_tags_file_db(path: &Path, tag_id: i64, name: &str, category: &str) {
        let conn = Connection::open(path).expect("open raw db");
        conn.execute_batch(
            "
            CREATE TABLE Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT DEFAULT 'ready'
            );
            CREATE TABLE Metadata (
                file_id INTEGER PRIMARY KEY,
                title TEXT,
                studio TEXT,
                release_date TEXT,
                summary TEXT,
                cover_image TEXT,
                sprite_sheet_path TEXT,
                source_type TEXT,
                FOREIGN KEY(file_id) REFERENCES Files(id)
            );
            CREATE TABLE Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                category TEXT
            );
            CREATE TABLE FileTags (
                file_id INTEGER,
                tag_id INTEGER,
                confidence FLOAT DEFAULT 1.0,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            );
            ",
        )
        .expect("create legacy tables");
        conn.execute(
            "INSERT INTO Files (id, path, size) VALUES (1, '/tmp/legacy-startup.mp4', 10)",
            [],
        )
        .expect("insert file");
        conn.execute(
            "INSERT INTO Tags (id, name, category) VALUES (?1, ?2, ?3)",
            params![tag_id, name, category],
        )
        .expect("insert tag");
        conn.execute(
            "INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (1, ?1, 1.0)",
            params![tag_id],
        )
        .expect("insert filetag");
    }

    /// Schema v3 形狀 DB（有 Jobs 無 SuspectedDuplicates）
    fn write_v3_shaped_file_db(path: &Path) {
        let conn = Connection::open(path).expect("open raw db");
        conn.execute_batch(
            "
            CREATE TABLE Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT DEFAULT 'ready'
            );
            CREATE TABLE Metadata (
                file_id INTEGER PRIMARY KEY,
                title TEXT,
                studio TEXT,
                release_date TEXT,
                summary TEXT,
                cover_image TEXT,
                sprite_sheet_path TEXT,
                source_type TEXT,
                FOREIGN KEY(file_id) REFERENCES Files(id)
            );
            CREATE TABLE Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT,
                UNIQUE(name, category)
            );
            CREATE UNIQUE INDEX idx_tags_name_ifnull_category
                ON Tags (name, IFNULL(category, ''));
            CREATE TABLE FileTags (
                file_id INTEGER,
                tag_id INTEGER,
                confidence FLOAT DEFAULT 1.0,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            );
            CREATE TABLE ErrorLog (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                category TEXT NOT NULL,
                file_id INTEGER,
                path TEXT,
                message TEXT NOT NULL,
                detail TEXT
            );
            CREATE TABLE Jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                status TEXT NOT NULL CHECK (
                    status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')
                ),
                type TEXT NOT NULL CHECK (type IN ('pipeline', 'phash')),
                payload TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                max_attempts INTEGER NOT NULL DEFAULT 3,
                error TEXT,
                retryable INTEGER NOT NULL DEFAULT 1 CHECK (retryable IN (0, 1)),
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO Files (id, path, size) VALUES (1, '/tmp/v3-upgrade.mp4', 10);
            PRAGMA user_version = 3;
            ",
        )
        .expect("seed v3 shaped db");
    }

    /// Schema v2 形狀 DB（有 nullsafe Tags 索引 無 Jobs 表）
    fn write_v2_shaped_file_db(path: &Path) {
        let conn = Connection::open(path).expect("open raw db");
        conn.execute_batch(
            "
            CREATE TABLE Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT DEFAULT 'ready'
            );
            CREATE TABLE Metadata (
                file_id INTEGER PRIMARY KEY,
                title TEXT,
                studio TEXT,
                release_date TEXT,
                summary TEXT,
                cover_image TEXT,
                sprite_sheet_path TEXT,
                source_type TEXT,
                FOREIGN KEY(file_id) REFERENCES Files(id)
            );
            CREATE TABLE Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT,
                UNIQUE(name, category)
            );
            CREATE UNIQUE INDEX idx_tags_name_ifnull_category
                ON Tags (name, IFNULL(category, ''));
            CREATE TABLE FileTags (
                file_id INTEGER,
                tag_id INTEGER,
                confidence FLOAT DEFAULT 1.0,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            );
            CREATE TABLE ErrorLog (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                category TEXT NOT NULL,
                file_id INTEGER,
                path TEXT,
                message TEXT NOT NULL,
                detail TEXT
            );
            INSERT INTO Files (id, path, size) VALUES (1, '/tmp/v2-upgrade.mp4', 10);
            PRAGMA user_version = 2;
            ",
        )
        .expect("seed v2 shaped db");
    }

    /// v0.1.0 形狀 DB user_version=0 且已是 UNIQUE(name, category) 無 nullsafe 索引
    fn write_v010_shaped_file_db(path: &Path) {
        let conn = Connection::open(path).expect("open raw db");
        conn.execute_batch(
            "
            CREATE TABLE Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT DEFAULT 'ready'
            );
            CREATE TABLE Metadata (
                file_id INTEGER PRIMARY KEY,
                title TEXT,
                studio TEXT,
                release_date TEXT,
                summary TEXT,
                cover_image TEXT,
                sprite_sheet_path TEXT,
                source_type TEXT,
                FOREIGN KEY(file_id) REFERENCES Files(id)
            );
            CREATE TABLE Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT,
                UNIQUE(name, category)
            );
            CREATE TABLE FileTags (
                file_id INTEGER,
                tag_id INTEGER,
                confidence FLOAT DEFAULT 1.0,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            );
            CREATE TABLE ErrorLog (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                category TEXT NOT NULL,
                file_id INTEGER,
                path TEXT,
                message TEXT NOT NULL,
                detail TEXT
            );
            INSERT INTO Files (id, path, size) VALUES (1, '/tmp/v010.mp4', 10);
            INSERT INTO Tags (id, name, category) VALUES (1, 'Keep', 'Tag');
            INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (1, 1, 1.0);
            PRAGMA user_version = 0;
            ",
        )
        .expect("seed v0.1.0 shaped db");
    }

    /// 含重複 NULL-category tags 的舊 DB（表約束允許 NULL 互異）
    fn write_duplicate_null_category_tags_db(path: &Path) {
        let conn = Connection::open(path).expect("open raw db");
        conn.execute_batch(
            "
            CREATE TABLE Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT DEFAULT 'ready'
            );
            CREATE TABLE Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT,
                UNIQUE(name, category)
            );
            CREATE TABLE FileTags (
                file_id INTEGER,
                tag_id INTEGER,
                confidence FLOAT DEFAULT 1.0,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            );
            CREATE TABLE ErrorLog (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                category TEXT NOT NULL,
                file_id INTEGER,
                path TEXT,
                message TEXT NOT NULL,
                detail TEXT
            );
            INSERT INTO Files (id, path, size) VALUES
                (1, '/tmp/dup-a.mp4', 10),
                (2, '/tmp/dup-b.mp4', 20);
            INSERT INTO Tags (id, name, category) VALUES
                (10, 'Dup', NULL),
                (11, 'Dup', NULL),
                (12, 'Dup', NULL),
                (20, 'Other', 'Tag');
            INSERT INTO FileTags (file_id, tag_id, confidence) VALUES
                (1, 10, 0.3),
                (1, 11, 0.9),
                (2, 11, 0.5),
                (2, 12, 0.8);
            PRAGMA user_version = 0;
            ",
        )
        .expect("seed duplicate null tags db");
    }

    /// 同名 category='' 與 NULL 共存 會撞 IFNULL 唯一鍵
    fn write_empty_string_and_null_category_db(path: &Path) {
        let conn = Connection::open(path).expect("open raw db");
        conn.execute_batch(
            "
            CREATE TABLE Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT DEFAULT 'ready'
            );
            CREATE TABLE Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT,
                UNIQUE(name, category)
            );
            CREATE TABLE FileTags (
                file_id INTEGER,
                tag_id INTEGER,
                confidence FLOAT DEFAULT 1.0,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            );
            CREATE TABLE ErrorLog (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                category TEXT NOT NULL,
                file_id INTEGER,
                path TEXT,
                message TEXT NOT NULL,
                detail TEXT
            );
            INSERT INTO Files (id, path, size) VALUES
                (1, '/tmp/empty-null-a.mp4', 10),
                (2, '/tmp/empty-null-b.mp4', 20);
            INSERT INTO Tags (id, name, category) VALUES
                (1, 'Clash', ''),
                (2, 'Clash', NULL),
                (3, 'Solo', '');
            INSERT INTO FileTags (file_id, tag_id, confidence) VALUES
                (1, 1, 0.4),
                (1, 2, 0.7),
                (2, 2, 1.0),
                (2, 3, 1.0);
            PRAGMA user_version = 0;
            ",
        )
        .expect("seed empty-string and null category db");
    }

    /// pre-#20 legacy Tags（name 唯一）加上殘留 Tags_mig
    fn write_pre20_with_tags_mig_file_db(path: &Path) {
        let conn = Connection::open(path).expect("open raw db");
        conn.execute_batch(
            "
            CREATE TABLE Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT DEFAULT 'ready'
            );
            CREATE TABLE Metadata (
                file_id INTEGER PRIMARY KEY,
                title TEXT,
                studio TEXT,
                release_date TEXT,
                summary TEXT,
                cover_image TEXT,
                sprite_sheet_path TEXT,
                source_type TEXT,
                FOREIGN KEY(file_id) REFERENCES Files(id)
            );
            CREATE TABLE Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                category TEXT
            );
            CREATE TABLE Tags_mig (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT,
                UNIQUE(name, category)
            );
            CREATE TABLE FileTags (
                file_id INTEGER,
                tag_id INTEGER,
                confidence FLOAT DEFAULT 1.0,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            );
            CREATE TABLE ErrorLog (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                category TEXT NOT NULL,
                file_id INTEGER,
                path TEXT,
                message TEXT NOT NULL,
                detail TEXT
            );
            INSERT INTO Files (id, path, size) VALUES (1, '/tmp/pre20-mig.mp4', 10);
            INSERT INTO Tags (id, name, category) VALUES (7, 'Legacy', 'Tag');
            INSERT INTO Tags_mig (id, name, category) VALUES (99, 'Stale', 'Tag');
            INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (1, 7, 1.0);
            PRAGMA user_version = 0;
            ",
        )
        .expect("seed pre-#20 with Tags_mig");
    }

    fn assert_no_foreign_key_check_rows(db: &Database) {
        let mut stmt = db
            .conn
            .prepare("PRAGMA foreign_key_check")
            .expect("fk check prepare");
        let mut rows = stmt.query([]).expect("fk check query");
        assert!(
            rows.next().expect("fk check next").is_none(),
            "expected no foreign_key_check rows after merge"
        );
    }

    fn filetag_confidence(db: &Database, file_id: i64, tag_id: i64) -> f64 {
        db.conn
            .query_row(
                "SELECT confidence FROM FileTags WHERE file_id = ?1 AND tag_id = ?2",
                params![file_id, tag_id],
                |row| row.get(0),
            )
            .expect("filetag confidence")
    }

    fn raw_user_version(path: &Path) -> i32 {
        let conn = Connection::open(path).expect("open raw");
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version")
    }

    fn raw_null_tag_count(path: &Path, name: &str) -> i64 {
        let conn = Connection::open(path).expect("open raw");
        conn.query_row(
            "SELECT COUNT(*) FROM Tags WHERE name = ?1 AND category IS NULL",
            params![name],
            |row| row.get(0),
        )
        .expect("count")
    }

    fn tag_row(db: &Database, id: i64) -> Tag {
        db.conn
            .query_row(
                "SELECT id, name, category FROM Tags WHERE id = ?1",
                params![id],
                |row| {
                    Ok(Tag {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        category: row.get(2)?,
                    })
                },
            )
            .expect("tag row")
    }

    fn tags_table_sql(db: &Database) -> String {
        db.conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'Tags'",
                [],
                |row| row.get(0),
            )
            .expect("Tags sql")
    }

    #[test]
    fn apply_scraped_tags_writes_tags_and_actors() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/sample.mp4", 1024, None)
            .expect("insert file");

        db.apply_scraped_tags(
            file_id,
            &["Outdoor".to_string(), "Beach".to_string()],
            &["Alice".to_string()],
        )
        .expect("apply tags");

        let tags = db.get_file_tags(file_id).expect("get tags");
        assert_eq!(tags.len(), 3);

        let outdoor = tags.iter().find(|t| t.name == "Outdoor").expect("Outdoor");
        assert_eq!(outdoor.category.as_deref(), Some("Tag"));

        let alice = tags.iter().find(|t| t.name == "Alice").expect("Alice");
        assert_eq!(alice.category.as_deref(), Some("Actor"));
    }

    #[test]
    fn get_or_create_tag_same_name_different_category_are_distinct() {
        let db = open_memory_db();
        let tag_id = db
            .get_or_create_tag("Alice", Some("Tag".to_string()))
            .expect("create Tag");
        let actor_id = db
            .get_or_create_tag("Alice", Some("Actor".to_string()))
            .expect("create Actor");

        assert_ne!(tag_id, actor_id);
        assert_eq!(tag_row(&db, tag_id).category.as_deref(), Some("Tag"));
        assert_eq!(tag_row(&db, actor_id).category.as_deref(), Some("Actor"));
    }

    #[test]
    fn get_or_create_tag_same_name_and_category_returns_same_id() {
        let db = open_memory_db();
        let first = db
            .get_or_create_tag("Beach", Some("Tag".to_string()))
            .expect("first");
        let second = db
            .get_or_create_tag("Beach", Some("Tag".to_string()))
            .expect("second");
        assert_eq!(first, second);
    }

    #[test]
    fn get_or_create_tag_repeated_calls_return_same_id() {
        let db = open_memory_db();
        let ids: Vec<i64> = (0..16)
            .map(|_| {
                db.get_or_create_tag("Repeated", Some("Tag".to_string()))
                    .expect("upsert")
            })
            .collect();
        assert!(ids.iter().all(|id| *id == ids[0]));
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM Tags WHERE name = ?1 AND category = ?2",
                params!["Repeated", "Tag"],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 1);
    }

    #[test]
    fn get_or_create_tag_concurrent_threads_return_same_id() {
        let path = temp_db_path("concurrent-got");
        {
            let db = Database::new(path.to_str().expect("utf8")).expect("init db");
            db.conn
                .busy_timeout(std::time::Duration::from_secs(10))
                .expect("busy_timeout");
        }

        let thread_count = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(thread_count));
        let path = std::sync::Arc::new(path);
        let handles: Vec<_> = (0..thread_count)
            .map(|_| {
                let path = std::sync::Arc::clone(&path);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let db = Database::new(path.to_str().expect("utf8")).expect("open db");
                    db.conn
                        .busy_timeout(std::time::Duration::from_secs(10))
                        .expect("busy_timeout");
                    barrier.wait();
                    db.get_or_create_tag("Race", Some("Tag".to_string()))
                })
            })
            .collect();

        let ids: Vec<i64> = handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("thread join")
                    .expect("no UNIQUE error")
            })
            .collect();
        assert!(ids.iter().all(|id| *id == ids[0]));

        let db = Database::new(path.to_str().expect("utf8")).expect("reopen");
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM Tags WHERE name = ?1 AND category = ?2",
                params!["Race", "Tag"],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 1);
        let _ = std::fs::remove_file(path.as_path());
    }

    #[test]
    fn bundled_sqlite_supports_returning() {
        // libsqlite3-sys 0.28 bundled 為 3.39.4 RETURNING 需 ≥ 3.35.0（3035000）
        assert!(
            rusqlite::version_number() >= 3_035_000,
            "bundled SQLite {} lacks RETURNING",
            rusqlite::version()
        );
    }

    #[test]
    fn get_or_create_tag_null_category_returns_same_id() {
        let db = open_memory_db();
        let first = db.get_or_create_tag("Uncategorized", None).expect("first");
        let second = db.get_or_create_tag("Uncategorized", None).expect("second");
        let as_tag = db
            .get_or_create_tag("Uncategorized", Some("Tag".to_string()))
            .expect("as Tag");

        assert_eq!(first, second);
        assert_ne!(first, as_tag);
        assert!(tag_row(&db, first).category.is_none());
        assert_eq!(tag_row(&db, as_tag).category.as_deref(), Some("Tag"));
    }

    #[test]
    fn get_or_create_tag_null_category_concurrent_threads_return_same_id() {
        let path = temp_db_path("concurrent-null-got");
        {
            let db = Database::new(path.to_str().expect("utf8")).expect("init db");
            db.conn
                .busy_timeout(std::time::Duration::from_secs(10))
                .expect("busy_timeout");
        }

        let thread_count = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(thread_count));
        let path = std::sync::Arc::new(path);
        let handles: Vec<_> = (0..thread_count)
            .map(|_| {
                let path = std::sync::Arc::clone(&path);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let db = Database::new(path.to_str().expect("utf8")).expect("open db");
                    db.conn
                        .busy_timeout(std::time::Duration::from_secs(10))
                        .expect("busy_timeout");
                    barrier.wait();
                    db.get_or_create_tag("NullRace", None)
                })
            })
            .collect();

        let ids: Vec<i64> = handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("thread join")
                    .expect("no UNIQUE error")
            })
            .collect();
        assert!(ids.iter().all(|id| *id == ids[0]));

        let db = Database::new(path.to_str().expect("utf8")).expect("reopen");
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM Tags WHERE name = ?1 AND category IS NULL",
                params!["NullRace"],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 1);
        let _ = std::fs::remove_file(path.as_path());
    }

    #[test]
    fn apply_scraped_tags_keeps_actor_and_tag_with_same_name() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/overlap.mp4", 512, None)
            .expect("insert file");

        db.apply_scraped_tags(
            file_id,
            &["Alice".to_string()],
            &["Alice".to_string()],
        )
        .expect("apply tags");

        let tags = db.get_file_tags(file_id).expect("get tags");
        assert_eq!(tags.len(), 2);

        let as_tag = tags
            .iter()
            .find(|t| t.category.as_deref() == Some("Tag"))
            .expect("Tag Alice");
        let as_actor = tags
            .iter()
            .find(|t| t.category.as_deref() == Some("Actor"))
            .expect("Actor Alice");

        assert_eq!(as_tag.name, "Alice");
        assert_eq!(as_actor.name, "Alice");
        assert_ne!(as_tag.id, as_actor.id);
    }

    #[test]
    fn startup_migrates_legacy_tags_and_preserves_filetags() {
        let path = temp_db_path("legacy");
        write_legacy_tags_file_db(&path, 42, "Shared", "Tag");

        let db = Database::new(path.to_str().expect("utf8")).expect("startup migrate");
        assert!(Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));

        let linked = db.get_file_tags(1).expect("get tags");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, 42);
        assert_eq!(linked[0].name, "Shared");

        let actor_id = db
            .get_or_create_tag("Shared", Some("Actor".to_string()))
            .expect("new Actor");
        assert_ne!(actor_id, 42);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn startup_migrate_is_idempotent_on_reopen() {
        let path = temp_db_path("idempotent");
        write_legacy_tags_file_db(&path, 1, "Shared", "Tag");

        let first = Database::new(path.to_str().expect("utf8")).expect("first open");
        let sql_after_first = tags_table_sql(&first);
        drop(first);

        let second = Database::new(path.to_str().expect("utf8")).expect("second open");
        let sql_after_second = tags_table_sql(&second);
        assert_eq!(sql_after_first, sql_after_second);
        assert!(Database::tags_schema_is_name_category_unique(
            &sql_after_second
        ));
        assert_eq!(tag_row(&second, 1).name, "Shared");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn foreign_keys_enabled_on_fresh_connection() {
        let db = open_memory_db();
        assert_eq!(foreign_keys_enabled(&db), 1);
        let mut stmt = db
            .conn
            .prepare("PRAGMA foreign_key_check")
            .expect("fk check prepare");
        let mut rows = stmt.query([]).expect("fk check query");
        assert!(
            rows.next().expect("fk check next").is_none(),
            "expected no foreign_key_check rows"
        );
    }

    #[test]
    fn startup_cleans_fk_orphans_and_logs_to_error_log() {
        let path = temp_db_path("orphan-cleanup");
        {
            let conn = Connection::open(&path).expect("raw open");
            // 種子 orphan 必須關 FK 才能寫入
            conn.execute_batch("PRAGMA foreign_keys = OFF").expect("fk off");
            conn.execute_batch(
                "
                CREATE TABLE Files (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    path TEXT NOT NULL UNIQUE,
                    size INTEGER,
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    phash TEXT,
                    status TEXT DEFAULT 'ready'
                );
                CREATE TABLE Metadata (
                    file_id INTEGER PRIMARY KEY,
                    title TEXT,
                    studio TEXT,
                    release_date TEXT,
                    summary TEXT,
                    cover_image TEXT,
                    sprite_sheet_path TEXT,
                    source_type TEXT,
                    FOREIGN KEY(file_id) REFERENCES Files(id)
                );
                CREATE TABLE Tags (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    category TEXT,
                    UNIQUE(name, category)
                );
                CREATE TABLE FileTags (
                    file_id INTEGER,
                    tag_id INTEGER,
                    confidence FLOAT DEFAULT 1.0,
                    PRIMARY KEY (file_id, tag_id),
                    FOREIGN KEY(file_id) REFERENCES Files(id),
                    FOREIGN KEY(tag_id) REFERENCES Tags(id)
                );
                INSERT INTO Files (id, path, size) VALUES (1, '/tmp/keep.mp4', 1);
                INSERT INTO Tags (id, name, category) VALUES (1, 'Keep', 'Tag');
                INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (1, 1, 1.0);
                INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (1, 999, 1.0);
                INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (999, 1, 1.0);
                INSERT INTO Metadata (file_id, title) VALUES (99999, 'orphan meta');
                ",
            )
            .expect("seed orphans");
        }

        let db = Database::new(path.to_str().expect("utf8")).expect("startup");
        assert_eq!(foreign_keys_enabled(&db), 1);
        let ft_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM FileTags", [], |row| row.get(0))
            .expect("ft count");
        assert_eq!(ft_count, 1);
        let meta_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM Metadata", [], |row| row.get(0))
            .expect("meta count");
        assert_eq!(meta_count, 0);
        let logs = db.list_error_logs(50).expect("logs");
        let summaries: Vec<_> = logs
            .iter()
            .filter(|e| {
                e.category == "database" && e.message.contains("startup orphan cleanup removed")
            })
            .collect();
        assert_eq!(
            summaries.len(),
            1,
            "expected exactly one orphan cleanup summary got {logs:?}"
        );
        let summary = summaries[0];
        assert!(
            summary.message.contains("FileTags(missing Files)=1"),
            "summary={}",
            summary.message
        );
        assert!(
            summary.message.contains("FileTags(missing Tags)=1"),
            "summary={}",
            summary.message
        );
        assert!(
            summary.message.contains("Metadata(missing Files)=1"),
            "summary={}",
            summary.message
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn swap_begin_failure_restores_foreign_keys() {
        let db = open_memory_db();
        assert_eq!(foreign_keys_enabled(&db), 1);
        db.conn
            .execute_batch(
                "
                CREATE TABLE Tags_mig (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    category TEXT,
                    UNIQUE(name, category)
                );
                DELETE FROM Tags;
                INSERT INTO Tags_mig (id, name, category) VALUES (1, 'FromMig', 'Tag');
                ",
            )
            .expect("seed empty Tags + Tags_mig");

        swap_tags_fault::arm_begin();
        let err = db
            .swap_empty_tags_with_tags_mig()
            .expect_err("injected begin failure");
        swap_tags_fault::clear();
        assert!(err.to_string().contains("injected BEGIN IMMEDIATE failure"));
        assert_eq!(foreign_keys_enabled(&db), 1);
        assert!(db.table_exists("Tags_mig").expect("Tags_mig kept"));
    }

    #[test]
    fn add_file_tag_enforces_fk_when_enabled() {
        let db = open_memory_db();
        assert_eq!(foreign_keys_enabled(&db), 1);
        let file_id = db
            .insert_file("/tmp/fk-tag.mp4", 1, None)
            .expect("insert file");
        let tag_id = db
            .get_or_create_tag("Keep", Some("Tag".to_string()))
            .expect("tag");

        db.add_file_tag(file_id, tag_id, 1.0).expect("valid link");
        assert_eq!(db.get_file_tags(file_id).expect("tags").len(), 1);

        let missing_file = db
            .add_file_tag(99999, tag_id, 1.0)
            .expect_err("missing file");
        assert!(
            missing_file.to_string().to_lowercase().contains("foreign"),
            "err={missing_file}"
        );
        let missing_tag = db
            .add_file_tag(file_id, 99999, 1.0)
            .expect_err("missing tag");
        assert!(
            missing_tag.to_string().to_lowercase().contains("foreign"),
            "err={missing_tag}"
        );
        let orphan_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM FileTags
                 WHERE file_id = 99999 OR tag_id = 99999",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(orphan_count, 0);
    }

    #[test]
    fn update_metadata_rejects_missing_file_when_fk_on() {
        let db = open_memory_db();
        assert_eq!(foreign_keys_enabled(&db), 1);
        let err = db
            .update_metadata(&Metadata {
                file_id: 99999,
                title: Some("orphan".into()),
                studio: None,
                release_date: None,
                summary: None,
                cover_image: None,
                sprite_sheet_path: None,
                source_type: None,
            })
            .expect_err("missing file");
        assert!(
            err.to_string().to_lowercase().contains("foreign"),
            "err={err}"
        );
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM Metadata", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 0);
    }

    #[test]
    fn persist_scrape_success_rejects_missing_file_when_fk_on() {
        let db = open_memory_db();
        assert_eq!(foreign_keys_enabled(&db), 1);
        let err = db
            .persist_scrape_success(
                &Metadata {
                    file_id: 99999,
                    title: Some("ghost".into()),
                    studio: None,
                    release_date: None,
                    summary: None,
                    cover_image: None,
                    sprite_sheet_path: None,
                    source_type: Some("nfo".into()),
                },
                &["TagA".into()],
                &["ActorA".into()],
                file_status::ready,
            )
            .expect_err("missing file");
        assert!(
            err.to_string().to_lowercase().contains("foreign"),
            "err={err}"
        );
        let meta: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM Metadata", [], |row| row.get(0))
            .expect("meta");
        let ft: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM FileTags", [], |row| row.get(0))
            .expect("ft");
        let tags: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM Tags", [], |row| row.get(0))
            .expect("tags");
        assert_eq!(meta, 0);
        assert_eq!(ft, 0);
        assert_eq!(tags, 0);
    }

    #[test]
    fn delete_file_record_removes_children_under_fk_on() {
        let db = open_memory_db();
        assert_eq!(foreign_keys_enabled(&db), 1);
        let file_id = db
            .insert_file("/tmp/fk-delete.mp4", 1, None)
            .expect("insert");
        db.update_metadata(&Metadata {
            file_id,
            title: Some("keep".into()),
            studio: None,
            release_date: None,
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: None,
        })
        .expect("meta");
        db.apply_scraped_tags(file_id, &["X".into()], &["Y".into()])
            .expect("tags");

        // 直接刪父列應因子列 FK 失敗
        let parent_err = db
            .conn
            .execute("DELETE FROM Files WHERE id = ?1", params![file_id])
            .expect_err("raw parent delete");
        assert!(
            parent_err.to_string().to_lowercase().contains("foreign"),
            "err={parent_err}"
        );

        db.delete_file_record(file_id).expect("app delete");
        assert_eq!(db.count_files().expect("files"), 0);
        let meta: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM Metadata", [], |row| row.get(0))
            .expect("meta");
        let ft: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM FileTags", [], |row| row.get(0))
            .expect("ft");
        assert_eq!(meta, 0);
        assert_eq!(ft, 0);
    }

    #[test]
    fn migrate_with_foreign_keys_on_preserves_filetags_and_restores_fk() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/fk-on-migrate.mp4", 256, None)
            .expect("insert file");

        db.conn
            .execute("PRAGMA foreign_keys = OFF", [])
            .expect("fk off for seed");
        seed_legacy_tags_schema(&db, 42, "Shared", "Tag");
        db.conn
            .execute(
                "INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (?1, 42, 1.0)",
                params![file_id],
            )
            .expect("seed FileTags");
        db.conn
            .execute("PRAGMA foreign_keys = ON", [])
            .expect("fk on before migrate");
        assert_eq!(foreign_keys_enabled(&db), 1);

        db.migrate_tags_unique_to_name_category()
            .expect("migrate with fk on");

        assert_eq!(foreign_keys_enabled(&db), 1);
        db.filetags_tags_foreign_key_check_ok()
            .expect("filetags->tags fk ok");

        let linked = db.get_file_tags(file_id).expect("get tags");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, 42);
    }

    #[test]
    fn migrate_rolls_back_on_mid_failure_and_keeps_legacy_tags() {
        let db = open_memory_db();
        seed_legacy_tags_schema(&db, 7, "KeepMe", "Tag");

        migrate_tags_fault::arm_after_insert();
        let err = db
            .migrate_tags_unique_to_name_category()
            .expect_err("injected fault");
        migrate_tags_fault::clear();
        assert!(err.to_string().contains("injected migrate fault"));

        assert!(!db.table_exists("Tags_mig").expect("Tags_mig check"));
        assert!(!Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
        assert_eq!(tag_row(&db, 7).name, "KeepMe");

        db.migrate_tags_unique_to_name_category()
            .expect("retry migrate");
        assert!(Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
    }

    #[test]
    fn migrate_rolls_back_when_filetags_tags_fk_check_fails() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/fk-check-fail.mp4", 64, None)
            .expect("insert file");
        seed_legacy_tags_schema(&db, 7, "KeepMe", "Tag");
        // 正式環境可能累積 orphan 列 測試時關 FK 才能寫入
        db.conn
            .execute("PRAGMA foreign_keys = OFF", [])
            .expect("fk off");
        db.conn
            .execute(
                "INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (?1, 999, 1.0)",
                params![file_id],
            )
            .expect("orphan filetag");

        let err = db
            .migrate_tags_unique_to_name_category()
            .expect_err("fk check should fail");
        assert!(err.to_string().contains("FileTags->Tags"));

        assert!(!db.table_exists("Tags_mig").expect("no leftover"));
        assert!(!Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
        assert_eq!(tag_row(&db, 7).name, "KeepMe");
    }

    #[test]
    fn migrate_ignores_unrelated_metadata_orphan_fk() {
        let db = open_memory_db();
        seed_legacy_tags_schema(&db, 5, "Keep", "Tag");
        // Metadata 指向不存在的 file 全庫 fk_check 會失敗 但不應阻擋 Tags 遷移
        db.conn
            .execute("PRAGMA foreign_keys = OFF", [])
            .expect("fk off");
        db.conn
            .execute(
                "INSERT INTO Metadata (file_id, title) VALUES (99999, 'orphan meta')",
                [],
            )
            .expect("orphan metadata");

        db.migrate_tags_unique_to_name_category()
            .expect("migrate despite metadata orphan");
        assert!(Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
        assert_eq!(tag_row(&db, 5).name, "Keep");
    }

    #[test]
    fn migrate_ignores_filetags_files_orphan_fk() {
        let db = open_memory_db();
        seed_legacy_tags_schema(&db, 5, "Keep", "Tag");
        // FileTags→Files orphan 不應阻擋 Tags 遷移 我們只檢查 FileTags→Tags
        db.conn
            .execute("PRAGMA foreign_keys = OFF", [])
            .expect("fk off");
        db.conn
            .execute(
                "INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (99999, 5, 1.0)",
                [],
            )
            .expect("orphan filetag to missing file");

        db.migrate_tags_unique_to_name_category()
            .expect("migrate despite FileTags->Files orphan");
        assert!(Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
        assert_eq!(tag_row(&db, 5).name, "Keep");
    }

    #[test]
    fn startup_backups_nonempty_tags_mig_when_tags_also_nonempty() {
        let path = temp_db_path("both-nonempty");
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        {
            let conn = Connection::open(&path).expect("raw open");
            // 預佔用附近秒數的備份名 迫使走碰撞後綴
            let mut ddl = String::from(
                "
                CREATE TABLE Files (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    path TEXT NOT NULL UNIQUE,
                    size INTEGER,
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    phash TEXT,
                    status TEXT DEFAULT 'ready'
                );
                CREATE TABLE Tags (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    category TEXT,
                    UNIQUE(name, category)
                );
                CREATE TABLE Tags_mig (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    category TEXT,
                    UNIQUE(name, category)
                );
                CREATE TABLE FileTags (
                    file_id INTEGER,
                    tag_id INTEGER,
                    confidence FLOAT DEFAULT 1.0,
                    PRIMARY KEY (file_id, tag_id)
                );
                ",
            );
            // 僅預佔用當前秒與鄰近一秒 迫使走碰撞後綴
            for offset in [0u64, 1] {
                ddl.push_str(&format!(
                    "CREATE TABLE Tags_mig_backup_{} (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        name TEXT NOT NULL,
                        category TEXT,
                        UNIQUE(name, category)
                    );",
                    secs.saturating_add(offset)
                ));
            }
            ddl.push_str(
                "
                INSERT INTO Files (id, path, size) VALUES (1, '/tmp/both.mp4', 1);
                INSERT INTO Tags (id, name, category) VALUES (10, 'KeepTags', 'Tag');
                INSERT INTO Tags_mig (id, name, category) VALUES (20, 'FromMig', 'Actor');
                INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (1, 10, 1.0);
                ",
            );
            conn.execute_batch(&ddl).expect("seed both nonempty");
        }

        let db = Database::new(path.to_str().expect("utf8")).expect("startup");
        assert!(!db.table_exists("Tags_mig").expect("Tags_mig renamed"));
        assert_eq!(tag_row(&db, 10).name, "KeepTags");
        let linked = db.get_file_tags(1).expect("filetags");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, 10);
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM Tags", [], |row| row.get::<_, i64>(0))
                .expect("tags count"),
            1
        );

        let mut stmt = db
            .conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name LIKE 'Tags_mig_backup_%'",
            )
            .expect("list backups");
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("map")
            .collect::<Result<Vec<_>, _>>()
            .expect("names");
        let backup_name = names
            .iter()
            .find(|name| {
                db.conn
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {name} WHERE id = 20 AND name = 'FromMig'"),
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or(0)
                    == 1
            })
            .expect("backup holding FromMig")
            .clone();
        // Tags_mig_backup_<secs> 已有三個底線 碰撞後綴會變成第四個或結尾 _1
        assert!(
            backup_name.ends_with("_1") || backup_name.matches('_').count() >= 4,
            "expected collision suffix in {backup_name}"
        );
        let logs = db.list_error_logs(20).expect("error logs");
        assert!(
            logs.iter().any(|e| {
                e.category == "migration"
                    && e.message.contains(&backup_name)
                    && e.message.contains("Tags_mig")
            }),
            "expected ErrorLog entry for Tags_mig backup got {logs:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn startup_recovers_orphan_tags_mig_when_tags_missing() {
        let path = temp_db_path("orphan-mig");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(
                "
                CREATE TABLE Files (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    path TEXT NOT NULL UNIQUE,
                    size INTEGER,
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    phash TEXT,
                    status TEXT DEFAULT 'ready'
                );
                CREATE TABLE FileTags (
                    file_id INTEGER,
                    tag_id INTEGER,
                    confidence FLOAT DEFAULT 1.0,
                    PRIMARY KEY (file_id, tag_id)
                );
                CREATE TABLE Tags_mig (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    category TEXT,
                    UNIQUE(name, category)
                );
                INSERT INTO Files (id, path, size) VALUES (1, '/tmp/orphan.mp4', 1);
                INSERT INTO Tags_mig (id, name, category) VALUES (11, 'Recovered', 'Actor');
                INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (1, 11, 1.0);
                ",
            )
            .expect("seed orphan Tags_mig without Tags");
        }

        let db = Database::new(path.to_str().expect("utf8")).expect("startup recover");
        assert!(!db.table_exists("Tags_mig").expect("Tags_mig gone"));
        assert!(Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
        assert_eq!(tag_row(&db, 11).name, "Recovered");
        let linked = db.get_file_tags(1).expect("filetags");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, 11);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn startup_uses_nonempty_tags_mig_when_tags_empty() {
        let path = temp_db_path("empty-tags-mig");
        {
            // 模擬真實啟動先建了空 Tags 但 Tags_mig 仍有資料
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(
                "
                CREATE TABLE Files (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    path TEXT NOT NULL UNIQUE,
                    size INTEGER,
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    phash TEXT,
                    status TEXT DEFAULT 'ready'
                );
                CREATE TABLE Tags (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    category TEXT,
                    UNIQUE(name, category)
                );
                CREATE TABLE Tags_mig (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    category TEXT,
                    UNIQUE(name, category)
                );
                CREATE TABLE FileTags (
                    file_id INTEGER,
                    tag_id INTEGER,
                    confidence FLOAT DEFAULT 1.0,
                    PRIMARY KEY (file_id, tag_id)
                );
                INSERT INTO Files (id, path, size) VALUES (1, '/tmp/empty-tags.mp4', 1);
                INSERT INTO Tags_mig (id, name, category) VALUES (21, 'FromMig', 'Tag');
                INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (1, 21, 1.0);
                ",
            )
            .expect("seed empty Tags + nonempty Tags_mig");
        }

        let db = Database::new(path.to_str().expect("utf8")).expect("startup reconcile");
        assert!(!db.table_exists("Tags_mig").expect("Tags_mig consumed"));
        assert_eq!(tag_row(&db, 21).name, "FromMig");
        let linked = db.get_file_tags(1).expect("filetags");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].name, "FromMig");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn startup_migrates_legacy_with_leftover_tags_mig() {
        let path = temp_db_path("leftover-mig");
        write_legacy_tags_file_db(&path, 9, "Real", "Tag");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(
                "
                CREATE TABLE Tags_mig (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    category TEXT,
                    UNIQUE(name, category)
                );
                INSERT INTO Tags_mig (id, name, category) VALUES (99, 'Stale', 'Tag');
                ",
            )
            .expect("seed leftover Tags_mig");
        }

        let db = Database::new(path.to_str().expect("utf8")).expect("startup");
        assert!(!db.table_exists("Tags_mig").expect("Tags_mig gone"));
        assert!(Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
        assert_eq!(tag_row(&db, 9).name, "Real");
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM Tags WHERE id = 99",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("stale count"),
            0
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_scraped_tags_replaces_previous_links() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/replace.mp4", 2048, None)
            .expect("insert file");

        db.apply_scraped_tags(file_id, &["Old".to_string()], &[])
            .expect("first apply");
        db.apply_scraped_tags(file_id, &["New".to_string()], &["Bob".to_string()])
            .expect("second apply");

        let tags = db.get_file_tags(file_id).expect("get tags");
        assert_eq!(tags.len(), 2);
        assert!(tags.iter().any(|t| t.name == "New"));
        assert!(tags.iter().any(|t| t.name == "Bob"));
        assert!(!tags.iter().any(|t| t.name == "Old"));
    }

    #[test]
    fn update_file_status_sets_scrape_failed() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/fail.mp4", 100, None)
            .expect("insert file");

        db.update_file_status(file_id, file_status::scrape_failed)
            .expect("update status");
        let file = db.get_file_by_id(file_id).expect("get file");
        assert_eq!(file.status, file_status::scrape_failed);
    }

    #[test]
    fn reconcile_marks_absent_path_missing_and_restores_ready() {
        let db = open_memory_db();
        let dir = std::env::temp_dir().join(format!(
            "idam-status-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("present.mp4");
        std::fs::write(&path, b"x").expect("touch");
        let path_str = path.to_str().expect("utf8");

        let file_id = db.insert_file(path_str, 1, None).expect("insert");
        assert_eq!(
            db.get_file_by_id(file_id).unwrap().status,
            file_status::ready
        );

        std::fs::remove_file(&path).expect("unlink");
        let changed = db.reconcile_missing_files().expect("reconcile");
        assert_eq!(changed, 1);
        assert_eq!(
            db.get_file_by_id(file_id).unwrap().status,
            file_status::missing
        );

        std::fs::write(&path, b"x").expect("restore");
        let changed = db.reconcile_missing_files().expect("reconcile again");
        assert_eq!(changed, 1);
        assert_eq!(
            db.get_file_by_id(file_id).unwrap().status,
            file_status::ready
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_overrides_processing_when_path_gone() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/idam-never-exists-status-xyz.mp4", 1, None)
            .expect("insert");
        db.update_file_status(file_id, file_status::processing)
            .expect("processing");
        let changed = db.reconcile_missing_files().expect("reconcile");
        assert_eq!(changed, 1);
        assert_eq!(
            db.get_file_by_id(file_id).unwrap().status,
            file_status::missing
        );
    }

    #[test]
    fn insert_conflict_clears_missing_when_rescanned() {
        let db = open_memory_db();
        let dir = std::env::temp_dir().join(format!(
            "idam-rescan-status-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("back.mp4");
        std::fs::write(&path, b"y").expect("touch");
        let path_str = path.to_str().expect("utf8");

        let file_id = db.insert_file(path_str, 1, None).expect("insert");
        db.update_file_status(file_id, file_status::missing)
            .expect("mark missing");

        db.insert_file(path_str, 2, None).expect("rescan");
        let file = db.get_file_by_path(path_str).expect("get").expect("exists");
        assert_eq!(file.status, file_status::ready);
        assert_eq!(file.size, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn insert_conflict_preserves_scrape_failed() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/keep-failed.mp4", 1, None)
            .expect("insert");
        db.update_file_status(file_id, file_status::scrape_failed)
            .expect("fail");
        db.insert_file("/tmp/keep-failed.mp4", 9, None)
            .expect("rescan");
        let file = db
            .get_file_by_path("/tmp/keep-failed.mp4")
            .expect("get")
            .expect("exists");
        assert_eq!(file.status, file_status::scrape_failed);
        assert_eq!(file.size, 9);
    }

    #[test]
    fn update_file_status_rejects_unknown_and_illegal() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/illegal-status.mp4", 1, None)
            .expect("insert");

        let err = db
            .update_file_status(file_id, "not_a_status")
            .expect_err("unknown");
        assert!(format!("{}", err).contains("unknown"));

        db.update_file_status(file_id, file_status::missing)
            .expect("to missing");
        let err = db
            .update_file_status(file_id, file_status::processing)
            .expect_err("missing cannot jump to processing");
        assert!(format!("{}", err).contains("illegal"));
    }

    #[test]
    fn scrape_status_sequence_matches_command_transitions() {
        // 模擬 scrape_file_command 的狀態步驟 無需 Tauri State
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/scrape-seq.mp4", 1, None)
            .expect("insert");

        db.update_file_status(file_id, file_status::processing)
            .expect("start scrape");
        db.update_file_status(file_id, file_status::scrape_failed)
            .expect("dual fail");
        assert_eq!(
            db.get_file_by_id(file_id).unwrap().status,
            file_status::scrape_failed
        );

        // 重試刮削
        db.update_file_status(file_id, file_status::processing)
            .expect("retry");
        db.update_file_status(file_id, file_status::ready)
            .expect("success");
        assert_eq!(db.get_file_by_id(file_id).unwrap().status, file_status::ready);

        // 預覽 after_transient_work 保留 scrape_failed
        db.update_file_status(file_id, file_status::scrape_failed)
            .expect("fail again");
        let previous = file_status::scrape_failed;
        db.update_file_status(file_id, file_status::processing)
            .expect("preview start");
        let next = file_status::after_transient_work(previous);
        db.update_file_status(file_id, next).expect("preview end");
        assert_eq!(
            db.get_file_by_id(file_id).unwrap().status,
            file_status::scrape_failed
        );
    }

    #[test]
    fn file_with_metadata_serializes_nested_file_object() {
        let payload = FileWithMetadata {
            file: FileRecord {
                id: 7,
                path: "/videos/a.mp4".to_string(),
                size: 10,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                phash: None,
                status: "ready".to_string(),
            },
            metadata: None,
            tags: Vec::new(),
            suspected_duplicate: None,
            path_collision: None,
        };
        let value = serde_json::to_value(&payload).expect("serialize");
        assert!(value.get("file").is_some(), "expected nested file object");
        assert!(value.get("id").is_none(), "must not flatten file fields");
        assert_eq!(value["file"]["id"], 7);
        assert_eq!(value["file"]["path"], "/videos/a.mp4");
        assert!(value.get("metadata").is_some());
        assert!(value.get("tags").is_some());
    }

    #[test]
    fn persist_scrape_success_keeps_sprite_and_writes_tags_status() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/scrape.mp4", 512, Some("phash".to_string()))
            .expect("insert file");

        db.update_metadata(&Metadata {
            file_id,
            title: Some("old".to_string()),
            studio: None,
            release_date: None,
            summary: None,
            cover_image: None,
            sprite_sheet_path: Some("/tmp/sprite.jpg".to_string()),
            source_type: None,
        })
        .expect("seed metadata");

        let metadata = Metadata {
            file_id,
            title: Some("new title".to_string()),
            studio: Some("Studio".to_string()),
            release_date: None,
            summary: None,
            cover_image: Some("https://example.test/cover.jpg".to_string()),
            sprite_sheet_path: Some("/tmp/sprite.jpg".to_string()),
            source_type: Some("scraped_avdc".to_string()),
        };

        let tags = db
            .persist_scrape_success(
                &metadata,
                &["Beach".to_string()],
                &["Alice".to_string()],
                "ready",
            )
            .expect("persist scrape");

        assert_eq!(tags.len(), 2);
        let saved = db.get_metadata(file_id).expect("get metadata").expect("present");
        assert_eq!(saved.title.as_deref(), Some("new title"));
        assert_eq!(saved.sprite_sheet_path.as_deref(), Some("/tmp/sprite.jpg"));
        let file = db.get_file_by_id(file_id).expect("get file");
        assert_eq!(file.status, "ready");
    }

    #[test]
    fn persist_scrape_success_rolls_back_on_failure() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/rollback.mp4", 128, None)
            .expect("insert file");

        db.update_file_status(file_id, "scrape_failed")
            .expect("seed status");

        // 破壞 Tags schema 讓 apply_scraped_tags 失敗以驗證交易回滾
        db.conn
            .execute("DROP TABLE Tags", [])
            .expect("drop tags");

        let metadata = Metadata {
            file_id,
            title: Some("should not persist".to_string()),
            studio: None,
            release_date: None,
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("scraped_avdc".to_string()),
        };

        let err = db
            .persist_scrape_success(&metadata, &["x".to_string()], &[], "ready")
            .expect_err("expected failure");
        assert!(matches!(err, rusqlite::Error::SqliteFailure(_, _)));

        // metadata 不應寫入 status 應維持 scrape_failed
        let meta = db.get_metadata(file_id).expect("get metadata");
        assert!(meta.is_none());
        let file = db.get_file_by_id(file_id).expect("get file");
        assert_eq!(file.status, "scrape_failed");
    }

    #[test]
    fn insert_file_coalesce_keeps_existing_phash_on_null() {
        let db = open_memory_db();
        db.insert_file("/tmp/keep-phash.mp4", 10, Some("aabbccddeeff0011".into()))
            .expect("insert");
        db.insert_file("/tmp/keep-phash.mp4", 20, None)
            .expect("reinsert without phash");
        let file = db
            .get_file_by_path("/tmp/keep-phash.mp4")
            .expect("query")
            .expect("exists");
        assert_eq!(file.size, 20);
        assert_eq!(file.phash.as_deref(), Some("aabbccddeeff0011"));
    }

    #[test]
    fn same_path_upsert_does_not_double_insert() {
        let db = open_memory_db();
        let (id1, is_new1) = db
            .upsert_file("/tmp/path-dedup.mp4", 100, None)
            .expect("first");
        assert!(is_new1);
        let (id2, is_new2) = db
            .upsert_file("/tmp/path-dedup.mp4", 200, Some("deadbeefdeadbeef".into()))
            .expect("second");
        assert!(!is_new2);
        assert_eq!(id1, id2);
        assert_eq!(db.count_files().expect("count"), 1);
        let file = db.get_file_by_id(id1).expect("get");
        assert_eq!(file.size, 200);
        assert_eq!(file.phash.as_deref(), Some("deadbeefdeadbeef"));
    }

    #[test]
    fn find_near_phash_duplicate_respects_hamming_threshold() {
        let db = open_memory_db();
        let keeper = db
            .insert_file(
                "/tmp/keeper.mp4",
                1,
                Some("0000000000000000".into()),
            )
            .expect("keeper");
        let candidate = db
            .insert_file("/tmp/cand.mp4", 1, None)
            .expect("candidate");

        // distance 8 → match
        let hit = db
            .find_near_phash_duplicate(candidate, "00000000000000ff", 8)
            .expect("query")
            .expect("should match");
        assert_eq!(hit.0.id, keeper);
        assert_eq!(hit.1, 8);

        // distance 9 → no match at max 8
        let miss = db
            .find_near_phash_duplicate(candidate, "00000000000001ff", 8)
            .expect("query");
        assert!(miss.is_none());
    }

    #[test]
    fn delete_file_record_removes_metadata_and_tags() {
        let db = open_memory_db();
        let id = db.insert_file("/tmp/drop.mp4", 1, None).expect("insert");
        db.update_metadata(&Metadata {
            file_id: id,
            title: Some("t".into()),
            studio: None,
            release_date: None,
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: None,
        })
        .expect("meta");
        db.apply_scraped_tags(id, &["X".into()], &[])
            .expect("tags");
        db.delete_file_record(id).expect("delete");
        assert_eq!(db.count_files().expect("count"), 0);
        assert!(db.get_metadata(id).expect("meta").is_none());
    }

    #[test]
    fn error_log_insert_list_and_clear() {
        let db = open_memory_db();
        let id = db.insert_file("/tmp/err.mp4", 1, None).expect("insert");
        db.insert_error_log(
            "scrape",
            Some(id),
            Some("/tmp/err.mp4"),
            "scrape failed",
            Some("detail line"),
        )
        .expect("log");
        db.insert_error_log("ffmpeg", None, None, "ffmpeg missing", None)
            .expect("log2");
        let logs = db.list_error_logs(10).expect("list");
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].category, "ffmpeg");
        assert_eq!(logs[1].category, "scrape");
        assert_eq!(logs[1].file_id, Some(id));
        let cleared = db.clear_error_logs().expect("clear");
        assert_eq!(cleared, 2);
        assert!(db.list_error_logs(10).expect("list2").is_empty());
    }

    #[test]
    fn list_organizable_skips_missing() {
        let db = open_memory_db();
        let a = db.insert_file("/tmp/a.mp4", 1, None).expect("a");
        let b = db.insert_file("/tmp/b.mp4", 1, None).expect("b");
        db.update_file_status(b, file_status::missing).expect("miss");
        let ids = db.list_organizable_file_ids().expect("list");
        assert_eq!(ids, vec![a]);
    }

    #[test]
    fn fresh_db_reaches_current_user_version() {
        let db = open_memory_db();
        assert_eq!(
            db.get_user_version().expect("user_version"),
            SCHEMA_USER_VERSION
        );
        assert!(
            db.tags_nullsafe_index_exists().expect("index check"),
            "fresh db should have nullsafe unique index"
        );
        assert_eq!(foreign_keys_enabled(&db), 1);
    }

    /// Mid-PR v5 DBs (364c6cb / 22dd37f / 01eb0f8) stamped user_version=5 without
    /// Files.organized_at — opening through Database::new must repair.
    fn write_v5_without_organized_at(path: &Path) {
        let conn = Connection::open(path).expect("open raw db");
        conn.execute_batch(
            "
            CREATE TABLE Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT DEFAULT 'ready'
            );
            CREATE TABLE Metadata (
                file_id INTEGER PRIMARY KEY,
                title TEXT,
                studio TEXT,
                release_date TEXT,
                summary TEXT,
                cover_image TEXT,
                sprite_sheet_path TEXT,
                source_type TEXT,
                FOREIGN KEY(file_id) REFERENCES Files(id)
            );
            CREATE TABLE Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT,
                UNIQUE(name, category)
            );
            CREATE UNIQUE INDEX idx_tags_name_ifnull_category
                ON Tags (name, IFNULL(category, ''));
            CREATE TABLE FileTags (
                file_id INTEGER,
                tag_id INTEGER,
                confidence FLOAT DEFAULT 1.0,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            );
            CREATE TABLE ErrorLog (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                category TEXT NOT NULL,
                file_id INTEGER,
                path TEXT,
                message TEXT NOT NULL,
                detail TEXT
            );
            CREATE TABLE Jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                status TEXT NOT NULL CHECK (
                    status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')
                ),
                type TEXT NOT NULL CHECK (type IN ('pipeline', 'phash')),
                payload TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                max_attempts INTEGER NOT NULL DEFAULT 3,
                error TEXT,
                retryable INTEGER NOT NULL DEFAULT 1 CHECK (retryable IN (0, 1)),
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE SuspectedDuplicates (
                file_id INTEGER PRIMARY KEY,
                keeper_id INTEGER NOT NULL,
                distance INTEGER NOT NULL CHECK (distance BETWEEN 0 AND 64),
                resolution TEXT NOT NULL DEFAULT 'pending'
                    CHECK (resolution IN ('pending', 'confirmed', 'ignored')),
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(keeper_id) REFERENCES Files(id)
            );
            CREATE TABLE DuplicateTombstones (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                phash TEXT,
                keeper_id INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO Files (id, path, size) VALUES (1, '/tmp/v5-no-org.mp4', 10);
            PRAGMA user_version = 5;
            ",
        )
        .expect("seed v5 without organized_at");
    }

    fn files_has_column(db: &Database, name: &str) -> bool {
        let mut stmt = db
            .conn
            .prepare("PRAGMA table_info(Files)")
            .expect("table_info");
        let cols = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("cols");
        for c in cols {
            if c.expect("col") == name {
                return true;
            }
        }
        false
    }

    #[test]
    fn v5_without_organized_at_is_repaired_on_open() {
        let path = temp_db_path("v5-no-organized-at");
        write_v5_without_organized_at(&path);
        assert_eq!(raw_user_version(&path), 5);
        {
            let conn = Connection::open(&path).expect("raw");
            let mut stmt = conn.prepare("PRAGMA table_info(Files)").expect("info");
            let cols: Vec<String> = stmt
                .query_map([], |row| row.get(1))
                .expect("map")
                .map(|r| r.expect("row"))
                .collect();
            assert!(
                !cols.iter().any(|c| c == "organized_at"),
                "precondition: column must be absent"
            );
        }

        let db = Database::new(path.to_str().expect("utf8")).expect("open repair");
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert!(
            files_has_column(&db, "organized_at"),
            "ensure_v5_columns must ADD organized_at on already-v5 DBs"
        );

        let tags = db
            .persist_scrape_success(
                &Metadata {
                    file_id: 1,
                    title: Some("repaired".into()),
                    studio: None,
                    release_date: None,
                    summary: None,
                    cover_image: None,
                    sprite_sheet_path: None,
                    source_type: Some("test".into()),
                },
                &["TagA".to_string()],
                &[],
                file_status::ready,
            )
            .expect("persist_scrape_success must succeed after repair");
        assert_eq!(tags.len(), 1);
        assert!(
            db.is_organized_file(1).expect("organized"),
            "scrape success marks organized_at"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v010_shaped_db_upgrades_without_data_loss() {
        let path = temp_db_path("v010-upgrade");
        write_v010_shaped_file_db(&path);
        assert_eq!(raw_user_version(&path), 0);

        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(
            db.get_user_version().expect("user_version"),
            SCHEMA_USER_VERSION
        );
        assert!(db.tags_nullsafe_index_exists().expect("index"));
        let linked = db.get_file_tags(1).expect("tags");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, 1);
        assert_eq!(linked[0].name, "Keep");
        assert_eq!(foreign_keys_enabled(&db), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn duplicate_null_category_tags_merge_on_upgrade() {
        let path = temp_db_path("dup-null-upgrade");
        write_duplicate_null_category_tags_db(&path);
        assert_eq!(raw_null_tag_count(&path, "Dup"), 3);

        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(
            db.get_user_version().expect("user_version"),
            SCHEMA_USER_VERSION
        );

        let null_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM Tags WHERE name = ?1 AND category IS NULL",
                params!["Dup"],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(null_count, 1);

        let keeper = tag_row(&db, 10);
        assert_eq!(keeper.name, "Dup");
        assert!(keeper.category.is_none());

        let tags_for_1 = db.get_file_tags(1).expect("file 1 tags");
        assert_eq!(tags_for_1.len(), 1);
        assert_eq!(tags_for_1[0].id, 10);
        assert!((filetag_confidence(&db, 1, 10) - 0.9).abs() < f64::EPSILON);

        let tags_for_2 = db.get_file_tags(2).expect("file 2 tags");
        assert_eq!(tags_for_2.len(), 1);
        assert_eq!(tags_for_2[0].id, 10);
        assert!((filetag_confidence(&db, 2, 10) - 0.8).abs() < f64::EPSILON);

        let other = tag_row(&db, 20);
        assert_eq!(other.name, "Other");
        assert!(db.tags_nullsafe_index_exists().expect("index"));
        assert_no_foreign_key_check_rows(&db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_string_and_null_category_same_name_merge_on_upgrade() {
        let path = temp_db_path("empty-null-clash");
        write_empty_string_and_null_category_db(&path);

        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(
            db.get_user_version().expect("user_version"),
            SCHEMA_USER_VERSION
        );

        let clash_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM Tags WHERE name = ?1",
                params!["Clash"],
                |row| row.get(0),
            )
            .expect("clash count");
        assert_eq!(clash_count, 1);
        let clash = tag_row(&db, 1);
        assert_eq!(clash.name, "Clash");
        assert!(clash.category.is_none());

        let solo = tag_row(&db, 3);
        assert_eq!(solo.name, "Solo");
        assert!(solo.category.is_none());

        let tags_for_1 = db.get_file_tags(1).expect("file 1");
        assert_eq!(tags_for_1.len(), 1);
        assert_eq!(tags_for_1[0].id, 1);
        assert!((filetag_confidence(&db, 1, 1) - 0.7).abs() < f64::EPSILON);

        let tags_for_2 = db.get_file_tags(2).expect("file 2");
        assert_eq!(tags_for_2.len(), 2);
        assert!(db.tags_nullsafe_index_exists().expect("index"));
        assert_no_foreign_key_check_rows(&db);

        let via_empty = db
            .get_or_create_tag("Clash", Some(String::new()))
            .expect("empty string normalizes");
        assert_eq!(via_empty, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pre20_db_with_tags_mig_upgrades_straight_to_v2() {
        let path = temp_db_path("pre20-tags-mig-v2");
        write_pre20_with_tags_mig_file_db(&path);

        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade to v2");
        assert_eq!(
            db.get_user_version().expect("user_version"),
            SCHEMA_USER_VERSION
        );
        assert!(Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
        assert!(!db.table_exists("Tags_mig").expect("Tags_mig gone"));
        assert_eq!(tag_row(&db, 7).name, "Legacy");
        let linked = db.get_file_tags(1).expect("filetags");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, 7);
        assert!(db.tags_nullsafe_index_exists().expect("index"));
        assert_no_foreign_key_check_rows(&db);
        assert_eq!(foreign_keys_enabled(&db), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn user_version_migration_is_idempotent_on_reopen() {
        let path = temp_db_path("uv-idempotent");
        write_v010_shaped_file_db(&path);

        let first = Database::new(path.to_str().expect("utf8")).expect("first");
        let version_first = first.get_user_version().expect("v1");
        let index_sql_first: String = first
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
                params![TAGS_NULLSAFE_UNIQUE_INDEX],
                |row| row.get(0),
            )
            .expect("index sql");
        drop(first);

        let second = Database::new(path.to_str().expect("utf8")).expect("second");
        assert_eq!(second.get_user_version().expect("v2"), version_first);
        assert_eq!(version_first, SCHEMA_USER_VERSION);
        let index_sql_second: String = second
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
                params![TAGS_NULLSAFE_UNIQUE_INDEX],
                |row| row.get(0),
            )
            .expect("index sql");
        assert_eq!(index_sql_first, index_sql_second);
        assert_eq!(tag_row(&second, 1).name, "Keep");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migration_2_failure_rolls_back_and_keeps_duplicates() {
        let path = temp_db_path("mig2-rollback");
        write_duplicate_null_category_tags_db(&path);

        migrate_null_tags_fault::arm_after_merge();
        let err = match Database::new(path.to_str().expect("utf8")) {
            Ok(_) => {
                migrate_null_tags_fault::clear();
                panic!("expected migration failure");
            }
            Err(e) => e,
        };
        migrate_null_tags_fault::clear();
        assert!(
            err.to_string().contains("injected migrate fault after null-tag merge"),
            "err={err}"
        );

        // baseline 已標成 1 但 migration 2 交易回滾 重複列仍在
        assert_eq!(raw_user_version(&path), SCHEMA_BASELINE_VERSION);
        assert_eq!(raw_null_tag_count(&path, "Dup"), 3);

        let conn = Connection::open(&path).expect("raw");
        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                params![TAGS_NULLSAFE_UNIQUE_INDEX],
                |row| row.get(0),
            )
            .expect("index count");
        assert_eq!(index_count, 0);

        // 失敗後可重試成功
        let db = Database::new(path.to_str().expect("utf8")).expect("retry");
        assert_eq!(
            db.get_user_version().expect("user_version"),
            SCHEMA_USER_VERSION
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM Tags WHERE name = ?1 AND category IS NULL",
                    params!["Dup"],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count"),
            1
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn user_version_newer_than_program_refuses_start() {
        let path = temp_db_path("uv-too-new");
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch("CREATE TABLE Files (id INTEGER PRIMARY KEY);")
                .expect("create");
            conn.execute(
                &format!("PRAGMA user_version = {}", SCHEMA_USER_VERSION + 1),
                [],
            )
            .expect("set version");
        }

        let err = match Database::new(path.to_str().expect("utf8")) {
            Ok(_) => panic!("expected refuse newer db"),
            Err(e) => e,
        };
        assert!(
            matches!(
                &err,
                rusqlite::Error::SqliteFailure(e, Some(_))
                    if e.extended_code == rusqlite::ffi::SQLITE_CANTOPEN
            ),
            "expected SqliteFailure(CANTOPEN) got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            !msg.contains("Invalid parameter name"),
            "dialog must not show InvalidParameterName prefix err={msg}"
        );
        assert!(
            msg.contains("較新版應用程式") && msg.contains("請升級應用程式"),
            "err={msg}"
        );
        assert!(
            msg.contains(&format!("{}", SCHEMA_USER_VERSION + 1)),
            "err={msg}"
        );
        assert_eq!(raw_user_version(&path), SCHEMA_USER_VERSION + 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_migration_target_returns_sqlite_misuse() {
        let db = open_memory_db();
        let unknown = SCHEMA_USER_VERSION + 99;
        let err = match db.run_migration(unknown) {
            Ok(_) => panic!("expected unknown migration target to fail"),
            Err(e) => e,
        };
        assert!(
            matches!(
                &err,
                rusqlite::Error::SqliteFailure(e, Some(_))
                    if e.extended_code == rusqlite::ffi::SQLITE_MISUSE
            ),
            "expected SqliteFailure(MISUSE) got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            !msg.contains("Invalid parameter name"),
            "must not show InvalidParameterName prefix err={msg}"
        );
        assert!(
            msg.contains("未知的 schema 遷移目標") && msg.contains(&format!("{unknown}")),
            "err={msg}"
        );
    }

    #[test]
    fn fresh_db_creates_jobs_and_suspected_tables_at_current_version() {
        let db = open_memory_db();
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert_eq!(SCHEMA_USER_VERSION, 8);
        assert!(db.jobs_table_exists().expect("Jobs exists"));
        assert!(db
            .suspected_duplicates_table_exists()
            .expect("SuspectedDuplicates exists"));
        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'Jobs'",
                [],
                |row| row.get(0),
            )
            .expect("Jobs sql");
        assert!(sql.contains("CHECK"), "status CHECK missing: {sql}");
        assert!(sql.contains("'queued'"), "queued status missing: {sql}");
        assert!(sql.contains("'running'"), "running status missing: {sql}");
    }

    #[test]
    fn v2_db_upgrades_to_current_with_jobs_and_suspected_tables() {
        let path = temp_db_path("v2-to-v4-jobs");
        write_v2_shaped_file_db(&path);
        assert_eq!(raw_user_version(&path), 2);
        {
            let conn = Connection::open(&path).expect("raw");
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'Jobs'",
                    [],
                    |row| row.get(0),
                )
                .expect("count");
            assert_eq!(n, 0, "v2 fixture must not have Jobs");
        }

        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(
            db.get_user_version().expect("uv"),
            SCHEMA_USER_VERSION
        );
        assert!(db.jobs_table_exists().expect("Jobs"));
        assert!(db
            .suspected_duplicates_table_exists()
            .expect("SuspectedDuplicates"));
        let file = db.get_file_by_id(1).expect("preserve file");
        // Open-time repair normalizes separators (Windows: /tmp → \tmp)
        assert_eq!(file.path, normalize_db_path("/tmp/v2-upgrade.mp4"));

        let id = db
            .insert_job(&NewJobRow {
                status: "queued",
                job_type: "pipeline",
                payload: "{\"file_id\":1}",
                attempts: 0,
                max_attempts: 3,
                error: None,
                retryable: true,
            })
            .expect("insert job");
        let row = db.get_job(id).expect("get job");
        assert_eq!(row.status, "queued");
        assert_eq!(row.job_type, "pipeline");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn jobs_and_suspected_migration_is_idempotent_on_reopen() {
        let path = temp_db_path("jobs-suspected-idempotent");
        write_v2_shaped_file_db(&path);
        let first = Database::new(path.to_str().expect("utf8")).expect("first");
        assert_eq!(
            first.get_user_version().expect("v"),
            SCHEMA_USER_VERSION
        );
        drop(first);
        let second = Database::new(path.to_str().expect("utf8")).expect("second");
        assert_eq!(
            second.get_user_version().expect("v"),
            SCHEMA_USER_VERSION
        );
        assert!(second.jobs_table_exists().expect("Jobs"));
        assert!(second
            .suspected_duplicates_table_exists()
            .expect("SuspectedDuplicates"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v3_db_upgrades_to_current_with_suspected_and_tombstones() {
        let path = temp_db_path("v3-to-current-suspected");
        write_v3_shaped_file_db(&path);
        assert_eq!(raw_user_version(&path), 3);
        {
            let conn = Connection::open(&path).expect("raw");
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'table' AND name = 'SuspectedDuplicates'",
                    [],
                    |row| row.get(0),
                )
                .expect("count");
            assert_eq!(n, 0, "v3 fixture must not have SuspectedDuplicates");
        }

        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert!(db
            .suspected_duplicates_table_exists()
            .expect("SuspectedDuplicates"));
        let file = db.get_file_by_id(1).expect("preserve file");
        assert_eq!(file.path, normalize_db_path("/tmp/v3-upgrade.mp4"));

        let keeper = db
            .insert_file("/tmp/v3-keeper.mp4", 2, Some("0000000000000000".into()))
            .expect("keeper");
        let candidate = db
            .insert_file("/tmp/v3-candidate.mp4", 3, Some("0000000000000003".into()))
            .expect("candidate");
        db.upsert_suspected_duplicate(candidate, keeper, 2)
            .expect("upsert suspicion");
        let row = db
            .get_pending_suspected_duplicate(candidate)
            .expect("get")
            .expect("pending");
        assert_eq!(row.keeper_id, keeper);
        assert_eq!(row.distance, 2);

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master
                 WHERE type = 'table' AND name = 'SuspectedDuplicates'",
                [],
                |row| row.get(0),
            )
            .expect("sql");
        assert!(
            sql.contains("distance BETWEEN 0 AND 64"),
            "distance CHECK missing: {sql}"
        );
        assert!(db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'DuplicateTombstones'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("tombstones")
            > 0);

        let far = db
            .insert_file("/tmp/v3-far.mp4", 4, None)
            .expect("far");
        let err = db
            .conn
            .execute(
                "INSERT INTO SuspectedDuplicates (file_id, keeper_id, distance, resolution)
                 VALUES (?1, ?2, 99, 'pending')",
                params![far, keeper],
            )
            .expect_err("distance 99 must fail CHECK");
        assert!(
            err.to_string().to_lowercase().contains("check")
                || err.to_string().contains("constraint"),
            "err={err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    fn write_v4_without_tombstones(path: &Path) {
        write_v3_shaped_file_db(path);
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(
            "
            CREATE TABLE SuspectedDuplicates (
                file_id INTEGER PRIMARY KEY,
                keeper_id INTEGER NOT NULL,
                distance INTEGER NOT NULL
                    CHECK (distance BETWEEN 0 AND 64),
                resolution TEXT NOT NULL DEFAULT 'pending'
                    CHECK (resolution IN ('pending', 'confirmed', 'ignored')),
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(keeper_id) REFERENCES Files(id)
            );
            PRAGMA user_version = 4;
            ",
        )
        .expect("v4 without tombstones");
    }

    fn write_v4_tombstones_without_size(path: &Path) {
        // Schema as on 7e72e54: DuplicateTombstones inside v4, no `size` column
        // (and with legacy keeper_phash).
        write_v4_without_tombstones(path);
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(
            "
            CREATE TABLE DuplicateTombstones (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                phash TEXT,
                keeper_id INTEGER,
                keeper_phash TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX idx_duplicate_tombstones_phash ON DuplicateTombstones(phash);
            INSERT INTO DuplicateTombstones (path, phash, keeper_id, keeper_phash)
            VALUES ('/tmp/legacy-tomb.mp4', 'abcd', 1, 'keeperhash');
            PRAGMA user_version = 4;
            ",
        )
        .expect("v4 premature tombstones");
    }

    #[test]
    fn v4_premature_tombstones_without_size_alters_on_v5() {
        let path = temp_db_path("v4-tomb-no-size-to-v5");
        write_v4_tombstones_without_size(&path);
        assert_eq!(raw_user_version(&path), 4);
        {
            let conn = Connection::open(&path).expect("raw");
            let mut stmt = conn
                .prepare("PRAGMA table_info(DuplicateTombstones)")
                .expect("info");
            let cols: Vec<String> = stmt
                .query_map([], |row| row.get(1))
                .expect("rows")
                .collect::<Result<_, _>>()
                .expect("cols");
            assert!(!cols.iter().any(|c| c == "size"), "fixture must lack size");
            assert!(cols.iter().any(|c| c == "keeper_phash"));
        }
        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        let mut stmt = db
            .conn
            .prepare("PRAGMA table_info(DuplicateTombstones)")
            .expect("info");
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("cols");
        assert!(cols.iter().any(|c| c == "size"), "v5 must ADD COLUMN size");
        let list = db.list_duplicate_tombstones().expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].path, normalize_db_path("/tmp/legacy-tomb.mp4"));
        assert_eq!(list[0].size, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v4_db_without_tombstones_upgrades_to_v5() {
        let path = temp_db_path("v4-to-v5-tombstones");
        write_v4_without_tombstones(&path);
        assert_eq!(raw_user_version(&path), 4);
        {
            let conn = Connection::open(&path).expect("raw");
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'table' AND name = 'DuplicateTombstones'",
                    [],
                    |row| row.get(0),
                )
                .expect("count");
            assert_eq!(n, 0, "v4 fixture must lack DuplicateTombstones");
        }
        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'table' AND name = 'DuplicateTombstones'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("tombstones")
                > 0
        );
        assert!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'table' AND name = 'PathCollisions'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("path collisions")
                > 0
        );
        db.insert_duplicate_tombstone("/tmp/tomb.mp4", 42, Some("aabb"), Some(1))
            .expect("insert tombstone");
        let list = db.list_duplicate_tombstones().expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].size, Some(42));
        let _ = std::fs::remove_file(&path);
    }

    fn write_v5_with_organized_at(path: &Path) {
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER NOT NULL,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT NOT NULL DEFAULT 'ready',
                organized_at DATETIME
            );
            CREATE TABLE Metadata (
                file_id INTEGER PRIMARY KEY,
                title TEXT,
                studio TEXT,
                release_date TEXT,
                summary TEXT,
                cover_image TEXT,
                sprite_sheet_path TEXT,
                source_type TEXT,
                FOREIGN KEY(file_id) REFERENCES Files(id)
            );
            CREATE TABLE Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT
            );
            CREATE TABLE FileTags (
                file_id INTEGER NOT NULL,
                tag_id INTEGER NOT NULL,
                confidence REAL,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            );
            CREATE TABLE ErrorLog (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                category TEXT NOT NULL,
                file_id INTEGER,
                path TEXT,
                message TEXT NOT NULL,
                detail TEXT
            );
            CREATE TABLE Jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                status TEXT NOT NULL CHECK (
                    status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')
                ),
                type TEXT NOT NULL CHECK (type IN ('pipeline', 'phash')),
                payload TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                max_attempts INTEGER NOT NULL DEFAULT 3,
                error TEXT,
                retryable INTEGER NOT NULL DEFAULT 1 CHECK (retryable IN (0, 1)),
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE SuspectedDuplicates (
                file_id INTEGER PRIMARY KEY,
                keeper_id INTEGER NOT NULL,
                distance INTEGER NOT NULL CHECK (distance BETWEEN 0 AND 64),
                resolution TEXT NOT NULL DEFAULT 'pending'
                    CHECK (resolution IN ('pending', 'confirmed', 'ignored')),
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(keeper_id) REFERENCES Files(id)
            );
            CREATE TABLE DuplicateTombstones (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                phash TEXT,
                keeper_id INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO Files (id, path, size, organized_at)
            VALUES (1, '/tmp/v5-with-org.mp4', 10, CURRENT_TIMESTAMP);
            PRAGMA user_version = 5;
            ",
        )
        .expect("seed v5 with organized_at");
    }

    #[test]
    fn fresh_db_lands_at_schema_v8() {
        let db = open_memory_db();
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert_eq!(SCHEMA_USER_VERSION, 8);
        let n: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'PathCollisions'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(n, 1);
        assert!(
            files_has_column(&db, "pending_move_to"),
            "fresh DB must have pending_move_to"
        );
        let batches: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'OrganizeBatches'",
                [],
                |row| row.get(0),
            )
            .expect("batches");
        let items: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'OrganizeBatchItems'",
                [],
                |row| row.get(0),
            )
            .expect("items");
        assert_eq!(batches, 1, "fresh DB must have OrganizeBatches");
        assert_eq!(items, 1, "fresh DB must have OrganizeBatchItems");
    }

    #[test]
    fn v5_with_organized_at_upgrades_to_v7_path_collisions_and_pending() {
        let path = temp_db_path("v5-to-v7-path-collisions");
        write_v5_with_organized_at(&path);
        assert_eq!(raw_user_version(&path), 5);
        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert!(
            files_has_column(&db, "organized_at"),
            "organized_at must survive v5→v7"
        );
        assert!(
            files_has_column(&db, "pending_move_to"),
            "pending_move_to must land via v7"
        );
        assert!(
            db.is_organized_file(1).expect("organized"),
            "pre-existing organized_at preserved"
        );
        let n: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'PathCollisions'",
                [],
                |row| row.get(0),
            )
            .expect("path collisions");
        assert!(n > 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migration_6_failure_rolls_back_stays_at_v5() {
        let path = temp_db_path("v5-mig6-rollback");
        write_v5_with_organized_at(&path);
        {
            let conn = Connection::open(&path).expect("raw");
            conn.execute_batch("CREATE VIEW PathCollisions AS SELECT 1 AS file_id;")
                .expect("poison view");
        }
        match Database::new(path.to_str().expect("utf8")) {
            Ok(_) => panic!("mig should fail when PathCollisions name is a view"),
            Err(err) => {
                let msg = err.to_string().to_lowercase();
                assert!(
                    msg.contains("index") || msg.contains("view"),
                    "expected index/view error, got {msg}"
                );
            }
        }
        assert_eq!(raw_user_version(&path), 5, "failed mig must not bump version");
        let conn = Connection::open(&path).expect("reopen");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'PathCollisions'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(n, 0, "failed mig must not leave PathCollisions table");
        let _ = std::fs::remove_file(&path);
    }

    /// Populated schema-v6 DB (PathCollisions + several Files rows) for v6→v7 upgrade
    fn write_v6_populated(path: &Path) {
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE Files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER NOT NULL,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                phash TEXT,
                status TEXT NOT NULL DEFAULT 'ready',
                organized_at DATETIME
            );
            CREATE TABLE Metadata (
                file_id INTEGER PRIMARY KEY,
                title TEXT,
                studio TEXT,
                release_date TEXT,
                summary TEXT,
                cover_image TEXT,
                sprite_sheet_path TEXT,
                source_type TEXT,
                FOREIGN KEY(file_id) REFERENCES Files(id)
            );
            CREATE TABLE Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT
            );
            CREATE TABLE FileTags (
                file_id INTEGER NOT NULL,
                tag_id INTEGER NOT NULL,
                confidence REAL,
                PRIMARY KEY (file_id, tag_id),
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(tag_id) REFERENCES Tags(id)
            );
            CREATE TABLE ErrorLog (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                category TEXT NOT NULL,
                file_id INTEGER,
                path TEXT,
                message TEXT NOT NULL,
                detail TEXT
            );
            CREATE TABLE Jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                status TEXT NOT NULL CHECK (
                    status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')
                ),
                type TEXT NOT NULL CHECK (type IN ('pipeline', 'phash')),
                payload TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                max_attempts INTEGER NOT NULL DEFAULT 3,
                error TEXT,
                retryable INTEGER NOT NULL DEFAULT 1 CHECK (retryable IN (0, 1)),
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE SuspectedDuplicates (
                file_id INTEGER PRIMARY KEY,
                keeper_id INTEGER NOT NULL,
                distance INTEGER NOT NULL CHECK (distance BETWEEN 0 AND 64),
                resolution TEXT NOT NULL DEFAULT 'pending'
                    CHECK (resolution IN ('pending', 'confirmed', 'ignored')),
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(keeper_id) REFERENCES Files(id)
            );
            CREATE TABLE DuplicateTombstones (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                size INTEGER,
                phash TEXT,
                keeper_id INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE PathCollisions (
                file_id INTEGER PRIMARY KEY,
                desired_path TEXT NOT NULL,
                existing_path TEXT NOT NULL,
                occupant_file_id INTEGER,
                resolution TEXT NOT NULL DEFAULT 'pending'
                    CHECK (resolution IN ('pending', 'suffix', 'skip', 'overwrite')),
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(file_id) REFERENCES Files(id),
                FOREIGN KEY(occupant_file_id) REFERENCES Files(id)
            );
            INSERT INTO Files (id, path, size, phash, status, organized_at) VALUES
                (1, '/lib/a.mp4', 100, 'aaaaaaaaaaaaaaaa', 'ready', CURRENT_TIMESTAMP),
                (2, '/lib/b.mp4', 200, NULL, 'ready', NULL),
                (3, '/inbox/c.mp4', 300, 'bbbbbbbbbbbbbbbb', 'path_collision', NULL);
            INSERT INTO PathCollisions (file_id, desired_path, existing_path, occupant_file_id, resolution)
            VALUES (3, '/lib/a.mp4', '/lib/a.mp4', 1, 'pending');
            INSERT INTO Metadata (file_id, title, studio, release_date, summary)
            VALUES (1, 'Alpha', 'StudioA', '2024-01-15', 'seed summary');
            INSERT INTO Tags (id, name, category) VALUES (1, 'ActorOne', 'Actor');
            INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (1, 1, 0.95);
            PRAGMA user_version = 6;
            ",
        )
        .expect("seed v6 populated");
    }

    #[test]
    fn v6_populated_upgrades_to_v7_pending_move_to() {
        let path = temp_db_path("v6-to-v7-pending");
        write_v6_populated(&path);
        assert_eq!(raw_user_version(&path), 6);
        {
            let conn = Connection::open(&path).expect("raw");
            let cols: Vec<String> = {
                let mut stmt = conn.prepare("PRAGMA table_info(Files)").unwrap();
                stmt.query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .map(|c| c.unwrap())
                    .collect()
            };
            assert!(
                !cols.iter().any(|c| c == "pending_move_to"),
                "v6 seed must not already have pending_move_to"
            );
        }
        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert!(files_has_column(&db, "pending_move_to"));
        assert!(files_has_column(&db, "pending_source_vol"));
        assert!(files_has_column(&db, "pending_source_idx"));
        // Populated rows survive
        let a = db.get_file_by_id(1).expect("file 1");
        assert_eq!(a.path, normalize_db_path("/lib/a.mp4"));
        assert_eq!(a.size, 100);
        assert_eq!(db.get_pending_move_to(1).expect("pending"), None);
        let meta = db.get_metadata(1).expect("meta").expect("file 1 metadata");
        assert_eq!(meta.title.as_deref(), Some("Alpha"));
        assert_eq!(meta.studio.as_deref(), Some("StudioA"));
        assert_eq!(meta.release_date.as_deref(), Some("2024-01-15"));
        assert_eq!(meta.summary.as_deref(), Some("seed summary"));
        let tags = db.get_file_tags(1).expect("filetags");
        assert_eq!(tags.len(), 1, "FileTags must survive v6→v7");
        assert_eq!(tags[0].name, "ActorOne");
        assert_eq!(tags[0].category.as_deref(), Some("Actor"));
        let conf = filetag_confidence(&db, 1, tags[0].id);
        assert!((conf - 0.95).abs() < f64::EPSILON, "FileTags confidence={conf}");
        let collision = db
            .get_pending_path_collision(3)
            .expect("collision")
            .expect("pending collision");
        assert_eq!(collision.desired_path, normalize_db_path("/lib/a.mp4"));
        // Idempotent re-open
        drop(db);
        let db2 = Database::new(path.to_str().expect("utf8")).expect("reopen");
        assert_eq!(db2.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert!(files_has_column(&db2, "pending_move_to"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migration_7_is_idempotent_when_column_already_present() {
        let path = temp_db_path("v6-mig7-idempotent");
        write_v6_populated(&path);
        {
            let conn = Connection::open(&path).expect("raw");
            conn.execute("ALTER TABLE Files ADD COLUMN pending_move_to TEXT", [])
                .expect("pre-add column");
            // Stay at user_version 6 so migration_7 still runs
            assert_eq!(raw_user_version(&path), 6);
        }
        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert!(files_has_column(&db, "pending_move_to"));
        db.set_pending_move_to(2, "/lib/dest.mp4", None).expect("set");
        assert_eq!(
            db.get_pending_move_to(2).expect("get"),
            Some(normalize_db_path("/lib/dest.mp4"))
        );
        assert!(files_has_column(&db, "pending_source_vol"));
        assert!(files_has_column(&db, "pending_source_idx"));
        let _ = std::fs::remove_file(&path);
    }

    /// Mid-flight 7996aae shape: user_version=7 with pending_move_to only
    fn write_v7_without_source_fs_id_columns(path: &Path) {
        write_v6_populated(path);
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(
            "
            ALTER TABLE Files ADD COLUMN pending_move_to TEXT;
            CREATE INDEX IF NOT EXISTS idx_files_pending_move_to
                ON Files(pending_move_to)
                WHERE pending_move_to IS NOT NULL;
            PRAGMA user_version = 7;
            ",
        )
        .expect("seed v7 without source fs id columns");
    }

    #[test]
    fn v7_without_source_fs_id_columns_is_repaired_on_open() {
        let path = temp_db_path("v7-no-source-fs-id");
        write_v7_without_source_fs_id_columns(&path);
        assert_eq!(raw_user_version(&path), 7);
        {
            let conn = Connection::open(&path).expect("raw");
            let cols: Vec<String> = {
                let mut stmt = conn.prepare("PRAGMA table_info(Files)").unwrap();
                stmt.query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .map(|c| c.unwrap())
                    .collect()
            };
            assert!(
                cols.iter().any(|c| c == "pending_move_to"),
                "precondition: pending_move_to present"
            );
            assert!(
                !cols.iter().any(|c| c == "pending_source_vol"),
                "precondition: pending_source_vol absent"
            );
            assert!(
                !cols.iter().any(|c| c == "pending_source_idx"),
                "precondition: pending_source_idx absent"
            );
        }

        let db = Database::new(path.to_str().expect("utf8")).expect("open repair");
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert!(
            files_has_column(&db, "pending_source_vol"),
            "ensure_v7_columns must ADD pending_source_vol on already-v7 DBs"
        );
        assert!(
            files_has_column(&db, "pending_source_idx"),
            "ensure_v7_columns must ADD pending_source_idx on already-v7 DBs"
        );
        db.set_pending_move_to(
            2,
            "/lib/dest.mp4",
            Some(crate::path_template::FsFileId {
                volume: 1,
                index: 42,
            }),
        )
        .expect("set_pending_move_to must succeed after repair");
        assert_eq!(
            db.get_pending_move_to(2).expect("get"),
            Some(normalize_db_path("/lib/dest.mp4"))
        );
        let row = db
            .get_pending_move_row(2)
            .expect("row")
            .expect("pending row");
        assert_eq!(row.pending_source_vol, Some(1));
        assert_eq!(row.pending_source_idx, Some(42));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migration_7_failure_rolls_back_stays_at_v6() {
        // Poison index name as a table so CREATE INDEX fails inside mig7 tx
        let path = temp_db_path("v6-mig7-rollback");
        write_v6_populated(&path);
        {
            let conn = Connection::open(&path).expect("raw");
            conn.execute_batch("CREATE TABLE idx_files_pending_move_to (x INTEGER);")
                .expect("poison index name");
        }
        match Database::new(path.to_str().expect("utf8")) {
            Ok(_) => panic!("mig should fail when index name is taken by a table"),
            Err(err) => {
                let msg = err.to_string().to_lowercase();
                assert!(
                    msg.contains("index")
                        || msg.contains("already")
                        || msg.contains("table")
                        || msg.contains("exists"),
                    "expected index/name conflict error, got {msg}"
                );
            }
        }
        assert_eq!(raw_user_version(&path), 6, "failed mig must not bump version");
        {
            let conn = Connection::open(&path).expect("reopen");
            let cols: Vec<String> = {
                let mut stmt = conn.prepare("PRAGMA table_info(Files)").unwrap();
                stmt.query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .map(|c| c.unwrap())
                    .collect()
            };
            assert!(
                !cols.iter().any(|c| c == "pending_move_to"),
                "failed mig must not leave pending_move_to column"
            );
            assert!(
                !cols.iter().any(|c| c == "pending_source_vol"),
                "failed mig must not leave pending_source_vol column"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Full v7 shape (pending columns) for v7→v8 upgrade tests
    fn write_v7_pending_complete(path: &Path) {
        write_v6_populated(path);
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(
            "
            ALTER TABLE Files ADD COLUMN pending_move_to TEXT;
            ALTER TABLE Files ADD COLUMN pending_source_vol INTEGER;
            ALTER TABLE Files ADD COLUMN pending_source_idx INTEGER;
            CREATE INDEX IF NOT EXISTS idx_files_pending_move_to
                ON Files(pending_move_to)
                WHERE pending_move_to IS NOT NULL;
            PRAGMA user_version = 7;
            ",
        )
        .expect("seed v7 complete");
    }

    #[test]
    fn v7_upgrades_to_v8_organize_batches() {
        let path = temp_db_path("v7-to-v8-batches");
        write_v7_pending_complete(&path);
        assert_eq!(raw_user_version(&path), 7);
        let db = Database::new(path.to_str().expect("utf8")).expect("upgrade");
        assert_eq!(db.get_user_version().expect("uv"), SCHEMA_USER_VERSION);
        assert!(files_has_column(&db, "pending_move_to"));
        let batches: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'OrganizeBatches'",
                [],
                |row| row.get(0),
            )
            .expect("batches");
        let items: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'OrganizeBatchItems'",
                [],
                |row| row.get(0),
            )
            .expect("items");
        assert_eq!(batches, 1);
        assert_eq!(items, 1);
        // Populated v7 rows survive
        let a = db.get_file_by_id(1).expect("file 1");
        assert_eq!(a.path, normalize_db_path("/lib/a.mp4"));
        let batch_id = db
            .create_organize_batch("completed", Some("smoke"))
            .expect("create batch");
        assert!(batch_id > 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v8_without_batch_tables_is_repaired_on_open() {
        let path = temp_db_path("v8-no-tables");
        write_v7_pending_complete(&path);
        {
            let conn = Connection::open(&path).expect("raw");
            conn.execute_batch("PRAGMA user_version = 8;")
                .expect("stamp v8 without tables");
        }
        assert_eq!(raw_user_version(&path), 8);
        {
            let conn = Connection::open(&path).expect("raw");
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'table' AND name = 'OrganizeBatches'",
                    [],
                    |row| row.get(0),
                )
                .expect("count");
            assert_eq!(n, 0, "precondition: no OrganizeBatches");
        }
        let db = Database::new(path.to_str().expect("utf8")).expect("repair");
        assert_eq!(db.get_user_version().expect("uv"), 8);
        let n: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'OrganizeBatches'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(n, 1, "ensure_v8_tables must CREATE OrganizeBatches");
        let item_n: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'OrganizeBatchItems'",
                [],
                |row| row.get(0),
            )
            .expect("items");
        assert_eq!(item_n, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migration_8_failure_rolls_back_stays_at_v7() {
        let path = temp_db_path("v7-mig8-rollback");
        write_v7_pending_complete(&path);
        {
            let conn = Connection::open(&path).expect("raw");
            // Poison table name as a view so CREATE TABLE IF NOT EXISTS is a no-op
            // then CREATE INDEX on a view fails.
            conn.execute_batch(
                "CREATE VIEW OrganizeBatches AS SELECT 1 AS id, 'x' AS status;",
            )
            .expect("poison view");
        }
        match Database::new(path.to_str().expect("utf8")) {
            Ok(_) => panic!("mig should fail when OrganizeBatches name is a view"),
            Err(err) => {
                let msg = err.to_string().to_lowercase();
                assert!(
                    msg.contains("index") || msg.contains("view") || msg.contains("table"),
                    "expected index/view error, got {msg}"
                );
            }
        }
        assert_eq!(raw_user_version(&path), 7, "failed mig must not bump version");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migration_5_failure_rolls_back_user_version() {
        // Real failure mode: CREATE VIEW DuplicateTombstones poisons the name so
        // CREATE TABLE IF NOT EXISTS is a no-op, then CREATE INDEX fails with
        // "views may not be indexed" / similar — assert message mentions index|view.
        let path = temp_db_path("v4-mig5-rollback");
        write_v4_without_tombstones(&path);
        {
            let conn = Connection::open(&path).expect("raw");
            conn.execute_batch("CREATE VIEW DuplicateTombstones AS SELECT 1 AS id;")
                .expect("poison view");
        }
        match Database::new(path.to_str().expect("utf8")) {
            Ok(_) => panic!("mig should fail when DuplicateTombstones name is a view"),
            Err(err) => {
                let msg = err.to_string().to_lowercase();
                assert!(
                    msg.contains("index") || msg.contains("view"),
                    "expected index/view error, got {msg}"
                );
            }
        }
        assert_eq!(raw_user_version(&path), 4, "failed mig must not bump version");
        let conn = Connection::open(&path).expect("reopen");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'DuplicateTombstones'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(n, 0, "failed mig must not leave DuplicateTombstones table");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn jobs_status_check_rejects_pending_alias() {
        let db = open_memory_db();
        let err = db
            .insert_job(&NewJobRow {
                status: "pending",
                job_type: "pipeline",
                payload: "{}",
                attempts: 0,
                max_attempts: 3,
                error: None,
                retryable: true,
            })
            .expect_err("pending must fail CHECK");
        assert!(
            err.to_string().to_lowercase().contains("check")
                || err.to_string().contains("constraint"),
            "err={err}"
        );
    }

    #[test]
    fn reset_running_jobs_to_queued_and_prune_terminal() {
        let db = open_memory_db();
        let a = db
            .insert_job(&NewJobRow {
                status: "running",
                job_type: "pipeline",
                payload: "{\"file_id\":1}",
                attempts: 1,
                max_attempts: 3,
                error: None,
                retryable: true,
            })
            .expect("a");
        let b = db
            .insert_job(&NewJobRow {
                status: "queued",
                job_type: "phash",
                payload: "{\"file_id\":2,\"path\":\"/t\"}",
                attempts: 0,
                max_attempts: 3,
                error: None,
                retryable: true,
            })
            .expect("b");
        assert_eq!(db.reset_running_jobs_to_queued().expect("reset"), 1);
        assert_eq!(db.get_job(a).expect("a").status, "queued");
        assert_eq!(db.get_job(b).expect("b").status, "queued");

        for i in 0..5 {
            let payload = format!("{{\"file_id\":{i}}}");
            let _ = db
                .insert_job(&NewJobRow {
                    status: "succeeded",
                    job_type: "pipeline",
                    payload: &payload,
                    attempts: 1,
                    max_attempts: 3,
                    error: None,
                    retryable: false,
                })
                .expect("term");
        }
        let removed = db.prune_terminal_jobs(2).expect("prune");
        assert!(removed >= 3, "removed={removed}");
        let left = db
            .list_jobs_with_status(&["succeeded"])
            .expect("list")
            .len();
        assert_eq!(left, 2);
    }

    /// #61: native Path::join occupant must be found via rendered desired path
    #[test]
    fn find_file_occupying_path_matches_native_join_and_rendered() {
        let dir = std::env::temp_dir().join(format!(
            "idam_occ_native_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let occupied = dir.join("StudioA").join("clip.mp4");
        std::fs::create_dir_all(occupied.parent().unwrap()).unwrap();
        std::fs::write(&occupied, b"x").unwrap();

        let db = open_memory_db();
        let occupant = db
            .insert_file(occupied.to_str().unwrap(), 1, None)
            .expect("occ");
        let mover = db
            .insert_file(dir.join("incoming.mp4").to_str().unwrap(), 2, None)
            .expect("mover");

        let rendered = crate::path_template::render_output_path(
            "{{studio}}/{{filename}}",
            &crate::path_template::TemplateContext {
                studio: "StudioA".to_string(),
                date: "d".to_string(),
                title: "t".to_string(),
                number: "n".to_string(),
                filename: "clip.mp4".to_string(),
                ext: "mp4".to_string(),
                actor: String::new(),
            },
            &dir,
        )
        .expect("render");

        assert_eq!(rendered, occupied);
        #[cfg(windows)]
        {
            let s = rendered.to_string_lossy();
            assert!(!s.contains('/'), "rendered must be native: {s}");
        }

        let found = db
            .find_file_occupying_path(rendered.to_str().unwrap(), mover)
            .expect("query")
            .expect("occupant");
        assert_eq!(found.id, occupant);
        assert_eq!(PathBuf::from(&found.path), occupied);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn repair_mixed_separators_normalizes_and_logs_unique_conflict() {
        let db = open_memory_db();
        // Bypass upsert normalize to seed legacy mixed-separator rows
        db.conn
            .execute(
                "INSERT INTO Files (path, size, status) VALUES (?1, 1, 'ready')",
                params![r"C:\lib\StudioA\x.mp4"],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO Files (path, size, status) VALUES (?1, 2, 'ready')",
                params![r"C:\lib/StudioA\x.mp4"],
            )
            .unwrap();
        let mixed_id: i64 = db
            .conn
            .query_row(
                "SELECT id FROM Files WHERE path = ?1",
                params![r"C:\lib/StudioA\x.mp4"],
                |row| row.get(0),
            )
            .unwrap();

        db.repair_mixed_separator_paths().expect("repair");

        let mixed_still: String = db
            .conn
            .query_row(
                "SELECT path FROM Files WHERE id = ?1",
                params![mixed_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            mixed_still, r"C:\lib/StudioA\x.mp4",
            "conflict row must stay unchanged"
        );
        let logs = db.list_error_logs(10).expect("logs");
        assert!(
            logs.iter().any(|e| e.category == "path_normalize"
                && e.message.contains("UNIQUE")
                && e.file_id == Some(mixed_id)),
            "expected ErrorLog for UNIQUE conflict got {logs:?}"
        );
        let n_before = logs
            .iter()
            .filter(|e| e.category == "path_normalize")
            .count();
        // Second open/repair must not rewrite the same category+path ErrorLog
        db.repair_mixed_separator_paths().expect("repair again");
        let n_after = db
            .list_error_logs(10)
            .unwrap()
            .iter()
            .filter(|e| e.category == "path_normalize")
            .count();
        assert_eq!(n_after, n_before, "ErrorLog must not duplicate on reopen");
    }

    #[cfg(windows)]
    #[test]
    fn repair_mixed_separators_updates_when_safe() {
        let db = open_memory_db();
        db.conn
            .execute(
                "INSERT INTO Files (path, size, status) VALUES (?1, 1, 'ready')",
                params![r"c:/lib/only\y.mp4"],
            )
            .unwrap();
        db.repair_mixed_separator_paths().expect("repair");
        let path: String = db
            .conn
            .query_row("SELECT path FROM Files LIMIT 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(path, r"C:\lib\only\y.mp4");
        assert!(db.list_error_logs(5).unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn upsert_mixed_separator_hits_existing_native_row() {
        let db = open_memory_db();
        let native = r"C:\lib\StudioA\clip.mp4";
        let (id1, is_new1) = db.upsert_file(native, 10, None).expect("first");
        assert!(is_new1);
        let (id2, is_new2) = db
            .upsert_file(r"C:/lib/StudioA/clip.mp4", 20, None)
            .expect("mixed");
        assert!(!is_new2, "mixed-separator upsert must hit existing row");
        assert_eq!(id1, id2);
        assert_eq!(db.count_files().unwrap(), 1);
        let row = db.get_file_by_path(native).unwrap().unwrap();
        assert_eq!(row.path, r"C:\lib\StudioA\clip.mp4");
        assert_eq!(row.size, 20);
    }

    /// Open-time repair must fix lowercase drive; later upsert must hit same row.
    #[cfg(windows)]
    #[test]
    fn open_repairs_lowercase_drive_then_upsert_hits_same_row() {
        let path = temp_db_path("lowercase-drive-repair");
        let _ = std::fs::remove_file(&path);
        {
            let db = Database::new(path.to_str().expect("utf8")).expect("create");
            db.conn
                .execute(
                    "INSERT INTO Files (path, size, status) VALUES (?1, 1, 'ready')",
                    params![r"c:\lib\x.mp4"],
                )
                .expect("seed lowercase drive");
            let stored: String = db
                .conn
                .query_row("SELECT path FROM Files LIMIT 1", [], |row| row.get(0))
                .unwrap();
            assert_eq!(stored, r"c:\lib\x.mp4", "precondition: raw SQL keeps lowercase");
        }
        let db = Database::new(path.to_str().expect("utf8")).expect("reopen");
        let row = db
            .get_file_by_path(r"C:\lib\x.mp4")
            .expect("get")
            .expect("row after repair");
        assert_eq!(row.path, r"C:\lib\x.mp4");
        let (id2, is_new) = db
            .upsert_file(r"C:\lib\x.mp4", 9, None)
            .expect("upsert");
        assert!(!is_new, "upsert after repair must hit the same row");
        assert_eq!(id2, row.id);
        assert_eq!(db.count_files().unwrap(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(windows)]
    #[test]
    fn repair_path_collisions_requeries_occupant_file_id() {
        let db = open_memory_db();
        let desired_native = r"C:\lib\StudioA\out.mp4";
        let occupant = db
            .insert_file(desired_native, 1, None)
            .expect("occupant");
        let mover = db
            .insert_file(r"C:\lib\incoming.mp4", 2, None)
            .expect("mover");
        db.conn
            .execute(
                "INSERT INTO PathCollisions
                    (file_id, desired_path, existing_path, occupant_file_id, resolution)
                 VALUES (?1, ?2, ?3, NULL, 'pending')",
                params![mover, r"C:\lib/StudioA/out.mp4", r"C:\lib/StudioA/out.mp4"],
            )
            .unwrap();
        db.repair_mixed_separator_paths().expect("repair");
        let (desired, existing, occ): (String, String, Option<i64>) = db
            .conn
            .query_row(
                "SELECT desired_path, existing_path, occupant_file_id
                 FROM PathCollisions WHERE file_id = ?1",
                params![mover],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(desired, desired_native);
        assert_eq!(existing, desired_native);
        assert_eq!(occ, Some(occupant));
    }

    /// #63: trailing-separator repair is idempotent and skips the write path when clean
    #[test]
    fn repair_trailing_separator_paths_idempotent() {
        let db = open_memory_db();
        #[cfg(windows)]
        let (raw, expect) = (r"C:\lib\StudioA\", r"C:\lib\StudioA");
        #[cfg(not(windows))]
        let (raw, expect) = ("/lib/StudioA/", "/lib/StudioA");
        db.conn
            .execute(
                "INSERT INTO Files (path, size, status) VALUES (?1, 1, 'ready')",
                params![raw],
            )
            .unwrap();
        assert!(db.needs_path_separator_repair().unwrap());
        db.repair_mixed_separator_paths().expect("repair");
        let path: String = db
            .conn
            .query_row("SELECT path FROM Files LIMIT 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(path, expect);
        assert!(!db.needs_path_separator_repair().unwrap());
        db.repair_mixed_separator_paths().expect("second repair no-op");
        assert!(db.list_error_logs(5).unwrap().is_empty());
    }

    /// #63 macOS: NFC repair + UNIQUE conflict leaves the row and logs once
    #[cfg(target_os = "macos")]
    #[test]
    fn repair_nfc_unique_conflict_logs_and_leaves_row() {
        let db = open_memory_db();
        let nfd = "cafe\u{0301}";
        let nfc = "caf\u{e9}";
        let nfd_path = format!("/lib/{nfd}/x.mp4");
        let nfc_path = format!("/lib/{nfc}/x.mp4");
        db.conn
            .execute(
                "INSERT INTO Files (path, size, status) VALUES (?1, 1, 'ready')",
                params![nfc_path.as_str()],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO Files (path, size, status) VALUES (?1, 2, 'ready')",
                params![nfd_path.as_str()],
            )
            .unwrap();
        let nfd_id: i64 = db
            .conn
            .query_row(
                "SELECT id FROM Files WHERE path = ?1",
                params![nfd_path.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        db.repair_mixed_separator_paths().expect("repair");
        let still: String = db
            .conn
            .query_row(
                "SELECT path FROM Files WHERE id = ?1",
                params![nfd_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(still, nfd_path, "UNIQUE conflict must leave NFD row unchanged");
        let logs = db.list_error_logs(10).unwrap();
        assert!(
            logs.iter()
                .any(|e| e.category == "path_normalize" && e.file_id == Some(nfd_id)),
            "expected path_normalize ErrorLog got {logs:?}"
        );
    }

    /// #63: on Linux/Windows, NFD on-disk names must stay byte-identical after upsert+repair
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn nfd_on_disk_path_survives_upsert_and_repair() {
        let nfd = "cafe\u{0301}";
        let nfc = "caf\u{e9}";
        assert_ne!(nfd.as_bytes(), nfc.as_bytes());

        let dir = {
            let mut d = std::env::temp_dir();
            d.push(format!(
                "idam-nfd-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let file_path = dir.join(format!("{nfd}.mp4"));
        std::fs::write(&file_path, b"nfd-bytes").unwrap();
        assert!(file_path.is_file(), "NFD file must exist on disk");

        let path_str = file_path.to_str().expect("utf8 path");
        assert!(
            path_str.contains(nfd),
            "temp path should carry NFD codepoints: {path_str}"
        );

        let db = open_memory_db();
        let (id, is_new) = db.upsert_file(path_str, 9, None).expect("upsert");
        assert!(is_new);
        let stored = db.get_file_by_path(path_str).unwrap().unwrap();
        assert_eq!(
            stored.path, path_str,
            "upsert must keep NFD codepoints on non-macOS"
        );
        assert!(
            std::path::Path::new(&stored.path).is_file(),
            "stored path must still resolve on disk"
        );

        assert!(
            !db.needs_path_separator_repair().unwrap(),
            "NFD-only path must not trigger repair on non-macOS"
        );
        db.repair_mixed_separator_paths().expect("repair");
        let after = db.get_file_by_id(id).expect("get after repair");
        assert_eq!(after.path, path_str, "repair must not NFC-convert on non-macOS");
        assert!(
            std::path::Path::new(&after.path).is_file(),
            "path after repair must still exist"
        );
        assert!(db.list_error_logs(5).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #63 S-1: clean Windows Volume path must not trip needs_path_separator_repair
    #[cfg(windows)]
    #[test]
    fn needs_repair_false_for_windows_volume_path() {
        let db = open_memory_db();
        let volume = r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\lib\x.mp4";
        assert_eq!(
            normalize_db_path(volume),
            volume,
            "precondition: Volume path already normalized"
        );
        db.conn
            .execute(
                "INSERT INTO Files (path, size, status) VALUES (?1, 1, 'ready')",
                params![volume],
            )
            .unwrap();
        assert!(
            !db.needs_path_separator_repair().unwrap(),
            "Volume path must not request repair"
        );
    }

    /// #63 S-1: Windows drive root `C:\` must not trip needs_path_separator_repair
    #[cfg(windows)]
    #[test]
    fn needs_repair_false_for_windows_drive_root() {
        let db = open_memory_db();
        let root = r"C:\";
        assert_eq!(normalize_db_path(root), root);
        db.conn
            .execute(
                "INSERT INTO Files (path, size, status) VALUES (?1, 1, 'ready')",
                params![root],
            )
            .unwrap();
        assert!(
            !db.needs_path_separator_repair().unwrap(),
            "drive root must not request repair"
        );
    }

    /// #63 S-1: Linux filename ending in `\` is a real name — not a trailing-sep dirty signal
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn needs_repair_false_for_linux_filename_ending_with_backslash() {
        let db = open_memory_db();
        // Unusual but legal on ext4; must not be treated as a Windows trailing sep
        let path = "/lib/ends-with-backslash\\";
        assert_eq!(
            normalize_db_path(path),
            path,
            "precondition: Linux keeps trailing backslash codepoint"
        );
        db.conn
            .execute(
                "INSERT INTO Files (path, size, status) VALUES (?1, 1, 'ready')",
                params![path],
            )
            .unwrap();
        assert!(
            !db.needs_path_separator_repair().unwrap(),
            "Linux path ending in '\\' must not request repair"
        );
    }
}

