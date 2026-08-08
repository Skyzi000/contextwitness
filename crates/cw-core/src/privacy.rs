/// True when `foreground_process` (a full path or a bare file name) matches any blacklist entry.
///
/// Only the file-name component is compared, whole and case-insensitively: an entry written as a
/// full path matches that file name in any directory, and nothing matches by prefix, suffix or
/// glob. Matching by fragment would suppress every program whose name merely contains the one
/// written.
///
/// This is a comparison of names and not of files. A directory with per-directory case sensitivity
/// can hold `Foo.exe` and `foo.exe` as two different files, and one blacklist entry matches both. A
/// comparison that cannot be carried out counts as a match, so failing to compare never ends with
/// recording a screen the user excluded.
pub fn is_blacklisted(foreground_process: &str, blacklist: &[String]) -> bool {
    let Some(foreground_file_name) = file_name_component(foreground_process) else {
        return false;
    };

    blacklist.iter().any(|entry| {
        file_name_component(entry)
            .is_some_and(|entry_file_name| file_names_equal(foreground_file_name, entry_file_name))
    })
}

/// Whether two file-name components are the same name under Windows' default file-name comparison.
///
/// Windows makes that judgement with an ordinal, case-insensitive comparison, so that is what we
/// call. No Unicode rule reproduces it: Windows folds ASCII, Cyrillic and accented letters, but
/// keeps Greek final sigma, sharp s and ligatures distinct.
///
/// Reporting two distinct files equal, which a case-sensitive directory can produce, is accepted:
/// what reaches here is a file-name component — the caller reduces a full path to its last one — so
/// there is no directory left to ask.
fn file_names_equal(left: &str, right: &str) -> bool {
    let left: Vec<u16> = left.encode_utf16().collect();
    let right: Vec<u16> = right.encode_utf16().collect();
    let result =
        unsafe { windows::Win32::Globalization::CompareStringOrdinal(&left, &right, true) };

    // A zero return means the call itself failed, and counts as a match.
    result != windows::Win32::Globalization::CSTR_LESS_THAN
        && result != windows::Win32::Globalization::CSTR_GREATER_THAN
}

fn file_name_component(path: &str) -> Option<&str> {
    path.rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
}

/// Whether collection may capture the current tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureDecision {
    /// Capture this tick.
    Capture,
    /// The foreground process is blacklisted; skip every monitor this tick.
    SkipBlacklisted {
        /// Foreground process value exactly as supplied by the caller.
        process: String,
    },
    /// The foreground process could not be determined while a blacklist is configured;
    /// skip this tick rather than risk capturing a blacklisted window.
    SkipUnknownForeground,
}

/// Decide whether a tick may capture. `foreground_process` is `None` when the OS query failed.
pub fn decide_capture(foreground_process: Option<&str>, blacklist: &[String]) -> CaptureDecision {
    if blacklist.is_empty() {
        return CaptureDecision::Capture;
    }

    match foreground_process {
        Some(process) if is_blacklisted(process, blacklist) => CaptureDecision::SkipBlacklisted {
            process: process.to_owned(),
        },
        Some(_) => CaptureDecision::Capture,
        None => CaptureDecision::SkipUnknownForeground,
    }
}

/// Manual collection pause state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseState {
    /// Collecting normally.
    Running,
    /// Paused until this instant; resumes automatically afterwards.
    Until(chrono::DateTime<chrono::Utc>),
    /// Paused until the user explicitly resumes.
    Indefinite,
}

