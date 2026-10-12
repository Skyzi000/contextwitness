#![deny(unsafe_op_in_unsafe_fn)]
//! Audio capture and transcription functionality for ContextWitness.

pub mod credentials;
mod processes;
mod select;
mod sessions;
mod stream;
mod transcribe;
mod vad;

use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};

use processes::Processes;
use select::{Ending, Seen, Target, judge};
use sessions::Sessions;
use stream::Stream;
use transcribe::{Api, Event, Local, Queue, Transcriber};

const PATROL: Duration = Duration::from_secs(1);
const SLOW_OPEN: Duration = Duration::from_secs(5);
const RETRY_AFTER: Duration = Duration::from_secs(30);

const FORCED_CUT_SECONDS: i64 = vad::FORCED_CUT_SAMPLES / vad::SAMPLE_RATE as i64;
const SETTLE_LAG: TimeDelta = TimeDelta::seconds(30);
const TIME_JUMP: TimeDelta = TimeDelta::seconds(5);
const SETTLE_HOLD: TimeDelta = TimeDelta::seconds(30);
const _: () = assert!(FORCED_CUT_SECONDS + TIME_JUMP.num_seconds() + 2 < SETTLE_LAG.num_seconds());
const _: () = assert!(FORCED_CUT_SECONDS + 5 <= SETTLE_HOLD.num_seconds());

const VAD_MODEL_FILE: &str = "silero_vad.onnx";
const VAD_MODEL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/vendor/silero_vad.onnx"
));

pub struct Settings {
    pub blacklist: Vec<String>,
    pub vad_model: PathBuf,
    pub backend: Backend,
    pub language: String,
}

