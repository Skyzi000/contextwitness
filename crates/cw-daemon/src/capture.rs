use cw_core::change::{Thumbnail, frame_changed};
use cw_core::config::{Config, DataPaths};
use cw_core::model::{Observation, OcrStatus, ScreenPayload, SourcePayload};
use cw_core::privacy::CaptureDecision;
use cw_store::control::{ControlEvent, EventKind, HealthKey};
use tracing::{debug, error, info, warn};

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
    let mut subject = Subject::default();
    let mut health_written: Option<std::time::Instant> = None;
    let mut save_failed: Option<SaveFailure> = None;
    let mut last_failure: Option<String> = None;

    loop {
        let started = std::time::Instant::now();
        match tick(
            &mut capture,
            ocr,
            &mut conn,
            &paths,
            &config,
            &mut subject,
            &mut health_written,
            &mut save_failed,
        ) {
            Ok(()) => last_failure = None,
            Err(error) => {
                // Fail closed: the tick may have died before it judged the pause or the privacy
                // gate, and failing open shows a screen the gate may have been refusing.
                capture.discard_pending();
                subject.aim(None);
                // The same failure repeating every tick is one line of news, not one line per
                // tick.
                let message = error.to_string();
                if last_failure.as_deref() != Some(message.as_str()) {
                    error!("tick failed: {message}");
                    last_failure = Some(message);
                }
            }
        }
        if let Some(remaining) = interval.checked_sub(started.elapsed()) {
            std::thread::sleep(remaining);
        }
    }
}

/// One frame a pass stored, as the daemon's log line and `capture-once`'s printed line describe
/// it.
pub(crate) struct Stored {
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
    subject: &mut Subject,
    health_written: &mut Option<std::time::Instant>,
    save_failed: &mut Option<SaveFailure>,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = chrono::Utc::now();

    if let Some(pause) = cw_store::control::get_pause(conn)? {
        let paused = match pause {
            cw_store::control::Pause::Indefinite => true,
            cw_store::control::Pause::Until(deadline) => chrono::Utc::now() < deadline,
        };
        if paused {
            capture.discard_pending();
            subject.aim(None);
            mark_tick(conn, health_written, started)?;
            debug!("capture is paused");
            return Ok(());
        }
    }

    let mut stored = None;
    let outcome = pass(
        capture,
        ocr,
        conn,
        paths,
        config,
        subject,
        save_failed,
        &mut stored,
    );
    report_then_finish(outcome, stored.as_ref(), |frame| {
        info!(
            width = frame.width,
            height = frame.height,
            ocr = ?frame.ocr_status,
            chars = frame.text_chars,
            path = %frame.relative_path,
            "stored frame"
        );
    })?;

    // When this mark becomes readable, every shot the pass pulled has been stored or refused. An
    // error return skips the mark, which stalls the closer — the safe direction.
    mark_tick(conn, health_written, started)?;

    Ok(())
}

