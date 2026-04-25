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

/// Single hop in an extends or references chain.
/// Carries requested ID, resolved canonical ref, on-disk source, trust label,
/// and alias expansion info for audit trails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainHop {
    /// What the parent asked for (raw, possibly an @alias).
    pub requested_id: String,
    /// Canonical ref after all alias expansion.
    pub resolved_ref: String,
    /// Resolved on-disk source path (identity for cycle tracking).
    pub source_path: PathBuf,
    /// Trust label: which tier (system/user/project) this came from.
    pub trust_class: TrustClass,
    /// If an alias was involved, the expansion chain.
    pub alias_resolution: Option<AliasHop>,
    /// Which resolution step added this hop.
    pub added_by: ResolutionStepName,
}

/// Record of alias expansion chain for audit and trust analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasHop {
    /// Ordered chain of alias expansions: ["@core", "@base", "directive:rye/agent/core/base"]
    pub expansion: Vec<String>,
    /// Depth used (≤ execution.alias_max_depth).
    pub depth: usize,
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
    ResolveExtendsChain,
    ResolveReferences,
}

impl std::fmt::Display for ResolutionStepName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolutionStepName::ResolveExtendsChain => write!(f, "resolve_extends_chain"),
            ResolutionStepName::ResolveReferences => write!(f, "resolve_references"),
        }
    }
}

/// Output of the full resolution pipeline.
/// Contains the ordered extends chain, lateral references edges, and per-step outputs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolutionOutput {
    /// Topologically ordered extends chain (bottom-up).
    pub ordered_refs: Vec<ChainHop>,
    /// Lateral references edges (deduped by (from_source_path, to_source_path) pair).
    pub references_edges: Vec<ResolutionEdge>,
    /// Per-step metadata (what each step computed).
    pub step_outputs: HashMap<String, serde_json::Value>,
}

/// Error type for resolution pipeline.
#[derive(Debug)]
pub enum ResolutionError {
    /// Cycle detected in extends chain.
    CycleDetected {
        chain: Vec<ChainHop>,
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
