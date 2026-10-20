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
use unicode_segmentation::UnicodeSegmentation;
use walkdir::WalkDir;

#[cfg(target_os = "macos")]
use unicode_normalization::UnicodeNormalization;

#[allow(non_upper_case_globals)]
const unknown_studio: &str = "Unknown";

#[allow(non_upper_case_globals)]
const undated: &str = "undated";

/// POSIX / common FS single-component byte limit
#[allow(non_upper_case_globals)]
const max_path_segment_bytes: usize = 255;

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
/// Reserved device names put `_` before the first `.` (`CON.foo` → `CON_.foo`)
pub fn sanitize_segment(raw: &str) -> String {
    sanitize_segment_inner(raw, false)
}

/// Like `sanitize_segment` but keeps a trailing extension when capping to 255 bytes
/// Reserved names use the same first-dot rule (`CON_.mp4`, `CON_.tar.gz`)
pub fn sanitize_filename_segment(raw: &str) -> String {
    sanitize_segment_inner(raw, true)
}

fn sanitize_segment_inner(raw: &str, preserve_extension: bool) -> String {
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
    out = finish_segment_cleanup(out, preserve_extension);
    // #59: each path component must fit common FS name limits
    out = if preserve_extension {
        cap_filename_utf8_bytes(&out, max_path_segment_bytes)
    } else {
        cap_segment_utf8_bytes(&out, max_path_segment_bytes)
    };
    // Re-run trailing cleanup + reserved-name after byte cap (S3)
    finish_segment_cleanup(out, preserve_extension)
}

fn finish_segment_cleanup(raw: String, _preserve_extension: bool) -> String {
    let out = raw
        .trim_matches(|c: char| c == '.' || c == ' ')
        .to_string();
    escape_windows_reserved_device_name(&out)
}

/// Windows treats the substring before the FIRST `.` as the device name.
/// Put `_` before that first dot so `CON.tar.gz` → `CON_.tar.gz` and dir
/// `CON.foo` → `CON_.foo` (appending `_` at the end leaves `CON.foo_` reserved).
fn escape_windows_reserved_device_name(name: &str) -> String {
    if !is_windows_reserved_device_name(name) {
        return name.to_string();
    }
    match name.find('.') {
        Some(i) => {
            let (head, tail) = name.split_at(i);
            if head.ends_with('_') {
                name.to_string()
            } else {
                format!("{head}_{tail}")
            }
        }
        None => {
            if name.ends_with('_') {
                name.to_string()
            } else {
                format!("{name}_")
            }
        }
    }
}

/// Truncate on UTF-8 char boundaries so the result is at most `max_bytes` long
fn cap_segment_utf8_bytes(raw: &str, max_bytes: usize) -> String {
    if raw.len() <= max_bytes {
        return raw.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    raw[..end].to_string()
}

/// Cap a filename by shortening the stem only so the extension is preserved
fn cap_filename_utf8_bytes(raw: &str, max_bytes: usize) -> String {
    if raw.len() <= max_bytes {
        return raw.to_string();
    }
    let path = Path::new(raw);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    if ext.len() >= max_bytes {
        // Extension alone does not fit keep a truncated whole name
        return cap_segment_utf8_bytes(raw, max_bytes);
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);
    let stem_budget = max_bytes - ext.len();
    let capped_stem = cap_segment_utf8_bytes(stem, stem_budget);
    format!("{capped_stem}{ext}")
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

/// Truncate by Unicode grapheme clusters appending ellipsis when cut (#59)
pub fn truncate_chars(raw: &str, max: usize, ellipsis: &str) -> String {
    if max == 0 {
        return String::new();
    }
    let graphemes: Vec<&str> = raw.graphemes(true).collect();
    if graphemes.len() <= max {
        return raw.to_string();
    }
    let ellipsis_len = ellipsis.graphemes(true).count();
    let keep = max.saturating_sub(ellipsis_len);
    let head: String = graphemes.into_iter().take(keep).collect();
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

    // Sanitize each path component after rendering last segment keeps extension (S3)
    let raw_parts: Vec<&str> = rendered
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        // Drop Windows drive token from parts list we rebuild it below
        .filter(|p| !is_windows_drive_token(p))
        .collect();
    let last_idx = raw_parts.len().saturating_sub(1);
    let parts: Vec<String> = raw_parts
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            if i == last_idx {
                sanitize_filename_segment(p)
            } else {
                sanitize_segment(p)
            }
        })
        .filter(|p| !p.is_empty())
        .collect();

    if parts.is_empty() {
        anyhow::bail!("Rendered output path has no valid segments");
    }

    // Build with PathBuf::push so Windows uses native `\` (not join("/"))
    let mut dest = if absolute {
        if let Some(drive) = windows_drive {
            // Drive root must be absolute on Windows (`C:\` not `C:`)
            #[cfg(windows)]
            {
                PathBuf::from(format!("{}:\\", drive))
            }
            #[cfg(not(windows))]
            {
                PathBuf::from(format!("{}:", drive))
            }
        } else {
            PathBuf::from(std::path::MAIN_SEPARATOR.to_string())
        }
    } else {
        base_dir.to_path_buf()
    };
    for part in &parts {
        dest.push(part);
    }

    Ok(dest)
}

