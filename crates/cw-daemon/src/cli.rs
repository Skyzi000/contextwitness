use std::io::Write as _;

use crate::{autostart, capture, delivery, episodes, logging, maintenance, tray};
use clap::{Parser, Subcommand};
use cw_core::atomic_file::create_temporary_beside;
use cw_core::config::{Config, ConfigError, DataPaths, StorageConfig, default_config_path};
use cw_store::control::{HealthKey, Pause};
use tracing::{error, info, warn};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
    SetConsoleCtrlHandler, SetConsoleMode,
};
use windows::Win32::System::Threading::CreateMutexW;

/// How long startup keeps asking for the monitor list before giving up. Enumeration fails while a
/// session is still coming up, and the reachability check below cannot be skipped, so this waits
/// for an answer instead of starting without one.
const MONITOR_ATTEMPTS: u32 = 5;
const MONITOR_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// How long `capture-once --wgc` waits for every monitor to answer. Past the fallback's own
/// five-second first-frame grace, which is what decides whether a silent session is broken or
/// merely idle: giving up first would report a monitor as missing that the backend had not
/// finished judging.
const WGC_ANSWER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);
const WGC_ANSWER_RETRY: std::time::Duration = std::time::Duration::from_millis(500);

/// The key `setup` rewrites, and the only place the data directory is configured.
const DATA_DIR_KEY: &str = "data_dir";

/// Width the status screen's labels are padded to, so its values line up in one column.
const STATUS_LABEL: usize = 14;

/// The name `run` claims for as long as it runs. `Local\` is the session namespace on purpose: one
/// collector per interactive session is the line worth drawing. A daemon in another session is a
/// second, unarbitrated writer for episode closure; v1 does not arbitrate those writers.
const INSTANCE_MUTEX: &str = "Local\\ContextWitness";

/// What the process ends with when a panic takes it down: the code Rust's own runtime uses, so
/// nothing downstream has to learn a second number for the same event.
const PANIC_EXIT: i32 = 101;

/// How long the panic hook waits before the exit. The file writer drains on its own thread and its
/// guard is not reachable from a hook, so the line just logged is still queued at that point.
const PANIC_FLUSH: std::time::Duration = std::time::Duration::from_millis(200);

type Failure = Box<dyn std::error::Error>;

#[derive(Parser)]
#[command(
    name = "contextwitness",
    version,
    about = "Captures the screen, reads it, and delivers what it saw to Hindsight.",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Capture, group and deliver until stopped, with the tray icon up.
    Run,
    /// Print what this installation is doing, in one screen.
    Status,
    /// Stop capturing, until a deadline or until `resume`.
    Pause {
        /// How long to stay paused: a whole number and its unit, as in `30m`, `90m` or `2h`.
        /// Left out, the pause lasts until `resume`.
        #[arg(value_parser = parse_pause)]
        duration: Option<chrono::TimeDelta>,
    },
    /// Start capturing again.
    Resume,
    /// Ask for the Hindsight credentials and the data directory, and write them.
    Setup,
    /// Start at logon, or stop doing so.
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
    /// Capture, read and store the way a tick would, then exit.
    CaptureOnce {
        /// Capture through the fallback backend (Windows Graphics Capture) instead of the
        /// primary, to check that the failover path works on this machine.
        #[arg(long)]
        wgc: bool,
    },
}

#[derive(Subcommand)]
enum AutostartAction {
    /// Start ContextWitness at logon.
    Enable,
    /// Stop starting ContextWitness at logon.
    Disable,
}

