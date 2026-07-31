//! WebP image files and the database rows that record them.

use crate::{StoreError, timestamp};
use chrono::Datelike;
use std::collections::HashSet;

const INSERT_IMAGE: &str = "INSERT INTO images \
     (observation_id, relative_path, byte_size, created_at) VALUES (?1, ?2, ?3, ?4)";
const COUNT_IMAGE_BY_ID: &str = "SELECT count(*) FROM images WHERE observation_id = ?1";
const SELECT_PATH_BY_ID: &str =
    "SELECT observation_id, relative_path, created_at FROM images WHERE observation_id = ?1";
const DELETE_IMAGE: &str = "DELETE FROM images WHERE observation_id = ?1";
// The report `orphan_rows` produces is read by a person and its order is the order the pictures were
// taken; the sweep collects into a set and pays for a sort no index serves.
const SELECT_IMAGE_ROWS: &str = "SELECT observation_id, relative_path, created_at FROM images ORDER BY created_at, observation_id";
const SELECT_IMAGE_PATHS: &str = "SELECT observation_id, relative_path, created_at FROM images";

/// Where an image for `id` taken at `at` is filed, relative to the image root.
///
/// Forward slashes on every platform. This string is a UNIQUE key that a sweep compares against
/// names read off the filesystem and that retention reads back later; if two callers spelled the
/// same file two ways, the constraint would let both exist and each would be invisible to the
/// other's lookup. Joining it onto a root with `Path::join` handles the separator when a real
/// path is needed.
fn relative_path(id: ulid::Ulid, at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "{:04}/{:02}/{:02}/{id}.webp",
        at.year(),
        at.month(),
        at.day()
    )
}

