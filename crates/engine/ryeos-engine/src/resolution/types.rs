use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::contracts::ItemSpace;

/// Trust classification for resolved items (for isolation profile enforcement and audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TrustClass {
    /// Item from immutable system bundle.
    TrustedBundle,
    /// Item from a trusted project source.
    TrustedProject,
    /// Item from project or untrusted sources.
    UntrustedProject,
    /// Item not signed or signature invalid.
    Unsigned,
}

/// Record of alias expansion chain for audit and trust analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasHop {
    /// Ordered chain of alias expansions: ["@core", "@base", "directive:ryeos/agent/core/base"]
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
#[serde(deny_unknown_fields)]
pub struct ResolvedAncestor {
    /// What the parent asked for (raw, possibly an `@alias`).
    pub requested_id: String,
    /// Canonical ref after all alias expansion.
    pub resolved_ref: String,
    /// Resolved on-disk source path (audit / diagnostic only).
    pub source_path: PathBuf,
    /// Typed resolution space. Policy must use this field rather than
    /// inferring space from `source_path` or `trust_class`.
    pub source_space: ItemSpace,
    /// Verified trust classification, kept distinct from source space.
    pub trust_class: TrustClass,
    /// If an alias was involved, the expansion chain.
    pub alias_resolution: Option<AliasHop>,
    /// Which resolution step produced this ancestor.
    pub added_by: ResolutionStepName,
    /// File content with the signature line stripped. Re-parsed by
    /// runtimes / composers using their own format-specific parser.
    pub raw_content: String,
    /// SHA-256 of the exact whole source file as it was opened and verified,
    /// including its signature envelope. This is the identity used to pin
    /// executable source bytes at the isolation boundary.
    pub source_content_digest: String,
    /// SHA-256 of `raw_content` (signature-stripped, post-strip bytes).
    /// Named explicitly so it can never be confused with the original
    /// file digest — what the runtime parses is `raw_content`, so the
    /// digest that binds the envelope is the digest of those bytes.
    pub raw_content_digest: String,
}

/// Edge in the references DAG (not hierarchical like extends).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionEdge {
    /// Source item's resolved canonical ref.
    pub from_ref: String,
    /// Source item's on-disk canonical path.
    pub from_source_path: PathBuf,
    /// Target item's resolved canonical ref.
    pub to_ref: String,
    /// Target item's on-disk canonical path.
    pub to_source_path: PathBuf,
    /// Typed resolution space of the target item.
    pub to_source_space: ItemSpace,
    /// Trust class of the target (edge-local, subject enforces isolation based on it).
    pub trust_class: TrustClass,
    /// Which step added this edge.
    pub added_by: ResolutionStepName,
}

/// Name of a resolution step (used for tracking and error messages).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
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

/// Stable classification for failures raised by a resolution step.
///
/// This is carried at the failure site so callers never have to infer
/// retryability or client responsibility from a human-readable reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionFailureClass {
    /// The resolved item's parsed or composed definition is invalid.
    InvalidDefinition,
    /// A resolver dependency was unavailable while resolving the item.
    DependencyUnavailable,
    /// An engine/handler invariant was violated.
    InternalInvariant,
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
#[serde(deny_unknown_fields)]
pub struct KindComposedView {
    pub composed: serde_json::Value,
    #[serde(default)]
    pub derived: std::collections::HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub policy_facts: std::collections::HashMap<String, serde_json::Value>,
}

impl KindComposedView {
    /// View used by composers (e.g. the `handler:ryeos/core/identity`
    /// composer binary) that perform
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
/// no separate payload map. `effective_trust_class` is the daemon-computed
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
    /// Verified, content-pinned items resolved through the references step.
    /// Deduplicated by canonical ref (first discovery wins; edges preserve
    /// topology). Each entry carries the full verified bytes + trust class
    /// so downstream consumers (slim-payload projection, context rendering)
    /// never need to re-resolve or re-verify.
    #[serde(default)]
    pub referenced_items: Vec<ResolvedAncestor>,
    /// Per-step metadata (what each step computed).
    pub step_outputs: HashMap<String, serde_json::Value>,
    /// Daemon-computed effective trust posture: weakest of
    /// `root.trust_class` and every `ancestors[i].trust_class`.
    /// Runtimes consume this directly; never recompute.
    pub effective_trust_class: TrustClass,
    /// Daemon-side composed view; `None` when no composer is registered
    /// for the kind. Phase 2 fills the `Directive` variant.
    pub composed: KindComposedView,
}

