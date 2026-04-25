use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::composers::ComposerRegistry;
use crate::contracts::{
    EngineContext, ExecutionCompletion, ExecutionHints, ExecutionPlan, PlanContext, ResolvedItem,
    VerifiedItem,
};
use crate::error::EngineError;
use crate::kind_registry::KindRegistry;
use crate::parsers::ParserDispatcher;
use crate::item_resolution::ResolutionRoots;
use crate::trust::TrustStore;
use crate::AI_DIR;

/// Concrete native engine.
///
/// Holds the kind registry and metadata parser registry. Exposes the
/// four pipeline methods directly — no trait boundary, no dyn dispatch
/// at the seam. The seam is the data contracts.
#[derive(Debug)]
pub struct Engine {
    pub kinds: KindRegistry,
    pub parser_dispatcher: ParserDispatcher,
    pub trust_store: TrustStore,
    /// Per-kind composer registry — owned by the engine so boot
    /// validation and the daemon-side resolution pipeline see the
    /// **same** instance (no split-brain between launcher and
    /// runtime construction sites).
    pub composers: ComposerRegistry,

    /// User-space root (parent of `AI_DIR`)
    pub user_root: Option<PathBuf>,
    /// System bundle roots (parents of `AI_DIR`)
    pub system_roots: Vec<PathBuf>,
}

