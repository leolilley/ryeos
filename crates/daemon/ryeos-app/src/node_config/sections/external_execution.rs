//! Node-owned external placement configuration. Signed configuration selects
//! authority; it is not evidence that a backend can safely execute it.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
#[cfg(any(test, feature = "test-support"))]
use ryeos_state::external_execution::transport::ExternalControllerTransportContract;
use ryeos_state::external_execution::transport::{
    ExternalControllerTransportSelection, ExternalNetworkInputPolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{
    CompiledNodeConfigItem, NodeConfigSection, NodeConfigSourceScope, NodeItemContext,
    SectionCardinality, SectionLoadPhase, SectionLoadSpec, SectionSignerPolicy, SectionTraversal,
};

pub mod compose;
pub mod compose_qualification;
pub mod provision;

pub const SECTION_NAME: &str = "external_execution";
pub struct ExternalExecutionSection;

/// Workload-specific authority in one node-signed placement binding. Direct
/// executable identity is supplied by its ordinary admitted Tool closure, not
/// by a provider declaration or a synthetic candidate-runtime selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExternalWorkloadBinding {
    StructuredSession(ExternalStructuredSessionBinding),
    QualificationStructuredSession(ExternalQualificationStructuredSessionBinding),
    DirectCommand {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalStructuredSessionBinding {
    pub(crate) provider_declaration_id: String,
    pub(crate) provider_configuration_destination: String,
    pub(crate) runtime_manifest_hash: String,
    pub(crate) runtime_selection_identity: String,
    pub(crate) configuration_adapter_artifact_hash: String,
    pub(crate) configuration_adapter_artifact_bytes: u64,
    pub(crate) connector_protocol: String,
    pub(crate) connector_artifact_hash: String,
    pub(crate) connector_artifact_bytes: u64,
}

/// A signed, zero-export placement choice for proving one captured runtime
/// through its Worker recipe before that runtime has a qualification. The
/// accepted root and current occurrence supply the use and contact authority;
/// this binding only selects exact executable and Worker identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalQualificationStructuredSessionBinding {
    pub(crate) provider_declaration_id: String,
    pub(crate) provider_configuration_destination: String,
    pub(crate) requirement_digest: String,
    pub(crate) worker_profile_hash: String,
    pub(crate) runtime_manifest_hash: String,
    pub(crate) runtime_member_sha256: String,
    pub(crate) configuration_adapter_artifact_hash: String,
    pub(crate) configuration_adapter_artifact_bytes: u64,
    pub(crate) connector_protocol: String,
    pub(crate) connector_artifact_hash: String,
    pub(crate) connector_artifact_bytes: u64,
}

/// Shared signed artifact coordinates for the two distinct session lanes.
/// Workload admission still checks the exact ordinary or qualification
/// program separately before these executables may be used.
pub(crate) struct ExternalSessionArtifactSelection<'a> {
    pub(crate) provider_declaration_id: &'a str,
    pub(crate) provider_configuration_destination: &'a str,
    pub(crate) configuration_adapter_artifact_hash: &'a str,
    pub(crate) configuration_adapter_artifact_bytes: u64,
    pub(crate) connector_protocol: &'a str,
    pub(crate) connector_artifact_hash: &'a str,
    pub(crate) connector_artifact_bytes: u64,
}

impl ExternalWorkloadBinding {
    pub(crate) fn structured_session(&self) -> Result<&ExternalStructuredSessionBinding> {
        match self {
            Self::StructuredSession(session) => Ok(session),
            Self::QualificationStructuredSession(_) => anyhow::bail!(
                "qualification-only binding cannot authorize an ordinary structured session"
            ),
            Self::DirectCommand {} => anyhow::bail!(
                "external direct-command binding cannot authorize a structured session"
            ),
        }
    }

    pub(crate) fn qualification_structured_session(
        &self,
    ) -> Result<&ExternalQualificationStructuredSessionBinding> {
        match self {
            Self::QualificationStructuredSession(session) => Ok(session),
            _ => anyhow::bail!("external binding has no qualification-only session authority"),
        }
    }

    pub(crate) fn session_artifacts(&self) -> Result<ExternalSessionArtifactSelection<'_>> {
        let selection = match self {
            Self::StructuredSession(session) => ExternalSessionArtifactSelection {
                provider_declaration_id: &session.provider_declaration_id,
                provider_configuration_destination: &session.provider_configuration_destination,
                configuration_adapter_artifact_hash: &session.configuration_adapter_artifact_hash,
                configuration_adapter_artifact_bytes: session.configuration_adapter_artifact_bytes,
                connector_protocol: &session.connector_protocol,
                connector_artifact_hash: &session.connector_artifact_hash,
                connector_artifact_bytes: session.connector_artifact_bytes,
            },
            Self::QualificationStructuredSession(session) => ExternalSessionArtifactSelection {
                provider_declaration_id: &session.provider_declaration_id,
                provider_configuration_destination: &session.provider_configuration_destination,
                configuration_adapter_artifact_hash: &session.configuration_adapter_artifact_hash,
                configuration_adapter_artifact_bytes: session.configuration_adapter_artifact_bytes,
                connector_protocol: &session.connector_protocol,
                connector_artifact_hash: &session.connector_artifact_hash,
                connector_artifact_bytes: session.connector_artifact_bytes,
            },
            Self::DirectCommand {} => {
                anyhow::bail!("external direct command has no session artifact selection")
            }
        };
        Ok(selection)
    }

    fn validate(&self) -> Result<()> {
        let (
            provider_declaration_id,
            provider_configuration_destination,
            runtime_manifest_hash,
            configuration_adapter_artifact_hash,
            configuration_adapter_artifact_bytes,
            connector_protocol,
            connector_artifact_hash,
            connector_artifact_bytes,
        ) = match self {
            Self::StructuredSession(session) => (
                &session.provider_declaration_id,
                &session.provider_configuration_destination,
                &session.runtime_manifest_hash,
                &session.configuration_adapter_artifact_hash,
                session.configuration_adapter_artifact_bytes,
                &session.connector_protocol,
                &session.connector_artifact_hash,
                session.connector_artifact_bytes,
            ),
            Self::QualificationStructuredSession(session) => {
                validate_content_identity(&session.requirement_digest)?;
                validate_content_identity(&session.worker_profile_hash)?;
                validate_content_identity(&session.runtime_member_sha256)?;
                (
                    &session.provider_declaration_id,
                    &session.provider_configuration_destination,
                    &session.runtime_manifest_hash,
                    &session.configuration_adapter_artifact_hash,
                    session.configuration_adapter_artifact_bytes,
                    &session.connector_protocol,
                    &session.connector_artifact_hash,
                    session.connector_artifact_bytes,
                )
            }
            Self::DirectCommand {} => return Ok(()),
        };
        ryeos_state::external_content::products::validate_name(provider_declaration_id)?;
        ryeos_state::objects::validate_session_configuration_destination(
            provider_configuration_destination,
        )?;
        for value in [
            runtime_manifest_hash,
            configuration_adapter_artifact_hash,
            connector_artifact_hash,
        ] {
            validate_content_identity(value)?;
        }
        if let Self::StructuredSession(session) = self {
            validate_content_identity(&session.runtime_selection_identity)?;
        }
        for bytes in [
            configuration_adapter_artifact_bytes,
            connector_artifact_bytes,
        ] {
            validate_artifact_bytes(bytes)?;
        }
        ensure!(
            connector_protocol.as_str()
                == ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL,
            "placement binding requires the exact bounded connector contract"
        );
        Ok(())
    }
}

fn validate_content_identity(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "placement binding requires canonical content identities"
    );
    Ok(())
}

fn validate_artifact_bytes(bytes: u64) -> Result<()> {
    ensure!(
        (1..=1024 * 1024 * 1024).contains(&bytes),
        "placement binding requires bounded exact artifact sizes"
    );
    Ok(())
}

/// Signed placement selection of one independently published product proof.
/// It deliberately does not contain the proof itself or grant contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalRuntimeQualificationBinding {
    pub(crate) attestation_hash: String,
    pub(crate) owner_principal: String,
    pub(crate) qualification:
        ryeos_state::external_content::products::composition::ProductRelationshipQualification,
    /// Required nullable. `Some` selects a remote qualification testimony
    /// accepted under local policy: `attestation_hash` is then the origin's
    /// qualification attestation hash and `owner_principal` the attesting
    /// operator. `None` is the local qualification path, unchanged.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(crate) remote_testimony: Option<ExternalRemoteQualificationTestimonyBinding>,
}

/// Names one local acceptance of a remote testimony and the local policy it
/// was accepted under. It selects testimony; it never grants launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalRemoteQualificationTestimonyBinding {
    pub(crate) acceptance_hash: String,
    pub(crate) acceptance_policy_ref: String,
}

impl ExternalRemoteQualificationTestimonyBinding {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_content_identity(&self.acceptance_hash)?;
        let reference =
            ryeos_engine::canonical_ref::CanonicalRef::parse(&self.acceptance_policy_ref)?;
        ensure!(
            reference.kind == "config"
                && reference.to_string() == self.acceptance_policy_ref
                && !self.acceptance_policy_ref.contains('@')
                && self.acceptance_policy_ref.len() <= 2048,
            "remote qualification testimony requires an exact acceptance policy config ref"
        );
        Ok(())
    }
}

impl ExternalRuntimeQualificationBinding {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_content_identity(&self.attestation_hash)?;
        let owner = self
            .owner_principal
            .strip_prefix("fp:")
            .context("external runtime qualification owner must be a fingerprint principal")?;
        validate_content_identity(owner)?;
        self.qualification.validate()?;
        ensure!(
            self.qualification.policy_ref.is_some(),
            "external runtime qualification requires a signed policy"
        );
        if let Some(remote) = &self.remote_testimony {
            remote.validate()?;
        }
        Ok(())
    }
}

