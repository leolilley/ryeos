use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Trust classification for resolved items (for sandbox profile enforcement and audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustClass {
    /// Item from immutable system bundle.
    TrustedSystem,
    /// Item from user's ~/.ai/.
    TrustedUser,
    /// Item from project or untrusted sources.
    UntrustedUserSpace,
    /// Item not signed or signature invalid.
    Unsigned,
}

/// Record of alias expansion chain for audit and trust analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasHop {
    /// Ordered chain of alias expansions: ["@core", "@base", "directive:rye/agent/core/base"]
    pub expansion: Vec<String>,
    /// Depth used (≤ execution.alias_max_depth).
    pub depth: usize,
}

/// Verified, daemon-side snapshot of a resolved item — collapses what
/// used to be `ChainHop` (resolution metadata) and `ItemPayload`
/// (verified bytes) into a single struct.
///
/// Carries the *exact bytes* the daemon read and verified, plus the
/// signature-derived trust class. Runtimes that receive a
/// `ResolvedAncestor` in `LaunchEnvelope.resolution` MUST treat it as
/// the source of truth — re-reading `source_path` from disk would
/// re-introduce the TOCTOU window the daemon already closed.
///
/// `source_path` is **diagnostic only** — never use it as a structural
/// join key. The previous `ancestor_payloads: HashMap<PathBuf, ...>`
/// indirection has been deleted because it made "ancestor with no
/// payload" representable; the merged struct makes the invariant
/// structural.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedAncestor {
    /// What the parent asked for (raw, possibly an `@alias`).
    pub requested_id: String,
    /// Canonical ref after all alias expansion.
    pub resolved_ref: String,
    /// Resolved on-disk source path (audit / diagnostic only).
    pub source_path: PathBuf,
    /// Trust label: which tier (system/user/project) this came from.
    pub trust_class: TrustClass,
    /// If an alias was involved, the expansion chain.
    pub alias_resolution: Option<AliasHop>,
    /// Which resolution step produced this ancestor.
    pub added_by: ResolutionStepName,
    /// File content with the signature line stripped. Re-parsed by
    /// runtimes / composers using their own format-specific parser.
    pub raw_content: String,
    /// SHA-256 of `raw_content` (signature-stripped, post-strip bytes).
    /// Named explicitly so it can never be confused with the original
    /// file digest — what the runtime parses is `raw_content`, so the
    /// digest that binds the envelope is the digest of those bytes.
    pub raw_content_digest: String,
}

/// Edge in the references DAG (not hierarchical like extends).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionEdge {
    /// Source item's resolved canonical ref.
    pub from_ref: String,
    /// Source item's on-disk canonical path.
    pub from_source_path: PathBuf,
    /// Target item's resolved canonical ref.
    pub to_ref: String,
    /// Target item's on-disk canonical path.
    pub to_source_path: PathBuf,
    /// Trust class of the target (edge-local, subject enforces sandbox based on it).
    pub trust_class: TrustClass,
    /// Which step added this edge.
    pub added_by: ResolutionStepName,
}

/// Name of a resolution step (used for tracking and error messages).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStepName {
    /// Pseudo-step for failures raised before any declared step has run
    /// (e.g. root load, kind lookup). Keeps step attribution honest:
    /// triage points at the actual phase, not at "extends" by default.
    PipelineInit,
    ResolveExtendsChain,
    ResolveReferences,
}

impl std::fmt::Display for ResolutionStepName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolutionStepName::PipelineInit => write!(f, "pipeline_init"),
            ResolutionStepName::ResolveExtendsChain => write!(f, "resolve_extends_chain"),
            ResolutionStepName::ResolveReferences => write!(f, "resolve_references"),
        }
    }
}

/// Daemon-computed composed view for a kind.
///
/// Generic across kinds and across composition strategies: every kind
/// has a registered composer (boot validation guarantees this), so
/// every resolution carries a `KindComposedView`. Engine code never
/// names a kind here.
///
/// Three slots:
///   * `composed` — the merged payload (typically the effective
///     header). Identity-style composition just clones the root parser
///     output into this slot.
///   * `derived` — strategy-derived auxiliary outputs by name (e.g. a
///     composer rule may extract `body` as `String` or
///     `composed_context` as a `Map<String, Vec<String>>`). Names are
///     declared in the kind schema's `composer_config`, NOT
///     hardcoded.
///   * `policy_facts` — daemon-policy values (e.g. `effective_caps`)
///     that the launcher and other policy-side consumers read by name.
///     Path-extracted from `composed` per `composer_config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KindComposedView {
    pub composed: serde_json::Value,
    #[serde(default)]
    pub derived: std::collections::HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub policy_facts: std::collections::HashMap<String, serde_json::Value>,
}

