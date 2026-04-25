//! Resolution context — stateful pipeline execution environment.
//!
//! Threads engine refs (`KindRegistry`, `MetadataParserRegistry`,
//! `ResolutionRoots`, `TrustStore`) through the steps so they can do real
//! work — load items from disk, verify signatures, parse metadata.
//!
//! The context itself owns mutable pipeline state (ancestors, edges,
//! visited set, step outputs). Steps mutate it via `&mut`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::contracts::ItemSpace;
use crate::item_resolution::ResolutionRoots;
use crate::kind_registry::KindRegistry;
use crate::parsers::ParserDispatcher;
use crate::trust::TrustStore;

use super::alias::AliasResolver;
use super::decl::ResolutionStepDecl;
use super::types::{
    execution_trust, KindComposedView, ResolutionEdge, ResolutionError, ResolutionOutput,
    ResolutionStepName, ResolvedAncestor, TrustClass,
};

/// Result of loading an item: identity / trust / raw bytes (as a
/// `ResolvedAncestor` minus its hop-specific fields), and the parsed
/// metadata document the steps inspect.
///
/// Steps fill in `requested_id`, `resolved_ref`, `alias_resolution`,
/// `added_by` from their own context when promoting a `LoadedItem` to a
/// full `ResolvedAncestor`.
pub(crate) struct LoadedItem {
    pub source_path: PathBuf,
    pub trust_class: TrustClass,
    pub raw_content: String,
    pub raw_content_digest: String,
    /// Engine-side parsed metadata document (e.g. extracted markdown/xml
    /// fields). Used by steps to read `extends`/`references` etc.; not
    /// shipped in the envelope (runtimes re-parse `raw_content`).
    pub parsed: Value,
}

impl LoadedItem {
    pub fn source_path(&self) -> &PathBuf {
        &self.source_path
    }

    /// Promote a verified load result to a full `ResolvedAncestor` by
    /// stamping the hop-specific fields the caller knows.
    pub(crate) fn into_ancestor(
        self,
        requested_id: String,
        resolved_ref: String,
        alias_resolution: Option<super::types::AliasHop>,
        added_by: ResolutionStepName,
    ) -> ResolvedAncestor {
        ResolvedAncestor {
            requested_id,
            resolved_ref,
            source_path: self.source_path,
            trust_class: self.trust_class,
            alias_resolution,
            added_by,
            raw_content: self.raw_content,
            raw_content_digest: self.raw_content_digest,
        }
    }
}

/// Execution environment for the resolution pipeline.
///
/// Borrows the engine registries for the lifetime of one pipeline run;
/// owns the mutable composition state.
pub struct ResolutionContext<'a> {
    /// The item the pipeline is resolving for.
    pub current_ref: CanonicalRef,

    // Engine refs (borrowed).
    pub(crate) kinds: &'a KindRegistry,
    pub(crate) parsers: &'a ParserDispatcher,
    pub(crate) roots: &'a ResolutionRoots,
    pub(crate) trust_store: &'a TrustStore,

    /// Alias expansion (recursive, with depth + cycle protection).
    pub(crate) aliases: AliasResolver,

    /// Loaded root item — populated once at pipeline start. Steps read
    /// the root's parsed metadata via `root_loaded.parsed` instead of
    /// re-loading from disk on every step entry.
    pub(crate) root_loaded: LoadedItem,
    /// Promoted root ancestor (built once from `root_loaded`); shipped
    /// as `ResolutionOutput.root` in `into_output`.
    pub(crate) root_ancestor: ResolvedAncestor,

    /// Bottom-up topological extends chain (deepest ancestor first),
    /// each ancestor inlining its verified bytes — replaces the old
    /// `ordered_refs` + `ancestor_payloads` HashMap pair.
    pub ancestors: Vec<ResolvedAncestor>,
    /// Parsed values produced by the parser dispatcher for each
    /// ancestor, in lockstep with `ancestors`. Steps push here in the
    /// same order they push to `ancestors` so composers can iterate
    /// them as parallel slices.
    pub ancestor_parsed: Vec<Value>,
    /// Lateral references edges, deduplicated by `(from_source_path, to_source_path)`.
    pub references_edges: Vec<ResolutionEdge>,
    /// Source paths currently on the DFS recursion stack — populated on
    /// entry, cleared on return. A path appearing here when about to be
    /// recursed into is a true cycle (A → … → A), distinct from a node
    /// that has already been fully resolved.
    pub(crate) visiting_extends: HashSet<PathBuf>,
    /// Source paths whose extends subtree has already been resolved and
    /// pushed to `ancestors`. Presence-only — used to dedup
    /// independently of the on-stack cycle test.
    pub(crate) done_extends: HashSet<PathBuf>,
    /// Per-step structured output.
    pub step_outputs: HashMap<String, Value>,
}

