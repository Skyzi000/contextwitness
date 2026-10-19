use std::fmt;
use std::path::PathBuf;

pub const URL_VARIABLE: &str = "CONTEXTWITNESS_TRANSCRIPTION_URL";
pub const KEY_VARIABLE: &str = "CONTEXTWITNESS_TRANSCRIPTION_KEY";

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub api_url: String,
    pub api_key: Option<String>,
}

#[derive(serde::Deserialize)]
struct Document {
    transcription: Option<Section>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Section {
    api_url: Option<String>,
    api_key: Option<String>,
}

impl Credentials {
    /// The environment and the file are each taken whole, never mixed: a URL from one source with
    /// a key from the other would send that key to a server it was never meant for.
    pub fn load() -> Result<Option<Self>, CredentialsError> {
        if let Some(credentials) =
            Self::from_environment(from_env(URL_VARIABLE), from_env(KEY_VARIABLE))?
        {
            return Ok(Some(credentials));
        }
        let path = dirs::config_dir()
            .ok_or(CredentialsError::NoConfigDir)?
            .join("ContextWitness")
            .join("credentials.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_document(&text, path),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(CredentialsError::Io { path, source }),
        }
    }

    fn from_environment(
        api_url: Option<String>,
        api_key: Option<String>,
    ) -> Result<Option<Self>, CredentialsError> {
        match (api_url, api_key) {
            (Some(api_url), api_key) => Ok(Some(Self { api_url, api_key })),
            (None, Some(_)) => Err(CredentialsError::EnvKeyWithoutUrl),
            (None, None) => Ok(None),
        }
    }

    fn from_document(text: &str, path: PathBuf) -> Result<Option<Self>, CredentialsError> {
        let document: Document = match toml::from_str(text) {
            Ok(document) => document,
            Err(error) => {
                let line = error
                    .span()
                    .and_then(|span| text.get(..span.start))
                    .map(|before| before.matches('\n').count() + 1);
                return Err(CredentialsError::Parse { path, line });
            }
        };
        let Some(section) = document.transcription else {
            return Ok(None);
        };
        let filled = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
        let api_key = filled(section.api_key);
        match filled(section.api_url) {
            Some(api_url) => Ok(Some(Self { api_url, api_key })),
            None if api_key.is_none() => Ok(None),
            None => Err(CredentialsError::NoUrl { path }),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("api_url", &"<redacted>")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

fn from_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Never carries the key.
#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error("failed to locate the user configuration directory")]
    NoConfigDir,
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The parser's own message quotes the offending line, which may be the one holding the key.
    #[error(
        "{path} could not be parsed{}; [transcription] takes api_url and api_key, both strings",
        at_line(.line)
    )]
    Parse { path: PathBuf, line: Option<usize> },
    #[error("{path} holds a transcription api_key but no api_url")]
    NoUrl { path: PathBuf },
    #[error(
        "{} is set without {}, which would pair the key with a URL from another source; set both \
         or neither",
        KEY_VARIABLE,
        URL_VARIABLE
    )]
    EnvKeyWithoutUrl,
}

