use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead as _, Write as _};
use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{capture, episodes, logging};
use chrono::{DateTime, TimeDelta, Utc};
use cw_core::config::{AudioBackend, AudioConfig, Config, DataPaths};
use cw_core::model::{AudioPayload, Observation};
use cw_store::control::{HealthKey, Pause};
use tracing::{error, info, warn};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

const FIRST_WAIT: Duration = Duration::from_secs(5);
const LONGEST_WAIT: Duration = Duration::from_secs(300);
const LASTED: Duration = Duration::from_secs(60);
const SETTLED_STEP: TimeDelta = TimeDelta::seconds(10);
const SKIP_GAP: TimeDelta = TimeDelta::seconds(30);

pub fn worker(
    config_path: &Path,
    data_dir: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(logging::level())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .init();
    std::thread::Builder::new()
        .name("parent-watch".to_owned())
        .spawn(|| {
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
            // Not `eprintln!`, which panics when the parent took the other end of stderr with it.
            let _ = writeln!(
                std::io::stderr(),
                "stdin ended: the process that started this audio worker is gone, so it exits"
            );
            std::process::exit(0);
        })?;

    // `load_from_path` answers a missing file with the defaults.
    if !config_path.is_file() {
        return Err(format!("{} is not a file", config_path.display()).into());
    }
    let config = Config::load_from_path(config_path)?;
    let paths = DataPaths::new(data_dir);
    let conn = cw_store::db::open(&paths.database())?;
    let settings = cw_audio::Settings {
        blacklist: config.privacy.process_blacklist,
        vad_model: cw_audio::write_vad_model(&paths.root).map_err(|error| {
            format!(
                "writing the voice activity model into {} failed: {error}",
                paths.root.display()
            )
        })?,
        backend: backend(&config.audio, &paths)?,
        language: config.audio.language.clone(),
    };
    let mut host = StoreHost::new(conn);

    info!(
        database = %paths.database().display(),
        backend = ?config.audio.backend,
        model_dir = %config.audio.resolve_model_dir(&paths).display(),
        "starting to record what applications play"
    );
    match cw_audio::run(settings, &mut host)? {}
}

fn backend(
    audio: &AudioConfig,
    paths: &DataPaths,
) -> Result<cw_audio::Backend, Box<dyn std::error::Error>> {
    match audio.backend {
        AudioBackend::Local => Ok(cw_audio::Backend::Local {
            model_dir: audio.resolve_model_dir(paths),
        }),
        AudioBackend::Api => {
            let credentials = cw_audio::credentials::Credentials::load()?.ok_or_else(|| {
                format!(
                    "audio.backend = \"api\" needs a transcription API, and none is configured; \
                     set api_url under [transcription] in credentials.toml, or {} (with {} when \
                     the API wants a key)",
                    cw_audio::credentials::URL_VARIABLE,
                    cw_audio::credentials::KEY_VARIABLE
                )
            })?;
            Ok(cw_audio::Backend::Api {
                url: credentials.api_url,
                key: credentials.api_key,
                model: audio.api_model.clone(),
            })
        }
    }
}

struct StoreHost {
    conn: rusqlite::Connection,
    failures: HashMap<&'static str, String>,
    settled_written: Option<DateTime<Utc>>,
    open_skips: HashMap<String, (ulid::Ulid, DateTime<Utc>)>,
}

impl StoreHost {
    fn new(conn: rusqlite::Connection) -> Self {
        Self {
            conn,
            failures: HashMap::new(),
            settled_written: None,
            open_skips: HashMap::new(),
        }
    }

    fn report<T>(
        &mut self,
        what: &'static str,
        outcome: Result<T, cw_store::StoreError>,
    ) -> Option<T> {
        match outcome {
            Ok(value) => {
                self.failures.remove(what);
                Some(value)
            }
            Err(error) => {
                let key = capture::save_failure_key(&error);
                if self.failures.get(what) != Some(&key) {
                    error!("{what}: {error}");
                    self.failures.insert(what, key);
                }
                None
            }
        }
    }
}

impl cw_audio::Host for StoreHost {
    fn paused(&mut self) -> bool {
        let pause = cw_store::control::get_pause(&self.conn);
        match self.report("the pause could not be read, so audio stays paused", pause) {
            Some(None) => false,
            Some(Some(Pause::Indefinite)) | None => true,
            Some(Some(Pause::Until(deadline))) => Utc::now() < deadline,
        }
    }

