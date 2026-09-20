//! Closed JSON wire contract for occurrence bootstrap and frame exchange.
//!
//! Transport completion is not application evidence. These types only prevent
//! the daemon route and protected guest client from interpreting different
//! request/response shapes.

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use url::Url;

use super::admission::AdmittedExternalCandidateProgram;
use super::{
    ExecutionChannelBinding, MAX_CHUNK_BYTES, MAX_FRAME_BYTES, hash, validate_channel_public_key,
};

pub const EXTERNAL_CHANNEL_TRANSPORT_SCHEMA: u32 = 1;
pub const MAX_PLACEMENT_ID_BYTES: usize = 256;
pub const MAX_OCCURRENCE_ID_BYTES: usize = 512;
pub const EXTERNAL_CHANNEL_ROUTE_CONTRACT: &str = "ryeos.external-channel.http-json.v1";
pub const EXTERNAL_CHANNEL_ATTACH_PATH: &str = "/external-execution/channel/attach";
pub const EXTERNAL_CHANNEL_EXCHANGE_PATH: &str = "/external-execution/channel/exchange";
pub const MAX_TLS_ROOT_CERTIFICATES: usize = 8;
pub const MAX_TLS_ROOT_CERTIFICATE_BYTES: usize = 64 * 1024;
pub const MAX_TLS_ROOT_BUNDLE_BYTES: usize = 256 * 1024;

/// Node-signed, non-secret authority for the controller transport. The actual
/// root certificates are delivered only through the protected supervisor
/// bootstrap and must reproduce `tls_root_bundle_digest` exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalControllerTransportContract {
    pub schema: u32,
    pub https_origin: String,
    pub route_contract: String,
    pub tls_root_bundle_digest: String,
    pub connect_timeout_ms: u32,
    pub request_timeout_ms: u32,
    pub maximum_response_bytes: u64,
}

impl ExternalControllerTransportContract {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "unsupported external controller transport schema"
        );
        let origin =
            Url::parse(&self.https_origin).context("external controller origin is not a URL")?;
        ensure!(
            origin.scheme() == "https"
                && origin.host_str().is_some()
                && origin.username().is_empty()
                && origin.password().is_none()
                && origin.path() == "/"
                && origin.query().is_none()
                && origin.fragment().is_none()
                && origin.origin().ascii_serialization() == self.https_origin,
            "external controller origin is not a canonical HTTPS origin"
        );
        ensure!(
            self.route_contract == EXTERNAL_CHANNEL_ROUTE_CONTRACT,
            "unsupported external controller route contract"
        );
        hash(&self.tls_root_bundle_digest)?;
        ensure!(
            (1..=10_000).contains(&self.connect_timeout_ms)
                && (1..=30_000).contains(&self.request_timeout_ms),
            "external controller transport timeouts exceed bounds"
        );
        ensure!(
            (4_096..=2 * 1024 * 1024).contains(&self.maximum_response_bytes),
            "external controller response bound is invalid"
        );
        Ok(())
    }

    pub fn attach_url(&self) -> Result<Url> {
        self.validate()?;
        Url::parse(&format!(
            "{}{}",
            self.https_origin, EXTERNAL_CHANNEL_ATTACH_PATH
        ))
        .context("compose external attachment URL")
    }

    pub fn exchange_url(&self) -> Result<Url> {
        self.validate()?;
        Url::parse(&format!(
            "{}{}",
            self.https_origin, EXTERNAL_CHANNEL_EXCHANGE_PATH
        ))
        .context("compose external exchange URL")
    }
}

