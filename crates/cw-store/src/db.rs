//! Owns how the database file is opened and how its schema is allowed to change.

use std::path::PathBuf;

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

/// Errors produced while opening or migrating the database.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Creating the directory that holds the database failed.
    #[error("failed to create database directory {path}: {source}")]
    Directory {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// Opening the database or applying its connection contract failed.
    #[error("failed to open database {path}: {source}")]
    Open {
        /// Database file path.
        path: PathBuf,
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },
    /// The database was not created by ContextWitness.
    #[error(
        "database {path} was not created by ContextWitness (application id {found}); point \
         storage.data_dir at a directory of its own"
    )]
    ForeignDatabase {
        /// Database file path.
        path: PathBuf,
        /// Application id found in the database header.
        found: i32,
    },
    /// The database could not be put into WAL mode.
    #[error(
        "database {path} is in {actual} mode, not WAL; the subsystems that each hold their own \
         connection cannot share a database without it"
    )]
    JournalMode {
        /// Database file path.
        path: PathBuf,
        /// Journal mode the database is actually in.
        actual: String,
    },
    /// Applying a database migration failed.
    #[error("failed to migrate database {path}: {source}")]
    Migrate {
        /// Database file path.
        path: PathBuf,
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },
    /// The database schema version is outside the range this build understands.
    #[error(
        "database {path} has schema version {found}; versions 0 through {supported} are understood"
    )]
    UnsupportedSchema {
        /// Database file path.
        path: PathBuf,
        /// Schema version found in the database.
        found: i32,
        /// Highest schema version this build understands.
        supported: i32,
    },
}

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
            if entries == 0 && read_user_version(conn, path)? == 0 {
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
fn read_user_version(
    conn: &rusqlite::Connection,
    path: &std::path::Path,
) -> Result<i32, StoreError> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| StoreError::Migrate {
            path: path.to_path_buf(),
            source,
        })
}

/// Put the database into WAL mode, failing when it does not take.
///
/// `PRAGMA journal_mode = WAL` reports a refusal by returning the mode the database is actually in
/// rather than by failing, so the returned row is the only evidence that the statement did what it
/// says.
fn enable_wal(conn: &rusqlite::Connection, path: &std::path::Path) -> Result<(), StoreError> {
    let actual = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|source| StoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    if actual.eq_ignore_ascii_case("wal") {
        Ok(())
    } else {
        Err(StoreError::JournalMode {
            path: path.to_path_buf(),
            actual,
        })
    }
}

/// Open the database at `path`, apply the connection contract and bring the schema up to
/// [`SCHEMA_VERSION`].
///
/// Every subsystem opens its own connection (design section 7), so every open migrates; there is
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

    // Before the version gate, because "whose file is this" has to be settled before "which
    // schema is it at" — and both before the WAL switch, which is the first thing that writes.
    ownership(&conn, path)?;

    // Refuse a file this build cannot handle BEFORE changing anything about it. Everything applied
    // above is per-connection, but the WAL switch below is written into the database header and
    // would outlive the refusal. This read is NOT the migration decision — that one is taken again
    // inside the write transaction, where it is safe from a concurrent first start. A newer build
    // could still move the file forward between this read and the switch; that window is accepted,
    // because the most it can cost is a journal mode change a newer build of this program would
    // have made anyway.
    // Neither of these two is the decision the schema rests on — both are taken again inside the
    // write transaction, on the view the writes actually land on.
    let current = read_user_version(&conn, path)?;
    if !(0..=SCHEMA_VERSION).contains(&current) {
        return Err(StoreError::UnsupportedSchema {
            path: path.to_path_buf(),
            found: current,
            supported: SCHEMA_VERSION,
        });
    }

    enable_wal(&conn, path)?;
    migrate(&mut conn, path)?;
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
    let current = read_user_version(&transaction, path)?;

    // The lower bound is not decoration: current as usize on a negative number skips every
    // migration and would hand back a database with no tables and no error.
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
        // Written only when something was applied, so an already-current database's open stays a
        // read.
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
    use super::{APPLICATION_ID, SCHEMA_VERSION, StoreError, open};
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

        // These three values are design section 7's contract between subsystems that each hold
        // their own connection.
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
            "a re-run of 0001_init.sql — which is what an IF NOT EXISTS or a drop-and-recreate \
             would allow — would erase what is already stored, and the version marker is the \
             only thing preventing it"
        );
    }

    #[test]
    fn a_schema_version_this_build_cannot_migrate_from_is_rejected() {
        // Above the range means a newer build has already written the file; below it would
        // otherwise skip every migration and return an empty database as though it had succeeded.
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

        match open(&path) {
            Err(StoreError::UnsupportedSchema { .. }) => {}
            Err(error) => panic!("expected UnsupportedSchema, got {error:?}"),
            Ok(_) => panic!("the unsupported schema version was accepted"),
        }

        let conn = rusqlite::Connection::open(&path)
            .expect("the refused database should still be openable");
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("the refused database journal mode should be readable");
        assert_eq!(
            journal_mode, "delete",
            "refusing a file this build cannot handle means leaving it exactly as it was found, \
             and the WAL switch is written into the database header"
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
            drop(conn);

            match open(&path) {
                Err(StoreError::ForeignDatabase { found, .. }) => assert_eq!(found, 0),
                Err(error) => panic!("expected ForeignDatabase, got {error:?}"),
                Ok(_) => panic!("the unclaimed version-marked database was accepted"),
            }

            let conn = rusqlite::Connection::open(&path)
                .expect("the refused database should still be openable");
            let journal_mode: String = conn
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .expect("the refused database journal mode should be readable");
            let user_version: i32 = conn
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .expect("the refused database version marker should be readable");
            let observations: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = 'observations'",
                    [],
                    |row| row.get(0),
                )
                .expect("the ContextWitness table count should be readable");

            assert_eq!(
                (journal_mode.as_str(), user_version, observations),
                ("delete", found, 0),
                "SCHEMA_VERSION is the case that used to be accepted and handed back with no \
                 tables in it, and a marker that is not zero is one somebody wrote, so the file \
                 is not ours to take"
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
        drop(conn);

        match open(&path) {
            Err(StoreError::ForeignDatabase { found, .. }) => assert_eq!(found, 0),
            Err(error) => panic!("expected ForeignDatabase, got {error:?}"),
            Ok(_) => panic!("the other application's database was accepted"),
        }

        let conn = rusqlite::Connection::open(&path)
            .expect("the refused database should still be openable");
        let body: String = conn
            .query_row("SELECT body FROM notes", [], |row| row.get(0))
            .expect("the other application's row should remain readable");
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("the refused database journal mode should be readable");
        let user_version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("the refused database schema version should be readable");
        let observations: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'observations'",
                [],
                |row| row.get(0),
            )
            .expect("the ContextWitness table count should be readable");

        assert_eq!(
            (
                body.as_str(),
                journal_mode.as_str(),
                user_version,
                observations
            ),
            ("someone else's data", "delete", 0, 0),
            "a database another program owns must come back exactly as it was found, and \
             user_version in particular is the field that program would be using for its own \
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

        // An empty file someone else has already put their name on is still theirs.
        match open(&path) {
            Err(StoreError::ForeignDatabase { found, .. }) => assert_eq!(found, 1),
            Err(error) => panic!("expected ForeignDatabase, got {error:?}"),
            Ok(_) => panic!("the other application's database was accepted"),
        }
    }
}
