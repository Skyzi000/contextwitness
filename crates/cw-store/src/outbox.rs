use crate::{StoreError, timestamp};

/// The first retry waits this long, and every further one waits twice its predecessor up to
/// [`MAX_DELAY_SECONDS`] (plan Task 20).
const FIRST_DELAY_SECONDS: i64 = 30;
const MAX_DELAY_SECONDS: i64 = 900;

const SELECT_DUE: &str = "SELECT o.episode_id, o.attempts, e.document_id, e.end_at, e.content, \
     e.metadata_json FROM outbox o JOIN episodes e ON e.id = o.episode_id \
     WHERE o.state IN ('pending', 'failed') AND o.next_attempt_at IS NOT NULL \
     AND o.next_attempt_at <= ?1 ORDER BY o.next_attempt_at, e.start_at, e.id LIMIT ?2";
const CLAIM: &str = "UPDATE outbox SET state = 'delivering' \
     WHERE episode_id = ?1 AND state IN ('pending', 'failed')";
const MARK_DELIVERED: &str = "UPDATE outbox \
     SET state = 'delivered', next_attempt_at = NULL, last_error = NULL, last_note = NULL \
     WHERE episode_id = ?1";
const SELECT_ATTEMPTS: &str = "SELECT attempts FROM outbox WHERE episode_id = ?1";
const MARK_FAILED: &str = "UPDATE outbox \
     SET state = 'failed', attempts = ?2, next_attempt_at = ?3, last_error = ?4, \
     last_note = NULL WHERE episode_id = ?1";
const MARK_WAITING_ERROR: &str = "UPDATE outbox \
     SET state = 'pending', next_attempt_at = ?2, last_error = ?3, last_note = NULL \
     WHERE episode_id = ?1";
const MARK_WAITING_PROGRESS: &str = "UPDATE outbox \
     SET state = 'pending', next_attempt_at = ?2, last_note = ?3, last_error = NULL \
     WHERE episode_id = ?1";
const REQUEUE_DELIVERING: &str = "UPDATE outbox SET state = 'pending' WHERE state = 'delivering'";
const COUNT_BY_STATE: &str = "SELECT state, count(*) FROM outbox GROUP BY state ORDER BY state";
const NEWEST_ERROR: &str = "SELECT e.document_id, o.state, o.last_error, \
     (SELECT count(*) FROM outbox WHERE last_error IS NOT NULL) \
     FROM outbox o JOIN episodes e ON e.id = o.episode_id \
     WHERE o.last_error IS NOT NULL ORDER BY e.start_at DESC, e.id DESC LIMIT 1";

/// An entry whose turn has come, carrying the episode snapshot the delivery worker sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    /// Primary key of both rows.
    pub episode_id: ulid::Ulid,
    pub document_id: String,
    /// End of the window, which is the `timestamp` the memory is retained under.
    pub end_at: chrono::DateTime<chrono::Utc>,
    /// The delivery body, exactly as it was snapshotted.
    pub content: String,
    /// The retain metadata, still as the JSON text it was snapshotted as: it is already wire
    /// format, and decoding and re-encoding it here would be rebuilding the snapshot.
    pub metadata_json: String,
    /// Attempts made so far, this one not counted.
    pub attempts: u32,
}

/// What [`mark_failed`] should schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Wait [`backoff_delay`] for the attempt count this failure produces.
    Backoff,
    /// Wait until this instant, which a server asked for.
    At(chrono::DateTime<chrono::Utc>),
    /// Do not try again: the entry stays `failed` and no query finds it due.
    Never,
}

/// How long a retry waits after `attempts` failed attempts: 30s, 60s, … capped at 15 minutes.
pub fn backoff_delay(attempts: u32) -> chrono::TimeDelta {
    // The shift is clamped before it runs: a wide shift wraps to a negative count, and
    // `TimeDelta::seconds` answers that with a panic.
    let seconds = FIRST_DELAY_SECONDS << attempts.saturating_sub(1).min(30);

    chrono::TimeDelta::seconds(seconds.min(MAX_DELAY_SECONDS))
}

