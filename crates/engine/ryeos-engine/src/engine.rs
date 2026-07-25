use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::composers::ComposerRegistry;
use crate::contracts::{
    EngineContext, ExecutionCompletion, ExecutionHints, ExecutionPlan, PlanContext, ResolvedItem,
    VerifiedItem,
};
use crate::error::EngineError;
use crate::item_resolution::{ResolutionRoot, ResolutionRoots};
use crate::kind_registry::KindRegistry;
use crate::launch_preparers::LaunchPreparerRegistry;
use crate::parsers::ParserDispatcher;
use crate::protocols::ProtocolRegistry;
use crate::runtime_registry::RuntimeRegistry;
use crate::trust::TrustStore;
use crate::AI_DIR;

/// Request for an effective, composed item value.
#[derive(Debug, Clone)]
pub struct EffectiveItemRequest {
    pub item_ref: CanonicalRef,
    pub expected_kind: Option<String>,
    pub project_root: Option<PathBuf>,
}

/// Source metadata for an effective item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveItemSource {
    pub path: PathBuf,
    /// Whole-file SHA-256 of the exact root bytes used by resolution.
    pub content_hash: String,
    /// The installed bundle root (parent of `.ai/`) when the item
    /// came from an installed bundle space. `None` for project-space
    /// items, or when the resolver cannot determine
    /// the bundle boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_root: Option<PathBuf>,
}

/// Diagnostic emitted while producing an effective item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveItemDiagnostic {
    pub level: String,
    pub message: String,
}

/// Engine-owned effective item response. This is valid for executable
/// and non-executable kinds; callers decide whether to execute,
/// render, inspect, or otherwise consume the composed value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveItem {
    pub requested_ref: String,
    pub canonical_ref: String,
    pub kind: String,
    pub trusted: bool,
    pub trust_class: crate::resolution::TrustClass,
    pub root_trust_class: crate::resolution::TrustClass,
    pub source: EffectiveItemSource,
    pub provenance: crate::resolution::ResolutionProvenance,
    pub composed_value: Value,
    pub derived: std::collections::HashMap<String, Value>,
    pub policy_facts: std::collections::HashMap<String, Value>,
    pub diagnostics: Vec<EffectiveItemDiagnostic>,
}

/// Trust, parser dispatch, and the downstream cache fingerprint captured for
/// one request under one checked installed-bundle generation.
#[derive(Debug, Clone)]
pub struct EffectiveRequestSnapshot {
    pub trust_store: TrustStore,
    pub parser_dispatcher: ParserDispatcher,
    pub registry_fingerprint: String,
    /// Effective item-trust identity, including the caller-overlay identity
    /// even when that overlay adds no new signer.
    pub effective_trust_identity: String,
    /// Process-local identity of the immutable admitted engine/bundle
    /// generation backing this snapshot.
    pub request_engine_generation_identity: String,
}

/// Concrete native engine.
///
/// Holds the kind registry and metadata parser registry. Exposes the
/// four pipeline methods directly — no trait boundary, no dyn dispatch
/// at the seam. The seam is the data contracts.
#[derive(Debug, Clone)]
pub struct Engine {
    pub kinds: KindRegistry,
    pub parser_dispatcher: ParserDispatcher,
    /// Combined item trust for the current project/request.
    pub trust_store: TrustStore,
    /// Persistent node trust used exclusively for installed bundle
    /// schemas, handlers, protocols, and native executable manifests. Project
    /// keys and caller-scoped overlays never enter this store.
    pub node_trust_store: TrustStore,
    /// Per-kind composer registry — owned by the engine so boot
    /// validation and the daemon-side resolution pipeline see the
    /// **same** instance (no split-brain between launcher and
    /// runtime construction sites).
    pub composers: ComposerRegistry,

    /// Catalog of verified `kind: runtime` items, scanned at engine
    /// init via `RuntimeRegistry::build_from_bundles`. Empty by
    /// default so test sites that construct an engine directly without
    /// a runtimes scan still compile.
    pub runtimes: RuntimeRegistry,

    /// Boot-bound runtime→launch-preparer registry. Handler preparation is
    /// always resolved through this verified binding rather than looking up a
    /// handler dynamically at launch time.
    pub launch_preparers: LaunchPreparerRegistry,

    /// Protocol registry — loaded from base roots at engine init.
    /// Protocol descriptors declare wire contracts for subprocess
    /// terminators. Empty by default for test compatibility.
    pub protocols: ProtocolRegistry,

    /// Operator-supplied allowlist + snapshot for host-env passthrough
    /// (`${VAR}` in tool env values). Populated once at daemon bootstrap
    /// from `RYEOS_TOOL_ENV_PASSTHROUGH`. Empty by default for test
    /// compatibility.
    pub host_env: crate::runtime::HostEnvBindings,

    /// System bundle roots (parents of `AI_DIR`)
    pub bundle_roots: Vec<PathBuf>,

    /// Immutable signed-registration identities corresponding one-to-one with
    /// `bundle_roots`. Production engines populate this from the retained node
    /// generation; directory basenames are never treated as bundle identity.
    registered_bundle_roots: Vec<crate::item_resolution::RegisteredBundleRoot>,

    /// Operator-owned `.ai/` root. This is intentionally excluded from
    /// ordinary item resolution and is admitted only for signed launch-config
    /// inputs, between an active project and installed bundles.
    operator_ai_root: Option<PathBuf>,

    /// Generation guard shared with launch preparation. It is inert for
    /// directly-constructed test engines and active for node engines.
    isolation_generation: std::sync::Arc<crate::isolation::IsolationRuntime>,

    /// Base item trust for a project-scoped engine, excluding the project's
    /// mutable trust directory. This lets every request re-read project trust
    /// and observe both additions and removals.
    request_trust_base: Option<TrustStore>,

    /// Distinguishes caller-supplied trust authority even when every supplied
    /// signer was already present in the persistent trust base.
    request_trust_overlay_identity: Option<String>,

    /// Shared only by clones of this admitted engine generation. Cache keys
    /// also include the effective generation/trust identities.
    parser_overlay_cache: std::sync::Arc<crate::parser_overlay_cache::ParserOverlayCache>,
}

/// Read-only engine view bound to one verified installed-bundle generation.
///
/// A caller resolving a coherent batch (for example a surface and all of its
/// views) uses this view so the generation is locked and verified once around
/// the whole batch instead of once per item.
pub struct CheckedEngineGeneration<'a> {
    engine: &'a Engine,
}