impl<'a> ResolutionContext<'a> {
    /// Build a context after the pipeline has loaded the root item.
    ///
    /// `root_loaded` is provided by the caller (typically
    /// `run_resolution_pipeline`) so the root is loaded exactly once,
    /// not per-step.
    pub(crate) fn new(
        current_ref: CanonicalRef,
        kinds: &'a KindRegistry,
        parsers: &'a ParserDispatcher,
        roots: &'a ResolutionRoots,
        trust_store: &'a TrustStore,
        aliases: AliasResolver,
        root_loaded: LoadedItem,
    ) -> Self {
        // Build the root ancestor up-front so `into_output` can ship a
        // `ResolvedAncestor` as `root` without keeping `LoadedItem` alive.
        let root_ancestor = ResolvedAncestor {
            requested_id: current_ref.to_string(),
            resolved_ref: current_ref.to_string(),
            source_path: root_loaded.source_path.clone(),
            trust_class: root_loaded.trust_class,
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: root_loaded.raw_content.clone(),
            raw_content_digest: root_loaded.raw_content_digest.clone(),
        };
        Self {
            current_ref,
            kinds,
            parsers,
            roots,
            trust_store,
            aliases,
            root_loaded,
            root_ancestor,
            ancestors: Vec::new(),
            ancestor_parsed: Vec::new(),
            references_edges: Vec::new(),
            visiting_extends: HashSet::new(),
            done_extends: HashSet::new(),
            step_outputs: HashMap::new(),
        }
    }

    /// Consume the context and produce the final pipeline output.
    ///
    /// Composition happens here while the parsed values are still in
    /// scope — the parser dispatcher's `Value` outputs never make it
    /// into the envelope, only the composed view does.
    pub fn into_output(
        self,
        composers: &crate::composers::ComposerRegistry,
        kind: &str,
    ) -> Result<ResolutionOutput, ResolutionError> {
        let executor_trust_class =
            execution_trust(self.root_ancestor.trust_class, &self.ancestors);

        let composed = match composers.get(kind) {
            Some((c, cfg)) => c.compose(
                cfg,
                &self.root_ancestor,
                &self.root_loaded.parsed,
                &self.ancestors,
                &self.ancestor_parsed,
            )?,
            // Test-only escape hatch: if no composer is bound for the
            // kind, surface an identity view so downstream consumers
            // see a uniform shape. Production paths go through
            // `ComposerRegistry::from_kinds`, which fails loud at
            // boot when a kind has no composer.
            None => KindComposedView::identity(self.root_loaded.parsed.clone()),
        };

        Ok(ResolutionOutput {
            root: self.root_ancestor,
            ancestors: self.ancestors,
            references_edges: self.references_edges,
            step_outputs: self.step_outputs,
            executor_trust_class,
            composed,
        })
    }

    /// Run a single declared step.
    pub fn run_step(&mut self, decl: &ResolutionStepDecl) -> Result<(), ResolutionError> {
        match decl {
            ResolutionStepDecl::ResolveExtendsChain { field, max_depth } => {
                super::steps::extends::run(self, field, *max_depth)
            }
            ResolutionStepDecl::ResolveReferences { field, max_depth } => {
                super::steps::references::run(self, field, *max_depth)
            }
        }
    }

