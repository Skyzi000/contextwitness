use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::blocking::multipart::{Form, Part};
use reqwest::blocking::{Client, Response};
use reqwest::{StatusCode, Url};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig};

use crate::vad::{SAMPLE_RATE, Segment};
use crate::{Error, Utterance};

const BACKLOG: usize = 64;
const BACKLOG_FULL: &str = "transcription backlog is full";
const API_ATTEMPTS: usize = 3;
const API_RETRY_WAITS: [Duration; API_ATTEMPTS - 1] =
    [Duration::from_secs(2), Duration::from_secs(4)];
const API_TIMEOUT: Duration = Duration::from_secs(60);
const API_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REASON_CHARS: usize = 200;
const ERROR_BODY_BYTES: u64 = 4 * 1024;
const ANSWER_BYTES: u64 = 1024 * 1024;
const ENDPOINT: [&str; 2] = ["audio", "transcriptions"];

pub(crate) trait Transcriber: Send {
    fn transcribe(&mut self, samples: &[f32]) -> Result<String, String>;
}

pub(crate) struct Local {
    recognizer: OfflineRecognizer,
}

impl Local {
    pub(crate) fn new(model_dir: &Path, language: &str) -> Result<Self, Error> {
        let file = |name: &str| Some(model_dir.join(name).to_string_lossy().into_owned());
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
            model: file("model.int8.onnx"),
            language: Some(
                if language.is_empty() {
                    "auto"
                } else {
                    language
                }
                .to_owned(),
            ),
            use_itn: true,
        };
        config.model_config.tokens = file("tokens.txt");
        config.model_config.num_threads = 1;
        OfflineRecognizer::create(&config)
            .map(|recognizer| Self { recognizer })
            .ok_or_else(|| Error::Model(model_dir.to_owned()))
    }
}

impl Transcriber for Local {
    fn transcribe(&mut self, samples: &[f32]) -> Result<String, String> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE as i32, samples);
        self.recognizer.decode(&stream);
        stream
            .get_result()
            .map(|result| result.text)
            .ok_or_else(|| "the recognizer returned no result".to_owned())
    }
}

pub(crate) struct Api {
    http: Client,
    endpoint: Url,
    key: Option<String>,
    model: String,
    language: String,
    retry_waits: [Duration; API_ATTEMPTS - 1],
}

#[derive(serde::Deserialize)]
struct Transcript {
    text: String,
}

impl Api {
    pub(crate) fn new(
        url: &str,
        key: Option<String>,
        model: String,
        language: String,
    ) -> Result<Self, Error> {
        let mut endpoint = Url::parse(url).map_err(|error| Error::ApiUrl(error.to_string()))?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(Error::ApiUrl("it is neither http nor https".to_owned()));
        }
        let complete = endpoint
            .path()
            .trim_end_matches('/')
            .ends_with(&format!("/{}", ENDPOINT.join("/")));
        if !complete {
            endpoint
                .path_segments_mut()
                .map_err(|()| Error::ApiUrl("it cannot carry a path".to_owned()))?
                .pop_if_empty()
                .extend(ENDPOINT);
        }
        let http = Client::builder()
            .user_agent(concat!("contextwitness/", env!("CARGO_PKG_VERSION")))
            .timeout(API_TIMEOUT)
            .connect_timeout(API_CONNECT_TIMEOUT)
            .build()
            .map_err(|error| Error::HttpClient(error.to_string()))?;
        Ok(Self {
            http,
            endpoint,
            key,
            model,
            language,
            retry_waits: API_RETRY_WAITS,
        })
    }

    /// The reason in `Err` is stored and delivered with the episode, so nothing the server wrote
    /// goes into it: an error body can quote the key back.
    fn post(&self, wav: &[u8]) -> Result<String, (String, bool)> {
        let file = Part::bytes(wav.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|error| (error.to_string(), false))?;
        let mut form = Form::new()
            .part("file", file)
            .text("response_format", "json");
        if !self.model.is_empty() {
            form = form.text("model", self.model.clone());
        }
        if !self.language.is_empty() {
            form = form.text("language", self.language.clone());
        }
        let mut request = self.http.post(self.endpoint.clone()).multipart(form);
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }

        let mut response = request.send().map_err(|error| {
            let retryable = !error.is_builder();
            (transport(error), retryable)
        })?;
        let status = response.status();
        if !status.is_success() {
            let retryable = status.is_server_error()
                || status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::REQUEST_TIMEOUT;
            let body = read_up_to(&mut response, ERROR_BODY_BYTES).unwrap_or_default();
            tracing::warn!(
                status = %status,
                body = %loggable(&body, self.key.as_deref()),
                "the transcription API answered with an error"
            );
            return Err((format!("HTTP {status}"), retryable));
        }

        let body = read_up_to(&mut response, ANSWER_BYTES + 1).map_err(|error| {
            let reason = format!("the response could not be read: {}", error.kind());
            (reason, true)
        })?;
        if body.len() as u64 > ANSWER_BYTES {
            return Err(("the response is larger than 1 MiB".to_owned(), false));
        }
        serde_json::from_slice::<Transcript>(&body)
            .map(|transcript| transcript.text)
            .map_err(|error| {
                tracing::warn!(
                    category = ?error.classify(),
                    line = error.line(),
                    column = error.column(),
                    "the transcription API's answer is not JSON with a text field"
                );
                (
                    "the response is not JSON with a text field".to_owned(),
                    false,
                )
            })
    }
}

