//! #73 batch organize preview → confirm execute → one-click undo
//!
//! Concept only (peach-mitao/peach AGPL-3.0) — implemented in Rust on top of
//! schema v7 `pending_move_to` crash recovery. Preview is read-only.

use crate::database::{
    Database, NewOrganizeBatchItem, OrganizeBatch, OrganizeBatchItem, OrganizedAtWrite,
    MAX_ORGANIZE_UNDO_ATTEMPTS,
};
use crate::file_status;
use crate::path_suppression::PathSuppression;
use crate::path_template::{self, TemplateContext};
use crate::pipeline::{self, dest_matches_mover_identity};
use crate::settings::AppSettings;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const OUTCOME_MOVE: &str = "move";
const OUTCOME_NOOP: &str = "noop";
const OUTCOME_COLLISION: &str = "collision";
const OUTCOME_MISSING: &str = "missing";
const OUTCOME_ERROR: &str = "error";
const OUTCOME_BLOCKED: &str = "blocked";

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

/// Confirmed plan entry from a prior preview (file_id + previewed dst)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrganizeExecutePlanItem {
    pub file_id: i64,
    pub dst_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrganizeExecuteResult {
    pub batch_id: i64,
    pub moved: usize,
    pub skipped: usize,
    pub failed: usize,
    pub plan_changed: usize,
    pub items: Vec<OrganizeBatchItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrganizeUndoResult {
    pub batch_id: i64,
    pub undone: usize,
    pub refused: usize,
    /// True when any refused item can be retried via undo again
    pub can_retry: bool,
    pub hint: Option<String>,
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
    dedupe_batch_destinations(&mut items);
    let move_count = items.iter().filter(|i| i.outcome == OUTCOME_MOVE).count();
    let skip_count = items.len().saturating_sub(move_count);
    Ok(OrganizePreviewResult {
        items,
        move_count,
        skip_count,
    })
}

/// Within one preview batch, only the first file may claim a normalized destination
fn dedupe_batch_destinations(items: &mut [OrganizePreviewItem]) {
    let mut claimed: HashMap<String, i64> = HashMap::new();
    for item in items.iter_mut() {
        if item.outcome != OUTCOME_MOVE {
            continue;
        }
        if item.dst_path.trim().is_empty() {
            continue;
        }
        let key = path_template::normalize_db_path(&item.dst_path);
        if let Some(&first_id) = claimed.get(&key) {
            item.outcome = OUTCOME_COLLISION.to_string();
            item.detail = Some(format!(
                "batch destination already claimed by file_id={first_id}"
            ));
            item.occupant_file_id = Some(first_id);
        } else {
            claimed.insert(key, item.file_id);
        }
    }
}

fn unknown_src_placeholder(file_id: i64) -> String {
    format!("(unknown file_id={file_id})")
}

/// Keep first plan per file_id
fn dedupe_plans(plans: Vec<OrganizeExecutePlanItem>) -> Vec<OrganizeExecutePlanItem> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(plans.len());
    for plan in plans {
        if seen.insert(plan.file_id) {
            out.push(plan);
        }
    }
    out
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
                src_path: unknown_src_placeholder(file_id),
                dst_path: unknown_src_placeholder(file_id),
                size: 0,
                phash: None,
                outcome: OUTCOME_ERROR.to_string(),
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
            outcome: OUTCOME_BLOCKED.to_string(),
            detail: Some(format!("status={}", file.status)),
            occupant_file_id: None,
        });
    }

    let source = PathBuf::from(&file.path);
    if !source.is_file() {
        return Ok(OrganizePreviewItem {
            file_id,
            src_path: file.path.clone(),
            dst_path: file.path.clone(),
            size: file.size,
            phash: file.phash.clone(),
            outcome: OUTCOME_MISSING.to_string(),
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
                dst_path: file.path.clone(),
                size: file.size,
                phash: file.phash.clone(),
                outcome: OUTCOME_ERROR.to_string(),
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
            outcome: OUTCOME_NOOP.to_string(),
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
            outcome: OUTCOME_COLLISION.to_string(),
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
        outcome: OUTCOME_MOVE.to_string(),
        detail: None,
        occupant_file_id: None,
    })
}

struct PendingBatchRow {
    preview: OrganizePreviewItem,
    status: &'static str,
    collision: Option<&'static str>,
    src_organized_at: Option<String>,
    /// None when the Files row is already gone (FK-safe insert)
    file_id: Option<i64>,
}