    /// Record arbitrary structured output for a step.
    pub fn record_step_output(&mut self, step: &str, output: Value) {
        self.step_outputs.insert(step.to_string(), output);
    }

    /// Load an item using the context's borrowed registries.
    /// Thin wrapper around `load_item_at` so steps don't have to thread
    /// `kinds`/`parsers`/`roots`/`trust_store` themselves.
    pub(crate) fn load_item(
        &self,
        ref_: &CanonicalRef,
        referenced_by: &str,
        step: ResolutionStepName,
    ) -> Result<LoadedItem, ResolutionError> {
        load_item_at(
            self.kinds,
            self.parsers,
            self.roots,
            self.trust_store,
            ref_,
            referenced_by,
            step,
        )
    }
}

/// Load an item from disk, verify its signature, parse its metadata.
///
/// `step` is the caller's step name — flowed into any failure so triage
/// points at the actual step (extends vs references, or `PipelineInit`
/// for root-load failures) instead of always claiming the extends step
/// failed.
///
/// Mirrors `plan_builder::resolve_executor_chain` so the trust posture
/// is identical to what the dispatcher uses. Free function so the
/// pipeline can load the root before any `ResolutionContext` exists.
pub(crate) fn load_item_at(
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    ref_: &CanonicalRef,
    referenced_by: &str,
    step: ResolutionStepName,
) -> Result<LoadedItem, ResolutionError> {
    let kind_schema = kinds
        .get(&ref_.kind)
        .ok_or_else(|| ResolutionError::StepFailed {
            step,
            reason: format!("unknown kind: {}", ref_.kind),
        })?;

    let result = crate::item_resolution::resolve_item_full(roots, kind_schema, ref_).map_err(
        |_| ResolutionError::MissingItem {
            item_ref: ref_.to_string(),
            referenced_by: referenced_by.to_string(),
        },
    )?;

    let content =
        std::fs::read_to_string(&result.winner_path).map_err(|e| ResolutionError::StepFailed {
            step,
            reason: format!("read {}: {e}", result.winner_path.display()),
        })?;

    let source_format = kind_schema
        .resolved_format_for(&result.matched_ext)
        .ok_or_else(|| ResolutionError::StepFailed {
            step,
            reason: format!("no source format for ext {}", result.matched_ext),
        })?;

    // Verify integrity / trust class.
    let sig_header =
        crate::item_resolution::parse_signature_header(&content, &source_format.signature);
    let signed_trusted = match &sig_header {
        Some(header) => {
            if let Some(actual_hash) =
                crate::trust::content_hash_after_signature(&content, &source_format.signature)
            {
                if actual_hash != header.content_hash {
                    return Err(ResolutionError::IntegrityFailure {
                        item_ref: ref_.to_string(),
                        reason: format!(
                            "content hash mismatch (expected {}, got {actual_hash})",
                            header.content_hash
                        ),
                    });
                }
            }
            Some(trust_store.is_trusted(&header.signer_fingerprint))
        }
        None => None,
    };

    let trust_class = match (signed_trusted, result.winner_space) {
        (None, _) => TrustClass::Unsigned,
        (Some(false), _) => TrustClass::UntrustedUserSpace,
        (Some(true), ItemSpace::System) => TrustClass::TrustedSystem,
        (Some(true), ItemSpace::User) => TrustClass::TrustedUser,
        (Some(true), ItemSpace::Project) => TrustClass::UntrustedUserSpace,
    };

    let parsed = parsers
        .dispatch(
            &source_format.parser,
            &content,
            Some(&result.winner_path),
            &source_format.signature,
        )
        .map_err(|e| ResolutionError::StepFailed {
            step,
            reason: format!("parse {}: {e}", result.winner_path.display()),
        })?;

    // Strip the signature line so the envelope ships clean bytes —
    // runtimes don't need (or trust) the daemon-stripped signature.
    //
    // Envelope-aware: only strip lines that match THIS kind's
    // signature envelope. A `# rye:signed:...` line in the body of a
    // markdown directive (envelope `<!-- ... -->`) is NOT part of the
    // bootstrap signature layer and must reach the runtime intact —
    // identical to what the parser dispatcher sees.
    let raw_content = lillux::signature::strip_signature_lines_with_envelope(
        &content,
        &source_format.signature.prefix,
        source_format.signature.suffix.as_deref(),
    );
    // Hash the *post-strip* bytes — what the runtime actually parses
    // and what the envelope binds to. Naming this `raw_content_digest`
    // makes the relationship structural; the original-file digest was
    // ambiguous and never re-verified by anyone downstream.
    let raw_content_digest = crate::item_resolution::content_hash(&raw_content);

    Ok(LoadedItem {
        source_path: result.winner_path,
        trust_class,
        raw_content,
        raw_content_digest,
        parsed,
    })
}