impl PauseState {
    /// True while collection must stay stopped. `Until` expires exactly at its instant
    /// (a deadline equal to `now` is already expired).
    pub fn is_paused(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        match self {
            Self::Running => false,
            Self::Until(deadline) => now < *deadline,
            Self::Indefinite => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_start() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-25T00:00:00Z")
            .expect("the fixed test timestamp must be valid RFC 3339")
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn blacklist_matches_exe_name_case_insensitively() {
        let blacklist = ["KeePass.exe".into()];

        assert!(
            is_blacklisted(r"C:\Program Files\KeePass\KEEPASS.EXE", &blacklist),
            "the file name should match regardless of ASCII case"
        );
        assert!(
            !is_blacklisted(r"C:\Windows\explorer.exe", &blacklist),
            "a different executable file name must not match"
        );
    }

    #[test]
    fn blacklist_matches_non_ascii_names_case_insensitively() {
        let blacklist = ["КиПасс.exe".into()];

        assert!(
            is_blacklisted(r"C:\Program Files\Пример\КИПАСС.EXE", &blacklist),
            "the file name should match regardless of Unicode case"
        );
        assert!(
            !is_blacklisted(r"C:\Program Files\Пример\КИПАСЫ.EXE", &blacklist),
            "a different non-ASCII executable file name must not match"
        );
    }

    #[test]
    fn blacklist_follows_windows_file_name_identity() {
        // Every row was verified on NTFS by creating both names in one directory and observing
        // whether one file or two resulted.
        let cases = [
            ("\u{041A}i.exe", "\u{043A}i.exe", true), // Cyrillic capital ka / small ka
            ("\u{00E9}.exe", "\u{00C9}.exe", true), // Latin small e with acute / capital E with acute
            ("\u{039F}\u{03C2}.exe", "\u{039F}\u{03A3}.exe", false), // Greek omicron and final sigma / omicron and capital sigma
            ("stra\u{00DF}e.exe", "STRA\u{1E9E}E.EXE", false), // Latin small sharp s / capital sharp s
            ("stra\u{00DF}e.exe", "STRASSE.EXE", false), // Latin small sharp s / ASCII double s
            ("\u{FB01}le.exe", "FILE.EXE", false),       // Latin small ligature fi / ASCII FI
        ];

        for (entry, foreground, expected) in cases {
            assert_eq!(
                is_blacklisted(foreground, &[entry.to_owned()]),
                expected,
                "Windows file-name identity for entry {entry:?} and foreground {foreground:?} should be {expected}"
            );
        }
    }

    #[test]
    fn blacklist_entry_may_be_a_full_path() {
        let blacklist = [r"C:\Other\KeePass.exe".into()];

        assert!(
            is_blacklisted(r"D:\elsewhere\keepass.exe", &blacklist),
            "only the file-name components of the input and entry should be compared"
        );
    }

    #[test]
    fn blacklist_matches_forward_slash_paths() {
        let blacklist = ["KeePass.exe".into()];

        assert!(
            is_blacklisted("C:/Program Files/KeePass/KeePass.exe", &blacklist),
            "forward-slash paths should be supported"
        );
    }

    #[test]
    fn blacklist_does_not_substring_match() {
        let blacklist = ["pass.exe".into()];

        assert!(
            !is_blacklisted(r"C:\x\KeePass.exe", &blacklist),
            "blacklist entries must match the whole file name"
        );
    }

    #[test]
    fn empty_blacklist_never_matches() {
        assert!(
            !is_blacklisted("KeePass.exe", &[]),
            "an empty blacklist should never match any process"
        );
    }

    #[test]
    fn decide_capture_allows_everything_when_blacklist_is_empty() {
        assert_eq!(
            decide_capture(Some("keepass.exe"), &[]),
            CaptureDecision::Capture,
            "an empty blacklist should allow a known foreground process"
        );
        assert_eq!(
            decide_capture(None, &[]),
            CaptureDecision::Capture,
            "an empty blacklist should allow an unknown foreground process"
        );
    }

    #[test]
    fn decide_capture_skips_blacklisted_process() {
        let foreground_process = r"C:\Program Files\KeePass\KEEPASS.EXE";
        let blacklist = ["KeePass.exe".into()];

        assert_eq!(
            decide_capture(Some(foreground_process), &blacklist),
            CaptureDecision::SkipBlacklisted {
                process: foreground_process.to_owned(),
            },
            "the skip decision should preserve the exact process string passed by the caller"
        );
    }

    #[test]
    fn decide_capture_allows_unlisted_process() {
        let blacklist = ["KeePass.exe".into()];

        assert_eq!(
            decide_capture(Some(r"C:\Windows\explorer.exe"), &blacklist),
            CaptureDecision::Capture,
            "a process absent from the blacklist should be captured"
        );
    }

    #[test]
    fn decide_capture_fails_closed_on_unknown_foreground() {
        let blacklist = ["KeePass.exe".into()];

        assert_eq!(
            decide_capture(None, &blacklist),
            CaptureDecision::SkipUnknownForeground,
            "an unknown foreground process should fail closed when a blacklist is configured"
        );
    }

    #[test]
    fn pause_until_expires_at_its_deadline() {
        let t0 = test_start();
        let deadline = t0 + chrono::Duration::minutes(30);
        let state = PauseState::Until(deadline);

        assert!(
            state.is_paused(t0),
            "a timed pause should be active before its deadline"
        );
        assert!(
            state.is_paused(deadline - chrono::Duration::seconds(1)),
            "a timed pause should remain active one second before its deadline"
        );
        assert!(
            !state.is_paused(deadline),
            "a timed pause should expire exactly at its deadline"
        );
        assert!(
            !state.is_paused(deadline + chrono::Duration::seconds(1)),
            "a timed pause should remain expired after its deadline"
        );
    }

    #[test]
    fn pause_indefinite_requires_explicit_resume() {
        let t0 = test_start();
        let a_year_later = t0 + chrono::Duration::days(365);

        assert!(
            PauseState::Indefinite.is_paused(t0),
            "an indefinite pause should be active immediately"
        );
        assert!(
            PauseState::Indefinite.is_paused(a_year_later),
            "an indefinite pause should not expire with time"
        );
        assert!(
            !PauseState::Running.is_paused(t0),
            "the running state should not be paused immediately"
        );
        assert!(
            !PauseState::Running.is_paused(a_year_later),
            "the running state should never be paused"
        );
    }
}
