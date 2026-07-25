use serde::{Deserialize, Serialize};

/// Complete ContextWitness configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct Config {
    /// Screen capture settings.
    pub capture: CaptureConfig,
    /// OCR settings.
    pub ocr: OcrConfig,
    /// Local storage settings.
    pub storage: StorageConfig,
    /// Privacy settings.
    pub privacy: PrivacyConfig,
    /// Hindsight integration settings.
    pub hindsight: HindsightConfig,
    /// Episode grouping settings.
    pub episode: EpisodeConfig,
    /// ActivityWatch integration settings.
    pub activitywatch: ActivityWatchConfig,
}

/// Screen capture settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct CaptureConfig {
    /// Seconds between capture attempts.
    pub interval_secs: u64,
    /// Per-pixel luma delta from 0 through 255.
    pub change_pixel_threshold: u8,
    /// Fraction of changed pixels required to keep a capture.
    pub change_ratio: f64,
    /// WebP encoding quality.
    pub webp_quality: u8,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            interval_secs: 2,
            change_pixel_threshold: 8,
            change_ratio: 0.002,
            webp_quality: 75,
        }
    }
}

/// OCR settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct OcrConfig {
    /// Languages requested from the OCR engine.
    pub languages: Vec<String>,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            languages: vec!["ja".into(), "en".into()],
        }
    }
}

/// Local storage settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct StorageConfig {
    /// Data directory, or an empty string to use the per-user default.
    pub data_dir: String,
    /// Number of days to retain captured images.
    pub image_retention_days: u32,
    /// Maximum captured image storage in GiB.
    pub image_retention_max_gib: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: String::new(),
            image_retention_days: 14,
            image_retention_max_gib: 50,
        }
    }
}

/// Privacy settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct PrivacyConfig {
    /// Process names for which capture is disabled.
    pub process_blacklist: Vec<String>,
}

/// Hindsight integration settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct HindsightConfig {
    /// Hindsight bank identifier.
    pub bank_id: String,
    /// Context label attached to stored observations.
    pub context_label: String,
}

impl Default for HindsightConfig {
    fn default() -> Self {
        Self {
            bank_id: "contextwitness".into(),
            context_label: "screen capture".into(),
        }
    }
}

/// Episode grouping settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct EpisodeConfig {
    /// Duration of an episode window in minutes.
    pub window_minutes: u32,
}

impl Default for EpisodeConfig {
    fn default() -> Self {
        Self { window_minutes: 5 }
    }
}

/// ActivityWatch integration settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct ActivityWatchConfig {
    /// Whether ActivityWatch integration is enabled.
    pub enabled: bool,
}

/// Errors produced while loading or resolving configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Reading an existing configuration file failed.
    #[error("failed to read config file {path}: {source}")]
    Read {
        /// Configuration file path.
        path: std::path::PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// The configuration file did not contain valid TOML for [`Config`].
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        /// Configuration file path.
        path: std::path::PathBuf,
        /// Underlying TOML deserialization error.
        source: toml::de::Error,
    },
    /// A per-user directory could not be resolved.
    #[error("could not resolve {what} directory for this user")]
    UnresolvedDir {
        /// Kind of directory that could not be resolved.
        what: &'static str,
    },
}

impl Config {
    /// Parse from a TOML string. Unknown keys are an error (typos must not be silently ignored).
    pub fn from_toml_str(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }

    /// Read `path`. A missing file is NOT an error: returns `Config::default()`.
    /// Any other IO error -> ConfigError::Read; invalid TOML -> ConfigError::Parse.
    pub fn load_from_path(path: &std::path::Path) -> Result<Config, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        Self::from_toml_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Returns `%APPDATA%\ContextWitness\config.toml` via [`dirs::config_dir`].
pub fn default_config_path() -> Result<std::path::PathBuf, ConfigError> {
    dirs::config_dir()
        .ok_or(ConfigError::UnresolvedDir { what: "config" })
        .map(|path| path.join("ContextWitness").join("config.toml"))
}

impl StorageConfig {
    /// Empty data_dir -> dirs::data_local_dir()/"ContextWitness"; otherwise the configured path as-is.
    pub fn resolve_data_dir(&self) -> Result<std::path::PathBuf, ConfigError> {
        if self.data_dir.is_empty() {
            dirs::data_local_dir()
                .ok_or(ConfigError::UnresolvedDir { what: "data" })
                .map(|path| path.join("ContextWitness"))
        } else {
            Ok(std::path::PathBuf::from(&self.data_dir))
        }
    }
}

/// Layout under the resolved data directory.
pub struct DataPaths {
    /// Resolved root data directory.
    pub root: std::path::PathBuf,
}

impl DataPaths {
    /// Creates paths rooted at `root`.
    pub fn new(root: std::path::PathBuf) -> DataPaths {
        Self { root }
    }

