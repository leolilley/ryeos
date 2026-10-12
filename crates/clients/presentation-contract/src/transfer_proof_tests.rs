use crate::{payload_digest::seat_payload_digest, transfer::*, transfer_proof::*};
use serde_json::json;

fn records() -> (
    TransferRecord,
    TransferRecord,
    TransferRecord,
    TransferRecord,
    TransferEvidence,
    TransferEvidence,
    TransferEvidence,
) {
    let export = json!({"mode":"copy","views":["view:test/a"]});
    let seat = |id: &str, side: &str| TransferSeat {
        presentation_id: id.into(),
        thread_id: format!("thread-{side}"),
        chain_root_id: format!("chain-{side}"),
        producer_incarnation: "ab".repeat(32),
        revision: 9,
    };
    let evidence = |marker: &str, seq| TransferEvidence {
        event_hash: marker.repeat(32),
        chain_seq: seq,
    };
    let prepared = TransferRecord {
        schema: 1,
        transfer: TransferIdentity {
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            source: seat("11111111-1111-4111-8111-111111111111", "source"),
            destination: seat("22222222-2222-4222-8222-222222222222", "destination"),
            export_digest: seat_payload_digest(&export).unwrap(),
        },
        phase: TransferPhase::Prepared {
            inbound: evidence("00", 1),
            freeze: None,
            export,
        },
    };
    let p = evidence("aa", 1);
    let a = evidence("bb", 2);
    let c = evidence("cc", 3);
    let reservation = json!({"mounts":["destination:1"]});
    let import_digest = seat_payload_digest(&reservation).unwrap();
    let mut accepted = prepared.clone();
    accepted.phase = TransferPhase::Accepted {
        preparation: p.clone(),
        reservation,
        import_digest: import_digest.clone(),
    };
    let mut committed = prepared.clone();
    committed.phase = TransferPhase::Committed {
        preparation: p.clone(),
        acceptance: a.clone(),
        import_digest: import_digest.clone(),
    };
    let mut activated = prepared.clone();
    activated.phase = TransferPhase::Activated {
        commit: c.clone(),
        import_digest,
    };
    (prepared, accepted, committed, activated, p, a, c)
}
fn testimony<'a>(
    record: &'a TransferRecord,
    evidence: &'a TransferEvidence,
) -> TransferTestimony<'a> {
    TransferTestimony { record, evidence }
}

#[test]
fn exact_phase_chain_is_consistent_and_abort_only_references_preparation() {
    let (p, a, c, x, pe, ae, ce) = records();
    let mut inbound = p.clone();
    inbound.phase = TransferPhase::InboundCommitment;
    verify_prepared(
        &p,
        testimony(
            &inbound,
            &TransferEvidence {
                event_hash: "00".repeat(32),
                chain_seq: 1,
            },
        ),
    )
    .unwrap();
    let mut wrong_prepared = p.clone();
    if let TransferPhase::Prepared { inbound: link, .. } = &mut wrong_prepared.phase {
        link.chain_seq += 1;
    }
    assert!(
        verify_prepared(
            &wrong_prepared,
            testimony(
                &inbound,
                &TransferEvidence {
                    event_hash: "00".repeat(32),
                    chain_seq: 1,
                },
            ),
        )
        .is_err()
    );
    verify_accepted(&a, testimony(&p, &pe)).unwrap();
    verify_committed(&c, testimony(&p, &pe), testimony(&a, &ae)).unwrap();
    verify_activated(&x, testimony(&c, &ce)).unwrap();
    let mut abort = p.clone();
    abort.phase = TransferPhase::Aborted {
        preparation: pe.clone(),
    };
    verify_aborted(&abort, testimony(&p, &pe)).unwrap();
    assert!(verify_aborted(&abort, testimony(&a, &pe)).is_err());
    assert!(verify_activated(&x, testimony(&abort, &ce)).is_err());
}

#[test]
fn hashes_and_chain_positions_must_both_match() {
    let (p, a, c, x, pe, ae, ce) = records();
    for hash in [false, true] {
        let alter = |e: &TransferEvidence| {
            let mut e = e.clone();
            if hash {
                e.event_hash = "ee".repeat(32)
            } else {
                e.chain_seq += 1
            };
            e
        };
        assert!(verify_accepted(&a, testimony(&p, &alter(&pe))).is_err());
        assert!(verify_committed(&c, testimony(&p, &pe), testimony(&a, &alter(&ae))).is_err());
        assert!(verify_activated(&x, testimony(&c, &alter(&ce))).is_err());
    }
}

#[test]
fn producer_revision_identity_and_import_commitments_cannot_change() {
    let (p, a, c, x, pe, ae, ce) = records();
    for side in [false, true] {
        let mut other = p.clone();
        if side {
            other.transfer.source.revision += 1;
        } else {
            other.transfer.destination.revision += 1;
        }
        assert!(verify_accepted(&a, testimony(&other, &pe)).is_err());
        other = p.clone();
        if side {
            other.transfer.source.producer_incarnation = "ef".repeat(32);
        } else {
            other.transfer.destination.producer_incarnation = "ef".repeat(32);
        }
        assert!(verify_accepted(&a, testimony(&other, &pe)).is_err());
    }
    let mut bad = c.clone();
    if let TransferPhase::Committed { import_digest, .. } = &mut bad.phase {
        *import_digest = "ff".repeat(32)
    };
    assert!(verify_committed(&bad, testimony(&p, &pe), testimony(&a, &ae)).is_err());
    assert!(verify_activated(&x, testimony(&bad, &ce)).is_err());
    assert!(verify_accepted(&p, testimony(&p, &pe)).is_err());
    assert!(verify_committed(&a, testimony(&p, &pe), testimony(&a, &ae)).is_err());
}

