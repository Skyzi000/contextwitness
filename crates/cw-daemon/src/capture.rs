// The capture tick: pause, privacy gate, change detection, OCR, store.

use std::collections::HashMap;

use cw_core::change::{Thumbnail, frame_changed};
use cw_core::config::{Config, DataPaths};
use cw_core::model::{Observation, ScreenPayload, SourcePayload};
use cw_core::privacy::CaptureDecision;
use cw_store::control::{ControlEvent, EventKind, HealthKey};
use tracing::{debug, error, info};

/// `last_tick_at` is written at most this often (design §7): the tick runs every couple of
/// seconds and the value it writes is only read by `status`.
const HEALTH_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Run the capture loop on this thread until the process ends.
pub fn run(
    mut capture: cw_capture::CaptureEngine,
    mut conn: rusqlite::Connection,
    paths: DataPaths,
    config: Config,
) -> ! {
    let interval = std::time::Duration::from_secs(config.capture.interval_secs);
    // The capture sessions are persistent, so the engine outlives the tick that reads from it.
    let mut previous: HashMap<String, Thumbnail> = HashMap::new();
    let mut health_written: Option<std::time::Instant> = None;

    loop {
        let started = std::time::Instant::now();
        if let Err(error) = tick(
            &mut capture,
            &mut conn,
            &paths,
            &config,
            &mut previous,
            &mut health_written,
        ) {
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

fn tick(
    capture: &mut cw_capture::CaptureEngine,
    conn: &mut rusqlite::Connection,
    paths: &DataPaths,
    config: &Config,
    previous: &mut HashMap<String, Thumbnail>,
    health_written: &mut Option<std::time::Instant>,
) -> Result<(), Box<dyn std::error::Error>> {
    mark_tick(conn, health_written)?;

    if let Some(pause) = cw_store::control::get_pause(conn)? {
        let paused = match pause {
            cw_store::control::Pause::Indefinite => true,
            cw_store::control::Pause::Until(deadline) => chrono::Utc::now() < deadline,
        };
        if paused {
            debug!("capture is paused");
            return Ok(());
        }
    }

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
        cw_store::control::record_event(
            conn,
            &ControlEvent {
                id: ulid::Ulid::new(),
                kind: EventKind::BlacklistSkip,
                at: chrono::Utc::now(),
                detail: Some(detail),
            },
        )?;
        // Without the process name: the audit trail is where that belongs, and the log is a file
        // this program keeps screen-derived names out of.
        debug!("tick skipped by the privacy gate");
        return Ok(());
    }

    let mut changed = Vec::new();
    for frame in capture.capture_all() {
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
                cw_ocr::recognize(
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
    for ((frame, thumbnail), ocr) in changed.into_iter().zip(outcomes) {
        let now = chrono::Utc::now();
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
        let mut observation = Observation::new_screen(payload, now);
        // This id and instant are the ones `images::save` gets below, which is what makes the
        // path recorded here the path it writes.
        if let SourcePayload::Screen(payload) = &mut observation.payload {
            payload.image_path = Some(cw_store::images::relative_path(observation.id, now));
        }
        cw_store::observations::insert(conn, &observation)?;
        let rgb = bgra_to_rgb(&frame.bgra);
        let stored = cw_store::images::save(
            conn,
            &paths.images(),
            observation.id,
            &rgb,
            frame.width,
            frame.height,
            f32::from(config.capture.webp_quality),
            now,
        )?;
        previous.insert(frame.monitor_id.clone(), thumbnail);
        stored_at = Some(now);
        info!(
            monitor = %frame.monitor_id,
            width = frame.width,
            height = frame.height,
            ocr = ?ocr.status,
            chars = text_chars,
            path = %stored,
            "stored frame"
        );
    }
    // Once per tick rather than once per monitor: all three writes would carry the same tick and
    // `status` reads one value.
    if let Some(at) = stored_at {
        cw_store::control::set_health(conn, HealthKey::LastCapture, at)?;
    }

    Ok(())
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
