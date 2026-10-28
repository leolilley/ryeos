//! Pure contract-boundary tests; no execution or signature admission is mocked.

use super::*;
use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace};
use ryeos_engine::hooks::{EFFECTIVE_HOOK_PLAN_DERIVED_KEY, EffectiveHookPlan};
use ryeos_engine::resolution::{KindComposedView, ResolutionStepName, ResolvedAncestor};
use serde_json::json;

#[test]
fn q_issuance_joins_actual_bound_turn_instead_of_signed_template() {
    use ryeos_external_execution_contract::qualification_request_binding::{
        QualificationRequestBindingInput, QualificationRequestBindingOutput,
    };
    use ryeos_state::external_content::products::qualification::{
        PRODUCT_QUALIFICATION_CONSUMER_WORKER_REQUEST_SCHEMA,
        ProductQualificationConsumerWorkerRequest,
    };
    use ryeos_state::external_execution::qualification_controller_canary::{
        QControllerCanaryBinding, QControllerCanaryContract, QControllerCanaryEvidence,
    };
    // Inert commitment fixture, not a fabricated adapter invocation or file proof.
    let mut request = ProductQualificationConsumerWorkerRequest {
        schema: PRODUCT_QUALIFICATION_CONSUMER_WORKER_REQUEST_SCHEMA.into(),
        credential_profile_id: None,
        fixture: None,
        controller_canary: Some(QControllerCanaryContract::OriginalControllerReadAndLocalRefusalV1),
        session_setup: json!({"provider_defined":"setup"}),
        body: json!({"provider_defined":"template"}),
    };
    let input = QualificationRequestBindingInput {
        schema: 1,
        template: request.body.clone(),
        controller_canary_path: "/private/controller-canary/canary".into(),
    };
    let output = QualificationRequestBindingOutput {
        schema: 1,
        input_digest: input.digest().unwrap(),
        resolved_body: json!({"provider_defined":"actual bound turn"}),
    };
    let binding: QControllerCanaryBinding = serde_json::from_value(json!({
        "schema":1,"controller_path":input.controller_canary_path,
        "initial_file":{"directory_identity":{"filesystem": {"device": 1},"inode": 2},
            "file_identity":{"filesystem": {"device": 1},"inode": 3},
            "content_sha256":"a".repeat(64),"size_bytes":65,"permission_mode":384},
        "provider_declaration_id":"provider","configuration_adapter_artifact_hash":"b".repeat(64),
        "configuration_adapter_artifact_bytes":1,
        "binding_input_digest":input.digest().unwrap(),"binding_output_digest":output.digest().unwrap(),
        "template_request_digest":canonical_value_digest(&request.body).unwrap(),
        "resolved_request_digest":canonical_value_digest(&output.resolved_body).unwrap(),
        "resolved_body":output.resolved_body,
    })).unwrap();
    let evidence = QControllerCanaryEvidence {
        schema: 1,
        terminal_file: binding.initial_file.clone(),
        binding,
    };
    let setup = canonical_value_digest(&request.session_setup).unwrap();
    let template = canonical_value_digest(&request.body).unwrap();
    let actual = &evidence.binding.resolved_request_digest;
    require_qualification_worker_request(&request, &setup, actual, Some(&evidence)).unwrap();
    assert!(
        require_qualification_worker_request(&request, &setup, &template, Some(&evidence)).is_err()
    );
    assert!(require_qualification_worker_request(&request, &setup, actual, None).is_err());
    assert!(
        require_qualification_worker_request(&request, &template, actual, Some(&evidence)).is_err()
    );
    let mut changed = evidence.clone();
    changed.terminal_file.content_sha256 = "f".repeat(64);
    assert!(
        require_qualification_worker_request(&request, &setup, actual, Some(&changed)).is_err()
    );
    request.body = json!({"provider_defined":"another template"});
    assert!(
        require_qualification_worker_request(&request, &setup, actual, Some(&evidence)).is_err()
    );
    request.controller_canary = None;
    let literal = canonical_value_digest(&request.body).unwrap();
    require_qualification_worker_request(&request, &setup, &literal, None).unwrap();
    assert!(
        require_qualification_worker_request(&request, &setup, &literal, Some(&evidence)).is_err()
    );
}