fn parallel_map_ordered<T, U>(items: &[T], operation: impl Fn(&T) -> U + Sync) -> Vec<U>
where
    T: Sync,
    U: Send,
{
    if items.len() <= 1 {
        return items.iter().map(operation).collect();
    }
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(8)
        .min(items.len());
    let chunk_size = items.len().div_ceil(worker_count);
    std::thread::scope(|scope| {
        let operation = &operation;
        items
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(move || chunk.iter().map(operation).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

impl CheckedEngineGeneration<'_> {
    pub fn resolve(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
    ) -> Result<ResolvedItem, EngineError> {
        self.engine.resolve_current(ctx, item_ref)
    }

    pub fn effective_item(
        &self,
        request: EffectiveItemRequest,
    ) -> Result<EffectiveItem, EngineError> {
        self.engine.effective_item_current(request)
    }

    pub fn verify(
        &self,
        ctx: &PlanContext,
        item: ResolvedItem,
    ) -> Result<VerifiedItem, EngineError> {
        self.engine.verify(ctx, item)
    }

    pub fn build_plan(
        &self,
        ctx: &PlanContext,
        item: &VerifiedItem,
        parameters: &Value,
        hints: &ExecutionHints,
    ) -> Result<ExecutionPlan, EngineError> {
        self.engine.build_plan_current(ctx, item, parameters, hints)
    }

    /// Resolve independent canonical items concurrently while retaining this
    /// generation and preserving input order.
    pub fn resolve_many(
        &self,
        ctx: &PlanContext,
        item_refs: &[CanonicalRef],
    ) -> Vec<Result<ResolvedItem, EngineError>> {
        parallel_map_ordered(item_refs, |item_ref| {
            self.engine.resolve_current(ctx, item_ref)
        })
    }

    /// Compose independent effective items concurrently while retaining this
    /// generation and preserving input order.
    pub fn effective_items(
        &self,
        requests: &[EffectiveItemRequest],
    ) -> Vec<Result<EffectiveItem, EngineError>> {
        parallel_map_ordered(requests, |request| {
            self.engine.effective_item_current(request.clone())
        })
    }
}

impl Engine {
    pub fn new(
        kinds: KindRegistry,
        parser_dispatcher: ParserDispatcher,
        bundle_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            kinds,
            parser_dispatcher,
            trust_store: TrustStore::empty(),
            node_trust_store: TrustStore::empty(),
            composers: ComposerRegistry::new(),
            runtimes: RuntimeRegistry::default(),
            launch_preparers: LaunchPreparerRegistry::default(),
            protocols: ProtocolRegistry::empty(),
            host_env: crate::runtime::HostEnvBindings::default(),
            bundle_roots,
            registered_bundle_roots: Vec::new(),
            operator_ai_root: None,
            isolation_generation: std::sync::Arc::new(crate::isolation::IsolationRuntime::default()),
            request_trust_base: None,
            request_trust_overlay_identity: None,
            parser_overlay_cache: std::sync::Arc::new(
                crate::parser_overlay_cache::ParserOverlayCache::default(),
            ),
        }
    }

    pub fn with_isolation_generation(
        mut self,
        isolation: std::sync::Arc<crate::isolation::IsolationRuntime>,
    ) -> Self {
        self.isolation_generation = isolation;
        self
    }

    pub fn with_registered_bundle_roots(
        mut self,
        registered: Vec<crate::item_resolution::RegisteredBundleRoot>,
    ) -> Self {
        self.registered_bundle_roots = registered;
        self
    }

    pub fn registered_bundle_name_for_root(&self, root: &std::path::Path) -> Option<&str> {
        self.registered_bundle_roots
            .iter()
            .find(|bundle| bundle.canonical_root == root)
            .map(|bundle| bundle.name.as_str())
    }

    /// Stable identity for this engine's complete admitted installed-bundle
    /// generation. Executor verification caches bind to the whole generation,
    /// rather than only the root that happened to publish a matching binary,
    /// so an all-roots ambiguity check can never be bypassed by a cache hit.
    pub fn registered_bundle_generation_fingerprint(&self) -> String {
        match self.isolation_generation.registered_generation_identity() {
            Some(identity) => lillux::cas::sha256_hex(
                format!("ryeos:admitted-process-generation:v1:{identity}").as_bytes(),
            ),
            // Directly-constructed fixture engines have no retained daemon
            // generation. Their cache namespace remains isolated by their
            // registry/handler/root identity and all signed manifest refs.
            None => self.request_engine_generation_identity(),
        }
    }

    /// Assert the daemon-only executor-cache binding invariant without
    /// widening normal engine authority APIs. Fixture/standalone engines have
    /// no registry-owned root identities and use the per-ref probe namespace.
    pub fn debug_assert_executor_cache_generation_identity(&self) {
        debug_assert!(
            self.registered_bundle_roots.is_empty()
                || self
                    .isolation_generation
                    .registered_generation_identity()
                    .is_some(),
            "registry-owned daemon bundle roots must carry their retained generation identity"
        );
    }

    pub fn with_operator_ai_root(mut self, operator_ai_root: PathBuf) -> Self {
        self.operator_ai_root = Some(operator_ai_root);
        self
    }

    fn checked_bundle_generation<T>(
        &self,
        operation: impl FnOnce() -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        self.with_checked_bundle_generation(|_| operation())
    }

    /// Run a coherent read batch against one verified installed-bundle
    /// generation. Supported bundle mutations are excluded for the duration;
    /// concurrent read batches remain independent.
    pub fn with_checked_bundle_generation<T, E>(
        &self,
        operation: impl FnOnce(&CheckedEngineGeneration<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<EngineError>,
    {
        let _operation_guard = self
            .isolation_generation
            .begin_registered_generation_operation()
            .map_err(E::from)?;
        self.isolation_generation
            .ensure_registered_generation_current()
            .map_err(E::from)?;
        let generation = CheckedEngineGeneration { engine: self };
        let value = operation(&generation)?;
        self.isolation_generation
            .ensure_registered_generation_current()
            .map_err(E::from)?;
        Ok(value)
    }

    pub fn with_trust_store(mut self, trust_store: TrustStore) -> Self {
        self.trust_store = trust_store;
        self.request_trust_base = None;
        self.request_trust_overlay_identity = None;
        self
    }

    pub fn with_node_trust_store(mut self, trust_store: TrustStore) -> Self {
        self.node_trust_store = trust_store;
        self
    }

    /// Derive a project-scoped engine from this already-admitted node
    /// generation.
    ///
    /// Installed schemas, handlers, runtimes, protocols, host bindings, and
    /// the isolation generation are immutable for a daemon generation and are
    /// cloned from the admitted engine. Only item trust is rebuilt from the
    /// pinned project root plus an optional caller-scoped overlay. This avoids
    /// re-admitting node executors while serving a request and guarantees the
    /// project engine cannot observe a different installed-bundle generation.
    pub fn for_project_root(
        &self,
        project_root: &Path,
        trust_overlay: Option<&TrustStore>,
    ) -> Result<Self, EngineError> {
        let mut request_trust_base = self.node_trust_store.clone();
        if let Some(overlay) = trust_overlay {
            request_trust_base.extend_from(overlay);
        }
        let trust_store = request_trust_base
            .with_project_keys(project_root)?
            .into_owned();
        let mut engine = self.clone();
        engine.trust_store = trust_store;
        engine.request_trust_base = Some(request_trust_base);
        engine.request_trust_overlay_identity = trust_overlay.map(TrustStore::fingerprint);
        Ok(engine)
    }

    fn effective_trust_store(
        &self,
        project_root: Option<&Path>,
    ) -> Result<Cow<'_, TrustStore>, EngineError> {
        match project_root {
            Some(root) => {
                let base = self
                    .request_trust_base
                    .as_ref()
                    .unwrap_or(&self.trust_store);
                Ok(base.with_project_keys(root)?)
            }
            None => Ok(Cow::Borrowed(&self.trust_store)),
        }
    }

    /// Capture the trust store, parser dispatcher, and downstream registry
    /// fingerprint from one coherent request snapshot.
    ///
    /// Project trust is always re-read before the parser overlay cache is
    /// consulted. The entire operation runs inside the installed-bundle
    /// generation guard.
    pub fn effective_request_snapshot(
        &self,
        project_root: Option<&Path>,
    ) -> Result<EffectiveRequestSnapshot, EngineError> {
        self.checked_bundle_generation(|| self.effective_request_snapshot_current(project_root))
    }

    fn effective_request_snapshot_current(
        &self,
        project_root: Option<&Path>,
    ) -> Result<EffectiveRequestSnapshot, EngineError> {
        let trust_store = self.effective_trust_store(project_root)?.into_owned();
        let parser_dispatcher =
            self.effective_parser_dispatcher_with_trust(project_root, &trust_store)?;
        let registry_fingerprint =
            self.fingerprint_for(parser_dispatcher.parser_tools.fingerprint());
        let effective_trust_identity = self.effective_trust_identity(&trust_store);
        let request_engine_generation_identity = self.request_engine_generation_identity();
        Ok(EffectiveRequestSnapshot {
            trust_store,
            parser_dispatcher,
            registry_fingerprint,
            effective_trust_identity,
            request_engine_generation_identity,
        })
    }

    fn effective_trust_identity(&self, effective: &TrustStore) -> String {
        let base = self
            .request_trust_base
            .as_ref()
            .unwrap_or(&self.trust_store);
        let mut identity = Vec::new();
        append_identity_field(&mut identity, effective.fingerprint().as_bytes());
        append_identity_field(&mut identity, base.fingerprint().as_bytes());
        match &self.request_trust_overlay_identity {
            Some(overlay) => {
                identity.push(1);
                append_identity_field(&mut identity, overlay.as_bytes());
            }
            None => identity.push(0),
        }
        lillux::cas::sha256_hex(&identity)
    }

    /// Install the catalog of `kind: runtime` items, normally built
    /// once at daemon startup by scanning bundle roots. Optional —
    /// `Engine::new` initializes the field to an empty registry.
    pub fn with_runtimes(mut self, runtimes: RuntimeRegistry) -> Self {
        self.runtimes = runtimes;
        self
    }

    pub fn with_launch_preparers(mut self, launch_preparers: LaunchPreparerRegistry) -> Self {
        self.launch_preparers = launch_preparers;
        self
    }

    /// Install the daemon's composer registry. Boot uses this same
    /// instance for validation; the launcher pulls it back off the
    /// engine when running the resolution pipeline so the two sides
    /// can never diverge.
    pub fn with_composers(mut self, composers: ComposerRegistry) -> Self {
        self.composers = composers;
        self
    }

    /// Install the protocol registry, loaded from base roots at engine
    /// init. Empty by default for test compatibility.
    pub fn with_protocols(mut self, protocols: ProtocolRegistry) -> Self {
        self.protocols = protocols;
        self
    }

    /// Install the host-env passthrough bindings. Populated once at
    /// daemon bootstrap from `RYEOS_TOOL_ENV_PASSTHROUGH`. Empty by
    /// default for test compatibility.
    pub fn with_host_env(mut self, host_env: crate::runtime::HostEnvBindings) -> Self {
        self.host_env = host_env;
        self
    }

    /// Resolve a canonical ref to a concrete item.
    pub fn resolve(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
    ) -> Result<ResolvedItem, EngineError> {
        self.checked_bundle_generation(|| self.resolve_current(ctx, item_ref))
    }

    fn resolve_current(
        &self,
        ctx: &PlanContext,
        item_ref: &CanonicalRef,
    ) -> Result<ResolvedItem, EngineError> {
        // Materialize project context
        let project_root = match &ctx.project_context {
            crate::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
            _ => None,
        };

        // Kind schemas are system-only — no project overlay
        let kind_schema =
            self.kinds
                .get(&item_ref.kind)
                .ok_or_else(|| EngineError::UnsupportedKind {
                    kind: item_ref.kind.clone(),
                })?;

        // Build resolution roots (system-first order)
        let roots = self.resolution_roots(project_root.clone());

        tracing::debug!(item_ref = %item_ref, "resolving item");

        // Resolve to file path + space + matched extension (with clash diagnostics)
        let result = crate::item_resolution::resolve_item_full(&roots, kind_schema, item_ref)?;

        // Read file content
        let content = std::fs::read_to_string(&result.winner_path).map_err(|e| {
            EngineError::Internal(format!(
                "failed to read {}: {e}",
                result.winner_path.display()
            ))
        })?;

        // Compute content hash
        let hash = crate::item_resolution::content_hash(&content);

        // Parse signature header using the matched extension's envelope
        let signature_header = kind_schema.spec_for(&result.matched_ext).and_then(|spec| {
            crate::item_resolution::parse_signature_header(&content, &spec.signature)
        });

        // Build ResolvedSourceFormat from the matched extension
        let source_format = kind_schema
            .resolved_format_for(&result.matched_ext)
            .ok_or_else(|| {
                EngineError::Internal(format!(
                    "matched extension {} has no source format in schema",
                    result.matched_ext
                ))
            })?;

        // Pin the exact signature-stripped bytes consumed by runtimes. Hook
        // occurrence identities use this digest, not the whole signed-file
        // digest carried in `content_hash`.
        let raw_content = lillux::signature::strip_signature_lines_with_envelope(
            &content,
            &source_format.signature.prefix,
            source_format.signature.suffix.as_deref(),
        );
        let raw_content_digest = crate::item_resolution::content_hash(&raw_content);

        // Parse raw document via the **effective** parser dispatcher
        // — the boot dispatcher overlaid by this project's
        // `.ai/parsers/` if any. Then apply extraction rules from
        // the schema.
        let request_snapshot = self.effective_request_snapshot_current(project_root.as_deref())?;
        let parsed = request_snapshot.parser_dispatcher.dispatch(
            &source_format.parser,
            &content,
            Some(&result.winner_path),
            &source_format.signature,
        )?;
        // Path-anchoring validator runs BEFORE metadata extraction
        // populates the typed slots — a failure here is a structural
        // mismatch between metadata and on-disk location, not a parse
        // error. Item rejected at load time, daemon stays kind-agnostic.
        crate::kind_registry::validate_metadata_anchoring(
            &parsed,
            &kind_schema.extraction_rules,
            &kind_schema.directory,
            &result.winner_ai_root,
            &result.winner_path,
        )
        .map_err(|source| EngineError::MetadataAnchoringFailed {
            canonical_ref: item_ref.to_string(),
            source: Box::new(source),
        })?;

        let metadata = crate::kind_registry::apply_extraction_rules(
            &parsed,
            &kind_schema.extraction_rules,
            &result.winner_path,
            &kind_schema.directory,
        );

        tracing::debug!(
            item_ref = %item_ref,
            source_path = %result.winner_path.display(),
            space = %result.winner_space.as_str(),
            resolved_from = %result.winner_label,
            shadowed = result.shadowed.len(),
            "resolved item"
        );

        Ok(ResolvedItem {
            canonical_ref: item_ref.clone(),
            kind: item_ref.kind.clone(),
            source_path: result.winner_path,
            source_space: result.winner_space,
            resolved_from: result.winner_label,
            shadowed: result.shadowed,
            materialized_project_root: project_root,
            raw_content_digest,
            content_hash: hash,
            signature_header,
            source_format,
            metadata,
        })
    }

    /// Verify trust and integrity on a resolved item.
    ///
    /// Trust is the configured store plus keys explicitly declared by this
    /// request's project root.
    pub fn verify(
        &self,
        ctx: &PlanContext,
        item: ResolvedItem,
    ) -> Result<VerifiedItem, EngineError> {
        let project_root = match &ctx.project_context {
            crate::contracts::ProjectContext::LocalPath { path } => Some(path.as_path()),
            _ => None,
        };
        let trust_store = self.effective_trust_store(project_root)?;
        let result = crate::trust::verify_resolved_item(item, &trust_store);
        if let Ok(ref verified) = result {
            tracing::debug!(
                item_ref = %verified.resolved.canonical_ref,
                trust_class = ?verified.trust_class,
                "verified item"
            );
        }
        result
    }

    /// Resolve, verify, compose, and return an effective item value.
    ///
    /// Unlike [`Engine::build_plan`], this is intentionally
    /// non-executing and works for non-executable kinds such as
    /// `surface` and `client`. It reuses the same resolution pipeline
    /// and composer registry that launch paths use, so service/API/CLI
    /// consumers do not grow parallel item semantics.
    pub fn effective_item(
        &self,
        request: EffectiveItemRequest,
    ) -> Result<EffectiveItem, EngineError> {
        self.checked_bundle_generation(|| self.effective_item_current(request))
    }

    fn effective_item_current(
        &self,
        request: EffectiveItemRequest,
    ) -> Result<EffectiveItem, EngineError> {
        let ref_str = request.item_ref.to_string();

        if let Some(expected) = &request.expected_kind {
            if expected != &request.item_ref.kind {
                return Err(EngineError::EffectiveItemWrongKind {
                    canonical_ref: ref_str,
                    expected: expected.clone(),
                    found: request.item_ref.kind.clone(),
                });
            }
        }

        let roots = self.resolution_roots(request.project_root.clone());
        let project_root = request.project_root.as_deref();
        let request_snapshot = self.effective_request_snapshot_current(project_root)?;
        let output = crate::resolution::run_effective_item_pipeline(
            &request.item_ref,
            &self.kinds,
            &request_snapshot.parser_dispatcher,
            &roots,
            &request_snapshot.trust_store,
            &self.composers,
        )
        .map_err(|e| {
            // Map resolution pipeline errors to typed effective-item
            // error variants so consumers can branch on error code.
            use crate::resolution::ResolutionError;
            match &e {
                ResolutionError::StepFailed { .. } => EngineError::EffectiveItemCompositionFailed {
                    canonical_ref: ref_str.clone(),
                    reason: e.to_string(),
                },
                ResolutionError::CycleDetected { .. }
                | ResolutionError::MaxDepthExceeded { .. } => {
                    EngineError::EffectiveItemCompositionFailed {
                        canonical_ref: ref_str.clone(),
                        reason: e.to_string(),
                    }
                }
                ResolutionError::IntegrityFailure { reason, .. } => {
                    EngineError::EffectiveItemUntrusted {
                        canonical_ref: ref_str.clone(),
                        fingerprint: reason.clone(),
                    }
                }
                ResolutionError::MissingItem { item_ref, .. } => {
                    EngineError::EffectiveItemNotFound {
                        canonical_ref: item_ref.clone(),
                    }
                }
                ResolutionError::ComposedValueContractViolation {
                    item_ref, report, ..
                } => EngineError::ComposedValueContractViolation {
                    canonical_ref: item_ref.clone(),
                    report: report.clone(),
                },
                _ => EngineError::EffectiveItemCompositionFailed {
                    canonical_ref: ref_str.clone(),
                    reason: e.to_string(),
                },
            }
        })?;

        let trust_class = output.effective_trust_class;
        let trusted = matches!(
            trust_class,
            crate::resolution::TrustClass::TrustedBundle
                | crate::resolution::TrustClass::TrustedProject
        );
        let provenance = output.provenance();

        // Determine bundle_root: check if the source path falls under
        // one of the bundle roots (installed bundle spaces). The bundle
        // root is the parent of the `.ai/` directory.
        let bundle_root = self
            .bundle_roots
            .iter()
            .find(|root| output.root.source_path.starts_with(root))
            .cloned();

        // Build diagnostics from the resolution output.
        let mut diagnostics = Vec::new();

        // Shadowing diagnostics: if ancestors exist, note the extends
        // chain.
        if !output.ancestors.is_empty() {
            diagnostics.push(EffectiveItemDiagnostic {
                level: "info".into(),
                message: format!(
                    "extends chain: {} -> {}",
                    output.root.resolved_ref,
                    output
                        .ancestors
                        .iter()
                        .map(|a| a.resolved_ref.as_str())
                        .collect::<Vec<_>>()
                        .join(" -> ")
                ),
            });
        }

        Ok(EffectiveItem {
            requested_ref: request.item_ref.to_string(),
            canonical_ref: output.root.resolved_ref.clone(),
            kind: request.item_ref.kind,
            trusted,
            trust_class,
            root_trust_class: output.root.trust_class,
            source: EffectiveItemSource {
                path: output.root.source_path,
                content_hash: output.root.source_content_digest,
                bundle_root,
            },
            provenance,
            composed_value: output.composed.composed,
            derived: output.composed.derived,
            policy_facts: output.composed.policy_facts,
            diagnostics,
        })
    }

    /// Build a normalized execution plan from a verified item.
    ///
    /// Checks execution scope on the principal before building.
    /// Uses system-only kind schemas and system+user trust.
    pub fn build_plan(
        &self,
        ctx: &PlanContext,
        item: &VerifiedItem,
        parameters: &Value,
        hints: &ExecutionHints,
    ) -> Result<ExecutionPlan, EngineError> {
        self.checked_bundle_generation(|| self.build_plan_current(ctx, item, parameters, hints))
    }

    fn build_plan_current(
        &self,
        ctx: &PlanContext,
        item: &VerifiedItem,
        parameters: &Value,
        hints: &ExecutionHints,
    ) -> Result<ExecutionPlan, EngineError> {
        crate::scope::check_execution_scope(&ctx.requested_by)?;

        tracing::debug!(
            item_ref = %item.resolved.canonical_ref,
            "building execution plan"
        );

        let project_root = match &ctx.project_context {
            crate::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
            _ => None,
        };
        let roots = self.resolution_roots(project_root.clone());
        let request_snapshot = self.effective_request_snapshot_current(project_root.as_deref())?;

        crate::plan_builder::build_plan(crate::plan_builder::BuildPlanInput {
            item,
            parameters,
            hints,
            ctx,
            kinds: &self.kinds,
            parsers: &request_snapshot.parser_dispatcher,
            roots: &roots,
            registry_fingerprint: &request_snapshot.registry_fingerprint,
            trust_store: &request_snapshot.trust_store,
            node_trust_store: &self.node_trust_store,
            host_env: &self.host_env,
        })
    }

    /// Resolve which execution routine a root item's executor chain terminal
    /// selects, without building a subprocess plan.
    ///
    /// The dispatcher uses this to branch subprocess vs method-dispatch on the
    /// terminal's typed `terminal_executor:` descriptor (never on the alias
    /// name or terminal ref). Acquires the same per-request roots / effective
    /// parsers / trust store as `build_plan`.
    pub fn resolve_terminal_executor(
        &self,
        root_source_path: &std::path::Path,
        root_executor_id: &str,
        root_kind: &str,
        project_root: Option<PathBuf>,
    ) -> Result<crate::plan_builder::ResolvedTerminalExecutor, EngineError> {
        self.checked_bundle_generation(|| {
            self.resolve_terminal_executor_current(
                root_source_path,
                root_executor_id,
                root_kind,
                project_root,
            )
        })
    }

    fn resolve_terminal_executor_current(
        &self,
        root_source_path: &std::path::Path,
        root_executor_id: &str,
        root_kind: &str,
        project_root: Option<PathBuf>,
    ) -> Result<crate::plan_builder::ResolvedTerminalExecutor, EngineError> {
        let roots = self.resolution_roots(project_root.clone());
        let request_snapshot = self.effective_request_snapshot_current(project_root.as_deref())?;
        crate::plan_builder::resolve_terminal_executor(
            root_executor_id,
            root_source_path,
            root_kind,
            &self.kinds,
            &request_snapshot.parser_dispatcher,
            &roots,
            &request_snapshot.trust_store,
        )
    }

    /// Execute a plan via Lillux subprocess dispatch.
    pub fn execute_plan(
        &self,
        ctx: &EngineContext,
        plan: ExecutionPlan,
    ) -> Result<ExecutionCompletion, EngineError> {
        self.checked_bundle_generation(|| {
            tracing::debug!(plan_id = %plan.plan_id, "executing plan");
            let result = crate::dispatch::execute_plan(&plan, ctx);
            if let Ok(ref completion) = result {
                tracing::info!(plan_id = %plan.plan_id, status = ?completion.status, "plan execution completed");
            }
            result
        })
    }

    /// Spawn a plan's subprocess without waiting.
    /// Returns a handle the daemon can use to persist pid/pgid before waiting.
    pub fn spawn_plan(
        &self,
        ctx: &EngineContext,
        plan: &ExecutionPlan,
    ) -> Result<crate::dispatch::SpawnedExecutionAwaitingAttachment, EngineError> {
        self.checked_bundle_generation(|| {
            tracing::debug!(plan_id = %plan.plan_id, "spawning plan");
            crate::dispatch::spawn_plan(plan, ctx)
        })
    }

    /// Build resolution roots for a given project root (project-first order).
    pub fn resolution_roots(&self, project_root: Option<PathBuf>) -> ResolutionRoots {
        if !self.registered_bundle_roots.is_empty() {
            return ResolutionRoots::from_registered(project_root, &self.registered_bundle_roots);
        }
        let system_ai: Vec<PathBuf> = self.bundle_roots.iter().map(|p| p.join(AI_DIR)).collect();
        let project_ai = project_root.map(|p| p.join(AI_DIR));
        ResolutionRoots::from_flat(project_ai, system_ai)
    }

    /// Add operator configuration to launch-config lookup only. Keeping this
    /// separate prevents mutable node state from becoming a general item root.
    pub fn launch_config_roots(&self, roots: &ResolutionRoots) -> ResolutionRoots {
        let mut ordered = roots.ordered.clone();
        let Some(operator_ai_root) = &self.operator_ai_root else {
            return ResolutionRoots { ordered };
        };
        if ordered.iter().any(|root| root.ai_root == *operator_ai_root) {
            return ResolutionRoots { ordered };
        }
        let position = ordered
            .iter()
            .position(|root| root.space == crate::contracts::ItemSpace::Bundle)
            .unwrap_or(ordered.len());
        ordered.insert(
            position,
            ResolutionRoot {
                space: crate::contracts::ItemSpace::Project,
                label: "operator".to_string(),
                ai_root: operator_ai_root.clone(),
            },
        );
        ResolutionRoots { ordered }
    }

    /// Composite cache fingerprint over the kind registry and the
    /// **boot-time** parser tool registry. Use
    /// `effective_registry_fingerprint(project_root)` for per-request
    /// fingerprints that include the project's parser overlay.
    pub fn registry_fingerprint(&self) -> String {
        self.fingerprint_for(self.parser_dispatcher.parser_tools.fingerprint())
    }

    /// Per-request composite fingerprint that folds in the **effective**
    /// parser registry — i.e. the boot registry overlaid by the
    /// project's `.ai/parsers/`. Plan caches must key on this so a
    /// project-local parser change invalidates downstream entries.
    ///
    pub fn effective_registry_fingerprint(
        &self,
        project_root: Option<&Path>,
    ) -> Result<String, EngineError> {
        Ok(self
            .effective_request_snapshot(project_root)?
            .registry_fingerprint)
    }

    /// Compose the engine's composite fingerprint over the kind
    /// registry, the supplied parser-tools fingerprint, and the
    /// composer set. Pub-crate so callers (notably `build_plan`) can
    /// derive a fingerprint from a `ParserDispatcher` they already
    /// loaded — preserving the single-snapshot guarantee.
    pub(crate) fn fingerprint_for(&self, parser_tools_fp: &str) -> String {
        // Composers contribute a stable digest of their registered
        // kinds: changing the composer set must invalidate any cache
        // keyed off the fingerprint.
        let mut composer_kinds: Vec<&str> = self.composers.kinds().collect();
        composer_kinds.sort();
        let composer_fp = lillux::cas::sha256_hex(composer_kinds.join(",").as_bytes());
        let combined = format!(
            "{}|{}|{}",
            self.kinds.fingerprint(),
            parser_tools_fp,
            composer_fp,
        );
        lillux::cas::sha256_hex(combined.as_bytes())
    }

    /// Build the effective parser dispatcher for a request.
    ///
    /// Without a project root, returns a clone of the boot dispatcher
    /// (cheap — `ParserRegistry` is `HashMap`-cloning, the handler
    /// registry is held by `Arc`).
    ///
    /// With a project root, applies `with_project_overlay` against
    /// the project's `.ai/parsers/` so descriptors declared inside
    /// the project shadow base entries with the same canonical ref.
    pub fn effective_parser_dispatcher(
        &self,
        project_root: Option<&Path>,
    ) -> Result<ParserDispatcher, EngineError> {
        Ok(self
            .effective_request_snapshot(project_root)?
            .parser_dispatcher)
    }

    fn effective_parser_dispatcher_with_trust(
        &self,
        project_root: Option<&Path>,
        trust_store: &TrustStore,
    ) -> Result<ParserDispatcher, EngineError> {
        match project_root {
            None => Ok(self.parser_dispatcher.clone()),
            Some(path) => {
                // The `parser` kind is load-bearing: it tells the
                // overlay loader which directory to scan, which file
                // extensions to accept, and which signature envelope
                // to verify with. A manually-constructed engine that
                // forgot to register it would otherwise *silently*
                // lose its project overlays — turning a project's
                // `.ai/parsers/` into a no-op the moment a project
                // root is supplied. Fail loud instead so the
                // misconfiguration surfaces at the first
                // `resolve` / `build_plan` instead of as a confusing
                // "ParserNotRegistered" two layers down. Production
                // boots register the parser kind via
                // `KindRegistry::load_base`, so this only fires for
                // test fixtures and embeddings.
                if self.kinds.get("parser").is_none() {
                    return Err(EngineError::SchemaLoaderError {
                        reason: "parser kind schema not registered — \
                                 required for parser overlay loading"
                            .into(),
                    });
                }
                let overlay_root =
                    crate::parsers::ParserRegistry::project_overlay_root(path, &self.kinds)?;
                if !overlay_root.exists() {
                    tracing::debug!(
                        project_root = %path.display(),
                        rebuild_reason = "no_overlay",
                        "using base parser dispatcher"
                    );
                    return Ok(self.parser_dispatcher.clone());
                }

                let metadata =
                    crate::parser_overlay_cache::fingerprint_parser_overlay(&overlay_root)?;
                let base_trust = self
                    .request_trust_base
                    .as_ref()
                    .unwrap_or(&self.trust_store);
                let key = crate::parser_overlay_cache::ParserOverlayCacheKey {
                    project_root: path.to_path_buf(),
                    overlay_fingerprint: metadata.fingerprint,
                    effective_trust_fingerprint: trust_store.fingerprint(),
                    base_trust_fingerprint: base_trust.fingerprint(),
                    caller_overlay_identity: self.request_trust_overlay_identity.clone(),
                    generation_fingerprint: self.request_engine_generation_identity(),
                };
                self.parser_overlay_cache.get_or_build(
                    key,
                    metadata.cacheable,
                    metadata.total_file_bytes,
                    || {
                        let overlaid = self.parser_dispatcher.parser_tools.with_project_overlay(
                            path,
                            trust_store,
                            &self.kinds,
                        )?;
                        Ok(self.parser_dispatcher.with_parser_tools(overlaid))
                    },
                )
            }
        }
    }

    fn request_engine_generation_identity(&self) -> String {
        let mut generation = Vec::new();
        if let Some(identity) = self.isolation_generation.registered_generation_identity() {
            append_identity_field(&mut generation, &identity.to_le_bytes());
        }
        append_identity_field(&mut generation, self.registry_fingerprint().as_bytes());
        append_identity_field(
            &mut generation,
            self.parser_dispatcher.handler_cache_identity().as_bytes(),
        );
        for registered in &self.registered_bundle_roots {
            append_identity_field(&mut generation, registered.name.as_bytes());
            append_identity_field(
                &mut generation,
                registered.canonical_root.as_os_str().as_encoded_bytes(),
            );
        }
        lillux::cas::sha256_hex(&generation)
    }
}

fn append_identity_field(bytes: &mut Vec<u8>, field: &[u8]) {
    bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
    bytes.extend_from_slice(field);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        EffectivePrincipal, ExecutionHints, ItemSpace, Principal, ProjectContext, TrustClass,
    };
    use crate::trust::{TrustStore, TrustedSigner};
    use base64::Engine as _;
    use lillux::crypto::SigningKey;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct CountingGenerationLifeline {
        begins: AtomicUsize,
        checks: AtomicUsize,
    }

    impl crate::isolation::IsolationGenerationLifeline for CountingGenerationLifeline {
        fn begin_operation(&self) -> Result<Box<dyn Send + Sync>, String> {
            self.begins.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(()))
        }

        fn ensure_current(&self) -> Result<(), String> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    fn test_trust_store() -> TrustStore {
        let sk = test_signing_key();
        let vk = sk.verifying_key();
        let fp = crate::trust::compute_fingerprint(&vk);
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp,
            verifying_key: vk,
            label: None,
        }])
    }

    fn sign_schema_yaml(yaml: &str) -> String {
        // composed_value_contract is now mandatory on every kind
        // schema; inject an empty mapping for tests that don't
        // exercise contract semantics.
        let yaml_owned = if yaml.contains("composed_value_contract") {
            yaml.to_string()
        } else {
            {
                let with_contract = format!(
                    "{yaml}composed_value_contract:\n  root_type: mapping\n  required: {{}}\n"
                );
                if with_contract.contains("composer:") {
                    with_contract
                } else {
                    format!("{with_contract}composer: handler:ryeos/core/identity\n")
                }
            }
        };
        let yaml_owned = if yaml_owned.contains("effective_trust:") {
            yaml_owned
        } else {
            format!("{yaml_owned}effective_trust:\n  include_references: false\n")
        };
        let yaml_owned = if yaml_owned.contains("resolution:") {
            yaml_owned
        } else {
            format!("{yaml_owned}resolution: []\n")
        };
        lillux::signature::sign_content(&yaml_owned, &test_signing_key(), "#", None)
    }

    const TOOL_SCHEMA_YAML: &str = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser: parser:ryeos/core/python/tool-header
    signature:
      prefix: \"#\"
      after_shebang: true
