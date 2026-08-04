//! Atomic file publication without replacing an existing destination.

/// GENERIC_READ | GENERIC_WRITE | DELETE. The DELETE right is what lets a handle rename its own
/// file. Read is asked for because Microsoft documents that creating a file across a network with
/// write alone sends more and smaller writes, since the redirector cannot use the cache manager,
/// and can occasionally answer `ERROR_ACCESS_DENIED`; `storage.data_dir` may be a share and
/// `%APPDATA%` may be redirected to one.
const RENAMABLE_WRITE_ACCESS: u32 = 0x8000_0000 | 0x4000_0000 | 0x0001_0000;
/// FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE — every shareable right given away, which
/// is as much as a handle can offer and still not the same as not being open. Sharing is checked in
/// both directions: a newcomer whose own share mode leaves out a right this handle holds is refused
/// with a sharing violation, so anything that opens files exclusively is locked out for as long as
/// this is held. Unaffected are opening the file for reading or for writing with the sharing the
/// standard library asks for, and renaming or removing it by name.
const FULL_SHARE_MODE: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;

/// How many names are tried before giving up. Each attempt costs one failed `open`. Sixty-four
/// leftovers from earlier runs — the process id in the name repeats — would exhaust it with nothing
/// racing at all, which is why what it returns then is the platform's own answer about the last
/// name rather than an error this module invented.
const TEMPORARY_ATTEMPTS: u32 = 64;

/// Create a file beside `destination` under a name this call has to itself, and return both.
///
/// Beside the destination, never elsewhere: a cross-volume rename is not atomic. The name is
/// reserved by creating it, not chosen by hoping — `create_new` fails rather than opening what is
/// already there, and what is already there may be a link, in which case a truncating open empties
/// the file at the other end of it, with this program's rights and before the destination's
/// no-clobber rename can refuse anything. A taken name is therefore a reason to try the next one.
/// The filesystem is also the only party that can keep two attempts apart, since the other one may
/// be in another process: two callers both start at zero and exactly one of them gets it.
pub fn create_temporary_beside(
    destination: &std::path::Path,
) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    use std::os::windows::fs::OpenOptionsExt;

    let mut last = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    for attempt in 0..TEMPORARY_ATTEMPTS {
        let mut temporary = destination.as_os_str().to_os_string();
        temporary.push(format!(".tmp-{}-{attempt}", std::process::id()));
        let temporary = std::path::PathBuf::from(temporary);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(RENAMABLE_WRITE_ACCESS)
            .share_mode(FULL_SHARE_MODE)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            // Not only `AlreadyExists`: on Windows a name held by a directory answers
            // `PermissionDenied`, and so does a deleted name on a filesystem that keeps it until
            // its last handle closes. Either way this call did not get the name, which is the only
            // thing it needs to know before trying the next one.
            Err(source)
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                last = source;
            }
            Err(source) => return Err(source),
        }
    }
    Err(last)
}

/// The `io::Error` for a Win32 failure reported as an `HRESULT`.
///
/// `from_raw_os_error` wants the raw Win32 code, and an HRESULT is not one: a Win32 error arrives
/// wrapped as `0x8007_0000 | code`, so handing it over whole loses the classification — access
/// denied stops being `PermissionDenied` and becomes a number no documentation lists. Anything from
/// another facility has no Win32 code to recover and is carried across whole.
fn io_error(error: windows::core::Error) -> std::io::Error {
    const FACILITY_WIN32: i32 = 0x8007_0000u32 as i32;
    let code = error.code().0;
    if code & 0xFFFF_0000u32 as i32 == FACILITY_WIN32 {
        std::io::Error::from_raw_os_error(code & 0xFFFF)
    } else {
        std::io::Error::other(error)
    }
}

