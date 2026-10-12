use crate::seat_control::*;
use serde_json::{Value, json};

#[test]
fn extraction_preserves_existing_control_preimages_and_uuid_bytes() {
    let original = json!({
        "action":"claim", "schema":2, "operation_id":"ignored-derived-self-id",
        "presentation_id":"22222222-2222-4222-8222-222222222222",
        "thread_id":"seat", "chain_root_id":"root",
        "predecessor":{"kind":"initialization", "event_hash":"ab".repeat(32)},
        "intent_nonce":"11111111-1111-4111-8111-111111111111", "claim":{"kind":"fresh"}
    });
    // Frozen from the pre-extraction client-base preimage/UUID algorithm.
    // These are exact bytes, not merely equality between two new helpers.
    for (kind, digest, operation_id) in [
        (
            "fresh",
            "5fb13967e0b28a65477d1eeba338294f0106ada503f39eb0258f6b9dfdf9ff50",
            "5fb13967-e0b2-8a65-877d-1eeba338294f",
        ),
        (
            "fresh_independent",
            "ce575af15aa6701cfd16533f3924fbad0699d4236ed3f0e8fdb6ff97963e0111",
            "ce575af1-5aa6-801c-bd16-533f3924fbad",
        ),
        (
            "resume",
            "4cef3c2f8416611f9f427f5d95a1c718aa3cbe6dec9c7fbd1f01bf7ec2c983bd",
            "4cef3c2f-8416-811f-9f42-7f5d95a1c718",
        ),
        (
            "takeover",
            "a00fb40d34a3bc29feaba56a125cdce9f048b0b254132dcbcd2721cae92292ed",
            "a00fb40d-34a3-8c29-beab-a56a125cdce9",
        ),
        (
            "relinquish",
            "ba70ed024ab5ae5a3a4885097bf96b8ea3da4ed3b0e4b5ad173557f6246a1461",
            "ba70ed02-4ab5-8e5a-ba48-85097bf96b8e",
        ),
        (
            "close",
            "a510308bf870b3c9774f76c490a55a615fcd90d5b4f3288faf78245d2aff7a19",
            "a510308b-f870-83c9-b74f-76c490a55a61",
        ),
    ] {
        let mut wire = original.clone();
        match kind {
            "fresh" => {}
            "fresh_independent" => {
                wire["intent_nonce"] = json!("33333333-3333-4333-8333-333333333333")
            }
            "resume" | "takeover" => {
                wire["predecessor"]["kind"] = json!("control");
                wire["claim"] =
                    json!({"kind":kind, "expected_producer_incarnation":"ef".repeat(32)});
            }
            "relinquish" | "close" => {
                wire["predecessor"]["kind"] = json!("control");
                wire["action"] = json!(kind);
                wire.as_object_mut().unwrap().remove("claim");
                wire["producer_incarnation"] = json!("ef".repeat(32));
            }
            _ => unreachable!(),
        }
        let mut preimage = wire.clone();
        preimage.as_object_mut().unwrap().remove("operation_id");
        assert_eq!(
            crate::payload_digest::seat_payload_digest(&json!({
                "domain":"ryeos.ui.seat-control.v2", "request":preimage,
            }))
            .unwrap(),
            digest,
            "{kind}"
        );
        let mut request: UiSeatControlRequest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(request.derive_identity().unwrap(), operation_id, "{kind}");
        request.assign_identity().unwrap();
        request.validate().unwrap();
        wire["operation_id"] = json!(operation_id);
        assert_eq!(serde_json::to_value(&request).unwrap(), wire, "{kind}");
    }
}

fn fresh(nonce: &str) -> UiSeatControlRequest {
    let mut request = UiSeatControlRequest::Claim {
        schema: 2,
        operation_id: String::new(),
        presentation_id: "22222222-2222-4222-8222-222222222222".into(),
        thread_id: "seat".into(),
        chain_root_id: "root".into(),
        predecessor: UiSeatControlPredecessor::Initialization {
            event_hash: "ab".repeat(32),
        },
        intent_nonce: nonce.into(),
        claim: UiSeatProducerClaim::Fresh,
    };
    request.assign_identity().unwrap();
    request
}

