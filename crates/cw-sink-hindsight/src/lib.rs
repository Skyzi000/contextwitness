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

/// The `observations_mission` a new bank starts with: tells Hindsight's consolidation what the
/// episode text is (OCR of screen captures) and what the display alone cannot establish.
const OBSERVATIONS_MISSION: &str = "Sources are OCR text from captures of the foreground window, grouped into time windows. Entries carry a timestamp and, where known, the application and window title of the captured window. Consecutive identical captures are folded into one entry. OCR may misrecognize displayed text, and chunks may omit entry headers. The display alone does not establish that the user read, wrote, ran, or agreed with the text, even when the text may be the user's own. Keep the time, application, and window title exactly as they appear in the input, and do not invent them where they are missing. The same text observed again is not independent confirmation. Consolidate related screen records into observations about which applications, documents, pages, and topics were on screen and when; do not create a new observation for a repetition that adds nothing.";

/// Bounds the whole exchange, request-body upload included, so the largest configurable episode
/// over a slow link must fit within it; detecting a dead host promptly is [`CONNECT_TIMEOUT`]'s
/// job, not this one's.
// ponytail: fixed ceiling, promote to a config knob if real episodes ever approach it.
const HTTP_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A control exchange — the status GET, the bank list and creation — carries no episode text,
/// so it must not inherit the upload-sized [`HTTP_TIMEOUT`]: a server that stalls mid-exchange
/// would hold the single delivery thread for 600 s, and at bank initialization that gates
/// every delivery.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// How much of a server reply travels into an error message: the server's stated reason is the
/// diagnosis and must reach the operator, bounded so a refusal that
/// quotes the submission back cannot flood a log line.
const BODY_EXCERPT_BYTES: usize = 2048;

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
        if let Some(credentials) = Self::from_environment(
            from_env("CONTEXTWITNESS_HINDSIGHT_URL"),
            from_env("CONTEXTWITNESS_HINDSIGHT_TOKEN"),
        )? {
            return Ok(Some(credentials));
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
        Self::from_document(&file, path)
    }

    /// [`load`](Self::load)'s environment decision, apart from the process environment.
    fn from_environment(
        api_url: Option<String>,
        token: Option<String>,
    ) -> Result<Option<Self>, CredentialsError> {
        match (api_url, token) {
            (Some(api_url), token) => Ok(Some(Self { api_url, token })),
            (None, Some(_)) => Err(CredentialsError::EnvTokenWithoutUrl),
            (None, None) => Ok(None),
        }
    }

    /// [`load`](Self::load)'s document interpretation, apart from the file it came from.
    fn from_document(
        document: &Map<String, Value>,
        path: PathBuf,
    ) -> Result<Option<Self>, CredentialsError> {
        let field = |key: &str| {
            document
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|value| !value.trim().is_empty())
        };
        let token = field("hindsightApiToken");
        match field("hindsightApiUrl") {
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
            .field("api_url", &display_api_url(&self.api_url))
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
    /// Salts the operation id and the wire document id: document ids repeat across installs
    /// sharing a bank, so every id that must not collide across them carries this row's ULID.
    pub episode_id: &'a str,
    /// Stable per window within this install; [`retain`](HindsightClient::retain) suffixes
    /// [`episode_id`](Self::episode_id) onto it for the wire, so a resubmit replaces this
    /// install's document and `update_mode` replace can never hand it another install's.
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
    /// The server's stated reason when the operation failed.
    error_message: Option<String>,
}

const BANK_LIST_PAGE_SIZE: usize = 100;

#[derive(serde::Deserialize)]
struct BankListPage {
    banks: Vec<BankListEntry>,
    total: u64,
}

