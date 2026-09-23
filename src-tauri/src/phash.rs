//! Video perceptual hash (phash) aligned with Stash / StashDB.
//!
//! Algorithm (same structure as stashapp/stash `videophash`):
//! 1. Sample 25 frames between 5% and 95% of duration via ffmpeg
//! 2. Scale each frame to 160x160 and tile into a 5x5 sprite
//! 3. Compute a 64-bit DCT perceptual hash (goimagehash-style) on the sprite
//!
//! Stored as lowercase hex (`{:016x}`) for StashDB `?phash=` lookups.
//! Frame extraction failure must not abort import callers should treat Err as skip.
//!
//! # Near-duplicate policy (MVP / issue #11)
//!
//! Hamming distance on the 64-bit value. Two hashes are near-identical when
//! `hamming_distance <= near_phash_max_distance` (default **8**, Stash-style).
//! Ingest strategy: **skip with log + keep the existing Files row** (delete the
//! provisional newer row). No duplicate-link UX in Phase 1.

use anyhow::{bail, Context, Result};
use image::{imageops, DynamicImage, GenericImageView, GrayImage, ImageBuffer, Luma, Rgba};
use std::fs;
use std::path::{Path, PathBuf};

use crate::ffmpeg;

#[allow(non_upper_case_globals)]
const screenshot_size: u32 = 160;
#[allow(non_upper_case_globals)]
const sprite_columns: u32 = 5;
#[allow(non_upper_case_globals)]
const sprite_rows: u32 = 5;
#[allow(non_upper_case_globals)]
const dct_size: usize = 64;
#[allow(non_upper_case_globals)]
const hash_size: usize = 8;

/// Max Hamming distance treated as the same / near-identical video (MVP).
#[allow(non_upper_case_globals)]
pub const near_phash_max_distance: u32 = 8;

/// Parse a 16-char hex phash into a u64. Accepts optional `0x` prefix.
pub fn parse_phash_hex(hex: &str) -> Option<u64> {
    let trimmed = hex.trim();
    if trimmed.is_empty() {
        return None;
    }
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if body.len() != 16 || !body.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(body, 16).ok()
}

/// Bitwise Hamming distance between two 64-bit hashes.
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// True when distance is within the MVP near-duplicate threshold.
pub fn is_near_duplicate(a: u64, b: u64) -> bool {
    hamming_distance(a, b) <= near_phash_max_distance
}

/// Compare two hex phash strings. Returns None if either fails to parse.
pub fn hex_hamming_distance(a: &str, b: &str) -> Option<u32> {
    let left = parse_phash_hex(a)?;
    let right = parse_phash_hex(b)?;
    Some(hamming_distance(left, right))
}

/// Compute a Stash-style video phash for `file_path`.
pub fn calculate_video_phash(file_path: &Path) -> Result<String> {
    if !ffmpeg::check_ffmpeg_available() {
        bail!("ffmpeg is not available");
    }

    let duration = ffmpeg::get_video_duration(file_path)
        .with_context(|| format!("ffprobe duration failed for {:?}", file_path))?;

    if duration <= 0.0 || !duration.is_finite() {
        bail!("invalid video duration: {}", duration);
    }

    let work_dir = make_work_dir(file_path)?;
    let result = (|| {
        let frames = extract_sprite_frames(file_path, duration, &work_dir)?;
        let sprite = combine_sprite(&frames)?;
        let hash = perception_hash(&sprite);
        Ok(format!("{:016x}", hash))
    })();

    let _ = fs::remove_dir_all(&work_dir);
    result
}

fn make_work_dir(file_path: &Path) -> Result<PathBuf> {
    let stem = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    let unique = format!(
        "idam-phash-{}-{}",
        stem,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique);
    fs::create_dir_all(&dir).with_context(|| format!("create temp dir {:?}", dir))?;
    Ok(dir)
}