impl ResolutionOutput {
    /// Structured provenance graph for effective-item consumers and
    /// execution policy. It deliberately excludes `raw_content`; this is
    /// audit/provenance metadata, not another payload channel.
    pub fn provenance(&self) -> ResolutionProvenance {
        ResolutionProvenance {
            root: ResolutionProvenanceNode::from(&self.root),
            ancestors: self
                .ancestors
                .iter()
                .map(ResolutionProvenanceNode::from)
                .collect(),
            references: self
                .references_edges
                .iter()
                .map(ResolutionProvenanceEdge::from)
                .collect(),
            referenced_items: self
                .referenced_items
                .iter()
                .map(ResolutionProvenanceNode::from)
                .collect(),
        }
    }

    /// Slim launch-time snapshot of this resolution for durable
    /// persistence as a braid event. See [`AsLaunchedResolutionDigest`].
    pub fn as_launched_digest(&self) -> AsLaunchedResolutionDigest {
        AsLaunchedResolutionDigest {
            root: ResolutionDigestNode::from(&self.root),
            ancestors: self
                .ancestors
                .iter()
                .map(ResolutionDigestNode::from)
                .collect(),
            effective_trust_class: self.effective_trust_class,
            policy_facts: self.composed.policy_facts.clone(),
        }
    }
}

/// One node (root or extends ancestor) in an as-launched digest: the
/// resolved ref plus the content digest that pins the exact bytes the
/// launcher composed. Trust class travels with it so a weak link reads
/// without re-resolving. Deliberately excludes `raw_content` — the full
/// bytes are reconstructable from CAS by digest when a read needs them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionDigestNode {
    pub requested_id: String,
    pub resolved_ref: String,
    pub source_space: ItemSpace,
    pub trust_class: TrustClass,
    pub raw_content_digest: String,
}

impl From<&ResolvedAncestor> for ResolutionDigestNode {
    fn from(item: &ResolvedAncestor) -> Self {
        Self {
            requested_id: item.requested_id.clone(),
            resolved_ref: item.resolved_ref.clone(),
            source_space: item.source_space,
            trust_class: item.trust_class,
            raw_content_digest: item.raw_content_digest.clone(),
        }
    }
}

/// Slim, launch-time snapshot of an item resolution — the extends chain
/// as composed (refs + content digests), the composed `policy_facts`, and
/// the effective trust class. Persisted as a braid event at launch so the
/// explain view can render what a thread actually launched with rather
/// than a fresh re-resolve. Digests only: the full composed value is
/// reconstructable from CAS by digest when a read needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsLaunchedResolutionDigest {
    /// Resolved root item ref + content digest.
    pub root: ResolutionDigestNode,
    /// Extends-chain ancestors (deepest first), refs + content digests.
    pub ancestors: Vec<ResolutionDigestNode>,
    /// Daemon-folded weakest-link trust class at launch.
    pub effective_trust_class: TrustClass,
    /// Composed daemon-policy facts (e.g. `effective_caps`) the launcher read.
    #[serde(default)]
    pub policy_facts: HashMap<String, serde_json::Value>,
}

/// Payload-free provenance for a resolved effective item.
///
/// The root and ancestor list form the effective inheritance chain;
/// `references` and `referenced_items` carry lateral graph provenance
/// for kinds that declare `resolve_references`. Trust folding is a
/// policy decision: `ResolutionOutput.effective_trust_class` folds root
/// and ancestors; reference consumers inspect reference node/edge trust
/// explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionProvenance {
    pub root: ResolutionProvenanceNode,
    pub ancestors: Vec<ResolutionProvenanceNode>,
    pub references: Vec<ResolutionProvenanceEdge>,
    pub referenced_items: Vec<ResolutionProvenanceNode>,
}

/// One provenance node in a resolved effective item graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionProvenanceNode {
    pub requested_id: String,
    pub resolved_ref: String,
    pub source_path: PathBuf,
    pub source_space: ItemSpace,
    pub trust_class: TrustClass,
    pub alias_resolution: Option<AliasHop>,
    pub added_by: ResolutionStepName,
    pub source_content_digest: String,
    pub raw_content_digest: String,
}

impl From<&ResolvedAncestor> for ResolutionProvenanceNode {
    fn from(item: &ResolvedAncestor) -> Self {
        Self {
            requested_id: item.requested_id.clone(),
            resolved_ref: item.resolved_ref.clone(),
            source_path: item.source_path.clone(),
            source_space: item.source_space,
            trust_class: item.trust_class,
            alias_resolution: item.alias_resolution.clone(),
            added_by: item.added_by,
            source_content_digest: item.source_content_digest.clone(),
            raw_content_digest: item.raw_content_digest.clone(),
        }
    }
}

