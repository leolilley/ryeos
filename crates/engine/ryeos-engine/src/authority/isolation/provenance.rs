use std::collections::{BTreeMap, BTreeSet};

use ryeos_isolation_protocol::{
    ISOLATION_ADAPTER_PROTOCOL, InspectedArtifact, IsolationAdapterProtocolVersion,
    IsolationArtifactRole, IsolationBackendSelection, IsolationCapability, IsolationPlan,
};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use super::{IsolationBackendStatus, IsolationMode};
use crate::error::EngineError;

const ISOLATION_ADAPTER_PROTOCOL_IDENTITY_PREFIX: &str = "ryeos.isolation-adapter/v";

/// Canonical identity retained in historical launch provenance.
///
/// Active adapter declarations and requests continue to use
/// [`IsolationAdapterProtocolVersion`], which accepts only the current wire.
/// This type exists solely so an authenticated historical launch can retain
/// the exact protocol generation that actually produced it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IsolationAdapterProtocolIdentity(String);

impl IsolationAdapterProtocolIdentity {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn parse(value: String) -> Result<Self, String> {
        let version = value
            .strip_prefix(ISOLATION_ADAPTER_PROTOCOL_IDENTITY_PREFIX)
            .ok_or_else(|| {
                "isolation adapter protocol identity has the wrong family".to_string()
            })?;
        if version.is_empty()
            || !version.bytes().all(|byte| byte.is_ascii_digit())
            || version.starts_with('0')
        {
            return Err(
                "isolation adapter protocol identity requires a canonical positive u32 version"
                    .to_string(),
            );
        }
        let parsed = version
            .parse::<u32>()
            .map_err(|_| "isolation adapter protocol identity version exceeds u32".to_string())?;
        if parsed == 0 || parsed.to_string() != version {
            return Err(
                "isolation adapter protocol identity requires a canonical positive u32 version"
                    .to_string(),
            );
        }
        Ok(Self(value))
    }
}

impl From<IsolationAdapterProtocolVersion> for IsolationAdapterProtocolIdentity {
    fn from(version: IsolationAdapterProtocolVersion) -> Self {
        match version {
            IsolationAdapterProtocolVersion::Current => {
                Self(ISOLATION_ADAPTER_PROTOCOL.to_string())
            }
        }
    }
}

impl Serialize for IsolationAdapterProtocolIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for IsolationAdapterProtocolIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

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
    pub adapter_protocol: Option<IsolationAdapterProtocolIdentity>,
    pub payloads: BTreeMap<IsolationArtifactRole, InspectedArtifact>,
    pub effective_capabilities: BTreeSet<IsolationCapability>,
    /// Actually qualified node-generation guarantees, not merely implemented
    /// backend features. The policy digest binds the selected configuration;
    /// process attachment separately retains the exact allocated scope.
    pub process_scope_capabilities: BTreeSet<lillux::ProcessScopeCapability>,
    /// Sealed target-local network inputs in this resolved node generation.
    /// These are local admission facts, not portable program dependencies.
    /// An isolated-network launch receives none of these files; its concrete
    /// mount selection remains committed by plan_digest.
    pub network_runtime_files: BTreeMap<std::path::PathBuf, String>,
    pub plan_digest: Option<String>,
}

impl IsolationLaunchProvenance {
    /// Project one concrete launch onto the node-owned isolation class that
    /// can be promised before a target process is compiled. The redacted plan
    /// digest is deliberately excluded: it is an attempt fact, while every
    /// other field identifies the retained policy/backend generation and its
    /// effective capabilities.
    pub fn admission_class(&self) -> Self {
        Self {
            plan_digest: None,
            ..self.clone()
        }
    }

    /// Require two launch records to belong to the same node isolation class.
    /// A preflight class has no plan digest; a later concrete attempt may have
    /// one without changing the class promised before authority moved.
    pub fn has_same_admission_class(&self, other: &Self) -> bool {
        self.admission_class() == other.admission_class()
    }
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
pub struct IsolationRequestAwaitingAttachment {
    request: lillux::SubprocessRequest,
    scope: Option<lillux::ProcessScope>,
}

impl IsolationRequestAwaitingAttachment {
    pub(super) fn new(
        request: lillux::SubprocessRequest,
        scope: Option<lillux::ProcessScope>,
    ) -> Self {
        Self { request, scope }
    }

