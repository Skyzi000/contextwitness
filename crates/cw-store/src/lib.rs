#![deny(unsafe_op_in_unsafe_fn)]
//! Persistent storage functionality for ContextWitness.

pub mod db;
pub mod observations;
mod timestamp;

use std::path::PathBuf;

/// Everything this crate can fail with.
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
    /// A statement against the database failed.
    #[error("database statement failed: {source}")]
    Sql {
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },
    /// A row and the value it stands for could not be converted into one another.
    #[error("observation {id} cannot be converted to or from its stored form: {source}")]
    Encoding {
        /// Primary key of the row, as it is spelled in the database.
        id: String,
        /// What made the conversion fail.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A duration handed in does not fit the column that has to store it.
    #[error(
        "observation {id} has duration_ms {duration_ms}, which does not fit SQLite's signed \
         64-bit INTEGER"
    )]
    DurationOutOfRange {
        /// Primary key of the observation that was not stored.
        id: String,
        /// The value that does not fit.
        duration_ms: u64,
    },
}
