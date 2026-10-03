use cw_core::change::{Thumbnail, frame_changed};
use cw_core::config::{Config, DataPaths};
use cw_core::model::{
    CaptureState, CaptureStatus, Observation, OcrStatus, ScreenPayload, SourcePayload, StateSpan,
};
use cw_core::privacy::CaptureDecision;
use cw_store::control::{ControlEvent, EventKind, HealthKey};
use tracing::{debug, error, info, warn};

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
    let mut save_failed: Option<SaveFailure> = None;
    let mut last_failure: Option<String> = None;
    let mut recorder = Recorder::default();

    loop {
        let started = std::time::Instant::now();
        match tick(
            &mut capture,
            ocr,
            &mut conn,
            &paths,
            &config,
            &mut subject,
            &mut recorder,
            &mut save_failed,
        ) {
            Ok(()) => last_failure = None,
            Err(error) => {
                // Fail closed: the tick may have died before it judged the pause or the privacy
                // gate, and failing open shows a screen the gate may have been refusing.
                capture.discard_pending();
                subject.aim(None);
                recorder.open = None;
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
    recorder: &mut Recorder,
    save_failed: &mut Option<SaveFailure>,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = chrono::Utc::now();
    let interval = std::time::Duration::from_secs(config.capture.interval_secs);

    if let Some(pause) = cw_store::control::get_pause(conn)? {
        let paused = match pause {
            cw_store::control::Pause::Indefinite => true,
            cw_store::control::Pause::Until(deadline) => chrono::Utc::now() < deadline,
        };
        if paused {
            capture.discard_pending();
            subject.aim(None);
            let status = CaptureStatus {
                state: CaptureState::Paused,
                process: None,
                title: None,
                detail: None,
            };
            recorder.record(conn, started, Some(status), interval)?;
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
    let status = report_then_finish(outcome, stored.as_ref(), |frame| {
        info!(
            width = frame.width,
            height = frame.height,
            ocr = ?frame.ocr_status,
            chars = frame.text_chars,
            path = %frame.relative_path,
            "stored frame"
        );
    })?;
    // When the tick mark this writes becomes readable, every shot the pass pulled has been stored
    // or refused and the tick's state recorded. An error return skips the mark, which stalls the
    // closer — the safe direction.
    recorder.record(conn, started, status, interval)?;

    Ok(())
}

/// Capture the foreground window once: the privacy gate, change detection against `subject`'s
/// baseline, OCR, and the observation and image rows if it changed. The pause is not consulted
/// here — `tick` owns that, and `capture-once` is a user asking for this pass in particular.
/// `save_failed` holds the last save failure reported, so a failure that repeats is only news the
/// first time. `stored` receives the frame as it lands, so an error return leaves the report of
/// the store in the caller's hands rather than taking it down with the pass. Answers `None` when a
/// frame was stored, and otherwise what the pass saw instead.
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
) -> Result<Option<CaptureStatus>, Box<dyn std::error::Error>> {
    let foreground = cw_capture::foreground();
    let saw = |state, detail| CaptureStatus {
        state,
        process: foreground.process.clone(),
        title: foreground.title.clone(),
        detail,
    };
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
                detail: Some(detail.clone()),
            },
        )?;
        debug!("tick skipped by the privacy gate");
        return Ok(Some(CaptureStatus {
            state: CaptureState::Excluded,
            process: None,
            title: None,
            detail: Some(detail),
        }));
    }

    let Some(target) = foreground.target else {
        capture.release();
        subject.aim(None);
        return Ok(Some(saw(CaptureState::NoTarget, None)));
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
            return Ok(Some(match error {
                cw_capture::CaptureError::Recoverable(cw_capture::Recoverable::NoNewFrame) => {
                    saw(CaptureState::NoNewFrame, None)
                }
                error => saw(CaptureState::CaptureFailed, Some(error.to_string())),
            }));
        }
    };

    let rgba = bgra_to_rgba(&frame.bgra);
    let thumbnail = Thumbnail::from_rgba(&rgba, frame.width, frame.height, frame.dpi_scale)?;
    if !frame_changed(subject.baseline.as_ref(), &thumbnail, &config.capture) {
        return Ok(Some(saw(CaptureState::Unchanged, None)));
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
        foreground_process: foreground.process.clone(),
        foreground_window_title: foreground.title.clone(),
        foreground_hwnd: Some(target.hwnd as i64),
        foreground_pid: Some(target.pid),
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
    let longest_wait = longest_wait(std::time::Duration::from_secs(config.capture.interval_secs))?;
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
        |conn| cw_store::capture_states::mark_recorded(conn, captured_at, longest_wait),
    ) {
        Ok(()) => *save_failed = None,
        Err(error) => {
            let key = save_failure_key(&error);
            if save_failed.as_ref().is_none_or(|last| last.key != key) {
                error!("failed to store frame: {error}");
                *save_failed = Some(SaveFailure {
                    key: key.clone(),
                    message: error.to_string(),
                });
            }
            return Ok(Some(saw(CaptureState::SaveFailed, Some(key))));
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

    Ok(None)
}

/// Hands every stored frame to `report` before the outcome may leave, so a failing pass cannot
/// take the report of its stores down with it.
pub(crate) fn report_then_finish(
    outcome: Result<Option<CaptureStatus>, Box<dyn std::error::Error>>,
    stored: Option<&Stored>,
    report: impl FnOnce(&Stored),
) -> Result<Option<CaptureStatus>, Box<dyn std::error::Error>> {
    if let Some(frame) = stored {
        report(frame);
    }
    outcome
}

/// The longest stretch between two records that is not written as one with nothing recorded.
fn longest_wait(
    interval: std::time::Duration,
) -> Result<chrono::TimeDelta, chrono::OutOfRangeError> {
    chrono::TimeDelta::from_std(interval * 2)
}

/// The capture-state span still open.
#[derive(Default)]
struct Recorder {
    open: Option<(ulid::Ulid, CaptureStatus)>,
}

impl Recorder {
    /// Record the tick that started at `started` and stored a frame, or stored none for the reason
    /// `status` gives, and mark it as the last tick. A frame accounts for the gap before it as it is
    /// saved; a tick that stored none accounts for the gap before its start here. The store and
    /// this recorder change together or not at all.
    fn record(
        &mut self,
        conn: &mut rusqlite::Connection,
        started: chrono::DateTime<chrono::Utc>,
        status: Option<CaptureStatus>,
        interval: std::time::Duration,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let longest_wait = longest_wait(interval)?;
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let last_tick = cw_store::control::get_health(&transaction, HealthKey::LastTick)?;
        if status.is_some() {
            cw_store::capture_states::mark_recorded(&transaction, started, longest_wait)?;
        }
        let unbroken =
            last_tick.is_some_and(|tick| tick <= started && started - tick <= longest_wait);
        let open = match (status, self.open.clone()) {
            (None, _) => None,
            (Some(status), Some((id, current))) if current == status && unbroken => {
                cw_store::capture_states::extend(&transaction, id, started)?;
                Some((id, current))
            }
            (Some(status), _) => {
                let id = ulid::Ulid::generate();
                cw_store::capture_states::insert(
                    &transaction,
                    &StateSpan {
                        id,
                        status: status.clone(),
                        start_at: started,
                        end_at: started,
                    },
                )?;
                Some((id, status))
            }
        };
        cw_store::control::set_health(&transaction, HealthKey::LastTick, started)?;
        transaction.commit()?;

        self.open = open;
        Ok(())
    }
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
    use super::{Recorder, Stored, Subject, report_then_finish, save_failure_key};
    use chrono::{DateTime, TimeDelta, Utc};
    use cw_capture::{CaptureError, Recoverable, Target};
    use cw_core::change::Thumbnail;
    use cw_core::model::{CaptureState, CaptureStatus, StateSpan};
    use cw_store::StoreError;

    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

    fn database() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir =
            tempfile::tempdir().expect("the temporary database directory should be creatable");
        let conn = cw_store::db::open(&dir.path().join("db.sqlite3"))
            .expect("the fresh database should initialize");
        (dir, conn)
    }

    fn seen(state: CaptureState) -> Option<CaptureStatus> {
        Some(CaptureStatus {
            state,
            process: Some("editor.exe".to_owned()),
            title: Some("Example Page".to_owned()),
            detail: None,
        })
    }

    fn spans(conn: &rusqlite::Connection) -> Vec<StateSpan> {
        cw_store::capture_states::overlapping(
            conn,
            DateTime::<Utc>::UNIX_EPOCH,
            Utc::now() + TimeDelta::days(1),
        )
        .expect("the spans should be readable")
    }

    fn bounds(spans: &[StateSpan], state: CaptureState) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        spans
            .iter()
            .filter(|span| span.status.state == state)
            .map(|span| (span.start_at, span.end_at))
            .collect()
    }

    #[test]
    fn an_unchanged_status_extends_its_span() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let second = first + TimeDelta::seconds(10);

        recorder
            .record(&mut conn, first, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the first tick should record");
        recorder
            .record(&mut conn, second, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the second tick should record");

        let spans = spans(&conn);
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start_at, spans[0].end_at), (first, second));
    }

    #[test]
    fn a_different_status_opens_a_new_span() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let second = first + TimeDelta::seconds(10);
        let mut retitled = seen(CaptureState::Unchanged);
        if let Some(status) = &mut retitled {
            status.title = Some("Other Page".to_owned());
        }

        recorder
            .record(&mut conn, first, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the first tick should record");
        recorder
            .record(&mut conn, second, retitled, INTERVAL)
            .expect("the second tick should record");

        assert_eq!(
            bounds(&spans(&conn), CaptureState::Unchanged),
            [(first, first), (second, second)]
        );
    }

    #[test]
    fn a_stored_frame_closes_the_span() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let third = first + TimeDelta::seconds(15);

        for (at, status) in [
            (first, seen(CaptureState::Unchanged)),
            (first + TimeDelta::seconds(5), None),
            (third, seen(CaptureState::Unchanged)),
        ] {
            recorder
                .record(&mut conn, at, status, INTERVAL)
                .expect("the tick should record");
        }

        assert_eq!(
            bounds(&spans(&conn), CaptureState::Unchanged),
            [(first, first), (third, third)]
        );
    }

    fn store_frame(conn: &mut rusqlite::Connection, at: DateTime<Utc>) {
        let frame = cw_core::model::Observation::new_screen(
            cw_core::model::ScreenPayload {
                width: 1,
                height: 1,
                image_path: None,
                ocr_status: cw_core::model::OcrStatus::NoText,
                ocr_error: None,
                ocr_text: None,
                ocr_langs: Vec::new(),
                foreground_process: Some("editor.exe".to_owned()),
                foreground_window_title: Some("Example Page".to_owned()),
                foreground_hwnd: None,
                foreground_pid: None,
            },
            at,
        );
        let transaction = conn
            .transaction()
            .expect("the frame's transaction should open");
        cw_store::observations::insert(&transaction, &frame).expect("the frame should be stored");
        cw_store::capture_states::mark_recorded(
            &transaction,
            at,
            super::longest_wait(INTERVAL).expect("the interval should fit"),
        )
        .expect("the frame should account for the gap around it");
        transaction
            .commit()
            .expect("the frame's transaction should commit");
    }

    #[test]
    fn a_first_tick_that_stores_still_records_the_stretch_since_the_previous_run() {
        let (_dir, mut conn) = database();
        let started = Utc::now();
        let previous_end = started - TimeDelta::hours(1);
        cw_store::capture_states::insert(
            &conn,
            &StateSpan {
                id: ulid::Ulid::generate(),
                status: CaptureStatus {
                    state: CaptureState::Paused,
                    process: None,
                    title: None,
                    detail: None,
                },
                start_at: previous_end - TimeDelta::minutes(1),
                end_at: previous_end,
            },
        )
        .expect("the earlier run's span should be stored");
        let frame = started + TimeDelta::seconds(1);
        store_frame(&mut conn, frame);

        Recorder::default()
            .record(&mut conn, started, None, INTERVAL)
            .expect("the first tick should record");

        assert_eq!(
            bounds(&spans(&conn), CaptureState::Unrecorded),
            [(previous_end, frame)]
        );
    }

    #[test]
    fn a_tick_that_stalls_before_storing_leaves_the_stall_unrecorded() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let stalled = first + TimeDelta::seconds(10);
        let frame = first + TimeDelta::hours(1);

        recorder
            .record(&mut conn, first, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the first tick should record");
        store_frame(&mut conn, frame);
        recorder
            .record(&mut conn, stalled, None, INTERVAL)
            .expect("the stalled tick should record");

        assert_eq!(
            bounds(&spans(&conn), CaptureState::Unrecorded),
            [(first, frame)]
        );
    }

    #[test]
    fn a_frame_captured_before_its_tick_started_ends_the_gap_at_the_frame() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let frame = first + TimeDelta::seconds(3599);
        let started = first + TimeDelta::hours(1);

        recorder
            .record(&mut conn, first, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the first tick should record");
        store_frame(&mut conn, frame);
        recorder
            .record(&mut conn, started, None, INTERVAL)
            .expect("the storing tick should record");

        assert_eq!(
            bounds(&spans(&conn), CaptureState::Unrecorded),
            [(first, frame)]
        );
    }

    #[test]
    fn a_gap_starts_at_the_frame_the_last_tick_stored() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let frame = first + TimeDelta::seconds(1);
        let resumed = first + TimeDelta::hours(1);

        store_frame(&mut conn, frame);
        recorder
            .record(&mut conn, first, None, INTERVAL)
            .expect("the storing tick should record");
        recorder
            .record(&mut conn, resumed, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the resumed tick should record");

        assert_eq!(
            bounds(&spans(&conn), CaptureState::Unrecorded),
            [(frame, resumed)]
        );
    }

    #[test]
    fn a_late_frame_inside_a_recorded_span_leaves_no_gap() {
        let (_dir, mut conn) = database();
        let first = Utc::now();
        let frame = first + TimeDelta::milliseconds(3_589_999);
        store_frame(&mut conn, first);
        cw_store::capture_states::insert(
            &conn,
            &StateSpan {
                id: ulid::Ulid::generate(),
                status: seen(CaptureState::NoNewFrame).expect("a status"),
                start_at: first + TimeDelta::seconds(10),
                end_at: first + TimeDelta::seconds(3590),
            },
        )
        .expect("the recorded span should be stored");
        store_frame(&mut conn, frame);

        Recorder::default()
            .record(&mut conn, first + TimeDelta::hours(1), None, INTERVAL)
            .expect("the storing tick should record");

        assert_eq!(bounds(&spans(&conn), CaptureState::Unrecorded), []);
    }

    #[test]
    fn a_late_frame_inside_a_gap_cuts_the_gap_at_the_frame() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let resumed = first + TimeDelta::hours(1);
        let frame = resumed - TimeDelta::milliseconds(1);

        for at in [first, resumed] {
            recorder
                .record(&mut conn, at, seen(CaptureState::NoNewFrame), INTERVAL)
                .expect("the tick should record");
        }
        store_frame(&mut conn, frame);
        recorder
            .record(&mut conn, resumed + TimeDelta::seconds(10), None, INTERVAL)
            .expect("the storing tick should record");

        assert_eq!(
            bounds(&spans(&conn), CaptureState::Unrecorded),
            [(first, frame)]
        );
    }

    #[test]
    fn a_tick_that_stores_a_late_frame_still_counts_as_a_tick_after_a_restart() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let frame = first - TimeDelta::milliseconds(1);

        recorder
            .record(&mut conn, first, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the first tick should record");
        store_frame(&mut conn, frame);
        recorder
            .record(
                &mut conn,
                first + TimeDelta::microseconds(10_000_500),
                None,
                INTERVAL,
            )
            .expect("the storing tick should record");
        Recorder::default()
            .record(
                &mut conn,
                first + TimeDelta::microseconds(20_001_000),
                seen(CaptureState::NoNewFrame),
                INTERVAL,
            )
            .expect("the first tick after the restart should record");

        assert_eq!(bounds(&spans(&conn), CaptureState::Unrecorded), []);
    }

    #[test]
    fn a_clock_set_back_starts_a_new_span_and_keeps_the_old_one() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let set_back = first - TimeDelta::minutes(5);

        for at in [first, first + TimeDelta::seconds(10), set_back] {
            recorder
                .record(&mut conn, at, seen(CaptureState::Unchanged), INTERVAL)
                .expect("the tick should record");
        }

        assert_eq!(
            bounds(&spans(&conn), CaptureState::Unchanged),
            [
                (set_back, set_back),
                (first, first + TimeDelta::seconds(10))
            ]
        );
    }

    #[test]
    fn a_frame_keeps_the_gap_before_it_when_its_tick_never_records() {
        let (_dir, mut conn) = database();
        let first = Utc::now();
        let frame = first + TimeDelta::hours(1);

        Recorder::default()
            .record(&mut conn, first, seen(CaptureState::NoNewFrame), INTERVAL)
            .expect("the first tick should record");
        store_frame(&mut conn, frame);
        Recorder::default()
            .record(
                &mut conn,
                frame + TimeDelta::seconds(10),
                seen(CaptureState::Unchanged),
                INTERVAL,
            )
            .expect("the next tick should record");

        assert_eq!(
            bounds(&spans(&conn), CaptureState::Unrecorded),
            [(first, frame)]
        );
    }

    #[test]
    fn a_status_seen_again_after_a_gap_another_writer_recorded_opens_a_new_span() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let frame = first + TimeDelta::hours(1);
        let resumed = frame + TimeDelta::seconds(1);

        recorder
            .record(&mut conn, first, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the first tick should record");
        store_frame(&mut conn, frame);
        recorder
            .record(&mut conn, resumed, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the resumed tick should record");

        let spans = spans(&conn);
        let stopped = first + TimeDelta::minutes(30);
        let utc = chrono::FixedOffset::east_opt(0).expect("UTC should be an offset");
        assert!(cw_core::episode::build_episode(stopped, 5, utc, &[], &spans).is_none());
        assert_eq!(bounds(&spans, CaptureState::Unrecorded), [(first, frame)]);
        assert_eq!(
            bounds(&spans, CaptureState::Unchanged),
            [(first, first), (resumed, resumed)]
        );
    }

    #[test]
    fn a_frame_whose_tick_failed_to_record_still_ends_the_gap() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let next = first + TimeDelta::seconds(25);

        recorder
            .record(&mut conn, first, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the first tick should record");
        store_frame(&mut conn, first + TimeDelta::seconds(12));
        recorder
            .record(&mut conn, next, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the next tick should record");

        assert_eq!(bounds(&spans(&conn), CaptureState::Unrecorded), []);
    }

    #[test]
    fn a_gap_past_twice_the_interval_is_unrecorded_and_starts_a_new_span() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let resumed = first + TimeDelta::seconds(30);

        recorder
            .record(&mut conn, first, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the first tick should record");
        recorder
            .record(&mut conn, resumed, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the resumed tick should record");

        let spans = spans(&conn);
        assert_eq!(bounds(&spans, CaptureState::Unrecorded), [(first, resumed)]);
        assert_eq!(
            bounds(&spans, CaptureState::Unchanged),
            [(first, first), (resumed, resumed)]
        );
    }

    #[test]
    fn a_failed_tick_leaves_the_next_status_its_own_span() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();
        let second = first + TimeDelta::seconds(10);

        recorder
            .record(&mut conn, first, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the first tick should record");
        recorder.open = None;
        recorder
            .record(&mut conn, second, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the tick after the failure should record");

        assert_eq!(
            bounds(&spans(&conn), CaptureState::Unchanged),
            [(first, first), (second, second)]
        );
    }

    #[test]
    fn a_record_that_fails_leaves_the_recorder_as_it_was() {
        let (_dir, mut conn) = database();
        let mut recorder = Recorder::default();
        let first = Utc::now();

        recorder
            .record(&mut conn, first, seen(CaptureState::Unchanged), INTERVAL)
            .expect("the first tick should record");
        let open = recorder.open.clone();
        conn.execute_batch("DROP TABLE capture_states")
            .expect("the table should drop");

        let second = first + TimeDelta::seconds(10);
        recorder
            .record(&mut conn, second, seen(CaptureState::NoTarget), INTERVAL)
            .expect_err("a record with nowhere to go should fail");

        assert_eq!(recorder.open, open);
    }

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
        assert!(report_then_finish(Ok(None), Some(&stored), |_| {}).is_ok());
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
