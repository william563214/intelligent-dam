use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[allow(non_upper_case_globals)]
const settings_filename: &str = "settings.json";

fn default_stashdb_url() -> String {
    "https://stashdb.org".to_string()
}

fn default_output_template() -> String {
    "{{studio}}/{{date}}_{{title}}.mp4".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    #[serde(default)]
    pub scan_directories: Vec<String>,
    #[serde(default)]
    pub avdc_path: String,
    #[serde(default = "default_stashdb_url")]
    pub stashdb_url: String,
    #[serde(default = "default_output_template")]
    pub output_template: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            scan_directories: Vec::new(),
            avdc_path: String::new(),
            stashdb_url: default_stashdb_url(),
            output_template: default_output_template(),
        }
    }
}

pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(app_data_dir: &Path) -> Self {
        Self {
            path: app_data_dir.join(settings_filename),
        }
    }

    pub fn load(&self) -> Result<AppSettings> {
        if !self.path.exists() {
            return Ok(AppSettings::default());
        }

        let content = fs::read_to_string(&self.path)
            .with_context(|| format!("Failed to read settings from {}", self.path.display()))?;
        let settings: AppSettings = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse settings from {}", self.path.display()))?;
        Ok(settings)
    }

    pub fn save(&self, settings: &AppSettings) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create settings directory {}", parent.display()))?;
        }

        let content = serde_json::to_string_pretty(settings)
            .context("Failed to serialize settings")?;
        fs::write(&self.path, content)
            .with_context(|| format!("Failed to write settings to {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn load_returns_defaults_when_missing() {
        let dir = env::temp_dir().join(format!(
            "intelligent_dam_settings_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = SettingsStore::new(&dir);
        let settings = store.load().unwrap();
        assert!(settings.scan_directories.is_empty());
        assert_eq!(settings.stashdb_url, "https://stashdb.org");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = env::temp_dir().join(format!(
            "intelligent_dam_settings_roundtrip_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = SettingsStore::new(&dir);
        let mut settings = AppSettings::default();
        settings.scan_directories = vec!["/videos".to_string(), "/clips".to_string()];
        settings.avdc_path = "/opt/avdc.py".to_string();
        settings.stashdb_url = "https://example.test".to_string();
        settings.output_template = "{{title}}.mp4".to_string();
        store.save(&settings).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.scan_directories, settings.scan_directories);
        assert_eq!(loaded.avdc_path, settings.avdc_path);
        assert_eq!(loaded.stashdb_url, settings.stashdb_url);
        assert_eq!(loaded.output_template, settings.output_template);
        let _ = fs::remove_dir_all(&dir);
    }
}
