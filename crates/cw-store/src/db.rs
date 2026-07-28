//! Owns how the database file is opened and how its schema is allowed to change.

use std::path::PathBuf;

/// Every migration, in order. A script's index plus one is the schema version it produces, so
/// [`SCHEMA_VERSION`] cannot drift away from the list.
const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

/// The schema version this build understands.
pub const SCHEMA_VERSION: i32 = MIGRATIONS.len() as i32;

/// Applied to every connection, per design section 7. WAL and the busy timeout are the contract
/// between the subsystems that each hold their own connection; foreign keys are off by default in
/// SQLite and have to be asked for per connection.
/// The busy timeout comes first: it is the only one of the three that has to be in place before
/// another statement runs, because switching a brand-new file to WAL needs an exclusive lock and
/// would otherwise fail outright against a concurrent first open.
const CONNECTION_CONTRACT: &str = "\
    PRAGMA busy_timeout = 5000;\n\
    PRAGMA journal_mode = WAL;\n\
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
    conn.execute_batch(CONNECTION_CONTRACT)
        .map_err(|source| StoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
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
    let current: i32 = transaction
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| StoreError::Migrate {
            path: path.to_path_buf(),
            source,
        })?;

    // The lower bound is not decoration: current as usize on a negative number skips every
    // migration and would hand back a database with no tables and no error.
    if !(0..=SCHEMA_VERSION).contains(&current) {
        return Err(StoreError::UnsupportedSchema {
            path: path.to_path_buf(),
            found: current,
            supported: SCHEMA_VERSION,
        });
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
    use super::{SCHEMA_VERSION, StoreError, open};
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
        for found in [SCHEMA_VERSION + 1, -1] {
            let dir = tempdir().expect("the temporary database directory should be creatable");
            let path = dir.path().join("db.sqlite3");
            let conn = rusqlite::Connection::open(&path)
                .expect("the schema-version test database should be openable");
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
}
