#![deny(unsafe_op_in_unsafe_fn)]
//! Hindsight sink functionality for ContextWitness.

use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::{StatusCode, Url};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

/// Steers what Hindsight extracts from the episodes this daemon sends.
const RETAIN_MISSION: &str = "Record the user's on-screen activity as a time-anchored activity log. Focus on what the user was actually doing: the applications in use, the documents and pages being viewed or edited, and the substance of on-screen text. Ignore boilerplate UI text such as menu labels, button captions, and window chrome. Prefer concrete, time-anchored statements over generalizations.";
const RETAIN_EXTRACTION_MODE: &str = "concise";
const RETAIN_CHUNK_SIZE: u32 = 3000;

// Retain is synchronous, and reqwest's own 30s default is far under Hindsight's LLM extraction.
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
        let env_url = from_env("CONTEXTWITNESS_HINDSIGHT_URL");
        let env_token = from_env("CONTEXTWITNESS_HINDSIGHT_TOKEN");
        let file = if env_url.is_some() && env_token.is_some() {
            None
        } else {
            match std::fs::read_to_string(&path) {
                // Deserialized as a map rather than a `Value`: a root that is not a JSON object
                // holds neither key, and reading it as unconfigured would hide the parse error.
                Ok(text) => Some(serde_json::from_str::<Map<String, Value>>(&text).map_err(
                    |source| CredentialsError::Parse {
                        path: path.clone(),
                        source,
                    },
                )?),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => return Err(CredentialsError::Io { path, source }),
            }
        };
        let from_file = |key: &str| {
            file.as_ref()
                .and_then(|map| map.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|value| !value.trim().is_empty())
        };
        let api_url = env_url.or_else(|| from_file("hindsightApiUrl"));
        let token = env_token.or_else(|| from_file("hindsightApiToken"));
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

        let text = format!("{}\n", Value::Object(document));

        // Renamed onto the file, never written into it: `fs::write` truncates first, and an
        // interruption leaves invalid JSON that neither `setup` nor `load` can recover. The
        // rename replaces the destination, which cw-core's no-clobber publish will not do.
        let (temporary, mut file) =
            cw_core::atomic_file::create_temporary_beside(&path).map_err(|source| {
                CredentialsError::Io {
                    path: path.clone(),
                    source,
                }
            })?;
        let written = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        if let Err(source) = written.and_then(|()| std::fs::rename(&temporary, &path)) {
            let _ = std::fs::remove_file(&temporary);
            return Err(CredentialsError::Io { path, source });
        }

        Ok(path)
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }
}

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

/// The retain body, borrowed rather than built as a `Value`: every expression inside `json!` goes
/// through `serde_json::to_value`, which parses the metadata `RawValue` into a map and re-emits it
/// — alphabetized, since `Value` maps are `BTreeMap` here — so the snapshot would not arrive as the
/// bytes it was stored as. Serializing through `serde_json`'s writer, which is what `.json` uses,
/// writes a `RawValue` out verbatim.
#[derive(serde::Serialize)]
struct RetainRequest<'a> {
    /// Explicit: the caller must be able to read 2xx as processed, not as queued.
    #[serde(rename = "async")]
    is_async: bool,
    items: [RetainItemWire<'a>; 1],
}

#[derive(serde::Serialize)]
struct RetainItemWire<'a> {
    content: &'a str,
    document_id: &'a str,
    timestamp: String,
    context: &'a str,
    metadata: &'a RawValue,
    /// Explicit: a retry of the same document_id must replace, never append.
    update_mode: &'static str,
}

/// A base URL [`parse_api_url`] refused.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidApiUrl(String);

