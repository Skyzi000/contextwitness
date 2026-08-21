//! Owns how the database file is opened and how its schema is allowed to change.

use crate::StoreError;

/// Every migration, in order. A script's index plus one is the schema version it produces, so
/// [`SCHEMA_VERSION`] cannot drift away from the list.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_outbox_last_note.sql"),
];

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
/// constant on purpose: the conversion meets its contention while taking the RESERVED lock, a
/// transition the busy handler is never invoked for, so that waiting is ours to do.
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

/// Refuse a database whose marks say another program is using it.
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
/// Converting a rollback-journal database to WAL runs in a write transaction, and the contention
/// is met while its RESERVED lock is taken — a transition the pager never invokes the busy
/// handler for (its own table: taking SHARED from nothing waits, RESERVED from SHARED does not),
/// and the btree retry above the pager is bypassed with the conversion's read transaction open.
/// So contention comes back `SQLITE_BUSY` at once instead of waiting out `PRAGMA busy_timeout`.
/// Every subsystem opens its own connection, so a first start is several of them meeting
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
        // `Connection::open` on a missing directory fails with SQLITE_CANTOPEN, which names
        // nothing the user can act on.
        std::fs::create_dir_all(directory).map_err(|source| StoreError::Directory {
            path: directory.to_path_buf(),
            source,
        })?;
    }

    let mut conn = rusqlite::Connection::open(path).map_err(|source| StoreError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    // A clean close of a WAL database's last connection folds the log into the main file and
    // deletes it, so the checkpoint is held off until the database is accepted as ours.
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )
    .map_err(|source| StoreError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    conn.execute_batch(CONNECTION_SETTINGS)
        .map_err(|source| StoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;

    // One snapshot, not three reads: outside a transaction, another connection committing its
    // migration between two of `ownership`'s questions makes a database this program just created
    // look like somebody else's. Ownership before the version gate, so no write lock is ever asked
    // for on a stranger's database.
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
    }

    // Migrate first, switch second: the WAL switch is written into the database header, where it
    // would outlive a refusal. Switching before `migrate` decides the file is ours leaves a window
    // for another program to create its own database at this path and take the change.
    migrate(&mut conn, path)?;
    enable_wal(&conn, path)?;
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        false,
    )
    .map_err(|source| StoreError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(conn)
}

