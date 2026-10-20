use crate::{payload_digest::seat_payload_digest, transfer::*};
use serde_json::{Value, json};

#[test]
fn transfer_family_classification_includes_lifecycle_and_future_versions() {
    assert!(is_transfer_testimony(TRANSFER_EVENT));
    assert!(is_transfer_testimony(
        crate::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT
    ));
    for earlier_or_later in [
        "contract.ryeos_ui.presentation_transfer_lifecycle.v1",
        "contract.ryeos_ui.presentation_transfer_lifecycle.v2",
        "contract.ryeos_ui.presentation_transfer_lifecycle.v4",
    ] {
        assert!(is_transfer_testimony(earlier_or_later));
    }
    assert_eq!(
        crate::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT,
        "contract.ryeos_ui.presentation_transfer_lifecycle.v3"
    );
    assert!(!is_transfer_testimony("seat.facet"));
    assert!(!is_transfer_testimony("contract.ryeos_ui.other.v1"));
}

fn seat(id: &str, suffix: &str) -> TransferSeat {
    TransferSeat {
        presentation_id: id.into(),
        thread_id: format!("thread-{suffix}"),
        chain_root_id: format!("chain-{suffix}"),
        producer_incarnation: "ab".repeat(32),
        revision: 7,
    }
}
fn record() -> TransferRecord {
    let export =
        json!({"mode":"copy","view_ref":"view:test/one","nested":{"event_hash":"ee".repeat(32)}});
    TransferRecord {
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
    }
}
fn evidence(marker: &str, seq: u64) -> TransferEvidence {
    TransferEvidence {
        event_hash: marker.repeat(32),
        chain_seq: seq,
    }
}
fn reservation() -> Value {
    json!({"mounts":["destination:1"]})
}
fn import_digest() -> String {
    seat_payload_digest(&reservation()).unwrap()
}

#[test]
fn prepared_freeze_links_are_required_only_for_move_and_ordered() {
    let mut prepared = record();
    let export = json!({"mode":"move","view_ref":"view:test/one"});
    prepared.transfer.export_digest = seat_payload_digest(&export).unwrap();
    prepared.phase = TransferPhase::Prepared {
        inbound: evidence("00", 1),
        freeze: Some(MoveFreezeEvidence {
            preparation: evidence("aa", 2),
            completion: evidence("bb", 3),
        }),
        export,
    };
    prepared.validate_shape().unwrap();
    assert_eq!(prepared.evidence_links().len(), 3);
    assert_eq!(prepared.phase_evidence_links(), vec![&evidence("00", 1)]);
    assert_eq!(
        prepared.freeze_event_links(),
        vec![&evidence("aa", 2), &evidence("bb", 3)]
    );
    let mut missing = serde_json::to_value(&prepared).unwrap();
    missing["phase"].as_object_mut().unwrap().remove("freeze");
    assert!(serde_json::from_value::<TransferRecord>(missing).is_err());
    if let TransferPhase::Prepared { freeze, .. } = &mut prepared.phase {
        let links = freeze.as_mut().unwrap();
        links.completion.chain_seq = links.preparation.chain_seq;
    }
    assert!(prepared.validate_shape().is_err());
    if let TransferPhase::Prepared { freeze, export, .. } = &mut prepared.phase {
        freeze.as_mut().unwrap().completion.chain_seq += 1;
        *export = json!({"mode":"copy","view_ref":"view:test/one"});
        prepared.transfer.export_digest = seat_payload_digest(export).unwrap();
    }
    assert!(prepared.validate_shape().is_err());
}
fn event(record: &TransferRecord) -> Value {
    json!({"event_type":TRANSFER_EVENT,"thread_id":record.event_owner().thread_id,"chain_root_id":record.event_owner().chain_root_id,
        "payload":TransferEventPayload {seq:4,operation_id:record.transfer.operation_id.clone(),payload:record.clone()}})
}

#[test]
fn non_advancing_witness_pins_origin_only_during_preparation() {
    let prepared = record();
    let wrapper = TransferEventPayload {
        seq: 4,
        operation_id: prepared.transfer.operation_id.clone(),
        payload: prepared.clone(),
    };
    let source = &prepared.transfer.source;
    assert!(
        wrapper
            .validate_current_witness(4, 7, &source.presentation_id, &source.producer_incarnation)
            .is_ok()
    );
    assert!(
        wrapper
            .validate_current_witness(5, 7, &source.presentation_id, &source.producer_incarnation)
            .is_err()
    );
    assert!(
        wrapper
            .validate_current_witness(4, 8, &source.presentation_id, &source.producer_incarnation)
            .is_err()
    );
    assert!(
        wrapper
            .validate_current_witness(4, 7, &source.presentation_id, &"cd".repeat(32))
            .is_err()
    );

    let mut later = wrapper;
    later.payload.phase = TransferPhase::Committed {
        preparation: evidence("aa", 1),
        acceptance: evidence("bb", 2),
        import_digest: import_digest(),
    };
    assert!(
        later
            .validate_current_witness(4, 8, &source.presentation_id, &"cd".repeat(32))
            .is_ok()
    );
    assert!(
        later
            .validate_current_witness(5, 8, &source.presentation_id, &"cd".repeat(32))
            .is_err()
    );
}

