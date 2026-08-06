#![deny(unsafe_op_in_unsafe_fn)]
//! Hindsight sink functionality for ContextWitness.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

/// Steers what Hindsight extracts from the episodes this daemon sends.
const RETAIN_MISSION: &str = "Record the user's on-screen activity as a time-anchored activity log. Focus on what the user was actually doing: the applications in use, the documents and pages being viewed or edited, and the substance of on-screen text. Ignore boilerplate UI text such as menu labels, button captions, and window chrome. Prefer concrete, time-anchored statements over generalizations.";
const RETAIN_EXTRACTION_MODE: &str = "concise";
const RETAIN_CHUNK_SIZE: u32 = 3000;

// Retain is synchronous, so this waits out Hindsight's LLM extraction of a whole episode; reqwest's
// own 30s default would turn every large episode into a spurious retryable failure.
// ponytail: fixed ceiling, promote to a config knob if real episodes ever approach it.
const HTTP_TIMEOUT: Duration = Duration::from_secs(600);

/// Hindsight connection details, resolved from `~/.hindsight/contextwitness.json` or the
/// environment.
#[derive(Clone)]
pub struct Credentials {
    api_url: String,
    token: String,
}

impl Credentials {
    /// `Ok(None)` means delivery is simply not configured, which is a normal state.
    pub fn load() -> Result<Option<Self>, CredentialsError> {
        let path = dirs::home_dir()
            .ok_or(CredentialsError::NoHome)?
            .join(".hindsight")
            .join("contextwitness.json");
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => Some(serde_json::from_str::<Value>(&text).map_err(|source| {
                CredentialsError::Parse {
                    path: path.clone(),
                    source,
                }
            })?),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(CredentialsError::Io { path, source }),
        };
        let from_file = |key: &str| {
            file.as_ref()
                .and_then(|value| value.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|value| !value.trim().is_empty())
        };
        let api_url =
            from_env("CONTEXTWITNESS_HINDSIGHT_URL").or_else(|| from_file("hindsightApiUrl"));
        let token =
            from_env("CONTEXTWITNESS_HINDSIGHT_TOKEN").or_else(|| from_file("hindsightApiToken"));
        match (api_url, token) {
            (Some(api_url), Some(token)) => Ok(Some(Self { api_url, token })),
            (None, None) => Ok(None),
            (api_url, _) => Err(CredentialsError::Incomplete {
                missing: if api_url.is_none() { "URL" } else { "token" },
                path,
            }),
        }
    }

    /// Write the URL and token that [`load`](Self::load) reads, creating `~/.hindsight` when it is
    /// missing, and answer the file they landed in. Any other key in that file is kept as it was:
    /// this owns two of them and cannot know what wrote the rest.
    ///
    /// The environment is not touched, so `CONTEXTWITNESS_HINDSIGHT_URL` and
    /// `CONTEXTWITNESS_HINDSIGHT_TOKEN` still outrank whatever this writes.
    pub fn save(api_url: &str, token: &str) -> Result<PathBuf, CredentialsError> {
        let directory = dirs::home_dir()
            .ok_or(CredentialsError::NoHome)?
            .join(".hindsight");
        std::fs::create_dir_all(&directory).map_err(|source| CredentialsError::Io {
            path: directory.clone(),
            source,
        })?;
        let path = directory.join("contextwitness.json");
        // Deserialized as a map rather than a `Value`, so a file holding anything but a JSON object
        // is refused as the parse error it is instead of needing an error of its own.
        let mut document: Map<String, Value> = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| CredentialsError::Parse {
                path: path.clone(),
                source,
            })?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(source) => return Err(CredentialsError::Io { path, source }),
        };
        document.insert("hindsightApiUrl".to_owned(), api_url.into());
        document.insert("hindsightApiToken".to_owned(), token.into());

        // `Value`'s own `Display`, because serializing a map of strings has no failure to report.
        std::fs::write(&path, format!("{}\n", Value::Object(document))).map_err(|source| {
            CredentialsError::Io {
                path: path.clone(),
                source,
            }
        })?;

        Ok(path)
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }
}

// Hand-written so the token cannot reach a log through a `{:?}` anywhere upstream.
impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("api_url", &self.api_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

fn from_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Everything credential loading can fail with. Never carries the token.
#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error("failed to locate the user profile directory")]
    NoHome,
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("hindsight {missing} is missing; run `contextwitness setup` to write {path}")]
    Incomplete {
        missing: &'static str,
        path: PathBuf,
    },
}

/// One episode, ready to hand to Hindsight. Maps onto the API's `MemoryItem`.
pub struct RetainItem<'a> {
    /// Stable id; re-sending the same one replaces the previous document server-side.
    pub document_id: &'a str,
    pub content: &'a str,
    /// End of the episode window.
    pub timestamp: DateTime<Utc>,
    pub context: &'a str,
    /// The retain metadata snapshot, still as the JSON text it was stored as, spliced into the
    /// request verbatim. Parsing and re-serialising it here would rebuild the snapshot, and the
    /// stored bytes are the wire bytes. Hindsight rejects non-string values, and what the store
    /// holds already satisfies that.
    pub metadata: &'a RawValue,
}

