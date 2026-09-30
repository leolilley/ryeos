//! Closed JSON wire contract for occurrence bootstrap and frame exchange.
//!
//! Transport completion is not application evidence. These types only prevent
//! the daemon route and protected guest client from interpreting different
//! request/response shapes.

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use url::Url;

use super::admission::AdmittedExternalExecutionProgram;
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
pub const MAX_EXTERNAL_NETWORK_INPUT_BYTES: u64 = 64 * 1024;
/// Two maximally sized base64 inputs plus fixed schema/digest keys and values.
/// The 1024-byte metadata allowance exceeds the canonical fixed overhead.
pub const MAX_EXTERNAL_NETWORK_CAPTURE_JSON_BYTES: usize =
    2 * (MAX_EXTERNAL_NETWORK_INPUT_BYTES as usize).div_ceil(3) * 4 + 1024;

/// Explicit target-local source selection; never ambient discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalNetworkInputSelection {
    pub source: String,
    pub max_bytes: u64,
}

impl ExternalNetworkInputSelection {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.source.starts_with('/')
                && self.source.len() <= 4096
                && !self.source.contains('\0')
                && self.source[1..]
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."),
            "external network input source is not a lexical absolute file path"
        );
        ensure!(
            (1..=MAX_EXTERNAL_NETWORK_INPUT_BYTES).contains(&self.max_bytes),
            "external network input byte bound is invalid"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalNetworkInputPolicy {
    pub resolver: ExternalNetworkInputSelection,
    pub hosts: ExternalNetworkInputSelection,
}

impl ExternalNetworkInputPolicy {
    pub fn validate(&self) -> Result<()> {
        self.resolver.validate()?;
        self.hosts.validate()
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        crate::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.external-network-input-policy.v1",
            "policy": self,
        }))
    }
}

/// Protected occurrence-local bytes. State validates but never reads host files.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCapturedNetworkInputs {
    pub schema: u32,
    pub policy_digest: String,
    pub resolver_base64: String,
    pub hosts_base64: String,
    pub resolver_digest: String,
    pub hosts_digest: String,
}

impl ExternalCapturedNetworkInputs {
    pub fn from_bytes(
        policy: &ExternalNetworkInputPolicy,
        resolver: &[u8],
        hosts: &[u8],
    ) -> Result<Self> {
        policy.validate()?;
        ensure!(
            resolver.len() as u64 <= policy.resolver.max_bytes
                && hosts.len() as u64 <= policy.hosts.max_bytes,
            "external captured network inputs exceed policy bounds"
        );
        let capture = Self {
            schema: 1,
            policy_digest: policy.digest()?,
            resolver_base64: STANDARD.encode(resolver),
            hosts_base64: STANDARD.encode(hosts),
            resolver_digest: lillux::sha256_hex(resolver),
            hosts_digest: lillux::sha256_hex(hosts),
        };
        capture.validate_for(policy)?;
        Ok(capture)
    }

    pub fn validate_for(&self, policy: &ExternalNetworkInputPolicy) -> Result<()> {
        ensure!(
            self.policy_digest == policy.digest()?,
            "external network capture changed its policy"
        );
        ensure!(
            self.resolver_bytes()?.len() as u64 <= policy.resolver.max_bytes
                && self.hosts_bytes()?.len() as u64 <= policy.hosts.max_bytes,
            "external captured network inputs exceed policy bounds"
        );
        Ok(())
    }

    fn decode(&self, encoded: &str, digest: &str) -> Result<Vec<u8>> {
        ensure!(
            self.schema == 1,
            "unsupported external network capture schema"
        );
        hash(&self.policy_digest)?;
        hash(digest)?;
        ensure!(
            encoded.len() <= (MAX_EXTERNAL_NETWORK_INPUT_BYTES as usize).div_ceil(3) * 4,
            "external network capture encoding exceeds bounds"
        );
        let bytes = STANDARD
            .decode(encoded)
            .context("external network capture is invalid base64")?;
        ensure!(
            bytes.len() as u64 <= MAX_EXTERNAL_NETWORK_INPUT_BYTES
                && STANDARD.encode(&bytes) == encoded
                && lillux::sha256_hex(&bytes) == digest,
            "external network capture changed its canonical bytes"
        );
        Ok(bytes)
    }

    pub fn resolver_bytes(&self) -> Result<Vec<u8>> {
        self.decode(&self.resolver_base64, &self.resolver_digest)
    }

    pub fn hosts_bytes(&self) -> Result<Vec<u8>> {
        self.decode(&self.hosts_base64, &self.hosts_digest)
    }