/// Entries waiting for a delivery attempt that is due at `now`, earliest due instant first;
/// the episode window and id only break ties. Ordered by window instead, a parked backlog of
/// old episodes would outrank every younger episode on every pass and starve fresh work.
pub fn fetch_due(
    conn: &rusqlite::Connection,
    now: chrono::DateTime<chrono::Utc>,
    limit: u32,
) -> Result<Vec<Due>, StoreError> {
    let mut statement = conn
        .prepare(SELECT_DUE)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query(rusqlite::params![timestamp::to_sql(now)?, i64::from(limit)])
        .map_err(|source| StoreError::Sql { source })?;
    let mut due = Vec::new();

    while let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? {
        due.push(from_row(row)?);
    }

    Ok(due)
}

/// Claim an entry for an attempt. `false` when nothing was claimed, because the entry is already
/// being delivered, is delivered, or is gone.
pub fn mark_delivering(
    conn: &rusqlite::Connection,
    episode_id: ulid::Ulid,
) -> Result<bool, StoreError> {
    let claimed = conn
        .execute(CLAIM, [episode_id.to_string()])
        .map_err(|source| StoreError::Sql { source })?;

    Ok(claimed == 1)
}

/// Record that the episode reached Hindsight. Terminal: nothing finds the entry due again.
pub fn mark_delivered(
    conn: &rusqlite::Connection,
    episode_id: ulid::Ulid,
) -> Result<(), StoreError> {
    conn.execute(MARK_DELIVERED, [episode_id.to_string()])
        .map_err(|source| StoreError::Sql { source })?;

    Ok(())
}

/// Record a failed attempt: one more attempt counted, `retry` deciding when the next one is due,
/// and `last_error` kept as the caller spelled it.
pub fn mark_failed(
    conn: &mut rusqlite::Connection,
    episode_id: ulid::Ulid,
    at: chrono::DateTime<chrono::Utc>,
    retry: Retry,
    last_error: &str,
) -> Result<(), StoreError> {
    let id_text = episode_id.to_string();
    // The count is read and written under one lock: a count read outside the transaction can be
    // stale by the time the delay is computed from it.
    let transaction = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|source| StoreError::Sql { source })?;
    let attempts: i64 = transaction
        .query_one(SELECT_ATTEMPTS, [id_text.as_str()], |row| row.get(0))
        .map_err(|source| StoreError::Sql { source })?;
    let attempts = attempts.saturating_add(1);
    let next_attempt_at = match retry {
        Retry::Never => None,
        Retry::At(instant) => Some(instant),
        Retry::Backoff => Some(
            at.checked_add_signed(backoff_delay(u32::try_from(attempts).unwrap_or(u32::MAX)))
                .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC),
        ),
    };
    let next_attempt_at = next_attempt_at.map(timestamp::to_sql).transpose()?;

    transaction
        .execute(
            MARK_FAILED,
            rusqlite::params![id_text, attempts, next_attempt_at, last_error],
        )
        .map_err(|source| StoreError::Sql { source })?;

    transaction
        .commit()
        .map_err(|source| StoreError::Sql { source })
}

