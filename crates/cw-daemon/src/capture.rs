// The capture tick: pause, privacy gate, change detection, OCR, store.

use std::collections::HashMap;

use cw_core::change::{Thumbnail, frame_changed};
use cw_core::config::{Config, DataPaths};
use cw_core::model::{Observation, OcrStatus, ScreenPayload, SourcePayload};
use cw_core::privacy::CaptureDecision;
use cw_store::control::{ControlEvent, EventKind, HealthKey};
use tracing::{debug, error, info};

/// `last_tick_at` is written at most this often (design §7): the tick runs every couple of
/// seconds and the value it writes is only read by `status`.
const HEALTH_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Run the capture loop on this thread until the process ends.
pub fn run(
    mut capture: cw_capture::CaptureEngine,
    ocr: &dyn cw_ocr::OcrEngine,
    mut conn: rusqlite::Connection,
    paths: DataPaths,
    config: Config,
) -> ! {
    let interval = std::time::Duration::from_secs(config.capture.interval_secs);
    // The capture sessions are persistent, so the engine outlives the tick that reads from it.
    let mut previous: HashMap<String, Thumbnail> = HashMap::new();
    let mut health_written: Option<std::time::Instant> = None;
    let mut threshold_warned: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        let started = std::time::Instant::now();
        if let Err(error) = tick(
            &mut capture,
            ocr,
            &mut conn,
            &paths,
            &config,
            &mut previous,
            &mut health_written,
            &mut threshold_warned,
        ) {
            // A tick can die before it judged the pause or the privacy gate — `mark_tick` and
            // `get_pause` both talk to the store — and the fallback's callbacks kept writing
            // frames the whole time. Failing closed costs at most one frame on an errored tick;
            // failing open shows a screen the gate may have been refusing.
            capture.discard_pending();
            error!("tick failed: {error}");
        }
        // One tick at a time, and an overrun coalesces into a single immediate rerun rather than a
        // backlog (design §7): this loop is sequential, so the only thing to get right is that a
        // tick which outran the interval does not then sleep a whole one on top.
        if let Some(remaining) = interval.checked_sub(started.elapsed()) {
            std::thread::sleep(remaining);
        }
    }
}

/// One frame a pass stored, as the daemon's log line and `capture-once`'s printed line describe
/// it. The recognized text is deliberately not here: neither destination is a place screen content
/// goes, and only its length is reported.
pub(crate) struct Stored {
    pub monitor_id: String,
    pub width: u32,
    pub height: u32,
    pub ocr_status: OcrStatus,
    pub text_chars: usize,
    /// Where the image landed, relative to the images directory.
    pub relative_path: String,
}