#[test]
fn release_requires_exact_abort_acceptance_and_import_commitment() {
    let (prepared, accepted, _, _, pe, ae, _) = records();
    let abort_evidence = TransferEvidence {
        event_hash: "ee".repeat(32),
        chain_seq: 3,
    };
    let mut aborted = prepared.clone();
    aborted.phase = TransferPhase::Aborted {
        preparation: pe.clone(),
    };
    let import_digest = match &accepted.phase {
        TransferPhase::Accepted { import_digest, .. } => import_digest.clone(),
        _ => unreachable!(),
    };
    let mut released = prepared.clone();
    released.phase = TransferPhase::Released {
        abort: abort_evidence.clone(),
        acceptance: ae.clone(),
        import_digest,
    };
    verify_released(
        &released,
        testimony(&aborted, &abort_evidence),
        testimony(&accepted, &ae),
    )
    .unwrap();
    let mut tampered = released.clone();
    if let TransferPhase::Released { import_digest, .. } = &mut tampered.phase {
        *import_digest = "ff".repeat(32);
    }
    assert!(
        verify_released(
            &tampered,
            testimony(&aborted, &abort_evidence),
            testimony(&accepted, &ae),
        )
        .is_err()
    );
    assert!(
        verify_released(
            &released,
            testimony(&prepared, &abort_evidence),
            testimony(&accepted, &ae),
        )
        .is_err()
    );
}

#[test]
fn retained_data_requires_the_exact_commit_and_unchanged_import() {
    let (prepared, accepted, committed, activated, pe, _, ce) = records();
    let mut retained = activated.clone();
    let TransferPhase::Activated {
        commit,
        import_digest,
    } = &activated.phase
    else {
        unreachable!()
    };
    retained.phase = TransferPhase::Retained {
        commit: commit.clone(),
        import_digest: import_digest.clone(),
    };
    verify_retained(&retained, testimony(&committed, &ce)).unwrap();
    assert!(verify_activated(&retained, testimony(&committed, &ce)).is_err());
    assert!(verify_retained(&activated, testimony(&committed, &ce)).is_err());
    for wrong_phase in [&prepared, &accepted, &activated] {
        assert!(verify_retained(&retained, testimony(wrong_phase, &ce)).is_err());
    }
    let mut aborted = prepared.clone();
    aborted.phase = TransferPhase::Aborted { preparation: pe };
    assert!(verify_retained(&retained, testimony(&aborted, &ce)).is_err());

    let mut wrong_hash = ce.clone();
    wrong_hash.event_hash = "ee".repeat(32);
    let mut wrong_position = ce.clone();
    wrong_position.chain_seq += 1;
    for evidence in [&wrong_hash, &wrong_position] {
        assert!(verify_retained(&retained, testimony(&committed, evidence)).is_err());
    }
    let mut changed_commit = committed.clone();
    changed_commit.transfer.destination.producer_incarnation = "ef".repeat(32);
    assert!(verify_retained(&retained, testimony(&changed_commit, &ce)).is_err());
    if let TransferPhase::Retained { import_digest, .. } = &mut retained.phase {
        *import_digest = "ff".repeat(32);
    }
    assert!(verify_retained(&retained, testimony(&committed, &ce)).is_err());
}

#[test]
fn observation_accepts_only_an_exact_same_transfer_destination_terminal() {
    let (prepared, accepted, committed, activated, pe, ae, _) = records();
    let terminal_evidence = TransferEvidence {
        event_hash: "dd".repeat(32),
        chain_seq: 4,
    };
    let mut observed = prepared.clone();
    observed.phase = TransferPhase::SettlementObserved {
        settlement: terminal_evidence.clone(),
    };
    let mut retained = activated.clone();
    let TransferPhase::Activated {
        commit,
        import_digest,
    } = &activated.phase
    else {
        unreachable!()
    };
    retained.phase = TransferPhase::Retained {
        commit: commit.clone(),
        import_digest: import_digest.clone(),
    };
    let mut aborted = prepared.clone();
    aborted.phase = TransferPhase::Aborted { preparation: pe };
    let mut released = prepared.clone();
    released.phase = TransferPhase::Released {
        abort: TransferEvidence {
            event_hash: "ee".repeat(32),
            chain_seq: 3,
        },
        acceptance: ae,
        import_digest: import_digest.clone(),
    };
    for terminal in [&activated, &retained, &released] {
        verify_settlement_observed(&observed, testimony(terminal, &terminal_evidence)).unwrap();
        let mut wrong_hash = terminal_evidence.clone();
        wrong_hash.event_hash = "ff".repeat(32);
        let mut wrong_position = terminal_evidence.clone();
        wrong_position.chain_seq += 1;
        for evidence in [&wrong_hash, &wrong_position] {
            assert!(verify_settlement_observed(&observed, testimony(terminal, evidence)).is_err());
        }
        let mut foreign = terminal.clone();
        foreign.transfer.source.revision += 1;
        assert!(
            verify_settlement_observed(&observed, testimony(&foreign, &terminal_evidence)).is_err()
        );
    }
    for nonterminal in [&prepared, &accepted, &committed, &aborted, &observed] {
        assert!(
            verify_settlement_observed(&observed, testimony(nonterminal, &terminal_evidence))
                .is_err()
        );
    }
    assert!(
        verify_settlement_observed(&activated, testimony(&activated, &terminal_evidence)).is_err()
    );
}
