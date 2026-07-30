//! WebP image files and the database rows that record them.

use crate::{StoreError, timestamp};
use chrono::Datelike;
use std::collections::HashSet;

/// GENERIC_WRITE | DELETE. The DELETE right is what lets a handle rename its own file.
const RENAMABLE_WRITE_ACCESS: u32 = 0x4000_0000 | 0x0001_0000;
/// FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
const TEMPORARY_SHARE_MODE: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;

const INSERT_IMAGE: &str = "INSERT INTO images \
     (observation_id, relative_path, byte_size, created_at) VALUES (?1, ?2, ?3, ?4)";
const SELECT_PATH_BY_ID: &str =
    "SELECT observation_id, relative_path, created_at FROM images WHERE observation_id = ?1";
const DELETE_IMAGE: &str = "DELETE FROM images WHERE observation_id = ?1";
const SELECT_IMAGE_ROWS: &str = "SELECT observation_id, relative_path, created_at FROM images ORDER BY created_at, observation_id";

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

/// Encode `pixels` as WebP, put the file in place, and record that it exists.
///
/// The file is written to a temporary name, flushed, renamed into place, and only then registered.
/// A crash before the registration leaves a file [`sweep_orphan_files`] removes; a crash after it
/// leaves nothing to clean.
#[allow(clippy::too_many_arguments)]
pub fn save(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    id: ulid::Ulid,
    pixels: &[u8],
    width: u32,
    height: u32,
    quality: f32,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<String, StoreError> {
    use std::os::windows::fs::OpenOptionsExt;

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
    std::fs::create_dir_all(parent).map_err(|source| StoreError::ImageWrite {
        path: parent.to_path_buf(),
        source,
    })?;

    let temporary = cw_core::atomic_file::temporary_path_beside(&destination);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .access_mode(RENAMABLE_WRITE_ACCESS)
        .share_mode(TEMPORARY_SHARE_MODE)
        .open(&temporary)
        .map_err(|source| StoreError::ImageWrite {
            path: temporary.clone(),
            source,
        })?;
    let write_result =
        std::io::Write::write_all(&mut file, &encoded).and_then(|()| file.sync_all());
    if let Err(source) = write_result {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(StoreError::ImageWrite {
            path: temporary,
            source,
        });
    }

    match cw_core::atomic_file::rename_without_replacing(&file, &destination) {
        Ok(true) => {
            // The rename is a metadata change on this handle, and Windows buffers those; closing the
            // handle does not push them. Without this the row can commit while the name it points at
            // has not reached the disk, which is the one direction the write order exists to rule out.
            if let Err(source) = file.sync_all() {
                let _ = std::fs::remove_file(&destination);
                drop(file);
                return Err(StoreError::ImageWrite {
                    path: destination,
                    source,
                });
            }
            drop(file);
        }
        Ok(false) => {
            drop(file);
            remove_temporary(&temporary)?;
            // A taken name means the same observation is being saved twice or two ULIDs collided.
            // Either way, the second write must not overwrite the first: the row already written
            // records a byte_size that would no longer describe the file.
            return Err(StoreError::ImageExists {
                id: id_text,
                path: destination,
            });
        }
        Err(source) => {
            drop(file);
            remove_temporary(&temporary)?;
            return Err(StoreError::ImageWrite {
                path: destination,
                source,
            });
        }
    }

    if let Err(source) = conn.execute(
        INSERT_IMAGE,
        rusqlite::params![id_text, relative, byte_size, created_at],
    ) {
        let already_registered = matches!(
            source.sqlite_extended_error_code(),
            Some(rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY)
                | Some(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)
        );
        // Both unique indexes on this table carry the same fact: `observation_id` is the primary
        // key and `relative_path` is derived from it, so one observation cannot collide with
        // another's path and either violation means this observation already has an image.
        // Measured 2026-07-30, a retry with the same id and instant reports the path index (2067),
        // not the primary key (1555), so keying on the primary key alone never fired.
        let _ = std::fs::remove_file(&destination);
        if already_registered {
            return Err(StoreError::ImageExists {
                id: id_text,
                path: destination,
            });
        }
        return Err(StoreError::Sql { source });
    }

    Ok(relative)
}

/// Remove an image and the record that it existed.
///
/// Leaves the observation row alone: the OCR text and the payload are the point of the record and
/// outlive the picture. Empty day directories are left behind on purpose: pruning one could race
/// with [`save`] between creating that directory and opening its temporary file, while a few
/// hundred empty entries a year cost nothing and only the startup sweep walks them.
pub fn delete(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    id: ulid::Ulid,
) -> Result<(), StoreError> {
    let relative = {
        let mut statement = conn
            .prepare(SELECT_PATH_BY_ID)
            .map_err(|source| StoreError::Sql { source })?;
        let mut rows = statement
            .query([id.to_string()])
            .map_err(|source| StoreError::Sql { source })?;
        let Some(row) = rows.next().map_err(|source| StoreError::Sql { source })? else {
            return Ok(());
        };
        image_path_from_row(row)?
    };
    let path = root.join(relative);

    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(StoreError::ImageWrite {
                path: path.clone(),
                source,
            });
        }
    }

    conn.execute(DELETE_IMAGE, [id.to_string()])
        .map_err(|source| StoreError::Sql { source })?;

    Ok(())
}

/// Remove image files no row knows about, and report how many went.
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
    let mut removed = 0;

    for path in files {
        let relative = path_relative_to_root(root, &path)?;
        if !registered.contains(&relative) {
            std::fs::remove_file(&path).map_err(|source| StoreError::ImageWrite {
                path: path.clone(),
                source,
            })?;
            removed += 1;
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
                return Err(StoreError::ImageWrite {
                    path: root.join(relative),
                    source,
                });
            }
        }
    }

    Ok(orphaned)
}

