mod database;
mod connectivity;
mod file_stability;
mod file_status;
mod scanner;
mod ffmpeg;
mod path_suppression;
mod path_template;
mod phash;
mod pipeline;
mod scraper;
mod settings;
mod task_queue;
mod watcher;
mod commands;

use database::Database;
use settings::SettingsStore;
use std::sync::{mpsc, Arc, Mutex};
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use task_queue::TaskQueue;
use watcher::WatcherManager;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

fn show_database_init_error(
    app: &tauri::App,
    db_path: &std::path::Path,
    err: &impl std::fmt::Display,
) {
    let message = format!(
        "無法初始化資料庫 應用程式即將結束\n\n路徑: {}\n錯誤: {}\n\n若持續失敗 請備份後刪除損壞的資料庫檔再重試",
        db_path.display(),
        err
    );
    eprintln!("error: {message}");
    // blocking_show 不可在主執行緒呼叫 改背景執行緒顯示後再結束
    let handle = app.handle().clone();
    let (tx, rx) = mpsc::sync_channel(0);
    std::thread::spawn(move || {
        handle
            .dialog()
            .message(&message)
            .title("Intelligent DAM — 資料庫錯誤")
            .kind(MessageDialogKind::Error)
            .blocking_show();
        let _ = tx.send(());
    });
    let _ = rx.recv();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // 初始化資料庫與設定儲存（同屬 app data dir）
            let app_data_dir = app.path().app_data_dir().unwrap();
            std::fs::create_dir_all(&app_data_dir).unwrap();
            let db_path = app_data_dir.join("intelligent_dam.db");

            let db = match Database::new(db_path.to_str().unwrap()) {
                Ok(db) => db,
                Err(e) => {
                    show_database_init_error(app, &db_path, &e);
                    return Err(Box::new(e));
                }
            };
            let settings_store = SettingsStore::new(&app_data_dir);

            // Arc so Watcher ingest worker can share the same mutex
            app.manage(Arc::new(Mutex::new(db)));
            app.manage(Mutex::new(settings_store));
            app.manage(WatcherManager::new());
            app.manage(Arc::new(TaskQueue::new()));
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
            commands::get_task_queue_status_command,
            commands::retry_task_command,
            commands::test_connectivity_command,
            commands::canonicalize_path_command,
            commands::get_error_logs_command,
            commands::clear_error_logs_command,
            commands::enqueue_pipeline_jobs_command,
            commands::organize_eligible_files_command,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
