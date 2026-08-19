use chrono::TimeDelta;
use cw_sink_hindsight::{Credentials, DeliveryError, HindsightClient, RetainItem, RetainOutcome};
use cw_store::control::HealthKey;
use cw_store::outbox::{self, Retry};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{
    CreateMutexW, INFINITE, ReleaseMutex, WaitForSingleObject,
};

/// How long the worker waits when nothing is due.
const IDLE: std::time::Duration = std::time::Duration::from_secs(30);
/// Entries claimed per pass. One entry carries a whole window's OCR text, so a backlog is read a
/// few at a time rather than in one allocation.
const BATCH: u32 = 4;
/// The still-processing poll waits a tenth of the episode's age, within these bounds: a fresh
/// episode is polled promptly, and a stale backlog is not re-polled every 30 seconds.
const POLL_FLOOR_SECONDS: i64 = 30;
const POLL_CEILING_SECONDS: i64 = 900;

fn poll_delay(age: TimeDelta) -> TimeDelta {
    (age / 10).clamp(
        TimeDelta::seconds(POLL_FLOOR_SECONDS),
        TimeDelta::seconds(POLL_CEILING_SECONDS),
    )
}

/// The stuck warn's window — the observed wait and the throttle are deliberately one 15-minute
/// span — kept apart from the poll pacing clamp so retuning one cannot silently move the other.
/// The delivery-staleness bar sits one episode period above it: deliveries are serial, so by the
/// time a fresh episode has waited this window the previous delivery is naturally a period old.
const STUCK_WARN_SECONDS: i64 = 900;

/// How long a server-failed operation waits between watch polls. Flat and long: the watch is
/// for a human-issued retry on the server, and failed entries paced like fresh polls would
/// spend most of [`BATCH`] per [`IDLE`] watching instead of delivering once a backlog grows.
const FAILED_WATCH_SECONDS: i64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StuckWarn {
    /// A server whose background worker is disabled accepts every submission and holds it
    /// pending forever — from here that looks exactly like slow extraction, so the one usable
    /// signal is that nothing has ever completed.
    NeverDelivered,
    Wedged,
}