#[derive(serde::Deserialize)]
struct BankListEntry {
    bank_id: String,
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
        /// A bounded excerpt of the operation-status body — the server's stated reason.
        server_report: String,
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

/// Parse a Hindsight API base URL: absolute, http or https, with a host, no username or
/// password, no query, no fragment, and — once surrounding whitespace is trimmed — no tab,
/// carriage return or newline. The URL grammar strips those silently, so
/// the parsed URL would name a different host or path than the one spelled. Userinfo is
/// refused because Hindsight authenticates with a Bearer token or not at all, and reqwest
/// turns URL userinfo into a Basic Authorization header of its own, beside any configured
/// Bearer. Repeated trailing slashes collapse here: the endpoint builder strips one, and any
/// beyond it would ride into every request URL as an empty path segment.
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
    // The scheme is not named back: a schemeless spelling like `alice:secret@host/api` parses
    // with the username slot as its scheme, and these messages reach screens and logs.
    if !matches!(url.scheme(), "http" | "https") {
        return Err(InvalidApiUrl(
            "the URL must start with http:// or https://".to_owned(),
        ));
    }
    if url.host_str().is_none() {
        return Err(InvalidApiUrl("the URL names no host".to_owned()));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(InvalidApiUrl(
            "a query or fragment does not belong in a base URL".to_owned(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(InvalidApiUrl(
            "a username or password does not belong in the URL; supply an API token instead"
                .to_owned(),
        ));
    }
    if url.path().ends_with("//") {
        let trimmed = url.path().trim_end_matches('/').to_owned();
        url.set_path(&trimmed);
    }
    Ok(url)
}

/// The URL as logs and `status` may print it, or the refusal in place of it. That a credential
/// spelled into the URL never reaches a screen is a rule about the call sites — every printed
/// URL must route through here rather than the raw text.
pub fn display_api_url(text: &str) -> String {
    match parse_api_url(text) {
        Ok(url) => String::from(url),
        Err(error) => format!("unusable: {error}"),
    }
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
                message: format!("failed to build the HTTP client: {}", error_chain(&error)),
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

    /// Applies the initial settings only when the bank list reports it absent; a bank the list
    /// confirmed as existing is never written. The PATCH both creates a missing bank and sets
    /// its config, so a bank another client creates between the check and the PATCH still
    /// receives the settings once.
    pub fn ensure_bank(&self, bank_id: &str) -> Result<(), DeliveryError> {
        vet_bank_id(bank_id)?;
        if self.bank_exists(bank_id)? {
            return Ok(());
        }
        let url = self.endpoint(&["v1", "default", "banks", bank_id, "config"])?;
        let desired = json!({
            "updates": {
                "retain_extraction_mode": "chunks",
                "store_document_text": false,
                "observations_mission": OBSERVATIONS_MISSION,
            }
        });
        accept(
            self.send(self.http.patch(url).json(&desired).timeout(CONTROL_TIMEOUT))?,
            INITIALIZE_BANK_OPERATION,
        )
        .map(drop)
    }

    /// The list's `q` matches by substring against the id and the name both, so only the
    /// exact `bank_id` comparison decides. The list also reorders between requests
    /// (last-write order), so absence is concluded only when one response covered its
    /// stated `total`; anything less is an error, never absence. The walk is bounded by
    /// the first response's `total`, so a list that grows mid-walk is not chased.
    fn bank_exists(&self, bank_id: &str) -> Result<bool, DeliveryError> {
        let mut offset: u64 = 0;
        let mut requests: u64 = 0;
        let mut first_total: Option<u64> = None;
        loop {
            let mut url = self.endpoint(&["v1", "default", "banks"])?;
            {
                let mut query = url.query_pairs_mut();
                query
                    .append_pair("q", bank_id)
                    .append_pair("limit", &BANK_LIST_PAGE_SIZE.to_string())
                    .append_pair("offset", &offset.to_string());
            }
            let response = accept(
                self.send(self.http.get(url).timeout(CONTROL_TIMEOUT))?,
                LIST_BANKS_OPERATION,
            )?;
            let text = read_strict_text(response, LIST_BANKS_OPERATION)?;
            let page: BankListPage = parse_text(&text, LIST_BANKS_OPERATION)?;
            requests += 1;
            let total = *first_total.get_or_insert(page.total);
            if page.banks.iter().any(|bank| bank.bank_id == bank_id) {
                return Ok(true);
            }
            offset += page.banks.len() as u64;
            if offset >= total {
                if requests == 1 {
                    return Ok(false);
                }
                return Err(DeliveryError::Retryable {
                    message: format!(
                        "hindsight {LIST_BANKS_OPERATION} for {bank_id:?} walked the {total} \
                         banks the first response counted across {requests} reordered pages \
                         without finding it, so whether the bank exists stayed undecidable"
                    ),
                    retry_after: None,
                });
            }
            if page.banks.len() < BANK_LIST_PAGE_SIZE {
                return Err(DeliveryError::Retryable {
                    message: format!(
                        "hindsight {LIST_BANKS_OPERATION} for {bank_id:?} answered {} banks, \
                         short of the requested {BANK_LIST_PAGE_SIZE} and of the {total} the \
                         first response counted, so whether the bank exists stayed undecidable",
                        page.banks.len(),
                    ),
                    retry_after: None,
                });
            }
        }
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
        let (status, report) = self.operation_status(bank_id, &operation_id)?;
        if status != OPERATION_ABSENT {
            return operation_outcome(&status, &operation_id, &report);
        }
        let url = self.endpoint(&["v1", "default", "banks", bank_id, "memories"])?;
        let document_id = format!("{}-{}", item.document_id, item.episode_id);
        let request = RetainRequest {
            is_async: true,
            operation_id: &operation_id,
            items: [RetainItemWire {
                content: item.content,
                document_id: &document_id,
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

    /// The operation's status, and the server's account of it — its `error_message` when it
    /// states one, the whole status body otherwise, so a failing operation never reports
    /// nothing.
    fn operation_status(
        &self,
        bank_id: &str,
        operation_id: &str,
    ) -> Result<(String, String), DeliveryError> {
        let url = self.endpoint(&[
            "v1",
            "default",
            "banks",
            bank_id,
            "operations",
            operation_id,
        ])?;
        let response = accept(
            self.send(self.http.get(url).timeout(CONTROL_TIMEOUT))?,
            POLL_OPERATION,
        )
        .map_err(exchange_fault)?;
        let text = read_text(response, POLL_OPERATION)?;
        let operation: OperationWire = parse_text(&text, POLL_OPERATION)?;
        let report = operation.error_message.unwrap_or(text);
        Ok((operation.status, report))
    }

    /// Adds the Bearer form when a token is configured, as in Hindsight's own integrations.
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
            .field("base_url", &display_api_url(self.base_url.as_str()))
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
        message: transport_message(&error),
        retry_after: None,
    }
}

fn transport_message(error: &(dyn std::error::Error + 'static)) -> String {
    format!("hindsight request failed: {}", error_chain(error))
}

/// `reqwest::Error`'s display stops at the failing step ("error sending request for url (…)",
/// "builder error"); which step failed — refused, timed out, reset — is below it in `source()`.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut chain = error.to_string();
    // reqwest's blocking body reads hand back its own error re-wrapped in `io::Error`, so a
    // layer can repeat its parent word for word.
    let mut last = chain.clone();
    let mut source = error.source();
    while let Some(cause) = source {
        let rendered = cause.to_string();
        if rendered != last {
            chain.push_str(": ");
            chain.push_str(&rendered);
            last = rendered;
        }
        source = cause.source();
    }
    excerpt(&chain)
}

const RETAIN_OPERATION: &str = "retain";
const INITIALIZE_BANK_OPERATION: &str = "initialize bank";
const LIST_BANKS_OPERATION: &str = "list banks";
const POLL_OPERATION: &str = "operation status";

fn accept(response: Response, operation: &str) -> Result<Response, DeliveryError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after(&response);
    let body = match read_prefix(response) {
        Ok(text) => excerpt(&text),
        Err(error) => format!("(the body could not be read: {})", error_chain(&error)),
    };
    let message =
        format!("hindsight {operation} failed with HTTP {status}; the server said: {body}");
    if retryable_status(status) {
        Err(DeliveryError::Retryable {
            message,
            retry_after,
        })
    } else {
        Err(DeliveryError::Permanent { message })
    }
}

/// At most [`BODY_EXCERPT_BYTES`] and one spare byte — the spare is how [`excerpt`] knows there
/// was more — so a misbehaving proxy's error page is never buffered whole.
fn read_prefix(response: Response) -> std::io::Result<String> {
    use std::io::Read;
    let mut prefix = Vec::new();
    response
        .take(BODY_EXCERPT_BYTES as u64 + 1)
        .read_to_end(&mut prefix)?;
    Ok(String::from_utf8_lossy(&prefix).into_owned())
}

/// Text bound for one log line, bounded and printable. A refusal can quote the submitted input
/// back — for a retain that is a whole episode — so the excerpt keeps a megabyte out of every
/// log line while the actual reason still reaches the operator; control characters are escaped
/// so one log line stays one line.
fn excerpt(text: &str) -> String {
    // Decided before the trim: `read_prefix` stops one byte past the bound, and a trimmed
    // leading newline must not erase the marker that spare byte carries.
    let mut truncated = text.len() > BODY_EXCERPT_BYTES;
    let text = text.trim();
    if text.is_empty() {
        return "(empty body)".to_owned();
    }
    // The budget bounds the output, escapes included: a control character spends its escaped
    // width, so a body of nothing but controls cannot ride a 2048-byte allowance into a
    // several-fold longer log line.
    let mut bounded = String::with_capacity(text.len().min(BODY_EXCERPT_BYTES));
    for character in text.chars() {
        let start = bounded.len();
        if character.is_control() {
            bounded.extend(character.escape_default());
        } else {
            bounded.push(character);
        }
        if bounded.len() > BODY_EXCERPT_BYTES {
            bounded.truncate(start);
            truncated = true;
            break;
        }
    }
    // No byte count in the marker: under the capped read the total is unknown here.
    if truncated {
        bounded.push_str(" [truncated]");
    }
    bounded
}

fn parse<T: serde::de::DeserializeOwned>(
    response: Response,
    operation: &str,
) -> Result<T, DeliveryError> {
    parse_text(&read_text(response, operation)?, operation)
}

/// Bounds even the success-path read: every parsed body is a small control answer, so a
/// megabyte already means a broken server — the cap turns it into a decode error instead of
/// an unbounded buffer on the single delivery thread.
const PARSED_BODY_CAP: u64 = 1024 * 1024;

fn read_text(response: Response, operation: &str) -> Result<String, DeliveryError> {
    use std::io::Read;
    let mut body = Vec::new();
    response
        .take(PARSED_BODY_CAP)
        .read_to_end(&mut body)
        .map_err(|error| DeliveryError::Retryable {
            message: format!(
                "hindsight {operation} answered an unreadable body: {}",
                error_chain(&error)
            ),
            retry_after: None,
        })?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Strict UTF-8, unlike [`read_text`]: a lossy read could morph a `bank_id` into a
/// near-miss of the exact match, reading absence where the bank exists.
fn read_strict_text(response: Response, operation: &str) -> Result<String, DeliveryError> {
    use std::io::Read;
    let mut body = Vec::new();
    response
        .take(PARSED_BODY_CAP)
        .read_to_end(&mut body)
        .map_err(|error| DeliveryError::Retryable {
            message: format!(
                "hindsight {operation} answered an unreadable body: {}",
                error_chain(&error)
            ),
            retry_after: None,
        })?;
    match std::str::from_utf8(&body) {
        Ok(text) => Ok(text.to_owned()),
        Err(error) => Err(DeliveryError::Retryable {
            message: format!(
                "hindsight {operation} answered an undecodable body: {error}; the server said: \
                 {}",
                excerpt(&String::from_utf8_lossy(&body))
            ),
            retry_after: None,
        }),
    }
}

fn parse_text<T: serde::de::DeserializeOwned>(
    text: &str,
    operation: &str,
) -> Result<T, DeliveryError> {
    serde_json::from_str(text).map_err(|error| DeliveryError::Retryable {
        // The decode error itself is bounded too: serde embeds the whole body in a struct-level
        // type mismatch (`Unexpected::Str`), which on a 2xx answer is capped only by
        // `PARSED_BODY_CAP`.
        message: format!(
            "hindsight {operation} answered an undecodable body: {}; the server said: {}",
            excerpt(&error.to_string()),
            excerpt(text)
        ),
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

/// Maps the server's operation status; [`OPERATION_ABSENT`] never arrives —
/// [`HindsightClient::retain`] consumes it as the submit cue. The operation id rides every
/// non-completed outcome — the operator's handle on the server's operations API, derived from
/// nothing but the bank and episode ids — and `report`, the status body, rides the failing ones
/// as the server's own account of what happened.
fn operation_outcome(
    status: &str,
    operation_id: &str,
    report: &str,
) -> Result<RetainOutcome, DeliveryError> {
    match status {
        "completed" => Ok(RetainOutcome::Delivered),
        "pending" | "processing" => Ok(RetainOutcome::Processing {
            operation_id: operation_id.to_owned(),
        }),
        "failed" => Ok(RetainOutcome::Failed {
            operation_id: operation_id.to_owned(),
            server_report: excerpt(report),
        }),
        "cancelled" => Err(DeliveryError::Permanent {
            message: format!(
                "hindsight retain failed: operation {operation_id} was cancelled on the server; \
                 the server said: {}",
                excerpt(report)
            ),
        }),
        _ => Err(DeliveryError::Retryable {
            message: format!(
                "hindsight retain failed: operation {operation_id} answered the unrecognized \
                 status \"{}\"; the server said: {}",
                excerpt(status),
                excerpt(report)
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
/// Permanent. On the retain path [`exchange_fault`] overrides that verdict and bank
/// initialization backs off rather than condemns, so the split steers retry shape — never an
/// episode's survival.
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
            api_url: "http://alice:sesame@localhost:0".to_owned(),
            token: Some("the-secret-token".to_owned()),
        };
        let printed = format!("{credentials:?}");
        assert!(!printed.contains("the-secret-token"), "{printed}");
        assert!(!printed.contains("sesame"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");

        let client = HindsightClient::new(Credentials {
            api_url: "http://localhost:0".to_owned(),
            token: Some("the-secret-token".to_owned()),
        })
        .expect("building the client makes no request");
        let printed = format!("{client:?}");
        assert!(!printed.contains("the-secret-token"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
    }

    #[test]
    fn a_tokenless_source_still_configures_delivery() {
        let document: Map<String, Value> =
            [("hindsightApiUrl".to_owned(), "http://host:1/api".into())]
                .into_iter()
                .collect();
        let credentials =
            Credentials::from_document(&document, PathBuf::from("contextwitness.json"))
                .unwrap()
                .expect("a URL alone is a configured deployment, not a broken one");
        assert_eq!(credentials.api_url(), "http://host:1/api");
        assert_eq!(credentials.token, None);

        let credentials = Credentials::from_environment(Some("http://host:2/api".to_owned()), None)
            .unwrap()
            .expect("a URL alone is a configured deployment, not a broken one");
        assert_eq!(credentials.token, None);
    }

    #[test]
    fn each_credential_source_is_taken_whole_or_not_at_all() {
        let credentials = Credentials::from_environment(
            Some("http://host:3/api".to_owned()),
            Some("the-token".to_owned()),
        )
        .unwrap()
        .expect("a full credential set configures delivery");
        assert_eq!(credentials.token.as_deref(), Some("the-token"));

        let document: Map<String, Value> = [
            ("hindsightApiUrl".to_owned(), "http://host:4/api".into()),
            ("hindsightApiToken".to_owned(), "the-token".into()),
        ]
        .into_iter()
        .collect();
        let credentials =
            Credentials::from_document(&document, PathBuf::from("contextwitness.json"))
                .unwrap()
                .expect("a full credential set configures delivery");
        assert_eq!(
            credentials.token.as_deref(),
            Some("the-token"),
            "the file's token must ride along, not just its URL"
        );

        assert!(matches!(
            Credentials::from_environment(None, Some("the-token".to_owned())),
            Err(CredentialsError::EnvTokenWithoutUrl)
        ));
        assert!(
            Credentials::from_environment(None, None).unwrap().is_none(),
            "an empty environment defers to the file"
        );

        let document: Map<String, Value> = [("hindsightApiToken".to_owned(), "the-token".into())]
            .into_iter()
            .collect();
        assert!(matches!(
            Credentials::from_document(&document, PathBuf::from("contextwitness.json")),
            Err(CredentialsError::NoUrl { .. })
        ));
        assert!(
            Credentials::from_document(&Map::new(), PathBuf::from("contextwitness.json"))
                .unwrap()
                .is_none(),
            "a document holding neither key configures nothing"
        );
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
    fn a_configured_token_travels_as_bearer() {
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

        let refused = HindsightClient::new(Credentials {
            api_url: "http://alice:sesame@localhost:0".to_owned(),
            token: Some("the-secret-token".to_owned()),
        })
        .expect_err("a URL-spelled credential beside a token would send Basic and Bearer both");
        assert!(!refused.to_string().contains("alice"), "{refused}");
        assert!(!refused.to_string().contains("sesame"), "{refused}");
    }

    #[test]
    fn reqwest_turns_url_userinfo_into_a_basic_header_beside_the_bearer() {
        let client = HindsightClient::new(Credentials {
            api_url: "http://localhost:0".to_owned(),
            token: Some("the-secret-token".to_owned()),
        })
        .expect("building the client makes no request");
        let request = client
            .authorize(client.http.get("http://alice:sesame@localhost:0/x"))
            .build()
            .expect("a plain GET builds");
        let values: Vec<_> = request
            .headers()
            .get_all(reqwest::header::AUTHORIZATION)
            .iter()
            .map(|value| value.to_str().expect("the header is ASCII").to_owned())
            .collect();
        assert_eq!(
            values,
            ["Basic YWxpY2U6c2VzYW1l", "Bearer the-secret-token"],
            "the header pair that justifies refusing URL userinfo in `parse_api_url`"
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
    fn a_credential_spelled_into_the_url_never_reaches_the_display_form() {
        const REFUSED: &str = "unusable: a username or password does not belong in the URL; supply an API token \
             instead";
        assert_eq!(
            display_api_url("http://alice:secret@host:8000/api"),
            REFUSED
        );
        assert_eq!(display_api_url("http://alice@host/api"), REFUSED);
        assert_eq!(display_api_url("http://:secret@host/api"), REFUSED);
        assert_eq!(
            display_api_url("ftp://host/x"),
            "unusable: the URL must start with http:// or https://"
        );
        assert_eq!(
            display_api_url("ftp://alice:secret@host/x"),
            "unusable: the URL must start with http:// or https://"
        );
        assert_eq!(
            display_api_url("alice:secret@host:8000/api"),
            "unusable: the URL must start with http:// or https://"
        );
        assert_eq!(
            display_api_url("http://alice:secret@host/api?x=1"),
            "unusable: a query or fragment does not belong in a base URL"
        );
    }

    #[test]
    fn a_credential_spelled_into_the_url_is_refused_before_it_can_travel() {
        for spelled in [
            "http://alice:sesame@host:8000/api",
            "http://alice@host/api",
            "http://:sesame@host/api",
            "https://alice:sesame@host/api",
        ] {
            let refused = parse_api_url(spelled).expect_err("userinfo must not parse");
            assert_eq!(
                refused.to_string(),
                "a username or password does not belong in the URL; supply an API token instead"
            );
        }
        assert!(
            parse_api_url("http://host:8000/api").is_ok(),
            "an ordinary URL must stay accepted"
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
        let report = r#"{"status":"whatever","error":"the extractor's own account"}"#;
        let outcome = |status| operation_outcome(status, "the-operation-id", report);
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
                operation_id: "the-operation-id".to_owned(),
                server_report: report.to_owned(),
            },
            "a failed operation must stay watchable and carry the server's stated reason: the \
             operations body is the only place that reason exists"
        );
        let cancelled = outcome("cancelled").unwrap_err();
        assert!(!cancelled.is_retryable());
        assert!(
            cancelled.to_string().contains("the-operation-id"),
            "the operator's server-side handle must be named: {cancelled}"
        );
        assert!(
            cancelled
                .to_string()
                .contains("the extractor's own account"),
            "the server's account must ride the condemnation: {cancelled}"
        );
        for unhandled in [OPERATION_ABSENT, "someday-maybe"] {
            let unknown = outcome(unhandled).unwrap_err();
            assert!(
                unknown.is_retryable(),
                "an unmapped status must stay retryable, never a verdict: {unhandled}"
            );
            assert!(
                unknown.to_string().contains(unhandled),
                "the unrecognized value is the diagnosis: {unknown}"
            );
            assert!(
                unknown.to_string().contains("the-operation-id"),
                "{unhandled}"
            );
            assert!(
                unknown.to_string().contains("the extractor's own account"),
                "the server's account must ride the unknown-status report: {unknown}"
            );
        }
        let flooded = operation_outcome(
            &"z".repeat(BODY_EXCERPT_BYTES * 4),
            "the-operation-id",
            &"y".repeat(BODY_EXCERPT_BYTES * 4),
        )
        .unwrap_err();
        assert!(
            flooded.to_string().len() < BODY_EXCERPT_BYTES * 3,
            "an unrecognized status and its report are server-controlled text and must stay \
             bounded: {} bytes",
            flooded.to_string().len()
        );
        assert!(flooded.to_string().contains("[truncated]"), "{flooded}");
    }

    #[test]
    fn an_undecodable_body_is_reported_in_bounded_words() {
        let flood = format!("\"{}\"", "z".repeat(BODY_EXCERPT_BYTES * 4));
        let message = match parse_text::<OperationWire>(&flood, POLL_OPERATION) {
            Ok(_) => panic!("a bare string body must not decode as an operation"),
            Err(error) => error.to_string(),
        };
        assert!(
            message.len() < BODY_EXCERPT_BYTES * 3,
            "the decode error embeds the whole body on a struct-level mismatch and must be \
             bounded like the body itself: {} bytes",
            message.len()
        );
        assert!(message.contains("undecodable"), "{message}");
        assert!(message.contains("[truncated]"), "{message}");
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

    /// An answer, a raw-bytes answer, or a hang-up that reads the request and never answers.
    enum Turn {
        Answer { status_line: String, body: String },
        AnswerBytes { status_line: String, body: Vec<u8> },
        HangUp,
    }

    fn answer(status_line: &str, body: String) -> Turn {
        Turn::Answer {
            status_line: status_line.to_owned(),
            body,
        }
    }

    fn answer_bytes(status_line: &str, body: Vec<u8>) -> Turn {
        Turn::AnswerBytes {
            status_line: status_line.to_owned(),
            body,
        }
    }

    /// Answers each connection with the next [`Turn`] and sends every request seen once the
    /// turns are spent.
    fn scripted_server(
        turns: Vec<Turn>,
    ) -> (std::sync::mpsc::Receiver<Vec<(String, String)>>, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback binds");
        let port = listener
            .local_addr()
            .expect("the socket has an address")
            .port();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut requests = Vec::new();
            for turn in turns {
                let (mut stream, _) = listener.accept().expect("the client connects");
                let bound = Some(std::time::Duration::from_secs(10));
                stream.set_read_timeout(bound).expect("the timeout applies");
                stream
                    .set_write_timeout(bound)
                    .expect("the timeout applies");
                requests.push(read_request(&mut stream));
                let (status_line, body) = match turn {
                    Turn::Answer { status_line, body } => (status_line, body.into_bytes()),
                    Turn::AnswerBytes { status_line, body } => (status_line, body),
                    Turn::HangUp => continue,
                };
                let mut response = format!(
                    "{status_line}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(&body);
                std::io::Write::write_all(&mut stream, &response).expect("the response writes");
            }
            let _ = sender.send(requests);
        });
        (receiver, port)
    }

    fn bank_client(port: u16) -> HindsightClient {
        HindsightClient::new(Credentials {
            api_url: format!("http://127.0.0.1:{port}"),
            token: None,
        })
        .expect("building the client makes no request")
    }

    fn bank_list_page(banks: Vec<Value>, total: u64, offset: u64) -> String {
        json!({"banks": banks, "total": total, "limit": 100, "offset": offset}).to_string()
    }

    fn bank_profile(bank_id: &str, name: &str) -> Value {
        json!({"bank_id": bank_id, "name": name, "mission": "the owner's own mission"})
    }

    fn asked(requests: &[(String, String)]) -> Vec<&str> {
        requests.iter().map(|(asked, _)| asked.as_str()).collect()
    }

    fn seen_requests(
        seen: &std::sync::mpsc::Receiver<Vec<(String, String)>>,
    ) -> Vec<(String, String)> {
        seen.recv_timeout(std::time::Duration::from_secs(30))
            .expect("the server saw every scripted exchange")
    }

    #[test]
    fn the_delivery_lifecycle_submits_the_body_once_and_then_only_asks() {
        let operation_id = retain_operation_id("bank", "ep-1");
        let scripted = [
            r#"{"status":"not_found"}"#.to_owned(),
            format!(r#"{{"operation_id":"{operation_id}"}}"#),
            r#"{"status":"processing"}"#.to_owned(),
            r#"{"status":"failed","error_message":"the extractor gave out"}"#.to_owned(),
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
                operation_id: operation_id.clone(),
                server_report: "the extractor gave out".to_owned(),
            },
            "the server's error_message is the reason the operator reads"
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
        assert!(
            submitted.contains(r#""document_id":"doc-ep-1""#),
            "the wire document id must carry the episode id: bare wall-clock ids repeat across \
             installs sharing a bank, and replace mode would overwrite the other install: {submitted}"
        );
    }

    #[test]
    fn an_existing_bank_is_never_written_on_initialization_or_restart() {
        let list = bank_list_page(
            vec![
                // Id and name both match the substring query; only the exact bank_id
                // comparison tells it from the target.
                bank_profile("shadow-bank", "the bank of someone else"),
                bank_profile("bank", "the target"),
            ],
            2,
            0,
        );
        let (seen, port) = scripted_server(vec![
            answer("HTTP/1.1 200 OK", list.clone()),
            answer("HTTP/1.1 200 OK", list),
        ]);
        bank_client(port)
            .ensure_bank("bank")
            .expect("a bank the list names exactly is ready as it stands");
        // A fresh client stands in for a process restart.
        bank_client(port)
            .ensure_bank("bank")
            .expect("a restart must find the same bank the same way");
        assert_eq!(
            asked(&seen_requests(&seen)),
            [
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
            ],
            "an existing bank owes the read-only list check and nothing else, before or after \
             a restart"
        );
    }

    #[test]
    fn a_missing_bank_is_initialized_once_with_exactly_the_three_settings() {
        let (seen, port) = scripted_server(vec![
            answer("HTTP/1.1 200 OK", bank_list_page(vec![], 0, 0)),
            answer(
                "HTTP/1.1 200 OK",
                r#"{"bank_id":"bank","name":"the target","config":{}}"#.to_owned(),
            ),
            answer(
                "HTTP/1.1 200 OK",
                bank_list_page(
                    vec![json!({
                        "bank_id": "bank",
                        "name": "the target",
                        "mission": "the owner's own mission, changed after initialization",
                        "retain_chunk_size": 8000,
                    })],
                    1,
                    0,
                ),
            ),
        ]);
        bank_client(port)
            .ensure_bank("bank")
            .expect("an absent bank is this client's to initialize");
        bank_client(port)
            .ensure_bank("bank")
            .expect("the initialized bank now exists, whatever its settings are");
        let requests = seen_requests(&seen);
        assert_eq!(
            asked(&requests),
            [
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
                "PATCH /v1/default/banks/bank/config",
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
            ],
            "initialization is one list check and one config PATCH; every later initialization \
             only asks"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&requests[1].1).expect("the PATCH body is JSON"),
            json!({
                "updates": {
                    "retain_extraction_mode": "chunks",
                    "store_document_text": false,
                    "observations_mission": OBSERVATIONS_MISSION,
                }
            }),
            "initialization carries exactly the three agreed settings inside `updates`, and \
             nothing else: no PUT, no extra flags"
        );
    }

    #[test]
    fn an_undecidable_bank_list_never_falls_back_to_creation() {
        for (status_line, body) in [
            (
                "HTTP/1.1 401 Unauthorized",
                r#"{"detail":"the server's own words"}"#,
            ),
            (
                "HTTP/1.1 404 Not Found",
                r#"{"detail":"the server's own words"}"#,
            ),
            (
                "HTTP/1.1 500 Internal Server Error",
                r#"{"detail":"the server's own words"}"#,
            ),
            ("HTTP/1.1 200 OK", r#"{"banks":"the server's own words"}"#),
        ] {
            let (seen, port) = scripted_server(vec![answer(status_line, body.to_owned())]);
            let error = bank_client(port)
                .ensure_bank("bank")
                .expect_err("a list that cannot be read is not an absent bank");
            assert!(error.is_retryable(), "{status_line}: {error}");
            assert!(
                error.to_string().contains(LIST_BANKS_OPERATION),
                "{status_line}: the refusal must name the list exchange, not a creation that \
                 followed it: {error}"
            );
            assert!(
                error.to_string().contains("the server's own words"),
                "{status_line}: the bounded server words are the diagnosis: {error}"
            );
            assert_eq!(
                asked(&seen_requests(&seen)),
                ["GET /v1/default/banks?q=bank&limit=100&offset=0"],
                "{status_line}: an undecidable list must not lead into a settings write"
            );
        }
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback binds");
            listener
                .local_addr()
                .expect("the socket has an address")
                .port()
        };
        let refused = bank_client(port)
            .ensure_bank("bank")
            .expect_err("a server that cannot be reached leaves the bank undecidable");
        assert!(refused.is_retryable(), "{refused}");
        assert!(
            refused.to_string().contains("os error"),
            "the refused connection's cause lives in source() and must reach the log: {refused}"
        );
    }

    #[test]
    fn a_lost_initialization_answer_does_not_rewrite_settings_on_the_next_attempt() {
        let (seen, port) = scripted_server(vec![
            answer("HTTP/1.1 200 OK", bank_list_page(vec![], 0, 0)),
            Turn::HangUp,
            answer(
                "HTTP/1.1 200 OK",
                bank_list_page(vec![bank_profile("bank", "the target")], 1, 0),
            ),
        ]);
        let lost = bank_client(port)
            .ensure_bank("bank")
            .expect_err("a lost answer leaves initialization unconfirmed");
        assert!(lost.is_retryable(), "{lost}");
        bank_client(port)
            .ensure_bank("bank")
            .expect("the bank the lost exchange initialized is now simply present");
        assert_eq!(
            asked(&seen_requests(&seen)),
            [
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
                "PATCH /v1/default/banks/bank/config",
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
            ],
            "the next attempt must re-ask the list, never re-send the settings: the server may \
             have initialized the bank and its owner changed it"
        );
    }

    #[test]
    fn a_refused_initialization_is_an_error_and_a_later_attempt_can_still_succeed() {
        let (seen, port) = scripted_server(vec![
            answer("HTTP/1.1 200 OK", bank_list_page(vec![], 0, 0)),
            answer(
                "HTTP/1.1 403 Forbidden",
                r#"{"detail":"the server's own words"}"#.to_owned(),
            ),
            answer("HTTP/1.1 200 OK", bank_list_page(vec![], 0, 0)),
            answer(
                "HTTP/1.1 200 OK",
                r#"{"bank_id":"bank","config":{},"overrides":{}}"#.to_owned(),
            ),
        ]);
        let refused = bank_client(port)
            .ensure_bank("bank")
            .expect_err("a refused PATCH is not a successful initialization");
        assert!(refused.is_retryable(), "{refused}");
        assert!(
            refused.to_string().contains(INITIALIZE_BANK_OPERATION),
            "the refusal must name the initialization exchange: {refused}"
        );
        assert!(
            refused.to_string().contains("the server's own words"),
            "the bounded server words are the diagnosis: {refused}"
        );
        bank_client(port)
            .ensure_bank("bank")
            .expect("a later initialization attempt can still succeed");
        assert_eq!(
            asked(&seen_requests(&seen)),
            [
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
                "PATCH /v1/default/banks/bank/config",
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
                "PATCH /v1/default/banks/bank/config",
            ],
            "the refused initialization must reach BankGate's backoff and be retried as a whole: \
             list check first, then the PATCH again"
        );
    }

    #[test]
    fn the_bank_list_is_followed_until_the_target_or_the_end() {
        let filler: Vec<Value> = (0..100)
            .map(|index| bank_profile(&format!("decoy-{index:03}"), "a decoy bank"))
            .collect();
        let (seen, port) = scripted_server(vec![
            answer("HTTP/1.1 200 OK", bank_list_page(filler, 101, 0)),
            answer(
                "HTTP/1.1 200 OK",
                bank_list_page(vec![bank_profile("bank", "the target")], 101, 100),
            ),
        ]);
        bank_client(port)
            .ensure_bank("bank")
            .expect("the target sits on the second page");
        assert_eq!(
            asked(&seen_requests(&seen)),
            [
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
                "GET /v1/default/banks?q=bank&limit=100&offset=100",
            ],
            "a full page advances the offset; the found target ends the walk before any write"
        );
    }

    #[test]
    fn a_target_deep_in_the_list_is_still_found() {
        let filler_page = |offset: u64| {
            let filler: Vec<Value> = (0..100)
                .map(|index| bank_profile(&format!("decoy-{offset:04}-{index:03}"), "a decoy bank"))
                .collect();
            bank_list_page(filler, 1101, offset)
        };
        let mut turns: Vec<Turn> = (0..10_u64)
            .map(|page| answer("HTTP/1.1 200 OK", filler_page(page * 100)))
            .collect();
        turns.push(answer(
            "HTTP/1.1 200 OK",
            bank_list_page(vec![bank_profile("bank", "the target")], 1101, 1000),
        ));
        let (seen, port) = scripted_server(turns);
        bank_client(port)
            .ensure_bank("bank")
            .expect("the target sits on the eleventh page");
        let requests = seen_requests(&seen);
        assert_eq!(
            requests.len(),
            11,
            "the walk must follow the server's own total, so a page cap cannot strand an \
             existing bank"
        );
        assert_eq!(
            requests.last().expect("the walk reached the last page").0,
            "GET /v1/default/banks?q=bank&limit=100&offset=1000"
        );
    }

    #[test]
    fn a_short_page_after_the_first_never_concludes_absent() {
        let filler: Vec<Value> = (0..100)
            .map(|index| bank_profile(&format!("decoy-{index:03}"), "a decoy bank"))
            .collect();
        let (seen, port) = scripted_server(vec![
            answer("HTTP/1.1 200 OK", bank_list_page(filler, 101, 0)),
            answer(
                "HTTP/1.1 200 OK",
                bank_list_page(
                    vec![bank_profile("decoy-100", "the last decoy bank")],
                    101,
                    100,
                ),
            ),
        ]);
        let undecidable = bank_client(port)
            .ensure_bank("bank")
            .expect_err("a walk that never saw the bank cannot call it absent");
        assert!(undecidable.is_retryable(), "{undecidable}");
        assert!(
            undecidable.to_string().contains(LIST_BANKS_OPERATION)
                && undecidable.to_string().contains("undecidable"),
            "{undecidable}"
        );
        assert_eq!(
            asked(&seen_requests(&seen)),
            [
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
                "GET /v1/default/banks?q=bank&limit=100&offset=100",
            ],
            "the official list reorders by last write between requests, so a short late page \
             proves neither presence nor absence and no settings write may follow"
        );
    }

    #[test]
    fn a_first_page_short_of_its_own_total_never_concludes_absent() {
        let (seen, port) = scripted_server(vec![answer(
            "HTTP/1.1 200 OK",
            bank_list_page(vec![bank_profile("shadow-bank", "a kin decoy")], 2, 0),
        )]);
        let undecidable = bank_client(port)
            .ensure_bank("bank")
            .expect_err("a page that contradicts its own total cannot acquit a write");
        assert!(undecidable.is_retryable(), "{undecidable}");
        assert!(
            undecidable.to_string().contains("undecidable"),
            "{undecidable}"
        );
        assert_eq!(
            asked(&seen_requests(&seen)),
            ["GET /v1/default/banks?q=bank&limit=100&offset=0"],
            "a first response that held fewer banks than it counted cannot prove absence"
        );
    }

    #[test]
    fn substring_kin_without_an_exact_match_still_counts_as_absent() {
        let (seen, port) = scripted_server(vec![
            answer(
                "HTTP/1.1 200 OK",
                bank_list_page(
                    vec![
                        bank_profile("bank-2", "kin"),
                        bank_profile("shadow-bank", "kin"),
                    ],
                    2,
                    0,
                ),
            ),
            answer(
                "HTTP/1.1 200 OK",
                r#"{"bank_id":"bank","name":"the target"}"#.to_owned(),
            ),
        ]);
        bank_client(port)
            .ensure_bank("bank")
            .expect("kin alone is not the bank, so initialization is free to run");
        assert_eq!(
            asked(&seen_requests(&seen)),
            [
                "GET /v1/default/banks?q=bank&limit=100&offset=0",
                "PATCH /v1/default/banks/bank/config",
            ],
            "the exact bank_id alone decides; substring kin must not block initialization"
        );
    }

    #[test]
    fn an_invalid_utf8_bank_list_is_undecodable_not_absent() {
        let mut body = br#"{"banks":[{"bank_id":"ban"#.to_vec();
        body.push(0xFF);
        body.extend_from_slice(br#""}],"total":1,"limit":100,"offset":0}"#);
        let (seen, port) = scripted_server(vec![answer_bytes("HTTP/1.1 200 OK", body)]);
        let undecodable = bank_client(port)
            .ensure_bank("bank")
            .expect_err("bytes that cannot state a bank_id cannot acquit a write either");
        assert!(undecodable.is_retryable(), "{undecodable}");
        assert!(
            undecodable.to_string().contains("undecodable"),
            "{undecodable}"
        );
        assert_eq!(
            asked(&seen_requests(&seen)),
            ["GET /v1/default/banks?q=bank&limit=100&offset=0"],
            "a bank_id the exact match could not read must not morph into a near-miss and \
             license a settings write"
        );
    }

    #[test]
    fn the_excerpt_bounds_the_body_on_a_character_boundary() {
        let text = "あ".repeat(BODY_EXCERPT_BYTES);
        let bounded = excerpt(&text);
        assert!(bounded.len() < text.len(), "{} bytes", bounded.len());
        assert!(bounded.ends_with(" [truncated]"), "{bounded}");
        assert!(
            excerpt(&format!("\n{}", "z".repeat(BODY_EXCERPT_BYTES))).ends_with(" [truncated]"),
            "a trimmed leading newline must not erase the marker"
        );
        let flooded = excerpt(&"\u{1f}".repeat(BODY_EXCERPT_BYTES));
        assert!(
            flooded.len() <= BODY_EXCERPT_BYTES + " [truncated]".len(),
            "escaping must spend the budget, not multiply it: {} bytes",
            flooded.len()
        );
        assert!(flooded.ends_with(" [truncated]"), "{flooded}");
        assert_eq!(excerpt("  "), "(empty body)");
        assert_eq!(excerpt("short"), "short");
        assert_eq!(
            excerpt("a\r\nb"),
            "a\\r\\nb",
            "control characters must not break the log line they ride"
        );
    }

    #[test]
    fn a_refusal_carries_the_servers_bounded_words() {
        let sentinel = "the server's stated reason";
        let filler = "z".repeat(BODY_EXCERPT_BYTES);
        let scripted = [
            ("HTTP/1.1 200 OK", r#"{"status":"not_found"}"#.to_owned()),
            (
                "HTTP/1.1 422 Unprocessable Entity",
                format!(r#"{{"detail":"rejected: {sentinel}","echo":"{filler}THE-TAIL-MARKER"}}"#),
            ),
        ];
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback binds");
        let port = listener
            .local_addr()
            .expect("the socket has an address")
            .port();
        let server = std::thread::spawn(move || {
            for (status_line, body) in scripted {
                let (mut stream, _) = listener.accept().expect("the client connects");
                let bound = Some(std::time::Duration::from_secs(10));
                stream.set_read_timeout(bound).expect("the timeout applies");
                stream
                    .set_write_timeout(bound)
                    .expect("the timeout applies");
                read_request(&mut stream);
                let response = format!(
                    "{status_line}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                std::io::Write::write_all(&mut stream, response.as_bytes())
                    .expect("the response writes");
            }
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
        let refusal = client.retain("bank", &item).unwrap_err().to_string();
        assert!(
            refusal.contains(sentinel),
            "the server's stated reason is the diagnosis and must reach the operator: {refusal}"
        );
        assert!(
            refusal.contains("HTTP 422"),
            "the status still names the refusal class: {refusal}"
        );
        assert!(
            !refusal.contains("THE-TAIL-MARKER"),
            "the excerpt bound must hold: {refusal}"
        );
        assert!(refusal.contains("[truncated"), "{refusal}");
        server.join().expect("the server saw both exchanges");
    }

    #[test]
    fn a_transport_message_names_every_cause_bounded_and_printable() {
        #[derive(Debug)]
        struct Layer(String, Option<Box<Layer>>);
        impl std::fmt::Display for Layer {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1
                    .as_deref()
                    .map(|cause| cause as &(dyn std::error::Error + 'static))
            }
        }
        let root = Layer(
            format!("os says\u{1f}no: {}", "z".repeat(BODY_EXCERPT_BYTES)),
            None,
        );
        let error = Layer(
            "error sending request".to_owned(),
            Some(Box::new(Layer(
                "connect refused".to_owned(),
                Some(Box::new(Layer(
                    "connect refused".to_owned(),
                    Some(Box::new(root)),
                ))),
            ))),
        );
        let message = transport_message(&error);
        assert!(
            message.starts_with(
                "hindsight request failed: error sending request: connect refused: os says\\u{1f}no: "
            ),
            "every cause below the top display must reach the operator, printable: {message}"
        );
        assert!(message.ends_with(" [truncated]"), "{message}");
        assert!(
            message.len()
                <= "hindsight request failed: ".len() + BODY_EXCERPT_BYTES + " [truncated]".len(),
            "{} bytes",
            message.len()
        );
    }

    #[test]
    fn a_body_cut_mid_read_names_the_underlying_cause() {
        let scripted = [
            "HTTP/1.1 200 OK\r\ncontent-length: 50\r\nconnection: close\r\n\r\nhello",
            "HTTP/1.1 422 Unprocessable Entity\r\ncontent-length: 50\r\nconnection: close\r\n\r\nhello",
        ];
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback binds");
        let port = listener
            .local_addr()
            .expect("the socket has an address")
            .port();
        let server = std::thread::spawn(move || {
            for response in scripted {
                let (mut stream, _) = listener.accept().expect("the client connects");
                let bound = Some(std::time::Duration::from_secs(10));
                stream.set_read_timeout(bound).expect("the timeout applies");
                stream
                    .set_write_timeout(bound)
                    .expect("the timeout applies");
                read_request(&mut stream);
                std::io::Write::write_all(&mut stream, response.as_bytes())
                    .expect("the response writes");
            }
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
        let unreadable = client.retain("bank", &item).unwrap_err().to_string();
        assert!(
            unreadable.contains("answered an unreadable body"),
            "{unreadable}"
        );
        assert!(
            unreadable.contains("error reading a body"),
            "the read failure's cause lives in source() and must reach the log: {unreadable}"
        );
        let refused = client.retain("bank", &item).unwrap_err().to_string();
        assert!(
            refused.contains("(the body could not be read: "),
            "{refused}"
        );
        assert!(
            refused.contains("error reading a body"),
            "the refusal's unreadable body must still name its cause: {refused}"
        );
        server.join().expect("the server saw both exchanges");
    }

    #[test]
    fn a_refused_connection_reports_the_operating_systems_cause() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback binds");
            listener
                .local_addr()
                .expect("the socket has an address")
                .port()
        };
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
        let error = client.retain("bank", &item).unwrap_err();
        assert!(
            error.is_retryable(),
            "an unreachable server is a retry, not a verdict"
        );
        let message = error.to_string();
        assert!(
            message.contains("os error"),
            "the top display stops at 'error sending request'; the refused/timed-out/reset \
             cause lives in source() and must reach the log: {message}"
        );
    }
}
