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

/// Bounds the whole exchange, request-body upload included, so the largest configurable episode
/// over a slow link must fit within it; detecting a dead host promptly is [`CONNECT_TIMEOUT`]'s
/// job, not this one's.
// ponytail: fixed ceiling, promote to a config knob if real episodes ever approach it.
const HTTP_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A control exchange — the status GET, the bank upsert — carries no episode text, so it must
/// not inherit the upload-sized [`HTTP_TIMEOUT`]: a server that stalls mid-exchange would hold
/// the single delivery thread for 600 s, and at the bank upsert that gates every delivery.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// Hindsight connection details, resolved from `~/.hindsight/contextwitness.json` or the
/// environment. The token is optional, as it is for Hindsight's own integrations: a server that
/// requires none is a normal deployment.
#[derive(Clone)]
pub struct Credentials {
    api_url: String,
    token: Option<String>,
}

impl Credentials {
    /// `Ok(None)` means delivery is simply not configured, which is a normal state.
    ///
    /// The environment and the file are each taken whole, never mixed: a URL from one source with
    /// a token from the other would send that token to a server it was never meant for. So when
    /// `CONTEXTWITNESS_HINDSIGHT_URL` is set, the environment is the entire credential set and the
    /// file goes unread; `CONTEXTWITNESS_HINDSIGHT_TOKEN` on its own is an error rather than a
    /// silent fallback to the file's URL.
    pub fn load() -> Result<Option<Self>, CredentialsError> {
        let env_token = from_env("CONTEXTWITNESS_HINDSIGHT_TOKEN");
        if let Some(api_url) = from_env("CONTEXTWITNESS_HINDSIGHT_URL") {
            return Ok(Some(Self {
                api_url,
                token: env_token,
            }));
        }
        if env_token.is_some() {
            return Err(CredentialsError::EnvTokenWithoutUrl);
        }
        let path = dirs::home_dir()
            .ok_or(CredentialsError::NoHome)?
            .join(".hindsight")
            .join("contextwitness.json");
        let file = match std::fs::read_to_string(&path) {
            // Deserialized as a map rather than a `Value`: a root that is not a JSON object
            // holds neither key, and reading it as unconfigured would hide the parse error.
            Ok(text) => serde_json::from_str::<Map<String, Value>>(&text).map_err(|source| {
                CredentialsError::Parse {
                    path: path.clone(),
                    source,
                }
            })?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(CredentialsError::Io { path, source }),
        };
        let from_file = |key: &str| {
            file.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|value| !value.trim().is_empty())
        };
        let token = from_file("hindsightApiToken");
        match from_file("hindsightApiUrl") {
            Some(api_url) => Ok(Some(Self { api_url, token })),
            None if token.is_none() => Ok(None),
            None => Err(CredentialsError::NoUrl { path }),
        }
    }

    /// Write the URL and token that [`load`](Self::load) reads, creating `~/.hindsight` when it is
    /// missing, and answer the file they landed in. `None` removes any recorded token: the file
    /// says what setup last answered, and a token left behind would ride along to the new URL. Any
    /// other key in that file is kept as it was: this owns two of them and cannot know what wrote
    /// the rest.
    ///
    /// The environment is not touched: when `CONTEXTWITNESS_HINDSIGHT_URL` is set,
    /// [`load`](Self::load) takes the environment whole and reads nothing of what this writes.
    pub fn save(api_url: &str, token: Option<&str>) -> Result<PathBuf, CredentialsError> {
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
        Self::apply(&mut document, api_url, token);

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

    /// [`save`](Self::save)'s document mutation, apart from the file it lands in.
    fn apply(document: &mut Map<String, Value>, api_url: &str, token: Option<&str>) {
        // A blank token is the no-token state, not a value to record.
        let token = token.filter(|token| !token.trim().is_empty());
        document.insert("hindsightApiUrl".to_owned(), api_url.into());
        match token {
            Some(token) => {
                document.insert("hindsightApiToken".to_owned(), token.into());
            }
            None => {
                document.remove("hindsightApiToken");
            }
        }
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
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
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
    #[error("{path} holds a token but no URL; run `contextwitness setup` to rewrite it")]
    NoUrl { path: PathBuf },
    #[error(
        "CONTEXTWITNESS_HINDSIGHT_TOKEN is set without CONTEXTWITNESS_HINDSIGHT_URL, which \
         would pair the token with a URL from another source; set both or neither"
    )]
    EnvTokenWithoutUrl,
}