/// Capture the foreground window once: the privacy gate, change detection against `subject`'s
/// baseline, OCR, and the observation and image rows if it changed. The pause is not consulted
/// here — `tick` owns that, and `capture-once` is a user asking for this pass in particular.
/// `save_failed` holds the last save failure reported, so a failure that repeats is only news the
/// first time. `stored` receives the frame as it lands, so an error return leaves the report of
/// the store in the caller's hands rather than taking it down with the pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pass(
    capture: &mut cw_capture::CaptureEngine,
    ocr: &dyn cw_ocr::OcrEngine,
    conn: &mut rusqlite::Connection,
    paths: &DataPaths,
    config: &Config,
    subject: &mut Subject,
    save_failed: &mut Option<SaveFailure>,
    stored: &mut Option<Stored>,
) -> Result<(), Box<dyn std::error::Error>> {
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
        subject.aim(None);
        cw_store::control::record_event(
            conn,
            &ControlEvent {
                id: ulid::Ulid::generate(),
                kind: EventKind::BlacklistSkip,
                at: chrono::Utc::now(),
                detail: Some(detail),
            },
        )?;
        debug!("tick skipped by the privacy gate");
        return Ok(());
    }

    let Some(target) = foreground.target else {
        capture.release();
        subject.aim(None);
        return Ok(());
    };
    subject.aim(Some(target));
    let frame = match capture.capture(target) {
        Ok(frame) => {
            subject.delivered();
            frame
        }
        Err(error) => {
            if subject.failed(&error) {
                warn!("capture failed: {error}");
            }
            return Ok(());
        }
    };

    let rgba = bgra_to_rgba(&frame.bgra);
    let thumbnail = Thumbnail::from_rgba(&rgba, frame.width, frame.height, frame.dpi_scale)?;
    if !frame_changed(subject.baseline.as_ref(), &thumbnail, &config.capture) {
        return Ok(());
    }

    let ocr = ocr.recognize(
        &frame.bgra,
        frame.width,
        frame.height,
        &config.ocr.languages,
    );
    let captured_at = frame.captured_at;
    let text_chars = ocr.text.as_deref().map_or(0, |text| text.chars().count());
    let payload = ScreenPayload {
        width: frame.width,
        height: frame.height,
        image_path: None,
        ocr_status: ocr.status.clone(),
        ocr_error: ocr.error,
        ocr_text: ocr.text,
        ocr_langs: ocr.langs,
        foreground_process: foreground.process,
        foreground_window_title: foreground.title,
    };
    let mut observation = Observation::new_screen(payload, captured_at);
    let offset =
        chrono::TimeZone::offset_from_utc_datetime(&chrono::Local, &captured_at.naive_utc());
    let mut relative_path = String::new();
    // The `images/` prefix is the payload's spelling only: delivered paths are
    // data_dir-relative, the images table keys on the path relative to the images root.
    if let SourcePayload::Screen(payload) = &mut observation.payload {
        relative_path = cw_store::images::relative_path(
            observation.id,
            captured_at.with_timezone(&offset),
            payload.foreground_process.as_deref(),
            payload.foreground_window_title.as_deref(),
        );
        payload.image_path = Some(format!("images/{relative_path}"));
    }
    let rgb = bgra_to_rgb(&frame.bgra);
    match cw_store::images::save_with_observation(
        conn,
        &paths.images(),
        &observation,
        &relative_path,
        &rgb,
        frame.width,
        frame.height,
        f32::from(config.capture.webp_quality),
        captured_at,
    ) {
        Ok(()) => *save_failed = None,
        Err(error) => {
            let key = save_failure_key(&error);
            if save_failed.as_ref().is_none_or(|last| last.key != key) {
                error!("failed to store frame: {error}");
                *save_failed = Some(SaveFailure {
                    key,
                    message: error.to_string(),
                });
            }
            return Ok(());
        }
    }
    subject.baseline = Some(thumbnail);
    *stored = Some(Stored {
        width: frame.width,
        height: frame.height,
        ocr_status: ocr.status,
        text_chars,
        relative_path,
    });
    cw_store::control::advance_health(conn, HealthKey::LastCapture, captured_at)?;

    Ok(())
}

/// Hands every stored frame to `report` before the outcome may leave, so a failing pass cannot
/// take the report of its stores down with it.
pub(crate) fn report_then_finish(
    outcome: Result<(), Box<dyn std::error::Error>>,
    stored: Option<&Stored>,
    report: impl FnOnce(&Stored),
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(frame) = stored {
        report(frame);
    }
    outcome
}

/// The standing save failure: the spelling repeats are judged by, and the full message
/// of the first failure that spelled it, for `capture-once`'s summary.
pub(crate) struct SaveFailure {
    pub key: String,
    pub message: String,
}

#[derive(Default)]
pub(crate) struct Subject {
    target: Option<cw_capture::Target>,
    baseline: Option<Thumbnail>,
    reported: Option<String>,
}

impl Subject {
    pub(crate) fn aim(&mut self, target: Option<cw_capture::Target>) {
        if self.target != target {
            *self = Self {
                target,
                ..Self::default()
            };
        }
    }

    fn failed(&mut self, error: &cw_capture::CaptureError) -> bool {
        if matches!(
            error,
            cw_capture::CaptureError::Recoverable(cw_capture::Recoverable::NoNewFrame)
        ) {
            return false;
        }
        let line = error.to_string();
        if self.reported.as_ref() == Some(&line) {
            return false;
        }
        self.reported = Some(line);
        true
    }

    fn delivered(&mut self) {
        self.reported = None;
    }

    pub(crate) fn failure(&self) -> Option<&str> {
        self.reported.as_deref()
    }
}

/// The spelling `save_failed` deduplicates on. Several store errors name the freshly minted
/// observation id, or the image path derived from it, so their full messages never repeat; the
/// key keeps the parts that spell the same while the cause does.
fn save_failure_key(error: &cw_store::StoreError) -> String {
    use cw_store::StoreError;
    match error {
        StoreError::ImageIo { source, .. } => format!("image io: {source}"),
        StoreError::Encode { reason, .. } => format!("encode: {reason}"),
        StoreError::Insert { source, .. } => format!("insert: {source}"),
        StoreError::NotFaithful { .. } => "observation would not read back as given".to_owned(),
        StoreError::DurationOutOfRange { duration_ms, .. } => {
            format!("duration {duration_ms} out of range")
        }
        other => other.to_string(),
    }
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
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    rgba
}

