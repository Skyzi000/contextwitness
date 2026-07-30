//! Atomic file publication without replacing an existing destination.

/// Distinguishes concurrent publish attempts within one process; the process id distinguishes
/// processes.
static NEXT_TEMPORARY_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A temporary path beside `destination`, distinct for every call.
///
/// Sharing one temporary between two publish attempts is not a near miss: the share mode lets the
/// second `open` succeed, its `truncate` discards bytes the first has already flushed, and the
/// loser's handle goes on writing into the file after the winner has published it under the
/// destination name (all measured 2026-07-27).
pub fn temporary_path_beside(destination: &std::path::Path) -> std::path::PathBuf {
    let id = NEXT_TEMPORARY_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut temporary = destination.as_os_str().to_os_string();
    temporary.push(format!(".tmp-{}-{id}", std::process::id()));
    std::path::PathBuf::from(temporary)
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
    // MOVEFILE_REPLACE_EXISTING and silently replaces the destination (measured 2026-07-27).
    // Creating the destination first and filling it afterwards is no better — the name exists
    // before the content does, so a concurrent `setup` is told the config is ready, writes the
    // user's settings into the empty shell, and has them replaced a moment later. Renaming by
    // handle with ReplaceIfExists = FALSE is the only operation that makes the name appear
    // already holding the full template, and it works on exFAT as well as NTFS (both measured).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CONFIG_TOML;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// GENERIC_WRITE | DELETE. The DELETE right is what lets a handle rename its own file.
    const RENAMABLE_WRITE_ACCESS: u32 = 0x4000_0000 | 0x0001_0000;
    /// FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
    const TEMPORARY_SHARE_MODE: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;

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
            .share_mode(TEMPORARY_SHARE_MODE)
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
}
