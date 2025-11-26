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
    // 確定 AV_Data_Capture 路徑
    let avdc_script = avdc_path
        .map(|p| PathBuf::from(p))
        .or_else(|| {
            // 嘗試常見的路徑
            let common_paths = vec![
                "AV_Data_Capture.py",
                "./AV_Data_Capture.py",
                "python -m AV_Data_Capture",
            ];
            None // 簡化處理，需要配置
        })
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
                for nfo_entry in entries {
                    if let Ok(nfo_entry) = nfo_entry {
                        let nfo_path = nfo_entry.path();
                        if nfo_path.extension().and_then(|e| e.to_str()) == Some("nfo") {
                            return Ok(nfo_path);
                        }
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

fn parse_nfo_file(nfo_path: &Path) -> Result<ScrapedMetadata> {
    let content = fs::read_to_string(nfo_path)
        .with_context(|| format!("Failed to read NFO file: {:?}", nfo_path))?;

    // 簡化的 XML 解析（使用正則表達式作為備選方案）
    // 如果 quick-xml 有問題，可以使用簡單的字串匹配
    let title = extract_xml_tag(&content, "title");
    let studio = extract_xml_tag(&content, "studio");
    let release_date = extract_xml_tag(&content, "releasedate");
    let summary = extract_xml_tag(&content, "plot");
    let mut cover_image = extract_xml_tag(&content, "thumb");
    
    let mut tags = Vec::new();
    // 提取所有 tag
    if let Ok(tag_re) = Regex::new(r"<tag>(.*?)</tag>") {
        for cap in tag_re.captures_iter(&content) {
            if let Some(tag) = cap.get(1) {
                tags.push(tag.as_str().to_string());
            }
        }
    }
    
    let mut actors = Vec::new();
    // 提取所有 actor/name
    if let Ok(actor_re) = Regex::new(r"<actor>.*?<name>(.*?)</name>.*?</actor>") {
        for cap in actor_re.captures_iter(&content) {
            if let Some(actor) = cap.get(1) {
                actors.push(actor.as_str().to_string());
            }
        }
    }

    // 如果找不到封面，嘗試在相同目錄查找圖片
    if cover_image.is_none() {
        let nfo_dir = nfo_path.parent().unwrap();
        for entry in fs::read_dir(nfo_dir).unwrap_or_else(|_| {
            fs::read_dir(".").unwrap()
        }) {
            if let Ok(entry) = entry {
                let path = entry.path();
                if let Some(ext) = path.extension() {
                    if ["jpg", "jpeg", "png"].contains(&ext.to_str().unwrap_or("").to_lowercase().as_str()) {
                        cover_image = Some(path.to_str().unwrap().to_string());
                        break;
                    }
                }
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
    // 優先嘗試 AV_Data_Capture
    match scrape_with_avdc(file_path, avdc_path).await {
        Ok(metadata) => return Ok((metadata, "scraped_avdc".to_string())),
        Err(e) => {
            eprintln!("AV_Data_Capture failed: {}", e);
        }
    }

    // 如果 AV_Data_Capture 失敗，嘗試 StashDB（需要 phash）
    if let Some(phash) = phash {
        match scrape_with_stashdb(&phash, stashdb_url).await {
            Ok(metadata) => return Ok((metadata, "scraped_stashdb".to_string())),
            Err(e) => {
                eprintln!("StashDB failed: {}", e);
            }
        }
    }

    // 都失敗了
    anyhow::bail!("All scraping methods failed")
}

// 將 ScrapedMetadata 轉換為資料庫 Metadata
pub fn to_database_metadata(
    scraped: ScrapedMetadata,
    file_id: i64,
    source_type: String,
) -> Metadata {
    Metadata {
        file_id,
        title: scraped.title,
        studio: scraped.studio,
        release_date: scraped.release_date,
        summary: scraped.summary,
        cover_image: scraped.cover_image,
        sprite_sheet_path: None,
        source_type: Some(source_type),
    }
}