impl Engine {
    pub fn new(
        kinds: KindRegistry,
        parser_dispatcher: ParserDispatcher,
        user_root: Option<PathBuf>,
        system_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            kinds,
            parser_dispatcher,
            trust_store: TrustStore::empty(),
            composers: ComposerRegistry::new(),
            user_root,
            system_roots,
        }
    }

    pub fn with_trust_store(mut self, trust_store: TrustStore) -> Self {
        self.trust_store = trust_store;
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

    /// Resolve a canonical ref to a concrete item.
    pub fn resolve(
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
        let kind_schema = self.kinds.get(&item_ref.kind).ok_or_else(|| {
            EngineError::UnsupportedKind {
                kind: item_ref.kind.clone(),
            }
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
        let signature_header = kind_schema
            .spec_for(&result.matched_ext)
            .and_then(|spec| {
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

        // Parse raw document via the **effective** parser dispatcher
        // — the boot dispatcher overlaid by this project's
        // `.ai/parsers/` if any. Then apply extraction rules from
        // the schema.
        let effective = self.effective_parser_dispatcher(project_root.as_deref())?;
        let parsed = effective.dispatch(
            &source_format.parser,
            &content,
            Some(&result.winner_path),
            &source_format.signature,
        )?;
        let metadata = crate::kind_registry::apply_extraction_rules(
            &parsed,
            &kind_schema.extraction_rules,
            &result.winner_path,
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
            content_hash: hash,
            signature_header,
            source_format,
            metadata,
        })
    }

    /// Verify trust and integrity on a resolved item.
    ///
    /// Trust store is system + user only — no project widening.
    pub fn verify(
        &self,
        _ctx: &PlanContext,
        item: ResolvedItem,
    ) -> Result<VerifiedItem, EngineError> {
        let result = crate::trust::verify_resolved_item(item, &self.trust_store);
        if let Ok(ref verified) = result {
            tracing::debug!(
                item_ref = %verified.resolved.canonical_ref,
                trust_class = ?verified.trust_class,
                "verified item"
            );
        }
        result
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

        // Per-request: parser tools may be overlaid by the project's
        // `.ai/parsers/`; the cache fingerprint must reflect that.
        //
        // **Single-snapshot guarantee**: the dispatcher and the
        // fingerprint MUST be derived from the same overlay read.
        // Calling `effective_parser_dispatcher(..)` and
        // `effective_registry_fingerprint(..)` separately would walk
        // `.ai/parsers/` twice, opening a window where a concurrent
        // write to the overlay produces a plan whose runtime
        // behaviour (snapshot A) and cache key (snapshot B) disagree.
        let effective_parsers =
            self.effective_parser_dispatcher(project_root.as_deref())?;
        let effective_fp =
            self.fingerprint_for(effective_parsers.parser_tools.fingerprint());

        // Kind schemas and trust are system-only — no overlays
        crate::plan_builder::build_plan(
            item,
            parameters,
            hints,
            ctx,
            &self.kinds,
            &effective_parsers,
            &roots,
            &effective_fp,
            &self.trust_store,
        )
    }

    /// Execute a plan via Lillux subprocess dispatch.
    pub fn execute_plan(
        &self,
        ctx: &EngineContext,
        plan: ExecutionPlan,
    ) -> Result<ExecutionCompletion, EngineError> {
        tracing::debug!(plan_id = %plan.plan_id, "executing plan");
        let result = crate::dispatch::execute_plan(&plan, ctx);
        if let Ok(ref completion) = result {
            tracing::info!(plan_id = %plan.plan_id, status = ?completion.status, "plan execution completed");
        }
        result
    }

    /// Spawn a plan's subprocess without waiting.
    /// Returns a handle the daemon can use to persist pid/pgid before waiting.
    pub fn spawn_plan(
        &self,
        ctx: &EngineContext,
        plan: &ExecutionPlan,
    ) -> Result<crate::dispatch::SpawnedExecution, EngineError> {
        tracing::debug!(plan_id = %plan.plan_id, "spawning plan");
        crate::dispatch::spawn_plan(plan, ctx)
    }

    /// Build resolution roots for a given project root (system-first order).
    pub fn resolution_roots(&self, project_root: Option<PathBuf>) -> ResolutionRoots {
        let system_ai: Vec<PathBuf> = self
            .system_roots
            .iter()
            .map(|p| p.join(AI_DIR))
            .collect();
        let user_ai = self.user_root.clone().map(|p| p.join(AI_DIR));
        let project_ai = project_root.map(|p| p.join(AI_DIR));
        ResolutionRoots::from_flat(project_ai, user_ai, system_ai)
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
    /// NOTE: this performs an independent overlay read. Callers that
    /// already hold an effective `ParserDispatcher` MUST instead call
    /// `fingerprint_for(dispatcher.parser_tools.fingerprint())` to
    /// guarantee dispatcher and fingerprint are derived from the
    /// same snapshot of `.ai/parsers/`. See `build_plan` for the
    /// canonical pattern. This entry point exists for callers that
    /// don't already have a dispatcher in hand (tests, diagnostics).
    pub fn effective_registry_fingerprint(
        &self,
        project_root: Option<&Path>,
    ) -> Result<String, EngineError> {
        let dispatcher = self.effective_parser_dispatcher(project_root)?;
        Ok(self.fingerprint_for(dispatcher.parser_tools.fingerprint()))
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
                let overlaid = self
                    .parser_dispatcher
                    .parser_tools
                    .with_project_overlay(path, &self.trust_store, &self.kinds)?;
                Ok(self.parser_dispatcher.with_parser_tools(overlaid))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        EffectivePrincipal, ExecutionHints, ItemSpace, Principal, ProjectContext, TrustClass,
    };
    use crate::trust::{TrustedSigner, TrustStore};
    use lillux::crypto::SigningKey;
    use std::fs;
    use std::path::Path;

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
            { let with_contract = format!("{yaml}composed_value_contract:\n  root_type: mapping\n  required: {{}}\n"); if with_contract.contains("composer:") { with_contract } else { format!("{with_contract}composer: rye/core/identity\n") } }
        };
        lillux::signature::sign_content(&yaml_owned, &test_signing_key(), "#", None)
    }

    const TOOL_SCHEMA_YAML: &str = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser: parser:rye/core/python/ast
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
            None,
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
        let dir = std::env::temp_dir().join(format!(
            "rye_engine_test_{}_{}",
            std::process::id(),
            nanos
        ));
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
    fn resolve_rejects_unknown_kind() {
        let engine = test_engine();
        let ctx = test_plan_context();
        let r = CanonicalRef::parse("tool:rye/bash/bash").unwrap();
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
        let project_root = roots.ordered.iter().find(|r| r.space == ItemSpace::Project).unwrap();
        assert_eq!(project_root.ai_root, PathBuf::from("/workspace/project/.ai"));
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
            "# rye:signed:2026-04-10T00:00:00Z:abc123:sigdata:fp_test\nprint('hello')\n",
        )
        .unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            None,
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
        assert_eq!(resolved.source_format.parser, "parser:rye/core/python/ast");
        assert!(resolved.signature_header.is_some());
        let sig = resolved.signature_header.unwrap();
        assert_eq!(sig.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(sig.content_hash, "abc123");
        assert_eq!(sig.signer_fingerprint, "fp_test");
        assert_eq!(
            resolved.materialized_project_root,
            Some(project_dir)
        );
        assert!(!resolved.content_hash.is_empty());
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
        format!(
            "# rye:signed:2026-04-10T00:00:00Z:{hash}:{sig_b64}:{fingerprint}\n{body}"
        )
    }

    use base64::Engine as _;

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

        let body = "print('hello')\n";
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
            None,
            vec![],
        )
        .with_trust_store(trust_store);

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath {
                path: project_dir,
            },
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
        fs::write(tool_dir.join("hello.py"), "print('hello')\n").unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            None,
            vec![],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath {
                path: project_dir,
            },
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

        let body = "print('hello')\n";
        let content = signed_tool_content(body, &signing_key, &fp);
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.py"), &content).unwrap();

        // Engine with EMPTY trust store
        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            None,
            vec![],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath {
                path: project_dir,
            },
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
        let overlay_dir = project_dir.join(crate::AI_DIR).join(crate::KIND_SCHEMAS_DIR).join("tool");
        fs::create_dir_all(&overlay_dir).unwrap();
        let overlay_yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".yaml\"]
    parser: parser:rye/core/yaml/yaml
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
        fs::write(tool_dir.join("hello.py"), "print('hello')\n").unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            None,
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
        assert_eq!(resolved.source_format.parser, "parser:rye/core/python/ast");
    }

    #[test]
    fn resolve_system_first_with_clash() {
        let project_dir = tempdir();
        let system_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        // Write the same item in both system and project
        let sys_tool_dir = system_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&sys_tool_dir).unwrap();
        fs::write(sys_tool_dir.join("hello.py"), "# system\nprint('sys')\n").unwrap();

        let proj_tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&proj_tool_dir).unwrap();
        fs::write(proj_tool_dir.join("hello.py"), "# project\nprint('proj')\n").unwrap();

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            None,
            vec![system_dir],
        );

        let ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::LocalPath {
                path: project_dir,
            },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        };

        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();

        // System wins
        assert_eq!(resolved.source_space, ItemSpace::System);
        assert_eq!(resolved.resolved_from, "system(node)");

        // Project is shadowed
        assert_eq!(resolved.shadowed.len(), 1);
        assert_eq!(resolved.shadowed[0].space, ItemSpace::Project);
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

    const PARSER_KIND_SCHEMA: &str = "\
location:
  directory: parsers
