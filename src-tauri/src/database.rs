use crate::file_status;
use crate::phash;
use rusqlite::{Connection, Result as SqliteResult, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

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

        self.migrate_tags_unique_to_name_category()?;

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

    fn foreign_key_check_ok(&self) -> SqliteResult<()> {
        let mut stmt = self.conn.prepare("PRAGMA foreign_key_check")?;
        let mut rows = stmt.query([])?;
        if rows.next()?.is_some() {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::ConstraintViolation,
                    extended_code: 787,
                },
                Some("foreign_key_check failed after Tags migration".to_string()),
            ));
        }
        Ok(())
    }

    /// 既有 DB 若仍為 name 唯一 則重建為 (name, category) 唯一
    /// 整段在單一交易內完成 失敗則 ROLLBACK 並保留原 Tags
    fn migrate_tags_unique_to_name_category(&self) -> SqliteResult<()> {
        // 舊版非交易遷移若在 DROP 後 RENAME 前中斷 資料會困在 Tags_mig
        if !self.table_exists("Tags")? && self.table_exists("Tags_mig")? {
            self.conn
                .execute("ALTER TABLE Tags_mig RENAME TO Tags", [])?;
        }

        let table_sql = match self.tags_table_sql()? {
            Some(sql) => sql,
            None => return Ok(()),
        };

        if Self::tags_schema_is_name_category_unique(&table_sql) {
            // 已是新 schema 時清掉先前失敗留下的空 Tags_mig
            if self.table_exists("Tags_mig")? {
                self.conn.execute("DROP TABLE Tags_mig", [])?;
            }
            return Ok(());
        }

        // PRAGMA foreign_keys 在交易內無效 必須在 BEGIN 之前關閉
        let foreign_keys_on: i64 = self
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        self.conn.execute("PRAGMA foreign_keys = OFF", [])?;

        let migrate_result = self.run_tags_unique_migration_tx();

        let restore_result = if foreign_keys_on != 0 {
            self.conn.execute("PRAGMA foreign_keys = ON", []).map(|_| ())
        } else {
            Ok(())
        };

        match (migrate_result, restore_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(e)) => Err(e),
            (Err(e), Ok(())) => Err(e),
            (Err(_migrate), Err(restore)) => Err(restore),
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
            self.foreign_key_check_ok()?;
            self.conn.execute("COMMIT", [])?;
            Ok(())
        })();

        match tx_result {
            Ok(()) => Ok(()),
            Err(e) => {
                // 交易已失敗時 ROLLBACK 清掉未完成變更
                self.conn.execute("ROLLBACK", [])?;
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

    fn foreign_key_check_empty(db: &Database) {
        let mut stmt = db
            .conn
            .prepare("PRAGMA foreign_key_check")
            .expect("prepare fk check");
        let mut rows = stmt.query([]).expect("fk check");
        assert!(rows.next().expect("fk row").is_none());
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
    fn migrate_legacy_tags_preserves_filetags_and_allows_same_name_categories() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/legacy-tags.mp4", 128, None)
            .expect("insert file");

        // 先建 legacy schema 再綁定 file_id 驗證遷移後 FileTags 仍指向原 id
        seed_legacy_tags_schema(&db, 42, "Shared", "Tag");
        db.conn
            .execute(
                "INSERT INTO FileTags (file_id, tag_id, confidence) VALUES (?1, 42, 1.0)",
                params![file_id],
            )
            .expect("seed FileTags");

        db.migrate_tags_unique_to_name_category()
            .expect("migrate");

        let linked = db.get_file_tags(file_id).expect("get tags");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, 42);
        assert_eq!(linked[0].name, "Shared");
        assert_eq!(linked[0].category.as_deref(), Some("Tag"));

        let tag_id = db
            .get_or_create_tag("Shared", Some("Tag".to_string()))
            .expect("existing Tag");
        let actor_id = db
            .get_or_create_tag("Shared", Some("Actor".to_string()))
            .expect("new Actor");

        assert_eq!(tag_id, 42);
        assert_ne!(tag_id, actor_id);
        assert_eq!(tag_row(&db, actor_id).category.as_deref(), Some("Actor"));
    }

    #[test]
    fn migrate_tags_unique_is_idempotent() {
        let db = open_memory_db();
        seed_legacy_tags_schema(&db, 1, "Shared", "Tag");

        db.migrate_tags_unique_to_name_category()
            .expect("first migrate");
        let sql_after_first = tags_table_sql(&db);
        db.migrate_tags_unique_to_name_category()
            .expect("second migrate");
        let sql_after_second = tags_table_sql(&db);

        assert_eq!(sql_after_first, sql_after_second);
        assert!(Database::tags_schema_is_name_category_unique(
            &sql_after_second
        ));

        let tag_id = db
            .get_or_create_tag("Shared", Some("Tag".to_string()))
            .expect("existing");
        let again = db
            .get_or_create_tag("Shared", Some("Tag".to_string()))
            .expect("again");
        assert_eq!(tag_id, again);
    }

    #[test]
    fn migrate_with_foreign_keys_on_preserves_filetags_and_restores_fk() {
        let db = open_memory_db();
        let file_id = db
            .insert_file("/tmp/fk-on-migrate.mp4", 256, None)
            .expect("insert file");

        // 置換 legacy schema 時暫時關 FK 再打開 模擬正式環境 FK ON
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
        foreign_key_check_empty(&db);

        let linked = db.get_file_tags(file_id).expect("get tags");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, 42);
        assert_eq!(linked[0].name, "Shared");

        let actor_id = db
            .get_or_create_tag("Shared", Some("Actor".to_string()))
            .expect("new Actor");
        assert_ne!(actor_id, 42);
    }

    #[test]
    fn migrate_rolls_back_on_mid_failure_and_keeps_legacy_tags() {
        let db = open_memory_db();
        seed_legacy_tags_schema(&db, 7, "KeepMe", "Tag");
        let before_sql = tags_table_sql(&db);
        assert!(!Database::tags_schema_is_name_category_unique(&before_sql));

        migrate_tags_fault::arm_after_insert();
        let err = db
            .migrate_tags_unique_to_name_category()
            .expect_err("injected fault");
        migrate_tags_fault::clear();
        assert!(err.to_string().contains("injected migrate fault"));

        assert!(!db.table_exists("Tags_mig").expect("Tags_mig check"));
        let after_sql = tags_table_sql(&db);
        assert!(!Database::tags_schema_is_name_category_unique(&after_sql));
        let kept = tag_row(&db, 7);
        assert_eq!(kept.name, "KeepMe");
        assert_eq!(kept.category.as_deref(), Some("Tag"));

        // 失敗後可重新遷移成功
        db.migrate_tags_unique_to_name_category()
            .expect("retry migrate");
        assert!(Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
        assert_eq!(tag_row(&db, 7).name, "KeepMe");
    }

    #[test]
    fn migrate_succeeds_with_leftover_tags_mig() {
        let db = open_memory_db();
        seed_legacy_tags_schema(&db, 9, "Real", "Tag");

        // 模擬先前非交易遷移 INSERT 後失敗留下的殘表
        db.conn
            .execute_batch(
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

        db.migrate_tags_unique_to_name_category()
            .expect("migrate with leftover");

        assert!(!db.table_exists("Tags_mig").expect("Tags_mig gone"));
        assert!(Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
        assert_eq!(tag_row(&db, 9).name, "Real");
        assert!(db.conn
            .query_row(
                "SELECT COUNT(*) FROM Tags WHERE id = 99",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count stale")
            == 0);
    }

    #[test]
    fn migrate_recovers_tags_mig_when_tags_table_missing() {
        let db = open_memory_db();
        // 模擬 DROP Tags 後 RENAME 前中斷 資料只在 Tags_mig
        db.conn
            .execute_batch(
                "
                DROP TABLE Tags;
                CREATE TABLE Tags_mig (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    category TEXT,
                    UNIQUE(name, category)
                );
                INSERT INTO Tags_mig (id, name, category) VALUES (11, 'Recovered', 'Actor');
                ",
            )
            .expect("seed orphan Tags_mig");

        db.migrate_tags_unique_to_name_category()
            .expect("recover");

        assert!(!db.table_exists("Tags_mig").expect("Tags_mig gone"));
        assert!(db.table_exists("Tags").expect("Tags restored"));
        assert!(Database::tags_schema_is_name_category_unique(
            &tags_table_sql(&db)
        ));
        assert_eq!(tag_row(&db, 11).name, "Recovered");
        assert_eq!(tag_row(&db, 11).category.as_deref(), Some("Actor"));
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