fn at_line(line: &Option<usize>) -> String {
    line.map(|line| format!(" (line {line})"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Result<Option<Credentials>, CredentialsError> {
        Credentials::from_document(text, PathBuf::from("credentials.toml"))
    }

    fn credentials(api_url: &str, api_key: Option<&str>) -> Credentials {
        Credentials {
            api_url: api_url.to_owned(),
            api_key: api_key.map(str::to_owned),
        }
    }

    #[test]
    fn the_file_gives_the_url_with_or_without_a_key() {
        let both = "[transcription]\napi_url = \"http://host:1/v1\"\napi_key = \"k\"\n";
        assert_eq!(
            document(both).unwrap(),
            Some(credentials("http://host:1/v1", Some("k")))
        );
        let url_only = "[transcription]\napi_url = \"http://host:1/v1\"\n";
        assert_eq!(
            document(url_only).unwrap(),
            Some(credentials("http://host:1/v1", None))
        );
        let blank_key = "[transcription]\napi_url = \"http://host:1/v1\"\napi_key = \"  \"\n";
        assert_eq!(
            document(blank_key).unwrap(),
            Some(credentials("http://host:1/v1", None))
        );
    }

    #[test]
    fn a_file_without_the_section_or_with_an_empty_one_is_unconfigured() {
        assert_eq!(document("").unwrap(), None);
        assert_eq!(document("[other]\nx = 1\n").unwrap(), None);
        assert_eq!(document("[transcription]\n").unwrap(), None);
        assert_eq!(document("[transcription]\napi_url = \"\"\n").unwrap(), None);
    }

    #[test]
    fn a_key_in_the_file_without_a_url_is_an_error() {
        assert!(matches!(
            document("[transcription]\napi_key = \"k\"\n"),
            Err(CredentialsError::NoUrl { .. })
        ));
    }

    #[test]
    fn a_file_that_does_not_parse_is_an_error_that_names_the_line_and_leaves_the_key_out() {
        for broken in [
            "[transcription]\napi_url = \"http://host:1/v1\"\napi_key = \"the-secret\n",
            "[transcription]\napi_url = \"http://host:1/v1\"\napi_key = 5ecret\n",
            "[transcription]\napi_url = \"http://host:1/v1\"\napi_key = 1234567\n",
        ] {
            let error = document(broken).unwrap_err();
            assert!(
                matches!(error, CredentialsError::Parse { line: Some(3), .. }),
                "{error:?}"
            );
            let shown = format!("{error} {error:?}");
            assert!(
                !shown.contains("ecret") && !shown.contains("1234567"),
                "{shown}"
            );
        }
    }

    #[test]
    fn the_environment_is_taken_whole_or_not_at_all() {
        assert_eq!(
            Credentials::from_environment(Some("http://host:2/v1".to_owned()), None).unwrap(),
            Some(credentials("http://host:2/v1", None))
        );
        assert_eq!(
            Credentials::from_environment(
                Some("http://host:2/v1".to_owned()),
                Some("k".to_owned())
            )
            .unwrap(),
            Some(credentials("http://host:2/v1", Some("k")))
        );
        assert!(matches!(
            Credentials::from_environment(None, Some("k".to_owned())),
            Err(CredentialsError::EnvKeyWithoutUrl)
        ));
        assert_eq!(Credentials::from_environment(None, None).unwrap(), None);
    }

    #[test]
    fn debug_output_leaves_the_key_and_the_url_out() {
        let shown = format!(
            "{:?}",
            credentials("http://user:the-password@host:1/v1", Some("the-secret"))
        );
        assert!(
            !shown.contains("the-secret") && !shown.contains("the-password"),
            "{shown}"
        );
    }

    #[test]
    fn a_misspelt_key_in_the_section_is_an_error_at_its_line() {
        let misspelt =
            "[transcription]\napi_url = \"http://host:1/v1\"\napi-key = \"the-secret\"\n";
        let error = document(misspelt).unwrap_err();
        assert!(
            matches!(error, CredentialsError::Parse { line: Some(3), .. }),
            "{error:?}"
        );
        assert!(!error.to_string().contains("the-secret"), "{error}");
    }

    #[test]
    fn an_error_at_the_start_of_a_line_is_reported_at_that_line() {
        let error = document("[transcription]\n\n= 1\n").unwrap_err();
        assert!(
            matches!(error, CredentialsError::Parse { line: Some(3), .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_error_about_a_lone_key_variable_names_both_variables() {
        let shown = CredentialsError::EnvKeyWithoutUrl.to_string();
        assert!(
            shown.contains(KEY_VARIABLE) && shown.contains(URL_VARIABLE),
            "{shown}"
        );
    }
}