#[test]
fn zero_participant_qualification_requires_exact_subordinate_authority() {
    use ryeos_engine::protocol_vocabulary::CallbackChannel;
    use ryeos_engine::protocols::QualificationCallbackAuthority::{RemoteConsumer, ScopedProducer};
    for subordinate in [None, Some(ScopedProducer), Some(RemoteConsumer)] {
        for purpose in [None, Some(ScopedProducer), Some(RemoteConsumer)] {
            for channel in [CallbackChannel::None, CallbackChannel::Http] {
                let admitted = subordinate == purpose
                    && matches!(
                        (subordinate, channel),
                        (None, CallbackChannel::None) | (Some(_), CallbackChannel::Http)
                    );
                assert_eq!(
                    require_zero_participant_lane(subordinate, purpose, channel).is_ok(),
                    admitted,
                    "subordinate={subordinate:?}, purpose={purpose:?}, channel={channel:?}"
                );
            }
        }
    }
}

#[test]
fn direct_settlement_requires_one_signed_terminal_digest() {
    let event = |payload| {
        ryeos_state::objects::thread_event::NewEvent::new(
            "T-verifier",
            "T-verifier",
            ryeos_state::event_types::THREAD_COMPLETED,
        )
        .payload(payload)
        .build()
    };
    let exact = event(json!({"process_settlement_witness_digest":"a".repeat(64)}));
    assert_eq!(
        signed_terminal_process_settlement_digest(std::slice::from_ref(&exact)).unwrap(),
        "a".repeat(64)
    );
    assert!(signed_terminal_process_settlement_digest(&[]).is_err());
    assert!(signed_terminal_process_settlement_digest(&[event(json!({}))]).is_err());
    assert!(
        signed_terminal_process_settlement_digest(&[event(json!({
            "process_settlement_witness_digest":"not-a-hash"
        }))])
        .is_err()
    );
    assert!(signed_terminal_process_settlement_digest(&[exact.clone(), exact]).is_err());
}

fn resolution() -> ResolutionOutput {
    ResolutionOutput {
        root: ResolvedAncestor {
            requested_id: "tool:test/probe".into(),
            resolved_ref: "tool:test/probe".into(),
            source_path: "/diagnostic/probe.yaml".into(),
            source_space: ItemSpace::Project,
            source_root: ItemSourceRoot::Project,
            trust_class: TrustClass::TrustedProject,
            signer_fingerprint: Some("a".repeat(64)),
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: "fixture".into(),
            source_content_digest: "b".repeat(64),
            raw_content_digest: "c".repeat(64),
        },
        ancestors: Vec::new(),
        references_edges: Vec::new(),
        referenced_items: Vec::new(),
        step_outputs: Default::default(),
        effective_trust_class: TrustClass::TrustedProject,
        composed: KindComposedView {
            composed: json!({"arbitrary_contract_field":{"keep":true}}),
            derived: Default::default(),
            policy_facts: Default::default(),
        },
    }
}

fn required_call() -> ExecutionEvidenceRequiredCallWire {
    ExecutionEvidenceRequiredCallWire {
        call_id: "opaque-call".into(),
        request: json!({
            "item_id":"tool:test/probe", "params":{"value":17},
            "thread":"inline", "ref_bindings":{}, "product_selections":[]
        }),
    }
}