pub enum Backend {
    Local {
        model_dir: PathBuf,
    },
    Api {
        url: String,
        key: Option<String>,
        model: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utterance {
    /// Executable name of the process whose tree was recorded, which also speaks for its children.
    pub process: String,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub outcome: Result<String, String>,
}

pub trait Host {
    fn paused(&mut self) -> bool;
    fn utterance(&mut self, utterance: Utterance);
    /// Every utterance that started before `through` has already been passed to
    /// [`utterance`](Self::utterance).
    fn settled(&mut self, through: DateTime<Utc>);
    /// Called on every patrol with the listed programs that kept audio from being recorded; a
    /// process without a readable name is the empty string.
    fn skipped(&mut self, by: &BTreeSet<String>, at: DateTime<Utc>);
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("COM could not be initialized: {0}")]
    Com(String),
    #[error("the process table could not be read: {0}")]
    ProcessTable(std::io::Error),
    #[error("the voice activity model {0} could not be loaded")]
    VadModel(PathBuf),
    #[error("{0} is not a usable SenseVoice model folder: it needs model.int8.onnx and tokens.txt")]
    Model(PathBuf),
    #[error("the transcription API URL is unusable: {0}")]
    ApiUrl(String),
    #[error("failed to build the HTTP client: {0}")]
    HttpClient(String),
    #[error("failed to start a thread: {0}")]
    Thread(std::io::Error),
    #[error("the transcriber stopped")]
    TranscriberStopped,
}

/// A model folder whose files are present but wrong or corrupt does not come back as an error:
/// the recognizer library ends the whole process.
pub fn run(settings: Settings, host: &mut dyn Host) -> Result<Infallible, Error> {
    wasapi::initialize_mta()
        .ok()
        .map_err(|error| Error::Com(error.to_string()))?;
    let processes = Processes::new().map_err(Error::ProcessTable)?;
    if vad::Segmenter::new(&settings.vad_model).is_none() {
        return Err(Error::VadModel(settings.vad_model));
    }
    let transcriber: Box<dyn Transcriber> = match settings.backend {
        Backend::Local { model_dir } => Box::new(Local::new(&model_dir, &settings.language)?),
        Backend::Api { url, key, model } => {
            Box::new(Api::new(&url, key, model, settings.language)?)
        }
    };
    let (events_in, events) = mpsc::channel();
    let (queue, transcriber) = transcribe::start(transcriber, events_in).map_err(Error::Thread)?;

    let mut patrol = Patrol {
        blacklist: settings.blacklist,
        vad_model: settings.vad_model,
        queue,
        processes,
        sessions: Sessions::default(),
        streams: HashMap::new(),
        trouble: None,
    };
    let mut pending = Pending::default();
    let mut hold = Hold::default();
    loop {
        let began = Instant::now();
        if transcriber.is_finished() {
            return Err(Error::TranscriberStopped);
        }
        let skipped = if host.paused() {
            patrol.end_all(Ending::Paused);
            BTreeSet::new()
        } else {
            patrol.walk()
        };
        host.skipped(&skipped, Utc::now());
        take_events(&events, began + PATROL, &mut pending, host)?;
        let now = Utc::now();
        if hold.clear(now)
            && let Some(through) = pending.settled(now)
        {
            host.settled(through);
        }
    }
}

fn take_events(
    events: &mpsc::Receiver<Event>,
    until: Instant,
    pending: &mut Pending,
    host: &mut dyn Host,
) -> Result<(), Error> {
    loop {
        let left = until.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(Event::Cut(started_at)) => pending.cut(started_at),
            Ok(Event::Done {
                started_at,
                utterance,
            }) => {
                pending.done(started_at);
                if let Some(utterance) = utterance {
                    host.utterance(utterance);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                pending.unread = false;
                return Ok(());
            }
            Err(RecvTimeoutError::Disconnected) => return Err(Error::TranscriberStopped),
        }
        if left.is_zero() {
            pending.unread = true;
            return Ok(());
        }
    }
}

pub fn write_vad_model(dir: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join(VAD_MODEL_FILE);
    if std::fs::read(&path).is_ok_and(|existing| existing == VAD_MODEL) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(&path, VAD_MODEL)?;
    Ok(path)
}

struct Open {
    stream: Stream,
    process: Arc<str>,
    opened: Instant,
    last_active: Instant,
    slow_open_reported: bool,
    ended: Option<Instant>,
}

impl Open {
    fn recording(&self, now: Instant) -> bool {
        !self.stream.ended() && (self.stream.started() || now - self.opened <= SLOW_OPEN)
    }
}

struct Patrol {
    blacklist: Vec<String>,
    vad_model: PathBuf,
    queue: Queue,
    processes: Processes,
    sessions: Sessions,
    streams: HashMap<Target, Open>,
    trouble: Option<String>,
}

impl Patrol {
    fn end_all(&mut self, ending: Ending) {
        if !self.streams.is_empty() {
            tracing::info!(
                streams = self.streams.len(),
                reason = ending.reason(),
                "closing every audio stream"
            );
        }
        for (_, open) in self.streams.drain() {
            open.stream.close(ending.close());
        }
    }

    fn report(&mut self, trouble: Option<String>) {
        if self.trouble == trouble {
            return;
        }
        match &trouble {
            Some(trouble) => tracing::warn!("{trouble}"),
            None => tracing::info!("the patrol works again"),
        }
        self.trouble = trouble;
    }