/// Encode `pixels` as WebP, register it, and put the file in place.
///
/// The file is written to a temporary name and flushed before a transaction registers it, renames
/// it into place, flushes the rename and commits. A crash before the commit leaves at most a file
/// [`sweep_orphan_files`] removes; a crash after it leaves nothing to clean.
#[allow(clippy::too_many_arguments)]
pub fn save(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    id: ulid::Ulid,
    pixels: &[u8],
    width: u32,
    height: u32,
    quality: f32,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<String, StoreError> {
    let id_text = id.to_string();
    // 16,383 is the encoder's dimension limit, not one imposed by this program.
    if !(1..=16_383).contains(&width)
        || !(1..=16_383).contains(&height)
        || !(0.0..=100.0).contains(&quality)
    {
        return Err(StoreError::Encode { id: id_text });
    }

    let expected_len = u64::from(width) * u64::from(height) * 3;
    let actual_len = u64::try_from(pixels.len()).map_err(|_| StoreError::Encode {
        id: id_text.clone(),
    })?;
    if actual_len != expected_len {
        return Err(StoreError::Encode { id: id_text });
    }

    let created_at = timestamp::to_sql(at)?;
    let relative = relative_path(id, at);
    // `encode` would unwrap this error and panic on the capture path.
    let encoded = webp::Encoder::from_rgb(pixels, width, height)
        .encode_simple(false, quality)
        .map_err(|_| StoreError::Encode {
            id: id_text.clone(),
        })?;
    let byte_size = i64::try_from(encoded.len()).map_err(|_| StoreError::Encode {
        id: id_text.clone(),
    })?;

    let destination = root.join(&relative);
    let parent = destination
        .parent()
        .expect("an image path with date directories always has a parent");
    std::fs::create_dir_all(parent).map_err(|source| StoreError::ImageIo {
        path: parent.to_path_buf(),
        source,
    })?;

    let (temporary, mut file) = cw_core::atomic_file::create_temporary_beside(&destination)
        .map_err(|source| StoreError::ImageIo {
            path: destination.clone(),
            source,
        })?;
    let write_result =
        std::io::Write::write_all(&mut file, &encoded).and_then(|()| file.sync_all());
    if let Err(source) = write_result {
        // The handle holds the DELETE right, so unlinking while it is open leaves no moment in
        // which another opener can keep the name.
        remove_image_file(&temporary)?;
        drop(file);
        return Err(StoreError::ImageIo {
            path: temporary,
            source,
        });
    }

    // The INSERT decides a conflict before anything is published. The handle stays open through
    // the commit so the destination remains removable while anything can still fail. A crash
    // before the commit leaves at most an unregistered file, which is what the sweep exists for.
    // `delete` takes the same IMMEDIATE lock, so no two decisions about this observation's row can
    // be made at once. Its file removal happens after its commit and cannot take this file: it
    // removes nothing unless the name was occupied while it still held the lock, and while the name
    // is occupied this rename cannot have put anything there.
    let transaction = match conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
    {
        Ok(transaction) => transaction,
        Err(source) => {
            remove_image_file(&temporary)?;
            return Err(StoreError::Sql { source });
        }
    };

    if let Err(source) = transaction.execute(
        INSERT_IMAGE,
        rusqlite::params![id_text, relative, byte_size, created_at],
    ) {
        // Measured 2026-07-31: an identical retry violates both indexes and SQLite reports the path
        // index, `SQLITE_CONSTRAINT_UNIQUE` — and so does another observation's row already holding
        // this path, which is a database that disagrees with itself rather than a retry. The code
        // alone cannot separate them, so the row is asked for instead; a constraint violation aborts
        // the statement and leaves the transaction usable. A count that cannot be taken leaves the
        // original error to speak for itself, because nothing has established that anything is
        // registered.
        let already_registered = matches!(
            transaction.query_one(COUNT_IMAGE_BY_ID, [id_text.as_str()], |row| row
                .get::<_, i64>(0)),
            Ok(1..)
        );
        remove_image_file(&temporary)?;
        if already_registered {
            return Err(StoreError::ImageAlreadyRegistered { id: id_text });
        }
        return Err(StoreError::Sql { source });
    }

    match cw_core::atomic_file::rename_without_replacing(&file, &destination) {
        Ok(true) => {}
        Ok(false) => {
            remove_image_file(&temporary)?;
            // Not `ImageAlreadyRegistered`: the insert above has already succeeded, so this
            // observation has no row. Whatever holds the name is unregistered — the file a
            // crash between this rename and the commit leaves behind, or something this
            // program did not write — and saying the database already knows about it would
            // point recovery the wrong way.
            return Err(StoreError::ImageIo {
                path: destination,
                source: std::io::Error::from(std::io::ErrorKind::AlreadyExists),
            });
        }
        Err(source) => {
            remove_image_file(&temporary)?;
            return Err(StoreError::ImageIo {
                path: destination,
                source,
            });
        }
    }

    // The rename is a metadata change on this handle, and Windows buffers those; closing the
    // handle does not push them.
    if let Err(source) = file.sync_all() {
        remove_image_file(&destination)?;
        return Err(StoreError::ImageIo {
            path: destination,
            source,
        });
    }

    if let Err(source) = transaction.commit() {
        remove_image_file(&destination)?;
        return Err(StoreError::Sql { source });
    }
    drop(file);

    Ok(relative)
}

/// Remove an image and the record that it existed.
///
/// This is an explicit request for one image, so it removes the row even when the file has already
/// gone: the rule that a row without its file is reported and kept binds [`orphan_rows`] and
/// [`sweep_orphan_files`], which run on their own and must never decide a picture is expendable.
/// The observation, its OCR text and its payload are untouched. Nothing records that the image
/// existed once this row is gone, and that is the point: retention removes an image to reclaim
/// space, and a row kept for a file that is gone would keep charging its `byte_size` against a
/// budget that is already free.
///
/// The row is committed before the file is removed, so a failure in between leaves an unregistered
/// file for the next sweep rather than a row whose file is gone. That is the residue worth having:
/// the failure this has to survive is a full disk during retention, and a row kept for a file that
/// is gone would charge its `byte_size` against a budget that is already free.
///
/// Whether there is a file to remove is decided while the transaction still holds the lock. If the
/// name is occupied, a `save` for the same observation racing this call finds it held and fails
/// with [`StoreError::ImageIo`], because the rename refuses to replace. What that guarantees is only
/// that no save can take the name before the removal is through; one arriving afterwards succeeds
/// the first time. If the name is empty — a row whose file has already gone — nothing is
/// removed at all, so a save that follows this commit keeps the file it renames into that name.
///
/// Leaves the observation row alone: the OCR text and the payload are the point of the record and
/// outlive the picture. Empty day directories are left behind on purpose: pruning one could race
/// with [`save`] between creating that directory and opening its temporary file, while a few
/// hundred empty entries a year cost nothing and only the startup sweep walks them.
pub fn delete(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    id: ulid::Ulid,
) -> Result<(), StoreError> {
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
        return Ok(());
    };
    let path = root.join(relative);
    // Decided while the transaction still holds the lock, because after the commit this name stops
    // being this call's business. A row whose file is already gone is a state this store tolerates,
    // and there the whole of the work left is nothing: a `save` for this observation that follows
    // the commit renames its new file into this very name, and a removal running afterwards would
    // take it away while its fresh row said it was there. When the name is occupied, no save can
    // reach it first — the rename refuses to replace, so it fails until this removal is through.
    // A name that cannot even be asked about counts as occupied, so the removal below reports the
    // real error rather than this line inventing one.
    // The question is whether a directory entry exists under this name, which is the question
    // `rename_without_replacing` answers, and not whether anything can be read through it.
    // Measured 2026-07-31: for a symlink whose target is gone, `try_exists` reports the name as
    // empty while `create_new` on it still fails with `AlreadyExists` and `remove_file` still has
    // an entry to remove — so following the link would leave that entry standing in the way of
    // every later save for this observation — and not until the next startup either, because the
    // sweep collects only entries the filesystem calls files and a link is not one.
    let occupied = match path.symlink_metadata() {
        Ok(_) => true,
        Err(source) => source.kind() != std::io::ErrorKind::NotFound,
    };

    transaction
        .execute(DELETE_IMAGE, [id.to_string()])
        .map_err(|source| StoreError::Sql { source })?;
    transaction
        .commit()
        .map_err(|source| StoreError::Sql { source })?;

    // A file removal cannot be rolled back, so the two media cannot commit together and one of them
    // is left over on failure. A leftover file is unregistered and the next sweep takes it; a
    // leftover row is one `orphan_rows` reports and nothing removes, and retention would keep
    // charging its `byte_size` against a disk budget that is already free.
    if occupied {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            // Nothing left to remove is the outcome this was asked for.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(StoreError::ImageIo { path, source }),
        }
    }

    Ok(())
}

