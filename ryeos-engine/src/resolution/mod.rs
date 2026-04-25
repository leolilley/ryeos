//! Resolution pipeline — daemon-side preprocessing of executable items.
//!
//! Walks `extends` / `references` DAGs declared in kind schemas, expands
//! aliases recursively, detects cycles on resolved source paths, and emits
//! a structured `ResolutionOutput` that runtimes consume via
//! `LaunchEnvelope.resolution`.

pub mod alias;
pub mod context;
pub mod decl;
pub mod steps;
pub mod types;

pub use alias::AliasResolver;
pub use context::ResolutionContext;
pub use decl::ResolutionStepDecl;
pub use types::{
    execution_trust, AliasHop, KindComposedView, ResolutionEdge,
    ResolutionError, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
};

use crate::canonical_ref::CanonicalRef;
use crate::composers::ComposerRegistry;
use crate::item_resolution::ResolutionRoots;
use crate::kind_registry::KindRegistry;
use crate::parsers::ParserDispatcher;
use crate::trust::TrustStore;

/// Run the full resolution pipeline for an item.
///
/// Looks up the item's kind, validates that the kind is executable, builds
/// the alias resolver from `execution.aliases`, then dispatches each
/// declared `ResolutionStepDecl` in order.
///
/// All errors are hard failures — a partial pipeline never reaches the
/// runtime. The returned `ResolutionOutput` is what populates
/// `LaunchEnvelope.resolution`.
pub fn run_resolution_pipeline(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    composers: &ComposerRegistry,
) -> Result<ResolutionOutput, ResolutionError> {
    let kind_schema = kinds.get(&item.kind).ok_or_else(|| {
        ResolutionError::KindNotExecutable {
            kind: item.kind.clone(),
        }
    })?;

    let execution =
        kind_schema
            .execution
            .as_ref()
            .ok_or_else(|| ResolutionError::KindNotExecutable {
                kind: item.kind.clone(),
            })?;

    // Reject duplicate `resolve_extends_chain` declarations per kind —
    // the step's state (`visiting_extends`, `done_extends`,
    // `ordered_refs`) lives on `ResolutionContext`, not per-invocation,
    // so two extends steps would cross-contaminate. v1 explicitly only
    // supports a single extends step.
    let extends_decls = execution
        .resolution
        .iter()
        .filter(|d| matches!(d, ResolutionStepDecl::ResolveExtendsChain { .. }))
        .count();
    if extends_decls > 1 {
        return Err(ResolutionError::StepFailed {
            step: ResolutionStepName::ResolveExtendsChain,
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
    let references_decls = execution
        .resolution
        .iter()
        .filter(|d| matches!(d, ResolutionStepDecl::ResolveReferences { .. }))
        .count();
    if references_decls > 1 {
        return Err(ResolutionError::StepFailed {
            step: ResolutionStepName::ResolveReferences,
            reason: format!(
                "kind `{}` declares {references_decls} `resolve_references` steps; \
                 at most one is allowed",
                item.kind
            ),
        });
    }

    let alias_resolver = AliasResolver::new(execution.aliases.clone(), execution.alias_max_depth);

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

    for decl in &execution.resolution {
        ctx.run_step(decl)?;
    }

    // Composition runs inside `into_output` while the parser
    // dispatcher's parsed values are still in scope. The envelope
    // never carries those values — only the composed view does.
    ctx.into_output(composers, &item.kind)
}
