use crate::control_cursor::{head, load_cursor, store_cursor};
use crate::{StoreError, images, timestamp};

// `created_at` is compared as text, which is time order only for the spelling `timestamp::to_sql`
// writes.
const SELECT_EXPIRED_PAGE: &str = "SELECT created_at, observation_id, byte_size FROM images \
     WHERE (created_at, observation_id) > (?1, ?2) AND created_at < ?3 \
     ORDER BY created_at, observation_id LIMIT ?4";
const SELECT_EXPIRED_REMAINS: &str = "SELECT 1 FROM images \
     WHERE (created_at, observation_id) > (?1, ?2) AND created_at < ?3 LIMIT 1";
const SELECT_OLDEST_PAGE: &str = "SELECT created_at, observation_id, byte_size FROM images \
     WHERE (created_at, observation_id) > (?1, ?2) AND (created_at, observation_id) <= (?3, ?4) \
     ORDER BY created_at, observation_id LIMIT ?5";
const SELECT_HIGH_WATER: &str = "SELECT created_at, observation_id, byte_size FROM images \
     ORDER BY created_at DESC, observation_id DESC LIMIT 1";
const SELECT_TOTAL_BYTES: &str = "SELECT total_bytes FROM image_budget WHERE id = 1";

/// The `control_state` key the expired pass's cursor is stored under.
const EXPIRED_CURSOR: &str = "retention_expired_cursor";
/// The `control_state` key the budget pass's cursor is stored under.
const BUDGET_CURSOR: &str = "retention_budget_cursor";

const BYTES_PER_GIB: u64 = 1 << 30;

#[cfg(not(test))]
const BATCH: i64 = 1000;
#[cfg(test)]
const BATCH: i64 = 3;

#[cfg(not(test))]
const MAX_PAGES: u32 = 100;
#[cfg(test)]
const MAX_PAGES: u32 = 4;

const WORK_SPAN: std::time::Duration = std::time::Duration::from_secs(1);
const PAUSE: std::time::Duration = std::time::Duration::from_millis(250);

struct Pacer {
    since: std::time::Instant,
}

impl Pacer {
    fn due(&self, now: std::time::Instant) -> bool {
        now.saturating_duration_since(self.since) >= WORK_SPAN
    }