pub fn main() {
    let outcome = match Cli::parse().command {
        Command::Run => daemon(),
        Command::Status => status(),
        Command::Pause { duration } => pause(duration),
        Command::Resume => resume(),
        Command::Setup => setup(),
        Command::Autostart { action } => set_autostart(&action),
        Command::CaptureOnce { wgc } => capture_once(wgc),
    };

    if let Err(error) = outcome {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// The daemon: everything this program does on its own, until the process ends.
fn daemon() -> ! {
    claim_single_instance();
    let dpi_aware = cw_capture::make_dpi_aware();
    cw_ocr::init_runtime();

    let config_path = match default_config_path() {
        Ok(path) => path,
        Err(error) => refuse_before_logging(&format!("resolving the config path failed: {error}")),
    };
    let (created, config, data_dir) = match load_startup_config(&config_path) {
        Ok(loaded) => loaded,
        Err(error) => refuse_before_logging(&format!(
            "startup failed with {}: {error}",
            config_path.display()
        )),
    };
    let paths = DataPaths::new(data_dir);
    let logging = logging::init(&paths);
    install_panic_hook();
    if created {
        info!(path = %config_path.display(), "wrote the default config");
    }

    // Without PER_MONITOR_AWARE_V2 every capture arrives at a virtualized resolution and every DPI
    // reads 96, silently: a visible refusal over a daemon that degrades without saying so.
    if !dpi_aware {
        error!(
            "the process could not become PER_MONITOR_AWARE_V2, and capture would be silently degraded"
        );
        drop(logging);
        std::process::exit(1);
    }

    let mut capture = cw_capture::CaptureEngine::new();
    if let Err(message) = check_thresholds(&mut capture, &config, &config_path) {
        error!("{message}");
        drop(logging);
        std::process::exit(1);
    }

    let mut conn = cw_store::db::open(&paths.database()).expect("opening the database failed");
    maintenance::sweep_orphans(&mut conn, &paths);
    let mut cursor: episodes::Cursor = None;
    match episodes::close_due(
        &mut conn,
        &mut cursor,
        config.episode.window_minutes,
        chrono::Utc::now(),
    ) {
        Ok(0) => {}
        Ok(registered) => info!(registered, "registered windows that closed while stopped"),
        Err(error) => error!("the startup episode rescan failed: {error}"),
    }

    spawn("delivery", {
        let conn = open_for("delivery", &paths);
        let data_dir = paths.root.clone();
        let config = config.clone();
        move || delivery::run(conn, data_dir, config)
    });
    spawn("episodes", {
        let conn = open_for("episodes", &paths);
        let config = config.clone();
        move || episodes::run(conn, config, cursor)
    });
    spawn("maintenance", {
        let conn = open_for("maintenance", &paths);
        let config = config.clone();
        let paths = DataPaths::new(paths.root.clone());
        move || maintenance::run(conn, paths, config)
    });
    tray::spawn(DataPaths::new(paths.root.clone()));

    info!(
        interval_secs = config.capture.interval_secs,
        database = %paths.database().display(),
        "capturing"
    );
    let ocr = cw_ocr::WindowsOcr;
    capture::run(capture, &ocr, conn, paths, config)
}

/// Refuse to be the second daemon in this session. The handle is never closed: the OS drops it
/// however the process dies, which is why the claim is a mutex and not a lock file — there is no
/// stale lock to recognize and clean up after a crash.
fn claim_single_instance() {
    let name = windows::core::HSTRING::from(INSTANCE_MUTEX);
    let _held = unsafe { CreateMutexW(None, false, &name) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        eprintln!("contextwitness is already running in this session.");
        std::process::exit(1);
    }
}

/// Whether this call wrote the default config, the config itself, and the directory it names.
/// Fallible as one piece: the daemon has nothing to do with a startup that got halfway, and the
/// caller has one place to report every way it can end.
fn load_startup_config(
    config_path: &std::path::Path,
) -> Result<(bool, Config, std::path::PathBuf), ConfigError> {
    let created = Config::write_default_if_missing(config_path)?;
    let config = Config::load_from_path(config_path)?;
    let data_dir = config.storage.resolve_data_dir()?;

    Ok((created, config, data_dir))
}

/// Report a startup that never got a usable config, and end. The log goes under the *default* data
/// directory, which is the only one still standing: the config that would have named another one
/// is the thing that failed. A machine where even that has no spelling leaves no place to install
/// a log subscriber, so `error!` lands on nothing and the exit code is all the caller gets.
fn refuse_before_logging(message: &str) -> ! {
    let logging = StorageConfig::default()
        .resolve_data_dir()
        .ok()
        .and_then(|root| logging::init(&DataPaths::new(root)));
    error!("{message}");
    drop(logging);
    std::process::exit(1);
}

/// A panicking worker takes the whole process with it (plan Task 25). Left to itself that thread
/// dies alone and leaves a daemon that still holds its tray icon and captures nothing; falling over
/// is visible, and autostart brings it back at the next logon.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("panicked");
        let thread = std::thread::current();
        error!(
            thread = thread.name().unwrap_or("unnamed"),
            location = info
                .location()
                .map(std::panic::Location::to_string)
                .unwrap_or_default(),
            "{message}"
        );
        std::thread::sleep(PANIC_FLUSH);
        std::process::exit(PANIC_EXIT);
    }));
}