/// Normalize a path string for DB storage and string equality lookups.
///
/// Chosen rules (#63) documented for callers and the PR:
///
/// 1. **NFC (macOS only)** — APFS/HFS+ often expose NFD; apply Unicode NFC so the
///    stored string matches what macOS APIs typically return after normalization.
///    Linux (ext4) and Windows (NTFS) compare bytes: keep original codepoints so an
///    on-disk NFD name is not rewritten to a non-existent path. Cross-platform NFC
///    matching is deferred to a v7 lookup-key column alongside #66.
/// 2. **Trailing separators** — strip trailing `/` or `\` except roots (`/`, `C:\`,
///    volume root `\\?\Volume{…}\`). UNC share roots may drop the trailing sep.
/// 3. **Case** — do **not** ASCII-fold path components for storage. `Files.path` UNIQUE
///    is case-sensitive; Win/macOS occupancy uses `COLLATE NOCASE` (ASCII A–Z only).
///    Drive letters are still uppercased on Windows (`c:\` → `C:\`).
/// 4. **Windows prefixes** — see [`normalize_windows_extended_prefix`].
/// 5. **Windows separators** — `/` → `\`, collapse repeated `\`, keep UNC `\\` prefix.
///
/// This is the single entry point for DB path writes and string lookups.
pub fn normalize_db_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    #[cfg(windows)]
    {
        normalize_db_path_windows(path)
    }
    #[cfg(not(windows))]
    {
        normalize_db_path_unix(path)
    }
}

#[cfg(target_os = "macos")]
fn apply_nfc(s: &str) -> String {
    s.nfc().collect()
}

#[cfg(not(windows))]
fn normalize_db_path_unix(path: &str) -> String {
    #[cfg(target_os = "macos")]
    let owned = apply_nfc(path);
    #[cfg(target_os = "macos")]
    let path = owned.as_str();
    let collapsed = collapse_separator(path, '/');
    strip_trailing_separators(&collapsed, '/')
}

#[cfg(windows)]
fn normalize_db_path_windows(path: &str) -> String {
    // Prefix handling before `/` → `\` so Volume / UNC forms stay recognizable
    let stripped = normalize_windows_extended_prefix(path);
    let mut s = stripped.replace('/', "\\");

    // Uppercase drive letter (`c:\lib` → `C:\lib`)
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive = (bytes[0] as char).to_ascii_uppercase();
        s.replace_range(0..1, &drive.to_string());
    }

    // Collapse repeated `\` while preserving a UNC / Volume `\\` prefix
    let collapsed = if let Some(rest) = s.strip_prefix("\\\\") {
        format!("\\\\{}", collapse_separator(rest, '\\'))
    } else {
        collapse_separator(&s, '\\')
    };
    // No NFC on Windows — NTFS compares bytes; rewriting NFD would orphan rows
    strip_trailing_separators(&collapsed, '\\')
}

