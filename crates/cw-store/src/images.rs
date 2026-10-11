//! WebP image files and the database rows that record them.

use crate::control_cursor::{Cursor, head, load_cursor, store_cursor};
use crate::{StoreError, observations, timestamp};
use chrono::{Datelike, Timelike};
use std::collections::HashSet;

const INSERT_IMAGE: &str = "INSERT INTO images \
     (observation_id, relative_path, byte_size, created_at) VALUES (?1, ?2, ?3, ?4)";
const COUNT_IMAGE_BY_ID: &str = "SELECT count(*) FROM images WHERE observation_id = ?1";
const SELECT_PATH_BY_ID: &str =
    "SELECT observation_id, relative_path, created_at FROM images WHERE observation_id = ?1";
const DELETE_IMAGE: &str = "DELETE FROM images WHERE observation_id = ?1";
// `created_at` is compared as text, which is time order only for the spelling `timestamp::to_sql`
// writes.
const SELECT_SCAN_PAGE: &str = "SELECT observation_id, relative_path, created_at FROM images \
     WHERE (created_at, observation_id) > (?1, ?2) AND (created_at, observation_id) <= (?3, ?4) \
     ORDER BY created_at, observation_id LIMIT ?5";
const SELECT_SCAN_HIGH_WATER: &str = "SELECT created_at, observation_id FROM images \
     ORDER BY created_at DESC, observation_id DESC LIMIT 1";

/// The `control_state` key the row scan's cursor is stored under.
const SCANNER_CURSOR: &str = "scanner_cursor";
/// The `control_state` key the row scan's cycle mark is stored under.
const SCANNER_HIGH_WATER: &str = "scanner_high_water";

#[cfg(not(test))]
const BATCH: i64 = 1000;
#[cfg(test)]
const BATCH: i64 = 3;

#[cfg(not(test))]
const MAX_PAGES: u32 = 100;
#[cfg(test)]
const MAX_PAGES: u32 = 4;

/// How many candidates one sweep transaction judges and removes.
#[cfg(not(test))]
const SWEEP_BATCH: usize = 100;
#[cfg(test)]
const SWEEP_BATCH: usize = 2;

/// What [`delete`] found under the id it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The row is gone and the file it named was unlinked.
    Removed,
    /// The row is gone and the image data was not there: the name empty, or the entry a link whose
    /// target is gone.
    MissingFile,
    /// No row was registered under that id.
    NoRow,
}

/// What one invocation of [`scan_orphan_rows`] saw.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RowScan {
    pub examined: u64,
    pub missing: Vec<String>,
    pub undecodable: u64,
    pub finished: bool,
}

const PROCESS_CHARS: usize = 32;
const TITLE_CHARS: usize = 48;
const LABEL_CHARS: usize = PROCESS_CHARS + 1 + TITLE_CHARS;

/// Where an image for `id` taken at `at` is filed, relative to the image root.
///
/// Forward slashes on every platform. This string is a UNIQUE key that a sweep compares against
/// names read off the filesystem and that retention reads back later; if two callers spelled the
/// same file two ways, the constraint would let both exist and each would be invisible to the
/// other's lookup. Joining it onto a root with `Path::join` handles the separator when a real
/// path is needed.
pub fn relative_path(
    id: ulid::Ulid,
    at: chrono::DateTime<chrono::FixedOffset>,
    process: Option<&str>,
    title: Option<&str>,
) -> String {
    let process = process.filter(|process| !process.is_empty());
    let mut label = sanitize(process.unwrap_or("unknown"), PROCESS_CHARS);
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        label.push('_');
        label.push_str(&sanitize(title, TITLE_CHARS));
    }
    format!(
        "{:04}/{:02}/{:02}/{:02}{:02}{:02}_{label}_{id}.webp",
        at.year(),
        at.month(),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

pub(crate) fn legacy_path(id: ulid::Ulid, at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "{:04}/{:02}/{:02}/{id}.webp",
        at.year(),
        at.month(),
        at.day()
    )
}

fn is_forbidden(c: char) -> bool {
    c.is_ascii_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

fn sanitize(text: &str, max_chars: usize) -> String {
    text.chars()
        .take(max_chars)
        .map(|c| if is_forbidden(c) { '_' } else { c })
        .collect()
}

/// Encode `pixels` as WebP, register it, and put the file in place.
///
/// The file is written to a temporary name and flushed before a transaction registers it, renames
/// it into place, flushes the rename and commits. A crash before the commit leaves at most a file
/// [`sweep_orphan_files`] removes from where its walk reaches, once nothing refuses the removal;
/// a crash after it leaves nothing to clean.
///
/// The observation this image belongs to must already be committed, since the image row references
/// it. A caller that is writing both takes [`save_with_observation`], which commits the two rows
/// together.
///
/// `relative` is the file's name as [`relative_path`] spells it for `id`. A name [`delete`] would
/// refuse to read back is refused before anything is written.
#[allow(clippy::too_many_arguments)]
pub fn save(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    id: ulid::Ulid,
    relative: &str,
    pixels: &[u8],
    width: u32,
    height: u32,
    quality: f32,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    save_registering(
        conn,
        root,
        id,
        None,
        relative,
        pixels,
        width,
        height,
        quality,
        at,
        |_| Ok(()),
    )
}

/// [`save`], with `observation` inserted into the same transaction as the image row.
///
/// The two rows commit together or not at all. Committing the observation first leaves, on a crash
/// in between, a row whose payload names an image that will never be written: the orphan sweep
/// reconciles unregistered files and registered rows and has nothing to say about that path, so it
/// stays and every episode carrying that observation carries the dead name with it. A crash before
/// this commit leaves at most an unregistered file, which is what [`save`] leaves and what
/// [`sweep_orphan_files`] collects.
///
/// `within` writes in the same transaction, after both rows; an error from it commits nothing.
#[allow(clippy::too_many_arguments)]
pub fn save_with_observation(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    observation: &cw_core::model::Observation,
    relative: &str,
    pixels: &[u8],
    width: u32,
    height: u32,
    quality: f32,
    at: chrono::DateTime<chrono::Utc>,
    within: impl FnOnce(&rusqlite::Connection) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    save_registering(
        conn,
        root,
        observation.id,
        Some(observation),
        relative,
        pixels,
        width,
        height,
        quality,
        at,
        within,
    )
}

/// `id` is the observation the image is filed under, and is `observation.id` whenever an
/// observation is given: the file's name is written from it and the image row keys on it, so the
/// two disagreeing would register the picture against a row that is not the one being committed.
#[allow(clippy::too_many_arguments)]
fn save_registering(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    id: ulid::Ulid,
    observation: Option<&cw_core::model::Observation>,
    relative: &str,
    pixels: &[u8],
    width: u32,
    height: u32,
    quality: f32,
    at: chrono::DateTime<chrono::Utc>,
    within: impl FnOnce(&rusqlite::Connection) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let id_text = id.to_string();
    // 16,383 is the encoder's dimension limit, not one imposed by this program.
    if !(1..=16_383).contains(&width)
        || !(1..=16_383).contains(&height)
        || !(0.0..=100.0).contains(&quality)
    {
        return Err(StoreError::Encode {
            id: id_text,
            reason: format!(
                "width {width}, height {height}, quality {quality}; the encoder takes dimensions \
                 in 1..=16383 and quality in 0..=100"
            ),
        });
    }

    let expected_len = u64::from(width) * u64::from(height) * 3;
    let actual_len = u64::try_from(pixels.len()).map_err(|_| StoreError::Encode {
        id: id_text.clone(),
        reason: "the pixel buffer length does not fit u64".to_owned(),
    })?;
    if actual_len != expected_len {
        return Err(StoreError::Encode {
            id: id_text,
            reason: format!("{actual_len} bytes cannot be {width} x {height} RGB pixels"),
        });
    }

    let created_at = timestamp::to_sql(at)?;
    let relative = checked_path(id, at, relative)?;
    // `encode` would unwrap this error and panic on the capture path.
    let encoded = webp::Encoder::from_rgb(pixels, width, height)
        .encode_simple(false, quality)
        .map_err(|source| StoreError::Encode {
            id: id_text.clone(),
            reason: format!("{source:?}"),
        })?;
    let byte_size = i64::try_from(encoded.len()).map_err(|_| StoreError::Encode {
        id: id_text.clone(),
        reason: "the encoded size does not fit SQLite's INTEGER".to_owned(),
    })?;

    let destination = root.join(&relative);
    let parent = destination
        .parent()
        .expect("an image path with date directories always has a parent");
    std::fs::create_dir_all(parent).map_err(|source| StoreError::ImageIo {
        path: parent.to_path_buf(),
        source,
    })?;

    let (_temporary, mut file) = cw_core::atomic_file::create_undeletable_temporary_beside(
        &destination,
    )
    .map_err(|source| StoreError::ImageIo {
        path: destination.clone(),
        source,
    })?;
    let write_result =
        std::io::Write::write_all(&mut file, &encoded).and_then(|()| file.sync_all());
    if let Err(source) = write_result {
        discard_written_file(&file);
        drop(file);
        return Err(StoreError::ImageIo {
            path: destination,
            source,
        });
    }

    // The INSERT decides a conflict before anything is published, and `delete` takes the same
    // IMMEDIATE lock, so no two decisions about this observation's row can be made at once.
    let transaction = match conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
    {
        Ok(transaction) => transaction,
        Err(source) => {
            discard_written_file(&file);
            return Err(StoreError::Sql { source });
        }
    };

    if let Some(observation) = observation
        && let Err(error) = observations::insert(&transaction, observation)
    {
        discard_written_file(&file);
        return Err(error);
    }

    if let Err(source) = transaction.execute(
        INSERT_IMAGE,
        rusqlite::params![id_text, relative, byte_size, created_at],
    ) {
        let already_registered = matches!(
            transaction.query_one(COUNT_IMAGE_BY_ID, [id_text.as_str()], |row| row
                .get::<_, i64>(0)),
            Ok(1..)
        );
        discard_written_file(&file);
        if already_registered {
            return Err(StoreError::ImageAlreadyRegistered { id: id_text });
        }
        return Err(StoreError::Sql { source });
    }

    if let Err(error) = within(&transaction) {
        discard_written_file(&file);
        return Err(error);
    }

    match cw_core::atomic_file::rename_without_replacing(&file, &destination) {
        Ok(true) => {}
        Ok(false) => {
            discard_written_file(&file);
            // Not `ImageAlreadyRegistered`: the insert above already succeeded, so once this rolls
            // back the observation has no row and that error would point recovery the wrong way.
            return Err(StoreError::ImageIo {
                path: destination,
                source: std::io::Error::from(std::io::ErrorKind::AlreadyExists),
            });
        }
        Err(source) => {
            discard_written_file(&file);
            return Err(StoreError::ImageIo {
                path: destination,
                source,
            });
        }
    }

    // The rename is a metadata change on this handle, and Windows buffers those; closing the
    // handle does not push them.
    if let Err(source) = file.sync_all() {
        discard_written_file(&file);
        return Err(StoreError::ImageIo {
            path: destination,
            source,
        });
    }

    if let Err(source) = transaction.commit() {
        // The picture stays: SQLite does not promise that every failed commit rolled back, and
        // discarding it risks a registered row whose file is gone. Every other failure above can
        // discard — none of them reached the commit.
        return Err(StoreError::Sql { source });
    }
    drop(file);

    Ok(())
}

/// Remove an image and the row that registers it.
///
/// This is an explicit request for one image, so it removes the row even when the file has already
/// gone. The answer says which it was: the picture unlinked, the row removed for a picture that was
/// not there any more, or no such row to begin with. Nothing that runs unasked may remove such a
/// row: [`scan_orphan_rows`] reports a row whose file is gone and deletes nothing, and
/// [`sweep_orphan_files`] removes a file only under the rule its own
/// documentation gives, which states what that rule leaves out. Neither is allowed to decide a
/// picture is expendable. The row is removed rather than marked, and that is deliberate; what goes
/// on recording that there was a picture is the observation's payload, which keeps its path.
///
/// The entry is opened before the row is committed and the removal is addressed to that handle, so
/// a name that will not open — a directory, or a link to one — is reported with the row still in
/// place, and whatever is published into the name after the commit is never taken for the file the
/// row named.
///
/// The row is committed before the file is removed, so a failure in between leaves an unregistered
/// entry rather than a row whose file is gone. An unregistered file is collected by a later sweep
/// from where its walk reaches, once whatever refused the removal has cleared; an unregistered
/// link never is — the sweep
/// collects only files — so a link the removal could not take keeps the name until something
/// other than this store clears it. That is still the residue worth having: the failure this has
/// to survive is a full disk during retention, and a row kept for a file that is gone would go on
/// charging its `byte_size` against a budget that is already free.
///
/// The removal itself never frees the name early: a removal through a handle frees it only when
/// the last handle closes, and this call's handle closes before this call returns. A `save` for
/// the same observation whose rename reaches the name while it is still taken is refused rather
/// than replacing anything; one whose rename lands after it is freed finds it free and succeeds
/// the first time. What refuses the earlier rename is the name being taken and not this call
/// holding it: the handle leaves the entry renamable and removable by name, and anything free to
/// move the entry aside frees the name sooner. If the name is empty — a row whose file has
/// already gone — only the row is removed, so a save that follows this commit keeps the file it
/// renames into that name.
///
/// Empty day directories are left behind on purpose, because pruning one could race with [`save`]
/// between creating that directory and opening its temporary file, while a few hundred empty
/// entries a year cost nothing and only the orphan sweep walks them.
pub fn delete(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    id: ulid::Ulid,
) -> Result<DeleteOutcome, StoreError> {
    let transaction = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|source| StoreError::Sql { source })?;
    let relative = {
        let mut statement = transaction
            .prepare(SELECT_PATH_BY_ID)
            .map_err(|source| StoreError::Sql { source })?;
        let mut rows = statement
            .query([id.to_string()])
            .map_err(|source| StoreError::Sql { source })?;
        rows.next()
            .map_err(|source| StoreError::Sql { source })?
            .map(image_path_from_row)
            .transpose()?
    };
    let Some(relative) = relative else {
        transaction
            .commit()
            .map_err(|source| StoreError::Sql { source })?;
        return Ok(DeleteOutcome::NoRow);
    };
    let path = root.join(relative);
    // Diagnosis only, and it decides nothing below.
    let data_gone =
        std::fs::metadata(&path).is_err_and(|source| source.kind() == std::io::ErrorKind::NotFound);
    let held = match cw_core::atomic_file::open_entry_for_removal(&path) {
        Ok(file) => Some(file),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(StoreError::ImageIo { path, source }),
    };

    transaction
        .execute(DELETE_IMAGE, [id.to_string()])
        .map_err(|source| StoreError::Sql { source })?;
    transaction
        .commit()
        .map_err(|source| StoreError::Sql { source })?;

    let Some(file) = held else {
        return Ok(DeleteOutcome::MissingFile);
    };
    cw_core::atomic_file::delete_by_handle(&file)
        .map_err(|source| StoreError::ImageIo { path, source })?;

    Ok(if data_gone {
        DeleteOutcome::MissingFile
    } else {
        DeleteOutcome::Removed
    })
}