/// Extract a frontmatter field as a single string.
///
/// Strict: missing field is `Ok(None)`, but every other malformed
/// shape is a hard error so a typo can't quietly degrade to "no
/// parent declared".
///
///   * field absent  → `Ok(None)`
///   * `null`        → Err (use absence; explicit null is ambiguous)
///   * single string → `Ok(Some(s))`
///   * array         → Err (multi-parent deferred to advanced path)
///   * anything else → Err
pub(crate) fn field_as_string(
    parsed: &Value,
    field: &str,
) -> Result<Option<String>, super::types::ResolutionError> {
    match parsed.get(field) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Null) => Err(super::types::ResolutionError::StepFailed {
            step: super::types::ResolutionStepName::ResolveExtendsChain,
            reason: format!(
                "field `{field}` is explicitly null; omit the field instead of declaring it null"
            ),
        }),
        Some(Value::Array(_)) => Err(super::types::ResolutionError::StepFailed {
            step: super::types::ResolutionStepName::ResolveExtendsChain,
            reason: format!(
                "field `{field}` must be a single string; multi-parent extends \
                 is deferred to the advanced resolution path"
            ),
        }),
        Some(other) => Err(super::types::ResolutionError::StepFailed {
            step: super::types::ResolutionStepName::ResolveExtendsChain,
            reason: format!(
                "field `{field}` must be a string; got {}",
                other_type(other)
            ),
        }),
    }
}

fn other_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Extract a frontmatter field as a list of strings.
///
/// Strict mirror of `field_as_string`: every malformed shape is a hard
/// error so silent edge-loss can't happen.
///
///   * field absent           → `Ok(vec![])`
///   * single string          → `Ok(vec![s])`
///   * array of all strings   → `Ok(strings)`
///   * `null`                 → Err
///   * array w/ any non-string → Err
///   * anything else          → Err
pub(crate) fn field_as_list(
    parsed: &Value,
    field: &str,
    step: super::types::ResolutionStepName,
) -> Result<Vec<String>, super::types::ResolutionError> {
    match parsed.get(field) {
        None => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for (i, v) in arr.iter().enumerate() {
                match v {
                    Value::String(s) => out.push(s.clone()),
                    other => {
                        return Err(super::types::ResolutionError::StepFailed {
                            step,
                            reason: format!(
                                "field `{field}` element [{i}] must be a string; got {}",
                                other_type(other)
                            ),
                        });
                    }
                }
            }
            Ok(out)
        }
        Some(Value::Null) => Err(super::types::ResolutionError::StepFailed {
            step,
            reason: format!(
                "field `{field}` is explicitly null; omit the field instead of declaring it null"
            ),
        }),
        Some(other) => Err(super::types::ResolutionError::StepFailed {
            step,
            reason: format!(
                "field `{field}` must be a string or array of strings; got {}",
                other_type(other)
            ),
        }),
    }
}

/// Canonicalize a possibly-bare ID against a default kind.
///
/// `directive:foo/bar` and `@alias` pass through unchanged.
/// A bare `foo/bar` becomes `<default_kind>:foo/bar`.
pub(crate) fn ensure_canonical(id: &str, default_kind: &str) -> String {
    if id.starts_with('@') || id.contains(':') {
        id.to_string()
    } else {
        format!("{default_kind}:{id}")
    }
}
