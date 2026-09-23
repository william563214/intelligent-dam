use crate::database::Database;
use crate::file_status;
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

/// File row that still needs ffmpeg phash (computed outside DB lock).
#[derive(Debug, Clone)]
pub struct PhashJob {
    pub file_id: i64,
    pub path: PathBuf,
}

// 支援的影片副檔名
#[allow(non_upper_case_globals)]
const video_extensions: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg",
];

/// True when path has a known video extension (case-insensitive).
pub fn is_video_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| video_extensions.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Walk a directory and collect video files. No DB and no ffmpeg.
pub fn scan_directory<P: AsRef<Path>>(path: P) -> Result<Vec<ScannedFile>> {
    let mut files = Vec::new();

    for entry in WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();

        if is_video_path(path) {
            let metadata = std::fs::metadata(path)
                .with_context(|| format!("Failed to read metadata for {:?}", path))?;

            files.push(ScannedFile {
                path: path.to_path_buf(),
                size: metadata.len(),
                phash: None,
            });
        }
    }

    Ok(files)
}

/// Walk multiple directories. Continues on per-directory errors.
pub fn scan_directories(paths: &[String]) -> Vec<ScannedFile> {
    let mut all = Vec::new();
    for path in paths {
        match scan_directory(path) {
            Ok(files) => all.extend(files),
            Err(e) => {
                eprintln!("Failed to scan directory {}: {}", path, e);
            }
        }
    }
    all
}

/// Compute perceptual hash for a video file (ffmpeg). Safe outside DB lock.
pub fn calculate_phash(file_path: &Path) -> Result<String> {
    phash::calculate_video_phash(file_path)
}

/// Short-lock friendly insert: write rows without waiting on ffmpeg.
/// Returns imported ids and jobs for files that still lack a phash.
pub fn import_files_pending_phash(
    db: &Database,
    files: &[ScannedFile],
) -> Result<(Vec<i64>, Vec<PhashJob>)> {
    let mut imported_ids = Vec::new();
    let mut jobs = Vec::new();

    for file in files {
        let path_str = match file.path.to_str() {
            Some(s) => s,
            None => {
                eprintln!("Failed to import file {:?}: non-utf8 path", file.path);
                continue;
            }
        };

        let insert_phash = file
            .phash
            .clone()
            .filter(|h| !h.trim().is_empty());

        if let Err(e) = db.insert_file(path_str, file.size, insert_phash) {
            eprintln!("Failed to import file {:?}: {}", file.path, e);
            continue;
        }

        let record = match db.get_file_by_path(path_str) {
            Ok(Some(r)) => r,
            Ok(None) => {
                eprintln!("Failed to read back imported file {}", path_str);
                continue;
            }
            Err(e) => {
                eprintln!("Failed to read back imported file {}: {}", path_str, e);
                continue;
            }
        };

        imported_ids.push(record.id);

        let needs_phash = record
            .phash
            .as_ref()
            .map(|h| h.trim().is_empty())
            .unwrap_or(true);
        if needs_phash {
            // scrape_failed 不可因算 phash 被洗成 processing→ready
            if file_status::allows_transient_processing(&record.status) {
                if let Err(e) = db.update_file_status(record.id, file_status::processing) {
                    eprintln!(
                        "Failed to mark processing for {}: {}",
                        path_str, e
                    );
                }
            }
            jobs.push(PhashJob {
                file_id: record.id,
                path: file.path.clone(),
            });
        }
    }

    Ok((imported_ids, jobs))
}

/// Run ffmpeg phash for pending jobs. No DB access.
pub fn compute_phash_jobs(jobs: Vec<PhashJob>) -> Vec<(i64, Option<String>)> {
    let mut results = Vec::with_capacity(jobs.len());
    for job in jobs {
        let hash = match calculate_phash(&job.path) {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!(
                    "phash skipped for {:?}: {:#} (import continues without phash)",
                    job.path, e
                );
                None
            }
        };
        results.push((job.file_id, hash));
    }
    results
}

