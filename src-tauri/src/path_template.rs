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

/// True when `desired` already exists on disk and is not the same as `source`
pub fn destination_collides(source: &Path, desired: &Path) -> bool {
    if paths_equal(source, desired) {
        return false;
    }
    desired.exists()
}

/// Move `source` to `dest` creating parent dirs
/// Prefers rename falls back to copy+remove on cross-device errors
pub fn move_file(source: &Path, dest: &Path) -> Result<()> {
    move_file_inner(source, dest, false)
}

/// Overwrite `dest` if it exists then move `source` onto it
pub fn move_file_overwrite(source: &Path, dest: &Path) -> Result<()> {
    if paths_equal(source, dest) {
        return Ok(());
    }
    if dest.exists() {
        fs::remove_file(dest).with_context(|| {
            format!("Failed to remove existing destination {}", dest.display())
        })?;
    }
    move_file(source, dest)
}

/// Test helper force the copy+remove fallback path (simulates cross-device rename)
#[cfg(test)]
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
        move_file_overwrite(&src, &dest).expect("overwrite");
        assert!(!src.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"new-bytes");
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
