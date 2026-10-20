//! Handlebars path templates for organize / rename (issue #9 / #40)
//!
//! Renders `output_template` with metadata fields then moves the file
//! Parent directories are created on demand Files.path is updated only after
//! a successful filesystem move
//!
//! #40 registers helpers date case truncate default path-sanitize

use anyhow::{Context, Result};
use handlebars::{
    Context as HbContext, Handlebars, Helper, HelperResult, Output, RenderContext,
};
use regex::Regex;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[allow(non_upper_case_globals)]
const unknown_studio: &str = "Unknown";

#[allow(non_upper_case_globals)]
const undated: &str = "undated";

/// Values available inside `output_template`
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TemplateContext {
    pub studio: String,
    pub date: String,
    pub title: String,
    pub number: String,
    pub filename: String,
    pub ext: String,
    pub actor: String,
}

impl TemplateContext {
    /// Build context from optional metadata fields and the source path
    pub fn from_metadata(
        source: &Path,
        studio: Option<&str>,
        release_date: Option<&str>,
        title: Option<&str>,
        first_actor: Option<&str>,
    ) -> Self {
        let filename = source
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("video")
            .to_string();
        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("video")
            .to_string();
        let ext = source
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("mp4")
            .to_string();
        let number = extract_number(&stem).unwrap_or_else(|| stem.clone());

        Self {
            studio: sanitize_segment(
                studio
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(unknown_studio),
            ),
            date: sanitize_segment(
                release_date
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(undated),
            ),
            title: sanitize_segment(
                title
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&stem),
            ),
            number: sanitize_segment(&number),
            filename: sanitize_segment(&filename),
            ext: sanitize_segment(&ext),
            actor: sanitize_segment(
                first_actor
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(""),
            ),
        }
    }

    /// Sample context for Settings template preview (#40)
    pub fn sample() -> Self {
        Self {
            studio: "StudioX".to_string(),
            date: "2024-01-02".to_string(),
            title: "Sample Title".to_string(),
            number: "ABC-123".to_string(),
            filename: "ABC-123.mp4".to_string(),
            ext: "mp4".to_string(),
            actor: "Alice".to_string(),
        }
    }
}

/// True when the DOS/Windows reserved device stem matches (CON PRN AUX NUL COM1-9 LPT1-9)
/// Case-insensitive; `CON.mp4` counts (stem before first `.`)
pub fn is_windows_reserved_device_name(segment: &str) -> bool {
    let trimmed = segment.trim().trim_matches(|c: char| c == '.' || c == ' ');
    if trimmed.is_empty() {
        return false;
    }
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Strip characters that are illegal or awkward in path segments
/// Windows reserved device names get a trailing `_` (e.g. `CON.mp4` → `CON.mp4_`)
pub fn sanitize_segment(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(trimmed.len() + 1);
    for ch in trimmed.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => out.push('_'),
            c if c.is_control() => out.push('_'),
            c => out.push(c),
        }
    }
    // Collapse runs of dots / spaces at ends of the segment
    let mut out = out
        .trim_matches(|c: char| c == '.' || c == ' ')
        .to_string();
    if is_windows_reserved_device_name(&out) {
        out.push('_');
    }
    out
}

/// Extract a release-style code from a stem e.g. `ABC-123` or `abc_123`
pub fn extract_number(stem: &str) -> Option<String> {
    let re = Regex::new(r"(?i)([A-Z]{2,10})[-_\s]?(\d{2,5})").ok()?;
    let caps = re.captures(stem)?;
    let prefix = caps.get(1)?.as_str().to_uppercase();
    let digits = caps.get(2)?.as_str();
    Some(format!("{}-{}", prefix, digits))
}

/// Title-case words separated by whitespace
pub fn titlecase_words(raw: &str) -> String {
    raw.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let mut out = first.to_uppercase().collect::<String>();
                    out.push_str(&chars.as_str().to_lowercase());
                    out
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format an ISO-ish date (`YYYY-MM-DD` or `YYYYMMDD`) with tokens YYYY MM DD YY
/// Unknown / undated / unparseable values return the input unchanged
pub fn format_date_string(date: &str, pattern: &str) -> String {
    let trimmed = date.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(undated) {
        return trimmed.to_string();
    }
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return trimmed.to_string();
    }
    let year = &digits[0..4];
    let month = &digits[4..6];
    let day = &digits[6..8];
    let yy = &digits[2..4];
    pattern
        .replace("YYYY", year)
        .replace("YY", yy)
        .replace("MM", month)
        .replace("DD", day)
}

