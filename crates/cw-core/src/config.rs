use serde::{Deserialize, Serialize};

/// Commented TOML listing every setting at its built-in default. Written on first run so the
/// user has a discoverable, editable starting point.
pub const DEFAULT_CONFIG_TOML: &str = r#"# ContextWitness configuration.
# Every value below is the built-in default; delete a line to keep using that default.

[capture]
# Seconds between capture attempts.
interval_secs = 2
# A pixel counts as changed when its grayscale value moves by more than this (0-254).
change_pixel_threshold = 8
# Store and OCR a frame once more than this many screen pixels changed.
# Counted in source-screen pixels, so the same edit behaves the same on every monitor.
# The default is roughly seven to ten characters of text.
change_area_pixels = 1000
# WebP encoder quality (0-100).
webp_quality = 75

[ocr]
# Languages offered to the OCR engine, most important first.
languages = ["ja", "en"]

[storage]
# Where captures and the database live. Empty means the per-user local data directory.
data_dir = ""
# Delete stored images older than this many days.
image_retention_days = 14
# Delete the oldest images once stored images exceed this size, in GiB.
image_retention_max_gib = 50

[privacy]
# Capture stops entirely while one of these executables is in the foreground.
# Matched case-insensitively against the executable file name, e.g. ["KeePass.exe"].
process_blacklist = []

[hindsight]
# Memory bank that receives episodes.
bank_id = "contextwitness"
# Context label sent with every episode.
context_label = "screen capture"

[episode]
# Observations are grouped into episodes of this length.
window_minutes = 5