#[test]
fn inline_participant_is_a_same_chain_child_of_its_parent() {
    use ryeos_state::objects::thread_snapshot::ThreadSnapshotBuilder;
    let parent = ThreadSnapshotBuilder::new(
        "T-parent",
        "T-root",
        "graph_run",
        "graph:test/flow",
        "@graph",
    )
    .build();
    let mut child = ThreadSnapshotBuilder::new(
        "T-child",
        "T-root",
        "tool_run",
        "tool:test/probe",
        "@subprocess",
    )
    .build();
    child.upstream_thread_id = Some("T-parent".into());
    require_inline_child_placement(&child, &parent, "T-child").unwrap();
    assert!(require_inline_child_placement(&child, &parent, "T-another-child").is_err());

    let mut independent_root = child.clone();
    independent_root.chain_root_id = "T-child".into();
    independent_root.upstream_thread_id = None;
    assert!(require_inline_child_placement(&independent_root, &parent, "T-child").is_err());

    let mut other_chain = child.clone();
    other_chain.chain_root_id = "T-other-root".into();
    assert!(require_inline_child_placement(&other_chain, &parent, "T-child").is_err());

    let mut other_upstream = child.clone();
    other_upstream.upstream_thread_id = Some("T-sibling".into());
    assert!(require_inline_child_placement(&other_upstream, &parent, "T-child").is_err());

    let mut no_upstream = child;
    no_upstream.upstream_thread_id = None;
    assert!(require_inline_child_placement(&no_upstream, &parent, "T-child").is_err());
}

#[test]
fn static_action_preserves_inputs_but_refuses_every_unsupported_control() {
    let original = required_call();
    let parsed = static_action(&original).unwrap();
    assert_eq!(parsed.item_id, "tool:test/probe");
    assert_eq!(parsed.params, json!({"value":17}));
    assert!(parsed.operation_id.is_none());
    for (field, value) in [
        ("operation_id", json!("a".repeat(64))),
        ("thread", json!("detached")),
        ("call", json!({"method":"inspect"})),
        ("facets", json!({"cohort":"other"})),
        ("launch_window", json!({"key":"window","width":1})),
        ("ref_bindings", json!({"runtime":"config:test/other"})),
        (
            "product_selections",
            json!([{
                "target":{"kind":"root"},
                "selection":{
                    "declaration_id":"subject",
                    "witness_hash":"b".repeat(64),
                    "witness_source":{"kind":"local_capture"},
                    "qualification_hash":null
                }
            }]),
        ),
        ("unrecognized_control", json!(true)),
    ] {
        let mut changed = original.clone();
        changed.request[field] = value;
        if field != "unrecognized_control" {
            serde_json::from_value::<ryeos_runtime::callback::ActionPayload>(
                changed.request.clone(),
            )
            .unwrap_or_else(|error| panic!("invalid {field} fixture: {error}"));
        }
        assert!(static_action(&changed).is_err(), "accepted {field}");
    }
    // Kind eligibility is not a string allowlist in this generic boundary.
    let mut other_kind = original;
    other_kind.request["item_id"] = json!("directive:test/probe");
    assert_eq!(
        static_action(&other_kind).unwrap().item_id,
        "directive:test/probe"
    );
}

#[test]
fn program_projection_preserves_composed_derived_policy_and_ancestor_order() {
    let mut resolution = resolution();
    resolution
        .composed
        .derived
        .insert("source_closure".into(), json!({"proof":"retained"}));
    resolution
        .composed
        .derived
        .insert("effective_hook_plan".into(), json!({"must_not_drop":true}));
    resolution
        .composed
        .policy_facts
        .insert("effective_caps".into(), json!(["one", "two"]));
    for name in ["config:test/deep", "config:test/near"] {
        let mut ancestor = resolution.root.clone();
        ancestor.requested_id = name.into();
        resolution.ancestors.push(ancestor);
    }
    let projected = program(&resolution, &"d".repeat(64)).unwrap();
    assert_eq!(projected.canonical_ref, resolution.root.resolved_ref);
    assert_eq!(projected.effective_definition_digest, "d".repeat(64));
    assert_eq!(
        serde_json::to_value(projected.composed).unwrap(),
        serde_json::to_value(&resolution.composed).unwrap()
    );
    assert_eq!(
        projected.ancestor_requested_ids,
        ["config:test/deep", "config:test/near"]
    );
}