/// Acquired structured-session runtime compatibility, distinct from the
/// guest-owner snapshot testimony above. This selects proof, never grants it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalRuntimeContentQualificationBinding {
    pub(crate) activation_ref: String,
    pub(crate) coordinate_id: String,
    pub(crate) attestation_hash: String,
    pub(crate) owner_principal: String,
    pub(crate) required_claims: Vec<String>,
    /// Required nullable. `Some` selects a remote content testimony accepted
    /// under local policy: `coordinate_id` and `attestation_hash` are then the
    /// origin's, `owner_principal` is the attesting operator and
    /// `activation_ref` names the content on both nodes. `None` is the local
    /// qualification path, unchanged.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(crate) remote_testimony: Option<ExternalRemoteQualificationTestimonyBinding>,
}

impl ExternalRuntimeContentQualificationBinding {
    pub(crate) fn validate(&self) -> Result<()> {
        let reference = ryeos_engine::canonical_ref::CanonicalRef::parse(&self.activation_ref)?;
        ensure!(
            reference.kind == "config"
                && reference.to_string() == self.activation_ref
                && !self.activation_ref.contains('@')
                && self.activation_ref.len() <= 2048,
            "runtime content qualification requires an exact activation config ref"
        );
        validate_content_identity(&self.coordinate_id)?;
        validate_content_identity(&self.attestation_hash)?;
        validate_content_identity(
            self.owner_principal
                .strip_prefix("fp:")
                .context("runtime content qualification owner must be a fingerprint principal")?,
        )?;
        ensure!(!self.required_claims.is_empty()
            && self.required_claims.len() <= ryeos_state::external_content::products::qualification::MAX_PRODUCT_QUALIFICATION_CLAIMS
            && self.required_claims.windows(2).all(|pair| pair[0] < pair[1]),
            "runtime content qualification requires bounded sorted compatibility claims");
        for claim in &self.required_claims {
            ryeos_state::external_content::products::validate_name(claim)?;
        }
        if let Some(remote) = &self.remote_testimony {
            remote.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingDocument {
    kind: String,
    schema: u32,
    protocol: String,
    workload: ExternalWorkloadBinding,
    backend: String,
    account: String,
    capacity_group: String,
    credential_generation: String,
    settings_schema_digest: String,
    settings_digest: String,
    settings: Value,
    backend_artifact_hash: String,
    backend_artifact_bytes: u64,
    supervisor_artifact_hash: String,
    supervisor_artifact_bytes: u64,
    /// Expected independently qualified guest-owner runtime, not the
    /// candidate's structured-session runtime or a provider snapshot ID.
    guest_runtime_manifest_hash: String,
    /// Null is explicit for backends without an installed runtime witness;
    /// presence alone never grants activation or provider contact.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    runtime_qualification: Option<ExternalRuntimeQualificationBinding>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    runtime_content_qualification: Option<ExternalRuntimeContentQualificationBinding>,
    launcher_artifact_hash: String,
    launcher_artifact_bytes: u64,
    network_policy: String,
    storage_policy: String,
    cleanup_proof: String,
    /// Host adapter contact inputs are signed independently of the guest's
    /// controller transport. A file-relay guest has no network authority.
    provider_network_inputs: ExternalNetworkInputPolicy,
    controller_transport: ExternalControllerTransportSelection,
    controller_tls_root_certificates_der_base64: Vec<String>,
    max_active: u16,
    timeout_seconds: u32,
    contact_timeout_seconds: u32,
    observation_timeout_seconds: u32,
    cleanup_timeout_seconds: u32,
    max_workspace_bytes: u64,
    max_export_bytes: u64,
    max_transfer_bytes: u64,
    max_guest_package_regular_bytes: u64,
    max_guest_package_framed_bytes: u64,
}

pub(crate) fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl BindingDocument {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.kind == "node"
                && self.schema
                    == match &self.controller_transport {
                        ExternalControllerTransportSelection::HttpJson(_) => 17,
                        ExternalControllerTransportSelection::ControllerFileRelay(_) =>
                            match &self.workload {
                                ExternalWorkloadBinding::QualificationStructuredSession(_) => 18,
                                ExternalWorkloadBinding::StructuredSession(_) => 19,
                                ExternalWorkloadBinding::DirectCommand {} => 0,
                            },
                    },
            "unsupported external placement binding schema for selected transport"
        );
        self.workload.validate()?;
        ensure!(
            self.protocol == ryeos_state::external_execution::admission::PROTOCOL,
            "unsupported external placement protocol"
        );
        for value in [&self.backend, &self.account, &self.capacity_group] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 128
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
                "placement backend/account must be bounded identifiers, not URLs or credentials"
            );
        }
        for value in [
            &self.credential_generation,
            &self.settings_schema_digest,
            &self.settings_digest,
            &self.backend_artifact_hash,
            &self.supervisor_artifact_hash,
            &self.guest_runtime_manifest_hash,
            &self.launcher_artifact_hash,
        ] {
            validate_content_identity(value)?;
        }
        if let Some(qualification) = &self.runtime_qualification {
            qualification.validate()?;
        }
        if let Some(qualification) = &self.runtime_content_qualification {
            qualification.validate()?;
            self.workload.structured_session()?;
        }
        ensure!(
            self.settings.is_object()
                && lillux::canonical_json(&self.settings)?.len() <= 64 * 1024
                && self.settings_digest == adapter_settings_digest(&self.settings)?,
            "placement adapter settings changed their signed identity"
        );
        for bytes in [
            self.backend_artifact_bytes,
            self.supervisor_artifact_bytes,
            self.launcher_artifact_bytes,
        ] {
            validate_artifact_bytes(bytes)?;
        }
        ensure!(
            (1..=64).contains(&self.max_active) && (1..=3600).contains(&self.timeout_seconds),
            "placement binding limits exceed allocation bounds"
        );
        ensure!(
            (1..=60).contains(&self.contact_timeout_seconds)
                && (1..=300).contains(&self.observation_timeout_seconds)
                && (1..=600).contains(&self.cleanup_timeout_seconds),
            "placement binding lifecycle deadlines exceed bounds"
        );
        ensure!(
            self.storage_policy == "ephemeral_private_candidate_v1"
                && self.cleanup_proof == "provider_terminal_occurrence_v1",
            "placement binding requests an unsupported lifecycle contract"
        );
        const MAX_BYTES: u64 = 1 << 40;
        ensure!(
            (1..=MAX_BYTES).contains(&self.max_workspace_bytes)
                && (1..=MAX_BYTES).contains(&self.max_transfer_bytes),
            "placement binding storage or transfer budgets exceed bounds"
        );
        const MAX_GUEST_PACKAGE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
        ensure!(
            (1..=MAX_GUEST_PACKAGE_BYTES).contains(&self.max_guest_package_regular_bytes)
                && (1..=MAX_GUEST_PACKAGE_BYTES).contains(&self.max_guest_package_framed_bytes)
                && self.max_guest_package_regular_bytes < self.max_guest_package_framed_bytes,
            "placement binding guest package budgets exceed the upload bound"
        );
        match &self.workload {
            ExternalWorkloadBinding::StructuredSession(_) => ensure!(
                (1..=self.max_workspace_bytes).contains(&self.max_export_bytes)
                    && self.max_transfer_bytes >= self.max_export_bytes,
                "structured-session placement export budgets exceed bounds"
            ),
            ExternalWorkloadBinding::QualificationStructuredSession(_) => ensure!(
                self.max_export_bytes == 0
                    && self.runtime_qualification.is_none()
                    && self.runtime_content_qualification.is_none(),
                "qualification-only placement cannot authorize export or borrow ordinary runtime qualification"
            ),
            ExternalWorkloadBinding::DirectCommand {} => ensure!(
                self.max_export_bytes == 0,
                "direct-command placement cannot authorize candidate export"
            ),
        }
        self.controller_transport.validate()?;
        self.provider_network_inputs.validate()?;
        match &self.controller_transport {
            ExternalControllerTransportSelection::HttpJson(http) => ensure!(
                self.network_policy == "supervisor_pinned_owner_only_candidate_denied_v1"
                    && ryeos_state::external_execution::transport::external_tls_root_bundle_digest(
                        &self.controller_tls_root_certificates_der_base64,
                    )? == http.tls_root_bundle_digest,
                "external controller TLS roots changed their signed identity"
            ),
            ExternalControllerTransportSelection::ControllerFileRelay(relay) => ensure!(
                matches!(
                    &self.workload,
                    ExternalWorkloadBinding::QualificationStructuredSession(_)
                        | ExternalWorkloadBinding::StructuredSession(_)
                ) && self.network_policy == "controller_file_relay_guest_deny_all_v1"
                    && self.backend == "render-sandbox-early-access"
                    && self.controller_tls_root_certificates_der_base64.is_empty()
                    && self.settings.get("owner_id").and_then(Value::as_str)
                        == Some(relay.owner_workspace_id.as_str()),
                "external controller file relay requires signed Render Q owner and no TLS roots"
            ),
        }
        Ok(())
    }

    fn backend_contract(&self) -> ExternalPlacementBackendContract {
        ExternalPlacementBackendContract {
            workload: self.workload.clone(),
            backend: self.backend.clone(),
            account: self.account.clone(),
            capacity_group: self.capacity_group.clone(),
            settings_schema_digest: self.settings_schema_digest.clone(),
            settings_digest: self.settings_digest.clone(),
            settings: self.settings.clone(),
            backend_artifact_hash: self.backend_artifact_hash.clone(),
            backend_artifact_bytes: self.backend_artifact_bytes,
            supervisor_artifact_hash: self.supervisor_artifact_hash.clone(),
            supervisor_artifact_bytes: self.supervisor_artifact_bytes,
            guest_runtime_manifest_hash: self.guest_runtime_manifest_hash.clone(),
            runtime_qualification: self.runtime_qualification.clone(),
            runtime_content_qualification: self.runtime_content_qualification.clone(),
            launcher_artifact_hash: self.launcher_artifact_hash.clone(),
            launcher_artifact_bytes: self.launcher_artifact_bytes,
            network_policy: self.network_policy.clone(),
            storage_policy: self.storage_policy.clone(),
            cleanup_proof: self.cleanup_proof.clone(),
            provider_network_inputs: self.provider_network_inputs.clone(),
            controller_transport: self.controller_transport.clone(),
            controller_tls_root_certificates_der_base64: self
                .controller_tls_root_certificates_der_base64
                .clone(),
            max_active: self.max_active,
            timeout_seconds: self.timeout_seconds,
            contact_timeout_seconds: self.contact_timeout_seconds,
            observation_timeout_seconds: self.observation_timeout_seconds,
            cleanup_timeout_seconds: self.cleanup_timeout_seconds,
            max_workspace_bytes: self.max_workspace_bytes,
            max_export_bytes: self.max_export_bytes,
            max_transfer_bytes: self.max_transfer_bytes,
            max_guest_package_regular_bytes: self.max_guest_package_regular_bytes,
            max_guest_package_framed_bytes: self.max_guest_package_framed_bytes,
        }
    }
}