/// Abort rather than warn when a monitor's change threshold cannot be reached.
fn check_thresholds(
    capture: &mut cw_capture::CaptureEngine,
    config: &Config,
    config_path: &std::path::Path,
) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 1..=MONITOR_ATTEMPTS {
        match capture.monitors() {
            // A session still coming up and an RDP reconnect both list no monitors, and every
            // monitor of an empty list passes every check.
            Ok(monitors) if monitors.is_empty() => {
                warn!(attempt, "listing monitors answered with no monitors");
                last_error = Some("the enumeration listed no monitors".to_owned());
                std::thread::sleep(MONITOR_RETRY);
            }
            Ok(monitors) => {
                let unreachable: Vec<_> = monitors
                    .iter()
                    .filter(|monitor| {
                        !cw_core::change::change_threshold_is_reachable(
                            monitor.width,
                            monitor.height,
                            monitor.dpi_scale,
                            &config.capture,
                        )
                    })
                    .map(|monitor| {
                        format!(
                            "{} ({}x{} at {}x scale) tops out at {:.0} changed logical pixels",
                            monitor.id,
                            monitor.width,
                            monitor.height,
                            monitor.dpi_scale,
                            cw_core::change::max_logical_pixels(
                                monitor.width,
                                monitor.height,
                                monitor.dpi_scale
                            )
                        )
                    })
                    .collect();
                if unreachable.is_empty() {
                    return Ok(());
                }
                return Err(format!(
                    "capture.change_area_logical_pixels is {}, which no pixel change on these \
                     monitors can exceed: {}. Lower it in {}",
                    config.capture.change_area_logical_pixels,
                    unreachable.join("; "),
                    config_path.display()
                ));
            }
            Err(error) => {
                warn!(attempt, "listing monitors failed: {error}");
                last_error = Some(error.to_string());
                std::thread::sleep(MONITOR_RETRY);
            }
        }
    }

    Err(format!(
        "the monitors could not be listed in {MONITOR_ATTEMPTS} attempts, so the change threshold \
         could not be checked against them: {}",
        last_error.unwrap_or_default()
    ))
}

fn open_for(subsystem: &str, paths: &DataPaths) -> rusqlite::Connection {
    cw_store::db::open(&paths.database()).unwrap_or_else(|error| {
        panic!("opening the {subsystem} database connection failed: {error}")
    })
}

fn spawn<F>(name: &str, worker: F)
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(worker)
        .unwrap_or_else(|error| panic!("spawning the {name} thread failed: {error}"));
}

/// One screen of what this installation has been doing. Nothing here initializes logging: this is
/// a question asked from a console, and the daemon's own log file is not where the answer goes.
///
/// Every line answers for itself, so one unreadable piece of state does not take the screen with
/// it — the reason to run this at all is usually that something is wrong.
fn status() -> Result<(), Failure> {
    let (_config, paths, conn) = open_store()?;

    field("data dir", &paths.root.display().to_string());
    field("capture", &pause_state(&conn));
    for (label, key) in [
        ("last tick", HealthKey::LastTick),
        ("last capture", HealthKey::LastCapture),
        ("last delivery", HealthKey::LastDelivery),
    ] {
        field(
            label,
            &match cw_store::control::get_health(&conn, key) {
                Ok(Some(at)) => moment(at),
                Ok(None) => "never".to_owned(),
                Err(error) => format!("unreadable: {error}"),
            },
        );
    }
    field(
        "outbox",
        &match cw_store::outbox::counts_by_state(&conn) {
            Ok(counts) if counts.is_empty() => "empty".to_owned(),
            Ok(counts) => counts
                .iter()
                .map(|(state, count)| format!("{state} {count}"))
                .collect::<Vec<_>>()
                .join(", "),
            Err(error) => format!("unreadable: {error}"),
        },
    );
    field(
        "episodes",
        &match conn.query_one("SELECT count(*) FROM episodes", [], |row| {
            row.get::<_, i64>(0)
        }) {
            Ok(count) => count.to_string(),
            Err(error) => format!("unreadable: {error}"),
        },
    );
    field(
        "error",
        &last_error_line(cw_store::outbox::newest_error(&conn)),
    );
    field(
        "hindsight",
        &match cw_sink_hindsight::Credentials::load() {
            Ok(Some(credentials)) => {
                match cw_sink_hindsight::parse_api_url(credentials.api_url()) {
                    Ok(_) => format!(
                        "delivering to {}",
                        cw_sink_hindsight::display_api_url(credentials.api_url())
                    ),
                    Err(error) => format!("configured URL is unusable: {error}"),
                }
            }
            Ok(None) => "not configured; run `contextwitness setup`".to_owned(),
            Err(error) => format!("unreadable: {error}"),
        },
    );
    field(
        "autostart",
        &match autostart::is_enabled() {
            Ok(true) => "starts at logon".to_owned(),
            Ok(false) => "does not start at logon".to_owned(),
            Err(error) => format!("unreadable: {error}"),
        },
    );

    Ok(())
}