    fn utterance(&mut self, utterance: cw_audio::Utterance) {
        let chars = utterance
            .outcome
            .as_ref()
            .map_or(0, |text| text.chars().count());
        let (text, error) = match utterance.outcome {
            Ok(text) => (Some(text), None),
            Err(error) => (None, Some(error)),
        };
        let transcribed = text.is_some();
        let observation = Observation::new_audio(
            AudioPayload {
                process: utterance.process.clone(),
                text,
                error,
            },
            utterance.started_at,
            utterance.duration_ms,
        );
        let stored = cw_store::observations::insert(&self.conn, &observation);
        if self
            .report("an utterance could not be stored and is dropped", stored)
            .is_some()
        {
            info!(
                process = %utterance.process,
                duration_ms = utterance.duration_ms,
                transcribed,
                chars,
                "stored utterance"
            );
        }
    }

    fn settled(&mut self, through: DateTime<Utc>) {
        if self
            .settled_written
            .is_some_and(|written| through < written + SETTLED_STEP)
        {
            return;
        }
        let extended = self.open_skips.values().try_for_each(|(id, last_seen)| {
            cw_store::audio_skips::extend(&self.conn, *id, *last_seen)
        });
        if self
            .report(
                "the end of a blacklist skip could not be stored, so the settled mark stays",
                extended,
            )
            .is_none()
        {
            return;
        }
        let advanced =
            cw_store::control::advance_health(&self.conn, HealthKey::AudioSettled, through);
        if self
            .report("the settled mark could not be advanced", advanced)
            .is_some()
        {
            self.settled_written = Some(through);
        }
    }

    fn skipped(&mut self, by: &BTreeSet<String>, at: DateTime<Utc>) {
        let ended: Vec<_> = self
            .open_skips
            .iter()
            .filter(|(_, (_, last_seen))| at - *last_seen > SKIP_GAP)
            .map(|(name, (id, last_seen))| (name.clone(), *id, *last_seen))
            .collect();
        for (name, id, last_seen) in ended {
            let extended = cw_store::audio_skips::extend(&self.conn, id, last_seen);
            if self
                .report("the end of a blacklist skip could not be stored", extended)
                .is_some()
            {
                self.open_skips.remove(&name);
            }
        }

        for name in by {
            if let Some((_, last_seen)) = self.open_skips.get_mut(name) {
                *last_seen = at;
                continue;
            }
            let process = (!name.is_empty()).then_some(name.as_str());
            let begun = cw_store::audio_skips::begin(&self.conn, process, at);
            if let Some(id) = self.report("a blacklist skip could not be stored", begun) {
                self.open_skips.insert(name.clone(), (id, at));
            }
        }
    }
}

pub fn supervise(
    conn: &rusqlite::Connection,
    config_path: &Path,
    data_dir: &Path,
    window_minutes: u32,
    worker_alive: &AtomicBool,
) -> ! {
    let mut wait = None;
    loop {
        let started = Instant::now();
        let (ended, last_line) =
            match episodes::begin_audio_windows(conn, window_minutes, Utc::now()) {
                Ok(()) => run_worker(config_path, data_dir, worker_alive),
                Err(error) => (format!("was not started: {error}"), None),
            };
        let next = restart_wait(wait, started.elapsed());
        let last_line = last_line
            .map(|line| format!("; its last line was: {line}"))
            .unwrap_or_default();
        warn!(
            "the audio worker {ended}; starting it again in {} s{last_line}",
            next.as_secs()
        );
        std::thread::sleep(next);
        wait = Some(next);
    }
}

fn run_worker(
    config_path: &Path,
    data_dir: &Path,
    worker_alive: &AtomicBool,
) -> (String, Option<String>) {
    // Raised before the spawn: the closer reads a lowered flag as nothing being stored.
    worker_alive.store(true, Ordering::Relaxed);
    let exit = spawn_worker(config_path, data_dir).map(|mut child| {
        info!(pid = child.id(), "started the audio worker");
        // The worker exits when its stdin closes, and `wait` closes a stdin left in the child.
        let _stdin = child.stdin.take();
        let last_line = child.stderr.take().and_then(relay);
        let status = child
            .wait()
            .expect("waiting for the audio worker should not fail");
        (status, last_line)
    });
    worker_alive.store(false, Ordering::Relaxed);

    match exit {
        Ok((status, last_line)) => (format!("ended with {status}"), last_line),
        Err(error) => (format!("could not be started: {error}"), None),
    }
}

