//! Closed interpreter input for the Modal sandbox lease provider.
//!
//! The serialized spec names only fixed facade routes, field projections,
//! operation kinds and code-reviewed proof profile identifiers, plus the
//! digest of the one facade source the adapter will talk to. It cannot
//! introduce a URL, a method, an expression or a capability.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::{
    LifecycleCapability, MAX_LIFECYCLE_PROVIDER_SPEC_BYTES, from_json_slice_strict,
    valid_lease_budget_weight,
};
use ryeos_remote_lease_core::{
    AllowListCapability, EgressHostSyntax, EgressPolicy, EgressProtocol,
    GuestChannelProfile, LEASE_CREATE_BOUND, LEASE_LIFETIME_BOUND, LEASE_NETWORK_POLICY,
    LEASE_TERMINATE_OBSERVED, LeaseProviderFacts, NetworkPolicyCapability, NetworkPolicyType,
};
use serde::Deserialize;

/// Provider id on the evidence derived from the signed spec, so a requirement
/// entry can accept or refuse it by `providers`.
pub const SPEC_EVIDENCE_PROVIDER: &str = "modal_sandbox_v1";
/// The facade response schema this adapter knows how to read.
pub const FACADE_RESPONSE_SCHEMA: &str = "ryeos.modal_lease_facade.response.v1";
/// Modal's hard sandbox lifetime.
pub const PROVIDER_MAXIMUM_LEASE_SECONDS: u32 = 24 * 60 * 60;
/// The facade refuses lifetimes below this.
pub const PROVIDER_MINIMUM_LEASE_SECONDS: u32 = 60;
/// The facade accepts at most this many allow-list rules.
const FACADE_MAX_RULES: u32 = 4;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSpec {
    schema: u32,
    provider_profile: ProviderProfile,
    settings_schema_digest: String,
    origin_profile: OriginProfile,
    /// Node lease-budget seconds one second of a Modal lease costs. Required:
    /// a spec that leaves it out is refused, never given a default.
    budget_weight: u32,
    network_policy: ModalNetworkPolicy,
    facade: FacadePin,
    operations: Operations,
    #[serde(skip, default = "deny_all_only")]
    core_network_policy: NetworkPolicyCapability,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProviderProfile {
    ModalSandboxV1,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OriginProfile {
    ModalLeaseFacadeV1,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum ModalNetworkType {
    #[serde(rename = "block-network")]
    BlockNetwork,
    #[serde(rename = "domain-allow-list")]
    DomainAllowList,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModalNetworkPolicy {
    creatable_types: Vec<ModalNetworkType>,
    #[serde(default, deserialize_with = "non_null_option")]
    allow_list: Option<AllowListCapability>,
}

/// The one facade source the adapter is pinned to.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FacadePin {
    pub path: String,
    pub sha256: String,
    pub app: String,
    pub lease_app: String,
    pub endpoint_label: String,
    pub response_schema: String,
    pub policy: FacadePolicyInputs,
}

/// The facade's fixed lease policy inputs, as the facade source defines
/// them. The adapter recomputes the facade's policy digest from these, so a
/// lease whose stored digest differs was not created under this policy.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FacadePolicyInputs {
    pub schema: String,
    pub cpu: f64,
    pub memory_mib: u32,
    pub image: String,
    pub command: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OperationKind {
    CreateOnce,
    ObserveExactOccurrence,
    UploadThenRunOncePending,
    TerminateOnceThenObserveExact,
    ControllerRelayedGuestFrames,
    UnsupportedPending,
}

/// A fixed facade route. The path and the selector are code, not data.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteName {
    Create,
    GetByName,
    GetById,
    Terminate,
    Upload,
    RunOnce,
}

impl RouteName {
    pub fn path(self) -> &'static str {
        match self {
            Self::Create => "/v1/create",
            Self::GetByName | Self::GetById => "/v1/get",
            Self::Terminate => "/v1/terminate",
            Self::Upload => "/v1/upload",
            Self::RunOnce => "/v1/run-once",
        }
    }

    /// The facade `operation` an envelope from this route must name.
    pub fn operation(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::GetByName | Self::GetById => "get",
            Self::Terminate => "terminate",
            Self::Upload => "upload",
            Self::RunOnce => "run_once",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProofProfile {
    ModalFacadeCreateV1,
    ModalNoRequestSentV1,
    ModalFacadeObservedByNameV1,
    ModalFacadeTerminalV1,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum LifetimeSource {
    #[serde(rename = "reservation.maximum_lifetime_seconds")]
    ReservationMaximumLifetimeSeconds,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum NetworkSource {
    #[serde(rename = "lease.guest_network_allow")]
    LeaseGuestNetworkAllow,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LifetimeField {
    source: LifetimeSource,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct NetworkField {
    source: NetworkSource,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    timeout_seconds: LifetimeField,
    network: NetworkField,
}

fn create_body() -> CreateBody {
    CreateBody {
        timeout_seconds: LifetimeField {
            source: LifetimeSource::ReservationMaximumLifetimeSeconds,
        },
        network: NetworkField {
            source: NetworkSource::LeaseGuestNetworkAllow,
        },
    }
}

/// One operation declaration. Every field beyond `kind` is optional and an
/// operation is accepted only when it equals one of the exact declarations
/// the adapter implements.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OperationDecl {
    kind: OperationKind,
    #[serde(default, deserialize_with = "non_null_option")]
    route: Option<RouteName>,
    #[serde(default, deserialize_with = "non_null_option")]
    mutation_route: Option<RouteName>,
    #[serde(default, deserialize_with = "non_null_option")]
    observation_route: Option<RouteName>,
    #[serde(default, deserialize_with = "non_null_option")]
    upload_route: Option<RouteName>,
    #[serde(default, deserialize_with = "non_null_option")]
    run_route: Option<RouteName>,
    #[serde(default, deserialize_with = "non_null_option")]
    body: Option<CreateBody>,
    #[serde(default, deserialize_with = "non_null_option")]
    bind_proof_profile: Option<ProofProfile>,
    #[serde(default, deserialize_with = "non_null_option")]
    no_occurrence_proof_profile: Option<ProofProfile>,
    #[serde(default, deserialize_with = "non_null_option")]
    terminal_proof_profile: Option<ProofProfile>,
}

impl OperationDecl {
    fn bare(kind: OperationKind) -> Self {
        Self {
            kind,
            route: None,
            mutation_route: None,
            observation_route: None,
            upload_route: None,
            run_route: None,
            body: None,
            bind_proof_profile: None,
            no_occurrence_proof_profile: None,
            terminal_proof_profile: None,
        }
    }

    fn unsupported() -> Self {
        Self::bare(OperationKind::UnsupportedPending)
    }

    fn allocate() -> Self {
        Self {
            route: Some(RouteName::Create),
            body: Some(create_body()),
            bind_proof_profile: Some(ProofProfile::ModalFacadeCreateV1),
            no_occurrence_proof_profile: Some(ProofProfile::ModalNoRequestSentV1),
            ..Self::bare(OperationKind::CreateOnce)
        }
    }

    fn reconcile_allocation() -> Self {
        Self {
            route: Some(RouteName::GetByName),
            bind_proof_profile: Some(ProofProfile::ModalFacadeObservedByNameV1),
            ..Self::bare(OperationKind::ObserveExactOccurrence)
        }
    }

    fn terminate() -> Self {
        Self {
            mutation_route: Some(RouteName::Terminate),
            observation_route: Some(RouteName::GetById),
            terminal_proof_profile: Some(ProofProfile::ModalFacadeTerminalV1),
            ..Self::bare(OperationKind::TerminateOnceThenObserveExact)
        }
    }

    fn reconcile_termination() -> Self {
        Self {
            route: Some(RouteName::GetById),
            terminal_proof_profile: Some(ProofProfile::ModalFacadeTerminalV1),
            ..Self::bare(OperationKind::ObserveExactOccurrence)
        }
    }

    fn is_unsupported(&self) -> bool {
        *self == Self::unsupported()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operations {
    allocate: OperationDecl,
    reconcile_allocation: OperationDecl,
    activate_supervisor: OperationDecl,
    reconcile_supervisor_activation: OperationDecl,
    relay_file: OperationDecl,
    snapshot: OperationDecl,
    guest_channel: OperationDecl,
    terminate: OperationDecl,
    reconcile_termination: OperationDecl,
}

fn deny_all_only() -> NetworkPolicyCapability {
    NetworkPolicyCapability {
        creatable_types: vec![NetworkPolicyType::DenyAll],
        allow_list: None,
    }
}

fn non_null_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl ProviderSpec {
    pub fn parse(bytes: &[u8], settings_schema_digest: &str) -> Result<Self> {
        let maximum = usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES).unwrap_or(usize::MAX);
        ensure!(
            !bytes.is_empty() && bytes.len() <= maximum,
            "provider spec exceeds its signed byte bound"
        );
        let mut spec: Self = from_json_slice_strict(bytes, maximum)?;
        spec.validate(settings_schema_digest)?;
        Ok(spec)
    }

    fn validate(&mut self, settings_schema_digest: &str) -> Result<()> {
        ensure!(
            self.schema == 1
                && self.provider_profile == ProviderProfile::ModalSandboxV1
                && self.origin_profile == OriginProfile::ModalLeaseFacadeV1
                && self.settings_schema_digest == settings_schema_digest,
            "provider spec identity or profile is unsupported"
        );
        ensure!(
            valid_lease_budget_weight(self.budget_weight),
            "provider spec budget_weight is outside the contract's bound"
        );
        self.validate_facade()?;
        self.core_network_policy = self.network_policy.to_core()?;
        self.validate_operations()
    }

    fn validate_facade(&self) -> Result<()> {
        let facade = &self.facade;
        ensure!(
            lillux::valid_hash(&facade.sha256)
                && !facade.sha256.bytes().any(|byte| byte.is_ascii_uppercase())
                && valid_facade_name(&facade.app)
                && valid_facade_name(&facade.lease_app)
                && valid_facade_name(&facade.endpoint_label)
                && facade.response_schema == FACADE_RESPONSE_SCHEMA
                && facade.policy.schema == "ryeos.modal_lease_policy.v1"
                && facade.policy.cpu.is_finite()
                && facade.policy.cpu > 0.0
                && facade.policy.memory_mib > 0
                && !facade.policy.image.is_empty()
                && !facade.policy.command.is_empty()
                && !facade.path.is_empty()
                && facade.path.len() <= 256
                && !facade.path.starts_with('/')
                && facade.path.split('/').all(|part| {
                    !part.is_empty()
                        && part != "."
                        && part != ".."
                        && part.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                        })
                }),
            "provider spec facade pin is invalid"
        );
        Ok(())
    }

    fn validate_operations(&self) -> Result<()> {
        let operations = &self.operations;
        ensure!(
            operations.allocate == OperationDecl::allocate(),
            "provider spec allocate operation is unsupported"
        );
        ensure!(
            operations.reconcile_allocation == OperationDecl::reconcile_allocation()
                || operations.reconcile_allocation.is_unsupported(),
            "provider spec allocation reconciliation is unsupported"
        );
        ensure!(
            operations.reconcile_supervisor_activation.is_unsupported()
                && operations.relay_file.is_unsupported()
                && operations.snapshot.is_unsupported(),
            "provider spec declares an operation this adapter does not implement"
        );
        // Guest activation and the guest channel stay unsupported until
        // activation exists, so Ready is never attested.
        ensure!(
            operations.activate_supervisor.is_unsupported()
                && operations.guest_channel.is_unsupported(),
            "provider spec declares an activation or guest channel this adapter does not implement"
        );
        ensure!(
            operations.terminate == OperationDecl::terminate()
                && operations.reconcile_termination == OperationDecl::reconcile_termination(),
            "provider spec termination operations are unsupported"
        );
        Ok(())
    }

    pub fn facade(&self) -> &FacadePin {
        &self.facade
    }

    pub fn reconciles_allocation(&self) -> bool {
        self.operations.reconcile_allocation == OperationDecl::reconcile_allocation()
    }

    /// The exact policy a lease create requests for the admitted guest egress
    /// hosts. Workload: a remote lease whose guest may reach only its
    /// controller.
    pub fn egress_policy(
        &self,
        lease_egress: &std::collections::BTreeSet<String>,
    ) -> Result<EgressPolicy> {
        self.core_network_policy
            .egress_policy(SPEC_EVIDENCE_PROVIDER, lease_egress)
    }

    /// Whether the exact observed-terminal and reconcile-by-name profiles are
    /// declared, deriving the lifecycle capabilities the adapter may claim.
    pub fn effective_capabilities(&self) -> BTreeSet<LifecycleCapability> {
        let mut capabilities = BTreeSet::new();
        if self.operations.allocate.no_occurrence_proof_profile
            == Some(ProofProfile::ModalNoRequestSentV1)
        {
            capabilities.insert(LifecycleCapability::AuthoritativeNoOccurrence);
        }
        if self.reconciles_allocation() {
            capabilities.insert(LifecycleCapability::ExactAllocationReconciliation);
        }
        if self.operations.terminate == OperationDecl::terminate()
            && self.operations.reconcile_termination == OperationDecl::reconcile_termination()
        {
            capabilities.insert(LifecycleCapability::ExactTerminalObservation);
        }
        capabilities
    }
}

fn valid_facade_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

impl ModalNetworkPolicy {
    fn to_core(&self) -> Result<NetworkPolicyCapability> {
        let capability = match (self.creatable_types.as_slice(), &self.allow_list) {
            ([ModalNetworkType::BlockNetwork], None) => deny_all_only(),
            (
                [
                    ModalNetworkType::BlockNetwork,
                    ModalNetworkType::DomainAllowList,
                ],
                Some(allow_list),
            ) => {
                ensure!(
                    allow_list.rule_protocol == EgressProtocol::Https
                        && allow_list.host_syntax == EgressHostSyntax::ExactDnsName
                        && (1..=FACADE_MAX_RULES).contains(&allow_list.max_rules),
                    "provider spec allow-list must be https, exact DNS names, at most \
                     {FACADE_MAX_RULES} rules"
                );
                NetworkPolicyCapability {
                    creatable_types: vec![NetworkPolicyType::DenyAll, NetworkPolicyType::AllowList],
                    allow_list: Some(AllowListCapability {
                        rule_protocol: allow_list.rule_protocol,
                        max_rules: allow_list.max_rules,
                        host_syntax: allow_list.host_syntax,
                    }),
                }
            }
            _ => anyhow::bail!(
                "provider spec network policy declaration is unsupported: creatable_types must \
                 be [block-network] alone, or [block-network, domain-allow-list] with allow_list \
                 bounds"
            ),
        };
        capability.validate().context("provider spec network policy")?;
        Ok(capability)
    }
}

impl LeaseProviderFacts for ProviderSpec {
    fn evidence_provider(&self) -> &str {
        SPEC_EVIDENCE_PROVIDER
    }

    fn maximum_lease_seconds(&self) -> u32 {
        PROVIDER_MAXIMUM_LEASE_SECONDS
    }

    fn budget_weight(&self) -> u32 {
        self.budget_weight
    }

    fn attested_lease_guarantees(&self) -> BTreeSet<&'static str> {
        let mut guarantees = BTreeSet::new();
        let allocate = &self.operations.allocate;
        if allocate.kind == OperationKind::CreateOnce
            && allocate.bind_proof_profile == Some(ProofProfile::ModalFacadeCreateV1)
        {
            guarantees.insert(LEASE_CREATE_BOUND);
        }
        if let Some(body) = &allocate.body {
            if body.timeout_seconds.source == LifetimeSource::ReservationMaximumLifetimeSeconds {
                guarantees.insert(LEASE_LIFETIME_BOUND);
            }
            if body.network.source == NetworkSource::LeaseGuestNetworkAllow {
                guarantees.insert(LEASE_NETWORK_POLICY);
            }
        }
        if self
            .effective_capabilities()
            .contains(&LifecycleCapability::ExactTerminalObservation)
        {
            guarantees.insert(LEASE_TERMINATE_OBSERVED);
        }
        guarantees
    }

    fn guest_channel_profile(&self) -> Result<GuestChannelProfile> {
        match self.operations.guest_channel.kind {
            OperationKind::UnsupportedPending => Ok(GuestChannelProfile::UnsupportedPending),
            OperationKind::ControllerRelayedGuestFrames => {
                Ok(GuestChannelProfile::ControllerRelayedFrames)
            }
            kind => anyhow::bail!(
                "provider spec guest channel kind {kind:?} is not an implemented guest transport"
            ),
        }
    }

    fn guest_activation_declared(&self) -> bool {
        false
    }

    fn network_policy_capability(&self) -> &NetworkPolicyCapability {
        &self.core_network_policy
    }

    fn egress_evidence_basis(&self, policy: &EgressPolicy) -> String {
        match policy.policy_type {
            NetworkPolicyType::DenyAll => {
                "the signed provider spec creates this lease through the pinned facade with network \
                 type block-network, and binds the lease only when the facade echoes a policy \
                 record with block_network true and an empty allow list under a matching policy \
                 digest; provider-attested through the facade, not independently observed"
                    .to_owned()
            }
            NetworkPolicyType::AllowList => format!(
                "the signed provider spec creates this lease through the pinned facade with a \
                 domain allow list of exactly the rules [{}], and binds the lease only when the \
                 facade echoes exactly those hosts under a matching policy digest; provider \
                 attested through the facade: egress is TLS on port 443 only, matched by SNI; \
                 residual: DNS resolution of unlisted names is not filtered, so data can leave in \
                 DNS queries",
                policy.rules_text()
            ),
        }
    }

    fn valid_lease_id(&self, id: &str) -> bool {
        valid_sandbox_id(id)
    }
}

/// The facade's sandbox id form: `sb-` followed by 1 to 64 alphanumerics.
pub fn valid_sandbox_id(id: &str) -> bool {
    id.strip_prefix("sb-").is_some_and(|rest| {
        (1..=64).contains(&rest.len()) && rest.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const FIXTURE: &[u8] = include_bytes!("../fixtures/provider-spec.json");
    pub const SCHEMA: &[u8] = include_bytes!("../fixtures/settings.schema.json");

    fn schema_digest() -> String {
        lillux::sha256_hex(SCHEMA)
    }

    fn fixture_value() -> serde_json::Value {
        serde_json::from_slice(FIXTURE).unwrap()
    }

    fn parse_value(value: &serde_json::Value) -> Result<ProviderSpec> {
        ProviderSpec::parse(&serde_json::to_vec(value).unwrap(), &schema_digest())
    }

    #[test]
    fn fixture_parses_and_pins_the_signed_facade() {
        let spec = ProviderSpec::parse(FIXTURE, &schema_digest()).unwrap();
        assert_eq!(
            spec.facade().sha256,
            "a9ebf7940e72bdfad4264c5075df04689681998ab1b0994d9588cc451fcb67ba"
        );
        assert_eq!(spec.facade().app, "ryeos-lease-facade");
        assert_eq!(spec.facade().lease_app, "ryeos-lease-sandboxes");
        assert_eq!(spec.budget_weight(), 1);
        assert!(spec.reconciles_allocation());
        let guarantees = spec.attested_lease_guarantees();
        for name in [
            LEASE_CREATE_BOUND,
            LEASE_LIFETIME_BOUND,
            LEASE_NETWORK_POLICY,
            LEASE_TERMINATE_OBSERVED,
        ] {
            assert!(guarantees.contains(name), "{name}");
        }
        // Activation is not designed yet, so Ready is not attested.
        assert!(!guarantees.contains(ryeos_remote_lease_core::GUEST_READY_AUTHENTICATED));
        assert!(!spec.guest_activation_declared());
        assert_eq!(
            spec.guest_channel_profile().unwrap(),
            GuestChannelProfile::UnsupportedPending
        );
        assert_eq!(spec.effective_capabilities().len(), 3);
    }

    #[test]
    fn bundle_copy_is_byte_identical_to_the_fixture() {
        let bundle = include_bytes!("../../../../bundles/modal-sandbox/specs/provider-spec.json");
        assert_eq!(bundle.as_slice(), FIXTURE);
    }

    #[test]
    fn facade_path_names_the_checked_in_facade_file() {
        let spec = ProviderSpec::parse(FIXTURE, &schema_digest()).unwrap();
        let facade = include_bytes!("../../../../bundles/modal-sandbox/facade/ryeos_lease_facade.py");
        assert_eq!(spec.facade().path, "facade/ryeos_lease_facade.py");
        assert_eq!(lillux::sha256_hex(facade), spec.facade().sha256);
    }

    #[test]
    fn budget_weight_is_required_and_bounded() {
        let mut value = fixture_value();
        value.as_object_mut().unwrap().remove("budget_weight");
        assert!(parse_value(&value).is_err());
        for weight in [0u64, 1001, 4_000_000_000] {
            let mut value = fixture_value();
            value["budget_weight"] = weight.into();
            assert!(parse_value(&value).is_err(), "{weight}");
        }
        for weight in [1u64, 1000] {
            let mut value = fixture_value();
            value["budget_weight"] = weight.into();
            assert_eq!(parse_value(&value).unwrap().budget_weight(), weight as u32);
        }
    }

    #[test]
    fn unknown_fields_and_identity_changes_are_refused() {
        let mut value = fixture_value();
        value["extra"] = 1.into();
        assert!(parse_value(&value).is_err());
        let mut value = fixture_value();
        value["facade"]["extra"] = 1.into();
        assert!(parse_value(&value).is_err());
        let mut value = fixture_value();
        value["operations"]["allocate"]["extra"] = 1.into();
        assert!(parse_value(&value).is_err());
        let mut value = fixture_value();
        value["settings_schema_digest"] = "0".repeat(64).into();
        assert!(parse_value(&value).is_err());
        let mut value = fixture_value();
        value["schema"] = 2.into();
        assert!(parse_value(&value).is_err());
    }

    #[test]
    fn facade_pin_is_validated() {
        for (field, bad) in [
            ("sha256", "abc"),
            ("sha256", "A9EBF7940E72BDFAD4264C5075DF04689681998AB1B0994D9588CC451FCB67BA"),
            ("app", "Upper"),
            ("lease_app", ""),
            ("endpoint_label", "a b"),
            ("response_schema", "ryeos.modal_lease_facade.response.v2"),
            ("path", "/abs/path.py"),
            ("path", "../escape.py"),
        ] {
            let mut value = fixture_value();
            value["facade"][field] = bad.into();
            assert!(parse_value(&value).is_err(), "{field}={bad}");
        }
    }

    #[test]
    fn facade_policy_inputs_are_pinned_and_validated() {
        let spec = ProviderSpec::parse(FIXTURE, &schema_digest()).unwrap();
        let inputs = &spec.facade().policy;
        assert_eq!(inputs.memory_mib, 1024);
        assert_eq!(inputs.command.len(), 3);
        assert_eq!(
            inputs.image,
            "modal.debian_slim(python_version=3.12)+mkdir -p /ryeos/guest"
        );
        let mut value = fixture_value();
        value["facade"].as_object_mut().unwrap().remove("policy");
        assert!(parse_value(&value).is_err());
        for (field, bad) in [
            ("cpu", serde_json::json!(0.0)),
            ("memory_mib", serde_json::json!(0)),
            ("image", serde_json::json!("")),
            ("command", serde_json::json!([])),
            ("schema", serde_json::json!("ryeos.modal_lease_policy.v2")),
            ("extra", serde_json::json!(1)),
        ] {
            let mut value = fixture_value();
            value["facade"]["policy"][field] = bad;
            assert!(parse_value(&value).is_err(), "{field}");
        }
    }

    #[test]
    fn network_policy_declaration_is_closed() {
        for policy in [
            serde_json::json!({"creatable_types": ["domain-allow-list"]}),
            serde_json::json!({"creatable_types": ["block-network", "domain-allow-list"]}),
            serde_json::json!({"creatable_types": ["block-network"], "allow_list": {
                "rule_protocol": "https", "max_rules": 4, "host_syntax": "exact_dns_name"}}),
            serde_json::json!({"creatable_types": ["block-network", "domain-allow-list"],
                "allow_list": {"rule_protocol": "https", "max_rules": 5,
                "host_syntax": "exact_dns_name"}}),
            serde_json::json!({"creatable_types": ["block-network", "domain-allow-list"],
                "allow_list": {"rule_protocol": "https", "max_rules": 4,
                "host_syntax": "exact_dns_name_or_leftmost_wildcard"}}),
            serde_json::json!({"creatable_types": ["block-network", "allow-all"]}),
        ] {
            let mut value = fixture_value();
            value["network_policy"] = policy.clone();
            assert!(parse_value(&value).is_err(), "{policy}");
        }
        let mut value = fixture_value();
        value["network_policy"] = serde_json::json!({"creatable_types": ["block-network"]});
        let spec = parse_value(&value).unwrap();
        let hosts: BTreeSet<String> = ["controller.example.com".to_owned()].into();
        assert!(spec.egress_policy(&hosts).is_err());
        assert_eq!(
            spec.egress_policy(&BTreeSet::new()).unwrap(),
            EgressPolicy::deny_all()
        );
    }

    #[test]
    fn allow_list_policy_derives_exact_https_rules() {
        let spec = ProviderSpec::parse(FIXTURE, &schema_digest()).unwrap();
        let hosts: BTreeSet<String> =
            ["b.example.com".to_owned(), "a.example.com".to_owned()].into();
        let policy = spec.egress_policy(&hosts).unwrap();
        assert_eq!(policy.policy_type, NetworkPolicyType::AllowList);
        assert_eq!(policy.rules[0].domain, "a.example.com");
        let five: BTreeSet<String> = (0..5).map(|i| format!("h{i}.example.com")).collect();
        assert!(spec.egress_policy(&five).is_err());
        let wildcard: BTreeSet<String> = ["*.example.com".to_owned()].into();
        assert!(spec.egress_policy(&wildcard).is_err());
    }

    #[test]
    fn operations_must_equal_an_implemented_declaration() {
        for (operation, patch) in [
            ("allocate", serde_json::json!({"kind": "create_once", "route": "get_by_id"})),
            ("terminate", serde_json::json!({"kind": "observe_exact_occurrence"})),
            ("snapshot", serde_json::json!({"kind": "create_once"})),
            ("relay_file", serde_json::json!({"kind": "controller_relayed_guest_frames"})),
            ("reconcile_termination", serde_json::json!({"kind": "unsupported_pending"})),
        ] {
            let mut value = fixture_value();
            value["operations"][operation] = patch;
            assert!(parse_value(&value).is_err(), "{operation}");
        }
        // Reconciling by name and the channel may be switched off.
        let mut value = fixture_value();
        value["operations"]["reconcile_allocation"] = serde_json::json!({"kind": "unsupported_pending"});
        let spec = parse_value(&value).unwrap();
        assert!(!spec.reconciles_allocation());
        assert!(
            !spec
                .effective_capabilities()
                .contains(&LifecycleCapability::ExactAllocationReconciliation)
        );
        // Activation and the guest channel are refused in any other form.
        for (operation, patch) in [
            (
                "activate_supervisor",
                serde_json::json!({
                    "kind": "upload_then_run_once_pending",
                    "upload_route": "upload",
                    "run_route": "run_once"
                }),
            ),
            (
                "guest_channel",
                serde_json::json!({"kind": "controller_relayed_guest_frames"}),
            ),
        ] {
            let mut value = fixture_value();
            value["operations"][operation] = patch;
            assert!(parse_value(&value).is_err(), "{operation}");
        }
    }

    #[test]
    fn explicit_null_operation_fields_are_refused() {
        let mut value = fixture_value();
        value["operations"]["terminate"]["route"] = serde_json::Value::Null;
        assert!(parse_value(&value).is_err());
    }

    #[test]
    fn sandbox_id_form_matches_the_facade() {
        assert!(valid_sandbox_id("sb-abc123"));
        assert!(valid_sandbox_id(&format!("sb-{}", "a".repeat(64))));
        for bad in ["", "sb-", "sb-a_b", "sbx-abc", "sb-é", &format!("sb-{}", "a".repeat(65))] {
            assert!(!valid_sandbox_id(bad), "{bad:?}");
        }
    }
}
