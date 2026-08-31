//! The `control_state` row a paged pass resumes from.
//!
//! One spelling, read and written here alone: two passes that stored their place two ways would
//! each read the other's row as the head and start over.

use crate::StoreError;

const SELECT_CURSOR: &str = "SELECT value FROM control_state WHERE key = ?1";
const UPSERT_CURSOR: &str = "INSERT INTO control_state (key, value) VALUES (?1, ?2) \
     ON CONFLICT(key) DO UPDATE SET value = excluded.value";

/// The spelling `timestamp::to_sql` writes cannot hold it.
const CURSOR_SEPARATOR: char = '|';

/// The `(created_at, observation_id)` key a pass stopped at. Each pass keeps its own
/// `control_state` key to store one under.
pub(crate) type Cursor = (String, String);

pub(crate) fn head() -> Cursor {
    (String::new(), String::new())
}

/// The head is the empty spelling, and every other cursor carries both columns around one
/// separator. Anything else in the row was written by something other than this program: reading it
/// as the head costs one re-scan and can never skip a row.
fn spell_cursor((created_at, id): &Cursor) -> String {
    if created_at.is_empty() {
        String::new()
    } else {
        format!("{created_at}{CURSOR_SEPARATOR}{id}")
    }
}

fn parse_cursor(value: &str) -> Cursor {
    match value.split_once(CURSOR_SEPARATOR) {
        Some((created_at, id)) if !created_at.is_empty() => (created_at.to_owned(), id.to_owned()),
        _ => head(),
    }
}

pub(crate) fn load_cursor(conn: &rusqlite::Connection, key: &str) -> Result<Cursor, StoreError> {
    let mut statement = conn
        .prepare(SELECT_CURSOR)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query([key])
        .map_err(|source| StoreError::Sql { source })?;
    let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? else {
        return Ok(head());
    };
    Ok(parse_cursor(&row.get::<_, String>(0).unwrap_or_default()))
}

pub(crate) fn store_cursor(
    conn: &rusqlite::Connection,
    key: &str,
    cursor: &Cursor,
) -> Result<(), StoreError> {
    conn.execute(UPSERT_CURSOR, rusqlite::params![key, spell_cursor(cursor)])
        .map(|_| ())
        .map_err(|source| StoreError::Sql { source })
}
