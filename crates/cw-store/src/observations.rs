//! Observations, as rows.

use crate::{StoreError, timestamp};
use cw_core::model::{Observation, SourcePayload};

const SELECT_BY_ID: &str = "SELECT id, source, observed_at, duration_ms, schema_version, payload \
     FROM observations WHERE id = ?1";

/// Half-open in its contract, inclusive in its SQL: `[start, end)` is exactly `[start, end - 1ns]`
/// because the instants this schema represents are the nanosecond grid — `to_sql` refuses an
/// overflowing nanosecond field, so nothing storable lies strictly between the two.
/// `build_episode` takes `[start, end)`, and adjacent
/// five-minute windows have to tile: with both bounds inclusive an observation landing exactly on a
/// boundary is delivered in two episodes, whose differing document ids mean nothing downstream
/// notices. The `id` tiebreak is not decoration — one tick captures several monitors and can stamp
/// them with the same instant, and SQLite does not promise an order among equal sort keys.
const SELECT_IN_WINDOW: &str = "SELECT id, source, observed_at, duration_ms, schema_version, payload FROM observations \
     WHERE observed_at >= ?1 AND observed_at <= ?2 ORDER BY observed_at, id";

/// Store one observation.
///
/// A plain `INSERT`: the id is a fresh ULID per capture, so a collision is a bug in the caller and
/// comes back as the primary key violation it is. `OR REPLACE` would answer it by silently
/// destroying whichever observation was there first.
pub fn insert(conn: &rusqlite::Connection, observation: &Observation) -> Result<(), StoreError> {
    let id = observation.id.to_string();
    let observed_at = timestamp::to_sql(observation.observed_at)?;
    let duration_ms = observation
        .duration_ms
        .map(|ms| {
            i64::try_from(ms).map_err(|_| StoreError::DurationOutOfRange {
                id: id.clone(),
                duration_ms: ms,
            })
        })
        .transpose()?;
    let payload = payload_json(observation).map_err(|source| StoreError::Encoding {
        id: id.clone(),
        source,
    })?;
    // The write path must not accept anything `decode` cannot return: `from_parts` resolves the
    // source column into a variant, so a payload naming a kind this build knows comes back as that
    // kind rather than as what was handed in. Asking whether the round trip is faithful keeps the
    // list of known sources in cw-core, where it belongs.
    let stored_as = serde_json::from_str(&payload)
        .map_err(|source| StoreError::Encoding {
            id: id.clone(),
            source: Box::new(source),
        })
        .and_then(|raw| {
            SourcePayload::from_parts(observation.payload.kind(), raw).map_err(|source| {
                StoreError::Encoding {
                    id: id.clone(),
                    source: Box::new(source),
                }
            })
        })?;
    if stored_as != observation.payload {
        return Err(StoreError::NotFaithful { id });
    }

    conn.execute(
        "INSERT INTO observations \
         (id, source, observed_at, duration_ms, schema_version, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            id,
            observation.payload.kind(),
            observed_at,
            duration_ms,
            i64::from(observation.schema_version),
            payload,
        ],
    )
    .map_err(|source| StoreError::Insert {
        id: id.clone(),
        source,
    })?;

    Ok(())
}

/// The observation with this id, or `None` when the database holds no such row.
pub fn find_by_id(
    conn: &rusqlite::Connection,
    id: ulid::Ulid,
) -> Result<Option<Observation>, StoreError> {
    let mut statement = conn
        .prepare(SELECT_BY_ID)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query([id.to_string()])
        .map_err(|source| StoreError::Sql { source })?;

    let mut observation = None;
    while let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? {
        observation = Some(from_row(row)?);
    }

    Ok(observation)
}

