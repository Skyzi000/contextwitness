//! Pause state, health marks and the audit trail the CLI, the tray and the daemon share.

use crate::{StoreError, timestamp};

/// The recorded pause, as [`get_pause`] answers it: no clock is consulted, so whether capture
/// is stopped is its reader's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// Asked to stop until this instant. A deadline already past is still answered, for the
    /// reader to compare against its one notion of now.
    Until(chrono::DateTime<chrono::Utc>),
    /// Asked to stop with no end, until somebody resumes.
    Indefinite,
}

/// A moment `contextwitness status` reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthKey {
    /// When the last completed capture pass started.
    LastTick,
    /// When a frame was last stored.
    LastCapture,
    /// When an episode was last delivered.
    LastDelivery,
}

/// What an audit row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Capture was asked to stop. Asking again for a pause already in force records this too, so a
    /// row is a request rather than proof that anything moved.
    Paused,
    /// Capture was asked to start again. A resume with nothing to resume records this too, so a row
    /// is a request rather than proof that anything moved.
    Resumed,
    /// A tick captured nothing because the foreground process is blacklisted.
    BlacklistSkip,
}

/// One row of the audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlEvent {
    /// Primary key.
    pub id: ulid::Ulid,
    /// What was asked for or observed.
    pub kind: EventKind,
    /// When it happened.
    pub at: chrono::DateTime<chrono::Utc>,
    /// Anything worth keeping beside the kind, such as the deadline a pause was given.
    pub detail: Option<String>,
}

/// The `control_state` key a pause deadline is stored under.
const PAUSE_UNTIL: &str = "pause_until";
/// The `control_state` key an endless pause is stored under.
const PAUSE_INDEFINITE: &str = "pause_indefinite";
/// The only value `pause_indefinite` is ever written with. The key alone does not say it: a row
/// carrying anything else was written by something other than this program, and this program is
/// not in a position to guess what it meant.
const INDEFINITELY: &str = "1";

impl HealthKey {
    /// The `control_state` key this mark is stored under.
    const fn key(self) -> &'static str {
        match self {
            Self::LastTick => "last_tick_at",
            Self::LastCapture => "last_capture_at",
            Self::LastDelivery => "last_delivery_at",
        }
    }
}

impl EventKind {
    /// How this kind is spelled in the `kind` column.
    const fn spelling(self) -> &'static str {
        match self {
            Self::Paused => "pause",
            Self::Resumed => "resume",
            Self::BlacklistSkip => "blacklist_skip",
        }
    }

    /// Read a `kind` column back, refusing one this build does not know.
    fn from_spelling(text: &str) -> Option<Self> {
        match text {
            "pause" => Some(Self::Paused),
            "resume" => Some(Self::Resumed),
            "blacklist_skip" => Some(Self::BlacklistSkip),
            _ => None,
        }
    }
}

const SELECT_PAUSE: &str = "SELECT key, value FROM control_state WHERE key IN (?1, ?2)";
const UPSERT_STATE: &str = "INSERT INTO control_state (key, value) VALUES (?1, ?2) \
     ON CONFLICT(key) DO UPDATE SET value = excluded.value";
const DELETE_STATE: &str = "DELETE FROM control_state WHERE key = ?1";
const DELETE_PAUSE: &str = "DELETE FROM control_state WHERE key IN (?1, ?2)";
const SELECT_STATE: &str = "SELECT value FROM control_state WHERE key = ?1";
const INSERT_EVENT: &str =
    "INSERT INTO control_events (id, kind, at, detail) VALUES (?1, ?2, ?3, ?4)";
const SELECT_EVENTS_IN_WINDOW: &str = "SELECT id, kind, at, detail FROM control_events \
     WHERE at >= ?1 AND at <= ?2 ORDER BY at, id";

