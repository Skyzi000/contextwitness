// Image housekeeping: the startup orphan pass and the periodic retention sweep.

use cw_core::config::{Config, DataPaths};
use tracing::{error, info, warn};

/// Plan Task 21's cadence for the retention sweep.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Run retention on this thread until the process ends: once now, then hourly.
pub fn run(mut conn: rusqlite::Connection, paths: DataPaths, config: Config) -> ! {
    loop {
        match cw_store::retention::sweep(
            &mut conn,
            &paths.images(),
            chrono::Utc::now(),
            config.storage.image_retention_days,
            config.storage.image_retention_max_gib,
        ) {
            Ok(swept) => {
                if swept.expired + swept.over_budget + swept.skipped > 0 {
                    info!(
                        expired = swept.expired,
                        over_budget = swept.over_budget,
                        freed_bytes = swept.freed_bytes,
                        skipped = swept.skipped,
                        "retention sweep"
                    );
                }
            }
            Err(error) => error!("retention sweep failed: {error}"),
        }
        std::thread::sleep(SWEEP_INTERVAL);
    }
}

/// Collect image files a previous run renamed into place but never registered, and report rows
/// whose file is gone. Startup only: while a tick is saving, an unregistered file is indistinguish-
/// able from an orphan.
pub fn sweep_orphans(conn: &rusqlite::Connection, paths: &DataPaths) {
    match cw_store::images::sweep_orphan_files(conn, &paths.images()) {
        Ok(0) => {}
        Ok(removed) => info!(removed, "collected unregistered image files"),
        Err(error) => error!("sweeping orphan image files failed: {error}"),
    }
    match cw_store::images::orphan_rows(conn, &paths.images()) {
        Ok(rows) if rows.is_empty() => {}
        // Reported, never deleted: the row is the record that the image existed, and its OCR text
        // lives in the observation either way.
        Ok(rows) => warn!(
            count = rows.len(),
            first = %rows[0],
            "image rows whose file is missing"
        ),
        Err(error) => error!("checking for missing image files failed: {error}"),
    }
}
