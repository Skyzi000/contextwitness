use chrono::{DateTime, TimeDelta, Utc};
use cw_store::control::HealthKey;
use cw_store::episodes::Registration;
use tracing::{debug, error, info};

/// The source `cw_core::episode::build_episode` stamps on what it builds. Spelled here because
/// `latest_end` is asked per source and cw-core does not export the constant.
const SOURCE: &str = "screen";
/// A window is only built this long after it closed: the tick that observed its
/// last instants may still be writing them, and an episode is a snapshot that is not rebuilt.
/// `close_due` subtracts it from two clocks — the wall clock, and the writer's last tick — since
/// only the second of them stops when the writer does.
const GRACE_SECONDS: i64 = 60;
const _: () = assert!((cw_capture::STALE_SHOT_SECONDS as i64) < GRACE_SECONDS);
// cw-core bounds `capture.interval_secs` by its own spelling of the staleness allowance, which
// it cannot import; this crate is where both spellings are visible.
const _: () = assert!(cw_core::config::MAX_CAPTURE_INTERVAL_SECS == cw_capture::STALE_SHOT_SECONDS);
/// How often the closer looks. A window closes every `window_minutes`, so this only decides how
/// much of the grace period is overshot.
const POLL: std::time::Duration = std::time::Duration::from_secs(30);

const EARLIEST_RECORD: &str = "SELECT min(at) FROM (\
     SELECT min(observed_at) AS at FROM observations \
     UNION ALL SELECT min(start_at) FROM capture_states)";