/// What a parked entry has to say for itself. The two kinds land in different columns, and
/// each write clears the other: a healthy in-flight note must never answer as an error, and a
/// note superseding an error means that error is resolved.
#[derive(Debug, Clone, Copy)]
pub enum Note<'a> {
    /// Healthy in-flight work — carries the operation id so a wedged operation stays
    /// addressable from the database.
    Progress(&'a str),
    /// A failure the operator should read, e.g. a server-side failed operation under watch.
    Error(&'a str),
}

/// Park the entry until `until` with no attempt counted: the caller is coming back to look
/// again — at in-flight work, or at a server-side failure awaiting its retry — and looking is
/// not a delivery attempt.
pub fn mark_waiting(
    conn: &rusqlite::Connection,
    episode_id: ulid::Ulid,
    until: chrono::DateTime<chrono::Utc>,
    note: Note<'_>,
) -> Result<(), StoreError> {
    let (sql, text) = match note {
        Note::Progress(text) => (MARK_WAITING_PROGRESS, text),
        Note::Error(text) => (MARK_WAITING_ERROR, text),
    };
    conn.execute(
        sql,
        rusqlite::params![episode_id.to_string(), timestamp::to_sql(until)?, text],
    )
    .map_err(|source| StoreError::Sql { source })?;

    Ok(())
}

/// Put every entry a previous run left mid-flight back in the queue, and answer how many there
/// were. A `delivering` row is an attempt whose outcome nobody recorded; its redelivery opens
/// with a status question on the entry's fixed operation id, so what that costs is a lookup,
/// not a duplicate submission.
pub fn requeue_delivering(conn: &rusqlite::Connection) -> Result<usize, StoreError> {
    conn.execute(REQUEUE_DELIVERING, [])
        .map_err(|source| StoreError::Sql { source })
}

/// How many entries sit in each state, in state order, for `contextwitness status`. A state is
/// reported as its row spells it rather than checked against the four this build writes: the
/// answer is a report, and a value nobody here writes is the one worth seeing.
pub fn counts_by_state(conn: &rusqlite::Connection) -> Result<Vec<(String, i64)>, StoreError> {
    let mut statement = conn
        .prepare(COUNT_BY_STATE)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query([])
        .map_err(|source| StoreError::Sql { source })?;
    let mut counts = Vec::new();

    while let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? {
        let state = row.get(0).map_err(|source| StoreError::Sql { source })?;
        let count = row.get(1).map_err(|source| StoreError::Sql { source })?;
        counts.push((state, count));
    }

    Ok(counts)
}

/// The latest affected window still carrying a delivery error, for `contextwitness status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewestError {
    pub document_id: String,
    pub state: String,
    pub message: String,
    /// How many entries currently carry an error, this one counted.
    pub entries: i64,
}

/// The table records no error instant, so "newest" is the latest affected window, not necessarily
/// the last error written. Ordered by window rather than by id: ids come from `Ulid::generate`,
/// which does not order within a millisecond, and a catch-up pass closes several windows in one.
pub fn newest_error(conn: &rusqlite::Connection) -> Result<Option<NewestError>, StoreError> {
    let mut statement = conn
        .prepare(NEWEST_ERROR)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query([])
        .map_err(|source| StoreError::Sql { source })?;
    let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? else {
        return Ok(None);
    };
    let read = |index| row.get(index).map_err(|source| StoreError::Sql { source });
    Ok(Some(NewestError {
        document_id: read(0)?,
        state: read(1)?,
        message: read(2)?,
        entries: row.get(3).map_err(|source| StoreError::Sql { source })?,
    }))
}

fn from_row(row: &rusqlite::Row<'_>) -> Result<Due, StoreError> {
    let id: String = row.get(0).map_err(|source| StoreError::Sql { source })?;
    decode(row).map_err(|source| StoreError::Encoding { id, source })
}

