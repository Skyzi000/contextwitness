use crate::atomic_file::{create_temporary_beside, delete_by_handle, rename_without_replacing};
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
# Store and OCR a frame once more than this many logical pixels changed.
# Logical means at 100% display scaling, so a higher display scale does not inflate the
# measurement by itself. The default is roughly ten characters of text.
change_area_logical_pixels = 600
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
# Observations are grouped into episodes of this length (1-1440).
window_minutes = 5
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
}

/// Screen capture settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct CaptureConfig {
    /// Seconds between capture attempts. At least 1.
    pub interval_secs: u64,
    /// Per-pixel luma delta, 0 through 254. A pixel counts as changed when it moves by strictly
    /// more than this, so 255 would mean nothing ever changed and [`Config::validate`] refuses it.
    pub change_pixel_threshold: u8,
    /// Store and OCR a frame once more than this many logical pixels changed since the stored
    /// frame.
    pub change_area_logical_pixels: u32,
    /// WebP encoding quality, 0 through 100.
    pub webp_quality: u8,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            interval_secs: 2,
            change_pixel_threshold: 8,
            change_area_logical_pixels: 600,
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
    /// Context label sent with every episode.
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
    /// Duration of an episode window in minutes. From 1 to 1440 — one day.
    pub window_minutes: u32,
}

impl Default for EpisodeConfig {
    fn default() -> Self {
        Self { window_minutes: 5 }
    }
}

