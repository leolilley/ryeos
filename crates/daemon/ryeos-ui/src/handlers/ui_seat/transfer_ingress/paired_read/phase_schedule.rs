//! Consistency scheduling of already completed app proofs, not event replay.
//! Seat-local positions and typed evidence links constrain this order. The
//! deterministic choice among unrelated nodes grants no publication authority.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use ryeos_app::thread_lifecycle::presentation_transfer::UiTransferFixedHistoryProof;
use ryeos_ui_presentation_contract::transfer::{TransferEvidence, TransferRecord};

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ScheduledTransferPhase {
    pub(super) evidence: TransferEvidence,
    pub(super) record: TransferRecord,
}

/// Only completed, opaque app proofs enter the production scheduling boundary.
/// This neither folds ordinary events nor supplies a browser completion fence.
pub(super) fn schedule_cut<'a>(
    proofs: impl IntoIterator<Item = &'a UiTransferFixedHistoryProof>,
    check_cancelled: impl Fn() -> Result<()>,
) -> Result<Vec<ScheduledTransferPhase>> {
    schedule_lifecycles(
        proofs.into_iter().map(|proof| proof.lifecycle()),
        check_cancelled,
    )
}

pub(super) fn schedule_historical_cut(
    proof: &ryeos_app::thread_lifecycle::presentation_transfer::UiHistoricalTransferCutProof,
    check_cancelled: impl Fn() -> Result<()>,
) -> Result<Vec<ScheduledTransferPhase>> {
    schedule_lifecycles(
        proof
            .identities()
            .keys()
            .filter_map(|id| proof.lifecycle(id)),
        check_cancelled,
    )
}

fn schedule_lifecycles<'a>(
    lifecycles: impl IntoIterator<
        Item = &'a ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
    >,
    check_cancelled: impl Fn() -> Result<()>,
) -> Result<Vec<ScheduledTransferPhase>> {
    let mut phases = Vec::new();
    for lifecycle in lifecycles {
        check_cancelled()?;
        let inbound = lifecycle.inbound();
        phases.push(ScheduledTransferPhase {
            evidence: inbound.evidence.clone(),
            record: inbound.record.clone(),
        });
        for testimony in [
            lifecycle.orphan_cancelled(),
            lifecycle.prepared(),
            lifecycle.accepted(),
            lifecycle.committed(),
            lifecycle.aborted(),
            lifecycle.activated(),
            lifecycle.retained(),
            lifecycle.released(),
            lifecycle.settlement_observed(),
        ]
        .into_iter()
        .flatten()
        {
            phases.push(ScheduledTransferPhase {
                evidence: testimony.evidence.clone(),
                record: testimony.record.clone(),
            });
        }
    }
    schedule_nodes_checked(phases, check_cancelled)
}

// Pure consistency helper for raw synthetic test nodes. It is private and
// cannot mint completed app proofs or qualify unauthenticated testimony.
#[cfg(test)]
fn schedule_nodes(phases: Vec<ScheduledTransferPhase>) -> Result<Vec<ScheduledTransferPhase>> {
    schedule_nodes_checked(phases, || Ok(()))
}