#[test]
fn closed_contract_refuses_missing_unknown_and_predecessor_shapes() {
    let value = serde_json::to_value(record()).unwrap();
    let mut unknown = value.clone();
    unknown["grant"] = json!(true);
    assert!(serde_json::from_value::<TransferRecord>(unknown).is_err());
    let mut missing = value.clone();
    missing.as_object_mut().unwrap().remove("transfer");
    assert!(serde_json::from_value::<TransferRecord>(missing).is_err());
    let mut predecessor = record();
    predecessor.schema = 0;
    assert!(predecessor.validate_shape().is_err());
    let mut unknownphase = value;
    unknownphase["phase"]["kind"] = json!("force_commit");
    assert!(serde_json::from_value::<TransferRecord>(unknownphase).is_err());
    let mut wrapper = event(&record());
    wrapper["payload"]["extra"] = json!(0);
    assert!(from_event_value(&wrapper).is_err());
}

#[test]
fn phase_owner_and_indexed_operation_wrapper_are_exact() {
    let mut record = record();
    let phases = [
        TransferPhase::InboundCommitment,
        record.phase.clone(),
        TransferPhase::Accepted {
            preparation: evidence("aa", 1),
            reservation: reservation(),
            import_digest: import_digest(),
        },
        TransferPhase::Committed {
            preparation: evidence("aa", 1),
            acceptance: evidence("bb", 2),
            import_digest: import_digest(),
        },
        TransferPhase::Aborted {
            preparation: evidence("aa", 1),
        },
        TransferPhase::Activated {
            commit: evidence("dd", 3),
            import_digest: import_digest(),
        },
        TransferPhase::Retained {
            commit: evidence("dd", 3),
            import_digest: import_digest(),
        },
        TransferPhase::Released {
            abort: evidence("ee", 4),
            acceptance: evidence("bb", 2),
            import_digest: import_digest(),
        },
        TransferPhase::SettlementObserved {
            settlement: evidence("ff", 5),
        },
    ];
    for phase in phases {
        record.phase = phase;
        let good = event(&record);
        assert_eq!(from_event_value(&good).unwrap().payload, record);
        for field in ["thread_id", "chain_root_id"] {
            let mut wrong = good.clone();
            wrong[field] = json!("foreign");
            assert!(from_event_value(&wrong).is_err());
        }
        let mut wrong = good.clone();
        wrong["payload"]["operation_id"] = json!("44444444-4444-4444-8444-444444444444");
        assert!(from_event_value(&wrong).is_err());
        let mut wrong = good;
        wrong["event_type"] = json!("predecessor");
        assert!(from_event_value(&wrong).is_err());
    }
}

#[test]
fn prepared_commitment_and_selected_record_bound_are_not_repaired() {
    let mut record = record();
    record.validate_shape().unwrap();
    let size = serde_json::to_vec(&record).unwrap().len() as u64;
    record.validate_bounds(size).unwrap();
    assert!(record.validate_bounds(size - 1).is_err());
    assert!(record.validate_bounds(0).is_err());
    if let TransferPhase::Prepared { export, .. } = &mut record.phase {
        export["view_ref"] = json!("view:test/tampered");
    }
    assert!(record.validate_shape().is_err());
    record.phase = TransferPhase::Prepared {
        inbound: evidence("00", 1),
        freeze: None,
        export: json!([]),
    };
    assert!(record.validate_shape().is_err());
}

#[test]
fn accepted_reservation_is_mandatory_closed_and_exactly_committed() {
    let mut accepted = record();
    accepted.phase = TransferPhase::Accepted {
        preparation: evidence("aa", 1),
        reservation: reservation(),
        import_digest: import_digest(),
    };
    accepted.validate_shape().unwrap();
    let mut missing = serde_json::to_value(&accepted).unwrap();
    missing["phase"]
        .as_object_mut()
        .unwrap()
        .remove("reservation");
    assert!(serde_json::from_value::<TransferRecord>(missing).is_err());
    if let TransferPhase::Accepted { reservation, .. } = &mut accepted.phase {
        reservation["mounts"] = json!(["destination:other"]);
    }
    assert!(accepted.validate_shape().is_err());
    if let TransferPhase::Accepted { reservation, .. } = &mut accepted.phase {
        *reservation = json!([]);
    }
    assert!(accepted.validate_shape().is_err());
}