fn loggable(body: &[u8], key: Option<&str>) -> String {
    let body = String::from_utf8_lossy(body);
    match key.filter(|key| !key.is_empty()) {
        Some(key) => excerpt(&body.replace(key, "<key>")),
        None => excerpt(&body),
    }
}

fn read_up_to(response: &mut Response, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    response.take(limit).read_to_end(&mut body)?;
    Ok(body)
}

impl Transcriber for Api {
    fn transcribe(&mut self, samples: &[f32]) -> Result<String, String> {
        let wav = wav(samples);
        let mut waits = self.retry_waits.iter();
        loop {
            match self.post(&wav) {
                Ok(text) => return Ok(text),
                Err((reason, true)) => match waits.next() {
                    Some(wait) => std::thread::sleep(*wait),
                    None => return Err(reason),
                },
                Err((reason, false)) => return Err(reason),
            }
        }
    }
}

/// The URL is left out: it may carry credentials, and the reason travels into the episode text.
fn transport(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut reason = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !reason.ends_with(&cause_text) {
            reason = format!("{reason}: {cause_text}");
        }
        source = cause.source();
    }
    excerpt(&reason)
}

fn excerpt(reason: &str) -> String {
    let line = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(REASON_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line,
    }
}

/// Built in memory: the audio must never reach the disk.
fn wav(samples: &[f32]) -> Vec<u8> {
    let data_bytes = (samples.len() * 2) as u32;
    let rate = SAMPLE_RATE as u32;
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        let pcm = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        wav.extend_from_slice(&pcm.to_le_bytes());
    }
    wav
}

pub(crate) enum Event {
    Cut(DateTime<Utc>),
    Done {
        started_at: DateTime<Utc>,
        utterance: Option<Utterance>,
    },
}

struct Job {
    process: Arc<str>,
    segment: Segment,
}

impl Job {
    fn utterance(&self, outcome: Result<String, String>) -> Utterance {
        Utterance {
            process: self.process.to_string(),
            started_at: self.segment.started_at,
            duration_ms: (self.segment.samples.len() * 1000 / SAMPLE_RATE) as u64,
            outcome,
        }
    }
}

/// Each submitted utterance is answered by exactly one `Cut` and then one `Done`.
#[derive(Clone)]
pub(crate) struct Queue {
    jobs: SyncSender<Job>,
    events: Sender<Event>,
}

impl Queue {
    pub(crate) fn submit(&self, process: &Arc<str>, segment: Segment) {
        let started_at = segment.started_at;
        let _ = self.events.send(Event::Cut(started_at));
        let job = Job {
            process: process.clone(),
            segment,
        };
        let utterance = match self.jobs.try_send(job) {
            Ok(()) => return,
            Err(TrySendError::Full(job)) => {
                tracing::warn!(process = %process, "{BACKLOG_FULL}; an utterance was dropped");
                Some(job.utterance(Err(BACKLOG_FULL.to_owned())))
            }
            Err(TrySendError::Disconnected(_)) => None,
        };
        let _ = self.events.send(Event::Done {
            started_at,
            utterance,
        });
    }
}