/// Whether capture is allowed to run, as the recorded pause and this moment together say. Not
/// whether a daemon is up: the marks below are what answer that. `get_pause` consults no clock, so
/// an expired deadline is still recorded and is reported as the expired thing it is.
fn pause_state(conn: &rusqlite::Connection) -> String {
    match cw_store::control::get_pause(conn) {
        Ok(None) => "not paused".to_owned(),
        Ok(Some(Pause::Indefinite)) => "paused until resumed".to_owned(),
        Ok(Some(Pause::Until(deadline))) if chrono::Utc::now() < deadline => {
            format!("paused until {}", moment(deadline))
        }
        Ok(Some(Pause::Until(deadline))) => {
            format!(
                "not paused; the pause until {} has expired",
                moment(deadline)
            )
        }
        Err(error) => format!("unreadable: {error}"),
    }
}

fn pause(duration: Option<chrono::TimeDelta>) -> Result<(), Failure> {
    let (_config, _paths, mut conn) = open_store()?;
    let now = chrono::Utc::now();
    let pause = match duration {
        Some(duration) => Pause::Until(
            now.checked_add_signed(duration)
                .ok_or("that pause would end after the last instant this program can record")?,
        ),
        None => Pause::Indefinite,
    };

    cw_store::control::set_pause(&mut conn, pause, ulid::Ulid::generate(), now)?;
    match pause {
        Pause::Until(deadline) => println!("capture paused until {}.", moment(deadline)),
        Pause::Indefinite => println!("capture paused until `contextwitness resume`."),
    }

    Ok(())
}

fn resume() -> Result<(), Failure> {
    let (_config, _paths, mut conn) = open_store()?;

    cw_store::control::resume(&mut conn, ulid::Ulid::generate(), chrono::Utc::now())?;
    println!("capture resumed.");

    Ok(())
}

fn set_autostart(action: &AutostartAction) -> Result<(), Failure> {
    match action {
        AutostartAction::Enable => {
            autostart::enable()?;
            println!("ContextWitness will start at logon.");
        }
        AutostartAction::Disable => {
            autostart::disable()?;
            println!("ContextWitness will no longer start at logon.");
        }
    }

    Ok(())
}

/// Capture with nothing else running: no workers, no tray, no file log. What it stores it stores
/// exactly as a tick would, so this is also how one checks that capture works at all.
fn capture_once(wgc: bool) -> Result<(), Failure> {
    if !cw_capture::make_dpi_aware() {
        return Err(Failure::from(
            "the process could not become PER_MONITOR_AWARE_V2, so capture would be silently degraded",
        ));
    }
    cw_ocr::init_runtime();

    let (config, paths, mut conn) = open_store()?;
    let ocr = cw_ocr::WindowsOcr;
    let mut capture = cw_capture::CaptureEngine::new();
    if wgc {
        capture.force_fallback();
    }
    let mut previous = std::collections::HashMap::new();
    let mut save_failed = std::collections::HashMap::new();
    let mut stored = Vec::new();
    let mut failed = capture::pass(
        &mut capture,
        &ocr,
        &mut conn,
        &paths,
        &config,
        &mut previous,
        &mut save_failed,
        None,
        &mut stored,
    )
    .err();
    // WGC delivers each monitor's first frame from a callback thread on its own schedule, so a
    // pass can find sessions with nothing composed yet; they persist across passes, so ask again.
    let mut awaited: Option<Vec<String>> = None;
    if wgc && failed.is_none() {
        match capture.monitors() {
            Err(error) => failed = Some(error.into()),
            Ok(monitors) => {
                let expected: Vec<String> =
                    monitors.into_iter().map(|monitor| monitor.id).collect();
                let deadline = std::time::Instant::now() + WGC_ANSWER_DEADLINE;
                failed = await_answers(
                    &expected,
                    &mut stored,
                    || std::time::Instant::now() >= deadline,
                    |missing, stored| {
                        std::thread::sleep(WGC_ANSWER_RETRY);
                        capture::pass(
                            &mut capture,
                            &ocr,
                            &mut conn,
                            &paths,
                            &config,
                            &mut previous,
                            &mut save_failed,
                            (!expected.is_empty()).then_some(missing),
                            stored,
                        )
                        .err()
                    },
                );
                awaited = Some(expected);
            }
        }
    }

    let (lines, exit) = closing_report(&stored, save_failed, failed, awaited.as_deref());
    for line in lines {
        println!("{line}");
    }
    exit
}

