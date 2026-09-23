mod database;
mod file_stability;
mod file_status;
mod scanner;
mod ffmpeg;
mod path_template;
mod phash;
mod pipeline;
mod scraper;
mod settings;
mod watcher;
mod commands;

use database::Database;
use settings::SettingsStore;
use std::sync::{Arc, Mutex};
use tauri::Manager;
use watcher::WatcherManager;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // 初始化資料庫與設定儲存（同屬 app data dir）
            let app_data_dir = app.path().app_data_dir().unwrap();
            std::fs::create_dir_all(&app_data_dir).unwrap();
            let db_path = app_data_dir.join("intelligent_dam.db");

            let db = Database::new(db_path.to_str().unwrap())
                .expect("Failed to initialize database");
            let settings_store = SettingsStore::new(&app_data_dir);

            // Arc so Watcher ingest worker can share the same mutex
            app.manage(Arc::new(Mutex::new(db)));
            app.manage(Mutex::new(settings_store));
            app.manage(WatcherManager::new());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::scan_directory_command,
            commands::scan_multiple_directories_command,
            commands::get_all_files_command,
            commands::scrape_file_command,
            commands::update_file_metadata_command,
            commands::generate_preview_command,
            commands::generate_thumbnail_command,
            commands::check_ffmpeg_available_command,
            commands::get_settings_command,
            commands::save_settings_command,
            commands::start_watcher_command,
            commands::stop_watcher_command,
            commands::get_watcher_status_command,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
