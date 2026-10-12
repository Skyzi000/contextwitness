use crate::capture_states::{spans_where, spelling};
use crate::{StoreError, timestamp};
use cw_core::model::{CaptureState, StateSpan};

const INSERT: &str =
    "INSERT INTO audio_skips (id, start_at, end_at, process) VALUES (?1, ?2, ?2, ?3)";
const EXTEND: &str = "UPDATE audio_skips SET end_at = ?2 WHERE id = ?1";
const SELECT_OVERLAPPING: &str = "SELECT id, ?3, start_at, end_at, process, NULL, NULL \
     FROM audio_skips WHERE end_at >= ?1 AND start_at < ?2 ORDER BY start_at, id";

pub fn begin(
    conn: &rusqlite::Connection,
    process: Option<&str>,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<ulid::Ulid, StoreError> {
    let id = ulid::Ulid::generate();
    conn.execute(
        INSERT,
        rusqlite::params![id.to_string(), timestamp::to_sql(at)?, process],
    )
    .map_err(|source| StoreError::Sql { source })?;

    Ok(id)
}

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

pub fn overlapping(
    conn: &rusqlite::Connection,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<StateSpan>, StoreError> {
    if end <= start {
        return Ok(Vec::new());
    }

    spans_where(
        conn,
        SELECT_OVERLAPPING,
        rusqlite::params![
            timestamp::to_sql(start)?,
            timestamp::to_sql(end)?,
            spelling(CaptureState::Excluded),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::{begin, extend, overlapping};
    use crate::{StoreError, db};
    use chrono::{DateTime, Utc};
    use cw_core::model::{CaptureState, CaptureStatus, StateSpan};
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

    fn skip(
        conn: &rusqlite::Connection,
        process: Option<&str>,
        start_at: &str,
        end_at: &str,
    ) -> StateSpan {
        let id = begin(conn, process, at(start_at)).expect("the skip should be stored");
        extend(conn, id, at(end_at)).expect("the skip should extend");
        StateSpan {
            id,
            status: CaptureStatus {
                state: CaptureState::Excluded,
                process: process.map(str::to_owned),
                title: None,
                detail: None,
            },
            start_at: at(start_at),
            end_at: at(end_at),
        }
    }

    #[test]
    fn a_skip_begins_as_one_instant_and_extending_moves_only_its_end() {
        let (_dir, conn) = database();
        let began = at("2026-07-25T12:00:00.123456789Z");
        let id = begin(&conn, Some("vault.exe"), began).expect("the skip should be stored");
        let everything = || {
            overlapping(
                &conn,
                at("2000-01-01T00:00:00Z"),
                at("3000-01-01T00:00:00Z"),
            )
            .expect("every skip should be readable")
        };
        let stored = StateSpan {
            id,
            status: CaptureStatus {
                state: CaptureState::Excluded,
                process: Some("vault.exe".to_owned()),
                title: None,
                detail: None,
            },
            start_at: began,
            end_at: began,
        };
        assert_eq!(everything(), std::slice::from_ref(&stored));

        extend(&conn, id, at("2026-07-25T12:00:30Z")).expect("the skip should extend");
        assert_eq!(
            everything(),
            [StateSpan {
                end_at: at("2026-07-25T12:00:30Z"),
                ..stored
            }]
        );
    }

    #[test]
    fn extending_a_skip_that_does_not_exist_is_an_error() {
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
    fn the_overlap_takes_a_skip_ending_at_the_start_and_not_one_starting_at_the_end() {
        let (_dir, conn) = database();
        skip(
            &conn,
            Some("vault.exe"),
            "2026-07-25T11:50:00Z",
            "2026-07-25T11:59:59.999999999Z",
        );
        let ending_at_start = skip(
            &conn,
            Some("vault.exe"),
            "2026-07-25T11:55:00Z",
            "2026-07-25T12:00:00Z",
        );
        let unnamed = skip(&conn, None, "2026-07-25T12:01:00Z", "2026-07-25T12:02:00Z");
        let starting_before_end = skip(
            &conn,
            Some("vault.exe"),
            "2026-07-25T12:04:59.999999999Z",
            "2026-07-25T12:10:00Z",
        );
        skip(
            &conn,
            Some("vault.exe"),
            "2026-07-25T12:05:00Z",
            "2026-07-25T12:06:00Z",
        );
        let covering = skip(
            &conn,
            Some("editor.exe"),
            "2026-07-25T11:00:00Z",
            "2026-07-25T13:00:00Z",
        );

        let found = overlapping(
            &conn,
            at("2026-07-25T12:00:00Z"),
            at("2026-07-25T12:05:00Z"),
        )
        .expect("the window should be readable");

        assert_eq!(
            found,
            [covering, ending_at_start, unnamed, starting_before_end]
        );
    }

    #[test]
    fn skips_that_start_together_are_ordered_by_id() {
        let (_dir, conn) = database();
        let mut together = [
            skip(
                &conn,
                Some("vault.exe"),
                "2026-07-25T12:01:00Z",
                "2026-07-25T12:02:00Z",
            ),
            skip(&conn, None, "2026-07-25T12:01:00Z", "2026-07-25T12:03:00Z"),
        ];
        together.sort_by_key(|span| span.id);

        let found = overlapping(
            &conn,
            at("2026-07-25T12:00:00Z"),
            at("2026-07-25T12:05:00Z"),
        )
        .expect("the window should be readable");

        assert_eq!(found, together);
    }

    #[test]
    fn an_empty_or_reversed_window_is_empty_rather_than_an_error() {
        let (_dir, conn) = database();
        let t = at("2026-07-25T12:00:00Z");
        skip(
            &conn,
            Some("vault.exe"),
            "2026-07-25T11:00:00Z",
            "2026-07-25T13:00:00Z",
        );

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
}
