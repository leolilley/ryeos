//! Closed interpreter input for the Render reference lifecycle provider.
//!
//! The serialized format contains only fixed route names, field projections,
//! operation kinds, and code-reviewed proof profile identifiers. It cannot
//! introduce a URL, HTTP method, expression, script, status code, or capability.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::{
    LifecycleCapability, MAX_LEASE_BUDGET_WEIGHT, MAX_LIFECYCLE_PROVIDER_SPEC_BYTES,
    from_json_slice_strict, valid_lease_budget_weight,
};
pub(crate) use ryeos_remote_lease_core::{
    EgressPolicy, EgressRule, GUEST_READY_AUTHENTICATED, GuestChannelProfile, LEASE_CREATE_BOUND,
    LEASE_LIFETIME_BOUND, LEASE_NETWORK_POLICY, LEASE_TERMINATE_OBSERVED, NetworkPolicyCapability,
    NetworkPolicyType,
};
#[cfg(test)]
pub(crate) use ryeos_remote_lease_core::EgressProtocol;

/// Provider id on the evidence this adapter derives from the signed spec, so
/// a requirement entry can accept or refuse it by `providers`. Refusals name
/// the spec by it.
pub(crate) const SPEC_EVIDENCE_PROVIDER: &str = "render_sandbox_v1";

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderSpec {
    schema: u32,
    provider_profile: ProviderProfile,
    settings_schema_digest: String,
    bootstrap_settings_schema_digest: String,
    qualification_settings_schema_digest: String,
    origin_profile: OriginProfile,
    /// How many node lease-budget seconds one second of a Render lease
    /// costs. Required: a spec that leaves it out is refused, never given a
    /// default.
    budget_weight: u32,
    network_policy: NetworkPolicyCapability,
    routes: Routes,
    operations: Operations,
    #[serde(skip)]
    bootstrap_profile_digest: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProviderProfile {
    RenderSandboxV1,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OriginProfile {
    RenderApiV1,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RouteName {
    SandboxCollection,
    SandboxById,
    SandboxTerminateById,
    SandboxFileUploadTokenById,
    SandboxFileDownloadTokenById,
    SandboxRunStreamTokenById,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum RouteLiteral {
    #[serde(rename = "v1")]
    V1,
    #[serde(rename = "sandboxes")]
    Sandboxes,
    #[serde(rename = "terminate")]
    Terminate,
    #[serde(rename = "files")]
    Files,
    #[serde(rename = "upload")]
    Upload,
    #[serde(rename = "download")]
    Download,
    #[serde(rename = "runs")]
    Runs,
    #[serde(rename = "stream")]
    Stream,
    #[serde(rename = "token")]
    Token,
}

impl RouteLiteral {
    fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::Sandboxes => "sandboxes",
            Self::Terminate => "terminate",
            Self::Files => "files",
            Self::Upload => "upload",
            Self::Download => "download",
            Self::Runs => "runs",
            Self::Stream => "stream",
            Self::Token => "token",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum RouteBinding {
    #[serde(rename = "occurrence_id")]
    OccurrenceId,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RouteSegment {
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    literal: Option<RouteLiteral>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    bound: Option<RouteBinding>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RouteSpec {
    segments: Vec<RouteSegment>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    query: Option<RouteQuery>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RouteQuery {
    #[serde(rename = "ownerId")]
    owner_id: StringSource,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    path: Option<FilePathSource>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum FilePathFieldSource {
    #[serde(rename = "operation.file_path")]
    OperationFilePath,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FilePathSource {
    source: FilePathFieldSource,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StringFieldSource {
    #[serde(rename = "settings.owner_id")]
    SettingsOwnerId,
    #[serde(rename = "settings.region")]
    SettingsRegion,
    #[serde(rename = "settings.snapshot_id")]
    SettingsSnapshotId,
    #[serde(rename = "qualification.locator.snapshot_id")]
    QualificationLocatorSnapshotId,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct StringSource {
    source: StringFieldSource,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum PlanFieldSource {
    #[serde(rename = "settings.plan")]
    SettingsPlan,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PlanSource {
    source: PlanFieldSource,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum LifetimeFieldSource {
    #[serde(rename = "reservation.maximum_lifetime_seconds")]
    ReservationMaximumLifetimeSeconds,
    #[serde(rename = "qualification.maximum_lifetime_seconds")]
    QualificationMaximumLifetimeSeconds,
    #[serde(rename = "bootstrap.maximum_lifetime_seconds")]
    BootstrapMaximumLifetimeSeconds,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LifetimeSource {
    source: LifetimeFieldSource,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct NetworkPolicyTypeLiteral {
    #[serde(rename = "type")]
    policy_type: NetworkPolicyType,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum EgressFieldSource {
    #[serde(rename = "lease.guest_network_allow")]
    LeaseGuestNetworkAllow,
}

/// A create body's `networkPolicy`: a fixed type, or the admitted lease's
/// egress list. The expected mapping of each create operation is compared
/// exactly, so a spec cannot carry both or neither.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct NetworkPolicyMapping {
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    literal: Option<NetworkPolicyTypeLiteral>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    source: Option<EgressFieldSource>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CreateBodyMapping {
    #[serde(rename = "ownerId")]
    owner_id: StringSource,
    plan: PlanSource,
    region: StringSource,
    #[serde(rename = "timeoutSeconds")]
    timeout_seconds: LifetimeSource,
    #[serde(rename = "networkPolicy")]
    network_policy: NetworkPolicyMapping,
    #[serde(
        rename = "snapshotId",
        default,
        deserialize_with = "deserialize_non_null_option"
    )]
    snapshot_id: Option<StringSource>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PreconditionProfile {
    RenderConfiguredRuntimeSnapshotV1,
    RenderQualifiedGuestRuntimeV1,
    RenderBoundRuntimeSnapshotV1,
    RenderBoundQualificationOccurrenceV1,
    RenderMaterializationBootstrapV1,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum BindProofProfile {
    #[serde(rename = "render_sandbox_create_201_v1")]
    RenderSandboxCreate201V1,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum NoOccurrenceProofProfile {
    #[serde(rename = "render_no_request_sent_v1")]
    RenderNoRequestSentV1,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum TerminalProofProfile {
    #[serde(rename = "render_sandbox_terminal_v1")]
    RenderSandboxTerminalV1,
    #[serde(rename = "render_sandbox_terminated_list_v1")]
    RenderSandboxTerminatedListV1,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OperationKind {
    CreateOnce,
    UploadThenRunOncePending,
    UploadThenRunOnce,
    JournalClaimedFileRelay,
    UnsupportedPending,
    TerminateOnceThenObserveExact,
    ObserveExactOccurrence,
    ControllerRelayedGuestFrames,
}

/// Every possible field is explicitly typed; `validate` enforces which fields
/// each operation kind may carry. Unknown serialized fields are rejected.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperationSpec {
    kind: OperationKind,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    route: Option<RouteName>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    precondition_profile: Option<PreconditionProfile>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    body: Option<CreateBodyMapping>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    bind_proof_profile: Option<BindProofProfile>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    no_occurrence_proof_profile: Option<NoOccurrenceProofProfile>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    mutation_route: Option<RouteName>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    observation_route: Option<RouteName>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    terminal_proof_profile: Option<TerminalProofProfile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Operations {
    pub(crate) allocate: OperationSpec,
    bootstrap_create: OperationSpec,
    qualification_create: OperationSpec,
    qualification_verify: OperationSpec,
    reconcile_allocation: OperationSpec,
    activate_supervisor: OperationSpec,
    reconcile_supervisor_activation: OperationSpec,
    relay_file: OperationSpec,
    guest_channel: OperationSpec,
    pub(crate) terminate: OperationSpec,
    pub(crate) reconcile_termination: OperationSpec,
    bootstrap_terminated_list: OperationSpec,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Routes {
    sandbox_collection: RouteSpec,
    sandbox_by_id: RouteSpec,
    sandbox_terminate_by_id: RouteSpec,
    sandbox_file_upload_token_by_id: RouteSpec,
    sandbox_file_download_token_by_id: RouteSpec,
    sandbox_run_stream_token_by_id: RouteSpec,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RouteTarget {
    pub(crate) path_segments: Vec<String>,
    pub(crate) owner_id_query: Option<String>,
    pub(crate) file_path_query: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlanValue {
    Starter,
    Standard,
    Pro,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CreateProjection {
    pub(crate) owner_id: String,
    pub(crate) plan: PlanValue,
    pub(crate) region: String,
    pub(crate) timeout_seconds: u32,
    pub(crate) network_policy: EgressPolicy,
    pub(crate) snapshot_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BootstrapCreateProjection {
    pub(crate) owner_id: String,
    pub(crate) plan: PlanValue,
    pub(crate) region: String,
    pub(crate) timeout_seconds: u32,
    pub(crate) network_policy: EgressPolicy,
}

impl ProviderSpec {
    pub(crate) fn parse(bytes: &[u8], settings_schema_digest: &str) -> Result<Self> {
        ensure!(
            !bytes.is_empty()
                && bytes.len()
                    <= usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES).unwrap_or(usize::MAX),
            "provider spec exceeds its signed byte bound"
        );
        let mut spec: Self = from_json_slice_strict(
            bytes,
            usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES).unwrap_or(usize::MAX),
        )?;
        spec.validate(settings_schema_digest)?;
        let value: serde_json::Value = from_json_slice_strict(
            bytes,
            usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES).unwrap_or(usize::MAX),
        )?;
        let profile = value
            .get("operations")
            .and_then(|value| value.get("bootstrap_create"))
            .context("signed provider spec lacks bootstrap profile")?;
        spec.bootstrap_profile_digest = lillux::sha256_hex(
            ryeos_external_execution_contract::canonical_json(&(
                "ryeos.render-bootstrap-profile.v1",
                profile,
            ))?
            .as_slice(),
        );
        Ok(spec)
    }

    fn validate(&self, settings_schema_digest: &str) -> Result<()> {
        ensure!(
            self.schema == 8
                && self.provider_profile == ProviderProfile::RenderSandboxV1
                && self.origin_profile == OriginProfile::RenderApiV1
                && self.settings_schema_digest == settings_schema_digest
                && self.bootstrap_settings_schema_digest
                    == lillux::sha256_hex(include_bytes!(
                        "../fixtures/bootstrap-settings.schema.json"
                    ))
                && self.qualification_settings_schema_digest
                    == lillux::sha256_hex(include_bytes!(
                        "../fixtures/qualification-settings.schema.json"
                    )),
            "provider spec identity or profile is unsupported"
        );
        ensure!(
            valid_lease_budget_weight(self.budget_weight),
            "provider spec budget_weight {} is outside 1..={MAX_LEASE_BUDGET_WEIGHT}",
            self.budget_weight
        );
        ensure!(
            route_matches(
                &self.routes.sandbox_collection,
                &[
                    RouteSegmentExpectation::Literal(RouteLiteral::V1),
                    RouteSegmentExpectation::Literal(RouteLiteral::Sandboxes)
                ],
                ExpectedRouteQuery::None,
            ) && route_matches(
                &self.routes.sandbox_by_id,
                &[
                    RouteSegmentExpectation::Literal(RouteLiteral::V1),
                    RouteSegmentExpectation::Literal(RouteLiteral::Sandboxes),
                    RouteSegmentExpectation::Bound(RouteBinding::OccurrenceId)
                ],
                ExpectedRouteQuery::Owner,
            ) && route_matches(
                &self.routes.sandbox_terminate_by_id,
                &[
                    RouteSegmentExpectation::Literal(RouteLiteral::V1),
                    RouteSegmentExpectation::Literal(RouteLiteral::Sandboxes),
                    RouteSegmentExpectation::Bound(RouteBinding::OccurrenceId),
                    RouteSegmentExpectation::Literal(RouteLiteral::Terminate)
                ],
                ExpectedRouteQuery::Owner,
            ) && route_matches(
                &self.routes.sandbox_file_upload_token_by_id,
                &[
                    RouteSegmentExpectation::Literal(RouteLiteral::V1),
                    RouteSegmentExpectation::Literal(RouteLiteral::Sandboxes),
                    RouteSegmentExpectation::Bound(RouteBinding::OccurrenceId),
                    RouteSegmentExpectation::Literal(RouteLiteral::Files),
                    RouteSegmentExpectation::Literal(RouteLiteral::Upload),
                    RouteSegmentExpectation::Literal(RouteLiteral::Token),
                ],
                ExpectedRouteQuery::OwnerAndFilePath,
            ) && route_matches(
                &self.routes.sandbox_file_download_token_by_id,
                &[
                    RouteSegmentExpectation::Literal(RouteLiteral::V1),
                    RouteSegmentExpectation::Literal(RouteLiteral::Sandboxes),
                    RouteSegmentExpectation::Bound(RouteBinding::OccurrenceId),
                    RouteSegmentExpectation::Literal(RouteLiteral::Files),
                    RouteSegmentExpectation::Literal(RouteLiteral::Download),
                    RouteSegmentExpectation::Literal(RouteLiteral::Token),
                ],
                ExpectedRouteQuery::OwnerAndFilePath,
            ) && route_matches(
                &self.routes.sandbox_run_stream_token_by_id,
                &[
                    RouteSegmentExpectation::Literal(RouteLiteral::V1),
                    RouteSegmentExpectation::Literal(RouteLiteral::Sandboxes),
                    RouteSegmentExpectation::Bound(RouteBinding::OccurrenceId),
                    RouteSegmentExpectation::Literal(RouteLiteral::Runs),
                    RouteSegmentExpectation::Literal(RouteLiteral::Stream),
                    RouteSegmentExpectation::Literal(RouteLiteral::Token),
                ],
                ExpectedRouteQuery::Owner,
            ),
            "provider spec route profile is unsupported"
        );
        self.network_policy.validate()?;
        self.validate_operations()
    }

    /// The exact policy a lease create requests for `lease_egress`, the
    /// admitted lease's guest egress hosts, derived from the signed
    /// `network_policy` declaration. Workload: a remote Codex lease whose
    /// guest must reach only its controller.
    pub(crate) fn egress_policy(&self, lease_egress: &BTreeSet<String>) -> Result<EgressPolicy> {
        self.network_policy
            .egress_policy(SPEC_EVIDENCE_PROVIDER, lease_egress)
    }

    pub(crate) fn network_policy_capability(&self) -> &NetworkPolicyCapability {
        &self.network_policy
    }

    /// The node lease-budget seconds one second of a Render lease costs.
    pub(crate) fn budget_weight(&self) -> u32 {
        self.budget_weight
    }

    fn validate_operations(&self) -> Result<()> {
        let allocate = &self.operations.allocate;
        ensure!(
            allocate.kind == OperationKind::CreateOnce
                && allocate.route == Some(RouteName::SandboxCollection)
                && allocate.precondition_profile
                    == Some(PreconditionProfile::RenderConfiguredRuntimeSnapshotV1)
                && allocate.body.as_ref() == Some(&expected_create_body_mapping())
                && allocate.bind_proof_profile == Some(BindProofProfile::RenderSandboxCreate201V1)
                && allocate.no_occurrence_proof_profile
                    == Some(NoOccurrenceProofProfile::RenderNoRequestSentV1)
                && allocate.mutation_route.is_none()
                && allocate.observation_route.is_none()
                && allocate.terminal_proof_profile.is_none(),
            "provider spec allocation operation is unsupported"
        );
        let bootstrap = &self.operations.bootstrap_create;
        ensure!(
            bootstrap.kind == OperationKind::CreateOnce
                && bootstrap.route == Some(RouteName::SandboxCollection)
                && bootstrap.precondition_profile
                    == Some(PreconditionProfile::RenderMaterializationBootstrapV1)
                && bootstrap.body.as_ref() == Some(&expected_bootstrap_create_body_mapping())
                && bootstrap.bind_proof_profile == Some(BindProofProfile::RenderSandboxCreate201V1)
                && bootstrap.no_occurrence_proof_profile
                    == Some(NoOccurrenceProofProfile::RenderNoRequestSentV1)
                && bootstrap.mutation_route.is_none()
                && bootstrap.observation_route.is_none()
                && bootstrap.terminal_proof_profile.is_none(),
            "provider spec bootstrap create operation is unsupported"
        );
        let qualification = &self.operations.qualification_create;
        ensure!(
            qualification.kind == OperationKind::CreateOnce
                && qualification.route == Some(RouteName::SandboxCollection)
                && qualification.precondition_profile
                    == Some(PreconditionProfile::RenderBoundRuntimeSnapshotV1)
                && qualification.body.as_ref()
                    == Some(&expected_qualification_create_body_mapping())
                && qualification.bind_proof_profile
                    == Some(BindProofProfile::RenderSandboxCreate201V1)
                && qualification.no_occurrence_proof_profile
                    == Some(NoOccurrenceProofProfile::RenderNoRequestSentV1)
                && qualification.mutation_route.is_none()
                && qualification.observation_route.is_none()
                && qualification.terminal_proof_profile.is_none(),
            "provider spec qualification create operation is unsupported"
        );
        let verifier = &self.operations.qualification_verify;
        ensure!(
            verifier.kind == OperationKind::UploadThenRunOnce
                && verifier.precondition_profile
                    == Some(PreconditionProfile::RenderBoundQualificationOccurrenceV1)
                && verifier.route.is_none()
                && verifier.body.is_none()
                && verifier.bind_proof_profile.is_none()
                && verifier.no_occurrence_proof_profile.is_none()
                && verifier.mutation_route == Some(RouteName::SandboxFileUploadTokenById)
                && verifier.observation_route == Some(RouteName::SandboxRunStreamTokenById)
                && verifier.terminal_proof_profile.is_none(),
            "provider spec restored verifier operation is unsupported"
        );
        ensure!(
            self.operations.reconcile_allocation.is_empty_pending()
                && (self.operations.activate_supervisor.is_empty_pending()
                    || self
                        .operations
                        .activate_supervisor
                        .is_one_shot_pending_activation())
                && self
                    .operations
                    .reconcile_supervisor_activation
                    .is_empty_pending(),
            "provider spec reconciliation or activation operation is unsupported"
        );
        ensure!(
            self.operations.relay_file.is_empty_pending()
                || (self
                    .operations
                    .activate_supervisor
                    .is_one_shot_pending_activation()
                    && self.operations.relay_file.is_journal_claimed_file_relay()),
            "provider spec relay file operation is unsupported"
        );
        // The guest channel is either absent or the controller-relayed
        // transport: the guest reaches the controller over its own egress and
        // the controller relays the guest's first frames to this adapter on an
        // inherited descriptor. Only a guest activated one-shot with its
        // per-lease key can sign a Ready, so the relayed transport requires
        // one-shot activation.
        ensure!(
            self.operations.guest_channel.is_empty_pending()
                || (self
                    .operations
                    .activate_supervisor
                    .is_one_shot_pending_activation()
                    && self
                        .operations
                        .guest_channel
                        .is_controller_relayed_guest_frames()),
            "provider spec guest channel operation is unsupported"
        );
        let terminate = &self.operations.terminate;
        ensure!(
            terminate.kind == OperationKind::TerminateOnceThenObserveExact
                && terminate.route.is_none()
                && terminate.precondition_profile.is_none()
                && terminate.body.is_none()
                && terminate.bind_proof_profile.is_none()
                && terminate.no_occurrence_proof_profile.is_none()
                && terminate.mutation_route == Some(RouteName::SandboxTerminateById)
                && terminate.observation_route == Some(RouteName::SandboxById)
                && terminate.terminal_proof_profile
                    == Some(TerminalProofProfile::RenderSandboxTerminalV1),
            "provider spec termination operation is unsupported"
        );
        let reconcile = &self.operations.reconcile_termination;
        ensure!(
            reconcile.kind == OperationKind::ObserveExactOccurrence
                && reconcile.route == Some(RouteName::SandboxById)
                && reconcile.precondition_profile.is_none()
                && reconcile.body.is_none()
                && reconcile.bind_proof_profile.is_none()
                && reconcile.no_occurrence_proof_profile.is_none()
                && reconcile.mutation_route.is_none()
                && reconcile.observation_route.is_none()
                && reconcile.terminal_proof_profile
                    == Some(TerminalProofProfile::RenderSandboxTerminalV1),
            "provider spec termination reconciliation is unsupported"
        );
        let terminal_list = &self.operations.bootstrap_terminated_list;
        ensure!(
            terminal_list.kind == OperationKind::ObserveExactOccurrence
                && terminal_list.route == Some(RouteName::SandboxCollection)
                && terminal_list.precondition_profile.is_none()
                && terminal_list.body.is_none()
                && terminal_list.bind_proof_profile.is_none()
                && terminal_list.no_occurrence_proof_profile.is_none()
                && terminal_list.mutation_route.is_none()
                && terminal_list.observation_route.is_none()
                && terminal_list.terminal_proof_profile
                    == Some(TerminalProofProfile::RenderSandboxTerminatedListV1),
            "provider spec bootstrap terminal list is unsupported"
        );
        Ok(())
    }

    pub(crate) fn effective_capabilities(&self) -> BTreeSet<LifecycleCapability> {
        let mut capabilities = BTreeSet::new();
        if self.operations.allocate.kind == OperationKind::CreateOnce
            && self.operations.allocate.no_occurrence_proof_profile
                == Some(NoOccurrenceProofProfile::RenderNoRequestSentV1)
        {
            capabilities.insert(LifecycleCapability::AuthoritativeNoOccurrence);
        }
        if self.operations.terminate.kind == OperationKind::TerminateOnceThenObserveExact
            && self.operations.terminate.terminal_proof_profile
                == Some(TerminalProofProfile::RenderSandboxTerminalV1)
            && self.operations.reconcile_termination.kind == OperationKind::ObserveExactOccurrence
            && self.operations.reconcile_termination.terminal_proof_profile
                == Some(TerminalProofProfile::RenderSandboxTerminalV1)
        {
            capabilities.insert(LifecycleCapability::ExactTerminalObservation);
        }
        // Starting requires both protected one-shot delivery and the exact
        // journal-claimed relay used by the authenticated supervisor channel.
        // This capability does not attest Ready: activation still returns
        // SupervisorPending until the channel owner verifies Ready and Release.
        if self.one_shot_activation_enabled() && self.relay_file_enabled() {
            capabilities.insert(LifecycleCapability::SupervisorActivation);
        }
        capabilities
    }

    /// The implemented guest transport the signed spec selects. Every other
    /// operation kind is refused by name, so adding a kind forces a decision
    /// here.
    pub(crate) fn guest_channel(&self) -> Result<GuestChannelProfile> {
        let kind = self.operations.guest_channel.kind;
        match kind {
            OperationKind::UnsupportedPending => Ok(GuestChannelProfile::UnsupportedPending),
            OperationKind::ControllerRelayedGuestFrames => {
                Ok(GuestChannelProfile::ControllerRelayedFrames)
            }
            OperationKind::CreateOnce
            | OperationKind::UploadThenRunOncePending
            | OperationKind::UploadThenRunOnce
            | OperationKind::JournalClaimedFileRelay
            | OperationKind::TerminateOnceThenObserveExact
            | OperationKind::ObserveExactOccurrence => anyhow::bail!(
                "provider spec guest channel kind {kind:?} is not an implemented guest transport"
            ),
        }
    }

    /// Provider-attested lease guarantees for remote Codex exec-server leases,
    /// derived only from implemented operation kinds and proof profiles. The
    /// signed data names mechanisms; it can never name a guarantee directly.
    pub(crate) fn lease_guarantees(&self) -> BTreeSet<&'static str> {
        let mut guarantees = BTreeSet::new();
        let allocate = &self.operations.allocate;
        if allocate.kind == OperationKind::CreateOnce
            && allocate.bind_proof_profile == Some(BindProofProfile::RenderSandboxCreate201V1)
        {
            guarantees.insert(LEASE_CREATE_BOUND);
        }
        if let Some(body) = &allocate.body {
            if body.timeout_seconds.source == LifetimeFieldSource::ReservationMaximumLifetimeSeconds
            {
                guarantees.insert(LEASE_LIFETIME_BOUND);
            }
            if body.network_policy.source == Some(EgressFieldSource::LeaseGuestNetworkAllow) {
                guarantees.insert(LEASE_NETWORK_POLICY);
            }
        }
        if self
            .effective_capabilities()
            .contains(&LifecycleCapability::ExactTerminalObservation)
        {
            guarantees.insert(LEASE_TERMINATE_OBSERVED);
        }
        let channel_implemented = match self.guest_channel() {
            Ok(GuestChannelProfile::ControllerRelayedFrames) => true,
            Ok(GuestChannelProfile::UnsupportedPending) | Err(_) => false,
        };
        if channel_implemented && self.one_shot_activation_enabled() {
            guarantees.insert(GUEST_READY_AUTHENTICATED);
        }
        guarantees
    }

    /// This only describes the installed adapter's implementation. The
    /// controller separately authenticates the exact qualified runtime
    /// product before permitting allocation or first activation contact.
    pub(crate) fn one_shot_activation_enabled(&self) -> bool {
        self.operations
            .activate_supervisor
            .is_one_shot_pending_activation()
    }

    pub(crate) fn relay_file_enabled(&self) -> bool {
        self.operations.relay_file.is_journal_claimed_file_relay()
    }

    pub(crate) fn relay_file_upload_route(&self) -> Result<RouteName> {
        ensure!(
            self.relay_file_enabled(),
            "signed Render relay file operation is disabled"
        );
        Ok(self
            .operations
            .relay_file
            .mutation_route
            .expect("validated relay upload route"))
    }

    pub(crate) fn relay_file_download_route(&self) -> Result<RouteName> {
        ensure!(
            self.relay_file_enabled(),
            "signed Render relay file operation is disabled"
        );
        Ok(self
            .operations
            .relay_file
            .observation_route
            .expect("validated relay download route"))
    }

    pub(crate) fn api_base(&self) -> &'static str {
        match self.origin_profile {
            OriginProfile::RenderApiV1 => "https://api.render.com/",
        }
    }

    pub(crate) fn allocation_route(&self) -> Option<RouteName> {
        self.operations.allocate.route
    }

    pub(crate) fn qualification_create_route(&self) -> Option<RouteName> {
        self.operations.qualification_create.route
    }

    pub(crate) fn bootstrap_create_route(&self) -> Option<RouteName> {
        self.operations.bootstrap_create.route
    }

    pub(crate) fn bootstrap_profile_digest(&self) -> &str {
        &self.bootstrap_profile_digest
    }

    pub(crate) fn qualification_verifier_routes(&self) -> (RouteName, RouteName) {
        // `validate_operations` has already checked both finite route names.
        (
            self.operations.qualification_verify.mutation_route.unwrap(),
            self.operations
                .qualification_verify
                .observation_route
                .unwrap(),
        )
    }

    pub(crate) fn allocation_requires_configured_runtime_snapshot(&self) -> bool {
        self.operations.allocate.precondition_profile
            == Some(PreconditionProfile::RenderConfiguredRuntimeSnapshotV1)
    }

    pub(crate) fn allocation_bind_proof_enabled(&self) -> bool {
        self.operations.allocate.bind_proof_profile.is_some()
    }

    pub(crate) fn allocation_no_occurrence_proof_enabled(&self) -> bool {
        self.operations
            .allocate
            .no_occurrence_proof_profile
            .is_some()
    }

    pub(crate) fn termination_mutation_route(&self) -> Option<RouteName> {
        self.operations.terminate.mutation_route
    }

    pub(crate) fn termination_observation_route(&self) -> Option<RouteName> {
        self.operations.terminate.observation_route
    }

    pub(crate) fn termination_terminal_proof_enabled(&self) -> bool {
        self.operations.terminate.terminal_proof_profile.is_some()
    }

    pub(crate) fn reconciliation_route(&self) -> Option<RouteName> {
        self.operations.reconcile_termination.route
    }

    pub(crate) fn bootstrap_terminal_list_route(&self) -> Option<RouteName> {
        self.operations.bootstrap_terminated_list.route
    }

    pub(crate) fn reconciliation_terminal_proof_enabled(&self) -> bool {
        self.operations
            .reconcile_termination
            .terminal_proof_profile
            .is_some()
    }

    pub(crate) fn route_target(
        &self,
        name: RouteName,
        occurrence_id: Option<&str>,
        settings_owner_id: &str,
        file_path: Option<&str>,
    ) -> Result<RouteTarget> {
        let route = match name {
            RouteName::SandboxCollection => &self.routes.sandbox_collection,
            RouteName::SandboxById => &self.routes.sandbox_by_id,
            RouteName::SandboxTerminateById => &self.routes.sandbox_terminate_by_id,
            RouteName::SandboxFileUploadTokenById => &self.routes.sandbox_file_upload_token_by_id,
            RouteName::SandboxFileDownloadTokenById => {
                &self.routes.sandbox_file_download_token_by_id
            }
            RouteName::SandboxRunStreamTokenById => &self.routes.sandbox_run_stream_token_by_id,
        };
        let mut path_segments = Vec::with_capacity(route.segments.len());
        let mut has_occurrence_binding = false;
        for segment in &route.segments {
            match (segment.literal, segment.bound) {
                (Some(literal), None) => path_segments.push(literal.as_str().to_owned()),
                (None, Some(RouteBinding::OccurrenceId)) => {
                    let id = occurrence_id.context("route requires a bound occurrence id")?;
                    ensure!(valid_occurrence_id(id), "invalid bound occurrence id");
                    has_occurrence_binding = true;
                    path_segments.push(id.to_owned());
                }
                _ => anyhow::bail!("provider spec contains an invalid route segment"),
            }
        }
        ensure!(
            has_occurrence_binding == occurrence_id.is_some(),
            "bound occurrence id does not match the selected route"
        );
        let owner_id_query = match &route.query {
            Some(query) if query.owner_id.source == StringFieldSource::SettingsOwnerId => {
                Some(settings_owner_id.to_owned())
            }
            Some(_) => anyhow::bail!("provider spec query projection is unsupported"),
            None => None,
        };
        let file_path_query = match (&route.query, file_path) {
            (Some(query), Some(path))
                if query.path.as_ref().is_some_and(|source| {
                    source.source == FilePathFieldSource::OperationFilePath
                }) =>
            {
                ensure!(
                    path.starts_with('/')
                        && path.len() <= 2048
                        && path.split('/').skip(1).all(|segment| {
                            !segment.is_empty()
                                && segment != "."
                                && segment != ".."
                                && segment.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric()
                                        || matches!(byte, b'-' | b'_' | b'.')
                                })
                        }),
                    "operation file path is invalid"
                );
                Some(path.to_owned())
            }
            (Some(query), None) if query.path.is_none() => None,
            (None, None) => None,
            _ => anyhow::bail!("operation file path does not match the signed route"),
        };
        Ok(RouteTarget {
            path_segments,
            owner_id_query,
            file_path_query,
        })
    }

    pub(crate) fn create_projection(
        &self,
        settings_owner_id: &str,
        settings_plan: PlanValue,
        settings_region: &str,
        settings_snapshot_id: &str,
        reservation_lifetime_seconds: u32,
        lease_egress: &BTreeSet<String>,
    ) -> Result<CreateProjection> {
        let mapping = self
            .operations
            .allocate
            .body
            .as_ref()
            .context("provider spec has no allocation body mapping")?;
        let owner_id = match mapping.owner_id.source {
            StringFieldSource::SettingsOwnerId => settings_owner_id,
            _ => anyhow::bail!("provider spec owner projection is unsupported"),
        };
        let region = match mapping.region.source {
            StringFieldSource::SettingsRegion => settings_region,
            _ => anyhow::bail!("provider spec region projection is unsupported"),
        };
        let snapshot_id = match mapping
            .snapshot_id
            .as_ref()
            .context("allocation snapshot mapping missing")?
            .source
        {
            StringFieldSource::SettingsSnapshotId => settings_snapshot_id,
            _ => anyhow::bail!("provider spec snapshot projection is unsupported"),
        };
        let plan = match mapping.plan.source {
            PlanFieldSource::SettingsPlan => settings_plan,
        };
        let timeout_seconds = match mapping.timeout_seconds.source {
            LifetimeFieldSource::ReservationMaximumLifetimeSeconds => reservation_lifetime_seconds,
            LifetimeFieldSource::QualificationMaximumLifetimeSeconds => {
                anyhow::bail!("worker allocation cannot select qualification lifetime")
            }
            LifetimeFieldSource::BootstrapMaximumLifetimeSeconds => {
                anyhow::bail!("worker allocation cannot select bootstrap lifetime")
            }
        };
        let network_policy = match (
            &mapping.network_policy.literal,
            mapping.network_policy.source,
        ) {
            (None, Some(EgressFieldSource::LeaseGuestNetworkAllow)) => {
                self.egress_policy(lease_egress)?
            }
            _ => anyhow::bail!("provider spec allocation network policy projection is unsupported"),
        };
        Ok(CreateProjection {
            owner_id: owner_id.to_owned(),
            plan,
            region: region.to_owned(),
            timeout_seconds,
            network_policy,
            snapshot_id: snapshot_id.to_owned(),
        })
    }

    pub(crate) fn qualification_create_projection(
        &self,
        settings_owner_id: &str,
        settings_plan: PlanValue,
        settings_region: &str,
        bound_snapshot_id: &str,
        maximum_lifetime_seconds: u32,
    ) -> Result<CreateProjection> {
        let mapping = self
            .operations
            .qualification_create
            .body
            .as_ref()
            .context("provider spec has no qualification create mapping")?;
        ensure!(
            mapping.owner_id.source == StringFieldSource::SettingsOwnerId
                && mapping.region.source == StringFieldSource::SettingsRegion
                && mapping.plan.source == PlanFieldSource::SettingsPlan
                && mapping.snapshot_id.as_ref().map(|source| source.source)
                    == Some(StringFieldSource::QualificationLocatorSnapshotId)
                && mapping.timeout_seconds.source
                    == LifetimeFieldSource::QualificationMaximumLifetimeSeconds
                && mapping.network_policy
                    == NetworkPolicyMapping {
                        literal: Some(NetworkPolicyTypeLiteral {
                            policy_type: NetworkPolicyType::DenyAll,
                        }),
                        source: None,
                    },
            "qualification create mapping differs from its reviewed field sources"
        );
        ensure!(
            bound_snapshot_id.starts_with("snp-")
                && bound_snapshot_id.len() <= 256
                && bound_snapshot_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (1..=3600).contains(&maximum_lifetime_seconds),
            "qualification create snapshot or lifetime is invalid"
        );
        Ok(CreateProjection {
            owner_id: settings_owner_id.to_owned(),
            plan: settings_plan,
            region: settings_region.to_owned(),
            timeout_seconds: maximum_lifetime_seconds,
            network_policy: EgressPolicy::deny_all(),
            snapshot_id: bound_snapshot_id.to_owned(),
        })
    }

    pub(crate) fn bootstrap_create_projection(
        &self,
        settings_owner_id: &str,
        settings_plan: PlanValue,
        settings_region: &str,
        maximum_lifetime_seconds: u32,
    ) -> Result<BootstrapCreateProjection> {
        let mapping = self
            .operations
            .bootstrap_create
            .body
            .as_ref()
            .context("provider spec has no bootstrap create mapping")?;
        ensure!(
            mapping == &expected_bootstrap_create_body_mapping()
                && (1..=3600).contains(&maximum_lifetime_seconds),
            "bootstrap create mapping or lifetime differs from signed profile"
        );
        Ok(BootstrapCreateProjection {
            owner_id: settings_owner_id.to_owned(),
            plan: settings_plan,
            region: settings_region.to_owned(),
            timeout_seconds: maximum_lifetime_seconds,
            network_policy: EgressPolicy::deny_all(),
        })
    }
}

impl OperationSpec {
    fn is_journal_claimed_file_relay(&self) -> bool {
        self.kind == OperationKind::JournalClaimedFileRelay
            && self.route.is_none()
            && self.precondition_profile.is_none()
            && self.body.is_none()
            && self.bind_proof_profile.is_none()
            && self.no_occurrence_proof_profile.is_none()
            && self.mutation_route == Some(RouteName::SandboxFileUploadTokenById)
            && self.observation_route == Some(RouteName::SandboxFileDownloadTokenById)
            && self.terminal_proof_profile.is_none()
    }

    fn is_controller_relayed_guest_frames(&self) -> bool {
        self.kind == OperationKind::ControllerRelayedGuestFrames
            && self.route.is_none()
            && self.precondition_profile.is_none()
            && self.body.is_none()
            && self.bind_proof_profile.is_none()
            && self.no_occurrence_proof_profile.is_none()
            && self.mutation_route.is_none()
            && self.observation_route.is_none()
            && self.terminal_proof_profile.is_none()
    }

    fn is_one_shot_pending_activation(&self) -> bool {
        self.kind == OperationKind::UploadThenRunOncePending
            && self.precondition_profile == Some(PreconditionProfile::RenderQualifiedGuestRuntimeV1)
            && self.route.is_none()
            && self.body.is_none()
            && self.bind_proof_profile.is_none()
            && self.no_occurrence_proof_profile.is_none()
            && self.mutation_route.is_none()
            && self.observation_route.is_none()
            && self.terminal_proof_profile.is_none()
    }

    fn is_empty_pending(&self) -> bool {
        self.kind == OperationKind::UnsupportedPending
            && self.route.is_none()
            && self.precondition_profile.is_none()
            && self.body.is_none()
            && self.bind_proof_profile.is_none()
            && self.no_occurrence_proof_profile.is_none()
            && self.mutation_route.is_none()
            && self.observation_route.is_none()
            && self.terminal_proof_profile.is_none()
    }
}

enum RouteSegmentExpectation {
    Literal(RouteLiteral),
    Bound(RouteBinding),
}

#[derive(Clone, Copy)]
enum ExpectedRouteQuery {
    None,
    Owner,
    OwnerAndFilePath,
}

fn route_matches(
    route: &RouteSpec,
    expected_segments: &[RouteSegmentExpectation],
    expected_query: ExpectedRouteQuery,
) -> bool {
    route.segments.len() == expected_segments.len()
        && route
            .segments
            .iter()
            .zip(expected_segments)
            .all(|(actual, expected)| match expected {
                RouteSegmentExpectation::Literal(value) => {
                    actual.literal == Some(*value) && actual.bound.is_none()
                }
                RouteSegmentExpectation::Bound(value) => {
                    actual.literal.is_none() && actual.bound == Some(*value)
                }
            })
        && match (&route.query, expected_query) {
            (None, ExpectedRouteQuery::None) => true,
            (Some(query), ExpectedRouteQuery::Owner) => {
                query.owner_id.source == StringFieldSource::SettingsOwnerId && query.path.is_none()
            }
            (Some(query), ExpectedRouteQuery::OwnerAndFilePath) => {
                query.owner_id.source == StringFieldSource::SettingsOwnerId
                    && query.path.as_ref().is_some_and(|source| {
                        source.source == FilePathFieldSource::OperationFilePath
                    })
            }
            _ => false,
        }
}

fn expected_create_body_mapping() -> CreateBodyMapping {
    CreateBodyMapping {
        owner_id: StringSource {
            source: StringFieldSource::SettingsOwnerId,
        },
        plan: PlanSource {
            source: PlanFieldSource::SettingsPlan,
        },
        region: StringSource {
            source: StringFieldSource::SettingsRegion,
        },
        timeout_seconds: LifetimeSource {
            source: LifetimeFieldSource::ReservationMaximumLifetimeSeconds,
        },
        network_policy: NetworkPolicyMapping {
            literal: None,
            source: Some(EgressFieldSource::LeaseGuestNetworkAllow),
        },
        snapshot_id: Some(StringSource {
            source: StringFieldSource::SettingsSnapshotId,
        }),
    }
}

fn expected_qualification_create_body_mapping() -> CreateBodyMapping {
    CreateBodyMapping {
        owner_id: StringSource {
            source: StringFieldSource::SettingsOwnerId,
        },
        plan: PlanSource {
            source: PlanFieldSource::SettingsPlan,
        },
        region: StringSource {
            source: StringFieldSource::SettingsRegion,
        },
        timeout_seconds: LifetimeSource {
            source: LifetimeFieldSource::QualificationMaximumLifetimeSeconds,
        },
        network_policy: NetworkPolicyMapping {
            literal: Some(NetworkPolicyTypeLiteral {
                policy_type: NetworkPolicyType::DenyAll,
            }),
            source: None,
        },
        snapshot_id: Some(StringSource {
            source: StringFieldSource::QualificationLocatorSnapshotId,
        }),
    }
}

fn expected_bootstrap_create_body_mapping() -> CreateBodyMapping {
    CreateBodyMapping {
        owner_id: StringSource {
            source: StringFieldSource::SettingsOwnerId,
        },
        plan: PlanSource {
            source: PlanFieldSource::SettingsPlan,
        },
        region: StringSource {
            source: StringFieldSource::SettingsRegion,
        },
        timeout_seconds: LifetimeSource {
            source: LifetimeFieldSource::BootstrapMaximumLifetimeSeconds,
        },
        network_policy: NetworkPolicyMapping {
            literal: Some(NetworkPolicyTypeLiteral {
                policy_type: NetworkPolicyType::DenyAll,
            }),
            source: None,
        },
        snapshot_id: None,
    }
}

fn valid_occurrence_id(id: &str) -> bool {
    id.starts_with("sbx-")
        && id.len() > "sbx-".len()
        && id.len() <= 512
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn deserialize_non_null_option<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Test-only variant of a signed spec that declares deny-all creation only,
/// so tests cover a spec without allow-list creation while the fixture stays
/// byte-identical to the authored bundle spec.
#[cfg(test)]
pub(crate) fn deny_all_only_spec(bytes: &[u8]) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    value["network_policy"] = serde_json::json!({"creatable_types": ["deny-all"]});
    serde_json::to_vec_pretty(&value).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS_SCHEMA_DIGEST: &str =
        "f5b4994d71a6896ddad77bf2a08716fdc3527ea4f91579f084112ca69dad7172";

    /// The authored spec with activation, relay and guest channel unselected
    /// and deny-all creation only: the narrowest profile the parser accepts.
    fn fixture() -> Vec<u8> {
        let mut value: serde_json::Value = serde_json::from_slice(&deny_all_only_spec(
            include_bytes!("../fixtures/provider-spec.json"),
        ))
        .unwrap();
        value["operations"]["activate_supervisor"] =
            serde_json::json!({"kind": "unsupported_pending"});
        value["operations"]["relay_file"] = serde_json::json!({"kind": "unsupported_pending"});
        value["operations"]["guest_channel"] = serde_json::json!({"kind": "unsupported_pending"});
        serde_json::to_vec_pretty(&value).unwrap()
    }

    #[test]
    fn fixture_spec_is_the_authored_bundle_spec() {
        assert_eq!(
            include_bytes!("../fixtures/provider-spec.json").as_slice(),
            include_bytes!("../../../../bundles/render-sandbox/specs/provider-spec.json")
                .as_slice()
        );
    }

    #[test]
    fn authored_render_profile_selects_only_pending_startup_and_exact_file_relay() {
        let bytes = include_bytes!("../../../../bundles/render-sandbox/specs/provider-spec.json");
        let spec = ProviderSpec::parse(bytes, SETTINGS_SCHEMA_DIGEST).unwrap();
        assert!(spec.one_shot_activation_enabled());
        assert!(spec.relay_file_enabled());
        assert_eq!(
            spec.relay_file_upload_route().unwrap(),
            RouteName::SandboxFileUploadTokenById
        );
        assert_eq!(
            spec.relay_file_download_route().unwrap(),
            RouteName::SandboxFileDownloadTokenById
        );
        assert!(
            spec.operations
                .reconcile_supervisor_activation
                .is_empty_pending()
        );
        assert!(spec.operations.reconcile_allocation.is_empty_pending());
        assert_eq!(
            spec.effective_capabilities(),
            BTreeSet::from([
                LifecycleCapability::SupervisorActivation,
                LifecycleCapability::AuthoritativeNoOccurrence,
                LifecycleCapability::ExactTerminalObservation,
            ])
        );

        // A source/profile edit cannot turn a pending launch into reconciliation
        // evidence or admit a different relay route through the closed parser.
        let mut wrong_reconciliation: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        wrong_reconciliation["operations"]["reconcile_supervisor_activation"] =
            serde_json::json!({"kind": "observe_exact_occurrence", "route": "sandbox_by_id"});
        assert!(
            ProviderSpec::parse(
                &serde_json::to_vec(&wrong_reconciliation).unwrap(),
                SETTINGS_SCHEMA_DIGEST,
            )
            .is_err()
        );
        let mut wrong_relay: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        wrong_relay["operations"]["relay_file"]["observation_route"] =
            serde_json::json!("sandbox_run_stream_token_by_id");
        assert!(
            ProviderSpec::parse(
                &serde_json::to_vec(&wrong_relay).unwrap(),
                SETTINGS_SCHEMA_DIGEST,
            )
            .is_err()
        );
    }

    #[test]
    fn the_budget_weight_is_required_and_bounded() {
        assert_eq!(
            ProviderSpec::parse(&fixture(), SETTINGS_SCHEMA_DIGEST)
                .unwrap()
                .budget_weight(),
            1
        );
        let value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        let parse = |value: &serde_json::Value| {
            ProviderSpec::parse(&serde_json::to_vec(value).unwrap(), SETTINGS_SCHEMA_DIGEST)
        };
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove("budget_weight");
        let refusal = format!("{:#}", parse(&missing).unwrap_err());
        assert!(refusal.contains("budget_weight"), "{refusal}");
        for weight in [0, MAX_LEASE_BUDGET_WEIGHT + 1] {
            let mut out_of_bound = value.clone();
            out_of_bound["budget_weight"] = serde_json::json!(weight);
            let refusal = format!("{:#}", parse(&out_of_bound).unwrap_err());
            assert!(
                refusal.contains(&format!(
                    "provider spec budget_weight {weight} is outside 1..={MAX_LEASE_BUDGET_WEIGHT}"
                )),
                "{refusal}"
            );
        }
        for refused in [serde_json::json!(-1), serde_json::json!(1.5), serde_json::json!("1")] {
            let mut malformed = value.clone();
            malformed["budget_weight"] = refused;
            assert!(parse(&malformed).is_err());
        }
        let mut heavy = value;
        heavy["budget_weight"] = serde_json::json!(MAX_LEASE_BUDGET_WEIGHT);
        assert_eq!(parse(&heavy).unwrap().budget_weight(), MAX_LEASE_BUDGET_WEIGHT);
    }

    #[test]
    fn relay_file_requires_explicit_exact_signed_operation_and_activation_selection() {
        let mut value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        let pending = ProviderSpec::parse(&fixture(), SETTINGS_SCHEMA_DIGEST).unwrap();
        assert!(!pending.relay_file_enabled());
        assert!(
            !pending
                .effective_capabilities()
                .contains(&LifecycleCapability::SupervisorActivation)
        );
        assert!(pending.relay_file_upload_route().is_err());
        assert!(pending.relay_file_download_route().is_err());
        let mut missing = value.clone();
        missing["operations"]
            .as_object_mut()
            .unwrap()
            .remove("relay_file");
        assert!(
            ProviderSpec::parse(
                &serde_json::to_vec(&missing).unwrap(),
                SETTINGS_SCHEMA_DIGEST
            )
            .is_err()
        );
        missing = value.clone();
        missing["schema"] = serde_json::json!(4);
        assert!(
            ProviderSpec::parse(
                &serde_json::to_vec(&missing).unwrap(),
                SETTINGS_SCHEMA_DIGEST
            )
            .is_err()
        );

        value["operations"]["relay_file"] = serde_json::json!({
            "kind": "journal_claimed_file_relay",
            "mutation_route": "sandbox_file_upload_token_by_id",
            "observation_route": "sandbox_file_download_token_by_id"
        });
        assert!(
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
        value["operations"]["activate_supervisor"] = serde_json::json!({
            "kind": "upload_then_run_once_pending",
            "precondition_profile": "render_qualified_guest_runtime_v1"
        });
        let selected =
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .unwrap();
        assert_eq!(
            selected.relay_file_upload_route().unwrap(),
            RouteName::SandboxFileUploadTokenById
        );
        assert_eq!(
            selected.relay_file_download_route().unwrap(),
            RouteName::SandboxFileDownloadTokenById
        );
        assert_eq!(
            selected.effective_capabilities(),
            BTreeSet::from([
                LifecycleCapability::SupervisorActivation,
                LifecycleCapability::AuthoritativeNoOccurrence,
                LifecycleCapability::ExactTerminalObservation,
            ]),
            "activation plus relay admits startup, not runtime or reconciliation testimony"
        );

        value["operations"]["relay_file"]["observation_route"] =
            serde_json::json!("sandbox_run_stream_token_by_id");
        assert!(
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
        value["operations"]["relay_file"]["observation_route"] =
            serde_json::json!("sandbox_file_download_token_by_id");
        value["operations"]["relay_file"]["route"] = serde_json::json!("sandbox_collection");
        assert!(
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
    }

    #[test]
    fn one_shot_activation_profile_is_closed_and_reconciliation_stays_pending() {
        let mut value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        value["operations"]["activate_supervisor"] = serde_json::json!({
            "kind": "upload_then_run_once_pending",
            "precondition_profile": "render_qualified_guest_runtime_v1"
        });
        let encoded = serde_json::to_vec(&value).unwrap();
        let spec = ProviderSpec::parse(&encoded, SETTINGS_SCHEMA_DIGEST).unwrap();
        assert!(spec.one_shot_activation_enabled());
        assert_eq!(
            spec.effective_capabilities(),
            BTreeSet::from([
                LifecycleCapability::AuthoritativeNoOccurrence,
                LifecycleCapability::ExactTerminalObservation,
            ]),
            "one-shot delivery cannot claim Ready or runtime admission"
        );
        value["operations"]["reconcile_supervisor_activation"] = serde_json::json!({
            "kind": "upload_then_run_once_pending",
            "precondition_profile": "render_qualified_guest_runtime_v1"
        });
        assert!(
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
        value["operations"]["reconcile_supervisor_activation"] =
            serde_json::json!({"kind": "unsupported_pending"});
        value["operations"]["activate_supervisor"]["route"] =
            serde_json::json!("sandbox_collection");
        assert!(
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
    }

    #[test]
    fn render_spec_interprets_only_reviewed_routes_and_proof_profiles() {
        let spec = ProviderSpec::parse(&fixture(), SETTINGS_SCHEMA_DIGEST).unwrap();
        assert_eq!(
            spec.effective_capabilities(),
            BTreeSet::from([
                LifecycleCapability::AuthoritativeNoOccurrence,
                LifecycleCapability::ExactTerminalObservation,
            ])
        );
        assert!(!spec.one_shot_activation_enabled());
        assert_eq!(
            spec.route_target(
                RouteName::SandboxById,
                Some("sbx-fixture-1"),
                "owner-1",
                None
            )
            .unwrap(),
            RouteTarget {
                path_segments: vec!["v1".into(), "sandboxes".into(), "sbx-fixture-1".into()],
                owner_id_query: Some("owner-1".into()),
                file_path_query: None,
            }
        );
        assert_eq!(
            spec.route_target(
                RouteName::SandboxFileUploadTokenById,
                Some("sbx-fixture-1"),
                "owner-1",
                Some("/ryeos/activation/guest-package"),
            )
            .unwrap(),
            RouteTarget {
                path_segments: vec![
                    "v1".into(),
                    "sandboxes".into(),
                    "sbx-fixture-1".into(),
                    "files".into(),
                    "upload".into(),
                    "token".into(),
                ],
                owner_id_query: Some("owner-1".into()),
                file_path_query: Some("/ryeos/activation/guest-package".into()),
            }
        );
        assert_eq!(
            spec.route_target(
                RouteName::SandboxFileDownloadTokenById,
                Some("sbx-fixture-1"),
                "owner-1",
                Some("/ryeos/relay/attach-request"),
            )
            .unwrap(),
            RouteTarget {
                path_segments: vec![
                    "v1".into(),
                    "sandboxes".into(),
                    "sbx-fixture-1".into(),
                    "files".into(),
                    "download".into(),
                    "token".into(),
                ],
                owner_id_query: Some("owner-1".into()),
                file_path_query: Some("/ryeos/relay/attach-request".into()),
            }
        );
        assert!(
            spec.route_target(
                RouteName::SandboxRunStreamTokenById,
                Some("sbx-fixture-1"),
                "owner-1",
                Some("/ryeos/activation/guest-package"),
            )
            .is_err()
        );
        assert_eq!(spec.api_base(), "https://api.render.com/");
    }

    #[test]
    fn spec_rejects_unknown_configuration_and_unreviewed_routes() {
        let fixture = String::from_utf8(fixture()).unwrap();
        let with_capability_claim = fixture.replacen(
            "\"schema\": 8,",
            "\"schema\": 8,\n  \"capabilities\": [\"exact_terminal_observation\"],",
            1,
        );
        assert!(
            ProviderSpec::parse(with_capability_claim.as_bytes(), SETTINGS_SCHEMA_DIGEST).is_err()
        );

        let arbitrary_route =
            fixture.replace("\"literal\": \"sandboxes\"", "\"literal\": \"other-host\"");
        assert!(ProviderSpec::parse(arbitrary_route.as_bytes(), SETTINGS_SCHEMA_DIGEST).is_err());

        let arbitrary_method = fixture.replace(
            "\"kind\": \"create_once\",",
            "\"kind\": \"create_once\", \"method\": \"DELETE\",",
        );
        assert!(ProviderSpec::parse(arbitrary_method.as_bytes(), SETTINGS_SCHEMA_DIGEST).is_err());

        let arbitrary_source = fixture.replace(
            "reservation.maximum_lifetime_seconds",
            "reservation.arbitrary_expression",
        );
        assert!(ProviderSpec::parse(arbitrary_source.as_bytes(), SETTINGS_SCHEMA_DIGEST).is_err());

        let arbitrary_proof = fixture.replace(
            "render_sandbox_terminal_v1",
            "provider_claims_terminal_success_v1",
        );
        assert!(ProviderSpec::parse(arbitrary_proof.as_bytes(), SETTINGS_SCHEMA_DIGEST).is_err());
        let arbitrary_token_route =
            fixture.replacen("\"literal\": \"upload\"", "\"literal\": \"download\"", 1);
        assert!(
            ProviderSpec::parse(arbitrary_token_route.as_bytes(), SETTINGS_SCHEMA_DIGEST).is_err()
        );
        let arbitrary_upload_source =
            fixture.replace("operation.file_path", "settings.snapshot_id");
        assert!(
            ProviderSpec::parse(arbitrary_upload_source.as_bytes(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
        let arbitrary_download_route = fixture.replacen(
            "\"sandbox_file_download_token_by_id\": {",
            "\"sandbox_file_download_token_by_id\": {\"unreviewed\": true,",
            1,
        );
        assert!(
            ProviderSpec::parse(arbitrary_download_route.as_bytes(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
    }

    #[test]
    fn create_projection_uses_only_the_fixed_field_sources() {
        let spec = ProviderSpec::parse(&fixture(), SETTINGS_SCHEMA_DIGEST).unwrap();
        let projection = spec
            .create_projection(
                "owner-1",
                PlanValue::Standard,
                "oregon",
                "snp-fixture-1",
                900,
                &BTreeSet::new(),
            )
            .unwrap();
        assert_eq!(projection.owner_id, "owner-1");
        assert_eq!(projection.plan, PlanValue::Standard);
        assert_eq!(projection.region, "oregon");
        assert_eq!(projection.timeout_seconds, 900);
        assert_eq!(projection.network_policy, EgressPolicy::deny_all());
        assert_eq!(projection.snapshot_id, "snp-fixture-1");
    }

    #[test]
    fn qualification_create_selects_only_retained_locator_snapshot() {
        let spec = ProviderSpec::parse(&fixture(), SETTINGS_SCHEMA_DIGEST).unwrap();
        assert_eq!(
            spec.qualification_create_route(),
            Some(RouteName::SandboxCollection)
        );
        let projection = spec
            .qualification_create_projection(
                "owner-1",
                PlanValue::Starter,
                "oregon",
                "snp-bound-1",
                900,
            )
            .unwrap();
        assert_eq!(projection.snapshot_id, "snp-bound-1");
        assert_eq!(projection.timeout_seconds, 900);
        assert_eq!(projection.network_policy, EgressPolicy::deny_all());
        assert!(
            spec.qualification_create_projection(
                "owner-1",
                PlanValue::Starter,
                "oregon",
                "../ambient",
                900,
            )
            .is_err()
        );
        let mut changed: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        changed["operations"]["qualification_create"]["body"]["snapshotId"]["source"] =
            serde_json::json!("settings.snapshot_id");
        assert!(
            ProviderSpec::parse(
                &serde_json::to_vec(&changed).unwrap(),
                SETTINGS_SCHEMA_DIGEST
            )
            .is_err()
        );
        changed["operations"]["qualification_create"]["body"]["snapshotId"]["source"] =
            serde_json::json!("qualification.locator.snapshot_id");
        changed["qualification_settings_schema_digest"] = serde_json::json!("0".repeat(64));
        assert!(
            ProviderSpec::parse(
                &serde_json::to_vec(&changed).unwrap(),
                SETTINGS_SCHEMA_DIGEST
            )
            .is_err()
        );
    }

    #[test]
    fn bootstrap_create_is_unsnapshotted_and_closed_to_other_field_sources() {
        let spec = ProviderSpec::parse(&fixture(), SETTINGS_SCHEMA_DIGEST).unwrap();
        assert_eq!(
            spec.bootstrap_create_route(),
            Some(RouteName::SandboxCollection)
        );
        let projected = spec
            .bootstrap_create_projection("owner-1", PlanValue::Starter, "oregon", 900)
            .unwrap();
        assert_eq!(projected.owner_id, "owner-1");
        assert_eq!(projected.timeout_seconds, 900);
        assert_eq!(projected.network_policy, EgressPolicy::deny_all());
        let mut value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        value["operations"]["bootstrap_create"]["body"]["snapshotId"] =
            serde_json::json!({"source":"settings.snapshot_id"});
        assert!(
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
        value["operations"]["bootstrap_create"]["body"]
            .as_object_mut()
            .unwrap()
            .remove("snapshotId");
        value["operations"]["bootstrap_create"]["body"]["timeoutSeconds"] =
            serde_json::json!({"source":"reservation.maximum_lifetime_seconds"});
        assert!(
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
    }

    #[test]
    fn restored_verifier_routes_are_finite_and_separate_from_activation() {
        let spec = ProviderSpec::parse(&fixture(), SETTINGS_SCHEMA_DIGEST).unwrap();
        assert_eq!(
            spec.qualification_verifier_routes(),
            (
                RouteName::SandboxFileUploadTokenById,
                RouteName::SandboxRunStreamTokenById,
            )
        );
        assert!(
            spec.route_target(
                RouteName::SandboxFileUploadTokenById,
                Some("sbx-restored"),
                "owner-1",
                Some("/ryeos/qualification"),
            )
            .is_ok()
        );
        let mut changed: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        changed["operations"]["qualification_verify"]["observation_route"] =
            serde_json::json!("sandbox_by_id");
        assert!(
            ProviderSpec::parse(
                &serde_json::to_vec(&changed).unwrap(),
                SETTINGS_SCHEMA_DIGEST
            )
            .is_err()
        );
        changed["operations"]["qualification_verify"]["observation_route"] =
            serde_json::json!("sandbox_run_stream_token_by_id");
        changed["operations"]["qualification_verify"]["precondition_profile"] =
            serde_json::json!("render_qualified_guest_runtime_v1");
        assert!(
            ProviderSpec::parse(
                &serde_json::to_vec(&changed).unwrap(),
                SETTINGS_SCHEMA_DIGEST
            )
            .is_err()
        );
    }

    #[test]
    fn signed_relayed_guest_channel_is_the_only_ready_attestation() {
        let bytes = include_bytes!("../../../../bundles/render-sandbox/specs/provider-spec.json");
        let spec = ProviderSpec::parse(bytes, SETTINGS_SCHEMA_DIGEST).unwrap();
        assert_eq!(
            spec.guest_channel().unwrap(),
            GuestChannelProfile::ControllerRelayedFrames
        );
        assert_eq!(
            spec.lease_guarantees(),
            BTreeSet::from([
                LEASE_CREATE_BOUND,
                LEASE_LIFETIME_BOUND,
                LEASE_NETWORK_POLICY,
                LEASE_TERMINATE_OBSERVED,
                GUEST_READY_AUTHENTICATED,
            ])
        );

        // The pending slot attests no Ready.
        let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        value["operations"]["guest_channel"] = serde_json::json!({"kind": "unsupported_pending"});
        let pending =
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .unwrap();
        assert_eq!(
            pending.guest_channel().unwrap(),
            GuestChannelProfile::UnsupportedPending
        );
        assert!(
            !pending
                .lease_guarantees()
                .contains(GUEST_READY_AUTHENTICATED)
        );

        // The relayed transport needs a guest activated one-shot with its
        // per-lease key; without that activation the spec is refused.
        let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        value["operations"]["activate_supervisor"] =
            serde_json::json!({"kind": "unsupported_pending"});
        value["operations"]["relay_file"] = serde_json::json!({"kind": "unsupported_pending"});
        assert!(
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );

        // The slot is required, the schema is exact, and no other transport
        // kind or field is accepted.
        let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        value["schema"] = serde_json::json!(7);
        assert!(
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
        let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        value["operations"]
            .as_object_mut()
            .unwrap()
            .remove("guest_channel");
        assert!(
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_err()
        );
        for claimed in [
            serde_json::json!({"kind": "journal_claimed_file_relay",
                "mutation_route": "sandbox_file_upload_token_by_id",
                "observation_route": "sandbox_file_download_token_by_id"}),
            serde_json::json!({"kind": "upload_then_run_once_pending",
                "precondition_profile": "render_qualified_guest_runtime_v1"}),
            serde_json::json!({"kind": "unsupported_pending", "route": "sandbox_by_id"}),
            serde_json::json!({"kind": "controller_relayed_guest_frames",
                "route": "sandbox_by_id"}),
            serde_json::json!({"kind": "controller_relayed_guest_frames",
                "observation_route": "sandbox_run_stream_token_by_id"}),
        ] {
            value["operations"]["guest_channel"] = claimed;
            assert!(
                ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                    .is_err()
            );
        }
    }

    #[test]
    fn spec_rejects_explicit_null_for_omitted_fields() {
        let mut value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        value["operations"]["reconcile_allocation"]["route"] = serde_json::Value::Null;
        let encoded = serde_json::to_vec(&value).unwrap();
        assert!(ProviderSpec::parse(&encoded, SETTINGS_SCHEMA_DIGEST).is_err());
    }

    fn bundle_spec_bytes() -> &'static [u8] {
        include_bytes!("../../../../bundles/render-sandbox/specs/provider-spec.json")
    }

    fn hosts(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn allow_list_creation_is_declared_by_signed_spec_data() {
        // The authored bundle declares allow-list creation of exact HTTPS
        // hostnames; the fixture declares deny-all only.
        let bundle = ProviderSpec::parse(bundle_spec_bytes(), SETTINGS_SCHEMA_DIGEST).unwrap();
        let controller = hosts(&["controller.example.com"]);
        assert_eq!(
            bundle.egress_policy(&controller).unwrap(),
            EgressPolicy {
                policy_type: NetworkPolicyType::AllowList,
                rules: vec![EgressRule {
                    domain: "controller.example.com".into(),
                    protocol: EgressProtocol::Https,
                }],
            }
        );
        assert_eq!(
            bundle.egress_policy(&BTreeSet::new()).unwrap(),
            EgressPolicy::deny_all()
        );
        let projection = bundle
            .create_projection(
                "owner-1",
                PlanValue::Standard,
                "oregon",
                "snp-fixture-1",
                900,
                &controller,
            )
            .unwrap();
        assert_eq!(projection.network_policy.rules.len(), 1);
        // Bootstrap and qualification Sandboxes stay deny-all regardless.
        assert_eq!(
            bundle
                .bootstrap_create_projection("owner-1", PlanValue::Starter, "oregon", 900)
                .unwrap()
                .network_policy,
            EgressPolicy::deny_all()
        );

        let fixture = ProviderSpec::parse(&fixture(), SETTINGS_SCHEMA_DIGEST).unwrap();
        let refusal = format!("{:#}", fixture.egress_policy(&controller).unwrap_err());
        assert!(
            refusal.contains("creatable_types [deny-all] only"),
            "{refusal}"
        );
        assert!(refusal.contains("controller.example.com"), "{refusal}");
        assert!(
            fixture
                .create_projection(
                    "owner-1",
                    PlanValue::Standard,
                    "oregon",
                    "snp-fixture-1",
                    900,
                    &controller,
                )
                .is_err(),
            "a deny-all-only spec never downgrades a requested allow-list"
        );
        assert_eq!(
            fixture.egress_policy(&BTreeSet::new()).unwrap(),
            EgressPolicy::deny_all()
        );
    }

    #[test]
    fn network_policy_declaration_is_closed() {
        let base: serde_json::Value = serde_json::from_slice(bundle_spec_bytes()).unwrap();
        let parses = |change: &dyn Fn(&mut serde_json::Value)| {
            let mut value = base.clone();
            change(&mut value);
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .is_ok()
        };
        assert!(parses(&|_| {}));
        assert!(!parses(&|value| {
            value.as_object_mut().unwrap().remove("network_policy");
        }));
        assert!(!parses(&|value| {
            value["network_policy"]["creatable_types"] = serde_json::json!(["allow-all"]);
        }));
        assert!(!parses(&|value| {
            value["network_policy"]["creatable_types"] =
                serde_json::json!(["deny-all", "allow-list", "allow-all"]);
        }));
        assert!(!parses(&|value| {
            value["network_policy"]["creatable_types"] = serde_json::json!(["allow-list"]);
        }));
        // Allow-list without its bounds, or bounds without allow-list.
        assert!(!parses(&|value| {
            value["network_policy"]
                .as_object_mut()
                .unwrap()
                .remove("allow_list");
        }));
        assert!(!parses(&|value| {
            value["network_policy"]["creatable_types"] = serde_json::json!(["deny-all"]);
        }));
        assert!(!parses(&|value| {
            value["network_policy"]["allow_list"]["rule_protocol"] = serde_json::json!("http");
        }));
        assert!(!parses(&|value| {
            value["network_policy"]["allow_list"]["max_rules"] = serde_json::json!(0);
        }));
        assert!(!parses(&|value| {
            value["network_policy"]["allow_list"]["max_rules"] = serde_json::json!(33);
        }));
        assert!(!parses(&|value| {
            value["network_policy"]["allow_list"]["host_syntax"] = serde_json::json!("any");
        }));
        // The allocation body takes its policy from the lease, and the other
        // creates stay literal deny-all; the deprecated alias is refused.
        assert!(!parses(&|value| {
            value["operations"]["allocate"]["body"]["networkPolicy"] =
                serde_json::json!({"literal": {"type": "deny-all"}});
        }));
        assert!(!parses(&|value| {
            value["operations"]["allocate"]["body"]["networkPolicy"] = serde_json::json!({
                "literal": {"type": "deny-all"}, "source": "lease.guest_network_allow"
            });
        }));
        assert!(!parses(&|value| {
            value["operations"]["bootstrap_create"]["body"]["networkPolicy"] =
                serde_json::json!({"source": "lease.guest_network_allow"});
        }));
        assert!(!parses(&|value| {
            value["operations"]["qualification_create"]["body"]["networkPolicy"] =
                serde_json::json!({"literal": {"type": "allow-list"}});
        }));
        assert!(!parses(&|value| {
            value["operations"]["bootstrap_create"]["body"]["networkPolicy"] =
                serde_json::json!({"literal": {"default": "deny-all"}});
        }));
    }

    #[test]
    fn allow_list_hosts_are_bounded_exact_names() {
        let mut value: serde_json::Value = serde_json::from_slice(bundle_spec_bytes()).unwrap();
        let exact = ProviderSpec::parse(bundle_spec_bytes(), SETTINGS_SCHEMA_DIGEST).unwrap();
        for refused in [
            "https://controller.example.com",
            "controller.example.com:443",
            "controller.example.com/path",
            "Controller.example.com",
            "controller.example.com.",
            "controller",
            "10.0.0.1",
            "-controller.example.com",
            "controller_1.example.com",
            "api.*.example.com",
            "*",
            "*.com",
            "",
        ] {
            assert!(
                exact.egress_policy(&hosts(&[refused])).is_err(),
                "{refused:?} must be refused"
            );
        }
        // Wildcards need the spec's leftmost-wildcard syntax.
        let wildcard = format!(
            "{:#}",
            exact.egress_policy(&hosts(&["*.example.com"])).unwrap_err()
        );
        assert!(wildcard.contains("only as exact_dns_name"), "{wildcard}");
        value["network_policy"]["allow_list"]["host_syntax"] =
            serde_json::json!("exact_dns_name_or_leftmost_wildcard");
        let wide =
            ProviderSpec::parse(&serde_json::to_vec(&value).unwrap(), SETTINGS_SCHEMA_DIGEST)
                .unwrap();
        assert!(wide.egress_policy(&hosts(&["*.example.com"])).is_ok());
        assert!(wide.egress_policy(&hosts(&["api.*.example.com"])).is_err());
        assert!(wide.egress_policy(&hosts(&["*.*.example.com"])).is_err());

        // The rule count is bounded by the signed spec.
        let five = hosts(&[
            "a.example.com",
            "b.example.com",
            "c.example.com",
            "d.example.com",
            "e.example.com",
        ]);
        let bounded = format!("{:#}", exact.egress_policy(&five).unwrap_err());
        assert!(bounded.contains("at most 4 rules"), "{bounded}");
        assert_eq!(
            exact
                .egress_policy(&hosts(&[
                    "d.example.com",
                    "a.example.com",
                    "c.example.com",
                    "b.example.com"
                ]))
                .unwrap()
                .rules
                .iter()
                .map(|rule| rule.domain.as_str())
                .collect::<Vec<_>>(),
            vec![
                "a.example.com",
                "b.example.com",
                "c.example.com",
                "d.example.com"
            ],
            "rules are requested in ascending order"
        );
        let long_label = format!("{}.example.com", "a".repeat(64));
        assert!(exact.egress_policy(&hosts(&[&long_label])).is_err());
        let long_name = format!("{}example.com", "abcdefghi.".repeat(25));
        assert!(long_name.len() > 253);
        assert!(exact.egress_policy(&hosts(&[&long_name])).is_err());
    }
}
