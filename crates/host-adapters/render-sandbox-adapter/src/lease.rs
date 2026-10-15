//! Render's answers to the shared lease core's provider questions.
//!
//! Lease admission, guest Ready verification, resume state and the relayed
//! guest-frame transport live in `ryeos_remote_lease_core`. What a signed
//! Render spec establishes for them is read here: the evidence provider id,
//! Render's hard Sandbox lifetime, the guarantees derived from the spec's
//! implemented operation kinds, the selected guest channel, the egress policy
//! Render can create and what its echo proves, and the Sandbox id form.

use std::collections::BTreeSet;

use anyhow::Result;
use ryeos_remote_lease_core::{
    EgressPolicy, GuestChannelProfile, LeaseProviderFacts, NetworkPolicyCapability,
    NetworkPolicyType,
};

use crate::provider_spec::{ProviderSpec, SPEC_EVIDENCE_PROVIDER};

/// Render's hard Sandbox lifetime. A lease asking for more cannot be backed.
const PROVIDER_MAXIMUM_LEASE_SECONDS: u32 = 24 * 60 * 60;

impl LeaseProviderFacts for ProviderSpec {
    fn evidence_provider(&self) -> &str {
        SPEC_EVIDENCE_PROVIDER
    }

    fn maximum_lease_seconds(&self) -> u32 {
        PROVIDER_MAXIMUM_LEASE_SECONDS
    }