fn empty_hook_plan() -> Value {
    json!({
        "schema":ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_SCHEMA,
        "owner_kind":"contract-defined-kind",
        "event_contracts":{"completed":{
            "context_contract":{"schema":ryeos_engine::hooks::HOOK_CONTEXT_SCHEMA,"allowed_roots":["event"]},
            "allowed_results":["observation"]
        }},
        "authored":{"hooks":[],"dispatch_caps":[]},
        "builtin":{"hooks":[],"dispatch_caps":[]},
        "infrastructure":{"hooks":[],"dispatch_caps":[]},
        "context":{"hooks":[],"dispatch_caps":[]},
        "operator":{"hooks":[],"dispatch_caps":[]},
        "project":{"hooks":[],"dispatch_caps":[]},
        "sources":[]
    })
}

#[test]
fn leaf_proof_rejects_real_hooks_in_every_layer_and_malformed_plans() {
    let mut resolution = resolution();
    require_hookless_participant(&resolution).unwrap();
    let empty = empty_hook_plan();
    EffectiveHookPlan::from_value(&empty).unwrap();
    resolution
        .composed
        .derived
        .insert(EFFECTIVE_HOOK_PLAN_DERIVED_KEY.into(), empty.clone());
    require_hookless_participant(&resolution).unwrap();
    for (layer, space, trust) in [
        ("authored", "", ""),
        ("builtin", "bundle", "trusted_bundle"),
        ("infrastructure", "bundle", "trusted_bundle"),
        ("context", "bundle", "trusted_bundle"),
        ("operator", "node", "trusted_node"),
        ("project", "project", "trusted_project"),
    ] {
        let mut hooks = empty.clone();
        hooks[layer]["hooks"] = json!([{
            "id":"observe", "event":"completed", "result":"observation",
            "action":{"item_id":"tool:test/hidden", "ref_bindings":{}, "params":{}}
        }]);
        if layer != "authored" {
            hooks["sources"] = json!([{
                "layer":layer, "canonical_ref":"config:test/hooks", "source_space":space,
                "trust_class":trust, "signer_fingerprint":"a".repeat(64),
                "source_raw_content_digest":"b".repeat(64)
            }]);
        }
        // Prove refusal is for executable hooks, not a malformed fixture.
        EffectiveHookPlan::from_value(&hooks).unwrap();
        resolution
            .composed
            .derived
            .insert(EFFECTIVE_HOOK_PLAN_DERIVED_KEY.into(), hooks);
        assert!(
            require_hookless_participant(&resolution)
                .unwrap_err()
                .to_string()
                .contains("executable hooks"),
            "accepted {layer}"
        );
    }
    for malformed in [Value::Null, json!({}), json!({"schema":"predecessor"})] {
        resolution
            .composed
            .derived
            .insert(EFFECTIVE_HOOK_PLAN_DERIVED_KEY.into(), malformed);
        assert!(require_hookless_participant(&resolution).is_err());
    }
}

#[test]
fn projector_identity_retains_descriptor_and_binary_authority_independently() {
    let source = ryeos_engine::handlers::VerifiedExecutionEvidenceProjectorIdentity {
        canonical_ref: "tool:test/projector".into(),
        descriptor_content_digest: "1".repeat(64),
        descriptor_signer_fingerprint: "2".repeat(64),
        binary_content_digest: "3".repeat(64),
        binary_manifest_digest: "4".repeat(64),
        binary_signer_fingerprint: "5".repeat(64),
    };
    let projected = projector_identity(&source);
    assert_eq!(
        serde_json::to_value(&projected).unwrap(),
        json!({
            "canonical_ref":"tool:test/projector", "descriptor_content_digest":"1".repeat(64),
            "descriptor_signer_fingerprint":"2".repeat(64), "binary_content_digest":"3".repeat(64),
            "binary_manifest_digest":"4".repeat(64), "binary_signer_fingerprint":"5".repeat(64)
        })
    );
    for field in 0..6 {
        let mut changed = source.clone();
        match field {
            0 => changed.canonical_ref = "tool:test/replacement".into(),
            1 => changed.descriptor_content_digest = "6".repeat(64),
            2 => changed.descriptor_signer_fingerprint = "6".repeat(64),
            3 => changed.binary_content_digest = "6".repeat(64),
            4 => changed.binary_manifest_digest = "6".repeat(64),
            5 => changed.binary_signer_fingerprint = "6".repeat(64),
            _ => unreachable!(),
        }
        assert_ne!(
            projector_identity(&changed),
            projected,
            "lost field {field}"
        );
    }
}

