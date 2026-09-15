//! Pure contract-boundary tests; no execution or signature admission is mocked.

use super::*;
use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace};
use ryeos_engine::hooks::{EFFECTIVE_HOOK_PLAN_DERIVED_KEY, EffectiveHookPlan};
use ryeos_engine::resolution::{KindComposedView, ResolutionStepName, ResolvedAncestor};
use serde_json::json;

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
fn inline_participant_is_an_independent_root_not_a_continuation() {
    let child = ryeos_state::objects::thread_snapshot::ThreadSnapshotBuilder::new(
        "T-child",
        "T-child",
        "tool_run",
        "tool:test/probe",
        "@subprocess",
    )
    .build();
    require_inline_child_root(&child, "T-child").unwrap();
    assert!(require_inline_child_root(&child, "T-another-child").is_err());

    let mut continuation = child.clone();
    continuation.chain_root_id = "T-parent".into();
    assert!(require_inline_child_root(&continuation, "T-child").is_err());

    let mut upstream = child;
    upstream.upstream_thread_id = Some("T-parent".into());
    assert!(require_inline_child_root(&upstream, "T-child").is_err());
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
                "selection":{"declaration_id":"subject", "witness_hash":"b".repeat(64), "qualification_hash":null}
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
