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
// The report `orphan_rows` produces is read by a person and its order is the order the pictures
// were taken. The sweep collects into a set, so it asks without an `ORDER BY` at all rather than
// paying for a sort no index serves.
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

    let (_temporary, mut file) = cw_core::atomic_file::create_temporary_beside(&destination)
        .map_err(|source| StoreError::ImageIo {
            path: destination.clone(),
            source,
        })?;
    let write_result =
        std::io::Write::write_all(&mut file, &encoded).and_then(|()| file.sync_all());
    if let Err(source) = write_result {
        // Addressed to the handle and not to a name: nothing else in this function needs to know
        // what the temporary was called.
        discard_written_file(&file, &destination)?;
        drop(file);
        return Err(StoreError::ImageIo {
            path: destination,
            source,
        });
    }

    // The INSERT decides a conflict before anything is published. The handle stays open through
    // the commit so the destination remains removable while anything can still fail. A crash
    // before the commit leaves at most an unregistered file, which is what the sweep exists for.
    // `delete` takes the same IMMEDIATE lock, so no two decisions about this observation's row can
    // be made at once. A `delete` whose transaction ran before this one cannot take this file: it
    // removes nothing unless the name was occupied while it still held the lock, and while the name
    // is occupied this rename cannot have put anything there. One that takes the lock after this
    // commit does remove the file published here, which is what deleting this observation means.
    let transaction = match conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
    {
        Ok(transaction) => transaction,
        Err(source) => {
            discard_written_file(&file, &destination)?;
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
        discard_written_file(&file, &destination)?;
        if already_registered {
            return Err(StoreError::ImageAlreadyRegistered { id: id_text });
        }
        return Err(StoreError::Sql { source });
    }

    match cw_core::atomic_file::rename_without_replacing(&file, &destination) {
        Ok(true) => {}
        Ok(false) => {
            discard_written_file(&file, &destination)?;
            // Not `ImageAlreadyRegistered`: the insert above has already succeeded, so once this
            // rolls back the observation has no row, and saying the database already knows about
            // the image would point recovery the wrong way. Nothing this program registered can
            // hold the name either — it writes one spelling per id and that spelling is this
            // row's — though a row planted with the same path spelled another way would survive
            // the index, since SQLite compares TEXT as bytes while an ordinary Windows directory
            // resolves names without regard to case. On a directory marked case-sensitive, which
            // `storage.data_dir` can be, those are two names and this rename never meets the other
            // file at all. Either way it is a database disagreeing with itself, and it is the sweep
            // that reports such a row rather than this branch.
            return Err(StoreError::ImageIo {
                path: destination,
                source: std::io::Error::from(std::io::ErrorKind::AlreadyExists),
            });
        }
        Err(source) => {
            discard_written_file(&file, &destination)?;
            return Err(StoreError::ImageIo {
                path: destination,
                source,
            });
        }
    }

    // The rename is a metadata change on this handle, and Windows buffers those; closing the
    // handle does not push them.
    if let Err(source) = file.sync_all() {
        discard_written_file(&file, &destination)?;
        return Err(StoreError::ImageIo {
            path: destination,
            source,
        });
    }

    if let Err(source) = transaction.commit() {
        // The picture stays. An error here does not prove the row is not there — SQLite does not
        // promise that every failed commit rolled back — and discarding it would turn that
        // uncertainty into the one outcome this store refuses: a registered row whose file is gone,
        // which nothing removes and retention keeps charging against a budget that is already free.
        // If the transaction did roll back, what is left is an unregistered file, which is what the
        // startup sweep collects. Every other failure above can discard, because none of them has
        // reached the commit.
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
/// name is occupied, a `save` for the same observation whose rename reaches that name before the
/// removal does finds it held and fails with [`StoreError::ImageIo`], because the rename refuses to
/// replace. What that guarantees is only that no save can take the name before the removal is
/// through; one whose rename lands after it finds the name free and succeeds the first time. If the
/// name is empty — a row whose file has already gone — nothing is
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

/// A file is removed only when nothing registered names it, it still resolves to the name it was
/// listed under, and it is not what one of the registered names reaches. That last question is put
/// to the registered names no enumerated file spelled and not to every row; the comment inside says
/// what that leaves out and what asking about all of them would cost. Reports how many went.
///
/// This is a startup operation and must not run while anything is saving: a file renamed into place
/// but not yet registered is indistinguishable from an orphan.
pub fn sweep_orphan_files(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
) -> Result<usize, StoreError> {
    // Resolved once, before anything is read, so that the baseline every candidate is compared
    // against is the one taken here rather than one taken after the walk. What this does not do is
    // pin the tree that gets enumerated: what is held is a path and not a handle, so `collect_files`
    // resolves this spelling again and a link put at the root's name in between is followed. What
    // keeps that from costing anything is the per-candidate check further down — a file listed
    // under the replacement does not resolve to the name it was listed under, so it is kept. That
    // check is load-bearing here and not merely a second opinion. Resolving the root after the walk
    // would move the baseline itself, and then every file under the replacement would compare as
    // though it belonged here.
    let root = match std::fs::canonicalize(root) {
        Ok(resolved) => resolved,
        // Nothing there at all is the ordinary state before the first save and there is nothing to
        // sweep, but that is a question about the entries on this path and not about what they lead
        // to, and it has to be put to the whole path rather than to its last component.
        // `canonicalize` answers `NotFound` for a name that was never there, for a link whose target
        // is gone, and for a path leading through either of those — measured 2026-08-01, and the
        // raw code does not separate them either. What decides is the deepest entry that does
        // exist: none at all, or a directory, and the rest of the path is simply not created yet;
        // anything else, and no image can ever be written here — `create_dir_all` answers
        // `AlreadyExists` — so calling it the state before the first save would report a clean
        // pass on every startup of a store that cannot work at all. The registered names below are
        // read by a weaker rule on purpose: `canonicalize` answering `NotFound` is taken as absent
        // there without asking about the entry, because that loop only needs to know which file a
        // row names, and a name leading nowhere names none. This root has to be walked.
        Err(source) => {
            let unreadable = StoreError::ImageIo {
                path: root.to_path_buf(),
                source,
            };
            for name in root.ancestors() {
                match std::fs::symlink_metadata(name) {
                    // Nothing is here. Keep climbing — the path may simply not be created yet, and
                    // running out of names means none of it is.
                    Err(absent) if absent.kind() == std::io::ErrorKind::NotFound => {}
                    // The name cannot be answered about, which is not the same as nothing being
                    // there. Measured 2026-08-01: a component the filesystem will not take — one
                    // holding a `|`, or longer than a component may be — answers `InvalidFilename`
                    // while the directory above it is perfectly ordinary, and a `storage.data_dir`
                    // typed with such a character is exactly that. Reporting a clean sweep on a
                    // doubt would report it on every startup.
                    Err(_) => return Err(unreadable),
                    // The deepest entry that does exist. The root itself arriving here is a name
                    // taken by something `canonicalize` refused. Otherwise what is left of the path
                    // is waiting to be created, and only a directory can hold it — asked with
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
            // Nothing on the whole path is there, its anchor included. For a relative root that is
            // the ordinary state before the first save: what holds it is the directory the process
            // is in, and that is never one of these names — measured 2026-08-01, the last name
            // `ancestors()` yields for a relative path is the empty one, which answers `NotFound`
            // itself. For an absolute root it means the drive or the share the path is anchored to
            // is not there, and `create_dir_all` creates a tail but never an anchor, so no image can
            // ever be written here. Measured 2026-08-01 with an unmapped drive letter: every name on
            // the path, `Z:\` included, answers `NotFound`, so the anchor's own absence is the only
            // thing that separates this from a directory waiting to be created.
            return if root.is_absolute() {
                Err(unreadable)
            } else {
                Ok(0)
            };
        }
    };
    let registered = registered_paths(conn)?;
    let mut files = Vec::new();
    collect_files(&root, &mut files)?;
    sweep_collected_files(&root, &registered, &files)
}

/// `root` must be spelled the way `canonicalize` answers, and `files` must have been listed by
/// walking that spelling: a candidate is required to resolve to the name it was listed under, and a
/// root spelled any other way makes every listed name fail that on its first component. Resolving
/// the root here would resolve it after the caller's walk, which is the window that separation
/// exists to close.
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
        // Opened before anything is decided, so that what goes at the end is the file this
        // iteration examined. `remove_file` would resolve the name a third time — after the walk
        // that listed it and after the `canonicalize` below — and a directory replaced above it in
        // between sends the removal wherever the replacement leads. A candidate that cannot be
        // opened is kept, for the same reason as one that cannot be resolved: two startups can
        // reach the same orphan, and the one that arrives second has nothing left to do. Measured
        // 2026-08-01, a directory cannot be opened this way at all, so no pass can remove one.
        let Ok(file) = cw_core::atomic_file::open_for_removal(path) else {
            continue;
        };
        let Ok(identity) = std::fs::canonicalize(path) else {
            continue;
        };
        // Every name here is resolved once to list it and again to act on it, and what stands under
        // it can be replaced in between — the entry itself, or a directory above it, since Windows
        // follows a reparse point met partway along a path. No spelling protects against that, so
        // the question is what the filesystem answers with. `collect_files` keeps only what the
        // filesystem calls a file and a link is not one, so every candidate was an ordinary file
        // when it was listed, and an ordinary file resolves to the name it was listed under.
        // Measured 2026-08-01 on two volumes, over a tree holding a hardlink, a Japanese name,
        // names with spaces and dots and twenty levels of nesting: every listed file canonicalized
        // to its own listed path, and a name reached through a directory link did not. Anything
        // answering differently is no longer what was listed, and what it now reaches may be a
        // registered image. This settles containment too: every listed name is under the root by
        // construction, so a candidate that is its own name is inside the root.
        if identity.as_path() != path.as_path() || unspelled_identities.contains(&identity) {
            continue;
        }

        // Addressed to the handle opened above, so no name is resolved between the last check and
        // the removal. A candidate that will not go is kept and the pass carries on, for the same
        // reason as one that cannot be opened or resolved: a single file must not decide whether
        // every other orphan is collected, and a file that fails the same way on every startup
        // would mean none of them ever are. Measured 2026-08-01, that is reachable with no race and
        // no privilege — a read-only file opens for removal and then answers `PermissionDenied` to
        // the disposition call. It is not counted, because the count is of files that went.
        if cw_core::atomic_file::delete_by_handle(&file).is_ok() {
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
                return Err(StoreError::ImageIo {
                    path: root.join(relative),
                    source,
                });
            }
        }
    }

    Ok(orphaned)
}

/// Discard the file `save` wrote, whichever name it answers to now. `destination` is only what the
/// failure is reported against; nothing is looked up by it.
fn discard_written_file(
    file: &std::fs::File,
    destination: &std::path::Path,
) -> Result<(), StoreError> {
    cw_core::atomic_file::delete_by_handle(file).map_err(|source| StoreError::ImageIo {
        path: destination.to_path_buf(),
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
    fn an_image_whose_commit_fails_stays_for_the_sweep() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let taken_at = at(2026, 7, 30);
        // No observation is inserted, so this image's row breaks the foreign key `db::open` turns
        // on. Measured 2026-08-01: the check is enforced as the INSERT arrives, unless it is
        // deferred — and then it is the COMMIT that answers `ConstraintViolation` (787), which is
        // the one failure the last statement of `save` can be given from here. `save` never reads
        // or writes this pragma, and SQLite clears it when the transaction ends.
        conn.execute_batch("PRAGMA defer_foreign_keys = ON")
            .expect("the pragma should apply");

        let result = save(
            &mut conn,
            &root,
            id,
            &pixels(93),
            WIDTH,
            HEIGHT,
            75.0,
            taken_at,
        );

        assert!(matches!(result, Err(StoreError::Sql { .. })), "{result:?}");
        // The picture is left where it was published, which is what the startup sweep collects.
        // Discarding it instead would be the one outcome this store refuses if the row did survive.
        let published = root.join(format!("2026/07/30/{id}.webp"));
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
        let id = ulid::Ulid::new();
        insert_observation(&conn, id, at(2026, 7, 30));

        // Another connection holds the write lock this save needs, and this one is told not to wait
        // for it, so the transaction cannot begin at all — after the temporary has been reserved
        // and written.
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
            &pixels(7),
            WIDTH,
            HEIGHT,
            75.0,
            at(2026, 7, 30),
        );

        assert!(matches!(result, Err(StoreError::Sql { .. })), "{result:?}");
        drop(held);
        let mut left_behind = Vec::new();
        super::collect_files(&root, &mut left_behind)
            .expect("the image tree should be collectable");
        assert!(left_behind.is_empty(), "{left_behind:?}");
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
    fn an_orphan_that_became_a_link_to_a_registered_image_is_not_removed() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 82);
        let picture = root.join(&relative);
        let orphan = root.join("2026").join("07").join("30").join("orphan.webp");
        std::fs::write(&orphan, b"not registered")
            .expect("the hand-placed file should be writable");

        let resolved = std::fs::canonicalize(&root).expect("the image root should resolve");
        let registered =
            super::registered_paths(&conn).expect("the registered paths should be readable");
        let mut files = Vec::new();
        super::collect_files(&resolved, &mut files).expect("the image tree should be collectable");

        // Both were ordinary files when they were listed, so the registered one is spelled by a
        // listed file and is not among the names the sweep asks about. The orphan then becomes a
        // link to it.
        std::fs::remove_file(&orphan).expect("the hand-placed file should be removable");
        let Ok(()) = std::os::windows::fs::symlink_file(&picture, &orphan) else {
            return;
        };

        let removed = sweep_collected_files(&resolved, &registered, &files)
            .expect("the sweep should succeed");

        assert_eq!(removed, 0);
        assert!(picture.is_file());
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

        // The root itself is an ordinary directory, so resolving it after the link exists answers
        // the same as resolving it before.
        let resolved = std::fs::canonicalize(&root).expect("the image root should resolve");

        // The name as it was enumerated, before the directory above it became a link. Nothing here
        // needs a race: the link can be in place before the sweep starts.
        let enumerated = vec![resolved.join("2026").join("orphan.webp")];
        let removed = super::sweep_collected_files(&resolved, &HashSet::new(), &enumerated)
            .expect("the sweep should succeed without removing anything");

        assert_eq!(removed, 0);
        assert!(victim.is_file());
    }

    #[test]
    fn a_root_replaced_after_the_walk_does_not_redirect_the_sweep() {
        let (dir, _conn, root) = database();
        std::fs::create_dir_all(&root).expect("the image root should be creatable");
        // What the caller resolves before it reads anything.
        let resolved = std::fs::canonicalize(&root).expect("the image root should resolve");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("the outside directory should be creatable");
        let victim = outside.join("orphan.webp");
        std::fs::write(&victim, b"a file this program has no business touching")
            .expect("the outside file should be writable");
        std::fs::rename(&root, dir.path().join("moved"))
            .expect("the image root should be movable aside");

        // Creating a directory link needs Developer Mode or SeCreateSymbolicLinkPrivilege, so this
        // case cannot be built everywhere the suite runs.
        let Ok(()) = std::os::windows::fs::symlink_dir(&outside, &root) else {
            return;
        };

        // The whole root now leads elsewhere. A sweep that resolved it here rather than before its
        // walk would take the replacement for its own baseline and find this file inside it.
        let enumerated = vec![resolved.join("orphan.webp")];
        let removed = super::sweep_collected_files(&resolved, &HashSet::new(), &enumerated)
            .expect("the sweep should succeed without removing anything");

        assert_eq!(removed, 0);
        assert!(victim.is_file());
    }

    #[test]
    fn a_sweep_of_a_root_that_is_not_there_yet_removes_nothing() {
        let (_dir, conn, root) = database();

        // Every startup before the first save finds no image root at all, and that is not a failure
        // to report — it is a directory with nothing in it to sweep.
        assert!(!root.exists());
        assert_eq!(
            sweep_orphan_files(&conn, &root).expect("a sweep before the first save should succeed"),
            0
        );
    }

    #[test]
    fn a_root_whose_entry_is_there_and_will_not_resolve_is_reported() {
        let (dir, conn, root) = database();
        let nowhere = dir.path().join("nowhere");

        // Creating a directory link needs Developer Mode or SeCreateSymbolicLinkPrivilege, so this
        // case cannot be built everywhere the suite runs.
        let Ok(()) = std::os::windows::fs::symlink_dir(&nowhere, &root) else {
            return;
        };

        // `canonicalize` answers `NotFound` here, exactly as it does for a name that was never
        // there — but this name is taken, and every save will fail on it until someone clears it.
        let result = sweep_orphan_files(&conn, &root);

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_root_under_a_directory_that_does_not_exist_yet_removes_nothing() {
        let (dir, conn, _root) = database();
        let root = dir.path().join("not-yet").join("images");

        // A first run whose whole data directory is still to be created. The deepest entry that
        // does exist is the temporary directory, and it is a directory, so nothing here is wrong.
        assert_eq!(
            sweep_orphan_files(&conn, &root).expect("a sweep before the first save should succeed"),
            0
        );
    }

    #[test]
    fn a_root_whose_parent_leads_nowhere_is_reported() {
        let (dir, conn, _root) = database();
        let parent = dir.path().join("data");
        let root = parent.join("images");

        // Creating a directory link needs Developer Mode or SeCreateSymbolicLinkPrivilege, so this
        // case cannot be built everywhere the suite runs.
        let Ok(()) = std::os::windows::fs::symlink_dir(dir.path().join("nowhere"), &parent) else {
            return;
        };

        // The root's own entry is absent here exactly as it is in the test above, and the two are
        // told apart by what is standing above it: a link whose target is gone, under which
        // `create_dir_all` answers `AlreadyExists` and no image can ever be written.
        let result = sweep_orphan_files(&conn, &root);

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_root_the_filesystem_will_not_answer_about_is_reported() {
        let (dir, conn, _root) = database();
        // A `storage.data_dir` typed with a character Windows does not take. Measured 2026-08-01:
        // both `canonicalize` and `symlink_metadata` answer `InvalidFilename` here, while the
        // directory holding it is an ordinary one — so nothing about this name has been shown to
        // be absent, and it needs no special privilege to arrange.
        let root = dir.path().join("im|ages");

        let result = sweep_orphan_files(&conn, &root);

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_relative_root_that_is_not_there_yet_removes_nothing() {
        let (_dir, conn, _root) = database();
        // `resolve_data_dir` hands back what was configured, so a relative `storage.data_dir` is
        // something a user can write. Every name on such a path can be absent while the directory
        // holding it — the one the process is in — is perfectly ordinary and is never among them,
        // so running out of names says nothing here. This is the other side of the check that
        // reports an absolute root whose drive is not there.
        let root = std::path::PathBuf::from("cw-store-root-that-was-never-created");
        assert!(!root.exists(), "the test would say nothing if this existed");

        assert_eq!(
            sweep_orphan_files(&conn, &root).expect("a relative root is simply not created yet"),
            0
        );
    }

    #[test]
    fn a_root_anchored_to_a_drive_that_is_not_there_is_reported() {
        let (_dir, conn, _root) = database();
        // Any letter with no volume behind it. Measured 2026-08-01: every name on such a path
        // answers `NotFound`, the anchor included, so this is the arrangement in which running out
        // of names is the only signal there is.
        let Some(letter) = ('D'..='Z').find(|letter| {
            std::fs::symlink_metadata(format!("{letter}:\\"))
                .is_err_and(|absent| absent.kind() == std::io::ErrorKind::NotFound)
        }) else {
            return;
        };
        let root = std::path::PathBuf::from(format!("{letter}:\\ContextWitness\\images"));

        let result = sweep_orphan_files(&conn, &root);

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_root_that_is_a_directory_no_one_may_open_is_reported() {
        let (_dir, conn, root) = database();
        std::fs::create_dir(&root).expect("the image root should be creatable");
        let Ok(user) = std::env::var("USERNAME") else {
            return;
        };
        let path = root.to_string_lossy().to_string();
        // A guard and not a call, because a call is skipped by exactly the failure this test exists
        // to detect: a panic anywhere below unwinds past every restore written as a statement, and
        // measured 2026-08-01 a directory left holding this deny cannot be removed by anything —
        // not by the temporary directory, not by `Remove-Item -Recurse -Force`. Provoked with a
        // `panic!` in place of the sweep: with the restore written as a call the entry survived
        // holding `(DENY)(RX)` and the whole temporary tree became unremovable; with this it did
        // not. Constructed before the deny is applied, so the exit taken when `icacls` cannot even
        // be waited on is covered, and declared after the temporary directory so that it runs
        // before the directory is taken away.
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
        let _restore = RestoreEntry {
            path: path.as_str(),
            user: user.as_str(),
        };
        // std has no way to set an ACL and this crate may hold no `unsafe`, so the check is asked
        // of the tool Windows ships with. Measured 2026-08-01: after this, `canonicalize` answers
        // `PermissionDenied` while `metadata` still answers that it is a directory — the one state
        // that separates a root which is there and will not open from a path not created yet.
        let denied = std::process::Command::new("icacls")
            .args([
                path.as_str(),
                "/deny",
                &format!("{user}:(RX,RA,RD)"),
                "/inheritance:r",
            ])
            .output();
        // `Err` here does not mean the tool never ran: the standard library spawns the child first
        // and the wait that follows is fallible on its own, so the deny may already be applied.
        // Nothing here decides which of the two happened, because nothing has to: the guard above
        // runs on this exit as on every other, and on a directory that was never denied it changes
        // nothing.
        let Ok(output) = denied else {
            return;
        };
        if !output.status.success() || std::fs::canonicalize(&root).is_ok() {
            return;
        }

        let result = sweep_orphan_files(&conn, &root);

        // Nothing is dropped or restored by hand here. The temporary directory has to outlive the
        // connection that holds `db.sqlite3` open and the guard above has to run before either, and
        // leaving the scope — including by unwinding out of the assertion below — already drops
        // them in that order.

        assert!(
            matches!(result, Err(StoreError::ImageIo { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn an_orphan_that_will_not_go_does_not_stop_the_others() {
        let (_dir, conn, root) = database();
        std::fs::create_dir_all(&root).expect("the image root should be creatable");
        let stubborn = root.join("stubborn.webp");
        let ordinary = root.join("ordinary.webp");
        std::fs::write(&stubborn, b"an orphan").expect("the file should be writable");
        std::fs::write(&ordinary, b"an orphan").expect("the file should be writable");
        // Measured 2026-08-01: this file opens for removal and then refuses the disposition call
        // with `PermissionDenied`, which is the reachable form of a candidate that will not go.
        let mut attributes = std::fs::metadata(&stubborn)
            .expect("the file should be there")
            .permissions();
        attributes.set_readonly(true);
        std::fs::set_permissions(&stubborn, attributes).expect("the attribute should be settable");

        // The attribute is left set on purpose and nothing here puts it back: measured 2026-08-01,
        // `std::fs::remove_file` clears it and succeeds, so the temporary directory can still take
        // the file away — which is the same difference this test is about.
        let removed = sweep_orphan_files(&conn, &root);

        assert_eq!(
            removed.expect("one file that will not go must not fail the pass"),
            1
        );
        assert!(stubborn.exists(), "the one that will not go should be kept");
        assert!(!ordinary.exists(), "the other one should have gone");
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
        let resolved = std::fs::canonicalize(&root).expect("the image root should resolve");
        let registered =
            super::registered_paths(&conn).expect("the registered paths should be readable");
        let mut files = Vec::new();
        super::collect_files(&resolved, &mut files)
            .expect("the image tree should be collectable for both startups");

        assert_eq!(
            sweep_collected_files(&resolved, &registered, &files)
                .expect("the first startup sweep should succeed"),
            1
        );
        assert_eq!(
            sweep_collected_files(&resolved, &registered, &files)
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
    fn a_registered_name_that_is_a_link_to_its_image_is_not_reported() {
        let (dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 91);
        let registered = root.join(&relative);
        let moved = dir.path().join("moved.webp");
        std::fs::rename(&registered, &moved).expect("the image should be movable aside");

        // Creating a symlink needs Developer Mode or SeCreateSymbolicLinkPrivilege, so this case
        // cannot be built everywhere the suite runs.
        let Ok(()) = std::os::windows::fs::symlink_file(&moved, &registered) else {
            return;
        };

        // The row still reaches its picture, which is all a registered name has to do.
        assert!(
            orphan_rows(&conn, &root)
                .expect("the report should succeed")
                .is_empty()
        );
    }

    #[test]
    fn a_registered_name_held_by_a_directory_is_reported() {
        let (_dir, mut conn, root) = database();
        let id = ulid::Ulid::new();
        let relative = save_test_image(&mut conn, &root, id, at(2026, 7, 30), 92);
        let registered = root.join(&relative);
        std::fs::remove_file(&registered).expect("the image should be removable");
        std::fs::create_dir(&registered).expect("a directory should take the freed name");

        // Something is there under that name and it is not this row's picture.
        assert_eq!(
            orphan_rows(&conn, &root).expect("the report should succeed"),
            vec![relative]
        );
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