/// What `capture-once` says after its passes, and what it exits with: the `--wgc` wait's
/// no-answer notice, every stored frame's line, then the save-failure summary — printed beside a
/// pass failure that owns the exit, the exit itself when the pass succeeded.
fn closing_report(
    stored: &[capture::Stored],
    save_failed: std::collections::HashMap<String, capture::SaveFailure>,
    failed: Option<Failure>,
    awaited: Option<&[String]>,
) -> (Vec<String>, Result<(), Failure>) {
    let mut lines = Vec::new();
    // Nothing after a pass failure: the abort, not the deadline, is why answers are missing.
    if let Some(expected) = awaited
        && failed.is_none()
    {
        let missing: Vec<&str> = expected
            .iter()
            .filter(|id| {
                !stored.iter().any(|frame| &frame.monitor_id == *id)
                    && !save_failed.contains_key(*id)
            })
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            lines.push(format!(
                "{} of {} monitors answered within {}s; nothing from {}",
                expected.len() - missing.len(),
                expected.len(),
                WGC_ANSWER_DEADLINE.as_secs(),
                missing.join(", ")
            ));
        }
    }
    if stored.is_empty() && save_failed.is_empty() && failed.is_none() {
        lines.push(
            "nothing was stored: the privacy gate refused this moment, or no monitor answered."
                .to_owned(),
        );
    }
    for frame in stored {
        lines.push(format!(
            "{} {}x{} ocr {:?} {} chars {}",
            frame.monitor_id,
            frame.width,
            frame.height,
            frame.ocr_status,
            frame.text_chars,
            frame.relative_path
        ));
    }
    if !save_failed.is_empty() {
        let mut failures: Vec<String> = save_failed
            .into_iter()
            .map(|(monitor_id, failure)| format!("{monitor_id}: {}", failure.message))
            .collect();
        failures.sort();
        let summary = format!("saving failed on {}", failures.join("; "));
        return match failed {
            Some(failed) => {
                lines.push(summary);
                (lines, Err(failed))
            }
            None => (lines, Err(summary.into())),
        };
    }
    match failed {
        Some(failed) => (lines, Err(failed)),
        None => (lines, Ok(())),
    }
}

/// Whether every monitor in `expected` has stored a frame. An empty `expected` is a machine with
/// no usable monitors — the only call sits in the arm where enumeration succeeded — and nothing
/// can be waited for, so it is answered as complete.
fn all_answered(stored: &[capture::Stored], expected: &[String]) -> bool {
    expected
        .iter()
        .all(|id| stored.iter().any(|frame| &frame.monitor_id == id))
}

/// The `--wgc` wait: keeps asking `pass_once` about the monitors that have not answered until
/// every one has, a pass fails, or `expired` says the deadline arrived. The wait only ever adds
/// to `stored` — what earlier passes landed is never given back.
fn await_answers(
    expected: &[String],
    stored: &mut Vec<capture::Stored>,
    mut expired: impl FnMut() -> bool,
    mut pass_once: impl FnMut(
        &std::collections::HashSet<String>,
        &mut Vec<capture::Stored>,
    ) -> Option<Failure>,
) -> Option<Failure> {
    let mut failed = None;
    while failed.is_none() && !all_answered(stored, expected) && !expired() {
        let missing: std::collections::HashSet<String> = expected
            .iter()
            .filter(|id| !stored.iter().any(|frame| &frame.monitor_id == *id))
            .cloned()
            .collect();
        failed = pass_once(&missing, stored);
    }
    failed
}

/// Ask for what this program cannot work out on its own, and write it down. Nothing here talks to
/// Hindsight: what the user types is taken as given, and delivery reports for itself.
fn setup() -> Result<(), Failure> {
    setup_credentials()?;
    println!();
    setup_data_dir()?;
    println!("\nRestart ContextWitness for any of this to take effect.");

    Ok(())
}

fn setup_credentials() -> Result<(), Failure> {
    println!("Hindsight delivery. An empty URL leaves the current credentials alone.");
    let api_url = ask("  API URL: ")?;
    if api_url.is_empty() {
        println!("  credentials unchanged.");
        return Ok(());
    }
    if let Err(error) = cw_sink_hindsight::parse_api_url(&api_url) {
        return Err(format!("the URL was not saved: {error}").into());
    }
    let token =
        ask_secret("  API token (replaces the stored one; empty when the server needs none): ")?;
    let token = (!token.is_empty()).then_some(token);

    let path = cw_sink_hindsight::Credentials::save(&api_url, token.as_deref())?;
    println!(
        "  wrote {}{}",
        path.display(),
        if token.is_some() { "" } else { " (no token)" }
    );

    Ok(())
}

