//! Handlebars path templates for organize / rename (issue #9)
//!
//! Renders `output_template` with metadata fields then moves the file
//! Parent directories are created on demand Files.path is updated only after
//! a successful filesystem move

use anyhow::{Context, Result};
use handlebars::Handlebars;
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
}

/// Strip characters that are illegal or awkward in path segments
pub fn sanitize_segment(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => out.push('_'),
            c if c.is_control() => out.push('_'),
            c => out.push(c),
        }
    }
    // Collapse runs of dots / spaces at ends of the segment
    out.trim_matches(|c: char| c == '.' || c == ' ').to_string()
}

/// Extract a release-style code from a stem e.g. `ABC-123` or `abc_123`
pub fn extract_number(stem: &str) -> Option<String> {
    let re = Regex::new(r"(?i)([A-Z]{2,10})[-_\s]?(\d{2,5})").ok()?;
    let caps = re.captures(stem)?;
    let prefix = caps.get(1)?.as_str().to_uppercase();
    let digits = caps.get(2)?.as_str();
    Some(format!("{}-{}", prefix, digits))
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

    let mut hb = Handlebars::new();
    hb.register_escape_fn(handlebars::no_escape);
    // Strict mode off so missing helpers do not explode missing vars render empty
    hb.set_strict_mode(false);

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
        .map(|p| sanitize_segment(p))
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

/// Move `source` to `dest` creating parent dirs
/// Prefers rename falls back to copy+remove on cross-device errors
pub fn move_file(source: &Path, dest: &Path) -> Result<()> {
    move_file_inner(source, dest, false)
}

/// Test helper force the copy+remove fallback path (simulates cross-device rename)
pub fn move_file_via_copy(source: &Path, dest: &Path) -> Result<()> {
    move_file_inner(source, dest, true)
}

fn move_file_inner(source: &Path, dest: &Path, force_copy: bool) -> Result<()> {
    if source == dest {
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
}