/// Whether the observed wait has earned the throttled stuck warn, and which flavor. The wait is
/// measured from this worker's first sighting — an old episode's age is not the server's holding
/// time — and a long wait alone stays quiet while deliveries land: the signal is "in flight but
/// nothing completes". `period` is the episode window length: a healthy serial pipeline delivers
/// at most once per period, so a delivery younger than the period plus the window is the
/// pipeline's own cadence, not a wedge.
fn stuck_warn(
    waited: TimeDelta,
    period: TimeDelta,
    in_flight: &InFlight,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<StuckWarn> {
    let window = TimeDelta::seconds(STUCK_WARN_SECONDS);
    let stalled = in_flight
        .last_delivery
        .is_none_or(|at| now - at >= window + period);
    if waited >= window && stalled && in_flight.warned_at.is_none_or(|at| now - at >= window) {
        Some(if in_flight.last_delivery.is_none() {
            StuckWarn::NeverDelivered
        } else {
            StuckWarn::Wedged
        })
    } else {
        None
    }
}

/// Whether the bank has been made ready, and when it may be asked again if it has not. Owned by
/// [`run`], so what one pass learned outlives it.
#[derive(Default)]
struct BankGate {
    ready: bool,
    attempts: u32,
    not_before: Option<chrono::DateTime<chrono::Utc>>,
}

/// What this worker has watched the server hold in flight. Owned by [`run`]; the delivery clock
/// is seeded from the durable record so a restart does not read as a server delivering nothing.
#[derive(Default)]
struct InFlight {
    first_seen: std::collections::HashMap<ulid::Ulid, chrono::DateTime<chrono::Utc>>,
    /// Episodes whose server-side failure this run has already warned about, so the watch's
    /// every-poll observation does not repeat the warn.
    failed_warned: std::collections::HashSet<ulid::Ulid>,
    last_delivery: Option<chrono::DateTime<chrono::Utc>>,
    warned_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Run the delivery worker on this thread. Returns as soon as it learns that delivery is not
/// configured — the outbox keeps filling, and a run after `contextwitness setup` picks it up —
/// or that the server cannot run this client, which only a restart after the server upgrade
/// resumes.
/// Configured, it waits until it holds the machine-wide claim on `data_dir` — so the outbox rows
/// of one directory have one deliverer however many daemons reach it — unless the claim cannot be
/// created or taken; that is warned about and the worker then delivers unguarded.
pub fn run(
    mut conn: rusqlite::Connection,
    data_dir: std::path::PathBuf,
    config: cw_core::config::Config,
) {
    let credentials = match Credentials::load() {
        Ok(Some(credentials)) => credentials,
        Ok(None) => {
            info!(
                "hindsight delivery is not configured, so episodes stay queued locally; run \
                 `contextwitness setup` to configure it"
            );
            return;
        }
        Err(error) => {
            error!("hindsight credentials could not be read: {error}");
            return;
        }
    };
    info!(url = %credentials.api_url(), "delivering episodes to hindsight");
    let client = match HindsightClient::new(credentials) {
        Ok(client) => client,
        Err(error) => {
            error!("the hindsight client could not be built: {error}");
            return;
        }
    };
    let _claim = claim_sole_deliverer(&data_dir);
    requeue_until_success(&conn);
    let mut bank = BankGate::default();
    let last_delivery = match cw_store::control::get_health(&conn, HealthKey::LastDelivery) {
        Ok(at) => at,
        Err(error) => {
            // Unknown history must not become the "never delivered" claim the stuck warn
            // keys off `None`, so the degrade leans toward a recent delivery instead.
            warn!("the last-delivery record could not be read: {error}");
            Some(chrono::Utc::now())
        }
    };
    let mut in_flight = InFlight {
        last_delivery,
        ..InFlight::default()
    };

    loop {
        match deliver_batch(&mut conn, &client, &config, &mut bank, &mut in_flight) {
            Ok(Pass::Progressed(BATCH..)) => continue,
            Ok(Pass::Progressed(_)) => {}
            Ok(Pass::ServerUnsupported) => {
                error!(
                    "the hindsight server cannot run this client, so delivery is stopped and \
                     episodes stay queued; upgrade the server, then restart contextwitness"
                );
                return;
            }
            Err(error) => {
                error!("delivery pass failed: {error}");
                requeue_until_success(&conn);
            }
        }
        std::thread::sleep(IDLE);
    }
}

/// Return claimed entries to the queue, waiting out failures: a `delivering` row is found by no
/// later pass, so delivering again before the requeue has landed would strand whatever it covers
/// for the life of the process.
fn requeue_until_success(conn: &rusqlite::Connection) {
    retry_requeue(
        || outbox::requeue_delivering(conn),
        || std::thread::sleep(IDLE),
    );
}

/// The retry of [`requeue_until_success`], with the attempt and the wait handed in so the failure
/// leg holds still for a test.
fn retry_requeue(
    mut requeue: impl FnMut() -> Result<usize, cw_store::StoreError>,
    mut wait: impl FnMut(),
) {
    loop {
        match requeue() {
            Ok(0) => return,
            Ok(requeued) => {
                info!(requeued, "requeued deliveries left in flight");
                return;
            }
            Err(error) => {
                error!("requeueing in-flight deliveries failed: {error}");
                wait();
            }
        }
    }
}

/// Wait until this process is the one delivering for `data_dir`, and answer with the claim it then
/// holds. When the mutex cannot be created it answers `None`; when a wait fails it answers the
/// handle without the claim; either way the worker warns and runs unguarded. `Global\`, unlike the
/// daemon's own per-session instance claim: two interactive sessions can be pointed at one data
/// directory, and it is that directory's outbox rows — not the session — that only one worker
/// should touch.
///
/// The claim is dropped when the worker returns — the server-unsupported exit — releasing the
/// mutex to whatever standby waits on it; a crash needs no cleanup, the OS drops the handle with
/// the process.
fn claim_sole_deliverer(data_dir: &std::path::Path) -> Option<Claim> {
    let path = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    // A digest, never the path: `Global\` names are enumerable by every user on the machine, and a
    // data directory's spelling usually carries the name of the user who owns it.
    let name = windows::core::HSTRING::from(format!(
        "Global\\ContextWitness-delivery-{:016x}",
        fnv1a(&path.to_string_lossy().to_lowercase())
    ));
    let handle = match unsafe { CreateMutexW(None, false, &name) } {
        Ok(handle) => handle,
        Err(error) => {
            warn!("the delivery claim could not be created, so this worker is unguarded: {error}");
            return None;
        }
    };

    // WAIT_ABANDONED is the previous holder having died while holding it; the claim is taken.
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 | WAIT_ABANDONED => {}
        WAIT_TIMEOUT => {
            info!("another contextwitness delivers for this data directory; standing by");
            match unsafe { WaitForSingleObject(handle, INFINITE) } {
                WAIT_OBJECT_0 | WAIT_ABANDONED => {}
                outcome => warn!(
                    ?outcome,
                    "waiting for the delivery claim failed, so this worker is unguarded"
                ),
            }
        }
        outcome => warn!(
            ?outcome,
            "the delivery claim could not be taken, so this worker is unguarded"
        ),
    }

    Some(Claim(handle))
}

/// The held delivery claim; dropping it hands the mutex to a standby worker instead of camping
/// on it for the rest of the process's life.
struct Claim(HANDLE);

impl Drop for Claim {
    fn drop(&mut self) {
        // ReleaseMutex fails when the claim was never actually taken — the unguarded path.
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// FNV-1a over `text`. All this has to do is spread one machine's handful of data directories over
/// distinct names; nothing reads the digest back.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    hash
}

/// What a pass concluded: how many entries were delivered, or that the server cannot run this
/// client at all. Deliveries alone count — a parked entry leaves the due set by itself, and
/// counting it would submit a cold backlog in one unpaced burst.
enum Pass {
    Progressed(u32),
    ServerUnsupported,
}

/// One pass over what is due.
fn deliver_batch(
    conn: &mut rusqlite::Connection,
    client: &HindsightClient,
    config: &cw_core::config::Config,
    bank: &mut BankGate,
    in_flight: &mut InFlight,
) -> Result<Pass, Box<dyn std::error::Error>> {
    let due = outbox::fetch_due(conn, chrono::Utc::now(), BATCH)?;
    if due.is_empty() {
        return Ok(Pass::Progressed(0));
    }
    if !bank.ready {
        if bank.not_before.is_some_and(|at| chrono::Utc::now() < at) {
            return Ok(Pass::Progressed(0));
        }
        if let Err(error) = client.ensure_bank(&config.hindsight.bank_id) {
            let now = chrono::Utc::now();
            bank.attempts += 1;
            // A permanent error backs off like a retryable one instead of ending the worker: the
            // bank is configuration state, repairable while the daemon runs.
            let delay = error
                .retry_after()
                .and_then(|after| TimeDelta::from_std(after).ok())
                .unwrap_or_else(|| outbox::backoff_delay(bank.attempts));
            bank.not_before = now.checked_add_signed(delay);
            warn!(
                bank = %config.hindsight.bank_id,
                attempts = bank.attempts,
                retry_in_seconds = delay.num_seconds(),
                "hindsight bank is not ready: {error}"
            );
            return Ok(Pass::Progressed(0));
        }
        bank.ready = true;
    }

    let mut progressed = 0;
    for entry in due {
        if !outbox::mark_delivering(conn, entry.episode_id)? {
            continue;
        }
        let metadata = match serde_json::value::RawValue::from_string(entry.metadata_json) {
            Ok(metadata) => metadata,
            Err(error) => {
                let message = format!("stored metadata is not valid JSON: {error}");
                error!(document = %entry.document_id, "{message}");
                outbox::mark_failed(
                    conn,
                    entry.episode_id,
                    chrono::Utc::now(),
                    Retry::Never,
                    &message,
                )?;
                continue;
            }
        };
        let episode_id = entry.episode_id.to_string();
        let item = RetainItem {
            episode_id: &episode_id,
            document_id: &entry.document_id,
            content: &entry.content,
            timestamp: entry.end_at,
            context: &config.hindsight.context_label,
            metadata: &metadata,
        };

        let outcome = client.retain(&config.hindsight.bank_id, &item);
        let now = chrono::Utc::now();
        match outcome {
            Ok(RetainOutcome::Delivered) => {
                outbox::mark_delivered(conn, entry.episode_id)?;
                cw_store::control::set_health(conn, HealthKey::LastDelivery, now)?;
                in_flight.first_seen.remove(&entry.episode_id);
                in_flight.failed_warned.remove(&entry.episode_id);
                in_flight.last_delivery = Some(now);
                progressed += 1;
                debug!(document = %entry.document_id, "delivered episode");
            }
            Ok(RetainOutcome::Processing { operation_id }) => {
                // A failed operation retried on the server is processing anew: if it fails
                // again, that failure deserves its own warn.
                in_flight.failed_warned.remove(&entry.episode_id);
                let note =
                    format!("hindsight is still processing the episode (operation {operation_id})");
                let until = now
                    .checked_add_signed(poll_delay(now - entry.end_at))
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC);
                let first_seen = *in_flight.first_seen.entry(entry.episode_id).or_insert(now);
                let waited = now - first_seen;
                let period = TimeDelta::minutes(i64::from(config.episode.window_minutes));
                if let Some(kind) = stuck_warn(waited, period, in_flight, now) {
                    in_flight.warned_at = Some(now);
                    let message = match kind {
                        StuckWarn::NeverDelivered => {
                            "no retain has ever completed; check that the hindsight server's background worker is running"
                        }
                        StuckWarn::Wedged => {
                            "the retain is still in flight and nothing has completed recently; the server may be wedged"
                        }
                    };
                    warn!(
                        document = %entry.document_id,
                        operation = %operation_id,
                        waited_minutes = waited.num_minutes(),
                        in_flight = in_flight.first_seen.len(),
                        "{message}"
                    );
                } else {
                    debug!(document = %entry.document_id, "{note}");
                }
                outbox::mark_waiting(conn, entry.episode_id, until, &note)?;
            }
            Ok(RetainOutcome::Failed { operation_id }) => {
                let note = format!(
                    "hindsight failed the retain (operation {operation_id}); the reason and a \
                     retry are on the server's operations API, and this episode keeps watching \
                     for that retry"
                );
                let until = now
                    .checked_add_signed(TimeDelta::seconds(FAILED_WATCH_SECONDS))
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC);
                // Nothing is running server-side, so the stuck warn must not count it in flight.
                in_flight.first_seen.remove(&entry.episode_id);
                if in_flight.failed_warned.insert(entry.episode_id) {
                    warn!(document = %entry.document_id, operation = %operation_id, "{note}");
                } else {
                    debug!(document = %entry.document_id, "{note}");
                }
                outbox::mark_waiting(conn, entry.episode_id, until, &note)?;
            }
            Err(error @ DeliveryError::Unsupported { .. }) => {
                let message = error.to_string();
                warn!(document = %entry.document_id, "delivery failed: {message}");
                outbox::mark_waiting(conn, entry.episode_id, now, &message)?;
                return Ok(Pass::ServerUnsupported);
            }
            Err(error) => {
                // Within the error path, only a condemned entry leaves the map: a retryable
                // failure keeps its clock so flaky polls cannot silence the stuck warn.
                if !error.is_retryable() {
                    in_flight.first_seen.remove(&entry.episode_id);
                    in_flight.failed_warned.remove(&entry.episode_id);
                }
                // The sink's message, which keeps the token and the response body — which would
                // echo screen text back — out of what is stored and logged.
                let message = error.to_string();
                let retry = if error.is_retryable() {
                    error
                        .retry_after()
                        .and_then(|after| TimeDelta::from_std(after).ok())
                        .and_then(|after| now.checked_add_signed(after))
                        .map_or(Retry::Backoff, Retry::At)
                } else {
                    Retry::Never
                };
                warn!(
                    document = %entry.document_id,
                    attempts = entry.attempts + 1,
                    retryable = error.is_retryable(),
                    "delivery failed: {message}"
                );
                outbox::mark_failed(conn, entry.episode_id, now, retry, &message)?;
            }
        }
    }

    Ok(Pass::Progressed(progressed))
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use cw_store::StoreError;

    #[test]
    fn the_poll_delay_grows_with_the_episodes_age() {
        assert_eq!(super::poll_delay(TimeDelta::zero()), TimeDelta::seconds(30));
        assert_eq!(
            super::poll_delay(TimeDelta::seconds(-3600)),
            TimeDelta::seconds(30),
            "a window ending in the future must still poll at the floor"
        );
        assert_eq!(
            super::poll_delay(TimeDelta::seconds(3000)),
            TimeDelta::seconds(300)
        );
        assert_eq!(
            super::poll_delay(TimeDelta::days(1)),
            TimeDelta::seconds(900)
        );
    }

    #[test]
    fn the_stuck_warn_fires_only_wedged_and_throttled() {
        use super::{STUCK_WARN_SECONDS, StuckWarn, stuck_warn};

        let now = chrono::Utc::now();
        let window = TimeDelta::seconds(STUCK_WARN_SECONDS);
        let period = TimeDelta::minutes(5);
        let in_flight =
            |delivered_ago: Option<TimeDelta>, warned_ago: Option<TimeDelta>| super::InFlight {
                last_delivery: delivered_ago.map(|ago| now - ago),
                warned_at: warned_ago.map(|ago| now - ago),
                ..super::InFlight::default()
            };

        assert_eq!(
            stuck_warn(window / 2, period, &in_flight(None, None), now),
            None,
            "a short wait never warns"
        );
        assert_eq!(
            stuck_warn(window, period, &in_flight(None, None), now),
            Some(StuckWarn::NeverDelivered),
            "zero completions ever is the background-worker signal"
        );
        assert_eq!(
            stuck_warn(window, period, &in_flight(Some(window / 3), None), now),
            None,
            "a delivery within the window is a draining backlog, not a wedge"
        );
        assert_eq!(
            stuck_warn(window, period, &in_flight(Some(window * 2), None), now),
            Some(StuckWarn::Wedged),
            "a delivering install must never be told nothing has ever completed"
        );
        assert_eq!(
            stuck_warn(
                window,
                period,
                &in_flight(Some(window * 2), Some(window / 3)),
                now
            ),
            None,
            "the warn is throttled to one per window"
        );
        assert_eq!(
            stuck_warn(
                window,
                period,
                &in_flight(Some(window * 2), Some(window)),
                now
            ),
            Some(StuckWarn::Wedged),
            "the throttle expires after a full window"
        );

        let hourly = TimeDelta::minutes(60);
        assert_eq!(
            stuck_warn(
                window,
                hourly,
                &in_flight(Some(hourly + window / 3), None),
                now
            ),
            None,
            "an hourly pipeline's previous delivery is a period old by nature, not a wedge"
        );
        assert_eq!(
            stuck_warn(window, hourly, &in_flight(Some(hourly + window), None), now),
            Some(StuckWarn::Wedged),
            "a gap past the period plus the window is no longer the pipeline's own cadence"
        );
    }

    #[test]
    fn a_failed_requeue_waits_and_asks_again() {
        let events = std::cell::RefCell::new(Vec::new());
        let mut attempts = 0;
        super::retry_requeue(
            || {
                attempts += 1;
                events.borrow_mut().push("attempt");
                if attempts == 1 {
                    Err(StoreError::Sql {
                        source: rusqlite::Error::InvalidQuery,
                    })
                } else {
                    Ok(1)
                }
            },
            || events.borrow_mut().push("wait"),
        );
        assert_eq!(*events.borrow(), ["attempt", "wait", "attempt"]);
    }
}
