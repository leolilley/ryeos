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
mod definition_identity;
pub mod steps;
pub mod types;

pub use alias::AliasResolver;
pub use context::ResolutionContext;
pub use corpus::{
    CorpusItemProjection, CorpusReferenceEdge, resolve_item_for_corpus,
    resolve_item_for_corpus_under_project_authority,
};
pub use decl::ResolutionStepDecl;
pub use definition_identity::{
    DefinitionChangeCategory, DefinitionChangeKind, DefinitionIdentityChange,
    DefinitionIdentityDiff, DefinitionIdentityDocument, DefinitionValueSummary,
    DefinitionValueType, MAX_IDENTITY_COORDINATE_BYTES, MAX_IDENTITY_DIFF_ROWS,
    MAX_IDENTITY_DIFF_VISITS, MAX_PUBLIC_SCALAR_BYTES,
};
pub use types::{
    AliasHop, AsLaunchedResolutionDigest, EffectiveDefinitionDigest,
    EffectiveDefinitionDigestError, KindComposedView, ResolutionDigestNode, ResolutionEdge,
    ResolutionError, ResolutionFailureClass, ResolutionOutput, ResolutionProvenance,
    ResolutionProvenanceEdge, ResolutionProvenanceNode, ResolutionStepName, ResolvedAncestor,
    RetainedResolutionOutput, TrustClass, effective_trust,
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
    run_resolution_pipeline_with_probes(item, kinds, parsers, roots, trust_store, composers)
        .map(|(output, _probes)| output)
}

/// As [`run_resolution_pipeline`], but also returns the negative dependencies
/// (paths probed absent at a precedence >= each item's winner) accumulated
/// across the whole resolution. The admission-side resolution cache uses these
/// to prove a cached outcome is still current without recomputing; ordinary
/// callers use [`run_resolution_pipeline`] and discard them.
pub fn run_resolution_pipeline_with_probes(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    composers: &ComposerRegistry,
) -> Result<(ResolutionOutput, Vec<crate::contracts::ProbedAbsence>), ResolutionError> {
    run_resolution_pipeline_with_probes_from_authority(
        item,
        kinds,
        parsers,
        roots,
        trust_store,
        composers,
        None,
    )
}

/// Run the effective pipeline with project discovery and reads sourced
/// exclusively from one admitted project-content authority.
#[allow(clippy::too_many_arguments)]
pub fn run_resolution_pipeline_with_probes_under_project_authority(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    composers: &ComposerRegistry,
    project_root: &std::path::Path,
    project_content: &dyn crate::project_content::AuthoritativeProjectContent,
) -> Result<(ResolutionOutput, Vec<crate::contracts::ProbedAbsence>), ResolutionError> {
    run_resolution_pipeline_with_probes_from_authority(
        item,
        kinds,
        parsers,
        roots,
        trust_store,
        composers,
        Some((project_root, project_content)),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_resolution_pipeline_with_probes_from_authority(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    composers: &ComposerRegistry,
    project_authority: Option<(
        &std::path::Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<(ResolutionOutput, Vec<crate::contracts::ProbedAbsence>), ResolutionError> {
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
        project_authority,
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
    run_effective_item_pipeline_with_probes(item, kinds, parsers, roots, trust_store, composers)
        .map(|(output, _probes)| output)
}

/// As [`run_effective_item_pipeline`], but also returns the negative
/// dependencies used by admitted resolution caches.
pub fn run_effective_item_pipeline_with_probes(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    composers: &ComposerRegistry,
) -> Result<(ResolutionOutput, Vec<crate::contracts::ProbedAbsence>), ResolutionError> {
    run_effective_item_pipeline_with_probes_from_authority(
        item,
        kinds,
        parsers,
        roots,
        trust_store,
        composers,
        None,
    )
}

/// Resolve and compose an executable or data item exclusively beneath one
/// admitted project-content authority.
#[allow(clippy::too_many_arguments)]
pub fn run_effective_item_pipeline_with_probes_under_project_authority(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    composers: &ComposerRegistry,
    project_root: &std::path::Path,
    project_content: &dyn crate::project_content::AuthoritativeProjectContent,
) -> Result<(ResolutionOutput, Vec<crate::contracts::ProbedAbsence>), ResolutionError> {
    run_effective_item_pipeline_with_probes_from_authority(
        item,
        kinds,
        parsers,
        roots,
        trust_store,
        composers,
        Some((project_root, project_content)),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_effective_item_pipeline_with_probes_from_authority(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    composers: &ComposerRegistry,
    project_authority: Option<(
        &std::path::Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<(ResolutionOutput, Vec<crate::contracts::ProbedAbsence>), ResolutionError> {
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
        project_authority,
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
    project_authority: Option<(
        &std::path::Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<(ResolutionOutput, Vec<crate::contracts::ProbedAbsence>), ResolutionError> {
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
    let services = context::ResolutionServices::new(kinds, parsers, roots, trust_store);
    let root_loaded = context::load_item_at(
        services,
        item,
        "<root>",
        ResolutionStepName::PipelineInit,
        project_authority,
    )?;

    let mut ctx = ResolutionContext::new(
        item.clone(),
        services,
        alias_resolver,
        root_loaded,
        project_authority,
    );

    for decl in resolution {
        ctx.run_step(decl)?;
    }

    // Composition runs inside `into_output` while the parser
    // dispatcher's parsed values are still in scope. The envelope
    // never carries those values — only the composed view does.
    ctx.into_output_with_probes(composers, &item.kind, include_references_in_trust)
}