fn setup_data_dir() -> Result<(), Failure> {
    let config_path = default_config_path()?;
    if Config::write_default_if_missing(&config_path)? {
        println!("Wrote the default config to {}.", config_path.display());
    }
    let current = Config::load_from_path(&config_path)?
        .storage
        .resolve_data_dir()?;

    println!(
        "Where captures and the database live. Empty keeps {}.",
        current.display()
    );
    let answer = ask("  data directory: ")?;
    if answer.is_empty() {
        println!("  data directory unchanged.");
        return Ok(());
    }

    let text = std::fs::read_to_string(&config_path)?;
    let Some(line) = data_dir_line(&text) else {
        println!(
            "  {} has no `{DATA_DIR_KEY} = \"...\"` line, so it was left untouched; add one under \
             [storage] by hand.",
            config_path.display()
        );
        return Ok(());
    };
    let mut updated = text;
    updated.replace_range(line, &format!("{DATA_DIR_KEY} = {}", toml_string(&answer)));
    let parsed = Config::from_toml_str(&updated).map_err(|error| {
        format!("the updated config would not parse, so it was not written: {error}")
    })?;
    parsed
        .storage
        .resolve_data_dir()
        .map_err(|error| format!("{error}; the config was not written"))?;
    // The line scan is lexical and can hit a `data_dir = ` line inside a multi-line string: the
    // replacement lands there, the file still parses, and the real key keeps its old value.
    if parsed.storage.data_dir != answer {
        return Err(format!(
            "the rewrite did not reach the [storage] data_dir key, so the config was not written; \
             set it by hand in {}",
            config_path.display()
        )
        .into());
    }

    // Renamed onto the config, never written into it: `fs::write` truncates first, and an
    // interruption leaves a config that parses as every default, collecting elsewhere in silence.
    let (temporary, mut file) = create_temporary_beside(&config_path)?;
    let written = file
        .write_all(updated.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|()| std::fs::rename(&temporary, &config_path)) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    println!("  set {DATA_DIR_KEY} in {}", config_path.display());

    Ok(())
}

/// Where the `data_dir` key sits in `text`, as the byte range of its line without the line ending.
/// The first one wins: `[storage]` is the only table carrying this key, and a config with two of
/// them is one the loader already refuses.
fn data_dir_line(text: &str) -> Option<std::ops::Range<usize>> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim_start();
        if let Some(rest) = body.strip_prefix(DATA_DIR_KEY)
            && rest.trim_start().starts_with('=')
        {
            let start = offset + (line.len() - body.len());
            return Some(start..offset + line.trim_end_matches(['\r', '\n']).len());
        }
        offset += line.len();
    }

    None
}

/// `value` as a TOML basic string. JSON's string escaping and TOML's agree on everything a Windows
/// path can hold, and a JSON encoder is already here.
fn toml_string(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

/// `30m`, `90m` or `2h`. A unit is required rather than assumed: a pause of the wrong length is
/// capture the user wanted and did not get, or capture they wanted stopped and did not.
fn parse_pause(text: &str) -> Result<chrono::TimeDelta, String> {
    let (digits, minutes_each) = if let Some(digits) = text.strip_suffix(['m', 'M']) {
        (digits, 1)
    } else if let Some(digits) = text.strip_suffix(['h', 'H']) {
        (digits, 60)
    } else {
        return Err(format!(
            "`{text}` carries no unit; write minutes as `30m` or hours as `2h`"
        ));
    };
    let count: i64 = digits
        .parse()
        .map_err(|_| format!("`{digits}` is not a whole number"))?;
    let minutes = count
        .checked_mul(minutes_each)
        .and_then(chrono::TimeDelta::try_minutes)
        .ok_or_else(|| format!("`{text}` is longer than this program can record"))?;
    if minutes <= chrono::TimeDelta::zero() {
        return Err(format!("`{text}` is a pause that is already over"));
    }

    Ok(minutes)
}

/// The config, the paths it resolves to, and a connection to the database under them. Missing
/// tables are not a case to handle: `db::open` creates the schema it does not find.
fn open_store() -> Result<(Config, DataPaths, rusqlite::Connection), Failure> {
    let config = Config::load_from_path(&default_config_path()?)?;
    let paths = DataPaths::new(config.storage.resolve_data_dir()?);
    let conn = cw_store::db::open(&paths.database())?;

    Ok((config, paths, conn))
}

fn field(label: &str, value: &str) {
    println!("{label:<STATUS_LABEL$}{value}");
}

/// The `status` screen's budget for the recorded message: enough for the HTTP status, the
/// operation id and the start of the server's words on one screen line or two — the log files
/// and the database keep the whole text.
const LAST_ERROR_DISPLAY_CHARS: usize = 200;

/// The delivery failure carried by the latest affected window — not necessarily the newest
/// failure recorded. The sink's messages name the HTTP status, the server's words and the
/// operation id, and this line carries their first [`LAST_ERROR_DISPLAY_CHARS`] characters.
fn last_error_line(
    newest: Result<Option<cw_store::outbox::NewestError>, cw_store::StoreError>,
) -> String {
    match newest {
        Ok(None) => "none recorded".to_owned(),
        Ok(Some(error)) => {
            let mut message: String = error
                .message
                .chars()
                .take(LAST_ERROR_DISPLAY_CHARS)
                .collect();
            if message.len() < error.message.len() {
                message.push('…');
            }
            let more = match error.entries - 1 {
                0 => String::new(),
                others => format!(" (+{others} more entries carry errors)"),
            };
            format!("[{} {}] {message}{more}", error.state, error.document_id)
        }
        Err(error) => format!("unreadable: {error}"),
    }
}

/// A recorded instant as the local wall clock, which is the one whoever reads this screen is
/// comparing it against.
fn moment(at: chrono::DateTime<chrono::Utc>) -> String {
    at.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// Prompt for one line, trimmed. Empty means the caller's "leave this alone".
fn ask(label: &str) -> std::io::Result<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;

    Ok(line.trim().to_owned())
}

/// Prompt for one line the console does not show. The newline is printed here because the Enter
/// that ended the line was not echoed either.
fn ask_secret(label: &str) -> std::io::Result<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    let read = {
        let _echo_off = EchoOff::new()?;
        std::io::stdin().read_line(&mut line)
    };
    println!();
    read?;

    Ok(line.trim().to_owned())
}

