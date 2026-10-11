use cw_core::config::{Config, DataPaths};
use tracing::{error, info, warn};

const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// The cadence taken instead while a pass has work it did not reach.
const SHORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Run retention, the image-row scan and the orphan-file sweep on this thread until the process
/// ends: once now, then hourly, or every minute for as long as either paged pass has more to do
/// than one wake may take. The file sweep walks the whole image tree, so it keeps its hourly
/// cadence through those catch-up wakes instead of re-walking every minute.
pub fn run(mut conn: rusqlite::Connection, paths: DataPaths, config: Config) -> ! {
    let mut sweep_due = std::time::Instant::now();
    loop {
        // A pass that failed shortens nothing: what it did not reach is unknown, and a wake every
        // minute would only repeat the failure.
        let mut scan_finished = true;
        let mut retention_truncated = false;
        match cw_store::retention::sweep(
            &mut conn,
            &paths.images(),
            chrono::Utc::now(),
            config.storage.image_retention_days,
            config.storage.image_retention_max_gib,
        ) {
            Ok(swept) => {
                retention_truncated = swept.truncated;
                if swept.expired + swept.over_budget + swept.skipped > 0 {
                    info!(
                        expired = swept.expired,
                        over_budget = swept.over_budget,
                        freed_bytes = swept.freed_bytes,
                        skipped = swept.skipped,
                        missing = swept.missing,
                        truncated = swept.truncated,
                        "retention sweep"
                    );
                }
            }
            Err(error) => error!("retention sweep failed: {error}"),
        }
        match cw_store::images::scan_orphan_rows(&mut conn, &paths.images()) {
            Ok(scan) => {
                scan_finished = scan.finished;
                // Reported, never deleted: the row is the record that the image existed.
                if let Some(first) = scan.missing.first() {
                    warn!(
                        count = scan.missing.len(),
                        first = %first,
                        "image rows whose file is missing"
                    );
                }
                if scan.undecodable > 0 {
                    warn!(
                        undecodable = scan.undecodable,
                        "image rows that cannot be read back"
                    );
                }
            }
            Err(error) => error!("scanning for missing image files failed: {error}"),
        }
        if std::time::Instant::now() >= sweep_due {
            match cw_store::images::sweep_orphan_files(&mut conn, &paths.images()) {
                Ok(0) => {}
                Ok(removed) => info!(removed, "collected unregistered image files"),
                Err(error) => error!("sweeping orphan image files failed: {error}"),
            }
            sweep_due = std::time::Instant::now() + SWEEP_INTERVAL;
        }
        std::thread::sleep(next_interval(scan_finished, retention_truncated));
    }
}

fn next_interval(scan_finished: bool, retention_truncated: bool) -> std::time::Duration {
    if scan_finished && !retention_truncated {
        SWEEP_INTERVAL
    } else {
        SHORT_INTERVAL
    }
}

#[cfg(test)]
mod tests {
    use super::{SHORT_INTERVAL, SWEEP_INTERVAL, next_interval};

    #[test]
    fn a_pass_with_more_to_do_than_it_reached_is_woken_sooner() {
        assert_eq!(next_interval(true, false), SWEEP_INTERVAL);
        assert_eq!(next_interval(false, false), SHORT_INTERVAL);
        assert_eq!(next_interval(true, true), SHORT_INTERVAL);
        assert_eq!(next_interval(false, true), SHORT_INTERVAL);
    }
}