/// Collect image files no row registers. Reports how many removals the filesystem accepted.
///
/// A file is removed unless a row spells its exact relative path, or it does not resolve to the
/// name it was enumerated under, or the filesystem refuses. Manual symlinks, renames and database
/// surgery inside the image tree are unsupported: the sweep may collect what such arrangements
/// leave reachable, and [`scan_orphan_rows`] reports what they broke as missing rows.
///
/// The walk beneath the root enters real directories and takes real files only, and it runs outside
/// the write lock. Each batch of candidates is judged and removed under one transaction holding
/// that lock — the same lock [`save`] holds across its rename and its commit, so a file renamed
/// into place is registered by the time its batch reads the rows, or the save that renamed it never
/// committed. No hold spans more than one batch's judgement and unlinks — a hundred candidates —
/// and none spans the walk; SQLite promises no order among waiters, so how long a contending save
/// waits is not bounded here.
pub fn sweep_orphan_files(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
) -> Result<usize, StoreError> {
    let root = match std::fs::canonicalize(root) {
        Ok(resolved) => resolved,
        // `canonicalize` answers `NotFound` for a name that was never there, for a link whose
        // target is gone, and for a path through either, so what decides is the deepest entry that
        // does exist: none at all, or a directory.
        Err(source) => {
            let unreadable = StoreError::ImageIo {
                path: root.to_path_buf(),
                source,
            };
            for name in root.ancestors() {
                match std::fs::symlink_metadata(name) {
                    Err(absent) if absent.kind() == std::io::ErrorKind::NotFound => {}
                    // A component Windows will not take — one holding a `|` — answers
                    // `InvalidFilename` while the directory above it is perfectly ordinary, which
                    // is not the same as nothing being there.
                    Err(_) => return Err(unreadable),
                    // `metadata`, which follows links, because a data directory junctioned onto
                    // another volume is a directory to everything else here.
                    Ok(_) => {
                        return if name != root
                            && std::fs::metadata(name).is_ok_and(|entry| entry.is_dir())
                        {
                            Ok(0)
                        } else {
                            Err(unreadable)
                        };
                    }
                }
            }
            // Nothing on the whole path is there. A root naming its own anchor carries it among
            // these names and `create_dir_all` makes a tail but never an anchor; a root naming none
            // is held by the process's directory, never among them. `is_absolute` is the wrong
            // question: `X:images` is anchored to drive X exactly as `X:\images` is, yet only the
            // second is absolute to Rust while both lead with a `Prefix`.
            return if matches!(
                root.components().next(),
                Some(std::path::Component::Prefix(_))
            ) {
                Err(unreadable)
            } else {
                Ok(0)
            };
        }
    };
    let mut removed = 0;
    let mut batch: Batch = Vec::with_capacity(SWEEP_BATCH);
    for path in Walk::open(&root)? {
        let path = path?;
        batch.push((path_relative_to_root(&root, &path)?, path));
        if batch.len() == SWEEP_BATCH {
            removed += sweep_batch(conn, &batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        removed += sweep_batch(conn, &batch)?;
    }

    Ok(removed)
}

/// Candidates paired with the relative path they were enumerated under.
type Batch = Vec<(String, std::path::PathBuf)>;

/// The paths must have been enumerated by walking `root` in the spelling `canonicalize` answers: a
/// candidate is required to resolve to the name it was enumerated under, and a root spelled any
/// other way makes every name fail that on its first component. Resolving the root here would
/// resolve it after the walk, which is the window that separation exists to close.
///
/// Every read this batch makes precedes its first unlink, so a SQL failure before the unlinks
/// leaves the batch with nothing removed. The transaction writes nothing, so a commit that fails
/// after them loses nothing on the database side.
fn sweep_batch(conn: &mut rusqlite::Connection, batch: &Batch) -> Result<usize, StoreError> {
    let transaction = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|source| StoreError::Sql { source })?;
    let registered = registered_in_batch(&transaction, batch)?;
    let mut removed = 0;

    for (relative, path) in batch {
        if registered.contains(relative) {
            continue;
        }
        // Kept without a diagnostic: a live save holds its temporary undeletable, so every pass
        // that races one comes through here.
        let Ok(file) = cw_core::atomic_file::open_for_removal(path) else {
            continue;
        };
        let Ok(identity) = cw_core::atomic_file::final_path_by_handle(&file) else {
            continue;
        };
        // What stands under a name can be replaced between resolving it and acting on it, and
        // Windows follows a reparse point met partway along a path, so the question put here is
        // what the open file calls itself. Every enumerated name is under the root by construction.
        if identity.as_path() != path.as_path() {
            continue;
        }

        // Addressed to the handle opened above, so no name is resolved between the last check and
        // the removal. A candidate that will not go is kept: a read-only file opens for removal and
        // then answers `PermissionDenied`, and one such file must not stop the pass.
        if cw_core::atomic_file::delete_by_handle(&file).is_ok() {
            removed += 1;
        }
    }

    transaction
        .commit()
        .map_err(|source| StoreError::Sql { source })?;
    Ok(removed)
}

/// The relative paths of this batch that a row spells exactly. `relative_path` is TEXT under the
/// default collation, so the comparison is byte for byte.
fn registered_in_batch(
    conn: &rusqlite::Connection,
    batch: &Batch,
) -> Result<HashSet<String>, StoreError> {
    let placeholders = std::iter::repeat_n("?", batch.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut statement = conn
        .prepare(&format!(
            "SELECT relative_path FROM images WHERE relative_path IN ({placeholders})"
        ))
        .map_err(|source| StoreError::Sql { source })?;
    let rows = statement
        .query_map(
            rusqlite::params_from_iter(batch.iter().map(|(relative, _)| relative)),
            |row| row.get(0),
        )
        .map_err(|source| StoreError::Sql { source })?;
    rows.collect::<rusqlite::Result<HashSet<String>>>()
        .map_err(|source| StoreError::Sql { source })
}

/// Report rows whose file is gone, a bounded stretch of the table per invocation. Nothing is
/// deleted, and nothing is written but the two keys that carry the cycle.
///
/// A cycle starts by marking the newest key in the table and walks the index up to that mark; a row
/// saved after the mark is left for the next cycle, and the mark lives in `control_state` beside the
/// cursor, so a restart resumes under the same one. An invocation that runs out of pages answers
/// `finished: false` and leaves the cursor where it stood; the one that drains the range returns
/// both keys to the head, so the next cycle takes a fresh mark and re-offers every row — including
/// the ones this cycle could not read back.
pub fn scan_orphan_rows(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
) -> Result<RowScan, StoreError> {
    let mut scan = RowScan::default();
    let (mut cursor, mut high_water) = load_keys(conn)?;
    if high_water == head() {
        let Some(mark) = newest_key(conn)? else {
            scan.finished = true;
            return Ok(scan);
        };
        high_water = mark;
    }

    let mut finished = false;
    for _ in 0..MAX_PAGES {
        let batch = scan_page(
            conn,
            rusqlite::params![cursor.0, cursor.1, high_water.0, high_water.1, BATCH],
        )?;
        if batch.is_empty() {
            finished = true;
            break;
        }
        for (created_at, id, relative) in batch {
            cursor = (created_at, id);
            scan.examined += 1;
            let relative = match relative {
                Ok(relative) => relative,
                // Stepped over, not propagated: a row no reader can spell would otherwise hold up
                // every row behind it, on this cycle and on every one after it.
                Err(StoreError::Encoding { .. } | StoreError::ImageIo { .. }) => {
                    scan.undecodable += 1;
                    continue;
                }
                Err(other) => return Err(other),
            };
            match std::fs::metadata(root.join(&relative)) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => scan.missing.push(relative),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    scan.missing.push(relative);
                }
                Err(source) => {
                    return Err(StoreError::ImageIo {
                        path: root.join(relative),
                        source,
                    });
                }
            }
        }
    }
    // The last permitted page can reach the mark without a further page running to find nothing
    // above it.
    if !finished {
        finished = cursor == high_water;
    }
    if finished {
        store_keys(conn, &head(), &head())?;
    } else {
        store_keys(conn, &cursor, &high_water)?;
    }
    scan.finished = finished;

    Ok(scan)
}

