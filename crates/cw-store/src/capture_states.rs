//! What capture saw on the ticks that stored no frame, and the stretches with nothing recorded, as
//! spans.

use crate::{StoreError, timestamp};
use cw_core::model::{CaptureState, CaptureStatus, StateSpan};

const INSERT: &str = "INSERT INTO capture_states \
     (id, state, start_at, end_at, process, title, detail) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
const EXTEND: &str = "UPDATE capture_states SET end_at = ?2 WHERE id = ?1";
const DELETE: &str = "DELETE FROM capture_states WHERE id = ?1";
/// `start_at < end` asked as `start_at <= end - 1ns`, as `observations::find_in_window` does.
const SELECT_OVERLAPPING: &str = "SELECT id, state, start_at, end_at, process, title, detail \
     FROM capture_states WHERE end_at >= ?1 AND start_at <= ?2 ORDER BY start_at, id";
const SELECT_ACROSS: &str = "SELECT id, state, start_at, end_at, process, title, detail \
     FROM capture_states WHERE state = ?2 AND end_at > ?1 AND start_at < ?1 ORDER BY start_at, id";
/// SQLite takes a bare column in a `max()` query from the row the maximum came from.
const SELECT_RECORDED_UNTIL: &str = "SELECT id, max(at) FROM (\
     SELECT id, max(end_at) AS at FROM capture_states WHERE end_at < ?1 \
     UNION ALL SELECT id, max(observed_at) FROM observations WHERE observed_at < ?1 \
     UNION ALL SELECT id, ?1 FROM capture_states WHERE end_at >= ?1 AND start_at < ?1)";

/// How a state is spelled in the `state` column.
const fn spelling(state: CaptureState) -> &'static str {
    match state {
        CaptureState::Paused => "paused",
        CaptureState::Excluded => "excluded",
        CaptureState::NoTarget => "no_target",
        CaptureState::CaptureFailed => "capture_failed",
        CaptureState::NoNewFrame => "no_new_frame",
        CaptureState::Unchanged => "unchanged",
        CaptureState::SaveFailed => "save_failed",
        CaptureState::Unrecorded => "unrecorded",
    }
}

/// Read a `state` column back, refusing one this build does not know.
fn from_spelling(text: &str) -> Option<CaptureState> {
    match text {
        "paused" => Some(CaptureState::Paused),
        "excluded" => Some(CaptureState::Excluded),
        "no_target" => Some(CaptureState::NoTarget),
        "capture_failed" => Some(CaptureState::CaptureFailed),
        "no_new_frame" => Some(CaptureState::NoNewFrame),
        "unchanged" => Some(CaptureState::Unchanged),
        "save_failed" => Some(CaptureState::SaveFailed),
        "unrecorded" => Some(CaptureState::Unrecorded),
        _ => None,
    }
}

/// Store one span.
pub fn insert(conn: &rusqlite::Connection, span: &StateSpan) -> Result<(), StoreError> {
    conn.execute(
        INSERT,
        rusqlite::params![
            span.id.to_string(),
            spelling(span.status.state),
            timestamp::to_sql(span.start_at)?,
            timestamp::to_sql(span.end_at)?,
            span.status.process,
            span.status.title,
            span.status.detail,
        ],
    )
    .map_err(|source| StoreError::Sql { source })?;

    Ok(())
}

/// Move the end of the span with this id to `end_at`. A span that does not exist is an error.
pub fn extend(
    conn: &rusqlite::Connection,
    id: ulid::Ulid,
    end_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    let updated = conn
        .execute(
            EXTEND,
            rusqlite::params![id.to_string(), timestamp::to_sql(end_at)?],
        )
        .map_err(|source| StoreError::Sql { source })?;
    if updated == 0 {
        return Err(StoreError::Sql {
            source: rusqlite::Error::QueryReturnedNoRows,
        });
    }

    Ok(())
}