/// One episode, ready to hand to Hindsight. Maps onto the API's `MemoryItem`.
pub struct RetainItem<'a> {
    /// Feeds only the operation id, never the wire: document ids repeat across installs sharing
    /// a bank, so the id that must not collide is salted with this row's ULID instead.
    pub episode_id: &'a str,
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
    #[serde(rename = "async")]
    is_async: bool,
    operation_id: &'a str,
    items: [RetainItemWire<'a>; 1],
}

#[derive(serde::Deserialize)]
struct RetainAccepted {
    operation_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct OperationWire {
    status: String,
}

/// `Processing` and `Failed` ask the caller to try [`retain`](HindsightClient::retain) again
/// later: the fixed operation id lets that later call find the same operation and ask how it
/// stands, without re-submitting the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetainOutcome {
    Delivered,
    Processing {
        operation_id: String,
    },
    /// The extraction ran out of the server's own retries. The episode is parked and the
    /// operation watched rather than condemned: the operations API can `/retry` it, and a
    /// condemned entry would never poll again, making that documented recovery invisible here.
    Failed {
        operation_id: String,
    },
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
        });
    }
    Ok(())
}

/// Blocking Hindsight client.
pub struct HindsightClient {
    http: Client,
    base_url: Url,
    token: Option<String>,
}

impl HindsightClient {
    pub fn new(credentials: Credentials) -> Result<Self, DeliveryError> {
        let base_url =
            parse_api_url(&credentials.api_url).map_err(|error| DeliveryError::Permanent {
                message: format!("the hindsight API URL is unusable: {error}"),
            })?;
        let http = Client::builder()
            .user_agent(concat!("contextwitness/", env!("CARGO_PKG_VERSION")))
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|error| DeliveryError::Permanent {
                message: format!("failed to build the HTTP client: {error}"),
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
                })?;
            path.pop_if_empty().extend(segments);
        }
        Ok(url)
    }

    /// Upserts the bank with the fixed retain settings. The server creates a missing bank and
    /// merges the settings into an existing one's config.
    pub fn ensure_bank(&self, bank_id: &str) -> Result<(), DeliveryError> {
        vet_bank_id(bank_id)?;
        let url = self.endpoint(&["v1", "default", "banks", bank_id])?;
        let desired = json!({
            "retain_mission": RETAIN_MISSION,
            "retain_extraction_mode": RETAIN_EXTRACTION_MODE,
            "retain_chunk_size": RETAIN_CHUNK_SIZE,
        });
        accept(
            self.send(self.http.put(url).json(&desired).timeout(CONTROL_TIMEOUT))?,
            UPSERT_BANK_OPERATION,
        )
        .map(drop)
    }

    /// One delivery step for an episode, cheapest question first: ask how its fixed operation
    /// stands, and submit the body only when the server has never seen it. The submission carries
    /// the episode's whole text, and re-POSTing it as a status check would upload, parse and
    /// validate that text on every poll. `Delivered` only when the server reports it completed.
    pub fn retain(
        &self,
        bank_id: &str,
        item: &RetainItem<'_>,
    ) -> Result<RetainOutcome, DeliveryError> {
        vet_bank_id(bank_id)?;
        let operation_id = retain_operation_id(bank_id, item.episode_id);
        let status = self.operation_status(bank_id, &operation_id)?;
        if status != OPERATION_ABSENT {
            return operation_outcome(&status, &operation_id);
        }
        let url = self.endpoint(&["v1", "default", "banks", bank_id, "memories"])?;
        let request = RetainRequest {
            is_async: true,
            operation_id: &operation_id,
            items: [RetainItemWire {
                content: item.content,
                document_id: item.document_id,
                timestamp: item.timestamp.to_rfc3339_opts(SecondsFormat::Secs, true),
                context: item.context,
                metadata: item.metadata,
                update_mode: "replace",
            }],
        };
        let accepted: RetainAccepted = parse(
            accept(
                self.send(self.http.post(url).json(&request))?,
                RETAIN_OPERATION,
            )
            .map_err(exchange_fault)?,
            RETAIN_OPERATION,
        )?;
        vet_operation_echo(&operation_id, accepted.operation_id.as_deref())?;
        // Answered directly: an asynchronous extraction cannot be complete in the same instant,
        // so a status GET here buys one round trip and a spurious not-yet-visible failure path.
        Ok(RetainOutcome::Processing { operation_id })
    }

    fn operation_status(&self, bank_id: &str, operation_id: &str) -> Result<String, DeliveryError> {
        let url = self.endpoint(&[
            "v1",
            "default",
            "banks",
            bank_id,
            "operations",
            operation_id,
        ])?;
        let operation: OperationWire = parse(
            accept(
                self.send(self.http.get(url).timeout(CONTROL_TIMEOUT))?,
                POLL_OPERATION,
            )
            .map_err(exchange_fault)?,
            POLL_OPERATION,
        )?;
        Ok(operation.status)
    }

    /// The Authorization header travels only when there is a token, as in Hindsight's own
    /// integrations.
    fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    fn send(&self, request: RequestBuilder) -> Result<Response, DeliveryError> {
        self.authorize(request).send().map_err(transport)
    }
}