/// Windows `\\?\` / `\\.\` (also `/` forms):
/// - `\\?\C:\…` / `\\.\C:\…` → `C:\…`
/// - `\\?\UNC\server\share\…` → `\\server\share\…`
/// - `\\?\Volume{GUID}\…` / `\\.\Volume{GUID}\…` → keep `\\?\Volume{GUID}\…`
///   (never strip to a bare `Volume{…}` path)
/// - other `\\.\device` → keep `\\.\device`
#[cfg(windows)]
fn normalize_windows_extended_prefix(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() < 4 {
        return path.to_string();
    }
    let is_extended = (bytes[0] == b'\\' || bytes[0] == b'/')
        && (bytes[1] == b'\\' || bytes[1] == b'/')
        && (bytes[2] == b'?' || bytes[2] == b'.')
        && (bytes[3] == b'\\' || bytes[3] == b'/');
    if !is_extended {
        return path.to_string();
    }
    let prefix_mark = bytes[2];
    let rest = &path[4..];
    if is_windows_volume_prefix(rest) {
        return format!("\\\\?\\{rest}");
    }
    if is_windows_unc_prefix(rest) {
        return format!("\\\\{}", &rest[4..]);
    }
    if is_windows_drive_prefix(rest) {
        return rest.to_string();
    }
    if prefix_mark == b'.' {
        // Non-drive device namespace (`\\.\PhysicalDrive0`)
        return format!("\\\\.\\{rest}");
    }
    // Other `\\?\…` — strip like drive/UNC (legacy behavior)
    rest.to_string()
}

#[cfg(windows)]
fn is_windows_volume_prefix(rest: &str) -> bool {
    let b = rest.as_bytes();
    b.len() >= 7
        && b[0].eq_ignore_ascii_case(&b'V')
        && b[1].eq_ignore_ascii_case(&b'O')
        && b[2].eq_ignore_ascii_case(&b'L')
        && b[3].eq_ignore_ascii_case(&b'U')
        && b[4].eq_ignore_ascii_case(&b'M')
        && b[5].eq_ignore_ascii_case(&b'E')
        && b[6] == b'{'
}

#[cfg(windows)]
fn is_windows_unc_prefix(rest: &str) -> bool {
    let b = rest.as_bytes();
    b.len() >= 4
        && b[0].eq_ignore_ascii_case(&b'U')
        && b[1].eq_ignore_ascii_case(&b'N')
        && b[2].eq_ignore_ascii_case(&b'C')
        && (b[3] == b'\\' || b[3] == b'/')
}

#[cfg(windows)]
fn is_windows_drive_prefix(rest: &str) -> bool {
    let b = rest.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn collapse_separator(s: &str, sep: char) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_sep = false;
    for ch in s.chars() {
        if ch == sep {
            if prev_sep {
                continue;
            }
            prev_sep = true;
            out.push(ch);
        } else {
            prev_sep = false;
            out.push(ch);
        }
    }
    out
}

/// Strip trailing `sep` unless doing so would destroy a filesystem root.
fn strip_trailing_separators(path: &str, sep: char) -> String {
    if path.is_empty() {
        return String::new();
    }
    let mut end = path.len();
    while end > 0 {
        let ch = match path[..end].chars().next_back() {
            Some(c) if c == sep => c,
            _ => break,
        };
        let prev = end - ch.len_utf8();
        let candidate = &path[..prev];
        if is_path_root_prefix(candidate, sep) {
            break;
        }
        end = prev;
    }
    path[..end].to_string()
}

