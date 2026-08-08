use crate::{StoreError, timestamp};

const INSERT_EPISODE: &str = "INSERT INTO episodes \
     (id, source, start_at, end_at, document_id, content, metadata_json, created_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
const INSERT_OUTBOX: &str = "INSERT INTO outbox (episode_id, state, attempts, next_attempt_at) \
     VALUES (?1, 'pending', 0, ?2)";
const COUNT_BY_DOCUMENT_ID: &str = "SELECT count(*) FROM episodes WHERE document_id = ?1";
const SELECT_LATEST: &str =
    "SELECT id, end_at FROM episodes WHERE source = ?1 ORDER BY end_at DESC, id DESC LIMIT 1";

/// What [`insert_with_outbox`] found once it held the write lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// The window was stored and queued under this id, which this call assigned.
    Registered(ulid::Ulid),
    /// A row already carries this `document_id`: the window is queued or delivered already.
    AlreadyRegistered,
}

pub fn insert_with_outbox(
    conn: &mut rusqlite::Connection,
    id: ulid::Ulid,
    episode: &cw_core::episode::Episode,
    created_at: chrono::DateTime<chrono::Utc>,
) -> Result<Registration, StoreError> {
    let id_text = id.to_string();
    let start_at = timestamp::to_sql(episode.start_at)?;
    let end_at = timestamp::to_sql(episode.end_at)?;
    let created_at = timestamp::to_sql(created_at)?;
    // Snapshotted rather than rebuilt at delivery: a retry, or a delivery after the renderer
    // changed, sends the same bytes under the same document id.
    let metadata_json =
        serde_json::to_string(&episode.metadata).map_err(|source| StoreError::Encoding {
            id: id_text.clone(),
            source: Box::new(source),
        })?;

    let transaction = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|source| StoreError::Sql { source })?;

    if let Err(source) = transaction.execute(
        INSERT_EPISODE,
        rusqlite::params![
            id_text,
            episode.source,
            start_at,
            end_at,
            episode.document_id,
            episode.content,
            metadata_json,
            created_at,
        ],
    ) {
        let already_registered = matches!(
            transaction.query_one(
                COUNT_BY_DOCUMENT_ID,
                [episode.document_id.as_str()],
                |row| { row.get::<_, i64>(0) }
            ),
            Ok(1..)
        );
        if already_registered {
            return Ok(Registration::AlreadyRegistered);
        }
        return Err(StoreError::Sql { source });
    }

    transaction
        .execute(INSERT_OUTBOX, rusqlite::params![id_text, created_at])
        .map_err(|source| StoreError::Sql { source })?;

    transaction
        .commit()
        .map_err(|source| StoreError::Sql { source })?;

    Ok(Registration::Registered(id))
}

/// The end of the last window this source has an episode for — where a startup rescan picks up.
/// `None` when the source has none.
pub fn latest_end(
    conn: &rusqlite::Connection,
    source: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, StoreError> {
    // The ORDER BY compares text; it is time order only because every row spells its instant at one
    // width — `timestamp::to_sql`'s promise, not RFC 3339's.
    let mut statement = conn
        .prepare(SELECT_LATEST)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query([source])
        .map_err(|source| StoreError::Sql { source })?;
    let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? else {
        return Ok(None);
    };
    let id: String = row.get(0).map_err(|source| StoreError::Sql { source })?;
    let end_at: String = row.get(1).map_err(|source| StoreError::Sql { source })?;

    timestamp::from_sql(&end_at)
        .map(Some)
        .map_err(|source| StoreError::Encoding { id, source })
}
