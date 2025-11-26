use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub async fn generate_thumbnail(
    video_path: &Path,
    output_path: &Path,
    timestamp: Option<f64>,
) -> Result<()> {
    let timestamp_str = timestamp
        .map(|t| format!("{:.2}", t))
        .unwrap_or_else(|| "1.0".to_string());

    let output = Command::new("ffmpeg")
        .arg("-i")
        .arg(video_path.to_str().unwrap())
        .arg("-ss")
        .arg(&timestamp_str)
        .arg("-vframes")
        .arg("1")
        .arg("-y") // 覆蓋已存在的檔案
        .arg(output_path.to_str().unwrap())
        .output()
        .with_context(|| "Failed to execute ffmpeg")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("FFmpeg failed: {}", stderr);
    }

    Ok(())
}

pub async fn generate_sprite_sheet(
    video_path: &Path,
    output_path: &Path,
    grid_size: (u32, u32),
) -> Result<String> {
    let (cols, rows) = grid_size;
    let total_frames = cols * rows;
    
    // 先獲取影片時長
    let duration = get_video_duration(video_path)?;
    let fps = total_frames as f64 / duration;
    
    // 生成 Sprite Sheet
    let filter = format!(
        "fps={},scale=160:90,tile={}x{}",
        fps, cols, rows
    );

    let output = Command::new("ffmpeg")
        .arg("-i")
        .arg(video_path.to_str().unwrap())
        .arg("-vf")
        .arg(&filter)
        .arg("-frames:v")
        .arg(total_frames.to_string())
        .arg("-y")
        .arg(output_path.to_str().unwrap())
        .output()
        .with_context(|| "Failed to execute ffmpeg for sprite sheet")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("FFmpeg sprite sheet generation failed: {}", stderr);
    }

    // 生成 VTT 檔案
    let vtt_path = output_path.with_extension("vtt");
    generate_vtt_file(&vtt_path, output_path, grid_size, duration)?;

    Ok(vtt_path.to_str().unwrap().to_string())
}

fn get_video_duration(video_path: &Path) -> Result<f64> {
    let output = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(video_path.to_str().unwrap())
        .output()
        .with_context(|| "Failed to execute ffprobe")?;

    if !output.status.success() {
        anyhow::bail!("ffprobe failed to get video duration");
    }

    let duration_str = String::from_utf8_lossy(&output.stdout);
    let duration = duration_str
        .trim()
        .parse::<f64>()
        .with_context(|| format!("Failed to parse duration: {}", duration_str))?;

    Ok(duration)
}

fn generate_vtt_file(
    vtt_path: &Path,
    sprite_path: &Path,
    grid_size: (u32, u32),
    duration: f64,
) -> Result<()> {
    let (cols, rows) = grid_size;
    let total_frames = cols * rows;
    let frame_duration = duration / total_frames as f64;
    let sprite_width = 160;
    let sprite_height = 90;

    let sprite_filename = sprite_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sprite.jpg");

    let mut vtt_content = "WEBVTT\n\n".to_string();

    for i in 0..total_frames {
        let start_time = i as f64 * frame_duration;
        let end_time = (i + 1) as f64 * frame_duration;
        
        let col = i % cols;
        let row = i / cols;
        
        let x = col * sprite_width;
        let y = row * sprite_height;

        vtt_content.push_str(&format!(
            "{:02}:{:02}:{:02}.{:03} --> {:02}:{:02}:{:02}.{:03}\n",
            (start_time as u32) / 3600,
            ((start_time as u32) % 3600) / 60,
            (start_time as u32) % 60,
            ((start_time * 1000.0) as u32) % 1000,
            (end_time as u32) / 3600,
            ((end_time as u32) % 3600) / 60,
            (end_time as u32) % 60,
            ((end_time * 1000.0) as u32) % 1000,
        ));
        
        vtt_content.push_str(&format!(
            "{}#xywh={},{},{},{}\n\n",
            sprite_filename, x, y, sprite_width, sprite_height
        ));
    }

    std::fs::write(vtt_path, vtt_content)
        .with_context(|| format!("Failed to write VTT file: {:?}", vtt_path))?;

    Ok(())
}

pub fn check_ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