fn relay(output: impl std::io::Read) -> Option<String> {
    let mut last = None;
    for line in std::io::BufReader::new(output)
        .split(b'\n')
        .map_while(Result::ok)
    {
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end();
        info!("audio worker: {line}");
        if !line.is_empty() {
            last = Some(line.to_owned());
        }
    }

    last
}

fn spawn_worker(config_path: &Path, data_dir: &Path) -> std::io::Result<std::process::Child> {
    Command::new(std::env::current_exe()?)
        .arg("audio-worker")
        .arg("--config")
        .arg(config_path)
        .arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
}

fn restart_wait(previous: Option<Duration>, lived: Duration) -> Duration {
    match previous {
        Some(previous) if lived < LASTED => (previous * 2).min(LONGEST_WAIT),
        _ => FIRST_WAIT,
    }
}

#[cfg(test)]
mod tests {
    use super::{StoreHost, backend, relay, restart_wait};
    use chrono::{DateTime, TimeDelta, Utc};
    use cw_audio::{Host as _, Utterance};
    use cw_core::config::{AudioConfig, DataPaths};
    use cw_core::model::{AudioPayload, SourcePayload};
    use cw_store::control::{HealthKey, Pause};
    use std::collections::BTreeSet;
    use std::time::Duration;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("the test timestamp should be valid")
            .with_timezone(&Utc)
    }

    fn host() -> (tempfile::TempDir, StoreHost) {
        let dir =
            tempfile::tempdir().expect("the temporary database directory should be creatable");
        let conn = cw_store::db::open(&dir.path().join("db.sqlite3"))
            .expect("the fresh database should initialize");
        (dir, StoreHost::new(conn))
    }

    #[test]
    fn utterances_are_stored_as_the_rows_an_audio_episode_is_built_from() {
        let (_dir, mut host) = host();
        let heard = at("2026-07-24T16:01:00Z");
        let failed = heard + TimeDelta::seconds(2);
        host.utterance(Utterance {
            process: "talk.exe".to_owned(),
            started_at: heard,
            duration_ms: 1_500,
            outcome: Ok("one two three".to_owned()),
        });
        host.utterance(Utterance {
            process: "player.exe".to_owned(),
            started_at: failed,
            duration_ms: 700,
            outcome: Err("the request timed out".to_owned()),
        });

        let window = at("2026-07-24T16:00:00Z");
        let stored = cw_store::observations::find_in_window(
            &host.conn,
            window,
            window + TimeDelta::minutes(5),
        )
        .expect("the utterances should be readable");
        let rows: Vec<_> = stored
            .iter()
            .map(|row| (row.observed_at, row.duration_ms, row.payload.clone()))
            .collect();
        assert_eq!(
            rows,
            [
                (
                    heard,
                    Some(1_500),
                    SourcePayload::Audio(AudioPayload {
                        process: "talk.exe".to_owned(),
                        text: Some("one two three".to_owned()),
                        error: None,
                    })
                ),
                (
                    failed,
                    Some(700),
                    SourcePayload::Audio(AudioPayload {
                        process: "player.exe".to_owned(),
                        text: None,
                        error: Some("the request timed out".to_owned()),
                    })
                ),
            ]
        );

        let utc = chrono::FixedOffset::east_opt(0).expect("a zero offset is valid");
        let episode = cw_core::episode::build_audio_episode(window, 5, utc, &stored, &[])
            .expect("the window holds utterances");
        assert!(
            episode
                .content
                .contains("[talk.exe]\n  16:01:00 one two three")
        );
        assert!(
            episode
                .content
                .contains("[player.exe]\n  16:01:02 [transcription failed: the request timed out]")
        );
    }

    #[test]
    fn paused_follows_the_recorded_pause_and_its_deadline() {
        let (_dir, mut host) = host();
        let now = Utc::now();
        let pause = |host: &mut StoreHost, pause| {
            cw_store::control::set_pause(&mut host.conn, pause, ulid::Ulid::generate(), now)
                .expect("the pause should be stored");
        };

        assert!(!host.paused());
        pause(&mut host, Pause::Indefinite);
        assert!(host.paused());
        cw_store::control::resume(&mut host.conn, ulid::Ulid::generate(), now)
            .expect("the pause should be cleared");
        assert!(!host.paused());
        pause(&mut host, Pause::Until(now + TimeDelta::hours(1)));
        assert!(host.paused());
        pause(&mut host, Pause::Until(now - TimeDelta::seconds(1)));
        assert!(!host.paused());
    }

    #[test]
    fn a_pause_that_cannot_be_read_is_a_pause() {
        let (_dir, mut host) = host();
        host.conn
            .execute("DROP TABLE control_state", [])
            .expect("the table should be removable");

        assert!(host.paused());
    }

    #[test]
    fn settled_writes_the_first_mark_and_then_only_steps_of_ten_seconds_forward() {
        let (_dir, mut host) = host();
        let mark = |host: &StoreHost| {
            cw_store::control::get_health(&host.conn, HealthKey::AudioSettled)
                .expect("the settled mark should be readable")
        };

        assert_eq!(mark(&host), None);
        host.settled(at("2026-07-24T16:00:30Z"));
        assert_eq!(mark(&host), Some(at("2026-07-24T16:00:30Z")));
        host.settled(at("2026-07-24T16:00:39.999Z"));
        assert_eq!(mark(&host), Some(at("2026-07-24T16:00:30Z")));
        host.settled(at("2026-07-24T16:00:40Z"));
        assert_eq!(mark(&host), Some(at("2026-07-24T16:00:40Z")));
        host.settled(at("2026-07-24T16:00:20Z"));
        assert_eq!(mark(&host), Some(at("2026-07-24T16:00:40Z")));
    }

    #[test]
    fn a_settled_mark_that_could_not_be_written_is_tried_again_at_the_next_call() {
        let (_dir, mut host) = host();
        host.settled(at("2026-07-24T16:00:30Z"));
        host.conn
            .execute_batch("ALTER TABLE control_state RENAME TO control_state_away")
            .expect("the table should be movable");
        host.settled(at("2026-07-24T16:00:45Z"));
        host.conn
            .execute_batch("ALTER TABLE control_state_away RENAME TO control_state")
            .expect("the table should be movable back");

        host.settled(at("2026-07-24T16:00:46Z"));
        assert_eq!(
            cw_store::control::get_health(&host.conn, HealthKey::AudioSettled)
                .expect("the settled mark should be readable"),
            Some(at("2026-07-24T16:00:46Z"))
        );
    }

    fn by(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn skips(host: &StoreHost) -> Vec<(Option<String>, DateTime<Utc>, DateTime<Utc>)> {
        cw_store::audio_skips::overlapping(
            &host.conn,
            at("2000-01-01T00:00:00Z"),
            at("3000-01-01T00:00:00Z"),
        )
        .expect("the skips should be readable")
        .into_iter()
        .map(|span| (span.status.process, span.start_at, span.end_at))
        .collect()
    }

    fn settled_mark(host: &StoreHost) -> Option<DateTime<Utc>> {
        cw_store::control::get_health(&host.conn, HealthKey::AudioSettled)
            .expect("the settled mark should be readable")
    }

    const FIRST_SKIP: &str = "2026-07-24T16:01:00Z";

    fn after(seconds: i64) -> DateTime<Utc> {
        at(FIRST_SKIP) + TimeDelta::seconds(seconds)
    }

    fn vault(
        start_at: DateTime<Utc>,
        end_at: DateTime<Utc>,
    ) -> (Option<String>, DateTime<Utc>, DateTime<Utc>) {
        (Some("vault.exe".to_owned()), start_at, end_at)
    }

    #[test]
    fn a_name_seen_on_several_patrols_is_one_row_whose_end_is_written_before_the_mark_moves() {
        let (_dir, mut host) = host();
        host.skipped(&by(&["vault.exe"]), after(0));
        for second in 1..=5 {
            host.skipped(&by(&["vault.exe", ""]), after(second));
        }
        assert_eq!(
            skips(&host),
            [vault(after(0), after(0)), (None, after(1), after(1))]
        );

        host.settled(at("2026-07-24T16:01:05Z"));
        assert_eq!(
            skips(&host),
            [vault(after(0), after(5)), (None, after(1), after(5))]
        );
        assert_eq!(settled_mark(&host), Some(at("2026-07-24T16:01:05Z")));
    }

    #[test]
    fn a_name_absent_for_more_than_thirty_seconds_is_closed_where_it_was_last_seen() {
        let (_dir, mut host) = host();
        host.skipped(&by(&["vault.exe"]), after(0));
        host.skipped(&by(&["vault.exe"]), after(4));
        host.skipped(&by(&[]), after(34));
        assert_eq!(
            skips(&host),
            [vault(after(0), after(0))],
            "exactly thirty seconds of absence leave it open"
        );

        host.skipped(&by(&[]), after(35));
        assert_eq!(skips(&host), [vault(after(0), after(4))]);

        host.skipped(&by(&["vault.exe"]), after(36));
        assert_eq!(
            skips(&host),
            [vault(after(0), after(4)), vault(after(36), after(36))],
            "the return is a new row"
        );
    }

    #[test]
    fn a_name_that_returns_within_thirty_seconds_continues_its_row() {
        let (_dir, mut host) = host();
        host.skipped(&by(&["vault.exe"]), after(0));
        host.skipped(&by(&[]), after(1));
        host.skipped(&by(&["vault.exe"]), after(30));
        host.skipped(&by(&[]), after(59));
        host.skipped(&by(&["vault.exe"]), after(60));

        host.settled(at("2026-07-24T16:01:30Z"));
        assert_eq!(skips(&host), [vault(after(0), after(60))]);
    }

    #[test]
    fn a_name_seen_again_after_thirty_seconds_without_a_patrol_is_a_new_row() {
        let (_dir, mut host) = host();
        host.skipped(&by(&["vault.exe"]), after(0));
        host.skipped(&by(&["vault.exe"]), after(3_600));

        assert_eq!(
            skips(&host),
            [vault(after(0), after(0)), vault(after(3_600), after(3_600))]
        );
    }

    #[test]
    fn a_skip_whose_end_cannot_be_written_holds_the_settled_mark_back() {
        let (_dir, mut host) = host();
        host.skipped(&by(&["vault.exe"]), after(0));
        host.skipped(&by(&["vault.exe"]), after(5));
        host.conn
            .execute_batch("ALTER TABLE audio_skips RENAME TO audio_skips_away")
            .expect("the table should be movable");
        host.settled(at("2026-07-24T16:01:05Z"));
        host.conn
            .execute_batch("ALTER TABLE audio_skips_away RENAME TO audio_skips")
            .expect("the table should be movable back");
        assert_eq!(settled_mark(&host), None);
        assert_eq!(skips(&host), [vault(after(0), after(0))]);

        host.settled(at("2026-07-24T16:01:06Z"));
        assert_eq!(settled_mark(&host), Some(at("2026-07-24T16:01:06Z")));
        assert_eq!(skips(&host), [vault(after(0), after(5))]);
    }

    #[test]
    fn a_skip_that_could_not_be_stored_begins_at_the_next_patrol() {
        let (_dir, mut host) = host();
        host.conn
            .execute_batch("ALTER TABLE audio_skips RENAME TO audio_skips_away")
            .expect("the table should be movable");
        host.skipped(&by(&["vault.exe"]), after(0));
        host.conn
            .execute_batch("ALTER TABLE audio_skips_away RENAME TO audio_skips")
            .expect("the table should be movable back");

        host.skipped(&by(&["vault.exe"]), after(1));
        assert_eq!(skips(&host), [vault(after(1), after(1))]);
    }

    #[test]
    fn relaying_answers_the_last_line_that_is_not_blank() {
        assert_eq!(
            relay(&b"first\r\nerror: no model \xff\r\n\n  \n"[..]).as_deref(),
            Some("error: no model \u{fffd}")
        );
        assert_eq!(relay(&b"\n"[..]), None);
    }

    #[test]
    fn the_local_backend_without_a_model_folder_setting_reads_the_data_directory_s_models() {
        let paths = DataPaths::new("D:/data".into());
        let Ok(cw_audio::Backend::Local { model_dir }) = backend(&AudioConfig::default(), &paths)
        else {
            panic!("the default config asks for the local backend");
        };
        assert!(
            model_dir.starts_with(paths.models()),
            "{}",
            model_dir.display()
        );
    }

    #[test]
    fn the_wait_doubles_to_its_ceiling_and_starts_over_after_a_worker_that_lasted() {
        let mut wait = None;
        let waits: Vec<u64> = std::iter::repeat_with(|| {
            let next = restart_wait(wait, Duration::from_secs(1));
            wait = Some(next);
            next.as_secs()
        })
        .take(8)
        .collect();
        assert_eq!(waits, [5, 10, 20, 40, 80, 160, 300, 300]);

        assert_eq!(restart_wait(wait, Duration::from_secs(59)).as_secs(), 300);
        assert_eq!(restart_wait(wait, Duration::from_secs(60)).as_secs(), 5);
    }
}
