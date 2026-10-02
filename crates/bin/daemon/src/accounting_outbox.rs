//! Accounting audit outbox publisher.
//!
//! Publishes committed ledger transitions to the target thread chain as
//! typed provider/resource budget transition events, exactly once, in
//! per-operation transition order. This is a daemon-only audit path — it may
//! append after thread terminality, which ordinary runtimes cannot.
//!
//! Idempotency across a crash between CAS append and outbox acknowledgement:
//! every projected accounting transition retains its unique transition ID,
//! canonical payload fingerprint, typed operation coordinate, and chain sequence.
//! Retry acknowledges only that exact identity; it never infers publication
//! from a later summary row.

use ryeos_app::accounting_db::AccountingDb;
use ryeos_app::state_store::{NewEventRecord, StateStore};
use std::sync::Arc;

const IDLE_POLL: lillux::time::Duration = lillux::time::Duration::from_millis(1_000);
const ERROR_BACKOFF: lillux::time::Duration = lillux::time::Duration::from_millis(5_000);
const CLAIM_LEASE_MS: i64 = 30_000;

fn now_ms() -> i64 {
    lillux::time::timestamp_millis()
}

pub async fn run_publisher(ledger: Arc<AccountingDb>, store: Arc<StateStore>) {
    loop {
        let claimed = {
            let ledger = ledger.clone();
            tokio::task::spawn_blocking(move || {
                ledger.claim_next_unpublished(now_ms(), CLAIM_LEASE_MS)
            })
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
    // Exact-once recovery after append-before-ack: only the complete identity
    // recorded from the projected daemon event proves this outbox row was
    // already appended.
    match row.event_type.as_str() {
        ryeos_state::event_types::PROVIDER_ATTEMPT_BUDGET_TRANSITION_V1 => {
            if let Some(projected) =
                store.get_provider_attempt_budget_transition_identity(&row.transition_id)?
            {
                if projected.attempt_id != row.operation_id
                    || projected.transition_sequence != i64::from(row.transition_sequence)
                    || projected.payload_fingerprint != row.payload_fingerprint
                {
                    anyhow::bail!(
                        "provider outbox recovery identity contradicts transition {}",
                        row.transition_id
                    );
                }
                ledger.mark_outbox_published(row.outbox_seq, projected.chain_seq)?;
                return Ok(());
            }
        }
        ryeos_state::event_types::RESOURCE_BUDGET_TRANSITION_V1 => {
            if let Some(projected) =
                store.get_resource_budget_transition_identity(&row.transition_id)?
            {
                if projected.operation_id != row.operation_id
                    || projected.transition_sequence != i64::from(row.transition_sequence)
                    || projected.payload_fingerprint != row.payload_fingerprint
                {
                    anyhow::bail!(
                        "resource outbox recovery identity contradicts transition {}",
                        row.transition_id
                    );
                }
                ledger.mark_outbox_published(row.outbox_seq, projected.chain_seq)?;
                return Ok(());
            }
        }
        ryeos_state::event_types::RESOURCE_PRECONTACT_BUDGET_TRANSITION_V1 => {
            if let Some(chain_seq) = recover_precontact_publication(store, &row)? {
                ledger.mark_outbox_published(row.outbox_seq, chain_seq)?;
                return Ok(());
            }
        }
        unknown => anyhow::bail!("accounting outbox has unknown event type `{unknown}`"),
    }

    let thread_id = row
        .payload
        .get("thread_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("outbox payload has no thread_id"))?
        .to_owned();
    let record = NewEventRecord {
        event_type: row.event_type.clone(),
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

// Audit comparison only: no executable/funding/cleanup authority is minted.
fn recover_precontact_publication(
    store: &StateStore,
    row: &ryeos_app::accounting_db::OutboxRow,
) -> anyhow::Result<Option<i64>> {
    let incoming: ryeos_accounting::ResourcePrecontactBudgetTransitionV1 =
        serde_json::from_value(row.payload.clone())?;
    incoming.validate().map_err(anyhow::Error::msg)?;
    if incoming.operation_id != row.operation_id
        || incoming.transition_id != row.transition_id
        || incoming.transition_sequence != row.transition_sequence
        || incoming.audit_chain_root_id != row.audit_chain_root_id
    {
        anyhow::bail!("precontact outbox coordinates differ from exact payload");
    }
    let fingerprint = lillux::sha256_hex(lillux::canonical_json(&row.payload)?.as_bytes());
    if fingerprint != row.payload_fingerprint {
        anyhow::bail!("precontact outbox canonical payload fingerprint differs");
    }
    let records = store.thread_events_by_type_operation_id(
        &incoming.thread_id,
        &row.event_type,
        &row.operation_id,
    )?;
    if records.len() > 2 {
        anyhow::bail!("precontact publication has excess historical transitions");
    }
    let mut decision = None;
    let mut exact = None;
    for record in records {
        let candidate: ryeos_accounting::ResourcePrecontactBudgetTransitionV1 =
            serde_json::from_value(record.payload.clone())?;
        candidate.validate().map_err(anyhow::Error::msg)?;
        // At most two precontact events exist: reservation decision
        // and authoritative no-birth release. Every comparison keeps
        // their SAME immutable original envelope, never a process.
        if candidate.operation_id != incoming.operation_id
            || candidate.owner_gate_id != incoming.owner_gate_id
            || candidate.original_reservation_id != incoming.original_reservation_id
            || candidate.envelope_digest != incoming.envelope_digest
            || candidate.capsule_hash != incoming.capsule_hash
            || candidate.budget_authority_site_id != incoming.budget_authority_site_id
            || candidate.ledger_epoch != incoming.ledger_epoch
            || candidate.execution_budget_id != incoming.execution_budget_id
            || candidate.root_chain_id != incoming.root_chain_id
            || candidate.audit_chain_root_id != incoming.audit_chain_root_id
            || candidate.thread_id != incoming.thread_id
            || candidate.launch_generation != incoming.launch_generation
            || candidate.stable_resource_id != incoming.stable_resource_id
            || candidate.authority_digest != incoming.authority_digest
            || candidate.reserved_usd_nanos != incoming.reserved_usd_nanos
            || record.chain_root_id != incoming.audit_chain_root_id
            || record.thread_id != incoming.thread_id
            || record.event_hash.is_none()
        {
            anyhow::bail!("precontact publication changed original financial authority");
        }
        match candidate.transition {
            ryeos_accounting::ResourceBudgetState::Reserved
            | ryeos_accounting::ResourceBudgetState::ReservationDenied => {
                if decision.is_some() {
                    anyhow::bail!("precontact publication repeats reservation decision");
                }
                decision = Some((candidate.transition, candidate.occurred_at_ms));
            }
            ryeos_accounting::ResourceBudgetState::ReleasedUnissued => {
                if !decision.is_some_and(|(state, time)| {
                    state == ryeos_accounting::ResourceBudgetState::Reserved
                        && time <= candidate.occurred_at_ms
                }) {
                    anyhow::bail!("precontact release lacks its original prior reserve");
                }
            }
            _ => unreachable!("validated closed precontact event"),
        }
        if candidate.transition_id == incoming.transition_id {
            let fingerprint =
                lillux::sha256_hex(lillux::canonical_json(&record.payload)?.as_bytes());
            if fingerprint != row.payload_fingerprint
                || record.payload != row.payload
                || exact.is_some()
            {
                anyhow::bail!("precontact lost-ACK publication is not exact once");
            }
            exact = Some(record.chain_seq);
        }
    }
    if incoming.transition == ryeos_accounting::ResourceBudgetState::ReleasedUnissued {
        if !decision.is_some_and(|(state, time)| {
            state == ryeos_accounting::ResourceBudgetState::Reserved
                && time <= incoming.occurred_at_ms
        }) {
            anyhow::bail!("new precontact release lacks published original reserve");
        }
    } else if decision.is_some_and(|(state, _)| state != incoming.transition) {
        anyhow::bail!("precontact outbox decision contradicts retained decision");
    }
    Ok(exact)
}

#[cfg(test)]
mod precontact_audit_tests {
    use super::*;
    use ryeos_accounting::{HexDigest, ResourceBudgetState, ResourcePrecontactBudgetTransitionV1};

    fn row(root: &str, transition: ResourceBudgetState) -> ryeos_app::accounting_db::OutboxRow {
        let sequence = if transition == ResourceBudgetState::ReleasedUnissued {
            2
        } else {
            1
        };
        let operation = "a".repeat(64);
        let event = ResourcePrecontactBudgetTransitionV1 {
            version: 1,
            transition_id: ResourcePrecontactBudgetTransitionV1::transition_id(
                &operation, sequence,
            ),
            transition_sequence: sequence,
            operation_id: operation.clone(),
            budget_authority_site_id: "audit-only-site".into(),
            ledger_epoch: 1,
            execution_budget_id: "audit-only-budget".into(),
            root_chain_id: root.into(),
            audit_chain_root_id: root.into(),
            thread_id: root.into(),
            launch_generation: "audit-only-original".into(),
            owner_gate_id: HexDigest::new("b".repeat(64)).unwrap(),
            original_reservation_id: HexDigest::new("c".repeat(64)).unwrap(),
            envelope_digest: HexDigest::new("d".repeat(64)).unwrap(),
            capsule_hash: HexDigest::new("e".repeat(64)).unwrap(),
            stable_resource_id: "audit-only-resource".into(),
            authority_digest: HexDigest::new("f".repeat(64)).unwrap(),
            transition,
            reserved_usd_nanos: 100,
            occurred_at_ms: i64::from(sequence),
        };
        event.validate().unwrap();
        let payload = serde_json::to_value(&event).unwrap();
        ryeos_app::accounting_db::OutboxRow {
            outbox_seq: 1,
            operation_id: operation,
            audit_chain_root_id: root.into(),
            transition_sequence: sequence,
            transition_id: event.transition_id,
            event_type: ryeos_state::event_types::RESOURCE_PRECONTACT_BUDGET_TRANSITION_V1.into(),
            payload_fingerprint: lillux::sha256_hex(
                lillux::canonical_json(&payload).unwrap().as_bytes(),
            ),
            payload,
        }
    }

    fn append(store: &StateStore, row: &ryeos_app::accounting_db::OutboxRow) -> i64 {
        store
            .append_events(
                &row.audit_chain_root_id,
                &row.audit_chain_root_id,
                &[NewEventRecord {
                    event_type: row.event_type.clone(),
                    storage_class: "indexed".into(),
                    payload: row.payload.clone(),
                }],
            )
            .unwrap()[0]
            .chain_seq
    }

    // These exercise the actual CAS/projection lookup used after a lost ACK.
    // Fabricated audit metadata is comparison data, never funding authority.
    #[test]
    fn precontact_lost_ack_recovers_exact_original_audit_without_process_projection() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = ryeos_app::handler_context::budget_test_support::build(
            dir.path(),
            lillux::time::Duration::from_secs(60),
        )
        .unwrap();
        let store = fixture.state.state_store.as_ref();
        let reserve = row(&fixture.thread_id, ResourceBudgetState::Reserved);
        assert_eq!(
            recover_precontact_publication(store, &reserve).unwrap(),
            None
        );
        let seq = append(store, &reserve);
        assert_eq!(
            recover_precontact_publication(store, &reserve).unwrap(),
            Some(seq)
        );
        assert!(
            store
                .get_resource_budget_transition_identity(&reserve.transition_id)
                .unwrap()
                .is_none()
        );
        let release = row(&fixture.thread_id, ResourceBudgetState::ReleasedUnissued);
        assert_eq!(
            recover_precontact_publication(store, &release).unwrap(),
            None
        );
        let release_seq = append(store, &release);
        assert_eq!(
            recover_precontact_publication(store, &release).unwrap(),
            Some(release_seq)
        );
        let mut changed = row(&fixture.thread_id, ResourceBudgetState::Reserved);
        changed.payload["occurred_at_ms"] = serde_json::json!(100);
        changed.payload_fingerprint =
            lillux::sha256_hex(lillux::canonical_json(&changed.payload).unwrap().as_bytes());
        assert!(recover_precontact_publication(store, &changed).is_err());
        append(store, &reserve);
        assert!(recover_precontact_publication(store, &reserve).is_err());
    }

    #[test]
    fn precontact_audit_refuses_release_without_original_reserve_or_after_denial() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = ryeos_app::handler_context::budget_test_support::build(
            dir.path(),
            lillux::time::Duration::from_secs(60),
        )
        .unwrap();
        let store = fixture.state.state_store.as_ref();
        let release = row(&fixture.thread_id, ResourceBudgetState::ReleasedUnissued);
        assert!(recover_precontact_publication(store, &release).is_err());
        let denied = row(&fixture.thread_id, ResourceBudgetState::ReservationDenied);
        let seq = append(store, &denied);
        assert_eq!(
            recover_precontact_publication(store, &denied).unwrap(),
            Some(seq)
        );
        assert!(recover_precontact_publication(store, &release).is_err());
        assert!(
            recover_precontact_publication(
                store,
                &row(&fixture.thread_id, ResourceBudgetState::Reserved)
            )
            .is_err()
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn actual_precontact_producer_publisher_ack_and_lost_ack_keep_two_original_audits() {
        // Both real producer legs pass publish_one and actual ledger ACK.
        // Exercise append-before-ACK loss on each leg in separate fixtures.
        for lost_sequence in [1_u32, 2] {
            let dir = tempfile::tempdir().unwrap();
            let fixture =
                ryeos_app::accounting_db::precontact_publication_fixture_for_test(dir.path());
            let ledger = fixture.state.accounting.as_ref().unwrap();
            let store = fixture.state.state_store.as_ref();
            assert!(ledger.startup_verify().unwrap().hard_admission_enabled);
            assert_eq!(ledger.unpublished_outbox_stats().unwrap().0, 2);
            let mut operation = None;
            for sequence in [1_u32, 2] {
                let row = ledger
                    .claim_next_unpublished(now_ms(), CLAIM_LEASE_MS)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    row.event_type,
                    ryeos_state::event_types::RESOURCE_PRECONTACT_BUDGET_TRANSITION_V1
                );
                assert_eq!(row.transition_sequence, sequence);
                let event: ResourcePrecontactBudgetTransitionV1 =
                    serde_json::from_value(row.payload.clone()).unwrap();
                event.validate().unwrap();
                assert_eq!(
                    event.transition,
                    if sequence == 1 {
                        ResourceBudgetState::Reserved
                    } else {
                        ResourceBudgetState::ReleasedUnissued
                    }
                );
                let retry = row.clone();
                operation = Some(row.operation_id.clone());
                if sequence == lost_sequence {
                    append(store, &row); // actual CAS append, deliberately no ACK
                }
                publish_one(ledger, store, row).unwrap();
                publish_one(ledger, store, retry).unwrap(); // exact ACK re-drive
                assert_eq!(
                    ledger.unpublished_outbox_stats().unwrap().0,
                    u64::from(2 - sequence)
                );
            }
            assert!(
                ledger
                    .claim_next_unpublished(now_ms(), CLAIM_LEASE_MS)
                    .unwrap()
                    .is_none()
            );
            let retained = store
                .thread_events_by_type_operation_id(
                    &fixture.thread_id,
                    ryeos_state::event_types::RESOURCE_PRECONTACT_BUDGET_TRANSITION_V1,
                    operation.as_deref().unwrap(),
                )
                .unwrap();
            assert_eq!(retained.len(), 2);
            assert!(retained[0].chain_seq < retained[1].chain_seq);
            for record in retained {
                let event: ResourcePrecontactBudgetTransitionV1 =
                    serde_json::from_value(record.payload).unwrap();
                assert!(
                    store
                        .get_resource_budget_transition_identity(&event.transition_id)
                        .unwrap()
                        .is_none()
                );
            }
            assert!(ledger.startup_verify().unwrap().hard_admission_enabled);
        }
    }
}