#[allow(clippy::too_many_arguments)]
fn tick(
    capture: &mut cw_capture::CaptureEngine,
    ocr: &dyn cw_ocr::OcrEngine,
    conn: &mut rusqlite::Connection,
    paths: &DataPaths,
    config: &Config,
    previous: &mut HashMap<String, Thumbnail>,
    health_written: &mut Option<std::time::Instant>,
    threshold_warned: &mut std::collections::HashSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    mark_tick(conn, health_written)?;

    if let Some(pause) = cw_store::control::get_pause(conn)? {
        let paused = match pause {
            cw_store::control::Pause::Indefinite => true,
            cw_store::control::Pause::Until(deadline) => chrono::Utc::now() < deadline,
        };
        if paused {
            // The fallback's callback threads keep writing frames while the pass is not reading;
            // without this, the first tick after the pause could store a screen from the middle
            // of it. (A frame from the gap between the last paused tick and the actual lift is
            // still accepted — the ceiling is one tick interval.)
            capture.discard_pending();
            debug!("capture is paused");
            return Ok(());
        }
    }

    // An unplugged monitor would otherwise keep its thumbnail for the life of the process. Retained
    // against the monitors that exist, not the ones `capture_all` answered with: that call omits
    // any monitor with no new frame, and dropping those would throw away the baseline that
    // sub-threshold changes accumulate against. An enumeration that fails leaves the map alone.
    if let Ok(monitors) = capture.monitors() {
        previous.retain(|id, _| monitors.iter().any(|monitor| &monitor.id == id));
        // Startup aborted on this; a monitor plugged in since then gets an error instead, because
        // ending the daemon here would trade one silent monitor for silence on all of them. Once
        // per monitor: unplugging it clears the entry, so plugging it back in — possibly at a new
        // resolution — is judged afresh.
        threshold_warned.retain(|id| monitors.iter().any(|monitor| &monitor.id == id));
        for monitor in &monitors {
            if !cw_core::change::change_threshold_is_reachable(
                monitor.width,
                monitor.height,
                monitor.dpi_scale,
                &config.capture,
            ) && threshold_warned.insert(monitor.id.clone())
            {
                error!(
                    monitor = %monitor.id,
                    "this monitor ({}x{} at {}x scale) tops out at {:.0} changed logical pixels, \
                     under capture.change_area_logical_pixels: it will store nothing after its \
                     first frame until the threshold is lowered",
                    monitor.width,
                    monitor.height,
                    monitor.dpi_scale,
                    cw_core::change::max_logical_pixels(
                        monitor.width,
                        monitor.height,
                        monitor.dpi_scale
                    )
                );
            }
        }
    }

    for stored in pass(capture, ocr, conn, paths, config, previous, None)? {
        info!(
            monitor = %stored.monitor_id,
            width = stored.width,
            height = stored.height,
            ocr = ?stored.ocr_status,
            chars = stored.text_chars,
            path = %stored.relative_path,
            "stored frame"
        );
    }

    Ok(())
}

