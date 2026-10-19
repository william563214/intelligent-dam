use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[allow(non_upper_case_globals)]
const settings_filename: &str = "settings.json";

#[allow(non_upper_case_globals)]
const settings_temp_extension: &str = "json.tmp";

#[allow(non_upper_case_globals)]
const settings_backup_extension: &str = "json.bak";

fn default_stashdb_url() -> String {
    "https://stashdb.org".to_string()
}

fn default_output_template() -> String {
    // Relative templates resolve against the source file's parent Prefer an
    // absolute library root in Settings when possible Moves under a recursive
    // watch tree are suppressed + skip-already-organized so loops are safe
    "{{studio}}/{{date}}_{{title}}.{{ext}}".to_string()
}

fn default_near_phash_max_distance_setting() -> u32 {
    crate::phash::default_near_phash_max_distance
}

/// Clamp persisted near-phash threshold to a sane Hamming range.
pub fn clamp_near_phash_max_distance(value: u32) -> u32 {
    crate::phash::clamp_near_phash_max_distance(value)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    /// Max Hamming distance for near-phash suspected matches (default 4).
    /// Env `IDAM_NEAR_PHASH_MAX_DISTANCE` overrides at resolve time.
    #[serde(default = "default_near_phash_max_distance_setting")]
    pub near_phash_max_distance: u32,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            scan_directories: Vec::new(),
            avdc_path: String::new(),
            stashdb_url: default_stashdb_url(),
            output_template: default_output_template(),
            near_phash_max_distance: default_near_phash_max_distance_setting(),
        }
    }
}

impl AppSettings {
    /// Effective near-phash threshold (env wins over this setting).
    pub fn effective_near_phash_max_distance(&self) -> u32 {
        crate::phash::resolve_near_phash_max_distance(Some(self.near_phash_max_distance))
    }

