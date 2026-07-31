#![deny(unsafe_op_in_unsafe_fn)]
//! Persistent storage functionality for ContextWitness.

pub mod control;
pub mod db;
pub mod images;
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
    /// A timestamp is not one this schema represents.
    #[error(
        "timestamp {at} is not one this schema stores: it keeps the nanosecond grid in the years \
         0000 through 9999, spelled with nine fractional digits and a trailing Z"
    )]
    TimestampOutOfRange {
        /// What was refused: the spelling, when this schema simply does not write one like it, or a
        /// description naming the field it was spelled from, when the spelling cannot stand for the
        /// value on its own. That happens for every overflowing nanosecond field, two ways — either
        /// the text is one an ordinary stored instant already owns, or it is a `:60` that no
        /// storable instant owns and that a window query loses even though the half-open contract
        /// places it inside the window.
        at: String,
    },
    /// Storing an observation failed in SQLite.
    #[error("failed to store observation {id}: {source}")]
    Insert {
        /// Primary key of the observation that was not stored.
        id: String,
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },
    /// An observation would not read back as the value it was given.
    #[error("observation {id} would not read back as the value it was given, so it was not stored")]
    NotFaithful {
        /// Primary key of the observation that was not stored.
        id: String,
    },
    /// A control row could not be read as the value it stands for.
    #[error("control row {subject} cannot be read: {source}")]
    Control {
        /// The `control_state` key, or the `control_events` id, the failure is about. A failure
        /// that is about more than one row names all of them.
        subject: String,
        /// What made the row unreadable.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Appending to the audit trail failed.
    #[error("failed to record control event {id}: {source}")]
    RecordEvent {
        /// Primary key of the event that was not recorded.
        id: String,
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },
    /// Encoding a frame to WebP failed.
    #[error("failed to encode the image for observation {id}")]
    Encode {
        /// Observation the frame belongs to.
        id: String,
    },
    /// Reading, writing or enumerating something under the image root failed.
    #[error("image path {path} could not be read, written or enumerated: {source}")]
    ImageIo {
        /// File or directory the operation was refused on.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// An image is already registered for an observation.
    #[error("observation {id} already has an image registered")]
    ImageAlreadyRegistered {
        /// Observation whose image was to be written.
        id: String,
    },
}