impl fmt::Debug for HindsightClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HindsightClient")
            .field("base_url", &self.base_url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
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
    /// The server itself cannot run this client — a fault of the deployment, not a verdict on
    /// any one episode.
    #[error("{message}")]
    Unsupported { message: String },
}

impl DeliveryError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }

    /// The server's own `Retry-After`, when it sent one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Retryable { retry_after, .. } => *retry_after,
            Self::Permanent { .. } | Self::Unsupported { .. } => None,
        }
    }
}

fn transport(error: reqwest::Error) -> DeliveryError {
    DeliveryError::Retryable {
        message: format!("hindsight request failed: {error}"),
        retry_after: None,
    }
}

const RETAIN_OPERATION: &str = "retain";
const UPSERT_BANK_OPERATION: &str = "upsert bank";
const POLL_OPERATION: &str = "operation status";

fn accept(response: Response, operation: &str) -> Result<Response, DeliveryError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    // The body stays out: Hindsight echoes rejected input back, and that is captured screen text.
    let message = format!(
        "hindsight {operation} failed with HTTP {status}{}",
        status_hint(operation, status)
    );
    if retryable_status(status) {
        Err(DeliveryError::Retryable {
            message,
            retry_after: retry_after(&response),
        })
    } else {
        Err(DeliveryError::Permanent { message })
    }
}

fn parse<T: serde::de::DeserializeOwned>(
    response: Response,
    operation: &str,
) -> Result<T, DeliveryError> {
    // The decode error stays out of the message: serde spells it with fragments of the body.
    response.json().map_err(|_| DeliveryError::Retryable {
        message: format!("hindsight {operation} answered an unreadable body"),
        retry_after: None,
    })
}

/// A refused exchange is never a verdict on the episode: the asynchronous POST only submits and
/// the poll only asks — the verdict is the operation status alone.
fn exchange_fault(error: DeliveryError) -> DeliveryError {
    match error {
        DeliveryError::Permanent { message } => DeliveryError::Retryable {
            message,
            retry_after: None,
        },
        other => other,
    }
}

fn vet_operation_echo(sent: &str, answered: Option<&str>) -> Result<(), DeliveryError> {
    // Case-insensitive: UUID hex is compared caseless per RFC 9562, and a server re-rendering
    // the id through a UUID type must not read as one that ignored it.
    if answered.is_some_and(|answered| answered.eq_ignore_ascii_case(sent)) {
        return Ok(());
    }
    Err(DeliveryError::Unsupported {
        message: "hindsight retain failed: the server ignored the supplied operation id; \
                  hindsight v0.8.6 or later is required"
            .to_owned(),
    })
}

/// The status a server answers for an operation it has never seen — the one status
/// [`HindsightClient::retain`] consumes itself, as the cue to submit.
const OPERATION_ABSENT: &str = "not_found";

/// Maps the server's operation status; the incoming value never travels into a message, and
/// [`OPERATION_ABSENT`] never arrives — [`HindsightClient::retain`] consumes it as the submit
/// cue. The operation id rides every non-completed outcome — the operator's only handle on the
/// server's operations API, and derived from nothing but the bank and episode ids.
fn operation_outcome(status: &str, operation_id: &str) -> Result<RetainOutcome, DeliveryError> {
    match status {
        "completed" => Ok(RetainOutcome::Delivered),
        "pending" | "processing" => Ok(RetainOutcome::Processing {
            operation_id: operation_id.to_owned(),
        }),
        "failed" => Ok(RetainOutcome::Failed {
            operation_id: operation_id.to_owned(),
        }),
        "cancelled" => Err(DeliveryError::Permanent {
            message: format!(
                "hindsight retain failed: operation {operation_id} was cancelled on the server"
            ),
        }),
        _ => Err(DeliveryError::Retryable {
            message: format!(
                "hindsight retain failed: operation {operation_id} answered an unrecognized status"
            ),
            retry_after: None,
        }),
    }
}