    /// Normalize fields before persist.
    pub fn normalize_for_save(&mut self) {
        self.near_phash_max_distance =
            clamp_near_phash_max_distance(self.near_phash_max_distance);
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

    fn temp_path(&self) -> PathBuf {
        self.path.with_extension(settings_temp_extension)
    }

    fn backup_path(&self) -> PathBuf {
        self.path.with_extension(settings_backup_extension)
    }

    fn backup_corrupt_file(&self) -> Result<()> {
        let backup_path = self.backup_path();
        let _ = fs::remove_file(&backup_path);
        fs::rename(&self.path, &backup_path).with_context(|| {
            format!(
                "Failed to back up corrupt settings to {}",
                backup_path.display()
            )
        })?;
        Ok(())
    }

    fn replace_atomically(&self, temp_path: &Path) -> Result<()> {
        match fs::rename(temp_path, &self.path) {
            Ok(()) => Ok(()),
            Err(_) => {
                // Windows cannot rename over an existing file
                let _ = fs::remove_file(&self.path);
                fs::rename(temp_path, &self.path).with_context(|| {
                    format!("Failed to replace settings at {}", self.path.display())
                })
            }
        }
    }

    pub fn load(&self) -> Result<AppSettings> {
        if !self.path.exists() {
            return Ok(AppSettings::default());
        }

        let content = fs::read_to_string(&self.path)
            .with_context(|| format!("Failed to read settings from {}", self.path.display()))?;

        match serde_json::from_str::<AppSettings>(&content) {
            Ok(settings) => Ok(settings),
            Err(_) => {
                // Corrupt JSON: preserve the bad file then keep callers unblocked
                if let Err(backup_err) = self.backup_corrupt_file() {
                    eprintln!(
                        "Failed to back up corrupt settings at {}: {:#}",
                        self.path.display(),
                        backup_err
                    );
                }
                Ok(AppSettings::default())
            }
        }
    }

    pub fn save(&self, settings: &AppSettings) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create settings directory {}", parent.display())
            })?;
        }

        let mut normalized = settings.clone();
        normalized.normalize_for_save();

        let content =
            serde_json::to_string_pretty(&normalized).context("Failed to serialize settings")?;
        let temp_path = self.temp_path();

        if let Err(write_err) = fs::write(&temp_path, &content) {
            let _ = fs::remove_file(&temp_path);
            return Err(write_err).with_context(|| {
                format!("Failed to write temp settings to {}", temp_path.display())
            });
        }

        if let Err(replace_err) = self.replace_atomically(&temp_path) {
            let _ = fs::remove_file(&temp_path);
            return Err(replace_err);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[allow(non_upper_case_globals)]
    static test_counter: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = test_counter.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "intelligent_dam_{}_{}_{}_{}",
            label,
            std::process::id(),
            nanos,
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_returns_defaults_when_missing() {
        let dir = unique_temp_dir("missing");
        let store = SettingsStore::new(&dir);
        let settings = store.load().unwrap();
        assert_eq!(settings, AppSettings::default());
        assert!(
            settings.output_template.contains("{{ext}}"),
            "default template should use {{{{ext}}}} got {}",
            settings.output_template
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = unique_temp_dir("roundtrip");
        let store = SettingsStore::new(&dir);
        let mut settings = AppSettings::default();
        settings.scan_directories = vec!["/videos".to_string(), "/clips".to_string()];
        settings.avdc_path = "/opt/avdc.py".to_string();
        settings.stashdb_url = "https://example.test".to_string();
        settings.output_template = "{{title}}.mp4".to_string();
        settings.near_phash_max_distance = 3;
        store.save(&settings).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, settings);
        assert_eq!(loaded.near_phash_max_distance, 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_uses_atomic_temp_then_rename() {
        let dir = unique_temp_dir("atomic");
        let store = SettingsStore::new(&dir);
        let mut settings = AppSettings::default();
        settings.avdc_path = "/atomic/avdc.py".to_string();
        store.save(&settings).unwrap();

        let settings_path = dir.join(settings_filename);
        let temp_path = settings_path.with_extension(settings_temp_extension);
        assert!(settings_path.exists());
        assert!(!temp_path.exists());

        let on_disk = fs::read_to_string(&settings_path).unwrap();
        let parsed: AppSettings = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(parsed.avdc_path, "/atomic/avdc.py");

        // Second save should still replace cleanly without leaving tmp
        settings.avdc_path = "/atomic/avdc2.py".to_string();
        store.save(&settings).unwrap();
        assert!(settings_path.exists());
        assert!(!temp_path.exists());
        let loaded = store.load().unwrap();
        assert_eq!(loaded.avdc_path, "/atomic/avdc2.py");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_backs_up_corrupt_json_and_returns_defaults() {
        let dir = unique_temp_dir("corrupt");
        let store = SettingsStore::new(&dir);
        let settings_path = dir.join(settings_filename);
        let backup_path = settings_path.with_extension(settings_backup_extension);
        fs::write(&settings_path, "{not-valid-json").unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded, AppSettings::default());
        assert!(backup_path.exists());
        assert!(!settings_path.exists());
        let backup_content = fs::read_to_string(&backup_path).unwrap();
        assert_eq!(backup_content, "{not-valid-json");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_corrupt_json_does_not_error_for_callers() {
        let dir = unique_temp_dir("corrupt_ok");
        let store = SettingsStore::new(&dir);
        fs::write(dir.join(settings_filename), "[]").unwrap();
        assert!(store.load().is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_legacy_json_without_near_phash_uses_default() {
        let dir = unique_temp_dir("legacy_phash");
        let store = SettingsStore::new(&dir);
        fs::write(
            dir.join(settings_filename),
            r#"{"scanDirectories":[],"avdcPath":"","stashdbUrl":"https://stashdb.org","outputTemplate":"{{ext}}"}"#,
        )
        .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(
            loaded.near_phash_max_distance,
            crate::phash::default_near_phash_max_distance
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_clamps_near_phash_max_distance() {
        let dir = unique_temp_dir("clamp_phash");
        let store = SettingsStore::new(&dir);
        let mut settings = AppSettings::default();
        settings.near_phash_max_distance = 99;
        store.save(&settings).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.near_phash_max_distance, 32);
        let _ = fs::remove_dir_all(&dir);
    }
}
