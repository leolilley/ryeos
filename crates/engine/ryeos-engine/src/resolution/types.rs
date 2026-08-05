use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::contracts::ItemSpace;

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Trust classification for resolved items (for isolation profile enforcement and audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TrustClass {
    /// Item from immutable system bundle.
    TrustedBundle,
    /// Item from a trusted project source.
    TrustedProject,
    /// Launch configuration signed by a node-trusted publisher and loaded
    /// from the node-local configuration layer.
    TrustedNode,
    /// Item from project or untrusted sources.
    UntrustedProject,
    /// Item not signed or signature invalid.
    Unsigned,
}

/// Record of alias expansion chain for audit and trust analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasHop {
    /// Ordered chain of alias expansions:
    /// `["@core", "@base", "directive:ryeos/agent/core/base"]`
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
    /// Fingerprint declared by the verified signature envelope. Required for
    /// trusted items so recovery can apply current signer revocation without
    /// re-resolving the item ref.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub signer_fingerprint: Option<String>,
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
            referenced_items: self
                .referenced_items
                .iter()
                .map(ResolutionDigestNode::from)
                .collect(),
            effective_trust_class: self.effective_trust_class,
            policy_facts: self.composed.policy_facts.clone(),
        }
    }

    /// Exact identity of the admitted effective executable definition.
    ///
    /// This commits to ordered definition contributors, lateral references,
    /// trust/signer/source-space evidence, and the complete composed view. It
    /// deliberately excludes paths, resolver diagnostics, invocation data,
    /// timestamps, raw payload bytes, and whole signature envelopes.
    pub fn effective_definition_digest(
        &self,
    ) -> Result<EffectiveDefinitionDigest, EffectiveDefinitionDigestError> {
        let mut referenced_items = self
            .referenced_items
            .iter()
            .map(EffectiveContributorV1::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        referenced_items.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));

        let mut reference_edges = self
            .references_edges
            .iter()
            .map(EffectiveReferenceEdgeV1::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        reference_edges.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));

        let seed = EffectiveDefinitionSeedV1 {
            schema: "ryeos.effective_definition.v1",
            root: EffectiveContributorV1::try_from(&self.root)?,
            ancestors: self
                .ancestors
                .iter()
                .map(EffectiveContributorV1::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            referenced_items,
            reference_edges,
            effective_trust_class: self.effective_trust_class,
            composed: &self.composed,
        };
        let value = serde_json::to_value(seed).map_err(|error| {
            EffectiveDefinitionDigestError(format!("serialize effective-definition seed: {error}"))
        })?;
        let canonical = lillux::cas::canonical_json(&value).map_err(|error| {
            EffectiveDefinitionDigestError(format!(
                "canonicalize effective-definition seed: {error}"
            ))
        })?;
        EffectiveDefinitionDigest::parse(lillux::cas::sha256_hex(canonical.as_bytes()))
    }
}

/// Canonical lower-case SHA-256 digest used for exact executable identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EffectiveDefinitionDigest(String);

