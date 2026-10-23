//! #73 batch organize preview → confirm execute → one-click undo
//!
//! Concept only (peach-mitao/peach AGPL-3.0) — implemented in Rust on top of
//! schema v7 `pending_move_to` crash recovery. Preview is read-only.

use crate::database::{
    Database, NewOrganizeBatchItem, OrganizeBatch, OrganizeBatchItem,
};
use crate::file_status;
use crate::path_suppression::PathSuppression;
use crate::path_template::{self, TemplateContext};
use crate::pipeline::{self, dest_matches_mover_identity};
use crate::settings::AppSettings;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[allow(non_upper_case_globals)]
const outcome_move: &str = "move";
#[allow(non_upper_case_globals)]
const outcome_noop: &str = "noop";
#[allow(non_upper_case_globals)]
const outcome_collision: &str = "collision";
#[allow(non_upper_case_globals)]
const outcome_missing: &str = "missing";
#[allow(non_upper_case_globals)]
const outcome_error: &str = "error";
#[allow(non_upper_case_globals)]
const outcome_blocked: &str = "blocked";

/// One row in a read-only organize preview
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrganizePreviewItem {
    pub file_id: i64,
    pub src_path: String,
    pub dst_path: String,
    pub size: u64,
    pub phash: Option<String>,
    /// move | noop | collision | missing | blocked | error
    pub outcome: String,
    pub detail: Option<String>,
    pub occupant_file_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrganizePreviewResult {
    pub items: Vec<OrganizePreviewItem>,
    pub move_count: usize,
    pub skip_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrganizeExecuteResult {
    pub batch_id: i64,
    pub moved: usize,
    pub skipped: usize,
    pub failed: usize,
    pub items: Vec<OrganizeBatchItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrganizeUndoResult {
    pub batch_id: i64,
    pub undone: usize,
    pub refused: usize,
    pub batch: OrganizeBatch,
    pub items: Vec<OrganizeBatchItem>,
}

/// Read-only preview of template moves for the given file ids (or all organizable)
pub fn preview_batch_organize(
    db: &Arc<Mutex<Database>>,
    settings: &AppSettings,
    file_ids: Option<Vec<i64>>,
) -> Result<OrganizePreviewResult> {
    let template = settings.output_template.trim();
    if template.is_empty() {
        anyhow::bail!("output_template is empty");
    }

    let ids = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        match file_ids {
            Some(ids) if !ids.is_empty() => ids,
            _ => guard.list_organizable_file_ids()?,
        }
    };

    let mut items = Vec::with_capacity(ids.len());
    for file_id in ids {
        items.push(preview_one(db, template, file_id)?);
    }
    let move_count = items.iter().filter(|i| i.outcome == outcome_move).count();
    let skip_count = items.len().saturating_sub(move_count);
    Ok(OrganizePreviewResult {
        items,
        move_count,
        skip_count,
    })
}

fn preview_one(
    db: &Arc<Mutex<Database>>,
    template: &str,
    file_id: i64,
) -> Result<OrganizePreviewItem> {
    let guard = db
        .lock()
        .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
    let file = match guard.get_file_by_id(file_id) {
        Ok(f) => f,
        Err(e) => {
            return Ok(OrganizePreviewItem {
                file_id,
                src_path: String::new(),
                dst_path: String::new(),
                size: 0,
                phash: None,
                outcome: outcome_error.to_string(),
                detail: Some(e.to_string()),
                occupant_file_id: None,
            });
        }
    };

    if file_status::blocks_auto_pipeline(&file.status) {
        return Ok(OrganizePreviewItem {
            file_id,
            src_path: file.path.clone(),
            dst_path: file.path.clone(),
            size: file.size,
            phash: file.phash.clone(),
            outcome: outcome_blocked.to_string(),
            detail: Some(format!("status={}", file.status)),
            occupant_file_id: None,
        });
    }

    let source = PathBuf::from(&file.path);
    if !source.is_file() {
        return Ok(OrganizePreviewItem {
            file_id,
            src_path: file.path.clone(),
            dst_path: String::new(),
            size: file.size,
            phash: file.phash.clone(),
            outcome: outcome_missing.to_string(),
            detail: Some("source file missing on disk".into()),
            occupant_file_id: None,
        });
    }

    let meta = guard.get_metadata(file_id)?;
    let tags = guard.get_file_tags(file_id).unwrap_or_default();
    let first_actor = tags
        .iter()
        .find(|t| t.category.as_deref() == Some("Actor"))
        .map(|t| t.name.as_str());
    let ctx = TemplateContext::from_metadata(
        &source,
        meta.as_ref().and_then(|m| m.studio.as_deref()),
        meta.as_ref().and_then(|m| m.release_date.as_deref()),
        meta.as_ref().and_then(|m| m.title.as_deref()),
        first_actor,
    );
    let base_dir = source
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let desired = match path_template::render_output_path(template, &ctx, &base_dir) {
        Ok(p) => p,
        Err(e) => {
            return Ok(OrganizePreviewItem {
                file_id,
                src_path: file.path.clone(),
                dst_path: String::new(),
                size: file.size,
                phash: file.phash.clone(),
                outcome: outcome_error.to_string(),
                detail: Some(format!("{e:#}")),
                occupant_file_id: None,
            });
        }
    };
    let dst_str = desired
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Desired path is not valid UTF-8"))?
        .to_string();
    let dst_norm = path_template::normalize_db_path(&dst_str);

    if path_template::paths_equal(&source, &desired) {
        return Ok(OrganizePreviewItem {
            file_id,
            src_path: file.path.clone(),
            dst_path: dst_norm,
            size: file.size,
            phash: file.phash.clone(),
            outcome: outcome_noop.to_string(),
            detail: Some("already at template path".into()),
            occupant_file_id: None,
        });
    }

    if path_template::destination_collides(&source, &desired) {
        let occupant = guard
            .find_file_occupying_path(&dst_norm, file_id)?
            .map(|f| f.id);
        return Ok(OrganizePreviewItem {
            file_id,
            src_path: file.path.clone(),
            dst_path: dst_norm,
            size: file.size,
            phash: file.phash.clone(),
            outcome: outcome_collision.to_string(),
            detail: Some("destination already exists".into()),
            occupant_file_id: occupant,
        });
    }

    Ok(OrganizePreviewItem {
        file_id,
        src_path: path_template::normalize_db_path(&file.path),
        dst_path: dst_norm,
        size: file.size,
        phash: file.phash.clone(),
        outcome: outcome_move.to_string(),
        detail: None,
        occupant_file_id: None,
    })
}

/// Confirm and execute moves for previewed `move` outcomes; writes schema v8 batch log
pub fn execute_batch_organize(
    db: &Arc<Mutex<Database>>,
    settings: &AppSettings,
    file_ids: Vec<i64>,
    suppressions: Option<&PathSuppression>,
) -> Result<OrganizeExecuteResult> {
    let preview = preview_batch_organize(db, settings, Some(file_ids))?;
    let to_move: Vec<_> = preview
        .items
        .iter()
        .filter(|i| i.outcome == outcome_move)
        .cloned()
        .collect();
    let skipped = preview.items.len().saturating_sub(to_move.len());

    let batch_id = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let batch_id = guard.create_organize_batch("executing", Some("batch organize"))?;
        for item in &preview.items {
            let status = if item.outcome == outcome_move {
                "planned"
            } else {
                "skipped"
            };
            let collision = if item.outcome == outcome_collision {
                Some("destination_exists")
            } else {
                None
            };
            let src = if item.src_path.is_empty() {
                ""
            } else {
                item.src_path.as_str()
            };
            let dst = if item.dst_path.is_empty() {
                src
            } else {
                item.dst_path.as_str()
            };
            guard.insert_organize_batch_item(
                batch_id,
                &NewOrganizeBatchItem {
                    file_id: item.file_id,
                    src_path: src,
                    dst_path: dst,
                    size: item.size,
                    phash: item.phash.as_deref(),
                    status,
                    collision,
                },
            )?;
        }
        batch_id
    };

    let mut moved = 0usize;
    let mut failed = 0usize;

    let planned_ids = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard
            .list_organize_batch_items(batch_id)?
            .into_iter()
            .filter(|i| i.status == "planned")
            .collect::<Vec<_>>()
    };

    for item in planned_ids {
        {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            guard.update_organize_batch_item_status(item.id, "moving", None)?;
        }

        let source = PathBuf::from(&item.src_path);
        let dest = PathBuf::from(&item.dst_path);
        match pipeline::finalize_move(db, item.file_id, &source, &dest, suppressions) {
            Ok(()) => {
                let guard = db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
                guard.update_organize_batch_item_status(item.id, "moved", None)?;
                moved += 1;
            }
            Err(e) => {
                let detail = format!("{e:#}");
                let guard = db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
                guard.update_organize_batch_item_status(item.id, "failed", Some(&detail))?;
                let _ = guard.insert_error_log(
                    "batch_organize",
                    Some(item.file_id),
                    Some(&item.src_path),
                    "batch organize move failed",
                    Some(&detail),
                );
                failed += 1;
            }
        }
    }

    let items = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.set_organize_batch_status(batch_id, "completed")?;
        guard.refresh_organize_batch_statuses()?;
        guard.list_organize_batch_items(batch_id)?
    };

    Ok(OrganizeExecuteResult {
        batch_id,
        moved,
        skipped,
        failed,
        items,
    })
}