    fn walk(&mut self) -> BTreeSet<String> {
        let table = match self.processes.table() {
            Ok(table) => table,
            Err(error) => {
                self.report(Some(format!(
                    "the process list could not be read, so nothing is recorded: {error}"
                )));
                self.end_all(Ending::Unjudged);
                return BTreeSet::new();
            }
        };
        let sessions = self.sessions.active_pids();
        self.report(sessions.as_ref().err().map(|error| {
            format!("the audio sessions could not be listed, so nothing new is opened: {error}")
        }));
        let sessions = sessions.ok();

        let now = Instant::now();
        let seen: Vec<Seen> = self
            .streams
            .iter_mut()
            .map(|(target, open)| {
                let recording = open.recording(now);
                let alive = table
                    .get(&target.pid)
                    .is_some_and(|process| process.created == target.created);
                let playing = sessions
                    .as_ref()
                    .is_none_or(|active| active.contains(&target.pid));
                if recording && alive && playing {
                    open.last_active = now;
                }
                Seen {
                    target: *target,
                    idle: now - open.last_active,
                    recording,
                }
            })
            .collect();
        let verdict = judge(
            &table,
            sessions.as_ref(),
            &seen,
            std::process::id(),
            &self.blacklist,
        );

        for (target, ending) in verdict.endings {
            if let Some(open) = self.streams.remove(&target) {
                tracing::info!(
                    process = %open.process,
                    pid = target.pid,
                    reason = ending.reason(),
                    "closing an audio stream"
                );
                open.stream.close(ending.close());
            }
        }

        for target in verdict.openings {
            let Some(process) = table.get(&target.pid) else {
                continue;
            };
            let process: Arc<str> = process.exe.as_str().into();
            tracing::info!(process = %process, pid = target.pid, "opening an audio stream");
            match Stream::open(
                target.pid,
                process.clone(),
                self.vad_model.clone(),
                self.queue.clone(),
            ) {
                Ok(stream) => {
                    self.streams.insert(
                        target,
                        Open {
                            stream,
                            process,
                            opened: now,
                            last_active: now,
                            slow_open_reported: false,
                            ended: None,
                        },
                    );
                }
                Err(error) => tracing::warn!(
                    process = %process,
                    %error,
                    "an audio stream's thread could not be started"
                ),
            }
        }

        self.streams.retain(|target, open| {
            if open.stream.ended() {
                return now - *open.ended.get_or_insert(now) < RETRY_AFTER;
            }
            if !open.slow_open_reported && !open.stream.started() && now - open.opened > SLOW_OPEN {
                tracing::warn!(
                    process = %open.process,
                    pid = target.pid,
                    seconds = SLOW_OPEN.as_secs(),
                    "an audio stream has still not opened"
                );
                open.slow_open_reported = true;
            }
            true
        });

        verdict.skipped
    }
}

#[derive(Default)]
struct Pending {
    starts: Vec<DateTime<Utc>>,
    unread: bool,
}

impl Pending {
    fn cut(&mut self, started_at: DateTime<Utc>) {
        self.starts.push(started_at);
    }

    fn done(&mut self, started_at: DateTime<Utc>) {
        if let Some(index) = self
            .starts
            .iter()
            .position(|pending| *pending == started_at)
        {
            self.starts.swap_remove(index);
        }
    }

    fn settled(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        (!self.unread).then(|| {
            self.starts
                .iter()
                .copied()
                .fold(now - SETTLE_LAG, DateTime::min)
        })
    }
}

/// Withholds `settled` after a break in time. An utterance open across one is dated before it and
/// cut after it, so the lag alone would let `settled` pass its start.
#[derive(Default)]
struct Hold {
    last_turn: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
}

impl Hold {
    fn clear(&mut self, now: DateTime<Utc>) -> bool {
        if let Some(last) = self.last_turn.replace(now)
            && now - last > TIME_JUMP
        {
            tracing::info!(
                seconds = (now - last).num_seconds(),
                hold_seconds = SETTLE_HOLD.num_seconds(),
                "time jumped between two patrols; holding the settled mark back"
            );
            self.until = Some(now + SETTLE_HOLD);
        }
        if self.until.is_some_and(|until| now >= until) {
            self.until = None;
        }
        self.until.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-25T01:00:00Z")
            .expect("the fixed test timestamp is valid RFC 3339")
            .with_timezone(&Utc)
            + TimeDelta::seconds(seconds)
    }

    struct Deaf;

    impl Host for Deaf {
        fn paused(&mut self) -> bool {
            false
        }

        fn utterance(&mut self, _: Utterance) {}

        fn settled(&mut self, _: DateTime<Utc>) {}

        fn skipped(&mut self, _: &BTreeSet<String>, _: DateTime<Utc>) {}
    }

    #[test]
    fn events_still_waiting_when_the_patrol_is_due_are_left_and_hold_the_settled_mark_back() {
        let (events_in, events) = mpsc::channel();
        for second in 0..3 {
            events_in
                .send(Event::Cut(at(second)))
                .expect("the receiver is alive");
        }
        let mut pending = Pending::default();

        take_events(&events, Instant::now(), &mut pending, &mut Deaf).expect("the sender is alive");
        assert_eq!(pending.starts, [at(0)]);
        assert_eq!(pending.settled(at(100)), None);

        let soon = Instant::now() + Duration::from_millis(50);
        take_events(&events, soon, &mut pending, &mut Deaf).expect("the sender is alive");
        assert_eq!(pending.starts, [at(0), at(1), at(2)]);
        assert_eq!(pending.settled(at(100)), Some(at(0)));
    }