fn bgra_to_rgb(bgra: &[u8]) -> Vec<u8> {
    bgra.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Stored, Subject, report_then_finish, save_failure_key};
    use cw_capture::{CaptureError, Recoverable, Target};
    use cw_core::change::Thumbnail;
    use cw_store::StoreError;

    fn stored_frame() -> Stored {
        Stored {
            width: 1,
            height: 1,
            ocr_status: cw_core::model::OcrStatus::NoText,
            text_chars: 0,
            relative_path: String::new(),
        }
    }

    #[test]
    fn a_failing_pass_still_reports_every_stored_frame() {
        let stored = stored_frame();
        let mut reported = 0;
        let outcome = report_then_finish(Err("refused".into()), Some(&stored), |_| {
            reported += 1;
        });
        assert_eq!(reported, 1);
        assert_eq!(outcome.unwrap_err().to_string(), "refused");
        assert!(report_then_finish(Ok(()), Some(&stored), |_| {}).is_ok());
    }

    fn target(hwnd: isize, pid: u32) -> Target {
        Target { hwnd, pid }
    }

    fn access_lost() -> CaptureError {
        Recoverable::AccessLost.into()
    }

    fn aimed_with_history(at: Target) -> Subject {
        let mut subject = Subject::default();
        subject.aim(Some(at));
        subject.baseline = Some(
            Thumbnail::from_rgba(&[0, 0, 0, 255], 1, 1, 1.0)
                .expect("a one-pixel frame should build a thumbnail"),
        );
        assert!(subject.failed(&access_lost()));
        subject
    }

    fn switched(subject: &Subject) -> bool {
        subject.baseline.is_none() && subject.reported.is_none()
    }

    #[test]
    fn the_same_target_keeps_the_baseline() {
        let mut subject = aimed_with_history(target(1, 10));
        subject.aim(Some(target(1, 10)));
        assert!(subject.baseline.is_some());
        assert!(subject.reported.is_some());
    }

    #[test]
    fn a_different_target_drops_the_baseline() {
        let mut subject = aimed_with_history(target(1, 10));
        subject.aim(Some(target(2, 10)));
        assert!(switched(&subject));
    }

    #[test]
    fn a_tick_without_a_target_makes_the_next_one_a_switch() {
        let mut subject = aimed_with_history(target(1, 10));
        subject.aim(None);
        subject.aim(Some(target(1, 10)));
        assert!(switched(&subject));
    }

    #[test]
    fn the_same_hwnd_under_another_pid_is_a_switch() {
        let mut subject = aimed_with_history(target(1, 10));
        subject.aim(Some(target(1, 11)));
        assert!(switched(&subject));
    }

    #[test]
    fn a_repeated_failure_is_news_once_and_no_new_frame_keeps_the_history() {
        let mut subject = aimed_with_history(target(1, 10));
        assert!(!subject.failed(&access_lost()));
        assert!(!subject.failed(&Recoverable::NoNewFrame.into()));
        assert!(!subject.failed(&access_lost()));

        subject.delivered();
        assert!(subject.failed(&access_lost()));

        subject.aim(Some(target(2, 10)));
        assert!(subject.failed(&access_lost()));
    }

    #[test]
    fn a_save_failure_key_survives_a_fresh_observation_id() {
        let io_first = save_failure_key(&StoreError::ImageIo {
            path: "root/2026/08/16/01J5AAAAAAAAAAAAAAAAAAAAAA.webp".into(),
            source: std::io::Error::other("disk full"),
        });
        let io_second = save_failure_key(&StoreError::ImageIo {
            path: "root/2026/08/16/01J5BBBBBBBBBBBBBBBBBBBBBB.webp".into(),
            source: std::io::Error::other("disk full"),
        });
        assert_eq!(io_first, io_second);

        let encode_first = save_failure_key(&StoreError::Encode {
            id: "01J5AAAAAAAAAAAAAAAAAAAAAA".to_owned(),
            reason: "the image is too wide".to_owned(),
        });
        let encode_second = save_failure_key(&StoreError::Encode {
            id: "01J5BBBBBBBBBBBBBBBBBBBBBB".to_owned(),
            reason: "the image is too wide".to_owned(),
        });
        assert_eq!(encode_first, encode_second);

        let insert_first = save_failure_key(&StoreError::Insert {
            id: "01J5AAAAAAAAAAAAAAAAAAAAAA".to_owned(),
            source: rusqlite::Error::InvalidQuery,
        });
        let insert_second = save_failure_key(&StoreError::Insert {
            id: "01J5BBBBBBBBBBBBBBBBBBBBBB".to_owned(),
            source: rusqlite::Error::InvalidQuery,
        });
        assert_eq!(insert_first, insert_second);

        let distinct = [&io_first, &encode_first, &insert_first];
        for (index, key) in distinct.iter().enumerate() {
            for other in &distinct[index + 1..] {
                assert_ne!(key, other);
            }
        }
    }
}