/// Every observation with `start <= observed_at < end`, oldest first.
pub fn find_in_window(
    conn: &rusqlite::Connection,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<Observation>, StoreError> {
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
        .prepare(SELECT_IN_WINDOW)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query(rusqlite::params![
            timestamp::to_sql(start)?,
            timestamp::to_sql(last)?,
        ])
        .map_err(|source| StoreError::Sql { source })?;
    let mut observations = Vec::new();

    while let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? {
        observations.push(from_row(row)?);
    }

    Ok(observations)
}

/// The payload column's text: the payload's own JSON, with no discriminator inside it. The `source`
/// column is the single place the kind is written (design section 4.1).
fn payload_json(
    observation: &Observation,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    Ok(serde_json::to_string(
        &observation.payload.to_payload_json()?,
    )?)
}

/// Rebuild an observation from one row.
///
/// The id is read as text first and kept, because it is what names the row in every error below —
/// including the one for an id that is not a ULID.
fn from_row(row: &rusqlite::Row<'_>) -> Result<Observation, StoreError> {
    let id: String = row.get(0).map_err(|source| StoreError::Sql { source })?;
    decode(row).map_err(|source| StoreError::Encoding { id, source })
}

/// Everything about a row that can be wrong in a way SQLite cannot see.
fn decode(
    row: &rusqlite::Row<'_>,
) -> Result<Observation, Box<dyn std::error::Error + Send + Sync>> {
    let id: String = row.get(0)?;
    let id = ulid::Ulid::from_string(&id)?;
    let source: String = row.get(1)?;
    let observed_at: String = row.get(2)?;
    let observed_at = timestamp::from_sql(&observed_at)?;
    let duration_ms = row
        .get::<_, Option<i64>>(3)?
        .map(u64::try_from)
        .transpose()?;
    let schema_version = u32::try_from(row.get::<_, i64>(4)?)?;
    let payload: String = row.get(5)?;
    let raw = serde_json::from_str(&payload)?;
    let payload = SourcePayload::from_parts(&source, raw)?;

    Ok(Observation {
        id,
        observed_at,
        duration_ms,
        schema_version,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::{find_by_id, find_in_window, insert, payload_json};
    use crate::{StoreError, db};
    use chrono::{DateTime, TimeDelta, TimeZone, Timelike, Utc};
    use cw_core::model::{
        CURRENT_SCHEMA_VERSION, Observation, OcrStatus, ScreenPayload, SourcePayload,
    };
    use tempfile::tempdir;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("the test timestamp should be valid")
            .with_timezone(&Utc)
    }

    fn fully_populated_screen_payload() -> ScreenPayload {
        ScreenPayload {
            monitor_id: "monitor-1".to_owned(),
            width: 2560,
            height: 1440,
            image_path: Some("screens/観測.png".to_owned()),
            ocr_status: OcrStatus::Succeeded,
            ocr_error: Some("認識できなかった小さな領域".to_owned()),
            ocr_text: Some("次の会議は午後三時です".to_owned()),
            ocr_langs: vec!["ja-JP".to_owned(), "en-US".to_owned()],
            foreground_process: Some("notepad.exe".to_owned()),
            foreground_window_title: Some("議事録 — メモ帳".to_owned()),
        }
    }

    fn screen_observation_at(observed_at: DateTime<Utc>) -> Observation {
        Observation::new_screen(fully_populated_screen_payload(), observed_at)
    }

    #[test]
    fn an_observation_round_trips_through_the_database() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let mut observation = screen_observation_at(at("2026-07-25T12:34:56.632079500Z"));
        observation.duration_ms = Some(1_234);

        insert(&conn, &observation).expect("the observation should be stored");
        let restored =
            find_by_id(&conn, observation.id).expect("the stored observation should be readable");

        assert_eq!(restored, Some(observation));
    }

    #[test]
    fn an_absent_id_is_not_an_error() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");

        let found = find_by_id(&conn, ulid::Ulid::new())
            .expect("an absent observation should be a successful lookup");

        assert_eq!(found, None);
    }

    #[test]
    fn an_unknown_source_round_trips_untouched() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let source = "audio-v2".to_owned();
        let raw = serde_json::json!({
            "codec": "pcm_s16le",
            "samples": [1, 2, 3],
            "metadata": {
                "speaker": null,
                "labels": ["会議", "音声"]
            }
        });
        let observation = Observation {
            id: ulid::Ulid::new(),
            observed_at: at("2026-07-25T12:34:56.123456789Z"),
            duration_ms: Some(2_000),
            schema_version: CURRENT_SCHEMA_VERSION,
            payload: SourcePayload::Unknown {
                source: source.clone(),
                raw,
            },
        };

        insert(&conn, &observation).expect("the unknown observation should be stored");
        let stored_source: String = conn
            .query_row(
                "SELECT source FROM observations WHERE id = ?1",
                [observation.id.to_string()],
                |row| row.get(0),
            )
            .expect("the stored source should be readable");
        let restored =
            find_by_id(&conn, observation.id).expect("the unknown observation should be readable");

        assert_eq!(stored_source, source);
        assert_eq!(restored, Some(observation));
    }

    #[test]
    fn an_observation_that_would_read_back_as_a_different_value_is_refused() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let mut raw = serde_json::to_value(fully_populated_screen_payload())
            .expect("the screen payload should serialize");
        raw.as_object_mut()
            .expect("the screen payload JSON should be an object")
            .insert(
                "future_field".to_owned(),
                serde_json::json!("must not be lost"),
            );
        let observation = Observation {
            id: ulid::Ulid::new(),
            observed_at: at("2026-07-25T12:34:56Z"),
            duration_ms: None,
            schema_version: CURRENT_SCHEMA_VERSION,
            payload: SourcePayload::Unknown {
                source: "screen".to_owned(),
                raw,
            },
        };
        let expected_id = observation.id.to_string();

        // Without the check this row reads back as Screen, with `future_field` silently gone.
        let error = insert(&conn, &observation)
            .expect_err("the observation that would change should be refused");
        match error {
            StoreError::NotFaithful { id } => assert_eq!(id, expected_id),
            other => panic!("expected NotFaithful, got {other:?}"),
        }
        let found = find_by_id(&conn, observation.id)
            .expect("the refused observation lookup should succeed");

        assert_eq!(found, None);
    }

    #[test]
    fn the_window_includes_its_start_and_excludes_its_end() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let start = at("2026-07-25T12:00:00Z");
        let end = at("2026-07-25T12:05:00Z");
        let before = screen_observation_at(start - TimeDelta::nanoseconds(1));
        let at_start = screen_observation_at(start);
        let before_end = screen_observation_at(end - TimeDelta::nanoseconds(1));
        let at_end = screen_observation_at(end);

        for observation in [&before, &at_start, &before_end, &at_end] {
            insert(&conn, observation).expect("the boundary observation should be stored");
        }
        let found =
            find_in_window(&conn, start, end).expect("the boundary window should be readable");

        assert_eq!(found, vec![at_start, before_end]);
    }

    #[test]
    fn the_window_returns_observations_oldest_first() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let start = at("2026-07-25T12:00:00Z");
        let oldest = screen_observation_at(start);
        let middle = screen_observation_at(start + TimeDelta::seconds(1));
        let newest = screen_observation_at(start + TimeDelta::seconds(2));

        for observation in [&newest, &oldest, &middle] {
            insert(&conn, observation).expect("the out-of-order observation should be stored");
        }
        let found = find_in_window(&conn, start, start + TimeDelta::seconds(3))
            .expect("the ordered window should be readable");

        assert_eq!(found, vec![oldest, middle, newest]);
    }

    #[test]
    fn two_observations_at_one_instant_come_back_in_id_order() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let observed_at = at("2026-07-25T12:00:00Z");
        let higher_id = ulid::Ulid::from(2u128);
        let lower_id = ulid::Ulid::from(1u128);
        assert!(higher_id > lower_id);
        let mut higher = screen_observation_at(observed_at);
        higher.id = higher_id;
        let mut lower = screen_observation_at(observed_at);
        lower.id = lower_id;

        insert(&conn, &higher).expect("the higher-id observation should be stored first");
        insert(&conn, &lower).expect("the lower-id observation should be stored second");
        // Without `, id` in the ORDER BY these come back in whatever order the query plan produced,
        // which is the one thing a stable listing has to rule out.
        let found = find_in_window(&conn, observed_at, observed_at + TimeDelta::nanoseconds(1))
            .expect("the tied observations should be readable");

        assert_eq!(found, vec![lower, higher]);
    }

    #[test]
    fn a_whole_second_and_the_fractions_after_it_compare_correctly_in_sql() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let start = at("2026-07-25T12:00:00Z");
        let whole_second = screen_observation_at(start);
        let last_fraction = screen_observation_at(at("2026-07-25T12:00:00.999999999Z"));

        insert(&conn, &last_fraction).expect("the fractional observation should be stored");
        insert(&conn, &whole_second).expect("the whole-second observation should be stored");
        // If zero fractions are dropped, the fractional row sorts below this window's lower bound,
        // so this query loses a row rather than merely returning the rows out of order.
        let found = find_in_window(&conn, start, at("2026-07-25T12:00:01Z"))
            .expect("the one-second window should be readable");

        assert_eq!(found, vec![whole_second, last_fraction]);
    }

    #[test]
    fn a_timestamp_the_schema_cannot_spell_is_never_stored() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let observed_at = Utc
            .with_ymd_and_hms(10_000, 1, 1, 0, 0, 0)
            .single()
            .expect("year 10000 should be valid");
        let observation = screen_observation_at(observed_at);

        let error = insert(&conn, &observation)
            .expect_err("the timestamp without a fixed width should be refused");
        assert!(matches!(error, StoreError::TimestampOutOfRange { .. }));
        let found = find_by_id(&conn, observation.id)
            .expect("the refused observation lookup should succeed");
        assert_eq!(found, None);
    }

    #[test]
    fn a_leap_second_no_window_query_could_return_never_reaches_the_database() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let observed_at = Utc
            .with_ymd_and_hms(2016, 12, 31, 23, 59, 59)
            .single()
            .expect("the test timestamp should be valid")
            .with_nanosecond(1_500_000_000)
            .expect("the leap second should be valid");
        let observation = screen_observation_at(observed_at);

        // Without a row to find, an empty result proves nothing about the query.
        let ordinary = screen_observation_at(at("2016-12-31T23:59:59.999999999Z"));
        insert(&conn, &ordinary).expect("the ordinary observation should be stored");

        // The row is refused because a window query would lose it even though the half-open
        // contract places it inside.
        let error = insert(&conn, &observation)
            .expect_err("the timestamp no window query could return should be refused");
        assert!(matches!(error, StoreError::TimestampOutOfRange { .. }));
        let found = find_by_id(&conn, observation.id)
            .expect("the refused observation lookup should succeed");
        assert_eq!(found, None);

        let found = find_in_window(
            &conn,
            at("2016-12-31T23:55:00Z"),
            at("2017-01-01T00:00:00Z"),
        )
        .expect("the window should be readable");
        assert_eq!(found, vec![ordinary]);
    }

    #[test]
    fn an_empty_or_reversed_window_is_empty_rather_than_an_error() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let t = at("2026-07-25T12:34:56Z");
        let observation = screen_observation_at(t);

        insert(&conn, &observation).expect("the ordinary observation should be stored");

        let empty = find_in_window(&conn, t, t).expect("the empty window should be readable");
        assert_eq!(empty, Vec::new());
        let reversed = find_in_window(&conn, t + TimeDelta::seconds(1), t)
            .expect("the reversed window should be readable");
        assert_eq!(reversed, Vec::new());
        let unspellable = find_in_window(&conn, DateTime::<Utc>::MAX_UTC, DateTime::<Utc>::MAX_UTC)
            .expect("the unspellable empty window should be readable");
        assert_eq!(unspellable, Vec::new());
    }

    #[test]
    fn every_observation_the_schema_can_store_is_inside_a_window_it_can_search() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let observation = screen_observation_at(at("9999-12-31T23:59:59.999999999Z"));
        let start = at("9999-12-31T23:55:00Z");
        let end = Utc
            .with_ymd_and_hms(10_000, 1, 1, 0, 0, 0)
            .single()
            .expect("year 10000 should be valid");

        insert(&conn, &observation).expect("the last spellable observation should be stored");
        // The exclusive end is year 10000, which has no spelling of its own. This row was
        // previously stranded by a reader that refused to look for it.
        let found = find_in_window(&conn, start, end)
            .expect("the last spellable observation should be searchable");
        assert_eq!(found, vec![observation]);

        let found = find_in_window(&conn, DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MIN_UTC)
            .expect("a window ending at the earliest instant should be empty");
        assert_eq!(found, Vec::new());
    }

    #[test]
    fn a_duplicate_id_is_refused() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let observation = screen_observation_at(at("2026-07-25T12:34:56Z"));

        insert(&conn, &observation).expect("the first observation should be stored");
        let error = insert(&conn, &observation)
            .expect_err("the duplicate observation id should be refused");
        let stored = find_by_id(&conn, observation.id)
            .expect("the first observation should remain readable");

        match error {
            StoreError::Insert { id, .. } => assert_eq!(id, observation.id.to_string()),
            other => panic!("expected Insert, got {other:?}"),
        }
        assert_eq!(stored, Some(observation));
    }

    #[test]
    fn a_duration_that_cannot_be_stored_leaves_no_row() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let observation = Observation {
            id: ulid::Ulid::new(),
            observed_at: at("2026-07-25T12:34:56Z"),
            duration_ms: Some(u64::MAX),
            schema_version: CURRENT_SCHEMA_VERSION,
            payload: SourcePayload::Screen(fully_populated_screen_payload()),
        };
        let expected_id = observation.id.to_string();

        let error =
            insert(&conn, &observation).expect_err("the out-of-range duration should be refused");
        match error {
            StoreError::DurationOutOfRange { id, duration_ms } => {
                assert_eq!(id, expected_id);
                assert_eq!(duration_ms, u64::MAX);
            }
            other => panic!("expected DurationOutOfRange, got {other:?}"),
        }
        let found = find_by_id(&conn, observation.id)
            .expect("the refused observation lookup should succeed");

        assert_eq!(found, None);
    }

    #[test]
    fn a_row_that_is_not_an_observation_is_refused() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        let observation = screen_observation_at(at("2026-07-25T12:34:56Z"));
        let id = observation.id.to_string();
        let payload = payload_json(&observation).expect("the valid payload should serialize");
        conn.execute(
            "INSERT INTO observations \
             (id, source, observed_at, duration_ms, schema_version, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                &id,
                observation.payload.kind(),
                "not a timestamp",
                Option::<i64>::None,
                i64::from(observation.schema_version),
                payload,
            ],
        )
        .expect("the malformed observation row should be writable");

        let error = find_by_id(&conn, observation.id)
            .expect_err("the malformed observation should be refused");
        match error {
            StoreError::Encoding { id: actual, .. } => assert_eq!(actual, id),
            other => panic!("expected Encoding, got {other:?}"),
        }
    }
}