/// One-click undo of the latest batch that still has `moved` items
pub fn undo_last_organize_batch(
    db: &Arc<Mutex<Database>>,
    suppressions: Option<&PathSuppression>,
) -> Result<OrganizeUndoResult> {
    let batch = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard
            .latest_undoable_organize_batch()?
            .ok_or_else(|| anyhow::anyhow!("no undoable organize batch"))?
    };

    {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.set_organize_batch_status(batch.id, "undoing")?;
    }

    let moved_items = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let mut items = guard.list_organize_batch_items(batch.id)?;
        items.retain(|i| i.status == "moved");
        items.reverse();
        items
    };

    let mut undone = 0usize;
    let mut refused = 0usize;

    for item in moved_items {
        match undo_one_item(db, &item, suppressions) {
            Ok(true) => undone += 1,
            Ok(false) => refused += 1,
            Err(e) => {
                let detail = format!("{e:#}");
                if let Ok(guard) = db.lock() {
                    let _ = guard.update_organize_batch_item_status(
                        item.id,
                        "undo_refused",
                        Some(&detail),
                    );
                    let _ = guard.insert_error_log(
                        "batch_undo",
                        Some(item.file_id),
                        Some(&item.dst_path),
                        "batch undo failed",
                        Some(&detail),
                    );
                }
                refused += 1;
            }
        }
    }

    let (batch, items) = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.refresh_organize_batch_statuses()?;
        let batch = guard.get_organize_batch(batch.id)?;
        let items = guard.list_organize_batch_items(batch.id)?;
        (batch, items)
    };

    Ok(OrganizeUndoResult {
        batch_id: batch.id,
        undone,
        refused,
        batch,
        items,
    })
}