pub(crate) fn start(
    mut transcriber: Box<dyn Transcriber>,
    events: Sender<Event>,
) -> std::io::Result<(Queue, JoinHandle<()>)> {
    let (jobs, backlog) = sync_channel::<Job>(BACKLOG);
    let queue = Queue {
        jobs,
        events: events.clone(),
    };
    let worker = std::thread::Builder::new()
        .name("cw-audio transcriber".to_owned())
        .spawn(move || work(transcriber.as_mut(), &backlog, &events))?;
    Ok((queue, worker))
}

fn work(transcriber: &mut dyn Transcriber, backlog: &Receiver<Job>, events: &Sender<Event>) {
    for job in backlog {
        let outcome = transcriber.transcribe(&job.segment.samples);
        let utterance = job.utterance(outcome);
        match &utterance.outcome {
            Ok(text) => tracing::debug!(
                process = %utterance.process,
                duration_ms = utterance.duration_ms,
                chars = text.chars().count(),
                "transcribed an utterance"
            ),
            Err(reason) => tracing::warn!(
                process = %utterance.process,
                duration_ms = utterance.duration_ms,
                reason,
                "transcription failed"
            ),
        }
        let blank = matches!(&utterance.outcome, Ok(text) if text.trim().is_empty());
        let done = Event::Done {
            started_at: utterance.started_at,
            utterance: (!blank).then_some(utterance),
        };
        if events.send(done).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use chrono::TimeDelta;

    use super::*;
    use crate::vad::{FORCED_CUT_SAMPLES, Segmenter, duration};

    struct Request {
        head: String,
        body: Vec<u8>,
    }

    impl Request {
        fn body_text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }

        fn field(&self, name: &str) -> Option<String> {
            let body = self.body_text();
            let (_, after) = body.split_once(&format!("name=\"{name}\"\r\n\r\n"))?;
            Some(after.split_once("\r\n")?.0.to_owned())
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Request {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
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
            .expect("the multipart body is sent with its length");
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).expect("the body reads");
            assert!(read > 0, "the client hung up mid-body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        Request {
            head,
            body: bytes[header_end..].to_vec(),
        }
    }

    fn scripted_server(answers: &[(&str, &str)]) -> (mpsc::Receiver<Vec<Request>>, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback binds");
        let port = listener
            .local_addr()
            .expect("the socket has an address")
            .port();
        let answers: Vec<(String, String)> = answers
            .iter()
            .map(|(status_line, body)| ((*status_line).to_owned(), (*body).to_owned()))
            .collect();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status_line, body) in answers {
                let (mut stream, _) = listener.accept().expect("the client connects");
                let bound = Some(Duration::from_secs(10));
                stream.set_read_timeout(bound).expect("the timeout applies");
                stream
                    .set_write_timeout(bound)
                    .expect("the timeout applies");
                requests.push(read_request(&mut stream));
                if status_line == HANG_UP {
                    continue;
                }
                let response = format!(
                    "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
            let _ = sender.send(requests);
        });
        (receiver, port)
    }

    fn seen(requests: &mpsc::Receiver<Vec<Request>>) -> Vec<Request> {
        requests
            .recv_timeout(Duration::from_secs(30))
            .expect("the server saw every scripted exchange")
    }

    fn api(base: &str, key: Option<&str>, model: &str, language: &str) -> Api {
        let mut api = Api::new(
            base,
            key.map(str::to_owned),
            model.to_owned(),
            language.to_owned(),
        )
        .expect("the client builds without a request");
        api.retry_waits = [Duration::ZERO; API_ATTEMPTS - 1];
        api
    }

    const OK: &str = "HTTP/1.1 200 OK";
    const HANG_UP: &str = "";

    #[test]
    fn the_api_request_is_a_multipart_post_carrying_the_wav_and_the_settings() {
        let (requests, port) = scripted_server(&[(OK, r#"{"text":" spoken words ","task":"x"}"#)]);
        let mut api = api(
            &format!("http://127.0.0.1:{port}/v1/"),
            Some("the-key"),
            "model-a",
            "ja",
        );
        let samples = [0.0, 0.5, -0.5];

        assert_eq!(
            api.transcribe(&samples),
            Ok(" spoken words ".to_owned()),
            "the text travels on exactly as the server gave it"
        );

        let requests = seen(&requests);
        let request = &requests[0];
        let head = request.head.to_ascii_lowercase();
        assert!(
            head.starts_with("post /v1/audio/transcriptions http/1.1\r\n"),
            "{head}"
        );
        assert!(
            head.contains("\r\nauthorization: bearer the-key\r\n"),
            "{head}"
        );
        assert!(
            head.contains("\r\ncontent-type: multipart/form-data; boundary="),
            "{head}"
        );
        assert_eq!(request.field("response_format").as_deref(), Some("json"));
        assert_eq!(request.field("model").as_deref(), Some("model-a"));
        assert_eq!(request.field("language").as_deref(), Some("ja"));
        let body = request.body_text();
        assert!(
            body.contains(
                "name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
            ),
            "{body}"
        );
        let expected = wav(&samples);
        assert!(
            request
                .body
                .windows(expected.len())
                .any(|window| window == expected),
            "the file part is the utterance as a WAV"
        );
    }

    #[test]
    fn without_a_key_no_authorization_travels_and_empty_settings_send_no_field() {
        let (requests, port) = scripted_server(&[(OK, r#"{"text":"words"}"#)]);
        let mut api = api(&format!("http://127.0.0.1:{port}"), None, "", "");

        assert_eq!(api.transcribe(&[0.25]), Ok("words".to_owned()));

        let requests = seen(&requests);
        let head = requests[0].head.to_ascii_lowercase();
        assert!(
            head.starts_with("post /audio/transcriptions http/1.1\r\n"),
            "{head}"
        );
        assert!(!head.contains("authorization"), "{head}");
        assert_eq!(
            requests[0].field("response_format").as_deref(),
            Some("json")
        );
        assert_eq!(requests[0].field("model"), None);
        assert_eq!(requests[0].field("language"), None);
    }

    #[test]
    fn a_failing_server_is_asked_three_times_and_its_last_status_is_the_reason() {
        let down = "HTTP/1.1 503 Service Unavailable";
        let (requests, port) = scripted_server(&[
            (down, r#"{"error":"warming up"}"#),
            (down, r#"{"error":"warming up"}"#),
            (
                "HTTP/1.1 500 Internal Server Error",
                "{\"error\":\n \"the model\tcrashed\"}",
            ),
        ]);
        let mut api = api(&format!("http://127.0.0.1:{port}"), None, "", "");

        assert_eq!(
            api.transcribe(&[0.25]),
            Err("HTTP 500 Internal Server Error".to_owned())
        );
        assert_eq!(seen(&requests).len(), 3);
    }

    #[test]
    fn a_server_that_recovers_is_asked_until_it_answers() {
        let (requests, port) = scripted_server(&[
            ("HTTP/1.1 429 Too Many Requests", "slow down"),
            (OK, r#"{"text":"words"}"#),
        ]);
        let mut api = api(&format!("http://127.0.0.1:{port}"), None, "", "");

        assert_eq!(api.transcribe(&[0.25]), Ok("words".to_owned()));
        assert_eq!(seen(&requests).len(), 2);
    }

    #[test]
    fn a_refusal_is_not_asked_again() {
        let (requests, port) =
            scripted_server(&[("HTTP/1.1 401 Unauthorized", r#"{"error":"bad key"}"#)]);
        let mut api = api(&format!("http://127.0.0.1:{port}"), Some("k"), "", "");

        assert_eq!(
            api.transcribe(&[0.25]),
            Err("HTTP 401 Unauthorized".to_owned())
        );
        assert_eq!(seen(&requests).len(), 1);
    }

    #[test]
    fn an_answer_without_text_is_a_failure() {
        let (_requests, port) = scripted_server(&[(OK, r#"{"segments":[]}"#)]);
        let mut api = api(&format!("http://127.0.0.1:{port}"), None, "", "");
        assert_eq!(
            api.transcribe(&[0.25]),
            Err("the response is not JSON with a text field".to_owned())
        );
    }

    #[test]
    fn nothing_the_server_writes_reaches_the_stored_reason() {
        let echo = "Incorrect API key provided: sk-proj-abcd************wxyz.";
        let refusal = format!(r#"{{"error":{{"message":"{echo}"}}}}"#);
        let not_an_object = format!(r#""{echo}""#);
        let not_json = format!("<html>{echo}</html>");
        for (status_line, body) in [
            ("HTTP/1.1 401 Unauthorized", &refusal),
            ("HTTP/1.1 400 Bad Request", &refusal),
            (OK, &not_an_object),
            (OK, &not_json),
        ] {
            let (_requests, port) = scripted_server(&[(status_line, body)]);
            let mut api = api(
                &format!("http://127.0.0.1:{port}"),
                Some("sk-proj-abcdefghwxyz"),
                "",
                "",
            );
            let reason = api.transcribe(&[0.25]).unwrap_err();
            assert!(
                !reason.contains("sk-") && !reason.contains("wxyz") && !reason.contains("key"),
                "{status_line}: {reason}"
            );
        }
    }

    #[test]
    fn a_key_the_server_echoes_reaches_neither_the_delivered_utterance_nor_the_log() {
        let key = "sk-proj-abcdefghwxyz";
        let refusal = format!(r#"{{"error":"Invalid bearer token '{key}' ({key})"}}"#);
        assert_eq!(
            loggable(refusal.as_bytes(), Some(key)),
            r#"{"error":"Invalid bearer token '<key>' (<key>)"}"#
        );
        let (_requests, port) = scripted_server(&[("HTTP/1.1 401 Unauthorized", &refusal)]);
        let api = api(&format!("http://127.0.0.1:{port}"), Some(key), "", "");
        let (events_in, events) = mpsc::channel();
        let (queue, _worker) = start(Box::new(api), events_in).expect("the worker thread starts");
        queue.submit(&"talk.exe".into(), segment(7));

        assert!(matches!(next(&events), Event::Cut(_)));
        match next(&events) {
            Event::Done {
                utterance: Some(utterance),
                ..
            } => assert_eq!(utterance.outcome, Err("HTTP 401 Unauthorized".to_owned())),
            _ => panic!("the failure is delivered as an utterance"),
        }
    }

    #[test]
    fn the_logged_excerpt_of_an_error_body_leaves_the_key_out_wherever_it_falls() {
        let key = "the-secret-key";
        let at_the_cut = format!("{}{key} and more", "x".repeat(REASON_CHARS - 5));
        let logged = loggable(at_the_cut.as_bytes(), Some(key));
        assert!(
            !logged.contains("the-s"),
            "a key the excerpt would have cut in two is gone whole: {logged}"
        );
        assert_eq!(logged.chars().count(), REASON_CHARS + 1);

        let quoted = "no key was configured, so the-secret-key is just text";
        assert_eq!(loggable(quoted.as_bytes(), None), quoted);
        assert_eq!(loggable(quoted.as_bytes(), Some("")), quoted);
    }

    #[test]
    fn an_answer_larger_than_the_cap_is_refused_unparsed() {
        let body = format!(r#"{{"text":"{}"}}"#, "a".repeat(ANSWER_BYTES as usize + 1));
        let (_requests, port) = scripted_server(&[(OK, &body)]);
        let mut api = api(&format!("http://127.0.0.1:{port}"), None, "", "");
        assert_eq!(
            api.transcribe(&[0.25]),
            Err("the response is larger than 1 MiB".to_owned())
        );
    }

    #[test]
    fn a_key_that_cannot_be_a_header_fails_once_without_being_quoted() {
        let mut api = api("http://127.0.0.1:9", Some("the-secret\nkey"), "", "");
        api.retry_waits = [Duration::from_secs(30); API_ATTEMPTS - 1];
        let began = std::time::Instant::now();
        let reason = api.transcribe(&[0.25]).unwrap_err();
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "no retry is waited for"
        );
        assert!(!reason.contains("the-secret"), "{reason}");
    }

    #[test]
    fn the_endpoint_is_appended_unless_the_url_already_ends_with_it() {
        let endpoint = |url: &str| {
            Api::new(url, None, String::new(), String::new())
                .expect("a usable URL")
                .endpoint
                .to_string()
        };
        assert_eq!(
            endpoint("https://host/v1"),
            "https://host/v1/audio/transcriptions"
        );
        assert_eq!(
            endpoint("https://host/v1/"),
            "https://host/v1/audio/transcriptions"
        );
        assert_eq!(
            endpoint("https://host"),
            "https://host/audio/transcriptions"
        );
        assert_eq!(
            endpoint("https://host/v1/audio/transcriptions"),
            "https://host/v1/audio/transcriptions"
        );
        assert_eq!(
            endpoint("https://host/v1/audio/transcriptions?api-version=1"),
            "https://host/v1/audio/transcriptions?api-version=1"
        );
        assert_eq!(
            endpoint("https://host/audio"),
            "https://host/audio/audio/transcriptions"
        );
    }

    #[test]
    fn a_server_that_hangs_up_is_asked_three_times_and_the_reason_leaves_the_url_out() {
        let (requests, port) = scripted_server(&[(HANG_UP, ""); API_ATTEMPTS]);
        let mut api = api(
            &format!("http://user:the-secret@127.0.0.1:{port}/v1?token=the-secret"),
            None,
            "",
            "",
        );
        let reason = api.transcribe(&[0.25]).unwrap_err();
        assert!(reason.starts_with("error sending request"), "{reason}");
        assert!(
            !reason.contains("the-secret") && !reason.contains("127.0.0.1"),
            "{reason}"
        );
        assert_eq!(seen(&requests).len(), API_ATTEMPTS);
    }

    #[test]
    fn an_api_url_that_cannot_be_a_base_is_refused() {
        for url in ["", "host:1/v1", "ftp://host/v1", "mailto:someone@host"] {
            assert!(
                matches!(
                    Api::new(url, None, String::new(), String::new()),
                    Err(Error::ApiUrl(_))
                ),
                "{url:?}"
            );
        }
    }

    #[test]
    fn a_long_reason_is_cut_on_a_character_boundary() {
        let long = "あ".repeat(REASON_CHARS + 50);
        let cut = excerpt(&long);
        assert_eq!(cut.chars().count(), REASON_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(excerpt("short\n reason"), "short reason");
    }

    #[test]
    fn the_wav_is_sixteen_bit_mono_at_sixteen_kilohertz() {
        let bytes = wav(&[0.0, 1.0, -1.0, 2.0, -2.0]);
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four"));
        let u16_at = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().expect("two"));
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32_at(4) as usize, bytes.len() - 8);
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(
            (
                u32_at(16),
                u16_at(20),
                u16_at(22),
                u32_at(24),
                u32_at(28),
                u16_at(32),
                u16_at(34)
            ),
            (16, 1, 1, 16_000, 32_000, 2, 16)
        );
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32_at(40), 10);
        let pcm: Vec<i16> = bytes[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|sample| i16::from_le_bytes(*sample))
            .collect();
        assert_eq!(pcm, [0, 32_767, -32_767, 32_767, -32_767]);
    }

    fn started(seconds: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-25T01:00:00Z")
            .expect("the fixed test timestamp is valid RFC 3339")
            .with_timezone(&Utc)
            + TimeDelta::seconds(seconds)
    }

    fn segment(seconds: i64) -> Segment {
        Segment {
            started_at: started(seconds),
            samples: vec![0.5; SAMPLE_RATE / 2],
        }
    }

    struct Gated {
        text: &'static str,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Transcriber for Gated {
        fn transcribe(&mut self, _samples: &[f32]) -> Result<String, String> {
            let _ = self.entered.send(());
            let _ = self.release.recv();
            Ok(self.text.to_owned())
        }
    }

    struct Gate {
        queue: Queue,
        events: mpsc::Receiver<Event>,
        entered: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
    }

    fn gated(text: &'static str) -> Gate {
        let (entered_in, entered) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (events_in, events) = mpsc::channel();
        let transcriber = Gated {
            text,
            entered: entered_in,
            release: released,
        };
        let (queue, _worker) =
            start(Box::new(transcriber), events_in).expect("the worker thread starts");
        Gate {
            queue,
            events,
            entered,
            release,
        }
    }

    fn next(events: &mpsc::Receiver<Event>) -> Event {
        events
            .recv_timeout(Duration::from_secs(30))
            .expect("an event arrives")
    }

    #[test]
    fn an_utterance_is_announced_when_cut_and_delivered_when_transcribed() {
        let gate = gated(" words ");
        let process: Arc<str> = "talk.exe".into();
        gate.queue.submit(&process, segment(7));

        assert!(matches!(next(&gate.events), Event::Cut(at) if at == started(7)));
        gate.release.send(()).expect("the worker waits");
        match next(&gate.events) {
            Event::Done {
                started_at,
                utterance: Some(utterance),
            } => {
                assert_eq!(started_at, started(7));
                assert_eq!(
                    utterance,
                    Utterance {
                        process: "talk.exe".to_owned(),
                        started_at: started(7),
                        duration_ms: 500,
                        outcome: Ok(" words ".to_owned()),
                    }
                );
            }
            _ => panic!("the transcribed utterance comes after its announcement"),
        }
    }

    #[test]
    fn a_blank_transcript_is_done_without_an_utterance() {
        let gate = gated(" \n");
        gate.queue.submit(&"talk.exe".into(), segment(7));
        gate.release.send(()).expect("the worker waits");

        assert!(matches!(next(&gate.events), Event::Cut(_)));
        assert!(matches!(
            next(&gate.events),
            Event::Done { started_at, utterance: None } if started_at == started(7)
        ));
    }

    #[test]
    fn an_utterance_beyond_the_backlog_is_dropped_and_reported_as_a_failure() {
        let gate = gated("words");
        let process: Arc<str> = "talk.exe".into();
        gate.queue.submit(&process, segment(0));
        gate.entered
            .recv_timeout(Duration::from_secs(30))
            .expect("the worker takes the first utterance off the backlog");
        for index in 1..=BACKLOG as i64 {
            gate.queue.submit(&process, segment(index));
        }
        let beyond = BACKLOG as i64 + 1;
        gate.queue.submit(&process, segment(beyond));

        let mut cut = 0;
        let dropped = loop {
            match next(&gate.events) {
                Event::Cut(_) => cut += 1,
                Event::Done { utterance, .. } => break utterance.expect("the drop is reported"),
            }
        };
        assert_eq!(
            cut,
            BACKLOG + 2,
            "every submitted utterance is announced first"
        );
        assert_eq!(dropped.started_at, started(beyond));
        assert_eq!(
            dropped.outcome,
            Err("transcription backlog is full".to_owned())
        );

        for _ in 0..=BACKLOG {
            gate.release.send(()).expect("the worker waits");
        }
        let delivered: Vec<DateTime<Utc>> = (0..=BACKLOG)
            .map(|_| match next(&gate.events) {
                Event::Done {
                    utterance: Some(utterance),
                    ..
                } => {
                    assert_eq!(utterance.outcome, Ok("words".to_owned()));
                    utterance.started_at
                }
                _ => panic!("the backlog is transcribed in order"),
            })
            .collect();
        assert_eq!(
            delivered,
            (0..=BACKLOG as i64).map(started).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_model_folder_with_missing_files_is_an_error() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        assert!(matches!(
            Local::new(dir.path(), ""),
            Err(Error::Model(path)) if path == dir.path()
        ));
    }

    fn sample() -> (std::path::PathBuf, Vec<f32>) {
        let model_dir = std::path::PathBuf::from(
            std::env::var_os("CW_AUDIO_TEST_MODEL_DIR")
                .expect("CW_AUDIO_TEST_MODEL_DIR names a SenseVoice model folder"),
        );
        let path = model_dir.join("test_wavs").join("ja.wav");
        let wave = sherpa_onnx::Wave::read(&path.to_string_lossy())
            .expect("the model folder ships test_wavs/ja.wav");
        assert_eq!(wave.sample_rate() as usize, SAMPLE_RATE);
        (model_dir, wave.samples().to_vec())
    }

    fn segments(samples: &[f32], packet: usize) -> Vec<Segment> {
        segments_cut_after(FORCED_CUT_SAMPLES, samples, packet)
    }

    fn segments_cut_after(forced_cut: i64, samples: &[f32], packet: usize) -> Vec<Segment> {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let model = crate::write_vad_model(dir.path()).expect("the model writes");
        let mut segmenter = Segmenter::new(&model).expect("the embedded model loads");
        segmenter.forced_cut = forced_cut;
        let mut cut = Vec::new();
        for (index, packet_samples) in samples.chunks(packet).enumerate() {
            segmenter.push(
                packet_samples,
                started(0) + duration(index * packet),
                &mut cut,
            );
        }
        segmenter.flush(&mut cut);
        cut
    }

    fn shape(segments: &[Segment]) -> Vec<(DateTime<Utc>, usize)> {
        segments
            .iter()
            .map(|segment| (segment.started_at, segment.samples.len()))
            .collect()
    }

    #[test]
    #[ignore = "needs a SenseVoice model folder in CW_AUDIO_TEST_MODEL_DIR"]
    fn the_sample_recording_is_cut_into_utterances_and_transcribed() {
        let (model_dir, samples) = sample();
        let by_packet = segments(&samples, 160);
        assert!(!by_packet.is_empty());
        assert_eq!(
            shape(&by_packet),
            shape(&segments(&samples, 512)),
            "the detector cuts 10 ms packets as it cuts its own window size"
        );

        let mut local = Local::new(&model_dir, "").expect("the model folder loads");
        for segment in &by_packet {
            let text = local
                .transcribe(&segment.samples)
                .expect("the utterance transcribes");
            assert!(!text.trim().is_empty());
            println!(
                "{} +{} ms: {} characters",
                segment.started_at.format("%H:%M:%S%.3f"),
                segment.samples.len() * 1000 / SAMPLE_RATE,
                text.chars().count()
            );
        }
    }

    fn hiss(samples: usize) -> impl Iterator<Item = f32> {
        (0..samples).map(|index| if index % 2 == 0 { 1e-4 } else { -1e-4 })
    }

    fn loud(samples: &[f32]) -> usize {
        samples
            .iter()
            .position(|sample| sample.abs() > 0.05)
            .expect("the sample recording has speech in it")
    }

    #[test]
    #[ignore = "needs a SenseVoice model folder in CW_AUDIO_TEST_MODEL_DIR"]
    fn an_utterance_begins_with_the_sound_that_led_up_to_it() {
        let (_, speech) = sample();
        let speech = &speech[loud(&speech) - SAMPLE_RATE / 20..];
        let played: Vec<f32> = hiss(SAMPLE_RATE)
            .chain(std::iter::repeat_n(0.0, SAMPLE_RATE / 5))
            .chain(speech.iter().copied())
            .collect();
        let loud = loud(&played);

        let first = segments(&played, 160).remove(0);
        let microseconds = (first.started_at - started(0))
            .num_microseconds()
            .expect("the utterance starts within the recording");
        let from = (microseconds as usize * SAMPLE_RATE + 500_000) / 1_000_000;
        println!(
            "the utterance starts {} ms before the speech gets loud",
            (loud as i64 - from as i64) * 1000 / SAMPLE_RATE as i64
        );
        assert!(
            from + SAMPLE_RATE * 3 / 10 <= loud,
            "more than the detector alone goes back, and past the silence it was not fed"
        );
        assert_eq!(
            first.samples[..loud - from],
            played[from..loud],
            "and it is the sound that was played then"
        );
    }

    #[test]
    #[ignore = "needs a SenseVoice model folder in CW_AUDIO_TEST_MODEL_DIR"]
    fn speech_that_lasts_under_half_a_second_is_an_utterance() {
        let (_, speech) = sample();
        let short = &speech[loud(&speech)..][..SAMPLE_RATE * 2 / 5];
        let played: Vec<f32> = hiss(SAMPLE_RATE)
            .chain(short.iter().copied())
            .chain(hiss(SAMPLE_RATE))
            .collect();
        assert_eq!(segments(&played, 160).len(), 1);
    }

    #[test]
    #[ignore = "needs a SenseVoice model folder in CW_AUDIO_TEST_MODEL_DIR"]
    fn speech_the_detector_does_not_close_is_cut_by_force_without_losing_any() {
        let (_, samples) = sample();
        let speech = &segments(&samples, 160)[0].samples;
        let unbroken: Vec<f32> = speech
            .iter()
            .copied()
            .cycle()
            .take(70 * SAMPLE_RATE)
            .collect();
        let report = |cut: &[Segment]| {
            for segment in cut {
                println!(
                    "{} +{} ms",
                    segment.started_at.format("%H:%M:%S%.3f"),
                    segment.samples.len() * 1000 / SAMPLE_RATE
                );
            }
        };

        let by_the_detector = segments(&unbroken, 160);
        report(&by_the_detector);
        assert!(
            by_the_detector
                .iter()
                .all(|segment| segment.samples.len() <= 23 * SAMPLE_RATE)
        );

        println!("forced after 8 s:");
        let forced = segments_cut_after(8 * SAMPLE_RATE as i64, &unbroken, 160);
        report(&forced);
        assert!(forced.len() >= 8);
        for pair in forced.windows(2) {
            assert!(pair[0].samples.len() <= 9 * SAMPLE_RATE);
            let end = pair[0].started_at + duration(pair[0].samples.len());
            let between = (pair[1].started_at - end).num_milliseconds();
            assert!(
                between.abs() <= 1,
                "{between} ms between one cut and the next utterance"
            );
        }
    }
}
