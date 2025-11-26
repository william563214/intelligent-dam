use crate::database::{Database, FileWithMetadata, Metadata};
use crate::ffmpeg;
use crate::scanner;
use crate::scraper::{scrape_metadata, to_database_metadata};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::State;

#[derive(Debug, Serialize, Deserialize)]
pub struct ScanResult {
    pub imported_count: usize,
    pub file_ids: Vec<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ScrapeResult {
    pub success: bool,
    pub metadata: Option<Metadata>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn scan_directory_command(
    path: String,
    db: State<'_, Mutex<Database>>,
) -> Result<ScanResult, String> {
    let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
    
    let imported_ids = scanner::scan_and_import(&db, &path)
        .map_err(|e| format!("Scan failed: {}", e))?;

    Ok(ScanResult {
        imported_count: imported_ids.len(),
        file_ids: imported_ids,
    })
}

#[tauri::command]
pub async fn scan_multiple_directories_command(
    paths: Vec<String>,
    db: State<'_, Mutex<Database>>,
) -> Result<ScanResult, String> {
    if paths.is_empty() {
        return Err("No directories provided".to_string());
    }

    let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
    
    let imported_ids = scanner::scan_multiple_directories(&db, paths)
        .map_err(|e| format!("Scan failed: {}", e))?;

    Ok(ScanResult {
        imported_count: imported_ids.len(),
        file_ids: imported_ids,
    })
}

#[tauri::command]
pub async fn get_all_files_command(
    db: State<'_, Mutex<Database>>,
) -> Result<Vec<FileWithMetadata>, String> {
    let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
    
    db.get_files_with_metadata()
        .map_err(|e| format!("Failed to get files: {}", e))
}

#[tauri::command]
pub async fn scrape_file_command(
    file_id: i64,
    avdc_path: Option<String>,
    stashdb_url: Option<String>,
    db: State<'_, Mutex<Database>>,
) -> Result<ScrapeResult, String> {
    // 獲取檔案資訊（在 await 之前完成）
    let (file_path, phash) = {
        let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
        let file = db.get_file_by_id(file_id)
            .map_err(|e| format!("File not found: {}", e))?;
        (PathBuf::from(&file.path), file.phash.clone())
    };
    
    // 執行刮削（不需要持有鎖）
    match scrape_metadata(
        &file_path,
        phash,
        avdc_path.as_deref(),
        stashdb_url.as_deref(),
    ).await {
        Ok((scraped_meta, source_type)) => {
            // 轉換為資料庫格式
            let db_metadata = to_database_metadata(scraped_meta, file_id, source_type);
            
            // 儲存到資料庫（重新獲取鎖）
            {
                let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
                db.update_metadata(&db_metadata)
                    .map_err(|e| format!("Failed to save metadata: {}", e))?;
            }
            
            Ok(ScrapeResult {
                success: true,
                metadata: Some(db_metadata),
                error: None,
            })
        }
        Err(e) => {
            Ok(ScrapeResult {
                success: false,
                metadata: None,
                error: Some(e.to_string()),
            })
        }
    }
}

#[tauri::command]
pub async fn update_file_metadata_command(
    file_id: i64,
    metadata: Metadata,
    db: State<'_, Mutex<Database>>,
) -> Result<(), String> {
    let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
    
    db.update_metadata(&metadata)
        .map_err(|e| format!("Failed to update metadata: {}", e))
}

#[tauri::command]
pub async fn generate_preview_command(
    file_id: i64,
    db: State<'_, Mutex<Database>>,
) -> Result<String, String> {
    // 獲取檔案資訊（在 await 之前完成）
    let file_path = {
        let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
        let file = db.get_file_by_id(file_id)
            .map_err(|e| format!("File not found: {}", e))?;
        PathBuf::from(&file.path)
    };
    
    // 生成輸出路徑
    let output_dir = std::env::temp_dir().join("intelligent_dam");
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("Failed to create output directory: {}", e))?;
    
    let sprite_path = output_dir.join(format!("sprite_{}.jpg", file_id));
    
    // 生成 Sprite Sheet（不需要持有鎖）
    let vtt_path_str = ffmpeg::generate_sprite_sheet(&file_path, &sprite_path, (10, 10))
        .await
        .map_err(|e| format!("Failed to generate sprite sheet: {}", e))?;
    
    // 更新資料庫中的 sprite_sheet_path（重新獲取鎖）
    {
        let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
        if let Some(mut metadata) = db.get_metadata(file_id)
            .map_err(|e| format!("Failed to get metadata: {}", e))? {
            metadata.sprite_sheet_path = Some(vtt_path_str.clone());
            db.update_metadata(&metadata)
                .map_err(|e| format!("Failed to update metadata: {}", e))?;
        } else {
            // 創建新的 metadata
            let new_metadata = Metadata {
                file_id,
                title: None,
                studio: None,
                release_date: None,
                summary: None,
                cover_image: None,
                sprite_sheet_path: Some(vtt_path_str.clone()),
                source_type: None,
            };
            db.update_metadata(&new_metadata)
                .map_err(|e| format!("Failed to create metadata: {}", e))?;
        }
    }
    
    Ok(vtt_path_str)
}

#[tauri::command]
pub async fn generate_thumbnail_command(
    file_id: i64,
    timestamp: Option<f64>,
    db: State<'_, Mutex<Database>>,
) -> Result<String, String> {
    // 獲取檔案資訊（在 await 之前完成）
    let file_path = {
        let db = db.lock().map_err(|e| format!("Database lock error: {}", e))?;
        let file = db.get_file_by_id(file_id)
            .map_err(|e| format!("File not found: {}", e))?;
        PathBuf::from(&file.path)
    };
    
    // 生成輸出路徑
    let output_dir = std::env::temp_dir().join("intelligent_dam");
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("Failed to create output directory: {}", e))?;
    
    let thumbnail_path = output_dir.join(format!("thumb_{}.jpg", file_id));
    
    // 生成縮圖（不需要持有鎖）
    ffmpeg::generate_thumbnail(&file_path, &thumbnail_path, timestamp)
        .await
        .map_err(|e| format!("Failed to generate thumbnail: {}", e))?;
    
    Ok(thumbnail_path.to_str().unwrap().to_string())
}

#[tauri::command]
pub fn check_ffmpeg_available_command() -> bool {
    ffmpeg::check_ffmpeg_available()
}

