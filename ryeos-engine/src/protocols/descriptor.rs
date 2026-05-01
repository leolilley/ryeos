//! Protocol descriptor — the signed YAML shape for the `protocol` kind.

use serde::{Deserialize, Serialize};

use crate::protocol_vocabulary::{
    CallbackChannel, EnvInjection, LifecycleMode, ProtocolCapabilities,
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

    /// Dispatch capability bits (replaces `DispatchCapabilities`).
    pub capabilities: ProtocolCapabilities,

    /// Lifecycle expectations.
    pub lifecycle: ProtocolLifecycle,

    /// Callback channel kind.
    pub callback_channel: CallbackChannel,
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