#[test]
fn retention_edges_are_only_typed_phase_witnesses() {
    let mut inbound = record();
    inbound.phase = TransferPhase::InboundCommitment;
    assert!(
        inbound.evidence_links().is_empty(),
        "inbound commitment carries no predecessor edge"
    );
    assert_eq!(inbound.event_owner(), &inbound.transfer.destination);
    let mut record = record();
    assert_eq!(record.evidence_links(), vec![&evidence("00", 1)]);
    record.phase = TransferPhase::Committed {
        preparation: evidence("aa", 1),
        acceptance: evidence("bb", 2),
        import_digest: import_digest(),
    };
    assert_eq!(
        record
            .evidence_links()
            .iter()
            .map(|link| link.chain_seq)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    record.validate_shape().unwrap();
    record.phase = TransferPhase::Activated {
        commit: evidence("dd", 3),
        import_digest: import_digest(),
    };
    assert_eq!(record.evidence_links()[0].event_hash, "dd".repeat(32));
    record.phase = TransferPhase::Retained {
        commit: evidence("dd", 3),
        import_digest: import_digest(),
    };
    record.validate_shape().unwrap();
    assert_eq!(record.event_owner(), &record.transfer.destination);
    assert_eq!(record.evidence_links(), vec![&evidence("dd", 3)]);
    record.phase = TransferPhase::Released {
        abort: evidence("ee", 4),
        acceptance: evidence("bb", 2),
        import_digest: import_digest(),
    };
    assert_eq!(
        record
            .evidence_links()
            .iter()
            .map(|link| link.event_hash.clone())
            .collect::<Vec<_>>(),
        vec!["ee".repeat(32), "bb".repeat(32)]
    );
    record.phase = TransferPhase::SettlementObserved {
        settlement: evidence("ff", 5),
    };
    record.validate_shape().unwrap();
    assert_eq!(record.event_owner(), &record.transfer.source);
    assert_eq!(record.evidence_links(), vec![&evidence("ff", 5)]);
    record.phase = TransferPhase::Accepted {
        preparation: evidence("AA", 1),
        reservation: reservation(),
        import_digest: import_digest(),
    };
    assert!(record.validate_shape().is_err());
    record.phase = TransferPhase::Committed {
        preparation: evidence("aa", 1),
        acceptance: evidence("aa", 2),
        import_digest: import_digest(),
    };
    assert!(record.validate_shape().is_err());
}

#[test]
fn settlement_shapes_require_exact_closed_typed_links_and_commitments() {
    let mut record = record();
    let phases = [
        (
            TransferPhase::Retained {
                commit: evidence("dd", 3),
                import_digest: import_digest(),
            },
            "commit",
        ),
        (
            TransferPhase::SettlementObserved {
                settlement: evidence("ff", 5),
            },
            "settlement",
        ),
    ];
    for (phase, link_field) in phases {
        record.phase = phase;
        let value = serde_json::to_value(&record).unwrap();
        let mut missing = value.clone();
        missing["phase"].as_object_mut().unwrap().remove(link_field);
        assert!(serde_json::from_value::<TransferRecord>(missing).is_err());
        let mut unknown = value.clone();
        unknown["phase"]["restore_authority"] = json!(true);
        assert!(serde_json::from_value::<TransferRecord>(unknown).is_err());
        let mut malformed = value;
        malformed["phase"][link_field]["event_hash"] = json!("FF".repeat(32));
        assert!(
            serde_json::from_value::<TransferRecord>(malformed)
                .unwrap()
                .validate_shape()
                .is_err()
        );
    }
    record.phase = TransferPhase::Retained {
        commit: evidence("dd", 3),
        import_digest: "FF".repeat(32),
    };
    assert!(record.validate_shape().is_err());
}

#[test]
fn source_and_destination_cannot_alias_seat_identity() {
    let original = record();
    for field in ["presentation", "thread", "chain"] {
        let mut bad = original.clone();
        match field {
            "presentation" => {
                bad.transfer.destination.presentation_id =
                    bad.transfer.source.presentation_id.clone()
            }
            "thread" => bad.transfer.destination.thread_id = bad.transfer.source.thread_id.clone(),
            _ => bad.transfer.destination.chain_root_id = bad.transfer.source.chain_root_id.clone(),
        }
        assert!(bad.validate_shape().is_err());
    }
    let mut bad = original;
    bad.transfer.source.producer_incarnation = "AB".repeat(32);
    assert!(bad.validate_shape().is_err());
}