/// True when `candidate` is a root that must keep its trailing separator,
/// or a drive letter (`C:`) that must stay as `C:\`.
fn is_path_root_prefix(candidate: &str, sep: char) -> bool {
    if candidate.is_empty() {
        // `/` or `\` alone — keep the separator
        return true;
    }
    if sep == '/' && candidate == "/" {
        return true;
    }
    #[cfg(windows)]
    {
        let b = candidate.as_bytes();
        // `C:` → keep `C:\`
        if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return true;
        }
        // Volume root `\\?\Volume{guid}` keeps trailing `\`
        // UNC share roots may drop the trailing sep (`\\server\share\` → `\\server\share`)
        if let Some(rest) = candidate.strip_prefix(r"\\?\") {
            if is_windows_volume_prefix(rest) {
                if let Some(close) = rest.find('}') {
                    return close + 1 == rest.len();
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = sep;
    }
    false
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
/// `is_taken` may reject filesystem and DB-owned paths (#59)
pub fn unique_destination_avoiding<F>(desired: &Path, is_taken: F) -> PathBuf
where
    F: Fn(&Path) -> bool,
{
    if !is_taken(desired) {
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
        if !is_taken(&candidate) {
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

/// True when `path` is an overwrite sidecar (`.idam-bak` / `.idam-bak-N` / `.idam-tmp-*`)
/// Crash recovery (#50) must never heal Files.path onto these leftovers (#59)
/// Uses ends_with / numeric suffix so `x.idam-bak.mp4` is not treated as a sidecar
pub fn is_idam_transient_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if name.starts_with(idam_tmp_prefix) {
        return true;
    }
    if name.ends_with(idam_bak_suffix) {
        return true;
    }
    // allocate_backup_path may use Demo.mp4.idam-bak-2
    if let Some(idx) = name.rfind(idam_bak_suffix) {
        let after = &name[idx + idam_bak_suffix.len()..];
        if let Some(num) = after.strip_prefix('-') {
            return !num.is_empty() && num.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}

/// Sibling `.idam-bak` path for an overwrite destination (primary name only)
pub fn primary_idam_bak_path(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    parent.join(format!("{name}{idam_bak_suffix}"))
}

/// Result of looking up occupant `.idam-bak` beside an overwrite destination (B1)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OccupantBakLookup {
    /// Only the primary `name.idam-bak` exists
    Primary(PathBuf),
    /// No bak beside dest
    Missing,
    /// Numbered `.idam-bak-N` present (alone or with primary) refuse recovery
    Ambiguous { extras: Vec<PathBuf> },
}

/// Crash recovery accepts only the main-name `.idam-bak` (never `-N` lexicographic pick)
pub fn lookup_occupant_bak(dest: &Path) -> OccupantBakLookup {
    let primary = primary_idam_bak_path(dest);
    let primary_ok = primary.is_file();
    let mut extras: Vec<PathBuf> = Vec::new();
    let Some(parent) = dest.parent() else {
        return if primary_ok {
            OccupantBakLookup::Primary(primary)
        } else {
            OccupantBakLookup::Missing
        };
    };
    let Some(name) = dest.file_name().and_then(|s| s.to_str()) else {
        return if primary_ok {
            OccupantBakLookup::Primary(primary)
        } else {
            OccupantBakLookup::Missing
        };
    };
    let prefix = format!("{name}{idam_bak_suffix}-");
    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let fname = entry.file_name();
            let Some(fname) = fname.to_str() else {
                continue;
            };
            if let Some(num) = fname.strip_prefix(&prefix) {
                if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                    extras.push(path);
                }
            }
        }
    }
    extras.sort();
    if !extras.is_empty() {
        return OccupantBakLookup::Ambiguous { extras };
    }
    if primary_ok {
        OccupantBakLookup::Primary(primary)
    } else {
        OccupantBakLookup::Missing
    }
}

/// Walk configured library roots for leftover overwrite sidecars (#59)
pub fn scan_idam_transient_leftovers(roots: &[impl AsRef<Path>]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        let root = root.as_ref();
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.into_path();
            if is_idam_transient_path(&path) {
                found.push(path);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

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

fn bak_path_hint(backup: Option<&Path>) -> String {
    match backup {
        Some(p) => p.display().to_string(),
        None => "(no bak)".to_string(),
    }
}

/// Restore files after a failed DB commit following overwrite
pub fn restore_overwrite_files(
    source: &Path,
    dest: &Path,
    backup: Option<&Path>,
) -> Result<()> {
    // Prefer moving dest (new content) back to source when source is gone
    if !source.exists() && dest.exists() && !is_same_file_identity(source, dest) {
        if let Err(rename_err) = fs::rename(dest, source) {
            // Cross-volume fallback
            if let Err(copy_err) = fs::copy(dest, source) {
                anyhow::bail!(
                    "Failed to restore source from dest (rename {rename_err}; copy failed: {copy_err}); occupant backup at {}",
                    bak_path_hint(backup)
                );
            }
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
                    "Failed to restore backup {} → {} (occupant backup at {})",
                    bak.display(),
                    dest.display(),
                    bak.display()
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
    let inject_move_fail = overwrite_fault::should_fail_after_backup();
    #[cfg(not(any(test, feature = "test-support")))]
    let inject_move_fail = false;

    let move_result = if inject_move_fail {
        Err(anyhow::anyhow!("injected overwrite failure after backup"))
    } else if force_copy {
        copy_sync_rename_then_remove(source, dest)
    } else {
        match fs::rename(source, dest) {
            Ok(()) => Ok(()),
            Err(_) => copy_sync_rename_then_remove(source, dest),
        }
    };

    if let Err(e) = move_result {
        match restore_overwrite_after_fs_error(source, dest, backup.as_deref()) {
            Ok(()) => return Err(e),
            Err(rb) => {
                // Combined error like the injected-fail path: include bak so callers
                // (commands) can surface it and write ErrorLog
                let bak_hint = bak_path_hint(backup.as_deref());
                let message = format!(
                    "overwrite failed and restore also failed; occupant may remain at {bak_hint}"
                );
                let detail = format!("{rb:#}");
                eprintln!("path_template: {message}; restore: {detail}");
                return Err(e).context(format!("{message}; restore: {detail}"));
            }
        }
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
    #[cfg(any(test, feature = "test-support"))]
    if overwrite_fault::should_fail_restore() {
        anyhow::bail!(
            "injected restore failure; occupant backup at {}",
            bak_path_hint(backup)
        );
    }
    // Clean partial dest / temp leftovers then put backup back
    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    if let Some(bak) = backup {
        if bak.exists() {
            fs::rename(bak, dest).with_context(|| {
                format!(
                    "Failed to restore backup {} → {} (occupant backup at {})",
                    bak.display(),
                    dest.display(),
                    bak.display()
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
        #[allow(non_upper_case_globals)]
        static fail_restore: Cell<bool> = const { Cell::new(false) };
    }

    pub fn arm_fail_after_backup() {
        fail_after_backup.with(|f| f.set(true));
    }

    pub fn arm_fail_restore() {
        fail_restore.with(|f| f.set(true));
    }

    pub fn clear() {
        fail_after_backup.with(|f| f.set(false));
        fail_restore.with(|f| f.set(false));
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

    pub fn should_fail_restore() -> bool {
        fail_restore.with(|f| {
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
        // First-dot rule (#63): dirs and multi-dot names stay non-reserved
        assert_eq!(sanitize_segment("CON.mp4"), "CON_.mp4");
        assert_eq!(sanitize_segment("com1.AVI"), "com1_.AVI");
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
        let next = unique_destination_avoiding(&first, |p| p.exists());
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
        // Grapheme cluster (family emoji) counts as one unit not many scalars
        let family = "👨‍👩‍👧‍👦xyz";
        assert_eq!(truncate_chars(family, 2, ""), "👨‍👩‍👧‍👦x");
    }

    #[test]
    fn sanitize_segment_caps_at_255_bytes() {
        let long = "a".repeat(300);
        let out = sanitize_segment(&long);
        assert!(out.len() <= max_path_segment_bytes);
        assert_eq!(out.len(), max_path_segment_bytes);
    }

    #[test]
    fn sanitize_filename_segment_keeps_extension_when_capping() {
        let stem = "a".repeat(300);
        let raw = format!("{stem}.mp4");
        let out = sanitize_filename_segment(&raw);
        assert!(out.len() <= max_path_segment_bytes);
        assert!(
            out.ends_with(".mp4"),
            "extension must survive byte cap got {out}"
        );
        assert_eq!(out.len(), max_path_segment_bytes);
    }

    #[test]
    fn sanitize_filename_segment_puts_underscore_on_stem() {
        // S-B: keep extension so the scanner still sees a video file
        assert_eq!(sanitize_filename_segment("CON.mp4"), "CON_.mp4");
        assert_eq!(sanitize_filename_segment("com1.MKV"), "com1_.MKV");
        assert_eq!(sanitize_filename_segment("CON"), "CON_");
    }

    #[test]
    fn lookup_occupant_bak_refuses_numbered_sidecars() {
        let dir = unique_temp_dir("bak_lookup");
        let dest = dir.join("Demo.mp4");
        fs::write(&dest, b"x").unwrap();
        let primary = dir.join("Demo.mp4.idam-bak");
        let numbered = dir.join("Demo.mp4.idam-bak-2");
        fs::write(&primary, b"old").unwrap();
        fs::write(&numbered, b"old2").unwrap();
        match lookup_occupant_bak(&dest) {
            OccupantBakLookup::Ambiguous { extras } => {
                assert!(extras.iter().any(|p| p == &numbered));
            }
            other => panic!("expected Ambiguous got {other:?}"),
        }
        fs::remove_file(&numbered).unwrap();
        match lookup_occupant_bak(&dest) {
            OccupantBakLookup::Primary(p) => assert_eq!(p, primary),
            other => panic!("expected Primary got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_destination_avoiding_respects_predicate() {
        let dir = unique_temp_dir("uniq_db");
        let desired = dir.join("Demo.mp4");
        // Nothing on disk but predicate claims desired is taken
        let next = unique_destination_avoiding(&desired, |p| p == desired.as_path());
        assert_eq!(next, dir.join("Demo_2.mp4"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_idam_transient_leftovers_reports_bak_and_tmp() {
        let dir = unique_temp_dir("scan_bak");
        let bak = dir.join("Demo.mp4.idam-bak");
        let tmp = dir.join(".idam-tmp-1-2");
        let keep = dir.join("Demo.mp4");
        fs::write(&bak, b"old").unwrap();
        fs::write(&tmp, b"tmp").unwrap();
        fs::write(&keep, b"ok").unwrap();
        let found = scan_idam_transient_leftovers(&[dir.as_path()]);
        assert!(found.iter().any(|p| p == &bak));
        assert!(found.iter().any(|p| p == &tmp));
        assert!(!found.iter().any(|p| p == &keep));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_idam_transient_path_detects_bak_and_tmp() {
        assert!(is_idam_transient_path(Path::new("/lib/Demo.mp4.idam-bak")));
        assert!(is_idam_transient_path(Path::new("/lib/Demo.mp4.idam-bak-2")));
        assert!(is_idam_transient_path(Path::new("/lib/.idam-tmp-1-2")));
        assert!(!is_idam_transient_path(Path::new("/lib/Demo.mp4")));
        // Mid-name `.idam-bak` must not false-positive (N1)
        assert!(!is_idam_transient_path(Path::new("/lib/x.idam-bak.mp4")));
        assert!(!is_idam_transient_path(Path::new("/lib/idam-bak-not-a-sidecar.mp4")));
        assert_eq!(
            primary_idam_bak_path(Path::new("/lib/Demo.mp4")),
            PathBuf::from("/lib/Demo.mp4.idam-bak")
        );
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
    fn move_file_overwrite_same_file_identity_skips_backup() {
        let dir = unique_temp_dir("ovw_same");
        let src = dir.join("clip.mp4");
        let link = dir.join("clip-link.mp4");
        fs::write(&src, b"same-bytes").unwrap();
        fs::hard_link(&src, &link).expect("hard link");
        assert!(is_same_file_identity(&src, &link));
        let outcome = move_file_overwrite(&src, &link).expect("same-file overwrite");
        assert!(outcome.case_only);
        assert!(outcome.backup.is_none());
        // Dest must keep the bytes no bak leftover (hard-link rename is not case-fold)
        assert!(link.exists(), "dest path must remain after same-identity rename");
        assert_eq!(fs::read(&link).unwrap(), b"same-bytes");
        assert!(
            !dir.join("clip.mp4.idam-bak").exists()
                && !dir.join("clip-link.mp4.idam-bak").exists(),
            "same-identity path must not create .idam-bak"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Real case-fold overwrite on case-insensitive volumes (Windows CI / macOS default)
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn move_file_overwrite_case_only_clip_to_clip_preserves_content() {
        let dir = unique_temp_dir("ovw_case");
        let src = dir.join("clip.mp4");
        let dest = dir.join("Clip.mp4");
        fs::write(&src, b"case-bytes").unwrap();
        assert!(
            is_same_file_identity(&src, &dest),
            "expected case-insensitive FS so clip.mp4 and Clip.mp4 are one file"
        );
        let outcome = move_file_overwrite(&src, &dest).expect("case-only overwrite");
        assert!(outcome.case_only);
        assert!(outcome.backup.is_none());
        assert!(dest.exists(), "Clip.mp4 must exist after case-only rename");
        assert_eq!(fs::read(&dest).unwrap(), b"case-bytes");
        assert!(
            !dir.join("clip.mp4.idam-bak").exists()
                && !dir.join("Clip.mp4.idam-bak").exists(),
            "case-only rename must not leave .idam-bak"
        );
        let leftover_bak = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".idam-bak")
            });
        assert!(!leftover_bak, "no .idam-bak residue in directory");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_file_overwrite_restore_failure_includes_bak_path() {
        let dir = unique_temp_dir("ovw_rb_fail");
        let src = dir.join("new.mp4");
        let dest = dir.join("old.mp4");
        fs::write(&src, b"new-bytes").unwrap();
        fs::write(&dest, b"old-bytes").unwrap();
        overwrite_fault::arm_fail_after_backup();
        overwrite_fault::arm_fail_restore();
        let err = move_file_overwrite(&src, &dest).expect_err("injected double fail");
        overwrite_fault::clear();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("idam-bak") || msg.contains("occupant"),
            "combined error must mention bak path: {msg}"
        );
        assert!(
            msg.contains("restore"),
            "combined error must mention restore failure: {msg}"
        );
        let bak = dir.join("old.mp4.idam-bak");
        assert!(
            bak.exists(),
            "occupant must remain in .idam-bak when restore is injected to fail"
        );
        assert_eq!(fs::read(&bak).unwrap(), b"old-bytes");
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

    /// #61: render must not emit `/` on Windows (native PathBuf::push only)
    #[cfg(windows)]
    #[test]
    fn render_output_path_uses_native_separators_on_windows() {
        let ctx = TemplateContext {
            studio: "StudioA".to_string(),
            date: "2024-06-01".to_string(),
            title: "Demo".to_string(),
            number: "ABC-1".to_string(),
            filename: "ABC-1.mp4".to_string(),
            ext: "mp4".to_string(),
            actor: String::new(),
        };
        let base = PathBuf::from(r"C:\lib");
        let dest = render_output_path("{{studio}}/{{date}}_{{title}}.{{ext}}", &ctx, &base)
            .expect("render");
        let s = dest.to_string_lossy();
        assert!(
            !s.contains('/'),
            "Windows rendered path must not contain '/': {s}"
        );
        assert_eq!(dest, base.join("StudioA").join("2024-06-01_Demo.mp4"));
    }

    #[cfg(windows)]
    #[test]
    fn normalize_db_path_windows_slash_drive_and_collapse() {
        assert_eq!(
            normalize_db_path(r"c:/lib/StudioA/x.mp4"),
            r"C:\lib\StudioA\x.mp4"
        );
        assert_eq!(
            normalize_db_path(r"C:\lib\\StudioA\/x.mp4"),
            r"C:\lib\StudioA\x.mp4"
        );
        assert_eq!(
            normalize_db_path(r"C:\lib\StudioA\x.mp4"),
            r"C:\lib\StudioA\x.mp4"
        );
        // Lowercase drive with native separators
        assert_eq!(
            normalize_db_path(r"c:\lib\StudioA\x.mp4"),
            r"C:\lib\StudioA\x.mp4"
        );
    }

    #[cfg(windows)]
    #[test]
    fn normalize_db_path_windows_unc_and_verbatim_prefixes() {
        assert_eq!(
            normalize_db_path(r"\\server\share\StudioA\x.mp4"),
            r"\\server\share\StudioA\x.mp4"
        );
        assert_eq!(
            normalize_db_path(r"\\server/share/StudioA/x.mp4"),
            r"\\server\share\StudioA\x.mp4"
        );
        // \\?\C:\ → C:\
        assert_eq!(
            normalize_db_path(r"\\?\C:\lib\StudioA\x.mp4"),
            r"C:\lib\StudioA\x.mp4"
        );
        assert_eq!(
            normalize_db_path(r"\\?\c:\lib/StudioA/x.mp4"),
            r"C:\lib\StudioA\x.mp4"
        );
        // \\?\UNC\server\share → \\server\share
        assert_eq!(
            normalize_db_path(r"\\?\UNC\server\share\StudioA\x.mp4"),
            r"\\server\share\StudioA\x.mp4"
        );
        assert_eq!(
            normalize_db_path(r"\\?\unc\server/share/StudioA/x.mp4"),
            r"\\server\share\StudioA\x.mp4"
        );
        // Never convert `/` while the verbatim prefix is still present:
        // stripping first then converting yields a drive path, not `\\?\C:/...`
        let verbatim_mixed = r"\\?\C:/lib/StudioA/x.mp4";
        let out = normalize_db_path(verbatim_mixed);
        assert!(!out.starts_with(r"\\?\"), "verbatim prefix must be stripped: {out}");
        assert_eq!(out, r"C:\lib\StudioA\x.mp4");
    }

    /// #63: trailing separators must be stripped (roots keep a single separator)
    #[test]
    fn normalize_db_path_strips_trailing_separators() {
        #[cfg(windows)]
        {
            assert_eq!(
                normalize_db_path(r"C:\lib\StudioA\"),
                r"C:\lib\StudioA"
            );
            assert_eq!(
                normalize_db_path(r"C:\lib\StudioA\\"),
                r"C:\lib\StudioA"
            );
            assert_eq!(normalize_db_path(r"C:\"), r"C:\");
            assert_eq!(
                normalize_db_path(r"\\server\share\StudioA\"),
                r"\\server\share\StudioA"
            );
            assert_eq!(
                normalize_db_path(r"\\server\share\"),
                r"\\server\share"
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(normalize_db_path("/lib/StudioA/"), "/lib/StudioA");
            assert_eq!(normalize_db_path("/lib/StudioA///"), "/lib/StudioA");
            assert_eq!(normalize_db_path("/"), "/");
        }
    }

    /// #63: `\\?\Volume{…}` must stay a volume path; `\\.\` drive form folds to `C:\`
    #[cfg(windows)]
    #[test]
    fn normalize_db_path_windows_volume_and_dot_prefixes() {
        let guid = r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\lib\x.mp4";
        let out = normalize_db_path(guid);
        assert!(
            out.starts_with(r"\\?\Volume{"),
            "Volume GUID prefix must be kept: {out}"
        );
        assert_eq!(
            out,
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\lib\x.mp4"
        );
        // Mixed separators under Volume — still Volume, never bare `Volume{…}`
        let mixed = r"\\?\Volume{12345678-1234-1234-1234-123456789abc}/lib/x.mp4";
        let mixed_out = normalize_db_path(mixed);
        assert!(
            mixed_out.starts_with(r"\\?\Volume{"),
            "must not strip Volume to bare name: {mixed_out}"
        );
        assert!(!mixed_out.contains('/'), "seps normalized under Volume: {mixed_out}");

        assert_eq!(
            normalize_db_path(r"\\.\C:\lib\x.mp4"),
            r"C:\lib\x.mp4"
        );
        assert_eq!(
            normalize_db_path(r"\\.\c:/lib/x.mp4"),
            r"C:\lib\x.mp4"
        );
        let vol_dot = r"\\.\Volume{12345678-1234-1234-1234-123456789abc}\lib\x.mp4";
        let vol_dot_out = normalize_db_path(vol_dot);
        assert_eq!(
            vol_dot_out,
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\lib\x.mp4"
        );
        // Non-drive device namespace keeps \\.\
        assert_eq!(
            normalize_db_path(r"\\.\PhysicalDrive0"),
            r"\\.\PhysicalDrive0"
        );
    }

    /// #63: NFC only on macOS; Linux/Windows keep original codepoints
    #[test]
    fn normalize_db_path_nfc_macos_only_preserves_elsewhere() {
        // é as e + combining acute (NFD) vs precomposed (NFC)
        let nfd = "cafe\u{0301}";
        let nfc = "caf\u{e9}";
        assert_ne!(nfd, nfc, "precondition: NFD != NFC as Rust strings");
        #[cfg(windows)]
        let input = format!(r"C:\lib\{nfd}\x.mp4");
        #[cfg(not(windows))]
        let input = format!("/lib/{nfd}/x.mp4");
        let out = normalize_db_path(&input);
        #[cfg(target_os = "macos")]
        {
            assert_eq!(out, format!("/lib/{nfc}/x.mp4"));
            assert_eq!(normalize_db_path(&out), out, "NFC idempotent");
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(
                out.contains(nfd),
                "non-macOS must keep NFD codepoints got {out}"
            );
            assert_eq!(out, normalize_db_path(&out));
            // nfc only used on macOS branch; keep binding for assert_ne precondition
            let _ = nfc;
        }
    }

    /// #63: reserved device escape puts `_` before the FIRST dot
    #[test]
    fn sanitize_reserved_puts_underscore_before_first_dot() {
        assert_eq!(sanitize_segment("CON.tar.gz"), "CON_.tar.gz");
        assert_eq!(sanitize_segment("CON.foo"), "CON_.foo");
        assert_eq!(sanitize_segment("con.mp4"), "con_.mp4");
        assert_eq!(sanitize_filename_segment("CON.tar.gz"), "CON_.tar.gz");
        assert_eq!(sanitize_filename_segment("COM1.foo.bar"), "COM1_.foo.bar");
        // Still reserved if `_` were only appended after the last component
        assert!(!is_windows_reserved_device_name("CON_.tar.gz"));
        assert!(!is_windows_reserved_device_name("CON_.foo"));
        assert!(is_windows_reserved_device_name("CON.foo_"));
    }
}