fn undo_one_item(
    db: &Arc<Mutex<Database>>,
    item: &OrganizeBatchItem,
    suppressions: Option<&PathSuppression>,
) -> Result<bool> {
    let current = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.get_file_by_id(item.file_id)?
    };
    let current_path = path_template::normalize_db_path(&current.path);
    let expected_dst = path_template::normalize_db_path(&item.dst_path);
    let expected_src = path_template::normalize_db_path(&item.src_path);

    if current_path != expected_dst {
        refuse_undo(
            db,
            item,
            "undo refused: Files.path is not the logged destination",
            &format!("path={current_path} expected={expected_dst}"),
        )?;
        return Ok(false);
    }

    let dest = Path::new(&item.dst_path);
    if !dest.is_file() {
        refuse_undo(
            db,
            item,
            "undo refused: destination file missing on disk",
            &item.dst_path,
        )?;
        return Ok(false);
    }

    if !dest_matches_mover_identity(dest, item.size, item.phash.as_deref())? {
        let dest_size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        refuse_undo(
            db,
            item,
            "undo refused: size/phash mismatch at destination",
            &format!(
                "logged_size={} dest_size={} phash={:?}",
                item.size, dest_size, item.phash
            ),
        )?;
        return Ok(false);
    }

    let src = Path::new(&item.src_path);
    if src.exists() && !path_template::paths_equal(src, dest) {
        refuse_undo(
            db,
            item,
            "undo refused: source path occupied on disk",
            &item.src_path,
        )?;
        return Ok(false);
    }

    {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        if let Some(occupant) = guard.find_file_occupying_path(&expected_src, item.file_id)? {
            refuse_undo(
                db,
                item,
                "undo refused: source path occupied by another Files row",
                &format!("occupant_id={} path={}", occupant.id, occupant.path),
            )?;
            return Ok(false);
        }
        guard.update_organize_batch_item_status(item.id, "undoing", None)?;
    }

    pipeline::finalize_move(db, item.file_id, dest, src, suppressions)
        .with_context(|| format!("undo move {} → {}", item.dst_path, item.src_path))?;

    {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let after = guard.get_file_by_id(item.file_id)?;
        if path_template::normalize_db_path(&after.path) != expected_src {
            anyhow::bail!(
                "undo completed but Files.path is {} not {}",
                after.path,
                expected_src
            );
        }
        guard.update_organize_batch_item_status(item.id, "undone", None)?;
    }
    Ok(true)
}

