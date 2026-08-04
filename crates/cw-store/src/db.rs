//! Owns how the database file is opened and how its schema is allowed to change.

use crate::StoreError;

/// Every migration, in order. A script's index plus one is the schema version it produces, so
/// [`SCHEMA_VERSION`] cannot drift away from the list.
const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

/// The schema version this build understands.
pub const SCHEMA_VERSION: i32 = MIGRATIONS.len() as i32;

/// `PRAGMA application_id` of a ContextWitness database: ASCII "CWit". SQLite keeps this header
/// field so a program can tell whose file it is looking at before it writes anything.
const APPLICATION_ID: i32 = 0x4357_6974;

/// Per-connection settings, applied before anything else. Neither of these changes the database
/// itself, so both are safe to apply to a file this build may turn out to be unable to handle;
/// WAL is not, and is set separately below. The busy timeout comes first: it is the one that has
/// to be in place before any statement that can meet a lock held by another connection.
const CONNECTION_SETTINGS: &str = "\
    PRAGMA busy_timeout = 5000;\n\
    PRAGMA foreign_keys = ON;\n";

/// How long to keep retrying the WAL switch. The same budget as the busy timeout and a separate
/// constant on purpose: SQLite's busy handler does not cover this statement, so the waiting is ours
/// to do.
const WAL_SWITCH_DEADLINE: std::time::Duration = std::time::Duration::from_millis(5000);

/// How long to leave a contending connection alone between attempts. Long enough that the retries
/// are not a spin, short enough to be invisible against the worst wait actually measured.
const WAL_SWITCH_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(2);

/// What the database header says this file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    /// Already carries our application id.
    Ours,
    /// Carries no application id, holds nothing and has no version marker, so there is nothing to
    /// take over.
    FreeToClaim,
}

/// Refuse a database that belongs to another program.
///
/// `user_version` cannot answer this: zero is SQLite's default and most applications never set it,
/// so "version 0" means "not one of ours yet" only once the file is known to be ours in the first
/// place. An id of zero on a database that holds nothing is the one case where there is nothing to
/// take over, so that file is claimed rather than refused.
///
/// A `user_version` that is not zero is not a default — some program wrote it — so a file carrying
/// one is being managed by somebody even while it is still empty, and claiming it would overwrite
/// the marker of an owner we can see is using the field. Zero is the only value that says nothing.
fn ownership(conn: &rusqlite::Connection, path: &std::path::Path) -> Result<Ownership, StoreError> {
    let found: i32 = conn
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|source| StoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;

    match found {
        APPLICATION_ID => Ok(Ownership::Ours),
        0 => {
            let entries: i64 = conn
                .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
                .map_err(|source| StoreError::Open {
                    path: path.to_path_buf(),
                    source,
                })?;
            let version = read_user_version(conn).map_err(|source| StoreError::Open {
                path: path.to_path_buf(),
                source,
            })?;
            if entries == 0 && version == 0 {
                Ok(Ownership::FreeToClaim)
            } else {
                Err(StoreError::ForeignDatabase {
                    path: path.to_path_buf(),
                    found: 0,
                })
            }
        }
        found => Err(StoreError::ForeignDatabase {
            path: path.to_path_buf(),
            found,
        }),
    }
}

