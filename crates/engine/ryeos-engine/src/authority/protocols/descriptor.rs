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

    /// Optional bidirectional request/response channel carried independently
    /// of stdio. Absence means this is an ordinary one-shot protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<PersistentSessionProtocol>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionProtocol {
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
    if descriptor.callback_channel != CallbackChannel::Http
        || descriptor.stdin.shape != StdinShape::MethodCallEnvelope
        || descriptor.stdout.shape != StdoutShape::MethodCallResult
        || descriptor.stdout.mode != StdoutMode::Terminal
        || descriptor.lifecycle.mode != LifecycleMode::Managed
        || !injects_thread_auth
    {
        return Err(format!(
            "must declare http callbacks, method_call_envelope stdin, terminal method_call_result stdout, managed lifecycle, and RYEOSD_THREAD_AUTH_TOKEN from the thread_auth_token source; got callback={:?}, stdin={:?}, stdout={:?}/{:?}, lifecycle={:?}, canonical_thread_auth_binding={injects_thread_auth}",
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
    {
        return Err("persistent session descriptor is not canonical or bounded".to_owned());
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            },
            lifecycle: ProtocolLifecycle {
                mode: LifecycleMode::Managed,
            },
            callback_channel: CallbackChannel::Http,
            session: None,
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
}