#[test]
fn exact_retry_keeps_identity_but_independent_intents_do_not_deduplicate() {
    let first = fresh("11111111-1111-4111-8111-111111111111");
    let second = fresh("33333333-3333-4333-8333-333333333333");
    first.validate().unwrap();
    assert_ne!(first.operation_id(), second.operation_id());
    assert_eq!(first.derive_identity(), first.clone().derive_identity());
    let mut changed_self_id = first.clone();
    let UiSeatControlRequest::Claim { operation_id, .. } = &mut changed_self_id else {
        unreachable!()
    };
    *operation_id = "44444444-4444-4444-8444-444444444444".into();
    assert_eq!(first.derive_identity(), changed_self_id.derive_identity());
    assert!(changed_self_id.validate().is_err());
}

#[test]
fn control_commitment_includes_seat_predecessor_nonce_action_and_original_producer() {
    let value = serde_json::to_value(fresh("11111111-1111-4111-8111-111111111111")).unwrap();
    let original: UiSeatControlRequest = serde_json::from_value(value.clone()).unwrap();
    for (pointer, replacement) in [
        ("/thread_id", serde_json::json!("other-seat")),
        ("/chain_root_id", serde_json::json!("other-root")),
        (
            "/predecessor/event_hash",
            serde_json::json!("cd".repeat(32)),
        ),
        (
            "/intent_nonce",
            serde_json::json!("55555555-5555-4555-8555-555555555555"),
        ),
    ] {
        let mut changed = value.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        let changed: UiSeatControlRequest = serde_json::from_value(changed).unwrap();
        assert_ne!(original.derive_identity(), changed.derive_identity());
        assert!(changed.validate().is_err());
    }
    let mut relinquish = value.clone();
    relinquish["action"] = serde_json::json!("relinquish");
    relinquish.as_object_mut().unwrap().remove("claim");
    relinquish["producer_incarnation"] = serde_json::json!("ef".repeat(32));
    relinquish["predecessor"]["kind"] = serde_json::json!("control");
    let request: UiSeatControlRequest = serde_json::from_value(relinquish.clone()).unwrap();
    let identity = request.derive_identity().unwrap();
    relinquish["action"] = serde_json::json!("close");
    let close: UiSeatControlRequest = serde_json::from_value(relinquish.clone()).unwrap();
    assert_ne!(identity, close.derive_identity().unwrap());
    relinquish["action"] = serde_json::json!("relinquish");
    relinquish["predecessor"]["event_hash"] = serde_json::json!("12".repeat(32));
    let after_same_incarnation: UiSeatControlRequest = serde_json::from_value(relinquish).unwrap();
    assert_ne!(identity, after_same_incarnation.derive_identity().unwrap());
}

#[test]
fn predecessor_random_id_contract_and_wrong_predecessor_kind_are_not_current() {
    let value = serde_json::to_value(fresh("11111111-1111-4111-8111-111111111111")).unwrap();
    for field in ["schema", "predecessor", "intent_nonce", "chain_root_id"] {
        let mut predecessor = value.clone();
        predecessor.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<UiSeatControlRequest>(predecessor).is_err());
    }
    let mut wrong_kind = value.clone();
    wrong_kind["predecessor"]["kind"] = serde_json::json!("control");
    assert!(
        serde_json::from_value::<UiSeatControlRequest>(wrong_kind)
            .unwrap()
            .derive_identity()
            .is_err()
    );
    let mut old_version = value.clone();
    old_version["schema"] = serde_json::json!(1);
    assert!(
        serde_json::from_value::<UiSeatControlRequest>(old_version)
            .unwrap()
            .derive_identity()
            .is_err()
    );
    let mut extra = value;
    extra["grant"] = serde_json::json!("not-a-control-field");
    assert!(serde_json::from_value::<UiSeatControlRequest>(extra).is_err());
}

fn testimony(mut request: UiSeatControlRequest) -> UiSeatProducer {
    request.assign_identity().unwrap();
    let (claim, predecessor, producer_incarnation, state) = match &request {
        UiSeatControlRequest::Claim { claim, .. } => (
            Some(claim.clone()),
            match claim {
                UiSeatProducerClaim::Fresh => None,
                UiSeatProducerClaim::Resume {
                    expected_producer_incarnation,
                }
                | UiSeatProducerClaim::Takeover {
                    expected_producer_incarnation,
                } => Some(expected_producer_incarnation.clone()),
            },
            "cd".repeat(32),
            UiSeatProducerState::Active,
        ),
        UiSeatControlRequest::Relinquish {
            producer_incarnation,
            ..
        } => (
            None,
            Some(producer_incarnation.clone()),
            producer_incarnation.clone(),
            UiSeatProducerState::Relinquished,
        ),
        UiSeatControlRequest::Close {
            producer_incarnation,
            ..
        } => (
            None,
            Some(producer_incarnation.clone()),
            producer_incarnation.clone(),
            UiSeatProducerState::Closed,
        ),
    };
    UiSeatProducer {
        schema_version: SEAT_PRODUCER_SCHEMA.into(),
        operation_id: request.operation_id().into(),
        request,
        claim,
        predecessor,
        producer_incarnation,
        state,
        next_engine_seq: 7,
    }
}

