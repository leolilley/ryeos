use std::path::PathBuf;

use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::contracts::{
    EngineContext, ExecutionCompletion, ExecutionHints, ExecutionPlan, PlanContext, ResolvedItem,
    VerifiedItem,
};
use crate::error::EngineError;
use crate::kind_registry::KindRegistry;
use crate::metadata::MetadataParserRegistry;
use crate::resolution::ResolutionRoots;
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
        parsers: MetadataParserRegistry,
        user_root: Option<PathBuf>,
        system_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            kinds,
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
        let result = crate::resolution::resolve_item_full(&roots, kind_schema, item_ref)?;

        // Read file content
        let content = std::fs::read_to_string(&result.winner_path).map_err(|e| {
            EngineError::Internal(format!(
                "failed to read {}: {e}",
                result.winner_path.display()
            ))
        })?;

        // Compute content hash
        let hash = crate::resolution::content_hash(&content);

        // Parse signature header using the matched extension's envelope
        let signature_header = kind_schema
            .spec_for(&result.matched_ext)
            .and_then(|spec| {
                crate::resolution::parse_signature_header(&content, &spec.signature)
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

        // Parse raw document, then apply extraction rules from the schema
        let parsed = self.parsers.extract(&content, &source_format.parser_id)?;
        let metadata = crate::metadata::apply_extraction_rules(
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
        let roots = self.resolution_roots(project_root);

        // Kind schemas and trust are system-only — no overlays
        crate::plan_builder::build_plan(
            item,
            parameters,
            hints,
            ctx,
            &self.kinds,
            &self.parsers,
            &roots,
            self.kinds.fingerprint(),
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

    /// Get the kind registry's cache fingerprint.
    pub fn registry_fingerprint(&self) -> &str {
        self.kinds.fingerprint()
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
        lillux::signature::sign_content(yaml, &test_signing_key(), "#", None)
    }

    const TOOL_SCHEMA_YAML: &str = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
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
    }

    fn test_engine() -> Engine {
        Engine::new(
            KindRegistry::empty(),
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
        let ts = test_trust_store();
        write_signed_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("hello.py"), "print('hello')\n").unwrap();

        let engine = Engine::new(
            kinds,
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
    parser_id: yaml/yaml
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
            MetadataParserRegistry::with_builtins(),
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
        assert_eq!(resolved.source_format.parser_id, "python/ast");
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
            MetadataParserRegistry::with_builtins(),
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
}
