use chrono::TimeDelta;
use cw_sink_hindsight::{Credentials, HindsightClient, RetainItem};
use cw_store::control::HealthKey;
use cw_store::outbox::{self, Retry};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{CreateMutexW, INFINITE, WaitForSingleObject};

/// How long the worker waits when nothing is due.
const IDLE: std::time::Duration = std::time::Duration::from_secs(30);
/// Entries claimed per pass. One entry carries a whole window's OCR text, so a backlog is read a
/// few at a time rather than in one allocation.
const BATCH: u32 = 4;

/// Whether the bank has been made ready, and when it may be asked again if it has not. Owned by
/// [`run`], so what one pass learned outlives it.
#[derive(Default)]
struct BankGate {
    ready: bool,
    attempts: u32,
    not_before: Option<chrono::DateTime<chrono::Utc>>,
}

/// Run the delivery worker on this thread. Returns as soon as it learns that delivery is not
/// configured: the outbox keeps filling, and a run after `contextwitness setup` picks it up.
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
    match outbox::requeue_delivering(&conn) {
        Ok(0) => {}
        Ok(requeued) => info!(requeued, "requeued deliveries left in flight"),
        Err(error) => error!("requeueing in-flight deliveries failed: {error}"),
    }
    let mut bank = BankGate::default();

    loop {
        match deliver_batch(&mut conn, &client, &config, &mut bank) {
            Ok(BATCH..) => continue,
            Ok(_) => {}
            Err(error) => {
                error!("delivery pass failed: {error}");
                match outbox::requeue_delivering(&conn) {
                    Ok(0) => {}
                    Ok(count) => info!(count, "requeued entries the failed pass left claimed"),
                    Err(error) => error!("claimed entries could not be requeued: {error}"),
                }
            }
        }
        std::thread::sleep(IDLE);
    }
}

/// Wait until this process is the one delivering for `data_dir`, and answer with the claim it then
/// holds. When the mutex cannot be created it answers `None`; when a wait fails it answers the
/// handle without the claim; either way the worker warns and runs unguarded. `Global\`, unlike the
/// daemon's own per-session instance claim: two interactive sessions can be pointed at one data
/// directory, and it is that directory's outbox rows — not the session — that only one worker
/// should touch.
///
/// The handle is never closed: the OS drops it however the process dies, so there is no stale claim
/// to recognize and clean up after a crash. `HANDLE` closes nothing when the binding goes, which is
/// what lets the caller simply hold it.
fn claim_sole_deliverer(data_dir: &std::path::Path) -> Option<HANDLE> {
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

    Some(handle)
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

/// One pass over what is due, answering how many entries were delivered.
fn deliver_batch(
    conn: &mut rusqlite::Connection,
    client: &HindsightClient,
    config: &cw_core::config::Config,
    bank: &mut BankGate,
) -> Result<u32, Box<dyn std::error::Error>> {
    let due = outbox::fetch_due(conn, chrono::Utc::now(), BATCH)?;
    if due.is_empty() {
        return Ok(0);
    }
    if !bank.ready {
        if bank.not_before.is_some_and(|at| chrono::Utc::now() < at) {
            return Ok(0);
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
            return Ok(0);
        }
        bank.ready = true;
    }

    let mut delivered = 0;
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
        let item = RetainItem {
            document_id: &entry.document_id,
            content: &entry.content,
            timestamp: entry.end_at,
            context: &config.hindsight.context_label,
            metadata: &metadata,
        };

        let outcome = client.retain(&config.hindsight.bank_id, &item);
        let now = chrono::Utc::now();
        match outcome {
            Ok(()) => {
                outbox::mark_delivered(conn, entry.episode_id)?;
                cw_store::control::set_health(conn, HealthKey::LastDelivery, now)?;
                delivered += 1;
                debug!(document = %entry.document_id, "delivered episode");
            }
            Err(error) => {
                // The sink's message, which keeps the token and the response body — which would
                // echo screen text back — out of what is stored and logged.
                let message = error.to_string();
                let retry = if error.is_bank_missing() {
                    bank.ready = false;
                    bank.not_before = None;
                    info!(
                        bank = %config.hindsight.bank_id,
                        "hindsight bank is missing, so the next pass checks it again"
                    );
                    Retry::Backoff
                } else if error.is_retryable() {
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

    Ok(delivered)
}