pub fn external_tls_root_bundle_digest(certificates_der_base64: &[String]) -> Result<String> {
    ensure!(
        !certificates_der_base64.is_empty()
            && certificates_der_base64.len() <= MAX_TLS_ROOT_CERTIFICATES,
        "external TLS root bundle count exceeds bounds"
    );
    let mut total = 0_usize;
    let mut prior: Option<&str> = None;
    for certificate in certificates_der_base64 {
        ensure!(
            prior.is_none_or(|value| value < certificate.as_str()),
            "external TLS root bundle is not uniquely ordered"
        );
        prior = Some(certificate);
        let decoded = STANDARD
            .decode(certificate)
            .map_err(|_| anyhow::anyhow!("external TLS root certificate is invalid base64"))?;
        ensure!(
            !decoded.is_empty()
                && decoded.len() <= MAX_TLS_ROOT_CERTIFICATE_BYTES
                && STANDARD.encode(&decoded) == *certificate,
            "external TLS root certificate is not canonical"
        );
        total = total
            .checked_add(decoded.len())
            .context("external TLS root bundle byte overflow")?;
    }
    ensure!(
        total <= MAX_TLS_ROOT_BUNDLE_BYTES,
        "external TLS root bundle exceeds its byte bound"
    );
    crate::objects::canonical_value_digest(&serde_json::json!({
        "domain":"ryeos.external-controller-tls-roots.v1",
        "certificates_der_base64":certificates_der_base64,
    }))
}

/// Secret-bearing, occurrence-specific input consumed only by the protected
/// guest supervisor. It is intentionally not `Debug` and must never enter
/// project CAS, candidate mounts, argv, logs, or public receipts.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSupervisorBootstrap {
    pub schema: u32,
    pub controller: ExternalControllerTransportContract,
    pub tls_root_certificates_der_base64: Vec<String>,
    pub placement_thread_id: String,
    pub occurrence_id: String,
    pub allocation_request_digest: String,
    pub admitted_capsule_hash: String,
    pub base_snapshot_hash: String,
    pub execution_binding_hash: String,
    pub supervisor_runtime_hash: String,
    pub candidate_program: AdmittedExternalCandidateProgram,
    pub owner_public_key: String,
    pub bootstrap_capability: String,
    pub attachment_deadline_ms: i64,
    pub execution_timeout_seconds: u32,
    pub post_execution_timeout_seconds: u32,
    pub channel_max_bytes: u64,
}