    /// Returns `root/db.sqlite3`.
    pub fn database(&self) -> std::path::PathBuf {
        self.root.join("db.sqlite3")
    }

    /// Returns `root/images`.
    pub fn images(&self) -> std::path::PathBuf {
        self.root.join("images")
    }

    /// Returns `root/logs`.
    pub fn logs(&self) -> std::path::PathBuf {
        self.root.join("logs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_path(test_name: &str) -> std::path::PathBuf {
        let counter = TEMP_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "contextwitness-{test_name}-{}-{counter}",
            std::process::id()
        ))
    }

    #[test]
    fn default_config_matches_design_doc() {
        let config = Config::default();

        assert_eq!(config.capture.interval_secs, 2);
        assert_eq!(config.capture.change_pixel_threshold, 8);
        assert_eq!(config.capture.change_ratio, 0.002);
        assert_eq!(config.capture.webp_quality, 75);
        assert_eq!(config.ocr.languages, vec!["ja".to_owned(), "en".to_owned()]);
        assert_eq!(config.storage.data_dir, "");
        assert_eq!(config.storage.image_retention_days, 14);
        assert_eq!(config.storage.image_retention_max_gib, 50);
        assert!(config.privacy.process_blacklist.is_empty());
        assert_eq!(config.hindsight.bank_id, "contextwitness");
        assert_eq!(config.hindsight.context_label, "screen capture");
        assert_eq!(config.episode.window_minutes, 5);
        assert!(!config.activitywatch.enabled);
    }

    #[test]
    fn partial_toml_fills_remaining_defaults() {
        let config = Config::from_toml_str("[capture]\ninterval_secs = 5\n")
            .expect("partial config should parse");
        let expected = Config {
            capture: CaptureConfig {
                interval_secs: 5,
                ..CaptureConfig::default()
            },
            ..Config::default()
        };

        assert_eq!(config, expected);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::from_toml_str("[capture]\ninterval_secz = 3\n").is_err());
        assert!(Config::from_toml_str("[capturr]\n").is_err());
    }

    #[test]
    fn empty_data_dir_resolves_under_local_appdata() -> Result<(), ConfigError> {
        let resolved = StorageConfig::default().resolve_data_dir()?;
        let expected = dirs::data_local_dir()
            .expect("the test user should have a local data directory")
            .join("ContextWitness");

        assert!(resolved.ends_with("ContextWitness"));
        assert_eq!(resolved, expected);
        Ok(())
    }

    #[test]
    fn explicit_data_dir_is_used_verbatim() {
        let storage = StorageConfig {
            data_dir: "D:/somewhere".to_owned(),
            ..StorageConfig::default()
        };

        assert_eq!(
            storage
                .resolve_data_dir()
                .expect("an explicit data directory should resolve"),
            std::path::PathBuf::from("D:/somewhere")
        );
    }

    #[test]
    fn data_paths_derive_layout() {
        let paths = DataPaths::new("X".into());

        assert_eq!(paths.database(), std::path::PathBuf::from("X/db.sqlite3"));
        assert_eq!(paths.images(), std::path::PathBuf::from("X/images"));
        assert_eq!(paths.logs(), std::path::PathBuf::from("X/logs"));
    }

    #[test]
    fn missing_config_file_yields_defaults() {
        let temp_dir = unique_temp_path("missing-config");
        let path = temp_dir.join("config.toml");
        assert!(!temp_dir.exists());

        let config =
            Config::load_from_path(&path).expect("a missing config file should use defaults");

        assert_eq!(config, Config::default());
    }

    #[test]
    fn invalid_toml_file_yields_parse_error() {
        let temp_dir = unique_temp_path("invalid-config");
        std::fs::create_dir(&temp_dir).expect("the unique test directory should be creatable");
        let path = temp_dir.join("config.toml");
        std::fs::write(&path, "[capture\n").expect("the invalid test config should be writable");

        let result = Config::load_from_path(&path);

        std::fs::remove_file(&path).expect("the invalid test config should be removable");
        std::fs::remove_dir(&temp_dir).expect("the empty test directory should be removable");
        assert!(matches!(result, Err(ConfigError::Parse { .. })));
    }
}
