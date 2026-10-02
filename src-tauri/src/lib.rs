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
use std::path::Path;
use std::sync::{Arc, Mutex};
use tauri::Manager;
use task_queue::TaskQueue;
use watcher::WatcherManager;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

#[allow(non_upper_case_globals)]
const database_error_dialog_title: &str = "Intelligent DAM — 資料庫錯誤";

/// 組裝資料庫初始化失敗的使用者訊息 供對話框與日誌共用
pub(crate) fn format_database_init_error(
    db_path: &Path,
    err: &impl std::fmt::Display,
) -> String {
    format!(
        "無法初始化資料庫 應用程式即將結束\n\n路徑: {}\n錯誤: {}\n\n若持續失敗 請備份後刪除損壞的資料庫檔再重試",
        db_path.display(),
        err
    )
}

/// 同步顯示錯誤對話框 必須可在 Tauri setup 主執行緒呼叫
///
/// Linux 絕不使用 rfd gtk3 `show()`：其 `GtkGlobalThread::run_blocking` 會在
/// tao `run_return` / `with_thread_default` 持有的 default MainContext 內靜默死鎖
/// Linux 順序：zenity → kdialog → 僅 stderr（呼叫端已 eprintln）後由呼叫端 exit(1)
/// 桌面非 Linux（macOS / Windows）使用 rfd 原生對話框
fn show_blocking_error_dialog(title: &str, message: &str) {
    // 測試專用：略過任何對話框 UI 讓呼叫端直接 exit(1)
    // 正式發行路徑請勿設定 IDAM_SKIP_ERROR_DIALOG
    if std::env::var_os("IDAM_SKIP_ERROR_DIALOG").is_some() {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        // 若 zenity 已成功啟動但 display 失敗 仍視為已嘗試顯示（有 exit code 即返回）
        if let Ok(status) = std::process::Command::new("zenity")
            .arg("--error")
            .arg(format!("--title={title}"))
            .arg(format!("--text={message}"))
            .arg("--no-markup")
            .arg("--width=520")
            .status()
        {
            if status.code().is_some() {
                return;
            }
        }
        if let Ok(status) = std::process::Command::new("kdialog")
            .arg(format!("--title={title}"))
            .arg("--error")
            .arg(message)
            .status()
        {
            if status.code().is_some() {
                return;
            }
        }
        // 無 zenity / kdialog：不呼叫 rfd 僅依賴呼叫端 stderr
        return;
    }

    #[cfg(all(desktop, not(target_os = "linux")))]
    {
        let _ = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title(title)
            .set_description(message)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }
}

/// 顯示錯誤後以 exit(1) 結束 不回傳 Err 以免 Failed to setup app panic
fn show_database_init_error_and_exit(db_path: &Path, err: &impl std::fmt::Display) -> ! {
    let message = format_database_init_error(db_path, err);
    eprintln!("error: {message}");
    show_blocking_error_dialog(database_error_dialog_title, &message);
    std::process::exit(1);
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
                    show_database_init_error_and_exit(&db_path, &e);
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

#[cfg(test)]
mod tests {
    use super::format_database_init_error;
    use crate::database::Database;
    use std::path::PathBuf;

    #[test]
    fn format_database_init_error_includes_path_and_error() {
        let path = PathBuf::from("/tmp/broken.db");
        let msg = format_database_init_error(&path, &"file is not a database");
        assert!(msg.contains("/tmp/broken.db"));
        assert!(msg.contains("file is not a database"));
        assert!(msg.contains("無法初始化資料庫"));
    }

    #[test]
    fn database_new_fails_on_garbage_file() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.temp/tests");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(format!(
            "corrupt-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&path, b"this is not a sqlite database").expect("write garbage");
        let err = match Database::new(path.to_str().expect("utf8")) {
            Ok(_) => panic!("corrupt db should fail to open"),
            Err(e) => e,
        };
        let msg = format_database_init_error(&path, &err);
        assert!(msg.contains("無法初始化資料庫"));
        assert!(msg.contains(path.to_str().unwrap()));
        let _ = std::fs::remove_file(&path);
    }
}
