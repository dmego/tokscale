use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokscale_core::scanner::ScannerSettings;

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
    #[serde(skip)]
    invalid_autosubmit: Option<serde_json::Value>,
    #[serde(skip)]
    invalid_autosubmit_error: Option<String>,
    /// Persistent scanner configuration. Allows users to pin additional
    /// OpenCode SQLite paths (and, in future, other scanner overrides)
    /// without having to set env vars on every invocation.
    ///
    /// `#[serde(default)]` makes this a drop-in addition — settings.json
    /// files written before the field existed still load cleanly, and an
    /// empty `"scanner": {}` is equivalent to not setting it at all.
    #[serde(default)]
    pub scanner: ScannerSettings,
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
            invalid_autosubmit: None,
            invalid_autosubmit_error: None,
            scanner: ScannerSettings::default(),
        }
    }
}

/// Thin helper that loads settings and returns just the scanner portion.
///
/// Every CLI entry point that builds `LocalParseOptions`/`ReportOptions`
/// calls this so user-configured scanner paths are honored on every
/// invocation. Errors during load fall through to
/// [`ScannerSettings::default`] — a missing or malformed settings.json
/// should never break `tokscale` runs.
pub fn load_scanner_settings() -> ScannerSettings {
    Settings::load().scanner
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
        Self::load_lenient().unwrap_or_default()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn load_strict() -> Result<Self> {
        let path = Self::config_path()?;
        match fs::read_to_string(path) {
            Ok(content) => Self::parse_strict(&content),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn load_lenient() -> Result<Self> {
        let path = Self::config_path()?;
        match fs::read_to_string(path) {
            Ok(content) => Self::parse_lenient(&content),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn parse_strict(content: &str) -> Result<Self> {
        let settings: Settings = serde_json::from_str(content)?;
        Ok(Self::clamp_values(settings))
    }

    fn parse_lenient(content: &str) -> Result<Self> {
        let mut value: serde_json::Value = serde_json::from_str(content)?;
        let autosubmit = value
            .as_object_mut()
            .and_then(|object| object.remove("autosubmit"));
        let settings: Settings = serde_json::from_value(value)?;
        let mut settings = Self::clamp_values(settings);

        match autosubmit {
            Some(raw) if raw.is_null() => settings.autosubmit = None,
            Some(raw) => match serde_json::from_value::<AutosubmitConfig>(raw.clone()) {
                Ok(config) => settings.autosubmit = Some(config),
                Err(err) => {
                    settings.invalid_autosubmit = Some(raw);
                    settings.invalid_autosubmit_error = Some(err.to_string());
                }
            },
            None => settings.autosubmit = None,
        }

        Ok(settings)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let mut value = serde_json::to_value(self)?;
        if self.autosubmit.is_none() {
            if let Some(raw) = &self.invalid_autosubmit {
                if let Some(object) = value.as_object_mut() {
                    object.insert("autosubmit".to_string(), raw.clone());
                }
            }
        }
        let content = serde_json::to_string_pretty(&value)?;

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

    pub fn has_invalid_autosubmit(&self) -> bool {
        self.invalid_autosubmit.is_some()
    }

    pub fn invalid_autosubmit_error(&self) -> Option<&str> {
        self.invalid_autosubmit_error.as_deref()
    }

    pub fn clear_invalid_autosubmit(&mut self) {
        self.invalid_autosubmit = None;
        self.invalid_autosubmit_error = None;
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
mod scanner_tests {
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

    #[test]
    #[serial]
    fn save_preserves_invalid_autosubmit_payload_after_lenient_load() {
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

        let mut loaded = Settings::load();
        loaded.color_palette = "green".to_string();
        loaded.save().unwrap();

        let saved = std::fs::read_to_string(Settings::config_path().unwrap()).unwrap();
        let saved_json: serde_json::Value = serde_json::from_str(&saved).unwrap();

        assert_eq!(saved_json["colorPalette"], "green");
        assert_eq!(saved_json["autosubmit"]["enabled"], true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn settings_load_backfills_scanner_when_missing_from_json() {
        // Older settings.json files predate the `scanner` key. They must
        // still deserialize cleanly and fall through to ScannerSettings::default.
        let json = r#"{
            "colorPalette": "blue",
            "autoRefreshEnabled": false,
            "autoRefreshMs": 60000,
            "includeUnusedModels": false,
            "nativeTimeoutMs": 300000
        }"#;
        let parsed: Settings = serde_json::from_str(json).unwrap();
        assert!(parsed.scanner.opencode_db_paths.is_empty());
    }

    #[test]
    fn settings_load_reads_scanner_opencode_db_paths() {
        let json = r#"{
            "colorPalette": "blue",
            "autoRefreshEnabled": false,
            "autoRefreshMs": 60000,
            "includeUnusedModels": false,
            "nativeTimeoutMs": 300000,
            "scanner": {
                "opencodeDbPaths": [
                    "/custom/one.db",
                    "/custom/opencode-stable.db"
                ]
            }
        }"#;
        let parsed: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed.scanner.opencode_db_paths,
            vec![
                PathBuf::from("/custom/one.db"),
                PathBuf::from("/custom/opencode-stable.db"),
            ]
        );
    }

    #[test]
    fn settings_load_reads_scanner_extra_scan_paths() {
        let json = r#"{
            "colorPalette": "blue",
            "autoRefreshEnabled": false,
            "autoRefreshMs": 60000,
            "includeUnusedModels": false,
            "nativeTimeoutMs": 300000,
            "scanner": {
                "extraScanPaths": {
                    "codex": ["/tmp/project-a/.codex/sessions"],
                    "openclaw": ["/tmp/imports/openclaw/agents"]
                }
            }
        }"#;
        let parsed: Settings = serde_json::from_str(json).unwrap();
        let serialized = serde_json::to_value(&parsed).unwrap();

        assert_eq!(
            serialized["scanner"]["extraScanPaths"]["codex"][0],
            serde_json::json!("/tmp/project-a/.codex/sessions")
        );
        assert_eq!(
            serialized["scanner"]["extraScanPaths"]["openclaw"][0],
            serde_json::json!("/tmp/imports/openclaw/agents")
        );
    }

    #[test]
    fn settings_accepts_empty_scanner_object() {
        // `"scanner": {}` is the documented "no-op" form; must be valid.
        let json = r#"{
            "colorPalette": "blue",
            "autoRefreshEnabled": false,
            "autoRefreshMs": 60000,
            "includeUnusedModels": false,
            "nativeTimeoutMs": 300000,
            "scanner": {}
        }"#;
        let parsed: Settings = serde_json::from_str(json).unwrap();
        assert!(parsed.scanner.opencode_db_paths.is_empty());
    }

    #[test]
    fn settings_round_trips_scanner_section_through_json() {
        // Saving and loading must preserve scanner paths verbatim so that
        // the TUI settings save flow never drops the key silently.
        let mut settings = Settings::default();
        settings.scanner.opencode_db_paths = vec![PathBuf::from("/a/b/opencode.db")];
        let serialized = serde_json::to_string(&settings).unwrap();
        let parsed: Settings = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            parsed.scanner.opencode_db_paths,
            vec![PathBuf::from("/a/b/opencode.db")]
        );
    }

    #[test]
    fn settings_round_trips_scanner_extra_scan_paths_through_json() {
        let json = r#"{
            "colorPalette": "blue",
            "autoRefreshEnabled": false,
            "autoRefreshMs": 60000,
            "includeUnusedModels": false,
            "nativeTimeoutMs": 300000,
            "scanner": {
                "extraScanPaths": {
                    "gemini": ["/tmp/imports/gemini/tmp"]
                }
            }
        }"#;

        let parsed: Settings = serde_json::from_str(json).unwrap();
        let serialized = serde_json::to_string(&parsed).unwrap();
        let round_trip: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert_eq!(
            round_trip["scanner"]["extraScanPaths"]["gemini"][0],
            serde_json::json!("/tmp/imports/gemini/tmp")
        );
    }
}
