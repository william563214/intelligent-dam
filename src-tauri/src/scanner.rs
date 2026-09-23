use crate::database::Database;
use crate::phash;
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
#[allow(non_upper_case_globals)]
const video_extensions: &[&str] = &[
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
                if video_extensions.contains(&ext_str.to_lowercase().as_str()) {
                    let metadata = std::fs::metadata(path)
                        .with_context(|| format!("Failed to read metadata for {:?}", path))?;

                    files.push(ScannedFile {
                        path: path.to_path_buf(),
                        size: metadata.len(),
                        phash: None, // 於 import 時計算
                    });
                }
            }
        }
    }

    Ok(files)
}

/// Compute perceptual hash for a video file.
/// Failures are returned to the caller so import can continue with null phash.
pub fn calculate_phash(file_path: &Path) -> Result<String> {
    phash::calculate_video_phash(file_path)
}

pub fn import_files(db: &Database, files: Vec<ScannedFile>) -> Result<Vec<i64>> {
    let mut imported_ids = Vec::new();

    for file in files {
        let phash = match &file.phash {
            Some(existing) if !existing.trim().is_empty() => Some(existing.clone()),
            _ => match calculate_phash(&file.path) {
                Ok(hash) => Some(hash),
                Err(e) => {
                    eprintln!(
                        "phash skipped for {:?}: {:#} (import continues without phash)",
                        file.path, e
                    );
                    None
                }
            },
        };

        match db.insert_file(file.path.to_str().unwrap(), file.size, phash) {
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

pub fn scan_and_import<P: AsRef<Path>>(db: &Database, path: P) -> Result<Vec<i64>> {
    let scanned_files = scan_directory(path)?;
    import_files(db, scanned_files)
}

/// 掃描多個目錄並匯入
pub fn scan_multiple_directories(db: &Database, paths: Vec<String>) -> Result<Vec<i64>> {
    let mut all_imported_ids = Vec::new();

    for path in paths {
        match scan_and_import(db, &path) {
            Ok(ids) => {
                all_imported_ids.extend(ids);
            }
            Err(e) => {
                eprintln!("Failed to scan directory {}: {}", path, e);
                // 繼續掃描其他目錄，不中斷整個流程
            }
        }
    }

    Ok(all_imported_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn open_memory_db() -> Database {
        Database::new(":memory:").expect("open memory db")
    }

    #[test]
    fn import_records_phash_when_computable() {
        if !crate::ffmpeg::check_ffmpeg_available() {
            eprintln!("skip: ffmpeg unavailable");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "idam-scan-phash-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        let video = dir.join("sample.mp4");

        let status = Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=10",
                "-t",
                "2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&video)
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success());

        let db = open_memory_db();
        let size = fs::metadata(&video).unwrap().len();
        let ids = import_files(
            &db,
            vec![ScannedFile {
                path: video.clone(),
                size,
                phash: None,
            }],
        )
        .expect("import");

        assert_eq!(ids.len(), 1);
        let record = db.get_file_by_id(ids[0]).expect("get file");
        let hash = record.phash.expect("phash should be set");
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_continues_when_phash_fails() {
        let db = open_memory_db();
        let missing = PathBuf::from("/tmp/idam-missing-for-phash-scan.mp4");
        let ids = import_files(
            &db,
            vec![ScannedFile {
                path: missing.clone(),
                size: 1,
                phash: None,
            }],
        )
        .expect("import should not fail");

        // insert may succeed with null phash even for missing path (we don't open the file for insert)
        assert_eq!(ids.len(), 1);
        let record = db.get_file_by_id(ids[0]).expect("get file");
        assert!(record.phash.is_none());
    }

    #[test]
    fn import_preserves_precomputed_phash() {
        let db = open_memory_db();
        let ids = import_files(
            &db,
            vec![ScannedFile {
                path: PathBuf::from("/tmp/prehashed.mp4"),
                size: 42,
                phash: Some("abcdef0123456789".to_string()),
            }],
        )
        .expect("import");
        let record = db.get_file_by_id(ids[0]).expect("get");
        assert_eq!(record.phash.as_deref(), Some("abcdef0123456789"));
    }
}