/// Exact non-secret adapter contract projected from the signed binding. A
/// backend may narrow implementation behavior but may not replace any field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ExternalPlacementBackendContract {
    pub(crate) workload: ExternalWorkloadBinding,
    pub(crate) backend: String,
    pub(crate) account: String,
    pub(crate) capacity_group: String,
    pub(crate) settings_schema_digest: String,
    pub(crate) settings_digest: String,
    pub(crate) settings: Value,
    pub(crate) backend_artifact_hash: String,
    pub(crate) backend_artifact_bytes: u64,
    pub(crate) supervisor_artifact_hash: String,
    pub(crate) supervisor_artifact_bytes: u64,
    pub(crate) guest_runtime_manifest_hash: String,
    pub(crate) runtime_qualification: Option<ExternalRuntimeQualificationBinding>,
    pub(crate) runtime_content_qualification: Option<ExternalRuntimeContentQualificationBinding>,
    pub(crate) launcher_artifact_hash: String,
    pub(crate) launcher_artifact_bytes: u64,
    pub(crate) network_policy: String,
    pub(crate) storage_policy: String,
    pub(crate) cleanup_proof: String,
    pub(crate) provider_network_inputs: ExternalNetworkInputPolicy,
    pub(crate) controller_transport: ExternalControllerTransportSelection,
    pub(crate) controller_tls_root_certificates_der_base64: Vec<String>,
    pub(crate) max_active: u16,
    pub(crate) timeout_seconds: u32,
    pub(crate) contact_timeout_seconds: u32,
    pub(crate) observation_timeout_seconds: u32,
    pub(crate) cleanup_timeout_seconds: u32,
    pub(crate) max_workspace_bytes: u64,
    pub(crate) max_export_bytes: u64,
    pub(crate) max_transfer_bytes: u64,
    pub(crate) max_guest_package_regular_bytes: u64,
    pub(crate) max_guest_package_framed_bytes: u64,
}

impl ExternalPlacementBackendContract {
    /// Ordinary commands have no session readiness contract. Their one startup
    /// window is the existing signed allocation-contact plus attachment-
    /// observation horizon, anchored at the first reservation, never renewed
    /// by polling or by starting the supervisor. Execution has its own bound.
    pub(crate) fn direct_startup_budget_ms(&self) -> Result<u64> {
        ensure!(
            matches!(self.workload, ExternalWorkloadBinding::DirectCommand {}),
            "direct startup budget requires an ordinary-command binding"
        );
        u64::from(self.contact_timeout_seconds)
            .checked_add(u64::from(self.observation_timeout_seconds))
            .and_then(|seconds| seconds.checked_mul(1_000))
            .context("external direct startup budget overflow")
    }
}

fn binding_digest(id: &str, signer: &str, signed_source: &str) -> Result<String> {
    let source_hash = lillux::cas::sha256_hex(signed_source.as_bytes());
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.external-placement-binding.v1", "id": id,
        "signer": signer, "signed_source": source_hash,
    }))
}

pub(crate) fn adapter_settings_digest(settings: &Value) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(settings)?.as_bytes(),
    ))
}

fn capacity_owner(signer: &str, document: &BindingDocument) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.external-placement-capacity.v1", "node": signer,
        "backend": document.backend, "account": document.account,
        "capacity_group": document.capacity_group,
    }))
}

/// Private recovery authority retained before allocation contact. It contains
/// no credential plaintext, provider URL or occurrence token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetainedExternalExecutionBinding {
    schema: u32,
    id: String,
    document: BindingDocument,
    signed_source: String,
    signer: String,
    signer_verifying_key: [u8; 32],
    digest: String,
    capacity_owner: String,
}