/// Rename the open file to `destination`, failing instead of replacing when that name is taken.
/// `Ok(true)` when the file now lives at `destination`, `Ok(false)` when something else already
/// does.
pub fn rename_without_replacing(
    file: &std::fs::File,
    destination: &std::path::Path,
) -> std::io::Result<bool> {
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
    use windows::Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, HANDLE},
        Storage::FileSystem::{FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle},
    };

    // `fs::rename` cannot be used to publish a config: on Windows it is MoveFileExW with
    // MOVEFILE_REPLACE_EXISTING and silently replaces the destination.
    // Creating the destination first and filling it afterwards is no better — the name exists
    // before the content does, so a concurrent `setup` is told the config is ready, writes the
    // user's settings into the empty shell, and has them replaced a moment later. Renaming by
    // handle with ReplaceIfExists = FALSE makes the name appear already holding the full template,
    // and it works on exFAT as well as NTFS. It is not the only call that would refuse a taken
    // destination: `MoveFileExW` with no flags answers `ERROR_ALREADY_EXISTS` and leaves both files
    // as they were. What the handle adds is that it
    // moves the file this call opened, rather than whatever its source name has come to mean by
    // now.
    let destination = std::path::absolute(destination)?;
    let destination: Vec<u16> = destination.as_os_str().encode_wide().collect();
    let name_bytes = destination.len() * std::mem::size_of::<u16>();
    let mut buf = vec![0u64; (std::mem::size_of::<FILE_RENAME_INFO>() + name_bytes).div_ceil(8)];
    let result = unsafe {
        let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = HANDLE::default();
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(
            destination.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            destination.len(),
        );
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileRenameInfo,
            buf.as_ptr().cast(),
            (buf.len() * 8) as u32,
        )
    };
    let already_exists = windows::core::HRESULT::from_win32(ERROR_ALREADY_EXISTS.0); // 0x800700B7

    match result {
        Ok(()) => Ok(true),
        Err(error) if error.code() == already_exists => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

/// DELETE. Enough to remove the file through the handle and nothing else; the file is not read and
/// not written.
const REMOVABLE_ACCESS: u32 = 0x0001_0000;

/// Open an existing file so that it can be removed through the handle rather than through its name.
///
/// A name can be given to another file, and a directory above it can be replaced by a link, so
/// resolving a name a second time to remove it can reach something else. The handle settles which
/// file that is at the open, and nothing done to the name afterwards moves it; between deciding and
/// the open there is no such hold. This opens an ordinary file with DELETE alone and refuses a
/// directory, which is the guarantee the caller gets: a directory can never be removed this way.
/// Which error a directory produces is not part of it, because the spelling changes the answer.
/// The file stays readable, writable, renamable and removable by name while the handle is held;
/// what holding it does cost anyone else is on `FULL_SHARE_MODE`.
pub fn open_for_removal(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .access_mode(REMOVABLE_ACCESS)
        .share_mode(FULL_SHARE_MODE)
        .open(path)
}

/// FILE_FLAG_OPEN_REPARSE_POINT. Opens the entry standing at the name rather than whatever it leads
/// to, and only for the last component: directories on the way are still followed.
const OPEN_ENTRY_ITSELF: u32 = 0x0020_0000;

/// Open whatever entry stands at `path` so that it can be removed through the handle, without
/// following it.
///
/// Same as [`open_for_removal`] except in what it does with a link: that one opens the file at the
/// other end, and this one opens the link. Removing a name means removing the entry that carries
/// it, so a caller that was asked to clear a name wants this; a caller that decided about a file's
/// contents wants the other.
///
/// A directory is still refused, so no pass can remove one this way.
pub fn open_entry_for_removal(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .access_mode(REMOVABLE_ACCESS)
        .share_mode(FULL_SHARE_MODE)
        .custom_flags(OPEN_ENTRY_ITSELF)
        .open(path)
}

/// How many rounds of asking for the room and then for the name before giving up. The file can be
/// renamed while a handle to it is held, so an answer that outgrew the buffer measured for it is an
/// outcome and not a fault; what must not happen is asking forever.
const FINAL_PATH_ATTEMPTS: u32 = 4;

/// The name this handle refers to, as the filesystem spells it now.
///
/// Resolving a name twice can reach two files, so a check made against a freshly resolved name says
/// nothing about a handle opened before it. This asks the handle. The answer comes back in the
/// `\\?\` form `canonicalize` also produces, so the two compare without normalising either.
pub fn final_path_by_handle(file: &std::fs::File) -> std::io::Result<std::path::PathBuf> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW},
    };

    let handle = HANDLE(file.as_raw_handle());
    path_from_sized_query(|buffer| unsafe {
        GetFinalPathNameByHandleW(handle, buffer, FILE_NAME_NORMALIZED)
    })
}