/// Read `PRAGMA user_version`, the marker that decides which migrations still have to run.
///
/// The failure is the caller's to map: of the three call sites only [`migrate`] is applying a
/// migration, and an error spelled "failed to migrate" from the two that are still gating the
/// database would name work that never began.
fn read_user_version(conn: &rusqlite::Connection) -> rusqlite::Result<i32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Put the database into WAL mode, failing when it does not take.
///
/// `PRAGMA journal_mode = WAL` reports a refusal by returning the mode the database is actually in
/// rather than by failing, so the returned row is the only evidence the statement did what it says.
/// `query_one` rather than `query_row`, because the mode change commits when the statement halts,
/// which is the step after that row: `query_row` never takes it and rusqlite discards the reset it
/// does instead, so an `SQLITE_IOERR` or `SQLITE_FULL` from that commit would be dropped and this
/// function would report success.
///
/// Converting a rollback-journal database to WAL needs an exclusive lock, and this is the one
/// statement `PRAGMA busy_timeout` does not reach — it comes back `SQLITE_BUSY` at once instead of
/// waiting. Every subsystem opens its own connection, so a first start is several of them meeting
/// on this one conversion, and without the wait below most such starts fail. Reasserting WAL on a
/// database that already has it needs no exclusive lock and succeeds even while another connection
/// is writing, so no open waits here once the file is in WAL.
fn enable_wal(conn: &rusqlite::Connection, path: &std::path::Path) -> Result<(), StoreError> {
    let deadline = std::time::Instant::now() + WAL_SWITCH_DEADLINE;
    loop {
        match conn.query_one("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(actual) if actual.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(actual) => {
                return Err(StoreError::JournalMode {
                    path: path.to_path_buf(),
                    actual,
                });
            }
            Err(source) => {
                let contended = matches!(
                    source.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                );
                if !contended || std::time::Instant::now() >= deadline {
                    return Err(StoreError::Open {
                        path: path.to_path_buf(),
                        source,
                    });
                }
                std::thread::sleep(WAL_SWITCH_RETRY_PAUSE);
            }
        }
    }
}