impl EffectiveDefinitionDigest {
    pub fn parse(value: String) -> Result<Self, EffectiveDefinitionDigestError> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(EffectiveDefinitionDigestError(
                "effective definition digest must be 64 lower-case hex characters".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EffectiveDefinitionDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveDefinitionDigestError(pub String);

impl std::fmt::Display for EffectiveDefinitionDigestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EffectiveDefinitionDigestError {}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct EffectiveDefinitionSeedV1<'a> {
    schema: &'static str,
    root: EffectiveContributorV1,
    ancestors: Vec<EffectiveContributorV1>,
    referenced_items: Vec<EffectiveContributorV1>,
    reference_edges: Vec<EffectiveReferenceEdgeV1>,
    effective_trust_class: TrustClass,
    composed: &'a KindComposedView,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct EffectiveContributorV1 {
    canonical_ref: String,
    root_raw_content_digest: String,
    source_space: crate::contracts::ItemSpace,
    trust_class: TrustClass,
    signer_fingerprint: Option<String>,
    added_by: ResolutionStepName,
}

impl EffectiveContributorV1 {
    fn sort_key(&self) -> (&str, &str, &str, &str, &str, &str) {
        (
            &self.canonical_ref,
            &self.root_raw_content_digest,
            self.source_space.as_str(),
            trust_class_name(self.trust_class),
            self.signer_fingerprint.as_deref().unwrap_or(""),
            resolution_step_name(self.added_by),
        )
    }
}

impl TryFrom<&ResolvedAncestor> for EffectiveContributorV1 {
    type Error = EffectiveDefinitionDigestError;

    fn try_from(value: &ResolvedAncestor) -> Result<Self, Self::Error> {
        require_canonical_ref("canonical ref", &value.resolved_ref)?;
        require_lower_sha256("root raw content digest", &value.raw_content_digest)?;
        if matches!(
            value.trust_class,
            TrustClass::TrustedBundle | TrustClass::TrustedProject | TrustClass::TrustedNode
        ) {
            let signer = value.signer_fingerprint.as_deref().ok_or_else(|| {
                EffectiveDefinitionDigestError(format!(
                    "trusted contributor `{}` has no signer fingerprint",
                    value.resolved_ref
                ))
            })?;
            require_lower_sha256("signer fingerprint", signer)?;
        } else if let Some(signer) = value.signer_fingerprint.as_deref() {
            require_lower_sha256("signer fingerprint", signer)?;
        }
        Ok(Self {
            canonical_ref: value.resolved_ref.clone(),
            root_raw_content_digest: value.raw_content_digest.clone(),
            source_space: value.source_space,
            trust_class: value.trust_class,
            signer_fingerprint: value.signer_fingerprint.clone(),
            added_by: value.added_by,
        })
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct EffectiveReferenceEdgeV1 {
    from_ref: String,
    to_ref: String,
    to_source_space: crate::contracts::ItemSpace,
    trust_class: TrustClass,
    added_by: ResolutionStepName,
}

impl EffectiveReferenceEdgeV1 {
    fn sort_key(&self) -> (&str, &str, &str, &str, &str) {
        (
            &self.from_ref,
            &self.to_ref,
            self.to_source_space.as_str(),
            trust_class_name(self.trust_class),
            resolution_step_name(self.added_by),
        )
    }
}

impl TryFrom<&ResolutionEdge> for EffectiveReferenceEdgeV1 {
    type Error = EffectiveDefinitionDigestError;

    fn try_from(value: &ResolutionEdge) -> Result<Self, Self::Error> {
        require_canonical_ref("reference edge source", &value.from_ref)?;
        require_canonical_ref("reference edge target", &value.to_ref)?;
        Ok(Self {
            from_ref: value.from_ref.clone(),
            to_ref: value.to_ref.clone(),
            to_source_space: value.to_source_space,
            trust_class: value.trust_class,
            added_by: value.added_by,
        })
    }
}

fn require_canonical_ref(label: &str, value: &str) -> Result<(), EffectiveDefinitionDigestError> {
    crate::canonical_ref::CanonicalRef::parse(value)
        .map(|_| ())
        .map_err(|error| {
            EffectiveDefinitionDigestError(format!(
                "effective-definition seed {label} `{value}` is not canonical: {error}"
            ))
        })
}

fn require_lower_sha256(label: &str, value: &str) -> Result<(), EffectiveDefinitionDigestError> {
    EffectiveDefinitionDigest::parse(value.to_string())
        .map(|_| ())
        .map_err(|_| EffectiveDefinitionDigestError(format!("invalid {label}: `{value}`")))
}

fn trust_class_name(value: TrustClass) -> &'static str {
    match value {
        TrustClass::TrustedBundle => "trusted_bundle",
        TrustClass::TrustedProject => "trusted_project",
        TrustClass::TrustedNode => "trusted_node",
        TrustClass::UntrustedProject => "untrusted_project",
        TrustClass::Unsigned => "unsigned",
    }
}

fn resolution_step_name(value: ResolutionStepName) -> &'static str {
    match value {
        ResolutionStepName::PipelineInit => "pipeline_init",
        ResolutionStepName::ResolveExtendsChain => "resolve_extends_chain",
        ResolutionStepName::ResolveReferences => "resolve_references",
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub signer_fingerprint: Option<String>,
    pub raw_content_digest: String,
}

impl From<&ResolvedAncestor> for ResolutionDigestNode {
    fn from(item: &ResolvedAncestor) -> Self {
        Self {
            requested_id: item.requested_id.clone(),
            resolved_ref: item.resolved_ref.clone(),
            source_space: item.source_space,
            trust_class: item.trust_class,
            signer_fingerprint: item.signer_fingerprint.clone(),
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
    /// Lateral signed inputs whose verified bytes participated in the
    /// resolution/composition. Retained so recovery can apply signer
    /// revocation transitively without resolving their mutable refs.
    pub referenced_items: Vec<ResolutionDigestNode>,
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub signer_fingerprint: Option<String>,
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
            signer_fingerprint: item.signer_fingerprint.clone(),
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
            TrustClass::TrustedNode => 3,
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
            signer_fingerprint: matches!(
                trust,
                TrustClass::TrustedBundle | TrustClass::TrustedProject
            )
            .then(|| "fixture-signer".to_string()),
            alias_resolution: None,
            added_by: ResolutionStepName::ResolveExtendsChain,
            raw_content: String::new(),
            source_content_digest: String::new(),
            raw_content_digest: String::new(),
        }
    }

    fn effective_digest_fixture() -> ResolutionOutput {
        let contributor = |resolved_ref: &str, digest_byte: char, added_by: ResolutionStepName| {
            ResolvedAncestor {
                requested_id: resolved_ref.to_string(),
                resolved_ref: resolved_ref.to_string(),
                source_path: PathBuf::from(format!("/diagnostic/{digest_byte}")),
                source_space: ItemSpace::Bundle,
                trust_class: TrustClass::TrustedBundle,
                signer_fingerprint: Some("f".repeat(64)),
                alias_resolution: None,
                added_by,
                raw_content: format!("body-{digest_byte}"),
                source_content_digest: digest_byte.to_string().repeat(64),
                raw_content_digest: digest_byte.to_string().repeat(64),
            }
        };
        let root = contributor("graph:test/root", 'a', ResolutionStepName::PipelineInit);
        let ancestor = contributor(
            "graph:test/base",
            'b',
            ResolutionStepName::ResolveExtendsChain,
        );
        let reference = contributor(
            "tool:test/audit",
            'c',
            ResolutionStepName::ResolveReferences,
        );
        ResolutionOutput {
            root,
            ancestors: vec![ancestor],
            references_edges: vec![ResolutionEdge {
                from_ref: "graph:test/root".to_string(),
                from_source_path: PathBuf::from("/diagnostic/root"),
                to_ref: "tool:test/audit".to_string(),
                to_source_path: PathBuf::from("/diagnostic/audit"),
                to_source_space: ItemSpace::Bundle,
                trust_class: TrustClass::TrustedBundle,
                added_by: ResolutionStepName::ResolveReferences,
            }],
            referenced_items: vec![reference],
            step_outputs: HashMap::new(),
            effective_trust_class: TrustClass::TrustedBundle,
            composed: KindComposedView {
                composed: serde_json::json!({"config": {"start": "a", "nodes": {"a": {}}}}),
                derived: HashMap::from([
                    ("z".to_string(), serde_json::json!({"b": 2, "a": 1})),
                    ("a".to_string(), serde_json::json!(true)),
                ]),
                policy_facts: HashMap::from([
                    ("effective_caps".to_string(), serde_json::json!(["cap:a"])),
                    ("other".to_string(), serde_json::json!({"y": 2, "x": 1})),
                ]),
            },
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
    fn effective_definition_digest_is_canonical_and_ignores_diagnostics() {
        let original = effective_digest_fixture();
        let mut reordered = original.clone();
        reordered.composed.derived = HashMap::from([
            ("a".to_string(), serde_json::json!(true)),
            ("z".to_string(), serde_json::json!({"a": 1, "b": 2})),
        ]);
        reordered.composed.policy_facts = HashMap::from([
            ("other".to_string(), serde_json::json!({"x": 1, "y": 2})),
            ("effective_caps".to_string(), serde_json::json!(["cap:a"])),
        ]);
        reordered.root.source_path = PathBuf::from("/different/location");
        reordered.root.raw_content = "different signature-stripped bytes".to_string();
        reordered.root.source_content_digest = "9".repeat(64);
        reordered.root.requested_id = "@diagnostic-alias".to_string();
        reordered.step_outputs.insert(
            "pipeline_init".to_string(),
            serde_json::json!({"trace": true}),
        );

        assert_eq!(
            original.effective_definition_digest().unwrap(),
            reordered.effective_definition_digest().unwrap()
        );
    }

    #[test]
    fn effective_definition_digest_commits_to_behavior_and_provenance() {
        let original = effective_digest_fixture();
        let expected = original.effective_definition_digest().unwrap();

        let mut cases = Vec::new();
        let mut changed = original.clone();
        changed.root.raw_content_digest = "d".repeat(64);
        cases.push(changed);
        let mut changed = original.clone();
        changed.root.source_space = ItemSpace::Project;
        cases.push(changed);
        let mut changed = original.clone();
        changed.root.trust_class = TrustClass::TrustedProject;
        cases.push(changed);
        let mut changed = original.clone();
        changed.root.signer_fingerprint = Some("e".repeat(64));
        cases.push(changed);
        let mut changed = original.clone();
        changed.composed.composed["config"]["start"] = serde_json::json!("b");
        cases.push(changed);
        let mut changed = original.clone();
        changed
            .composed
            .derived
            .insert("a".to_string(), serde_json::json!(false));
        cases.push(changed);
        let mut changed = original.clone();
        changed.references_edges[0].to_source_space = ItemSpace::Project;
        cases.push(changed);

        for changed in cases {
            assert_ne!(expected, changed.effective_definition_digest().unwrap());
        }
    }

    #[test]
    fn effective_definition_digest_rejects_noncanonical_provenance_edges() {
        let mut resolution = effective_digest_fixture();
        resolution.references_edges[0].to_ref = "not-a-canonical-ref".to_string();

        let error = resolution.effective_definition_digest().unwrap_err();
        assert!(error.to_string().contains("reference edge target"));
        assert!(error.to_string().contains("not canonical"));
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
        let back: AsLaunchedResolutionDigest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(back, digest);

        let mut missing_signer = wire;
        missing_signer["root"]
            .as_object_mut()
            .unwrap()
            .remove("signer_fingerprint");
        assert!(serde_json::from_value::<AsLaunchedResolutionDigest>(missing_signer).is_err());
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
