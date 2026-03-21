use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::themes::ThemeName;
use crate::commands::autosubmit::AutosubmitConfig;

const DEFAULT_AUTO_REFRESH_MS: u64 = 60_000;
const MIN_AUTO_REFRESH_MS: u64 = 30_000;
const MAX_AUTO_REFRESH_MS: u64 = 3_600_000;

const DEFAULT_NATIVE_TIMEOUT_MS: u64 = 300_000;
const MIN_NATIVE_TIMEOUT_MS: u64 = 5_000;
const MAX_NATIVE_TIMEOUT_MS: u64 = 3_600_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_color_palette")]
    pub color_palette: String,
    #[serde(default)]
    pub auto_refresh_enabled: bool,
    #[serde(default = "default_auto_refresh_ms")]
    pub auto_refresh_ms: u64,
    #[serde(default)]
    pub include_unused_models: bool,
    #[serde(default = "default_native_timeout_ms")]
    pub native_timeout_ms: u64,
    #[serde(default)]
    pub autosubmit: Option<AutosubmitConfig>,
}

fn default_color_palette() -> String {
    "blue".to_string()
}

fn default_auto_refresh_ms() -> u64 {
    DEFAULT_AUTO_REFRESH_MS
}

fn default_native_timeout_ms() -> u64 {
    DEFAULT_NATIVE_TIMEOUT_MS
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            color_palette: default_color_palette(),
            auto_refresh_enabled: false,
            auto_refresh_ms: DEFAULT_AUTO_REFRESH_MS,
            include_unused_models: false,
            native_timeout_ms: DEFAULT_NATIVE_TIMEOUT_MS,
            autosubmit: None,
        }
    }
}

impl Settings {
    fn config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?
            .join("tokscale");

        if !config_dir.exists() {
            fs::create_dir_all(&config_dir)?;
        }

        Ok(config_dir.join("settings.json"))
    }

    pub fn load() -> Self {
        Self::load_strict()
            .or_else(|_| Self::load_without_autosubmit())
            .unwrap_or_default()
    }

    pub fn load_strict() -> Result<Self> {
        let path = Self::config_path()?;
        match fs::read_to_string(path) {
            Ok(content) => Self::parse_strict(&content),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    fn load_without_autosubmit() -> Result<Self> {
        let path = Self::config_path()?;
        match fs::read_to_string(path) {
            Ok(content) => Self::parse_without_autosubmit(&content),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    fn parse_strict(content: &str) -> Result<Self> {
        let settings: Settings = serde_json::from_str(content)?;
        Ok(Self::clamp_values(settings))
    }

    fn parse_without_autosubmit(content: &str) -> Result<Self> {
        let mut value: serde_json::Value = serde_json::from_str(content)?;
        if let Some(object) = value.as_object_mut() {
            object.remove("autosubmit");
        }
        let settings: Settings = serde_json::from_value(value)?;
        Ok(Self::clamp_values(settings))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let content = serde_json::to_string_pretty(self)?;

        // Atomic write: write to temp file, sync, then rename
        // Matches the pattern used in tui/cache.rs and pricing/cache.rs
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let tmp_filename = format!(".settings.{}.{:x}.tmp", std::process::id(), nanos);
        let temp_path = path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(&tmp_filename);

        let write_result = (|| -> Result<()> {
            let mut file = fs::File::create(&temp_path)?;
            use std::io::Write;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            if fs::rename(&temp_path, &path).is_err() {
                // Windows: rename can't overwrite; copy then cleanup so destination is never removed first.
                fs::copy(&temp_path, &path)?;
                let _ = fs::remove_file(&temp_path);
            }
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }

        write_result
    }

    pub fn theme_name(&self) -> ThemeName {
        self.color_palette.parse().unwrap_or(ThemeName::Blue)
    }

    pub fn set_theme(&mut self, theme: ThemeName) {
        self.color_palette = theme.as_str().to_string();
    }

    pub fn get_auto_refresh_interval(&self) -> Option<Duration> {
        if self.auto_refresh_enabled && self.auto_refresh_ms > 0 {
            Some(Duration::from_millis(self.auto_refresh_ms))
        } else {
            None
        }
    }

    pub fn get_native_timeout(&self) -> Duration {
        let timeout_ms = if let Ok(env_val) = std::env::var("TOKSCALE_NATIVE_TIMEOUT_MS") {
            env_val.parse::<u64>().unwrap_or(self.native_timeout_ms)
        } else {
            self.native_timeout_ms
        };

        let clamped = timeout_ms.clamp(MIN_NATIVE_TIMEOUT_MS, MAX_NATIVE_TIMEOUT_MS);
        Duration::from_millis(clamped)
    }

    fn clamp_values(mut settings: Self) -> Self {
        settings.auto_refresh_ms = settings
            .auto_refresh_ms
            .clamp(MIN_AUTO_REFRESH_MS, MAX_AUTO_REFRESH_MS);
        settings.native_timeout_ms = settings
            .native_timeout_ms
            .clamp(MIN_NATIVE_TIMEOUT_MS, MAX_NATIVE_TIMEOUT_MS);
        settings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::ffi::OsString;
    use tempfile::TempDir;

    struct TestEnvGuard {
        _temp: TempDir,
        previous_home: Option<OsString>,
        previous_xdg_config_home: Option<OsString>,
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            match &self.previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.previous_xdg_config_home {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    fn with_temp_config_dir() -> TestEnvGuard {
        let temp = TempDir::new().unwrap();
        let previous_home = std::env::var_os("HOME");
        let previous_xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
        let config_home = temp.path().join("xdg-config");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::create_dir_all(temp.path().join("Library/Application Support")).unwrap();
        std::env::set_var("HOME", temp.path());
        std::env::set_var("XDG_CONFIG_HOME", &config_home);
        TestEnvGuard {
            _temp: temp,
            previous_home,
            previous_xdg_config_home,
        }
    }

    fn write_settings_fixture(base: &std::path::Path, content: &str) {
        for dir in [
            base.join(".config/tokscale"),
            base.join("xdg-config/tokscale"),
            base.join("Library/Application Support/tokscale"),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("settings.json"), content).unwrap();
        }
    }

    #[test]
    #[serial]
    fn load_preserves_non_autosubmit_settings_when_autosubmit_is_invalid() {
        let env = with_temp_config_dir();
        write_settings_fixture(
            env._temp.path(),
            r#"{
  "colorPalette": "purple",
  "autoRefreshEnabled": true,
  "autoRefreshMs": 45000,
  "includeUnusedModels": true,
  "nativeTimeoutMs": 120000,
  "autosubmit": {
    "enabled": true
  }
}"#,
        );

        let loaded = Settings::load();

        assert_eq!(loaded.color_palette, "purple");
        assert!(loaded.auto_refresh_enabled);
        assert_eq!(loaded.auto_refresh_ms, 45_000);
        assert!(loaded.include_unused_models);
        assert_eq!(loaded.native_timeout_ms, 120_000);
        assert!(loaded.autosubmit.is_none());
    }
}
