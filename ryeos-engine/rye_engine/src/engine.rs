use std::path::PathBuf;

use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::contracts::{
    EngineContext, ExecutionCompletion, ExecutionHints, ExecutionPlan, PlanContext, ResolvedItem,
    VerifiedItem,
};
use crate::error::EngineError;
use crate::executor_registry::ExecutorRegistry;
use crate::kind_registry::KindRegistry;
use crate::metadata::MetadataParserRegistry;
use crate::resolution::ResolutionRoots;
use crate::trust::TrustStore;
use crate::AI_DIR;

/// Concrete native engine.
///
/// Holds the kind registry, executor registry, and metadata parser
/// registry. Exposes the four pipeline methods directly — no trait
/// boundary, no dyn dispatch at the seam. The seam is the data contracts.
#[derive(Debug)]
pub struct Engine {
    pub kinds: KindRegistry,
    pub executors: ExecutorRegistry,
    pub parsers: MetadataParserRegistry,
    pub trust_store: TrustStore,

    /// User-space root (parent of `AI_DIR`)
    pub user_root: Option<PathBuf>,
    /// System bundle roots (parents of `AI_DIR`)
    pub system_roots: Vec<PathBuf>,
}

impl Engine {
    pub fn new(
        kinds: KindRegistry,
        executors: ExecutorRegistry,
        parsers: MetadataParserRegistry,
        user_root: Option<PathBuf>,
        system_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            kinds,
            executors,
            parsers,
            trust_store: TrustStore::empty(),
            user_root,
            system_roots,
        }
    }

    pub fn with_trust_store(mut self, trust_store: TrustStore) -> Self {
        self.trust_store = trust_store;
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

        // Apply project kind-schema overlay
        let effective_kinds = self.effective_kinds(ctx)?;

        // Validate kind against the (possibly overlaid) registry
        let kind_schema = effective_kinds.get(&item_ref.kind).ok_or_else(|| {
            EngineError::UnsupportedKind {
                kind: item_ref.kind.clone(),
            }
        })?;

        // Build resolution roots
        let roots = self.resolution_roots(project_root.clone());

        tracing::debug!(item_ref = %item_ref, "resolving item");

        // Resolve to file path + space + matched extension
        let (source_path, source_space, matched_ext) =
            crate::resolution::resolve_item(&roots, kind_schema, item_ref)?;

        // Read file content
        let content = std::fs::read_to_string(&source_path).map_err(|e| {
            EngineError::Internal(format!(
                "failed to read {}: {e}",
                source_path.display()
            ))
        })?;

        // Compute content hash
        let hash = crate::resolution::content_hash(&content);

        // Parse signature header using the matched extension's envelope
        let signature_header = kind_schema
            .spec_for(&matched_ext)
            .and_then(|spec| {
                crate::resolution::parse_signature_header(&content, &spec.signature)
            });

        // Build ResolvedSourceFormat from the matched extension
        let source_format = kind_schema
            .resolved_format_for(&matched_ext)
            .ok_or_else(|| {
                EngineError::Internal(format!(
                    "matched extension {matched_ext} has no source format in schema"
                ))
            })?;

        // Parse raw document, then apply extraction rules from the schema
        let parsed = self.parsers.extract(&content, &source_format.parser_id)?;
        let metadata = crate::metadata::apply_extraction_rules(
            &parsed,
            &kind_schema.extraction_rules,
            &source_path,
        );

        tracing::debug!(
            item_ref = %item_ref,
            source_path = %source_path.display(),
            space = %source_space.as_str(),
            "resolved item"
        );

        Ok(ResolvedItem {
            canonical_ref: item_ref.clone(),
            kind: item_ref.kind.clone(),
            source_path,
            source_space,
            materialized_project_root: project_root,
            content_hash: hash,
            signature_header,
            source_format,
            metadata,
        })
    }

    /// Derive the effective kind registry for a request, applying project
    /// kind-schema overlay when a project root is available.
    fn effective_kinds(&self, ctx: &PlanContext) -> Result<KindRegistry, EngineError> {
        match &ctx.project_context {
            crate::contracts::ProjectContext::LocalPath { path } => {
                let project_kinds_path = path.join(crate::AI_DIR).join(crate::KIND_SCHEMAS_DIR);
                if project_kinds_path.exists() {
                    self.kinds.with_project_overlay(&project_kinds_path)
                } else {
                    Ok(self.kinds.clone())
                }
            }
            _ => Ok(self.kinds.clone()),
        }
    }

    /// Derive an effective trust store for a request, including project-local
    /// trusted keys when a project root is available in the context.
    ///
    /// Merges project-local keys into the startup trust store (which already
    /// contains user + system keys). Project keys take priority on conflict.
    fn effective_trust_store(&self, ctx: &PlanContext) -> Result<TrustStore, EngineError> {
        match &ctx.project_context {
            crate::contracts::ProjectContext::LocalPath { path } => {
                self.trust_store.with_project_keys(path)
            }
            _ => Ok(self.trust_store.clone()),
        }
    }

    /// Verify trust and integrity on a resolved item.
    pub fn verify(
        &self,
        ctx: &PlanContext,
        item: ResolvedItem,
    ) -> Result<VerifiedItem, EngineError> {
        let trust_store = self.effective_trust_store(ctx)?;
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

    /// Build a normalized execution plan from a verified item.
    ///
    /// Checks execution scope on the principal before building.
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
        let roots = self.resolution_roots(project_root);
        let effective_kinds = self.effective_kinds(ctx)?;
        let trust_store = self.effective_trust_store(ctx)?;

        crate::plan_builder::build_plan(
            item,
            parameters,
            hints,
            ctx,
            &self.executors,
            &effective_kinds,
            &self.parsers,
            &roots,
            effective_kinds.fingerprint(),
            &trust_store,
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

    /// Build resolution roots for a given project root.
    pub fn resolution_roots(&self, project_root: Option<PathBuf>) -> ResolutionRoots {
        ResolutionRoots {
            project: project_root.map(|p| p.join(AI_DIR)),
            user: self.user_root.clone().map(|p| p.join(AI_DIR)),
            system: self
                .system_roots
                .iter()
                .map(|p| p.join(AI_DIR))
                .collect(),
        }
    }

    /// Get the kind registry's cache fingerprint.
    pub fn registry_fingerprint(&self) -> &str {
        self.kinds.fingerprint()
    }

    /// Get the default executor ID for a kind, applying project overlay.
    pub fn default_executor_id_for(
        &self,
        ctx: &PlanContext,
        kind: &str,
    ) -> Result<Option<String>, EngineError> {
        let effective = self.effective_kinds(ctx)?;
        Ok(effective.default_executor_id(kind).map(String::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        EffectivePrincipal, ExecutionHints, ItemSpace, Principal, ProjectContext, TrustClass,
    };
    use crate::trust::TrustedSigner;
    use std::fs;

    fn test_engine() -> Engine {
        Engine::new(
            KindRegistry::empty(),
            ExecutorRegistry::new(),
            MetadataParserRegistry::with_builtins(),
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
        assert_eq!(engine.registry_fingerprint(), "empty");
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
        assert_eq!(
            roots.project,
            Some(PathBuf::from("/workspace/project/.ai"))
        );
    }

    #[test]
    fn resolution_roots_without_project() {
        let engine = test_engine();
        let roots = engine.resolution_roots(None);
        assert!(roots.project.is_none());
    }

    #[test]
    fn resolve_finds_item() {
        // Set up a temp project with kind schema and an actual tool file
        let project_dir = tempdir();
        let kinds_dir = tempdir();

        // Write kind schema for "tool" kind
        let tool_schema_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_schema_dir).unwrap();
        fs::write(
            tool_schema_dir.join("tool.kind-schema.yaml"),
            "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
",
        )
        .unwrap();

        // Load kind registry
        let kinds = KindRegistry::load_base(&[kinds_dir]).unwrap();

        // Write a tool file in the project's .ai/tools/ directory
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("hello.py"),
            "# rye:signed:2026-04-10T00:00:00Z:abc123:sigdata:fp_test\nprint('hello')\n",
        )
        .unwrap();

        let engine = Engine::new(
            kinds,
            ExecutorRegistry::new(),
            MetadataParserRegistry::with_builtins(),
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
        assert_eq!(resolved.source_format.parser_id, "python/ast");
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

    /// Helper: create a properly signed tool file and return its content.
    fn signed_tool_content(
        body: &str,
        signing_key: &ed25519_dalek::SigningKey,
        fingerprint: &str,
    ) -> String {
        use ed25519_dalek::Signer;
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
        let sig: ed25519_dalek::Signature = signing_key.sign(hash.as_bytes());
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

        // Kind schema
        let tool_schema_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_schema_dir).unwrap();
        fs::write(
            tool_schema_dir.join("tool.kind-schema.yaml"),
            "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
",
        )
        .unwrap();

        let kinds = KindRegistry::load_base(&[kinds_dir]).unwrap();

        // Generate a key pair
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fp = crate::trust::compute_fingerprint(&verifying_key);

        // Write a properly signed tool file
        let body = "print('hello')\n";
        let content = signed_tool_content(body, &signing_key, &fp);
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.py"), &content).unwrap();

        // Build engine with trust store
        let trust_store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key,
            label: None,
        }]);

        let engine = Engine::new(
            kinds,
            ExecutorRegistry::new(),
            MetadataParserRegistry::with_builtins(),
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

        let tool_schema_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_schema_dir).unwrap();
        fs::write(
            tool_schema_dir.join("tool.kind-schema.yaml"),
            "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
",
        )
        .unwrap();

        let kinds = KindRegistry::load_base(&[kinds_dir]).unwrap();

        // Write an unsigned tool file
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.py"), "print('hello')\n").unwrap();

        let engine = Engine::new(
            kinds,
            ExecutorRegistry::new(),
            MetadataParserRegistry::with_builtins(),
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

        let tool_schema_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_schema_dir).unwrap();
        fs::write(
            tool_schema_dir.join("tool.kind-schema.yaml"),
            "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
",
        )
        .unwrap();

        let kinds = KindRegistry::load_base(&[kinds_dir]).unwrap();

        // Generate key pair but DON'T add to trust store
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let fp = crate::trust::compute_fingerprint(&signing_key.verifying_key());

        let body = "print('hello')\n";
        let content = signed_tool_content(body, &signing_key, &fp);
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.py"), &content).unwrap();

        // Engine with EMPTY trust store
        let engine = Engine::new(
            kinds,
            ExecutorRegistry::new(),
            MetadataParserRegistry::with_builtins(),
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
    fn resolve_uses_project_kind_overlay() {
        // Base registry has tool kind with .py extension
        // Project overlay replaces tool kind with .rb extension only
        // A .rb tool file should resolve; a .py file should NOT
        let project_dir = tempdir();
        let kinds_dir = tempdir();

        // Base kind schema: tool → .py
        let tool_schema_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_schema_dir).unwrap();
        fs::write(
            tool_schema_dir.join("tool.kind-schema.yaml"),
            "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
",
        )
        .unwrap();

        let kinds = KindRegistry::load_base(&[kinds_dir]).unwrap();

        // Project overlay: tool → .yaml only
        let overlay_dir = project_dir.join(crate::AI_DIR).join(crate::KIND_SCHEMAS_DIR).join("tool");
        fs::create_dir_all(&overlay_dir).unwrap();
        fs::write(
            overlay_dir.join("tool.kind-schema.yaml"),
            "\
location:
  directory: tools
formats:
  - extensions: [\".yaml\"]
    parser_id: yaml/yaml
    signature:
      prefix: \"#\"
      after_shebang: false
",
        )
        .unwrap();

        // Write a .yaml tool file (should be found with overlay)
        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.yaml"), "name: hello\n").unwrap();

        let engine = Engine::new(
            kinds,
            ExecutorRegistry::new(),
            MetadataParserRegistry::with_builtins(),
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

        // .yaml file should resolve via overlay
        let ref_ = CanonicalRef::parse("tool:hello").unwrap();
        let resolved = engine.resolve(&ctx, &ref_).unwrap();
        assert_eq!(resolved.source_format.extension, ".yaml");
        assert_eq!(resolved.source_format.parser_id, "yaml/yaml");

        // .py file should NOT resolve (overlay replaced .py with .yaml)
        fs::write(tool_dir.join("other.py"), "print('hello')\n").unwrap();
        let ref_py = CanonicalRef::parse("tool:other").unwrap();
        let err = engine.resolve(&ctx, &ref_py).unwrap_err();
        assert!(matches!(err, EngineError::ItemNotFound { .. }));
    }
}