/// Console echo, off for as long as this lives. The mode goes back in a destructor rather than
/// after the read, because a read that fails leaves the console mute for everything that follows —
/// including the shell the user gets back. Ctrl+C ends the process without running destructors, so
/// [`restore_echo_on_ctrl_c`] covers that exit from the console's control thread.
struct EchoOff {
    handle: HANDLE,
    previous: CONSOLE_MODE,
}

/// The muted console's raw handle and previous mode. Written by [`EchoOff`] on the prompt thread,
/// read by [`restore_echo_on_ctrl_c`] on the control thread the console injects.
static MUTED: std::sync::Mutex<Option<(usize, u32)>> = std::sync::Mutex::new(None);

/// The restore [`EchoOff`]'s destructor cannot deliver on a Ctrl+C exit happens here instead.
/// Returning FALSE hands the event on, so the process still ends the default way.
unsafe extern "system" fn restore_echo_on_ctrl_c(_ctrl_type: u32) -> windows::core::BOOL {
    if let Ok(muted) = MUTED.lock()
        && let Some((handle, mode)) = *muted
    {
        let _ = unsafe { SetConsoleMode(HANDLE(handle as _), CONSOLE_MODE(mode)) };
    }
    windows::core::BOOL(0)
}

impl EchoOff {
    /// `None` when this program's input is not a console: a pipe or a file echoes nothing, and the
    /// mode being unavailable there is not a failure to read a secret from it.
    fn new() -> std::io::Result<Option<Self>> {
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.map_err(std::io::Error::other)?;
        let mut previous = CONSOLE_MODE::default();
        if unsafe { GetConsoleMode(handle, &mut previous) }.is_err() {
            return Ok(None);
        }
        // Refused rather than best-effort, like the mute below: echo off with no registered
        // restorer leaves the console mute after a Ctrl+C, the ordinary way out of a token
        // prompt. Registering twice would only make the restore run twice.
        unsafe { SetConsoleCtrlHandler(Some(restore_echo_on_ctrl_c), true) }
            .map_err(std::io::Error::other)?;
        // The record and the mute share one guard, so the handler — which must take the lock —
        // can never run between them and "restore" a mode that is yet to be muted. A lock this
        // thread cannot take means a restore nobody could perform, so the prompt is refused,
        // not muted unprotected.
        let mut muted = MUTED
            .lock()
            .map_err(|_| std::io::Error::other("the echo-restore record is unusable"))?;
        *muted = Some((handle.0 as usize, previous.0));
        // Refused rather than reported: this failing on something that *is* a console means the
        // next thing typed would be echoed, and that thing is the token.
        if let Err(error) = unsafe { SetConsoleMode(handle, previous & !ENABLE_ECHO_INPUT) } {
            *muted = None;
            return Err(std::io::Error::other(error));
        }

        Ok(Some(Self { handle, previous }))
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        let _ = unsafe { SetConsoleMode(self.handle, self.previous) };
        if let Ok(mut muted) = MUTED.lock() {
            *muted = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{await_answers, closing_report, last_error_line};
    use crate::capture::{SaveFailure, Stored};
    use cw_store::outbox::NewestError;

    #[test]
    fn the_last_error_line_names_the_failure_and_the_backlog() {
        assert_eq!(last_error_line(Ok(None)), "none recorded");
        let newest = NewestError {
            document_id: "screen-2026-08-01T12:00:00Z-5m".to_owned(),
            state: "failed".to_owned(),
            message: "hindsight retain failed with HTTP 422; the server said: no".to_owned(),
            entries: 1,
        };
        assert_eq!(
            last_error_line(Ok(Some(newest.clone()))),
            "[failed screen-2026-08-01T12:00:00Z-5m] hindsight retain failed with HTTP 422; \
             the server said: no"
        );
        let crowded = NewestError {
            entries: 3,
            ..newest
        };
        assert!(
            last_error_line(Ok(Some(crowded))).ends_with("(+2 more entries carry errors)"),
            "the backlog behind the latest affected window's error must be visible"
        );

        let flooded = NewestError {
            document_id: "screen-2026-08-01T12:00:00Z-5m".to_owned(),
            state: "failed".to_owned(),
            message: format!(
                "hindsight retain failed with HTTP 422; the server said: {}",
                "z".repeat(4000)
            ),
            entries: 3,
        };
        let line = last_error_line(Ok(Some(flooded)));
        assert!(
            line.len() < 300,
            "the status screen must show a bounded slice, the logs keep the rest: {} bytes",
            line.len()
        );
        assert!(line.contains('…'), "a cut must be visible: {line}");
        assert!(
            line.ends_with("(+2 more entries carry errors)"),
            "the backlog count must survive the cut: {line}"
        );
    }

    fn stored_frame(monitor_id: &str) -> Stored {
        Stored {
            monitor_id: monitor_id.to_owned(),
            width: 1,
            height: 1,
            ocr_status: cw_core::model::OcrStatus::NoText,
            text_chars: 0,
            relative_path: String::new(),
        }
    }

    #[test]
    fn a_pass_failure_exits_only_after_the_stored_and_save_failure_lines() {
        let refused = || {
            std::collections::HashMap::from([(
                "b".to_owned(),
                SaveFailure {
                    key: "encode: too wide".to_owned(),
                    message: "too wide".to_owned(),
                },
            )])
        };

        let (lines, exit) = closing_report(
            &[stored_frame("a")],
            refused(),
            Some("enumeration refused".into()),
            None,
        );
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("a "));
        assert_eq!(lines[1], "saving failed on b: too wide");
        assert_eq!(exit.unwrap_err().to_string(), "enumeration refused");

        let (lines, exit) = closing_report(&[stored_frame("a")], refused(), None, None);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            exit.unwrap_err().to_string(),
            "saving failed on b: too wide"
        );
    }