/// Parse a Hindsight API base URL: absolute, http or https, with a host, no query, no fragment,
/// and — once surrounding whitespace is trimmed — no tab, carriage return or newline. The URL
/// grammar strips those silently, so
/// the parsed URL would name a different host or path than the one spelled. Repeated trailing
/// slashes collapse here: the endpoint builder strips one, and any beyond it would ride into
/// every request URL as an empty path segment.
pub fn parse_api_url(text: &str) -> Result<Url, InvalidApiUrl> {
    let text = text.trim();
    if text.contains(['\t', '\r', '\n']) {
        return Err(InvalidApiUrl(
            "the URL carries a tab, carriage return or newline, which the URL grammar drops \
             rather than encodes"
                .to_owned(),
        ));
    }
    let mut url =
        Url::parse(text).map_err(|error| InvalidApiUrl(format!("not an absolute URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(InvalidApiUrl(format!(
            "the scheme is {}, not http or https",
            url.scheme()
        )));
    }
    if url.host_str().is_none() {
        return Err(InvalidApiUrl("the URL names no host".to_owned()));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(InvalidApiUrl(
            "a query or fragment does not belong in a base URL".to_owned(),
        ));
    }
    if url.path().ends_with("//") {
        let trimmed = url.path().trim_end_matches('/').to_owned();
        url.set_path(&trimmed);
    }
    Ok(url)
}

/// Refuse what the URL grammar drops rather than encodes: appended as a path segment, `.` and
/// `..` vanish into the path, and tab, carriage return and newline are stripped from the segment
/// text — before the dot spellings are matched — so an id carrying one travels as a different
/// name, or as a dot segment the literal match cannot see.
fn vet_bank_id(bank_id: &str) -> Result<(), DeliveryError> {
    if matches!(bank_id, "." | "..") || bank_id.contains(['\t', '\r', '\n']) {
        return Err(DeliveryError::Permanent {
            message: format!("bank id {bank_id:?} cannot travel as a path segment"),
            not_found: false,
        });
    }
    Ok(())
}

/// Blocking Hindsight 0.8.4 client.
pub struct HindsightClient {
    http: Client,
    base_url: Url,
    token: String,
}

impl HindsightClient {
    pub fn new(credentials: Credentials) -> Result<Self, DeliveryError> {
        let base_url =
            parse_api_url(&credentials.api_url).map_err(|error| DeliveryError::Permanent {
                message: format!("the hindsight API URL is unusable: {error}"),
                not_found: false,
            })?;
        let http = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|error| DeliveryError::Permanent {
                message: format!("failed to build the HTTP client: {error}"),
                not_found: false,
            })?;
        Ok(Self {
            http,
            base_url,
            token: credentials.token,
        })
    }

    /// The endpoint under the base URL, each entry of `segments` traveling as exactly one path
    /// segment — true of what the grammar percent-encodes, not of the spellings it drops, which
    /// `vet_bank_id` refuses before the one caller-supplied segment reaches here.
    fn endpoint(&self, segments: &[&str]) -> Result<Url, DeliveryError> {
        let mut url = self.base_url.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|()| DeliveryError::Permanent {
                    message: "the hindsight API URL cannot carry a path".to_owned(),
                    not_found: false,
                })?;
            path.pop_if_empty().extend(segments);
        }
        Ok(url)
    }

    /// Creates the bank when it is missing and converges its retain settings on the fixed values.
    pub fn ensure_bank(&self, bank_id: &str) -> Result<(), DeliveryError> {
        vet_bank_id(bank_id)?;
        let config_url = self.endpoint(&["v1", "default", "banks", bank_id, "config"])?;
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

        let response = self.send(self.http.get(config_url.clone()))?;
        if response.status() == StatusCode::NOT_FOUND {
            let bank_url = self.endpoint(&["v1", "default", "banks", bank_id])?;
            // Empty body: every field is optional and the settings land in the PATCH below, so
            // losing a create race cannot reset an existing bank.
            check(
                self.send(self.http.put(bank_url).json(&json!({})))?,
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
                    .patch(config_url)
                    .json(&json!({ "updates": desired })),
            )?,
            "update bank config",
        )?;
        Ok(())
    }

    /// Delivers one episode. `Ok` means Hindsight answered 2xx, which is delivery.
    pub fn retain(&self, bank_id: &str, item: &RetainItem<'_>) -> Result<(), DeliveryError> {
        vet_bank_id(bank_id)?;
        let url = self.endpoint(&["v1", "default", "banks", bank_id, "memories"])?;
        let request = RetainRequest {
            is_async: false,
            items: [RetainItemWire {
                content: item.content,
                document_id: item.document_id,
                timestamp: item.timestamp.to_rfc3339_opts(SecondsFormat::Secs, true),
                context: item.context,
                metadata: item.metadata,
                update_mode: "replace",
            }],
        };
        check(self.send(self.http.post(url).json(&request))?, "retain")?;
        Ok(())
    }

    fn send(&self, request: RequestBuilder) -> Result<Response, DeliveryError> {
        request.bearer_auth(&self.token).send().map_err(transport)
    }
}

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
    Permanent { message: String, not_found: bool },
}

