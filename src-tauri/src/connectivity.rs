//! Settings connectivity / availability checks (issue #12)
//!
//! FFmpeg binary on PATH AVDC script path exists StashDB URL reachable
//! StashDB check is async so the Tauri command does not block the UI runtime

use crate::ffmpeg;
use crate::settings::AppSettings;
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectivityCheck {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectivityReport {
    pub ffmpeg: ConnectivityCheck,
    pub avdc: ConnectivityCheck,
    pub stashdb: ConnectivityCheck,
}

pub async fn check_all(settings: &AppSettings) -> ConnectivityReport {
    ConnectivityReport {
        ffmpeg: check_ffmpeg(),
        avdc: check_avdc(&settings.avdc_path),
        stashdb: check_stashdb(&settings.stashdb_url).await,
    }
}

pub fn check_ffmpeg() -> ConnectivityCheck {
    if ffmpeg::check_ffmpeg_available() {
        ConnectivityCheck {
            ok: true,
            message: "FFmpeg 可用".to_string(),
        }
    } else {
        ConnectivityCheck {
            ok: false,
            message: "找不到 ffmpeg 請確認已安裝並在 PATH 中".to_string(),
        }
    }
}

pub fn check_avdc(avdc_path: &str) -> ConnectivityCheck {
    let trimmed = avdc_path.trim();
    if trimmed.is_empty() {
        return ConnectivityCheck {
            ok: false,
            message: "尚未設定 AV_Data_Capture 路徑".to_string(),
        };
    }
    let path = Path::new(trimmed);
    if !path.exists() {
        return ConnectivityCheck {
            ok: false,
            message: format!("路徑不存在: {}", trimmed),
        };
    }
    if !path.is_file() {
        return ConnectivityCheck {
            ok: false,
            message: format!("路徑不是檔案: {}", trimmed),
        };
    }
    ConnectivityCheck {
        ok: true,
        message: format!("AVDC 腳本存在: {}", trimmed),
    }
}

/// Best-effort HTTP reachability for the configured StashDB base URL
pub async fn check_stashdb(stashdb_url: &str) -> ConnectivityCheck {
    let trimmed = stashdb_url.trim();
    if trimmed.is_empty() {
        return ConnectivityCheck {
            ok: false,
            message: "尚未設定 StashDB URL".to_string(),
        };
    }

    let url = trimmed.trim_end_matches('/').to_string();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ConnectivityCheck {
                ok: false,
                message: format!("無法建立 HTTP 客戶端: {}", e),
            }
        }
    };

    // Prefer a light GET on the base URL any 2xx/3xx/4xx means host is reachable
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_server_error() {
                ConnectivityCheck {
                    ok: false,
                    message: format!("StashDB 回應錯誤 HTTP {}", status.as_u16()),
                }
            } else {
                ConnectivityCheck {
                    ok: true,
                    message: format!("StashDB 可連線 HTTP {}", status.as_u16()),
                }
            }
        }
        Err(e) => ConnectivityCheck {
            ok: false,
            message: format!("無法連線 StashDB: {}", e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avdc_empty_path_fails() {
        let r = check_avdc("");
        assert!(!r.ok);
        assert!(r.message.contains("尚未設定"));
    }

    #[test]
    fn avdc_missing_path_fails() {
        let r = check_avdc("/no/such/avdc_script_xyz.py");
        assert!(!r.ok);
        assert!(r.message.contains("不存在"));
    }

    #[test]
    fn avdc_existing_file_passes() {
        let dir = std::env::temp_dir().join(format!(
            "idam_avdc_check_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("AV_Data_Capture.py");
        std::fs::write(&script, "# stub\n").unwrap();
        let r = check_avdc(script.to_str().unwrap());
        assert!(r.ok, "{}", r.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stashdb_empty_url_fails() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r = rt.block_on(check_stashdb("  "));
        assert!(!r.ok);
    }
}