#[test]
fn projector_compatibility_ignores_only_unrelated_executor_set_changes() {
    let retained = ProductQualificationProjectorIdentity {
        canonical_ref: "handler:test/projector".into(),
        descriptor_content_digest: "1".repeat(64),
        descriptor_signer_fingerprint: "2".repeat(64),
        binary_content_digest: "3".repeat(64),
        binary_manifest_digest: "4".repeat(64),
        binary_signer_fingerprint: "5".repeat(64),
    };
    let mut current = retained.clone();
    assert!(compatible_projector_identity(&current, &retained));
    current.binary_manifest_digest = "6".repeat(64);
    assert!(compatible_projector_identity(&current, &retained));
    assert_ne!(
        current, retained,
        "aggregate provenance must remain retained"
    );

    for change in [
        |value: &mut ProductQualificationProjectorIdentity| {
            value.canonical_ref = "handler:test/other".into()
        },
        |value: &mut ProductQualificationProjectorIdentity| {
            value.descriptor_content_digest = "7".repeat(64)
        },
        |value: &mut ProductQualificationProjectorIdentity| {
            value.descriptor_signer_fingerprint = "7".repeat(64)
        },
        |value: &mut ProductQualificationProjectorIdentity| {
            value.binary_content_digest = "7".repeat(64)
        },
        |value: &mut ProductQualificationProjectorIdentity| {
            value.binary_signer_fingerprint = "7".repeat(64)
        },
    ] {
        let mut changed = current.clone();
        change(&mut changed);
        assert!(!compatible_projector_identity(&changed, &retained));
    }
}

#[test]
fn projector_refusal_is_not_an_empty_successful_description() {
    assert!(
        described(ExecutionEvidenceDescribeResponse::Refused {
            message: "unsupported contract".into()
        })
        .is_err()
    );
    assert!(
        described(ExecutionEvidenceDescribeResponse::Described {
            required_calls: Vec::new()
        })
        .unwrap()
        .is_empty()
    );
}

#[test]
fn probe_evidence_must_report_exactly_the_retained_member_digests() {
    let supervisor = lillux::sha256_hex(b"supervisor");
    let launcher = lillux::sha256_hex(b"launcher");
    let retained = std::collections::BTreeMap::from([
        ("launcher".to_owned(), launcher.clone()),
        ("supervisor".to_owned(), supervisor.clone()),
    ]);
    let field = ryeos_external_execution::restoration_verifier_delivery::CONSUMER_EVIDENCE_ARCHIVE_MEMBERS_FIELD;
    let matching = json!({ field: { "launcher": launcher, "supervisor": supervisor } });
    require_evidence_member_digests(&matching, &retained).unwrap();

    let swapped =
        json!({ field: { "launcher": lillux::sha256_hex(b"other"), "supervisor": supervisor } });
    assert!(require_evidence_member_digests(&swapped, &retained).is_err());
    let missing = json!({ field: { "supervisor": supervisor } });
    assert!(require_evidence_member_digests(&missing, &retained).is_err());
    let extra = json!({ field: { "launcher": launcher, "supervisor": supervisor, "planted": lillux::sha256_hex(b"planted") } });
    assert!(require_evidence_member_digests(&extra, &retained).is_err());
    assert!(require_evidence_member_digests(&json!({}), &retained).is_err());
    let malformed = json!({ field: { "launcher": "not-a-digest", "supervisor": supervisor } });
    assert!(require_evidence_member_digests(&malformed, &retained).is_err());
    assert!(require_evidence_member_digests(&json!({ field: [] }), &retained).is_err());
    // No members retained: evidence naming none, or an empty map, is exact.
    let none = std::collections::BTreeMap::new();
    require_evidence_member_digests(&json!({}), &none).unwrap();
    require_evidence_member_digests(&json!({ field: {} }), &none).unwrap();
    assert!(require_evidence_member_digests(&matching, &none).is_err());
}
