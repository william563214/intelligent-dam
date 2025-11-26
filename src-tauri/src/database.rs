use rusqlite::{Connection, Result as SqliteResult, params};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    #[serde(flatten)]
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
             ON CONFLICT(path) DO UPDATE SET size = ?2, phash = ?3",
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
}