/// Open the database at `path`, apply the connection contract and bring the schema up to
/// [`SCHEMA_VERSION`].
///
/// Every subsystem opens its own connection, so every open migrates; there is
/// no separate step a caller could forget to call.
pub fn open(path: &std::path::Path) -> Result<rusqlite::Connection, StoreError> {
    if let Some(directory) = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
    {
        // Connection::open on a missing directory fails with SQLITE_CANTOPEN, which names nothing
        // the user can act on, and every subsystem would otherwise have to create the directory.
        std::fs::create_dir_all(directory).map_err(|source| StoreError::Directory {
            path: directory.to_path_buf(),
            source,
        })?;
    }

    let mut conn = rusqlite::Connection::open(path).map_err(|source| StoreError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    conn.execute_batch(CONNECTION_SETTINGS)
        .map_err(|source| StoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;

    // One snapshot, not three reads. `ownership` asks three separate questions, and a statement
    // outside a transaction is its own implicit transaction, so another connection committing its
    // migration between two of them combines an `application_id` of 0 read before that commit with
    // a `sqlite_master` read after it — and calls a database this program has just created somebody
    // else's. Without the transaction that happened on more than half of concurrent first starts.
    // What makes it safe is not that a late connection waits, since it may find the migration
    // committed and take its own lock without waiting at all, but that all three of its answers
    // are answers about the same moment.
    //
    // Ownership before the version gate, because whose file this is has to be settled before which
    // schema it is at. Settling it first keeps this program from asking for a write lock on a
    // stranger's database, where a write that outlasts the busy timeout would have `migrate` report
    // SQLITE_BUSY rather than name the owner. Neither is authoritative: `migrate` takes both again
    // inside its write transaction, on the view its own writes land on.
    {
        let snapshot = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
            .map_err(|source| StoreError::Open {
                path: path.to_path_buf(),
                source,
            })?;
        ownership(&snapshot, path)?;
        let current = read_user_version(&snapshot).map_err(|source| StoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        if !(0..=SCHEMA_VERSION).contains(&current) {
            return Err(StoreError::UnsupportedSchema {
                path: path.to_path_buf(),
                found: current,
                supported: SCHEMA_VERSION,
            });
        }
        // Nothing was written, so there is nothing to commit and the rollback on drop is the end
        // of it.
    }

    // Migrate first, switch second. Everything applied above is per-connection; the WAL switch is
    // written into the database header, where it would outlive a refusal. Ordering it after the
    // migration means the only thing that changes the file is reached through the write transaction
    // that decided the file is ours, and that decision cannot be overtaken: reading ownership and
    // then switching left a window in which another program could create its own database at this
    // path between the two, and a permanent journal mode change would already have landed on it by
    // the time `migrate` refused.
    migrate(&mut conn, path)?;
    enable_wal(&conn, path)?;
    Ok(conn)
}

fn migrate(conn: &mut rusqlite::Connection, path: &std::path::Path) -> Result<(), StoreError> {
    // The version is read INSIDE the write transaction. On first start several subsystems open
    // the database at the same moment, and a version read taken outside would let two of them
    // both see 0 and both try to create the tables. IMMEDIATE, not deferred: a deferred
    // transaction begins read-only, and its upgrade to a write returns SQLITE_BUSY at once
    // instead of waiting out the busy timeout.
    let transaction = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|source| StoreError::Migrate {
            path: path.to_path_buf(),
            source,
        })?;
    let ownership = ownership(&transaction, path)?;
    let current = read_user_version(&transaction).map_err(|source| StoreError::Migrate {
        path: path.to_path_buf(),
        source,
    })?;

    // The lower bound is not decoration: `current as usize` on a negative number is an index far
    // past the end of MIGRATIONS — -1 becomes `usize::MAX` — and the slice below panics on it. A
    // guard that only excluded values above SCHEMA_VERSION would leave the daemon a value that
    // crashes it.
    if !(0..=SCHEMA_VERSION).contains(&current) {
        return Err(StoreError::UnsupportedSchema {
            path: path.to_path_buf(),
            found: current,
            supported: SCHEMA_VERSION,
        });
    }

    // Claiming is not conditional on there being migrations to run: the two are separate facts, and
    // a file we have decided to take over has to come out of this transaction carrying our name.
    if ownership == Ownership::FreeToClaim {
        transaction
            .pragma_update(None, "application_id", APPLICATION_ID)
            .map_err(|source| StoreError::Migrate {
                path: path.to_path_buf(),
                source,
            })?;
    }

    let pending = &MIGRATIONS[current as usize..];
    if !pending.is_empty() {
        for migration in pending {
            transaction
                .execute_batch(migration)
                .map_err(|source| StoreError::Migrate {
                    path: path.to_path_buf(),
                    source,
                })?;
        }
        // Written only when something was applied, so an already-current database leaves this
        // transaction without writing. That is a statement about this transaction and not about
        // `open`: a run that committed a migration and stopped before the WAL switch leaves a
        // database whose next open is schema-current and still changes its journal mode.
        transaction
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|source| StoreError::Migrate {
                path: path.to_path_buf(),
                source,
            })?;
    }

    transaction.commit().map_err(|source| StoreError::Migrate {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::{APPLICATION_ID, CONNECTION_SETTINGS, SCHEMA_VERSION, enable_wal, open};
    use crate::StoreError;
    use tempfile::tempdir;

    #[test]
    fn fresh_db_initializes_and_is_idempotent() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("data").join("db.sqlite3");
        assert!(!path.exists());

        let conn = open(&path).expect("the fresh database should initialize");
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("the initialized schema version should be readable");
        assert_eq!(version, 1);

        drop(conn);
        let conn = open(&path).expect("the initialized database should reopen");
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("the reopened schema version should be readable");
        assert_eq!(version, 1);
    }

    #[test]
    fn open_applies_the_sqlite_contract() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = open(&path).expect("the database should open with the SQLite contract");

        // The contract between subsystems that each hold their own connection.
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("the journal mode should be readable");
        let busy_timeout: i32 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .expect("the busy timeout should be readable");
        let foreign_keys: i32 = conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("the foreign key setting should be readable");

        assert_eq!(journal_mode, "wal");
        assert_eq!(busy_timeout, 5000);
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn a_first_start_where_every_subsystem_opens_at_once_succeeds() {
        // Every subsystem holds its own connection, so a first start is several opens at the same
        // moment against a database still in rollback-journal mode. They all meet on the WAL
        // conversion, which needs an exclusive lock and does not go through the busy timeout, so
        // without the wait `enable_wal` does on its own behalf most such starts fail.
        const CONNECTIONS: usize = 8;
        // Two races live here and each spacing favours one of them. Starting together, the
        // connections collide on the WAL conversion. Starting about a millisecond apart, a later
        // one runs its ownership reads while an earlier one is committing the migration. Which one
        // a given run hits is not something this test can tell, which is why it uses more than one
        // spacing; and neither is certain in one run, so a green pass is evidence and not proof.
        const SPACINGS: [std::time::Duration; 4] = [
            std::time::Duration::ZERO,
            std::time::Duration::from_micros(500),
            std::time::Duration::from_micros(900),
            std::time::Duration::from_micros(1500),
        ];

        let dir = tempdir().expect("the temporary database directory should be creatable");
        for (round, spacing) in SPACINGS.iter().enumerate() {
            let path = dir.path().join(format!("{round}.sqlite3"));
            let ready = std::sync::Arc::new(std::sync::Barrier::new(CONNECTIONS));
            let openers = (0..CONNECTIONS)
                .map(|index| {
                    let path = path.clone();
                    let ready = std::sync::Arc::clone(&ready);
                    let spacing = *spacing;
                    std::thread::spawn(move || {
                        ready.wait();
                        // The barrier is where the spacing is measured from; without it the threads
                        // would start whenever the runtime got round to them and the spacing would mean
                        // nothing.
                        std::thread::sleep(spacing * index as u32);
                        let conn = open(&path)?;
                        conn.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
                            .map_err(|source| StoreError::Open { path, source })
                    })
                })
                .collect::<Vec<_>>();

            for opener in openers {
                let mode = opener
                    .join()
                    .expect("an opening thread should not panic")
                    .expect("every simultaneous open should succeed");
                assert_eq!(
                    mode, "wal",
                    "every connection has to come back in WAL, not only the one that converted the file"
                );
            }
        }
    }

    #[test]
    fn the_wal_switch_outwaits_a_lock_released_within_its_deadline() {
        // The manufactured counterpart of the test above: that one needs the scheduler to
        // produce a collision, this one arranges its own. The blocker holds a write
        // transaction, and against a held write lock the conversion comes back busy at once
        // instead of waiting out the busy timeout — measured, a switch stripped of its
        // retries still gets past a read transaction, and fails on the spot against this.
        // The lock is taken before the switching thread exists and released 300ms after that
        // thread reports its connection ready, far inside WAL_SWITCH_DEADLINE — so all that
        // is left outside the arrangement is the step from the report to the first attempt,
        // and a switch that does not retry passes only if that one step outlasts the hold.
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let blocker =
            rusqlite::Connection::open(&path).expect("the blocking connection should open");
        blocker
            .execute("CREATE TABLE t (x INTEGER)", [])
            .expect("the rollback-journal database should be creatable");
        blocker
            .execute_batch("BEGIN IMMEDIATE")
            .expect("the blocking transaction should begin");

        let (ready, started) = std::sync::mpsc::channel();
        let switcher = std::thread::spawn({
            let path = path.clone();
            move || {
                let conn = rusqlite::Connection::open(&path)
                    .expect("the switching connection should open");
                conn.execute_batch(CONNECTION_SETTINGS)
                    .expect("the switching connection settings should apply");
                ready
                    .send(())
                    .expect("the main thread should be waiting for the report");
                enable_wal(&conn, &path)
            }
        });

        started
            .recv()
            .expect("the switching thread should report its connection ready");
        std::thread::sleep(std::time::Duration::from_millis(300));
        blocker
            .execute_batch("COMMIT")
            .expect("the blocking transaction should release its lock");

        switcher
            .join()
            .expect("the switching thread should not panic")
            .expect("the WAL switch should outwait a lock released within its deadline");
    }

    #[test]
    fn initial_migration_creates_every_table_and_index() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = open(&path).expect("the fresh database should initialize");
        let mut statement = conn
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type IN ('table','index') \
                 AND name NOT LIKE 'sqlite_%' \
                 ORDER BY name",
            )
            .expect("the schema object query should prepare");
        let names = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("the schema object query should execute")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("the schema object names should be readable");
        let names = names.iter().map(String::as_str).collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "control_events",
                "control_state",
                "episodes",
                "idx_obs_time",
                "images",
                "observations",
                "outbox",
            ],
            "this schema is the contract every later task is written against, so adding or \
             renaming a table has to be a deliberate edit here too"
        );
    }

    #[test]
    fn reopening_never_re_runs_a_migration() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = open(&path).expect("the fresh database should initialize");
        conn.execute(
            "INSERT INTO control_state (key, value) \
             VALUES ('pause_until', '2026-07-27T00:00:00Z')",
            [],
        )
        .expect("the test control state should be writable");

        drop(conn);
        let conn = open(&path).expect("the initialized database should reopen");
        let value: String = conn
            .query_row(
                "SELECT value FROM control_state WHERE key = 'pause_until'",
                [],
                |row| row.get(0),
            )
            .expect("the stored control state should remain readable");

        assert_eq!(
            value, "2026-07-27T00:00:00Z",
            "reopening an already-migrated database has to hand back what was stored. Measured \
             2026-07-30, a second run of 0001_init.sql fails with \"table observations already \
             exists\" and rolls back, so a broken version marker would make the reopen above panic \
             rather than reach this line — erasing this row would take a drop-and-recreate, which \
             that file deliberately does not use"
        );
    }

    #[test]
    fn a_schema_version_this_build_cannot_migrate_from_is_rejected() {
        // Above the range means a newer build has already written the file; below it would reach
        // the migration slice with an index cast from a negative number and panic there.
        // The database has to be ours for the version to be the thing that refuses it.
        for found in [SCHEMA_VERSION + 1, -1] {
            let dir = tempdir().expect("the temporary database directory should be creatable");
            let path = dir.path().join("db.sqlite3");
            let conn = rusqlite::Connection::open(&path)
                .expect("the schema-version test database should be openable");
            conn.pragma_update(None, "application_id", APPLICATION_ID)
                .expect("the ContextWitness application id should be writable");
            conn.pragma_update(None, "user_version", found)
                .expect("the unsupported schema version should be writable");
            drop(conn);

            match open(&path) {
                Err(StoreError::UnsupportedSchema { found: actual, .. }) => {
                    assert_eq!(actual, found)
                }
                Err(error) => panic!("expected UnsupportedSchema for {found}, got {error:?}"),
                Ok(_) => panic!("schema version {found} was accepted"),
            }
        }
    }

    #[test]
    fn an_unsupported_database_is_not_modified() {
        // The database has to be ours for the version to be the thing that refuses it.
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = rusqlite::Connection::open(&path)
            .expect("the unsupported-schema test database should be openable");
        conn.pragma_update(None, "application_id", APPLICATION_ID)
            .expect("the ContextWitness application id should be writable");
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("the unsupported schema version should be writable");
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("the starting journal mode should be readable");
        assert_eq!(
            journal_mode, "delete",
            "the test database should start in rollback-journal mode"
        );
        drop(conn);
        let before = std::fs::read(&path).expect("the test database should be readable");

        match open(&path) {
            Err(StoreError::UnsupportedSchema { .. }) => {}
            Err(error) => panic!("expected UnsupportedSchema, got {error:?}"),
            Ok(_) => panic!("the unsupported schema version was accepted"),
        }

        // Compared as bytes rather than by reading the three values back, because the name is about
        // the whole file: `open` applies its connection settings before it looks at the version, so
        // a settings line that turned out to write would change the file while all three of those
        // read back exactly as they were.
        let after = std::fs::read(&path).expect("the refused database should still be readable");
        assert_eq!(
            after, before,
            "refusing a file this build cannot handle must leave it as found, and a newer build's \
             version marker is the one thing that build needs intact"
        );
    }

    #[test]
    fn an_unclaimed_database_with_a_version_marker_is_refused_and_left_alone() {
        for found in [SCHEMA_VERSION, SCHEMA_VERSION + 1] {
            let dir = tempdir().expect("the temporary database directory should be creatable");
            let path = dir.path().join("db.sqlite3");
            let conn = rusqlite::Connection::open(&path)
                .expect("the version-marked database should be openable");
            conn.pragma_update(None, "user_version", found)
                .expect("the version marker should be writable");
            let journal_mode: String = conn
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .expect("the starting journal mode should be readable");
            assert_eq!(
                journal_mode, "delete",
                "the test database should start in rollback-journal mode"
            );
            drop(conn);
            let before = std::fs::read(&path).expect("the test database should be readable");

            match open(&path) {
                Err(StoreError::ForeignDatabase { found, .. }) => assert_eq!(found, 0),
                Err(error) => panic!("expected ForeignDatabase, got {error:?}"),
                Ok(_) => panic!("the unclaimed version-marked database was accepted"),
            }

            let after =
                std::fs::read(&path).expect("the refused database should still be readable");
            assert_eq!(
                after, before,
                "a marker that is not zero is one somebody wrote, so the file is not ours to \
                 take and refusing it must leave it exactly as found — SCHEMA_VERSION included, \
                 which is the value that looks most like ours"
            );
        }
    }

    #[test]
    fn a_database_that_cannot_use_wal_is_refused() {
        // The in-memory case stands in for the filesystem case, which has no seam: both refuse WAL
        // by returning the mode the database is actually in rather than by failing.
        match open(std::path::Path::new(":memory:")) {
            Err(StoreError::JournalMode { actual, .. }) => assert_eq!(actual, "memory"),
            Err(error) => panic!("expected JournalMode, got {error:?}"),
            Ok(_) => panic!("the database opened without WAL"),
        }
    }

    #[test]
    fn a_fresh_database_is_claimed() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = open(&path).expect("the fresh database should initialize");
        let application_id: i32 = conn
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .expect("the application id should be readable");

        // 0x43576974 spells "CWit" in ASCII.
        assert_eq!(application_id, 0x4357_6974);
    }

    #[test]
    fn another_applications_database_is_refused_and_left_alone() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = rusqlite::Connection::open(&path)
            .expect("the other application's database should be openable");
        conn.execute("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)", [])
            .expect("the other application's table should be creatable");
        conn.execute(
            "INSERT INTO notes (body) VALUES (?1)",
            ["someone else's data"],
        )
        .expect("the other application's row should be writable");
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("the starting journal mode should be readable");
        assert_eq!(
            journal_mode, "delete",
            "the test database should start in rollback-journal mode"
        );
        drop(conn);
        let before = std::fs::read(&path).expect("the test database should be readable");

        match open(&path) {
            Err(StoreError::ForeignDatabase { found, .. }) => assert_eq!(found, 0),
            Err(error) => panic!("expected ForeignDatabase, got {error:?}"),
            Ok(_) => panic!("the other application's database was accepted"),
        }

        let after = std::fs::read(&path).expect("the refused database should still be readable");
        assert_eq!(
            after, before,
            "refusing another program's database must leave the whole file as found — its data, \
             its journal mode, and the user_version that program would be using for its own \
             migrations"
        );
    }

    #[test]
    fn an_empty_database_claimed_by_another_application_is_refused() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = rusqlite::Connection::open(&path)
            .expect("the other application's database should be openable");
        conn.pragma_update(None, "application_id", 0x0000_0001)
            .expect("the other application's application id should be writable");
        drop(conn);
        let before = std::fs::read(&path).expect("the test database should be readable");

        // An empty file someone else has already put their name on is still theirs.
        match open(&path) {
            Err(StoreError::ForeignDatabase { found, .. }) => assert_eq!(found, 1),
            Err(error) => panic!("expected ForeignDatabase, got {error:?}"),
            Ok(_) => panic!("the other application's database was accepted"),
        }

        let after = std::fs::read(&path).expect("the refused database should still be readable");
        assert_eq!(
            after, before,
            "refusing an empty file another application claimed must leave the whole file as found"
        );
    }
}
