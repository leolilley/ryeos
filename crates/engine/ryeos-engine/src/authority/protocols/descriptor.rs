//! Protocol descriptor — the signed YAML shape for the `protocol` kind.

use serde::{Deserialize, Serialize};

use crate::protocol_vocabulary::{
    CallbackChannel, EnvInjection, EnvInjectionSource, LifecycleMode, ProtocolCapabilities,
    StdinShape, StdoutMode, StdoutShape,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolDescriptor {
    /// Discriminator. MUST equal "protocol" (validated at load).
    pub kind: String,

    /// Item name; matches filename stem.
    pub name: String,

    /// Display category. Used to derive the canonical ref:
    /// `protocol:<category>/<name>`.
    pub category: String,

    /// ABI contract version, e.g. "v1". Validated at load against
    /// `SUPPORTED_PROTOCOL_ABI_VERSION`.
    pub abi_version: String,

    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,

    /// Stdin envelope spec.
    pub stdin: ProtocolStdin,

    /// Stdout envelope spec (with mode).
    pub stdout: ProtocolStdout,

    /// Env vars injected by the daemon at spawn time.
    /// Empty list permitted.
    #[serde(default)]
    pub env_injections: Vec<EnvInjection>,

    /// Dispatch capability bits from the protocol descriptor.
    pub capabilities: ProtocolCapabilities,

    /// Lifecycle expectations.
    pub lifecycle: ProtocolLifecycle,

    /// Callback channel kind.
    pub callback_channel: CallbackChannel,

    /// This callback protocol may launch only under an independently admitted
    /// product-qualification purpose. The executor binds the purpose and
    /// narrows its callback bearer before exposing either to the verifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualification_callback: Option<QualificationCallbackAuthority>,

    /// Declares that this qualification callback protocol is the one class of
    /// daemon callback a `captured_execution` tool may receive. The class is
    /// valid only for a qualification callback whose method surface is closed
    /// and named here by digest; the daemon admits it only when that digest
    /// equals the digest of the surface it actually issues for the declared
    /// `qualification_callback` authority. Absent means captured execution
    /// refuses every callback and thread-auth credential on this protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_compatible_callback: Option<CapturedCompatibleCallback>,

    /// Optional bidirectional request/response channel carried independently
    /// of stdio. Absence means this is an ordinary one-shot protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<PersistentSessionProtocol>,

    /// Optional pure projector for execution-specific result/call evidence.
    /// Its owning protocol ref and signed digest are selected from the exact
    /// direct launch artifact identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_evidence: Option<ryeos_handler_protocol::ExecutionEvidenceProjectorDeclWire>,

    /// What a node must hold to run an item under this protocol, as data.
    /// Absent means the protocol asks nothing of the node beyond its ordinary
    /// admission. An item that lists several admissible protocols needs each
    /// of them to declare a holding so the node can pick the one it meets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holding: Option<ProtocolHolding>,
}

/// The guarantees and authorities a node holds while it runs a process under
/// one protocol. The requirements use the node-capability grammar and the
/// names a node already reports; the authorities reuse the isolation and
/// settlement vocabularies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolHolding {
    /// Capabilities a node must report before it may run this protocol.
    pub requires_guarantees: Vec<lillux::process_control::CapabilityRequirement>,
    /// Filesystem authority ceiling the process runs under.
    pub filesystem_authority: crate::isolation::IsolationFilesystemAuthorityCeiling,
    /// What settlement of the process root the node must observe at the end.
    pub root_settlement:
        ryeos_state::external_content::products::qualification::VerifierProcessSettlementAuthority,
}

/// Signed selection of qualification authority, not a Tool-kind shortcut.
/// Local containment and remote occurrence authority are distinct promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum QualificationCallbackAuthority {
    ScopedProducer,
    RemoteConsumer,
}

/// Binding of a captured-compatible callback protocol to its exact closed
/// runtime-method surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedCompatibleCallback {
    /// Lowercase SHA-256 identity of the closed callback method surface the
    /// daemon issues for this protocol's `qualification_callback` authority.
    pub surface_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentSessionChannel {
    InheritedUnixSocket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentSessionFraming {
    U32BeJson,
}

/// Generic ownership/reuse mechanics for a managed session process. This is
/// protocol vocabulary, not an executable-kind or product classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentSessionProcessMode {
    /// A bounded daemon pool may lease the process for independent requests.
    PooledRequests,
    /// One durable session owns the process until drain/termination.
    ExclusiveSession,
}