/// The fixed operation id of an episode's retain: resubmitting under the same id returns the
/// original operation instead of new work, which is what makes a retry safe.
fn retain_operation_id(bank_id: &str, episode_id: &str) -> String {
    let mut bits = fnv1a_128(&format!("{bank_id}\n{episode_id}"));
    // UUID version-8/variant bits: the server validates the id as a UUID.
    bits = (bits & !(0xf << 76)) | (0x8 << 76);
    bits = (bits & !(0x3 << 62)) | (0x2 << 62);
    let hex = format!("{bits:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// FNV-1a, 128 bits: wide enough that two episodes sharing an id is not a real event.
fn fnv1a_128(text: &str) -> u128 {
    let mut hash: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    for byte in text.as_bytes() {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    hash
}

/// Deployment faults, repairable while episodes wait; everything unlisted classifies
/// Permanent. On the retain path [`exchange_fault`] overrides that verdict and the bank upsert
/// backs off rather than condemns, so the split steers retry shape — never an episode's
/// survival.
fn retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT
                | StatusCode::TOO_MANY_REQUESTS
                | StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::NOT_FOUND
                | StatusCode::BAD_REQUEST
                | StatusCode::PAYLOAD_TOO_LARGE
        )
}

fn status_hint(operation: &str, status: StatusCode) -> &'static str {
    match (operation, status) {
        (UPSERT_BANK_OPERATION, StatusCode::NOT_FOUND) => {
            " (a missing bank cannot cause this, so the base URL is likely wrong)"
        }
        (UPSERT_BANK_OPERATION, StatusCode::UNPROCESSABLE_ENTITY) => {
            " (the server rejected this client's fixed bank settings as invalid)"
        }
        (RETAIN_OPERATION, StatusCode::NOT_FOUND) => {
            " (a missing bank cannot cause this; the memories route itself was refused)"
        }
        (RETAIN_OPERATION, StatusCode::PAYLOAD_TOO_LARGE) => {
            " (the body exceeds a proxy or server body-size limit; raising the limit or \
             shrinking the episode is needed)"
        }
        (RETAIN_OPERATION, StatusCode::UNPROCESSABLE_ENTITY) => {
            " (the server refused the content: validation or memory defense)"
        }
        _ => "",
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
            token: Some("the-secret-token".to_owned()),
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
    fn an_absent_or_blank_token_leaves_no_recorded_one_behind() {
        let mut document: Map<String, Value> = [
            ("hindsightApiToken".to_owned(), "stale".into()),
            ("retainEveryNTurns".to_owned(), 1.into()),
        ]
        .into_iter()
        .collect();
        Credentials::apply(&mut document, "http://host:1/api", Some("  "));
        assert_eq!(document.get("hindsightApiToken"), None);
        Credentials::apply(&mut document, "http://host:2/api", Some("t"));
        assert_eq!(document.get("hindsightApiToken"), Some(&Value::from("t")));
        Credentials::apply(&mut document, "http://host:3/api", None);
        assert_eq!(document.get("hindsightApiToken"), None);
        assert_eq!(
            document.get("hindsightApiUrl"),
            Some(&Value::from("http://host:3/api"))
        );
        assert_eq!(document.get("retainEveryNTurns"), Some(&Value::from(1)));
    }

    #[test]
    fn the_authorization_header_travels_only_with_a_token() {
        let header = |token: Option<String>| {
            let client = HindsightClient::new(Credentials {
                api_url: "http://localhost:0".to_owned(),
                token,
            })
            .expect("building the client makes no request");
            client
                .authorize(client.http.get("http://localhost:0/x"))
                .build()
                .expect("a plain GET builds")
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .cloned()
        };
        assert_eq!(header(None), None);
        let sent = header(Some("the-secret-token".to_owned())).expect("the token should travel");
        assert_eq!(
            sent.to_str().expect("the header is ASCII"),
            "Bearer the-secret-token"
        );
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
            token: None,
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

    #[test]
    fn deployment_faults_retry_and_everything_else_classifies_permanent() {
        for (status, retryable) in [
            (StatusCode::BAD_REQUEST, true),
            (StatusCode::UNAUTHORIZED, true),
            (StatusCode::FORBIDDEN, true),
            (StatusCode::NOT_FOUND, true),
            (StatusCode::REQUEST_TIMEOUT, true),
            (StatusCode::PAYLOAD_TOO_LARGE, true),
            (StatusCode::TOO_MANY_REQUESTS, true),
            (StatusCode::INTERNAL_SERVER_ERROR, true),
            (StatusCode::SERVICE_UNAVAILABLE, true),
            (StatusCode::UNPROCESSABLE_ENTITY, false),
            (StatusCode::CONFLICT, false),
            (StatusCode::GONE, false),
        ] {
            assert_eq!(retryable_status(status), retryable, "{status}");
        }
    }

    #[test]
    fn a_server_that_ignores_the_operation_id_is_refused() {
        assert!(vet_operation_echo("the-id", Some("the-id")).is_ok());
        assert!(
            vet_operation_echo("the-id", Some("THE-ID")).is_ok(),
            "UUID hex compares caseless, and a re-rendered echo is not an ignored id"
        );
        for answered in [None, Some("another-id")] {
            let error = vet_operation_echo("the-id", answered).unwrap_err();
            assert!(
                matches!(error, DeliveryError::Unsupported { .. }),
                "{error}"
            );
            assert!(error.to_string().contains("0.8.6"), "{error}");
        }
    }

    #[test]
    fn the_operation_id_is_a_stable_uuid_per_episode() {
        let id = retain_operation_id("bank", "ep-1");
        assert_eq!(
            id, "aaf7ec6e-1706-8fb0-9644-442e1b38dd3c",
            "a changed derivation re-buys extraction for every in-flight episode"
        );
        assert_ne!(id, retain_operation_id("bank", "ep-2"));
        assert_ne!(id, retain_operation_id("bank2", "ep-1"));
        assert_ne!(
            retain_operation_id("ab", "c"),
            retain_operation_id("a", "bc")
        );

        assert_eq!(id.as_bytes()[14], b'8', "{id}");
        assert!(
            matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "{id}"
        );
    }

    #[test]
    fn the_operation_status_decides_the_episode() {
        let outcome = |status| operation_outcome(status, "the-operation-id");
        assert_eq!(outcome("completed").unwrap(), RetainOutcome::Delivered);
        for waiting in ["pending", "processing"] {
            assert_eq!(
                outcome(waiting).unwrap(),
                RetainOutcome::Processing {
                    operation_id: "the-operation-id".to_owned()
                },
                "{waiting}"
            );
        }
        assert_eq!(
            outcome("failed").unwrap(),
            RetainOutcome::Failed {
                operation_id: "the-operation-id".to_owned()
            },
            "a failed operation must stay watchable: the server's /retry has no local effect \
             on an entry that never polls again"
        );
        let cancelled = outcome("cancelled").unwrap_err();
        assert!(!cancelled.is_retryable());
        assert!(
            cancelled.to_string().contains("the-operation-id"),
            "the operator's server-side handle must be named: {cancelled}"
        );
        for unhandled in [OPERATION_ABSENT, "someday-maybe"] {
            let unknown = outcome(unhandled).unwrap_err();
            assert!(
                unknown.is_retryable(),
                "an unmapped status must stay retryable, never a verdict: {unhandled}"
            );
            assert!(
                !unknown.to_string().contains("someday-maybe"),
                "{unhandled}"
            );
            assert!(
                unknown.to_string().contains("the-operation-id"),
                "{unhandled}"
            );
        }
    }

    #[test]
    fn a_refused_exchange_never_condemns_the_episode() {
        let refused = exchange_fault(DeliveryError::Permanent {
            message: "m".to_owned(),
        });
        assert!(refused.is_retryable(), "{refused}");
        let kept = exchange_fault(DeliveryError::Retryable {
            message: "m".to_owned(),
            retry_after: Some(Duration::from_secs(7)),
        });
        assert_eq!(
            kept.retry_after(),
            Some(Duration::from_secs(7)),
            "the server's own Retry-After must survive the downgrade path untouched"
        );
    }

    #[test]
    fn the_retain_wire_spellings_survive_serialization() {
        let metadata = RawValue::from_string(r#"{"b":"1","a":"2"}"#.to_owned())
            .expect("the object is valid JSON");
        let request = RetainRequest {
            is_async: true,
            operation_id: "the-id",
            items: [RetainItemWire {
                content: "c",
                document_id: "d",
                timestamp: "t".to_owned(),
                context: "x",
                metadata: &metadata,
                update_mode: "replace",
            }],
        };
        let body = serde_json::to_string(&request).expect("the request serializes");
        assert!(
            body.contains(r#""metadata":{"b":"1","a":"2"}"#),
            "the stored metadata bytes must reach the wire verbatim, not re-emitted alphabetized: {body}"
        );
        let value: Value = serde_json::from_str(&body).expect("the body is valid JSON");
        assert_eq!(value["async"], Value::Bool(true));
        assert_eq!(value["operation_id"], Value::from("the-id"));
        assert_eq!(value["items"][0]["update_mode"], Value::from("replace"));
    }

    /// One scripted exchange: the method and path asked, and the body that came with it.
    fn read_request(stream: &mut std::net::TcpStream) -> (String, String) {
        use std::io::Read;
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut buffer).expect("the request reads");
            assert!(read > 0, "the client hung up mid-request");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let head = String::from_utf8(bytes[..header_end].to_vec()).expect("the head is UTF-8");
        let content_length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>().expect("a numeric length"))
            })
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).expect("the body reads");
            assert!(read > 0, "the client hung up mid-body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        let request_line = head.lines().next().expect("a request line");
        let asked = request_line
            .rsplit_once(' ')
            .expect("an HTTP version")
            .0
            .to_owned();
        let body = String::from_utf8(bytes[header_end..].to_vec()).expect("the body is UTF-8");
        (asked, body)
    }

    #[test]
    fn the_delivery_lifecycle_submits_the_body_once_and_then_only_asks() {
        let operation_id = retain_operation_id("bank", "ep-1");
        let scripted = [
            r#"{"status":"not_found"}"#.to_owned(),
            format!(r#"{{"operation_id":"{operation_id}"}}"#),
            r#"{"status":"processing"}"#.to_owned(),
            r#"{"status":"failed"}"#.to_owned(),
            r#"{"status":"completed"}"#.to_owned(),
        ];
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback binds");
        let port = listener
            .local_addr()
            .expect("the socket has an address")
            .port();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut requests = Vec::new();
            for body in scripted {
                let (mut stream, _) = listener.accept().expect("the client connects");
                // Bounded, so a client that stops sending fails the test fast instead of
                // holding the accept loop for the client's own 600 s timeout.
                let bound = Some(std::time::Duration::from_secs(10));
                stream.set_read_timeout(bound).expect("the timeout applies");
                stream
                    .set_write_timeout(bound)
                    .expect("the timeout applies");
                requests.push(read_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                std::io::Write::write_all(&mut stream, response.as_bytes())
                    .expect("the response writes");
            }
            let _ = sender.send(requests);
        });

        let client = HindsightClient::new(Credentials {
            api_url: format!("http://127.0.0.1:{port}"),
            token: None,
        })
        .expect("building the client makes no request");
        let metadata = RawValue::from_string(r#"{"k":"v"}"#.to_owned()).expect("valid JSON");
        let item = RetainItem {
            episode_id: "ep-1",
            document_id: "doc",
            content: "the episode text",
            timestamp: Utc::now(),
            context: "ctx",
            metadata: &metadata,
        };
        let processing = RetainOutcome::Processing {
            operation_id: operation_id.clone(),
        };
        assert_eq!(client.retain("bank", &item).unwrap(), processing);
        assert_eq!(client.retain("bank", &item).unwrap(), processing);
        assert_eq!(
            client.retain("bank", &item).unwrap(),
            RetainOutcome::Failed {
                operation_id: operation_id.clone()
            }
        );
        assert_eq!(
            client.retain("bank", &item).unwrap(),
            RetainOutcome::Delivered,
            "a server-side /retry must be picked up by the plain next attempt"
        );

        let requests = receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the server saw every scripted exchange");
        let ask = format!("GET /v1/default/banks/bank/operations/{operation_id}");
        assert_eq!(
            requests
                .iter()
                .map(|(asked, _)| asked.as_str())
                .collect::<Vec<_>>(),
            [
                ask.as_str(),
                "POST /v1/default/banks/bank/memories",
                ask.as_str(),
                ask.as_str(),
                ask.as_str(),
            ],
            "the body must travel exactly once, and only after the server answers not_found"
        );
        let submitted = &requests[1].1;
        assert!(
            submitted.contains(&format!(r#""operation_id":"{operation_id}""#)),
            "{submitted}"
        );
        assert!(submitted.contains("the episode text"), "{submitted}");
    }
}
