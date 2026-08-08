use winreg::enums::{KEY_READ, KEY_SET_VALUE};

/// Where Windows reads this user's logon entries from.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The one value under [`RUN_KEY`] this program writes. Everything else there is somebody else's.
const VALUE_NAME: &str = "ContextWitness";

/// Register this executable to start at logon.
pub fn enable() -> std::io::Result<()> {
    // Windows splits an unquoted command on spaces, and under `C:\Program Files\...` that hands
    // the loader a truncated executable name.
    let command = format!("\"{}\" run", std::env::current_exe()?.display());

    run_key(KEY_SET_VALUE)?.set_value(VALUE_NAME, &command)
}

/// Remove the logon registration. Removing one that does not exist is not an error.
pub fn disable() -> std::io::Result<()> {
    match run_key(KEY_SET_VALUE)?.delete_value(VALUE_NAME) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Whether the logon registration exists.
pub fn is_enabled() -> std::io::Result<bool> {
    match run_key(KEY_READ)?.get_value::<String, _>(VALUE_NAME) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Open the Run key with only the access the caller needs. Opened rather than created: Windows
/// ships the key with the profile, so a call that had to create it is one addressing something
/// other than this user's hive.
fn run_key(access: u32) -> std::io::Result<winreg::RegKey> {
    winreg::HKCU.open_subkey_with_flags(RUN_KEY, access)
}