/// Termination and recovery authority independent of request reuse.
///
/// This is independent of session ownership. In particular, an externally
/// fenced placement incarnation must never be represented as a qualified local
/// process scope merely because both mechanisms serve one exclusive session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentSessionCleanupAuthority {
    /// A pooled process has no dedicated-session cleanup authority.
    NotRequired,
    /// Lillux owns a qualified, recoverable process scope on this node.
    LocalProcessScope,
    /// The signed node policy explicitly admits a trusted worker whose direct
    /// process group is the cleanup boundary. This is an operational lane for
    /// disposable, single-tenant placements; it is not containment and does
    /// not imply descendant-death proof against a hostile worker.
    TrustedProcessGroup,
    /// The node retains authority over every descendant of one dedicated
    /// worker, including ones that leave its process group, through a
    /// containment it has qualified and reports (a cgroup scope or the
    /// worker's retained PID-namespace init). A declared need, answered by
    /// reported capability; it grants no quiescence.
    DescendantContainment,
    /// A protected external supervisor owns one exact deployment occurrence.
    /// This does not claim authority over the provider's physical host.
    ExternalPlacementIncarnation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentSessionWorkspaceAuthority {
    EphemeralScratch,
    RuntimeWorkspace,
}

impl PersistentSessionWorkspaceAuthority {
    pub fn filesystem_ceiling(self) -> crate::isolation::IsolationFilesystemAuthorityCeiling {
        match self {
            Self::EphemeralScratch => {
                crate::isolation::IsolationFilesystemAuthorityCeiling::CapturedExecution
            }
            Self::RuntimeWorkspace => {
                crate::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentSessionNetworkAuthority {
    Isolated,
    NodePolicy,
}

impl PersistentSessionNetworkAuthority {
    pub fn network_ceiling(self) -> crate::isolation::IsolationNetworkAuthorityCeiling {
        match self {
            Self::Isolated => crate::isolation::IsolationNetworkAuthorityCeiling::Isolated,
            Self::NodePolicy => crate::isolation::IsolationNetworkAuthorityCeiling::NodePolicy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionStartupContractSelection {
    pub source_root: String,
    pub member: String,
    pub sha256: String,
}

impl PersistentSessionStartupContractSelection {
    /// Portable selection only; source/member and product authority belong to App.
    pub fn validate(&self) -> Result<(), String> {
        if self.source_root.is_empty()
            || matches!(self.source_root.as_str(), "." | "..")
            || self.source_root.len() > 64
            || !self
                .source_root
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            || self.member.is_empty()
            || self.member.len() > 1024
            || self.member.split('/').any(|p| {
                p.is_empty()
                    || p == "."
                    || p == ".."
                    || !p
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            })
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("startup contract requires a canonical source root/member/digest".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionProtocol {
    pub process_mode: PersistentSessionProcessMode,
    /// Explicit for every session. Admission validates the ownership pairing
    /// before any workload contact; predecessor descriptors cannot acquire a
    /// cleanup default by omission.
    pub cleanup_authority: PersistentSessionCleanupAuthority,
    /// Exact trusted resource cleanup contract, independently admitted by the
    /// node. Omission carries no resource cleanup claim; CPU pooled and
    /// resource-free exclusive protocols retain their existing identities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_cleanup_contract: Option<String>,
    /// Explicit signed source-member selection, not a cleanup or startup proof.
    /// Ordinary init-first products preserve their exact omission identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_contract: Option<PersistentSessionStartupContractSelection>,
    pub workspace_authority: PersistentSessionWorkspaceAuthority,
    pub network_authority: PersistentSessionNetworkAuthority,
    #[serde(default)]
    pub runtime_env_allowlist: Vec<String>,
    /// Optional environment slot carrying a daemon-minted boot identity that
    /// the child must echo in its readiness frame. This binds readiness to the
    /// exact held launch without assigning product meaning to the protocol.
    #[serde(default)]
    pub readiness_identity_env: Option<String>,
    pub channel: PersistentSessionChannel,
    pub channel_env: String,
    pub framing: PersistentSessionFraming,
    pub wire_protocol: String,
    pub wire_version: u32,
    pub max_frame_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolStdin {
    pub shape: StdinShape,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolStdout {
    pub shape: StdoutShape,
    pub mode: StdoutMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolLifecycle {
    pub mode: LifecycleMode,
}

/// Exact contract for `execution.method_dispatch.protocol`. Kept in the
/// engine so boot validation and live executor dispatch cannot drift.
pub fn validate_method_runtime_protocol(descriptor: &ProtocolDescriptor) -> Result<(), String> {
    let injects_thread_auth = descriptor.env_injections.iter().any(|injection| {
        injection.source == EnvInjectionSource::ThreadAuthToken
            && injection.name == "RYEOSD_THREAD_AUTH_TOKEN"
    });
    if descriptor.callback_channel != CallbackChannel::RuntimeSocket
        || descriptor.stdin.shape != StdinShape::MethodCallEnvelope
        || descriptor.stdout.shape != StdoutShape::MethodCallResult
        || descriptor.stdout.mode != StdoutMode::Terminal
        || descriptor.lifecycle.mode != LifecycleMode::Managed
        || !injects_thread_auth
    {
        return Err(format!(
            "must declare runtime_socket callbacks, method_call_envelope stdin, terminal method_call_result stdout, managed lifecycle, and RYEOSD_THREAD_AUTH_TOKEN from the thread_auth_token source; got callback={:?}, stdin={:?}, stdout={:?}/{:?}, lifecycle={:?}, canonical_thread_auth_binding={injects_thread_auth}",
            descriptor.callback_channel,
            descriptor.stdin.shape,
            descriptor.stdout.shape,
            descriptor.stdout.mode,
            descriptor.lifecycle.mode,
        ));
    }
    Ok(())
}

pub fn validate_persistent_session_protocol(
    descriptor: &ProtocolDescriptor,
) -> Result<&PersistentSessionProtocol, String> {
    let session = descriptor
        .session
        .as_ref()
        .ok_or_else(|| "does not declare a persistent session channel".to_owned())?;
    validate_session_resource_cleanup_contract(session)?;
    if descriptor.callback_channel != CallbackChannel::None
        || descriptor.stdin.shape != StdinShape::Opaque
        || descriptor.stdout.shape != StdoutShape::OpaqueBytes
        || descriptor.stdout.mode != StdoutMode::Terminal
        || descriptor.lifecycle.mode != LifecycleMode::Managed
        || descriptor.capabilities.allows_pushed_head
        || descriptor.capabilities.allows_target_site
        || descriptor.capabilities.allows_detached
    {
        return Err(
            "persistent sessions require callback-free opaque managed stdio and no dispatch capabilities"
                .to_owned(),
        );
    }
    if session.channel != PersistentSessionChannel::InheritedUnixSocket
        || !matches!(
            (session.process_mode, session.cleanup_authority),
            (
                PersistentSessionProcessMode::PooledRequests,
                PersistentSessionCleanupAuthority::NotRequired
            ) | (
                PersistentSessionProcessMode::PooledRequests,
                PersistentSessionCleanupAuthority::TrustedProcessGroup
            ) | (
                PersistentSessionProcessMode::ExclusiveSession,
                PersistentSessionCleanupAuthority::LocalProcessScope
            ) | (
                PersistentSessionProcessMode::ExclusiveSession,
                PersistentSessionCleanupAuthority::TrustedProcessGroup
            ) | (
                PersistentSessionProcessMode::ExclusiveSession,
                PersistentSessionCleanupAuthority::DescendantContainment
            ) | (
                PersistentSessionProcessMode::ExclusiveSession,
                PersistentSessionCleanupAuthority::ExternalPlacementIncarnation
            )
        )
        || session.channel_env.is_empty()
        || session.channel_env.len() > 128
        || !session
            .channel_env
            .bytes()
            .enumerate()
            .all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_uppercase() || (index != 0 && byte.is_ascii_digit())
            })
        || session.wire_protocol.is_empty()
        || session.wire_protocol.len() > 128
        || session.wire_protocol.trim() != session.wire_protocol
        || session.wire_protocol.chars().any(char::is_control)
        || session.wire_version == 0
        || session.max_frame_bytes == 0
        || session.max_frame_bytes > 16 * 1024 * 1024
        || descriptor
            .env_injections
            .iter()
            .any(|injection| injection.name == session.channel_env)
        || session.runtime_env_allowlist.len() > 32
        || session
            .runtime_env_allowlist
            .iter()
            .enumerate()
            .any(|(index, name)| {
                name.is_empty()
                    || name.len() > 128
                    || !name.bytes().enumerate().all(|(position, byte)| {
                        byte == b'_'
                            || byte.is_ascii_uppercase()
                            || (position != 0 && byte.is_ascii_digit())
                    })
                    || session.runtime_env_allowlist[..index].contains(name)
                    || name == &session.channel_env
            })
        || session.readiness_identity_env.as_ref().is_some_and(|name| {
            name.is_empty()
                || name.len() > 128
                || !name.bytes().enumerate().all(|(position, byte)| {
                    byte == b'_'
                        || byte.is_ascii_uppercase()
                        || (position != 0 && byte.is_ascii_digit())
                })
                || name == &session.channel_env
                || session.runtime_env_allowlist.contains(name)
        })
    {
        return Err("persistent session descriptor is not canonical or bounded".to_owned());
    }
    Ok(session)
}

/// Shape of a declared holding, and for a session protocol its agreement with
/// the session block: the holding's filesystem authority is the one the
/// workspace authority implies and its root settlement is the one the cleanup
/// authority implies, so the two cannot contradict each other.
pub fn validate_protocol_holding(descriptor: &ProtocolDescriptor) -> Result<(), String> {
    use ryeos_state::external_content::products::qualification::VerifierProcessSettlementAuthority as Settlement;

    let Some(holding) = &descriptor.holding else {
        return Ok(());
    };
    ryeos_state::node_guarantees::validate_required_guarantees(&holding.requires_guarantees)
        .map_err(|error| format!("holding: {error}"))?;
    let Some(session) = &descriptor.session else {
        return Ok(());
    };
    let expected_authority = session.workspace_authority.filesystem_ceiling();
    if holding.filesystem_authority != expected_authority {
        return Err(format!(
            "holding filesystem_authority {:?} contradicts the session workspace authority, which implies {expected_authority:?}",
            holding.filesystem_authority
        ));
    }
    let expected_settlement = match session.cleanup_authority {
        PersistentSessionCleanupAuthority::DescendantContainment => Settlement::ContainmentEmpty,
        PersistentSessionCleanupAuthority::TrustedProcessGroup => {
            Settlement::TrustedProcessGroupAbsent
        }
        PersistentSessionCleanupAuthority::LocalProcessScope => Settlement::ScopeEmpty,
        PersistentSessionCleanupAuthority::NotRequired
        | PersistentSessionCleanupAuthority::ExternalPlacementIncarnation => {
            return Err(format!(
                "holding is not defined for a session whose cleanup authority is {:?}",
                session.cleanup_authority
            ));
        }
    };
    if holding.root_settlement != expected_settlement {
        return Err(format!(
            "holding root_settlement {:?} contradicts the session cleanup authority, which implies {expected_settlement:?}",
            holding.root_settlement
        ));
    }
    Ok(())
}

/// Portable contract shape only; this cannot admit node or driver authority.
pub fn validate_session_resource_cleanup_contract(
    session: &PersistentSessionProtocol,
) -> Result<(), String> {
    if let Some(selection) = &session.startup_contract {
        selection.validate()?;
        if session.cleanup_authority != PersistentSessionCleanupAuthority::TrustedProcessGroup
            || session.process_mode != PersistentSessionProcessMode::PooledRequests
        {
            return Err(
                "startup contract selection requires explicit trusted pooled cleanup ownership"
                    .into(),
            );
        }
    }
    if let Some(digest) = &session.resource_cleanup_contract {
        if session.cleanup_authority != PersistentSessionCleanupAuthority::TrustedProcessGroup
            || digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(
                "resource cleanup contract requires trusted authority and a canonical digest"
                    .to_owned(),
            );
        }
    } else if session.process_mode == PersistentSessionProcessMode::PooledRequests
        && session.cleanup_authority == PersistentSessionCleanupAuthority::TrustedProcessGroup
    {
        return Err(
            "trusted pooled protocol requires an explicit resource cleanup contract".to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_selector_is_canonical_optional_and_trusted_ownership_only() {
        let selection = PersistentSessionStartupContractSelection {
            source_root: "source".into(),
            member: "startup-contract.json".into(),
            sha256: "a".repeat(64),
        };
        selection.validate().unwrap();
        for member in ["", "/absolute", "../parent", "a//b", "a/./b", "a\\b"] {
            let mut altered = selection.clone();
            altered.member = member.into();
            assert!(altered.validate().is_err());
        }
        let mut unknown = serde_json::to_value(&selection).unwrap();
        unknown["role"] = serde_json::json!("self-selected");
        assert!(
            serde_json::from_value::<PersistentSessionStartupContractSelection>(unknown).is_err()
        );
        let mut descriptor = persistent_protocol(
            PersistentSessionProcessMode::PooledRequests,
            PersistentSessionCleanupAuthority::NotRequired,
        );
        assert!(
            serde_json::to_value(&descriptor).unwrap()["session"]
                .get("startup_contract")
                .is_none()
        );
        descriptor.session.as_mut().unwrap().startup_contract = Some(selection.clone());
        assert!(validate_persistent_session_protocol(&descriptor).is_err());
        let mut trusted = persistent_protocol(
            PersistentSessionProcessMode::PooledRequests,
            PersistentSessionCleanupAuthority::TrustedProcessGroup,
        );
        trusted.session.as_mut().unwrap().startup_contract = Some(selection);
        validate_persistent_session_protocol(&trusted).unwrap(); // Portable metadata, not signed source admission.
        trusted.session.as_mut().unwrap().process_mode =
            PersistentSessionProcessMode::ExclusiveSession;
        assert!(validate_persistent_session_protocol(&trusted).is_err());
    }

    #[test]
    fn qualification_callback_authority_is_explicit_and_identity_bearing() {
        let mut descriptor = method_protocol();
        let ordinary = serde_json::to_value(&descriptor).unwrap();
        descriptor.qualification_callback = Some(QualificationCallbackAuthority::ScopedProducer);
        let local = serde_json::to_value(&descriptor).unwrap();
        descriptor.qualification_callback = Some(QualificationCallbackAuthority::RemoteConsumer);
        let remote = serde_json::to_value(&descriptor).unwrap();
        assert_ne!(ordinary, local);
        assert_ne!(local, remote);
        assert_eq!(remote["qualification_callback"], "remote_consumer");
        assert_eq!(
            serde_json::from_value::<ProtocolDescriptor>(remote.clone())
                .unwrap()
                .qualification_callback,
            Some(QualificationCallbackAuthority::RemoteConsumer)
        );
        let mut unsupported = remote;
        unsupported["qualification_callback"] = serde_json::json!("arbitrary_callback");
        assert!(serde_json::from_value::<ProtocolDescriptor>(unsupported).is_err());
        let mut predecessor = ordinary;
        predecessor["requires_qualification_purpose"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ProtocolDescriptor>(predecessor).is_err());
    }

    fn method_protocol() -> ProtocolDescriptor {
        ProtocolDescriptor {
            kind: "protocol".to_string(),
            name: "method_runtime".to_string(),
            category: "ryeos/core".to_string(),
            abi_version: "v1".to_string(),
            description: None,
            stdin: ProtocolStdin {
                shape: StdinShape::MethodCallEnvelope,
            },
            stdout: ProtocolStdout {
                shape: StdoutShape::MethodCallResult,
                mode: StdoutMode::Terminal,
            },
            env_injections: vec![EnvInjection {
                name: "RYEOSD_THREAD_AUTH_TOKEN".to_string(),
                source: EnvInjectionSource::ThreadAuthToken,
            }],
            capabilities: ProtocolCapabilities {
                allows_pushed_head: false,
                allows_target_site: false,
                allows_detached: false,
                pushed_head_requires_declaration: false,
            },
            lifecycle: ProtocolLifecycle {
                mode: LifecycleMode::Managed,
            },
            callback_channel: CallbackChannel::RuntimeSocket,
            qualification_callback: None,
            captured_compatible_callback: None,
            session: None,
            execution_evidence: None,
            holding: None,
        }
    }

    fn persistent_protocol(
        mode: PersistentSessionProcessMode,
        cleanup_authority: PersistentSessionCleanupAuthority,
    ) -> ProtocolDescriptor {
        ProtocolDescriptor {
            kind: "protocol".to_owned(),
            name: "session".to_owned(),
            category: "example".to_owned(),
            abi_version: "v1".to_owned(),
            description: None,
            stdin: ProtocolStdin {
                shape: StdinShape::Opaque,
            },
            stdout: ProtocolStdout {
                shape: StdoutShape::OpaqueBytes,
                mode: StdoutMode::Terminal,
            },
            env_injections: Vec::new(),
            capabilities: ProtocolCapabilities {
                allows_pushed_head: false,
                allows_target_site: false,
                allows_detached: false,
                pushed_head_requires_declaration: false,
            },
            lifecycle: ProtocolLifecycle {
                mode: LifecycleMode::Managed,
            },
            callback_channel: CallbackChannel::None,
            qualification_callback: None,
            captured_compatible_callback: None,
            session: Some(PersistentSessionProtocol {
                process_mode: mode,
                cleanup_authority,
                startup_contract: None,
                resource_cleanup_contract: (mode == PersistentSessionProcessMode::PooledRequests
                    && cleanup_authority == PersistentSessionCleanupAuthority::TrustedProcessGroup)
                    .then(|| "a".repeat(64)),
                workspace_authority: PersistentSessionWorkspaceAuthority::RuntimeWorkspace,
                network_authority: PersistentSessionNetworkAuthority::NodePolicy,
                runtime_env_allowlist: Vec::new(),
                readiness_identity_env: Some("SESSION_BOOT_IDENTITY".to_owned()),
                channel: PersistentSessionChannel::InheritedUnixSocket,
                channel_env: "SESSION_FD".to_owned(),
                framing: PersistentSessionFraming::U32BeJson,
                wire_protocol: "example.session".to_owned(),
                wire_version: 1,
                max_frame_bytes: 4096,
            }),
            execution_evidence: None,
            holding: None,
        }
    }

    #[test]
    fn method_protocol_requires_exact_callback_and_thread_auth_contract() {
        let mut descriptor = method_protocol();
        assert!(validate_method_runtime_protocol(&descriptor).is_ok());

        descriptor.callback_channel = CallbackChannel::None;
        assert!(validate_method_runtime_protocol(&descriptor).is_err());

        descriptor = method_protocol();
        descriptor.env_injections.clear();
        assert!(validate_method_runtime_protocol(&descriptor).is_err());

        descriptor = method_protocol();
        descriptor.env_injections[0].name = "ALTERNATE_THREAD_AUTH_TOKEN".to_string();
        assert!(validate_method_runtime_protocol(&descriptor).is_err());
    }

    #[test]
    fn persistent_session_cleanup_authority_matches_process_ownership() {
        for cleanup in [
            PersistentSessionCleanupAuthority::LocalProcessScope,
            PersistentSessionCleanupAuthority::TrustedProcessGroup,
            PersistentSessionCleanupAuthority::DescendantContainment,
            PersistentSessionCleanupAuthority::ExternalPlacementIncarnation,
        ] {
            assert!(
                validate_persistent_session_protocol(&persistent_protocol(
                    PersistentSessionProcessMode::ExclusiveSession,
                    cleanup,
                ))
                .is_ok()
            );
        }
        // Descendant containment belongs to one dedicated worker, never a pool.
        assert!(
            validate_persistent_session_protocol(&persistent_protocol(
                PersistentSessionProcessMode::PooledRequests,
                PersistentSessionCleanupAuthority::DescendantContainment,
            ))
            .is_err()
        );
        assert!(
            validate_persistent_session_protocol(&persistent_protocol(
                PersistentSessionProcessMode::ExclusiveSession,
                PersistentSessionCleanupAuthority::NotRequired,
            ))
            .is_err()
        );
        assert!(
            validate_persistent_session_protocol(&persistent_protocol(
                PersistentSessionProcessMode::PooledRequests,
                PersistentSessionCleanupAuthority::LocalProcessScope,
            ))
            .is_err()
        );
        assert!(
            validate_persistent_session_protocol(&persistent_protocol(
                PersistentSessionProcessMode::PooledRequests,
                PersistentSessionCleanupAuthority::NotRequired,
            ))
            .is_ok()
        );
        assert!(
            validate_persistent_session_protocol(&persistent_protocol(
                PersistentSessionProcessMode::PooledRequests,
                PersistentSessionCleanupAuthority::TrustedProcessGroup,
            ))
            .is_ok()
        );
        assert!(
            validate_persistent_session_protocol(&persistent_protocol(
                PersistentSessionProcessMode::PooledRequests,
                PersistentSessionCleanupAuthority::ExternalPlacementIncarnation,
            ))
            .is_err()
        );
    }

    #[test]
    fn resource_cleanup_contract_is_explicit_canonical_and_trusted_only() {
        let mut descriptor = persistent_protocol(
            PersistentSessionProcessMode::PooledRequests,
            PersistentSessionCleanupAuthority::TrustedProcessGroup,
        );
        assert!(validate_persistent_session_protocol(&descriptor).is_ok());
        for invalid in [
            None,
            Some(String::new()),
            Some("A".repeat(64)),
            Some("a".repeat(63)),
        ] {
            descriptor
                .session
                .as_mut()
                .unwrap()
                .resource_cleanup_contract = invalid;
            assert!(validate_persistent_session_protocol(&descriptor).is_err());
        }
        descriptor
            .session
            .as_mut()
            .unwrap()
            .resource_cleanup_contract = Some("a".repeat(64));
        descriptor.session.as_mut().unwrap().cleanup_authority =
            PersistentSessionCleanupAuthority::NotRequired;
        assert!(validate_persistent_session_protocol(&descriptor).is_err());
        descriptor.session.as_mut().unwrap().process_mode =
            PersistentSessionProcessMode::ExclusiveSession;
        descriptor.session.as_mut().unwrap().cleanup_authority =
            PersistentSessionCleanupAuthority::LocalProcessScope;
        assert!(validate_persistent_session_protocol(&descriptor).is_err());
    }

    #[test]
    fn resource_free_protocol_cleanup_omission_preserves_serialized_identity() {
        for (mode, cleanup) in [
            (
                PersistentSessionProcessMode::PooledRequests,
                PersistentSessionCleanupAuthority::NotRequired,
            ),
            (
                PersistentSessionProcessMode::ExclusiveSession,
                PersistentSessionCleanupAuthority::TrustedProcessGroup,
            ),
        ] {
            let descriptor = persistent_protocol(mode, cleanup);
            let before = serde_json::to_value(&descriptor).unwrap();
            assert!(before["session"].get("resource_cleanup_contract").is_none());
            let decoded: ProtocolDescriptor = serde_json::from_value(before.clone()).unwrap();
            assert!(validate_persistent_session_protocol(&decoded).is_ok());
            assert_eq!(serde_json::to_value(decoded).unwrap(), before);
        }
    }

    #[test]
    fn persistent_session_cleanup_authority_has_no_omission_default() {
        let descriptor = persistent_protocol(
            PersistentSessionProcessMode::PooledRequests,
            PersistentSessionCleanupAuthority::NotRequired,
        );
        let mut value = serde_json::to_value(descriptor).unwrap();
        value["session"]
            .as_object_mut()
            .unwrap()
            .remove("cleanup_authority");
        assert!(serde_json::from_value::<ProtocolDescriptor>(value).is_err());
    }

    fn holding_value(
        guarantees: serde_json::Value,
        authority: &str,
        settlement: &str,
    ) -> ProtocolHolding {
        serde_json::from_value(serde_json::json!({
            "requires_guarantees": guarantees,
            "filesystem_authority": authority,
            "root_settlement": settlement,
        }))
        .unwrap()
    }

    fn held_session(
        cleanup: PersistentSessionCleanupAuthority,
        workspace: PersistentSessionWorkspaceAuthority,
        holding: ProtocolHolding,
    ) -> ProtocolDescriptor {
        let mut descriptor =
            persistent_protocol(PersistentSessionProcessMode::ExclusiveSession, cleanup);
        descriptor.session.as_mut().unwrap().workspace_authority = workspace;
        descriptor.holding = Some(holding);
        descriptor
    }

    #[test]
    fn a_holding_that_agrees_with_its_session_block_is_valid() {
        let contained = held_session(
            PersistentSessionCleanupAuthority::DescendantContainment,
            PersistentSessionWorkspaceAuthority::EphemeralScratch,
            holding_value(
                serde_json::json!([
                    "isolation:enforced",
                    {"any_of": ["termination:scope", "termination:pid_namespace"]},
                ]),
                "captured_execution",
                "containment_empty",
            ),
        );
        validate_protocol_holding(&contained).unwrap();
        let trusted = held_session(
            PersistentSessionCleanupAuthority::TrustedProcessGroup,
            PersistentSessionWorkspaceAuthority::RuntimeWorkspace,
            holding_value(
                serde_json::json!([
                    {"name": "sessions:trusted_process_group", "accept": ["trusted"]},
                ]),
                "node_policy",
                "trusted_process_group_absent",
            ),
        );
        validate_protocol_holding(&trusted).unwrap();
        let scoped = held_session(
            PersistentSessionCleanupAuthority::LocalProcessScope,
            PersistentSessionWorkspaceAuthority::RuntimeWorkspace,
            holding_value(
                serde_json::json!(["termination:scope"]),
                "node_policy",
                "scope_empty",
            ),
        );
        validate_protocol_holding(&scoped).unwrap();
    }

    #[test]
    fn a_holding_that_contradicts_its_session_block_is_refused() {
        let guarantees = serde_json::json!(["isolation:enforced"]);
        let wrong_authority = held_session(
            PersistentSessionCleanupAuthority::DescendantContainment,
            PersistentSessionWorkspaceAuthority::EphemeralScratch,
            holding_value(guarantees.clone(), "node_policy", "containment_empty"),
        );
        let error = validate_protocol_holding(&wrong_authority).unwrap_err();
        assert!(error.contains("filesystem_authority"), "{error}");
        let wrong_settlement = held_session(
            PersistentSessionCleanupAuthority::DescendantContainment,
            PersistentSessionWorkspaceAuthority::EphemeralScratch,
            holding_value(
                guarantees.clone(),
                "captured_execution",
                "trusted_process_group_absent",
            ),
        );
        let error = validate_protocol_holding(&wrong_settlement).unwrap_err();
        assert!(error.contains("root_settlement"), "{error}");
        let undefined = held_session(
            PersistentSessionCleanupAuthority::ExternalPlacementIncarnation,
            PersistentSessionWorkspaceAuthority::RuntimeWorkspace,
            holding_value(guarantees, "node_policy", "scope_empty"),
        );
        assert!(validate_protocol_holding(&undefined).is_err());
    }

    #[test]
    fn a_holding_needs_bounded_non_empty_guarantees() {
        let mut descriptor = method_protocol();
        descriptor.holding = Some(holding_value(
            serde_json::json!(["isolation:enforced"]),
            "node_policy",
            "trusted_process_group_absent",
        ));
        validate_protocol_holding(&descriptor).unwrap();
        for guarantees in [
            serde_json::json!([]),
            serde_json::json!([""]),
            serde_json::json!(vec!["a"; 17]),
        ] {
            descriptor.holding = Some(holding_value(
                guarantees,
                "node_policy",
                "trusted_process_group_absent",
            ));
            assert!(validate_protocol_holding(&descriptor).is_err());
        }
    }

    #[test]
    fn a_descriptor_without_a_holding_is_valid_and_keeps_its_serialized_identity() {
        let descriptor = method_protocol();
        validate_protocol_holding(&descriptor).unwrap();
        let value = serde_json::to_value(&descriptor).unwrap();
        assert!(value.get("holding").is_none());
    }

    #[test]
    fn a_holding_block_is_closed_and_complete() {
        let mut value = serde_json::to_value(method_protocol()).unwrap();
        value["holding"] = serde_json::json!({
            "requires_guarantees": ["isolation:enforced"],
            "filesystem_authority": "captured_execution",
            "root_settlement": "containment_empty",
        });
        let decoded: ProtocolDescriptor = serde_json::from_value(value.clone()).unwrap();
        assert!(decoded.holding.is_some());
        let mut unknown = value.clone();
        unknown["holding"]["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ProtocolDescriptor>(unknown).is_err());
        for field in [
            "requires_guarantees",
            "filesystem_authority",
            "root_settlement",
        ] {
            let mut missing = value.clone();
            missing["holding"].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<ProtocolDescriptor>(missing).is_err());
        }
    }
}
