//! The node's lease admission round trip through the real provider-neutral
//! adapter judge (`ryeos-remote-lease-core`), not a hand-written verdict.
//!
//! The node builds its canonical stage-one request from workload data that
//! does not list `lease.lifetime_bound`; the adapter side parses it with its
//! own strict reader, requires the canonical bytes back, and judges it with
//! `admit_before_spend` against provider facts; the node then reads the
//! adapter's canonical verdict bytes with `require_admitted`. A provider spec
//! that does not bind the provider timeout to the reserved lifetime (for
//! Modal, an allocate body whose timeout is not the reservation's maximum
//! lifetime) yields the typed `LeaseLifetimeUnattested`; the same spec that
//! binds it is admitted with its budget weight.
//!
//! A daemon end-to-end drive is not available: lease admission runs only for
//! a placed session whose admitted structured-session profile names lease
//! requirements, through a signed external lifecycle adapter that answers
//! `admit-lease`, and the only adapter the daemon harness can install (the
//! synthetic lifecycle adapter) does not implement that operation.

use std::collections::BTreeSet;

use lillux::process_control::CapabilityRequirement;
use ryeos_remote_lease_core::{
    AllowListCapability, EgressHostSyntax, EgressPolicy, EgressProtocol, GuestChannelProfile,
    LEASE_CREATE_BOUND, LEASE_LIFETIME_BOUND, LEASE_NETWORK_POLICY, LeaseAdmissionRequest,
    LeaseProviderFacts, NetworkPolicyCapability, NetworkPolicyType, admit_before_spend,
};

use super::{LeaseAdmissionInputs, LeaseLifetimeUnattested, canonical_request, require_admitted};

const ADAPTER: &str = "remote-lease-integration";

/// Provider facts as a signed spec would answer them. `binds_lifetime` is
/// whether its create operation takes its timeout from the reservation's
/// maximum lifetime, the only basis on which it attests
/// `lease.lifetime_bound`.
struct SpecFacts {
    binds_lifetime: bool,
    network_policy: NetworkPolicyCapability,
}

impl SpecFacts {
    fn new(binds_lifetime: bool) -> Self {
        Self {
            binds_lifetime,
            network_policy: NetworkPolicyCapability {
                creatable_types: vec![NetworkPolicyType::DenyAll, NetworkPolicyType::AllowList],
                allow_list: Some(AllowListCapability {
                    rule_protocol: EgressProtocol::Https,
                    max_rules: 4,
                    host_syntax: EgressHostSyntax::ExactDnsName,
                }),
            },
        }
    }
}

impl LeaseProviderFacts for SpecFacts {
    fn evidence_provider(&self) -> &str {
        "integration_provider_v1"
    }

    fn maximum_lease_seconds(&self) -> u32 {
        24 * 60 * 60
    }

    fn budget_weight(&self) -> u32 {
        3
    }

    fn attested_lease_guarantees(&self) -> BTreeSet<&'static str> {
        let mut guarantees = BTreeSet::from([LEASE_CREATE_BOUND, LEASE_NETWORK_POLICY]);
        if self.binds_lifetime {
            guarantees.insert(LEASE_LIFETIME_BOUND);
        }
        guarantees
    }

    fn guest_channel_profile(&self) -> anyhow::Result<GuestChannelProfile> {
        Ok(GuestChannelProfile::ControllerRelayedFrames)
    }

    fn guest_activation_declared(&self) -> bool {
        true
    }

    fn network_policy_capability(&self) -> &NetworkPolicyCapability {
        &self.network_policy
    }

    fn egress_evidence_basis(&self, policy: &EgressPolicy) -> String {
        format!(
            "integration provider creates exactly [{}]",
            policy.rules_text()
        )
    }

    fn valid_lease_id(&self, id: &str) -> bool {
        id.starts_with("lease-")
    }
}

fn provider_spec_sha256(binds_lifetime: bool) -> String {
    lillux::sha256_hex(
        format!("integration provider spec binds_lifetime={binds_lifetime}").as_bytes(),
    )
}

/// A workload that lists what it needs before spend but leaves the lifetime
/// guarantee to the node, as every shipped workload may.
fn workload_inputs() -> LeaseAdmissionInputs {
    let requirement = |value: serde_json::Value| -> CapabilityRequirement {
        serde_json::from_value(value).unwrap()
    };
    LeaseAdmissionInputs {
        workload: "codex.authoring.remote".into(),
        requires_before_spend: vec![
            requirement(serde_json::json!({
                "name": "lease.network_policy", "accept": ["provider_attested"]
            })),
            requirement(serde_json::json!({
                "name": "lease.create_bound", "accept": ["provider_attested"]
            })),
        ],
        requires_from_guest: Vec::new(),
        controller_host: "controller.example.com".into(),
        maximum_lifetime_seconds: 14_400,
        activation_deadline_seconds: 300,
    }
}