";

    fn write_signed_tool_schema(kinds_dir: &Path) {
        let tool_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("tool.kind-schema.yaml"),
            sign_schema_yaml(TOOL_SCHEMA_YAML),
        )
        .unwrap();
        // The `parser` kind is load-bearing for any engine that may
        // be asked to resolve with a project root: `Engine::
        // effective_parser_dispatcher` requires it. Co-write it here
        // so every test fixture that ships a tool schema also ships
        // the minimum kind set a real engine needs.
        write_signed_parser_kind_schema(kinds_dir);
    }

    fn test_engine() -> Engine {
        Engine::new(
            KindRegistry::empty(),
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
    }

    fn test_plan_context() -> PlanContext {
        PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::None,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        }
    }

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir =
            std::env::temp_dir().join(format!("rye_engine_test_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn engine_construction() {
        let engine = test_engine();
        // The composite fingerprint is sha256(kinds_fp | parser_tools_fp);
        // both inputs are deterministic so the fingerprint must be
        // non-empty and stable across runs.
        let fp = engine.registry_fingerprint();
        assert!(!fp.is_empty());
        assert_eq!(fp, test_engine().registry_fingerprint());
    }

    #[test]
    fn operator_root_is_added_only_to_launch_config_precedence() {
        let engine = test_engine().with_operator_ai_root(PathBuf::from("/operator/.ai"));
        let ordinary = engine.resolution_roots(Some(PathBuf::from("/project")));
        assert_eq!(ordinary.ordered.len(), engine.bundle_roots.len() + 1);
        assert!(!ordinary
            .ordered
            .iter()
            .any(|root| root.ai_root == Path::new("/operator/.ai")));

        let launch = engine.launch_config_roots(&ordinary);
        assert_eq!(launch.ordered[0].label, "project");
        assert_eq!(launch.ordered[1].label, "operator");
        assert_eq!(
            launch.ordered[1].space,
            crate::contracts::ItemSpace::Project
        );
    }

    #[test]
    fn checked_generation_batches_multiple_resolutions_under_one_guard() {
        let lifeline = std::sync::Arc::new(CountingGenerationLifeline::default());
        let isolation = crate::isolation::IsolationRuntime::default().retain_registered_generation(
            lifeline.clone(),
            TrustStore::empty(),
            vec![],
        );
        let engine = test_engine().with_isolation_generation(std::sync::Arc::new(isolation));
        let ctx = test_plan_context();
        let item_ref = CanonicalRef::parse("tool:missing").unwrap();

        engine
            .with_checked_bundle_generation(|generation| -> Result<(), EngineError> {
                let results = generation.resolve_many(&ctx, &[item_ref.clone(), item_ref]);
                assert_eq!(results.len(), 2);
                assert!(results.into_iter().all(|result| result.is_err()));
                Ok(())
            })
            .unwrap();

        assert_eq!(lifeline.begins.load(Ordering::SeqCst), 1);
        assert_eq!(lifeline.checks.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retained_generation_identity_does_not_alias_same_registered_paths() {
        let roots = vec![crate::item_resolution::RegisteredBundleRoot {
            name: "same-name".to_string(),
            canonical_root: PathBuf::from("/same/canonical/root"),
        }];
        let first = crate::isolation::IsolationRuntime::default().retain_registered_generation(
            std::sync::Arc::new(CountingGenerationLifeline::default()),
            TrustStore::empty(),
            roots.clone(),
        );
        let second = crate::isolation::IsolationRuntime::default().retain_registered_generation(
            std::sync::Arc::new(CountingGenerationLifeline::default()),
            TrustStore::empty(),
            roots.clone(),
        );
        let first = test_engine()
            .with_registered_bundle_roots(roots.clone())
            .with_isolation_generation(std::sync::Arc::new(first));
        let second = test_engine()
            .with_registered_bundle_roots(roots)
            .with_isolation_generation(std::sync::Arc::new(second));

        assert_ne!(
            first.registered_bundle_generation_fingerprint(),
            second.registered_bundle_generation_fingerprint(),
        );
    }

    #[test]
    fn resolve_rejects_unknown_kind() {
        let engine = test_engine();
        let ctx = test_plan_context();
        let r = CanonicalRef::parse("tool:ryeos/bash/bash").unwrap();
        let err = engine.resolve(&ctx, &r).unwrap_err();
        assert!(
            matches!(err, EngineError::UnsupportedKind { ref kind } if kind == "tool"),
            "expected UnsupportedKind, got: {err:?}"
        );
    }

    #[test]
    fn resolution_roots_with_project() {
        let engine = test_engine();
        let roots = engine.resolution_roots(Some(PathBuf::from("/workspace/project")));
        assert!(roots.ordered.iter().any(|r| r.space == ItemSpace::Project));
        let project_root = roots
            .ordered
            .iter()
            .find(|r| r.space == ItemSpace::Project)
            .unwrap();
        assert_eq!(
            project_root.ai_root,
            PathBuf::from("/workspace/project/.ai")
        );
    }

    #[test]
    fn registered_resolution_roots_keep_project_ai_root_first() {
        let engine = test_engine().with_registered_bundle_roots(vec![
            crate::item_resolution::RegisteredBundleRoot {
                name: "core".to_owned(),
                canonical_root: PathBuf::from("/bundles/core"),
            },
        ]);
        let roots = engine.resolution_roots(Some(PathBuf::from("/workspace/project")));

        assert_eq!(roots.ordered.len(), 2);
        assert_eq!(roots.ordered[0].space, ItemSpace::Project);
        assert_eq!(roots.ordered[0].label, "project");
        assert_eq!(
            roots.ordered[0].ai_root,
            PathBuf::from("/workspace/project/.ai")
        );
        assert_eq!(roots.ordered[1].space, ItemSpace::Bundle);
        assert_eq!(roots.ordered[1].label, "bundle:core");
        assert_eq!(roots.ordered[1].ai_root, PathBuf::from("/bundles/core/.ai"));
    }

    #[test]
    fn resolution_roots_without_project() {
        let engine = test_engine();
        let roots = engine.resolution_roots(None);
        assert!(!roots.ordered.iter().any(|r| r.space == ItemSpace::Project));
    }

    #[test]
    fn resolve_finds_item() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("hello.py"),
            "# ryeos:signed:2026-04-10T00:00:00Z:abc123:sigdata:fp_test\n# ryeos-tool:\n#   note: hello\nprint('hello')\n",
        )
        .unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath {
                path: project_dir.clone(),
            },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();

        assert_eq!(resolved.kind, "tool");
        assert_eq!(resolved.source_space, ItemSpace::Project);
        assert_eq!(resolved.source_format.extension, ".py");
        assert_eq!(
            resolved.source_format.parser,
            "parser:ryeos/core/python/tool-header"
        );
        assert!(resolved.signature_header.is_some());
        let sig = resolved.signature_header.unwrap();
        assert_eq!(sig.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(sig.content_hash, "abc123");
        assert_eq!(sig.signer_fingerprint, "fp_test");
        assert_eq!(resolved.materialized_project_root, Some(project_dir));
        assert!(!resolved.content_hash.is_empty());
        assert_eq!(
            resolved.raw_content_digest,
            crate::item_resolution::content_hash(
                "# ryeos-tool:\n#   note: hello\nprint('hello')\n"
            )
        );
        assert_ne!(resolved.raw_content_digest, resolved.content_hash);
    }

    fn signed_tool_content(
        body: &str,
        signing_key: &lillux::crypto::SigningKey,
        fingerprint: &str,
    ) -> String {
        use lillux::crypto::Signer;
        use sha2::{Digest, Sha256};

        let hash = {
            let h = Sha256::digest(body.as_bytes());
            let mut out = String::with_capacity(64);
            for byte in h.iter() {
                use std::fmt::Write;
                let _ = write!(&mut out, "{byte:02x}");
            }
            out
        };
        let sig: lillux::crypto::Signature = signing_key.sign(hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        format!("# ryeos:signed:2026-04-10T00:00:00Z:{hash}:{sig_b64}:{fingerprint}\n{body}")
    }

    #[test]
    fn resolve_then_verify_trusted() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let signing_key = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fp = crate::trust::compute_fingerprint(&verifying_key);

        let body = "# ryeos-tool:\n#   note: hello\nprint('hello')\n";
        let content = signed_tool_content(body, &signing_key, &fp);
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.py"), &content).unwrap();

        let trust_store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key,
            label: None,
        }]);

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
        .with_trust_store(trust_store);

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();

        assert_eq!(verified.trust_class, TrustClass::Trusted);
        assert_eq!(verified.signer.as_ref().unwrap().0, fp);
    }

    #[test]
    fn resolve_then_verify_unsigned() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("hello.py"),
            "# ryeos-tool:\n#   note: hello\nprint('hello')\n",
        )
        .unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();

        assert_eq!(verified.trust_class, TrustClass::Unsigned);
        assert!(verified.signer.is_none());
    }

    #[test]
    fn resolve_then_verify_untrusted_signer() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let signing_key = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let fp = crate::trust::compute_fingerprint(&signing_key.verifying_key());

        let body = "# ryeos-tool:\n#   note: hello\nprint('hello')\n";
        let content = signed_tool_content(body, &signing_key, &fp);
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.py"), &content).unwrap();

        // Engine with EMPTY trust store
        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();

        assert_eq!(verified.trust_class, TrustClass::Untrusted);
        assert_eq!(verified.signer.as_ref().unwrap().0, fp);
    }

    #[test]
    fn resolve_ignores_project_kind_overlay() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        // Project overlay: tool → .yaml only — should be IGNORED
        let overlay_dir = project_dir
            .join(crate::AI_DIR)
            .join(crate::KIND_SCHEMAS_DIR)
            .join("tool");
        fs::create_dir_all(&overlay_dir).unwrap();
        let overlay_yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".yaml\"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: \"#\"
