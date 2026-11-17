//! The synthetic adapter's lease operations.
//!
//! The synthetic provider spec declares operation kinds and proof profiles, and
//! the lease guarantees this adapter attests are DERIVED from them, as the Render
//! adapter derives its own: a spec can name a mechanism, never a guarantee. Stage
//! one admission and the guest Ready check are the shared lease core's, run over
//! this spec: nothing here admits a lease by itself.

use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use ryeos_remote_lease_core::{
    EgressPolicy, GUEST_READY_AUTHENTICATED, GuestChannelProfile, LEASE_CREATE_BOUND,
    LEASE_LIFETIME_BOUND, LEASE_NETWORK_POLICY, LEASE_TERMINATE_OBSERVED, LeaseProviderFacts,
    NetworkPolicyCapability,
};
use serde::Deserialize;

/// The adapter id the e2e bundle declares for this adapter.
pub(crate) const ADAPTER_ID: &str = "synthetic-local";

/// Provider id on the evidence this spec yields.
pub(crate) const SPEC_EVIDENCE_PROVIDER: &str = "synthetic-lease";

/// An operation the spec declares: its kind, and the proof or source profile
/// the mechanism uses.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    kind: String,
    #[serde(default)]
    bind_proof_profile: Option<String>,
    #[serde(default)]
    timeout_source: Option<String>,
    #[serde(default)]
    network_policy_source: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operations {
    allocate: Operation,
    terminate: Operation,
    guest_channel: Operation,
    activate_supervisor: Operation,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyntheticSpec {
    schema: u32,
    maximum_lease_seconds: u32,
    budget_weight: u32,
    network_policy: NetworkPolicyCapability,
    operations: Operations,
}

impl SyntheticSpec {
    /// Parse the captured spec bytes under their captured digest.
    pub(crate) fn parse(bytes: &[u8], captured_sha256: &str) -> Result<Self> {
        ensure!(
            lillux::sha256_hex(bytes) == captured_sha256,
            "synthetic provider spec differs from its captured digest"
        );
        let spec: Self = serde_json::from_slice(bytes)?;
        ensure!(
            spec.schema == 2,
            "unsupported synthetic provider spec schema"
        );
        ensure!(
            spec.budget_weight >= 1 && spec.maximum_lease_seconds >= 1,
            "synthetic provider spec budget weight and lifetime must be positive"
        );
        spec.network_policy.validate()?;
        Ok(spec)
    }
}

impl LeaseProviderFacts for SyntheticSpec {
    fn evidence_provider(&self) -> &str {
        SPEC_EVIDENCE_PROVIDER
    }

    fn maximum_lease_seconds(&self) -> u32 {
        self.maximum_lease_seconds
    }

    fn budget_weight(&self) -> u32 {
        self.budget_weight
    }

    /// Derived only from the declared operation kinds and profiles.
    fn attested_lease_guarantees(&self) -> BTreeSet<&'static str> {
        let mut guarantees = BTreeSet::new();
        let allocate = &self.operations.allocate;
        if allocate.kind == "create_once"
            && allocate.bind_proof_profile.as_deref() == Some("synthetic_create_v1")
        {
            guarantees.insert(LEASE_CREATE_BOUND);
        }
        if allocate.timeout_source.as_deref() == Some("reservation") {
            guarantees.insert(LEASE_LIFETIME_BOUND);
        }
        if allocate.network_policy_source.as_deref() == Some("lease_guest_network_allow") {
            guarantees.insert(LEASE_NETWORK_POLICY);
        }
        if self.operations.terminate.kind == "terminate_once_then_observe_exact" {
            guarantees.insert(LEASE_TERMINATE_OBSERVED);
        }
        if self.guest_channel_profile().ok() == Some(GuestChannelProfile::ControllerRelayedFrames)
            && self.guest_activation_declared()
        {
            guarantees.insert(GUEST_READY_AUTHENTICATED);
        }
        guarantees
    }

    fn guest_channel_profile(&self) -> Result<GuestChannelProfile> {
        match self.operations.guest_channel.kind.as_str() {
            "unsupported_pending" => Ok(GuestChannelProfile::UnsupportedPending),
            "controller_relayed_guest_frames" => Ok(GuestChannelProfile::ControllerRelayedFrames),
            other => anyhow::bail!(
                "synthetic provider spec guest channel kind `{other}` is not an implemented transport"
            ),
        }
    }

    fn guest_activation_declared(&self) -> bool {
        self.operations.activate_supervisor.kind == "one_shot_pending_activation"
    }

    fn network_policy_capability(&self) -> &NetworkPolicyCapability {
        &self.network_policy
    }

    fn egress_evidence_basis(&self, policy: &EgressPolicy) -> String {
        format!(
            "the signed synthetic provider spec models this lease locally with network policy [{}]; \
             nothing is enforced beyond the local occurrence",
            policy.rules_text()
        )
    }

    /// The synthetic adapter names an occurrence `occ-` and 48 hex digits.
    fn valid_lease_id(&self, id: &str) -> bool {
        id.strip_prefix("occ-").is_some_and(|rest| {
            rest.len() == 48 && rest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    }
}

/// Stage-one admission over the shared lease core.
pub(crate) fn admit() -> Result<()> {
    ryeos_remote_lease_core::admit_lease(ADAPTER_ID, SyntheticSpec::parse)
}

/// Guest Ready verification over the shared lease core.
pub(crate) fn await_ready() -> Result<()> {
    ryeos_remote_lease_core::await_lease_ready(ADAPTER_ID, SyntheticSpec::parse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::process_control::{CapabilityRequirement, CapabilityRequirementSpec};
    use ryeos_remote_lease_core::{
        LEASE_SCHEMA, LeaseAdmissionRequest, LeaseAdmissionResponse, admit_before_spend,
    };

    /// The signed spec the e2e bundle publishes.
    pub(crate) fn spec_json() -> serde_json::Value {
        serde_json::from_slice(include_bytes!(
            "../../fixtures/synthetic-provider-spec.json"
        ))
        .unwrap()
    }

    fn parse(value: &serde_json::Value) -> Result<SyntheticSpec> {
        let bytes = serde_json::to_vec(value).unwrap();
        SyntheticSpec::parse(&bytes, &lillux::sha256_hex(&bytes))
    }

    fn provider_attested(name: &str) -> CapabilityRequirement {
        CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: Some(name.to_owned()),
            any_of: Vec::new(),
            accept: vec!["provider_attested".to_owned()],
            providers: Vec::new(),
        })
    }

    fn codex_lease_floor() -> Vec<CapabilityRequirement> {
        [
            GUEST_READY_AUTHENTICATED,
            LEASE_CREATE_BOUND,
            LEASE_LIFETIME_BOUND,
            LEASE_NETWORK_POLICY,
            LEASE_TERMINATE_OBSERVED,
        ]
        .into_iter()
        .map(provider_attested)
        .collect()
    }

    fn request(requires: Vec<CapabilityRequirement>) -> LeaseAdmissionRequest {
        LeaseAdmissionRequest {
            schema: LEASE_SCHEMA,
            adapter_id: ADAPTER_ID.into(),
            provider_spec_sha256: "a".repeat(64),
            workload: "codex.authoring.remote".into(),
            requires_before_spend: requires,
            requires_from_guest: Vec::new(),
            guest_network_allow: ["controller.example".to_owned()].into(),
            maximum_lifetime_seconds: 600,
            activation_deadline_seconds: 300,
        }
    }

    #[test]
    fn the_signed_spec_attests_exactly_what_its_operations_implement() {
        let spec = parse(&spec_json()).unwrap();
        assert_eq!(
            spec.attested_lease_guarantees(),
            BTreeSet::from([
                GUEST_READY_AUTHENTICATED,
                LEASE_CREATE_BOUND,
                LEASE_LIFETIME_BOUND,
                LEASE_NETWORK_POLICY,
                LEASE_TERMINATE_OBSERVED
            ])
        );
        // Removing a mechanism removes the guarantee it derived.
        let mut without = spec_json();
        without["operations"]["allocate"]["network_policy_source"] = serde_json::Value::Null;
        without["operations"]["allocate"]
            .as_object_mut()
            .unwrap()
            .remove("network_policy_source");
        let spec = parse(&without).unwrap();
        assert!(
            !spec
                .attested_lease_guarantees()
                .contains(LEASE_NETWORK_POLICY)
        );
    }

    #[test]
    fn the_shared_core_admits_the_codex_lease_floor_and_refuses_what_the_spec_lacks() {
        let spec = parse(&spec_json()).unwrap();
        let admitted = request(codex_lease_floor());
        admitted.validate().unwrap();
        let LeaseAdmissionResponse::Admitted { .. } =
            admit_before_spend(&spec, &admitted, &"b".repeat(64))
        else {
            panic!("the synthetic spec meets the codex lease floor");
        };

        let mut lacking = spec_json();
        lacking["operations"]["terminate"] = serde_json::json!({"kind": "terminate_once"});
        let spec = parse(&lacking).unwrap();
        let LeaseAdmissionResponse::Refused { unmet, .. } =
            admit_before_spend(&spec, &admitted, &"b".repeat(64))
        else {
            panic!("a spec with no observed termination cannot meet the floor");
        };
        assert!(unmet.contains_key(LEASE_TERMINATE_OBSERVED), "{unmet:?}");
    }

    #[test]
    fn a_digest_mismatch_and_a_bare_guarantee_name_in_the_spec_are_refused() {
        let bytes = serde_json::to_vec(&spec_json()).unwrap();
        assert!(SyntheticSpec::parse(&bytes, &"0".repeat(64)).is_err());
        // The spec carries mechanisms only; a guarantee list is not a field.
        let mut with_names = spec_json();
        with_names["attested_guarantees"] = serde_json::json!([LEASE_CREATE_BOUND]);
        assert!(parse(&with_names).is_err());
    }
}