    /// Consume the typed isolation result through Lillux's matching lifecycle
    /// operation. The raw request is never exposed, so a disabled-isolation
    /// direct launch cannot be silently downgraded to ordinary spawn.
    pub fn spawn(self) -> Result<lillux::ProcessAwaitingAttachment, lillux::SubprocessResult> {
        // The concrete scope was validated and retained BEFORE compiling the
        // plan. It cannot be swapped or omitted at this last launch seam.
        // Its higher owner separately journals recovery through any error.
        match self.scope {
            Some(scope) => scope
                .spawn_awaiting_attachment(self.request)
                .map_err(|error| error.result),
            None => lillux::spawn_awaiting_attachment(self.request),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance(protocol: &str) -> IsolationLaunchProvenance {
        IsolationLaunchProvenance {
            policy_digest: None,
            mode: IsolationMode::Enforce,
            backend: None,
            backend_status: IsolationBackendStatus::Available,
            bundle_manifest_digest: None,
            signer_fingerprint: None,
            adapter_digest: None,
            adapter_protocol: Some(serde_json::from_value(protocol.into()).unwrap()),
            payloads: BTreeMap::new(),
            effective_capabilities: BTreeSet::new(),
            process_scope_capabilities: BTreeSet::new(),
            network_runtime_files: BTreeMap::new(),
            plan_digest: Some("sha256:attempt".to_string()),
        }
    }

    #[test]
    fn historical_adapter_protocol_identity_round_trips_exactly() {
        let historical: IsolationAdapterProtocolIdentity =
            serde_json::from_str(r#""ryeos.isolation-adapter/v8""#).unwrap();
        assert_eq!(historical.as_str(), "ryeos.isolation-adapter/v8");
        assert_eq!(
            serde_json::to_string(&historical).unwrap(),
            r#""ryeos.isolation-adapter/v8""#
        );

        let current =
            IsolationAdapterProtocolIdentity::from(IsolationAdapterProtocolVersion::Current);
        assert_eq!(
            serde_json::to_value(&current).unwrap(),
            serde_json::to_value(IsolationAdapterProtocolVersion::Current).unwrap()
        );
    }

    #[test]
    fn historical_adapter_protocol_identity_requires_canonical_positive_u32() {
        for refused in [
            "ryeos.isolation-adapter/v",
            "ryeos.isolation-adapter/v0",
            "ryeos.isolation-adapter/v01",
            "ryeos.isolation-adapter/v+1",
            "ryeos.isolation-adapter/v-1",
            "ryeos.isolation-adapter/v 1",
            "ryeos.isolation-adapter/v4294967296",
            "ryeos.isolation-adapter/v1/extra",
            "other.isolation-adapter/v8",
        ] {
            assert!(
                serde_json::from_value::<IsolationAdapterProtocolIdentity>(refused.into()).is_err(),
                "noncanonical protocol identity unexpectedly admitted: {refused}"
            );
        }
        for admitted in [
            "ryeos.isolation-adapter/v1",
            "ryeos.isolation-adapter/v8",
            "ryeos.isolation-adapter/v9",
            "ryeos.isolation-adapter/v4294967295",
        ] {
            let identity: IsolationAdapterProtocolIdentity =
                serde_json::from_value(admitted.into()).unwrap();
            assert_eq!(identity.as_str(), admitted);
        }
    }

    #[test]
    fn admission_class_keeps_exact_historical_protocol_identity() {
        let historical = provenance("ryeos.isolation-adapter/v8");
        let mut same_class = historical.clone();
        same_class.plan_digest = Some("sha256:another-attempt".to_string());
        assert!(historical.has_same_admission_class(&same_class));

        let current = provenance("ryeos.isolation-adapter/v9");
        assert!(!historical.has_same_admission_class(&current));
    }
}