/// One page of `(created_at, observation_id, the path the row validates to)`.
type ScanPage = Vec<(String, String, Result<String, StoreError>)>;

fn scan_page(
    conn: &rusqlite::Connection,
    params: impl rusqlite::Params,
) -> Result<ScanPage, StoreError> {
    let mut statement = conn
        .prepare(SELECT_SCAN_PAGE)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query(params)
        .map_err(|source| StoreError::Sql { source })?;
    let mut batch = Vec::new();

    while let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? {
        let id: String = row.get(0).map_err(|source| StoreError::Sql { source })?;
        let created_at: String = row.get(2).map_err(|source| StoreError::Sql { source })?;
        batch.push((created_at, id, image_path_from_row(row)));
    }

    Ok(batch)
}

fn newest_key(conn: &rusqlite::Connection) -> Result<Option<Cursor>, StoreError> {
    let mut statement = conn
        .prepare(SELECT_SCAN_HIGH_WATER)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query([])
        .map_err(|source| StoreError::Sql { source })?;
    let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? else {
        return Ok(None);
    };
    let key = (
        row.get(0).map_err(|source| StoreError::Sql { source })?,
        row.get(1).map_err(|source| StoreError::Sql { source })?,
    );
    Ok(Some(key))
}

/// The two keys are only ever written together, so a cursor without its mark — or a mark without
/// its cursor — is a pair no invocation stored, and the cycle it belonged to cannot be resumed
/// under a bound this reader can trust. Both are read as the head, which starts a fresh cycle.
fn load_keys(conn: &rusqlite::Connection) -> Result<(Cursor, Cursor), StoreError> {
    let cursor = load_cursor(conn, SCANNER_CURSOR)?;
    let high_water = load_cursor(conn, SCANNER_HIGH_WATER)?;
    if (cursor == head()) != (high_water == head()) {
        return Ok((head(), head()));
    }
    Ok((cursor, high_water))
}

/// In one transaction: an invocation that stored its cursor without its mark, or the other way
/// round, would leave the pair `load_keys` refuses.
fn store_keys(
    conn: &mut rusqlite::Connection,
    cursor: &Cursor,
    high_water: &Cursor,
) -> Result<(), StoreError> {
    let transaction = conn
        .transaction()
        .map_err(|source| StoreError::Sql { source })?;
    store_cursor(&transaction, SCANNER_CURSOR, cursor)?;
    store_cursor(&transaction, SCANNER_HIGH_WATER, high_water)?;
    transaction
        .commit()
        .map_err(|source| StoreError::Sql { source })
}

/// Discard the file `save` wrote, whichever name it answers to now.
///
/// A discard that will not go is not reported. Every caller is already holding the error that says
/// why the save did not happen, and answering with this one instead would leave the caller with no
/// account of what it asked about. What a failed discard leaves behind is an unregistered file,
/// which the orphan sweep collects from where its walk reaches, once nothing refuses the removal.
fn discard_written_file(file: &std::fs::File) {
    let _ = cw_core::atomic_file::delete_by_handle(file);
}

/// A depth-first walk that holds one open `ReadDir` per level and nothing else. The whole listing
/// is not kept, because this reads whatever is under the image root rather than only what this
/// program wrote there, so neither its size nor its depth is this program's to assume; recursion
/// would put those same `ReadDir`s — on Windows each holds a `WIN32_FIND_DATAW` by value — on the
/// call stack, and running out of that aborts the process.
///
/// `root` arrives in the spelling `canonicalize` answered and the yielded paths inherit it, which
/// is what the sweep's identity check is written against. Anything that fails below the root is
/// passed over, and so is any single entry that cannot be answered about, wherever it sits: what
/// was never yielded cannot become a candidate, so nothing is removed on a guess. Two failures at
/// the root are reported instead, since either leaves the whole tree unseen — its listing not
/// opening for any reason other than absence, and its listing stopping partway. Absence is not one
/// of them, because a root that is not there has nothing under it to sweep.
struct Walk {
    root: std::path::PathBuf,
    stack: Vec<std::fs::ReadDir>,
}

impl Walk {
    fn open(root: &std::path::Path) -> Result<Self, StoreError> {
        let mut stack = Vec::new();
        match std::fs::read_dir(root) {
            Ok(entries) => stack.push(entries),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StoreError::ImageIo {
                    path: root.to_path_buf(),
                    source,
                });
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            stack,
        })
    }
}

impl Iterator for Walk {
    type Item = Result<std::path::PathBuf, StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let at_root = self.stack.len() == 1;
            let Some(entry) = self.stack.last_mut()?.next() else {
                self.stack.pop();
                continue;
            };
            let entry = match entry {
                Ok(entry) => entry,
                // The level is abandoned rather than the entry skipped: `ReadDir` promises nothing
                // about what follows an error, and one that keeps answering with it would never let
                // this walk end.
                Err(source) => {
                    self.stack.pop();
                    if at_root {
                        return Some(Err(StoreError::ImageIo {
                            path: self.root.clone(),
                            source,
                        }));
                    }
                    continue;
                }
            };
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&path) {
                    self.stack.push(entries);
                }
            } else if file_type.is_file() {
                return Some(Ok(path));
            }
        }
    }
}

fn path_relative_to_root(
    root: &std::path::Path,
    path: &std::path::Path,
) -> Result<String, StoreError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|source| StoreError::ImageIo {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
        })?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

/// The path a row claims, checked against the names this program has written for it:
/// `legacy_path` of its `created_at`, or a `relative_path` name ending in its id.
///
/// `relative_path` is TEXT and the schema constrains nothing, so a row edited by hand or damaged
/// can name anything at all — including a path that climbs out of the image root, which `delete`
/// would then remove.
fn checked_path(
    id: ulid::Ulid,
    created_at: chrono::DateTime<chrono::Utc>,
    stored: &str,
) -> Result<String, StoreError> {
    if stored == legacy_path(id, created_at) || is_labelled_path(id, stored) {
        Ok(stored.to_owned())
    } else {
        Err(StoreError::Encoding {
            id: id.to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the row names a path this program would not have written",
            )),
        })
    }
}

fn is_labelled_path(id: ulid::Ulid, stored: &str) -> bool {
    const SHAPE: &[u8] = b"0000/00/00/000000_";
    let Some(head) = stored.as_bytes().get(..SHAPE.len()) else {
        return false;
    };
    let head_fits = head.iter().zip(SHAPE).all(|(&byte, &shape)| {
        if shape == b'0' {
            byte.is_ascii_digit()
        } else {
            byte == shape
        }
    });
    if !head_fits {
        return false;
    }
    let Some(label) = stored[SHAPE.len()..].strip_suffix(&format!("_{id}.webp")) else {
        return false;
    };
    !label.is_empty() && label.chars().count() <= LABEL_CHARS && !label.contains(is_forbidden)
}

/// Read and validate the columns needed by image-row readers.
fn image_path_from_row(row: &rusqlite::Row<'_>) -> Result<String, StoreError> {
    let stored_id: String = row.get(0).map_err(|source| StoreError::Sql { source })?;
    let (id, relative, created_at) =
        decode_image_path(row).map_err(|source| StoreError::Encoding {
            id: stored_id,
            source,
        })?;
    checked_path(id, created_at, &relative)
}

fn decode_image_path(
    row: &rusqlite::Row<'_>,
) -> Result<
    (ulid::Ulid, String, chrono::DateTime<chrono::Utc>),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let stored_id: String = row.get(0)?;
    let id = ulid::Ulid::from_string(&stored_id)?;
    // `Ulid::from_string` accepts spellings this program never writes, and a row holding one then
    // matches nothing in `delete`, which looks a row up in the canonical spelling.
    if id.to_string() != stored_id {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the id is not spelled the way this program writes a ULID",
        )
        .into());
    }
    let relative: String = row.get(1)?;
    let created_at: String = row.get(2)?;
    let created_at = timestamp::from_sql(&created_at)?;
    Ok((id, relative, created_at))
}

#[cfg(test)]
mod tests {
    use super::{
        BATCH, Batch, DeleteOutcome, MAX_PAGES, RowScan, SCANNER_CURSOR, SCANNER_HIGH_WATER, Walk,
        checked_path, delete, legacy_path, relative_path, sanitize, save, save_with_observation,
        scan_orphan_rows, sweep_batch, sweep_orphan_files,
    };
    use crate::{StoreError, db, observations, timestamp};
    use chrono::{DateTime, FixedOffset, TimeDelta, TimeZone, Utc};
    use cw_core::model::{Observation, OcrStatus, ScreenPayload};
    use tempfile::{TempDir, tempdir};

    const WIDTH: u32 = 4;
    const HEIGHT: u32 = 3;
    const PER_PAGE: usize = BATCH as usize;
    /// More rows than one invocation of the scan may read.
    const REACH: usize = PER_PAGE * MAX_PAGES as usize;

    /// The candidates a sweep of `root` would judge, in the order it enumerates them.
    fn enumerate(root: &std::path::Path) -> Batch {
        Walk::open(root)
            .expect("the image tree should be walkable")
            .map(|path| {
                let path = path.expect("the image tree should be listable");
                let relative = super::path_relative_to_root(root, &path)
                    .expect("the candidate should be under the root");
                (relative, path)
            })
            .collect()
    }

    fn database() -> (TempDir, rusqlite::Connection, std::path::PathBuf) {
        let dir = tempdir().expect("the temporary image directory should be creatable");
        let conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the fresh database should initialize");
        let root = dir.path().join("images");
        (dir, conn, root)
    }

    /// A directory junction at `link` leading to `target`. A directory symlink reaches the same
    /// branches and needs Developer Mode or SeCreateSymbolicLinkPrivilege, which is why the tests
    /// built on one returned without asserting wherever that was absent. `mklink` is a `cmd`
    /// builtin and has no executable of its own. Answers whether the volume took the junction —
    /// reparse points are a filesystem feature — so a test steps past a volume that refuses them
    /// instead of failing over coverage it never provided.
    fn junction(link: &std::path::Path, target: &std::path::Path) -> bool {
        let created = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("cmd should be spawnable");
        created.status.success()
    }

