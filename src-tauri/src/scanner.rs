use crate::database::Database;
use crate::file_status;
use crate::path_suppression::PathSuppression;
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

/// Result of registering one path into Files (path-unique).
#[derive(Debug, Clone)]
pub struct ImportedFile {
    pub file_id: i64,
    /// True when this upsert created a new Files row.
    pub is_new: bool,
}

/// Outcome of applying a computed phash (for logs / callers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhashApplyOutcome {
    Written,
    /// Exact phash match (distance 0); provisional row removed and path suppressed.
    SkippedDuplicate {
        keeper_id: i64,
        distance: u32,
        path: PathBuf,
    },
    /// Near match (1..=threshold); row kept as suspected_duplicate — no auto-handle.
    SuspectedDuplicate {
        keeper_id: i64,
        distance: u32,
    },
    /// No hash produced; only processing→ready cleared when applicable.
    NoHash,
}

/// Internal result of evaluating a phash against existing rows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NearPhashDecision {
    Exact {
        keeper_id: i64,
        distance: u32,
        path: PathBuf,
    },
    Suspected {
        keeper_id: i64,
        distance: u32,
    },
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

/// Evaluate phash against existing rows.
/// Exact (distance 0) or confirmed tombstone phash: delete provisional + PathSuppress.
/// Suspected (1..=max_distance): mark status + SuspectedDuplicates row; keep file.
/// Already-ignored (file_id, keeper_id) pairs are treated as Distinct.
fn apply_near_phash_policy(
    db: &Database,
    file_id: i64,
    phash_hex: &str,
    suppressions: Option<&PathSuppression>,
    max_distance: u32,
    // When true write phash_hex before marking suspected (write_phashes path)
    write_phash_on_suspected: bool,
) -> Result<Option<NearPhashDecision>> {
    // Confirmed-duplicate tombstone by phash: permanent auto-skip
    if db.has_duplicate_tombstone_phash(phash_hex)? {
        let orphan_path = db.get_file_by_id(file_id)?.path;
        eprintln!(
            "phash dedup: tombstone hit for file_id {} path {} phash {}; removing provisional",
            file_id, orphan_path, phash_hex
        );
        db.delete_file_record(file_id)?;
        let path_buf = PathBuf::from(&orphan_path);
        if let Some(supp) = suppressions {
            supp.suppress(&path_buf);
        }
        return Ok(Some(NearPhashDecision::Exact {
            keeper_id: 0,
            distance: 0,
            path: path_buf,
        }));
    }

    match db.find_near_phash_duplicate(file_id, phash_hex, max_distance)? {
        Some((keeper, distance)) => {
            // B1: ignored pair must not re-flag
            if db.is_ignored_suspected_pair(file_id, keeper.id)? {
                eprintln!(
                    "phash dedup: skip re-flag file_id {} keeper {} (already ignored)",
                    file_id, keeper.id
                );
                return Ok(None);
            }

            match phash::classify_phash_distance(distance, max_distance) {
                phash::PhashMatchKind::Exact => {
                    let orphan_path = db.get_file_by_id(file_id)?.path;
                    eprintln!(
                        "phash dedup: exact match file_id {} path {} (keeper id {} distance 0); \
                         removing provisional row; PathSuppress orphan",
                        file_id, orphan_path, keeper.id
                    );
                    db.delete_file_record(file_id)?;
                    let path_buf = PathBuf::from(&orphan_path);
                    if let Some(supp) = suppressions {
                        supp.suppress(&path_buf);
                    } else {
                        eprintln!(
                            "phash dedup: warning - no PathSuppression bound; orphan {} may re-ingest",
                            orphan_path
                        );
                    }
                    Ok(Some(NearPhashDecision::Exact {
                        keeper_id: keeper.id,
                        distance,
                        path: path_buf,
                    }))
                }
                phash::PhashMatchKind::Suspected => {
                    let current = db.get_file_by_id(file_id)?.status;
                    // S1 decision: scrape_failed / missing stay as-is (no suspected mark)
                    if current == file_status::scrape_failed || current == file_status::missing {
                        eprintln!(
                            "phash dedup: skip suspected mark for file_id {} status={}",
                            file_id, current
                        );
                        if write_phash_on_suspected {
                            let _ = db.update_file_phash(file_id, phash_hex);
                        }
                        return Ok(None);
                    }
                    eprintln!(
                        "phash dedup: suspected duplicate file_id {} (keeper id {} distance {} \
                         <= {}); keeping row as suspected_duplicate — no auto-handle",
                        file_id, keeper.id, distance, max_distance
                    );
                    let phash_arg = if write_phash_on_suspected {
                        Some(phash_hex)
                    } else {
                        None
                    };
                    db.mark_suspected_duplicate_atomic(
                        file_id,
                        keeper.id,
                        distance,
                        phash_arg,
                    )?;
                    Ok(Some(NearPhashDecision::Suspected {
                        keeper_id: keeper.id,
                        distance,
                    }))
                }
                phash::PhashMatchKind::Distinct => Ok(None),
            }
        }
        None => Ok(None),
    }
}