    fn attested_lease_guarantees(&self) -> BTreeSet<&'static str> {
        self.lease_guarantees()
    }

    fn guest_channel_profile(&self) -> Result<GuestChannelProfile> {
        self.guest_channel()
    }

    fn guest_activation_declared(&self) -> bool {
        self.one_shot_activation_enabled()
    }

    fn network_policy_capability(&self) -> &NetworkPolicyCapability {
        ProviderSpec::network_policy_capability(self)
    }

    /// Includes what the measured Render behaviour leaves open.
    fn egress_evidence_basis(&self, policy: &EgressPolicy) -> String {
        match policy.policy_type {
            NetworkPolicyType::DenyAll => {
                "the signed provider spec creates this lease with Render networkPolicy type \
                 deny-all and no rules, and binds the lease only when Render echoes exactly that; \
                 measured: guest DNS resolution fails under deny-all"
                    .to_owned()
            }
            NetworkPolicyType::AllowList => format!(
                "the signed provider spec creates this lease with Render networkPolicy type \
                 allow-list and exactly the rules [{}], and binds the lease only when Render \
                 echoes exactly those; measured: only listed hosts are reachable, over HTTPS \
                 only, matched by exact hostname with TLS end to end; residual: DNS resolution \
                 of unlisted names is not filtered, so data can leave in DNS queries",
                policy.rules_text()
            ),
        }
    }

    fn valid_lease_id(&self, id: &str) -> bool {
        crate::valid_sandbox_id(id)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use lillux::process_control::{CapabilityRequirement, CapabilityRequirementSpec, ProofSource};
    use ryeos_remote_lease_core::{
        EgressPolicy, GUEST_READY_AUTHENTICATED, LEASE_CREATE_BOUND, LEASE_LIFETIME_BOUND,
        LEASE_NETWORK_POLICY, LEASE_SCHEMA, LEASE_TERMINATE_OBSERVED, LeaseAdmissionRequest,
        LeaseAdmissionResponse, RequirementMet, admit_before_spend,
    };

    use ryeos_remote_lease_core::LeaseProviderFacts;

    use crate::provider_spec::{ProviderSpec, SPEC_EVIDENCE_PROVIDER};

    const SETTINGS_SCHEMA_DIGEST: &str =
        "f5b4994d71a6896ddad77bf2a08716fdc3527ea4f91579f084112ca69dad7172";

    fn signed_spec() -> ProviderSpec {
        ProviderSpec::parse(
            include_bytes!("../../../../bundles/render-sandbox/specs/provider-spec.json"),
            SETTINGS_SCHEMA_DIGEST,
        )
        .unwrap()
    }

    /// The authored spec with its guest channel declared absent.
    fn pending_channel_spec() -> ProviderSpec {
        let mut value: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../../bundles/render-sandbox/specs/provider-spec.json"
        ))
        .unwrap();
        value["operations"]["guest_channel"] = serde_json::json!({"kind": "unsupported_pending"});
        ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST).unwrap()
    }

    fn provider_attested(name: &str) -> CapabilityRequirement {
        CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: Some(name.to_owned()),
            any_of: Vec::new(),
            accept: vec!["provider_attested".to_owned()],
            providers: vec![SPEC_EVIDENCE_PROVIDER.to_owned()],
        })
    }

    fn admission(
        requires: Vec<CapabilityRequirement>,
        from_guest: Vec<CapabilityRequirement>,
    ) -> LeaseAdmissionRequest {
        LeaseAdmissionRequest {
            schema: LEASE_SCHEMA,
            adapter_id: crate::ADAPTER_ID.into(),
            provider_spec_sha256: "a".repeat(64),
            workload: "codex.authoring.remote".into(),
            requires_before_spend: requires,
            requires_from_guest: from_guest,
            guest_network_allow: BTreeSet::new(),
            maximum_lifetime_seconds: 4 * 60 * 60,
            activation_deadline_seconds: 5 * 60,
            ready_clock_skew_ms: None,
        }
    }

    #[test]
    fn stage_one_judges_spec_evidence_by_grade_and_provider() {
        let spec = signed_spec();
        let request = admission(
            vec![
                provider_attested(LEASE_CREATE_BOUND),
                provider_attested(LEASE_LIFETIME_BOUND),
                provider_attested(LEASE_NETWORK_POLICY),
                provider_attested(LEASE_TERMINATE_OBSERVED),
            ],
            Vec::new(),
        );
        request.validate().unwrap();
        let LeaseAdmissionResponse::Admitted { met_by, .. } =
            admit_before_spend(&spec, &request, &"b".repeat(64))
        else {
            panic!("spec-derived guarantees accepted as provider-attested admit the lease");
        };
        assert_eq!(
            met_by[LEASE_CREATE_BOUND],
            RequirementMet {
                name: LEASE_CREATE_BOUND.into(),
                source: ProofSource::ProviderAttested,
                provider: SPEC_EVIDENCE_PROVIDER.into(),
            }
        );

        // A bare name asks for a measured proof, which the provider's word
        // is not; the refusal says so instead of silently accepting it.
        let bare = admission(vec![LEASE_CREATE_BOUND.into()], Vec::new());
        let LeaseAdmissionResponse::Refused { unmet, .. } =
            admit_before_spend(&spec, &bare, &"b".repeat(64))
        else {
            panic!("a measured-only entry cannot be met by provider-attested evidence");
        };
        assert!(unmet[LEASE_CREATE_BOUND].contains("only provider_attested-proven"));

        let mut other_provider = provider_attested(LEASE_CREATE_BOUND);
        if let CapabilityRequirement::Spec(spec_entry) = &mut other_provider {
            spec_entry.providers = vec!["other_cloud".to_owned()];
        }
        let LeaseAdmissionResponse::Refused { unmet, .. } = admit_before_spend(
            &spec,
            &admission(vec![other_provider], Vec::new()),
            &"b".repeat(64),
        ) else {
            panic!("an entry narrowed to another provider is unmet");
        };
        assert!(unmet[LEASE_CREATE_BOUND].contains("accepts only other_cloud"));

        // `any_of` is met by its first accepted alternative.
        let alternatives = CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: None,
            any_of: vec!["export.frozen_candidate".into(), LEASE_CREATE_BOUND.into()],
            accept: vec!["provider_attested".into()],
            providers: Vec::new(),
        });
        let LeaseAdmissionResponse::Admitted { met_by, .. } = admit_before_spend(
            &spec,
            &admission(vec![alternatives.clone()], Vec::new()),
            &"b".repeat(64),
        ) else {
            panic!("an any_of entry with one attested alternative is met");
        };
        assert_eq!(met_by[&alternatives.label()].name, LEASE_CREATE_BOUND);
    }

    #[test]
    fn stage_one_refuses_remote_codex_workload_with_plain_reasons() {
        let spec = pending_channel_spec();
        let mut request = admission(
            vec![
                provider_attested(LEASE_CREATE_BOUND),
                provider_attested(GUEST_READY_AUTHENTICATED),
                provider_attested("export.frozen_candidate"),
            ],
            vec!["guest.exec_route_closed".into()],
        );
        request.maximum_lifetime_seconds = 25 * 60 * 60;
        request.validate().unwrap();
        let LeaseAdmissionResponse::Refused { unmet, reason, .. } =
            admit_before_spend(&spec, &request, &"b".repeat(64))
        else {
            panic!("a remote Codex lease must refuse while no guest channel exists");
        };
        assert_eq!(
            unmet.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "export.frozen_candidate",
                GUEST_READY_AUTHENTICATED,
                LEASE_LIFETIME_BOUND,
            ]
        );
        assert!(reason.contains("declares guest_channel unsupported_pending"));
        assert!(reason.contains("hard lifetime"));
        assert!(reason.starts_with("`codex.authoring.remote` requires"));

        let guest_only = admission(Vec::new(), vec!["guest.exec_route_closed".into()]);
        let LeaseAdmissionResponse::Refused { unmet, reason, .. } =
            admit_before_spend(&spec, &guest_only, &"b".repeat(64))
        else {
            panic!("guest requirements need an authenticated Ready");
        };
        assert_eq!(
            unmet.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![GUEST_READY_AUTHENTICATED]
        );
        assert!(reason.contains("guest.exec_route_closed"));

        // The authored spec relays guest frames through the controller, so
        // it attests an authenticated Ready and stage one admits guest
        // entries for stage two to judge.
        let LeaseAdmissionResponse::Admitted { met_by, .. } =
            admit_before_spend(&signed_spec(), &guest_only, &"b".repeat(64))
        else {
            panic!("the controller-relayed guest channel attests an authenticated Ready");
        };
        assert!(met_by.is_empty());
        let ready_needed = admission(
            vec![provider_attested(GUEST_READY_AUTHENTICATED)],
            vec!["guest.exec_route_closed".into()],
        );
        let LeaseAdmissionResponse::Admitted { met_by, .. } =
            admit_before_spend(&signed_spec(), &ready_needed, &"b".repeat(64))
        else {
            panic!("the controller-relayed guest channel attests an authenticated Ready");
        };
        assert_eq!(
            met_by[GUEST_READY_AUTHENTICATED].source,
            ProofSource::ProviderAttested
        );
    }

    #[test]
    fn stage_one_admits_a_controller_allow_list_only_where_the_spec_declares_it() {
        let spec = signed_spec();
        let controller = BTreeSet::from(["controller.example.com".to_owned()]);
        let mut request = admission(vec![provider_attested(LEASE_NETWORK_POLICY)], Vec::new());
        request.guest_network_allow = controller.clone();
        request.validate().unwrap();
        let LeaseAdmissionResponse::Admitted {
            met_by,
            network_policy,
            network_policy_evidence,
            ..
        } = admit_before_spend(&spec, &request, &"b".repeat(64))
        else {
            panic!("the signed spec declares allow-list creation of exact hostnames");
        };
        assert_eq!(
            met_by[LEASE_NETWORK_POLICY],
            RequirementMet {
                name: LEASE_NETWORK_POLICY.into(),
                source: ProofSource::ProviderAttested,
                provider: SPEC_EVIDENCE_PROVIDER.into(),
            }
        );
        assert_eq!(
            serde_json::to_value(&network_policy).unwrap(),
            serde_json::json!({
                "type": "allow-list",
                "rules": [{"domain": "controller.example.com", "protocol": "https"}]
            })
        );
        assert_eq!(
            network_policy_evidence.source,
            ProofSource::ProviderAttested
        );
        assert_eq!(network_policy_evidence.provider, SPEC_EVIDENCE_PROVIDER);
        let basis = &network_policy_evidence.basis;
        assert!(basis.contains("type allow-list"), "{basis}");
        assert!(basis.contains("[https controller.example.com]"), "{basis}");
        assert!(
            basis.contains("DNS resolution of unlisted names is not filtered"),
            "{basis}"
        );

        // An empty list is deny-all.
        let LeaseAdmissionResponse::Admitted {
            network_policy,
            network_policy_evidence,
            ..
        } = admit_before_spend(
            &spec,
            &admission(vec![provider_attested(LEASE_NETWORK_POLICY)], Vec::new()),
            &"b".repeat(64),
        )
        else {
            panic!("a deny-all lease is admitted");
        };
        assert_eq!(network_policy, EgressPolicy::deny_all());
        assert!(network_policy_evidence.basis.contains("type deny-all"));

        // A spec that declares deny-all only refuses by name, even when the
        // workload does not list `lease.network_policy`; it never creates the
        // lease deny-all instead.
        let deny_only = ProviderSpec::parse(
            &crate::provider_spec::deny_all_only_spec(include_bytes!(
                "../fixtures/provider-spec.json"
            )),
            SETTINGS_SCHEMA_DIGEST,
        )
        .unwrap();
        let mut unlisted = admission(Vec::new(), Vec::new());
        unlisted.guest_network_allow = controller.clone();
        let LeaseAdmissionResponse::Refused { unmet, reason, .. } =
            admit_before_spend(&deny_only, &unlisted, &"b".repeat(64))
        else {
            panic!("a deny-all-only spec cannot create an allow-list lease");
        };
        assert_eq!(
            unmet.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![LEASE_NETWORK_POLICY]
        );
        assert!(
            reason.contains("creatable_types [deny-all] only"),
            "{reason}"
        );
        assert!(reason.contains("controller.example.com"), "{reason}");
        let LeaseAdmissionResponse::Refused { unmet, .. } =
            admit_before_spend(&deny_only, &request, &"b".repeat(64))
        else {
            panic!("a listed network policy entry is unmet by a deny-all-only spec");
        };
        assert!(unmet[LEASE_NETWORK_POLICY].contains("creatable_types [deny-all] only"));

        // A wildcard is well formed but this spec accepts exact names only.
        let mut wildcard = admission(Vec::new(), Vec::new());
        wildcard.guest_network_allow = BTreeSet::from(["*.example.com".to_owned()]);
        wildcard.validate().unwrap();
        let LeaseAdmissionResponse::Refused { reason, .. } =
            admit_before_spend(&spec, &wildcard, &"b".repeat(64))
        else {
            panic!("an exact-name spec refuses a wildcard host");
        };
        assert!(reason.contains("only as exact_dns_name"), "{reason}");

        // More hosts than the spec's rule bound.
        let mut many = admission(Vec::new(), Vec::new());
        many.guest_network_allow = (0..5)
            .map(|index| format!("host{index}.example.com"))
            .collect();
        many.validate().unwrap();
        let LeaseAdmissionResponse::Refused { reason, .. } =
            admit_before_spend(&spec, &many, &"b".repeat(64))
        else {
            panic!("the signed rule bound refuses five hosts");
        };
        assert!(reason.contains("at most 4 rules"), "{reason}");
    }

    #[test]
    fn render_lease_ids_are_sandbox_ids() {
        let spec = signed_spec();
        assert!(spec.valid_lease_id("sbx-x"));
        assert!(!spec.valid_lease_id("lease-1"));
        assert!(!spec.valid_lease_id("sbx-"));
    }
}