/// One lateral reference edge in the provenance graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionProvenanceEdge {
    pub from_ref: String,
    pub from_source_path: PathBuf,
    pub to_ref: String,
    pub to_source_path: PathBuf,
    pub to_source_space: ItemSpace,
    pub trust_class: TrustClass,
    pub added_by: ResolutionStepName,
}

impl From<&ResolutionEdge> for ResolutionProvenanceEdge {
    fn from(edge: &ResolutionEdge) -> Self {
        Self {
            from_ref: edge.from_ref.clone(),
            from_source_path: edge.from_source_path.clone(),
            to_ref: edge.to_ref.clone(),
            to_source_path: edge.to_source_path.clone(),
            to_source_space: edge.to_source_space,
            trust_class: edge.trust_class,
            added_by: edge.added_by,
        }
    }
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
    AliasCycle { expansion: Vec<String> },
    /// Alias not found in kind's execution.aliases.
    UnknownAlias { alias: String, kind: String },
    /// Referenced item does not exist.
    MissingItem {
        item_ref: String,
        referenced_by: String,
    },
    /// Item signature invalid or missing.
    IntegrityFailure { item_ref: String, reason: String },
    /// Path-anchoring validator caught a mismatch between metadata
    /// and on-disk location, OR a `required: true` rule found no
    /// value. Distinct from `IntegrityFailure` (signature/hash) and
    /// `StepFailed` (other resolution-step failure) so triage points
    /// at the metadata.rules schema, not at the signature path.
    MetadataAnchoringFailed {
        item_ref: String,
        source: Box<crate::kind_registry::MetadataAnchoringError>,
    },
    /// Kind has no execution block (not executable).
    KindNotExecutable { kind: String },
    /// Generic step failure.
    StepFailed {
        step: ResolutionStepName,
        class: ResolutionFailureClass,
        reason: String,
    },
    /// The composed value violates the kind's `composed_value_contract`.
    ///
    /// Carries the full `InstanceValidationReport` so consumers can
    /// render per-field diagnostics. Warnings are included but do
    /// **not** block resolution — only errors cause this variant.
    ComposedValueContractViolation {
        kind: String,
        item_ref: String,
        report: crate::contracts::InstanceValidationReport,
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
                    chain
                        .first()
                        .map(|h| &h.requested_id)
                        .unwrap_or(&"?".to_string())
                )
            }
            ResolutionError::MaxDepthExceeded { step, depth } => {
                write!(f, "{} exceeded max depth ({})", step, depth)
            }
            ResolutionError::AliasMaxDepthExceeded { alias, expansion } => {
                write!(
                    f,
                    "alias {} expansion chain too deep: {:?}",
                    alias, expansion
                )
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
                write!(
                    f,
                    "item {} referenced by {} not found",
                    item_ref, referenced_by
                )
            }
            ResolutionError::IntegrityFailure { item_ref, reason } => {
                write!(f, "integrity check failed for {}: {}", item_ref, reason)
            }
            ResolutionError::MetadataAnchoringFailed { item_ref, source } => {
                write!(f, "metadata anchoring failed for {}: {}", item_ref, source)
            }
            ResolutionError::KindNotExecutable { kind } => {
                write!(f, "kind {} has no execution block (not executable)", kind)
            }
            ResolutionError::StepFailed {
                step,
                class: _,
                reason,
            } => {
                write!(f, "{} failed: {}", step, reason)
            }
            ResolutionError::ComposedValueContractViolation {
                kind: _,
                item_ref,
                report,
            } => {
                write!(
                    f,
                    "composed value for {} violates contract ({} errors, {} warnings)",
                    item_ref,
                    report.errors.len(),
                    report.warnings.len(),
                )
            }
        }
    }
}

impl std::error::Error for ResolutionError {}

impl TrustClass {
    /// Strict ranking, strongest → weakest. Used by `effective_trust`
    /// to fold an extends chain into a single effective scalar.
    pub fn strength(self) -> u8 {
        match self {
            TrustClass::TrustedBundle => 3,
            TrustClass::TrustedProject => 2,
            TrustClass::UntrustedProject => 1,
            TrustClass::Unsigned => 0,
        }
    }

    pub fn min(self, other: TrustClass) -> TrustClass {
        if self.strength() <= other.strength() {
            self
        } else {
            other
        }
    }
}