/// Run the episode closer on this thread until the process ends. `cursor` carries on from the
/// startup rescan.
pub fn run(
    mut conn: rusqlite::Connection,
    config: cw_core::config::Config,
    mut cursor: Cursor,
    audio_worker_alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> ! {
    loop {
        std::thread::sleep(POLL);
        if let Err(error) = close_due(
            &mut conn,
            &mut cursor,
            config.episode.window_minutes,
            chrono::Utc::now(),
        ) {
            error!("closing episodes failed: {error}");
        }
        if let Err(error) = close_due_audio(
            &mut conn,
            config.episode.window_minutes,
            chrono::Utc::now(),
            audio_worker_alive.load(std::sync::atomic::Ordering::Relaxed),
        ) {
            error!("closing audio episodes failed: {error}");
        }
    }
}

/// The start of the earliest window not yet examined. `None` asks the store where to resume, which
/// is what the startup rescan begins with; afterwards it advances in this process, so a stretch of
/// windows with nothing in them is walked once rather than on every pass.
pub type Cursor = Option<DateTime<Utc>>;

/// Register every window that has closed, and whose grace has passed, since `cursor`.
///
/// Registering the same window twice is what the `document_id` UNIQUE is for, so an
/// `AlreadyRegistered` here is the rescan working rather than a failure.
pub fn close_due(
    conn: &mut rusqlite::Connection,
    cursor: &mut Cursor,
    window_minutes: u32,
    now: DateTime<Utc>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let Some(last_tick) =
        cw_store::control::get_health(conn, cw_store::control::HealthKey::LastTick)?
    else {
        // No mark has ever been published for this database, so nothing bounds what is still being
        // persisted and wall-clock grace alone cannot close a window.
        return Ok(0);
    };
    if cursor.is_none() {
        *cursor = resume_point(conn, window_minutes)?;
    }
    let Some(start) = cursor.as_mut() else {
        return Ok(0);
    };
    let window = TimeDelta::minutes(i64::from(window_minutes));
    // A stamp not yet stored can lie at most `STALE_SHOT_SECONDS` below the mark. The mark covers
    // this daemon's writer only: a concurrent `capture-once`, a daemon in another Windows session,
    // and backward wall-clock corrections are outside it.
    let deadline = (now - TimeDelta::seconds(GRACE_SECONDS))
        .min(last_tick - TimeDelta::seconds(GRACE_SECONDS));
    let mut registered = 0;

    while *start + window <= deadline {
        registered += usize::from(register_window(
            conn,
            *start,
            window_minutes,
            now,
            cw_core::episode::build_episode,
            cw_store::capture_states::overlapping,
        )?);
        *start += window;
    }

    Ok(registered)
}

/// Must run before the first audio worker is spawned: no utterance can start earlier than that.
pub fn begin_audio_windows(
    conn: &rusqlite::Connection,
    window_minutes: u32,
    now: DateTime<Utc>,
) -> Result<(), cw_store::StoreError> {
    if cw_store::control::get_health(conn, HealthKey::AudioClosed)?.is_none() {
        cw_store::control::set_health(
            conn,
            HealthKey::AudioClosed,
            cw_core::episode::window_start(now, window_minutes),
        )?;
    }

    Ok(())
}

/// `worker_alive` has to be true from before an audio worker is spawned until after it has exited.
pub fn close_due_audio(
    conn: &mut rusqlite::Connection,
    window_minutes: u32,
    now: DateTime<Utc>,
    worker_alive: bool,
) -> Result<usize, Box<dyn std::error::Error>> {
    let Some(closed) = cw_store::control::get_health(conn, HealthKey::AudioClosed)? else {
        return Ok(0);
    };
    let settled = if worker_alive {
        let Some(settled) = cw_store::control::get_health(conn, HealthKey::AudioSettled)? else {
            return Ok(0);
        };
        settled
    } else {
        now
    };
    let first = far_boundary(closed, window_minutes);
    let mut start = first;
    let window = TimeDelta::minutes(i64::from(window_minutes));
    let deadline = now.min(settled) - TimeDelta::seconds(GRACE_SECONDS);
    let mut registered = 0;
    let mut failed = None;

    while start + window <= deadline {
        match register_window(
            conn,
            start,
            window_minutes,
            now,
            cw_core::episode::build_audio_episode,
            cw_store::control::pauses,
        ) {
            Ok(new) => registered += usize::from(new),
            Err(error) => {
                failed = Some(error);
                break;
            }
        }
        start += window;
    }
    if start != first {
        cw_store::control::advance_health(conn, HealthKey::AudioClosed, start)?;
        if first != closed {
            report_skipped(closed, first);
        }
    }

    failed.map_or(Ok(registered), Err)
}

type Build = fn(
    DateTime<Utc>,
    u32,
    chrono::FixedOffset,
    &[cw_core::model::Observation],
    &[cw_core::model::StateSpan],
) -> Option<cw_core::episode::Episode>;

type Spans = fn(
    &rusqlite::Connection,
    DateTime<Utc>,
    DateTime<Utc>,
) -> Result<Vec<cw_core::model::StateSpan>, cw_store::StoreError>;

fn register_window(
    conn: &mut rusqlite::Connection,
    start: DateTime<Utc>,
    window_minutes: u32,
    now: DateTime<Utc>,
    build: Build,
    spans: Spans,
) -> Result<bool, Box<dyn std::error::Error>> {
    let end = start + TimeDelta::minutes(i64::from(window_minutes));
    // The offset that stood over this window, not the one standing now: the startup rescan
    // closes windows from days ago, and daylight saving puts the two an hour apart.
    let offset = chrono::TimeZone::offset_from_utc_datetime(&chrono::Local, &start.naive_utc());
    let observations = cw_store::observations::find_in_window(conn, start, end)?;
    let spans = spans(conn, start, end)?;
    let Some(episode) = build(start, window_minutes, offset, &observations, &spans) else {
        return Ok(false);
    };

    match cw_store::episodes::insert_with_outbox(conn, ulid::Ulid::generate(), &episode, now)? {
        Registration::Registered(id) => {
            info!(
                episode = %id,
                document = %episode.document_id,
                entries = %episode.metadata.entry_count,
                "queued episode for delivery"
            );
            Ok(true)
        }
        Registration::AlreadyRegistered => {
            debug!(document = %episode.document_id, "window is already registered");
            Ok(false)
        }
    }
}

fn resume_point(
    conn: &rusqlite::Connection,
    window_minutes: u32,
) -> Result<Cursor, Box<dyn std::error::Error>> {
    if let Some(end) = cw_store::episodes::latest_end(conn, SOURCE)? {
        return Ok(Some(resume_after(end, window_minutes)));
    }

    Ok(earliest_record(conn)?.map(|at| cw_core::episode::window_start(at, window_minutes)))
}

fn resume_after(end: DateTime<Utc>, window_minutes: u32) -> DateTime<Utc> {
    let next = far_boundary(end, window_minutes);
    if next != end {
        report_skipped(end, next);
    }

    next
}

fn far_boundary(end: DateTime<Utc>, window_minutes: u32) -> DateTime<Utc> {
    // A `window_minutes` changed since `end` was recorded leaves it off the new boundaries.
    // Resuming at the far boundary rather than the near one: the overlapping window would carry
    // the same observations again under a different `document_id`, and the UNIQUE reads the id
    // only.
    let aligned = cw_core::episode::window_start(end, window_minutes);
    if aligned == end {
        return end;
    }

    aligned + TimeDelta::minutes(i64::from(window_minutes))
}

fn report_skipped(from: DateTime<Utc>, until: DateTime<Utc>) {
    info!(
        %from,
        %until,
        "window length changed; this span is never registered rather than registered twice"
    );
}

fn earliest_record(
    conn: &rusqlite::Connection,
) -> Result<Option<DateTime<Utc>>, Box<dyn std::error::Error>> {
    let spelled: Option<String> = conn.query_one(EARLIEST_RECORD, [], |row| row.get(0))?;
    // cw-store spells every TEXT time column as RFC 3339 and keeps its reader private.
    let Some(spelled) = spelled else {
        return Ok(None);
    };

    Ok(Some(
        DateTime::parse_from_rfc3339(&spelled)?.with_timezone(&Utc),
    ))
}

#[cfg(test)]
mod tests {
    use super::{begin_audio_windows, close_due, close_due_audio};
    use chrono::{DateTime, Utc};
    use cw_core::model::{AudioPayload, CaptureState, CaptureStatus, Observation, StateSpan};
    use cw_store::control::HealthKey;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("the test timestamp should be valid")
            .with_timezone(&Utc)
    }

    fn database() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir =
            tempfile::tempdir().expect("the temporary database directory should be creatable");
        let conn = cw_store::db::open(&dir.path().join("db.sqlite3"))
            .expect("the fresh database should initialize");
        (dir, conn)
    }

    fn hear(conn: &rusqlite::Connection, observed_at: &str) {
        cw_store::observations::insert(
            conn,
            &Observation::new_audio(
                AudioPayload {
                    process: "talk.exe".to_owned(),
                    text: Some("one two three".to_owned()),
                    error: None,
                },
                at(observed_at),
                1_000,
            ),
        )
        .expect("the utterance should be stored");
    }

    fn pause(conn: &mut rusqlite::Connection, when: &str) {
        cw_store::control::set_pause(
            conn,
            cw_store::control::Pause::Indefinite,
            ulid::Ulid::generate(),
            at(when),
        )
        .expect("the pause should be stored");
    }

    fn resume(conn: &mut rusqlite::Connection, when: &str) {
        cw_store::control::resume(conn, ulid::Ulid::generate(), at(when))
            .expect("the resume should be stored");
    }

    fn settle(conn: &rusqlite::Connection, through: &str) {
        cw_store::control::advance_health(conn, HealthKey::AudioSettled, at(through))
            .expect("the settled mark should be stored");
    }

    fn begin(conn: &rusqlite::Connection, window_minutes: u32, now: &str) {
        begin_audio_windows(conn, window_minutes, at(now))
            .expect("the first audio window should be recordable");
    }

    fn close_audio(
        conn: &mut rusqlite::Connection,
        window_minutes: u32,
        now: &str,
        worker_alive: bool,
    ) -> usize {
        close_due_audio(conn, window_minutes, at(now), worker_alive)
            .expect("the audio closer should run")
    }

    fn closed(conn: &rusqlite::Connection) -> Option<DateTime<Utc>> {
        cw_store::control::get_health(conn, HealthKey::AudioClosed)
            .expect("the closed mark should be readable")
    }

    fn documents(conn: &rusqlite::Connection) -> Vec<String> {
        conn.prepare("SELECT document_id FROM episodes ORDER BY start_at")
            .expect("the episodes should be queryable")
            .query_map([], |row| row.get(0))
            .expect("the episodes should be readable")
            .collect::<Result<_, _>>()
            .expect("every document id should be text")
    }

    #[test]
    fn beginning_audio_windows_records_the_window_of_that_moment_once() {
        let (_dir, conn) = database();

        begin(&conn, 5, "2026-07-24T16:03:20Z");
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:00:00Z")));

        begin(&conn, 5, "2026-07-24T18:00:00Z");
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:00:00Z")));

        begin(&conn, 5, "2026-07-24T12:00:00Z");
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:00:00Z")));
    }

    #[test]
    fn a_window_holding_an_utterance_registers_an_audio_episode() {
        let (_dir, mut conn) = database();
        begin(&conn, 5, "2026-07-24T16:00:30Z");
        hear(&conn, "2026-07-24T16:01:00Z");
        settle(&conn, "2026-07-24T16:06:00Z");

        assert_eq!(close_audio(&mut conn, 5, "2026-07-24T16:10:00Z", true), 1);
        assert_eq!(documents(&conn), ["audio-2026-07-24T16:00:00Z-5m"]);
        let (source, content, outbox): (String, String, String) = conn
            .query_one(
                "SELECT e.source, e.content, o.state FROM episodes e \
                 JOIN outbox o ON o.episode_id = e.id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("the registered episode should be readable");
        assert_eq!(source, "audio");
        assert!(content.contains("[talk.exe]\n  "));
        assert!(content.ends_with(" one two three"));
        assert_eq!(outbox, "pending");
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:05:00Z")));
    }

    #[test]
    fn a_window_without_an_utterance_registers_nothing_and_moves_the_closed_mark() {
        let (_dir, mut conn) = database();
        begin(&conn, 5, "2026-07-24T16:00:30Z");
        pause(&mut conn, "2026-07-24T16:01:00Z");
        resume(&mut conn, "2026-07-24T16:02:00Z");

        assert_eq!(close_audio(&mut conn, 5, "2026-07-24T16:20:00Z", false), 0);
        assert!(documents(&conn).is_empty());
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:15:00Z")));
    }

    #[test]
    fn a_pause_line_runs_between_the_requests_and_not_over_what_a_screen_tick_joined() {
        let (_dir, mut conn) = database();
        begin(&conn, 5, "2026-07-24T16:00:30Z");
        pause(&mut conn, "2026-07-24T16:02:10Z");
        resume(&mut conn, "2026-07-24T16:02:12Z");
        hear(&conn, "2026-07-24T16:02:14Z");
        pause(&mut conn, "2026-07-24T16:02:18Z");
        resume(&mut conn, "2026-07-24T16:03:00Z");
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
                start_at: at("2026-07-24T16:02:10Z"),
                end_at: at("2026-07-24T16:02:50Z"),
            },
        )
        .expect("the span the screen ticks joined should be stored");

        assert_eq!(close_audio(&mut conn, 5, "2026-07-24T16:20:00Z", false), 1);
        let content: String = conn
            .query_one("SELECT content FROM episodes", [], |row| row.get(0))
            .expect("the registered episode should be readable");
        let stretches: Vec<_> = content
            .split("\n\n")
            .skip(1)
            .map(|block| {
                let mut words = block.split_whitespace();
                let from = at(words.next().expect("a block starts with a time"));
                let to = at(words.nth(1).expect("a block names its end"));
                (
                    from,
                    to,
                    block.ends_with("[audio capture pause requested for this period]"),
                )
            })
            .collect();
        assert_eq!(
            stretches,
            [
                (at("2026-07-24T16:02:10Z"), at("2026-07-24T16:02:12Z"), true),
                (
                    at("2026-07-24T16:02:14Z"),
                    at("2026-07-24T16:02:15Z"),
                    false
                ),
                (at("2026-07-24T16:02:18Z"), at("2026-07-24T16:03:00Z"), true),
            ]
        );
    }

    #[test]
    fn without_a_closed_mark_nothing_is_registered_and_nothing_is_written() {
        for worker_alive in [true, false] {
            let (_dir, mut conn) = database();
            hear(&conn, "2026-07-24T16:01:00Z");
            settle(&conn, "2026-07-24T16:30:00Z");

            assert_eq!(
                close_audio(&mut conn, 5, "2026-07-24T17:00:00Z", worker_alive),
                0
            );
            assert!(documents(&conn).is_empty());
            assert_eq!(closed(&conn), None);
        }
    }

    #[test]
    fn a_live_worker_keeps_a_window_open_until_its_settled_mark_passes_it() {
        let (_dir, mut conn) = database();
        begin(&conn, 5, "2026-07-24T16:00:30Z");
        hear(&conn, "2026-07-24T16:01:00Z");

        assert_eq!(close_audio(&mut conn, 5, "2026-07-24T17:00:00Z", true), 0);

        settle(&conn, "2026-07-24T16:05:59Z");
        assert_eq!(close_audio(&mut conn, 5, "2026-07-30T00:00:00Z", true), 0);
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:00:00Z")));

        settle(&conn, "2026-07-24T16:06:00Z");
        assert_eq!(close_audio(&mut conn, 5, "2026-07-24T16:05:59Z", true), 0);
        assert_eq!(close_audio(&mut conn, 5, "2026-07-30T00:00:00Z", true), 1);
        assert_eq!(documents(&conn), ["audio-2026-07-24T16:00:00Z-5m"]);
    }

    #[test]
    fn without_a_worker_windows_close_up_to_the_grace_whatever_the_settled_mark_says() {
        for stale_mark in [None, Some("2026-07-24T16:00:40Z")] {
            let (_dir, mut conn) = database();
            begin(&conn, 5, "2026-07-24T16:00:30Z");
            hear(&conn, "2026-07-24T16:01:00Z");
            if let Some(through) = stale_mark {
                settle(&conn, through);
            }

            assert_eq!(close_audio(&mut conn, 5, "2026-07-24T16:05:59Z", false), 0);
            assert_eq!(close_audio(&mut conn, 5, "2026-07-24T16:06:00Z", false), 1);
            assert_eq!(closed(&conn), Some(at("2026-07-24T16:05:00Z")));
        }
    }

    #[test]
    fn a_restart_resumes_audio_from_the_closed_mark() {
        let (dir, mut conn) = database();
        begin(&conn, 5, "2026-07-24T16:00:30Z");
        assert_eq!(close_audio(&mut conn, 5, "2026-07-24T16:10:00Z", false), 0);
        hear(&conn, "2026-07-24T16:02:00Z");
        hear(&conn, "2026-07-24T16:06:00Z");
        drop(conn);

        let mut conn =
            cw_store::db::open(&dir.path().join("db.sqlite3")).expect("the database should reopen");
        begin(&conn, 5, "2026-07-24T16:30:00Z");

        assert_eq!(close_audio(&mut conn, 5, "2026-07-24T16:30:00Z", false), 1);
        assert_eq!(documents(&conn), ["audio-2026-07-24T16:05:00Z-5m"]);
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:25:00Z")));
    }

    #[test]
    fn a_changed_window_length_resumes_audio_at_the_far_boundary() {
        let (_dir, mut conn) = database();
        begin(&conn, 5, "2026-07-24T16:05:00Z");
        hear(&conn, "2026-07-24T16:06:00Z");
        hear(&conn, "2026-07-24T16:11:00Z");

        assert_eq!(close_audio(&mut conn, 10, "2026-07-24T16:30:00Z", false), 1);
        assert_eq!(documents(&conn), ["audio-2026-07-24T16:10:00Z-10m"]);
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:20:00Z")));
    }

    #[test]
    fn a_window_length_changed_and_changed_back_before_a_window_closed_skips_nothing() {
        let (_dir, mut conn) = database();
        begin(&conn, 5, "2026-07-24T16:05:00Z");
        hear(&conn, "2026-07-24T16:07:00Z");

        assert_eq!(close_audio(&mut conn, 60, "2026-07-24T16:06:00Z", false), 0);
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:05:00Z")));

        assert_eq!(close_audio(&mut conn, 5, "2026-07-24T16:11:00Z", false), 1);
        assert_eq!(documents(&conn), ["audio-2026-07-24T16:05:00Z-5m"]);
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:10:00Z")));
    }

    #[test]
    fn a_window_that_fails_keeps_the_windows_closed_before_it() {
        let (_dir, mut conn) = database();
        begin(&conn, 5, "2026-07-24T16:00:30Z");
        hear(&conn, "2026-07-24T16:01:00Z");
        hear(&conn, "2026-07-24T16:06:00Z");
        let broken = at("2026-07-24T16:06:00Z");
        let unreadable = cw_store::observations::find_in_window(
            &conn,
            broken,
            broken + chrono::TimeDelta::seconds(1),
        )
        .expect("the utterance should be readable")[0]
            .id
            .to_string();
        conn.execute(
            "UPDATE observations SET payload = '{}' WHERE id = ?1",
            [&unreadable],
        )
        .expect("the payload should be replaceable");

        close_due_audio(&mut conn, 5, at("2026-07-24T16:30:00Z"), false)
            .expect_err("the unreadable utterance should fail its window");
        assert_eq!(documents(&conn), ["audio-2026-07-24T16:00:00Z-5m"]);
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:05:00Z")));

        conn.execute("DELETE FROM observations WHERE id = ?1", [&unreadable])
            .expect("the row should be removable");
        assert_eq!(close_audio(&mut conn, 5, "2026-07-24T16:30:00Z", false), 0);
        assert_eq!(closed(&conn), Some(at("2026-07-24T16:25:00Z")));
    }

    #[test]
    fn a_window_holding_only_capture_states_registers_an_episode() {
        let dir =
            tempfile::tempdir().expect("the temporary database directory should be creatable");
        let mut conn = cw_store::db::open(&dir.path().join("db.sqlite3"))
            .expect("the fresh database should initialize");
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
                start_at: at("2026-07-24T16:01:00Z"),
                end_at: at("2026-07-24T16:02:00Z"),
            },
        )
        .expect("the span should be stored");
        let later = at("2026-07-24T16:10:00Z");
        cw_store::control::set_health(&conn, cw_store::control::HealthKey::LastTick, later)
            .expect("the tick mark should be stored");

        let registered = close_due(&mut conn, &mut None, 5, later).expect("the closer should run");

        assert_eq!(registered, 1);
        let content: String = conn
            .query_one("SELECT content FROM episodes", [], |row| row.get(0))
            .expect("the registered episode should be readable");
        assert!(content.ends_with("\n  [capture paused]"));
    }
}
