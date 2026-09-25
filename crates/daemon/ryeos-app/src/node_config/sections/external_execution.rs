//! Node-owned external placement configuration. Signed configuration selects
//! authority; it is not evidence that a backend can safely execute it.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use ryeos_state::external_execution::transport::ExternalControllerTransportContract;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{
    CompiledNodeConfigItem, NodeConfigSection, NodeConfigSourceScope, NodeItemContext,
    SectionCardinality, SectionLoadPhase, SectionLoadSpec, SectionSignerPolicy, SectionTraversal,
};

pub const SECTION_NAME: &str = "external_execution";
pub struct ExternalExecutionSection;

/// Workload-specific authority in one node-signed placement binding. Direct
/// executable identity is supplied by its ordinary admitted Tool closure, not
/// by a provider declaration or a synthetic candidate-runtime selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExternalWorkloadBinding {
    StructuredSession(ExternalStructuredSessionBinding),
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

impl ExternalWorkloadBinding {
    pub(crate) fn structured_session(&self) -> Result<&ExternalStructuredSessionBinding> {
        match self {
            Self::StructuredSession(session) => Ok(session),
            Self::DirectCommand {} => anyhow::bail!(
                "external direct-command binding cannot authorize a structured session"
            ),
        }
    }

    fn validate(&self) -> Result<()> {
        let Self::StructuredSession(session) = self else {
            return Ok(());
        };
        ryeos_state::external_content::products::validate_name(&session.provider_declaration_id)?;
        ryeos_state::objects::validate_session_configuration_destination(
            &session.provider_configuration_destination,
        )?;
        for value in [
            &session.runtime_manifest_hash,
            &session.runtime_selection_identity,
            &session.configuration_adapter_artifact_hash,
            &session.connector_artifact_hash,
        ] {
            validate_content_identity(value)?;
        }
        for bytes in [
            session.configuration_adapter_artifact_bytes,
            session.connector_artifact_bytes,
        ] {
            validate_artifact_bytes(bytes)?;
        }
        ensure!(
            session.connector_protocol
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
    launcher_artifact_hash: String,
    launcher_artifact_bytes: u64,
    network_policy: String,
    storage_policy: String,
    cleanup_proof: String,
    controller_transport: ExternalControllerTransportContract,
    controller_tls_root_certificates_der_base64: Vec<String>,
    max_active: u16,
    timeout_seconds: u32,
    contact_timeout_seconds: u32,
    observation_timeout_seconds: u32,
    cleanup_timeout_seconds: u32,
    max_workspace_bytes: u64,
    max_export_bytes: u64,
    max_transfer_bytes: u64,
}

impl BindingDocument {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.kind == "node" && self.schema == 9,
            "unsupported external placement binding schema"
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
            &self.launcher_artifact_hash,
        ] {
            validate_content_identity(value)?;
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
            self.network_policy == "supervisor_pinned_owner_only_candidate_denied_v1"
                && self.storage_policy == "ephemeral_private_candidate_v1"
                && self.cleanup_proof == "provider_terminal_occurrence_v1",
            "placement binding requests an unsupported lifecycle contract"
        );
        const MAX_BYTES: u64 = 1 << 40;
        ensure!(
            (1..=MAX_BYTES).contains(&self.max_workspace_bytes)
                && (1..=MAX_BYTES).contains(&self.max_transfer_bytes),
            "placement binding storage or transfer budgets exceed bounds"
        );
        match &self.workload {
            ExternalWorkloadBinding::StructuredSession(_) => ensure!(
                (1..=self.max_workspace_bytes).contains(&self.max_export_bytes)
                    && self.max_transfer_bytes >= self.max_export_bytes,
                "structured-session placement export budgets exceed bounds"
            ),
            ExternalWorkloadBinding::DirectCommand {} => ensure!(
                self.max_export_bytes == 0,
                "direct-command placement cannot authorize candidate export"
            ),
        }
        self.controller_transport.validate()?;
        ensure!(
            ryeos_state::external_execution::transport::external_tls_root_bundle_digest(
                &self.controller_tls_root_certificates_der_base64,
            )? == self.controller_transport.tls_root_bundle_digest,
            "external controller TLS roots changed their signed identity"
        );
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
            launcher_artifact_hash: self.launcher_artifact_hash.clone(),
            launcher_artifact_bytes: self.launcher_artifact_bytes,
            network_policy: self.network_policy.clone(),
            storage_policy: self.storage_policy.clone(),
            cleanup_proof: self.cleanup_proof.clone(),
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
    pub(crate) launcher_artifact_hash: String,
    pub(crate) launcher_artifact_bytes: u64,
    pub(crate) network_policy: String,
    pub(crate) storage_policy: String,
    pub(crate) cleanup_proof: String,
    pub(crate) controller_transport: ExternalControllerTransportContract,
    pub(crate) controller_tls_root_certificates_der_base64: Vec<String>,
    pub(crate) max_active: u16,
    pub(crate) timeout_seconds: u32,
    pub(crate) contact_timeout_seconds: u32,
    pub(crate) observation_timeout_seconds: u32,
    pub(crate) cleanup_timeout_seconds: u32,
    pub(crate) max_workspace_bytes: u64,
    pub(crate) max_export_bytes: u64,
    pub(crate) max_transfer_bytes: u64,
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
        Ok(u64::from(self.contact_timeout_seconds)
            .checked_add(u64::from(self.observation_timeout_seconds))
            .and_then(|seconds| seconds.checked_mul(1_000))
            .context("external direct startup budget overflow")?)
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
                && self.signed_source.len() <= crate::node_document::MAX_ITEM_BYTES as usize,
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

    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let controller_tls_root_certificates_der_base64 =
            vec![STANDARD.encode(b"fixture controller TLS root")];
        let document = BindingDocument {
            kind: "node".into(),
            schema: 9,
            protocol: ryeos_state::external_execution::admission::PROTOCOL.into(),
            workload: ExternalWorkloadBinding::StructuredSession(ExternalStructuredSessionBinding {
                provider_declaration_id: "codex-hosted".into(),
                provider_configuration_destination: "environments.toml".into(),
                runtime_manifest_hash: "b".repeat(64),
                runtime_selection_identity: "c".repeat(64),
                configuration_adapter_artifact_hash: "2".repeat(64),
                configuration_adapter_artifact_bytes: 4096,
                connector_protocol: ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
                connector_artifact_hash: "f".repeat(64),
                connector_artifact_bytes: 4096,
            }),
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
            launcher_artifact_hash: "e".repeat(64),
            launcher_artifact_bytes: 4096,
            network_policy: "supervisor_pinned_owner_only_candidate_denied_v1".into(),
            storage_policy: "ephemeral_private_candidate_v1".into(),
            cleanup_proof: "provider_terminal_occurrence_v1".into(),
            controller_transport: ExternalControllerTransportContract {
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
                network_inputs: ryeos_state::external_execution::transport::ExternalNetworkInputPolicy {
                    resolver: ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                        source: "/etc/resolv.conf".into(),
                        max_bytes: 64 * 1024,
                    },
                    hosts: ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                        source: "/etc/hosts".into(),
                        max_bytes: 64 * 1024,
                    },
                },
            },
            controller_tls_root_certificates_der_base64,
            max_active: 1,
            timeout_seconds: 60,
            contact_timeout_seconds: 30,
            observation_timeout_seconds: 60,
            cleanup_timeout_seconds: 120,
            max_workspace_bytes: 1024,
            max_export_bytes: 512,
            max_transfer_bytes: 2048,
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
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<Self> {
        program.validate()?;
        controller_transport.validate()?;
        ensure!(
            ryeos_state::external_execution::transport::external_tls_root_bundle_digest(
                &controller_tls_root_certificates_der_base64,
            )? == controller_transport.tls_root_bundle_digest,
            "composed fixture TLS roots changed their transport identity"
        );
        let document = BindingDocument {
            kind: "node".into(),
            schema: 9,
            protocol: program.requirement.protocol.clone(),
            workload: ExternalWorkloadBinding::StructuredSession(
                ExternalStructuredSessionBinding {
                    provider_declaration_id: program.requirement.provider_declaration_id.clone(),
                    provider_configuration_destination: program
                        .requirement
                        .provider_configuration_destination
                        .clone(),
                    runtime_manifest_hash: program.runtime_manifest_hash.clone(),
                    runtime_selection_identity: program.selection_identity_digest.clone(),
                    configuration_adapter_artifact_hash,
                    configuration_adapter_artifact_bytes,
                    connector_protocol: program.requirement.connector_protocol.clone(),
                    connector_artifact_hash,
                    connector_artifact_bytes,
                },
            ),
            backend,
            account: "account".into(),
            capacity_group: "candidate-workers".into(),
            credential_generation: "a".repeat(64),
            settings_digest: adapter_settings_digest(&settings)?,
            settings_schema_digest,
            settings,
            backend_artifact_hash,
            backend_artifact_bytes,
            supervisor_artifact_hash,
            supervisor_artifact_bytes,
            launcher_artifact_hash,
            launcher_artifact_bytes,
            network_policy: "supervisor_pinned_owner_only_candidate_denied_v1".into(),
            storage_policy: "ephemeral_private_candidate_v1".into(),
            cleanup_proof: "provider_terminal_occurrence_v1".into(),
            controller_transport,
            controller_tls_root_certificates_der_base64,
            max_active: 1,
            timeout_seconds: 60,
            // The composed native fixture transfers the exact guest runtime
            // through descriptor authority during the one bounded activation
            // contact. Use the contract's maximum contact window so a large
            // admitted runtime is not measured against the smaller unit-
            // fixture default.
            contact_timeout_seconds: 60,
            observation_timeout_seconds: 60,
            cleanup_timeout_seconds: 120,
            max_workspace_bytes: 16 * 1024 * 1024,
            max_export_bytes: 8 * 1024 * 1024,
            max_transfer_bytes: 16 * 1024 * 1024,
        };
        document.validate()?;
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
    let identity = crate::identity::NodeIdentity::load(
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
    #[allow(clippy::too_many_arguments)]
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

    pub(crate) fn id(&self) -> &str {
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
        assert_eq!(value["schema"], 9);
        assert_eq!(value["workload"]["kind"], "structured_session");
        assert!(value.get("provider_declaration_id").is_none());
        assert!(value.get("runtime_selection_identity").is_none());
        let decoded: BindingDocument = serde_json::from_value(value.clone()).unwrap();
        assert!(decoded == session);

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
    use crate::vault::{NodeVault as _, SealedEnvelopeVault, default_sealed_store_path};

    fn fixture() -> (tempfile::TempDir, InstalledExternalExecutionBinding) {
        let root = tempfile::tempdir().unwrap();
        let identity = crate::identity::NodeIdentity::create(
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