/// The name a size-then-fill query settles on, from a `query` that follows the Win32 convention:
/// zero is a failure, a buffer that holds the name is filled and answered with the length without
/// its terminator, and a buffer that does not is answered with the length that would, terminator
/// included.
///
/// Separate from the call that supplies those answers because the arithmetic is what has to be
/// right — the same number means the name or means the room it needs, and reading it the wrong way
/// either abandons a file that was identified or hands back a truncated name as an identity. A
/// Win32 call cannot be made to answer to order, and this can.
///
/// A `query` that breaks the convention is refused rather than believed. An answer as long as the
/// buffer is one the convention cannot produce — it would be asking for room the buffer already
/// has — and the buffer behind it was never written to, so this gives back no name for it.
fn path_from_sized_query(
    mut query: impl FnMut(&mut [u16]) -> u32,
) -> std::io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    for _ in 0..FINAL_PATH_ATTEMPTS {
        // Asked for the room first, and then for the name. Running out of room in between is not a
        // failure and is not reported as one, so it is asked again rather than given whatever
        // `GetLastError` was left holding.
        let required = query(&mut []);
        if required == 0 {
            return Err(std::io::Error::last_os_error());
        }

        let mut buffer = vec![0; required as usize];
        let written = query(&mut buffer);
        if written == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Fitting strictly inside is what separates a name from the room a name needs.
        if (written as usize) < buffer.len() {
            return Ok(std::ffi::OsString::from_wide(&buffer[..written as usize]).into());
        }
    }

    Err(std::io::Error::other(
        "the query never answered with a name",
    ))
}

