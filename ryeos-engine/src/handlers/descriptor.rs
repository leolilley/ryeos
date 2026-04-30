//! Handler descriptors — bundle-shipped, signed YAML items declaring
//! a verified binary that implements a parser or composer handler
//! protocol. Loaded by `HandlerRegistry::load_base` via the Layer-1
//! raw signed-YAML loader; never goes through ParserDispatcher.
//!
//! Mirrors `runtime_registry::RuntimeYaml` shape but with a
//! distinct ABI namespace (SUPPORTED_HANDLER_ABI_VERSION) and a
//! distinct `serves` value-set (parser | composer).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerDescriptor {
    /// Display category. Mirrors runtime YAML.
    pub category: String,
    /// Item name (matches filename).
    pub name: String,
    /// Discriminator. MUST equal "handler" (validated at load).
    pub kind: String,
    /// Closed set. Validated at load against the engine's known set.
    pub serves: HandlerServes,
    /// Path-style ref into the bundle: `bin/<triple>/<name>`.
    /// Resolved by `resolve_bundle_binary_ref`; manifest-hash and
    /// trust-store verified.
    pub binary_ref: String,
    /// Protocol ABI. MUST equal SUPPORTED_HANDLER_ABI_VERSION.
    pub abi_version: String,
    /// Handler binaries are pure functions — no caller-derived caps.
    /// MUST be empty. Enforced at load.
    #[serde(default)]
    pub required_caps: Vec<String>,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HandlerServes {
    Parser,
    Composer,
}