impl RetainedExternalExecutionBinding {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 2,
            "unsupported retained placement binding schema"
        );
        ensure!(
            !self.id.is_empty() && self.id.len() <= 128,
            "retained placement binding id is invalid"
        );
        ensure!(
            self.signer.len() == 64
                && self
                    .signer
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "retained placement signer is invalid"
        );
        ensure!(
            !self.signed_source.is_empty()
                && self.signed_source.len()
                    <= ryeos_node_runtime::node_document::MAX_ITEM_BYTES as usize,
            "retained placement signed source is invalid"
        );
        self.document.validate()?;
        let verifying_key = lillux::crypto::VerifyingKey::from_bytes(&self.signer_verifying_key)
            .context("retained placement signer key is invalid")?;
        ensure!(
            lillux::signature::compute_fingerprint(&verifying_key) == self.signer,
            "retained placement signer key changed"
        );
        let (body, _) = lillux::signature::strip_canonical_signature_with_envelope(
            &self.signed_source,
            "#",
            None,
            false,
        )?;
        let envelope = ryeos_engine::contracts::SignatureEnvelope {
            prefix: "#".into(),
            suffix: None,
            after_shebang: false,
        };
        let header =
            ryeos_engine::item_resolution::parse_signature_header(&self.signed_source, &envelope)
                .context("retained placement source has no canonical signature")?;
        ensure!(
            header.signer_fingerprint == self.signer,
            "retained placement source signer changed"
        );
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: self.signer.clone(),
                verifying_key,
                label: None,
            },
        ]);
        let (class, _) = ryeos_engine::trust::verify_item_signature(
            &self.signed_source,
            &header,
            &envelope,
            &trust,
        )
        .context("verify retained placement signature")?;
        ensure!(
            class == ryeos_engine::contracts::TrustClass::Trusted,
            "retained placement signature is not trusted"
        );
        let decoded: BindingDocument =
            serde_yaml::from_str(&body).context("decode retained placement signed body")?;
        ensure!(
            decoded == self.document,
            "retained placement document contradicts its signed source"
        );
        ensure!(
            self.digest == binding_digest(&self.id, &self.signer, &self.signed_source)?,
            "retained placement binding digest changed"
        );
        ensure!(
            self.capacity_owner == capacity_owner(&self.signer, &self.document)?,
            "retained placement capacity domain changed"
        );
        Ok(())
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn capacity_owner(&self) -> &str {
        &self.capacity_owner
    }
    pub(crate) fn backend_contract(&self) -> ExternalPlacementBackendContract {
        self.document.backend_contract()
    }
    pub(crate) fn credential_access(
        &self,
    ) -> Result<crate::vault::placement::PlacementCredentialAccess> {
        crate::vault::placement::PlacementCredentialAccess::new(
            &self.capacity_owner,
            &self.document.credential_generation,
            &self.document.backend,
            &self.document.account,
        )
    }
    pub(crate) fn check_program(
        &self,
        program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
    ) -> Result<()> {
        check_program(&self.document, program)
    }
    /// Signed selection only. The accepted root, bound occurrence and current
    /// launch owner must be checked again before any provider contact.
    pub(crate) fn check_qualification_program(
        &self,
        program: &ryeos_state::external_execution::qualification_session::AdmittedExternalQualificationSessionProgram,
        purpose: &ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose,
    ) -> Result<()> {
        self.validate()?;
        check_qualification_program(&self.document, program, purpose)
    }
    /// Rejoin a compiled ordinary command to this exact retained generation.
    /// This is a content/limit check, not guest capability evidence or contact
    /// permission; the born-thread owner and backend must still be admitted.
    pub(crate) fn check_direct_program(
        &self,
        program: &ryeos_state::external_execution::admission::AdmittedExternalDirectProgram,
    ) -> Result<()> {
        self.validate()?;
        program.validate()?;
        check_direct_endpoint(&self.document, &self.id, &self.digest, program.projection())
    }
    pub(crate) fn canonical_json(&self) -> Result<String> {
        self.validate()?;
        Ok(lillux::canonical_json(&serde_json::to_value(self)?)?)
    }
    pub(crate) fn check_reservation_limits(
        &self,
        max_active: u16,
        timeout_seconds: u32,
    ) -> Result<()> {
        ensure!(
            max_active <= self.document.max_active
                && timeout_seconds <= self.document.timeout_seconds,
            "external reservation exceeds its retained placement limits"
        );
        Ok(())
    }

    /// The fixture binding's document as JSON, for a loader test that signs
    /// it as a node item.
    #[cfg(test)]
    pub(crate) fn test_fixture_document_value() -> Value {
        serde_json::to_value(&Self::test_fixture().document).expect("fixture binding document")
    }

    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let controller_tls_root_certificates_der_base64 =
            vec![STANDARD.encode(b"fixture controller TLS root")];
        let provider_network_inputs = ExternalNetworkInputPolicy {
            resolver: ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                source: "/etc/resolv.conf".into(),
                max_bytes: 64 * 1024,
            },
            hosts: ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                source: "/etc/hosts".into(),
                max_bytes: 64 * 1024,
            },
        };
        let document = BindingDocument {
            kind: "node".into(),
            schema: 17,
            protocol: ryeos_state::external_execution::admission::PROTOCOL.into(),
            workload: ExternalWorkloadBinding::StructuredSession(
                ExternalStructuredSessionBinding {
                    provider_declaration_id: "codex-hosted".into(),
                    provider_configuration_destination: "environments.toml".into(),
                    runtime_manifest_hash: "b".repeat(64),
                    runtime_selection_identity: "c".repeat(64),
                    configuration_adapter_artifact_hash: "2".repeat(64),
                    configuration_adapter_artifact_bytes: 4096,
                    connector_protocol:
                        ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
                    connector_artifact_hash: "f".repeat(64),
                    connector_artifact_bytes: 4096,
                },
            ),
            backend: "fixture".into(),
            account: "account".into(),
            capacity_group: "candidate-workers".into(),
            credential_generation: "a".repeat(64),
            settings_schema_digest: "3".repeat(64),
            settings: serde_json::json!({
                "region":"fixture-region", "plan":"fixture-plan"
            }),
            settings_digest: adapter_settings_digest(&serde_json::json!({
                "region":"fixture-region", "plan":"fixture-plan"
            }))
            .unwrap(),
            backend_artifact_hash: "d".repeat(64),
            backend_artifact_bytes: 4096,
            supervisor_artifact_hash: "1".repeat(64),
            supervisor_artifact_bytes: 4096,
            guest_runtime_manifest_hash: "4".repeat(64),
            runtime_qualification: None,
            runtime_content_qualification: None,
            launcher_artifact_hash: "e".repeat(64),
            launcher_artifact_bytes: 4096,
            network_policy: "supervisor_pinned_owner_only_candidate_denied_v1".into(),
            storage_policy: "ephemeral_private_candidate_v1".into(),
            cleanup_proof: "provider_terminal_occurrence_v1".into(),
            provider_network_inputs: provider_network_inputs.clone(),
            controller_transport: ExternalControllerTransportSelection::HttpJson(
                ExternalControllerTransportContract {
                    schema: 2,
                    https_origin: "https://controller.example:7443".into(),
                    route_contract:
                        ryeos_state::external_execution::transport::EXTERNAL_CHANNEL_ROUTE_CONTRACT
                            .into(),
                    tls_root_bundle_digest:
                        ryeos_state::external_execution::transport::external_tls_root_bundle_digest(
                            &controller_tls_root_certificates_der_base64,
                        )
                        .unwrap(),
                    connect_timeout_ms: 5_000,
                    request_timeout_ms: 10_000,
                    maximum_response_bytes: 1024 * 1024,
                    network_inputs: provider_network_inputs,
                },
            ),
            controller_tls_root_certificates_der_base64,
            max_active: 1,
            timeout_seconds: 60,
            contact_timeout_seconds: 30,
            observation_timeout_seconds: 60,
            cleanup_timeout_seconds: 120,
            max_workspace_bytes: 1024,
            max_export_bytes: 512,
            max_transfer_bytes: 2048,
            max_guest_package_regular_bytes: 1024 * 1024,
            max_guest_package_framed_bytes: 2 * 1024 * 1024,
        };
        let id = "fixture".to_owned();
        let key = lillux::crypto::SigningKey::from_bytes(&[37; 32]);
        let signer_verifying_key = key.verifying_key().to_bytes();
        let signer = lillux::signature::compute_fingerprint(&key.verifying_key());
        let body = serde_yaml::to_string(&document).unwrap();
        let signed_source =
            lillux::signature::sign_content_at(&body, &key, "#", None, "2026-09-20T00:00:00Z");
        Self {
            schema: 2,
            digest: binding_digest(&id, &signer, &signed_source).unwrap(),
            capacity_owner: capacity_owner(&signer, &document).unwrap(),
            id,
            document,
            signed_source,
            signer,
            signer_verifying_key,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn composed_test_fixture(
        program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
        controller_transport: ExternalControllerTransportContract,
        controller_tls_root_certificates_der_base64: Vec<String>,
        backend: String,
        backend_artifact_hash: String,
        backend_artifact_bytes: u64,
        settings_schema_digest: String,
        supervisor_artifact_hash: String,
        supervisor_artifact_bytes: u64,
        launcher_artifact_hash: String,
        launcher_artifact_bytes: u64,
        configuration_adapter_artifact_hash: String,
        configuration_adapter_artifact_bytes: u64,
        connector_artifact_hash: String,
        connector_artifact_bytes: u64,
        qualification: &PlacementQualificationSelection,
    ) -> Result<Self> {
        Self::composed_test_fixture_with_settings(
            program,
            controller_transport,
            controller_tls_root_certificates_der_base64,
            backend,
            backend_artifact_hash,
            backend_artifact_bytes,
            settings_schema_digest,
            supervisor_artifact_hash,
            supervisor_artifact_bytes,
            launcher_artifact_hash,
            launcher_artifact_bytes,
            configuration_adapter_artifact_hash,
            configuration_adapter_artifact_bytes,
            connector_artifact_hash,
            connector_artifact_bytes,
            serde_json::json!({"region":"composed-test", "plan":"bounded-fixture"}),
            qualification,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn composed_test_fixture_with_settings(
        program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
        controller_transport: ExternalControllerTransportContract,
        controller_tls_root_certificates_der_base64: Vec<String>,
        backend: String,
        backend_artifact_hash: String,
        backend_artifact_bytes: u64,
        settings_schema_digest: String,
        supervisor_artifact_hash: String,
        supervisor_artifact_bytes: u64,
        launcher_artifact_hash: String,
        launcher_artifact_bytes: u64,
        configuration_adapter_artifact_hash: String,
        configuration_adapter_artifact_bytes: u64,
        connector_artifact_hash: String,
        connector_artifact_bytes: u64,
        settings: Value,
        qualification: &PlacementQualificationSelection,
    ) -> Result<Self> {
        let provider_network_inputs = controller_transport.network_inputs.clone();
        let document = compose::compose_binding_document(compose::ComposeInputs {
            program,
            controller_transport: controller_transport.into(),
            provider_network_inputs,
            tls_roots_der_base64: controller_tls_root_certificates_der_base64,
            backend,
            account: "account".into(),
            capacity_group: compose::DEFAULT_CAPACITY_GROUP.into(),
            credential_generation: "a".repeat(64),
            settings,
            guest_runtime_manifest_hash: qualification.guest_runtime_manifest_hash.clone(),
            runtime_qualification: qualification.guest.as_ref().map(|guest| {
                ExternalRuntimeQualificationBinding {
                    attestation_hash: guest.attestation_hash.clone(),
                    owner_principal: guest.owner_principal.clone(),
                    qualification:
                        ryeos_state::external_content::products::composition::ProductRelationshipQualification {
                            policy_ref: Some(guest.policy_ref.clone()),
                            required_claims: guest.required_claims.clone(),
                        },
                    remote_testimony: None,
                }
            }),
            runtime_content_qualification: qualification.content.as_ref().map(|content| {
                ExternalRuntimeContentQualificationBinding {
                    activation_ref: content.activation_ref.clone(),
                    coordinate_id: content.coordinate_id.clone(),
                    attestation_hash: content.attestation_hash.clone(),
                    owner_principal: content.owner_principal.clone(),
                    required_claims: content.required_claims.clone(),
                    remote_testimony: None,
                }
            }),
            artifacts: compose::BundleArtifactFacts {
                backend_artifact_hash,
                backend_artifact_bytes,
                settings_schema_digest,
                supervisor_artifact_hash,
                supervisor_artifact_bytes,
                launcher_artifact_hash,
                launcher_artifact_bytes,
                configuration_adapter_artifact_hash,
                configuration_adapter_artifact_bytes,
                connector_artifact_hash,
                connector_artifact_bytes,
            },
            limits: compose::ComposeLimits {
                max_active: 1,
                timeout_seconds: 60,
                // The composed native fixture transfers the exact guest
                // runtime through descriptor authority during the one
                // bounded activation contact. Use the contract's maximum
                // contact window so a large admitted runtime is not measured
                // against the smaller unit-fixture default.
                contact_timeout_seconds: 60,
                observation_timeout_seconds: 60,
                cleanup_timeout_seconds: 120,
                max_workspace_bytes: 16 * 1024 * 1024,
                max_export_bytes: 8 * 1024 * 1024,
                max_transfer_bytes: 16 * 1024 * 1024,
                // Debug-built supervisor and launcher fixtures are captured
                // in full, alongside both distinct admitted runtime
                // destinations. Bound their aggregate package without
                // changing production policy.
                max_guest_package_regular_bytes: 768 * 1024 * 1024,
                max_guest_package_framed_bytes: 1024 * 1024 * 1024,
            },
        })?;
        let id = "composed-test".to_owned();
        let key = lillux::crypto::SigningKey::from_bytes(&[37; 32]);
        let signer_verifying_key = key.verifying_key().to_bytes();
        let signer = lillux::signature::compute_fingerprint(&key.verifying_key());
        let body = serde_yaml::to_string(&document)?;
        let signed_source =
            lillux::signature::sign_content_at(&body, &key, "#", None, "2026-09-21T00:00:00Z");
        let fixture = Self {
            schema: 2,
            digest: binding_digest(&id, &signer, &signed_source)?,
            capacity_owner: capacity_owner(&signer, &document)?,
            id,
            document,
            signed_source,
            signer,
            signer_verifying_key,
        };
        fixture.validate()?;
        Ok(fixture)
    }
}

/// Only verified node-config admission constructs this value. No deserialization
/// or public raw-field constructor; no credential reference in Debug output.
#[derive(Clone)]
pub struct InstalledExternalExecutionBinding {
    id: String,
    document: BindingDocument,
    signed_source: Arc<str>,
    signer: String,
    signer_verifying_key: [u8; 32],
    digest: String,
    capacity_owner: String,
}

impl std::fmt::Debug for InstalledExternalExecutionBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstalledExternalExecutionBinding")
            .field("id", &self.id)
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

/// Plain-data selection of the runtime proofs a composed test binding carries.
/// The binding types themselves stay crate-private; a downstream fixture
/// names the published attestations it qualified and nothing else.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub struct PlacementQualificationSelection {
    /// Expected independently qualified guest-owner runtime.
    pub guest_runtime_manifest_hash: String,
    pub guest: Option<GuestRuntimeQualificationSelection>,
    pub content: Option<ContentRuntimeQualificationSelection>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub struct GuestRuntimeQualificationSelection {
    pub attestation_hash: String,
    pub owner_principal: String,
    pub policy_ref: String,
    pub required_claims: Vec<String>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub struct ContentRuntimeQualificationSelection {
    pub activation_ref: String,
    pub coordinate_id: String,
    pub attestation_hash: String,
    pub owner_principal: String,
    pub required_claims: Vec<String>,
}

#[cfg(any(test, feature = "test-support"))]
impl PlacementQualificationSelection {
    /// A binding that selects no runtime proof, for an adapter that does not
    /// declare independent guest-runtime admission.
    pub fn unselected() -> Self {
        Self {
            guest_runtime_manifest_hash: "4".repeat(64),
            guest: None,
            content: None,
        }
    }
}

/// Provision only a fixture credential for an already-admitted, current-node
/// binding into the same persistent vault that a real daemon will reopen.
/// This grants no allocation authority and never constructs a raw vault key.
#[cfg(any(test, feature = "test-support"))]
pub fn provision_installed_test_placement_credential(
    app_root: &std::path::Path,
    binding: &InstalledExternalExecutionBinding,
    secret: &str,
) -> Result<()> {
    use crate::vault::NodeVault as _;

    // Reverify the exact signed source, including its document, capacity owner
    // and digest, before opening a vault that may perform recovery writes.
    binding.retained_generation()?;
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(
        &app_root
            .join(ryeos_engine::AI_DIR)
            .join("node/identity/private_key.pem"),
    )?;
    ensure!(
        binding.signer == identity.fingerprint()
            && binding.signer_verifying_key == identity.verifying_key().to_bytes(),
        "test placement credential binding is not owned by the current node"
    );
    let access = binding.credential_access()?;
    let value = zeroize::Zeroizing::new(access.test_value(secret));
    let vault = crate::vault::SealedEnvelopeVault::load(app_root)?;
    vault.provision_placement_credential(&access, value.as_str())
}

impl InstalledExternalExecutionBinding {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn composed_test_fixture_with_settings(
        program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
        controller_transport: ExternalControllerTransportContract,
        controller_tls_root_certificates_der_base64: Vec<String>,
        backend: String,
        backend_artifact_hash: String,
        backend_artifact_bytes: u64,
        settings_schema_digest: String,
        supervisor_artifact_hash: String,
        supervisor_artifact_bytes: u64,
        launcher_artifact_hash: String,
        launcher_artifact_bytes: u64,
        configuration_adapter_artifact_hash: String,
        configuration_adapter_artifact_bytes: u64,
        connector_artifact_hash: String,
        connector_artifact_bytes: u64,
        settings: Value,
        qualification: &PlacementQualificationSelection,
    ) -> Result<Self> {
        let retained = RetainedExternalExecutionBinding::composed_test_fixture_with_settings(
            program,
            controller_transport,
            controller_tls_root_certificates_der_base64,
            backend,
            backend_artifact_hash,
            backend_artifact_bytes,
            settings_schema_digest,
            supervisor_artifact_hash,
            supervisor_artifact_bytes,
            launcher_artifact_hash,
            launcher_artifact_bytes,
            configuration_adapter_artifact_hash,
            configuration_adapter_artifact_bytes,
            connector_artifact_hash,
            connector_artifact_bytes,
            settings,
            qualification,
        )?;
        Ok(Self {
            id: retained.id,
            document: retained.document,
            signed_source: retained.signed_source.into(),
            signer: retained.signer,
            signer_verifying_key: retained.signer_verifying_key,
            digest: retained.digest,
            capacity_owner: retained.capacity_owner,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    #[cfg(test)]
    pub(crate) fn capacity_owner(&self) -> &str {
        &self.capacity_owner
    }
    pub(crate) fn credential_access(
        &self,
    ) -> Result<crate::vault::placement::PlacementCredentialAccess> {
        crate::vault::placement::PlacementCredentialAccess::new(
            &self.capacity_owner,
            &self.document.credential_generation,
            &self.document.backend,
            &self.document.account,
        )
    }
    pub(crate) fn backend_contract(&self) -> ExternalPlacementBackendContract {
        self.document.backend_contract()
    }
    pub(crate) fn retained_generation(&self) -> Result<RetainedExternalExecutionBinding> {
        let retained = RetainedExternalExecutionBinding {
            schema: 2,
            id: self.id.clone(),
            document: self.document.clone(),
            signed_source: self.signed_source.to_string(),
            signer: self.signer.clone(),
            signer_verifying_key: self.signer_verifying_key,
            digest: self.digest.clone(),
            capacity_owner: self.capacity_owner.clone(),
        };
        retained.validate()?;
        Ok(retained)
    }
    /// This check grants no contact permit. Backend/artifact and credential
    /// readiness must still be independently verified by the placement owner.
    pub(crate) fn check_program(
        &self,
        program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
    ) -> Result<()> {
        check_program(&self.document, program)
    }

    pub(crate) fn check_qualification_program(
        &self,
        program: &ryeos_state::external_execution::qualification_session::AdmittedExternalQualificationSessionProgram,
        purpose: &ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose,
    ) -> Result<()> {
        check_qualification_program(&self.document, program, purpose)
    }

    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        let retained = RetainedExternalExecutionBinding::test_fixture();
        Self {
            id: retained.id,
            document: retained.document,
            signed_source: retained.signed_source.into(),
            signer: retained.signer,
            signer_verifying_key: retained.signer_verifying_key,
            digest: retained.digest,
            capacity_owner: retained.capacity_owner,
        }
    }

    /// Author and verify a direct-mode fixture generation. This does not
    /// provide a production constructor or authorize direct execution.
    #[cfg(test)]
    pub(crate) fn direct_test_fixture(timeout_seconds: u32) -> Self {
        let mut binding = Self::test_fixture();
        binding.document.workload = ExternalWorkloadBinding::DirectCommand {};
        binding.document.max_export_bytes = 0;
        binding.document.timeout_seconds = timeout_seconds;
        binding.document.validate().unwrap();
        let key = lillux::crypto::SigningKey::from_bytes(&[37; 32]);
        assert_eq!(key.verifying_key().to_bytes(), binding.signer_verifying_key);
        binding.signed_source = lillux::signature::sign_content_at(
            &serde_yaml::to_string(&binding.document).unwrap(),
            &key,
            "#",
            None,
            "2026-09-23T00:00:00Z",
        )
        .into();
        binding.digest =
            binding_digest(&binding.id, &binding.signer, &binding.signed_source).unwrap();
        binding.retained_generation().unwrap();
        binding
    }
}

fn check_direct_endpoint(
    document: &BindingDocument,
    id: &str,
    digest: &str,
    projection: &ryeos_state::external_execution::admission::ExternalDirectCommandProjection,
) -> Result<()> {
    ensure!(
        matches!(document.workload, ExternalWorkloadBinding::DirectCommand {})
            && document.max_export_bytes == 0,
        "ordinary external command requires a direct binding without candidate export"
    );
    ensure!(
        projection.endpoint_binding_id == id && projection.endpoint_binding_digest == digest,
        "external direct command changed its exact signed endpoint generation"
    );
    ensure!(
        (1..=u64::from(document.timeout_seconds)).contains(&projection.timeout_seconds),
        "external direct command exceeds its signed endpoint execution budget"
    );
    ensure!(
        matches!(
            projection.execution_mode,
            ryeos_external_execution_contract::ExternalExecutionMode::DirectCommand { .. }
        ),
        "external direct command cannot substitute a structured session"
    );
    Ok(())
}

fn check_program(
    document: &BindingDocument,
    program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
) -> Result<()> {
    let session = document.workload.structured_session()?;
    program.validate()?;
    ensure!(
        program.requirement.protocol == document.protocol
            && program.requirement.provider_declaration_id == session.provider_declaration_id
            && program.requirement.provider_configuration_destination
                == session.provider_configuration_destination
            && program.requirement.connector_protocol == session.connector_protocol
            && program.runtime_manifest_hash == session.runtime_manifest_hash
            && program.selection_identity_digest == session.runtime_selection_identity,
        "external candidate program contradicts installed placement binding"
    );
    Ok(())
}

fn check_qualification_program(
    document: &BindingDocument,
    program: &ryeos_state::external_execution::qualification_session::AdmittedExternalQualificationSessionProgram,
    purpose: &ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose,
) -> Result<()> {
    let session = document.workload.qualification_structured_session()?;
    program.validate_against_purpose(purpose)?;
    ensure!(
        document.max_export_bytes == 0
            && document.runtime_content_qualification.is_none()
            && program.requirement.protocol == document.protocol
            && program.requirement.provider_declaration_id == session.provider_declaration_id
            && program.requirement.provider_configuration_destination
                == session.provider_configuration_destination
            && program.requirement.connector_protocol == session.connector_protocol
            && program.requirement.qualification_requirement_digest()?
                == session.requirement_digest
            && program.qualification_use.profile_hash == session.worker_profile_hash
            && program.runtime_manifest_hash == session.runtime_manifest_hash
            && program.runtime_member.executable_sha256 == session.runtime_member_sha256,
        "qualification-only program contradicts its signed zero-export placement binding"
    );
    Ok(())
}

struct ParsedBinding {
    id: String,
    document: BindingDocument,
}
impl std::fmt::Debug for ParsedBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedBinding")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl CompiledNodeConfigItem for ParsedBinding {
    fn section_name(&self) -> &'static str {
        SECTION_NAME
    }
    fn admit(
        self: Box<Self>,
        target: &mut crate::node_config::loader::NodeConfigSnapshotBuilder,
        admission: &crate::node_config::loader::NodeConfigAdmission,
    ) -> Result<()> {
        let digest = binding_digest(
            &self.id,
            &admission.signer_fingerprint,
            &admission.signed_source,
        )?;
        // Rotation changes the binding, not the account's capacity domain.
        let capacity_owner = capacity_owner(&admission.signer_fingerprint, &self.document)?;
        target.push_external_execution(InstalledExternalExecutionBinding {
            id: self.id,
            document: self.document,
            signed_source: admission.signed_source.clone(),
            signer: admission.signer_fingerprint.clone(),
            signer_verifying_key: admission.signer_verifying_key,
            digest,
            capacity_owner,
        })
    }
}

impl NodeConfigSection for ExternalExecutionSection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }
    fn source_scope(&self) -> NodeConfigSourceScope {
        NodeConfigSourceScope::AppRootOnly
    }
    fn load_spec(&self) -> SectionLoadSpec {
        SectionLoadSpec {
            phase: SectionLoadPhase::Full,
            traversal: SectionTraversal::Flat,
            signer: SectionSignerPolicy::CurrentNode,
            cardinality: SectionCardinality::Any,
        }
    }
    fn parse(
        &self,
        ctx: &NodeItemContext,
        body: &Value,
    ) -> Result<Box<dyn CompiledNodeConfigItem>> {
        let document: BindingDocument =
            serde_json::from_value(body.clone()).context("invalid external placement binding")?;
        document.validate()?;
        Ok(Box::new(ParsedBinding {
            id: ctx.id.clone(),
            document,
        }))
    }
}

#[cfg(test)]
mod workload_binding_tests {
    use super::*;

    fn document() -> BindingDocument {
        RetainedExternalExecutionBinding::test_fixture().document
    }

    fn ordinary_relay_document() -> BindingDocument {
        let mut document = document();
        document.schema = 19;
        document.backend = "render-sandbox-early-access".into();
        document.settings = serde_json::json!({"owner_id":"render-owner"});
        document.settings_digest = adapter_settings_digest(&document.settings).unwrap();
        document.network_policy = "controller_file_relay_guest_deny_all_v1".into();
        document.controller_tls_root_certificates_der_base64.clear();
        document.controller_transport = ExternalControllerTransportSelection::ControllerFileRelay(
            ryeos_state::external_execution::transport::ExternalControllerFileRelayContract {
                schema: 3,
                route_contract:
                    ryeos_state::external_execution::transport::EXTERNAL_CHANNEL_RELAY_CONTRACT
                        .into(),
                owner_workspace_id: "render-owner".into(),
                private_mailbox_parent: "/run/ryeos/private-relay".into(),
                maximum_file_bytes: 1024 * 1024,
                maximum_frames: 1024,
                poll_interval_ms: 1000,
                poll_deadline_ms: 30_000,
            },
        );
        document
    }

    #[test]
    fn ordinary_relay_binding_uses_schema_19_and_refuses_cross_pairs() {
        let document = ordinary_relay_document();
        document.validate().unwrap();
        assert!(document.workload.structured_session().is_ok());

        let mut q_schema = document.clone();
        q_schema.schema = 18;
        assert!(q_schema.validate().is_err());

        let mut http_schema_19 = document.clone();
        let mut http = RetainedExternalExecutionBinding::test_fixture().document;
        http.schema = 19;
        http_schema_19.controller_transport = http.controller_transport;
        http_schema_19.controller_tls_root_certificates_der_base64 =
            http.controller_tls_root_certificates_der_base64;
        http_schema_19.network_policy = "supervisor_pinned_owner_only_candidate_denied_v1".into();
        assert!(http_schema_19.validate().is_err());
    }

    #[test]
    fn qualification_binding_has_exact_identity_and_no_export_or_ordinary_session_authority() {
        let mut document = document();
        let ExternalWorkloadBinding::StructuredSession(ordinary) = &document.workload else {
            unreachable!("fixture has an ordinary session")
        };
        let ordinary = ordinary.clone();
        document.workload = ExternalWorkloadBinding::QualificationStructuredSession(
            ExternalQualificationStructuredSessionBinding {
                provider_declaration_id: ordinary.provider_declaration_id.clone(),
                provider_configuration_destination: ordinary
                    .provider_configuration_destination
                    .clone(),
                requirement_digest: "a".repeat(64),
                worker_profile_hash: "b".repeat(64),
                runtime_manifest_hash: ordinary.runtime_manifest_hash.clone(),
                runtime_member_sha256: "c".repeat(64),
                configuration_adapter_artifact_hash: ordinary
                    .configuration_adapter_artifact_hash
                    .clone(),
                configuration_adapter_artifact_bytes: ordinary.configuration_adapter_artifact_bytes,
                connector_protocol: ordinary.connector_protocol.clone(),
                connector_artifact_hash: ordinary.connector_artifact_hash.clone(),
                connector_artifact_bytes: ordinary.connector_artifact_bytes,
            },
        );
        assert!(document.validate().is_err());
        document.max_export_bytes = 0;
        document.validate().unwrap();
        assert!(document.workload.structured_session().is_err());
        assert!(document.workload.qualification_structured_session().is_ok());
        document.runtime_content_qualification = Some(ExternalRuntimeContentQualificationBinding {
            activation_ref: "config:codex/runtime-activation".into(),
            coordinate_id: "1".repeat(64),
            attestation_hash: "2".repeat(64),
            owner_principal: format!("fp:{}", "3".repeat(64)),
            required_claims: vec!["runtime_compatible".into()],
            remote_testimony: None,
        });
        assert!(document.validate().is_err());
        document.runtime_content_qualification = None;
        document.runtime_qualification = Some(ExternalRuntimeQualificationBinding {
            attestation_hash: "4".repeat(64),
            owner_principal: format!("fp:{}", "5".repeat(64)),
            qualification: ryeos_state::external_content::products::composition::ProductRelationshipQualification {
                policy_ref: Some("config:render/snapshot-qualification".into()),
                required_claims: vec!["render_snapshot_v1".into()],
            },
            remote_testimony: None,
        });
        assert!(document.validate().is_err());
        let mut ordinary_binding = document.clone();
        ordinary_binding.workload = ExternalWorkloadBinding::StructuredSession(ordinary);
        ordinary_binding.max_export_bytes = 1;
        ordinary_binding.validate().unwrap();
    }

    #[test]
    fn session_runtime_content_selection_is_distinct_and_explicit() {
        let mut selected = document();
        selected.runtime_content_qualification = Some(ExternalRuntimeContentQualificationBinding {
            activation_ref: "config:codex/runtime-activation".into(),
            coordinate_id: "1".repeat(64),
            attestation_hash: "2".repeat(64),
            owner_principal: format!("fp:{}", "3".repeat(64)),
            required_claims: vec!["runtime_compatible".into()],
            remote_testimony: None,
        });
        selected.validate().unwrap();
        assert_eq!(
            selected.backend_contract().runtime_content_qualification,
            selected.runtime_content_qualification
        );
        assert!(selected.runtime_qualification.is_none());
        let mut missing = serde_json::to_value(&selected).unwrap();
        missing
            .as_object_mut()
            .unwrap()
            .remove("runtime_content_qualification");
        assert!(serde_json::from_value::<BindingDocument>(missing).is_err());
        let mut predecessor = selected.clone();
        predecessor.schema = 14;
        assert!(predecessor.validate().is_err());
        let mut direct = selected.clone();
        direct.workload = ExternalWorkloadBinding::DirectCommand {};
        assert!(direct.validate().is_err());
        let mut empty_claims = selected.clone();
        empty_claims
            .runtime_content_qualification
            .as_mut()
            .unwrap()
            .required_claims
            .clear();
        assert!(empty_claims.validate().is_err());
    }

    #[test]
    fn content_remote_testimony_selection_is_required_nullable_and_exact() {
        let mut selected = document();
        selected.runtime_content_qualification = Some(ExternalRuntimeContentQualificationBinding {
            activation_ref: "config:codex/runtime-activation".into(),
            coordinate_id: "1".repeat(64),
            attestation_hash: "2".repeat(64),
            owner_principal: format!("fp:{}", "3".repeat(64)),
            required_claims: vec!["runtime_compatible".into()],
            remote_testimony: Some(ExternalRemoteQualificationTestimonyBinding {
                acceptance_hash: "8".repeat(64),
                acceptance_policy_ref: "config:codex/runtime-acceptance".into(),
            }),
        });
        selected.validate().unwrap();
        let value = serde_json::to_value(&selected).unwrap();
        assert!(serde_json::from_value::<BindingDocument>(value.clone()).unwrap() == selected);

        let mut local = value.clone();
        local["runtime_content_qualification"]["remote_testimony"] = serde_json::Value::Null;
        let local = serde_json::from_value::<BindingDocument>(local).unwrap();
        local.validate().unwrap();
        assert!(
            local
                .runtime_content_qualification
                .unwrap()
                .remote_testimony
                .is_none()
        );

        let mut missing = value.clone();
        missing["runtime_content_qualification"]
            .as_object_mut()
            .unwrap()
            .remove("remote_testimony");
        assert!(serde_json::from_value::<BindingDocument>(missing).is_err());

        let mut unknown = value;
        unknown["runtime_content_qualification"]["remote_testimony"]["extra"] =
            serde_json::json!(1);
        assert!(serde_json::from_value::<BindingDocument>(unknown).is_err());

        for (field, bad) in [
            ("acceptance_hash", "not-a-hash"),
            ("acceptance_policy_ref", "tool:codex/runtime-acceptance"),
        ] {
            let mut wrong = selected.clone();
            let remote = wrong
                .runtime_content_qualification
                .as_mut()
                .unwrap()
                .remote_testimony
                .as_mut()
                .unwrap();
            match field {
                "acceptance_hash" => remote.acceptance_hash = bad.into(),
                _ => remote.acceptance_policy_ref = bad.into(),
            }
            assert!(wrong.validate().is_err(), "{field} {bad}");
        }
    }

    #[test]
    fn guest_owner_runtime_is_a_distinct_required_signed_identity() {
        let document = document();
        document.validate().unwrap();
        let contract = document.backend_contract();
        assert_eq!(
            contract.guest_runtime_manifest_hash,
            document.guest_runtime_manifest_hash
        );
        assert_ne!(
            contract.guest_runtime_manifest_hash,
            contract
                .workload
                .structured_session()
                .unwrap()
                .runtime_manifest_hash
        );
        assert!(contract.runtime_qualification.is_none());

        let mut missing = serde_json::to_value(&document).unwrap();
        missing
            .as_object_mut()
            .unwrap()
            .remove("guest_runtime_manifest_hash");
        assert!(serde_json::from_value::<BindingDocument>(missing).is_err());

        let mut missing_qualification = serde_json::to_value(&document).unwrap();
        missing_qualification
            .as_object_mut()
            .unwrap()
            .remove("runtime_qualification");
        assert!(serde_json::from_value::<BindingDocument>(missing_qualification).is_err());

        let mut qualified = document.clone();
        qualified.runtime_qualification = Some(ExternalRuntimeQualificationBinding {
            attestation_hash: "6".repeat(64),
            owner_principal: format!("fp:{}", "7".repeat(64)),
            qualification: ryeos_state::external_content::products::composition::ProductRelationshipQualification {
                policy_ref: Some("config:render/snapshot-qualification".into()),
                required_claims: vec!["render_snapshot_v1".into()],
            },
            remote_testimony: None,
        });
        qualified.validate().unwrap();
        assert_eq!(
            qualified.backend_contract().runtime_qualification,
            qualified.runtime_qualification
        );
        qualified
            .runtime_qualification
            .as_mut()
            .unwrap()
            .attestation_hash = "not-a-hash".into();
        assert!(qualified.validate().is_err());
        let mut missing_owner = serde_json::to_value(&qualified).unwrap();
        missing_owner["runtime_qualification"]
            .as_object_mut()
            .unwrap()
            .remove("owner_principal");
        assert!(serde_json::from_value::<BindingDocument>(missing_owner).is_err());
        let mut wrong_policy = qualified.clone();
        let qualification = wrong_policy.runtime_qualification.as_mut().unwrap();
        qualification.attestation_hash = "6".repeat(64);
        qualification.qualification.policy_ref = None;
        assert!(wrong_policy.validate().is_err());

        let mut old_schema = document.clone();
        old_schema.schema = 13;
        assert!(old_schema.validate().is_err());

        let mut malformed = document;
        malformed.guest_runtime_manifest_hash = "not-a-content-identity".into();
        assert!(malformed.validate().is_err());
    }

    #[test]
    fn remote_testimony_selection_is_required_nullable_and_exact() {
        let mut qualified = document();
        qualified.runtime_qualification = Some(ExternalRuntimeQualificationBinding {
            attestation_hash: "6".repeat(64),
            owner_principal: format!("fp:{}", "7".repeat(64)),
            qualification: ryeos_state::external_content::products::composition::ProductRelationshipQualification {
                policy_ref: Some("config:render/snapshot-qualification".into()),
                required_claims: vec!["render_snapshot_v1".into()],
            },
            remote_testimony: Some(ExternalRemoteQualificationTestimonyBinding {
                acceptance_hash: "8".repeat(64),
                acceptance_policy_ref: "config:render/snapshot-acceptance".into(),
            }),
        });
        qualified.validate().unwrap();
        assert_eq!(
            qualified.backend_contract().runtime_qualification,
            qualified.runtime_qualification
        );
        let value = serde_json::to_value(&qualified).unwrap();
        assert!(serde_json::from_value::<BindingDocument>(value.clone()).unwrap() == qualified);

        let mut local = value.clone();
        local["runtime_qualification"]["remote_testimony"] = serde_json::Value::Null;
        let local = serde_json::from_value::<BindingDocument>(local).unwrap();
        local.validate().unwrap();
        assert!(
            local
                .runtime_qualification
                .unwrap()
                .remote_testimony
                .is_none()
        );

        let mut missing = value.clone();
        missing["runtime_qualification"]
            .as_object_mut()
            .unwrap()
            .remove("remote_testimony");
        assert!(serde_json::from_value::<BindingDocument>(missing).is_err());

        let mut unknown = value.clone();
        unknown["runtime_qualification"]["remote_testimony"]["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<BindingDocument>(unknown).is_err());

        for (field, bad) in [
            ("acceptance_hash", "not-a-hash"),
            ("acceptance_policy_ref", "tool:render/snapshot-acceptance"),
            (
                "acceptance_policy_ref",
                "config:render/snapshot-acceptance@1",
            ),
        ] {
            let mut wrong = qualified.clone();
            let remote = wrong
                .runtime_qualification
                .as_mut()
                .unwrap()
                .remote_testimony
                .as_mut()
                .unwrap();
            match field {
                "acceptance_hash" => remote.acceptance_hash = bad.into(),
                _ => remote.acceptance_policy_ref = bad.into(),
            }
            assert!(wrong.validate().is_err(), "{field} {bad}");
        }
    }

    #[test]
    fn direct_startup_budget_uses_signed_contact_plus_observation_only() {
        // Exercise the authenticated generation, not a hand-built backend
        // contract. Execution and cleanup limits must not enter this sum.
        for (contact, observation, execution, cleanup, expected_ms) in [
            (1, 1, 30, 120, 2_000),
            (7, 19, 300, 120, 26_000),
            (60, 300, 3600, 120, 360_000),
        ] {
            let mut binding = InstalledExternalExecutionBinding::direct_test_fixture(execution);
            binding.document.contact_timeout_seconds = contact;
            binding.document.observation_timeout_seconds = observation;
            binding.document.cleanup_timeout_seconds = cleanup;
            binding.document.validate().unwrap();
            let key = lillux::crypto::SigningKey::from_bytes(&[37; 32]);
            binding.signed_source = lillux::signature::sign_content_at(
                &serde_yaml::to_string(&binding.document).unwrap(),
                &key,
                "#",
                None,
                "2026-09-23T00:00:00Z",
            )
            .into();
            binding.digest =
                binding_digest(&binding.id, &binding.signer, &binding.signed_source).unwrap();
            let retained = binding.retained_generation().unwrap();
            let contract = retained.backend_contract();
            let before = serde_json::to_value(&contract).unwrap();
            assert_eq!(contract.direct_startup_budget_ms().unwrap(), expected_ms);
            assert_eq!(contract.timeout_seconds, execution);
            assert_eq!(contract.cleanup_timeout_seconds, cleanup);
            assert_eq!(serde_json::to_value(&contract).unwrap(), before);
        }

        let session = InstalledExternalExecutionBinding::test_fixture()
            .retained_generation()
            .unwrap()
            .backend_contract();
        let before = serde_json::to_value(&session).unwrap();
        let error = session.direct_startup_budget_ms().unwrap_err();
        assert_eq!(
            error.to_string(),
            "direct startup budget requires an ordinary-command binding"
        );
        assert_eq!(session.timeout_seconds, 60);
        assert_eq!(session.contact_timeout_seconds, 30);
        assert_eq!(session.observation_timeout_seconds, 60);
        assert_eq!(serde_json::to_value(&session).unwrap(), before);
    }

    #[test]
    fn direct_endpoint_check_requires_exact_generation_mode_and_budget() {
        use ryeos_external_execution_contract::ExternalExecutionMode;
        use ryeos_state::external_execution::admission::{
            ExternalDirectCommandProjection, ExternalDirectNativeRequirements,
            ExternalDirectSealedInput,
        };
        let binding = InstalledExternalExecutionBinding::direct_test_fixture(30);
        let retained = binding.retained_generation().unwrap();
        let projection = ExternalDirectCommandProjection {
            workspace_view: ryeos_external_execution_contract::GuestWorkspaceView::FullSnapshot,
            target_limits: None,
            argv0: "/runtime/bin/check".into(),
            arguments: Vec::new(),
            cwd: "/workspace".into(),
            environment: std::collections::BTreeMap::new(),
            stdin: ExternalDirectSealedInput::from_bytes(b"input").unwrap(),
            endpoint_binding_id: binding.id().into(),
            endpoint_binding_digest: binding.digest().into(),
            execution_mode: ExternalExecutionMode::DirectCommand {
                stdout_max_bytes: 1024,
                stderr_max_bytes: 1024,
            },
            timeout_seconds: 30,
            native: ExternalDirectNativeRequirements::LinuxIsolatedReadOnly {
                required_arch: "x86_64".into(),
            },
        };
        let check = |projection: &ExternalDirectCommandProjection| {
            check_direct_endpoint(
                &retained.document,
                &retained.id,
                &retained.digest,
                projection,
            )
        };
        check(&projection).unwrap();
        for field in ["id", "digest", "zero_timeout", "long_timeout", "mode"] {
            let mut changed = projection.clone();
            match field {
                "id" => changed.endpoint_binding_id = "other-direct".into(),
                "digest" => changed.endpoint_binding_digest = "f".repeat(64),
                "zero_timeout" => changed.timeout_seconds = 0,
                "long_timeout" => changed.timeout_seconds = 31,
                "mode" => changed.execution_mode = ExternalExecutionMode::StructuredSession {},
                _ => unreachable!(),
            }
            assert!(check(&changed).is_err(), "accepted changed {field}");
        }
        let session = document();
        assert!(
            check_direct_endpoint(&session, &retained.id, &retained.digest, &projection).is_err()
        );
        let mut export = retained.document.clone();
        export.max_export_bytes = 1;
        assert!(
            check_direct_endpoint(&export, &retained.id, &retained.digest, &projection).is_err()
        );
    }

    #[test]
    fn external_workload_binding_has_closed_exact_variants() {
        let session = document();
        session.validate().unwrap();
        let value = serde_json::to_value(&session).unwrap();
        assert_eq!(value["schema"], 17);
        assert_eq!(value["workload"]["kind"], "structured_session");
        assert!(value.get("provider_declaration_id").is_none());
        assert!(value.get("runtime_selection_identity").is_none());
        let decoded: BindingDocument = serde_json::from_value(value.clone()).unwrap();
        assert!(decoded == session);
        let mut independent_host_policy = session.clone();
        independent_host_policy
            .provider_network_inputs
            .resolver
            .source = "/etc/ryeos-provider-resolver".into();
        independent_host_policy.validate().unwrap();
        assert_eq!(
            independent_host_policy
                .backend_contract()
                .provider_network_inputs,
            independent_host_policy.provider_network_inputs
        );
        assert_eq!(
            independent_host_policy.controller_transport, session.controller_transport,
            "host provider network inputs must not change guest controller transport"
        );

        let mut direct = session.clone();
        direct.workload = ExternalWorkloadBinding::DirectCommand {};
        direct.max_export_bytes = 0;
        direct.validate().unwrap();
        let direct_value = serde_json::to_value(&direct).unwrap();
        assert_eq!(
            direct_value["workload"],
            serde_json::json!({"kind":"direct_command"})
        );
        let decoded: BindingDocument = serde_json::from_value(direct_value.clone()).unwrap();
        assert!(decoded == direct);
        assert!(direct.workload.structured_session().is_err());

        for original in [value, direct_value] {
            let mut missing_provider_inputs = original.clone();
            missing_provider_inputs
                .as_object_mut()
                .unwrap()
                .remove("provider_network_inputs");
            assert!(serde_json::from_value::<BindingDocument>(missing_provider_inputs).is_err());
            for predecessor_schema in [15, 16] {
                let mut predecessor = original.clone();
                predecessor["schema"] = serde_json::json!(predecessor_schema);
                assert!(
                    serde_json::from_value::<BindingDocument>(predecessor)
                        .unwrap()
                        .validate()
                        .is_err()
                );
            }
            let mut unknown = original.clone();
            unknown["workload"]["unexpected"] = serde_json::json!(true);
            assert!(serde_json::from_value::<BindingDocument>(unknown).is_err());
            let mut absent = original.clone();
            absent.as_object_mut().unwrap().remove("workload");
            assert!(serde_json::from_value::<BindingDocument>(absent).is_err());
            let mut predecessor = original.clone();
            predecessor["schema"] = serde_json::json!(8);
            assert!(
                serde_json::from_value::<BindingDocument>(predecessor)
                    .unwrap()
                    .validate()
                    .is_err()
            );
            let mut unknown_kind = original;
            unknown_kind["workload"]["kind"] = serde_json::json!("other");
            assert!(serde_json::from_value::<BindingDocument>(unknown_kind).is_err());
        }
    }

    #[test]
    fn direct_workload_binding_refuses_session_fields_and_export_authority() {
        let mut direct = document();
        direct.workload = ExternalWorkloadBinding::DirectCommand {};
        assert!(
            direct
                .validate()
                .unwrap_err()
                .to_string()
                .contains("candidate export")
        );
        direct.max_export_bytes = 0;
        direct.validate().unwrap();
        let direct_value = serde_json::to_value(&direct).unwrap();
        let session_value = serde_json::to_value(document().workload).unwrap();
        for (field, value) in session_value.as_object().unwrap() {
            if field == "kind" {
                continue;
            }
            let mut nested = direct_value.clone();
            nested["workload"][field] = value.clone();
            assert!(
                serde_json::from_value::<BindingDocument>(nested).is_err(),
                "{field}"
            );
            let mut flat = direct_value.clone();
            flat[field] = value.clone();
            assert!(
                serde_json::from_value::<BindingDocument>(flat).is_err(),
                "{field}"
            );
        }
        direct.max_transfer_bytes = 0;
        assert!(direct.validate().is_err());
    }

    #[test]
    fn structured_workload_binding_preserves_required_authority_and_bounds() {
        let mut session = document();
        let value = serde_json::to_value(&session).unwrap();
        for field in value["workload"].as_object().unwrap().keys() {
            let mut missing = value.clone();
            missing["workload"].as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<BindingDocument>(missing).is_err(),
                "{field}"
            );
        }
        for (field, invalid) in [
            ("provider_declaration_id", serde_json::json!("")),
            (
                "provider_configuration_destination",
                serde_json::json!("../credentials"),
            ),
            ("runtime_manifest_hash", serde_json::json!("invalid")),
            ("runtime_selection_identity", serde_json::json!("invalid")),
            (
                "configuration_adapter_artifact_hash",
                serde_json::json!("invalid"),
            ),
            ("configuration_adapter_artifact_bytes", serde_json::json!(0)),
            ("connector_protocol", serde_json::json!("other")),
            ("connector_artifact_hash", serde_json::json!("invalid")),
            ("connector_artifact_bytes", serde_json::json!(1_u64 << 40)),
        ] {
            let mut invalid_value = value.clone();
            invalid_value["workload"][field] = invalid;
            let invalid_document: BindingDocument = serde_json::from_value(invalid_value).unwrap();
            assert!(invalid_document.validate().is_err(), "{field}");
        }
        session.max_export_bytes = 0;
        assert!(session.validate().is_err());
    }

    #[test]
    fn retained_binding_cannot_change_workload_without_signed_authority() {
        let mut retained = RetainedExternalExecutionBinding::test_fixture();
        retained.validate().unwrap();
        retained.document.workload = ExternalWorkloadBinding::DirectCommand {};
        retained.document.max_export_bytes = 0;
        retained.document.validate().unwrap();
        assert!(
            retained
                .validate()
                .unwrap_err()
                .to_string()
                .contains("contradicts its signed source")
        );
    }
}

#[cfg(test)]
mod persistent_placement_credential_tests {
    use super::*;
    use crate::vault::{
        NodeVault as _, NodeVaultReadAll as _, SealedEnvelopeVault, default_sealed_store_path,
    };

    fn fixture() -> (tempfile::TempDir, InstalledExternalExecutionBinding) {
        let root = tempfile::tempdir().unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(
            &root.path().join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let key = lillux::vault::VaultSecretKey::generate();
        lillux::vault::write_secret_key(
            &ryeos_vault::paths::default_vault_secret_key_path(root.path()),
            &key,
        )
        .unwrap();
        lillux::vault::write_public_key(
            &ryeos_vault::paths::default_vault_public_key_path(root.path()),
            &key.public_key(),
        )
        .unwrap();
        let mut binding = InstalledExternalExecutionBinding::test_fixture();
        binding.signer = identity.fingerprint().to_owned();
        binding.signer_verifying_key = identity.verifying_key().to_bytes();
        binding.signed_source = lillux::signature::sign_content_at(
            &serde_yaml::to_string(&binding.document).unwrap(),
            identity.signing_key(),
            "#",
            None,
            "2026-09-23T00:00:00Z",
        )
        .into();
        binding.digest =
            binding_digest(&binding.id, &binding.signer, &binding.signed_source).unwrap();
        binding.capacity_owner = capacity_owner(&binding.signer, &binding.document).unwrap();
        binding.retained_generation().unwrap();
        (root, binding)
    }

    #[test]
    fn persistent_placement_credential_survives_vault_reopen() {
        let (root, binding) = fixture();
        provision_installed_test_placement_credential(root.path(), &binding, "fixture-secret")
            .unwrap();
        // A repeated identical provisioning uses the existing immutable value.
        provision_installed_test_placement_credential(root.path(), &binding, "fixture-secret")
            .unwrap();
        let reopened = SealedEnvelopeVault::load(root.path()).unwrap();
        let access = binding.credential_access().unwrap();
        let retained = reopened.placement_credential(&access).unwrap();
        assert_eq!(access.decode(retained).unwrap().secret(), "fixture-secret");
        assert!(reopened.read_all("fixture-operator").unwrap().is_empty());
    }

    #[test]
    fn persistent_placement_credential_refuses_changed_secret() {
        let (root, binding) = fixture();
        provision_installed_test_placement_credential(root.path(), &binding, "fixture-secret")
            .unwrap();
        let path = default_sealed_store_path(root.path());
        let before = std::fs::read(&path).unwrap();
        let error =
            provision_installed_test_placement_credential(root.path(), &binding, "changed-secret")
                .unwrap_err();
        // The store-lock owner adds context around the immutable-generation
        // refusal. Match the exact inner cause, not just the outer lock label.
        assert!(
            error.chain().any(|cause| {
                cause.to_string() == "placement credential generation is immutable"
            })
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let reopened = SealedEnvelopeVault::load(root.path()).unwrap();
        let access = binding.credential_access().unwrap();
        let retained = reopened.placement_credential(&access).unwrap();
        assert_eq!(access.decode(retained).unwrap().secret(), "fixture-secret");
    }

    #[test]
    fn persistent_placement_credential_refuses_other_node_before_store_mutation() {
        let (root, binding) = fixture();
        let (_other_root, other_binding) = fixture();
        let path = default_sealed_store_path(root.path());
        assert!(!path.exists());
        let error =
            provision_installed_test_placement_credential(root.path(), &other_binding, "secret")
                .unwrap_err();
        assert!(error.to_string().contains("not owned by the current node"));
        assert!(!path.exists());

        provision_installed_test_placement_credential(root.path(), &binding, "fixture-secret")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            provision_installed_test_placement_credential(root.path(), &other_binding, "secret")
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn persistent_placement_credential_reverifies_signed_binding_before_store_mutation() {
        let (root, mut binding) = fixture();
        binding.document.account = "unsigned-change".into();
        let error = provision_installed_test_placement_credential(root.path(), &binding, "secret")
            .unwrap_err();
        assert!(error.to_string().contains("contradicts its signed source"));
        assert!(!default_sealed_store_path(root.path()).exists());
    }
}
