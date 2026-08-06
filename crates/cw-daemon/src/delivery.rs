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
    let mut bank_ready = false;

    loop {
        // A full batch *delivered* means a backlog is draining and the next pass should run now.
        // Deliveries, not attempts: a pass of nothing but failures must fall into the idle wait,
        // or a server answering with a short Retry-After would be re-asked in a hot loop with no
        // backoff at all.
        match deliver_batch(&mut conn, &client, &config, &mut bank_ready) {
            Ok(BATCH..) => continue,
            Ok(_) => {}
            Err(error) => error!("delivery pass failed: {error}"),
        }
        std::thread::sleep(IDLE);
    }
}

/// One pass over what is due, answering how many entries were delivered.
fn deliver_batch(
    conn: &mut rusqlite::Connection,
    client: &HindsightClient,
    config: &cw_core::config::Config,
    bank_ready: &mut bool,
) -> Result<u32, Box<dyn std::error::Error>> {
    let due = outbox::fetch_due(conn, chrono::Utc::now(), BATCH)?;
    if due.is_empty() {
        return Ok(0);
    }
    // Asked for once, and only when there is something to send: a bank that could not be reached
    // leaves the entries pending for the next pass rather than dropping the worker.
    if !*bank_ready {
        if let Err(error) = client.ensure_bank(&config.hindsight.bank_id) {
            warn!(bank = %config.hindsight.bank_id, "hindsight bank is not ready: {error}");
            return Ok(0);
        }
        *bank_ready = true;
    }

    let mut delivered = 0;
    for entry in due {
        if !outbox::mark_delivering(conn, entry.episode_id)? {
            // Somebody else has it, or it is no longer due.
            continue;
        }
        let now = chrono::Utc::now();
        // The stored snapshot is the wire form; this only checks that it is JSON, because splicing
        // text that is not into the request body would corrupt every item in it.
        let metadata = match serde_json::value::RawValue::from_string(entry.metadata_json) {
            Ok(metadata) => metadata,
            Err(error) => {
                let message = format!("stored metadata is not valid JSON: {error}");
                error!(document = %entry.document_id, "{message}");
                outbox::mark_failed(conn, entry.episode_id, now, Retry::Never, &message)?;
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

        match client.retain(&config.hindsight.bank_id, &item) {
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

    Ok(delivered)
}