    /// Puts back what a `/deny` on `path` took away. A guard and not a call, because a panic — the
    /// failure the tests that use this exist to detect — unwinds past a restore written as a
    /// statement, and a directory left holding such a deny is not removable by the `TempDir`
    /// cleanup that follows, so the whole temporary tree stays on disk. Construct it before
    /// applying the deny, so that the exit taken when `icacls` cannot even be waited on is covered,
    /// and after the temporary directory, so that it runs before that directory is taken away.
    struct RestoreEntry<'a> {
        path: &'a str,
        user: &'a str,
    }

    impl Drop for RestoreEntry<'_> {
        fn drop(&mut self) {
            let _ = std::process::Command::new("icacls")
                .args([self.path, "/reset"])
                .output();
            let _ = std::process::Command::new("icacls")
                .args([self.path, "/remove:d", self.user])
                .output();
        }
    }

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 12, 34, 56)
            .single()
            .expect("the test timestamp should be valid")
    }

    fn insert_observation(conn: &rusqlite::Connection, id: ulid::Ulid, observed_at: DateTime<Utc>) {
        observations::insert(conn, &observation(id, observed_at))
            .expect("the image's observation should be stored");
    }

    fn observation(id: ulid::Ulid, observed_at: DateTime<Utc>) -> Observation {
        let mut observation = Observation::new_screen(
            ScreenPayload {
                width: WIDTH,
                height: HEIGHT,
                image_path: None,
                ocr_status: OcrStatus::NoText,
                ocr_error: None,
                ocr_text: None,
                ocr_langs: vec!["en".to_owned()],
                foreground_process: None,
                foreground_window_title: None,
                foreground_hwnd: None,
                foreground_pid: None,
            },
            observed_at,
        );
        observation.id = id;
        observation
    }

    fn pixels(seed: u8) -> Vec<u8> {
        let len = usize::try_from(u64::from(WIDTH) * u64::from(HEIGHT) * 3)
            .expect("the synthetic frame should fit in memory");
        let rgb = [seed, seed.wrapping_add(73), seed.wrapping_add(149)];
        (0..len).map(|index| rgb[index % rgb.len()]).collect()
    }

    fn name(id: ulid::Ulid, taken_at: DateTime<Utc>) -> String {
        relative_path(
            id,
            taken_at.fixed_offset(),
            Some("synthetic.exe"),
            Some("Synthetic window"),
        )
    }

    fn save_test_image(
        conn: &mut rusqlite::Connection,
        root: &std::path::Path,
        id: ulid::Ulid,
        taken_at: DateTime<Utc>,
        seed: u8,
    ) -> String {
        insert_observation(conn, id, taken_at);
        let relative = name(id, taken_at);
        save(
            conn,
            root,
            id,
            &relative,
            &pixels(seed),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
        )
        .expect("the synthetic image should be saved");
        relative
    }

    /// Registers a row with no file beside it, so a scan reports its name without a picture being
    /// encoded per row.
    fn register_row(conn: &rusqlite::Connection, index: u128, taken_at: DateTime<Utc>) -> String {
        let id = ulid::Ulid::from(index);
        insert_observation(conn, id, taken_at);
        let relative = legacy_path(id, taken_at);
        conn.execute(
            "INSERT INTO images (observation_id, relative_path, byte_size, created_at) \
             VALUES (?1, ?2, 1, ?3)",
            rusqlite::params![
                id.to_string(),
                relative,
                timestamp::to_sql(taken_at).expect("the test timestamp should be spellable")
            ],
        )
        .expect("the test image row should be storable");
        relative
    }

    /// One row per minute from `start`, one more than a single invocation of the scan may read.
    fn register_more_than_one_scan(
        conn: &rusqlite::Connection,
        start: DateTime<Utc>,
    ) -> Vec<String> {
        (0..=REACH)
            .map(|step| {
                register_row(
                    conn,
                    1 + step as u128,
                    start + TimeDelta::minutes(step as i64),
                )
            })
            .collect()
    }

    fn control_value(conn: &rusqlite::Connection, key: &str) -> String {
        conn.query_row(
            "SELECT value FROM control_state WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .expect("the scan's control row should be readable")
    }

    #[test]
    fn save_writes_a_decodable_webp_and_registers_it_in_the_images_table() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);

        let relative = save_test_image(&mut conn, &root, id, taken_at, 10);
        let path = root.join(&relative);
        let bytes = std::fs::read(&path).expect("the saved WebP should be readable");
        let metadata =
            std::fs::metadata(&path).expect("the saved WebP metadata should be readable");
        let (stored_path, byte_size, created_at): (String, i64, String) = conn
            .query_row(
                "SELECT relative_path, byte_size, created_at FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("the image row should be readable");

        assert!(std::path::Path::new(&relative).is_relative());
        assert!(path.is_file());
        assert!(bytes.len() >= 12);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
        // Without the decode, handing the encoder its height and width the other way round stores
        // a scrambled picture and every assertion above still holds.
        let decoded = webp::Decoder::new(&bytes)
            .decode()
            .expect("the saved WebP should decode");
        assert_eq!(decoded.width(), WIDTH);
        assert_eq!(decoded.height(), HEIGHT);
        assert_eq!(stored_path, relative);
        assert_eq!(
            byte_size,
            i64::try_from(metadata.len()).expect("the test file size should fit SQLite")
        );
        assert_eq!(
            created_at,
            timestamp::to_sql(taken_at).expect("the test timestamp should be spellable")
        );
    }

    #[test]
    fn an_image_past_max_path_is_saved_with_its_observation() {
        let (dir, mut conn, _) = database();
        let root = dir.path().join("r".repeat(200)).join("images");
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        let relative = name(id, taken_at);
        assert!(root.join(&relative).as_os_str().len() > 260);

        save_with_observation(
            &mut conn,
            &root,
            &observation(id, taken_at),
            &relative,
            &pixels(10),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
            |_| Ok(()),
        )
        .expect("a long image path should be saved");

        assert!(
            observations::find_by_id(&conn, id)
                .expect("the observation should be readable")
                .is_some()
        );
        let stored_path: String = conn
            .query_one(
                "SELECT relative_path FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the image row should be readable");
        assert_eq!(stored_path, relative);
        assert!(root.join(&relative).is_file());
    }

    #[test]
    fn an_image_whose_commit_fails_stays_for_the_sweep() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        // No observation is inserted, so the image row breaks a foreign key. Deferred, it is the
        // COMMIT that answers — the one failure the last statement of `save` can be given here.
        conn.execute_batch("PRAGMA defer_foreign_keys = ON")
            .expect("the pragma should apply");
        let relative = name(id, taken_at);

        let result = save(
            &mut conn,
            &root,
            id,
            &relative,
            &pixels(93),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
        );

        assert!(matches!(result, Err(StoreError::Sql { .. })), "{result:?}");
        let published = root.join(relative);
        assert!(
            published.is_file(),
            "the published image should still be there"
        );
        let registered: i64 = conn
            .query_one("SELECT count(*) FROM images", [], |row| row.get(0))
            .expect("the image count should be readable");
        assert_eq!(registered, 0);
    }

    #[test]
    fn a_save_that_cannot_take_the_write_lock_leaves_no_file_behind() {
        let (dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        insert_observation(&conn, id, at(2026, 7, 30));

        let mut blocker =
            db::open(&dir.path().join("db.sqlite3")).expect("a second connection should open");
        let held = blocker
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("the second connection should take the write lock");
        conn.busy_timeout(std::time::Duration::ZERO)
            .expect("the busy timeout should be settable");

        let result = save(
            &mut conn,
            &root,
            id,
            &name(id, at(2026, 7, 30)),
            &pixels(7),
            WIDTH,
            HEIGHT,
            75.0,
            at(2026, 7, 30),
        );

        assert!(matches!(result, Err(StoreError::Sql { .. })), "{result:?}");
        drop(held);
        let left_behind = enumerate(&root);
        assert!(left_behind.is_empty(), "{left_behind:?}");
    }

    #[test]
    fn the_stored_path_is_the_same_string_on_every_platform() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::from(1u128);

        let stored = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 20);

        assert!(stored.contains('/'));
        assert!(!stored.contains('\\'));
        assert_eq!(
            stored,
            "2026/07/30/123456_synthetic.exe_Synthetic window_00000000000000000000000001.webp"
        );
    }

    #[test]
    fn a_second_save_for_one_observation_is_refused_and_keeps_the_first() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        let relative = save_test_image(&mut conn, &root, id, taken_at, 30);
        let path = root.join(&relative);
        let first = std::fs::read(&path).expect("the first WebP should be readable");

        let error = save(
            &mut conn,
            &root,
            id,
            &relative,
            &pixels(200),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
        )
        .expect_err("the second image should be refused");

        match error {
            StoreError::ImageAlreadyRegistered { id: actual } => {
                assert_eq!(actual, id.to_string());
            }
            other => panic!("expected ImageAlreadyRegistered, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&path).expect("the first WebP should remain readable"),
            first
        );
        let count: i64 = conn
            .query_row("SELECT count(*) FROM images", [], |row| row.get(0))
            .expect("the image count should be readable");
        assert_eq!(count, 1);
    }

    #[test]
    fn a_path_another_observation_holds_is_not_reported_as_this_one_being_registered() {
        let (_dir, mut conn, root) = database();
        let first_id = ulid::Ulid::generate();
        let second_id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        let relative = save_test_image(&mut conn, &root, first_id, taken_at, 31);
        let destination = root.join(&relative);
        conn.execute(
            "DELETE FROM images WHERE observation_id = ?1",
            [first_id.to_string()],
        )
        .expect("the first image row should be removable without touching its file");
        insert_observation(&conn, second_id, taken_at);
        conn.execute(
            "INSERT INTO images (observation_id, relative_path, byte_size, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                second_id.to_string(),
                relative,
                1_i64,
                timestamp::to_sql(taken_at).expect("the test timestamp should be spellable"),
            ],
        )
        .expect("the conflicting image row should be planted");

        // An identical retry and this planted path share one extended error code; the id count is
        // what separates them.
        let error = save(
            &mut conn,
            &root,
            first_id,
            &relative,
            &pixels(32),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
        )
        .expect_err("the path held by another observation should be refused");

        assert!(
            matches!(&error, StoreError::Sql { .. }),
            "expected Sql, got {error:?}"
        );
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM images WHERE observation_id = ?1",
                [first_id.to_string()],
                |row| row.get(0),
            )
            .expect("the first observation's image count should be readable");
        assert_eq!(count, 0);
        let entries = std::fs::read_dir(
            destination
                .parent()
                .expect("the image destination should have a parent"),
        )
        .expect("the image day directory should be readable")
        .collect::<Result<Vec<_>, _>>()
        .expect("the image day directory entries should be readable");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), destination);
    }

    #[test]
    fn a_frame_whose_pixels_do_not_match_its_size_is_refused() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let mut short = pixels(40);
        short.pop();
        let relative = name(id, taken_at);

        let error = save(
            &mut conn, &root, id, &relative, &short, WIDTH, HEIGHT, 75.0, taken_at,
        )
        .expect_err("the short frame should be refused");

        match error {
            StoreError::Encode { id: actual, .. } => assert_eq!(actual, id.to_string()),
            other => panic!("expected Encode, got {other:?}"),
        }

        let mut long = pixels(40);
        long.push(0);
        // The encoder ignores trailing bytes, so only this side of the check catches a length test
        // weakened to accept a buffer that is merely large enough.
        let error = save(
            &mut conn, &root, id, &relative, &long, WIDTH, HEIGHT, 75.0, taken_at,
        )
        .expect_err("the long frame should be refused");
        match error {
            StoreError::Encode { id: actual, .. } => assert_eq!(actual, id.to_string()),
            other => panic!("expected Encode, got {other:?}"),
        }
        assert!(!root.exists());
        let count: i64 = conn
            .query_row("SELECT count(*) FROM images", [], |row| row.get(0))
            .expect("the image count should be readable");
        assert_eq!(count, 0);
    }

    #[test]
    fn a_frame_larger_than_the_encoder_allows_is_refused_rather_than_panicking() {
        let (_dir, mut conn, root) = database();
        let taken_at = at(2026, 7, 30);
        let oversized_id = ulid::Ulid::generate();
        let oversized = vec![0; 49_152];

        let oversized_error = save(
            &mut conn,
            &root,
            oversized_id,
            &name(oversized_id, taken_at),
            &oversized,
            16_384,
            1,
            75.0,
            taken_at,
        )
        .expect_err("the frame above the encoder's dimension limit should be refused");
        assert!(matches!(oversized_error, StoreError::Encode { .. }));

        let zero_width_id = ulid::Ulid::generate();
        let zero_width_error = save(
            &mut conn,
            &root,
            zero_width_id,
            &name(zero_width_id, taken_at),
            &[],
            0,
            1,
            75.0,
            taken_at,
        )
        .expect_err("a zero-width frame should be refused");
        assert!(matches!(zero_width_error, StoreError::Encode { .. }));

        let quality_id = ulid::Ulid::generate();
        let quality_error = save(
            &mut conn,
            &root,
            quality_id,
            &name(quality_id, taken_at),
            &pixels(41),
            WIDTH,
            HEIGHT,
            -1.0,
            taken_at,
        )
        .expect_err("a quality below the encoder's range should be refused");
        assert!(matches!(quality_error, StoreError::Encode { .. }));
    }

    #[test]
    fn quality_at_both_ends_of_the_allowed_range_is_accepted() {
        let (_dir, mut conn, root) = database();
        let zero_id = ulid::Ulid::generate();
        let hundred_id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, zero_id, taken_at);
        insert_observation(&conn, hundred_id, taken_at);

        // A four-by-three block of three repeating colours compresses to the same handful of bytes
        // at either end of the range, so a `quality` the encoder never sees would go unnoticed.
        const DETAILED: u32 = 64;
        let detailed: Vec<u8> = (0..DETAILED * DETAILED * 3)
            .map(|index| {
                let index = u64::from(index);
                let pixel = index / 3;
                let x = pixel % u64::from(DETAILED);
                let y = pixel / u64::from(DETAILED);
                (x * 7 + y * 13 + index % 3 * 61) as u8
            })
            .collect();
        save(
            &mut conn,
            &root,
            zero_id,
            &name(zero_id, taken_at),
            &detailed,
            DETAILED,
            DETAILED,
            0.0,
            taken_at,
        )
        .expect("quality zero should be accepted");
        save(
            &mut conn,
            &root,
            hundred_id,
            &name(hundred_id, taken_at),
            &detailed,
            DETAILED,
            DETAILED,
            100.0,
            taken_at,
        )
        .expect("quality one hundred should be accepted");

        for id in [zero_id, hundred_id] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM images WHERE observation_id = ?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .expect("the image count should be readable");
            assert_eq!(count, 1);
        }

        let sizes: Vec<i64> = [zero_id, hundred_id]
            .iter()
            .map(|id| {
                conn.query_row(
                    "SELECT byte_size FROM images WHERE observation_id = ?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .expect("the image size should be readable")
            })
            .collect();
        assert!(
            sizes[0] < sizes[1],
            "the configured quality reached the encoder: {sizes:?}"
        );
    }

    #[test]
    fn a_frame_at_the_encoders_dimension_limit_is_accepted() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let pixels = vec![0_u8; 49_149];

        save(
            &mut conn,
            &root,
            id,
            &name(id, taken_at),
            &pixels,
            16_383,
            1,
            75.0,
            taken_at,
        )
        .expect("a frame at the encoder's dimension limit should be accepted");

        let second_id = ulid::Ulid::generate();
        insert_observation(&conn, second_id, taken_at);
        save(
            &mut conn,
            &root,
            second_id,
            &name(second_id, taken_at),
            &pixels,
            1,
            16_383,
            75.0,
            taken_at,
        )
        .expect("a tall frame at the encoder's dimension limit should be accepted");

        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the image count should be readable");
        assert_eq!(count, 1);

        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM images WHERE observation_id = ?1",
                [second_id.to_string()],
                |row| row.get(0),
            )
            .expect("the second image count should be readable");
        assert_eq!(count, 1);
    }

    #[test]
    fn a_timestamp_this_schema_cannot_store_is_refused_before_anything_is_written() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();

        let error = save(
            &mut conn,
            &root,
            id,
            &name(id, DateTime::<Utc>::MAX_UTC),
            &pixels(42),
            WIDTH,
            HEIGHT,
            75.0,
            DateTime::<Utc>::MAX_UTC,
        )
        .expect_err("the timestamp this schema cannot store should be refused");

        assert!(matches!(error, StoreError::TimestampOutOfRange { .. }));
        assert!(!root.exists());
    }

    #[test]
    fn delete_removes_file_and_images_row() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 50);
        let path = root.join(relative);
        let before = observation_row(&conn, &id.to_string());

        let outcome = delete(&mut conn, &root, id).expect("the image should be deleted");

        assert_eq!(outcome, DeleteOutcome::Removed);
        assert!(!path.exists());
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the image count should be readable");
        assert_eq!(count, 0);
        assert_eq!(
            observation_row(&conn, &id.to_string()),
            before,
            "the observation row must read back exactly as saved"
        );
    }

    fn observation_row(conn: &rusqlite::Connection, id: &str) -> Vec<rusqlite::types::Value> {
        conn.query_row("SELECT * FROM observations WHERE id = ?1", [id], |row| {
            (0..row.as_ref().column_count())
                .map(|column| row.get(column))
                .collect()
        })
        .expect("the observation row should be readable")
    }

    #[test]
    fn deleting_a_row_whose_file_is_already_gone_still_removes_the_row() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 51);
        std::fs::remove_file(root.join(relative))
            .expect("the saved file should be removable without touching its row");

        let outcome = delete(&mut conn, &root, id).expect("the explicit deletion should succeed");

        assert_eq!(outcome, DeleteOutcome::MissingFile);
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the image count should be readable");
        assert_eq!(count, 0);
    }

    #[test]
    fn an_entry_whose_target_is_gone_is_still_removed() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 53);
        let path = root.join(relative);
        let missing_target = path.with_file_name("missing-target.webp");
        std::fs::remove_file(&path)
            .expect("the saved file should be removable before replacement with a symlink");

        let Ok(()) = std::os::windows::fs::symlink_file(&missing_target, &path) else {
            return;
        };

        let outcome = delete(&mut conn, &root, id).expect("the dangling symlink should be deleted");

        assert_eq!(outcome, DeleteOutcome::MissingFile);
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the image count should be readable");
        assert_eq!(count, 0);
        assert!(path.symlink_metadata().is_err());
    }

    #[test]
    fn deleting_a_name_that_is_a_link_takes_the_link_and_not_what_it_points_at() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 80);
        let path = root.join(relative);
        let target = path.with_file_name("pointed-at.webp");
        std::fs::write(&target, b"the file at the other end")
            .expect("the link target should be writable");
        std::fs::remove_file(&path)
            .expect("the saved file should be removable before replacement with a symlink");

        let Ok(()) = std::os::windows::fs::symlink_file(&target, &path) else {
            return;
        };

        let outcome =
            delete(&mut conn, &root, id).expect("the link standing at the name should be deleted");

        assert_eq!(outcome, DeleteOutcome::Removed);
        assert!(path.symlink_metadata().is_err());
        assert!(target.is_file());
    }

    #[test]
    fn a_name_that_will_not_open_keeps_its_row() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 52);
        let path = root.join(relative);

        // A directory answers `PermissionDenied` to the open this removal needs, and measured, so
        // does a link to one; a missing file and a missing parent both come back as `NotFound`.
        std::fs::remove_file(&path).expect("the saved file should be removable before replacement");
        std::fs::create_dir(&path).expect("a directory should be creatable at the image path");

        let error = delete(&mut conn, &root, id)
            .expect_err("removing a directory as an image file should fail");
        match error {
            StoreError::ImageIo { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected ImageIo with PermissionDenied, got {other:?}"),
        }
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the image count should be readable");
        assert_eq!(count, 1);
        assert!(path.is_dir());
    }

    #[test]
    fn a_row_naming_a_path_this_program_would_not_write_is_refused() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let canonical = legacy_path(id, taken_at);
        let path = root.join(&canonical);
        std::fs::create_dir_all(
            path.parent()
                .expect("the hand-placed image should have a parent"),
        )
        .expect("the hand-placed image directory should be creatable");
        let contents = b"registered image";
        std::fs::write(&path, contents).expect("the hand-placed image should be writable");
        let stored = canonical.replace('/', "\\");
        conn.execute(
            "INSERT INTO images (observation_id, relative_path, byte_size, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                id.to_string(),
                stored,
                i64::try_from(contents.len()).expect("the test file size should fit SQLite"),
                timestamp::to_sql(taken_at).expect("the test timestamp should be spellable"),
            ],
        )
        .expect("the malformed image row should be inserted by hand");

        match delete(&mut conn, &root, id).expect_err("delete should refuse the malformed path") {
            StoreError::Encoding { id: actual, .. } => assert_eq!(actual, id.to_string()),
            other => panic!("expected Encoding, got {other:?}"),
        }
        let scan = scan_orphan_rows(&mut conn, &root).expect("the scan should step past the row");
        assert_eq!(scan.undecodable, 1);
        assert!(scan.missing.is_empty());
        assert!(path.is_file());
    }

    #[test]
    fn a_row_whose_timestamp_is_spelled_any_other_way_is_refused() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let canonical = legacy_path(id, taken_at);
        let path = root.join(&canonical);
        std::fs::create_dir_all(
            path.parent()
                .expect("the hand-placed image should have a parent"),
        )
        .expect("the hand-placed image directory should be creatable");
        let contents = b"registered image";
        std::fs::write(&path, contents).expect("the hand-placed image should be writable");
        conn.execute(
            "INSERT INTO images (observation_id, relative_path, byte_size, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                id.to_string(),
                canonical,
                i64::try_from(contents.len()).expect("the test file size should fit SQLite"),
                "2026-07-30T12:34:56+00:00",
            ],
        )
        .expect("the non-canonical timestamp image row should be inserted by hand");

        match delete(&mut conn, &root, id)
            .expect_err("delete should refuse the non-canonical timestamp")
        {
            StoreError::Encoding { id: actual, .. } => assert_eq!(actual, id.to_string()),
            other => panic!("expected Encoding, got {other:?}"),
        }
        let scan = scan_orphan_rows(&mut conn, &root).expect("the scan should step past the row");
        assert_eq!(scan.undecodable, 1);
        assert!(scan.missing.is_empty());
        assert!(path.is_file());
    }

    #[test]
    fn a_row_whose_id_is_spelled_any_other_way_is_refused_by_what_reads_rows() {
        let (_dir, mut conn, root) = database();
        let stored_id = "0000000000000128ggyhyyk08n";
        let id = ulid::Ulid::from_string(stored_id)
            .expect("the lower-cased observation id should still parse");
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        conn.execute(
            "UPDATE observations SET id = ?1 WHERE id = ?2",
            rusqlite::params![stored_id, id.to_string()],
        )
        .expect("the observation id should be made non-canonical before it has children");

        let relative = legacy_path(id, taken_at);
        let path = root.join(&relative);
        std::fs::create_dir_all(
            path.parent()
                .expect("the hand-placed image should have a parent"),
        )
        .expect("the hand-placed image directory should be creatable");
        let contents = b"registered image";
        std::fs::write(&path, contents).expect("the hand-placed image should be writable");

        conn.execute(
            "INSERT INTO images (observation_id, relative_path, byte_size, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                stored_id,
                relative,
                i64::try_from(contents.len()).expect("the test file size should fit SQLite"),
                timestamp::to_sql(taken_at).expect("the test timestamp should be spellable"),
            ],
        )
        .expect("the non-canonical image row should be inserted by hand");

        // `delete` answers the question asked: no row exists under the canonical id. Searching for
        // equivalent spellings would put an unindexed scan on the retention path.
        assert_eq!(
            delete(&mut conn, &root, id).expect("the canonical id should have nothing to delete"),
            DeleteOutcome::NoRow
        );

        let scan = scan_orphan_rows(&mut conn, &root).expect("the scan should step past the row");
        assert_eq!(scan.undecodable, 1);
        assert!(scan.missing.is_empty());
        assert!(path.is_file());
    }

    #[test]
    fn orphan_files_without_db_row_are_swept() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 80);
        let saved = root.join(relative);
        let unregistered = root.join("2026").join("07").join("29").join("orphan.webp");
        std::fs::create_dir_all(
            unregistered
                .parent()
                .expect("the hand-placed file should have a parent"),
        )
        .expect("the hand-placed file directory should be creatable");
        std::fs::write(&unregistered, b"not registered")
            .expect("the hand-placed file should be writable");

        let removed =
            sweep_orphan_files(&mut conn, &root).expect("the orphan sweep should succeed");

        assert_eq!(removed, 1);
        assert!(!unregistered.exists());
        assert!(saved.is_file());
        let count: i64 = conn
            .query_row("SELECT count(*) FROM images", [], |row| row.get(0))
            .expect("the image count should be readable");
        assert_eq!(count, 1);
    }

    #[test]
    fn a_temporary_no_discard_removed_is_swept_and_its_destination_is_not() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 80);
        let saved = root.join(relative);

        let mut leftover = saved.clone().into_os_string();
        leftover.push(format!(".tmp-{}-0", std::process::id()));
        let leftover = std::path::PathBuf::from(leftover);
        std::fs::write(&leftover, b"a temporary nothing removed")
            .expect("the leftover temporary should be writable");

        let removed =
            sweep_orphan_files(&mut conn, &root).expect("the orphan sweep should succeed");

        assert_eq!(removed, 1);
        assert!(!leftover.exists());
        assert!(saved.is_file());
    }

    #[test]
    fn the_saver_holds_its_temporary_undeletable() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const ERROR_SHARING_VIOLATION: i32 = 32;

        let (dir, _conn, root) = database();
        let taken_at = at(2026, 7, 30);
        let database_path = dir.path().join("db.sqlite3");
        let saving_root = root.clone();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop_saving = std::sync::Arc::clone(&stop);
        let saver = std::thread::spawn(move || {
            let mut conn = db::open(&database_path).expect("the saver's connection should open");
            let mut seed = 0u8;
            while !stop_saving.load(Ordering::Relaxed) {
                save_test_image(
                    &mut conn,
                    &saving_root,
                    ulid::Ulid::generate(),
                    taken_at,
                    seed,
                );
                seed = seed.wrapping_add(1);
            }
        });

        let leaf = root.join("2026").join("07").join("30");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut observed = 0usize;
        let mut betrayed = None;
        'poll: while observed == 0 && std::time::Instant::now() < deadline {
            let Ok(entries) = std::fs::read_dir(&leaf) else {
                continue;
            };
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().contains(".tmp-") {
                    continue;
                }
                let path = entry.path();
                match cw_core::atomic_file::open_for_removal(&path) {
                    // No save here fails, so nothing leaves residue: a temporary name that is
                    // still there is one a live save is holding.
                    Ok(_) => {
                        betrayed = Some(format!("{} opened for removal", path.display()));
                        break 'poll;
                    }
                    Err(source) if source.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
                        observed += 1;
                    }
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => {
                        betrayed = Some(format!("{} answered {source:?}", path.display()));
                        break 'poll;
                    }
                }
            }
        }

        stop.store(true, Ordering::Relaxed);
        if let Err(panic) = saver.join() {
            std::panic::resume_unwind(panic);
        }

        assert!(betrayed.is_none(), "{}", betrayed.unwrap_or_default());
        assert!(
            observed >= 1,
            "the save loop never exposed a temporary to look at, so nothing was tested"
        );
    }

    #[test]
    fn a_temporary_a_live_save_holds_is_kept_and_collected_once_that_handle_closes() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 81);
        let saved = root.join(relative);

        let (temporary, file) = cw_core::atomic_file::create_undeletable_temporary_beside(&saved)
            .expect("the live temporary should be reservable");

        let removed =
            sweep_orphan_files(&mut conn, &root).expect("the orphan sweep should succeed");

        assert_eq!(removed, 0);
        assert!(temporary.is_file());
        assert!(saved.is_file());

        drop(file);

        let removed =
            sweep_orphan_files(&mut conn, &root).expect("the orphan sweep should succeed");

        assert_eq!(removed, 1);
        assert!(!temporary.exists());
        assert!(saved.is_file());
    }

    #[test]
    fn an_orphan_that_became_a_link_to_a_registered_image_is_not_removed() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 82);
        let picture = root.join(&relative);
        let orphan = root.join("2026").join("07").join("30").join("orphan.webp");
        std::fs::write(&orphan, b"not registered")
            .expect("the hand-placed file should be writable");

        let resolved = std::fs::canonicalize(&root).expect("the image root should resolve");
        // Both were ordinary files when they were enumerated; the orphan then becomes a link to the
        // registered one, which is the window the identity check exists to close.
        let batch = enumerate(&resolved);
        std::fs::remove_file(&orphan).expect("the hand-placed file should be removable");
        let Ok(()) = std::os::windows::fs::symlink_file(&picture, &orphan) else {
            return;
        };

        let removed = sweep_batch(&mut conn, &batch).expect("the sweep should succeed");

        assert_eq!(removed, 0);
        assert!(picture.is_file());
    }

    #[test]
    fn an_unrelated_orphan_is_still_swept_when_a_registered_file_is_missing() {
        let (_dir, mut conn, root) = database();
        let first_id = ulid::Ulid::generate();
        let second_id = ulid::Ulid::generate();
        let first_relative = save_test_image(&mut conn, &root, first_id, at(2026, 7, 30), 83);
        let second_relative = save_test_image(&mut conn, &root, second_id, at(2026, 7, 31), 84);
        std::fs::remove_file(root.join(&first_relative))
            .expect("the first saved file should be removable without touching its row");
        let left_behind = root.join("2026/07/31/left-behind.webp");
        std::fs::create_dir_all(
            left_behind
                .parent()
                .expect("the hand-placed file should have a parent"),
        )
        .expect("the hand-placed file directory should be creatable");
        std::fs::write(&left_behind, b"not registered")
            .expect("the hand-placed file should be writable");

        let removed =
            sweep_orphan_files(&mut conn, &root).expect("the orphan sweep should succeed");

        assert_eq!(removed, 1);
        assert!(!left_behind.exists());
        assert!(root.join(second_relative).is_file());
        assert_eq!(
            scan_orphan_rows(&mut conn, &root)
                .expect("the scan should run")
                .missing,
            [first_relative]
        );
    }

    #[test]
    fn a_name_that_now_leads_outside_the_root_is_not_removed() {
        let (dir, mut conn, root) = database();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("the outside directory should be creatable");
        let victim = outside.join("orphan.webp");
        std::fs::write(&victim, b"a file this program has no business touching")
            .expect("the outside file should be writable");
        std::fs::create_dir_all(&root).expect("the image root should be creatable");
        let redirected = root.join("2026");

        if !junction(&redirected, &outside) {
            return;
        }

        let resolved = std::fs::canonicalize(&root).expect("the image root should resolve");

        let batch = vec![(
            "2026/orphan.webp".to_owned(),
            resolved.join("2026").join("orphan.webp"),
        )];
        let removed = sweep_batch(&mut conn, &batch)
            .expect("the sweep should succeed without removing anything");

        assert_eq!(removed, 0);
        assert!(victim.is_file());
    }

    #[test]
    fn a_root_replaced_after_the_walk_does_not_redirect_the_sweep() {
        let (dir, mut conn, root) = database();
        std::fs::create_dir_all(&root).expect("the image root should be creatable");
        let resolved = std::fs::canonicalize(&root).expect("the image root should resolve");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("the outside directory should be creatable");
        let victim = outside.join("orphan.webp");
        std::fs::write(&victim, b"a file this program has no business touching")
            .expect("the outside file should be writable");
        std::fs::rename(&root, dir.path().join("moved"))
            .expect("the image root should be movable aside");

        if !junction(&root, &outside) {
            return;
        }

        // The batch is what the walk had already yielded when the root was replaced under it.
        let batch = vec![("orphan.webp".to_owned(), resolved.join("orphan.webp"))];
        let removed = sweep_batch(&mut conn, &batch)
            .expect("the sweep should succeed without removing anything");

        assert_eq!(removed, 0);
        assert!(victim.is_file());
    }

    #[test]
    fn a_sweep_of_a_root_that_is_not_there_yet_removes_nothing() {
        let (_dir, mut conn, root) = database();

        assert!(!root.exists());
        assert_eq!(
            sweep_orphan_files(&mut conn, &root)
                .expect("a sweep before the first save should succeed"),
            0
        );
    }

    #[test]
    fn a_root_whose_entry_is_there_and_will_not_resolve_is_reported() {
        let (dir, mut conn, root) = database();
        let nowhere = dir.path().join("nowhere");

        if !junction(&root, &nowhere) {
            return;
        }

        let result = sweep_orphan_files(&mut conn, &root);

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_root_under_a_directory_that_does_not_exist_yet_removes_nothing() {
        let (dir, mut conn, _root) = database();
        let root = dir.path().join("not-yet").join("images");

        assert_eq!(
            sweep_orphan_files(&mut conn, &root)
                .expect("a sweep before the first save should succeed"),
            0
        );
    }

    #[test]
    fn a_root_whose_parent_leads_nowhere_is_reported() {
        let (dir, mut conn, _root) = database();
        let parent = dir.path().join("data");
        let root = parent.join("images");

        if !junction(&parent, &dir.path().join("nowhere")) {
            return;
        }

        // Told apart from the test above by what stands over the root: a link whose target is gone,
        // under which `create_dir_all` answers `AlreadyExists` and no image can ever be written.
        let result = sweep_orphan_files(&mut conn, &root);

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_root_the_filesystem_will_not_answer_about_is_reported() {
        let (dir, mut conn, _root) = database();
        let root = dir.path().join("im|ages");

        let result = sweep_orphan_files(&mut conn, &root);

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_root_that_is_an_ordinary_file_is_reported() {
        let (_dir, mut conn, root) = database();
        std::fs::write(&root, b"not a directory")
            .expect("the file standing in for the image root should be writable");

        let result = sweep_orphan_files(&mut conn, &root);

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_relative_root_that_is_not_there_yet_removes_nothing() {
        let (_dir, mut conn, _root) = database();
        let root = std::path::PathBuf::from("cw-store-root-that-was-never-created");
        assert!(!root.exists(), "the test would say nothing if this existed");

        assert_eq!(
            sweep_orphan_files(&mut conn, &root)
                .expect("a relative root is simply not created yet"),
            0
        );
    }

    #[test]
    fn a_root_anchored_to_a_drive_that_is_not_there_is_reported() {
        let (_dir, mut conn, _root) = database();
        let Some(letter) = ('D'..='Z').find(|letter| {
            std::fs::symlink_metadata(format!("{letter}:\\"))
                .is_err_and(|absent| absent.kind() == std::io::ErrorKind::NotFound)
        }) else {
            return;
        };
        for root in [
            std::path::PathBuf::from(format!("{letter}:\\ContextWitness\\images")),
            std::path::PathBuf::from(format!("{letter}:ContextWitness\\images")),
        ] {
            let result = sweep_orphan_files(&mut conn, &root);

            assert!(
                matches!(result, Err(StoreError::ImageIo { .. })),
                "{}: {result:?}",
                root.display()
            );
        }
    }

    #[test]
    fn a_root_that_is_a_directory_no_one_may_open_is_reported() {
        let (_dir, mut conn, root) = database();
        std::fs::create_dir(&root).expect("the image root should be creatable");
        let Ok(user) = std::env::var("USERNAME") else {
            return;
        };
        let path = root.to_string_lossy().to_string();
        let _restore = RestoreEntry {
            path: path.as_str(),
            user: user.as_str(),
        };
        // After the deny, `canonicalize` answers `PermissionDenied` while `symlink_metadata` still
        // answers that something is there, which is the pair the ancestor walk turns on.
        let denied = std::process::Command::new("icacls")
            .args([
                path.as_str(),
                "/deny",
                &format!("{user}:(RX,RA,RD)"),
                "/inheritance:r",
            ])
            .output();
        let Ok(output) = denied else {
            return;
        };
        if !output.status.success() || std::fs::canonicalize(&root).is_ok() {
            return;
        }

        let result = sweep_orphan_files(&mut conn, &root);

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_root_that_will_not_be_listed_is_reported() {
        let (_dir, mut conn, root) = database();
        std::fs::create_dir(&root).expect("the image root should be creatable");
        std::fs::write(root.join("ordinary.webp"), b"an orphan")
            .expect("the file should be writable");
        let Ok(user) = std::env::var("USERNAME") else {
            return;
        };
        let path = root.to_string_lossy().to_string();
        let _restore = RestoreEntry {
            path: path.as_str(),
            user: user.as_str(),
        };
        let denied = std::process::Command::new("icacls")
            .args([path.as_str(), "/deny", &format!("{user}:(RD)")])
            .output();
        let Ok(output) = denied else {
            return;
        };
        if !output.status.success()
            || std::fs::canonicalize(&root).is_err()
            || std::fs::read_dir(&root).is_ok()
        {
            return;
        }

        let result = sweep_orphan_files(&mut conn, &root);

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_directory_that_will_not_be_listed_does_not_stop_the_sweep() {
        let (_dir, mut conn, root) = database();
        let blocked = root.join("blocked");
        std::fs::create_dir_all(&blocked).expect("the image root should be creatable");
        let ordinary = root.join("ordinary.webp");
        std::fs::write(&ordinary, b"an orphan").expect("the file should be writable");
        std::fs::write(blocked.join("inner.webp"), b"an orphan")
            .expect("the file should be writable");
        let Ok(user) = std::env::var("USERNAME") else {
            return;
        };
        let path = blocked.to_string_lossy().to_string();
        let _restore = RestoreEntry {
            path: path.as_str(),
            user: user.as_str(),
        };
        let denied = std::process::Command::new("icacls")
            .args([path.as_str(), "/deny", &format!("{user}:(RX,RA,RD)")])
            .output();
        let Ok(output) = denied else {
            return;
        };
        if !output.status.success() || std::fs::read_dir(&blocked).is_ok() {
            return;
        }

        let removed = sweep_orphan_files(&mut conn, &root)
            .expect("one place that will not open is not the pass");

        assert_eq!(removed, 1);
        assert!(
            !ordinary.exists(),
            "the orphan the sweep could reach should be gone"
        );
        assert!(
            blocked.join("inner.webp").exists(),
            "nothing under a directory that was not listed is a candidate"
        );
    }

    #[test]
    fn an_orphan_that_will_not_go_does_not_stop_the_others() {
        let (_dir, mut conn, root) = database();
        let deeper = root.join("2026");
        std::fs::create_dir_all(&deeper).expect("the deeper directory should be creatable");
        let stubborn = root.join("stubborn.webp");
        let beside = root.join("beside.webp");
        let below = deeper.join("below.webp");
        std::fs::write(&stubborn, b"an orphan").expect("the file should be writable");
        std::fs::write(&beside, b"an orphan").expect("the file should be writable");
        std::fs::write(&below, b"an orphan").expect("the file should be writable");
        let mut attributes = std::fs::metadata(&stubborn)
            .expect("the file should be there")
            .permissions();
        attributes.set_readonly(true);
        std::fs::set_permissions(&stubborn, attributes).expect("the attribute should be settable");

        let resolved = std::fs::canonicalize(&root).expect("the image root should resolve");
        // The refuser sits between the two collectible candidates, so a pass abandoned on a
        // refused removal loses one of them under either iteration order.
        let batch = vec![
            ("beside.webp".to_owned(), resolved.join("beside.webp")),
            ("stubborn.webp".to_owned(), resolved.join("stubborn.webp")),
            (
                "2026/below.webp".to_owned(),
                resolved.join("2026").join("below.webp"),
            ),
        ];
        let removed = sweep_batch(&mut conn, &batch);

        assert_eq!(
            removed.expect("one file that will not go must not fail the pass"),
            2
        );
        assert!(stubborn.exists(), "the one that will not go should be kept");
        assert!(!beside.exists(), "the one beside it should have gone");
        assert!(!below.exists(), "the one below it should have gone");
    }

    #[test]
    fn a_sweep_whose_orphan_was_already_removed_still_succeeds() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 82);
        let saved = root.join(relative);
        let unregistered = root.join("2026").join("07").join("30").join("orphan.webp");
        std::fs::write(&unregistered, b"not registered")
            .expect("the hand-placed file should be writable");
        std::fs::remove_file(&unregistered)
            .expect("the hand-placed file should be removable before the sweep");

        assert_eq!(
            sweep_orphan_files(&mut conn, &root).expect("the orphan sweep should still succeed"),
            0
        );

        std::fs::write(&unregistered, b"not registered")
            .expect("the hand-placed file should be writable again");
        let resolved = std::fs::canonicalize(&root).expect("the image root should resolve");
        let batch = enumerate(&resolved);

        assert_eq!(
            sweep_batch(&mut conn, &batch).expect("the first pass over the batch should succeed"),
            1
        );
        assert_eq!(
            sweep_batch(&mut conn, &batch).expect("the second pass over the batch should succeed"),
            0
        );
        assert!(saved.is_file());
    }

    #[test]
    fn scanned_rows_without_file_are_reported_and_kept() {
        let (_dir, mut conn, root) = database();
        // Saved newest-first, so the promised order — the order the pictures were taken — cannot
        // be mistaken for the insertion order an unordered scan would answer with.
        let later = ulid::Ulid::generate();
        let later_relative = save_test_image(&mut conn, &root, later, at(2026, 7, 30), 90);
        let earlier = ulid::Ulid::generate();
        let earlier_relative = save_test_image(&mut conn, &root, earlier, at(2026, 7, 29), 93);
        std::fs::remove_file(root.join(&later_relative))
            .expect("the saved file should be removable without touching its row");
        std::fs::remove_file(root.join(&earlier_relative))
            .expect("the saved file should be removable without touching its row");

        let scan = scan_orphan_rows(&mut conn, &root).expect("the scan should run");

        assert_eq!(
            scan,
            RowScan {
                examined: 2,
                missing: vec![earlier_relative, later_relative],
                undecodable: 0,
                finished: true,
            }
        );
        let count: i64 = conn
            .query_row("SELECT count(*) FROM images", [], |row| row.get(0))
            .expect("the image count should be readable");
        assert_eq!(count, 2);
    }

    #[test]
    fn a_registered_name_that_is_a_link_to_its_image_is_not_reported() {
        let (dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 91);
        let registered = root.join(&relative);
        let moved = dir.path().join("moved.webp");
        std::fs::rename(&registered, &moved).expect("the image should be movable aside");

        let Ok(()) = std::os::windows::fs::symlink_file(&moved, &registered) else {
            return;
        };

        assert!(
            scan_orphan_rows(&mut conn, &root)
                .expect("the scan should run")
                .missing
                .is_empty()
        );
    }

    #[test]
    fn a_registered_name_held_by_a_directory_is_reported() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 92);
        let registered = root.join(&relative);
        std::fs::remove_file(&registered).expect("the image should be removable");
        std::fs::create_dir(&registered).expect("a directory should take the freed name");

        assert_eq!(
            scan_orphan_rows(&mut conn, &root)
                .expect("the scan should run")
                .missing,
            vec![relative]
        );
    }

    #[test]
    fn a_scan_of_an_empty_table_finishes_without_marking_anything() {
        let (_dir, mut conn, root) = database();

        let scan = scan_orphan_rows(&mut conn, &root).expect("the scan should run");

        assert_eq!(
            scan,
            RowScan {
                finished: true,
                ..RowScan::default()
            }
        );
        let stored: i64 = conn
            .query_row("SELECT count(*) FROM control_state", [], |row| row.get(0))
            .expect("the control rows should be countable");
        assert_eq!(stored, 0);
    }

    #[test]
    fn the_scan_resumes_under_its_mark_after_running_out_of_pages_and_a_reopen() {
        let (dir, mut conn, root) = database();
        let planted = register_more_than_one_scan(&conn, at(2026, 7, 1));

        let first = scan_orphan_rows(&mut conn, &root).expect("the first scan should run");
        assert_eq!(first.examined, REACH as u64);
        assert_eq!(first.missing, planted[..REACH]);
        assert!(!first.finished);

        drop(conn);
        let mut conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the store should reopen in place");

        let second = scan_orphan_rows(&mut conn, &root).expect("the second scan should run");
        assert_eq!(second.examined, 1);
        assert_eq!(second.missing, planted[REACH..]);
        assert!(second.finished);
    }

    #[test]
    fn a_row_saved_above_the_mark_waits_for_the_cycle_after_it() {
        let (_dir, mut conn, root) = database();
        let planted = register_more_than_one_scan(&conn, at(2026, 7, 1));
        assert!(
            !scan_orphan_rows(&mut conn, &root)
                .expect("the first scan should run")
                .finished
        );

        let late = register_row(&conn, 500, at(2026, 8, 1));

        let second = scan_orphan_rows(&mut conn, &root).expect("the second scan should run");
        assert_eq!(second.missing, planted[REACH..]);
        assert!(second.finished);

        let third = scan_orphan_rows(&mut conn, &root).expect("the third scan should run");
        let fourth = scan_orphan_rows(&mut conn, &root).expect("the fourth scan should run");
        assert!(!third.missing.contains(&late));
        assert!(fourth.missing.contains(&late));
    }

    #[test]
    fn a_row_the_scan_cannot_read_back_is_counted_and_the_rows_behind_it_still_examined() {
        let (_dir, mut conn, root) = database();
        let start = at(2026, 7, 1);
        // One row past the first page, so what carries the scan over the unreadable row is the
        // cursor and not the page it happened to sit in.
        let planted: Vec<String> = (0..=PER_PAGE)
            .map(|step| {
                register_row(
                    &conn,
                    1 + step as u128,
                    start + TimeDelta::minutes(step as i64),
                )
            })
            .collect();
        conn.execute(
            "UPDATE images SET relative_path = 'not a path this program would write' \
             WHERE observation_id = ?1",
            [ulid::Ulid::from(2u128).to_string()],
        )
        .expect("the planted path should be storable");

        let scan = scan_orphan_rows(&mut conn, &root).expect("the scan should run");

        assert_eq!(scan.examined, planted.len() as u64);
        assert_eq!(scan.undecodable, 1);
        assert_eq!(scan.missing, [&planted[..1], &planted[2..]].concat());
        assert!(scan.finished);
    }

    #[test]
    fn a_cursor_stored_without_its_mark_starts_the_cycle_over() {
        let (_dir, mut conn, root) = database();
        let start = at(2026, 7, 1);
        let planted = register_more_than_one_scan(&conn, start);
        // Past every row, so a scan that took this cursor for a resumable one would answer that
        // there was nothing left to look at.
        let spelled = format!(
            "{}|{}",
            timestamp::to_sql(start + TimeDelta::minutes(REACH as i64))
                .expect("the test timestamp should be spellable"),
            ulid::Ulid::from(1 + REACH as u128)
        );
        conn.execute(
            "INSERT INTO control_state (key, value) VALUES (?1, ?2)",
            rusqlite::params![SCANNER_CURSOR, spelled],
        )
        .expect("the half-stored cursor should be storable");

        let scan = scan_orphan_rows(&mut conn, &root).expect("the scan should run");

        assert_eq!(scan.examined, REACH as u64);
        assert_eq!(scan.missing, planted[..REACH]);
        assert!(!scan.finished);
    }

    #[test]
    fn a_range_drained_by_the_last_permitted_page_is_finished() {
        let (_dir, mut conn, root) = database();
        let start = at(2026, 7, 1);
        // Exactly one invocation's budget: no further page runs to find the range empty, so what
        // ends the cycle is the cursor having reached the mark.
        for step in 0..REACH {
            register_row(
                &conn,
                1 + step as u128,
                start + TimeDelta::minutes(step as i64),
            );
        }

        let scan = scan_orphan_rows(&mut conn, &root).expect("the scan should run");

        assert_eq!(scan.examined, REACH as u64);
        assert!(scan.finished);
        assert_eq!(control_value(&conn, SCANNER_CURSOR), "");
        assert_eq!(control_value(&conn, SCANNER_HIGH_WATER), "");
    }

    #[test]
    fn a_cycle_whose_mark_row_was_deleted_is_finished_by_its_empty_page() {
        let (_dir, mut conn, root) = database();
        let planted = register_more_than_one_scan(&conn, at(2026, 7, 1));
        assert!(
            !scan_orphan_rows(&mut conn, &root)
                .expect("the first scan should run")
                .finished
        );
        // The row the mark names, so the cursor can never reach it and only an empty page is left
        // to say the range is drained.
        conn.execute(
            "DELETE FROM images WHERE relative_path = ?1",
            [&planted[REACH]],
        )
        .expect("the marked row should be deletable");

        let scan = scan_orphan_rows(&mut conn, &root).expect("the second scan should run");

        assert_eq!(scan.examined, 0);
        assert!(scan.finished);
        assert_eq!(control_value(&conn, SCANNER_CURSOR), "");
        assert_eq!(control_value(&conn, SCANNER_HIGH_WATER), "");
    }

    #[test]
    fn the_scan_returns_both_keys_to_the_head_once_the_cycle_ends() {
        let (_dir, mut conn, root) = database();
        register_more_than_one_scan(&conn, at(2026, 7, 1));

        assert!(
            !scan_orphan_rows(&mut conn, &root)
                .expect("the first scan should run")
                .finished
        );
        assert!(!control_value(&conn, SCANNER_CURSOR).is_empty());
        assert!(!control_value(&conn, SCANNER_HIGH_WATER).is_empty());

        assert!(
            scan_orphan_rows(&mut conn, &root)
                .expect("the second scan should run")
                .finished
        );
        assert_eq!(control_value(&conn, SCANNER_CURSOR), "");
        assert_eq!(control_value(&conn, SCANNER_HIGH_WATER), "");
    }

    #[test]
    fn saving_again_over_a_row_whose_file_is_gone_leaves_the_row_and_no_new_file() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        let relative = save_test_image(&mut conn, &root, id, taken_at, 91);
        let path = root.join(&relative);
        let original_byte_size: i64 = conn
            .query_row(
                "SELECT byte_size FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the original byte size should be readable");
        std::fs::remove_file(&path)
            .expect("the saved file should be removable without touching its row");

        let error = save(
            &mut conn,
            &root,
            id,
            &relative,
            &pixels(201),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
        )
        .expect_err("the existing image row should refuse another file");

        assert!(matches!(error, StoreError::ImageAlreadyRegistered { .. }));
        let stored_byte_size: i64 = conn
            .query_row(
                "SELECT byte_size FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the original byte size should remain readable");
        assert_eq!(stored_byte_size, original_byte_size);
        assert!(!path.exists());
        let files = enumerate(&root);
        assert!(!files.iter().any(|(relative, _)| relative.contains(".tmp-")));
    }

    #[test]
    fn a_failed_rename_leaves_no_temporary_behind() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let relative = name(id, taken_at);
        let destination = root.join(&relative);
        std::fs::create_dir_all(
            destination
                .parent()
                .expect("the image destination should have a parent"),
        )
        .expect("the image directory should be creatable");
        std::fs::write(&destination, b"a file already owns the destination")
            .expect("the taken destination should be writable");

        let error = save(
            &mut conn,
            &root,
            id,
            &relative,
            &pixels(210),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
        )
        .expect_err("the taken destination should be refused");
        match error {
            StoreError::ImageIo { path, source } => {
                assert_eq!(path, destination);
                assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists);
            }
            other => panic!("expected ImageIo with AlreadyExists, got {other:?}"),
        }

        // No row is what tells the transaction apart from an autocommitted INSERT: the row write
        // had already run when the publish was refused, so only the rollback leaves this at zero.
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the image count should be readable");
        assert_eq!(count, 0);

        let entries = std::fs::read_dir(
            destination
                .parent()
                .expect("the destination should have a parent"),
        )
        .expect("the destination directory should be readable");
        for entry in entries {
            let name = entry
                .expect("the directory entry should be readable")
                .file_name();
            assert!(!name.to_string_lossy().contains(".tmp-"));
        }
    }

    #[test]
    fn sanitizing_replaces_each_forbidden_and_control_character_and_nothing_else() {
        assert_eq!(
            sanitize("a<b>c:d\"e/f\\g|h?i*j\u{0}k\n\u{1f}l\u{7f}m", usize::MAX),
            "a_b_c_d_e_f_g_h_i_j_k__l_m"
        );
        let untouched = "Ünïcode ウィンドウ .. \u{80} 🦀 - _";
        assert_eq!(sanitize(untouched, usize::MAX), untouched);
    }

    #[test]
    fn sanitizing_twice_changes_nothing() {
        let long = "あ<".repeat(40);
        for text in ["a<b>c", "\u{0}\u{7f}", "plain", long.as_str()] {
            let once = sanitize(text, 32);
            assert_eq!(sanitize(&once, 32), once);
        }
    }

    #[test]
    fn a_long_process_and_title_are_cut_at_a_character_boundary() {
        let id = ulid::Ulid::from(1u128);

        let relative = relative_path(
            id,
            at(2026, 7, 30).fixed_offset(),
            Some(&"あ".repeat(40)),
            Some(&"🦀".repeat(60)),
        );

        assert_eq!(
            relative,
            format!(
                "2026/07/30/123456_{}_{}_{id}.webp",
                "あ".repeat(32),
                "🦀".repeat(48)
            )
        );
    }

    #[test]
    fn the_name_carries_the_local_date_and_time_the_process_and_the_title() {
        let id = ulid::Ulid::from(1u128);
        let plus_nine = FixedOffset::east_opt(9 * 3600).expect("the test offset should be valid");
        let taken_at = Utc
            .with_ymd_and_hms(2026, 7, 30, 20, 34, 56)
            .single()
            .expect("the test timestamp should be valid");

        let relative = relative_path(
            id,
            taken_at.with_timezone(&plus_nine),
            Some("chrome.exe"),
            Some("Synthetic: window?"),
        );

        assert_eq!(
            relative,
            format!("2026/07/31/053456_chrome.exe_Synthetic_ window__{id}.webp")
        );
    }

    #[test]
    fn an_unknown_process_is_named_unknown_and_a_missing_or_empty_title_is_left_out() {
        let id = ulid::Ulid::from(1u128);
        let taken_at = at(2026, 7, 30).fixed_offset();

        for (process, title) in [(None, None), (Some(""), Some("")), (None, Some(""))] {
            assert_eq!(
                relative_path(id, taken_at, process, title),
                format!("2026/07/30/123456_unknown_{id}.webp")
            );
        }
        assert_eq!(
            relative_path(id, taken_at, None, Some("Synthetic window")),
            format!("2026/07/30/123456_unknown_Synthetic window_{id}.webp")
        );
        assert_eq!(
            relative_path(id, taken_at, Some("code.exe"), None),
            format!("2026/07/30/123456_code.exe_{id}.webp")
        );
    }

    #[test]
    fn a_legacy_row_and_a_labelled_row_are_both_accepted() {
        let id = ulid::Ulid::generate();
        let created_at = at(2026, 7, 30);
        let plus_nine = FixedOffset::east_opt(9 * 3600).expect("the test offset should be valid");

        for stored in [
            legacy_path(id, created_at),
            relative_path(id, created_at.with_timezone(&plus_nine), None, None),
            relative_path(
                id,
                (created_at + TimeDelta::days(1)).fixed_offset(),
                Some(&"p".repeat(40)),
                Some(&"t".repeat(60)),
            ),
            relative_path(
                id,
                created_at.fixed_offset(),
                Some(&"あ".repeat(40)),
                Some(&"🦀".repeat(60)),
            ),
        ] {
            assert_eq!(checked_path(id, created_at, &stored).ok(), Some(stored));
        }
    }

    #[test]
    fn a_row_not_bound_to_its_id_or_not_confined_to_its_day_is_refused() {
        let id = ulid::Ulid::generate();
        let other = ulid::Ulid::generate();
        let created_at = at(2026, 7, 30);

        for stored in [
            format!("2026/07/30/123456_chrome.exe_{other}.webp"),
            format!(
                "2026/07/30/123456_chrome.exe_{}.webp",
                id.to_string().to_lowercase()
            ),
            format!("2026/07/30/123456_../../../outside_{id}.webp"),
            format!("2026/07/30/123456_..\\..\\..\\outside_{id}.webp"),
            format!("2026/07/30/123456_chrome.exe_{id}.png"),
            format!("2026/07/30/123456_chrome.exe_{id}.webp.tmp"),
            format!("2026/07/30/123456__{id}.webp"),
            format!("2026/07/30/123456_a:b_{id}.webp"),
            format!("2026/07/30/123456_a\u{7}b_{id}.webp"),
            format!("2026/07/30/123456_{}_{id}.webp", "x".repeat(82)),
            format!("2026/07/30/12345_chrome.exe_{id}.webp"),
            format!("2026/7/30/123456_chrome.exe_{id}.webp"),
            format!("../2026/07/30/123456_chrome.exe_{id}.webp"),
            format!("2026/../../123456_chrome.exe_{id}.webp"),
            format!("2026\\07\\30\\123456_chrome.exe_{id}.webp"),
            legacy_path(id, created_at + TimeDelta::days(1)),
        ] {
            assert!(
                matches!(
                    checked_path(id, created_at, &stored),
                    Err(StoreError::Encoding { .. })
                ),
                "{stored}"
            );
        }
    }

    #[test]
    fn a_name_the_readers_would_refuse_is_refused_before_anything_is_written() {
        let (dir, mut conn, root) = database();
        let id = ulid::Ulid::generate();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);

        for relative in [
            name(ulid::Ulid::generate(), taken_at),
            format!("../2026/07/30/123456_escaped_{id}.webp"),
        ] {
            let error = save(
                &mut conn,
                &root,
                id,
                &relative,
                &pixels(60),
                WIDTH,
                HEIGHT,
                75.0,
                taken_at,
            )
            .expect_err("the name should be refused");
            assert!(matches!(error, StoreError::Encoding { .. }), "{error:?}");
        }
        assert!(!root.exists());
        assert!(!dir.path().join("2026").exists());
    }

    #[test]
    fn a_legacy_row_and_its_file_are_still_deleted() {
        let (_dir, mut conn, root) = database();
        let relative = register_row(&conn, 1, at(2026, 7, 30));
        let path = root.join(&relative);
        std::fs::create_dir_all(
            path.parent()
                .expect("the legacy image should have a parent"),
        )
        .expect("the legacy image directory should be creatable");
        std::fs::write(&path, b"a legacy image").expect("the legacy image should be writable");

        let outcome =
            delete(&mut conn, &root, ulid::Ulid::from(1u128)).expect("the legacy row should go");

        assert_eq!(outcome, DeleteOutcome::Removed);
        assert!(!path.exists());
    }
}
