use std::collections::{BTreeMap, BTreeSet};

use ryeos_isolation_protocol::{
    InspectedArtifact, IsolationAdapterProtocolVersion, IsolationArtifactRole,
    IsolationBackendSelection, IsolationCapability, IsolationPlan,
};
use serde::{Deserialize, Serialize};

use super::{IsolationBackendStatus, IsolationMode};
use crate::error::EngineError;

/// Secret-free identity of the exact isolation generation and compiled plan
/// used for one launch. Managed execution persists this in its launch ledger;
/// all other paths emit it to their audit surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationLaunchProvenance {
    pub policy_digest: Option<String>,
    pub mode: IsolationMode,
    pub backend: Option<IsolationBackendSelection>,
    pub backend_status: IsolationBackendStatus,
    pub bundle_manifest_digest: Option<String>,
    pub signer_fingerprint: Option<String>,
    pub adapter_digest: Option<String>,
    pub adapter_protocol: Option<IsolationAdapterProtocolVersion>,
    pub payloads: BTreeMap<IsolationArtifactRole, InspectedArtifact>,
    pub effective_capabilities: BTreeSet<IsolationCapability>,
    pub plan_digest: Option<String>,
}

pub struct AppliedIsolationLaunch {
    pub request: lillux::SubprocessRequest,
    pub provenance: IsolationLaunchProvenance,
}

/// A subprocess request compiled specifically for a launch that must remain
/// unable to execute user code until its exact process identity is durably
/// attached.
///
/// The inner request is intentionally private: callers must explicitly consume
/// this type when handing it to Lillux's attachment-aware spawn path instead of
/// accidentally passing it to an ordinary spawn API.
pub struct IsolationRequestAwaitingAttachment(lillux::SubprocessRequest);

impl IsolationRequestAwaitingAttachment {
    pub(super) fn new(request: lillux::SubprocessRequest) -> Self {
        Self(request)
    }

    /// Consume the typed isolation result through Lillux's matching lifecycle
    /// operation. The raw request is never exposed, so a disabled-isolation
    /// direct launch cannot be silently downgraded to ordinary spawn.
    pub fn spawn(self) -> Result<lillux::ProcessAwaitingAttachment, lillux::SubprocessResult> {
        lillux::spawn_awaiting_attachment(self.0)
    }
}

/// Provenance paired with an attachment-required subprocess request.
pub struct AppliedIsolationLaunchAwaitingAttachment {
    pub request: IsolationRequestAwaitingAttachment,
    pub provenance: IsolationLaunchProvenance,
}

pub(super) fn redacted_plan_digest(plan: &IsolationPlan) -> Result<String, EngineError> {
    let mut value =
        serde_json::to_value(plan).map_err(|error| EngineError::IsolationPolicyRefused {
            reason: format!("serialize isolation plan for audit: {error}"),
        })?;
    if let Some(arguments) = value
        .get_mut("target")
        .and_then(|target| target.get_mut("arguments"))
        .and_then(serde_json::Value::as_array_mut)
    {
        for argument in arguments {
            *argument = serde_json::Value::String("<redacted>".to_string());
        }
    }
    if let Some(environment) = value
        .get_mut("environment")
        .and_then(|environment| environment.get_mut("values"))
        .and_then(serde_json::Value::as_object_mut)
    {
        for value in environment.values_mut() {
            *value = serde_json::Value::String("<redacted>".to_string());
        }
    }
    let canonical =
        lillux::canonical_json(&value).map_err(|error| EngineError::IsolationPolicyRefused {
            reason: format!("canonicalize isolation plan audit: {error}"),
        })?;
    Ok(format!(
        "sha256:{}",
        lillux::sha256_hex(canonical.as_bytes())
    ))
}