/// What the database says about the pause, exactly as it is stored.
///
/// This does NOT consult the clock, and `Pause::Until` in the past is still `Some`. A tick has to
/// compare the deadline against the one instant it is treating as now, and a store function that
/// read the clock itself would let two calls inside one tick disagree with each other.
pub fn get_pause(conn: &rusqlite::Connection) -> Result<Option<Pause>, StoreError> {
    let mut statement = conn
        .prepare(SELECT_PAUSE)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query([PAUSE_UNTIL, PAUSE_INDEFINITE])
        .map_err(|source| StoreError::Sql { source })?;
    let mut pause_until = None;
    let mut pause_indefinite = None;

    while let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? {
        let key: String = row.get(0).map_err(|source| StoreError::Sql { source })?;
        let value = row
            .get::<_, String>(1)
            .map_err(|source| StoreError::Control {
                subject: key.clone(),
                source: Box::new(source),
            })?;
        if key == PAUSE_UNTIL {
            pause_until = Some(value);
        } else if key == PAUSE_INDEFINITE {
            pause_indefinite = Some(value);
        }
    }

    match (pause_until, pause_indefinite) {
        (Some(_), Some(_)) => Err(StoreError::Control {
            subject: format!("{PAUSE_UNTIL} + {PAUSE_INDEFINITE}"),
            source: invalid_data("both keys are set and only one of them can be true"),
        }),
        (None, Some(value)) => {
            if value == INDEFINITELY {
                Ok(Some(Pause::Indefinite))
            } else {
                Err(StoreError::Control {
                    subject: PAUSE_INDEFINITE.to_owned(),
                    source: invalid_data("the endless pause value is not the expected value"),
                })
            }
        }
        (Some(value), None) => read_timestamp(PAUSE_UNTIL, &value)
            .map(Pause::Until)
            .map(Some),
        (None, None) => Ok(None),
    }
}

/// Record that capture was asked to stop. Whether anything stops is its reader's side:
/// [`get_pause`] answers what was recorded, and a deadline already in the past is stored as given.
///
/// One transaction, because the two keys are one fact: a crash between writing the new key and
/// clearing the old one would leave a pair no reader is allowed to resolve.
pub fn set_pause(
    conn: &mut rusqlite::Connection,
    pause: Pause,
    event_id: ulid::Ulid,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    let (key, value, other, detail) = match pause {
        Pause::Until(until) => {
            let until = timestamp::to_sql(until)?;
            (PAUSE_UNTIL, until.clone(), PAUSE_INDEFINITE, Some(until))
        }
        Pause::Indefinite => (PAUSE_INDEFINITE, INDEFINITELY.to_owned(), PAUSE_UNTIL, None),
    };
    let transaction = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|source| StoreError::Sql { source })?;

    transaction
        .execute(UPSERT_STATE, rusqlite::params![key, value])
        .map_err(|source| StoreError::Sql { source })?;
    transaction
        .execute(DELETE_STATE, [other])
        .map_err(|source| StoreError::Sql { source })?;
    record_event(
        &transaction,
        &ControlEvent {
            id: event_id,
            kind: EventKind::Paused,
            at,
            detail,
        },
    )?;

    transaction
        .commit()
        .map_err(|source| StoreError::Sql { source })
}

/// Clear the recorded pause, and record that starting again was asked for. Whether anything
/// starts is its reader's side: [`get_pause`] answers what remains recorded, and a resume with
/// nothing to resume records the same event.
pub fn resume(
    conn: &mut rusqlite::Connection,
    event_id: ulid::Ulid,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    let transaction = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|source| StoreError::Sql { source })?;

    transaction
        .execute(DELETE_PAUSE, [PAUSE_UNTIL, PAUSE_INDEFINITE])
        .map_err(|source| StoreError::Sql { source })?;
    record_event(
        &transaction,
        &ControlEvent {
            id: event_id,
            kind: EventKind::Resumed,
            at,
            detail: None,
        },
    )?;

    transaction
        .commit()
        .map_err(|source| StoreError::Sql { source })
}

/// Record when something last happened.
pub fn set_health(
    conn: &rusqlite::Connection,
    which: HealthKey,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    conn.execute(
        UPSERT_STATE,
        rusqlite::params![which.key(), timestamp::to_sql(at)?],
    )
    .map_err(|source| StoreError::Sql { source })?;

    Ok(())
}