fn extract_sprite_frames(
    file_path: &Path,
    duration: f64,
    work_dir: &Path,
) -> Result<Vec<DynamicImage>> {
    let chunk_count = (sprite_columns * sprite_rows) as usize;
    let offset = 0.05 * duration;
    let step_size = (0.9 * duration) / chunk_count as f64;

    let mut frames = Vec::with_capacity(chunk_count);
    for i in 0..chunk_count {
        let time = offset + (i as f64) * step_size;
        let out = work_dir.join(format!("frame_{:02}.png", i));
        ffmpeg::extract_frame_at(file_path, &out, time, screenshot_size)
            .with_context(|| format!("extract frame {} at {:.3}s from {:?}", i, time, file_path))?;

        let img = image::open(&out)
            .with_context(|| format!("decode extracted frame {:?}", out))?;
        frames.push(img);
    }
    Ok(frames)
}

fn combine_sprite(frames: &[DynamicImage]) -> Result<DynamicImage> {
    if frames.is_empty() {
        bail!("no frames for sprite");
    }
    let (width, height) = frames[0].dimensions();
    let canvas_w = width * sprite_columns;
    let canvas_h = height * sprite_rows;
    let mut canvas: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(canvas_w, canvas_h, Rgba([0, 0, 0, 255]));

    for (index, frame) in frames.iter().enumerate() {
        let col = (index as u32) % sprite_columns;
        let row = (index as u32) / sprite_columns;
        let x = (col * width) as i64;
        let y = (row * height) as i64;
        imageops::replace(&mut canvas, &frame.to_rgba8(), x, y);
    }

    Ok(DynamicImage::ImageRgba8(canvas))
}

/// goimagehash-style PerceptionHash: 64x64 gray, DCT, top-left 8x8 vs median.
pub fn perception_hash(img: &DynamicImage) -> u64 {
    let resized = img.resize_exact(
        dct_size as u32,
        dct_size as u32,
        imageops::FilterType::Triangle,
    );
    let gray: GrayImage = resized.to_luma8();

    let mut pixels = vec![0.0_f64; dct_size * dct_size];
    for (i, Luma([v])) in gray.pixels().enumerate() {
        pixels[i] = f64::from(*v);
    }

    let low = dct2d_lowfreq(&pixels, dct_size, hash_size);
    let median = median_f64(&low);

    let mut hash: u64 = 0;
    for (idx, p) in low.iter().enumerate() {
        if *p > median {
            // MSB-first bit packing matches goimagehash leftShiftSet(64 - idx - 1)
            hash |= 1u64 << (63 - idx);
        }
    }
    hash
}

/// Separable DCT-II then keep the top-left `hash_n x hash_n` coefficients (row-major).
fn dct2d_lowfreq(input: &[f64], n: usize, hash_n: usize) -> Vec<f64> {
    // Row-wise DCT
    let mut rows = vec![0.0_f64; n * n];
    let mut buf = vec![0.0_f64; n];
    for y in 0..n {
        let row = &input[y * n..(y + 1) * n];
        dct1d(row, &mut buf);
        rows[y * n..(y + 1) * n].copy_from_slice(&buf);
    }

    // Column-wise DCT, only first hash_n columns needed for low-freq block
    let mut out = vec![0.0_f64; hash_n * hash_n];
    let mut col_in = vec![0.0_f64; n];
    let mut col_out = vec![0.0_f64; n];
    for x in 0..hash_n {
        for y in 0..n {
            col_in[y] = rows[y * n + x];
        }
        dct1d(&col_in, &mut col_out);
        for y in 0..hash_n {
            out[y * hash_n + x] = col_out[y];
        }
    }
    out
}

/// Unscaled DCT-II: X[k] = sum_n x[n] * cos(pi/N * (n+0.5) * k)
fn dct1d(input: &[f64], output: &mut [f64]) {
    let n = input.len();
    let pi_n = std::f64::consts::PI / n as f64;
    for k in 0..n {
        let mut sum = 0.0;
        for (n_idx, &x) in input.iter().enumerate() {
            sum += x * ((n_idx as f64 + 0.5) * k as f64 * pi_n).cos();
        }
        output[k] = sum;
    }
}