/// Truncate by Unicode scalar values appending ellipsis when cut
pub fn truncate_chars(raw: &str, max: usize, ellipsis: &str) -> String {
    if max == 0 {
        return String::new();
    }
    let count = raw.chars().count();
    if count <= max {
        return raw.to_string();
    }
    let keep = max.saturating_sub(ellipsis.chars().count()).max(0);
    let head: String = raw.chars().take(keep).collect();
    format!("{}{}", head, ellipsis)
}

fn helper_str_param<'a>(h: &'a Helper<'a>, idx: usize) -> &'a str {
    h.param(idx)
        .and_then(|p| p.value().as_str())
        .unwrap_or("")
}

fn helper_u64_param(h: &Helper<'_>, idx: usize, default: u64) -> u64 {
    h.param(idx)
        .and_then(|p| {
            p.value()
                .as_u64()
                .or_else(|| p.value().as_i64().map(|n| n.max(0) as u64))
                .or_else(|| {
                    p.value()
                        .as_str()
                        .and_then(|s| s.trim().parse::<u64>().ok())
                })
        })
        .unwrap_or(default)
}

fn upper_helper(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &HbContext,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    out.write(&helper_str_param(h, 0).to_uppercase())?;
    Ok(())
}

fn lower_helper(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &HbContext,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    out.write(&helper_str_param(h, 0).to_lowercase())?;
    Ok(())
}

fn titlecase_helper(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &HbContext,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    out.write(&titlecase_words(helper_str_param(h, 0)))?;
    Ok(())
}

fn default_helper(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &HbContext,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    let value = helper_str_param(h, 0);
    let fallback = helper_str_param(h, 1);
    if value.trim().is_empty() {
        out.write(fallback)?;
    } else {
        out.write(value)?;
    }
    Ok(())
}

fn truncate_helper(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &HbContext,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    let value = helper_str_param(h, 0);
    let max = helper_u64_param(h, 1, 0) as usize;
    let ellipsis = h
        .param(2)
        .and_then(|p| p.value().as_str())
        .unwrap_or("...");
    out.write(&truncate_chars(value, max, ellipsis))?;
    Ok(())
}

fn format_date_helper(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &HbContext,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    let date = helper_str_param(h, 0);
    let pattern = h
        .param(1)
        .and_then(|p| p.value().as_str())
        .unwrap_or("YYYY-MM-DD");
    out.write(&format_date_string(date, pattern))?;
    Ok(())
}

fn sanitize_helper(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &HbContext,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    out.write(&sanitize_segment(helper_str_param(h, 0)))?;
    Ok(())
}

fn register_path_helpers(hb: &mut Handlebars<'_>) {
    hb.register_helper("upper", Box::new(upper_helper));
    hb.register_helper("lower", Box::new(lower_helper));
    hb.register_helper("titlecase", Box::new(titlecase_helper));
    hb.register_helper("default", Box::new(default_helper));
    hb.register_helper("truncate", Box::new(truncate_helper));
    hb.register_helper("format_date", Box::new(format_date_helper));
    // Alias for path-safe segment scrubbing
    hb.register_helper("sanitize", Box::new(sanitize_helper));
    hb.register_helper("path_safe", Box::new(sanitize_helper));
}

fn configured_handlebars() -> Handlebars<'static> {
    let mut hb = Handlebars::new();
    hb.register_escape_fn(handlebars::no_escape);
    // Strict mode off so missing vars render empty helpers stay available
    hb.set_strict_mode(false);
    register_path_helpers(&mut hb);
    hb
}