/// Confirm and execute moves for previewed `(file_id, dst_path)` pairs
pub fn execute_batch_organize(
    db: &Arc<Mutex<Database>>,
    settings: &AppSettings,
    plans: Vec<OrganizeExecutePlanItem>,
    suppressions: Option<&PathSuppression>,
) -> Result<OrganizeExecuteResult> {
    let plans = dedupe_plans(plans);
    if plans.is_empty() {
        anyhow::bail!("execute plan is empty");
    }
    let template = settings.output_template.trim();
    if template.is_empty() {
        anyhow::bail!("output_template is empty");
    }

    let file_ids: Vec<i64> = plans.iter().map(|p| p.file_id).collect();
    let preview = preview_batch_organize(db, settings, Some(file_ids))?;
    let preview_by_id: HashMap<i64, OrganizePreviewItem> = preview
        .items
        .into_iter()
        .map(|i| (i.file_id, i))
        .collect();

    let mut new_items: Vec<PendingBatchRow> = Vec::with_capacity(plans.len());
    let mut plan_changed = 0usize;
    let mut skipped = 0usize;

    for plan in &plans {
        let planned_dst = path_template::normalize_db_path(&plan.dst_path);
        let file_exists = {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            guard.file_row_exists(plan.file_id)?
        };
        let src_organized_at = if file_exists {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            guard.get_file_organized_at(plan.file_id)?
        } else {
            None
        };

        let Some(live) = preview_by_id.get(&plan.file_id) else {
            skipped += 1;
            new_items.push(PendingBatchRow {
                preview: OrganizePreviewItem {
                    file_id: plan.file_id,
                    src_path: unknown_src_placeholder(plan.file_id),
                    dst_path: planned_dst,
                    size: 0,
                    phash: None,
                    outcome: OUTCOME_ERROR.to_string(),
                    detail: Some("file missing from recomputed preview".into()),
                    occupant_file_id: None,
                },
                status: "skipped",
                collision: None,
                src_organized_at,
                file_id: if file_exists {
                    Some(plan.file_id)
                } else {
                    None
                },
            });
            continue;
        };

        let live_dst = path_template::normalize_db_path(&live.dst_path);
        // Template destination drifted from the confirmed plan
        if live_dst != planned_dst
            && (live.outcome == OUTCOME_MOVE || live.outcome == OUTCOME_COLLISION)
        {
            plan_changed += 1;
            let mut changed = live.clone();
            changed.detail = Some(format!(
                "preview dst changed: planned={planned_dst} live={live_dst}"
            ));
            new_items.push(PendingBatchRow {
                preview: changed,
                status: "plan_changed",
                collision: Some("plan_changed"),
                src_organized_at,
                file_id: if file_exists {
                    Some(plan.file_id)
                } else {
                    None
                },
            });
            continue;
        }

        // Same planned dst with a live collision: keep planned so the execute
        // recheck can emit skipped(destination_collides) (TOCTOU after preview).
        if live.outcome == OUTCOME_MOVE
            || (live.outcome == OUTCOME_COLLISION && live_dst == planned_dst)
        {
            let mut preview = live.clone();
            preview.dst_path = planned_dst;
            new_items.push(PendingBatchRow {
                preview,
                status: "planned",
                collision: None,
                src_organized_at,
                file_id: if file_exists {
                    Some(plan.file_id)
                } else {
                    None
                },
            });
            continue;
        }

        skipped += 1;
        let mut preview = live.clone();
        // Keep the confirmed plan dst for audit when the file vanished mid-flight
        if !file_exists || live.outcome == OUTCOME_ERROR || live.outcome == OUTCOME_MISSING
        {
            preview.dst_path = planned_dst;
        }
        new_items.push(PendingBatchRow {
            preview,
            status: "skipped",
            collision: None,
            src_organized_at,
            file_id: if file_exists {
                Some(plan.file_id)
            } else {
                None
            },
        });
    }

    let batch_rows: Vec<NewOrganizeBatchItem<'_>> = new_items
        .iter()
        .map(|row| NewOrganizeBatchItem {
            file_id: row.file_id,
            src_path: row.preview.src_path.as_str(),
            dst_path: row.preview.dst_path.as_str(),
            size: row.preview.size,
            phash: row.preview.phash.as_deref(),
            status: row.status,
            collision: row.collision,
            src_organized_at: row.src_organized_at.as_deref(),
        })
        .collect();

    let batch_id = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        guard.create_organize_batch_with_items(
            "executing",
            Some("batch organize"),
            &batch_rows,
        )?
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
        let Some(file_id) = item.file_id else {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            guard.update_organize_batch_item_status(
                item.id,
                "skipped",
                Some("file deleted before execute"),
            )?;
            skipped += 1;
            continue;
        };

        let source = PathBuf::from(&item.src_path);
        let dest = PathBuf::from(&item.dst_path);
        let dst_norm = path_template::normalize_db_path(&item.dst_path);

        {
            let guard = db
                .lock()
                .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
            if path_template::destination_collides(&source, &dest) {
                guard.update_organize_batch_item_status(
                    item.id,
                    "skipped",
                    Some("destination_collides at execute"),
                )?;
                skipped += 1;
                continue;
            }
            if let Some(occupant) = guard.find_file_occupying_path(&dst_norm, file_id)? {
                guard.update_organize_batch_item_status(
                    item.id,
                    "skipped",
                    Some(&format!(
                        "destination occupied by file_id={} at execute",
                        occupant.id
                    )),
                )?;
                skipped += 1;
                continue;
            }
            guard.update_organize_batch_item_status(item.id, "moving", None)?;
        }

        match pipeline::finalize_move(db, file_id, &source, &dest, suppressions) {
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
                let still_pending = guard
                    .get_pending_move_to(file_id)?
                    .map(|p| !p.trim().is_empty())
                    .unwrap_or(false);
                if still_pending {
                    // Keep moving so startup reconcile / pending recovery can finish
                    guard.update_organize_batch_item_status(
                        item.id,
                        "moving",
                        Some(&detail),
                    )?;
                } else {
                    guard.update_organize_batch_item_status(item.id, "failed", Some(&detail))?;
                    failed += 1;
                }
                let _ = guard.insert_error_log(
                    "batch_organize",
                    Some(file_id),
                    Some(&item.src_path),
                    "batch organize move failed",
                    Some(&detail),
                );
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
        plan_changed,
        items,
    })
}

