use chrono::{DateTime, TimeDelta, Utc};
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

const EARLIEST_OBSERVATION: &str = "SELECT min(observed_at) FROM observations";

/// Run the episode closer on this thread until the process ends. `cursor` carries on from the
/// startup rescan.
pub fn run(
    mut conn: rusqlite::Connection,
    config: cw_core::config::Config,
    mut cursor: Cursor,
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
        let end = *start + window;
        // The offset that stood over this window, not the one standing now: the startup rescan
        // closes windows from days ago, and daylight saving puts the two an hour apart.
        let offset = chrono::TimeZone::offset_from_utc_datetime(&chrono::Local, &start.naive_utc());
        let observations = cw_store::observations::find_in_window(conn, *start, end)?;
        if let Some(episode) =
            cw_core::episode::build_episode(*start, window_minutes, offset, &observations)
        {
            match cw_store::episodes::insert_with_outbox(
                conn,
                ulid::Ulid::generate(),
                &episode,
                now,
            )? {
                Registration::Registered(id) => {
                    registered += 1;
                    info!(
                        episode = %id,
                        document = %episode.document_id,
                        entries = %episode.metadata.entry_count,
                        "queued episode for delivery"
                    );
                }
                Registration::AlreadyRegistered => {
                    debug!(document = %episode.document_id, "window is already registered");
                }
            }
        }
        *start = end;
    }

    Ok(registered)
}

fn resume_point(
    conn: &rusqlite::Connection,
    window_minutes: u32,
) -> Result<Cursor, Box<dyn std::error::Error>> {
    // A `window_minutes` changed since that episode leaves an end off the new boundaries. Resuming
    // at the far boundary rather than the near one: the overlapping window would carry the same
    // observations again under a different `document_id`, and the UNIQUE reads the id only.
    if let Some(end) = cw_store::episodes::latest_end(conn, SOURCE)? {
        let aligned = cw_core::episode::window_start(end, window_minutes);
        if aligned < end {
            let next = aligned + TimeDelta::minutes(i64::from(window_minutes));
            info!(
                from = %end,
                until = %next,
                "window length changed; this span is never registered rather than registered twice"
            );
            return Ok(Some(next));
        }
        return Ok(Some(aligned));
    }

    Ok(earliest_observation(conn)?.map(|at| cw_core::episode::window_start(at, window_minutes)))
}

fn earliest_observation(
    conn: &rusqlite::Connection,
) -> Result<Option<DateTime<Utc>>, Box<dyn std::error::Error>> {
    let spelled: Option<String> = conn.query_one(EARLIEST_OBSERVATION, [], |row| row.get(0))?;
    // cw-store spells every TEXT time column as RFC 3339 and keeps its reader private.
    let Some(spelled) = spelled else {
        return Ok(None);
    };

    Ok(Some(
        DateTime::parse_from_rfc3339(&spelled)?.with_timezone(&Utc),
    ))
}