fn refuse_undo(
    db: &Arc<Mutex<Database>>,
    item: &OrganizeBatchItem,
    message: &str,
    detail: &str,
) -> Result<()> {
    let guard = db
        .lock()
        .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
    guard.update_organize_batch_item_status(item.id, "undo_refused", Some(detail))?;
    let _ = guard.insert_error_log(
        "batch_undo",
        Some(item.file_id),
        Some(&item.dst_path),
        message,
        Some(detail),
    );
    Ok(())
}

/// After startup `recover_pending_moves`, heal batch item statuses left as moving/undoing
pub fn reconcile_organize_batches_after_recovery(db: &Arc<Mutex<Database>>) -> Result<usize> {
    let guard = db
        .lock()
        .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
    Ok(guard.reconcile_organize_batch_items_after_recovery()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Metadata;
    use crate::pipeline::recover_pending_moves;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("idam-batch-{label}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn open_db(dir: &Path) -> Arc<Mutex<Database>> {
        let path = dir.join("test.db");
        Arc::new(Mutex::new(
            Database::new(path.to_str().expect("utf8")).expect("db"),
        ))
    }

    fn settings_with_template(template: &str) -> AppSettings {
        let mut s = AppSettings::default();
        s.output_template = template.to_string();
        s
    }

    fn seed_file(db: &Arc<Mutex<Database>>, path: &Path, title: &str) -> i64 {
        let bytes = b"batch-organize-fixture-bytes";
        fs::write(path, bytes).expect("write");
        let guard = db.lock().expect("lock");
        let id = guard
            .insert_file(path.to_str().expect("utf8"), bytes.len() as u64, None)
            .expect("insert");
        let meta = Metadata {
            file_id: id,
            title: Some(title.into()),
            studio: Some("Studio".into()),
            release_date: Some("2024-01-01".into()),
            summary: None,
            cover_image: None,
            sprite_sheet_path: None,
            source_type: Some("test".into()),
        };
        guard.update_metadata(&meta).expect("meta");
        id
    }

    #[test]
    fn preview_is_read_only_no_fs_or_db_path_change() {
        let root = temp_root("preview-ro");
        let db = open_db(&root);
        let src = root.join("inbox").join("clip.mp4");
        fs::create_dir_all(src.parent().unwrap()).expect("mkdir");
        let id = seed_file(&db, &src, "Clip");
        let before = fs::read(&src).expect("read");
        let settings = settings_with_template("{{studio}}/{{title}}.{{ext}}");

        let preview = preview_batch_organize(&db, &settings, Some(vec![id])).expect("preview");
        assert_eq!(preview.move_count, 1);
        assert_eq!(preview.items[0].outcome, outcome_move);
        assert!(preview.items[0].dst_path.contains("Studio"));
        assert!(src.is_file(), "preview must not move the file");
        assert_eq!(fs::read(&src).expect("read"), before);
        let guard = db.lock().expect("lock");
        let file = guard.get_file_by_id(id).expect("file");
        assert_eq!(
            path_template::normalize_db_path(&file.path),
            path_template::normalize_db_path(src.to_str().unwrap())
        );
        assert!(guard.latest_undoable_organize_batch().unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn execute_then_undo_restores_paths() {
        let root = temp_root("exec-undo");
        let db = open_db(&root);
        let src = root.join("inbox").join("a.mp4");
        fs::create_dir_all(src.parent().unwrap()).expect("mkdir");
        let id = seed_file(&db, &src, "Alpha");
        let settings = settings_with_template("lib/{{title}}.{{ext}}");

        let exec = execute_batch_organize(&db, &settings, vec![id], None).expect("exec");
        assert_eq!(exec.moved, 1);
        assert_eq!(exec.failed, 0);
        let dest = PathBuf::from(&exec.items.iter().find(|i| i.status == "moved").unwrap().dst_path);
        assert!(dest.is_file());
        assert!(!src.is_file());
        {
            let guard = db.lock().expect("lock");
            let file = guard.get_file_by_id(id).expect("file");
            assert_eq!(
                path_template::normalize_db_path(&file.path),
                path_template::normalize_db_path(dest.to_str().unwrap())
            );
        }

        let undo = undo_last_organize_batch(&db, None).expect("undo");
        assert_eq!(undo.undone, 1);
        assert_eq!(undo.refused, 0);
        assert!(src.is_file());
        assert!(!dest.is_file());
        {
            let guard = db.lock().expect("lock");
            let file = guard.get_file_by_id(id).expect("file");
            assert_eq!(
                path_template::normalize_db_path(&file.path),
                path_template::normalize_db_path(src.to_str().unwrap())
            );
            assert_eq!(undo.batch.status, "undone");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_after_partial_failure_only_reverses_moved() {
        let root = temp_root("partial-fail");
        let db = open_db(&root);
        let src_ok = root.join("inbox").join("ok.mp4");
        let src_bad = root.join("inbox").join("bad.mp4");
        fs::create_dir_all(src_ok.parent().unwrap()).expect("mkdir");
        let id_ok = seed_file(&db, &src_ok, "OkTitle");
        let id_bad = seed_file(&db, &src_bad, "BadTitle");
        // Template is relative to source parent (inbox/) so collide at inbox/out/...
        let bad_dest = root.join("inbox").join("out").join("BadTitle.mp4");
        fs::create_dir_all(bad_dest.parent().unwrap()).expect("mkdir");
        fs::write(&bad_dest, b"occupant").expect("occupant");
        let settings = settings_with_template("out/{{title}}.{{ext}}");

        let preview =
            preview_batch_organize(&db, &settings, Some(vec![id_ok, id_bad])).expect("preview");
        assert_eq!(
            preview
                .items
                .iter()
                .find(|i| i.file_id == id_bad)
                .map(|i| i.outcome.as_str()),
            Some(outcome_collision)
        );

        let exec =
            execute_batch_organize(&db, &settings, vec![id_ok, id_bad], None).expect("exec");
        assert_eq!(exec.moved, 1, "only ok file moves");
        assert!(exec.skipped >= 1, "collision skipped");
        assert!(src_bad.is_file(), "failed/skipped source stays");

        let undo = undo_last_organize_batch(&db, None).expect("undo");
        assert_eq!(undo.undone, 1);
        assert_eq!(undo.refused, 0);
        assert!(src_ok.is_file());
        assert!(src_bad.is_file(), "skipped item must stay put");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_refuses_size_mismatch_and_continues() {
        let root = temp_root("undo-refuse");
        let db = open_db(&root);
        let src1 = root.join("inbox").join("one.mp4");
        let src2 = root.join("inbox").join("two.mp4");
        fs::create_dir_all(src1.parent().unwrap()).expect("mkdir");
        let id1 = seed_file(&db, &src1, "One");
        let id2 = seed_file(&db, &src2, "Two");
        let settings = settings_with_template("lib/{{title}}.{{ext}}");

        let exec = execute_batch_organize(&db, &settings, vec![id1, id2], None).expect("exec");
        assert_eq!(exec.moved, 2);

        let dest1 = {
            let guard = db.lock().expect("lock");
            PathBuf::from(guard.get_file_by_id(id1).unwrap().path)
        };
        // Tamper destination size so undo refuses this file
        fs::write(&dest1, b"tampered-different-size!!!!").expect("tamper");

        let undo = undo_last_organize_batch(&db, None).expect("undo");
        assert_eq!(undo.refused, 1);
        assert_eq!(undo.undone, 1);
        assert!(dest1.is_file(), "refused file stays at dest");
        assert!(src2.is_file() || {
            let guard = db.lock().unwrap();
            path_template::normalize_db_path(&guard.get_file_by_id(id2).unwrap().path)
                == path_template::normalize_db_path(src2.to_str().unwrap())
        });
        {
            let guard = db.lock().expect("lock");
            let logs = guard.list_error_logs(20).expect("logs");
            assert!(
                logs.iter().any(|l| l.category == "batch_undo"),
                "expected ErrorLog for refused undo got {logs:?}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn crash_mid_batch_recovers_via_pending_and_reconcile() {
        let root = temp_root("crash-mid");
        let db = open_db(&root);
        let src = root.join("inbox").join("crash.mp4");
        fs::create_dir_all(src.parent().unwrap()).expect("mkdir");
        let id = seed_file(&db, &src, "Crash");
        let dest = root.join("lib").join("Crash.mp4");
        fs::create_dir_all(dest.parent().unwrap()).expect("mkdir dest");

        let batch_id = {
            let guard = db.lock().expect("lock");
            let batch_id = guard
                .create_organize_batch("executing", Some("crash test"))
                .expect("batch");
            let item_id = guard
                .insert_organize_batch_item(
                    batch_id,
                    &NewOrganizeBatchItem {
                        file_id: id,
                        src_path: src.to_str().unwrap(),
                        dst_path: dest.to_str().unwrap(),
                        size: fs::metadata(&src).unwrap().len(),
                        phash: None,
                        status: "moving",
                        collision: None,
                    },
                )
                .expect("item");
            // Simulate: pending set + FS moved + DB not committed
            let fs_id = path_template::read_fs_file_id(&src).ok().flatten();
            guard
                .set_pending_move_to(id, dest.to_str().unwrap(), fs_id)
                .expect("pending");
            fs::rename(&src, &dest).expect("rename");
            let _ = item_id;
            batch_id
        };

        let n = recover_pending_moves(&db).expect("recover");
        assert_eq!(n, 1);
        let reconciled = reconcile_organize_batches_after_recovery(&db).expect("reconcile");
        assert!(reconciled >= 1);
        {
            let guard = db.lock().expect("lock");
            let file = guard.get_file_by_id(id).expect("file");
            assert_eq!(
                path_template::normalize_db_path(&file.path),
                path_template::normalize_db_path(dest.to_str().unwrap())
            );
            let items = guard.list_organize_batch_items(batch_id).expect("items");
            assert_eq!(items[0].status, "moved");
            assert!(guard.get_pending_move_to(id).unwrap().is_none());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn schema_v8_tables_exist_on_fresh_db() {
        let root = temp_root("schema-v8");
        let db = open_db(&root);
        let guard = db.lock().expect("lock");
        assert_eq!(guard.read_user_version().unwrap(), 8);
        let batch = guard.create_organize_batch("completed", None).unwrap();
        assert!(batch > 0);
        let _ = fs::remove_dir_all(&root);
    }
}