    #[test]
    fn with_nothing_pending_everything_older_than_the_lag_is_settled() {
        assert_eq!(Pending::default().settled(at(100)), Some(at(70)));
    }

    #[test]
    fn a_pending_utterance_holds_the_mark_at_its_start_until_it_is_done() {
        let mut pending = Pending::default();
        pending.cut(at(40));
        pending.cut(at(60));
        assert_eq!(pending.settled(at(100)), Some(at(40)));
        pending.done(at(40));
        assert_eq!(pending.settled(at(100)), Some(at(60)));
        pending.done(at(60));
        assert_eq!(pending.settled(at(100)), Some(at(70)));
    }

    #[test]
    fn a_pending_utterance_newer_than_the_lag_does_not_move_the_mark_forward() {
        let mut pending = Pending::default();
        pending.cut(at(90));
        assert_eq!(pending.settled(at(100)), Some(at(70)));
    }

    #[test]
    fn two_utterances_with_one_start_are_two_pending_entries() {
        let mut pending = Pending::default();
        pending.cut(at(40));
        pending.cut(at(40));
        pending.done(at(40));
        assert_eq!(pending.settled(at(100)), Some(at(40)));
        pending.done(at(40));
        assert_eq!(pending.settled(at(100)), Some(at(70)));
    }

    #[test]
    fn evenly_spaced_turns_are_never_held() {
        let mut hold = Hold::default();
        assert!(hold.clear(at(0)));
        assert!(hold.clear(at(1)));
        assert!(
            hold.clear(at(6)),
            "a turn exactly the jump length later is not a break"
        );
        assert!(hold.clear(at(2)), "a clock set back starts no hold");
    }

    fn held(hold: &mut Hold, from: i64, to: i64) -> Vec<i64> {
        (from..=to)
            .filter(|second| !hold.clear(at(*second)))
            .collect()
    }

    #[test]
    fn a_break_in_time_starts_a_hold_that_ends_after_its_length() {
        let mut hold = Hold::default();
        assert!(held(&mut hold, 0, 10).is_empty());
        assert_eq!(
            held(&mut hold, 3_600, 3_640),
            (3_600..3_630).collect::<Vec<_>>(),
            "an hour of sleep, then thirty seconds without a mark"
        );
    }

    #[test]
    fn a_second_break_during_a_hold_starts_it_over() {
        let mut hold = Hold::default();
        assert!(held(&mut hold, 0, 1).is_empty());
        assert_eq!(held(&mut hold, 7, 10), [7, 8, 9, 10]);
        assert_eq!(
            held(&mut hold, 20, 60),
            (20..50).collect::<Vec<_>>(),
            "held from the second break, past where the first hold would have ended"
        );
    }

    #[test]
    fn the_embedded_model_is_written_once_and_then_left_alone() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let nested = dir.path().join("data");
        let path = write_vad_model(&nested).expect("the model writes");
        assert_eq!(std::fs::read(&path).expect("the model reads"), VAD_MODEL);

        let modified = || {
            std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .expect("the file has a modification time")
        };
        let written = modified();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(write_vad_model(&nested).expect("the second call"), path);
        assert_eq!(modified(), written);

        std::fs::write(&path, b"truncated").expect("the file is replaced");
        write_vad_model(&nested).expect("the model is repaired");
        assert_eq!(std::fs::read(&path).expect("the model reads"), VAD_MODEL);
    }

    #[test]
    fn the_embedded_model_loads_from_a_folder_with_a_non_ascii_name() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = write_vad_model(&dir.path().join("音声 データ")).expect("the model writes");
        assert!(vad::Segmenter::new(&path).is_some());
    }

    #[test]
    fn a_missing_model_is_not_a_detector() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        assert!(vad::Segmenter::new(&dir.path().join(VAD_MODEL_FILE)).is_none());
    }
}
