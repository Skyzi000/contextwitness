use crate::{StoreError, images, timestamp};

// `created_at` is compared as text, which is time order only for the spelling `timestamp::to_sql`
// writes.
const SELECT_EXPIRED: &str = "SELECT observation_id, byte_size FROM images \
     WHERE created_at < ?1 ORDER BY created_at, observation_id";
const SELECT_OLDEST_FIRST: &str =
    "SELECT observation_id, byte_size FROM images ORDER BY created_at, observation_id";
const SELECT_TOTAL_BYTES: &str = "SELECT coalesce(sum(byte_size), 0) FROM images";

const BYTES_PER_GIB: u64 = 1 << 30;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Sweep {
    pub expired: usize,
    pub over_budget: usize,
    pub freed_bytes: u64,
    pub skipped: usize,
}

pub fn sweep(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    now: chrono::DateTime<chrono::Utc>,
    retention_days: u32,
    max_gib: u64,
) -> Result<Sweep, StoreError> {
    let mut swept = Sweep::default();

    // A cutoff `to_sql` refuses — a retention long enough to reach past year 0 — selects nothing
    // rather than failing the whole sweep.
    let cutoff = chrono::TimeDelta::try_days(i64::from(retention_days))
        .and_then(|span| now.checked_sub_signed(span))
        .and_then(|cutoff| timestamp::to_sql(cutoff).ok());
    if let Some(cutoff) = cutoff {
        for (id, byte_size) in candidates(conn, SELECT_EXPIRED, rusqlite::params![cutoff])? {
            if delete_one(conn, root, &id, byte_size, &mut swept)? {
                swept.expired += 1;
            }
        }
    }

    let cap = i64::try_from(max_gib.saturating_mul(BYTES_PER_GIB)).unwrap_or(i64::MAX);
    let mut total: i64 = conn
        .query_one(SELECT_TOTAL_BYTES, [], |row| row.get(0))
        .map_err(|source| StoreError::Sql { source })?;
    if total > cap {
        // ponytail: the whole table is read to find the oldest rows — no index on `created_at`, and
        // this only runs when the budget is already exceeded. Add one if the sweep gets slow.
        for (id, byte_size) in candidates(conn, SELECT_OLDEST_FIRST, [])? {
            if total <= cap {
                break;
            }
            if delete_one(conn, root, &id, byte_size, &mut swept)? {
                swept.over_budget += 1;
                total = total.saturating_sub(byte_size);
            }
        }
    }

    Ok(swept)
}

fn delete_one(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    id: &str,
    byte_size: i64,
    swept: &mut Sweep,
) -> Result<bool, StoreError> {
    // `images::delete` looks a row up in the canonical spelling of the id, so a row holding any
    // other spelling would delete nothing and answer `Ok` — counted as freed while it still
    // charges its `byte_size` against the budget.
    let Some(parsed) = ulid::Ulid::from_string(id)
        .ok()
        .filter(|parsed| parsed.to_string() == id)
    else {
        swept.skipped += 1;
        return Ok(false);
    };

    match images::delete(conn, root, parsed) {
        Ok(()) => {
            swept.freed_bytes += u64::try_from(byte_size).unwrap_or(0);
            Ok(true)
        }
        // One file the filesystem will not take back — or one row whose stored path cannot be read
        // back — must not decide whether every other expired image is deleted.
        Err(StoreError::ImageIo { .. } | StoreError::Encoding { .. }) => {
            swept.skipped += 1;
            Ok(false)
        }
        Err(other) => Err(other),
    }
}

fn candidates(
    conn: &rusqlite::Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<(String, i64)>, StoreError> {
    let mut statement = conn
        .prepare(sql)
        .map_err(|source| StoreError::Sql { source })?;
    let selected = statement
        .query_map(params, |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|source| StoreError::Sql { source })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| StoreError::Sql { source })?;
    Ok(selected)
}
