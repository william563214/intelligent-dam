use crate::database::{Database, FileRecord};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub size: u64,
    pub phash: Option<String>,
}

// 支援的影片副檔名
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg",
];

pub fn scan_directory<P: AsRef<Path>>(path: P) -> Result<Vec<ScannedFile>> {
    let mut files = Vec::new();

    for entry in WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        
        // 檢查是否為影片檔案
        if let Some(ext) = path.extension() {
            if let Some(ext_str) = ext.to_str() {
                if VIDEO_EXTENSIONS.contains(&ext_str.to_lowercase().as_str()) {
                    let metadata = std::fs::metadata(path)
                        .with_context(|| format!("Failed to read metadata for {:?}", path))?;
                    
                    files.push(ScannedFile {
                        path: path.to_path_buf(),
                        size: metadata.len(),
                        phash: None, // 將在後續步驟計算
                    });
                }
            }
        }
    }

    Ok(files)
}

pub fn calculate_phash(file_path: &Path) -> Result<String> {
    // 使用簡單的檔案大小和修改時間作為快速 Hash
    // 未來可以整合真正的感知 Hash 庫
    let metadata = std::fs::metadata(file_path)
        .with_context(|| format!("Failed to read metadata for {:?}", file_path))?;
    
    let modified = metadata.modified()
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    
    // 簡單的 Hash：size + modified_time
    let hash_input = format!("{}_{}", metadata.len(), modified);
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    hash_input.hash(&mut hasher);
    Ok(format!("{:x}", hasher.finish()))
}

pub fn import_files(db: &Database, files: Vec<ScannedFile>) -> Result<Vec<i64>> {
    let mut imported_ids = Vec::new();

    for file in files {
        // 計算 Hash（如果還沒有）
        let phash = if file.phash.is_none() {
            calculate_phash(&file.path).ok()
        } else {
            file.phash.clone()
        };

        // 插入資料庫
        match db.insert_file(
            file.path.to_str().unwrap(),
            file.size,
            phash,
        ) {
            Ok(id) => {
                imported_ids.push(id);
            }
            Err(e) => {
                eprintln!("Failed to import file {:?}: {}", file.path, e);
                // 繼續處理其他檔案
            }
        }
    }

    Ok(imported_ids)
}

pub fn scan_and_import<P: AsRef<Path>>(
    db: &Database,
    path: P,
) -> Result<Vec<i64>> {
    let scanned_files = scan_directory(path)?;
    import_files(db, scanned_files)
}