    fn pace(&mut self) {
        if self.due(std::time::Instant::now()) {
            std::thread::sleep(PAUSE);
            self.since = std::time::Instant::now();
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Sweep {
    pub expired: usize,
    pub over_budget: usize,
    pub freed_bytes: u64,
    pub skipped: usize,
    pub missing: usize,
    pub truncated: bool,
}

/// Delete what retention and the size cap no longer keep.
///
/// A pass that ran out of pages resumes at its cursor; only a pass that finishes — its empty page,
/// or the budget goal met inside the pass — returns that cursor to the head, and the under-cap
/// return taken before the budget pass leaves its cursor where it stood. Both cursors live in
/// `control_state`, so they survive a restart, and a crash mid-sweep loses at most the progress of
/// the invocation it interrupted.
pub fn sweep(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    now: chrono::DateTime<chrono::Utc>,
    retention_days: u32,
    max_gib: u64,
) -> Result<Sweep, StoreError> {
    let mut swept = Sweep::default();
    let mut pacer = Pacer {
        since: std::time::Instant::now(),
    };

    // A cutoff `to_sql` refuses — a retention long enough to reach past year 0 — selects nothing
    // rather than failing the whole sweep.
    let cutoff = chrono::TimeDelta::try_days(i64::from(retention_days))
        .and_then(|span| now.checked_sub_signed(span))
        .and_then(|cutoff| timestamp::to_sql(cutoff).ok());
    if let Some(cutoff) = cutoff {
        let mut cursor = load_cursor(conn, EXPIRED_CURSOR)?;
        let mut finished = false;
        for _ in 0..MAX_PAGES {
            let batch = page(
                conn,
                SELECT_EXPIRED_PAGE,
                rusqlite::params![cursor.0, cursor.1, cutoff, BATCH],
            )?;
            if batch.is_empty() {
                finished = true;
                break;
            }
            for (created_at, id, byte_size) in &batch {
                // Unconditionally: a row `delete_one` refuses must not be offered to every
                // later page again.
                cursor = (created_at.clone(), id.clone());
                if delete_one(conn, root, id, *byte_size, &mut swept, &mut pacer)?.is_some() {
                    swept.expired += 1;
                }
            }
        }
        // The last permitted page can drain the range without a further page running to find it
        // empty.
        if !finished {
            let mut statement = conn
                .prepare(SELECT_EXPIRED_REMAINS)
                .map_err(|source| StoreError::Sql { source })?;
            finished = !statement
                .exists(rusqlite::params![cursor.0, cursor.1, cutoff])
                .map_err(|source| StoreError::Sql { source })?;
        }
        if finished {
            cursor = head();
        } else {
            swept.truncated = true;
        }
        store_cursor(conn, EXPIRED_CURSOR, &cursor)?;
    }

    let cap = i64::try_from(max_gib.saturating_mul(BYTES_PER_GIB)).unwrap_or(i64::MAX);
    if total_bytes(conn)? <= cap {
        return Ok(swept);
    }
    // Read once, before the first page: the pass follows no key above the mark, so a save keyed at
    // the present cannot extend it, and a save keyed at or below it is inside it.
    let Some((high_created_at, high_id, _)) = page(conn, SELECT_HIGH_WATER, [])?.into_iter().next()
    else {
        return Ok(swept);
    };
    let mut cursor = load_cursor(conn, BUDGET_CURSOR)?;
    let mut finished = false;
    for _ in 0..MAX_PAGES {
        let mut total = total_bytes(conn)?;
        if total <= cap {
            finished = true;
            break;
        }
        let batch = page(
            conn,
            SELECT_OLDEST_PAGE,
            rusqlite::params![cursor.0, cursor.1, high_created_at, high_id, BATCH],
        )?;
        if batch.is_empty() {
            finished = true;
            break;
        }
        for (created_at, id, byte_size) in &batch {
            if total <= cap {
                break;
            }
            // After the break: a fetched row this pass never examined stays ahead of the cursor.
            cursor = (created_at.clone(), id.clone());
            if let Some(outcome) = delete_one(conn, root, id, *byte_size, &mut swept, &mut pacer)? {
                swept.over_budget += 1;
                // Something else deleted the row, and this total was read before that: it charges
                // for this row and for whatever else went with it, so subtracting one row's size
                // leaves the pass deleting past the cap. What `image_budget` says now is what is
                // actually stored.
                total = if outcome == images::DeleteOutcome::NoRow {
                    total_bytes(conn)?
                } else {
                    total.saturating_sub(*byte_size)
                };
            }
        }
    }
    // The last permitted page can reach the cap without the loop head running again to see it.
    if !finished {
        finished = total_bytes(conn)? <= cap;
    }
    if finished {
        cursor = head();
    } else {
        swept.truncated = true;
    }
    store_cursor(conn, BUDGET_CURSOR, &cursor)?;

    Ok(swept)
}

fn total_bytes(conn: &rusqlite::Connection) -> Result<i64, StoreError> {
    conn.query_one(SELECT_TOTAL_BYTES, [], |row| row.get(0))
        .map_err(|source| StoreError::Sql { source })
}

/// `None` when the row is still registered afterwards; otherwise what the goal state was reached
/// by.
fn delete_one(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    id: &str,
    byte_size: i64,
    swept: &mut Sweep,
    pacer: &mut Pacer,
) -> Result<Option<images::DeleteOutcome>, StoreError> {
    // `images::delete` looks a row up in the canonical spelling of the id, so a row holding any
    // other spelling would delete nothing and answer `Ok` — counted as freed while it still
    // charges its `byte_size` against the budget.
    let Some(parsed) = ulid::Ulid::from_string(id)
        .ok()
        .filter(|parsed| parsed.to_string() == id)
    else {
        swept.skipped += 1;
        return Ok(None);
    };

    let deleted = images::delete(conn, root, parsed);
    pacer.pace();
    match deleted {
        Ok(outcome) => {
            if outcome == images::DeleteOutcome::MissingFile {
                swept.missing += 1;
            }
            swept.freed_bytes += u64::try_from(byte_size).unwrap_or(0);
            Ok(Some(outcome))
        }
        // One file the filesystem will not take back — or one row whose stored path cannot be read
        // back — must not decide whether every other expired image is deleted.
        Err(StoreError::ImageIo { .. } | StoreError::Encoding { .. }) => {
            swept.skipped += 1;
            Ok(None)
        }
        Err(other) => Err(other),
    }
}

fn page(
    conn: &rusqlite::Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<(String, String, i64)>, StoreError> {
    let mut statement = conn
        .prepare(sql)
        .map_err(|source| StoreError::Sql { source })?;
    let selected = statement
        .query_map(params, |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|source| StoreError::Sql { source })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| StoreError::Sql { source })?;
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::{
        BATCH, BUDGET_CURSOR, BYTES_PER_GIB, EXPIRED_CURSOR, MAX_PAGES, PAUSE, Pacer, Sweep,
        WORK_SPAN, sweep,
    };
    use crate::{db, images, timestamp};
    use chrono::{DateTime, TimeDelta, TimeZone, Utc};
    use tempfile::{TempDir, tempdir};

    const PER_PAGE: usize = BATCH as usize;
    /// A retention no test row is old enough to reach, so the budget pass runs on its own.
    const KEEP_EVERYTHING: u32 = 3650;

    fn store() -> (TempDir, rusqlite::Connection, std::path::PathBuf) {
        let dir = tempdir().expect("the temporary store directory should be creatable");
        let conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the fresh database should initialize");
        let root = dir.path().join("images");
        (dir, conn, root)
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0)
            .single()
            .expect("the test timestamp should be valid")
    }

    fn gib(count: i64) -> i64 {
        count * i64::try_from(BYTES_PER_GIB).expect("a gibibyte should fit SQLite's INTEGER")
    }

    /// Registers a row with no file beside it: `images::delete` removes the row of a file that is
    /// already gone, so a sweep can be measured without encoding a picture per row.
    fn register(
        conn: &rusqlite::Connection,
        index: u128,
        created_at: DateTime<Utc>,
        byte_size: i64,
    ) -> String {
        let id = ulid::Ulid::from(index);
        register_spelled(conn, &id.to_string(), id, created_at, byte_size)
    }

    fn register_spelled(
        conn: &rusqlite::Connection,
        stored_id: &str,
        id: ulid::Ulid,
        created_at: DateTime<Utc>,
        byte_size: i64,
    ) -> String {
        let spelled =
            timestamp::to_sql(created_at).expect("the test timestamp should be spellable");
        conn.execute(
            "INSERT INTO observations (id, source, observed_at, schema_version, payload) \
             VALUES (?1, 'screen', ?2, 1, '{}')",
            rusqlite::params![stored_id, spelled],
        )
        .expect("the test observation should be storable");
        conn.execute(
            "INSERT INTO images (observation_id, relative_path, byte_size, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                stored_id,
                images::legacy_path(id, created_at),
                byte_size,
                spelled
            ],
        )
        .expect("the test image row should be storable");
        stored_id.to_owned()
    }

    /// Enough rows to exhaust every page of one invocation, each refused by `delete_one` for a
    /// spelling that is not the canonical ULID form, and each on a key of its own.
    fn register_refused_head(
        conn: &rusqlite::Connection,
        start: DateTime<Utc>,
        byte_size: i64,
    ) -> Vec<String> {
        (0..PER_PAGE * MAX_PAGES as usize)
            .map(|step| {
                let id = ulid::Ulid::from(10 + step as u128);
                register_spelled(
                    conn,
                    &id.to_string().to_lowercase(),
                    id,
                    start + TimeDelta::minutes(step as i64),
                    byte_size,
                )
            })
            .collect()
    }

    fn registered(conn: &rusqlite::Connection) -> Vec<String> {
        let mut statement = conn
            .prepare("SELECT observation_id FROM images ORDER BY created_at, observation_id")
            .expect("the image rows should be queryable");
        let rows = statement
            .query_map([], |row| row.get(0))
            .expect("the image row query should run");
        rows.collect::<rusqlite::Result<Vec<String>>>()
            .expect("the image rows should be readable")
    }

    fn budget(conn: &rusqlite::Connection) -> i64 {
        conn.query_one("SELECT total_bytes FROM image_budget", [], |row| row.get(0))
            .expect("the image budget should be readable")
    }

    /// A row with its file in place, so deleting it is a removal and not the missing-file case
    /// every other planted row stands for.
    fn save_registered(
        conn: &mut rusqlite::Connection,
        root: &std::path::Path,
        index: u128,
        created_at: DateTime<Utc>,
    ) -> String {
        let id = ulid::Ulid::from(index);
        let spelled =
            timestamp::to_sql(created_at).expect("the test timestamp should be spellable");
        conn.execute(
            "INSERT INTO observations (id, source, observed_at, schema_version, payload) \
             VALUES (?1, 'screen', ?2, 1, '{}')",
            rusqlite::params![id.to_string(), spelled],
        )
        .expect("the test observation should be storable");
        let pixels = vec![7u8; 4 * 3 * 3];
        let relative = images::relative_path(id, created_at.fixed_offset(), None, None);
        images::save(conn, root, id, &relative, &pixels, 4, 3, 75.0, created_at)
            .expect("the test image should be saved");
        relative
    }

    /// A committed delete landing in the middle of a pass, standing in for the concurrent deleter a
    /// single-threaded test cannot schedule: the trigger runs in the transaction that deletes
    /// `after`.
    fn delete_when_deleted(conn: &rusqlite::Connection, after: &str, taken: [&str; 2]) {
        let [first, second] = taken;
        conn.execute_batch(&format!(
            "CREATE TRIGGER concurrent_deleter AFTER DELETE ON images \
             WHEN old.observation_id = '{after}' BEGIN \
               DELETE FROM images WHERE observation_id IN ('{first}', '{second}'); \
             END"
        ))
        .expect("the concurrent-deleter trigger should be creatable");
    }

    /// A committed save landing in the middle of a pass, standing in for the concurrent saver a
    /// single-threaded test cannot schedule: the trigger runs in the transaction that deletes
    /// `after`.
    fn save_when_deleted(
        conn: &rusqlite::Connection,
        after: &str,
        late: ulid::Ulid,
        created_at: DateTime<Utc>,
        byte_size: i64,
    ) -> String {
        let spelled =
            timestamp::to_sql(created_at).expect("the test timestamp should be spellable");
        let path = images::legacy_path(late, created_at);
        conn.execute_batch(&format!(
            "CREATE TRIGGER late_saver AFTER DELETE ON images \
             WHEN old.observation_id = '{after}' BEGIN \
               INSERT INTO observations (id, source, observed_at, schema_version, payload) \
                 VALUES ('{late}', 'screen', '{spelled}', 1, '{{}}'); \
               INSERT INTO images (observation_id, relative_path, byte_size, created_at) \
                 VALUES ('{late}', '{path}', {byte_size}, '{spelled}'); \
             END"
        ))
        .expect("the late-saver trigger should be creatable");
        late.to_string()
    }

    #[test]
    fn the_expired_pass_crosses_batch_boundaries_without_missing_or_repeating_a_row() {
        let (_dir, mut conn, root) = store();
        // One `created_at` for every row, so the cursor's second column is the only thing that can
        // carry the pass past the first page.
        let created_at = now() - TimeDelta::days(30);
        let count = 2 * PER_PAGE + 1;
        for index in 1..=count {
            register(&conn, index as u128, created_at, 100);
        }

        let swept = sweep(&mut conn, &root, now(), 7, 1).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                expired: count,
                freed_bytes: 100 * count as u64,
                missing: count,
                ..Sweep::default()
            }
        );
        assert!(registered(&conn).is_empty());
        assert_eq!(budget(&conn), 0);
    }

    #[test]
    fn the_expired_pass_reads_a_row_it_cannot_delete_only_once() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        // The row `images::delete` will not match sits at the end of the first page, where a cursor
        // that stopped at it rather than past it would read it again on the next.
        let unmatched = ulid::Ulid::from(10u128);
        let planted = (0..2 * PER_PAGE + 1)
            .map(|step| {
                let created_at = start + TimeDelta::minutes(step as i64);
                if step == PER_PAGE - 1 {
                    register_spelled(
                        &conn,
                        &unmatched.to_string().to_lowercase(),
                        unmatched,
                        created_at,
                        100,
                    )
                } else {
                    register(&conn, 100 + step as u128, created_at, 100)
                }
            })
            .collect::<Vec<_>>();

        let swept = sweep(&mut conn, &root, now(), 7, 1).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                expired: 2 * PER_PAGE,
                freed_bytes: 200 * PER_PAGE as u64,
                skipped: 1,
                missing: 2 * PER_PAGE,
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), [planted[PER_PAGE - 1].clone()]);
        assert_eq!(budget(&conn), 100);
    }

    #[test]
    fn the_expired_pass_keeps_the_rows_at_and_after_the_cutoff() {
        let (_dir, mut conn, root) = store();
        let cutoff = now() - TimeDelta::days(7);
        let mut kept = Vec::new();
        for step in 0..PER_PAGE + 1 {
            let offset = TimeDelta::minutes(step as i64);
            register(
                &conn,
                1 + step as u128,
                cutoff - offset - TimeDelta::nanoseconds(1),
                100,
            );
            kept.push(register(&conn, 100 + step as u128, cutoff + offset, 100));
        }

        let swept = sweep(&mut conn, &root, now(), 7, 1).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                expired: PER_PAGE + 1,
                freed_bytes: 100 * (PER_PAGE as u64 + 1),
                missing: PER_PAGE + 1,
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), kept);
        assert_eq!(budget(&conn), 100 * (PER_PAGE as i64 + 1));
    }

    #[test]
    fn the_budget_pass_stops_at_the_cap_across_a_batch_boundary() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let planted = (0..2 * PER_PAGE + 2)
            .map(|step| {
                register(
                    &conn,
                    1 + step as u128,
                    start + TimeDelta::minutes(step as i64),
                    gib(1),
                )
            })
            .collect::<Vec<_>>();
        let cap = PER_PAGE as u64 + 1;

        let swept =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, cap).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                over_budget: PER_PAGE + 1,
                freed_bytes: (PER_PAGE as u64 + 1) * BYTES_PER_GIB,
                missing: PER_PAGE + 1,
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), planted[PER_PAGE + 1..]);
        assert_eq!(budget(&conn), gib(PER_PAGE as i64 + 1));
    }

    #[test]
    fn a_row_the_budget_pass_cannot_delete_is_skipped_once_and_still_charges_the_budget() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        // The row `images::delete` will not match sits at the end of the first page, where a cursor
        // that stopped at it rather than past it would read it again on the next.
        let unmatched = ulid::Ulid::from(10u128);
        let planted = (0..2 * PER_PAGE + 2)
            .map(|step| {
                let created_at = start + TimeDelta::minutes(step as i64);
                if step == PER_PAGE - 1 {
                    register_spelled(
                        &conn,
                        &unmatched.to_string().to_lowercase(),
                        unmatched,
                        created_at,
                        gib(1),
                    )
                } else {
                    register(&conn, 100 + step as u128, created_at, gib(1))
                }
            })
            .collect::<Vec<_>>();

        let swept = sweep(&mut conn, &root, now(), KEEP_EVERYTHING, PER_PAGE as u64)
            .expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                over_budget: PER_PAGE + 2,
                freed_bytes: (PER_PAGE as u64 + 2) * BYTES_PER_GIB,
                skipped: 1,
                missing: PER_PAGE + 2,
                ..Sweep::default()
            }
        );
        let left = registered(&conn);
        assert_eq!(left.len(), PER_PAGE);
        assert_eq!(left[0], planted[PER_PAGE - 1]);
        assert_eq!(budget(&conn), gib(PER_PAGE as i64));
    }

    #[test]
    fn the_expired_pass_counts_only_the_rows_whose_file_was_already_gone_as_missing() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        save_registered(&mut conn, &root, 1, start);
        let gone = save_registered(&mut conn, &root, 2, start + TimeDelta::minutes(1));
        std::fs::remove_file(root.join(&gone))
            .expect("the second image should be removable without touching its row");
        let stored = u64::try_from(budget(&conn)).expect("the test budget should fit u64");

        let swept = sweep(&mut conn, &root, now(), 7, 1).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                expired: 2,
                freed_bytes: stored,
                missing: 1,
                ..Sweep::default()
            }
        );
        assert!(registered(&conn).is_empty());
        assert!(!root.join(&gone).exists());
    }

    #[test]
    fn the_budget_pass_stops_at_the_cap_when_rows_ahead_of_it_go_with_the_one_it_deletes() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let planted = (0..2 * PER_PAGE)
            .map(|step| {
                register(
                    &conn,
                    100 + step as u128,
                    start + TimeDelta::minutes(step as i64),
                    gib(1),
                )
            })
            .collect::<Vec<_>>();
        // One row the pass reads next and one it would only reach on a later page: subtracting the
        // size of the row it reaches leaves the total it carries still charging for the other.
        delete_when_deleted(&conn, &planted[0], [&planted[1], &planted[4]]);

        let swept =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 3).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                over_budget: 2,
                freed_bytes: 2 * BYTES_PER_GIB,
                missing: 1,
                ..Sweep::default()
            }
        );
        assert_eq!(
            registered(&conn),
            [planted[2].clone(), planted[3].clone(), planted[5].clone()]
        );
        assert_eq!(budget(&conn), gib(3));
    }

    #[test]
    fn the_image_budget_follows_inserts_and_deletes_and_a_rollback_moves_nothing() {
        let (_dir, mut conn, root) = store();
        assert_eq!(budget(&conn), 0);

        let first = register(&conn, 1, now(), 400);
        assert_eq!(budget(&conn), 400);
        register(&conn, 2, now(), 600);
        assert_eq!(budget(&conn), 1000);

        let rolled_back = conn
            .transaction()
            .expect("the test transaction should begin");
        register(&rolled_back, 3, now(), 900);
        rolled_back
            .execute("DELETE FROM images WHERE observation_id = ?1", [&first])
            .expect("the test row should be deletable");
        assert_eq!(budget(&rolled_back), 1500);
        rolled_back
            .rollback()
            .expect("the test transaction should roll back");
        assert_eq!(budget(&conn), 1000);

        images::delete(&mut conn, &root, ulid::Ulid::from(1u128))
            .expect("the registered image should delete");
        assert_eq!(budget(&conn), 600);
    }

    #[test]
    fn the_budget_pass_leaves_a_row_keyed_past_the_high_water() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let planted = (0..2 * PER_PAGE)
            .map(|step| {
                register(
                    &conn,
                    100 + step as u128,
                    start + TimeDelta::minutes(step as i64),
                    gib(1),
                )
            })
            .collect::<Vec<_>>();
        let late = save_when_deleted(&conn, &planted[0], ulid::Ulid::from(9u128), now(), gib(1));

        // A cap of zero: every row the pass may reach has to go, so what the late row survives on
        // is the mark the pass took before its first page.
        let swept =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                over_budget: 2 * PER_PAGE,
                freed_bytes: 2 * PER_PAGE as u64 * BYTES_PER_GIB,
                missing: 2 * PER_PAGE,
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), [late]);
        assert_eq!(budget(&conn), gib(1));
    }

    #[test]
    fn the_budget_pass_deletes_a_row_saved_below_the_high_water() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let planted = (0..2 * PER_PAGE)
            .map(|step| {
                register(
                    &conn,
                    100 + step as u128,
                    start + TimeDelta::minutes(step as i64),
                    gib(1),
                )
            })
            .collect::<Vec<_>>();
        // Keyed between the last row of the first page and the first row of the second, so the
        // cursor has not passed it when the pass fetches again.
        save_when_deleted(
            &conn,
            &planted[PER_PAGE - 1],
            ulid::Ulid::from(9u128),
            start + TimeDelta::minutes(PER_PAGE as i64 - 1) + TimeDelta::seconds(30),
            gib(1),
        );

        let swept =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                over_budget: 2 * PER_PAGE + 1,
                freed_bytes: (2 * PER_PAGE as u64 + 1) * BYTES_PER_GIB,
                missing: 2 * PER_PAGE + 1,
                ..Sweep::default()
            }
        );
        assert!(registered(&conn).is_empty());
        assert_eq!(budget(&conn), 0);
    }

    #[test]
    fn the_budget_pass_resumes_at_the_oldest_row_it_had_not_reached() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let planted = (0..2 * PER_PAGE)
            .map(|step| {
                register(
                    &conn,
                    100 + step as u128,
                    start + TimeDelta::minutes(step as i64),
                    gib(1),
                )
            })
            .collect::<Vec<_>>();
        // Saved while the first page is still being worked through, and large enough to put the
        // total back over the cap the pass has just stopped at, with that page's oldest unreached
        // row still in the table.
        let late = save_when_deleted(&conn, &planted[1], ulid::Ulid::from(9u128), now(), gib(1));

        let swept = sweep(
            &mut conn,
            &root,
            now(),
            KEEP_EVERYTHING,
            2 * PER_PAGE as u64 - 2,
        )
        .expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                over_budget: PER_PAGE,
                freed_bytes: PER_PAGE as u64 * BYTES_PER_GIB,
                missing: PER_PAGE,
                ..Sweep::default()
            }
        );
        let mut left = planted[PER_PAGE..].to_vec();
        left.push(late);
        assert_eq!(registered(&conn), left);
        assert_eq!(budget(&conn), gib(2 * PER_PAGE as i64 - 2));
    }

    #[test]
    fn the_expired_pass_that_runs_out_of_pages_says_so_and_the_next_sweep_finishes_the_job() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let reach = PER_PAGE * MAX_PAGES as usize;
        for index in 0..=reach {
            register(
                &conn,
                1 + index as u128,
                start + TimeDelta::minutes(index as i64),
                100,
            );
        }

        let first = sweep(&mut conn, &root, now(), 7, 1).expect("the first sweep should run");
        assert_eq!(
            first,
            Sweep {
                expired: reach,
                freed_bytes: 100 * reach as u64,
                missing: reach,
                truncated: true,
                ..Sweep::default()
            }
        );

        let second = sweep(&mut conn, &root, now(), 7, 1).expect("the second sweep should run");
        assert_eq!(
            second,
            Sweep {
                expired: 1,
                freed_bytes: 100,
                missing: 1,
                ..Sweep::default()
            }
        );
        assert!(registered(&conn).is_empty());
    }

    #[test]
    fn the_budget_pass_that_runs_out_of_pages_says_so_and_the_next_sweep_finishes_the_job() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let reach = PER_PAGE * MAX_PAGES as usize;
        for index in 0..=reach {
            register(
                &conn,
                1 + index as u128,
                start + TimeDelta::minutes(index as i64),
                gib(1),
            );
        }

        let first =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0).expect("the first sweep should run");
        assert_eq!(
            first,
            Sweep {
                over_budget: reach,
                freed_bytes: reach as u64 * BYTES_PER_GIB,
                missing: reach,
                truncated: true,
                ..Sweep::default()
            }
        );

        let second = sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0)
            .expect("the second sweep should run");
        assert_eq!(
            second,
            Sweep {
                over_budget: 1,
                freed_bytes: BYTES_PER_GIB,
                missing: 1,
                ..Sweep::default()
            }
        );
        assert!(registered(&conn).is_empty());
        assert_eq!(budget(&conn), 0);
    }

    #[test]
    fn the_expired_pass_resumes_past_rows_it_cannot_delete_and_starts_over_once_it_finishes() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let refused = register_refused_head(&conn, start, 100);
        for step in 0..2usize {
            register(
                &conn,
                100 + step as u128,
                start + TimeDelta::minutes((refused.len() + step) as i64),
                100,
            );
        }
        let first = sweep(&mut conn, &root, now(), 7, 1).expect("the first sweep should run");
        assert_eq!(
            first,
            Sweep {
                skipped: refused.len(),
                truncated: true,
                ..Sweep::default()
            }
        );

        let second = sweep(&mut conn, &root, now(), 7, 1).expect("the second sweep should run");
        assert_eq!(
            second,
            Sweep {
                expired: 2,
                freed_bytes: 200,
                missing: 2,
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), refused);
        assert_eq!(budget(&conn), 100 * refused.len() as i64);

        let third = sweep(&mut conn, &root, now(), 7, 1).expect("the third sweep should run");
        assert_eq!(
            third,
            Sweep {
                skipped: refused.len(),
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), refused);
    }

    #[test]
    fn the_budget_pass_resumes_past_rows_it_cannot_delete_and_starts_over_once_it_finishes() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let refused = register_refused_head(&conn, start, gib(1));
        for step in 0..2usize {
            register(
                &conn,
                100 + step as u128,
                start + TimeDelta::minutes((refused.len() + step) as i64),
                gib(1),
            );
        }
        let first =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0).expect("the first sweep should run");
        assert_eq!(
            first,
            Sweep {
                skipped: refused.len(),
                truncated: true,
                ..Sweep::default()
            }
        );

        let second = sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0)
            .expect("the second sweep should run");
        assert_eq!(
            second,
            Sweep {
                over_budget: 2,
                freed_bytes: 2 * BYTES_PER_GIB,
                missing: 2,
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), refused);
        assert_eq!(budget(&conn), gib(refused.len() as i64));

        let third =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0).expect("the third sweep should run");
        assert_eq!(
            third,
            Sweep {
                skipped: refused.len(),
                truncated: true,
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), refused);
    }

    #[test]
    fn an_under_cap_sweep_leaves_the_budget_cursor_where_it_stood() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let refused = register_refused_head(&conn, start, gib(1));
        for step in 0..2usize {
            register(
                &conn,
                100 + step as u128,
                start + TimeDelta::minutes((refused.len() + step) as i64),
                gib(1),
            );
        }
        let first =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0).expect("the first sweep should run");
        assert_eq!(
            first,
            Sweep {
                skipped: refused.len(),
                truncated: true,
                ..Sweep::default()
            }
        );

        let under_cap = sweep(
            &mut conn,
            &root,
            now(),
            KEEP_EVERYTHING,
            refused.len() as u64 + 2,
        )
        .expect("the under-cap sweep should run");
        assert_eq!(under_cap, Sweep::default());

        let third =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0).expect("the third sweep should run");
        assert_eq!(
            third,
            Sweep {
                over_budget: 2,
                freed_bytes: 2 * BYTES_PER_GIB,
                missing: 2,
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), refused);
        assert_eq!(budget(&conn), gib(refused.len() as i64));
    }

    #[test]
    fn the_expired_cursor_survives_a_reopen_of_the_store() {
        let (dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let refused = register_refused_head(&conn, start, 100);
        for step in 0..2usize {
            register(
                &conn,
                100 + step as u128,
                start + TimeDelta::minutes((refused.len() + step) as i64),
                100,
            );
        }

        let first = sweep(&mut conn, &root, now(), 7, 1).expect("the first sweep should run");
        assert_eq!(
            first,
            Sweep {
                skipped: refused.len(),
                truncated: true,
                ..Sweep::default()
            }
        );

        drop(conn);
        let mut conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the store should reopen in place");

        let second = sweep(&mut conn, &root, now(), 7, 1).expect("the second sweep should run");
        assert_eq!(
            second,
            Sweep {
                expired: 2,
                freed_bytes: 200,
                missing: 2,
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), refused);
    }

    #[test]
    fn the_budget_cursor_survives_a_reopen_of_the_store() {
        let (dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let refused = register_refused_head(&conn, start, gib(1));
        for step in 0..2usize {
            register(
                &conn,
                100 + step as u128,
                start + TimeDelta::minutes((refused.len() + step) as i64),
                gib(1),
            );
        }

        let first =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0).expect("the first sweep should run");
        assert_eq!(
            first,
            Sweep {
                skipped: refused.len(),
                truncated: true,
                ..Sweep::default()
            }
        );

        drop(conn);
        let mut conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the store should reopen in place");

        let second = sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0)
            .expect("the second sweep should run");
        assert_eq!(
            second,
            Sweep {
                over_budget: 2,
                freed_bytes: 2 * BYTES_PER_GIB,
                missing: 2,
                ..Sweep::default()
            }
        );
        assert_eq!(registered(&conn), refused);
    }

    #[test]
    fn a_budget_pass_that_reaches_the_cap_on_its_last_page_is_finished_not_truncated() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let reach = PER_PAGE * MAX_PAGES as usize;
        for index in 0..reach + 2 {
            register(
                &conn,
                1 + index as u128,
                start + TimeDelta::minutes(index as i64),
                gib(1),
            );
        }

        let first =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 2).expect("the first sweep should run");
        assert_eq!(
            first,
            Sweep {
                over_budget: reach,
                freed_bytes: reach as u64 * BYTES_PER_GIB,
                missing: reach,
                ..Sweep::default()
            }
        );

        // Below every key the pass examined, so only a cursor returned to the head reaches it.
        register(&conn, 500, start + TimeDelta::seconds(30), gib(1));

        let second = sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0)
            .expect("the second sweep should run");
        assert_eq!(
            second,
            Sweep {
                over_budget: 3,
                freed_bytes: 3 * BYTES_PER_GIB,
                missing: 3,
                ..Sweep::default()
            }
        );
        assert!(registered(&conn).is_empty());
        assert_eq!(budget(&conn), 0);
    }

    #[test]
    fn an_expired_pass_that_drains_the_range_in_full_pages_is_finished_not_truncated() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        let reach = PER_PAGE * MAX_PAGES as usize;
        for index in 0..reach {
            register(
                &conn,
                1 + index as u128,
                start + TimeDelta::minutes(index as i64),
                100,
            );
        }

        let swept = sweep(&mut conn, &root, now(), 7, 1).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                expired: reach,
                freed_bytes: 100 * reach as u64,
                missing: reach,
                ..Sweep::default()
            }
        );
        assert!(registered(&conn).is_empty());
    }

    #[test]
    fn a_foreign_expired_cursor_is_read_as_the_head() {
        let (_dir, mut conn, root) = store();
        let start = now() - TimeDelta::days(30);
        for index in 0..2u128 {
            register(&conn, 1 + index, start, 100);
        }
        conn.execute(
            "INSERT INTO control_state (key, value) VALUES (?1, 'not a cursor')",
            [EXPIRED_CURSOR],
        )
        .expect("the foreign control row should be storable");

        let swept = sweep(&mut conn, &root, now(), 7, 1).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                expired: 2,
                freed_bytes: 200,
                missing: 2,
                ..Sweep::default()
            }
        );
        assert!(registered(&conn).is_empty());
    }

    #[test]
    fn a_budget_cursor_that_is_null_is_read_as_the_head() {
        let (_dir, mut conn, root) = store();
        for index in 0..2u128 {
            register(&conn, 1 + index, now(), gib(1));
        }
        conn.execute(
            "INSERT INTO control_state (key, value) VALUES (?1, NULL)",
            [BUDGET_CURSOR],
        )
        .expect("the foreign control row should be storable");

        let swept =
            sweep(&mut conn, &root, now(), KEEP_EVERYTHING, 0).expect("the sweep should run");

        assert_eq!(
            swept,
            Sweep {
                over_budget: 2,
                freed_bytes: 2 * BYTES_PER_GIB,
                missing: 2,
                ..Sweep::default()
            }
        );
        assert!(registered(&conn).is_empty());
        assert_eq!(budget(&conn), 0);
    }

    #[test]
    fn the_image_budget_refuses_a_second_row() {
        let (_dir, conn, _root) = store();

        for statement in [
            "INSERT INTO image_budget (id, total_bytes) VALUES (2, 0)",
            "INSERT INTO image_budget (total_bytes) VALUES (0)",
        ] {
            conn.execute(statement, [])
                .expect_err("a second budget row must be refused");
        }

        assert_eq!(budget(&conn), 0);
    }

    #[test]
    fn the_image_budget_row_cannot_be_deleted() {
        let (_dir, conn, _root) = store();
        register(&conn, 1, now(), 400);

        conn.execute("DELETE FROM image_budget WHERE id = 1", [])
            .expect_err("the budget row must not be deletable");

        assert_eq!(budget(&conn), 400);
    }

    #[test]
    fn the_image_budget_refuses_a_total_that_is_not_a_count_of_bytes() {
        let (_dir, conn, _root) = store();
        register(&conn, 1, now(), 400);

        for statement in [
            "UPDATE image_budget SET total_bytes = -1 WHERE id = 1",
            // An integer sum SQLite cannot hold comes back as a REAL, which is the write the type
            // check is here for.
            "UPDATE image_budget SET total_bytes = 9223372036854775807 + 1 WHERE id = 1",
            "UPDATE image_budget SET total_bytes = 'plenty' WHERE id = 1",
        ] {
            conn.execute(statement, [])
                .expect_err("the budget column must refuse the write");
        }

        assert_eq!(budget(&conn), 400);
    }

    #[test]
    fn changing_a_registered_size_moves_the_budget_by_the_difference() {
        let (_dir, conn, _root) = store();
        let first = register(&conn, 1, now(), 400);
        register(&conn, 2, now(), 600);

        conn.execute(
            "UPDATE images SET byte_size = 250 WHERE observation_id = ?1",
            [&first],
        )
        .expect("the registered size should be changeable");

        assert_eq!(budget(&conn), 850);
    }

    #[test]
    fn the_pacer_is_due_a_work_span_after_it_last_resumed() {
        let tick = std::time::Duration::from_nanos(1);
        let start = std::time::Instant::now();
        let mut pacer = Pacer { since: start };
        assert!(!pacer.due(start + WORK_SPAN - tick));
        assert!(pacer.due(start + WORK_SPAN));

        let before = std::time::Instant::now();
        pacer.since = before - WORK_SPAN;
        pacer.pace();
        assert!(pacer.since >= before + PAUSE);
    }
}