impl ExternalSupervisorBootstrap {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 2,
            "unsupported external supervisor bootstrap schema"
        );
        self.controller.validate()?;
        ensure!(
            external_tls_root_bundle_digest(&self.tls_root_certificates_der_base64)?
                == self.controller.tls_root_bundle_digest,
            "external supervisor TLS roots changed their signed identity"
        );
        bounded_text(
            &self.placement_thread_id,
            MAX_PLACEMENT_ID_BYTES,
            "external supervisor placement",
        )?;
        bounded_text(
            &self.occurrence_id,
            MAX_OCCURRENCE_ID_BYTES,
            "external supervisor occurrence",
        )?;
        for digest in [
            &self.allocation_request_digest,
            &self.admitted_capsule_hash,
            &self.base_snapshot_hash,
            &self.execution_binding_hash,
            &self.supervisor_runtime_hash,
        ] {
            hash(digest)?;
        }
        self.candidate_program.validate()?;
        ensure!(
            self.candidate_program.runtime_manifest_hash == self.supervisor_runtime_hash,
            "external supervisor program changed its runtime manifest"
        );
        validate_channel_public_key(&self.owner_public_key)?;
        let capability = STANDARD
            .decode(&self.bootstrap_capability)
            .map_err(|_| anyhow::anyhow!("external supervisor capability is invalid"))?;
        ensure!(
            capability.len() == 32 && STANDARD.encode(capability) == self.bootstrap_capability,
            "external supervisor capability is not canonical"
        );
        ensure!(
            self.attachment_deadline_ms > 0
                && (1..=3_600).contains(&self.execution_timeout_seconds)
                && (1..=900).contains(&self.post_execution_timeout_seconds),
            "external supervisor lifecycle deadlines exceed bounds"
        );
        ensure!(
            (1..=64 * 1024 * 1024).contains(&self.channel_max_bytes),
            "external supervisor channel byte bound is invalid"
        );
        Ok(())
    }

    pub fn validate_at(&self, now_ms: i64) -> Result<()> {
        self.validate()?;
        ensure!(
            now_ms > 0 && now_ms < self.attachment_deadline_ms,
            "external supervisor attachment deadline has expired"
        );
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes())
    }

    pub fn digest(&self) -> Result<String> {
        Ok(lillux::sha256_hex(&self.canonical_bytes()?))
    }

    pub fn attachment_request(
        &self,
        supervisor_signing_key: &lillux::crypto::SigningKey,
    ) -> Result<ExternalChannelAttachRequest> {
        self.validate()?;
        let request = ExternalChannelAttachRequest {
            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            placement_thread_id: self.placement_thread_id.clone(),
            occurrence_id: self.occurrence_id.clone(),
            bootstrap_capability: self.bootstrap_capability.clone(),
            supervisor_public_key: super::encode_channel_public_key(
                &supervisor_signing_key.verifying_key(),
            )?,
        };
        request.validate_for_bootstrap(self)?;
        Ok(request)
    }

    pub fn binding_max_frames(&self) -> Result<u32> {
        self.validate()?;
        Ok(u32::try_from(
            self.channel_max_bytes
                .div_ceil(MAX_CHUNK_BYTES as u64)
                .saturating_mul(4)
                .clamp(64, 65_536),
        )?)
    }

    pub fn validate_attached_binding(
        &self,
        binding: &ExecutionChannelBinding,
        supervisor_public_key: &str,
    ) -> Result<()> {
        self.validate()?;
        binding.validate()?;
        ensure!(
            binding.placement_thread_id == self.placement_thread_id
                && binding.occurrence_id == self.occurrence_id
                && binding.allocation_request_digest == self.allocation_request_digest
                && binding.admitted_capsule_hash == self.admitted_capsule_hash
                && binding.base_snapshot_hash == self.base_snapshot_hash
                && binding.execution_binding_hash == self.execution_binding_hash
                && binding.supervisor_runtime_hash == self.supervisor_runtime_hash
                && binding.candidate_program_digest == self.candidate_program.digest()?
                && binding.owner_public_key == self.owner_public_key
                && binding.supervisor_public_key == supervisor_public_key,
            "external attachment response changed its precommitted identity"
        );
        let execution_ms = i64::from(self.execution_timeout_seconds)
            .checked_mul(1_000)
            .context("external execution deadline overflow")?;
        let post_ms = i64::from(self.post_execution_timeout_seconds)
            .checked_mul(1_000)
            .context("external post-execution deadline overflow")?;
        ensure!(
            binding
                .execution_deadline_ms
                .checked_sub(binding.issued_at_ms)
                == Some(execution_ms)
                && binding
                    .expires_at_ms
                    .checked_sub(binding.execution_deadline_ms)
                    == Some(post_ms)
                && binding.max_bytes == self.channel_max_bytes
                && binding.max_frames == self.binding_max_frames()?,
            "external attachment response changed its signed lifecycle bounds"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChannelAttachRequest {
    pub schema: u32,
    pub placement_thread_id: String,
    pub occurrence_id: String,
    pub bootstrap_capability: String,
    pub supervisor_public_key: String,
}

impl ExternalChannelAttachRequest {
    pub fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.schema == EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            "unsupported external attachment schema"
        );
        bounded_text(
            &self.placement_thread_id,
            MAX_PLACEMENT_ID_BYTES,
            "external attachment placement",
        )?;
        bounded_text(
            &self.occurrence_id,
            MAX_OCCURRENCE_ID_BYTES,
            "external attachment occurrence",
        )?;
        ensure!(
            self.bootstrap_capability.len() == 44,
            "external attachment capability has the wrong length"
        );
        let capability = STANDARD
            .decode(&self.bootstrap_capability)
            .map_err(|_| anyhow::anyhow!("external attachment capability is invalid"))?;
        ensure!(
            capability.len() == 32 && STANDARD.encode(capability) == self.bootstrap_capability,
            "external attachment capability is not canonical"
        );
        validate_channel_public_key(&self.supervisor_public_key)?;
        Ok(())
    }

    pub fn principal_id(&self) -> String {
        format!("external-occurrence:{}", self.occurrence_id)
    }

    pub fn validate_for_bootstrap(&self, bootstrap: &ExternalSupervisorBootstrap) -> Result<()> {
        self.validate_shape()?;
        bootstrap.validate()?;
        ensure!(
            self.placement_thread_id == bootstrap.placement_thread_id
                && self.occurrence_id == bootstrap.occurrence_id
                && self.bootstrap_capability == bootstrap.bootstrap_capability,
            "external attachment request changed its sealed bootstrap"
        );
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate_shape()?;
        Ok(lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes())
    }

    pub fn digest(&self) -> Result<String> {
        Ok(lillux::sha256_hex(&self.canonical_bytes()?))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChannelAttachResponse {
    pub schema: u32,
    pub binding: ExecutionChannelBinding,
    pub binding_digest: String,
}

impl ExternalChannelAttachResponse {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            "unsupported external attachment response schema"
        );
        self.binding.validate()?;
        ensure!(
            self.binding.digest()? == self.binding_digest,
            "external attachment response changed its binding digest"
        );
        Ok(())
    }

    pub fn validate_for_bootstrap(
        &self,
        bootstrap: &ExternalSupervisorBootstrap,
        supervisor_public_key: &str,
    ) -> Result<()> {
        self.validate()?;
        bootstrap.validate_attached_binding(&self.binding, supervisor_public_key)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChannelExchangeRequest {
    pub schema: u32,
    pub placement_thread_id: String,
    pub occurrence_id: String,
    pub frame_base64: String,
}

impl ExternalChannelExchangeRequest {
    pub fn from_frame(
        placement_thread_id: impl Into<String>,
        occurrence_id: impl Into<String>,
        canonical_frame: &[u8],
    ) -> Result<Self> {
        ensure!(
            !canonical_frame.is_empty() && canonical_frame.len() <= MAX_FRAME_BYTES,
            "external exchange frame exceeds its wire bound"
        );
        let request = Self {
            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            placement_thread_id: placement_thread_id.into(),
            occurrence_id: occurrence_id.into(),
            frame_base64: STANDARD.encode(canonical_frame),
        };
        request.validate_shape()?;
        Ok(request)
    }

    pub fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.schema == EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            "unsupported external exchange schema"
        );
        bounded_text(
            &self.placement_thread_id,
            MAX_PLACEMENT_ID_BYTES,
            "external exchange placement",
        )?;
        bounded_text(
            &self.occurrence_id,
            MAX_OCCURRENCE_ID_BYTES,
            "external exchange occurrence",
        )?;
        self.decode_frame().map(|_| ())
    }

    pub fn decode_frame(&self) -> Result<Vec<u8>> {
        ensure!(
            !self.frame_base64.is_empty()
                && self.frame_base64.len() <= MAX_FRAME_BYTES.div_ceil(3) * 4,
            "external exchange frame exceeds its encoded bound"
        );
        let wire = STANDARD
            .decode(&self.frame_base64)
            .map_err(|_| anyhow::anyhow!("external exchange frame is invalid base64"))?;
        ensure!(
            wire.len() <= MAX_FRAME_BYTES && STANDARD.encode(&wire) == self.frame_base64,
            "external exchange frame is not canonical"
        );
        Ok(wire)
    }

    pub fn principal_id(&self) -> String {
        format!("external-occurrence:{}", self.occurrence_id)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChannelResponseFrame {
    pub sequence: u64,
    pub frame_digest: String,
    pub frame_base64: String,
}

impl ExternalChannelResponseFrame {
    pub fn new(sequence: u64, frame_digest: String, canonical_frame: &[u8]) -> Result<Self> {
        ensure!(
            !canonical_frame.is_empty() && canonical_frame.len() <= MAX_FRAME_BYTES,
            "external response frame exceeds its wire bound"
        );
        let frame = Self {
            sequence,
            frame_digest,
            frame_base64: STANDARD.encode(canonical_frame),
        };
        frame.decode_frame()?;
        Ok(frame)
    }

    pub fn decode_frame(&self) -> Result<Vec<u8>> {
        ensure!(
            self.sequence > 0,
            "external response frame sequence is zero"
        );
        super::hash(&self.frame_digest)?;
        ensure!(
            !self.frame_base64.is_empty()
                && self.frame_base64.len() <= MAX_FRAME_BYTES.div_ceil(3) * 4,
            "external response frame exceeds its encoded bound"
        );
        let wire = STANDARD
            .decode(&self.frame_base64)
            .map_err(|_| anyhow::anyhow!("external response frame is invalid base64"))?;
        ensure!(
            !wire.is_empty()
                && wire.len() <= MAX_FRAME_BYTES
                && STANDARD.encode(&wire) == self.frame_base64,
            "external response frame is not canonical"
        );
        Ok(wire)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChannelExchangeResponse {
    pub schema: u32,
    pub incoming_new: bool,
    pub incoming_sequence: u64,
    pub incoming_frame_digest: String,
    pub acknowledgement_frame_digest: Option<String>,
    pub outbound_frames: Vec<ExternalChannelResponseFrame>,
    pub urgent_revocation_frame: Option<ExternalChannelResponseFrame>,
}

impl ExternalChannelExchangeResponse {
    pub fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.schema == EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            "unsupported external exchange response schema"
        );
        ensure!(
            self.incoming_sequence > 0,
            "external exchange response incoming sequence is zero"
        );
        super::hash(&self.incoming_frame_digest)?;
        if let Some(digest) = self.acknowledgement_frame_digest.as_deref() {
            super::hash(digest)?;
        }
        ensure!(
            self.outbound_frames.len() <= 16,
            "external exchange response exceeds its route frame bound"
        );
        let mut bytes = 0_usize;
        for frame in &self.outbound_frames {
            bytes = bytes
                .checked_add(frame.decode_frame()?.len())
                .ok_or_else(|| anyhow::anyhow!("external response byte count overflow"))?;
        }
        ensure!(
            bytes <= 1024 * 1024,
            "external exchange response exceeds its route byte bound"
        );
        if let Some(frame) = self.urgent_revocation_frame.as_ref() {
            ensure!(
                frame.decode_frame()?.len() <= super::TERMINAL_CONTROL_BYTES as usize,
                "external urgent response exceeds its terminal-control bound"
            );
        }
        Ok(())
    }
}

fn bounded_text(value: &str, maximum: usize, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum,
        "{label} is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::external_execution::admission::{
        ExternalCandidateProcFilesystem, ExternalCandidateRequirement,
        ExternalCandidateRuntimeRecipe, PROTOCOL,
    };

    fn controller(roots: &[String]) -> ExternalControllerTransportContract {
        ExternalControllerTransportContract {
            schema: 1,
            https_origin: "https://controller.example:7443".into(),
            route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
            tls_root_bundle_digest: external_tls_root_bundle_digest(roots).unwrap(),
            connect_timeout_ms: 5_000,
            request_timeout_ms: 10_000,
            maximum_response_bytes: 1024 * 1024,
        }
    }

    fn bootstrap() -> ExternalSupervisorBootstrap {
        let roots = vec![STANDARD.encode(b"fixture DER root")];
        let runtime_recipe = ExternalCandidateRuntimeRecipe {
            schema: 1,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::new(),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: true,
            nested_sandbox: true,
        };
        let runtime_recipe_digest = runtime_recipe.digest().unwrap();
        ExternalSupervisorBootstrap {
            schema: 2,
            controller: controller(&roots),
            tls_root_certificates_der_base64: roots,
            placement_thread_id: "T-placement".into(),
            occurrence_id: "occurrence-one".into(),
            allocation_request_digest: "a".repeat(64),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            candidate_program: AdmittedExternalCandidateProgram {
                requirement: ExternalCandidateRequirement {
                    schema: 2,
                    protocol: PROTOCOL.into(),
                    runtime_product_declaration_id: "runtime".into(),
                    runtime_recipe,
                },
                runtime_manifest_hash: "e".repeat(64),
                runtime_witness_hash: "1".repeat(64),
                qualification_attestation_hash: "2".repeat(64),
                selection_identity_digest: "3".repeat(64),
                runtime_recipe_digest,
            },
            owner_public_key: super::super::encode_channel_public_key(
                &lillux::crypto::SigningKey::from_bytes(&[41; 32]).verifying_key(),
            )
            .unwrap(),
            bootstrap_capability: STANDARD.encode([42_u8; 32]),
            attachment_deadline_ms: 2_000_000,
            execution_timeout_seconds: 60,
            post_execution_timeout_seconds: 120,
            channel_max_bytes: 1024 * 1024,
        }
    }

    #[test]
    fn wire_contract_is_closed_canonical_and_bounded() {
        let request = ExternalChannelExchangeRequest::from_frame(
            "T-placement",
            "occurrence-one",
            b"signed-frame",
        )
        .unwrap();
        assert_eq!(request.decode_frame().unwrap(), b"signed-frame");
        let mut value = serde_json::to_value(&request).unwrap();
        value["ambient_url"] = serde_json::json!("https://wrong.invalid");
        assert!(serde_json::from_value::<ExternalChannelExchangeRequest>(value).is_err());

        let frame = ExternalChannelResponseFrame::new(
            1,
            lillux::sha256_hex(b"signed-frame"),
            b"signed-frame",
        )
        .unwrap();
        assert_eq!(frame.decode_frame().unwrap(), b"signed-frame");
        let invalid = ExternalChannelResponseFrame {
            frame_base64: "not canonical".into(),
            ..frame
        };
        assert!(invalid.decode_frame().is_err());
    }

    #[test]
    fn controller_transport_is_canonical_and_bootstrap_pins_every_binding_coordinate() {
        let bootstrap = bootstrap();
        bootstrap.validate_at(1_000_000).unwrap();
        assert_eq!(
            bootstrap.controller.attach_url().unwrap().as_str(),
            "https://controller.example:7443/external-execution/channel/attach"
        );
        assert_eq!(
            bootstrap.controller.exchange_url().unwrap().as_str(),
            "https://controller.example:7443/external-execution/channel/exchange"
        );
        let supervisor_public_key = super::super::encode_channel_public_key(
            &lillux::crypto::SigningKey::from_bytes(&[43; 32]).verifying_key(),
        )
        .unwrap();
        let binding = ExecutionChannelBinding {
            schema: 2,
            placement_thread_id: bootstrap.placement_thread_id.clone(),
            allocation_request_digest: bootstrap.allocation_request_digest.clone(),
            occurrence_id: bootstrap.occurrence_id.clone(),
            admitted_capsule_hash: bootstrap.admitted_capsule_hash.clone(),
            base_snapshot_hash: bootstrap.base_snapshot_hash.clone(),
            execution_binding_hash: bootstrap.execution_binding_hash.clone(),
            supervisor_runtime_hash: bootstrap.supervisor_runtime_hash.clone(),
            candidate_program_digest: bootstrap.candidate_program.digest().unwrap(),
            channel_nonce: "f".repeat(64),
            owner_public_key: bootstrap.owner_public_key.clone(),
            supervisor_public_key: supervisor_public_key.clone(),
            issued_at_ms: 1_000_000,
            execution_deadline_ms: 1_060_000,
            expires_at_ms: 1_180_000,
            max_frames: bootstrap.binding_max_frames().unwrap(),
            max_bytes: bootstrap.channel_max_bytes,
        };
        let response = ExternalChannelAttachResponse {
            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            binding_digest: binding.digest().unwrap(),
            binding,
        };
        response
            .validate_for_bootstrap(&bootstrap, &supervisor_public_key)
            .unwrap();

        let mut changed: ExternalSupervisorBootstrap =
            serde_json::from_value(serde_json::to_value(&bootstrap).unwrap()).unwrap();
        changed.controller.https_origin = "https://controller.example:443".into();
        assert!(changed.validate().is_err());
        let mut changed: ExternalSupervisorBootstrap =
            serde_json::from_value(serde_json::to_value(&bootstrap).unwrap()).unwrap();
        changed.tls_root_certificates_der_base64 = vec![STANDARD.encode(b"other root")];
        assert!(changed.validate().is_err());
        let mut changed: ExternalSupervisorBootstrap =
            serde_json::from_value(serde_json::to_value(&bootstrap).unwrap()).unwrap();
        changed
            .candidate_program
            .requirement
            .runtime_recipe
            .arguments
            .push("--changed".into());
        assert!(changed.validate().is_err());
        let mut changed_binding = response.binding.clone();
        changed_binding.owner_public_key = super::super::encode_channel_public_key(
            &lillux::crypto::SigningKey::from_bytes(&[44; 32]).verifying_key(),
        )
        .unwrap();
        assert!(
            bootstrap
                .validate_attached_binding(&changed_binding, &supervisor_public_key)
                .is_err()
        );
        let mut changed_binding = response.binding.clone();
        changed_binding.candidate_program_digest = "0".repeat(64);
        assert!(
            bootstrap
                .validate_attached_binding(&changed_binding, &supervisor_public_key)
                .is_err()
        );
    }
}
