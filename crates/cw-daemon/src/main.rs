#![deny(unsafe_op_in_unsafe_fn)]
//! The ContextWitness daemon entry point.

mod capture;
mod delivery;
mod episodes;
mod logging;
mod maintenance;

use cw_core::config::{Config, DataPaths, default_config_path};
use tracing::{error, info, warn};

/// How long startup keeps asking for the monitor list before giving up. Enumeration fails while a
/// session is still coming up, and the reachability check below cannot be skipped, so this waits
/// for an answer instead of starting without one.
const MONITOR_ATTEMPTS: u32 = 5;
const MONITOR_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

fn main() {
    cw_capture::make_dpi_aware();
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
    if created {
        info!(path = %config_path.display(), "wrote the default config");
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

    info!(
        interval_secs = config.capture.interval_secs,
        database = %paths.database().display(),
        "capturing"
    );
    capture::run(capture, conn, paths, config);
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