/// A file is removed only when nothing registered names it and the filesystem does not report it as
/// one of the registered files under another spelling. Reports how many went.
///
/// This is a startup operation and must not run while anything is saving: a file renamed into place
/// but not yet registered is indistinguishable from an orphan.
pub fn sweep_orphan_files(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
) -> Result<usize, StoreError> {
    let registered = registered_paths(conn)?;
    let mut files = Vec::new();
    collect_files(root, &mut files)?;
    sweep_collected_files(root, &registered, &files)
}

fn sweep_collected_files(
    root: &std::path::Path,
    registered: &HashSet<String>,
    files: &[std::path::PathBuf],
) -> Result<usize, StoreError> {
    // Whether two spellings name one file is the filesystem's rule, not this program's. A byte
    // comparison deletes a registered image on the case-insensitive directory this normally runs
    // on, and case is not the only way one file answers to two names: a registered name can be a
    // link to a file this loop enumerates under the name it really has, and that file is what the
    // picture is. So the question asked of every candidate is not how it is spelled but which file
    // it reaches, and it is asked against every registered name that no enumerated file spelled —
    // the rows whose file, if it is there at all, is under some other name. On the ordinary
    // directory nothing reaches this point, because every enumerated file is registered under the
    // name it was enumerated with. Measured 2026-07-30, `canonicalize` answers with the name
    // actually on disk, so two spellings of one file agree. A candidate whose identity cannot be
    // established is kept: leaving a leftover costs disk, and removing a registered image costs the
    // picture. Asking only about the names no enumerated file spelled is a cost decision and it
    // leaves something out — a registered name that was an ordinary file when it was enumerated and
    // has become a link by the time this runs is not asked about, so the file it now reaches can be
    // taken as an orphan. Asking about every registered name instead is what that would cost:
    // measured 2026-08-01 on this machine, `canonicalize` takes 169 µs and `symlink_metadata`
    // 107 µs, which is seventeen seconds of startup at a hundred thousand images, paid whenever
    // anything unregistered is under the root at all.
    let mut enumerated = Vec::with_capacity(files.len());
    for path in files {
        let relative = path_relative_to_root(root, path)?;
        enumerated.push((path, relative));
    }
    if enumerated
        .iter()
        .all(|(_, relative)| registered.contains(relative))
    {
        return Ok(0);
    }

    // Every name here is resolved once to enumerate it and again to act on it, and a directory
    // above it can be replaced in between: Windows follows a reparse point met partway along a
    // path. No spelling protects against that. `checked_path` keeps a stored path from naming
    // anything outside the image root, which is a statement about the string and not about where
    // the string leads, so the only thing worth checking is what the filesystem answers with. A
    // root that will not resolve leaves nothing that can be shown to be inside it.
    let Ok(canonical_root) = std::fs::canonicalize(root) else {
        return Ok(0);
    };

    let spelled: HashSet<&str> = enumerated
        .iter()
        .map(|(_, relative)| relative.as_str())
        .collect();
    let mut unspelled_identities = HashSet::new();
    for unspelled in registered
        .iter()
        .filter(|registered| !spelled.contains(registered.as_str()))
    {
        match std::fs::canonicalize(root.join(unspelled)) {
            Ok(identity) => {
                unspelled_identities.insert(identity);
            }
            // Nothing under the name, so no enumerated file can be what it names. This is the
            // ordinary state of a row whose file is gone, which the sweep must not let stop it.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            // The name is there and will not say which file it reaches. Any candidate might be
            // that file, so this pass has nothing it can safely remove. Measured 2026-08-01: a
            // symlink pointing at itself answers `FilesystemLoop` here while its entry exists.
            Err(_) => return Ok(0),
        }
    }
    let mut removed = 0;

    for (path, relative) in &enumerated {
        if registered.contains(relative) {
            continue;
        }
        let Ok(identity) = std::fs::canonicalize(path) else {
            continue;
        };
        if !identity.starts_with(&canonical_root) || unspelled_identities.contains(&identity) {
            continue;
        }

        // Addressed to the spelling that was checked, not to the one that was enumerated.
        match std::fs::remove_file(&identity) {
            Ok(()) => removed += 1,
            // Two startups can collect the same orphan. The one that loses the removal race
            // has nothing left to do, rather than a reason to abort.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StoreError::ImageIo {
                    path: identity,
                    source,
                });
            }
        }
    }

    Ok(removed)
}

