use rusqlite::{Connection, Result as SqliteResult, params};
use serde::{Deserialize, Serialize};

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
                status TEXT DEFAULT 'ready'
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

        // 標籤表
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS Tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                category TEXT
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

        Ok(())
    }

    pub fn insert_file(
        &self,
        path: &str,
        size: u64,
        phash: Option<String>,
    ) -> SqliteResult<i64> {
        self.conn.execute(
            "INSERT INTO Files (path, size, phash, status) VALUES (?1, ?2, ?3, 'ready')
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               phash = COALESCE(excluded.phash, Files.phash)",
            params![path, size as i64, phash],
        )?;
        Ok(self.conn.last_insert_rowid())
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
        // 嘗試獲取現有標籤
        match self.conn.query_row(
            "SELECT id FROM Tags WHERE name = ?1",
            params![name],
            |row| row.get(0),
        ) {
            Ok(id) => Ok(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                // 創建新標籤
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

    pub fn update_file_status(&self, file_id: i64, status: &str) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE Files SET status = ?1 WHERE id = ?2",
            params![status, file_id],
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
mod tests {
    use super::*;

    fn open_memory_db() -> Database {
        Database::new(":memory:").expect("open memory db")
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

        db.update_file_status(file_id, "scrape_failed")
            .expect("update status");
        let file = db.get_file_by_id(file_id).expect("get file");
        assert_eq!(file.status, "scrape_failed");
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
}