/// Capture every monitor once: the privacy gate, change detection against `previous`, OCR, and the
/// observation and image rows for whatever changed. The pause is not consulted here — `tick` owns
/// that, and `capture-once` is a user asking for this pass in particular. `only` narrows the pass
/// to those monitor ids: `capture-once`'s retries ask again about the monitors that have not
/// answered, and a monitor that already has must not gain a second frame from the same invocation.
pub(crate) fn pass(
    capture: &mut cw_capture::CaptureEngine,
    ocr: &dyn cw_ocr::OcrEngine,
    conn: &mut rusqlite::Connection,
    paths: &DataPaths,
    config: &Config,
    previous: &mut HashMap<String, Thumbnail>,
    only: Option<&std::collections::HashSet<String>>,
) -> Result<Vec<Stored>, Box<dyn std::error::Error>> {
    let foreground = cw_capture::foreground();
    let skip_detail = match cw_core::privacy::decide_capture(
        foreground.process.as_deref(),
        &config.privacy.process_blacklist,
    ) {
        CaptureDecision::Capture => None,
        // The process is the whole detail: a window title would put into the audit trail the very
        // thing the blacklist exists to keep out.
        CaptureDecision::SkipBlacklisted { process } => Some(process),
        CaptureDecision::SkipUnknownForeground => Some("unknown foreground process".to_owned()),
    };
    if let Some(detail) = skip_detail {
        // Same contract as the pause: a frame the fallback captured while the gate was closed
        // must not survive into the first allowed pass. Before the audit write, because the
        // discard cannot fail and the write can — an audit that answers SQLITE_BUSY must not
        // leave the gated frames waiting for a tick the gate no longer refuses.
        capture.discard_pending();
        cw_store::control::record_event(
            conn,
            &ControlEvent {
                id: ulid::Ulid::generate(),
                kind: EventKind::BlacklistSkip,
                at: chrono::Utc::now(),
                detail: Some(detail),
            },
        )?;
        // Without the process name: the audit trail is where that belongs, and the log is a file
        // this program keeps screen-derived names out of.
        debug!("tick skipped by the privacy gate");
        return Ok(Vec::new());
    }

    let mut changed = Vec::new();
    for frame in capture.capture_all() {
        // Skipped before change detection, so an unwanted monitor's baseline is not advanced by a
        // pass that was never going to store it.
        if only.is_some_and(|wanted| !wanted.contains(&frame.monitor_id)) {
            continue;
        }
        let rgba = bgra_to_rgba(&frame.bgra);
        let thumbnail = Thumbnail::from_rgba(&rgba, frame.width, frame.height, frame.dpi_scale)?;
        if frame_changed(previous.get(&frame.monitor_id), &thumbnail, &config.capture) {
            changed.push((frame, thumbnail));
        }
    }

    // One thread per changed monitor: OCR costs seconds per frame, and run in sequence it is the
    // whole tick. `recognize` initializes the Windows Runtime on whichever thread calls it.
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (frame, _) in &changed {
            handles.push(scope.spawn(move || {
                ocr.recognize(
                    &frame.bgra,
                    frame.width,
                    frame.height,
                    &config.ocr.languages,
                )
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("recognize does not panic"))
            .collect()
    });

    let mut stored_at = None;
    let mut stored_frames = Vec::new();
    for ((frame, thumbnail), ocr) in changed.into_iter().zip(outcomes) {
        // The backend's stamp, not now: OCR just spent seconds, and an observation dated after it
        // lands whole seconds late — far enough to put a frame in the wrong five-minute window.
        let captured_at = frame.captured_at;
        let text_chars = ocr.text.as_deref().map_or(0, |text| text.chars().count());
        let payload = ScreenPayload {
            monitor_id: frame.monitor_id.clone(),
            width: frame.width,
            height: frame.height,
            image_path: None,
            ocr_status: ocr.status.clone(),
            ocr_error: ocr.error,
            ocr_text: ocr.text,
            ocr_langs: ocr.langs,
            foreground_process: foreground.process.clone(),
            foreground_window_title: foreground.title.clone(),
        };
        let mut observation = Observation::new_screen(payload, captured_at);
        // This id and instant are the ones `save_with_observation` gets below, which is what makes
        // the path recorded here the path it writes. The `images/` prefix is the payload's spelling
        // only: delivered paths are data_dir-relative (design §4.2), while the images table keys
        // on the path relative to the images root.
        if let SourcePayload::Screen(payload) = &mut observation.payload {
            payload.image_path = Some(format!(
                "images/{}",
                cw_store::images::relative_path(observation.id, captured_at)
            ));
        }
        let rgb = bgra_to_rgb(&frame.bgra);
        // One transaction for the observation row and the image row: committed apart, a crash
        // between them would permanently leave an observation advertising a path no file will
        // ever answer to — the startup sweep reconciles files and image rows, not observations.
        let stored = cw_store::images::save_with_observation(
            conn,
            &paths.images(),
            &observation,
            &rgb,
            frame.width,
            frame.height,
            f32::from(config.capture.webp_quality),
            captured_at,
        )?;
        previous.insert(frame.monitor_id.clone(), thumbnail);
        stored_at = Some(captured_at);
        stored_frames.push(Stored {
            monitor_id: frame.monitor_id,
            width: frame.width,
            height: frame.height,
            ocr_status: ocr.status,
            text_chars,
            relative_path: stored,
        });
    }
    // Once per tick rather than once per monitor: all three writes would carry the same tick and
    // `status` reads one value.
    if let Some(at) = stored_at {
        cw_store::control::set_health(conn, HealthKey::LastCapture, at)?;
    }

    Ok(stored_frames)
}

fn mark_tick(
    conn: &rusqlite::Connection,
    health_written: &mut Option<std::time::Instant>,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = std::time::Instant::now();
    if health_written.is_some_and(|last| now.duration_since(last) < HEALTH_TICK_INTERVAL) {
        return Ok(());
    }
    cw_store::control::set_health(conn, HealthKey::LastTick, chrono::Utc::now())?;
    *health_written = Some(now);

    Ok(())
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = bgra.to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    rgba
}

fn bgra_to_rgb(bgra: &[u8]) -> Vec<u8> {
    bgra.chunks_exact(4)
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
        .collect()
}
