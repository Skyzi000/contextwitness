#![deny(unsafe_op_in_unsafe_fn)]
//! Persistent storage functionality for ContextWitness.

pub mod control;
pub mod db;
pub mod episodes;
pub mod images;
pub mod observations;
pub mod outbox;
pub mod retention;
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
    /// The database does not carry ContextWitness's application id.
    #[error(
        "database {path} carries application id {found}, not ContextWitness's; point \
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
        "database {path} is in {actual} mode, not WAL; this program runs several connections \
         against it at once and requires WAL"
    )]
    JournalMode {
        /// Database file path.
        path: PathBuf,
        /// Journal mode the database is actually in.
        actual: String,
    },
    /// The migration transaction failed: taking its write lock, reading the schema version it
    /// decides by, writing either marker, applying a script, or committing. The ownership check
    /// runs inside the same transaction but answers as `Open` or `ForeignDatabase`, never this.
    /// A database already at the current schema version still takes that lock, so this can
    /// report contention with no script having run.
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
    /// Whatever `rusqlite` refused: a statement SQLite rejected, or a value it would not hand back
    /// in the type asked for. The second is why reading a row's id reports this rather than
    /// `Encoding`, which names a row by the id that read did not produce.
    #[error("database operation failed: {source}")]
    Sql {
        /// The error `rusqlite` produced.
        source: rusqlite::Error,
    },
    /// A value and its stored form could not be converted into one another: on the way in,
    /// before anything reaches the database, or on the way out, from a row that is already there.
    #[error("observation {id} cannot be converted to or from its stored form: {source}")]
    Encoding {
        /// Primary key as the failing side spells it: the spelling this store writes on the way
        /// in, the row's own on the way out.
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
        /// value on its own.
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
    #[error("failed to encode the image for observation {id}: {reason}")]
    Encode {
        /// Observation the frame belongs to.
        id: String,
        /// What refused: the input check that failed, or the encoder's own answer.
        reason: String,
    },
    /// An operation on the image root, or on a path being handled as one below it, failed. Which
    /// operation is not recorded anywhere on this variant: it is raised for creating, writing,
    /// syncing, renaming, reading, enumerating and removing, and for a listed file that turns out
    /// not to be under the root at all.
    /// `source` does not close the gap — it says what went wrong and not what was attempted, so a
    /// `PermissionDenied` from a removal and one from creating a directory are the same value. A
    /// caller that has to tell them apart needs a field this variant does not have.
    #[error("an operation on image path {path} failed: {source}")]
    ImageIo {
        /// What this call was about: an image file, or a directory on the way to one. Where a name
        /// was reserved rather than chosen — the temporary a publish writes into — this is the
        /// destination, which is the only name the caller can act on.
        path: PathBuf,
        /// Underlying error, spelled as I/O even when no filesystem call produced it: a listed
        /// path that escapes the image root is a spelling failure, wrapped as `InvalidData`.
        source: std::io::Error,
    },
    /// An image is already registered for an observation.
    #[error("observation {id} already has an image registered")]
    ImageAlreadyRegistered {
        /// Observation whose image was to be written.
        id: String,
    },
}