/// Errors produced while reading, writing or resolving configuration.
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
    /// Writing the default configuration file failed.
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
        reason: &'static str,
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
                reason: "must be 254 or less; two pixels can never differ by more than 255",
            });
        }

        if self.capture.interval_secs < 1 {
            return Err(ConfigError::Invalid {
                field: "capture.interval_secs",
                reason: "must be at least 1 second",
            });
        }

        if self.capture.webp_quality > 100 {
            return Err(ConfigError::Invalid {
                field: "capture.webp_quality",
                reason: "must be between 0 and 100",
            });
        }

        // Capped at one day: entries render clock times without dates, and a day is the longest
        // window in which one clock time cannot stand for two moments.
        if !(1..=1440).contains(&self.episode.window_minutes) {
            return Err(ConfigError::Invalid {
                field: "episode.window_minutes",
                reason: "must be between 1 and 1440 minutes (one day)",
            });
        }

        if self.hindsight.bank_id.trim().is_empty() {
            return Err(ConfigError::Invalid {
                field: "hindsight.bank_id",
                reason: "must not be empty",
            });
        }

        Ok(())
    }

    /// Create `path` (and any missing parent directories) containing [`DEFAULT_CONFIG_TOML`]
    /// when it does not exist yet. Returns `true` when this call created the file, `false` when
    /// the name was already taken — by the config another process published first (publishing is
    /// what tests for it), or by whatever else stands at the name; [`Config::load_from_path`] is
    /// what tells those apart. Never overwrites an existing file.
    ///
    /// The content is written to a temporary file and renamed onto `path` only if that name is
    /// still free, so the config never exists in a half-written or empty state that another
    /// process could mistake for a finished one.
    pub fn write_default_if_missing(path: &std::path::Path) -> Result<bool, ConfigError> {
        // A fast path, not the check. Every startup after the first lands here, and the answer is
        // already on disk — without this the common case creates a temporary, writes the template,
        // flushes it to disk and deletes it again to learn what one `exists()` already said.
        // A config that appears after this test is still refused by `rename_without_replacing`
        // below, so nothing is overwritten either way. Removing this line would not be free,
        // though: when the config is there and its directory refuses new files, this is what
        // answers `Ok(false)` rather than reporting a write error for a file that needs nothing.
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

        let (_temporary, mut file) =
            create_temporary_beside(path).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        let write_result = std::io::Write::write_all(&mut file, DEFAULT_CONFIG_TOML.as_bytes())
            .and_then(|()| file.sync_all());
        if let Err(source) = write_result {
            // Left behind, this looks like a stale config to anyone reading the directory, and it
            // accumulates on every failed first run. Addressed to the handle, so what is discarded
            // is the file this call wrote and not whatever the name has come to mean.
            let _ = delete_by_handle(&file);
            drop(file);
            return Err(ConfigError::Write {
                path: path.to_path_buf(),
                source,
            });
        }

        match rename_without_replacing(&file, path) {
            Ok(true) => Ok(true),
            Ok(false) => {
                let _ = delete_by_handle(&file);
                drop(file);
                Ok(false)
            }
            Err(error) => {
                let _ = delete_by_handle(&file);
                drop(file);
                Err(ConfigError::Write {
                    path: path.to_path_buf(),
                    source: error,
                })
            }
        }
    }

    /// Read `path`. A missing file is NOT an error: returns `Config::default()`. An occupied
    /// name that still reads as missing — a link whose target is gone — IS one, because it
    /// blocks [`Config::write_default_if_missing`] and would keep the defaults in force while
    /// looking configured.
    /// Any other IO error -> ConfigError::Read; invalid TOML -> ConfigError::Parse.
    pub fn load_from_path(path: &std::path::Path) -> Result<Config, ConfigError> {
        // Two tries, because publishing is concurrent by design: `write_default_if_missing`
        // renames a finished config onto this name from any process, so metadata naming
        // something real where the read just found nothing means the file was published between
        // the two calls — the second read takes it. Measured: nothing static answers that way
        // (a directory or junction at the name refuses the read as PermissionDenied, not
        // NotFound), so the retry only ever chases a publication.
        for _ in 0..2 {
            let text = match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    // A name that answers NotFound can still be occupied: a link whose target
                    // is gone reads that way, and treating it as absence would run on defaults
                    // forever — writing the default template is refused by that very name, so
                    // nothing would ever surface it.
                    match path.symlink_metadata() {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            return Err(ConfigError::Read {
                                path: path.to_path_buf(),
                                source: std::io::Error::new(
                                    std::io::ErrorKind::NotFound,
                                    "the name is occupied by a link whose target is missing",
                                ),
                            });
                        }
                        Ok(_) => continue,
                        Err(meta) if meta.kind() == std::io::ErrorKind::NotFound => {
                            return Ok(Self::default());
                        }
                        // Any other answer is not absence but the ordinary read error the doc
                        // promises; reading it as absence would hide a denied name behind the
                        // defaults.
                        Err(meta) => {
                            return Err(ConfigError::Read {
                                path: path.to_path_buf(),
                                source: meta,
                            });
                        }
                    }
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

            return Ok(config);
        }

        // Reached only by two flips in a row: a name that keeps changing between a file and
        // absence mid-read is refused rather than chased further.
        Err(ConfigError::Read {
            path: path.to_path_buf(),
            source: std::io::Error::other("the name kept flipping between a file and absence"),
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
        assert_eq!(config.capture.change_area_logical_pixels, 600);
        assert_eq!(config.capture.webp_quality, 75);
        assert_eq!(config.ocr.languages, vec!["ja".to_owned(), "en".to_owned()]);
        assert_eq!(config.storage.data_dir, "");
        assert_eq!(config.storage.image_retention_days, 14);
        assert_eq!(config.storage.image_retention_max_gib, 50);
        assert!(config.privacy.process_blacklist.is_empty());
        assert_eq!(config.hindsight.bank_id, "contextwitness");
        assert_eq!(config.hindsight.context_label, "screen capture");
        assert_eq!(config.episode.window_minutes, 5);
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
        // Nothing may appear beside the config: no scratch left over from its creation, and
        // nothing from the second call, which is answered without writing at all.
        let entries: Vec<_> = std::fs::read_dir(
            path.parent()
                .expect("the test config path should have a parent directory"),
        )
        .expect("the test config directory should be readable")
        .collect::<Result<_, _>>()
        .expect("the test config directory entries should be readable");
        assert_eq!(
            entries.len(),
            1,
            "the existing config should remain the only directory entry"
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
    fn validate_accepts_the_smallest_and_largest_values_each_bound_allows() {
        // Each refusal above says only that one value is out; it cannot tell the intended bound
        // from one narrowed by a step, and a user is entitled to every value asserted here. The
        // top of the pixel threshold has its own test above and is not repeated.
        let mut config = Config::default();
        config.capture.interval_secs = 1;
        config.capture.change_pixel_threshold = 0;
        config.capture.webp_quality = 0;
        config.episode.window_minutes = 1;
        config.hindsight.bank_id = "x".to_owned();

        config
            .validate()
            .expect("the smallest value every bound allows should be accepted");

        config.capture.webp_quality = 100;
        config.episode.window_minutes = 1440;

        config
            .validate()
            .expect("the highest value each bound allows should be accepted");
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
    fn validate_rejects_a_window_longer_than_a_day() {
        let mut config = Config::default();
        config.episode.window_minutes = 1441;

        assert!(
            matches!(
                config.validate(),
                Err(ConfigError::Invalid {
                    field: "episode.window_minutes",
                    ..
                })
            ),
            "a window longer than a day should be rejected"
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
    fn a_dangling_config_link_is_an_error_not_absence() {
        let temp_dir = unique_temp_path("dangling-config-link");
        std::fs::create_dir(&temp_dir).expect("the unique test directory should be creatable");
        let path = temp_dir.join("config.toml");
        // Creating symlinks needs a privilege ordinary dev machines may not grant.
        let Ok(()) = std::os::windows::fs::symlink_file(temp_dir.join("missing.toml"), &path)
        else {
            std::fs::remove_dir_all(&temp_dir).expect("the test directory should be removable");
            return;
        };

        let result = Config::load_from_path(&path);

        std::fs::remove_dir_all(&temp_dir).expect("the test directory should be removable");
        assert!(
            matches!(result, Err(ConfigError::Read { .. })),
            "a name occupied by a dead link must not read as the defaults, got {result:?}"
        );
    }

    #[test]
    fn an_empty_config_file_loads_as_defaults() {
        let path = unique_temp_path("empty-config");
        std::fs::write(&path, "").expect("the empty test config should be writable");

        // A config the user has emptied reads the same as no config at all, which is what
        // `load_from_path` already does for a missing file.
        let config =
            Config::load_from_path(&path).expect("an empty config file should use defaults");

        std::fs::remove_file(&path).expect("the empty test config should be removable");
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
