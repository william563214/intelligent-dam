use crate::database::Metadata;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs;
use regex::Regex;

fn extract_xml_tag(content: &str, tag: &str) -> Option<String> {
    let pattern = format!(r"<{}>(.*?)</{}>", tag, tag);
    if let Ok(re) = Regex::new(&pattern) {
        if let Some(cap) = re.captures(content) {
            if let Some(text) = cap.get(1) {
                return Some(text.as_str().to_string());
            }
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrapedMetadata {
    pub title: Option<String>,
    pub studio: Option<String>,
    pub release_date: Option<String>,
    pub summary: Option<String>,
    pub cover_image: Option<String>,
    pub tags: Vec<String>,
    pub actors: Vec<String>,
}

// AV_Data_Capture 整合
pub async fn scrape_with_avdc(
    file_path: &Path,
    avdc_path: Option<&str>,
) -> Result<ScrapedMetadata> {
    // 確定 AV_Data_Capture 路徑 尚未設定時不猜測 由呼叫端從 Settings 提供
    let avdc_script = avdc_path
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("AV_Data_Capture path not configured"))?;

    // 執行 AV_Data_Capture
    let output = Command::new("python")
        .arg(avdc_script.to_str().unwrap())
        .arg(file_path.to_str().unwrap())
        .arg("-a") // 自動退出
        .output()
        .with_context(|| "Failed to execute AV_Data_Capture")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("AV_Data_Capture failed: {}", stderr);
    }

    // 查找輸出目錄（通常是 JAV_output）
    let output_dir = PathBuf::from("JAV_output");
    let file_stem = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // 查找對應的 NFO 檔案
    let nfo_path = find_nfo_file(&output_dir, file_stem)?;
    
    // 解析 NFO 檔案
    parse_nfo_file(&nfo_path)
}

fn find_nfo_file(output_dir: &Path, file_stem: &str) -> Result<PathBuf> {
    // 在輸出目錄中查找 NFO 檔案
    // AV_Data_Capture 通常會創建以番號或檔名命名的目錄
    for entry in fs::read_dir(output_dir)
        .with_context(|| format!("Failed to read output directory: {:?}", output_dir))?
    {
        let entry = entry?;
        let path = entry.path();
        
        if path.is_dir() {
            // 在子目錄中查找 NFO
            let nfo_path = path.join(format!("{}.nfo", file_stem));
            if nfo_path.exists() {
                return Ok(nfo_path);
            }
            
            // 或者查找目錄中第一個 NFO 檔案
            if let Ok(entries) = fs::read_dir(&path) {
                for nfo_entry in entries.flatten() {
                    let nfo_path = nfo_entry.path();
                    if nfo_path.extension().and_then(|e| e.to_str()) == Some("nfo") {
                        return Ok(nfo_path);
                    }
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("nfo") {
            // 直接在輸出目錄中的 NFO
            if path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .contains(file_stem)
            {
                return Ok(path);
            }
        }
    }

    anyhow::bail!("NFO file not found for {}", file_stem)
}

fn parse_nfo_content(content: &str) -> ScrapedMetadata {
    let title = extract_xml_tag(content, "title");
    let studio = extract_xml_tag(content, "studio");
    let release_date = extract_xml_tag(content, "releasedate")
        .or_else(|| extract_xml_tag(content, "premiered"));
    let summary = extract_xml_tag(content, "plot");
    let cover_image = extract_xml_tag(content, "thumb")
        .or_else(|| extract_xml_tag(content, "poster"));

    let mut tags = Vec::new();
    // AVDC NFO 常見為 <tag> 或 <genre>
    for tag_name in ["tag", "genre"] {
        let pattern = format!(r"(?is)<{}>(.*?)</{}>", tag_name, tag_name);
        if let Ok(tag_re) = Regex::new(&pattern) {
            for cap in tag_re.captures_iter(content) {
                if let Some(tag) = cap.get(1) {
                    let value = tag.as_str().trim().to_string();
                    if !value.is_empty() && !tags.iter().any(|t| t == &value) {
                        tags.push(value);
                    }
                }
            }
        }
    }

    let mut actors = Vec::new();
    // (?s) 讓 . 跨行 以解析多行 <actor><name>...</name></actor>
    if let Ok(actor_re) = Regex::new(r"(?is)<actor>.*?<name>(.*?)</name>.*?</actor>") {
        for cap in actor_re.captures_iter(content) {
            if let Some(actor) = cap.get(1) {
                let value = actor.as_str().trim().to_string();
                if !value.is_empty() && !actors.iter().any(|a| a == &value) {
                    actors.push(value);
                }
            }
        }
    }

    ScrapedMetadata {
        title,
        studio,
        release_date,
        summary,
        cover_image,
        tags,
        actors,
    }
}

fn parse_nfo_file(nfo_path: &Path) -> Result<ScrapedMetadata> {
    let content = fs::read_to_string(nfo_path)
        .with_context(|| format!("Failed to read NFO file: {:?}", nfo_path))?;

    let mut metadata = parse_nfo_content(&content);

    // 如果找不到封面 嘗試在相同目錄查找圖片
    if metadata.cover_image.is_none() {
        if let Some(nfo_dir) = nfo_path.parent() {
            if let Ok(entries) = fs::read_dir(nfo_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(ext) = path.extension() {
                        let ext_lower = ext.to_str().unwrap_or("").to_lowercase();
                        if ["jpg", "jpeg", "png"].contains(&ext_lower.as_str()) {
                            if let Some(path_str) = path.to_str() {
                                metadata.cover_image = Some(path_str.to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(metadata)
}

// StashDB API 整合
pub async fn scrape_with_stashdb(phash: &str, api_url: Option<&str>) -> Result<ScrapedMetadata> {
    let base_url = api_url.unwrap_or("https://stashdb.org");
    let url = format!("{}/api/v1/scenes?phash={}", base_url, phash);

    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| "Failed to request StashDB API")?;

    if !response.status().is_success() {
        anyhow::bail!("StashDB API returned error: {}", response.status());
    }

    let json: serde_json::Value = response
        .json()
        .await
        .with_context(|| "Failed to parse StashDB response")?;

    // 解析 StashDB JSON 回應
    let scene = json
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|arr| arr.first())
        .ok_or_else(|| anyhow::anyhow!("No scene found in StashDB response"))?;

    let title = scene.get("title").and_then(|t| t.as_str()).map(|s| s.to_string());
    let studio = scene
        .get("studio")
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string());
    let release_date = scene
        .get("date")
        .and_then(|d| d.as_str())
        .map(|s| s.to_string());
    let summary = scene
        .get("details")
        .and_then(|d| d.as_str())
        .map(|s| s.to_string());
    let cover_image = scene
        .get("image")
        .and_then(|i| i.as_str())
        .map(|s| s.to_string());

    let mut tags = Vec::new();
    if let Some(tags_array) = scene.get("tags").and_then(|t| t.as_array()) {
        for tag in tags_array {
            if let Some(name) = tag.get("name").and_then(|n| n.as_str()) {
                tags.push(name.to_string());
            }
        }
    }

    let mut actors = Vec::new();
    if let Some(performers_array) = scene.get("performers").and_then(|p| p.as_array()) {
        for performer in performers_array {
            if let Some(name) = performer.get("name").and_then(|n| n.as_str()) {
                actors.push(name.to_string());
            }
        }
    }

    Ok(ScrapedMetadata {
        title,
        studio,
        release_date,
        summary,
        cover_image,
        tags,
        actors,
    })
}

// 刮削策略：優先 AV_Data_Capture，失敗後嘗試 StashDB
pub async fn scrape_metadata(
    file_path: &Path,
    phash: Option<String>,
    avdc_path: Option<&str>,
    stashdb_url: Option<&str>,
) -> Result<(ScrapedMetadata, String)> {
    let mut errors: Vec<String> = Vec::new();

    // 優先嘗試 AV_Data_Capture
    match scrape_with_avdc(file_path, avdc_path).await {
        Ok(metadata) => return Ok((metadata, "scraped_avdc".to_string())),
        Err(e) => {
            let msg = format!("AV_Data_Capture failed: {}", e);
            eprintln!("{}", msg);
            errors.push(msg);
        }
    }

    // AVDC 失敗後 fallback 到 StashDB（需要 phash）
    match &phash {
        Some(hash) if !hash.trim().is_empty() => {
            match scrape_with_stashdb(hash, stashdb_url).await {
                Ok(metadata) => return Ok((metadata, "scraped_stashdb".to_string())),
                Err(e) => {
                    let msg = format!("StashDB failed: {}", e);
                    eprintln!("{}", msg);
                    errors.push(msg);
                }
            }
        }
        _ => {
            let msg = "StashDB skipped: phash not available".to_string();
            eprintln!("{}", msg);
            errors.push(msg);
        }
    }

    anyhow::bail!("All scraping methods failed: {}", errors.join("; "))
}

/// 將 ScrapedMetadata 的純文字欄位轉成資料庫 Metadata
/// tags / actors 需另外經 Database::apply_scraped_tags 寫入
/// existing_sprite_sheet_path 用於保留既有預覽路徑 避免刮削覆寫為空
pub fn to_database_metadata(
    scraped: &ScrapedMetadata,
    file_id: i64,
    source_type: String,
    existing_sprite_sheet_path: Option<String>,
) -> Metadata {
    Metadata {
        file_id,
        title: scraped.title.clone(),
        studio: scraped.studio.clone(),
        release_date: scraped.release_date.clone(),
        summary: scraped.summary.clone(),
        cover_image: scraped.cover_image.clone(),
        sprite_sheet_path: existing_sprite_sheet_path,
        source_type: Some(source_type),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_nfo_content_extracts_title_tags_and_actors() {
        let nfo = r#"
        <movie>
            <title>Sample Title</title>
            <studio>Demo Studio</studio>
            <releasedate>2024-01-02</releasedate>
            <plot>A short plot</plot>
            <thumb>https://example.test/cover.jpg</thumb>
            <genre>Outdoor</genre>
            <tag>Beach</tag>
            <actor>
                <name>Alice</name>
                <type>Actor</type>
            </actor>
            <actor>
                <name>Bob</name>
            </actor>
        </movie>
        "#;

        let scraped = parse_nfo_content(nfo);
        assert_eq!(scraped.title.as_deref(), Some("Sample Title"));
        assert_eq!(scraped.studio.as_deref(), Some("Demo Studio"));
        assert_eq!(scraped.release_date.as_deref(), Some("2024-01-02"));
        assert_eq!(scraped.cover_image.as_deref(), Some("https://example.test/cover.jpg"));
        assert!(scraped.tags.contains(&"Outdoor".to_string()));
        assert!(scraped.tags.contains(&"Beach".to_string()));
        assert_eq!(scraped.actors, vec!["Alice".to_string(), "Bob".to_string()]);
    }

    #[test]
    fn to_database_metadata_keeps_core_fields() {
        let scraped = ScrapedMetadata {
            title: Some("T".to_string()),
            studio: Some("S".to_string()),
            release_date: Some("2020-01-01".to_string()),
            summary: Some("Sum".to_string()),
            cover_image: Some("cover.jpg".to_string()),
            tags: vec!["t1".to_string()],
            actors: vec!["a1".to_string()],
        };
        let meta = to_database_metadata(&scraped, 42, "scraped_avdc".to_string(), None);
        assert_eq!(meta.file_id, 42);
        assert_eq!(meta.title.as_deref(), Some("T"));
        assert_eq!(meta.source_type.as_deref(), Some("scraped_avdc"));
        assert!(meta.sprite_sheet_path.is_none());
    }

    #[test]
    fn to_database_metadata_preserves_existing_sprite_path() {
        let scraped = ScrapedMetadata {
            title: Some("T".to_string()),
            studio: None,
            release_date: None,
            summary: None,
            cover_image: None,
            tags: Vec::new(),
            actors: Vec::new(),
        };
        let meta = to_database_metadata(
            &scraped,
            1,
            "scraped_stashdb".to_string(),
            Some("/tmp/sprite.jpg".to_string()),
        );
        assert_eq!(meta.sprite_sheet_path.as_deref(), Some("/tmp/sprite.jpg"));
    }
}