formats:
  - extensions: [\".yaml\"]
    parser: parser:rye/core/yaml/yaml
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
            None,
            vec![],
        )
        .with_trust_store(ts);

        let boot_fp = engine.registry_fingerprint();
        let no_project_fp = engine.effective_registry_fingerprint(None).unwrap();
        assert_eq!(boot_fp, no_project_fp);

        // Project ships a parser descriptor that shadows
        // `parser:rye/core/yaml/yaml`. Even though the descriptor
        // body is identical in shape to the test built-in, the
        // serialized bytes differ (different version field), so the
        // overlay MUST change the registry fingerprint.
        write_signed_parser_descriptor(
            &project_dir,
            "rye/core/yaml/yaml",
            "version: \"9.9.9-project-overlay\"\n\
             executor_id: \"native:parser_yaml_document\"\n\
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
            .get("parser:rye/core/yaml/yaml")
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
             executor_id: \"native:parser_yaml_document\"\n\
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
            None,
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

    /// Single-snapshot guarantee for `build_plan`: the parser
    /// dispatcher and the cache fingerprint MUST come from the same
    /// overlay read of `.ai/parsers/`. We can't easily race the file
    /// system in a unit test, so assert the structural identity that
    /// makes the guarantee hold by construction:
    ///
    ///   `effective_registry_fingerprint(p)`
    ///     ≡ `fingerprint_for(effective_parser_dispatcher(p)
    ///                            .parser_tools.fingerprint())`
    ///
    /// `build_plan` derives its fingerprint via the right-hand side,
    /// reusing the dispatcher it just loaded. The left-hand side is
    /// kept available for callers that don't already hold a
    /// dispatcher. As long as both spellings agree, no caller can
    /// open a TOCTOU window by accidentally calling them separately.
    /// If a future refactor splits the snapshot again, this assert
    /// catches the divergence in CI.
    #[test]
    fn effective_fingerprint_matches_dispatcher_derived_fingerprint() {
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
            "rye/core/yaml/yaml",
            "version: \"7.7.7-snapshot-test\"\n\
             executor_id: \"native:parser_yaml_document\"\n\
             parser_api_version: 1\n\
             parser_config: {}\n",
        );

        let engine = Engine::new(
            kinds,
            crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(),
            None,
            vec![],
        )
        .with_trust_store(ts);

        let via_helper = engine
            .effective_registry_fingerprint(Some(&project_dir))
            .expect("effective fingerprint loads");

        let dispatcher = engine
            .effective_parser_dispatcher(Some(&project_dir))
            .expect("effective dispatcher loads");
        let via_dispatcher =
            engine.fingerprint_for(dispatcher.parser_tools.fingerprint());

        assert_eq!(
            via_helper, via_dispatcher,
            "the two spellings of the per-request fingerprint MUST \
             agree — `build_plan` relies on this to derive its cache \
             key from the same dispatcher snapshot it executes \
             against. Divergence here means a concurrent overlay \
             write could produce a plan whose runtime behaviour and \
             cache key disagree."
        );

        // Test setup sanity: the overlay must actually shift the
        // fingerprint, otherwise the equality above is vacuous.
        assert_ne!(
            via_helper,
            engine.registry_fingerprint(),
            "test setup must produce a non-trivial overlay shift"
        );
    }
}