/// Every span with `end_at >= start` and `start_at < end`, ordered by start, then id.
pub fn overlapping(
    conn: &rusqlite::Connection,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<StateSpan>, StoreError> {
    if end <= start {
        return Ok(Vec::new());
    }

    // `checked_sub_signed` rather than `-`: `-` answers an underflow with a panic instead of an
    // empty window.
    let Some(last) = end.checked_sub_signed(chrono::TimeDelta::nanoseconds(1)) else {
        return Ok(Vec::new());
    };
    spans_where(
        conn,
        SELECT_OVERLAPPING,
        rusqlite::params![timestamp::to_sql(start)?, timestamp::to_sql(last)?],
    )
}

/// Account for a record at `at`: write the stretch since the record before it, the last tick mark
/// included, as `Unrecorded` when it is longer than `longest_wait`, and cut every `Unrecorded` span
/// `at` falls inside.
pub fn mark_recorded(
    conn: &rusqlite::Connection,
    at: chrono::DateTime<chrono::Utc>,
    longest_wait: chrono::TimeDelta,
) -> Result<(), StoreError> {
    let last_tick = crate::control::get_health(conn, crate::control::HealthKey::LastTick)?;
    let reached = recorded_until(conn, at)?.max(last_tick.filter(|tick| *tick < at));
    if let Some(start_at) = reached.filter(|last| at - *last > longest_wait) {
        insert(
            conn,
            &StateSpan {
                id: ulid::Ulid::generate(),
                status: CaptureStatus {
                    state: CaptureState::Unrecorded,
                    process: None,
                    title: None,
                    detail: None,
                },
                start_at,
                end_at: at,
            },
        )?;
    }
    split_unrecorded_at(conn, at, longest_wait)
}

/// Cut every `Unrecorded` span `at` falls strictly inside at `at`, keeping only the pieces longer
/// than `longest_wait`.
fn split_unrecorded_at(
    conn: &rusqlite::Connection,
    at: chrono::DateTime<chrono::Utc>,
    longest_wait: chrono::TimeDelta,
) -> Result<(), StoreError> {
    let across = spans_where(
        conn,
        SELECT_ACROSS,
        rusqlite::params![timestamp::to_sql(at)?, spelling(CaptureState::Unrecorded)],
    )?;
    for span in across {
        if at - span.start_at > longest_wait {
            extend(conn, span.id, at)?;
        } else {
            conn.execute(DELETE, [span.id.to_string()])
                .map_err(|source| StoreError::Sql { source })?;
        }
        if span.end_at - at > longest_wait {
            insert(
                conn,
                &StateSpan {
                    id: ulid::Ulid::generate(),
                    status: span.status,
                    start_at: at,
                    end_at: span.end_at,
                },
            )?;
        }
    }

    Ok(())
}

/// How far the record reaches toward `at`: `at` itself when a span starts before it and ends at or
/// after it, otherwise the latest span end or observation before it, or `None` when there is none.
fn recorded_until(
    conn: &rusqlite::Connection,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, StoreError> {
    let (id, reached): (Option<String>, Option<String>) = conn
        .query_one(SELECT_RECORDED_UNTIL, [timestamp::to_sql(at)?], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|source| StoreError::Sql { source })?;
    let Some(reached) = reached else {
        return Ok(None);
    };

    timestamp::from_sql(&reached)
        .map(Some)
        .map_err(|source| StoreError::Encoding {
            id: id.unwrap_or_default(),
            source,
        })
}

fn spans_where(
    conn: &rusqlite::Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<StateSpan>, StoreError> {
    let mut statement = conn
        .prepare(sql)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query(params)
        .map_err(|source| StoreError::Sql { source })?;
    let mut spans = Vec::new();

    while let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? {
        spans.push(from_row(row)?);
    }

    Ok(spans)
}

fn invalid_data(message: &'static str) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

/// Rebuild a span from one row, named by the id as the row spells it.
fn from_row(row: &rusqlite::Row<'_>) -> Result<StateSpan, StoreError> {
    let id: String = row.get(0).map_err(|source| StoreError::Sql { source })?;
    decode(row).map_err(|source| StoreError::Encoding { id, source })
}

/// Everything about a row that can be wrong in a way SQLite cannot see.
fn decode(row: &rusqlite::Row<'_>) -> Result<StateSpan, Box<dyn std::error::Error + Send + Sync>> {
    let stored_id: String = row.get(0)?;
    let id = ulid::Ulid::from_string(&stored_id)?;
    if id.to_string() != stored_id {
        return Err(invalid_data(
            "the id is not spelled the way this program writes a ULID",
        ));
    }
    let state: String = row.get(1)?;
    let state = from_spelling(&state)
        .ok_or_else(|| invalid_data("the capture state is not known to this build"))?;
    let start_at = timestamp::from_sql(&row.get::<_, String>(2)?)?;
    let end_at = timestamp::from_sql(&row.get::<_, String>(3)?)?;

    Ok(StateSpan {
        id,
        status: CaptureStatus {
            state,
            process: row.get(4)?,
            title: row.get(5)?,
            detail: row.get(6)?,
        },
        start_at,
        end_at,
    })
}

#[cfg(test)]
mod tests {
    use super::{INSERT, extend, insert, overlapping, recorded_until, split_unrecorded_at};
    use crate::{StoreError, db, observations, timestamp};
    use chrono::{DateTime, Utc};
    use cw_core::model::{
        CaptureState, CaptureStatus, Observation, OcrStatus, ScreenPayload, StateSpan,
    };
    use tempfile::{TempDir, tempdir};

    fn database() -> (TempDir, rusqlite::Connection) {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = db::open(&path).expect("the fresh database should initialize");
        (dir, conn)
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("the test timestamp should be valid")
            .with_timezone(&Utc)
    }

    fn span(id: u128, state: CaptureState, start_at: &str, end_at: &str) -> StateSpan {
        StateSpan {
            id: ulid::Ulid::from(id),
            status: CaptureStatus {
                state,
                process: Some("editor.exe".to_owned()),
                title: Some("Example Page".to_owned()),
                detail: Some("synthetic detail".to_owned()),
            },
            start_at: at(start_at),
            end_at: at(end_at),
        }
    }

    fn everything(conn: &rusqlite::Connection) -> Vec<StateSpan> {
        overlapping(conn, at("2000-01-01T00:00:00Z"), at("3000-01-01T00:00:00Z"))
            .expect("every span should be readable")
    }

    #[test]
    fn every_state_round_trips_through_the_database() {
        let (_dir, conn) = database();
        let states = [
            CaptureState::Paused,
            CaptureState::Excluded,
            CaptureState::NoTarget,
            CaptureState::CaptureFailed,
            CaptureState::NoNewFrame,
            CaptureState::Unchanged,
            CaptureState::SaveFailed,
            CaptureState::Unrecorded,
        ];
        let mut spans: Vec<_> = states
            .into_iter()
            .enumerate()
            .map(|(index, state)| {
                let start_at =
                    at("2026-07-25T12:00:00.123456789Z") + chrono::TimeDelta::minutes(index as i64);
                StateSpan {
                    id: ulid::Ulid::from(index as u128 + 1),
                    start_at,
                    end_at: start_at + chrono::TimeDelta::seconds(30),
                    ..span(0, state, "2026-07-25T12:00:00Z", "2026-07-25T12:00:00Z")
                }
            })
            .collect();
        spans[0].status.process = None;
        spans[0].status.title = None;
        spans[0].status.detail = None;

        for span in &spans {
            insert(&conn, span).expect("the span should be stored");
        }

        assert_eq!(everything(&conn), spans);
    }

    #[test]
    fn extending_moves_only_the_end() {
        let (_dir, conn) = database();
        let stored = span(
            1,
            CaptureState::Unchanged,
            "2026-07-25T12:00:00Z",
            "2026-07-25T12:00:00Z",
        );
        insert(&conn, &stored).expect("the span should be stored");

        extend(&conn, stored.id, at("2026-07-25T12:00:30Z")).expect("the span should extend");

        assert_eq!(
            everything(&conn),
            [StateSpan {
                end_at: at("2026-07-25T12:00:30Z"),
                ..stored
            }]
        );
    }

    #[test]
    fn extending_a_span_that_does_not_exist_is_an_error() {
        let (_dir, conn) = database();

        let error = extend(&conn, ulid::Ulid::from(1u128), at("2026-07-25T12:00:30Z"))
            .expect_err("extending nothing should fail");

        assert!(matches!(
            error,
            StoreError::Sql {
                source: rusqlite::Error::QueryReturnedNoRows
            }
        ));
    }

    #[test]
    fn the_overlap_takes_a_span_ending_at_the_start_and_not_one_starting_at_the_end() {
        let (_dir, conn) = database();
        let before = span(
            1,
            CaptureState::Paused,
            "2026-07-25T11:50:00Z",
            "2026-07-25T11:59:59.999999999Z",
        );
        let ending_at_start = span(
            9,
            CaptureState::Paused,
            "2026-07-25T11:55:00Z",
            "2026-07-25T12:00:00Z",
        );
        let tied_lower_id = span(
            2,
            CaptureState::NoTarget,
            "2026-07-25T11:55:00Z",
            "2026-07-25T12:01:00Z",
        );
        let starting_before_end = span(
            3,
            CaptureState::Unchanged,
            "2026-07-25T12:04:59.999999999Z",
            "2026-07-25T12:10:00Z",
        );
        let starting_at_end = span(
            4,
            CaptureState::Unchanged,
            "2026-07-25T12:05:00Z",
            "2026-07-25T12:06:00Z",
        );
        let covering = span(
            5,
            CaptureState::Excluded,
            "2026-07-25T11:00:00Z",
            "2026-07-25T13:00:00Z",
        );

        for stored in [
            &before,
            &ending_at_start,
            &tied_lower_id,
            &starting_before_end,
            &starting_at_end,
            &covering,
        ] {
            insert(&conn, stored).expect("the span should be stored");
        }
        let found = overlapping(
            &conn,
            at("2026-07-25T12:00:00Z"),
            at("2026-07-25T12:05:00Z"),
        )
        .expect("the window should be readable");

        assert_eq!(
            found,
            [
                covering,
                tied_lower_id,
                ending_at_start,
                starting_before_end
            ]
        );
    }

    #[test]
    fn an_empty_or_reversed_window_is_empty_rather_than_an_error() {
        let (_dir, conn) = database();
        let t = at("2026-07-25T12:00:00Z");
        insert(
            &conn,
            &span(
                1,
                CaptureState::Paused,
                "2026-07-25T11:00:00Z",
                "2026-07-25T13:00:00Z",
            ),
        )
        .expect("the span should be stored");

        assert_eq!(
            overlapping(&conn, t, t).expect("the empty window should be readable"),
            []
        );
        assert_eq!(
            overlapping(&conn, t + chrono::TimeDelta::seconds(1), t)
                .expect("the reversed window should be readable"),
            []
        );
    }

    #[test]
    fn a_state_this_build_does_not_know_is_refused() {
        let (_dir, conn) = database();
        let id = ulid::Ulid::from(1u128).to_string();
        let instant = timestamp::to_sql(at("2026-07-25T12:00:00Z"))
            .expect("the test timestamp should be spellable");
        conn.execute(
            INSERT,
            rusqlite::params![
                &id,
                "future_state",
                &instant,
                &instant,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
            ],
        )
        .expect("the unknown state row should be writable");

        let error = overlapping(
            &conn,
            at("2026-07-25T11:00:00Z"),
            at("2026-07-25T13:00:00Z"),
        )
        .expect_err("the unknown state should be refused");

        match error {
            StoreError::Encoding { id: actual, .. } => assert_eq!(actual, id),
            other => panic!("expected Encoding, got {other:?}"),
        }
    }

    #[test]
    fn the_last_record_is_the_later_of_the_last_span_and_the_last_observation() {
        let (_dir, conn) = database();
        let last_recorded =
            |conn: &rusqlite::Connection| recorded_until(conn, at("2026-07-26T00:00:00Z"));
        assert_eq!(last_recorded(&conn).expect("readable"), None);

        let stored = span(
            1,
            CaptureState::Unchanged,
            "2026-07-25T11:00:00Z",
            "2026-07-25T12:00:00Z",
        );
        insert(&conn, &stored).expect("the span should be stored");
        assert_eq!(
            last_recorded(&conn).expect("readable"),
            Some(at("2026-07-25T12:00:00Z"))
        );

        let observation = Observation::new_screen(
            ScreenPayload {
                width: 1,
                height: 1,
                image_path: None,
                ocr_status: OcrStatus::NoText,
                ocr_error: None,
                ocr_text: None,
                ocr_langs: Vec::new(),
                foreground_process: None,
                foreground_window_title: None,
            },
            at("2026-07-25T12:01:00Z"),
        );
        observations::insert(&conn, &observation).expect("the observation should be stored");
        assert_eq!(
            last_recorded(&conn).expect("readable"),
            Some(at("2026-07-25T12:01:00Z"))
        );

        extend(&conn, stored.id, at("2026-07-25T12:02:00Z")).expect("the span should extend");
        assert_eq!(
            last_recorded(&conn).expect("readable"),
            Some(at("2026-07-25T12:02:00Z"))
        );
        assert_eq!(
            recorded_until(&conn, at("2026-07-25T12:01:30Z")).expect("readable"),
            Some(at("2026-07-25T12:01:30Z"))
        );
    }

    #[test]
    fn the_record_reaches_only_what_lies_before_the_instant() {
        let (_dir, conn) = database();
        let observation = Observation::new_screen(
            ScreenPayload {
                width: 1,
                height: 1,
                image_path: None,
                ocr_status: OcrStatus::NoText,
                ocr_error: None,
                ocr_text: None,
                ocr_langs: Vec::new(),
                foreground_process: None,
                foreground_window_title: None,
            },
            at("2026-07-25T12:00:00Z"),
        );
        observations::insert(&conn, &observation).expect("the observation should be stored");
        insert(
            &conn,
            &span(
                1,
                CaptureState::Unchanged,
                "2026-07-25T12:00:00Z",
                "2026-07-25T12:00:00Z",
            ),
        )
        .expect("the span should be stored");

        assert_eq!(
            recorded_until(&conn, at("2026-07-25T12:00:00Z")).expect("readable"),
            None
        );
    }

    #[test]
    fn a_split_keeps_only_the_pieces_longer_than_the_wait() {
        let longest_wait = chrono::TimeDelta::seconds(20);
        for (split_at, kept) in [
            (
                "2026-07-25T12:30:00Z",
                vec![
                    ("2026-07-25T12:00:00Z", "2026-07-25T12:30:00Z"),
                    ("2026-07-25T12:30:00Z", "2026-07-25T13:00:00Z"),
                ],
            ),
            (
                "2026-07-25T12:59:59Z",
                vec![("2026-07-25T12:00:00Z", "2026-07-25T12:59:59Z")],
            ),
            (
                "2026-07-25T12:00:01Z",
                vec![("2026-07-25T12:00:01Z", "2026-07-25T13:00:00Z")],
            ),
            (
                "2026-07-25T13:00:00Z",
                vec![("2026-07-25T12:00:00Z", "2026-07-25T13:00:00Z")],
            ),
        ] {
            let (_dir, conn) = database();
            insert(
                &conn,
                &span(
                    1,
                    CaptureState::Unrecorded,
                    "2026-07-25T12:00:00Z",
                    "2026-07-25T13:00:00Z",
                ),
            )
            .expect("the gap should be stored");
            insert(
                &conn,
                &span(
                    2,
                    CaptureState::Unchanged,
                    "2026-07-25T13:00:00Z",
                    "2026-07-25T13:10:00Z",
                ),
            )
            .expect("the status should be stored");

            split_unrecorded_at(&conn, at(split_at), longest_wait).expect("the split should land");

            let spans = everything(&conn);
            let gaps: Vec<_> = spans
                .iter()
                .filter(|span| span.status.state == CaptureState::Unrecorded)
                .map(|span| (span.start_at, span.end_at))
                .collect();
            let expected: Vec<_> = kept.iter().map(|(from, to)| (at(from), at(to))).collect();
            assert_eq!(gaps, expected, "split at {split_at}");
            assert!(
                spans
                    .iter()
                    .any(|span| span.status.state == CaptureState::Unchanged),
                "split at {split_at}"
            );
        }
    }
}