fn remove_temporary(path: &std::path::Path) -> Result<(), StoreError> {
    std::fs::remove_file(path).map_err(|source| StoreError::ImageWrite {
        path: path.to_path_buf(),
        source,
    })
}

fn registered_paths(conn: &rusqlite::Connection) -> Result<HashSet<String>, StoreError> {
    let mut statement = conn
        .prepare(SELECT_IMAGE_ROWS)
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

fn collect_files(
    directory: &std::path::Path,
    files: &mut Vec<std::path::PathBuf>,
) -> Result<(), StoreError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(StoreError::ImageWrite {
                path: directory.to_path_buf(),
                source,
            });
        }
    };

    for entry in entries {
        let entry = entry.map_err(|source| StoreError::ImageWrite {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| StoreError::ImageWrite {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() {
            collect_files(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
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
        .map_err(|source| StoreError::ImageWrite {
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
    let relative: String = row.get(1)?;
    let created_at: String = row.get(2)?;
    let created_at = timestamp::from_sql(&created_at)?;
    Ok((id, relative, created_at))
}

#[cfg(test)]
mod tests {
    use super::{delete, orphan_rows, save, sweep_orphan_files};
    use crate::{StoreError, db, observations, timestamp};
    use chrono::{DateTime, TimeZone, Utc};
    use cw_core::model::{Observation, OcrStatus, ScreenPayload};
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
        conn: &rusqlite::Connection,
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
        let (_dir, conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);

        let relative = save_test_image(&conn, &root, id, taken_at, 10);
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
        let (_dir, conn, root) = database();
        let id = ulid::Ulid::from(1u128);

        let stored = save_test_image(&conn, &root, id, at(2026, 7, 30), 20);

        assert!(stored.contains('/'));
        assert!(!stored.contains('\\'));
        assert_eq!(stored, "2026/07/30/00000000000000000000000001.webp");
    }

    #[test]
    fn a_second_save_for_one_observation_is_refused_and_keeps_the_first() {
        let (_dir, conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        let relative = save_test_image(&conn, &root, id, taken_at, 30);
        let path = root.join(&relative);
        let first = std::fs::read(&path).expect("the first WebP should be readable");

        let error = save(
            &conn,
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
            StoreError::ImageExists {
                id: actual,
                path: actual_path,
            } => {
                assert_eq!(actual, id.to_string());
                assert_eq!(actual_path, path);
            }
            other => panic!("expected ImageExists, got {other:?}"),
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
    fn a_frame_whose_pixels_do_not_match_its_size_is_refused() {
        let (_dir, conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        insert_observation(&conn, id, taken_at);
        let mut short = pixels(40);
        short.pop();

        let error = save(&conn, &root, id, &short, WIDTH, HEIGHT, 75.0, taken_at)
            .expect_err("the short frame should be refused");

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
        let (_dir, conn, root) = database();
        let taken_at = at(2026, 7, 30);
        let oversized_id = ulid::Ulid::new();
        let oversized = vec![0; 49_152];

        // This length passes the pixel check and would reach the encoder's `unwrap`.
        let oversized_error = save(
            &conn,
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

        let zero_width_error = save(&conn, &root, ulid::Ulid::new(), &[], 0, 1, 75.0, taken_at)
            .expect_err("a zero-width frame should be refused");
        assert!(matches!(zero_width_error, StoreError::Encode { .. }));

        let quality_error = save(
            &conn,
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
    fn a_timestamp_this_schema_cannot_store_is_refused_before_anything_is_written() {
        let (_dir, conn, root) = database();

        let error = save(
            &conn,
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
        let (_dir, conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&conn, &root, id, at(2026, 7, 30), 50);
        let path = root.join(relative);

        delete(&conn, &root, id).expect("the image should be deleted");

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
        let (_dir, conn, root) = database();
        let id = ulid::Ulid::new();
        save_test_image(&conn, &root, id, at(2026, 7, 30), 60);

        delete(&conn, &root, id).expect("the image should be deleted");

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
    fn a_row_naming_a_path_this_program_would_not_write_is_refused() {
        let (_dir, conn, root) = database();
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
            delete(&conn, &root, id).expect_err("delete should refuse the malformed path"),
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
    fn orphan_files_without_db_row_are_swept_on_startup() {
        let (_dir, conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&conn, &root, id, at(2026, 7, 30), 80);
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
    fn orphan_rows_without_file_are_reported_and_kept() {
        let (_dir, conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&conn, &root, id, at(2026, 7, 30), 90);
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
        let (_dir, conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        let relative = save_test_image(&conn, &root, id, taken_at, 91);
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
            &conn,
            &root,
            id,
            &pixels(201),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
        )
        .expect_err("the existing image row should refuse another file");

        assert!(matches!(error, StoreError::ImageExists { .. }));
        let stored_byte_size: i64 = conn
            .query_row(
                "SELECT byte_size FROM images WHERE observation_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .expect("the original byte size should remain readable");
        assert_eq!(stored_byte_size, original_byte_size);
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_rename_leaves_no_temporary_behind() {
        let (_dir, conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        let relative = save_test_image(&conn, &root, id, taken_at, 100);
        let destination = root.join(relative);

        let error = save(
            &conn,
            &root,
            id,
            &pixels(210),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
        )
        .expect_err("the second image should be refused");
        assert!(matches!(error, StoreError::ImageExists { .. }));

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