fn median_f64(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::process::Command;

    fn solid_image(color: [u8; 3], w: u32, h: u32) -> DynamicImage {
        let img = RgbImage::from_fn(w, h, |_, _| Rgb(color));
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn perception_hash_is_stable_for_same_image() {
        let img = solid_image([40, 80, 120], 320, 240);
        let a = perception_hash(&img);
        let b = perception_hash(&img);
        assert_eq!(a, b);
        assert_ne!(format!("{:016x}", a), "0000000000000000");
    }

    #[test]
    fn perception_hash_differs_for_unlike_images() {
        let dark = perception_hash(&solid_image([10, 10, 10], 200, 200));
        let bright = perception_hash(&solid_image([240, 240, 240], 200, 200));
        // Gradient vs flat can collide on uniform colors; use patterned images
        let mut left = RgbImage::new(128, 128);
        let mut right = RgbImage::new(128, 128);
        for y in 0..128 {
            for x in 0..128 {
                left.put_pixel(x, y, Rgb([(x as u8).wrapping_mul(3), y as u8, 50]));
                right.put_pixel(x, y, Rgb([y as u8, (x as u8).wrapping_mul(5), 200]));
            }
        }
        let h1 = perception_hash(&DynamicImage::ImageRgb8(left));
        let h2 = perception_hash(&DynamicImage::ImageRgb8(right));
        assert_ne!(h1, h2);
        // solid dark/bright may share structure; ensure patterned differ
        let _ = (dark, bright);
    }

    #[test]
    fn calculate_video_phash_on_synthetic_clip() {
        if !ffmpeg::check_ffmpeg_available() {
            eprintln!("skip: ffmpeg unavailable");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "idam-phash-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        let video = dir.join("clip.mp4");

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
        assert!(status.success(), "ffmpeg failed to create test clip");

        let hash = calculate_video_phash(&video).expect("phash");
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

        let hash2 = calculate_video_phash(&video).expect("phash again");
        assert_eq!(hash, hash2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn calculate_video_phash_fails_on_missing_file() {
        let missing = PathBuf::from("/tmp/definitely-missing-idam-phash-xyz.mp4");
        let err = calculate_video_phash(&missing).expect_err("should fail");
        let msg = format!("{:#}", err);
        assert!(!msg.is_empty());
    }

    #[test]
    fn hamming_distance_counts_bit_differences() {
        assert_eq!(hamming_distance(0, 0), 0);
        assert_eq!(hamming_distance(0b1111, 0b0000), 4);
        assert_eq!(hamming_distance(0xffff_ffff_ffff_ffff, 0), 64);
        let a = 0x0123_4567_89ab_cdef;
        assert_eq!(hamming_distance(a, a), 0);
        assert_eq!(hamming_distance(a, a ^ 0b111), 3);
    }

    #[test]
    fn parse_phash_hex_accepts_16_hex_digits() {
        assert_eq!(
            parse_phash_hex("0123456789abcdef"),
            Some(0x0123_4567_89ab_cdef)
        );
        assert_eq!(
            parse_phash_hex("0x0123456789ABCDEF"),
            Some(0x0123_4567_89ab_cdef)
        );
        assert!(parse_phash_hex("short").is_none());
        assert!(parse_phash_hex("").is_none());
        assert!(parse_phash_hex("gggggggggggggggg").is_none());
    }

    #[test]
    fn near_duplicate_threshold_is_eight() {
        assert_eq!(near_phash_max_distance, 8);
        let base = 0u64;
        // Flip 8 bits → still near
        let near = 0b1111_1111u64;
        assert!(is_near_duplicate(base, near));
        // Flip 9 bits → not near
        let far = 0b1_1111_1111u64;
        assert!(!is_near_duplicate(base, far));
        assert_eq!(hex_hamming_distance("0000000000000000", "00000000000000ff"), Some(8));
        assert_eq!(hex_hamming_distance("0000000000000000", "00000000000001ff"), Some(9));
    }
}
