use crate::file_status;
use crate::phash;
use rusqlite::{Connection, Result as SqliteResult, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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
pub struct FileWithMetadata {
    // 保持巢狀 shape 與前端 FileWithMetadata.file / metadata / tags 對齊
    // 勿使用 flatten 否則 invoke JSON 會少掉 file 物件
    pub file: FileRecord,
    pub metadata: Option<Metadata>,
    pub tags: Vec<Tag>,
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

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn new(db_path: &str) -> SqliteResult<Self> {
        let conn = Connection::open(db_path)?;
        let db = Database { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> SqliteResult<()> {
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

    /// 單一交易 bulk 清除 orphan 若有刪除則寫一筆 ErrorLog 摘要
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

        let need_filetags_files = has_filetags && has_files;
        let need_metadata = has_metadata && has_files;
        if !need_filetags_files && !need_metadata && !clean_filetags_tags {
            return Ok(());
        }

        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let result = (|| -> SqliteResult<()> {
            let filetags_missing_files = if need_filetags_files {
                self.conn.execute(
                    "DELETE FROM FileTags WHERE file_id NOT IN (SELECT id FROM Files)",
                    [],
                )?
            } else {
                0
            };
            let metadata_missing_files = if need_metadata {
                self.conn.execute(
                    "DELETE FROM Metadata WHERE file_id NOT IN (SELECT id FROM Files)",
                    [],
                )?
            } else {
                0
            };
            let filetags_missing_tags = if clean_filetags_tags {
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
        let existed = self.get_file_by_path(path)?.is_some();
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
        let id = match self.get_file_by_path(path)? {
            Some(f) => f.id,
            None => self.conn.last_insert_rowid(),
        };
        Ok((id, !existed))
    }

    /// Insert or update by path; returns the stable row id (path-unique).
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
    pub fn count_files(&self) -> SqliteResult<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM Files", [], |row| row.get(0))
    }

    /// Find another Files row whose phash is within `max_distance` of `phash_hex`.
    /// Prefers the smallest id (oldest keeper) among matches.
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

    /// Remove a provisional Files row and its Metadata / FileTags (phash skip).
    pub fn delete_file_record(&self, file_id: i64) -> SqliteResult<()> {
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
        // 以 (name, category) 為鍵 IS 可正確比對 NULL category
        match self.conn.query_row(
            "SELECT id FROM Tags WHERE name = ?1 AND category IS ?2",
            params![name, category],
            |row| row.get(0),
        ) {
            Ok(id) => Ok(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.conn.execute(
                    "INSERT INTO Tags (name, category) VALUES (?1, ?2)",
                    params![name, category],
                )?;
                Ok(self.conn.last_insert_rowid())
            }
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

            result.push(FileWithMetadata {
                file,
                metadata,
                tags,
            });
        }
        Ok(result)
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

    /// File ids eligible for one-click / batch organize (not missing)
    pub fn list_organizable_file_ids(&self) -> SqliteResult<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM Files WHERE status != ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![file_status::missing], |row| row.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
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

    pub fn update_file_path(&self, file_id: i64, new_path: &str) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE Files SET path = ?1 WHERE id = ?2",
            params![new_path, file_id],
        )?;
        Ok(())
    }

    /// 刮削成功後在同一交易寫入 metadata tags 與 status
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

    fn open_memory_db() -> Database {
        Database::new(":memory:").expect("open memory db")
    }

    fn foreign_keys_enabled(db: &Database) -> i64 {
        db.conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("pragma foreign_keys")
    }

    fn ensure_temp_tests_dir() -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.temp/tests");
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
        assert_eq!(meta, 0);
        assert_eq!(ft, 0);
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
}

