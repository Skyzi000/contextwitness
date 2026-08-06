// The command-line surface: what every subcommand does, including starting the daemon itself.

use std::io::Write as _;

use crate::{autostart, capture, delivery, episodes, logging, maintenance, tray};
use clap::{Parser, Subcommand};
use cw_core::config::{Config, DataPaths, default_config_path};
use cw_store::control::{HealthKey, Pause};
use tracing::{error, info, warn};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
};
use windows::Win32::System::Threading::CreateMutexW;

/// How long startup keeps asking for the monitor list before giving up. Enumeration fails while a
/// session is still coming up, and the reachability check below cannot be skipped, so this waits
/// for an answer instead of starting without one.
const MONITOR_ATTEMPTS: u32 = 5;
const MONITOR_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// The key `setup` rewrites, and the only place the data directory is configured.
const DATA_DIR_KEY: &str = "data_dir";

/// Width the status screen's labels are padded to, so its values line up in one column.
const STATUS_LABEL: usize = 14;

/// The name `run` claims for as long as it runs. `Local\` is the session namespace on purpose: one
/// collector per interactive session is the line worth drawing. A daemon in another session writes
/// to that user's own data directory, and with WAL under it a second one is wasteful rather than
/// corrupting.
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
    /// Capture, read and store one frame per monitor, then exit.
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
    // Before the engine, the log file and the database: a refused second daemon must not have
    // touched any of them.
    claim_single_instance();
    // Asked for here, before any thread exists; judged below, once there is a log file to put the
    // reason in.
    let dpi_aware = cw_capture::make_dpi_aware();
    cw_ocr::init_runtime();

    let config_path = default_config_path().expect("resolving the config path failed");
    let created =
        Config::write_default_if_missing(&config_path).expect("writing the default config failed");
    let config = Config::load_from_path(&config_path).expect("loading the config failed");
    let data_dir = config
        .storage
        .resolve_data_dir()
        .expect("resolving the data directory failed");
    let paths = DataPaths::new(data_dir);
    // Held until the process ends: dropping it flushes the file writer.
    let logging = logging::init(&paths);
    install_panic_hook();
    if created {
        info!(path = %config_path.display(), "wrote the default config");
    }

    // Without PER_MONITOR_AWARE_V2 every capture arrives at a virtualized resolution and every
    // DPI reads 96, silently — the change threshold then measures the wrong pixels. The design
    // (§3.2) takes a visible refusal over a daemon that degrades without saying so.
    if !dpi_aware {
        error!(
            "the process could not become PER_MONITOR_AWARE_V2, and capture would be silently degraded"
        );
        drop(logging);
        std::process::exit(1);
    }

    // The capture sessions are persistent, so the engine outlives the tick that reads from it.
    let mut capture = cw_capture::CaptureEngine::new();
    if let Err(message) = check_thresholds(&mut capture, &config, &config_path) {
        error!("{message}");
        // Before the exit, which runs no destructor: without this the reason above never reaches
        // the log file.
        drop(logging);
        std::process::exit(1);
    }

    let mut conn = cw_store::db::open(&paths.database()).expect("opening the database failed");
    maintenance::sweep_orphans(&conn, &paths);
    match cw_store::outbox::requeue_delivering(&conn) {
        Ok(0) => {}
        // An attempt whose outcome nobody recorded: delivery is at-least-once, so it goes round
        // again under the same document id.
        Ok(requeued) => info!(requeued, "requeued deliveries left in flight"),
        Err(error) => error!("requeueing in-flight deliveries failed: {error}"),
    }
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

    // One connection per subsystem (design §7); each carries the same WAL and busy-timeout
    // contract because every one of them comes from `db::open`.
    spawn("delivery", {
        let conn = open_for("delivery", &paths);
        let config = config.clone();
        move || delivery::run(conn, config)
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
    // Held for the life of the process, and dropping this binding does nothing: the claim ends
    // with the process.
    let _held = unsafe { CreateMutexW(None, false, &name) };
    // A call that failed for any other reason leaves the daemon unguarded rather than refusing to
    // start: it says nothing about whether another one is up.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        eprintln!("contextwitness is already running in this session.");
        std::process::exit(1);
    }
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

/// Abort rather than warn when a monitor's change threshold cannot be reached (plan Task 13): the
/// comparison in `frame_changed` is strict, so such a monitor stores nothing after its first frame,
/// with no error anywhere — a silence the user cannot tell from working.
fn check_thresholds(
    capture: &mut cw_capture::CaptureEngine,
    config: &Config,
    config_path: &std::path::Path,
) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 1..=MONITOR_ATTEMPTS {
        match capture.monitors() {
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
                    "capture.change_area_logical_pixels is {}, which no change on these monitors \
                     can exceed, so nothing on them would ever be stored: {}. Lower it in {}",
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
    // Counted here rather than through the store, which has no count of its own and no other
    // caller that wants one.
    field(
        "episodes",
        &match conn.query_one("SELECT count(*) FROM episodes", [], |row| {
            row.get::<_, i64>(0)
        }) {
            Ok(count) => count.to_string(),
            Err(error) => format!("unreadable: {error}"),
        },
    );
    // The URL only, ever: the token is the one thing on this screen that must not be readable
    // over a shoulder.
    field(
        "hindsight",
        &match cw_sink_hindsight::Credentials::load() {
            Ok(Some(credentials)) => format!("delivering to {}", credentials.api_url()),
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

    // This records the audit row itself, in the same transaction as the state.
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

/// One capture pass and nothing else: no workers, no tray, no file log. What it stores it stores
/// exactly as a tick would, so this is also how one checks that capture works at all.
fn capture_once(wgc: bool) -> Result<(), Failure> {
    // The same refusal as the daemon's: a diagnostic that measures a virtualized screen would
    // report the wrong resolution as if capture worked.
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
    // Empty, so every monitor counts as changed: a single pass has nothing to compare against, and
    // asking for one means asking for what is on screen now.
    let mut previous = std::collections::HashMap::new();
    let mut stored = capture::pass(
        &mut capture,
        &ocr,
        &mut conn,
        &paths,
        &config,
        &mut previous,
    )?;
    // WGC sessions deliver their first frame from a callback thread, so the first pass can find
    // the mailboxes still empty. The sessions persist across passes; ask again briefly.
    if wgc {
        for _ in 0..6 {
            if !stored.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            stored.extend(capture::pass(
                &mut capture,
                &ocr,
                &mut conn,
                &paths,
                &config,
                &mut previous,
            )?);
        }
    }

    if stored.is_empty() {
        println!(
            "nothing was stored: the privacy gate refused this moment, or no monitor answered."
        );
    }
    for frame in stored {
        println!(
            "{} {}x{} ocr {:?} {} chars {}",
            frame.monitor_id,
            frame.width,
            frame.height,
            frame.ocr_status,
            frame.text_chars,
            frame.relative_path
        );
    }

    Ok(())
}

/// Ask for what this program cannot work out on its own, and write it down. Nothing here talks to
/// Hindsight: what the user types is taken as given, and delivery reports for itself.
fn setup() -> Result<(), Failure> {
    setup_credentials()?;
    println!();
    setup_data_dir()?;
    // Both are read once, at startup.
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
    let token = ask_secret("  API token: ")?;
    if token.is_empty() {
        // The URL alone would leave the file incomplete, which delivery reports as a broken
        // configuration rather than an absent one — worse than the state this started in.
        return Err("the token is required alongside the URL".into());
    }

    let path = cw_sink_hindsight::Credentials::save(&api_url, &token)?;
    println!("  wrote {}", path.display());

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
    // Checked before it is written, because the escaping above is the only thing between a Windows
    // path and a config the daemon then refuses to load.
    Config::from_toml_str(&updated).map_err(|error| {
        format!("the updated config would not parse, so it was not written: {error}")
    })?;
    std::fs::write(&config_path, updated)?;
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
/// including the shell the user gets back.
struct EchoOff {
    handle: HANDLE,
    previous: CONSOLE_MODE,
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
        // Refused rather than reported: this failing on something that *is* a console means the
        // next thing typed would be echoed, and that thing is the token.
        unsafe { SetConsoleMode(handle, previous & !ENABLE_ECHO_INPUT) }
            .map_err(std::io::Error::other)?;

        Ok(Some(Self { handle, previous }))
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // Nothing to do about a failure here: the console is the only place to report it, and it
        // is the thing that just did not answer.
        let _ = unsafe { SetConsoleMode(self.handle, self.previous) };
    }
}
