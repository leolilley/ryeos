//! Resolution pipeline — daemon-side preprocessing of effective items.
//!
//! Walks `extends` / `references` DAGs declared in kind schemas, expands
//! aliases recursively, detects cycles on resolved source paths, and emits
//! a structured `ResolutionOutput` that runtimes consume via
//! `LaunchEnvelope.resolution`.

pub mod alias;
pub mod context;
pub mod corpus;
pub mod decl;
pub mod steps;
pub mod types;

pub use alias::AliasResolver;
pub use context::ResolutionContext;
pub use corpus::{resolve_item_for_corpus, CorpusItemProjection, CorpusReferenceEdge};
pub use decl::ResolutionStepDecl;
pub use types::{
    effective_trust, AliasHop, AsLaunchedResolutionDigest, KindComposedView, ResolutionDigestNode,
    ResolutionEdge, ResolutionError, ResolutionFailureClass, ResolutionOutput,
    ResolutionProvenance, ResolutionProvenanceEdge, ResolutionProvenanceNode, ResolutionStepName,
    ResolvedAncestor, TrustClass,
};

use crate::canonical_ref::CanonicalRef;
use crate::composers::ComposerRegistry;
use crate::item_resolution::ResolutionRoots;
use crate::kind_registry::KindRegistry;
use crate::parsers::ParserDispatcher;
use crate::trust::TrustStore;

/// Run the full effective resolution pipeline for an executable item.
///
/// This preserves the launch-time contract: the item kind must declare
/// `execution:`. Execution is a consumer of the same effective item
/// substrate used by [`run_effective_item_pipeline`]; dispatch metadata
/// lives under `execution`, while resolution steps live only at the
/// kind-schema top level.
pub fn run_resolution_pipeline(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    composers: &ComposerRegistry,
) -> Result<ResolutionOutput, ResolutionError> {
    let kind_schema = kinds
        .get(&item.kind)
        .ok_or_else(|| ResolutionError::KindNotExecutable {
            kind: item.kind.clone(),
        })?;

    let execution =
        kind_schema
            .execution
            .as_ref()
            .ok_or_else(|| ResolutionError::KindNotExecutable {
                kind: item.kind.clone(),
            })?;

    run_item_pipeline_inner(
        item,
        kind_schema.resolution.as_slice(),
        kind_schema.effective_trust.include_references,
        AliasResolver::new(execution.aliases.clone(), execution.alias_max_depth),
        kinds,
        parsers,
        roots,
        trust_store,
        composers,
    )
}

/// Run the full resolution/composition pipeline for any item kind,
/// executable or not.
///
/// Every kind uses the same top-level `resolution:` declaration. The
/// `execution:` block only declares how an already-effective item is
/// consumed by launch/dispatch.
pub fn run_effective_item_pipeline(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    composers: &ComposerRegistry,
) -> Result<ResolutionOutput, ResolutionError> {
    let kind_schema = kinds
        .get(&item.kind)
        .ok_or_else(|| ResolutionError::StepFailed {
            step: ResolutionStepName::PipelineInit,
            class: ResolutionFailureClass::InvalidDefinition,
            reason: format!("unknown kind: {}", item.kind),
        })?;

    let alias_resolver = kind_schema
        .execution
        .as_ref()
        .map(|execution| AliasResolver::new(execution.aliases.clone(), execution.alias_max_depth))
        .unwrap_or_else(|| AliasResolver::new(Default::default(), 8));

    run_item_pipeline_inner(
        item,
        kind_schema.resolution.as_slice(),
        kind_schema.effective_trust.include_references,
        alias_resolver,
        kinds,
        parsers,
        roots,
        trust_store,
        composers,
    )
}

// The tail is one resolution environment (registries + roots + trust);
// both entry points thread it verbatim — a context struct would only
// rename the same nine things.
#[allow(clippy::too_many_arguments)]
fn run_item_pipeline_inner(
    item: &CanonicalRef,
    resolution: &[ResolutionStepDecl],
    include_references_in_trust: bool,
    alias_resolver: AliasResolver,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    composers: &ComposerRegistry,
) -> Result<ResolutionOutput, ResolutionError> {
    // Reject duplicate `resolve_extends_chain` declarations per kind —
    // the step's state (`visiting_extends`, `done_extends`,
    // `ordered_refs`) lives on `ResolutionContext`, not per-invocation,
    // so two extends steps would cross-contaminate. v1 explicitly only
    // supports a single extends step.
    let extends_decls = resolution
        .iter()
        .filter(|d| matches!(d, ResolutionStepDecl::ResolveExtendsChain { .. }))
        .count();
    if extends_decls > 1 {
        return Err(ResolutionError::StepFailed {
            step: ResolutionStepName::ResolveExtendsChain,
            class: ResolutionFailureClass::InvalidDefinition,
            reason: format!(
                "kind `{}` declares {extends_decls} `resolve_extends_chain` steps; \
                 at most one is allowed",
                item.kind
            ),
        });
    }

    // Same guard for `resolve_references`: per-step state lives on the
    // context (`references_edges`, `step_outputs["resolve_references"]`),
    // and a second declaration would cross-contaminate / silently
    // overwrite the recorded step output.
    let references_decls = resolution
        .iter()
        .filter(|d| matches!(d, ResolutionStepDecl::ResolveReferences { .. }))
        .count();
    if references_decls > 1 {
        return Err(ResolutionError::StepFailed {
            step: ResolutionStepName::ResolveReferences,
            class: ResolutionFailureClass::InvalidDefinition,
            reason: format!(
                "kind `{}` declares {references_decls} `resolve_references` steps; \
                 at most one is allowed",
                item.kind
            ),
        });
    }

    // Load the root once. Steps reuse this rather than re-loading on
    // every entry, and `into_output` ships it as `ResolutionOutput.root`.
    let root_loaded = context::load_item_at(
        kinds,
        parsers,
        roots,
        trust_store,
        item,
        "<root>",
        ResolutionStepName::PipelineInit,
    )?;

    let mut ctx = ResolutionContext::new(
        item.clone(),
        kinds,
        parsers,
        roots,
        trust_store,
        alias_resolver,
        root_loaded,
    );

    for decl in resolution {
        ctx.run_step(decl)?;
    }

    // Composition runs inside `into_output` while the parser
    // dispatcher's parsed values are still in scope. The envelope
    // never carries those values — only the composed view does.
    ctx.into_output(composers, &item.kind, include_references_in_trust)
}