    #[test]
    fn the_no_answer_notice_and_the_nothing_line_yield_to_a_pass_failure() {
        let expected = ["a".to_owned(), "b".to_owned()];

        let (lines, exit) = closing_report(
            &[stored_frame("a")],
            std::collections::HashMap::new(),
            None,
            Some(&expected),
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "1 of 2 monitors answered within 8s; nothing from b"
        );
        assert!(lines[1].starts_with("a "));
        assert!(exit.is_ok());

        let (lines, exit) = closing_report(
            &[stored_frame("a")],
            std::collections::HashMap::new(),
            Some("refused".into()),
            Some(&expected),
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("a "));
        assert_eq!(exit.unwrap_err().to_string(), "refused");

        let (lines, exit) = closing_report(
            &[],
            std::collections::HashMap::new(),
            Some("refused".into()),
            None,
        );
        assert!(lines.is_empty());
        assert_eq!(exit.unwrap_err().to_string(), "refused");
    }

    #[test]
    fn the_wgc_wait_only_adds_and_a_failing_pass_ends_it() {
        let expected = ["a".to_owned(), "b".to_owned()];

        let mut stored = vec![stored_frame("a")];
        let mut asked = Vec::new();
        let failed = await_answers(
            &expected,
            &mut stored,
            || false,
            |missing, stored| {
                let mut ids: Vec<String> = missing.iter().cloned().collect();
                ids.sort();
                asked.push(ids);
                stored.push(stored_frame("b"));
                None
            },
        );
        assert!(failed.is_none());
        assert_eq!(stored.len(), 2);
        assert_eq!(asked, [["b"]]);

        let mut stored = Vec::new();
        let mut ticks = 0;
        let mut passes = 0;
        let failed = await_answers(
            &expected,
            &mut stored,
            || {
                ticks += 1;
                ticks > 3
            },
            |_missing, stored| {
                passes += 1;
                stored.push(stored_frame("a"));
                Some("refused".into())
            },
        );
        assert_eq!(failed.unwrap().to_string(), "refused");
        assert_eq!(passes, 1);
        assert_eq!(stored.len(), 1);
    }
}