/// Report rows whose file is gone. Nothing is deleted.
pub fn orphan_rows(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
) -> Result<Vec<String>, StoreError> {
    let mut statement = conn
        .prepare(SELECT_IMAGE_ROWS)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query([])
        .map_err(|source| StoreError::Sql { source })?;
    let mut orphaned = Vec::new();

    while let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? {
        let relative = image_path_from_row(row)?;
        match std::fs::metadata(root.join(&relative)) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => orphaned.push(relative),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                orphaned.push(relative);
            }
            Err(source) => {
                return Err(StoreError::ImageIo {
                    path: root.join(relative),
                    source,
                });
            }
        }
    }

    Ok(orphaned)
}

fn remove_image_file(path: &std::path::Path) -> Result<(), StoreError> {
    std::fs::remove_file(path).map_err(|source| StoreError::ImageIo {
        path: path.to_path_buf(),
        source,
    })
}

fn registered_paths(conn: &rusqlite::Connection) -> Result<HashSet<String>, StoreError> {
    let mut statement = conn
        .prepare(SELECT_IMAGE_PATHS)
        .map_err(|source| StoreError::Sql { source })?;
    let mut rows = statement
        .query([])
        .map_err(|source| StoreError::Sql { source })?;
    let mut paths = HashSet::new();

    while let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? {
        paths.insert(image_path_from_row(row)?);
    }

    Ok(paths)
}