fn schedule_nodes_checked(
    phases: Vec<ScheduledTransferPhase>,
    check_cancelled: impl Fn() -> Result<()>,
) -> Result<Vec<ScheduledTransferPhase>> {
    check_cancelled()?;
    let mut nodes: BTreeMap<String, ScheduledTransferPhase> = BTreeMap::new();
    let mut identities = BTreeMap::new();
    let mut presentation_seats = BTreeMap::new();
    let mut seat_presentations = BTreeMap::new();
    let mut positions = BTreeMap::new();
    let mut local: BTreeMap<(String, String, String), BTreeMap<u64, String>> = BTreeMap::new();
    for phase in phases {
        check_cancelled()?;
        phase.record.validate_shape().map_err(anyhow::Error::msg)?;
        let hash = &phase.evidence.event_hash;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("transfer schedule evidence hash is not canonical");
        }
        if let Some(prior) = nodes.get(hash) {
            if prior != &phase {
                bail!("transfer schedule has conflicting duplicate evidence");
            }
            continue;
        }
        let transfer = &phase.record.transfer;
        if identities
            .get(&transfer.operation_id)
            .is_some_and(|prior| prior != transfer)
        {
            bail!("transfer schedule changed a full operation identity");
        }
        identities.insert(transfer.operation_id.clone(), transfer.clone());
        // Stable associations are checked independently of historical origins.
        for seat in [&transfer.source, &transfer.destination] {
            let coordinates = (seat.chain_root_id.clone(), seat.thread_id.clone());
            if presentation_seats
                .get(&seat.presentation_id)
                .is_some_and(|prior| prior != &coordinates)
                || seat_presentations
                    .get(&coordinates)
                    .is_some_and(|prior| prior != &seat.presentation_id)
            {
                bail!("transfer schedule changed a stable owner association");
            }
            presentation_seats.insert(seat.presentation_id.clone(), coordinates.clone());
            seat_presentations.insert(coordinates, seat.presentation_id.clone());
        }
        let owner = phase.record.event_owner();
        let position = (
            owner.chain_root_id.clone(),
            owner.thread_id.clone(),
            phase.evidence.chain_seq,
        );
        if positions.insert(position, hash.clone()).is_some() {
            bail!("transfer schedule has duplicate seat-local positions");
        }
        local
            .entry((
                owner.presentation_id.clone(),
                owner.chain_root_id.clone(),
                owner.thread_id.clone(),
            ))
            .or_default()
            .insert(phase.evidence.chain_seq, hash.clone());
        nodes.insert(hash.clone(), phase);
    }
    let mut edges = BTreeSet::new();
    for seat in local.values() {
        check_cancelled()?;
        let mut previous: Option<&String> = None;
        for hash in seat.values() {
            check_cancelled()?;
            if let Some(previous) = previous {
                edges.insert((previous.clone(), hash.clone()));
            }
            previous = Some(hash);
        }
    }
    for (hash, node) in &nodes {
        check_cancelled()?;
        for link in node.record.phase_evidence_links() {
            let Some(target) = nodes.get(&link.event_hash) else {
                bail!("transfer schedule evidence dependency is absent");
            };
            if target.evidence != *link || target.record.transfer != node.record.transfer {
                bail!("transfer schedule dependency changed evidence or full identity");
            }
            if &link.event_hash == hash {
                bail!("transfer schedule has a self dependency");
            }
            edges.insert((link.event_hash.clone(), hash.clone()));
        }
    }
    let mut indegrees = BTreeMap::new();
    for hash in nodes.keys() {
        check_cancelled()?;
        indegrees.insert(hash.clone(), 0usize);
    }
    let mut outgoing: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (from, to) in edges {
        check_cancelled()?;
        *indegrees.get_mut(&to).expect("validated graph target") += 1;
        outgoing.entry(from).or_default().push(to);
    }
    let mut ready = BTreeSet::new();
    for (hash, degree) in &indegrees {
        check_cancelled()?;
        if *degree == 0 {
            ready.insert(hash.clone());
        }
    }
    let mut scheduled = Vec::with_capacity(nodes.len());
    while let Some(hash) = ready.pop_first() {
        check_cancelled()?;
        scheduled.push(nodes.remove(&hash).expect("ready graph node"));
        for target in outgoing.remove(&hash).unwrap_or_default() {
            check_cancelled()?;
            let degree = indegrees.get_mut(&target).expect("validated graph target");
            *degree -= 1;
            if *degree == 0 {
                ready.insert(target);
            }
        }
    }
    if !nodes.is_empty() {
        bail!("transfer schedule contains a dependency cycle");
    }
    Ok(scheduled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_schedule_cannot_return_a_completed_cut() {
        let result = schedule_nodes_checked(Vec::new(), || bail!("cancelled"));
        assert!(result.is_err());
    }

    #[test]
    fn cancellation_after_node_collection_refuses_partial_schedule() {
        let operation = transfer(1, 1, 2);
        let checks = std::cell::Cell::new(0usize);
        let result = schedule_nodes_checked(
            vec![node(1, 1, &operation, TransferPhase::InboundCommitment)],
            || {
                let count = checks.get() + 1;
                checks.set(count);
                if count == 3 {
                    bail!("cancelled after node collection");
                }
                Ok(())
            },
        );
        assert_eq!(checks.get(), 3);
        assert!(result.is_err());
    }
    use ryeos_ui_presentation_contract::transfer::{
        TRANSFER_SCHEMA, TransferIdentity, TransferPhase, TransferSeat,
    };

    fn seat(index: u64) -> TransferSeat {
        TransferSeat {
            presentation_id: format!("00000000-0000-4000-8000-{index:012x}"),
            chain_root_id: format!("T-seat-{index}"),
            thread_id: format!("T-seat-{index}"),
            producer_incarnation: format!("{index:064x}"),
            revision: index,
        }
    }

    fn transfer(index: u64, source: u64, destination: u64) -> TransferIdentity {
        TransferIdentity {
            operation_id: format!("10000000-0000-4000-8000-{index:012x}"),
            source: seat(source),
            destination: seat(destination),
            export_digest: "a".repeat(64),
        }
    }

    fn node(
        hash: u64,
        sequence: u64,
        transfer: &TransferIdentity,
        phase: TransferPhase,
    ) -> ScheduledTransferPhase {
        ScheduledTransferPhase {
            evidence: TransferEvidence {
                event_hash: format!("{hash:064x}"),
                chain_seq: sequence,
            },
            record: TransferRecord {
                schema: TRANSFER_SCHEMA,
                transfer: transfer.clone(),
                phase,
            },
        }
    }

    #[test]
    fn prepared_and_accepted_follow_cross_seat_evidence_not_global_sequence() {
        let export = serde_json::json!({"mode":"copy"});
        let mut operation = transfer(1, 1, 2);
        operation.export_digest =
            ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(&export).unwrap();
        let inbound = node(9, 100, &operation, TransferPhase::InboundCommitment);
        let prepared = node(
            2,
            3,
            &operation,
            TransferPhase::Prepared {
                inbound: inbound.evidence.clone(),
                freeze: None,
                export,
            },
        );
        let reservation = serde_json::json!({});
        let import_digest =
            ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(&reservation)
                .unwrap();
        let accepted = node(
            1,
            101,
            &operation,
            TransferPhase::Accepted {
                preparation: prepared.evidence.clone(),
                reservation,
                import_digest,
            },
        );
        assert_eq!(
            schedule_nodes(vec![accepted.clone(), prepared.clone(), inbound.clone()]).unwrap(),
            vec![inbound, prepared, accepted]
        );
    }

    #[test]
    fn local_order_cross_seat_links_and_duplicate_edges_are_respected() {
        let operation = transfer(1, 1, 2);
        let inbound = node(9, 100, &operation, TransferPhase::InboundCommitment);
        // Cancellation's typed edge duplicates its immediate local-order edge.
        let cancelled = node(
            1,
            101,
            &operation,
            TransferPhase::OrphanCancelled {
                inbound: inbound.evidence.clone(),
            },
        );
        let observed = node(
            2,
            3,
            &operation,
            TransferPhase::SettlementObserved {
                settlement: cancelled.evidence.clone(),
            },
        );
        let scheduled = schedule_nodes(vec![
            observed.clone(),
            cancelled.clone(),
            inbound.clone(),
            inbound.clone(),
        ])
        .unwrap();
        assert_eq!(scheduled, vec![inbound, cancelled, observed]);
    }

    #[test]
    fn missing_wrong_sequence_and_cross_identity_dependencies_refuse() {
        let operation = transfer(1, 1, 2);
        let inbound = node(1, 1, &operation, TransferPhase::InboundCommitment);
        let mut cancelled = node(
            2,
            2,
            &operation,
            TransferPhase::OrphanCancelled {
                inbound: inbound.evidence.clone(),
            },
        );
        assert!(schedule_nodes(vec![cancelled.clone()]).is_err());
        let TransferPhase::OrphanCancelled { inbound: link } = &mut cancelled.record.phase else {
            unreachable!()
        };
        link.chain_seq += 1;
        assert!(schedule_nodes(vec![inbound.clone(), cancelled.clone()]).is_err());
        cancelled.record.phase = TransferPhase::OrphanCancelled {
            inbound: inbound.evidence.clone(),
        };
        cancelled.record.transfer = transfer(2, 1, 2);
        assert!(schedule_nodes(vec![inbound, cancelled]).is_err());
    }

    #[test]
    fn duplicate_hash_position_full_identity_and_owner_conflicts_refuse() {
        let operation = transfer(1, 1, 2);
        let inbound = node(1, 1, &operation, TransferPhase::InboundCommitment);
        let mut changed = inbound.clone();
        changed.evidence.chain_seq += 1;
        assert!(schedule_nodes(vec![inbound.clone(), changed]).is_err());
        let other = node(2, 1, &transfer(2, 3, 2), TransferPhase::InboundCommitment);
        assert!(schedule_nodes(vec![inbound.clone(), other]).is_err());
        let mut changed = node(3, 2, &operation, TransferPhase::InboundCommitment);
        changed.record.transfer.destination.revision += 1;
        assert!(schedule_nodes(vec![inbound.clone(), changed]).is_err());
        let mut owner = transfer(3, 3, 2);
        owner.destination.presentation_id = seat(4).presentation_id;
        assert!(
            schedule_nodes(vec![
                inbound,
                node(4, 3, &owner, TransferPhase::InboundCommitment)
            ])
            .is_err()
        );
    }

    #[test]
    fn self_dependency_and_cross_operation_local_order_cycle_refuse() {
        let operation = transfer(1, 1, 2);
        let evidence = TransferEvidence {
            event_hash: format!("{:064x}", 1),
            chain_seq: 1,
        };
        let self_link = node(
            1,
            1,
            &operation,
            TransferPhase::OrphanCancelled { inbound: evidence },
        );
        assert!(schedule_nodes(vec![self_link]).is_err());
        let other = transfer(2, 2, 1);
        let first_inbound = node(1, 2, &operation, TransferPhase::InboundCommitment);
        let second_inbound = node(2, 2, &other, TransferPhase::InboundCommitment);
        let first_source = node(
            3,
            1,
            &operation,
            TransferPhase::SettlementObserved {
                settlement: first_inbound.evidence.clone(),
            },
        );
        let second_source = node(
            4,
            1,
            &other,
            TransferPhase::SettlementObserved {
                settlement: second_inbound.evidence.clone(),
            },
        );
        // seat1: first_source -> second_inbound; seat2: second_source -> first_inbound;
        // exact cross-seat links complete the four-node cycle.
        assert!(
            schedule_nodes(vec![
                first_inbound,
                second_inbound,
                first_source,
                second_source
            ])
            .is_err()
        );
    }
}