    pub fn digest(&self) -> Result<String> {
        self.resolver_bytes()?;
        self.hosts_bytes()?;
        crate::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.external-captured-network-inputs.v1",
            "capture": self,
        }))
    }
}
/// Complete secret bootstrap ceiling at the protected supervisor boundary.
/// This includes the base64-expanded TLS roots and the admitted runtime recipe,
/// so a bootstrap accepted by the controller is representable by the fixed
/// sealed-descriptor launch contract.
pub const MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES: usize = 512 * 1024;

/// Node-signed, non-secret authority for the controller transport. The actual
/// root certificates are delivered only through the protected supervisor
/// bootstrap and must reproduce `tls_root_bundle_digest` exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalControllerTransportContract {
    pub schema: u32,
    pub network_inputs: ExternalNetworkInputPolicy,
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
            self.schema == 2,
            "unsupported external controller transport schema"
        );
        self.network_inputs.validate()?;
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
    /// Exact installed launcher executable selected by the signed placement
    /// generation. Observing a launcher digest after launch is not admission.
    pub launcher_artifact_hash: String,
    pub candidate_program: AdmittedExternalExecutionProgram,
    pub guest_input_identity: String,
    pub guest_inputs: ryeos_external_execution_contract::ExternalGuestInputProjection,
    pub owner_public_key: String,
    pub bootstrap_capability: String,
    pub attachment_deadline_ms: i64,
    pub execution_timeout_seconds: u32,
    pub post_execution_timeout_seconds: u32,
    /// Raw candidate-content ceiling. The separate channel ceiling includes
    /// authenticated framing and transfer encoding overhead.
    pub candidate_export_max_bytes: u64,
    pub channel_max_bytes: u64,
}