/// Short-lock friendly write-back of computed phashes.
/// 僅清除 processing→ready 不會覆寫 scrape_failed
pub fn write_phashes(db: &Database, results: &[(i64, Option<String>)]) -> Result<()> {
    for (file_id, phash) in results {
        if let Some(hash) = phash {
            if let Err(e) = db.update_file_phash(*file_id, hash) {
                eprintln!("Failed to write phash for file_id {}: {}", file_id, e);
            }
        }
        match db.get_file_by_id(*file_id) {
            Ok(file) => {
                if file.status == file_status::processing {
                    let next = file_status::after_transient_work(file_status::processing);
                    if let Err(e) = db.update_file_status(*file_id, next) {
                        eprintln!(
                            "Failed to clear processing for file_id {}: {}",
                            file_id, e
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to read file_id {} after phash: {}", file_id, e);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn open_memory_db() -> Database {
        Database::new(":memory:").expect("open memory db")
    }

    fn make_test_clip(dir: &Path) -> PathBuf {
        let video = dir.join("sample.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
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
        video
    }

    #[test]
    fn is_video_path_accepts_common_extensions() {
        assert!(is_video_path(Path::new("/tmp/a.mp4")));
        assert!(is_video_path(Path::new("/tmp/a.MKV")));
        assert!(!is_video_path(Path::new("/tmp/a.txt")));
        assert!(!is_video_path(Path::new("/tmp/noext")));
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
        let video = make_test_clip(&dir);

        let db = open_memory_db();
        let size = fs::metadata(&video).unwrap().len();
        let scanned = vec![ScannedFile {
            path: video.clone(),
            size,
            phash: None,
        }];

        let (ids, jobs) = import_files_pending_phash(&db, &scanned).expect("import");
        assert_eq!(ids.len(), 1);
        assert_eq!(jobs.len(), 1);

        let results = compute_phash_jobs(jobs);
        write_phashes(&db, &results).expect("write");

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
        let scanned = vec![ScannedFile {
            path: missing,
            size: 1,
            phash: None,
        }];

        let (ids, jobs) = import_files_pending_phash(&db, &scanned).expect("import");
        assert_eq!(ids.len(), 1);
        assert_eq!(jobs.len(), 1);

        let mut record = db.get_file_by_id(ids[0]).expect("get file");
        assert!(record.phash.is_none());
        assert_eq!(record.status, crate::file_status::processing);

        let results = compute_phash_jobs(jobs);
        write_phashes(&db, &results).expect("write");

        record = db.get_file_by_id(ids[0]).expect("get file");
        assert!(record.phash.is_none());
        assert_eq!(record.status, crate::file_status::ready);
    }

    #[test]
    fn scrape_failed_survives_full_import_phash_path() {
        let db = open_memory_db();
        let path = "/tmp/idam-scrape-failed-phash-rescan.mp4";
        let file_id = db.insert_file(path, 10, None).expect("insert");
        db.update_file_status(file_id, crate::file_status::scrape_failed)
            .expect("mark scrape_failed");
        // 模擬缺 phash 的失敗檔
        assert!(db.get_file_by_id(file_id).unwrap().phash.is_none());

        let scanned = vec![ScannedFile {
            path: PathBuf::from(path),
            size: 11,
            phash: None,
        }];
        let (ids, jobs) = import_files_pending_phash(&db, &scanned).expect("import");
        assert_eq!(ids.len(), 1);
        assert_eq!(jobs.len(), 1);

        let mid = db.get_file_by_id(ids[0]).expect("mid");
        assert_eq!(
            mid.status,
            crate::file_status::scrape_failed,
            "must not enter processing solely for phash"
        );

        let results = compute_phash_jobs(jobs);
        write_phashes(&db, &results).expect("write");

        let after = db.get_file_by_id(ids[0]).expect("after");
        assert_eq!(
            after.status,
            crate::file_status::scrape_failed,
            "phash completion must not promote scrape_failed to ready"
        );
        assert!(after.phash.is_none(), "missing file yields no phash");
        assert_eq!(after.size, 11);
    }

    #[test]
    fn import_preserves_precomputed_phash() {
        let db = open_memory_db();
        let scanned = vec![ScannedFile {
            path: PathBuf::from("/tmp/prehashed.mp4"),
            size: 42,
            phash: Some("abcdef0123456789".to_string()),
        }];
        let (ids, jobs) = import_files_pending_phash(&db, &scanned).expect("import");
        assert!(jobs.is_empty());
        let record = db.get_file_by_id(ids[0]).expect("get");
        assert_eq!(record.phash.as_deref(), Some("abcdef0123456789"));
    }

    #[test]
    fn rescan_skips_phash_job_when_hash_already_stored() {
        let db = open_memory_db();
        db.insert_file("/tmp/already.mp4", 10, Some("1122334455667788".into()))
            .expect("seed");

        let scanned = vec![ScannedFile {
            path: PathBuf::from("/tmp/already.mp4"),
            size: 11,
            phash: None,
        }];
        let (ids, jobs) = import_files_pending_phash(&db, &scanned).expect("import");
        assert_eq!(ids.len(), 1);
        assert!(jobs.is_empty());
        let record = db.get_file_by_id(ids[0]).expect("get");
        assert_eq!(record.phash.as_deref(), Some("1122334455667788"));
    }

    /// Guardrail: pending import + write-back must not hold the DB mutex across ffmpeg.
    #[test]
    fn phash_compute_does_not_require_db_lock() {
        if !crate::ffmpeg::check_ffmpeg_available() {
            eprintln!("skip: ffmpeg unavailable");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "idam-scan-unlock-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        let video = make_test_clip(&dir);
        let size = fs::metadata(&video).unwrap().len();

        let db = Arc::new(Mutex::new(open_memory_db()));
        let scanned = vec![ScannedFile {
            path: video,
            size,
            phash: None,
        }];

        let (ids, jobs) = {
            let guard = db.lock().expect("lock");
            import_files_pending_phash(&guard, &scanned).expect("import")
        };
        assert_eq!(jobs.len(), 1);

        // Hold the lock on another "thread" of execution while phash runs.
        // If compute_phash_jobs needed the mutex it would block forever.
        let db_held = Arc::clone(&db);
        let hold = std::thread::spawn(move || {
            let _guard = db_held.lock().expect("hold lock");
            std::thread::sleep(Duration::from_millis(200));
        });

        // Give the holder a moment to acquire
        std::thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        let results = compute_phash_jobs(jobs);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "phash must not block on DB mutex"
        );
        hold.join().expect("holder");

        {
            let guard = db.lock().expect("lock");
            write_phashes(&guard, &results).expect("write");
            let record = guard.get_file_by_id(ids[0]).expect("get");
            assert!(record.phash.is_some());
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
