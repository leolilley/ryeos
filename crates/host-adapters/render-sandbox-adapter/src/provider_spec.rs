//! Closed interpreter input for the Render reference lifecycle provider.
//!
//! The serialized format contains only fixed route names, field projections,
//! operation kinds, and code-reviewed proof profile identifiers. It cannot
//! introduce a URL, HTTP method, expression, script, status code, or capability.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::{
    LifecycleCapability, MAX_LIFECYCLE_PROVIDER_SPEC_BYTES, from_json_slice_strict,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderSpec {
    schema: u32,
    provider_profile: ProviderProfile,
    settings_schema_digest: String,
    origin_profile: OriginProfile,
    routes: Routes,
    operations: Operations,
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
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum RouteLiteral {
    #[serde(rename = "v1")]
    V1,
    #[serde(rename = "sandboxes")]
    Sandboxes,
    #[serde(rename = "terminate")]
    Terminate,
}

impl RouteLiteral {
    fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::Sandboxes => "sandboxes",
            Self::Terminate => "terminate",
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
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LifetimeSource {
    source: LifetimeFieldSource,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum NetworkPolicyDefault {
    DenyAll,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct NetworkPolicyValue {
    default: NetworkPolicyDefault,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct NetworkPolicyLiteral {
    literal: NetworkPolicyValue,
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
    network_policy: NetworkPolicyLiteral,
    #[serde(rename = "snapshotId")]
    snapshot_id: StringSource,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PreconditionProfile {
    RenderBaseSnapshotMatchV1,
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
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OperationKind {
    CreateOnce,
    UnsupportedPending,
    TerminateOnceThenObserveExact,
    ObserveExactOccurrence,
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
    reconcile_allocation: OperationSpec,
    activate_supervisor: OperationSpec,
    reconcile_supervisor_activation: OperationSpec,
    pub(crate) terminate: OperationSpec,
    pub(crate) reconcile_termination: OperationSpec,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Routes {
    sandbox_collection: RouteSpec,
    sandbox_by_id: RouteSpec,
    sandbox_terminate_by_id: RouteSpec,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RouteTarget {
    pub(crate) path_segments: Vec<String>,
    pub(crate) owner_id_query: Option<String>,
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
    pub(crate) network_policy_default: NetworkPolicyDefault,
    pub(crate) snapshot_id: String,
}

impl ProviderSpec {
    pub(crate) fn parse(bytes: &[u8], settings_schema_digest: &str) -> Result<Self> {
        ensure!(
            !bytes.is_empty()
                && bytes.len()
                    <= usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES).unwrap_or(usize::MAX),
            "provider spec exceeds its signed byte bound"
        );
        let spec: Self = from_json_slice_strict(
            bytes,
            usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES).unwrap_or(usize::MAX),
        )?;
        spec.validate(settings_schema_digest)?;
        Ok(spec)
    }

    fn validate(&self, settings_schema_digest: &str) -> Result<()> {
        ensure!(
            self.schema == 1
                && self.provider_profile == ProviderProfile::RenderSandboxV1
                && self.origin_profile == OriginProfile::RenderApiV1
                && self.settings_schema_digest == settings_schema_digest,
            "provider spec identity or profile is unsupported"
        );
        ensure!(
            route_matches(
                &self.routes.sandbox_collection,
                &[
                    RouteSegmentExpectation::Literal(RouteLiteral::V1),
                    RouteSegmentExpectation::Literal(RouteLiteral::Sandboxes)
                ],
                false,
            ) && route_matches(
                &self.routes.sandbox_by_id,
                &[
                    RouteSegmentExpectation::Literal(RouteLiteral::V1),
                    RouteSegmentExpectation::Literal(RouteLiteral::Sandboxes),
                    RouteSegmentExpectation::Bound(RouteBinding::OccurrenceId)
                ],
                true,
            ) && route_matches(
                &self.routes.sandbox_terminate_by_id,
                &[
                    RouteSegmentExpectation::Literal(RouteLiteral::V1),
                    RouteSegmentExpectation::Literal(RouteLiteral::Sandboxes),
                    RouteSegmentExpectation::Bound(RouteBinding::OccurrenceId),
                    RouteSegmentExpectation::Literal(RouteLiteral::Terminate)
                ],
                true,
            ),
            "provider spec route profile is unsupported"
        );
        self.validate_operations()
    }

    fn validate_operations(&self) -> Result<()> {
        let allocate = &self.operations.allocate;
        ensure!(
            allocate.kind == OperationKind::CreateOnce
                && allocate.route == Some(RouteName::SandboxCollection)
                && allocate.precondition_profile
                    == Some(PreconditionProfile::RenderBaseSnapshotMatchV1)
                && allocate.body.as_ref() == Some(&expected_create_body_mapping())
                && allocate.bind_proof_profile == Some(BindProofProfile::RenderSandboxCreate201V1)
                && allocate.no_occurrence_proof_profile
                    == Some(NoOccurrenceProofProfile::RenderNoRequestSentV1)
                && allocate.mutation_route.is_none()
                && allocate.observation_route.is_none()
                && allocate.terminal_proof_profile.is_none(),
            "provider spec allocation operation is unsupported"
        );
        ensure!(
            self.operations.reconcile_allocation.is_empty_pending()
                && self.operations.activate_supervisor.is_empty_pending()
                && self
                    .operations
                    .reconcile_supervisor_activation
                    .is_empty_pending(),
            "provider spec reconciliation or activation operation is unsupported"
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
        capabilities
    }

    pub(crate) fn api_base(&self) -> &'static str {
        match self.origin_profile {
            OriginProfile::RenderApiV1 => "https://api.render.com/",
        }
    }

    pub(crate) fn allocation_route(&self) -> Option<RouteName> {
        self.operations.allocate.route
    }

    pub(crate) fn allocation_requires_snapshot_match(&self) -> bool {
        self.operations.allocate.precondition_profile.is_some()
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
    ) -> Result<RouteTarget> {
        let route = match name {
            RouteName::SandboxCollection => &self.routes.sandbox_collection,
            RouteName::SandboxById => &self.routes.sandbox_by_id,
            RouteName::SandboxTerminateById => &self.routes.sandbox_terminate_by_id,
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
        Ok(RouteTarget {
            path_segments,
            owner_id_query,
        })
    }

    pub(crate) fn create_projection(
        &self,
        settings_owner_id: &str,
        settings_plan: PlanValue,
        settings_region: &str,
        settings_snapshot_id: &str,
        reservation_lifetime_seconds: u32,
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
        let snapshot_id = match mapping.snapshot_id.source {
            StringFieldSource::SettingsSnapshotId => settings_snapshot_id,
            _ => anyhow::bail!("provider spec snapshot projection is unsupported"),
        };
        let plan = match mapping.plan.source {
            PlanFieldSource::SettingsPlan => settings_plan,
        };
        let timeout_seconds = match mapping.timeout_seconds.source {
            LifetimeFieldSource::ReservationMaximumLifetimeSeconds => reservation_lifetime_seconds,
        };
        Ok(CreateProjection {
            owner_id: owner_id.to_owned(),
            plan,
            region: region.to_owned(),
            timeout_seconds,
            network_policy_default: mapping.network_policy.literal.default,
            snapshot_id: snapshot_id.to_owned(),
        })
    }
}

impl OperationSpec {
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

fn route_matches(
    route: &RouteSpec,
    expected_segments: &[RouteSegmentExpectation],
    owner_id_query: bool,
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
        && match (&route.query, owner_id_query) {
            (Some(query), true) => query.owner_id.source == StringFieldSource::SettingsOwnerId,
            (None, false) => true,
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
        network_policy: NetworkPolicyLiteral {
            literal: NetworkPolicyValue {
                default: NetworkPolicyDefault::DenyAll,
            },
        },
        snapshot_id: StringSource {
            source: StringFieldSource::SettingsSnapshotId,
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS_SCHEMA_DIGEST: &str =
        "12659bfc0c054e6192b3b42948f3e1c14d96dc9fe2bbc58856a42859eee97658";

    fn fixture() -> Vec<u8> {
        include_bytes!("../fixtures/provider-spec.json").to_vec()
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
        assert_eq!(
            spec.route_target(RouteName::SandboxById, Some("sbx-fixture-1"), "owner-1")
                .unwrap(),
            RouteTarget {
                path_segments: vec!["v1".into(), "sandboxes".into(), "sbx-fixture-1".into()],
                owner_id_query: Some("owner-1".into()),
            }
        );
        assert_eq!(spec.api_base(), "https://api.render.com/");
    }

    #[test]
    fn spec_rejects_unknown_configuration_and_unreviewed_routes() {
        let fixture = String::from_utf8(fixture()).unwrap();
        let with_capability_claim = fixture.replacen(
            "\"schema\": 1,",
            "\"schema\": 1,\n  \"capabilities\": [\"exact_terminal_observation\"],",
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
            )
            .unwrap();
        assert_eq!(projection.owner_id, "owner-1");
        assert_eq!(projection.plan, PlanValue::Standard);
        assert_eq!(projection.region, "oregon");
        assert_eq!(projection.timeout_seconds, 900);
        assert_eq!(
            projection.network_policy_default,
            NetworkPolicyDefault::DenyAll
        );
        assert_eq!(projection.snapshot_id, "snp-fixture-1");
    }

    #[test]
    fn spec_rejects_explicit_null_for_omitted_fields() {
        let mut value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        value["operations"]["reconcile_allocation"]["route"] = serde_json::Value::Null;
        let encoded = serde_json::to_vec(&value).unwrap();
        assert!(ProviderSpec::parse(&encoded, SETTINGS_SCHEMA_DIGEST).is_err());
    }
}