impl KindComposedView {
    /// View used by composers (e.g. `IdentityComposer`) that perform
    /// no merge: `composed` is the root parser output verbatim.
    pub fn identity(composed: serde_json::Value) -> Self {
        Self {
            composed,
            derived: std::collections::HashMap::new(),
            policy_facts: std::collections::HashMap::new(),
        }
    }

    /// Read a `Vec<String>` policy fact by name. Returns an empty
    /// vector when the fact is absent or shaped wrong — the schema
    /// shape is asserted by the composer at compose time, not here.
    pub fn policy_fact_string_seq(&self, name: &str) -> Vec<String> {
        self.policy_facts
            .get(name)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Read a derived string by name.
    pub fn derived_string(&self, name: &str) -> Option<&str> {
        self.derived.get(name).and_then(|v| v.as_str())
    }

    /// Read a derived `Map<String, Vec<String>>` by name.
    pub fn derived_string_seq_map(
        &self,
        name: &str,
    ) -> std::collections::HashMap<String, Vec<String>> {
        self.derived
            .get(name)
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| {
                        let items = v
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (k.clone(), items)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Output of the full resolution pipeline.
///
/// `root` is the resolved item itself; runtimes consume `root.raw_content`
/// as their own item body. `ancestors` is the bottom-up extends chain
/// (root excluded), each ancestor inlining its verified bytes —
/// no separate payload map. `executor_trust_class` is the daemon-computed
/// weakest-link fold over root + ancestors. `composed` carries the
/// daemon-side composed view (Phase 2) when a composer is registered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionOutput {
    /// The root item itself — always populated, even for kinds with no
    /// resolution steps (graphs, anything without extends).
    pub root: ResolvedAncestor,
    /// Topologically ordered extends ancestors (deepest ancestor first).
    /// Empty when the root has no extends or the kind has no extends step.
    pub ancestors: Vec<ResolvedAncestor>,
    /// Lateral references edges (deduped by (from_source_path, to_source_path) pair).
    pub references_edges: Vec<ResolutionEdge>,
    /// Per-step metadata (what each step computed).
    pub step_outputs: HashMap<String, serde_json::Value>,
    /// Daemon-computed executor trust posture: weakest of
    /// `root.trust_class` and every `ancestors[i].trust_class`.
    /// Runtimes consume this directly; never recompute.
    pub executor_trust_class: TrustClass,
    /// Daemon-side composed view; `None` when no composer is registered
    /// for the kind. Phase 2 fills the `Directive` variant.
    pub composed: KindComposedView,
}

/// Error type for resolution pipeline.
#[derive(Debug)]
pub enum ResolutionError {
    /// Cycle detected in extends chain.
    CycleDetected {
        chain: Vec<ResolvedAncestor>,
        edge_type: String,
    },
    /// Extends depth limit exceeded.
    MaxDepthExceeded {
        step: ResolutionStepName,
        depth: usize,
    },
    /// Alias expansion depth limit exceeded.
    AliasMaxDepthExceeded {
        alias: String,
        expansion: Vec<String>,
    },
    /// Cyclic alias reference detected.
    AliasCycle {
        expansion: Vec<String>,
    },
    /// Alias not found in kind's execution.aliases.
    UnknownAlias {
        alias: String,
        kind: String,
    },
    /// Referenced item does not exist.
    MissingItem {
        item_ref: String,
        referenced_by: String,
    },
    /// Item signature invalid or missing.
    IntegrityFailure {
        item_ref: String,
        reason: String,
    },
    /// Kind has no execution block (not executable).
    KindNotExecutable {
        kind: String,
    },
    /// Generic step failure.
    StepFailed {
        step: ResolutionStepName,
        reason: String,
    },
}

impl std::fmt::Display for ResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolutionError::CycleDetected { chain, edge_type } => {
                write!(
                    f,
                    "cycle detected in {} chain at depth {}: {} -> ...",
                    edge_type,
                    chain.len(),
                    chain.first().map(|h| &h.requested_id).unwrap_or(&"?".to_string())
                )
            }
            ResolutionError::MaxDepthExceeded { step, depth } => {
                write!(f, "{} exceeded max depth ({})", step, depth)
            }
            ResolutionError::AliasMaxDepthExceeded {
                alias,
                expansion,
            } => {
                write!(f, "alias {} expansion chain too deep: {:?}", alias, expansion)
            }
            ResolutionError::AliasCycle { expansion } => {
                write!(f, "cyclic alias reference: {:?}", expansion)
            }
            ResolutionError::UnknownAlias { alias, kind } => {
                write!(f, "unknown alias {} in kind {}", alias, kind)
            }
            ResolutionError::MissingItem {
                item_ref,
                referenced_by,
            } => {
                write!(f, "item {} referenced by {} not found", item_ref, referenced_by)
            }
            ResolutionError::IntegrityFailure { item_ref, reason } => {
                write!(f, "integrity check failed for {}: {}", item_ref, reason)
            }
            ResolutionError::KindNotExecutable { kind } => {
                write!(f, "kind {} has no execution block (not executable)", kind)
            }
            ResolutionError::StepFailed { step, reason } => {
                write!(f, "{} failed: {}", step, reason)
            }
        }
    }
}