/// Short-lock friendly insert: write rows without waiting on ffmpeg.
/// Returns imported entries and jobs for files that still lack a phash.
///
/// Path policy: upsert by UNIQUE path - same path never double-inserts.
/// Known path with no phash job (`!is_new` and empty jobs for that ingest):
/// Watcher must not enqueue pipeline (path-dedup skip).
/// Phash policy: exact → delete provisional + PathSuppress; near → suspected_duplicate.
/// Paths already in PathSuppression (TTL) are skipped before upsert (orphan quarantine).
pub fn import_files_pending_phash(
    db: &Database,
    files: &[ScannedFile],
    suppressions: Option<&PathSuppression>,
    max_distance: u32,
) -> Result<(Vec<ImportedFile>, Vec<PhashJob>)> {
    let mut imported = Vec::new();
    let mut jobs: Vec<PhashJob> = Vec::new();

    for file in files {
        let path_str = match file.path.to_str() {
            Some(s) => s,
            None => {
                eprintln!("Failed to import file {:?}: non-utf8 path", file.path);
                continue;
            }
        };

        if let Some(supp) = suppressions {
            if supp.is_suppressed(&file.path) {
                eprintln!(
                    "import: skip PathSuppressed path {} (phash orphan TTL / pipeline dest)",
                    path_str
                );
                continue;
            }
        }

        // Confirmed-duplicate tombstone by path (survives PathSuppress TTL)
        match db.has_duplicate_tombstone_path(path_str) {
            Ok(true) => {
                eprintln!(
                    "import: skip DuplicateTombstone path {} (confirmed duplicate)",
                    path_str
                );
                if let Some(supp) = suppressions {
                    supp.suppress(&file.path);
                }
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("import: tombstone path check failed for {}: {}", path_str, e);
            }
        }

        let insert_phash = file
            .phash
            .clone()
            .filter(|h| !h.trim().is_empty());

        let (file_id, is_new) = match db.upsert_file(path_str, file.size, insert_phash) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Failed to import file {:?}: {}", file.path, e);
                continue;
            }
        };

        let record = match db.get_file_by_id(file_id) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Failed to read back imported file {}: {}", path_str, e);
                continue;
            }
        };

        // Precomputed phash: exact skip or mark suspected
        if let Some(ref hash) = record.phash {
            if !hash.trim().is_empty() {
                match apply_near_phash_policy(
                    db,
                    record.id,
                    hash,
                    suppressions,
                    max_distance,
                    false,
                ) {
                    Ok(Some(NearPhashDecision::Exact { .. })) => continue,
                    Ok(Some(NearPhashDecision::Suspected { .. })) => {
                        imported.push(ImportedFile {
                            file_id: record.id,
                            is_new,
                        });
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!(
                            "phash dedup check failed for {}: {:#}",
                            path_str, e
                        );
                    }
                }
            }
        }

        imported.push(ImportedFile {
            file_id: record.id,
            is_new,
        });

        let needs_phash = record
            .phash
            .as_ref()
            .map(|h| h.trim().is_empty())
            .unwrap_or(true);
        if needs_phash && !jobs.iter().any(|j| j.file_id == record.id) {
            // scrape_failed / suspected_duplicate 不可因算 phash 被洗成 processing
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

    Ok((imported, jobs))
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
/// Exact → remove provisional + PathSuppress; near → suspected_duplicate (keep).
/// Only clears processing→ready on surviving non-suspected rows.
pub fn write_phashes(
    db: &Database,
    results: &[(i64, Option<String>)],
    suppressions: Option<&PathSuppression>,
    max_distance: u32,
) -> Result<Vec<(i64, PhashApplyOutcome)>> {
    let mut outcomes = Vec::with_capacity(results.len());
    for (file_id, phash_opt) in results {
        // Row may already be gone (e.g. concurrent dedup)
        if db.get_file_by_id(*file_id).is_err() {
            continue;
        }

        if let Some(hash) = phash_opt {
            match apply_near_phash_policy(
                db,
                *file_id,
                hash,
                suppressions,
                max_distance,
                true,
            ) {
                Ok(Some(NearPhashDecision::Exact {
                    keeper_id,
                    distance,
                    path,
                })) => {
                    outcomes.push((
                        *file_id,
                        PhashApplyOutcome::SkippedDuplicate {
                            keeper_id,
                            distance,
                            path,
                        },
                    ));
                    continue;
                }
                Ok(Some(NearPhashDecision::Suspected {
                    keeper_id,
                    distance,
                })) => {
                    outcomes.push((
                        *file_id,
                        PhashApplyOutcome::SuspectedDuplicate {
                            keeper_id,
                            distance,
                        },
                    ));
                    continue;
                }
                Ok(None) => {
                    if let Err(e) = db.update_file_phash(*file_id, hash) {
                        eprintln!("Failed to write phash for file_id {}: {}", file_id, e);
                    }
                    outcomes.push((*file_id, PhashApplyOutcome::Written));
                }
                Err(e) => {
                    // S1: per-file error — log and continue the batch
                    eprintln!(
                        "phash dedup failed for file_id {}: {:#} (batch continues)",
                        file_id, e
                    );
                    if let Err(pe) = db.update_file_phash(*file_id, hash) {
                        eprintln!(
                            "Failed to write phash for file_id {} after dedup err: {}",
                            file_id, pe
                        );
                    }
                    outcomes.push((*file_id, PhashApplyOutcome::Written));
                }
            }
        } else {
            outcomes.push((*file_id, PhashApplyOutcome::NoHash));
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
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[allow(non_upper_case_globals)]
    const test_near_phash_max_distance: u32 = phash::default_near_phash_max_distance;

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
    fn import_same_path_twice_does_not_double_insert() {
        let db = open_memory_db();
        let scanned = vec![
            ScannedFile {
                path: PathBuf::from("/tmp/path-once.mp4"),
                size: 10,
                phash: None,
            },
            ScannedFile {
                path: PathBuf::from("/tmp/path-once.mp4"),
                size: 20,
                phash: None,
            },
        ];
        let (imported, jobs) = import_files_pending_phash(&db, &scanned, None, test_near_phash_max_distance).expect("import");
        assert_eq!(imported.len(), 2, "both upserts report the same path");
        assert_eq!(imported[0].file_id, imported[1].file_id);
        assert!(imported[0].is_new);
        assert!(!imported[1].is_new);
        assert_eq!(db.count_files().expect("count"), 1);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].file_id, imported[0].file_id);
    }

    #[test]
    fn import_skips_precomputed_exact_phash_duplicate() {
        let db = open_memory_db();
        let keeper = db
            .insert_file(
                "/tmp/phash-keeper.mp4",
                1,
                Some("0000000000000000".into()),
            )
            .expect("keeper");

        let scanned = vec![ScannedFile {
            path: PathBuf::from("/tmp/phash-dup.mp4"),
            size: 2,
            phash: Some("0000000000000000".into()),
        }];
        let (imported, jobs) = import_files_pending_phash(
            &db,
            &scanned,
            None,
            test_near_phash_max_distance,
        )
        .expect("import");
        assert!(imported.is_empty(), "exact phash must skip provisional row");
        assert!(jobs.is_empty());
        assert_eq!(db.count_files().expect("count"), 1);
        assert!(db.get_file_by_id(keeper).is_ok());
        assert!(db
            .get_file_by_path("/tmp/phash-dup.mp4")
            .expect("q")
            .is_none());
    }

    #[test]
    fn import_marks_precomputed_near_phash_as_suspected() {
        let db = open_memory_db();
        let keeper = db
            .insert_file(
                "/tmp/phash-keeper-near.mp4",
                1,
                Some("0000000000000000".into()),
            )
            .expect("keeper");

        // distance 4 → suspected under default threshold 4
        let scanned = vec![ScannedFile {
            path: PathBuf::from("/tmp/phash-near.mp4"),
            size: 2,
            phash: Some("000000000000000f".into()),
        }];
        let (imported, jobs) = import_files_pending_phash(
            &db,
            &scanned,
            None,
            test_near_phash_max_distance,
        )
        .expect("import");
        assert_eq!(imported.len(), 1, "suspected near-phash must keep row");
        assert!(jobs.is_empty());
        assert_eq!(db.count_files().expect("count"), 2);
        let dup = db.get_file_by_id(imported[0].file_id).expect("dup");
        assert_eq!(dup.status, file_status::suspected_duplicate);
        let sus = db
            .get_pending_suspected_duplicate(imported[0].file_id)
            .expect("sus")
            .expect("pending");
        assert_eq!(sus.keeper_id, keeper);
        assert_eq!(sus.distance, 4);
    }

    #[test]
    fn exact_dedup_path_suppress_blocks_immediate_reingest() {
        let db = open_memory_db();
        let supp = PathSuppression::new(Duration::from_secs(60));
        let keeper = db
            .insert_file(
                "/tmp/loop-keeper.mp4",
                1,
                Some("0000000000000000".into()),
            )
            .expect("keeper");

        let orphan = PathBuf::from("/tmp/loop-orphan.mp4");
        let scanned = vec![ScannedFile {
            path: orphan.clone(),
            size: 2,
            phash: Some("0000000000000000".into()),
        }];

        let (imported, jobs) = import_files_pending_phash(
            &db,
            &scanned,
            Some(&supp),
            test_near_phash_max_distance,
        )
        .expect("first");
        assert!(imported.is_empty());
        assert!(jobs.is_empty());
        assert!(
            supp.is_suppressed(&orphan),
            "orphan path must be PathSuppressed"
        );
        assert_eq!(db.count_files().expect("count"), 1);
        assert!(db.get_file_by_id(keeper).is_ok());

        let (again, jobs2) = import_files_pending_phash(
            &db,
            &scanned,
            Some(&supp),
            test_near_phash_max_distance,
        )
        .expect("second");
        assert!(again.is_empty(), "suppressed path must not re-import");
        assert!(jobs2.is_empty());
        assert_eq!(db.count_files().expect("count"), 1);
    }

    #[test]
    fn write_phashes_skips_exact_duplicate_and_keeps_existing() {
        let db = open_memory_db();
        let supp = PathSuppression::new(Duration::from_secs(60));
        let keeper = db
            .insert_file(
                "/tmp/write-keeper.mp4",
                1,
                Some("ffffffffffffffff".into()),
            )
            .expect("keeper");
        let provisional = db
            .insert_file("/tmp/write-dup.mp4", 2, None)
            .expect("provisional");
        db.update_file_status(provisional, file_status::processing)
            .expect("processing");

        let results = vec![(provisional, Some("ffffffffffffffff".to_string()))];
        let outcomes = write_phashes(
            &db,
            &results,
            Some(&supp),
            test_near_phash_max_distance,
        )
        .expect("write");
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0].1 {
            PhashApplyOutcome::SkippedDuplicate {
                keeper_id,
                distance,
                path,
            } => {
                assert_eq!(*keeper_id, keeper);
                assert_eq!(*distance, 0);
                assert_eq!(path, Path::new("/tmp/write-dup.mp4"));
                assert!(supp.is_suppressed(path));
            }
            other => panic!("expected SkippedDuplicate, got {:?}", other),
        }
        assert_eq!(db.count_files().expect("count"), 1);
        assert!(db.get_file_by_id(keeper).is_ok());
        assert!(db.get_file_by_id(provisional).is_err());

        let scanned = vec![ScannedFile {
            path: PathBuf::from("/tmp/write-dup.mp4"),
            size: 3,
            phash: Some("ffffffffffffffff".into()),
        }];
        let (imp, _) = import_files_pending_phash(
            &db,
            &scanned,
            Some(&supp),
            test_near_phash_max_distance,
        )
        .expect("re");
        assert!(imp.is_empty());
        assert_eq!(db.count_files().expect("count"), 1);
    }

    #[test]
    fn write_phashes_marks_near_as_suspected_not_auto_skip() {
        let db = open_memory_db();
        let supp = PathSuppression::new(Duration::from_secs(60));
        let keeper = db
            .insert_file(
                "/tmp/write-keeper-near.mp4",
                1,
                Some("ffffffffffffffff".into()),
            )
            .expect("keeper");
        let provisional = db
            .insert_file("/tmp/write-near.mp4", 2, None)
            .expect("provisional");
        db.update_file_status(provisional, file_status::processing)
            .expect("processing");

        // distance 2 (flip 2 bits) → suspected
        let results = vec![(provisional, Some("fffffffffffffffc".to_string()))];
        let outcomes = write_phashes(
            &db,
            &results,
            Some(&supp),
            test_near_phash_max_distance,
        )
        .expect("write");
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0].1 {
            PhashApplyOutcome::SuspectedDuplicate {
                keeper_id,
                distance,
            } => {
                assert_eq!(*keeper_id, keeper);
                assert_eq!(*distance, 2);
            }
            other => panic!("expected SuspectedDuplicate, got {:?}", other),
        }
        assert_eq!(db.count_files().expect("count"), 2);
        let row = db.get_file_by_id(provisional).expect("kept");
        assert_eq!(row.status, file_status::suspected_duplicate);
        assert_eq!(row.phash.as_deref(), Some("fffffffffffffffc"));
        assert!(
            !supp.is_suppressed(Path::new("/tmp/write-near.mp4")),
            "suspected must not PathSuppress"
        );
    }

    #[test]
    fn write_phashes_identical_reencoded_different_cases() {
        let db = open_memory_db();
        let keeper = db
            .insert_file(
                "/tmp/case-keeper.mp4",
                1,
                Some("0000000000000000".into()),
            )
            .expect("keeper");

        // identical → exact skip
        let identical = db
            .insert_file("/tmp/case-identical.mp4", 2, None)
            .expect("id");
        let out = write_phashes(
            &db,
            &[(identical, Some("0000000000000000".into()))],
            None,
            test_near_phash_max_distance,
        )
        .expect("identical");
        assert!(matches!(
            out[0].1,
            PhashApplyOutcome::SkippedDuplicate { distance: 0, .. }
        ));
        assert!(db.get_file_by_id(identical).is_err());

        // re-encoded style (distance 3) → suspected
        let reenc = db.insert_file("/tmp/case-reenc.mp4", 3, None).expect("re");
        let out = write_phashes(
            &db,
            &[(reenc, Some("0000000000000007".into()))],
            None,
            test_near_phash_max_distance,
        )
        .expect("reenc");
        assert!(matches!(
            out[0].1,
            PhashApplyOutcome::SuspectedDuplicate {
                distance: 3,
                ..
            }
        ));
        assert_eq!(
            db.get_file_by_id(reenc).unwrap().status,
            file_status::suspected_duplicate
        );

        // different clip (distance 16) → written distinct
        let different = db
            .insert_file("/tmp/case-different.mp4", 4, None)
            .expect("diff");
        let out = write_phashes(
            &db,
            &[(different, Some("000000000000ffff".into()))],
            None,
            test_near_phash_max_distance,
        )
        .expect("diff");
        assert_eq!(out[0].1, PhashApplyOutcome::Written);
        assert_eq!(
            db.get_file_by_id(different).unwrap().status,
            file_status::ready
        );
        assert!(db.get_file_by_id(keeper).is_ok());
    }

    #[test]
    fn write_phashes_keeps_far_hashes() {
        let db = open_memory_db();
        let a = db
            .insert_file(
                "/tmp/far-a.mp4",
                1,
                Some("0000000000000000".into()),
            )
            .expect("a");
        let b = db.insert_file("/tmp/far-b.mp4", 2, None).expect("b");
        let outcomes = write_phashes(
            &db,
            &[(b, Some("000000000000ffff".to_string()))],
            None,
            test_near_phash_max_distance,
        )
        .expect("write");
        assert_eq!(outcomes[0].1, PhashApplyOutcome::Written);
        assert_eq!(db.count_files().expect("count"), 2);
        assert_eq!(
            db.get_file_by_id(b).unwrap().phash.as_deref(),
            Some("000000000000ffff")
        );
        assert_eq!(
            db.get_file_by_id(a).unwrap().phash.as_deref(),
            Some("0000000000000000")
        );
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

        let (imported, jobs) = import_files_pending_phash(&db, &scanned, None, test_near_phash_max_distance).expect("import");
        assert_eq!(imported.len(), 1);
        assert!(imported[0].is_new);
        assert_eq!(jobs.len(), 1);

        let results = compute_phash_jobs(jobs);
        write_phashes(&db, &results, None, test_near_phash_max_distance).expect("write");

        let record = db.get_file_by_id(imported[0].file_id).expect("get file");
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

        let (imported, jobs) = import_files_pending_phash(&db, &scanned, None, test_near_phash_max_distance).expect("import");
        assert_eq!(imported.len(), 1);
        assert_eq!(jobs.len(), 1);

        let mut record = db.get_file_by_id(imported[0].file_id).expect("get file");
        assert!(record.phash.is_none());
        assert_eq!(record.status, crate::file_status::processing);

        let results = compute_phash_jobs(jobs);
        write_phashes(&db, &results, None, test_near_phash_max_distance).expect("write");

        record = db.get_file_by_id(imported[0].file_id).expect("get file");
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
        assert!(db.get_file_by_id(file_id).unwrap().phash.is_none());

        let scanned = vec![ScannedFile {
            path: PathBuf::from(path),
            size: 11,
            phash: None,
        }];
        let (imported, jobs) = import_files_pending_phash(&db, &scanned, None, test_near_phash_max_distance).expect("import");
        assert_eq!(imported.len(), 1);
        assert!(!imported[0].is_new);
        assert_eq!(jobs.len(), 1);

        let mid = db.get_file_by_id(imported[0].file_id).expect("mid");
        assert_eq!(
            mid.status,
            crate::file_status::scrape_failed,
            "must not enter processing solely for phash"
        );

        let results = compute_phash_jobs(jobs);
        write_phashes(&db, &results, None, test_near_phash_max_distance).expect("write");

        let after = db.get_file_by_id(imported[0].file_id).expect("after");
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
        let (imported, jobs) = import_files_pending_phash(&db, &scanned, None, test_near_phash_max_distance).expect("import");
        assert!(jobs.is_empty());
        let record = db.get_file_by_id(imported[0].file_id).expect("get");
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
        let (imported, jobs) = import_files_pending_phash(&db, &scanned, None, test_near_phash_max_distance).expect("import");
        assert_eq!(imported.len(), 1);
        assert!(!imported[0].is_new);
        assert!(jobs.is_empty());
        let record = db.get_file_by_id(imported[0].file_id).expect("get");
        assert_eq!(record.phash.as_deref(), Some("1122334455667788"));
    }

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

        let (imported, jobs) = {
            let guard = db.lock().expect("lock");
            import_files_pending_phash(&guard, &scanned, None, test_near_phash_max_distance).expect("import")
        };
        assert_eq!(jobs.len(), 1);

        let db_held = Arc::clone(&db);
        let hold = std::thread::spawn(move || {
            let _guard = db_held.lock().expect("hold lock");
            std::thread::sleep(Duration::from_millis(200));
        });

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
            write_phashes(&guard, &results, None, test_near_phash_max_distance).expect("write");
            let record = guard.get_file_by_id(imported[0].file_id).expect("get");
            assert!(record.phash.is_some());
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignore_survives_rescan_import_and_write_phashes() {
        let db = open_memory_db();
        let keeper = db
            .insert_file(
                "/tmp/ignore-keeper.mp4",
                1,
                Some("0000000000000000".into()),
            )
            .expect("keeper");
        let scanned = vec![ScannedFile {
            path: PathBuf::from("/tmp/ignore-near.mp4"),
            size: 2,
            phash: Some("000000000000000f".into()),
        }];
        let (imported, _) = import_files_pending_phash(
            &db,
            &scanned,
            None,
            test_near_phash_max_distance,
        )
        .expect("import");
        let near_id = imported[0].file_id;
        assert_eq!(
            db.get_file_by_id(near_id).unwrap().status,
            file_status::suspected_duplicate
        );
        db.resolve_suspected_duplicate(near_id, "ignored")
            .expect("ignore");
        assert_eq!(db.get_file_by_id(near_id).unwrap().status, file_status::ready);

        let (again, jobs) = import_files_pending_phash(
            &db,
            &scanned,
            None,
            test_near_phash_max_distance,
        )
        .expect("rescan import");
        assert_eq!(again[0].file_id, near_id);
        assert!(jobs.is_empty());
        assert_eq!(db.get_file_by_id(near_id).unwrap().status, file_status::ready);
        let row = db
            .get_suspected_duplicate_row(near_id)
            .expect("row")
            .expect("exists");
        assert_eq!(row.resolution, "ignored");
        assert_eq!(row.keeper_id, keeper);

        let outcomes = write_phashes(
            &db,
            &[(near_id, Some("000000000000000f".into()))],
            None,
            test_near_phash_max_distance,
        )
        .expect("write");
        assert_eq!(outcomes[0].1, PhashApplyOutcome::Written);
        assert_eq!(db.get_file_by_id(near_id).unwrap().status, file_status::ready);
        assert_eq!(
            db.get_suspected_duplicate_row(near_id)
                .unwrap()
                .unwrap()
                .resolution,
            "ignored"
        );
    }

    #[test]
    fn rescan_keeper_does_not_reverse_flag() {
        let db = open_memory_db();
        let keeper = db
            .insert_file(
                "/tmp/rev-keeper.mp4",
                1,
                Some("0000000000000000".into()),
            )
            .expect("keeper");
        let near = db
            .insert_file(
                "/tmp/rev-near.mp4",
                2,
                Some("000000000000000f".into()),
            )
            .expect("near");
        db.mark_suspected_duplicate_atomic(near, keeper, 4, None)
            .expect("mark");
        assert_eq!(
            db.get_file_by_id(near).unwrap().status,
            file_status::suspected_duplicate
        );

        let scanned = vec![ScannedFile {
            path: PathBuf::from("/tmp/rev-keeper.mp4"),
            size: 1,
            phash: Some("0000000000000000".into()),
        }];
        let (imported, jobs) = import_files_pending_phash(
            &db,
            &scanned,
            None,
            test_near_phash_max_distance,
        )
        .expect("rescan keeper");
        assert_eq!(imported[0].file_id, keeper);
        assert!(jobs.is_empty());
        assert_eq!(db.get_file_by_id(keeper).unwrap().status, file_status::ready);
        assert!(db
            .get_pending_suspected_duplicate(keeper)
            .unwrap()
            .is_none());
    }
}