/// One-click undo of the latest batch that still has `moved` / `undo_refused` items
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

    let undo_items = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let mut items = guard.list_organize_batch_items(batch.id)?;
        items.retain(|i| i.status == "moved" || i.status == "undo_refused");
        items.reverse();
        items
    };

    let mut undone = 0usize;
    let mut refused = 0usize;

    for item in undo_items {
        match undo_one_item(db, &item, suppressions) {
            Ok(true) => undone += 1,
            Ok(false) => refused += 1,
            Err(e) => {
                let detail = format!("{e:#}");
                if let Ok(guard) = db.lock() {
                    let still_pending = item
                        .file_id
                        .map(|fid| {
                            guard
                                .get_pending_move_to(fid)
                                .ok()
                                .flatten()
                                .map(|p| !p.trim().is_empty())
                                .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    if still_pending {
                        // Mirror execute: leave undoing so startup reconcile can finish
                        let _ = guard.update_organize_batch_item_status(
                            item.id,
                            "undoing",
                            Some(&detail),
                        );
                    } else {
                        let _ = guard.record_organize_undo_refusal(
                            item.id,
                            false,
                            &detail,
                            MAX_ORGANIZE_UNDO_ATTEMPTS,
                        );
                        refused += 1;
                    }
                    let _ = guard.insert_error_log(
                        "batch_undo",
                        item.file_id,
                        Some(&item.dst_path),
                        "batch undo failed",
                        Some(&detail),
                    );
                } else {
                    refused += 1;
                }
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

    let can_retry = items
        .iter()
        .any(|i| i.status == "undo_refused" || i.status == "moved");
    let hint = if can_retry {
        Some("部分檔案還原被拒絕（可再次點「還原上一批」重試符合條件者）".into())
    } else if refused > 0 {
        Some("部分檔案還原永久失敗（目的地消失或已達重試上限）".into())
    } else {
        None
    };

    Ok(OrganizeUndoResult {
        batch_id: batch.id,
        undone,
        refused,
        can_retry,
        hint,
        batch,
        items,
    })
}

fn undo_one_item(
    db: &Arc<Mutex<Database>>,
    item: &OrganizeBatchItem,
    suppressions: Option<&PathSuppression>,
) -> Result<bool> {
    let Some(file_id) = item.file_id else {
        refuse_undo(
            db,
            item,
            true,
            "undo refused: file row deleted",
            "file_id is NULL",
        )?;
        return Ok(false);
    };

    let current = {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        match guard.get_file_by_id(file_id) {
            Ok(f) => f,
            Err(_) => {
                refuse_undo(
                    db,
                    item,
                    true,
                    "undo refused: file row deleted",
                    &format!("file_id={file_id}"),
                )?;
                return Ok(false);
            }
        }
    };
    let current_path = path_template::normalize_db_path(&current.path);
    let expected_dst = path_template::normalize_db_path(&item.dst_path);
    let expected_src = path_template::normalize_db_path(&item.src_path);

    if current_path != expected_dst {
        refuse_undo(
            db,
            item,
            true,
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
            true,
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
            false,
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
            false,
            "undo refused: source path occupied on disk",
            &item.src_path,
        )?;
        return Ok(false);
    }

    {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        if let Some(occupant) = guard.find_file_occupying_path(&expected_src, file_id)? {
            refuse_undo(
                db,
                item,
                false,
                "undo refused: source path occupied by another Files row",
                &format!("occupant_id={} path={}", occupant.id, occupant.path),
            )?;
            return Ok(false);
        }
        guard.update_organize_batch_item_status(item.id, "undoing", None)?;
    }

    let organized = OrganizedAtWrite::Restore(item.src_organized_at.clone());
    pipeline::finalize_move_with_organized(
        db,
        file_id,
        dest,
        src,
        suppressions,
        organized,
    )
    .with_context(|| format!("undo move {} → {}", item.dst_path, item.src_path))?;

    {
        let guard = db
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
        let after = guard.get_file_by_id(file_id)?;
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
    permanent: bool,
    message: &str,
    detail: &str,
) -> Result<()> {
    let guard = db
        .lock()
        .map_err(|e| anyhow::anyhow!("Database lock error: {}", e))?;
    guard.record_organize_undo_refusal(
        item.id,
        permanent,
        detail,
        MAX_ORGANIZE_UNDO_ATTEMPTS,
    )?;
    let _ = guard.insert_error_log(
        "batch_undo",
        item.file_id,
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
    use crate::pipeline::{finalize_move_fault, recover_pending_moves};
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
        AppSettings {
            output_template: template.to_string(),
            ..AppSettings::default()
        }
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

    fn plans_from_preview(preview: &OrganizePreviewResult) -> Vec<OrganizeExecutePlanItem> {
        preview
            .items
            .iter()
            .filter(|i| i.outcome == OUTCOME_MOVE)
            .map(|i| OrganizeExecutePlanItem {
                file_id: i.file_id,
                dst_path: i.dst_path.clone(),
            })
            .collect()
    }

    fn file_has_organized_at_col(db: &Database, file_id: i64) -> bool {
        db.organized_at_is_set(file_id).expect("organized_at probe")
    }

    #[test]
    fn dedupe_plans_keeps_first_per_file_id() {
        let plans = vec![
            OrganizeExecutePlanItem {
                file_id: 1,
                dst_path: "/a".into(),
            },
            OrganizeExecutePlanItem {
                file_id: 2,
                dst_path: "/b".into(),
            },
            OrganizeExecutePlanItem {
                file_id: 1,
                dst_path: "/a-dup".into(),
            },
        ];
        let out = dedupe_plans(plans);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].file_id, 1);
        assert_eq!(out[0].dst_path, "/a");
        assert_eq!(out[1].file_id, 2);
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
        assert_eq!(preview.items[0].outcome, OUTCOME_MOVE);
        assert!(preview.items[0].dst_path.contains("Studio"));
        assert!(src.is_file(), "preview must not move the file");
        assert_eq!(fs::read(&src).expect("read"), before);
        let guard = db.lock().expect("lock");
        let file = guard.get_file_by_id(id).expect("file");
        assert_eq!(
            path_template::normalize_db_path(&file.path),
            path_template::normalize_db_path(src.to_str().unwrap()),
            "preview must not change Files.path"
        );
        assert!(
            guard.latest_undoable_organize_batch().unwrap().is_none(),
            "preview must not create a batch"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn preview_dedupes_same_destination_within_batch() {
        let root = temp_root("preview-dedupe");
        let db = open_db(&root);
        let src_a = root.join("inbox").join("a.mp4");
        let src_b = root.join("inbox").join("b.mp4");
        fs::create_dir_all(src_a.parent().unwrap()).expect("mkdir");
        let id_a = seed_file(&db, &src_a, "SameTitle");
        let id_b = seed_file(&db, &src_b, "SameTitle");
        let settings = settings_with_template("lib/{{title}}.{{ext}}");

        let preview =
            preview_batch_organize(&db, &settings, Some(vec![id_a, id_b])).expect("preview");
        assert_eq!(preview.move_count, 1);
        let outcomes: HashMap<i64, String> = preview
            .items
            .iter()
            .map(|i| (i.file_id, i.outcome.clone()))
            .collect();
        assert_eq!(outcomes.get(&id_a).map(String::as_str), Some(OUTCOME_MOVE));
        assert_eq!(
            outcomes.get(&id_b).map(String::as_str),
            Some(OUTCOME_COLLISION)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn execute_skips_when_preview_destination_changed() {
        let root = temp_root("plan-changed");
        let db = open_db(&root);
        let src = root.join("inbox").join("a.mp4");
        fs::create_dir_all(src.parent().unwrap()).expect("mkdir");
        let id = seed_file(&db, &src, "Alpha");
        let settings = settings_with_template("lib/{{title}}.{{ext}}");
        let preview = preview_batch_organize(&db, &settings, Some(vec![id])).expect("preview");
        let mut plans = plans_from_preview(&preview);
        assert_eq!(plans.len(), 1);
        plans[0].dst_path = root.join("lib").join("Stale.mp4").to_string_lossy().into();

        let exec = execute_batch_organize(&db, &settings, plans, None).expect("exec");
        assert_eq!(exec.moved, 0);
        assert_eq!(exec.plan_changed, 1);
        assert!(src.is_file(), "plan_changed must not move");
        assert!(
            exec.items.iter().any(|i| i.status == "plan_changed"),
            "expected plan_changed item got {:?}",
            exec.items
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn execute_rechecks_collision_before_finalize() {
        let root = temp_root("exec-recheck");
        let db = open_db(&root);
        let src = root.join("inbox").join("a.mp4");
        fs::create_dir_all(src.parent().unwrap()).expect("mkdir");
        let id = seed_file(&db, &src, "Alpha");
        let settings = settings_with_template("lib/{{title}}.{{ext}}");
        let preview = preview_batch_organize(&db, &settings, Some(vec![id])).expect("preview");
        assert_eq!(preview.move_count, 1);
        let plans = plans_from_preview(&preview);
        let dest = PathBuf::from(&plans[0].dst_path);
        // Place an occupant after preview so recheck must skip
        fs::create_dir_all(dest.parent().unwrap()).expect("mkdir dest");
        fs::write(&dest, b"occupant-after-preview").expect("write occupant");

        let exec = execute_batch_organize(&db, &settings, plans, None).expect("exec");
        assert_eq!(exec.moved, 0);
        assert!(
            exec.items.iter().any(|i| {
                i.status == "skipped"
                    && i.error
                        .as_deref()
                        .is_some_and(|e| e.contains("destination_collides"))
            }),
            "expected skipped(destination_collides) got {:?}",
            exec.items
        );
        assert!(src.is_file(), "source must remain");
        assert!(dest.is_file(), "occupant must remain");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn execute_then_undo_restores_organized_at_snapshot() {
        let root = temp_root("exec-undo-org");
        let db = open_db(&root);
        let src = root.join("inbox").join("a.mp4");
        fs::create_dir_all(src.parent().unwrap()).expect("mkdir");
        let id = seed_file(&db, &src, "Alpha");
        // Pre-mark organized so undo must restore (not blindly clear)
        {
            let guard = db.lock().expect("lock");
            guard.mark_file_organized(id).expect("mark");
            assert!(file_has_organized_at_col(&guard, id));
        }
        let settings = settings_with_template("lib/{{title}}.{{ext}}");
        let preview = preview_batch_organize(&db, &settings, Some(vec![id])).expect("preview");
        let exec =
            execute_batch_organize(&db, &settings, plans_from_preview(&preview), None)
                .expect("exec");
        assert_eq!(exec.moved, 1);
        let dest = PathBuf::from(
            &exec
                .items
                .iter()
                .find(|i| i.status == "moved")
                .unwrap()
                .dst_path,
        );

        let undo = undo_last_organize_batch(&db, None).expect("undo");
        assert_eq!(undo.undone, 1);
        assert!(src.is_file());
        assert!(!dest.is_file());
        {
            let guard = db.lock().expect("lock");
            assert!(
                file_has_organized_at_col(&guard, id),
                "undo must restore pre-batch organized_at"
            );
            let item = undo.items.iter().find(|i| i.status == "undone").unwrap();
            assert!(item.src_organized_at.is_some());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn execute_then_undo_clears_organized_at_when_unset_before() {
        let root = temp_root("exec-undo-clear");
        let db = open_db(&root);
        let src = root.join("inbox").join("a.mp4");
        fs::create_dir_all(src.parent().unwrap()).expect("mkdir");
        let id = seed_file(&db, &src, "Alpha");
        {
            let guard = db.lock().expect("lock");
            assert!(!file_has_organized_at_col(&guard, id));
        }
        let settings = settings_with_template("lib/{{title}}.{{ext}}");
        let preview = preview_batch_organize(&db, &settings, Some(vec![id])).expect("preview");
        let exec =
            execute_batch_organize(&db, &settings, plans_from_preview(&preview), None)
                .expect("exec");
        assert_eq!(exec.moved, 1);
        let undo = undo_last_organize_batch(&db, None).expect("undo");
        assert_eq!(undo.undone, 1);
        {
            let guard = db.lock().expect("lock");
            assert!(
                !file_has_organized_at_col(&guard, id),
                "undo must clear when pre-batch organized_at was null"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn organized_file_can_be_deleted_with_batch_items() {
        let root = temp_root("delete-organized");
        let db = open_db(&root);
        let src = root.join("inbox").join("a.mp4");
        fs::create_dir_all(src.parent().unwrap()).expect("mkdir");
        let id = seed_file(&db, &src, "Alpha");
        let settings = settings_with_template("lib/{{title}}.{{ext}}");
        let preview = preview_batch_organize(&db, &settings, Some(vec![id])).expect("preview");
        let exec =
            execute_batch_organize(&db, &settings, plans_from_preview(&preview), None)
                .expect("exec");
        assert_eq!(exec.moved, 1);
        {
            let guard = db.lock().expect("lock");
            guard.delete_file_record(id).expect("delete organized file");
            let items = guard
                .list_organize_batch_items(exec.batch_id)
                .expect("items");
            assert!(
                items[0].file_id.is_none(),
                "ON DELETE SET NULL must null file_id"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn file_deleted_after_preview_batch_still_executes() {
        let root = temp_root("delete-after-preview");
        let db = open_db(&root);
        let src_keep = root.join("inbox").join("keep.mp4");
        let src_gone = root.join("inbox").join("gone.mp4");
        fs::create_dir_all(src_keep.parent().unwrap()).expect("mkdir");
        let id_keep = seed_file(&db, &src_keep, "Keep");
        let id_gone = seed_file(&db, &src_gone, "Gone");
        let settings = settings_with_template("lib/{{title}}.{{ext}}");
        let preview =
            preview_batch_organize(&db, &settings, Some(vec![id_keep, id_gone])).expect("preview");
        let plans = plans_from_preview(&preview);
        assert_eq!(plans.len(), 2);
        {
            let guard = db.lock().expect("lock");
            guard.delete_file_record(id_gone).expect("delete");
        }
        let _ = fs::remove_file(&src_gone);

        let exec = execute_batch_organize(&db, &settings, plans, None).expect("exec");
        assert_eq!(exec.moved, 1, "keep file still moves");
        assert!(
            exec.skipped + exec.plan_changed >= 1,
            "deleted file must not abort batch: {:?}",
            exec.items
        );
        let gone_item = exec
            .items
            .iter()
            .find(|i| {
                i.dst_path.contains("Gone")
                    || i.src_path.contains("gone")
                    || (i.file_id.is_none() && i.status == "skipped")
            })
            .expect("gone item");
        assert!(
            gone_item.file_id.is_none(),
            "deleted file must insert with null file_id: {gone_item:?}"
        );
        assert_ne!(
            gone_item.status, "moved",
            "deleted file must not move: {gone_item:?}"
        );
        {
            let guard = db.lock().unwrap();
            assert!(guard.get_file_by_id(id_keep).is_ok());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_finalize_fail_with_pending_leaves_undoing() {
        let root = temp_root("undo-pending");
        let db = open_db(&root);
        let src = root.join("inbox").join("a.mp4");
        fs::create_dir_all(src.parent().unwrap()).expect("mkdir");
        let id = seed_file(&db, &src, "Alpha");
        // Unset before batch: recover MarkNow would leave organized_at=now unless
        // reconcile restores the NULL snapshot when undoing → undone.
        {
            let guard = db.lock().expect("lock");
            assert!(!file_has_organized_at_col(&guard, id));
        }
        let settings = settings_with_template("lib/{{title}}.{{ext}}");
        let preview = preview_batch_organize(&db, &settings, Some(vec![id])).expect("preview");
        let exec =
            execute_batch_organize(&db, &settings, plans_from_preview(&preview), None)
                .expect("exec");
        assert_eq!(exec.moved, 1);
        let moved = exec
            .items
            .iter()
            .find(|i| i.status == "moved")
            .unwrap();
        let dest = PathBuf::from(&moved.dst_path);
        assert!(
            moved.src_organized_at.is_none(),
            "pre-batch organized_at was null"
        );
        {
            let guard = db.lock().expect("lock");
            assert!(
                file_has_organized_at_col(&guard, id),
                "forward move MarkNow sets organized_at"
            );
        }

        // FS restores to src, heal never runs → pending left set (mirrors crash mid-undo)
        finalize_move_fault::arm_fail_after_fs_leave_pending();
        let undo = undo_last_organize_batch(&db, None).expect("undo");
        finalize_move_fault::clear();

        assert_eq!(undo.undone, 0);
        assert_eq!(undo.refused, 0);
        assert!(
            undo.items.iter().any(|i| i.status == "undoing"),
            "pending present → leave undoing, got {:?}",
            undo.items
        );
        assert!(src.is_file(), "file restored on disk");
        assert!(!dest.is_file());
        {
            let guard = db.lock().expect("lock");
            assert!(
                guard.get_pending_move_to(id).unwrap().is_some(),
                "pending must remain for recover/reconcile"
            );
            // Files.path still at dest until recover heals
            let file = guard.get_file_by_id(id).unwrap();
            assert_eq!(
                path_template::normalize_db_path(&file.path),
                path_template::normalize_db_path(dest.to_str().unwrap())
            );
        }

        // Startup path: recover (MarkNow) then reconcile → undone + restore snapshot
        let n = recover_pending_moves(&db).expect("recover");
        assert_eq!(n, 1);
        {
            let guard = db.lock().expect("lock");
            assert!(
                file_has_organized_at_col(&guard, id),
                "recover MarkNow leaves organized_at set until reconcile restores"
            );
        }
        let reconciled = reconcile_organize_batches_after_recovery(&db).expect("reconcile");
        assert!(reconciled >= 1);
        {
            let guard = db.lock().expect("lock");
            let items = guard.list_organize_batch_items(exec.batch_id).unwrap();
            assert_eq!(items[0].status, "undone");
            let file = guard.get_file_by_id(id).unwrap();
            assert_eq!(
                path_template::normalize_db_path(&file.path),
                path_template::normalize_db_path(src.to_str().unwrap())
            );
            assert!(
                !file_has_organized_at_col(&guard, id),
                "reconcile undoing→undone must restore src_organized_at (NULL), not leave MarkNow"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_permanent_failure_not_latest_undoable() {
        let root = temp_root("undo-permanent");
        let db = open_db(&root);
        let src = root.join("inbox").join("a.mp4");
        fs::create_dir_all(src.parent().unwrap()).expect("mkdir");
        let id = seed_file(&db, &src, "Alpha");
        let settings = settings_with_template("lib/{{title}}.{{ext}}");
        let preview = preview_batch_organize(&db, &settings, Some(vec![id])).expect("preview");
        let exec =
            execute_batch_organize(&db, &settings, plans_from_preview(&preview), None)
                .expect("exec");
        assert_eq!(exec.moved, 1);
        let dest = {
            let guard = db.lock().unwrap();
            PathBuf::from(guard.get_file_by_id(id).unwrap().path)
        };
        // Permanent: remove destination file so undo cannot reverse
        fs::remove_file(&dest).expect("rm dest");

        let undo = undo_last_organize_batch(&db, None).expect("undo");
        assert_eq!(undo.refused, 1);
        assert!(!undo.can_retry);
        assert!(
            undo.items.iter().any(|i| i.status == "undo_failed"),
            "expected undo_failed got {:?}",
            undo.items
        );
        {
            let guard = db.lock().unwrap();
            assert!(
                guard.latest_undoable_organize_batch().unwrap().is_none(),
                "permanent failure must not stay latest undoable"
            );
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
            Some(OUTCOME_COLLISION)
        );

        // Only move-outcome plans are confirmed; collision stays out of the batch
        let exec =
            execute_batch_organize(&db, &settings, plans_from_preview(&preview), None)
                .expect("exec");
        assert_eq!(exec.moved, 1, "only ok file moves");
        assert!(src_bad.is_file(), "failed/skipped source stays");

        let undo = undo_last_organize_batch(&db, None).expect("undo");
        assert_eq!(undo.undone, 1);
        assert_eq!(undo.refused, 0);
        assert!(src_ok.is_file());
        assert!(src_bad.is_file(), "skipped item must stay put");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_refuses_size_mismatch_and_continues_with_retry_hint() {
        let root = temp_root("undo-refuse");
        let db = open_db(&root);
        let src1 = root.join("inbox").join("one.mp4");
        let src2 = root.join("inbox").join("two.mp4");
        fs::create_dir_all(src1.parent().unwrap()).expect("mkdir");
        let id1 = seed_file(&db, &src1, "One");
        let id2 = seed_file(&db, &src2, "Two");
        let settings = settings_with_template("lib/{{title}}.{{ext}}");

        let preview =
            preview_batch_organize(&db, &settings, Some(vec![id1, id2])).expect("preview");
        let exec =
            execute_batch_organize(&db, &settings, plans_from_preview(&preview), None)
                .expect("exec");
        assert_eq!(exec.moved, 2);

        let dest1 = {
            let guard = db.lock().expect("lock");
            PathBuf::from(guard.get_file_by_id(id1).unwrap().path)
        };
        fs::write(&dest1, b"tampered-different-size!!!!").expect("tamper");

        let undo = undo_last_organize_batch(&db, None).expect("undo");
        assert_eq!(undo.refused, 1);
        assert_eq!(undo.undone, 1);
        assert!(undo.can_retry, "refused items remain retryable");
        assert!(undo.hint.is_some());
        assert!(dest1.is_file(), "refused file stays at dest");
        {
            let guard = db.lock().expect("lock");
            let logs = guard.list_error_logs(20).expect("logs");
            assert!(
                logs.iter().any(|l| l.category == "batch_undo"),
                "expected ErrorLog for refused undo got {logs:?}"
            );
            assert!(
                guard.latest_undoable_organize_batch().unwrap().is_some(),
                "undo_refused batch stays undoable for retry"
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
            guard
                .insert_organize_batch_item(
                    batch_id,
                    &NewOrganizeBatchItem {
                        file_id: Some(id),
                        src_path: src.to_str().unwrap(),
                        dst_path: dest.to_str().unwrap(),
                        size: fs::metadata(&src).unwrap().len(),
                        phash: None,
                        status: "moving",
                        collision: None,
                        src_organized_at: None,
                    },
                )
                .expect("item");
            let fs_id = path_template::read_fs_file_id(&src).ok().flatten();
            guard
                .set_pending_move_to(id, dest.to_str().unwrap(), fs_id)
                .expect("pending");
            fs::rename(&src, &dest).expect("rename");
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
            assert!(
                guard.get_pending_move_to(id).unwrap().is_none(),
                "pending must be cleared after recover"
            );
            let items = guard.list_organize_batch_items(batch_id).expect("items");
            assert_eq!(items[0].status, "moved");
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

    #[test]
    fn error_preview_does_not_use_empty_src_path() {
        let root = temp_root("error-src");
        let db = open_db(&root);
        let settings = settings_with_template("lib/{{title}}.{{ext}}");
        let preview =
            preview_batch_organize(&db, &settings, Some(vec![999_999])).expect("preview");
        assert_eq!(preview.items[0].outcome, OUTCOME_ERROR);
        assert!(!preview.items[0].src_path.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn draft_v8_schema_is_rebuilt_on_open() {
        let root = temp_root("v8-rebuild");
        let path = root.join("legacy.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("open");
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
                    organized_at DATETIME,
                    pending_move_to TEXT,
                    pending_source_vol INTEGER,
                    pending_source_idx INTEGER
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
                CREATE TABLE Jobs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    status TEXT NOT NULL,
                    type TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    max_attempts INTEGER NOT NULL DEFAULT 3,
                    error TEXT,
                    retryable INTEGER NOT NULL DEFAULT 1,
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
                );
                CREATE TABLE SuspectedDuplicates (
                    file_id INTEGER PRIMARY KEY,
                    keeper_id INTEGER NOT NULL,
                    distance INTEGER NOT NULL,
                    resolution TEXT NOT NULL DEFAULT 'pending',
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    FOREIGN KEY(file_id) REFERENCES Files(id),
                    FOREIGN KEY(keeper_id) REFERENCES Files(id)
                );
                CREATE TABLE OrganizeBatches (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    status TEXT NOT NULL DEFAULT 'executing',
                    note TEXT
                );
                CREATE TABLE OrganizeBatchItems (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    batch_id INTEGER NOT NULL,
                    file_id INTEGER NOT NULL,
                    src_path TEXT NOT NULL,
                    dst_path TEXT NOT NULL,
                    size INTEGER NOT NULL,
                    phash TEXT,
                    status TEXT NOT NULL DEFAULT 'planned',
                    collision TEXT,
                    error TEXT,
                    FOREIGN KEY (batch_id) REFERENCES OrganizeBatches(id)
                );
                INSERT INTO Files (id, path, size) VALUES (1, '/tmp/x.mp4', 1);
                INSERT INTO OrganizeBatches (id, status) VALUES (1, 'completed');
                INSERT INTO OrganizeBatchItems
                    (batch_id, file_id, src_path, dst_path, size, status)
                    VALUES (1, 1, '/a', '/b', 1, 'moved');
                PRAGMA user_version = 8;
                ",
            )
            .expect("seed draft v8");
        }
        let db = Database::new(path.to_str().unwrap()).expect("open repair");
        let batch = db.get_organize_batch(1).expect("batch");
        assert_eq!(batch.status, "completed");
        let items = db.list_organize_batch_items(1).expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, "moved");
        assert_eq!(items[0].src_path, "/a");
        assert_eq!(items[0].dst_path, "/b");
        // Draft rows lacked src_organized_at; rebuild copies NULL (cannot invent a snapshot)
        assert!(items[0].src_organized_at.is_none());
        assert_eq!(items[0].file_id, Some(1));
        // After rebuild, deleting the Files row must succeed (ON DELETE SET NULL)
        db.delete_file_record(1).expect("delete must succeed");
        let items = db.list_organize_batch_items(1).expect("items");
        assert!(
            items[0].file_id.is_none(),
            "rebuild must use ON DELETE SET NULL"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
