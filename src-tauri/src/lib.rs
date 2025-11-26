mod database;
mod scanner;
mod ffmpeg;
mod scraper;
mod commands;

use database::Database;
use std::sync::Mutex;
use tauri::Manager;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // 初始化資料庫
            let app_data_dir = app.path().app_data_dir().unwrap();
            std::fs::create_dir_all(&app_data_dir).unwrap();
            let db_path = app_data_dir.join("intelligent_dam.db");
            
            let db = Database::new(db_path.to_str().unwrap())
                .expect("Failed to initialize database");
            
            app.manage(Mutex::new(db));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::scan_directory_command,
            commands::get_all_files_command,
            commands::scrape_file_command,
            commands::update_file_metadata_command,
            commands::generate_preview_command,
            commands::generate_thumbnail_command,
            commands::check_ffmpeg_available_command,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
