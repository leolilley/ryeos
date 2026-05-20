//! Daemon-resolved, frozen provider config the runtime consumes.
//!
//! The daemon resolves model_routing → ProviderConfig once at preflight,
//! validates it, and packages the result into this struct. The struct
//! is serialized into the launch envelope; the runtime deserializes
//! and uses it directly without re-touching disk.
//!
//! This closes two issues:
//!   (a) TOCTOU between preflight secret narrowing and runtime resolve.
//!   (b) Untrusted project-local provider YAML cannot redirect injected
//!       vault secrets — the daemon enforces trusted-source policy at
//!       resolution time and the snapshot is the only thing the runtime
//!       sees.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model_resolution::ProviderConfig;

/// Frozen, daemon-resolved provider config + provenance.
///
/// `config_hash` is sha256 over the canonical-JSON serialization of
/// `provider`. The daemon embeds this in the envelope so the runtime
/// can log/verify what it received.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedProviderSnapshot {
    pub provider_id: String,
    pub model_name: String,
    pub context_window: u64,
    /// Profile name that matched (or `None` if base config was used).
    pub matched_profile: Option<String>,
    /// Source root that supplied the provider config: `"system"` |
    /// `"user"` | `"project"`. Used by both daemon (to enforce trust
    /// policy) and runtime (to log).
    pub source_root: String,
    /// SHA-256 of canonical-JSON `provider`. Used only for diagnostics
    /// — the runtime does not re-validate, the daemon already did.
    pub config_hash: String,
    /// The fully-resolved (profile-merged, validated) provider config.
    pub provider: ProviderConfig,
}

impl ResolvedProviderSnapshot {
    /// Compute config_hash from `provider`.
    pub fn compute_hash(provider: &ProviderConfig) -> String {
        // serde_json with sorted keys = canonical-enough for hashing.
        let canonical = serde_json::to_string(provider)
            .unwrap_or_else(|_| String::new());
        let mut h = Sha256::new();
        h.update(canonical.as_bytes());
        format!("{:x}", h.finalize())
    }
}
