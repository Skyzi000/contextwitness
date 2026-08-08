use std::collections::HashMap;

use cw_core::change::{Thumbnail, frame_changed};
use cw_core::config::{Config, DataPaths};
use cw_core::model::{Observation, OcrStatus, ScreenPayload, SourcePayload};
use cw_core::privacy::CaptureDecision;
use cw_store::control::{ControlEvent, EventKind, HealthKey};
use tracing::{debug, error, info};

/// `last_tick_at` is written at most this often (design §7): a stale mark only ever holds
/// episode closure back, never moves it ahead.
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
    let mut previous: HashMap<String, Thumbnail> = HashMap::new();
    let mut health_written: Option<std::time::Instant> = None;
    let mut threshold_warned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut save_failed: HashMap<String, String> = HashMap::new();

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
            &mut save_failed,
        ) {
            // Fail closed: the tick may have died before it judged the pause or the privacy gate,
            // and failing open shows a screen the gate may have been refusing.
            capture.discard_pending();
            error!("tick failed: {error}");
        }
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
    save_failed: &mut HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = chrono::Utc::now();

    if let Some(pause) = cw_store::control::get_pause(conn)? {
        let paused = match pause {
            cw_store::control::Pause::Indefinite => true,
            cw_store::control::Pause::Until(deadline) => chrono::Utc::now() < deadline,
        };
        if paused {
            capture.discard_pending();
            mark_tick(conn, health_written, started)?;
            debug!("capture is paused");
            return Ok(());
        }
    }

    if let Ok(monitors) = capture.monitors() {
        previous.retain(|id, _| monitors.iter().any(|monitor| &monitor.id == id));
        threshold_warned.retain(|id| monitors.iter().any(|monitor| &monitor.id == id));
        save_failed.retain(|id, _| monitors.iter().any(|monitor| &monitor.id == id));
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

    for stored in pass(
        capture,
        ocr,
        conn,
        paths,
        config,
        previous,
        save_failed,
        None,
    )? {
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

    // When this mark becomes readable, every shot the pass pulled has been stored or refused. An
    // error return skips the mark, which stalls the closer — the safe direction.
    mark_tick(conn, health_written, started)?;

    Ok(())
}

/// Capture every monitor once: the privacy gate, change detection against `previous`, OCR, and the
/// observation and image rows for whatever changed. The pause is not consulted here — `tick` owns
/// that, and `capture-once` is a user asking for this pass in particular. `only` narrows the pass
/// to those monitor ids: `capture-once`'s retries ask again about the monitors that have not
/// answered, and a monitor that already has must not gain a second frame from the same invocation.
/// `save_failed` holds the last save error reported per monitor, so a failure that repeats is only
/// news the first time.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pass(
    capture: &mut cw_capture::CaptureEngine,
    ocr: &dyn cw_ocr::OcrEngine,
    conn: &mut rusqlite::Connection,
    paths: &DataPaths,
    config: &Config,
    previous: &mut HashMap<String, Thumbnail>,
    save_failed: &mut HashMap<String, String>,
    only: Option<&std::collections::HashSet<String>>,
) -> Result<Vec<Stored>, Box<dyn std::error::Error>> {
    let foreground = cw_capture::foreground();
    let skip_detail = match cw_core::privacy::decide_capture(
        foreground.process.as_deref(),
        &config.privacy.process_blacklist,
    ) {
        CaptureDecision::Capture => None,
        CaptureDecision::SkipBlacklisted { process } => Some(process),
        CaptureDecision::SkipUnknownForeground => Some("unknown foreground process".to_owned()),
    };
    if let Some(detail) = skip_detail {
        // Before the audit write: the discard cannot fail and the write can.
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
        // Without the process name: the audit trail is where that belongs, not the log.
        debug!("tick skipped by the privacy gate");
        return Ok(Vec::new());
    }

    let mut changed = Vec::new();
    for frame in capture.capture_all() {
        if only.is_some_and(|wanted| !wanted.contains(&frame.monitor_id)) {
            continue;
        }
        let rgba = bgra_to_rgba(&frame.bgra);
        let thumbnail = Thumbnail::from_rgba(&rgba, frame.width, frame.height, frame.dpi_scale)?;
        if frame_changed(previous.get(&frame.monitor_id), &thumbnail, &config.capture) {
            changed.push((frame, thumbnail));
        }
    }

    // `recognize` initializes the Windows Runtime on whichever thread calls it.
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
        // The `images/` prefix is the payload's spelling only: delivered paths are
        // data_dir-relative, the images table keys on the path relative to the images root.
        if let SourcePayload::Screen(payload) = &mut observation.payload {
            payload.image_path = Some(format!(
                "images/{}",
                cw_store::images::relative_path(observation.id, captured_at)
            ));
        }
        let rgb = bgra_to_rgb(&frame.bgra);
        let stored = match cw_store::images::save_with_observation(
            conn,
            &paths.images(),
            &observation,
            &rgb,
            frame.width,
            frame.height,
            f32::from(config.capture.webp_quality),
            captured_at,
        ) {
            Ok(stored) => {
                save_failed.remove(&frame.monitor_id);
                stored
            }
            // Isolated to the one monitor rather than ending the pass: a frame wider than WebP's
            // 16,383px limit would starve every monitor behind it in the enumeration.
            Err(error) => {
                let message = error.to_string();
                if save_failed.get(&frame.monitor_id) != Some(&message) {
                    error!(monitor = %frame.monitor_id, "failed to store frame: {message}");
                    save_failed.insert(frame.monitor_id.clone(), message);
                }
                continue;
            }
        };
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
    if let Some(at) = stored_at {
        cw_store::control::set_health(conn, HealthKey::LastCapture, at)?;
    }

    Ok(stored_frames)
}

fn mark_tick(
    conn: &rusqlite::Connection,
    health_written: &mut Option<std::time::Instant>,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = std::time::Instant::now();
    if health_written.is_some_and(|last| now.duration_since(last) < HEALTH_TICK_INTERVAL) {
        return Ok(());
    }
    cw_store::control::set_health(conn, HealthKey::LastTick, at)?;
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
