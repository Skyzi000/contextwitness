// Closing windows into episodes: the startup rescan and the periodic closer are one pass.

use chrono::{DateTime, TimeDelta, Utc};
use cw_store::episodes::Registration;
use tracing::{debug, error, info};

/// The source `cw_core::episode::build_episode` stamps on what it builds. Spelled here because
/// `latest_end` is asked per source and cw-core does not export the constant.
const SOURCE: &str = "screen";
/// A window is only built this long after it closed (plan Task 21): the tick that observed its
/// last instants may still be writing them, and an episode is a snapshot that is not rebuilt.
const GRACE_SECONDS: i64 = 60;
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
    if cursor.is_none() {
        *cursor = resume_point(conn, window_minutes)?;
    }
    let Some(start) = cursor.as_mut() else {
        // Nothing has ever been observed, so there is no window to close yet.
        return Ok(0);
    };
    let window = TimeDelta::minutes(i64::from(window_minutes));
    let deadline = now - TimeDelta::seconds(GRACE_SECONDS);
    // The machine's offset now, and only for the rendered body: ids and metadata are UTC, and
    // cw-core takes the offset as an argument so that nothing in it consults a clock.
    let offset = *chrono::Local::now().offset();
    let mut registered = 0;

    while *start + window <= deadline {
        let end = *start + window;
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
        // After the insert, so a window whose registration failed is the one the next pass retries.
        *start = end;
    }

    Ok(registered)
}

fn resume_point(
    conn: &rusqlite::Connection,
    window_minutes: u32,
) -> Result<Cursor, Box<dyn std::error::Error>> {
    // Taken through `window_start` rather than used as it stands: a `window_minutes` changed since
    // that episode was written leaves an end that is not a boundary of the current length.
    if let Some(end) = cw_store::episodes::latest_end(conn, SOURCE)? {
        return Ok(Some(cw_core::episode::window_start(end, window_minutes)));
    }

    Ok(earliest_observation(conn)?.map(|at| cw_core::episode::window_start(at, window_minutes)))
}

fn earliest_observation(
    conn: &rusqlite::Connection,
) -> Result<Option<DateTime<Utc>>, Box<dyn std::error::Error>> {
    let spelled: Option<String> = conn.query_one(EARLIEST_OBSERVATION, [], |row| row.get(0))?;
    // cw-store spells every TEXT time column with `to_rfc3339_opts(Nanos, true)` (design §4.3) and
    // keeps the reader for it private, so this asks RFC 3339 for the value that spelling stands
    // for. Only the resume point of a database with no episodes at all comes through here.
    let Some(spelled) = spelled else {
        return Ok(None);
    };

    Ok(Some(
        DateTime::parse_from_rfc3339(&spelled)?.with_timezone(&Utc),
    ))
}