#[test]
fn v3_testimony_keeps_existing_wire_and_validates_original_result_and_subject() {
    let fresh_request = fresh("11111111-1111-4111-8111-111111111111");
    let first = testimony(fresh_request.clone());
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        json!({
            "schema_version":"ryeos.ui.seat.producer.v3", "request":fresh_request,
            "operation_id":"5fb13967-e0b2-8a65-877d-1eeba338294f", "claim":{"kind":"fresh"},
            "predecessor":null, "producer_incarnation":"cd".repeat(32), "state":"active",
            "next_engine_seq":7,
        })
    );
    let mut requests = vec![first.request.clone()];
    for kind in ["resume", "takeover", "relinquish", "close"] {
        let mut wire = serde_json::to_value(&first.request).unwrap();
        wire["predecessor"]["kind"] = json!("control");
        if kind == "resume" || kind == "takeover" {
            wire["claim"] = json!({"kind":kind,"expected_producer_incarnation":"ef".repeat(32)});
        } else {
            wire["action"] = json!(kind);
            wire.as_object_mut().unwrap().remove("claim");
            wire["producer_incarnation"] = json!("ef".repeat(32));
        }
        requests.push(serde_json::from_value(wire).unwrap());
    }
    for request in requests {
        let producer = testimony(request);
        producer.validate().unwrap();
        producer.validate_subject("root", "seat").unwrap();
        assert!(producer.validate_subject("other-root", "seat").is_err());
        assert!(producer.validate_subject("root", "other-seat").is_err());
        let wire = serde_json::to_value(&producer).unwrap();
        let decoded: UiSeatProducer = serde_json::from_value(wire.clone()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
        for (field, value) in [
            ("schema_version", json!("ryeos.ui.seat.producer.v2")),
            (
                "operation_id",
                json!("11111111-1111-4111-8111-111111111111"),
            ),
            ("predecessor", json!("12".repeat(32))),
        ] {
            let mut changed = wire.clone();
            changed[field] = value;
            assert!(
                serde_json::from_value::<UiSeatProducer>(changed)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut changed_claim = wire.clone();
        changed_claim["claim"] = if producer.claim.is_some() {
            Value::Null
        } else {
            json!({"kind":"fresh"})
        };
        assert!(
            serde_json::from_value::<UiSeatProducer>(changed_claim)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut changed_state = wire.clone();
        changed_state["state"] = if producer.state == UiSeatProducerState::Active {
            json!("closed")
        } else {
            json!("active")
        };
        assert!(
            serde_json::from_value::<UiSeatProducer>(changed_state)
                .unwrap()
                .validate()
                .is_err()
        );
        if producer.state != UiSeatProducerState::Active {
            let mut changed_producer = wire.clone();
            changed_producer["producer_incarnation"] = json!("34".repeat(32));
            assert!(
                serde_json::from_value::<UiSeatProducer>(changed_producer)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut extra = wire;
        extra["grant"] = json!(true);
        assert!(serde_json::from_value::<UiSeatProducer>(extra).is_err());
    }
}

#[test]
fn fresh_control_result_requires_a_canonical_producer_incarnation() {
    let original = testimony(fresh("11111111-1111-4111-8111-111111111111"));
    original.validate().unwrap();
    for incarnation in [
        String::new(),
        "a".repeat(63),
        "A".repeat(64),
        "g".repeat(64),
        "é".repeat(32),
    ] {
        let mut changed = original.clone();
        changed.producer_incarnation = incarnation;
        assert!(changed.validate().is_err());
        assert_eq!(
            changed.request.operation_id(),
            original.request.operation_id()
        );
    }
    let mut canonical = original;
    canonical.producer_incarnation = "00".repeat(32);
    canonical.validate().unwrap();
}