impl std::error::Error for ResolutionError {}

impl TrustClass {
    /// Strict ranking, strongest → weakest. Used by `execution_trust`
    /// to fold an extends chain into a single executor scalar.
    fn strength(self) -> u8 {
        match self {
            TrustClass::TrustedSystem => 3,
            TrustClass::TrustedUser => 2,
            TrustClass::UntrustedUserSpace => 1,
            TrustClass::Unsigned => 0,
        }
    }
}

/// Reduce the executor's trust posture across the extends chain.
///
/// Execution trust is the **weakest** trust class observed across the
/// root item and every ancestor in `chain`. The intuition: when a child
/// inherits behaviour from a parent, the child can be no more trusted
/// than the least-trusted link it depends on for its definition.
///
/// Reference edges intentionally retain their per-edge `trust_class` and
/// are NOT folded in here — references are lateral and the subject
/// enforces sandbox per-reference at use time, not by collapsing them
/// into the executor scalar.
///
/// Order (strongest → weakest):
/// `TrustedSystem` > `TrustedUser` > `UntrustedUserSpace` > `Unsigned`.
pub fn execution_trust(root: TrustClass, chain: &[ResolvedAncestor]) -> TrustClass {
    let mut weakest = root;
    for hop in chain {
        if hop.trust_class.strength() < weakest.strength() {
            weakest = hop.trust_class;
        }
    }
    weakest
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ancestor(trust: TrustClass) -> ResolvedAncestor {
        ResolvedAncestor {
            requested_id: "x".to_string(),
            resolved_ref: "directive:x".to_string(),
            source_path: PathBuf::from("/x"),
            trust_class: trust,
            alias_resolution: None,
            added_by: ResolutionStepName::ResolveExtendsChain,
            raw_content: String::new(),
            raw_content_digest: String::new(),
        }
    }

    #[test]
    fn execution_trust_picks_weakest_in_chain() {
        let chain = vec![
            ancestor(TrustClass::TrustedSystem),
            ancestor(TrustClass::Unsigned),
            ancestor(TrustClass::TrustedUser),
        ];
        assert_eq!(
            execution_trust(TrustClass::TrustedSystem, &chain),
            TrustClass::Unsigned
        );
    }

    #[test]
    fn execution_trust_returns_root_when_chain_empty() {
        assert_eq!(
            execution_trust(TrustClass::TrustedUser, &[]),
            TrustClass::TrustedUser
        );
    }

    #[test]
    fn execution_trust_root_can_be_weakest() {
        let chain = vec![ancestor(TrustClass::TrustedSystem)];
        assert_eq!(
            execution_trust(TrustClass::Unsigned, &chain),
            TrustClass::Unsigned
        );
    }

    #[test]
    fn execution_trust_ranking_order() {
        // TrustedSystem > TrustedUser > UntrustedUserSpace > Unsigned
        let cases = [
            (TrustClass::TrustedSystem, TrustClass::TrustedUser, TrustClass::TrustedUser),
            (TrustClass::TrustedUser, TrustClass::UntrustedUserSpace, TrustClass::UntrustedUserSpace),
            (TrustClass::UntrustedUserSpace, TrustClass::Unsigned, TrustClass::Unsigned),
        ];
        for (a, b, expected) in cases {
            assert_eq!(execution_trust(a, &[ancestor(b)]), expected);
        }
    }
}