/// Render `template` with `context` into a PathBuf
/// Relative templates resolve against `base_dir` absolute templates are used as-is
pub fn render_output_path(
    template: &str,
    context: &TemplateContext,
    base_dir: &Path,
) -> Result<PathBuf> {
    let trimmed = template.trim();
    if trimmed.is_empty() {
        anyhow::bail!("output_template is empty");
    }

    // Re-sanitize so manual contexts cannot inject path separators into segments
    let safe = TemplateContext {
        studio: sanitize_segment(&context.studio),
        date: sanitize_segment(&context.date),
        title: sanitize_segment(&context.title),
        number: sanitize_segment(&context.number),
        filename: sanitize_segment(&context.filename),
        ext: sanitize_segment(&context.ext),
        actor: sanitize_segment(&context.actor),
    };

    let hb = configured_handlebars();
    let rendered = hb
        .render_template(trimmed, &safe)
        .with_context(|| format!("Failed to render output_template: {}", trimmed))?;

    let rendered = rendered.trim().replace('\\', "/");
    if rendered.is_empty() {
        anyhow::bail!("Rendered output path is empty");
    }

    let absolute = is_absolute_rendered(&rendered);
    let windows_drive = windows_drive_prefix(&rendered);

    // Sanitize each path component after rendering
    let parts: Vec<String> = rendered
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        // Drop Windows drive token from parts list we rebuild it below
        .filter(|p| !is_windows_drive_token(p))
        .map(sanitize_segment)
        .filter(|p| !p.is_empty())
        .collect();

    if parts.is_empty() {
        anyhow::bail!("Rendered output path has no valid segments");
    }

    let joined = parts.join("/");
    let dest = if absolute {
        if let Some(drive) = windows_drive {
            PathBuf::from(format!("{}:/{}", drive, joined))
        } else {
            PathBuf::from(format!("/{}", joined))
        }
    } else {
        base_dir.join(joined)
    };

    Ok(dest)
}

fn is_absolute_rendered(rendered: &str) -> bool {
    rendered.starts_with('/') || windows_drive_prefix(rendered).is_some()
}

/// True when the template string itself is absolute (not relative to source parent)
pub fn is_absolute_template(template: &str) -> bool {
    let trimmed = template.trim().replace('\\', "/");
    is_absolute_rendered(&trimmed)
}

fn windows_drive_prefix(rendered: &str) -> Option<char> {
    let mut chars = rendered.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() {
        return None;
    }
    if chars.next() != Some(':') {
        return None;
    }
    match chars.next() {
        Some('/') | Some('\\') => Some(drive.to_ascii_uppercase()),
        _ => None,
    }
}

fn is_windows_drive_token(part: &str) -> bool {
    let bytes = part.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Ensure destination is unique by appending `_N` before the extension
pub fn unique_destination(desired: &Path) -> PathBuf {
    if !desired.exists() {
        return desired.to_path_buf();
    }
    let parent = desired.parent().unwrap_or_else(|| Path::new("."));
    let stem = desired
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = desired
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| format!(".{}", e))
        .unwrap_or_default();

    for n in 2..10_000 {
        let candidate = parent.join(format!("{}_{}{}", stem, n, ext));
        if !candidate.exists() {
            return candidate;
        }
    }
    desired.to_path_buf()
}

/// True when paths refer to the same file (dev/ino or Windows volume+index)
pub fn is_same_file_identity(a: &Path, b: &Path) -> bool {
    same_file::is_same_file(a, b).unwrap_or(false)
}

/// True when `desired` already exists on disk and is not the same file as `source`
pub fn destination_collides(source: &Path, desired: &Path) -> bool {
    if is_same_file_identity(source, desired) || paths_equal(source, desired) {
        return false;
    }
    desired.exists()
}

/// Result of a safe overwrite move; delete `backup` only after DB commit succeeds
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverwriteMoveResult {
    pub backup: Option<PathBuf>,
    pub case_only: bool,
}

#[allow(non_upper_case_globals)]
const idam_bak_suffix: &str = ".idam-bak";

#[allow(non_upper_case_globals)]
const idam_tmp_prefix: &str = ".idam-tmp-";

/// Move `source` to `dest` creating parent dirs
/// Prefers rename falls back to copy+remove on cross-device errors
pub fn move_file(source: &Path, dest: &Path) -> Result<()> {
    move_file_inner(source, dest, false)
}

/// Overwrite `dest` safely: backup → rename or temp-copy → restore backup on failure
pub fn move_file_overwrite(source: &Path, dest: &Path) -> Result<OverwriteMoveResult> {
    move_file_overwrite_inner(source, dest, false)
}

/// Test helper force the copy+remove fallback path (simulates cross-device rename)
#[cfg(test)]
pub fn move_file_via_copy(source: &Path, dest: &Path) -> Result<()> {
    move_file_inner(source, dest, true)
}