/// Blocking Hindsight 0.8.4 client.
pub struct HindsightClient {
    http: Client,
    base_url: String,
    token: String,
}

impl HindsightClient {
    pub fn new(credentials: Credentials) -> Result<Self, DeliveryError> {
        let http = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|error| DeliveryError::Permanent {
                message: format!("failed to build the HTTP client: {error}"),
            })?;
        Ok(Self {
            http,
            base_url: credentials.api_url.trim_end_matches('/').to_owned(),
            token: credentials.token,
        })
    }

    /// Creates the bank when it is missing and converges its retain settings on the fixed values.
    pub fn ensure_bank(&self, bank_id: &str) -> Result<(), DeliveryError> {
        let config_url = format!("{}/v1/default/banks/{bank_id}/config", self.base_url);
        let desired: Map<String, Value> = [
            ("retain_mission".to_owned(), RETAIN_MISSION.into()),
            (
                "retain_extraction_mode".to_owned(),
                RETAIN_EXTRACTION_MODE.into(),
            ),
            ("retain_chunk_size".to_owned(), RETAIN_CHUNK_SIZE.into()),
        ]
        .into_iter()
        .collect();

        let response = self.send(self.http.get(&config_url))?;
        if response.status() == StatusCode::NOT_FOUND {
            let bank_url = format!("{}/v1/default/banks/{bank_id}", self.base_url);
            // Empty body: every field is optional and the settings land in the PATCH below, so
            // losing a create race cannot reset an existing bank.
            check(
                self.send(self.http.put(&bank_url).json(&json!({})))?,
                "create bank",
            )?;
        } else {
            let current: Value = check(response, "read bank config")?
                .json()
                .map_err(transport)?;
            let current = current.get("config");
            if desired
                .iter()
                .all(|(key, value)| current.and_then(|config| config.get(key)) == Some(value))
            {
                return Ok(());
            }
        }
        check(
            self.send(
                self.http
                    .patch(&config_url)
                    .json(&json!({ "updates": desired })),
            )?,
            "update bank config",
        )?;
        Ok(())
    }

    /// Delivers one episode. `Ok` means Hindsight answered 2xx, which is delivery.
    pub fn retain(&self, bank_id: &str, item: &RetainItem<'_>) -> Result<(), DeliveryError> {
        let url = format!("{}/v1/default/banks/{bank_id}/memories", self.base_url);
        let body = json!({
            // Explicit: the caller must be able to read 2xx as processed, not as queued.
            "async": false,
            "items": [{
                "content": item.content,
                "document_id": item.document_id,
                "timestamp": item.timestamp.to_rfc3339_opts(SecondsFormat::Secs, true),
                "context": item.context,
                "metadata": item.metadata,
                // Explicit: a retry of the same document_id must replace, never append.
                "update_mode": "replace",
            }],
        });
        check(self.send(self.http.post(&url).json(&body))?, "retain")?;
        Ok(())
    }

    fn send(&self, request: RequestBuilder) -> Result<Response, DeliveryError> {
        request.bearer_auth(&self.token).send().map_err(transport)
    }
}

// Hand-written so the token cannot reach a log through a `{:?}` anywhere upstream.
impl fmt::Debug for HindsightClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HindsightClient")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A failed exchange with Hindsight, classified so the outbox knows whether to try again.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("{message}")]
    Retryable {
        message: String,
        retry_after: Option<Duration>,
    },
    #[error("{message}")]
    Permanent { message: String },
}

impl DeliveryError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }

    /// The server's own `Retry-After`, when it sent one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Retryable { retry_after, .. } => *retry_after,
            Self::Permanent { .. } => None,
        }
    }
}

fn transport(error: reqwest::Error) -> DeliveryError {
    DeliveryError::Retryable {
        message: format!("hindsight request failed: {error}"),
        retry_after: None,
    }
}

fn check(response: Response, operation: &str) -> Result<Response, DeliveryError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    // The body stays out of the message: Hindsight echoes rejected input back, and here that input
    // is captured screen text.
    let message = format!("hindsight {operation} failed with HTTP {status}");
    if status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
    {
        Err(DeliveryError::Retryable {
            message,
            retry_after: retry_after(&response),
        })
    } else {
        Err(DeliveryError::Permanent { message })
    }
}

fn retry_after(response: &Response) -> Option<Duration> {
    let value = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .to_owned();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    // The other legal spelling is an HTTP-date; one already past yields no delay.
    let deadline = DateTime::parse_from_rfc2822(&value).ok()?;
    (deadline.with_timezone(&Utc) - Utc::now()).to_std().ok()
}
