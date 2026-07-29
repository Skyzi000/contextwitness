//! Pause state, health marks and the audit trail the CLI, the tray and the daemon share.

use crate::{StoreError, timestamp};

/// Whether capture is stopped, and until when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// Stopped until this instant, and running by itself afterwards.
    Until(chrono::DateTime<chrono::Utc>),
    /// Stopped with no end, until somebody resumes.
    Indefinite,
}

/// A moment `contextwitness status` reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthKey {
    /// When the capture loop last ran.
    LastTick,
    /// When a frame was last stored.
    LastCapture,
    /// When an episode was last delivered.
    LastDelivery,
}

/// What an audit row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Capture was stopped.
    Paused,
    /// Capture was started again.
    Resumed,
    /// A tick captured nothing because the foreground process is blacklisted.
    BlacklistSkip,
}

/// One row of the audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlEvent {
    /// Primary key.
    pub id: ulid::Ulid,
    /// What happened.
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
        let value =
            row.get::<_, rusqlite::types::Value>(1)
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
            if text_value(PAUSE_INDEFINITE, value)? == INDEFINITELY {
                Ok(Some(Pause::Indefinite))
            } else {
                Err(StoreError::Control {
                    subject: PAUSE_INDEFINITE.to_owned(),
                    source: invalid_data("the endless pause value is not the expected value"),
                })
            }
        }
        (Some(value), None) => read_timestamp(PAUSE_UNTIL, value)
            .map(Pause::Until)
            .map(Some),
        (None, None) => Ok(None),
    }
}

/// Stop capture, and record that it happened.
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

/// Start capture again, and record that it happened.
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
        .get::<_, rusqlite::types::Value>(0)
        .map_err(|source| StoreError::Control {
            subject: which.key().to_owned(),
            source: Box::new(source),
        })?;

    read_timestamp(which.key(), value).map(Some)
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

    // The window's last instant, which is what the statement compares against. `checked_sub_signed`
    // rather than `-`: subtracting from the earliest instant chrono has would panic, and a window
    // ending there holds nothing.
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

fn text_value(subject: &str, value: rusqlite::types::Value) -> Result<String, StoreError> {
    match value {
        rusqlite::types::Value::Text(value) => Ok(value),
        rusqlite::types::Value::Null => Err(StoreError::Control {
            subject: subject.to_owned(),
            source: invalid_data("the control state value is NULL"),
        }),
        _ => Err(StoreError::Control {
            subject: subject.to_owned(),
            source: invalid_data("the control state value is not TEXT"),
        }),
    }
}

fn read_timestamp(
    subject: &str,
    value: rusqlite::types::Value,
) -> Result<chrono::DateTime<chrono::Utc>, StoreError> {
    let value = text_value(subject, value)?;
    timestamp::from_sql(&value).map_err(|source| StoreError::Control {
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
    let id: String = row.get(0)?;
    let id = ulid::Ulid::from_string(&id)?;
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
            ulid::Ulid::new(),
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
            ulid::Ulid::new(),
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
            ulid::Ulid::new(),
            at(2026, 7, 30, 12, 0, 0),
        )
        .expect("the endless pause should be stored");

        resume(&mut conn, ulid::Ulid::new(), at(2026, 7, 30, 12, 1, 0))
            .expect("resuming should succeed");

        assert_eq!(
            get_pause(&conn).expect("the cleared pause should be readable"),
            None
        );
    }

    #[test]
    fn resuming_something_already_running_is_not_an_error() {
        let (_dir, mut conn) = database();

        resume(&mut conn, ulid::Ulid::new(), at(2026, 7, 30, 12, 0, 0))
            .expect("resuming an unpaused capture should succeed");

        assert_eq!(
            get_pause(&conn).expect("the absent pause should be readable"),
            None
        );
    }

    #[test]
    fn changing_the_kind_of_pause_leaves_only_one_key() {
        let (_dir, mut conn) = database();
        let first_deadline = at(2026, 8, 1, 12, 0, 0);

        set_pause(
            &mut conn,
            Pause::Until(first_deadline),
            ulid::Ulid::new(),
            at(2026, 7, 30, 12, 0, 0),
        )
        .expect("the deadline pause should be stored");
        set_pause(
            &mut conn,
            Pause::Indefinite,
            ulid::Ulid::new(),
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
            ulid::Ulid::new(),
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
        let event_id = ulid::Ulid::new();

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
            ulid::Ulid::new(),
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
        let pause_id = ulid::Ulid::new();
        let resume_id = ulid::Ulid::new();

        set_pause(&mut conn, Pause::Until(deadline), pause_id, paused_at)
            .expect("the pause and its event should be stored");
        resume(&mut conn, resume_id, resumed_at)
            .expect("the resume and its event should be stored");

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
    fn a_blacklist_skip_is_recorded_with_its_detail() {
        let (_dir, conn) = database();
        let event = ControlEvent {
            id: ulid::Ulid::new(),
            kind: EventKind::BlacklistSkip,
            at: at(2026, 7, 30, 12, 0, 0),
            detail: Some("notepad.exe".to_owned()),
        };

        record_event(&conn, &event).expect("the blacklist skip should be stored");
        let events = events_in_window(&conn, event.at, event.at + TimeDelta::seconds(1))
            .expect("the audit window should be readable");

        assert_eq!(events, [event]);
    }

    #[test]
    fn adjacent_event_windows_tile_without_sharing_an_event() {
        let (_dir, conn) = database();
        let first = ControlEvent {
            id: ulid::Ulid::new(),
            kind: EventKind::BlacklistSkip,
            at: at(2026, 7, 30, 12, 1, 0),
            detail: Some("first.exe".to_owned()),
        };
        let middle = ControlEvent {
            id: ulid::Ulid::new(),
            kind: EventKind::BlacklistSkip,
            at: at(2026, 7, 30, 12, 2, 0),
            detail: Some("middle.exe".to_owned()),
        };
        let last = ControlEvent {
            id: ulid::Ulid::new(),
            kind: EventKind::BlacklistSkip,
            at: at(2026, 7, 30, 12, 3, 0),
            detail: Some("last.exe".to_owned()),
        };

        for event in [&last, &first, &middle] {
            record_event(&conn, event).expect("the audit event should be stored");
        }
        let earlier = events_in_window(&conn, at(2026, 7, 30, 12, 0, 0), middle.at)
            .expect("the earlier window should be readable");
        let later = events_in_window(&conn, middle.at, at(2026, 7, 30, 12, 4, 0))
            .expect("the later window should be readable");

        assert_eq!(earlier, std::slice::from_ref(&first));
        assert_eq!(later, [middle.clone(), last.clone()]);
        let mut tiled = earlier;
        tiled.extend(later);
        assert_eq!(tiled, [first, middle, last]);
    }

    #[test]
    fn an_event_kind_this_build_does_not_know_is_refused() {
        let (_dir, conn) = database();
        let id = ulid::Ulid::new().to_string();
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
}