/// What the adapter process does with the node's request bytes: its strict
/// reader, its canonical-bytes check, its validation, then the stage-one
/// judgement, answered as its canonical verdict bytes.
fn adapter_verdict(request_bytes: &[u8], facts: &SpecFacts) -> Vec<u8> {
    let request: LeaseAdmissionRequest = ryeos_external_execution_contract::from_json_slice_strict(
        request_bytes,
        ryeos_external_execution_contract::MAX_LEASE_ADMISSION_BYTES,
    )
    .expect("the adapter reads the node's request");
    assert_eq!(
        ryeos_external_execution_contract::canonical_json(&request).unwrap(),
        request_bytes,
        "the node's request is the adapter's canonical form"
    );
    request.validate().expect("the node's request validates");
    assert_eq!(request.adapter_id, ADAPTER);
    let response = admit_before_spend(facts, &request, &lillux::sha256_hex(request_bytes));
    ryeos_external_execution_contract::canonical_json(&response).unwrap()
}

#[test]
fn a_spec_that_does_not_bind_the_lifetime_is_refused_typed_although_the_workload_omits_it() {
    let spec = provider_spec_sha256(false);
    let inputs = workload_inputs();
    assert!(
        inputs
            .requires_before_spend
            .iter()
            .all(|entry| entry.label() != LEASE_LIFETIME_BOUND),
        "the workload leaves the lifetime floor to the node"
    );
    let request = canonical_request(ADAPTER, &spec, &inputs).unwrap();
    let verdict = adapter_verdict(&request, &SpecFacts::new(false));
    let parsed: serde_json::Value = serde_json::from_slice(&verdict).unwrap();
    assert_eq!(parsed["verdict"], "refused", "{parsed}");
    assert!(
        parsed["unmet"].get(LEASE_LIFETIME_BOUND).is_some(),
        "the adapter judged the node's own floor: {parsed}"
    );

    let error = require_admitted(&verdict, &request, &spec, ADAPTER).unwrap_err();
    let typed = error
        .downcast_ref::<LeaseLifetimeUnattested>()
        .unwrap_or_else(|| panic!("untyped refusal: {error:#}"));
    assert_eq!(typed.adapter_id, ADAPTER);
    assert_eq!(typed.provider_spec_sha256, spec);
    assert!(
        typed.reason.contains("integration_provider_v1"),
        "the refusal carries the adapter's reason naming the spec: {}",
        typed.reason
    );
    let message = error.to_string();
    assert!(message.contains(LEASE_LIFETIME_BOUND), "{message}");
    assert!(message.contains(&spec), "{message}");
}

#[test]
fn a_spec_that_binds_the_lifetime_is_admitted_with_its_budget_weight() {
    let spec = provider_spec_sha256(true);
    let request = canonical_request(ADAPTER, &spec, &workload_inputs()).unwrap();
    let verdict = adapter_verdict(&request, &SpecFacts::new(true));
    let parsed: serde_json::Value = serde_json::from_slice(&verdict).unwrap();
    assert_eq!(parsed["verdict"], "admitted", "{parsed}");
    assert_eq!(
        parsed["met_by"][LEASE_LIFETIME_BOUND]["source"], "provider_attested",
        "{parsed}"
    );
    assert_eq!(
        require_admitted(&verdict, &request, &spec, ADAPTER).unwrap(),
        3
    );
}

#[test]
fn a_lifetime_above_the_providers_hard_limit_is_the_same_typed_refusal() {
    let spec = provider_spec_sha256(true);
    let mut inputs = workload_inputs();
    inputs.maximum_lifetime_seconds = 2 * 24 * 60 * 60;
    let request = canonical_request(ADAPTER, &spec, &inputs).unwrap();
    let verdict = adapter_verdict(&request, &SpecFacts::new(true));
    let error = require_admitted(&verdict, &request, &spec, ADAPTER).unwrap_err();
    let typed = error
        .downcast_ref::<LeaseLifetimeUnattested>()
        .unwrap_or_else(|| panic!("untyped refusal: {error:#}"));
    assert!(
        typed.reason.contains("hard lifetime"),
        "the adapter's lifetime reason survives: {}",
        typed.reason
    );
}