";
        fs::write(
            overlay_dir.join("tool.kind-schema.yaml"),
            sign_schema_yaml(overlay_yaml),
        )
        .unwrap();

        // Write a .py tool file (should resolve because system schema has .py)
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("hello.py"),
            "# ryeos-tool:\n#   note: hello\nprint('hello')\n",
        )
        .unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
        .with_trust_store(ts);

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath {
                path: project_dir.clone(),
            },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        // .py file should resolve (system schema, not project overlay)
        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();
        assert_eq!(resolved.source_format.extension, ".py");
        assert_eq!(
            resolved.source_format.parser,
            "parser:ryeos/core/python/tool-header"
        );
    }

    #[test]
    fn resolve_project_first_with_clash() {
        let project_dir = tempdir();
        let system_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        // Write the same item in both system and project
        let sys_tool_dir = system_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&sys_tool_dir).unwrap();
        fs::write(
            sys_tool_dir.join("hello.py"),
            "# ryeos-tool:\n#   note: system\nprint('sys')\n",
        )
        .unwrap();

        let proj_tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&proj_tool_dir).unwrap();
        fs::write(
            proj_tool_dir.join("hello.py"),
            "# ryeos-tool:\n#   note: project\nprint('proj')\n",
        )
        .unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![system_dir],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath { path: project_dir },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();

        // Project wins over bundles.
        assert_eq!(resolved.source_space, ItemSpace::Project);
        assert_eq!(resolved.resolved_from, "project");

        // Bundle copy is shadowed.
        assert_eq!(resolved.shadowed.len(), 1);
        assert_eq!(resolved.shadowed[0].space, ItemSpace::Bundle);
    }

    /// Without a project root, the effective dispatcher MUST be
    /// equivalent to the boot dispatcher — same parser tool registry,
    /// same fingerprint. The whole point of the per-request seam is
    /// that overlays cost nothing when there's no project to overlay.
    #[test]
    fn effective_dispatcher_no_project_root_returns_boot_clone() {
        let engine = test_engine();
        let effective = engine.effective_parser_dispatcher(None).unwrap();
        assert_eq!(
            effective.parser_tools.fingerprint(),
            engine.parser_dispatcher.parser_tools.fingerprint(),
            "no-project effective dispatcher must mirror boot fingerprint"
        );
        assert_eq!(
            engine.effective_registry_fingerprint(None).unwrap(),
            engine.registry_fingerprint(),
            "no-project effective composite fingerprint must equal boot fingerprint"
        );
    }

    #[test]
    fn project_scoped_engine_does_not_retain_deleted_project_trust() {
        let project_dir = tempdir();
        let trusted_dir = project_dir.join(crate::AI_DIR).join(crate::TRUST_KEYS_DIR);
        fs::create_dir_all(&trusted_dir).unwrap();

        let signing_key = SigningKey::from_bytes(&[77u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = crate::trust::compute_fingerprint(&verifying_key);
        let encoded = base64::engine::general_purpose::STANDARD.encode(verifying_key.as_bytes());
        let key_path = trusted_dir.join("project.pub");
        fs::write(&key_path, encoded).unwrap();

        let scoped = test_engine().for_project_root(&project_dir, None).unwrap();
        assert!(scoped.trust_store.is_trusted(&fingerprint));

        fs::remove_file(key_path).unwrap();
        let current = scoped.effective_trust_store(Some(&project_dir)).unwrap();
        assert!(
            !current.is_trusted(&fingerprint),
            "project trust removals must be visible to the next request"
        );
    }

    const PARSER_KIND_SCHEMA: &str = "\
location:
  directory: parsers
formats:
  - extensions: [\".yaml\"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: \"#\"
";

    fn write_signed_parser_kind_schema(kinds_dir: &Path) {
        let parser_dir = kinds_dir.join("parser");
        fs::create_dir_all(&parser_dir).unwrap();
        fs::write(
            parser_dir.join("parser.kind-schema.yaml"),
            sign_schema_yaml(PARSER_KIND_SCHEMA),
        )
        .unwrap();
    }

    /// Tool kind schema that points at a parser ref the test builtins
    /// do NOT register — only the project overlay supplies it. If
    /// resolution went through the boot dispatcher, parsing would
    /// fail with `ParserNotRegistered`. If it goes through the
    /// effective dispatcher, the overlay rescues the parse.
    const TOOL_SCHEMA_USING_PROJECT_PARSER: &str = "\
location:
  directory: tools
formats:
  - extensions: [\".pyx\"]
    parser: parser:proj/only
    signature:
      prefix: \"#\"
";

    fn write_signed_parser_descriptor(project_dir: &Path, rel_id: &str, yaml: &str) {
        let path = project_dir
            .join(crate::AI_DIR)
            .join("parsers")
            .join(format!("{rel_id}.yaml"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Parser descriptors require `output_schema`. Inject empty
        // mapping if not present so existing test fixtures that
        // don't exercise contract semantics keep working.
        let yaml_owned = if yaml.contains("output_schema") {
            yaml.to_string()
        } else {
            format!("{yaml}output_schema:\n  root_type: mapping\n  required: {{}}\n")
        };
        // sign_schema_yaml also injects composed_value_contract for
        // KIND schemas; that's harmless for descriptors since the
        // descriptor parser uses `deny_unknown_fields` only on its
        // own struct, and this body is appended as a top-level field
        // — in practice all tests that use this helper write
        // descriptors not kind schemas, so the contract injection
        // would actually corrupt them. Sign directly.
        let signed = lillux::signature::sign_content(&yaml_owned, &test_signing_key(), "#", None);
        fs::write(path, signed).unwrap();
    }

    /// A project's `.ai/parsers/` MUST surface in the per-request
    /// effective fingerprint — otherwise plan caches keyed off the
    /// boot fingerprint would silently serve stale results when a
    /// project ships its own parser overlay.
    #[test]
    fn effective_dispatcher_with_project_root_includes_overlay() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_parser_kind_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
        .with_trust_store(ts);

        let boot_fp = engine.registry_fingerprint();
        let no_project_fp = engine.effective_registry_fingerprint(None).unwrap();
        assert_eq!(boot_fp, no_project_fp);

        // Project ships a parser descriptor that shadows
        // `parser:ryeos/core/yaml/yaml`. Even though the descriptor
        // body is identical in shape to the test built-in, the
        // serialized bytes differ (different version field), so the
        // overlay MUST change the registry fingerprint.
        write_signed_parser_descriptor(
            &project_dir,
            "ryeos/core/yaml/yaml",
            "version: \"9.9.9-project-overlay\"\n\
             handler: \"handler:ryeos/core/yaml-document\"\n\
             parser_api_version: 1\n\
             parser_config: {}\n",
        );

        let with_project_fp = engine
            .effective_registry_fingerprint(Some(&project_dir))
            .expect("effective fingerprint with project root");

        assert_ne!(
            boot_fp, with_project_fp,
            "project overlay MUST shift the per-request fingerprint; \
             plan caches would otherwise serve stale results. \
             boot={boot_fp} project={with_project_fp}"
        );

        // And the dispatcher itself MUST carry the overlay's
        // descriptor — same canonical ref, project's version string.
        let effective = engine
            .effective_parser_dispatcher(Some(&project_dir))
            .unwrap();
        let descriptor = effective
            .parser_tools
            .get("parser:ryeos/core/yaml/yaml")
            .expect("project overlay descriptor present in effective dispatcher");
        assert_eq!(
            descriptor.version, "9.9.9-project-overlay",
            "effective dispatcher must serve the project's overlaid descriptor, \
             not the boot version"
        );
    }

    /// End-to-end: `engine.resolve()` MUST go through the per-request
    /// effective dispatcher. The system tool kind cites a parser ref
    /// (`parser:proj/only`) that the boot dispatcher does NOT register
    /// — only the project's `.ai/parsers/` overlay supplies it. If
    /// resolve still hit the boot dispatcher this test would fail
    /// with `ParserNotRegistered`.
    #[test]
    fn engine_resolve_uses_project_overlay_parser() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_parser_kind_schema(&kinds_dir);

        // Tool kind schema that names a parser only the project supplies.
        let tool_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("tool.kind-schema.yaml"),
            sign_schema_yaml(TOOL_SCHEMA_USING_PROJECT_PARSER),
        )
        .unwrap();

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        // Project-local parser descriptor — the only place
        // `parser:proj/only` is defined. Re-uses the yaml_document
        // native handler so we don't have to register a new one.
        write_signed_parser_descriptor(
            &project_dir,
            "proj/only",
            "version: \"1.0.0\"\n\
             handler: \"handler:ryeos/core/yaml-document\"\n\
             parser_api_version: 1\n\
             parser_config:\n  require_mapping: true\n",
        );

        // Tool file the engine will resolve. The body is valid YAML
        // (the proj/only parser is a yaml_document handler), so the
        // parse succeeds iff the overlay's descriptor is resolved.
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.pyx"), "name: hello\n").unwrap();

        // Empty-handler boot dispatcher would crash on parser lookup
        // even with the overlay if effective dispatcher wasn't used —
        // but the canonical-bundle test dispatcher provides handlers,
        // so the overlay just supplies the descriptor.
        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
        .with_trust_store(ts);

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath {
                path: project_dir.clone(),
            },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine
            .resolve(&ctx, &ref_)
            .expect("resolve must succeed via project overlay parser");
        assert_eq!(resolved.source_format.parser, "parser:proj/only");
        assert_eq!(resolved.source_format.extension, ".pyx");
    }

    /// The request snapshot must carry a cache fingerprint derived from the
    /// exact dispatcher it carries. `build_plan` consumes this same object, so
    /// parser behaviour, parser identity, and trust cannot come from separate
    /// overlay reads.
    #[test]
    fn effective_snapshot_fingerprint_matches_its_dispatcher() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        // Project ships a parser overlay so the effective fingerprint
        // genuinely diverges from boot — otherwise the structural
        // identity would still hold but the test would be trivial.
        write_signed_parser_descriptor(
            &project_dir,
            "ryeos/core/yaml/yaml",
            "version: \"7.7.7-snapshot-test\"\n\
             handler: \"handler:ryeos/core/yaml-document\"\n\
             parser_api_version: 1\n\
             parser_config: {}\n",
        );

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            vec![],
        )
        .with_trust_store(ts);

        let snapshot = engine
            .effective_request_snapshot(Some(&project_dir))
            .expect("effective request snapshot loads");
        let via_dispatcher =
            engine.fingerprint_for(snapshot.parser_dispatcher.parser_tools.fingerprint());

        assert_eq!(
            snapshot.registry_fingerprint, via_dispatcher,
            "the request snapshot fingerprint must describe the dispatcher \
             in that same snapshot"
        );

        // Test setup sanity: the overlay must actually shift the
        // fingerprint, otherwise the equality above is vacuous.
        assert_ne!(
            snapshot.registry_fingerprint,
            engine.registry_fingerprint(),
            "test setup must produce a non-trivial overlay shift"
        );
    }
}