fn decode(row: &rusqlite::Row<'_>) -> Result<Due, Box<dyn std::error::Error + Send + Sync>> {
    let stored_id: String = row.get(0)?;
    let episode_id = ulid::Ulid::from_string(&stored_id)?;
    if episode_id.to_string() != stored_id {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the id is not spelled the way this program writes a ULID",
        )));
    }
    let attempts = u32::try_from(row.get::<_, i64>(1)?)?;
    let document_id: String = row.get(2)?;
    let end_at: String = row.get(3)?;
    let end_at = timestamp::from_sql(&end_at)?;
    let content: String = row.get(4)?;
    let metadata_json: String = row.get(5)?;

    Ok(Due {
        episode_id,
        document_id,
        end_at,
        content,
        metadata_json,
        attempts,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Note, Retry, counts_by_state, fetch_due, mark_delivered, mark_delivering, mark_failed,
        mark_waiting, newest_error, requeue_delivering,
    };
    use crate::{db, episodes};
    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    fn test_episode(
        start_at: chrono::DateTime<Utc>,
        document_id: &str,
    ) -> cw_core::episode::Episode {
        let end_at = start_at + chrono::TimeDelta::minutes(30);
        cw_core::episode::Episode {
            source: "screen",
            start_at,
            end_at,
            document_id: document_id.to_owned(),
            content: "content".to_owned(),
            metadata: cw_core::episode::EpisodeMetadata {
                episode_start: start_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                episode_end: end_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                monitors: "[]".to_owned(),
                entry_count: "1".to_owned(),
                image_paths: "[]".to_owned(),
            },
        }
    }

    fn registered_episode(conn: &mut rusqlite::Connection) -> (ulid::Ulid, chrono::DateTime<Utc>) {
        let start_at = TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 1, 12, 0, 0)
            .single()
            .expect("the test timestamp should be valid");
        let episode = test_episode(start_at, "screen-2026-08-01T12:00:00Z-30m");
        let id = ulid::Ulid::generate();
        episodes::insert_with_outbox(conn, id, &episode, start_at)
            .expect("the episode should register");

        (id, start_at)
    }

    #[test]
    fn the_newest_episodes_error_answers_for_the_backlog() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let mut conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the fresh database should initialize");
        let start_at = TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 1, 12, 0, 0)
            .single()
            .expect("the test timestamp should be valid");
        let older = ulid::Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a valid ulid");
        let newer = ulid::Ulid::from_string("01BRZ3NDEKTSV4RRFFQ69G5FAV").expect("a valid ulid");
        episodes::insert_with_outbox(
            &mut conn,
            older,
            &test_episode(start_at, "screen-2026-08-01T12:00:00Z-30m"),
            start_at,
        )
        .expect("the episode should register");
        episodes::insert_with_outbox(
            &mut conn,
            newer,
            &test_episode(
                start_at + chrono::TimeDelta::hours(2),
                "screen-2026-08-01T14:00:00Z-30m",
            ),
            start_at,
        )
        .expect("the episode should register");
        assert_eq!(newest_error(&conn).expect("readable"), None);

        mark_failed(
            &mut conn,
            newer,
            start_at,
            Retry::Backoff,
            "the newer refusal",
        )
        .expect("the failure should record");
        mark_failed(
            &mut conn,
            older,
            start_at,
            Retry::Backoff,
            "the older refusal",
        )
        .expect("the failure should record");
        let error = newest_error(&conn)
            .expect("readable")
            .expect("two entries carry errors");
        assert_eq!(error.document_id, "screen-2026-08-01T14:00:00Z-30m");
        assert_eq!(error.state, "failed");
        assert_eq!(error.message, "the newer refusal");
        assert_eq!(error.entries, 2);

        assert!(mark_delivering(&conn, newer).expect("the claim should work"));
        mark_delivered(&conn, newer).expect("the delivery should record");
        let error = newest_error(&conn)
            .expect("readable")
            .expect("one entry still carries its error");
        assert_eq!(error.message, "the older refusal");
        assert_eq!(
            error.entries, 1,
            "a delivered entry must stop answering for errors"
        );

        let parked = ulid::Ulid::from_string("01CRZ3NDEKTSV4RRFFQ69G5FAV").expect("a valid ulid");
        episodes::insert_with_outbox(
            &mut conn,
            parked,
            &test_episode(
                start_at + chrono::TimeDelta::hours(4),
                "screen-2026-08-01T16:00:00Z-30m",
            ),
            start_at,
        )
        .expect("the episode should register");
        mark_waiting(
            &conn,
            parked,
            start_at,
            Note::Progress("hindsight is still processing the episode (operation op-3)"),
        )
        .expect("the wait should record");
        let error = newest_error(&conn)
            .expect("readable")
            .expect("the older error must still answer");
        assert_eq!(
            error.message, "the older refusal",
            "a healthy in-flight note must neither surface as an error nor mask one"
        );
        assert_eq!(error.entries, 1, "an in-flight note is not an error");

        mark_waiting(&conn, older, start_at, Note::Progress("accepted at last"))
            .expect("the wait should record");
        assert_eq!(
            newest_error(&conn).expect("readable"),
            None,
            "an accepted resubmission resolves the error it answered for"
        );

        mark_waiting(
            &conn,
            parked,
            start_at,
            Note::Error("the server said: the extractor gave out"),
        )
        .expect("the wait should record");
        let error = newest_error(&conn)
            .expect("readable")
            .expect("a parked server-side failure is an error");
        assert_eq!(error.state, "pending");
        assert_eq!(error.message, "the server said: the extractor gave out");
        assert_eq!(error.entries, 1);
    }

    #[test]
    fn the_newest_window_answers_even_when_it_drew_the_smaller_id() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let mut conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the fresh database should initialize");
        let start_at = TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 1, 12, 0, 0)
            .single()
            .expect("the test timestamp should be valid");
        let newer = ulid::Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a valid ulid");
        let older = ulid::Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FBV").expect("a valid ulid");
        assert_eq!(
            newer.timestamp_ms(),
            older.timestamp_ms(),
            "the inversion must live in the random bits"
        );
        assert!(newer < older, "the newer window must hold the smaller id");
        episodes::insert_with_outbox(
            &mut conn,
            older,
            &test_episode(start_at, "screen-2026-08-01T12:00:00Z-30m"),
            start_at,
        )
        .expect("the episode should register");
        episodes::insert_with_outbox(
            &mut conn,
            newer,
            &test_episode(
                start_at + chrono::TimeDelta::hours(2),
                "screen-2026-08-01T14:00:00Z-30m",
            ),
            start_at,
        )
        .expect("the episode should register");

        mark_failed(
            &mut conn,
            older,
            start_at,
            Retry::Backoff,
            "the older refusal",
        )
        .expect("the failure should record");
        mark_failed(
            &mut conn,
            newer,
            start_at,
            Retry::Backoff,
            "the newer refusal",
        )
        .expect("the failure should record");

        let error = newest_error(&conn)
            .expect("readable")
            .expect("two entries carry errors");
        assert_eq!(error.document_id, "screen-2026-08-01T14:00:00Z-30m");
        assert_eq!(error.message, "the newer refusal");
        assert_eq!(error.entries, 2);
    }

    fn note_and_error(
        conn: &rusqlite::Connection,
        id: ulid::Ulid,
    ) -> (Option<String>, Option<String>) {
        conn.query_row(
            "SELECT last_note, last_error FROM outbox WHERE episode_id = ?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the outbox row should read")
    }

    #[test]
    fn a_parked_note_never_survives_the_next_mark() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let mut conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the fresh database should initialize");
        let (id, start_at) = registered_episode(&mut conn);

        mark_waiting(
            &conn,
            id,
            start_at,
            Note::Progress("waiting on operation op-9"),
        )
        .expect("the wait should record");
        mark_waiting(&conn, id, start_at, Note::Error("the server said: no"))
            .expect("the wait should record");
        assert_eq!(
            note_and_error(&conn, id),
            (None, Some("the server said: no".to_owned())),
            "a parked error must replace the note, not sit beside it"
        );

        mark_waiting(
            &conn,
            id,
            start_at,
            Note::Progress("waiting on operation op-9"),
        )
        .expect("the wait should record");
        mark_failed(
            &mut conn,
            id,
            start_at,
            Retry::Backoff,
            "the transport gave out",
        )
        .expect("the failure should record");
        assert_eq!(
            note_and_error(&conn, id),
            (None, Some("the transport gave out".to_owned())),
            "a failure must replace the note, not sit beside it"
        );

        mark_waiting(
            &conn,
            id,
            start_at,
            Note::Progress("waiting on operation op-9"),
        )
        .expect("the wait should record");
        assert!(mark_delivering(&conn, id).expect("the claim should work"));
        mark_delivered(&conn, id).expect("the delivery should record");
        assert_eq!(
            note_and_error(&conn, id),
            (None, None),
            "a delivered entry must carry neither note nor error"
        );
    }

    #[test]
    fn an_earlier_due_instant_outranks_an_older_window() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let mut conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the fresh database should initialize");
        let (old_id, start_at) = registered_episode(&mut conn);
        let young_start = start_at + chrono::TimeDelta::hours(2);
        let young_id = ulid::Ulid::generate();
        let young_due = start_at + chrono::TimeDelta::hours(4);
        episodes::insert_with_outbox(
            &mut conn,
            young_id,
            &test_episode(young_start, "screen-2026-08-01T14:00:00Z-30m"),
            young_due,
        )
        .expect("the young episode should register");
        assert!(mark_delivering(&conn, old_id).expect("the claim should run"));
        mark_waiting(
            &conn,
            old_id,
            young_due + chrono::TimeDelta::minutes(10),
            Note::Progress("still processing"),
        )
        .expect("the wait should record");

        let now = young_due + chrono::TimeDelta::hours(1);
        let crowded = fetch_due(&conn, now, 1).expect("the due query should run");
        assert_eq!(
            crowded[0].episode_id, young_id,
            "a parked old window must not crowd earlier-due fresh work out of the batch"
        );
        let due: Vec<_> = fetch_due(&conn, now, 10)
            .expect("the due query should run")
            .into_iter()
            .map(|entry| entry.episode_id)
            .collect();
        assert_eq!(
            due,
            [young_id, old_id],
            "entries must be served by due instant, the window only breaking ties"
        );
    }

    #[test]
    fn a_claimed_entry_is_due_again_only_after_the_requeue() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let mut conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the fresh database should initialize");
        let (id, start_at) = registered_episode(&mut conn);
        let now = start_at + chrono::TimeDelta::hours(1);

        assert_eq!(
            fetch_due(&conn, now, 10)
                .expect("the due query should run")
                .len(),
            1
        );
        assert!(mark_delivering(&conn, id).expect("the claim should run"));
        assert!(
            fetch_due(&conn, now, 10)
                .expect("the due query should run")
                .is_empty(),
            "a claimed entry must not be handed to another pass"
        );

        assert_eq!(
            requeue_delivering(&conn).expect("the requeue should run"),
            1
        );
        assert_eq!(
            fetch_due(&conn, now, 10)
                .expect("the due query should run")
                .len(),
            1,
            "the requeued entry must be due again"
        );
    }

    #[test]
    fn a_waiting_entry_counts_no_attempt() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let mut conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the fresh database should initialize");
        let (id, start_at) = registered_episode(&mut conn);
        let now = start_at + chrono::TimeDelta::hours(1);
        let until = now + chrono::TimeDelta::hours(2);

        assert!(mark_delivering(&conn, id).expect("the claim should run"));
        mark_failed(&mut conn, id, now, Retry::Backoff, "one real failure")
            .expect("the failure should record");
        assert!(mark_delivering(&conn, id).expect("the claim should run"));
        mark_waiting(&conn, id, until, Note::Progress("still processing"))
            .expect("the wait should record");
        assert_eq!(
            counts_by_state(&conn).expect("the counts should run"),
            [("pending".to_owned(), 1)],
            "an in-flight wait must not be reported as a failure"
        );
        assert!(
            fetch_due(&conn, now + chrono::TimeDelta::minutes(16), 10)
                .expect("the due query should run")
                .is_empty(),
            "the waiting instant, not the failure backoff, must govern when the entry is due"
        );
        let due = fetch_due(&conn, until, 10).expect("the due query should run");
        assert_eq!(
            due.len(),
            1,
            "the waiting entry must come due at its instant"
        );
        assert_eq!(
            due[0].attempts, 1,
            "a poll must neither count as an attempt nor erase the counted one"
        );
    }
}