[activitywatch]
# Reserved for a future release; ActivityWatch is not part of v1.
enabled = false
"#;

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
    /// Capture once more than this many source-screen pixels changed since the stored frame.
    pub change_area_pixels: u32,
    /// WebP encoding quality.
    pub webp_quality: u8,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            interval_secs: 2,
            change_pixel_threshold: 8,
            change_area_pixels: 1000,
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
    #[error("failed to write config file {path}: {source}")]
    Write {
        /// Path that could not be written.
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
    /// A parsed configuration value would break collection.
    #[error("invalid configuration value for {field}: {reason}")]
    Invalid {
        /// TOML path of the invalid field.
        field: &'static str,
        /// Explanation of the accepted values.
        reason: String,
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

    /// Reject values that parse but would break collection. Called by [`Config::load_from_path`].
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.capture.change_pixel_threshold == 255 {
            return Err(ConfigError::Invalid {
                field: "capture.change_pixel_threshold",
                reason: "must be 254 or less; two pixels can never differ by more than 255"
                    .to_owned(),
            });
        }

        if self.capture.interval_secs < 1 {
            return Err(ConfigError::Invalid {
                field: "capture.interval_secs",
                reason: "must be at least 1 second".to_owned(),
            });
        }

        if self.capture.webp_quality > 100 {
            return Err(ConfigError::Invalid {
                field: "capture.webp_quality",
                reason: "must be between 0 and 100".to_owned(),
            });
        }

        if self.episode.window_minutes < 1 {
            return Err(ConfigError::Invalid {
                field: "episode.window_minutes",
                reason: "must be at least 1 minute".to_owned(),
            });
        }

        if self.hindsight.bank_id.trim().is_empty() {
            return Err(ConfigError::Invalid {
                field: "hindsight.bank_id",
                reason: "must not be empty".to_owned(),
            });
        }

        Ok(())
    }

    /// Create `path` (and any missing parent directories) containing [`DEFAULT_CONFIG_TOML`]
    /// when it does not exist yet. Returns `true` when a file was created, `false` when one
    /// was already present. Never overwrites an existing file.
    pub fn write_default_if_missing(path: &std::path::Path) -> Result<bool, ConfigError> {
        if path.exists() {
            return Ok(false);
        }

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }

        let mut temporary = path.as_os_str().to_os_string();
        temporary.push(format!(".tmp-{}", std::process::id()));
        let temporary = std::path::PathBuf::from(temporary);

        let write_result = std::fs::File::create(&temporary).and_then(|mut file| {
            std::io::Write::write_all(&mut file, DEFAULT_CONFIG_TOML.as_bytes())?;
            file.sync_all()
        });
        if let Err(source) = write_result {
            let _ = std::fs::remove_file(&temporary);
            return Err(ConfigError::Write {
                path: path.to_path_buf(),
                source,
            });
        }

        match std::fs::rename(&temporary, path) {
            Ok(()) => Ok(true),
            Err(source) => {
                let _ = std::fs::remove_file(&temporary);
                Err(ConfigError::Write {
                    path: path.to_path_buf(),
                    source,
                })
            }
        }
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

        let config = Self::from_toml_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;

        Ok(config)
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
        assert_eq!(config.capture.change_area_pixels, 1000);
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
    fn default_config_template_parses_to_defaults() {
        let config = Config::from_toml_str(DEFAULT_CONFIG_TOML)
            .expect("the built-in default config template should parse");

        assert_eq!(
            config,
            Config::default(),
            "the built-in config template should stay aligned with the defaults"
        );
    }

    #[test]
    fn write_default_if_missing_creates_file_and_parents() {
        let temp_dir = unique_temp_path("write-default");
        let path = temp_dir.join("nested").join("config.toml");
        assert!(!temp_dir.exists());

        let created = Config::write_default_if_missing(&path)
            .expect("the default config and its parent directories should be creatable");

        assert!(created, "the first call should create the config file");
        assert!(path.exists(), "the default config file should exist");
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("the newly created default config should be readable"),
            DEFAULT_CONFIG_TOML,
            "the created file should contain the exact default config template"
        );

        std::fs::write(&path, "user-owned contents")
            .expect("the test config should be replaceable before the second call");
        let created_again = Config::write_default_if_missing(&path)
            .expect("an existing config should not make default creation fail");

        assert!(
            !created_again,
            "the second call should report that the config already exists"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("the existing test config should be readable"),
            "user-owned contents",
            "an existing config file must never be overwritten"
        );

        std::fs::remove_dir_all(&temp_dir)
            .expect("the default config test directory should be removable");
    }

    #[test]
    fn write_default_if_missing_leaves_no_temporary_file() {
        let temp_dir = unique_temp_path("write-default-no-temporary-file");
        let path = temp_dir.join("config.toml");
        assert!(!temp_dir.exists());

        let created = Config::write_default_if_missing(&path)
            .expect("the default config should be creatable");

        assert!(created, "the first call should create the config file");
        // A leftover temporary file would look like stale config and accumulate on failed first runs.
        let entries: Vec<_> = std::fs::read_dir(&temp_dir)
            .expect("the test config directory should be readable")
            .collect::<Result<_, _>>()
            .expect("the test config directory entries should be readable");
        assert_eq!(entries.len(), 1, "only the config file should remain");
        assert_eq!(
            entries[0].file_name(),
            path.file_name()
                .expect("the test config path should have a file name"),
            "the remaining entry should be the destination config"
        );

        std::fs::remove_dir_all(&temp_dir)
            .expect("the default config test directory should be removable");
    }

    #[test]
    fn validate_accepts_defaults() {
        assert!(
            Config::default().validate().is_ok(),
            "the built-in defaults should be semantically valid"
        );
    }

    #[test]
    fn validate_rejects_saturated_pixel_threshold() {
        let mut config = Config::default();
        config.capture.change_pixel_threshold = 255;

        // The `abs_diff` of two `u8` values can never exceed 255.
        assert!(
            matches!(
                config.validate(),
                Err(ConfigError::Invalid {
                    field: "capture.change_pixel_threshold",
                    ..
                })
            ),
            "a saturated pixel threshold should be rejected"
        );

        config.capture.change_pixel_threshold = 254;
        assert!(
            config.validate().is_ok(),
            "a pixel threshold of 254 should remain valid"
        );
    }

    #[test]
    fn validate_rejects_zero_interval() {
        let mut config = Config::default();
        config.capture.interval_secs = 0;

        assert!(
            matches!(
                config.validate(),
                Err(ConfigError::Invalid {
                    field: "capture.interval_secs",
                    ..
                })
            ),
            "a zero capture interval should be rejected"
        );
    }

    #[test]
    fn validate_rejects_zero_window_minutes() {
        let mut config = Config::default();
        config.episode.window_minutes = 0;

        assert!(
            matches!(
                config.validate(),
                Err(ConfigError::Invalid {
                    field: "episode.window_minutes",
                    ..
                })
            ),
            "a zero episode window should be rejected"
        );
    }

    #[test]
    fn validate_rejects_excessive_webp_quality() {
        let mut config = Config::default();
        config.capture.webp_quality = 101;

        assert!(
            matches!(
                config.validate(),
                Err(ConfigError::Invalid {
                    field: "capture.webp_quality",
                    ..
                })
            ),
            "WebP quality above 100 should be rejected"
        );
    }

    #[test]
    fn validate_rejects_blank_bank_id() {
        let mut config = Config::default();
        config.hindsight.bank_id = "   ".to_owned();

        assert!(
            matches!(
                config.validate(),
                Err(ConfigError::Invalid {
                    field: "hindsight.bank_id",
                    ..
                })
            ),
            "a whitespace-only Hindsight bank ID should be rejected"
        );
    }

    #[test]
    fn load_from_path_rejects_invalid_values() {
        let temp_dir = unique_temp_path("invalid-values");
        std::fs::create_dir(&temp_dir).expect("the unique test directory should be creatable");
        let path = temp_dir.join("config.toml");
        std::fs::write(&path, "[capture]\nchange_pixel_threshold = 255\n")
            .expect("the semantically invalid test config should be writable");

        let result = Config::load_from_path(&path);

        std::fs::remove_file(&path).expect("the invalid test config should be removable");
        std::fs::remove_dir(&temp_dir).expect("the empty test directory should be removable");
        assert!(
            matches!(
                result,
                Err(ConfigError::Invalid {
                    field: "capture.change_pixel_threshold",
                    ..
                })
            ),
            "loading should reject a semantically invalid config"
        );
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