fn migrate(conn: &mut rusqlite::Connection, path: &std::path::Path) -> Result<(), StoreError> {
    // The version is read INSIDE the write transaction: a read taken outside would let two
    // simultaneous first starts both see 0 and both create the tables. IMMEDIATE, not deferred — a
    // deferred transaction's upgrade to a write returns SQLITE_BUSY instead of waiting out the
    // busy timeout.
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

    // The lower bound is not decoration: `current as usize` turns -1 into `usize::MAX`, and the
    // slice below panics on it.
    if !(0..=SCHEMA_VERSION).contains(&current) {
        return Err(StoreError::UnsupportedSchema {
            path: path.to_path_buf(),
            found: current,
            supported: SCHEMA_VERSION,
        });
    }

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
        assert_eq!(version, SCHEMA_VERSION);

        drop(conn);
        let conn = open(&path).expect("the initialized database should reopen");
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("the reopened schema version should be readable");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// The version-1 schema as it shipped, frozen: building the fixture from `MIGRATIONS[0]`
    /// would track any (forbidden) edit to the applied migration and hide exactly the
    /// divergence this test exists to catch. Verbatim, comments included — `sqlite_master`
    /// stores the CREATE text as written, and the equality check below compares that text.
    const VERSION_ONE_SCHEMA: &str = r#"
CREATE TABLE observations (
  id TEXT PRIMARY KEY,              -- ULID
  source TEXT NOT NULL,
  observed_at TEXT NOT NULL,        -- RFC 3339, UTC
  duration_ms INTEGER,
  schema_version INTEGER NOT NULL,  -- the payload's schema, not this file's
  payload TEXT NOT NULL             -- JSON
);

CREATE INDEX idx_obs_time ON observations(observed_at);

CREATE TABLE episodes (
  id TEXT PRIMARY KEY,              -- ULID
  source TEXT NOT NULL,
  start_at TEXT NOT NULL,
  end_at TEXT NOT NULL,
  document_id TEXT NOT NULL UNIQUE, -- the window's stable id; the UNIQUE is what stops a rescan
                                    -- from delivering the same window twice
  content TEXT NOT NULL,            -- the delivered body, snapshotted at close so a retry sends
                                    -- the same bytes rather than rebuilding them
  metadata_json TEXT NOT NULL,      -- the retain metadata, snapshotted for the same reason
  created_at TEXT NOT NULL
);

CREATE TABLE images (               -- the truth about which image files exist and how big they
                                    -- are; the observation's payload JSON is never rewritten
  observation_id TEXT PRIMARY KEY REFERENCES observations(id),
  relative_path TEXT NOT NULL UNIQUE,
  byte_size INTEGER NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE outbox (
  episode_id TEXT PRIMARY KEY REFERENCES episodes(id),
  state TEXT NOT NULL,              -- pending / delivering / delivered / failed
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at TEXT,
  last_error TEXT
);

CREATE TABLE control_events (       -- append-only audit trail: pause, resume, blacklist skips
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  at TEXT NOT NULL,
  detail TEXT
);

CREATE TABLE control_state (        -- current state, and the one that is authoritative;
                                    -- control_events is history
  key TEXT PRIMARY KEY,             -- 'pause_until' | 'pause_indefinite' | 'last_tick_at'
                                    -- | 'last_capture_at' | 'last_delivery_at'
  value TEXT
);
"#;

    fn schema_sql(conn: &rusqlite::Connection) -> Vec<String> {
        let mut statement = conn
            .prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
            .expect("the schema should be queryable");
        let rows = statement
            .query_map([], |row| row.get(0))
            .expect("the schema query should run");
        rows.collect::<Result<_, _>>()
            .expect("the schema rows should read")
    }

    #[test]
    fn an_existing_version_one_database_gains_the_outbox_note_column() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        {
            let conn = rusqlite::Connection::open(&path).expect("the raw database should open");
            conn.execute_batch(CONNECTION_SETTINGS)
                .expect("the connection settings should apply");
            conn.execute_batch(VERSION_ONE_SCHEMA)
                .expect("the frozen version-one schema should apply");
            conn.pragma_update(None, "application_id", APPLICATION_ID)
                .expect("the ownership mark should write");
            conn.pragma_update(None, "user_version", 1)
                .expect("the version should write");
        }

        let conn = open(&path).expect("the version-one database should migrate forward");
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("the migrated schema version should be readable");
        assert_eq!(version, SCHEMA_VERSION);
        conn.execute("UPDATE outbox SET last_note = NULL", [])
            .expect("the added column must exist on the migrated database");

        let fresh_dir = tempdir().expect("the temporary database directory should be creatable");
        let fresh =
            open(&fresh_dir.path().join("db.sqlite3")).expect("the fresh database should open");
        assert_eq!(
            schema_sql(&conn),
            schema_sql(&fresh),
            "a migrated version-one file must land on the schema a fresh database gets"
        );
    }

    #[test]
    fn open_applies_the_sqlite_contract() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = open(&path).expect("the database should open with the SQLite contract");

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

        // Only the journal mode persists in the file, so the contract has to come back on a reopen
        // of an existing database and not only on the open that created it.
        drop(conn);
        let conn = open(&path).expect("the existing database should reopen with the contract");
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("the reopened journal mode should be readable");
        let busy_timeout: i32 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .expect("the reopened busy timeout should be readable");
        let foreign_keys: i32 = conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("the reopened foreign key setting should be readable");
        assert_eq!(journal_mode, "wal");
        assert_eq!(busy_timeout, 5000);
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn a_first_start_where_every_subsystem_opens_at_once_succeeds() {
        const CONNECTIONS: usize = 8;
        // Each spacing favours a different race: starting together collides on the WAL conversion,
        // a millisecond apart runs one connection's ownership reads while another commits its
        // migration. Neither is certain in one run, so a green pass is evidence and not proof.
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
        // A held write lock, not a read lock: a switch stripped of its retries still gets past a
        // read transaction and fails on the spot against this one.
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

        // Compared as bytes: `open` applies its connection settings before it looks at the version,
        // and one that turned out to write would leave every value still reading back unchanged.
        let after = std::fs::read(&path).expect("the refused database should still be readable");
        assert_eq!(
            after, before,
            "refusing a file this build cannot handle must leave it as found, and a newer build's \
             version marker is the one thing that build needs intact"
        );
    }

    #[test]
    fn an_unsupported_wal_database_keeps_its_log() {
        // The fixture holds its log back the way `open` does, so the refusal has a log to leave.
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = rusqlite::Connection::open(&path)
            .expect("the WAL-refusal test database should be openable");
        conn.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )
        .expect("the fixture should hold its log back");
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .expect("the fixture should switch to WAL");
        assert_eq!(mode, "wal");
        conn.pragma_update(None, "application_id", APPLICATION_ID)
            .expect("the ContextWitness application id should be writable");
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("the unsupported schema version should be writable");
        drop(conn);
        let log = path.with_extension("sqlite3-wal");
        let main_before = std::fs::read(&path).expect("the test database should be readable");
        let log_before = std::fs::read(&log).expect("the fixture should have left a log");

        match open(&path) {
            Err(StoreError::UnsupportedSchema { .. }) => {}
            Err(error) => panic!("expected UnsupportedSchema, got {error:?}"),
            Ok(_) => panic!("the unsupported schema version was accepted"),
        }

        assert_eq!(
            std::fs::read(&path).expect("the refused database should still be readable"),
            main_before,
            "the main file must be left as found"
        );
        assert_eq!(
            std::fs::read(&log).expect("the refused database's log should still be there"),
            log_before,
            "the log must be left as found, not folded in and deleted"
        );
    }

    #[test]
    fn an_unclaimed_database_with_a_version_marker_is_refused_and_left_alone() {
        // -1 pins the refusal to "not zero" rather than "positive": past a weakened check it would
        // fall through to the version gate and be refused as an unsupported schema of ours.
        for found in [-1, SCHEMA_VERSION, SCHEMA_VERSION + 1] {
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
    fn a_foreign_database_with_a_hot_journal_is_recovered_before_refusal() {
        let dir = tempdir().expect("the temporary database directory should be creatable");
        let path = dir.path().join("db.sqlite3");
        let conn = rusqlite::Connection::open(&path)
            .expect("the other application's database should be openable");
        // A one-page cache spills pages to disk mid-transaction, so the copy below holds
        // uncommitted writes for the journal to take back.
        conn.execute_batch("PRAGMA cache_size = 1; CREATE TABLE notes (body BLOB)")
            .expect("the other application's table should be creatable");
        conn.execute("INSERT INTO notes VALUES (zeroblob(100000))", [])
            .expect("the other application's row should be writable");
        let committed = std::fs::read(&path).expect("the committed state should be readable");
        conn.execute_batch("BEGIN IMMEDIATE")
            .expect("the write transaction should begin");
        for _ in 0..50 {
            conn.execute("INSERT INTO notes VALUES (zeroblob(100000))", [])
                .expect("the uncommitted row should be writable");
        }
        let crash_dir = tempdir().expect("the crash-copy directory should be creatable");
        let crashed = crash_dir.path().join("db.sqlite3");
        let crashed_journal = crash_dir.path().join("db.sqlite3-journal");
        std::fs::copy(&path, &crashed).expect("the database should copy mid-transaction");
        std::fs::copy(dir.path().join("db.sqlite3-journal"), &crashed_journal)
            .expect("the journal should copy mid-transaction");
        drop(conn);
        let as_crashed = std::fs::read(&crashed).expect("the crashed copy should be readable");
        assert_ne!(
            as_crashed, committed,
            "the copy should hold uncommitted pages for recovery to take back"
        );

        match open(&crashed) {
            Err(StoreError::ForeignDatabase { found, .. }) => assert_eq!(found, 0),
            Err(error) => panic!("expected ForeignDatabase, got {error:?}"),
            Ok(_) => panic!("the crashed foreign database was accepted"),
        }

        let after = std::fs::read(&crashed).expect("the refused database should be readable");
        assert_eq!(
            after, committed,
            "recovery must land the file at its owner's last commit, not leave it as copied"
        );
        assert!(
            !crashed_journal.exists(),
            "recovery must remove the journal it played back"
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