/// Reduce the effective item's trust posture across the extends chain.
///
/// Effective trust is the **weakest** trust class observed across the
/// root item and every ancestor in `chain`. The intuition: when a child
/// inherits behaviour from a parent, the child can be no more trusted
/// than the least-trusted link it depends on for its definition.
///
/// Reference edges intentionally retain their per-edge `trust_class` and
/// are NOT folded in here — references are lateral and the subject
/// enforces isolation per-reference at use time, not by collapsing them
/// into the effective scalar.
///
/// Order (strongest → weakest):
/// `TrustedBundle` > `TrustedProject` > `UntrustedProject` > `Unsigned`.
pub fn effective_trust(root: TrustClass, chain: &[ResolvedAncestor]) -> TrustClass {
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
            source_space: ItemSpace::Bundle,
            trust_class: trust,
            alias_resolution: None,
            added_by: ResolutionStepName::ResolveExtendsChain,
            raw_content: String::new(),
            source_content_digest: String::new(),
            raw_content_digest: String::new(),
        }
    }

    #[test]
    fn effective_trust_picks_weakest_in_chain() {
        let chain = vec![
            ancestor(TrustClass::TrustedBundle),
            ancestor(TrustClass::Unsigned),
            ancestor(TrustClass::TrustedProject),
        ];
        assert_eq!(
            effective_trust(TrustClass::TrustedBundle, &chain),
            TrustClass::Unsigned
        );
    }

    #[test]
    fn effective_trust_returns_root_when_chain_empty() {
        assert_eq!(
            effective_trust(TrustClass::TrustedProject, &[]),
            TrustClass::TrustedProject
        );
    }

    #[test]
    fn effective_trust_root_can_be_weakest() {
        let chain = vec![ancestor(TrustClass::TrustedBundle)];
        assert_eq!(
            effective_trust(TrustClass::Unsigned, &chain),
            TrustClass::Unsigned
        );
    }

    #[test]
    fn as_launched_digest_captures_refs_digests_trust_and_policy_facts() {
        let mut root = ancestor(TrustClass::TrustedBundle);
        root.resolved_ref = "directive:root".to_string();
        root.raw_content = "root body".to_string();
        root.raw_content_digest = "rootdigest".to_string();
        let mut anc = ancestor(TrustClass::Unsigned);
        anc.resolved_ref = "directive:base".to_string();
        anc.raw_content_digest = "basedigest".to_string();

        let mut policy_facts = HashMap::new();
        policy_facts.insert("effective_caps".to_string(), serde_json::json!(["a"]));

        let output = ResolutionOutput {
            root,
            ancestors: vec![anc],
            references_edges: vec![],
            referenced_items: vec![],
            step_outputs: HashMap::new(),
            effective_trust_class: TrustClass::Unsigned,
            composed: KindComposedView {
                composed: serde_json::Value::Null,
                derived: HashMap::new(),
                policy_facts,
            },
        };

        let digest = output.as_launched_digest();
        assert_eq!(digest.root.resolved_ref, "directive:root");
        assert_eq!(digest.root.source_space, ItemSpace::Bundle);
        assert_eq!(digest.root.raw_content_digest, "rootdigest");
        assert_eq!(digest.ancestors.len(), 1);
        assert_eq!(digest.ancestors[0].resolved_ref, "directive:base");
        assert_eq!(digest.ancestors[0].raw_content_digest, "basedigest");
        assert_eq!(digest.ancestors[0].trust_class, TrustClass::Unsigned);
        assert_eq!(digest.effective_trust_class, TrustClass::Unsigned);
        assert_eq!(
            digest.policy_facts["effective_caps"],
            serde_json::json!(["a"])
        );

        // Round-trips through the braid-event payload wire form, and carries no
        // `raw_content` (slim: digests only).
        let wire = serde_json::to_value(&digest).unwrap();
        assert!(wire["root"].get("raw_content").is_none());
        assert_eq!(wire["root"]["source_space"], "bundle");
        let back: AsLaunchedResolutionDigest = serde_json::from_value(wire).unwrap();
        assert_eq!(back, digest);
    }

    #[test]
    fn effective_trust_ranking_order() {
        // TrustedBundle > TrustedProject > UntrustedProject > Unsigned
        let cases = [
            (
                TrustClass::TrustedBundle,
                TrustClass::TrustedProject,
                TrustClass::TrustedProject,
            ),
            (
                TrustClass::TrustedProject,
                TrustClass::UntrustedProject,
                TrustClass::UntrustedProject,
            ),
            (
                TrustClass::UntrustedProject,
                TrustClass::Unsigned,
                TrustClass::Unsigned,
            ),
        ];
        for (a, b, expected) in cases {
            assert_eq!(effective_trust(a, &[ancestor(b)]), expected);
        }
    }
}