impl DeliveryError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }

    /// Whether the answer was 404, which this API gives when the bank is not there — deleted, or
    /// never created at the URL the credentials now point at. That is configuration state and not a
    /// verdict on the episode, so the caller runs `ensure_bank` again instead of condemning it.
    /// Every other permanent failure is about the request that was sent.
    pub fn is_bank_missing(&self) -> bool {
        matches!(
            self,
            Self::Permanent {
                not_found: true,
                ..
            }
        )
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
    // The body stays out: Hindsight echoes rejected input back, and that is captured screen text.
    let message = format!("hindsight {operation} failed with HTTP {status}");
    // 401 and 403 say the credential is wrong, not the payload, so they are retryable against the
    // usual 4xx-is-permanent rule: a token expiring mid-run would otherwise leave every episode
    // attempted during the outage permanently unsendable.
    if status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::UNAUTHORIZED
        || status == StatusCode::FORBIDDEN
    {
        Err(DeliveryError::Retryable {
            message,
            retry_after: retry_after(&response),
        })
    } else {
        Err(DeliveryError::Permanent {
            message,
            not_found: status == StatusCode::NOT_FOUND,
        })
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
    let deadline = DateTime::parse_from_rfc2822(&value).ok()?;
    (deadline.with_timezone(&Utc) - Utc::now()).to_std().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_redacts_the_token() {
        let credentials = Credentials {
            api_url: "http://localhost:0".to_owned(),
            token: "the-secret-token".to_owned(),
        };
        let printed = format!("{credentials:?}");
        assert!(!printed.contains("the-secret-token"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");

        let client =
            HindsightClient::new(credentials).expect("building the client makes no request");
        let printed = format!("{client:?}");
        assert!(!printed.contains("the-secret-token"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
    }

    #[test]
    fn the_base_url_must_be_absolute_bare_http() {
        for bad in [
            "localhost:8000",
            "ftp://host/x",
            "http://host/api?x=1",
            "http://host/api#frag",
            "/v1",
            "not a url",
            "http://ho\tst:8000/api",
            "http://host:8000/a\rpi",
            "http://host:8000/api\n/v2",
        ] {
            assert!(parse_api_url(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse_api_url(" http://host:8000/api/ ")
                .expect("the padded URL should parse")
                .as_str(),
            "http://host:8000/api/"
        );
        for doubled in ["http://host:8000/api//", "http://host:8000/api///"] {
            assert_eq!(
                parse_api_url(doubled)
                    .expect("the over-slashed URL should parse")
                    .as_str(),
                "http://host:8000/api",
                "{doubled}"
            );
        }
        assert_eq!(
            parse_api_url("http://host/pre%20fix//")
                .expect("the encoded path should parse")
                .as_str(),
            "http://host/pre%20fix"
        );
    }

    #[test]
    fn a_bank_id_travels_as_one_path_segment() {
        let client = HindsightClient::new(Credentials {
            api_url: "http://localhost:0/prefix/".to_owned(),
            token: "t".to_owned(),
        })
        .expect("building the client makes no request");
        let url = client
            .endpoint(&["v1", "default", "banks", "we?ird/ba#nk%25", "memories"])
            .expect("the endpoint should build");
        assert_eq!(
            url.as_str(),
            "http://localhost:0/prefix/v1/default/banks/we%3Fird%2Fba%23nk%2525/memories"
        );

        assert!(vet_bank_id(".").is_err());
        assert!(vet_bank_id("..").is_err());
        assert!(vet_bank_id(".\t.").is_err());
        assert!(vet_bank_id("..\n").is_err());
        assert!(vet_bank_id("my\tbank").is_err());
        assert!(vet_bank_id("an-ordinary-bank").is_ok());
    }
}
