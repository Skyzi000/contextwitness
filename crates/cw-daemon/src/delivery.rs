// The outbox worker: claim what is due, retain it, record what happened.

use chrono::TimeDelta;
use cw_sink_hindsight::{Credentials, HindsightClient, RetainItem};
use cw_store::control::HealthKey;
use cw_store::outbox::{self, Retry};
use tracing::{debug, error, info, warn};

/// How long the worker waits when nothing is due. Episodes arrive once per window, and a retry
/// waits at least `outbox::backoff_delay`'s first step, so there is nothing to gain by looking
/// sooner.
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
pub fn run(mut conn: rusqlite::Connection, config: cw_core::config::Config) {
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
    let mut bank = BankGate::default();

    loop {
        // A full batch *delivered* means a backlog is draining and the next pass should run now.
        // Deliveries, not attempts: a pass of nothing but failures must fall into the idle wait,
        // or a server answering with a short Retry-After would be re-asked in a hot loop with no
        // backoff at all.
        match deliver_batch(&mut conn, &client, &config, &mut bank) {
            Ok(BATCH..) => continue,
            Ok(_) => {}
            Err(error) => {
                error!("delivery pass failed: {error}");
                // A pass that dies between the claim and the state write leaves its entry
                // 'delivering', which `fetch_due` does not see. One worker runs, so any
                // 'delivering' entry at a pass boundary is by definition abandoned. If the store
                // is what failed this fails too, and the next pass tries again.
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
    // Asked for once, and only when there is something to send: a bank that could not be reached
    // leaves the entries pending for a later pass rather than dropping the worker, and is asked
    // again on the same ladder the outbox retries on rather than once every idle wait.
    if !bank.ready {
        if bank.not_before.is_some_and(|at| chrono::Utc::now() < at) {
            // Reading the clock is all this pass costs; nothing goes over the wire until the
            // backoff has run out.
            return Ok(0);
        }
        if let Err(error) = client.ensure_bank(&config.hindsight.bank_id) {
            // Read after the call, for the reason the retain loop below gives.
            let now = chrono::Utc::now();
            bank.attempts += 1;
            // A permanent error backs off exactly like a retryable one instead of ending the
            // worker: the bank is configuration state, like the credentials, not payload state, so
            // it can be repaired while the daemon runs — and a retry that keeps coming, however
            // slowly, is what picks that repair up.
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
            // Somebody else has it, or it is no longer due.
            continue;
        }
        // The stored snapshot is the wire form; this only checks that it is JSON, because splicing
        // text that is not into the request body would corrupt every item in it.
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
        // Read after the call, not before it: retain blocks for up to the sink's HTTP timeout, so a
        // `now` taken beforehand can already be in the past by the time a `Retry-After` or a
        // backoff step is measured from it.
        let now = chrono::Utc::now();
        match outcome {
            Ok(()) => {
                outbox::mark_delivered(conn, entry.episode_id)?;
                cw_store::control::set_health(conn, HealthKey::LastDelivery, now)?;
                delivered += 1;
                debug!(document = %entry.document_id, "delivered episode");
            }
            Err(error) => {
                // The message is the sink's, which keeps the token and the response body — which
                // would echo screen text back — out of what is stored and logged.
                let message = error.to_string();
                // A 404 says the bank is gone from under a gate that has already opened, so every
                // retain answers the same until it is back. The gate shuts so the next pass runs
                // `ensure_bank`, which creates a missing bank, and a further ensure failure rides
                // the gate's own ladder — `attempts` is left where it is, so a bank that keeps
                // disappearing climbs that ladder rather than restarting it. The episode backs off
                // rather than being condemned, for the reason the sink records for 401 and 403: the
                // bank's existence is configuration state, repairable while the daemon runs, and an
                // episode has to outlive the gap to be there when the repair lands.
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