/// Test helper force cross-volume overwrite path (backup + temp copy + sync)
#[cfg(any(test, feature = "test-support"))]
pub fn move_file_overwrite_via_copy(
    source: &Path,
    dest: &Path,
) -> Result<OverwriteMoveResult> {
    move_file_overwrite_inner(source, dest, true)
}

/// Restore files after a failed DB commit following overwrite
pub fn restore_overwrite_files(
    source: &Path,
    dest: &Path,
    backup: Option<&Path>,
) -> Result<()> {
    // Prefer moving dest (new content) back to source when source is gone
    if !source.exists() && dest.exists() && !is_same_file_identity(source, dest) {
        if let Err(e) = fs::rename(dest, source) {
            // Cross-volume fallback
            fs::copy(dest, source).with_context(|| {
                format!(
                    "Failed to restore source from dest (rename {e}; copy failed)"
                )
            })?;
            let _ = fs::remove_file(dest);
        }
    }
    if let Some(bak) = backup {
        if bak.exists() {
            if dest.exists() && !is_same_file_identity(bak, dest) {
                let _ = fs::remove_file(dest);
            }
            fs::rename(bak, dest).with_context(|| {
                format!(
                    "Failed to restore backup {} → {}",
                    bak.display(),
                    dest.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Delete `.idam-bak` after a successful DB commit
pub fn discard_overwrite_backup(backup: Option<&Path>) {
    if let Some(bak) = backup {
        if bak.exists() {
            let _ = fs::remove_file(bak);
        }
    }
}

fn allocate_backup_path(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let primary = parent.join(format!("{name}{idam_bak_suffix}"));
    if !primary.exists() {
        return primary;
    }
    for n in 2..10_000 {
        let candidate = parent.join(format!("{name}{idam_bak_suffix}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    primary
}

fn allocate_temp_path(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(
        "{idam_tmp_prefix}{}-{}",
        std::process::id(),
        nanos
    ))
}

fn move_file_overwrite_inner(
    source: &Path,
    dest: &Path,
    force_copy: bool,
) -> Result<OverwriteMoveResult> {
    if !source.is_file() {
        anyhow::bail!("Source is not a file: {}", source.display());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create destination directory {}", parent.display())
        })?;
    }

    // Same inode / file index → case-only (or hard-link) rename; never delete self
    if is_same_file_identity(source, dest) {
        if source != dest {
            fs::rename(source, dest).with_context(|| {
                format!(
                    "Failed case-only rename {} → {}",
                    source.display(),
                    dest.display()
                )
            })?;
        }
        return Ok(OverwriteMoveResult {
            backup: None,
            case_only: true,
        });
    }

    let mut backup: Option<PathBuf> = None;
    if dest.exists() {
        let bak = allocate_backup_path(dest);
        fs::rename(dest, &bak).with_context(|| {
            format!(
                "Failed to backup destination {} → {}",
                dest.display(),
                bak.display()
            )
        })?;
        backup = Some(bak);
    }

    #[cfg(any(test, feature = "test-support"))]
    if overwrite_fault::should_fail_after_backup() {
        if let Err(rb) = restore_overwrite_after_fs_error(source, dest, backup.as_deref()) {
            return Err(anyhow::anyhow!(
                "injected overwrite failure after backup; restore also failed: {rb:#}"
            ));
        }
        anyhow::bail!("injected overwrite failure after backup");
    }

    let move_result = if force_copy {
        copy_sync_rename_then_remove(source, dest)
    } else {
        match fs::rename(source, dest) {
            Ok(()) => Ok(()),
            Err(_) => copy_sync_rename_then_remove(source, dest),
        }
    };

    if let Err(e) = move_result {
        let _ = restore_overwrite_after_fs_error(source, dest, backup.as_deref());
        return Err(e);
    }

    Ok(OverwriteMoveResult {
        backup,
        case_only: false,
    })
}

fn restore_overwrite_after_fs_error(
    _source: &Path,
    dest: &Path,
    backup: Option<&Path>,
) -> Result<()> {
    // Clean partial dest / temp leftovers then put backup back
    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    if let Some(bak) = backup {
        if bak.exists() {
            fs::rename(bak, dest).with_context(|| {
                format!(
                    "Failed to restore backup {} → {}",
                    bak.display(),
                    dest.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Cross-volume safe replace: copy → sync_all → rename into place → delete source
fn copy_sync_rename_then_remove(source: &Path, dest: &Path) -> Result<()> {
    let tmp = allocate_temp_path(dest);
    let copy_outcome = (|| -> Result<()> {
        fs::copy(source, &tmp)
            .with_context(|| format!("Copy to temp {} failed", tmp.display()))?;
        {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(&tmp)
                .with_context(|| format!("Failed to open temp {} for sync", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("Failed to sync_all temp {}", tmp.display()))?;
        }
        fs::rename(&tmp, dest).with_context(|| {
            format!(
                "Failed to rename temp {} → {}",
                tmp.display(),
                dest.display()
            )
        })?;
        fs::remove_file(source).with_context(|| {
            format!(
                "Copied to {} but failed to remove source {}",
                dest.display(),
                source.display()
            )
        })?;
        Ok(())
    })();

    if copy_outcome.is_err() {
        let _ = fs::remove_file(&tmp);
        // If rename into dest succeeded but source delete failed, leave dest
        // caller restores backup / source as needed
    }
    copy_outcome
}

fn move_file_inner(source: &Path, dest: &Path, force_copy: bool) -> Result<()> {
    if is_same_file_identity(source, dest) || source == dest {
        if source != dest {
            fs::rename(source, dest).with_context(|| {
                format!(
                    "Failed same-file rename {} → {}",
                    source.display(),
                    dest.display()
                )
            })?;
        }
        return Ok(());
    }
    if !source.is_file() {
        anyhow::bail!("Source is not a file: {}", source.display());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create destination directory {}", parent.display())
        })?;
    }

    if !force_copy {
        match fs::rename(source, dest) {
            Ok(()) => return Ok(()),
            Err(rename_err) => {
                return copy_then_remove(source, dest, Some(rename_err));
            }
        }
    }

    copy_then_remove(source, dest, None)
}

fn copy_then_remove(
    source: &Path,
    dest: &Path,
    rename_err: Option<std::io::Error>,
) -> Result<()> {
    fs::copy(source, dest).with_context(|| {
        if let Some(err) = rename_err {
            format!(
                "Rename failed ({}) and copy to {} also failed",
                err,
                dest.display()
            )
        } else {
            format!("Copy to {} failed", dest.display())
        }
    })?;
    if let Err(remove_err) = fs::remove_file(source) {
        let _ = fs::remove_file(dest);
        return Err(remove_err).with_context(|| {
            format!(
                "Copied to {} but failed to remove source {}",
                dest.display(),
                source.display()
            )
        });
    }
    Ok(())
}

/// Test-only fault injection for overwrite after backup is in place
#[cfg(any(test, feature = "test-support"))]
pub mod overwrite_fault {
    use std::cell::Cell;

    thread_local! {
        #[allow(non_upper_case_globals)]
        static fail_after_backup: Cell<bool> = const { Cell::new(false) };
    }

    pub fn arm_fail_after_backup() {
        fail_after_backup.with(|f| f.set(true));
    }

    pub fn clear() {
        fail_after_backup.with(|f| f.set(false));
    }

    pub fn should_fail_after_backup() -> bool {
        fail_after_backup.with(|f| {
            if f.get() {
                f.set(false);
                true
            } else {
                false
            }
        })
    }
}

/// Attempt to undo a move when DB update fails
pub fn rollback_move(from_new: &Path, to_old: &Path) -> Result<()> {
    move_file(from_new, to_old)
}

/// Compare paths for equality after light normalization
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a.to_string_lossy() == b.to_string_lossy(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "idam_tmpl_{}_{}_{}",
            label,
            std::process::id(),
            nanos
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sanitize_segment_strips_illegal_chars() {
        assert_eq!(sanitize_segment("A/B:C*?"), "A_B_C__");
        assert_eq!(sanitize_segment("  ok  "), "ok");
    }

    #[test]
    fn sanitize_segment_appends_underscore_for_windows_reserved_names() {
        assert_eq!(sanitize_segment("CON"), "CON_");
        assert_eq!(sanitize_segment("con"), "con_");
        assert_eq!(sanitize_segment("PRN"), "PRN_");
        assert_eq!(sanitize_segment("aux"), "aux_");
        assert_eq!(sanitize_segment("NUL"), "NUL_");
        assert_eq!(sanitize_segment("COM1"), "COM1_");
        assert_eq!(sanitize_segment("com9"), "com9_");
        assert_eq!(sanitize_segment("LPT1"), "LPT1_");
        assert_eq!(sanitize_segment("lpt9"), "lpt9_");
        assert_eq!(sanitize_segment("CON.mp4"), "CON.mp4_");
        assert_eq!(sanitize_segment("com1.AVI"), "com1.AVI_");
        assert_eq!(sanitize_segment("normal"), "normal");
        assert_eq!(sanitize_segment("CONSOLE"), "CONSOLE");
        assert!(!is_windows_reserved_device_name("COM10"));
    }

    #[test]
    fn extract_number_from_common_stems() {
        assert_eq!(
            extract_number("ABC-123_cd1"),
            Some("ABC-123".to_string())
        );
        assert_eq!(
            extract_number("ssis_001_uncensored"),
            Some("SSIS-001".to_string())
        );
        assert_eq!(extract_number("random"), None);
    }

    #[test]
    fn render_relative_template_joins_base() {
        let ctx = TemplateContext {
            studio: "StudioX".to_string(),
            date: "2024-01-02".to_string(),
            title: "Hello World".to_string(),
            number: "ABC-123".to_string(),
            filename: "ABC-123.mp4".to_string(),
            ext: "mp4".to_string(),
            actor: "Alice".to_string(),
        };
        let base = PathBuf::from("/library");
        let dest = render_output_path("{{studio}}/{{date}}_{{title}}.{{ext}}", &ctx, &base)
            .expect("render");
        assert_eq!(
            dest,
            PathBuf::from("/library/StudioX/2024-01-02_Hello World.mp4")
        );
    }

    #[test]
    fn render_absolute_template_keeps_root() {
        let ctx = TemplateContext::from_metadata(
            Path::new("/in/ABC-999.mp4"),
            Some("S"),
            Some("2020-05-05"),
            Some("T"),
            None,
        );
        let dest = render_output_path(
            "/media/{{studio}}/{{number}}_{{title}}.{{ext}}",
            &ctx,
            Path::new("/ignored"),
        )
        .expect("render");
        assert_eq!(dest, PathBuf::from("/media/S/ABC-999_T.mp4"));
    }

    #[test]
    fn render_sanitizes_slash_in_title() {
        let ctx = TemplateContext {
            studio: "X".to_string(),
            date: "d".to_string(),
            title: "a/b".to_string(),
            number: "N".to_string(),
            filename: "f.mp4".to_string(),
            ext: "mp4".to_string(),
            actor: String::new(),
        };
        let dest =
            render_output_path("{{title}}.mp4", &ctx, Path::new("/out")).expect("render");
        assert_eq!(dest, PathBuf::from("/out/a_b.mp4"));
    }

    #[test]
    fn move_file_creates_dirs_and_relocates() {
        let dir = unique_temp_dir("move");
        let src = dir.join("src.mp4");
        fs::write(&src, b"video-bytes").unwrap();
        let dest = dir.join("nested").join("out.mp4");
        move_file(&src, &dest).expect("move");
        assert!(!src.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"video-bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_destination_appends_suffix() {
        let dir = unique_temp_dir("uniq");
        let first = dir.join("clip.mp4");
        fs::write(&first, b"a").unwrap();
        let next = unique_destination(&first);
        assert_eq!(next, dir.join("clip_2.mp4"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_metadata_fallbacks() {
        let ctx = TemplateContext::from_metadata(
            Path::new("/tmp/SSIS-001_raw.mp4"),
            None,
            None,
            None,
            None,
        );
        assert_eq!(ctx.studio, unknown_studio);
        assert_eq!(ctx.date, undated);
        assert_eq!(ctx.number, "SSIS-001");
        assert_eq!(ctx.title, "SSIS-001_raw");
        assert_eq!(ctx.ext, "mp4");
    }

    #[test]
    fn render_strips_path_traversal_segments() {
        let ctx = TemplateContext {
            studio: "S".to_string(),
            date: "d".to_string(),
            title: "t".to_string(),
            number: "N".to_string(),
            filename: "f.mp4".to_string(),
            ext: "mp4".to_string(),
            actor: String::new(),
        };
        let dest = render_output_path(
            "{{studio}}/../../etc/{{title}}.{{ext}}",
            &ctx,
            Path::new("/library/inbox"),
        )
        .expect("render");
        assert_eq!(dest, PathBuf::from("/library/inbox/S/etc/t.mp4"));
        assert!(!dest.starts_with("/etc"));
    }

    #[test]
    fn rollback_move_restores_source() {
        let dir = unique_temp_dir("rollback");
        let src = dir.join("orig.mp4");
        let dest = dir.join("nested").join("moved.mp4");
        fs::write(&src, b"payload").unwrap();
        move_file(&src, &dest).unwrap();
        assert!(!src.exists());
        rollback_move(&dest, &src).unwrap();
        assert!(src.exists());
        assert!(!dest.exists());
        assert_eq!(fs::read(&src).unwrap(), b"payload");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_file_via_copy_fallback_relocates() {
        let dir = unique_temp_dir("copyfb");
        let src = dir.join("a.mp4");
        let dest = dir.join("b").join("c.mp4");
        fs::write(&src, b"cross-device-sim").unwrap();
        move_file_via_copy(&src, &dest).expect("copy move");
        assert!(!src.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"cross-device-sim");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn helpers_upper_lower_titlecase_default_truncate_date_sanitize() {
        let ctx = TemplateContext {
            studio: "studioX".to_string(),
            date: "2024-01-02".to_string(),
            title: "Hello World Extra".to_string(),
            number: "ABC-123".to_string(),
            filename: "f.mp4".to_string(),
            ext: "mp4".to_string(),
            actor: String::new(),
        };
        let base = Path::new("/library");

        let upper = render_output_path("{{upper studio}}/x.{{ext}}", &ctx, base).unwrap();
        assert_eq!(upper, PathBuf::from("/library/STUDIOX/x.mp4"));

        let lower = render_output_path("{{lower number}}.{{ext}}", &ctx, base).unwrap();
        assert_eq!(lower, PathBuf::from("/library/abc-123.mp4"));

        let titled =
            render_output_path("{{titlecase studio}}/a.{{ext}}", &ctx, base).unwrap();
        assert_eq!(titled, PathBuf::from("/library/Studiox/a.mp4"));

        let with_default =
            render_output_path("{{default actor \"UnknownActor\"}}/a.{{ext}}", &ctx, base)
                .unwrap();
        assert_eq!(with_default, PathBuf::from("/library/UnknownActor/a.mp4"));

        let truncated =
            render_output_path("{{truncate title 8}}.{{ext}}", &ctx, base).unwrap();
        assert_eq!(truncated, PathBuf::from("/library/Hello....mp4"));

        // Slash in format_date pattern becomes a path segment (year/month dirs)
        let dated = render_output_path(
            "{{format_date date \"YYYY/MM\"}}_{{title}}.{{ext}}",
            &ctx,
            base,
        )
        .unwrap();
        assert_eq!(
            dated,
            PathBuf::from("/library/2024/01_Hello World Extra.mp4")
        );

        let unsafe_title = TemplateContext {
            title: "a/b:c".to_string(),
            ..ctx.clone()
        };
        // sanitize helper runs before segment re-sanitize; slash becomes underscore
        let safe = render_output_path("{{sanitize title}}.{{ext}}", &unsafe_title, base).unwrap();
        assert_eq!(safe, PathBuf::from("/library/a_b_c.mp4"));

        let path_safe =
            render_output_path("{{path_safe title}}.{{ext}}", &unsafe_title, base).unwrap();
        assert_eq!(path_safe, PathBuf::from("/library/a_b_c.mp4"));
    }

    #[test]
    fn format_date_string_tokens_and_undated() {
        assert_eq!(format_date_string("2024-01-02", "YYYY/MM/DD"), "2024/01/02");
        assert_eq!(format_date_string("20240102", "YY-MM"), "24-01");
        assert_eq!(format_date_string("undated", "YYYY"), "undated");
        assert_eq!(format_date_string("nope", "YYYY"), "nope");
    }

    #[test]
    fn truncate_chars_respects_ellipsis_budget() {
        assert_eq!(truncate_chars("abcdef", 10, "..."), "abcdef");
        assert_eq!(truncate_chars("abcdef", 5, ".."), "abc..");
        assert_eq!(truncate_chars("abcdef", 0, "..."), "");
    }

    #[test]
    fn destination_collides_when_other_file_exists() {
        let dir = unique_temp_dir("collide");
        let src = dir.join("a.mp4");
        let dest = dir.join("b.mp4");
        fs::write(&src, b"a").unwrap();
        fs::write(&dest, b"b").unwrap();
        assert!(destination_collides(&src, &dest));
        assert!(!destination_collides(&src, &src));
        assert!(!destination_collides(&src, &dir.join("missing.mp4")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_file_overwrite_replaces_existing() {
        let dir = unique_temp_dir("ovw");
        let src = dir.join("new.mp4");
        let dest = dir.join("old.mp4");
        fs::write(&src, b"new-bytes").unwrap();
        fs::write(&dest, b"old-bytes").unwrap();
        let outcome = move_file_overwrite(&src, &dest).expect("overwrite");
        assert!(!src.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"new-bytes");
        assert!(outcome.backup.is_some());
        assert!(outcome.backup.as_ref().unwrap().exists());
        assert_eq!(
            fs::read(outcome.backup.as_ref().unwrap()).unwrap(),
            b"old-bytes"
        );
        discard_overwrite_backup(outcome.backup.as_deref());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_file_overwrite_failure_restores_occupant() {
        let dir = unique_temp_dir("ovw_fail");
        let src = dir.join("new.mp4");
        let dest = dir.join("old.mp4");
        fs::write(&src, b"new-bytes").unwrap();
        fs::write(&dest, b"old-bytes").unwrap();
        overwrite_fault::arm_fail_after_backup();
        let err = move_file_overwrite(&src, &dest).expect_err("injected fail");
        overwrite_fault::clear();
        assert!(err.to_string().contains("injected"));
        assert!(src.exists(), "source must remain");
        assert!(dest.exists(), "occupant must be restored");
        assert_eq!(fs::read(&dest).unwrap(), b"old-bytes");
        assert_eq!(fs::read(&src).unwrap(), b"new-bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_file_overwrite_via_copy_replaces_existing() {
        let dir = unique_temp_dir("ovw_copy");
        let src = dir.join("new.mp4");
        let dest = dir.join("old.mp4");
        fs::write(&src, b"new-bytes").unwrap();
        fs::write(&dest, b"old-bytes").unwrap();
        let outcome = move_file_overwrite_via_copy(&src, &dest).expect("copy overwrite");
        assert!(!outcome.case_only);
        assert!(!src.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"new-bytes");
        discard_overwrite_backup(outcome.backup.as_deref());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_file_overwrite_same_file_identity_is_case_only() {
        let dir = unique_temp_dir("ovw_same");
        let src = dir.join("clip.mp4");
        let link = dir.join("clip-link.mp4");
        fs::write(&src, b"same-bytes").unwrap();
        fs::hard_link(&src, &link).expect("hard link");
        assert!(is_same_file_identity(&src, &link));
        let outcome = move_file_overwrite(&src, &link).expect("same-file overwrite");
        assert!(outcome.case_only);
        assert!(outcome.backup.is_none());
        assert!(link.exists() || src.exists());
        let path = if link.exists() { &link } else { &src };
        assert_eq!(fs::read(path).unwrap(), b"same-bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_overwrite_files_puts_backup_back() {
        let dir = unique_temp_dir("ovw_restore");
        let src = dir.join("new.mp4");
        let dest = dir.join("old.mp4");
        let bak = dir.join("old.mp4.idam-bak");
        fs::write(&dest, b"new-on-dest").unwrap();
        fs::write(&bak, b"old-bytes").unwrap();
        restore_overwrite_files(&src, &dest, Some(&bak)).expect("restore");
        assert!(src.exists());
        assert_eq!(fs::read(&src).unwrap(), b"new-on-dest");
        assert_eq!(fs::read(&dest).unwrap(), b"old-bytes");
        assert!(!bak.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sample_context_renders_preview_template() {
        let ctx = TemplateContext::sample();
        let dest = render_output_path(
            "{{studio}}/{{format_date date \"YYYY\"}}_{{truncate title 6}}.{{ext}}",
            &ctx,
            Path::new("/library"),
        )
        .unwrap();
        assert_eq!(dest, PathBuf::from("/library/StudioX/2024_Sam....mp4"));
    }
}