/// Walks with an explicit worklist. Recursion here would put one `ReadDir` per level on the stack —
/// on Windows each holds a `WIN32_FIND_DATAW` by value — and this walk reads whatever is under the
/// image root rather than only what this program wrote there, so its depth is not this program's
/// to assume. Running out of stack aborts the process, and this runs at startup.
fn collect_files(
    directory: &std::path::Path,
    files: &mut Vec<std::path::PathBuf>,
) -> Result<(), StoreError> {
    let mut worklist: Vec<std::path::PathBuf> = vec![directory.to_path_buf()];
    while let Some(directory) = worklist.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(StoreError::ImageIo {
                    path: directory,
                    source,
                });
            }
        };

        for entry in entries {
            let entry = entry.map_err(|source| StoreError::ImageIo {
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| StoreError::ImageIo {
                path: path.clone(),
                source,
            })?;
            if file_type.is_dir() {
                worklist.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }

    Ok(())
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

/// The path a row claims, checked against the one this program would have written for it.
///
/// `relative_path` is TEXT and the schema constrains nothing, so a row edited by hand or damaged
/// can name anything at all — including a path that climbs out of the image root, which `delete`
/// would then remove. The write side has had one spelling since this file was written; this is the
/// read side finally agreeing with it.
fn checked_path(
    id: ulid::Ulid,
    created_at: chrono::DateTime<chrono::Utc>,
    stored: &str,
) -> Result<String, StoreError> {
    if stored == relative_path(id, created_at) {
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
    // The observation and control stores have had this check since they were written, but this one
    // did not: a lower-cased id was accepted everywhere except by `delete`, which looked it up in
    // canonical spelling and silently matched nothing.
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
    use super::{delete, orphan_rows, save, sweep_collected_files, sweep_orphan_files};
    use crate::{StoreError, db, observations, timestamp};
    use chrono::{DateTime, TimeZone, Utc};
    use cw_core::model::{Observation, OcrStatus, ScreenPayload};
    use std::collections::HashSet;
    use tempfile::{TempDir, tempdir};

    const WIDTH: u32 = 4;
    const HEIGHT: u32 = 3;

    fn database() -> (TempDir, rusqlite::Connection, std::path::PathBuf) {
        let dir = tempdir().expect("the temporary image directory should be creatable");
        let conn =
            db::open(&dir.path().join("db.sqlite3")).expect("the fresh database should initialize");
        let root = dir.path().join("images");
        (dir, conn, root)
    }

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 12, 34, 56)
            .single()
            .expect("the test timestamp should be valid")
    }

    fn insert_observation(conn: &rusqlite::Connection, id: ulid::Ulid, observed_at: DateTime<Utc>) {
        let mut observation = Observation::new_screen(
            ScreenPayload {
                monitor_id: "synthetic-monitor".to_owned(),
                width: WIDTH,
                height: HEIGHT,
                image_path: None,
                ocr_status: OcrStatus::NoText,
                ocr_error: None,
                ocr_text: None,
                ocr_langs: vec!["en".to_owned()],
                foreground_process: None,
                foreground_window_title: None,
            },
            observed_at,
        );
        observation.id = id;
        observations::insert(conn, &observation).expect("the image's observation should be stored");
    }

    fn pixels(seed: u8) -> Vec<u8> {
        let len = usize::try_from(u64::from(WIDTH) * u64::from(HEIGHT) * 3)
            .expect("the synthetic frame should fit in memory");
        let rgb = [seed, seed.wrapping_add(73), seed.wrapping_add(149)];
        (0..len).map(|index| rgb[index % rgb.len()]).collect()
    }

    fn save_test_image(
        conn: &mut rusqlite::Connection,
        root: &std::path::Path,
        id: ulid::Ulid,
        taken_at: DateTime<Utc>,
        seed: u8,
    ) -> String {
        insert_observation(conn, id, taken_at);
        save(conn, root, id, &pixels(seed), WIDTH, HEIGHT, 75.0, taken_at)
            .expect("the synthetic image should be saved")
    }

    #[test]
    fn save_writes_webp_atomically_and_registers_in_images_table() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
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
        // Decoding here is a test reading back what this test just wrote; the program itself still
        // only encodes. Without this, handing the encoder its height and width the other way round
        // stores a transposed picture and every assertion above still holds.
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
    fn the_stored_path_is_the_same_string_on_every_platform() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::from(1u128);

        let stored = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 20);

        assert!(stored.contains('/'));
        assert!(!stored.contains('\\'));
        assert_eq!(stored, "2026/07/30/00000000000000000000000001.webp");
    }

    #[test]
    fn a_second_save_for_one_observation_is_refused_and_keeps_the_first() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        let relative = save_test_image(&mut conn, &root, id, taken_at, 30);
        let path = root.join(&relative);
        let first = std::fs::read(&path).expect("the first WebP should be readable");

        let error = save(
            &mut conn,
            &root,
            id,
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
        let first_id = ulid::Ulid::new();
        let second_id = ulid::Ulid::new();
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

        // An identical retry and this planted path share one extended error code; the ID count is
        // what separates them.
        let error = save(
            &mut conn,
            &root,
            first_id,
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
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let mut short = pixels(40);
        short.pop();

        let error = save(&mut conn, &root, id, &short, WIDTH, HEIGHT, 75.0, taken_at)
            .expect_err("the short frame should be refused");

        match error {
            StoreError::Encode { id: actual } => assert_eq!(actual, id.to_string()),
            other => panic!("expected Encode, got {other:?}"),
        }

        let mut long = pixels(40);
        long.push(0);
        // The encoder ignores trailing bytes, so only this side of the check catches a length test
        // weakened to accept a buffer that is merely large enough.
        let error = save(&mut conn, &root, id, &long, WIDTH, HEIGHT, 75.0, taken_at)
            .expect_err("the long frame should be refused");
        match error {
            StoreError::Encode { id: actual } => assert_eq!(actual, id.to_string()),
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
        let oversized_id = ulid::Ulid::new();
        let oversized = vec![0; 49_152];

        // This length passes the pixel check, so the dimension guard is what refuses it.
        // `encode_simple` returns an error rather than panicking, which is why it is called instead
        // of `encode`.
        let oversized_error = save(
            &mut conn,
            &root,
            oversized_id,
            &oversized,
            16_384,
            1,
            75.0,
            taken_at,
        )
        .expect_err("the frame above the encoder's dimension limit should be refused");
        assert!(matches!(oversized_error, StoreError::Encode { .. }));

        let zero_width_error = save(
            &mut conn,
            &root,
            ulid::Ulid::new(),
            &[],
            0,
            1,
            75.0,
            taken_at,
        )
        .expect_err("a zero-width frame should be refused");
        assert!(matches!(zero_width_error, StoreError::Encode { .. }));

        let quality_error = save(
            &mut conn,
            &root,
            ulid::Ulid::new(),
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
        let zero_id = ulid::Ulid::new();
        let hundred_id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, zero_id, taken_at);
        insert_observation(&conn, hundred_id, taken_at);

        // The configuration allows the whole range, so the refusal of -1 says nothing about where
        // the accepted range actually ends.
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
            &mut conn, &root, zero_id, &detailed, DETAILED, DETAILED, 0.0, taken_at,
        )
        .expect("quality zero should be accepted");
        save(
            &mut conn, &root, hundred_id, &detailed, DETAILED, DETAILED, 100.0, taken_at,
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
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let pixels = vec![0_u8; 49_149];

        // The refusal case above cannot pin this bound: a frame one pixel wider fails the encoder
        // as well as the guard. Only this accepted side distinguishes the correct limit from one
        // narrowed by a pixel.
        save(&mut conn, &root, id, &pixels, 16_383, 1, 75.0, taken_at)
            .expect("a frame at the encoder's dimension limit should be accepted");

        let second_id = ulid::Ulid::new();
        insert_observation(&conn, second_id, taken_at);
        // Width and height are separate bounds, so a test of one says nothing about the other.
        save(
            &mut conn, &root, second_id, &pixels, 1, 16_383, 75.0, taken_at,
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

        let error = save(
            &mut conn,
            &root,
            ulid::Ulid::new(),
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
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 50);
        let path = root.join(relative);

        delete(&mut conn, &root, id).expect("the image should be deleted");

        assert!(!path.exists());
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
    fn delete_leaves_the_observation_row_untouched() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        save_test_image(&mut conn, &root, id, at(2026, 7, 30), 60);

        delete(&mut conn, &root, id).expect("the image should be deleted");

        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM observations WHERE id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the observation count should be readable");
        assert_eq!(count, 1);
    }

    #[test]
    fn deleting_a_row_whose_file_is_already_gone_still_removes_the_row() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 51);
        std::fs::remove_file(root.join(relative))
            .expect("the saved file should be removable without touching its row");

        // This is an explicit request rather than one of the automatic paths. Keeping such a row
        // is `orphan_rows`' rule, not this one's.
        delete(&mut conn, &root, id).expect("the explicit deletion should succeed");

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
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 53);
        let path = root.join(relative);
        let missing_target = path.with_file_name("missing-target.webp");
        std::fs::remove_file(&path)
            .expect("the saved file should be removable before replacement with a symlink");

        // Creating a symlink needs Developer Mode or SeCreateSymbolicLinkPrivilege, so this case
        // cannot be built everywhere the suite runs. It was measured on the development machine,
        // and the predicate it pins is justified there independently: `create_new` on such a name
        // fails with `AlreadyExists`, which is exactly what the publishing rename would meet.
        let Ok(()) = std::os::windows::fs::symlink_file(&missing_target, &path) else {
            return;
        };

        delete(&mut conn, &root, id).expect("the dangling symlink should be deleted");

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
    fn a_file_that_cannot_be_removed_leaves_no_row_behind() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 52);
        let path = root.join(relative);

        // Measured 2026-07-31 on this machine: `remove_file` against a directory fails with
        // `PermissionDenied` (raw OS error 5) and leaves it in place, while a missing file and a
        // missing parent directory both come back as `NotFound`. That is the deterministic
        // non-`NotFound` failure this needs, and the absent row is what tells this order apart from
        // removing the file first — that one returns before the row is ever touched.
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
        assert_eq!(count, 0);
        assert!(path.is_dir());
    }

    #[test]
    fn a_row_naming_a_path_this_program_would_not_write_is_refused() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let canonical = super::relative_path(id, taken_at);
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

        let errors = [
            delete(&mut conn, &root, id).expect_err("delete should refuse the malformed path"),
            sweep_orphan_files(&conn, &root)
                .expect_err("the sweep should refuse the malformed path"),
            orphan_rows(&conn, &root)
                .expect_err("the orphan report should refuse the malformed path"),
        ];

        for error in errors {
            match error {
                StoreError::Encoding { id: actual, .. } => assert_eq!(actual, id.to_string()),
                other => panic!("expected Encoding, got {other:?}"),
            }
        }
        assert!(path.is_file());
    }

    #[test]
    fn a_row_whose_timestamp_is_spelled_any_other_way_is_refused() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let canonical = super::relative_path(id, taken_at);
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

        let errors = [
            delete(&mut conn, &root, id)
                .expect_err("delete should refuse the non-canonical timestamp"),
            sweep_orphan_files(&conn, &root)
                .expect_err("the sweep should refuse the non-canonical timestamp"),
            orphan_rows(&conn, &root)
                .expect_err("the orphan report should refuse the non-canonical timestamp"),
        ];

        for error in errors {
            match error {
                StoreError::Encoding { id: actual, .. } => assert_eq!(actual, id.to_string()),
                other => panic!("expected Encoding, got {other:?}"),
            }
        }
        assert!(path.is_file());
    }

    #[test]
    fn a_row_whose_id_is_spelled_any_other_way_is_refused() {
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

        let relative = super::relative_path(id, taken_at);
        let path = root.join(&relative);
        std::fs::create_dir_all(
            path.parent()
                .expect("the hand-placed image should have a parent"),
        )
        .expect("the hand-placed image directory should be creatable");
        let contents = b"registered image";
        std::fs::write(&path, contents).expect("the hand-placed image should be writable");

        // This lower-cased spelling of a canonical ULID cannot be produced by this program, so
        // write the image row with plain SQL; every other column uses its canonical spelling.
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

        // `delete` answers the question asked: no row exists under the canonical id. The sweep is
        // what refuses this row; searching for equivalent spellings would put an unindexed scan on
        // the retention path.
        delete(&mut conn, &root, id).expect("the canonical id should have nothing to delete");

        let errors = [
            sweep_orphan_files(&conn, &root)
                .expect_err("the sweep should refuse the non-canonical id"),
            orphan_rows(&conn, &root)
                .expect_err("the orphan report should refuse the non-canonical id"),
        ];
        for error in errors {
            match error {
                StoreError::Encoding { id: actual, .. } => assert_eq!(actual, stored_id),
                other => panic!("expected Encoding for {stored_id}, got {other:?}"),
            }
        }
        assert!(path.is_file());
    }

    #[test]
    fn orphan_files_without_db_row_are_swept_on_startup() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
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

        let removed = sweep_orphan_files(&conn, &root).expect("the orphan sweep should succeed");

        assert_eq!(removed, 1);
        assert!(!unregistered.exists());
        assert!(saved.is_file());
        let count: i64 = conn
            .query_row("SELECT count(*) FROM images", [], |row| row.get(0))
            .expect("the image count should be readable");
        assert_eq!(count, 1);
    }

    #[test]
    fn an_unrelated_orphan_is_still_swept_when_a_registered_file_is_missing() {
        let (_dir, mut conn, root) = database();
        let first_id = ulid::Ulid::new();
        let second_id = ulid::Ulid::new();
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

        // A row whose file is missing must not make the sweep keep everything.
        let removed = sweep_orphan_files(&conn, &root).expect("the orphan sweep should succeed");

        assert_eq!(removed, 1);
        assert!(!left_behind.exists());
        assert!(root.join(second_relative).is_file());
        assert_eq!(
            orphan_rows(&conn, &root).expect("orphan rows should be reportable"),
            [first_relative]
        );
    }

    #[test]
    fn a_registered_image_spelled_in_another_case_is_not_swept() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::from_string("0000000000000128GGYHYYK08N")
            .expect("the fixed image id should parse");
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 81);
        let differently_spelled = relative.to_ascii_lowercase();
        std::fs::rename(root.join(&relative), root.join(&differently_spelled))
            .expect("the saved image should be renameable to another case");

        let removed = sweep_orphan_files(&conn, &root).expect("the orphan sweep should succeed");

        assert_eq!(removed, 0);
        let mut entries = std::fs::read_dir(
            root.join(&differently_spelled)
                .parent()
                .expect("the differently-spelled image should have a day directory"),
        )
        .expect("the day directory should remain readable");
        assert!(entries.any(|entry| {
            entry
                .expect("the day directory entry should be readable")
                .path()
                .is_file()
        }));
    }

    #[test]
    fn a_registered_image_reached_through_a_link_is_not_swept() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 31), 82);
        let path = root.join(relative);
        let moved = path.with_file_name("moved-by-something-else.webp");
        std::fs::rename(&path, &moved)
            .expect("the saved image should be movable under another name");

        // Creating a symlink needs Developer Mode or SeCreateSymbolicLinkPrivilege, so this case
        // cannot be built everywhere the suite runs.
        let Ok(()) = std::os::windows::fs::symlink_file(&moved, &path) else {
            return;
        };

        // Before this change the moved file was the only thing the sweep could see and it deleted
        // it, leaving the row pointing at a link to nothing.
        let removed = sweep_orphan_files(&conn, &root).expect("the orphan sweep should succeed");

        assert_eq!(removed, 0);
        assert!(moved.exists());
        assert!(
            std::fs::metadata(&path)
                .expect("the registered link should still reach the moved image")
                .is_file()
        );
    }

    #[test]
    fn an_orphan_is_kept_while_a_registered_name_will_not_say_what_it_reaches() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 85);
        let path = root.join(relative);
        std::fs::remove_file(&path)
            .expect("the saved file should be removable before replacement with a symlink");

        // Creating a symlink needs Developer Mode or SeCreateSymbolicLinkPrivilege, so this case
        // cannot be built everywhere the suite runs.
        let Ok(()) = std::os::windows::fs::symlink_file(&path, &path) else {
            return;
        };

        let left_behind = root.join("2026/07/31/left-behind.webp");
        std::fs::create_dir_all(
            left_behind
                .parent()
                .expect("the hand-placed file should have a parent"),
        )
        .expect("the hand-placed file directory should be creatable");
        std::fs::write(&left_behind, b"not registered")
            .expect("the hand-placed file should be writable");

        // The registered entry exists and answers `FilesystemLoop`, so nothing rules out that the
        // left-behind file is what that row names.
        let removed = sweep_orphan_files(&conn, &root).expect("the orphan sweep should succeed");

        assert_eq!(removed, 0);
        assert!(left_behind.is_file());
    }

    #[test]
    fn a_name_that_now_leads_outside_the_root_is_not_removed() {
        let (dir, _conn, root) = database();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("the outside directory should be creatable");
        let victim = outside.join("orphan.webp");
        std::fs::write(&victim, b"a file this program has no business touching")
            .expect("the outside file should be writable");
        std::fs::create_dir_all(&root).expect("the image root should be creatable");
        let redirected = root.join("2026");

        // Creating a directory link needs Developer Mode or SeCreateSymbolicLinkPrivilege, so this
        // case cannot be built everywhere the suite runs.
        let Ok(()) = std::os::windows::fs::symlink_dir(&outside, &redirected) else {
            return;
        };

        // The name as it was enumerated, before the directory above it became a link. Nothing here
        // needs a race: the link can be in place before the sweep starts.
        let enumerated = vec![redirected.join("orphan.webp")];
        let removed = super::sweep_collected_files(&root, &HashSet::new(), &enumerated)
            .expect("the sweep should succeed without removing anything");

        assert_eq!(removed, 0);
        assert!(victim.is_file());
    }

    #[test]
    fn a_sweep_whose_orphan_was_already_removed_still_succeeds() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 82);
        let saved = root.join(relative);
        let unregistered = root.join("2026").join("07").join("30").join("orphan.webp");
        std::fs::write(&unregistered, b"not registered")
            .expect("the hand-placed file should be writable");
        std::fs::remove_file(&unregistered)
            .expect("the hand-placed file should be removable before the sweep");

        assert_eq!(
            sweep_orphan_files(&conn, &root).expect("the orphan sweep should still succeed"),
            0
        );

        std::fs::write(&unregistered, b"not registered")
            .expect("the hand-placed file should be writable again");
        let registered =
            super::registered_paths(&conn).expect("the registered paths should be readable");
        let mut files = Vec::new();
        super::collect_files(&root, &mut files)
            .expect("the image tree should be collectable for both startups");

        assert_eq!(
            sweep_collected_files(&root, &registered, &files)
                .expect("the first startup sweep should succeed"),
            1
        );
        assert_eq!(
            sweep_collected_files(&root, &registered, &files)
                .expect("the second startup sweep should succeed"),
            0
        );
        assert!(saved.is_file());
    }

    #[test]
    fn orphan_rows_without_file_are_reported_and_kept() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 90);
        std::fs::remove_file(root.join(&relative))
            .expect("the saved file should be removable without touching its row");

        let rows = orphan_rows(&conn, &root).expect("orphan rows should be reportable");

        assert_eq!(rows, [relative]);
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the image count should be readable");
        assert_eq!(count, 1);
    }

    #[test]
    fn saving_again_over_a_row_whose_file_is_gone_leaves_the_row_and_no_new_file() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
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
        let mut files = Vec::new();
        super::collect_files(&root, &mut files)
            .expect("the image root should remain readable after the refusal");
        assert!(!files.iter().any(|candidate| {
            candidate
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".tmp-"))
        }));
    }

    #[test]
    fn a_destination_held_by_an_unregistered_file_is_not_reported_as_registered() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        let relative = save_test_image(&mut conn, &root, id, taken_at, 209);
        conn.execute(
            "DELETE FROM images WHERE observation_id = ?1",
            [id.to_string()],
        )
        .expect("the image row should be removable without touching its file");

        let error = save(
            &mut conn,
            &root,
            id,
            &pixels(210),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
        )
        .expect_err("the unregistered file should keep its destination");

        match error {
            StoreError::ImageIo { path, source } => {
                assert_eq!(path, root.join(relative));
                assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists);
            }
            other => panic!("expected ImageIo with AlreadyExists, got {other:?}"),
        }
        let count: i64 = conn
            .query_row("SELECT count(*) FROM images", [], |row| row.get(0))
            .expect("the image count should be readable");
        assert_eq!(count, 0);
    }

    #[test]
    fn a_failed_rename_leaves_no_temporary_behind() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let destination = root.join(super::relative_path(id, taken_at));
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

        // No row is what tells this transaction apart from three autocommitted statements.
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
}