/// Remove the file this handle refers to, whatever name it answers to by now.
///
/// A name can be given to another file between deciding to remove one and removing it, and every
/// caller here is holding the file it means. With the published file moved aside and a different
/// file put at its name, this removes the one that was written and leaves the
/// other, while `remove_file` on the name does the opposite. The entry disappears when the last
/// handle closes rather than at once, so a caller that needs the name free again must drop the file
/// first — every caller here is on its way out.
pub fn delete_by_handle(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
        },
    };

    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    let result = unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            std::ptr::addr_of!(info).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    result.map_err(io_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CONFIG_TOML;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_path(test_name: &str) -> std::path::PathBuf {
        let counter = TEMP_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "contextwitness-{test_name}-{}-{counter}",
            std::process::id()
        ))
    }

    #[test]
    fn rename_without_replacing_reports_a_taken_name_and_leaves_both_files() {
        use std::os::windows::fs::OpenOptionsExt;

        let temp_dir = unique_temp_path("rename-without-replacing-taken");
        assert!(!temp_dir.exists());
        std::fs::create_dir(&temp_dir)
            .expect("the unique rename test directory should be creatable");
        let destination = temp_dir.join("config.toml");
        let temporary = temp_dir.join("config.toml.tmp-test");
        std::fs::write(&destination, "a config another process finished first")
            .expect("the winning config should be writable");

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .access_mode(RENAMABLE_WRITE_ACCESS)
            .share_mode(FULL_SHARE_MODE)
            .open(&temporary)
            .expect("the temporary config should be openable");
        std::io::Write::write_all(&mut file, DEFAULT_CONFIG_TOML.as_bytes())
            .expect("the default config should be writable to the temporary");

        // The public entry point takes a fast path when a config is already present, so target the
        // helper directly: this is the branch where the config appears after that test, which
        // cannot be reached through the public entry point deterministically.
        let renamed = rename_without_replacing(&file, &destination)
            .expect("a taken destination should be reported without an error");
        drop(file);

        assert!(!renamed, "a taken destination should be reported as false");
        assert_eq!(
            std::fs::read_to_string(&destination)
                .expect("the winning config should remain readable"),
            "a config another process finished first"
        );
        assert!(
            temporary.exists(),
            "the rename should leave temporary cleanup to its caller"
        );

        std::fs::remove_dir_all(&temp_dir).expect("the rename test directory should be removable");
    }

    #[test]
    fn deleting_through_the_handle_takes_the_file_and_not_the_name() {
        let destination = unique_temp_path("delete-by-handle");
        let (_temporary, file) =
            create_temporary_beside(&destination).expect("the temporary should be reservable");
        assert!(
            rename_without_replacing(&file, &destination).expect("the publish should succeed"),
            "the destination should have been free"
        );

        // Another party moves the published file aside and puts a different one at its name.
        let moved = unique_temp_path("delete-by-handle-moved");
        std::fs::rename(&destination, &moved).expect("the published file should be movable");
        std::fs::write(&destination, b"someone else's").expect("the freed name should be writable");

        delete_by_handle(&file).expect("the file this handle holds should be removable");
        drop(file);

        assert!(!moved.exists(), "the file this handle wrote should be gone");
        assert_eq!(
            std::fs::read(&destination).expect("the file at the name should still be there"),
            b"someone else's"
        );
        std::fs::remove_file(&destination).expect("the test file should be removable");
    }

    #[test]
    fn a_file_opened_for_removal_goes_and_the_name_stays() {
        let path = unique_temp_path("open-for-removal");
        std::fs::write(&path, b"an orphan").expect("the file should be writable");

        let file = open_for_removal(&path).expect("an existing file should be openable to remove");

        // Another party moves it aside and puts a different file at its name.
        let moved = unique_temp_path("open-for-removal-moved");
        std::fs::rename(&path, &moved).expect("the file should be movable");
        std::fs::write(&path, b"someone else's").expect("the freed name should be writable");

        delete_by_handle(&file).expect("the file this handle holds should be removable");
        drop(file);

        assert!(!moved.exists(), "the file that was opened should be gone");
        assert_eq!(
            std::fs::read(&path).expect("the file at the name should still be there"),
            b"someone else's"
        );
        std::fs::remove_file(&path).expect("the test file should be removable");
    }

    #[test]
    fn a_file_held_for_removal_shares_read_write_rename_and_removal() {
        let path = unique_temp_path("open-for-removal-shares");
        std::fs::write(&path, b"an orphan").expect("the file should be writable");

        let file = open_for_removal(&path).expect("an existing file should be openable to remove");

        // What this handle gives away. Sharing is checked in both directions, so a newcomer whose
        // own share mode leaves out a right this handle holds is refused, and anything that opens
        // files exclusively is locked out while this is held. The read and write bits are covered
        // here and nowhere else. The removal bit is not: renaming a file needs it too, so every
        // test that moves one out from under its handle fails without it as well.
        std::fs::File::open(&path).expect("a reader sharing what the library shares should get in");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("a writer sharing what the library shares should get in");
        let moved = unique_temp_path("open-for-removal-shares-moved");
        std::fs::rename(&path, &moved).expect("the name should still be renamable");
        std::fs::remove_file(&moved).expect("the file should still be removable by name");

        drop(file);
    }

    #[test]
    fn a_directory_cannot_be_opened_for_removal() {
        let path = unique_temp_path("open-for-removal-directory");
        std::fs::create_dir(&path).expect("the directory should be creatable");

        // The refusal is the guarantee; which kind carries it is not, and no caller reads it.
        open_for_removal(&path).expect_err("a directory must not open for removal");

        std::fs::remove_dir(&path).expect("the test directory should be removable");
    }

    #[test]
    fn a_win32_failure_keeps_the_kind_the_operating_system_gave_it() {
        let access_denied = io_error(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x8007_0005u32 as i32,
        )));
        assert_eq!(access_denied.raw_os_error(), Some(5));
        assert_eq!(access_denied.kind(), std::io::ErrorKind::PermissionDenied);

        let file_not_found = io_error(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x8007_0002u32 as i32,
        )));
        assert_eq!(file_not_found.raw_os_error(), Some(2));
        assert_eq!(file_not_found.kind(), std::io::ErrorKind::NotFound);

        let other_facility = io_error(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x8004_0005u32 as i32,
        )));
        assert_eq!(other_facility.raw_os_error(), None);
    }

    #[test]
    fn an_occupied_temporary_name_is_left_untouched() {
        let temp_dir = unique_temp_path("temporary-name-occupied");
        std::fs::create_dir(&temp_dir).expect("the unique test directory should be creatable");
        let destination = temp_dir.join("config.toml");
        let victim = temp_dir.join("victim");
        let contents = "bytes that were not this program's to empty";
        std::fs::write(&victim, contents).expect("the victim file should be writable");

        // The name a fresh call tries first, planted as another name for a file this program has
        // no business touching. Opening it to truncate would empty the victim through the link.
        let mut planted = destination.as_os_str().to_os_string();
        planted.push(format!(".tmp-{}-0", std::process::id()));
        let planted = std::path::PathBuf::from(planted);
        std::fs::hard_link(&victim, &planted)
            .expect("a hard link on one volume should be creatable");

        let (temporary, file) = create_temporary_beside(&destination)
            .expect("a taken name should not stop the publish");

        assert_ne!(
            temporary, planted,
            "the planted name should have been left alone"
        );
        assert!(planted.exists(), "the planted entry should still be there");
        assert_eq!(
            std::fs::read_to_string(&victim).expect("the victim should remain readable"),
            contents
        );

        drop(file);
        std::fs::remove_dir_all(&temp_dir).expect("the test directory should be removable");
    }

    #[test]
    fn a_temporary_name_held_by_a_directory_is_stepped_past() {
        let temp_dir = unique_temp_path("temporary-name-directory");
        std::fs::create_dir(&temp_dir).expect("the unique test directory should be creatable");
        let destination = temp_dir.join("config.toml");

        // A reserving open on a name a directory holds answers
        // `PermissionDenied`, not `AlreadyExists`, so a loop that steps past only the latter gives
        // up here while every later name is free.
        let mut planted = destination.as_os_str().to_os_string();
        planted.push(format!(".tmp-{}-0", std::process::id()));
        let planted = std::path::PathBuf::from(planted);
        std::fs::create_dir(&planted).expect("the planted directory should be creatable");

        let (temporary, file) = create_temporary_beside(&destination)
            .expect("a name a directory holds should not stop the publish");

        assert_ne!(temporary, planted);
        assert!(temporary.is_file());

        drop(file);
        std::fs::remove_dir_all(&temp_dir).expect("the test directory should be removable");
    }

    #[test]
    fn two_attempts_at_one_destination_get_different_names() {
        let temp_dir = unique_temp_path("temporary-name-shared");
        std::fs::create_dir(&temp_dir).expect("the unique test directory should be creatable");
        let destination = temp_dir.join("config.toml");

        let (first_path, mut first) =
            create_temporary_beside(&destination).expect("the first temporary should be creatable");
        std::io::Write::write_all(&mut first, b"first")
            .expect("the first temporary should be writable");
        let (second_path, second) = create_temporary_beside(&destination)
            .expect("the second temporary should be creatable while the first is open");

        assert_ne!(first_path, second_path);
        assert_eq!(first_path.parent(), destination.parent());
        assert_eq!(second_path.parent(), destination.parent());
        drop(first);
        drop(second);
        assert_eq!(
            std::fs::read(&first_path).expect("the first temporary should remain readable"),
            b"first".as_slice(),
            "a shared temporary would have let the second attempt empty the first"
        );

        std::fs::remove_dir_all(&temp_dir).expect("the test directory should be removable");
    }

    #[test]
    fn the_final_path_is_spelled_the_way_canonicalize_spells_it() {
        let path = unique_temp_path("final-path-spelling");
        std::fs::write(&path, b"contents").expect("the file should be creatable");
        let canonical = std::fs::canonicalize(&path).expect("the file should canonicalize");

        let file = open_for_removal(&path).expect("the file should open for removal");
        let answered = final_path_by_handle(&file).expect("the handle should answer its own name");
        drop(file);

        // The sweep compares this against names taken from a walk over a canonicalized root, so an
        // answer in any other form would never match one of them and every orphan would be kept.
        assert_eq!(answered, canonical);

        std::fs::remove_file(&path).expect("the test file should be removable");
    }

    #[test]
    fn the_final_path_stays_with_the_open_file_when_the_name_is_given_to_another() {
        let held = unique_temp_path("final-path-held");
        let moved = unique_temp_path("final-path-held-under-a-longer-name-than-before");
        std::fs::write(&held, b"held").expect("the file should be creatable");

        // Opened under one name, which is then given to a different file. Anything that resolved
        // the name after this point would be told about the newcomer while still holding the file
        // it decided about, which is the confusion the sweep's check has to be immune to.
        let file = open_for_removal(&held).expect("the file should open for removal");
        std::fs::rename(&held, &moved).expect("a file open for removal should still be renamable");
        std::fs::write(&held, b"newcomer").expect("the freed name should take another file");

        let answered = final_path_by_handle(&file).expect("the handle should answer its own name");
        drop(file);

        let by_name = std::fs::canonicalize(&held).expect("the newcomer should resolve");
        let moved_now = std::fs::canonicalize(&moved).expect("the moved file should resolve");

        assert_eq!(answered, moved_now);
        assert_ne!(answered, by_name);

        std::fs::remove_file(&held).expect("the newcomer should be removable");
        std::fs::remove_file(&moved).expect("the moved file should be removable");
    }

    /// Answers about a name that is one step further along on every call, in the shape the Win32
    /// query uses: an empty or undersized buffer is answered with the room the name needs including
    /// its terminator, and one that fits is filled and answered with the length without it. Because
    /// the name moves on between the two calls of a round, a list that keeps growing is a name
    /// being renamed out from under the caller, and a list that stops growing is one that settles.
    fn query_over(names: &[&str]) -> impl FnMut(&mut [u16]) -> u32 {
        let names: Vec<Vec<u16>> = names.iter().map(|n| n.encode_utf16().collect()).collect();
        let mut call = 0usize;
        move |buffer: &mut [u16]| {
            let name = &names[call.min(names.len() - 1)];
            call += 1;
            if buffer.len() > name.len() {
                buffer[..name.len()].copy_from_slice(name);
                name.len() as u32
            } else {
                (name.len() + 1) as u32
            }
        }
    }

    #[test]
    fn a_name_that_grows_once_and_then_settles_is_answered() {
        // The first round measures for `ab` and is asked to hold `abcd`, so it is asked again.
        let answered = path_from_sized_query(query_over(&["ab", "abcd", "abcd", "abcd"]))
            .expect("a name that stops growing should be answered");

        assert_eq!(answered, std::path::PathBuf::from("abcd"));
    }

    #[test]
    fn a_name_that_never_settles_is_refused_rather_than_truncated() {
        // Every round measures for one name and is asked to hold a longer one. Answering at all
        // here would mean answering with a name that was cut to fit.
        let error = path_from_sized_query(query_over(&[
            "a", "ab", "abc", "abcd", "abcde", "abcdef", "abcdefg", "abcdefgh",
        ]))
        .expect_err("a name that keeps growing must not be answered");

        assert_eq!(error.kind(), std::io::ErrorKind::Other, "{error:?}");
    }

    #[test]
    fn a_query_that_answers_zero_is_a_failure() {
        // Which failure it is comes from the operating system and is not decided here; that it is
        // one is.
        path_from_sized_query(|_| 0).expect_err("zero must not be read as a name");
    }

    #[test]
    fn a_query_that_breaks_the_convention_is_given_back_no_name() {
        // An answer as long as the buffer asks for room the buffer already has, so no query that
        // follows the convention produces it. Believing it would hand back a buffer that was never
        // written to, as a name.
        let error = path_from_sized_query(|buffer| {
            if buffer.is_empty() {
                4
            } else {
                buffer.len() as u32
            }
        })
        .expect_err("an answer as long as the buffer must not be read as a name");

        assert_eq!(error.kind(), std::io::ErrorKind::Other, "{error:?}");
    }
}