/// When something last happened, or `None` if it never has.
pub fn get_health(
    conn: &rusqlite::Connection,
    which: HealthKey,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, StoreError> {
    let mut statement = conn
        .prepare(SELECT_STATE)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query([which.key()])
        .map_err(|source| StoreError::Sql { source })?;
    let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? else {
        return Ok(None);
    };
    let value = row
        .get::<_, String>(0)
        .map_err(|source| StoreError::Control {
            subject: which.key().to_owned(),
            source: Box::new(source),
        })?;

    read_timestamp(which.key(), &value).map(Some)
}

/// Append one row to the audit trail.
pub fn record_event(conn: &rusqlite::Connection, event: &ControlEvent) -> Result<(), StoreError> {
    conn.execute(
        INSERT_EVENT,
        rusqlite::params![
            event.id.to_string(),
            event.kind.spelling(),
            timestamp::to_sql(event.at)?,
            event.detail,
        ],
    )
    .map_err(|source| StoreError::RecordEvent {
        id: event.id.to_string(),
        source,
    })?;

    Ok(())
}

/// Every event in `[start, end)`, oldest first.
pub fn events_in_window(
    conn: &rusqlite::Connection,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<ControlEvent>, StoreError> {
    // An empty or reversed window is empty whatever its bounds spell, and answering it does not
    // require them to be spellable at all.
    if end <= start {
        return Ok(Vec::new());
    }

    // The window's last instant, which is what the statement compares against; asking for it rather
    // than for `end` is also what lets a window end where no spelling exists, as year 10000 does.
    // The guard above leaves `end` later than the earliest instant chrono has, so the subtraction
    // cannot fail. `checked_sub_signed` rather than `-` because `-` answers that case with a panic
    // instead of an empty window.
    let Some(last) = end.checked_sub_signed(chrono::TimeDelta::nanoseconds(1)) else {
        return Ok(Vec::new());
    };
    let mut statement = conn
        .prepare(SELECT_EVENTS_IN_WINDOW)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query(rusqlite::params![
            timestamp::to_sql(start)?,
            timestamp::to_sql(last)?,
        ])
        .map_err(|source| StoreError::Sql { source })?;
    let mut events = Vec::new();

    while let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? {
        events.push(event_from_row(row)?);
    }

    Ok(events)
}

fn read_timestamp(subject: &str, value: &str) -> Result<chrono::DateTime<chrono::Utc>, StoreError> {
    timestamp::from_sql(value).map_err(|source| StoreError::Control {
        subject: subject.to_owned(),
        source,
    })
}

fn invalid_data(message: &'static str) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

/// Rebuild a control event from one row.
///
/// The id is read as text first and kept, because it is what names the row in every error below —
/// including the one for an id that is not a ULID.
fn event_from_row(row: &rusqlite::Row<'_>) -> Result<ControlEvent, StoreError> {
    let id: String = row.get(0).map_err(|source| StoreError::Sql { source })?;
    decode_event(row).map_err(|source| StoreError::Control {
        subject: id,
        source,
    })
}

/// Everything about a row that can be wrong in a way SQLite cannot see.
fn decode_event(
    row: &rusqlite::Row<'_>,
) -> Result<ControlEvent, Box<dyn std::error::Error + Send + Sync>> {
    let stored_id: String = row.get(0)?;
    let id = ulid::Ulid::from_string(&stored_id)?;
    if id.to_string() != stored_id {
        return Err(invalid_data(
            "the id is not spelled the way this program writes a ULID",
        ));
    }
    let kind: String = row.get(1)?;
    let kind = EventKind::from_spelling(&kind)
        .ok_or_else(|| invalid_data("the control event kind is not known to this build"))?;
    let at: String = row.get(2)?;
    let at = timestamp::from_sql(&at)?;
    let detail = row.get(3)?;

    Ok(ControlEvent {
        id,
        kind,
        at,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ControlEvent, EventKind, HealthKey, INSERT_EVENT, PAUSE_INDEFINITE, PAUSE_UNTIL, Pause,
        UPSERT_STATE, events_in_window, get_health, get_pause, record_event, resume, set_health,
        set_pause,
    };
    use crate::{StoreError, db, timestamp};
    use chrono::{DateTime, TimeDelta, TimeZone, Utc};
    use tempfile::{TempDir, tempdir};

    const COUNT_PAUSE_KEYS: &str = "SELECT count(*) FROM control_state WHERE key IN (?1, ?2)";

    fn database() -> (TempDir, rusqlite::Connection) {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        (dir, conn)
    }

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
        TimeZone::with_ymd_and_hms(&Utc, year, month, day, hour, minute, second)
            .single()
            .expect("the test timestamp should be valid")
    }

    fn pause_key_count(conn: &rusqlite::Connection) -> i64 {
        conn.query_row(COUNT_PAUSE_KEYS, [PAUSE_UNTIL, PAUSE_INDEFINITE], |row| {
            row.get(0)
        })
        .expect("the pause key count should be readable")
    }

    fn assert_control_subject(error: StoreError, expected: &str) {
        match error {
            StoreError::Control { subject, .. } => assert_eq!(subject, expected),
            other => panic!("expected Control, got {other:?}"),
        }
    }

    #[test]
    fn a_pause_deadline_survives_the_round_trip() {
        let (_dir, mut conn) = database();
        let deadline = at(2026, 8, 1, 12, 30, 0);

        set_pause(
            &mut conn,
            Pause::Until(deadline),
            ulid::Ulid::generate(),
            at(2026, 7, 30, 12, 0, 0),
        )
        .expect("the deadline pause should be stored");

        assert_eq!(
            get_pause(&conn).expect("the pause should be readable"),
            Some(Pause::Until(deadline))
        );
    }

    #[test]
    fn an_endless_pause_survives_the_round_trip() {
        let (_dir, mut conn) = database();

        set_pause(
            &mut conn,
            Pause::Indefinite,
            ulid::Ulid::generate(),
            at(2026, 7, 30, 12, 0, 0),
        )
        .expect("the endless pause should be stored");

        assert_eq!(
            get_pause(&conn).expect("the pause should be readable"),
            Some(Pause::Indefinite)
        );
    }

    #[test]
    fn resuming_clears_the_pause() {
        let (_dir, mut conn) = database();
        set_pause(
            &mut conn,
            Pause::Indefinite,
            ulid::Ulid::generate(),
            at(2026, 7, 30, 12, 0, 0),
        )
        .expect("the endless pause should be stored");

        resume(&mut conn, ulid::Ulid::generate(), at(2026, 7, 30, 12, 1, 0))
            .expect("resuming should succeed");

        assert_eq!(
            get_pause(&conn).expect("the cleared pause should be readable"),
            None
        );
    }

    #[test]
    fn resuming_something_already_running_is_not_an_error() {
        let (_dir, mut conn) = database();

        resume(&mut conn, ulid::Ulid::generate(), at(2026, 7, 30, 12, 0, 0))
            .expect("resuming an unpaused capture should succeed");

        assert_eq!(
            get_pause(&conn).expect("the absent pause should be readable"),
            None
        );

        // The request changed nothing, which is exactly why the row matters: the trail records what
        // was asked for, not only what moved.
        let events = events_in_window(&conn, at(2026, 7, 30, 12, 0, 0), at(2026, 7, 30, 12, 0, 1))
            .expect("the audit window should be readable");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Resumed);
        assert_eq!(events[0].detail, None);
    }

    #[test]
    fn asking_again_for_a_pause_already_in_force_records_the_request() {
        let (_dir, mut conn) = database();
        let first_at = at(2026, 7, 30, 12, 0, 0);
        let second_at = at(2026, 7, 30, 12, 1, 0);
        // Fixed ids running against the timestamps, so a swap to `ORDER BY id, at` fails every run.
        let first_id = ulid::Ulid::from(9u128);
        let second_id = ulid::Ulid::from(1u128);

        set_pause(&mut conn, Pause::Indefinite, first_id, first_at)
            .expect("the first endless pause should be stored");
        set_pause(&mut conn, Pause::Indefinite, second_id, second_at)
            .expect("the second endless pause should be stored");

        assert_eq!(
            get_pause(&conn).expect("the endless pause should be readable"),
            Some(Pause::Indefinite)
        );
        assert_eq!(pause_key_count(&conn), 1);

        // The second request moved nothing. Its row exists because the trail records what was
        // asked for, exactly as `EventKind::Paused` documents; nothing else here checks that.
        let events = events_in_window(&conn, first_at, second_at + TimeDelta::seconds(1))
            .expect("the audit window should be readable");
        assert_eq!(
            events,
            [
                ControlEvent {
                    id: first_id,
                    kind: EventKind::Paused,
                    at: first_at,
                    detail: None,
                },
                ControlEvent {
                    id: second_id,
                    kind: EventKind::Paused,
                    at: second_at,
                    detail: None,
                },
            ]
        );
    }

    #[test]
    fn changing_the_kind_of_pause_leaves_only_one_key() {
        let (_dir, mut conn) = database();
        let first_deadline = at(2026, 8, 1, 12, 0, 0);

        set_pause(
            &mut conn,
            Pause::Until(first_deadline),
            ulid::Ulid::generate(),
            at(2026, 7, 30, 12, 0, 0),
        )
        .expect("the deadline pause should be stored");
        set_pause(
            &mut conn,
            Pause::Indefinite,
            ulid::Ulid::generate(),
            at(2026, 7, 30, 12, 1, 0),
        )
        .expect("the endless pause should replace the deadline");

        assert_eq!(pause_key_count(&conn), 1);
        assert_eq!(
            get_pause(&conn).expect("the endless pause should be readable"),
            Some(Pause::Indefinite)
        );

        let second_deadline = at(2026, 8, 2, 12, 0, 0);
        set_pause(
            &mut conn,
            Pause::Until(second_deadline),
            ulid::Ulid::generate(),
            at(2026, 7, 30, 12, 2, 0),
        )
        .expect("the deadline pause should replace the endless pause");

        assert_eq!(pause_key_count(&conn), 1);
        assert_eq!(
            get_pause(&conn).expect("the deadline pause should be readable"),
            Some(Pause::Until(second_deadline))
        );
    }

    #[test]
    fn a_pause_that_cannot_be_recorded_changes_nothing() {
        let (_dir, mut conn) = database();
        let event_id = ulid::Ulid::generate();

        set_pause(
            &mut conn,
            Pause::Indefinite,
            event_id,
            at(2026, 7, 30, 12, 0, 0),
        )
        .expect("the endless pause should be stored");

        // Without the transaction, this state change would land while its audit row did not; this
        // is the only test that can tell that implementation from the transactional one.
        let error = set_pause(
            &mut conn,
            Pause::Until(at(2026, 8, 1, 12, 0, 0)),
            event_id,
            at(2026, 7, 30, 12, 1, 0),
        )
        .expect_err("the duplicate event id should refuse the pause");

        match error {
            StoreError::RecordEvent { id, .. } => assert_eq!(id, event_id.to_string()),
            other => panic!("expected RecordEvent, got {other:?}"),
        }
        assert_eq!(
            get_pause(&conn).expect("the original pause should remain readable"),
            Some(Pause::Indefinite)
        );
        let event_count: i64 = conn
            .query_row("SELECT count(*) FROM control_events", [], |row| row.get(0))
            .expect("the control event count should be readable");
        assert_eq!(event_count, 1);
    }

    #[test]
    fn a_resume_that_cannot_be_recorded_changes_nothing() {
        let (_dir, mut conn) = database();
        let event_id = ulid::Ulid::generate();

        set_pause(
            &mut conn,
            Pause::Indefinite,
            event_id,
            at(2026, 7, 30, 12, 0, 0),
        )
        .expect("the endless pause should be stored");

        // This is the counterpart of `a_pause_that_cannot_be_recorded_changes_nothing`; without
        // the transaction, the pause would be gone with nothing recording that it went.
        let error = resume(&mut conn, event_id, at(2026, 7, 30, 12, 1, 0))
            .expect_err("the duplicate event id should refuse the resume");

        match error {
            StoreError::RecordEvent { id, .. } => assert_eq!(id, event_id.to_string()),
            other => panic!("expected RecordEvent, got {other:?}"),
        }
        assert_eq!(
            get_pause(&conn).expect("the original pause should remain readable"),
            Some(Pause::Indefinite)
        );
        let event_count: i64 = conn
            .query_row("SELECT count(*) FROM control_events", [], |row| row.get(0))
            .expect("the control event count should be readable");
        assert_eq!(event_count, 1);
    }

    #[test]
    fn a_pause_that_is_both_kinds_at_once_is_refused() {
        let (_dir, conn) = database();

        // Public functions cannot produce this contradictory state, so write it with plain SQL.
        conn.execute(
            UPSERT_STATE,
            rusqlite::params![
                PAUSE_UNTIL,
                timestamp::to_sql(at(2026, 8, 1, 12, 0, 0))
                    .expect("the test deadline should be spellable")
            ],
        )
        .expect("the deadline row should be writable");
        conn.execute(
            UPSERT_STATE,
            rusqlite::params![PAUSE_INDEFINITE, super::INDEFINITELY],
        )
        .expect("the endless row should be writable");

        let error = get_pause(&conn).expect_err("the contradictory pause should be refused");
        assert!(
            error
                .to_string()
                .contains("both keys are set and only one of them can be true")
        );
        assert_control_subject(error, &format!("{PAUSE_UNTIL} + {PAUSE_INDEFINITE}"));
    }

    #[test]
    fn an_endless_pause_spelled_any_other_way_is_refused() {
        let (_dir, conn) = database();

        // Public functions cannot produce these values, so write them with plain SQL.
        for value in [Some("0"), Some("yes"), None] {
            conn.execute(UPSERT_STATE, rusqlite::params![PAUSE_INDEFINITE, value])
                .expect("the malformed endless row should be writable");

            let error =
                get_pause(&conn).expect_err("the malformed endless pause should be refused");
            assert_control_subject(error, PAUSE_INDEFINITE);
        }
    }

    #[test]
    fn a_deadline_that_is_not_a_timestamp_is_refused() {
        let (_dir, conn) = database();

        // Public functions cannot produce this value, so write it with plain SQL.
        conn.execute(
            UPSERT_STATE,
            rusqlite::params![PAUSE_UNTIL, "not a timestamp"],
        )
        .expect("the malformed deadline row should be writable");

        let error = get_pause(&conn).expect_err("the malformed deadline should be refused");
        assert_control_subject(error, PAUSE_UNTIL);
    }

    #[test]
    fn a_deadline_in_the_past_is_still_what_is_stored() {
        let (_dir, mut conn) = database();
        let deadline = at(2020, 1, 1, 0, 0, 0);

        set_pause(
            &mut conn,
            Pause::Until(deadline),
            ulid::Ulid::generate(),
            at(2026, 7, 30, 12, 0, 0),
        )
        .expect("the past deadline should be stored");

        // Deciding whether this pause has expired belongs to the caller holding the tick's instant.
        assert_eq!(
            get_pause(&conn).expect("the past deadline should be readable"),
            Some(Pause::Until(deadline))
        );
    }

    #[test]
    fn every_health_mark_is_separate_and_absent_until_written() {
        let (_dir, conn) = database();
        let marks = [
            (HealthKey::LastTick, at(2026, 7, 30, 12, 0, 0)),
            (HealthKey::LastCapture, at(2026, 7, 30, 12, 1, 0)),
            (HealthKey::LastDelivery, at(2026, 7, 30, 12, 2, 0)),
        ];

        for (which, _) in marks {
            assert_eq!(
                get_health(&conn, which).expect("the absent health mark should be readable"),
                None
            );
        }
        for (which, instant) in marks {
            set_health(&conn, which, instant).expect("the health mark should be stored");
        }
        for (which, instant) in marks {
            assert_eq!(
                get_health(&conn, which).expect("the health mark should be readable"),
                Some(instant)
            );
        }

        let newer_tick = at(2026, 7, 30, 12, 3, 0);
        set_health(&conn, HealthKey::LastTick, newer_tick)
            .expect("the tick mark should be updated");
        assert_eq!(
            get_health(&conn, HealthKey::LastTick).expect("the tick mark should be readable"),
            Some(newer_tick)
        );
        assert_eq!(
            get_health(&conn, HealthKey::LastCapture)
                .expect("the capture mark should remain readable"),
            Some(marks[1].1)
        );
        assert_eq!(
            get_health(&conn, HealthKey::LastDelivery)
                .expect("the delivery mark should remain readable"),
            Some(marks[2].1)
        );
    }

    #[test]
    fn a_health_mark_that_is_not_a_timestamp_is_refused() {
        let (_dir, conn) = database();

        // Public functions cannot produce this value, so write it with plain SQL.
        conn.execute(
            UPSERT_STATE,
            rusqlite::params![HealthKey::LastTick.key(), "not a timestamp"],
        )
        .expect("the malformed health row should be writable");

        let error = get_health(&conn, HealthKey::LastTick)
            .expect_err("the malformed health mark should be refused");
        assert_control_subject(error, HealthKey::LastTick.key());
    }

    #[test]
    fn pause_and_resume_leave_their_own_audit_rows_in_time_order() {
        let (_dir, mut conn) = database();
        let paused_at = at(2026, 7, 30, 12, 1, 0);
        let resumed_at = at(2026, 7, 30, 12, 2, 0);
        let deadline = at(2026, 7, 30, 13, 0, 0);
        // Fixed ids running against the timestamps, so a swap to `ORDER BY id, at` fails every run.
        let pause_id = ulid::Ulid::from(9u128);
        let resume_id = ulid::Ulid::from(1u128);

        set_pause(&mut conn, Pause::Until(deadline), pause_id, paused_at)
            .expect("the pause and its event should be stored");
        resume(&mut conn, resume_id, resumed_at)
            .expect("the resume and its event should be stored");

        // This pause is a deadline rather than an endless one here. Without this line, an
        // implementation clearing only `pause_indefinite` passes every test in this file.
        assert!(matches!(get_pause(&conn), Ok(None)));

        let events = events_in_window(&conn, at(2026, 7, 30, 12, 0, 0), at(2026, 7, 30, 12, 3, 0))
            .expect("the audit window should be readable");
        assert_eq!(
            events,
            [
                ControlEvent {
                    id: pause_id,
                    kind: EventKind::Paused,
                    at: paused_at,
                    detail: Some(
                        timestamp::to_sql(deadline)
                            .expect("the expected deadline should be spellable")
                    ),
                },
                ControlEvent {
                    id: resume_id,
                    kind: EventKind::Resumed,
                    at: resumed_at,
                    detail: None,
                },
            ]
        );
    }

    #[test]
    fn two_events_at_one_instant_come_back_in_id_order() {
        let (_dir, conn) = database();
        let higher_id = ulid::Ulid::from(2u128);
        let lower_id = ulid::Ulid::from(1u128);
        assert!(higher_id > lower_id);
        let instant = at(2026, 7, 30, 12, 0, 0);
        let higher = ControlEvent {
            id: higher_id,
            kind: EventKind::BlacklistSkip,
            at: instant,
            detail: None,
        };
        let lower = ControlEvent {
            id: lower_id,
            kind: EventKind::BlacklistSkip,
            at: instant,
            detail: None,
        };

        record_event(&conn, &higher).expect("the higher-id audit event should be stored first");
        record_event(&conn, &lower).expect("the lower-id audit event should be stored second");
        // Without `, id` in the ORDER BY these come back in whatever order the query plan produced,
        // which is the one thing a stable listing has to rule out.
        let events = events_in_window(&conn, instant, instant + TimeDelta::seconds(1))
            .expect("the tied audit events should be readable");

        assert_eq!(events, [lower, higher]);
    }

    #[test]
    fn an_empty_or_reversed_event_window_is_empty_rather_than_an_error() {
        let (_dir, conn) = database();
        let t = at(2026, 7, 30, 12, 34, 56);
        let event = ControlEvent {
            id: ulid::Ulid::generate(),
            kind: EventKind::BlacklistSkip,
            at: t,
            detail: None,
        };

        record_event(&conn, &event).expect("the ordinary event should be stored");

        let empty = events_in_window(&conn, t, t).expect("the empty window should be readable");
        assert_eq!(empty, Vec::new());
        let reversed = events_in_window(&conn, t + TimeDelta::seconds(1), t)
            .expect("the reversed window should be readable");
        assert_eq!(reversed, Vec::new());
        let unspellable =
            events_in_window(&conn, DateTime::<Utc>::MAX_UTC, DateTime::<Utc>::MAX_UTC)
                .expect("the unspellable empty window should be readable");
        assert_eq!(unspellable, Vec::new());

        // Reversed *and* unspellable, which is the only combination that needs the guard: with the
        // start at MAX_UTC, anything reaching `to_sql(start)` answers TimestampOutOfRange where the
        // contract says empty. Each half alone is covered above and neither half alone would notice
        // the guard weakening to `end == start`.
        let reversed_and_unspellable = events_in_window(&conn, DateTime::<Utc>::MAX_UTC, t)
            .expect("the reversed unspellable window should be readable");
        assert_eq!(reversed_and_unspellable, Vec::new());
    }

    #[test]
    fn adjacent_event_windows_tile_without_sharing_an_event() {
        let (_dir, conn) = database();
        // Fixed ids running against the timestamps, so a swap to `ORDER BY id, at` fails every run.
        // The rows carry `Some(detail)` and are compared whole, so a stored detail's round-trip is
        // pinned here too.
        let first = ControlEvent {
            id: ulid::Ulid::from(4u128),
            kind: EventKind::BlacklistSkip,
            at: at(2026, 7, 30, 12, 1, 0),
            detail: Some("first.exe".to_owned()),
        };
        let middle = ControlEvent {
            id: ulid::Ulid::from(2u128),
            kind: EventKind::BlacklistSkip,
            at: at(2026, 7, 30, 12, 2, 0),
            detail: Some("middle.exe".to_owned()),
        };
        // The boundary row is what makes this test tell `<= end - 1 ns` apart from `< end - 1 ns`.
        let boundary = ControlEvent {
            id: ulid::Ulid::from(3u128),
            kind: EventKind::BlacklistSkip,
            at: middle
                .at
                .checked_sub_signed(chrono::TimeDelta::nanoseconds(1))
                .expect("the boundary timestamp should be representable"),
            detail: Some("boundary.exe".to_owned()),
        };
        let last = ControlEvent {
            id: ulid::Ulid::from(1u128),
            kind: EventKind::BlacklistSkip,
            at: at(2026, 7, 30, 12, 3, 0),
            detail: Some("last.exe".to_owned()),
        };

        for event in [&last, &first, &middle, &boundary] {
            record_event(&conn, event).expect("the audit event should be stored");
        }
        let earlier = events_in_window(&conn, at(2026, 7, 30, 12, 0, 0), middle.at)
            .expect("the earlier window should be readable");
        let later = events_in_window(&conn, middle.at, at(2026, 7, 30, 12, 4, 0))
            .expect("the later window should be readable");

        assert_eq!(earlier, [first.clone(), boundary.clone()]);
        assert_eq!(later, [middle.clone(), last.clone()]);
        let mut tiled = earlier;
        tiled.extend(later);
        assert_eq!(tiled, [first, boundary, middle, last]);
    }

    #[test]
    fn the_last_event_the_schema_can_spell_is_inside_a_window_that_finds_it() {
        let (_dir, conn) = database();
        let event = ControlEvent {
            id: ulid::Ulid::generate(),
            kind: EventKind::BlacklistSkip,
            at: at(9999, 12, 31, 23, 59, 59) + TimeDelta::nanoseconds(999_999_999),
            detail: None,
        };
        let start = at(9999, 12, 31, 23, 55, 0);
        let end = Utc
            .with_ymd_and_hms(10_000, 1, 1, 0, 0, 0)
            .single()
            .expect("year 10000 should be valid");

        record_event(&conn, &event).expect("the last spellable event should be stored");
        // The exclusive end has no spelling of its own. This is what tells `end - 1 ns` apart
        // from comparing against `end` itself.
        let events = events_in_window(&conn, start, end)
            .expect("the last spellable event should be searchable");
        assert_eq!(events, [event]);

        let events = events_in_window(&conn, DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MIN_UTC)
            .expect("a window ending at the earliest instant should be empty");
        assert!(events.is_empty());
    }

    #[test]
    fn an_event_kind_this_build_does_not_know_is_refused() {
        let (_dir, conn) = database();
        let id = ulid::Ulid::generate().to_string();
        let at = at(2026, 7, 30, 12, 0, 0);

        // Public functions cannot produce this kind, so write it with plain SQL.
        conn.execute(
            INSERT_EVENT,
            rusqlite::params![
                &id,
                "future_event",
                timestamp::to_sql(at).expect("the event timestamp should be spellable"),
                Option::<String>::None,
            ],
        )
        .expect("the unknown event row should be writable");

        let error = events_in_window(&conn, at, at + TimeDelta::seconds(1))
            .expect_err("the unknown event kind should be refused");
        assert_control_subject(error, &id);
    }

    #[test]
    fn an_event_id_spelled_any_other_way_is_refused() {
        let (_dir, conn) = database();
        let stored_id = "0000000000000128ggyhyyk08n";
        let at = at(2026, 7, 30, 12, 0, 0);

        // This lower-cased spelling of a canonical ULID cannot be produced by this program, so
        // write it with plain SQL; every other column uses its canonical spelling.
        conn.execute(
            INSERT_EVENT,
            rusqlite::params![
                stored_id,
                EventKind::BlacklistSkip.spelling(),
                timestamp::to_sql(at).expect("the event timestamp should be spellable"),
                Option::<String>::None,
            ],
        )
        .expect("the lower-cased event id should be writable");

        let error = events_in_window(&conn, at, at + TimeDelta::seconds(1))
            .expect_err("the lower-cased event id should be refused");
        assert_control_subject(error, stored_id);
    }
}