impl ExternalSupervisorBootstrap {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 7,
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
            &self.launcher_artifact_hash,
            &self.guest_input_identity,
        ] {
            hash(digest)?;
        }
        self.candidate_program
            .validate_guest_inputs(&self.guest_inputs)?;
        ensure!(
            self.candidate_program.runtime_manifest_hash()? == self.supervisor_runtime_hash,
            "external supervisor program changed its runtime manifest"
        );
        if let AdmittedExternalExecutionProgram::DirectCommand(program) = &self.candidate_program {
            ensure!(
                program.projection().endpoint_binding_digest == self.execution_binding_hash
                    && u64::from(self.execution_timeout_seconds)
                        <= program.projection().timeout_seconds,
                "external supervisor changed its direct endpoint or widened its tool timeout"
            );
        }
        ensure!(
            self.guest_inputs.base_snapshot.snapshot_hash == self.base_snapshot_hash
                && self.guest_inputs.identity_digest()? == self.guest_input_identity,
            "external supervisor guest inputs changed their retained identity"
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
        match self.candidate_program.execution_mode() {
            ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {} => {
                ensure!(
                    (1..=self.channel_max_bytes).contains(&self.candidate_export_max_bytes),
                    "external session supervisor export bound is invalid"
                );
            }
            ryeos_external_execution_contract::ExternalExecutionMode::DirectCommand { .. } => {
                ensure!(
                    self.candidate_export_max_bytes == 0,
                    "external direct supervisor has no candidate export authority"
                );
            }
        }
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
        let bytes = lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes();
        ensure!(
            bytes.len() <= MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
            "external supervisor bootstrap exceeds its sealed descriptor bound"
        );
        Ok(bytes)
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
                && binding.execution_mode == self.candidate_program.execution_mode()
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
                && binding.candidate_export_max_bytes == self.candidate_export_max_bytes
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
        AdmittedExternalCandidateProgram, ExternalCandidateProcFilesystem,
        ExternalCandidateRequirement, ExternalCandidateRuntimeRecipe, PROTOCOL,
    };

    fn controller(roots: &[String]) -> ExternalControllerTransportContract {
        ExternalControllerTransportContract {
            schema: 2,
            network_inputs: network_policy(),
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
            schema: 2,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::new(),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        };
        let runtime_recipe_digest = runtime_recipe.digest().unwrap();
        let guest_inputs = ryeos_external_execution_contract::ExternalGuestInputProjection {
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: ryeos_external_execution_contract::GuestBaseSnapshotInput {
                descriptor: 55,
                snapshot_hash: "c".repeat(64),
                closure_digest: "5".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![ryeos_external_execution_contract::GuestMountInput {
                role: ryeos_external_execution_contract::GuestMountRole::Product,
                authority_id: "runtime".into(),
                descriptor: 64,
                destination: "/runtime".into(),
                kind: ryeos_external_execution_contract::GuestMountKind::Directory,
                access: ryeos_external_execution_contract::GuestMountAccess::ReadOnly,
                normalized_mode: None,
                content_authority:
                    ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                        manifest_kind:
                            ryeos_external_execution_contract::GuestProductManifestKind::Content,
                        manifest_hash: "e".repeat(64),
                        manifest_descriptor: 65,
                        manifest_bytes: 256,
                    },
                bytes: 1,
            }],
            executable_search: vec!["/runtime/bin".into()],
            environment: BTreeMap::new(),
        };
        let guest_input_identity = guest_inputs.identity_digest().unwrap();
        let requirement = ExternalCandidateRequirement {
            schema: 6,
            protocol: PROTOCOL.into(),
            connector_protocol: crate::external_execution::admission::CONNECTOR_PROTOCOL.into(),
            execution_route:
                crate::external_execution::admission::ExternalCandidateExecutionRoute::ConnectorOnly,
            required_lifecycle_capabilities: std::collections::BTreeSet::new(),
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "runtime".into(),
            runtime_recipe,
        };
        let qualification_use =
            crate::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        ExternalSupervisorBootstrap {
            schema: 7,
            controller: controller(&roots),
            tls_root_certificates_der_base64: roots,
            placement_thread_id: "T-placement".into(),
            occurrence_id: "occurrence-one".into(),
            allocation_request_digest: "a".repeat(64),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            launcher_artifact_hash: "4".repeat(64),
            candidate_program: AdmittedExternalCandidateProgram {
                requirement,
                qualification_use,
                runtime_manifest_kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
                runtime_manifest_hash: "e".repeat(64),
                runtime_source: crate::external_execution::admission::ExternalCandidateRuntimeSource::CapturedProduct { witness_hash: "1".repeat(64) },
                qualification_attestation_hash: "2".repeat(64),
                selection_identity_digest: "3".repeat(64),
                runtime_recipe_digest,
            }
            .into(),
            guest_input_identity,
            guest_inputs,
            owner_public_key: super::super::encode_channel_public_key(
                &lillux::crypto::SigningKey::from_bytes(&[41; 32]).verifying_key(),
            )
            .unwrap(),
            bootstrap_capability: STANDARD.encode([42_u8; 32]),
            attachment_deadline_ms: 2_000_000,
            execution_timeout_seconds: 60,
            post_execution_timeout_seconds: 120,
            candidate_export_max_bytes: 512 * 1024,
            channel_max_bytes: 1024 * 1024,
        }
    }

    fn network_policy() -> ExternalNetworkInputPolicy {
        ExternalNetworkInputPolicy {
            resolver: ExternalNetworkInputSelection {
                source: "/etc/resolv.conf".into(),
                max_bytes: 65536,
            },
            hosts: ExternalNetworkInputSelection {
                source: "/etc/hosts".into(),
                max_bytes: 65536,
            },
        }
    }

    #[test]
    fn network_policy_is_required_closed_and_bounded() {
        let controller = controller(&[STANDARD.encode(b"fixture root")]);
        let mut value = serde_json::to_value(&controller).unwrap();
        value.as_object_mut().unwrap().remove("network_inputs");
        assert!(serde_json::from_value::<ExternalControllerTransportContract>(value).is_err());
        let mut value = serde_json::to_value(network_policy()).unwrap();
        value["ambient"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ExternalNetworkInputPolicy>(value).is_err());
        for field in ["resolver", "hosts"] {
            let mut value = serde_json::to_value(network_policy()).unwrap();
            value.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<ExternalNetworkInputPolicy>(value).is_err());
            let mut value = serde_json::to_value(network_policy()).unwrap();
            value[field]["ambient"] = serde_json::json!(true);
            assert!(serde_json::from_value::<ExternalNetworkInputPolicy>(value).is_err());
        }
        for path in [
            "",
            "/",
            "etc/hosts",
            "/etc/../hosts",
            "/etc/./hosts",
            "/etc//hosts",
            "/etc/hosts/",
            "/etc/hosts\0",
        ] {
            let mut policy = network_policy();
            policy.resolver.source = path.into();
            assert!(policy.validate().is_err(), "accepted {path:?}");
        }
        for max_bytes in [0, 65537, u64::MAX] {
            let mut policy = network_policy();
            policy.hosts.max_bytes = max_bytes;
            assert!(policy.validate().is_err());
        }
        let mut predecessor = controller;
        predecessor.schema = 1;
        assert!(predecessor.validate().is_err());
        let mut predecessor = bootstrap();
        predecessor.schema = 5;
        assert!(predecessor.validate().is_err());
    }

    #[test]
    fn network_capture_binds_exact_policy_and_canonical_bounded_bytes() {
        let policy = network_policy();
        let maximum =
            ExternalCapturedNetworkInputs::from_bytes(&policy, &vec![0; 65536], &vec![0; 65536])
                .unwrap();
        assert!(
            lillux::canonical_json(&serde_json::to_value(maximum).unwrap())
                .unwrap()
                .len()
                <= MAX_EXTERNAL_NETWORK_CAPTURE_JSON_BYTES
        );
        let capture = ExternalCapturedNetworkInputs::from_bytes(
            &policy,
            b"nameserver 127.0.0.1\n",
            b"127.0.0.1 localhost\n",
        )
        .unwrap();
        capture.validate_for(&policy).unwrap();
        assert_eq!(capture.resolver_bytes().unwrap(), b"nameserver 127.0.0.1\n");
        assert_eq!(capture.hosts_bytes().unwrap(), b"127.0.0.1 localhost\n");
        let mut changed_policy = policy.clone();
        changed_policy.resolver.source = "/selected/resolv.conf".into();
        assert!(capture.validate_for(&changed_policy).is_err());
        changed_policy = policy.clone();
        changed_policy.hosts.max_bytes -= 1;
        assert!(capture.validate_for(&changed_policy).is_err());
        let mut changed = capture.clone();
        changed.schema = 2;
        assert!(changed.digest().is_err());
        for encoded in [
            "eA".to_owned(),
            "eB==".to_owned(),
            "!".repeat(100_000),
            STANDARD.encode(b"changed"),
        ] {
            let mut changed = capture.clone();
            changed.resolver_base64 = encoded;
            assert!(changed.validate_for(&policy).is_err());
        }
        let mut value = serde_json::to_value(&capture).unwrap();
        value["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ExternalCapturedNetworkInputs>(value).is_err());
        assert!(ExternalCapturedNetworkInputs::from_bytes(&policy, &vec![0; 65537], b"").is_err());
        assert!(ExternalCapturedNetworkInputs::from_bytes(&policy, b"", &vec![0; 65537]).is_err());
        ExternalCapturedNetworkInputs::from_bytes(&policy, &vec![0; 65536], b"").unwrap();
        let mut small = policy.clone();
        small.hosts.max_bytes = 1;
        assert!(ExternalCapturedNetworkInputs::from_bytes(&small, b"", b"ab").is_err());
        assert_ne!(
            capture.digest().unwrap(),
            ExternalCapturedNetworkInputs::from_bytes(&policy, b"other", b"")
                .unwrap()
                .digest()
                .unwrap()
        );
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
    fn direct_bootstrap_requires_zero_export_and_exact_mode_endpoint_and_timeout() {
        use crate::external_execution::admission::ExternalDirectSealedInput;
        use ryeos_external_execution_contract::ExternalExecutionMode;

        let mut direct = bootstrap();
        direct.candidate_export_max_bytes = 0;
        direct.guest_inputs.inputs[0].destination = "/ryeos/realizations/runtime".into();
        direct.guest_inputs.executable_search.clear();
        direct.guest_input_identity = direct.guest_inputs.identity_digest().unwrap();
        // This is a transport-wire fixture, not evidence of app compilation or
        // allocation admission. Identity joins are all this test claims.
        direct.candidate_program = serde_json::from_value(serde_json::json!({
            "kind":"direct_command", "program": {
                "execution_plan_hash":"1".repeat(64), "execution_closure_digest":"2".repeat(64),
                "command":{"authority":"realization_member","executable_blob_hash":"3".repeat(64),
                    "realization_id":"runtime","realization_manifest_hash":direct.supervisor_runtime_hash,
                    "realization_mount_root":"execution_runtime","realization_mount":"runtime","relative_path":"bin/evaluate",
                    "execution_path":"/ryeos/realizations/runtime/bin/evaluate"},
                "projection":{"argv0":"/ryeos/realizations/runtime/bin/evaluate","arguments":[],"cwd":"/workspace",
                    "environment":direct.guest_inputs.environment,"stdin":ExternalDirectSealedInput::from_bytes(b"").unwrap(),
                    "endpoint_binding_id":"farm-direct","endpoint_binding_digest":direct.execution_binding_hash,
                    "execution_mode":{"kind":"direct_command","stdout_max_bytes":1024,"stderr_max_bytes":2048},
                    "timeout_seconds":60,"native":{"kind":"linux_isolated_read_only","required_arch":"x86_64"}},
                "guest_input_identity":direct.guest_input_identity
            }
        })).unwrap();
        direct.validate().unwrap();
        let supervisor_key = super::super::encode_channel_public_key(
            &lillux::crypto::SigningKey::from_bytes(&[43; 32]).verifying_key(),
        )
        .unwrap();
        let binding = ExecutionChannelBinding {
            schema: crate::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode: direct.candidate_program.execution_mode(),
            placement_thread_id: direct.placement_thread_id.clone(),
            allocation_request_digest: direct.allocation_request_digest.clone(),
            occurrence_id: direct.occurrence_id.clone(),
            admitted_capsule_hash: direct.admitted_capsule_hash.clone(),
            base_snapshot_hash: direct.base_snapshot_hash.clone(),
            execution_binding_hash: direct.execution_binding_hash.clone(),
            supervisor_runtime_hash: direct.supervisor_runtime_hash.clone(),
            candidate_program_digest: direct.candidate_program.digest().unwrap(),
            channel_nonce: "f".repeat(64),
            owner_public_key: direct.owner_public_key.clone(),
            supervisor_public_key: supervisor_key.clone(),
            issued_at_ms: 1_000_000,
            execution_deadline_ms: 1_060_000,
            expires_at_ms: 1_180_000,
            candidate_export_max_bytes: 0,
            max_frames: direct.binding_max_frames().unwrap(),
            max_bytes: direct.channel_max_bytes,
        };
        direct
            .validate_attached_binding(&binding, &supervisor_key)
            .unwrap();
        for mutation in ["mode", "stdout", "stderr", "endpoint", "timeout", "export"] {
            let mut changed = binding.clone();
            match mutation {
                "mode" => {
                    changed.execution_mode = ExternalExecutionMode::StructuredSession {};
                    changed.candidate_export_max_bytes = 1;
                }
                "stdout" => {
                    changed.execution_mode = ExternalExecutionMode::DirectCommand {
                        stdout_max_bytes: 1025,
                        stderr_max_bytes: 2048,
                    }
                }
                "stderr" => {
                    changed.execution_mode = ExternalExecutionMode::DirectCommand {
                        stdout_max_bytes: 1024,
                        stderr_max_bytes: 2049,
                    }
                }
                "endpoint" => changed.execution_binding_hash = "0".repeat(64),
                "timeout" => changed.execution_deadline_ms += 1,
                "export" => changed.candidate_export_max_bytes = 1,
                _ => unreachable!(),
            }
            assert!(
                direct
                    .validate_attached_binding(&changed, &supervisor_key)
                    .is_err(),
                "accepted binding {mutation}"
            );
        }
        let wire = serde_json::to_value(&direct).unwrap();
        for mutation in ["export", "channel", "endpoint", "timeout", "predecessor"] {
            let mut changed: ExternalSupervisorBootstrap =
                serde_json::from_value(wire.clone()).unwrap();
            match mutation {
                "export" => changed.candidate_export_max_bytes = 1,
                "channel" => changed.channel_max_bytes = 0,
                "endpoint" => changed.execution_binding_hash = "0".repeat(64),
                "timeout" => changed.execution_timeout_seconds = 61,
                "predecessor" => changed.schema = 6,
                _ => unreachable!(),
            }
            assert!(changed.validate().is_err(), "accepted bootstrap {mutation}");
        }
        let mut shorter: ExternalSupervisorBootstrap =
            serde_json::from_value(wire.clone()).unwrap();
        shorter.execution_timeout_seconds = 59;
        shorter.validate().unwrap();
        assert!(
            shorter
                .validate_attached_binding(&binding, &supervisor_key)
                .is_err()
        );
        let mut missing_tag = wire.clone();
        missing_tag["candidate_program"]
            .as_object_mut()
            .unwrap()
            .remove("kind");
        assert!(serde_json::from_value::<ExternalSupervisorBootstrap>(missing_tag).is_err());
        let mut untagged = wire;
        untagged["candidate_program"] = untagged["candidate_program"]["program"].clone();
        assert!(serde_json::from_value::<ExternalSupervisorBootstrap>(untagged).is_err());
        let mut session = bootstrap();
        session.validate().unwrap();
        session.candidate_export_max_bytes = 0;
        assert!(session.validate().is_err());
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
            schema: crate::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
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
            candidate_export_max_bytes: bootstrap.candidate_export_max_bytes,
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
            .worker_mut()
            .unwrap()
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
