//! Accounting audit outbox publisher.
//!
//! Publishes committed ledger transitions to the target thread chain as
//! `provider_attempt_budget_transition_v1` events, exactly once, in
//! per-attempt transition order. This is a daemon-only audit path — it may
//! append after thread terminality, which ordinary runtimes cannot.
//!
//! Idempotency across a crash between CAS append and outbox acknowledgement:
//! per-attempt publication is strictly ordered (the ledger only leases the
//! lowest unpublished transition), so a projection row whose
//! `transition_sequence` is at or beyond the claimed row proves the append
//! already happened; the claim is then acknowledged against the recorded
//! chain sequence instead of appending a duplicate.

use std::sync::Arc;
use std::time::Duration;

use ryeos_app::accounting_db::AccountingDb;
use ryeos_app::state_store::{NewEventRecord, StateStore};

const IDLE_POLL: Duration = Duration::from_millis(1_000);
const ERROR_BACKOFF: Duration = Duration::from_millis(5_000);
const CLAIM_LEASE_MS: i64 = 30_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

pub async fn run_publisher(ledger: Arc<AccountingDb>, store: Arc<StateStore>) {
    loop {
        let claimed = {
            let ledger = ledger.clone();
            tokio::task::spawn_blocking(move || ledger.claim_next_unpublished(now_ms(), CLAIM_LEASE_MS))
                .await
        };
        let row = match claimed {
            Ok(Ok(Some(row))) => row,
            Ok(Ok(None)) => {
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            }
            Ok(Err(error)) => {
                tracing::error!(%error, "accounting outbox claim failed");
                tokio::time::sleep(ERROR_BACKOFF).await;
                continue;
            }
            Err(join_error) => {
                tracing::error!(%join_error, "accounting outbox claim task failed");
                tokio::time::sleep(ERROR_BACKOFF).await;
                continue;
            }
        };

        let ledger_for_publish = ledger.clone();
        let store_for_publish = store.clone();
        let published = tokio::task::spawn_blocking(move || {
            publish_one(&ledger_for_publish, &store_for_publish, row)
        })
        .await;
        match published {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "accounting outbox publication failed; will retry");
                tokio::time::sleep(ERROR_BACKOFF).await;
            }
            Err(join_error) => {
                tracing::error!(%join_error, "accounting outbox publish task failed");
                tokio::time::sleep(ERROR_BACKOFF).await;
            }
        }
    }
}

fn publish_one(
    ledger: &AccountingDb,
    store: &StateStore,
    row: ryeos_app::accounting_db::OutboxRow,
) -> anyhow::Result<()> {
    // Exact-once recovery: ordered per-attempt publication means a projected
    // row at or beyond this sequence proves a prior append reached the chain
    // before its outbox acknowledgement was lost. Runtimes cannot author this
    // event type (the event service refuses it), so every projected row is
    // daemon-authored; for the equal-sequence case we additionally require
    // the projected row to match the claimed payload before acknowledging —
    // a divergent row is an integrity failure, never a silent skip.
    if let Some(projected) = store.get_provider_attempt_budget_latest(&row.attempt_id)? {
        let claimed_sequence = i64::from(row.transition_sequence);
        if projected.transition_sequence == claimed_sequence {
            let claimed_transition = row
                .payload
                .get("transition")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let claimed_occurred = row
                .payload
                .get("occurred_at_ms")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_default();
            if projected.transition != claimed_transition
                || projected.occurred_at_ms != claimed_occurred
            {
                anyhow::bail!(
                    "outbox recovery integrity failure for attempt {} sequence {}: projected \
                     `{}`@{} contradicts the committed ledger transition `{}`@{}",
                    row.attempt_id,
                    claimed_sequence,
                    projected.transition,
                    projected.occurred_at_ms,
                    claimed_transition,
                    claimed_occurred
                );
            }
            ledger.mark_outbox_published(row.outbox_seq, projected.chain_seq)?;
            return Ok(());
        }
        if projected.transition_sequence > claimed_sequence {
            ledger.mark_outbox_published(row.outbox_seq, projected.chain_seq)?;
            return Ok(());
        }
    }

    let thread_id = row
        .payload
        .get("thread_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("outbox payload has no thread_id"))?
        .to_owned();
    let record = NewEventRecord {
        event_type: ryeos_state::event_types::PROVIDER_ATTEMPT_BUDGET_TRANSITION_V1.to_owned(),
        storage_class: "indexed".to_owned(),
        payload: row.payload.clone(),
    };
    let persisted = store.append_events(&row.audit_chain_root_id, &thread_id, &[record])?;
    let chain_seq = persisted
        .first()
        .map(|event| event.chain_seq)
        .ok_or_else(|| anyhow::anyhow!("accounting transition append returned no record"))?;
    ledger.mark_outbox_published(row.outbox_seq, chain_seq)?;
    Ok(())
}
