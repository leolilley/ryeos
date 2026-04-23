//! Chain builder — turns a `VerifiedItem` into an `ExecutionPlan`.
//!
//! The builder follows the executor chain: starting from the root item's
//! `executor_id`, it walks through tool items until hitting a terminal
//! executor registered in the `ExecutorRegistry`. The script to execute
//! is the last resolved tool in the chain (or the root item itself if
//! the first executor_id is already terminal).
//!
//! Executor selection is driven by metadata (`executor_id`) and kind
//! defaults — the engine never branches on item kind strings.

use std::collections::HashMap;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::canonical_ref::CanonicalRef;
use crate::contracts::{
    ExecutionHints, ExecutionPlan, PlanCapabilities, PlanContext, PlanNode, PlanNodeId,
    TrustClass, VerifiedItem,
};
use crate::error::EngineError;
use crate::executor_registry::{ExecutorRegistry, SubprocessDispatch};
use crate::kind_registry::KindRegistry;
use crate::metadata::MetadataParserRegistry;
use crate::resolution::ResolutionRoots;
use crate::trust::TrustStore;

/// Maximum executor chain depth before we assume a cycle or misconfiguration.
const MAX_CHAIN_DEPTH: usize = 16;

/// Result of resolving the executor chain to a terminal subprocess dispatch.
struct ChainTerminal {
    /// The script to execute (source_path of the executor tool, or root
    /// item's source_path if the first executor_id was directly terminal)
    script_path: PathBuf,
    /// Terminal subprocess dispatch config from the registry
    dispatch: SubprocessDispatch,
    /// The terminal executor ID that was found in the registry
    terminal_executor_id: String,
    /// All executor IDs traversed in the chain (for debugging/auditing)
    chain: Vec<String>,
    /// Trust verification results for each intermediate chain hop: (executor_id, trust_class).
    /// Retained for daemon audit logging; not directly consumed by `build_plan`.
    #[allow(dead_code)]
    verified_chain: Vec<(String, TrustClass)>,
    /// Content hashes of intermediate chain hops (for cache key computation)
    chain_content_hashes: Vec<String>,
}

/// Resolve the executor chain from a starting executor_id to a terminal
/// subprocess dispatch.
///
/// The chain is: executor_id → (if not terminal, parse as canonical ref,
/// resolve tool, get its executor_id) → repeat until terminal.
///
/// Returns the resolved chain terminal with the script to execute and
/// the subprocess config.
fn resolve_executor_chain(
    starting_executor_id: &str,
    root_source_path: &PathBuf,
    executors: &ExecutorRegistry,
    kinds: &KindRegistry,
    parsers: &MetadataParserRegistry,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
) -> Result<ChainTerminal, EngineError> {
    let mut current_id = starting_executor_id.to_owned();
    let mut visited: Vec<String> = Vec::new();
    let mut last_resolved_path: Option<PathBuf> = None;
    let mut verified_chain: Vec<(String, TrustClass)> = Vec::new();
    let mut chain_content_hashes: Vec<String> = Vec::new();

    loop {
        // Cycle detection
        if visited.contains(&current_id) {
            visited.push(current_id);
            return Err(EngineError::CycleDetected { cycle: visited });
        }

        // Depth limit
        if visited.len() >= MAX_CHAIN_DEPTH {
            return Err(EngineError::ExecutorNotFound {
                executor_id: format!(
                    "{current_id} (executor chain exceeded max depth {MAX_CHAIN_DEPTH}; chain: {visited:?})"
                ),
            });
        }

        visited.push(current_id.clone());

        // Terminal case: executor_id is registered in the registry
        if let Some(config) = executors.get(&current_id) {
            let script_path = last_resolved_path
                .unwrap_or_else(|| root_source_path.clone());

            return Ok(ChainTerminal {
                script_path,
                dispatch: config.clone(),
                terminal_executor_id: current_id,
                chain: visited,
                verified_chain,
                chain_content_hashes,
            });
        }

        // Non-terminal: parse as canonical ref and resolve the tool
        let ref_ = CanonicalRef::parse(&current_id).map_err(|e| {
            EngineError::ExecutorNotFound {
                executor_id: format!(
                    "{current_id} (not in registry and not a valid canonical ref: {e})"
                ),
            }
        })?;

        // Validate the kind exists
        let kind_schema = kinds.get(&ref_.kind).ok_or_else(|| {
            EngineError::UnsupportedKind {
                kind: ref_.kind.clone(),
            }
        })?;

        // Resolve the tool item
        let (source_path, _space, matched_ext) =
            crate::resolution::resolve_item(roots, kind_schema, &ref_)?;

        // Read content and extract metadata to get the next executor_id
        let content = std::fs::read_to_string(&source_path).map_err(|e| {
            EngineError::Internal(format!(
                "failed to read executor tool {}: {e}",
                source_path.display()
            ))
        })?;

        let source_format = kind_schema
            .resolved_format_for(&matched_ext)
            .ok_or_else(|| {
                EngineError::Internal(format!(
                    "matched extension {matched_ext} has no source format in schema"
                ))
            })?;

        // Verify trust/integrity of this intermediate chain hop
        let sig_header = crate::resolution::parse_signature_header(&content, &source_format.signature);
        let trust_class = match &sig_header {
            Some(header) => {
                // Check content hash integrity
                if let Some(actual_hash) = crate::trust::content_hash_after_signature(&content, &source_format.signature) {
                    if actual_hash != header.content_hash {
                        return Err(EngineError::ContentHashMismatch {
                            canonical_ref: current_id.clone(),
                            expected: header.content_hash.clone(),
                            actual: actual_hash,
                        });
                    }
                }
                // Determine trust class
                if trust_store.is_trusted(&header.signer_fingerprint) {
                    TrustClass::Trusted
                } else {
                    TrustClass::Untrusted
                }
            }
            None => TrustClass::Unsigned,
        };
        verified_chain.push((current_id.clone(), trust_class));

        // Track content hash for cache key
        let content_hash = crate::resolution::content_hash(&content);
        chain_content_hashes.push(content_hash);

        let parsed = parsers.extract(&content, &source_format.parser_id)?;
        let metadata = crate::metadata::apply_extraction_rules(
            &parsed,
            &kind_schema.extraction_rules,
            &source_path,
        );

        // Track this resolved tool's path
        last_resolved_path = Some(source_path);

        // Get the next executor_id from this tool's metadata or kind default
        let default_executor_id = kinds.default_executor_id(&ref_.kind);
        let next_id = executors
            .resolve_executor_id(&metadata, default_executor_id)
            .ok_or_else(|| EngineError::ExecutorNotFound {
                executor_id: format!(
                    "<none> (executor chain hit {} which has no executor_id and kind '{}' has no default; chain: {visited:?})",
                    current_id, ref_.kind
                ),
            })?;

        current_id = next_id;
    }
}

/// Build an execution plan from a verified item.
///
/// This is the core chain builder logic. It:
/// 1. Resolves the effective executor ID (metadata → kind default)
/// 2. Follows the executor chain to a terminal subprocess dispatch
/// 3. Emits the DispatchSubprocess plan node
/// 4. Computes a cache key
/// 5. Declares capabilities
pub fn build_plan(
    item: &VerifiedItem,
    parameters: &serde_json::Value,
    hints: &ExecutionHints,
    ctx: &PlanContext,
    executors: &ExecutorRegistry,
    kinds: &KindRegistry,
    parsers: &MetadataParserRegistry,
    roots: &ResolutionRoots,
    registry_fingerprint: &str,
    trust_store: &TrustStore,
) -> Result<ExecutionPlan, EngineError> {
    let resolved = &item.resolved;
    let canonical_ref = resolved.canonical_ref.to_string();

    // Step 1: Resolve effective executor ID for the root item
    let default_executor_id = kinds.default_executor_id(&resolved.kind);
    let executor_id = executors
        .resolve_executor_id(&resolved.metadata, default_executor_id)
        .ok_or_else(|| EngineError::ExecutorNotFound {
            executor_id: format!(
                "<none> (kind={}, no executor_id in metadata, no kind default)",
                resolved.kind
            ),
        })?;

    // Step 2: Follow the executor chain to a terminal
    let terminal = resolve_executor_chain(
        &executor_id,
        &resolved.source_path,
        executors,
        kinds,
        parsers,
        roots,
        trust_store,
    )?;

    // Step 3: Build plan node
    let entrypoint_id = PlanNodeId(format!("entry:{canonical_ref}"));

    let working_directory = resolved.materialized_project_root.clone();

    // Build environment with standard bindings
    // Root item info goes in env so the executor tool knows what it's executing
    let mut environment = HashMap::new();
    environment.insert(
        "RYE_ITEM_PATH".to_owned(),
        resolved.source_path.to_string_lossy().to_string(),
    );
    environment.insert("RYE_ITEM_KIND".to_owned(), resolved.kind.clone());
    environment.insert(
        "RYE_ITEM_REF".to_owned(),
        canonical_ref.clone(),
    );
    if let Some(ref root) = resolved.materialized_project_root {
        environment.insert(
            "RYE_PROJECT_ROOT".to_owned(),
            root.to_string_lossy().to_string(),
        );
    }

    // Site context
    environment.insert("RYE_SITE_ID".to_owned(), ctx.current_site_id.clone());
    environment.insert("RYE_ORIGIN_SITE_ID".to_owned(), ctx.origin_site_id.clone());

    // Executor chain: if the script is different from the root item (chain
    // resolved through an executor tool), include the executor path and
    // the full chain for debugging/auditing
    if terminal.script_path != resolved.source_path {
        environment.insert(
            "RYE_EXECUTOR_PATH".to_owned(),
            terminal.script_path.to_string_lossy().to_string(),
        );
    }
    if terminal.chain.len() > 1 {
        environment.insert(
            "RYE_EXECUTOR_CHAIN".to_owned(),
            terminal.chain.join(" → "),
        );
    }

    // Build arguments: pass parameters as JSON on argv[1] if non-null
    let mut arguments = Vec::new();
    if !parameters.is_null() {
        arguments.push(parameters.to_string());
    }

    // Runtime bindings: declared empty, daemon fills at execution time
    let runtime_bindings = HashMap::new();

    let entry_node = PlanNode::DispatchSubprocess {
        id: entrypoint_id.clone(),
        script_path: terminal.script_path,
        interpreter: terminal.dispatch.interpreter.clone(),
        working_directory,
        environment,
        arguments,
        runtime_bindings,
    };

    let capabilities = PlanCapabilities {
        requires_subprocess: true,
        ..Default::default()
    };

    // Complete node
    let complete_id = PlanNodeId(format!("complete:{canonical_ref}"));
    let complete_node = PlanNode::Complete {
        id: complete_id,
    };

    // Step 4: Compute cache key (include chain content hashes)
    let cache_key = compute_cache_key(
        &canonical_ref,
        &resolved.content_hash,
        &terminal.terminal_executor_id,
        registry_fingerprint,
        parameters,
        hints,
        &terminal.chain_content_hashes,
    );

    // Step 5: Build plan ID
    let plan_id = format!("plan:{cache_key}");

    Ok(ExecutionPlan {
        plan_id,
        root_executor_id: terminal.terminal_executor_id,
        root_ref: canonical_ref,
        item_kind: resolved.kind.clone(),
        nodes: vec![entry_node, complete_node],
        entrypoint: entrypoint_id,
        capabilities,
        materialization_requirements: Vec::new(),
        cache_key,
        thread_kind: Some(resolved.kind.clone()),
        executor_chain: terminal.chain,
    })
}

/// Compute a deterministic cache key for a plan.
fn compute_cache_key(
    canonical_ref: &str,
    content_hash: &str,
    executor_id: &str,
    registry_fingerprint: &str,
    parameters: &serde_json::Value,
    hints: &ExecutionHints,
    chain_content_hashes: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rye:plan:v1:");
    hasher.update(canonical_ref.as_bytes());
    hasher.update(b":");
    hasher.update(content_hash.as_bytes());
    hasher.update(b":");
    hasher.update(executor_id.as_bytes());
    hasher.update(b":");
    hasher.update(registry_fingerprint.as_bytes());
    hasher.update(b":");
    hasher.update(parameters.to_string().as_bytes());
    hasher.update(b":");
    hasher.update(serde_json::to_string(&hints).unwrap_or_default().as_bytes());
    for h in chain_content_hashes {
        hasher.update(b":");
        hasher.update(h.as_bytes());
    }

    let hash = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in hash.iter() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical_ref::CanonicalRef;
    use crate::contracts::*;
    use crate::executor_registry::SubprocessDispatch;
    use crate::trust::{TrustedSigner, TrustStore};
    use crate::AI_DIR;
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

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir = std::env::temp_dir().join(format!(
            "rye_plan_test_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
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
metadata:
  rules:
    name:
      from: filename
    executor_id:
      from: path
      key: __executor_id__
";

    fn write_tool_schema(kinds_dir: &Path) {
        let tool_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("tool.kind-schema.yaml"),
            sign_schema_yaml(TOOL_SCHEMA_YAML),
        )
        .unwrap();
    }

    fn write_directive_schema(kinds_dir: &Path, default_executor: Option<&str>) {
        let dir_dir = kinds_dir.join("directive");
        fs::create_dir_all(&dir_dir).unwrap();
        let executor_line = match default_executor {
            Some(exec) => format!("\nexecution:\n  default_executor_id: \"{exec}\"\n"),
            None => String::new(),
        };
        let yaml = format!(
            "\
location:
  directory: directives{executor_line}
formats:
  - extensions: [\".md\"]
    parser_id: markdown/xml
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
"
        );
        fs::write(
            dir_dir.join("directive.kind-schema.yaml"),
            sign_schema_yaml(&yaml),
        )
        .unwrap();
    }

    fn make_verified_item(
        ref_str: &str,
        kind: &str,
        source_path: PathBuf,
        executor_id: Option<&str>,
        project_root: Option<PathBuf>,
    ) -> VerifiedItem {
        let canonical_ref = CanonicalRef::parse(ref_str).unwrap();
        VerifiedItem {
            resolved: ResolvedItem {
                canonical_ref,
                kind: kind.to_owned(),
                source_path,
                source_space: ItemSpace::Project,
                resolved_from: "project".to_owned(),
                shadowed: vec![],
                materialized_project_root: project_root,
                content_hash: "abc123".to_owned(),
                signature_header: None,
                source_format: ResolvedSourceFormat {
                    extension: ".py".to_owned(),
                    parser_id: "python/ast".to_owned(),
                    signature: SignatureEnvelope {
                        prefix: "#".to_owned(),
                        suffix: None,
                        after_shebang: true,
                    },
                },
                metadata: ItemMetadata {
                    executor_id: executor_id.map(String::from),
                    ..Default::default()
                },
            },
            signer: None,
            trust_class: TrustClass::Unsigned,
            pinned_version: None,
        }
    }

    fn test_plan_context(project_root: Option<PathBuf>) -> PlanContext {
        PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: match project_root {
                Some(p) => ProjectContext::LocalPath { path: p },
                None => ProjectContext::None,
            },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        }
    }

    fn test_ts() -> TrustStore {
        test_trust_store()
    }

    // ── Direct terminal tests (depth 1, no chain) ───────────────────

    #[test]
    fn direct_terminal_executor() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();
        let mut executors = ExecutorRegistry::new();
        executors.register(
            "@primitive_chain",
            SubprocessDispatch {
                interpreter: Some("python3".into()),
            },
        );

        let tool_path = project_dir.join("my_tool.py");
        let item = make_verified_item(
            "tool:my_tool",
            "tool",
            tool_path.clone(),
            Some("@primitive_chain"),
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir));
        let roots = ResolutionRoots::from_flat(None, None, vec![]);

        let plan = build_plan(
            &item,
            &serde_json::json!({"key": "value"}),
            &ExecutionHints::default(),
            &ctx,
            &executors,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &TrustStore::empty(),
        )
        .unwrap();

        assert_eq!(plan.root_executor_id, "@primitive_chain");
        assert_eq!(plan.root_ref, "tool:my_tool");

        match &plan.nodes[0] {
            PlanNode::DispatchSubprocess {
                script_path,
                interpreter,
                environment,
                arguments,
                ..
            } => {
                assert_eq!(script_path, &tool_path);
                assert_eq!(interpreter.as_deref(), Some("python3"));
                assert!(environment.contains_key("RYE_ITEM_PATH"));
                assert!(environment.contains_key("RYE_ITEM_REF"));
                assert_eq!(environment.get("RYE_SITE_ID").unwrap(), "site:test");
                assert_eq!(environment.get("RYE_ORIGIN_SITE_ID").unwrap(), "site:test");
                assert!(!environment.contains_key("RYE_EXECUTOR_PATH"));
                assert!(!environment.contains_key("RYE_EXECUTOR_CHAIN"));
                assert_eq!(arguments.len(), 1);
            }
            other => panic!("expected DispatchSubprocess, got: {other:?}"),
        }
    }

    #[test]
    fn no_executor_fails() {
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();
        let executors = ExecutorRegistry::new();

        let item = make_verified_item(
            "tool:my_tool",
            "tool",
            PathBuf::from("/tmp/test.py"),
            None,
            None,
        );

        let ctx = test_plan_context(None);
        let roots = ResolutionRoots::from_flat(None, None, vec![]);

        let err = build_plan(
            &item,
            &serde_json::Value::Null,
            &ExecutionHints::default(),
            &ctx,
            &executors,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &TrustStore::empty(),
        )
        .unwrap_err();

        assert!(matches!(err, EngineError::ExecutorNotFound { .. }));
    }

    #[test]
    fn unregistered_non_ref_executor_fails() {
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();
        let executors = ExecutorRegistry::new();

        let item = make_verified_item(
            "tool:my_tool",
            "tool",
            PathBuf::from("/tmp/test.py"),
            Some("nonexistent_no_colon"),
            None,
        );

        let ctx = test_plan_context(None);
        let roots = ResolutionRoots::from_flat(None, None, vec![]);

        let err = build_plan(
            &item,
            &serde_json::Value::Null,
            &ExecutionHints::default(),
            &ctx,
            &executors,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &TrustStore::empty(),
        )
        .unwrap_err();

        assert!(matches!(err, EngineError::ExecutorNotFound { .. }));
    }

    // ── Chain resolution tests (depth 2+) ───────────────────────────

    #[test]
    fn chain_resolution_depth_2() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();

        write_tool_schema(&kinds_dir);
        write_directive_schema(&kinds_dir, Some("tool:exec/directive"));

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();

        let mut executors = ExecutorRegistry::new();
        executors.register(
            "@primitive_chain",
            SubprocessDispatch {
                interpreter: Some("python3".into()),
            },
        );

        let tool_dir = project_dir.join(AI_DIR).join("tools").join("exec");
        fs::create_dir_all(&tool_dir).unwrap();
        let executor_script = tool_dir.join("directive.py");
        fs::write(
            &executor_script,
            "__executor_id__ = \"@primitive_chain\"\n# executor for directives\n",
        )
        .unwrap();

        let directive_dir = project_dir.join(AI_DIR).join("directives");
        fs::create_dir_all(&directive_dir).unwrap();
        let directive_path = directive_dir.join("init.md");
        fs::write(&directive_path, "# Init directive\n").unwrap();

        let item = make_verified_item(
            "directive:init",
            "directive",
            directive_path.clone(),
            None,
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir.clone()));
        let roots = ResolutionRoots::from_flat(Some(project_dir.join(AI_DIR)), None, vec![]);

        let plan = build_plan(
            &item,
            &serde_json::json!({"input": "value"}),
            &ExecutionHints::default(),
            &ctx,
            &executors,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &TrustStore::empty(),
        )
        .unwrap();

        assert_eq!(plan.root_executor_id, "@primitive_chain");
        assert_eq!(plan.root_ref, "directive:init");
        assert_eq!(plan.item_kind, "directive");

        match &plan.nodes[0] {
            PlanNode::DispatchSubprocess {
                script_path,
                interpreter,
                environment,
                ..
            } => {
                assert_eq!(script_path, &executor_script);
                assert_eq!(interpreter.as_deref(), Some("python3"));
                assert_eq!(
                    environment.get("RYE_ITEM_PATH").unwrap(),
                    &directive_path.to_string_lossy().to_string()
                );
                assert_eq!(environment.get("RYE_ITEM_KIND").unwrap(), "directive");
                assert_eq!(environment.get("RYE_ITEM_REF").unwrap(), "directive:init");
                assert_eq!(
                    environment.get("RYE_EXECUTOR_PATH").unwrap(),
                    &executor_script.to_string_lossy().to_string()
                );
                let chain = environment.get("RYE_EXECUTOR_CHAIN").unwrap();
                assert!(chain.contains("tool:exec/directive"));
                assert!(chain.contains("@primitive_chain"));
                assert_eq!(environment.get("RYE_SITE_ID").unwrap(), "site:test");
            }
            other => panic!("expected DispatchSubprocess, got: {other:?}"),
        }
    }

    #[test]
    fn chain_resolution_depth_3() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();

        write_tool_schema(&kinds_dir);
        write_directive_schema(&kinds_dir, Some("tool:exec/directive"));

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();

        let mut executors = ExecutorRegistry::new();
        executors.register(
            "@primitive_chain",
            SubprocessDispatch {
                interpreter: Some("python3".into()),
            },
        );

        let tool_dir = project_dir.join(AI_DIR).join("tools").join("exec");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("directive.py"),
            "__executor_id__ = \"tool:runtime/base\"\n",
        )
        .unwrap();

        let runtime_dir = project_dir.join(AI_DIR).join("tools").join("runtime");
        fs::create_dir_all(&runtime_dir).unwrap();
        let runtime_script = runtime_dir.join("base.py");
        fs::write(
            &runtime_script,
            "__executor_id__ = \"@primitive_chain\"\n",
        )
        .unwrap();

        let directive_dir = project_dir.join(AI_DIR).join("directives");
        fs::create_dir_all(&directive_dir).unwrap();
        let directive_path = directive_dir.join("init.md");
        fs::write(&directive_path, "# Init\n").unwrap();

        let item = make_verified_item(
            "directive:init",
            "directive",
            directive_path,
            None,
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir.clone()));
        let roots = ResolutionRoots::from_flat(Some(project_dir.join(AI_DIR)), None, vec![]);

        let plan = build_plan(
            &item,
            &serde_json::Value::Null,
            &ExecutionHints::default(),
            &ctx,
            &executors,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &TrustStore::empty(),
        )
        .unwrap();

        assert_eq!(plan.root_executor_id, "@primitive_chain");

        match &plan.nodes[0] {
            PlanNode::DispatchSubprocess { script_path, .. } => {
                assert_eq!(script_path, &runtime_script);
            }
            other => panic!("expected DispatchSubprocess, got: {other:?}"),
        }
    }

    #[test]
    fn chain_cycle_detected() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();
        let executors = ExecutorRegistry::new();

        let tool_dir = project_dir.join(AI_DIR).join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("a.py"),
            "__executor_id__ = \"tool:b\"\n",
        )
        .unwrap();
        fs::write(
            tool_dir.join("b.py"),
            "__executor_id__ = \"tool:a\"\n",
        )
        .unwrap();

        let item = make_verified_item(
            "tool:a",
            "tool",
            tool_dir.join("a.py"),
            Some("tool:b"),
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir.clone()));
        let roots = ResolutionRoots::from_flat(Some(project_dir.join(AI_DIR)), None, vec![]);

        let err = build_plan(
            &item,
            &serde_json::Value::Null,
            &ExecutionHints::default(),
            &ctx,
            &executors,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &TrustStore::empty(),
        )
        .unwrap_err();

        assert!(
            matches!(err, EngineError::CycleDetected { .. }),
            "expected CycleDetected, got: {err:?}"
        );
    }

    #[test]
    fn chain_missing_intermediate_tool() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);
        write_directive_schema(&kinds_dir, Some("tool:nonexistent"));

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();
        let executors = ExecutorRegistry::new();

        let directive_dir = project_dir.join(AI_DIR).join("directives");
        fs::create_dir_all(&directive_dir).unwrap();
        fs::write(directive_dir.join("init.md"), "# Init\n").unwrap();

        let item = make_verified_item(
            "directive:init",
            "directive",
            directive_dir.join("init.md"),
            None,
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir.clone()));
        let roots = ResolutionRoots::from_flat(Some(project_dir.join(AI_DIR)), None, vec![]);

        let err = build_plan(
            &item,
            &serde_json::Value::Null,
            &ExecutionHints::default(),
            &ctx,
            &executors,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &TrustStore::empty(),
        )
        .unwrap_err();

        assert!(
            matches!(err, EngineError::ItemNotFound { .. }),
            "expected ItemNotFound, got: {err:?}"
        );
    }

    // ── Cache key tests ─────────────────────────────────────────────

    #[test]
    fn cache_key_deterministic() {
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();
        let mut executors = ExecutorRegistry::new();
        executors.register(
            "@primitive_chain",
            SubprocessDispatch {
                interpreter: Some("python3".into()),
            },
        );

        let item = make_verified_item(
            "tool:my_tool",
            "tool",
            PathBuf::from("/tmp/test.py"),
            Some("@primitive_chain"),
            None,
        );

        let ctx = test_plan_context(None);
        let roots = ResolutionRoots::from_flat(None, None, vec![]);
        let params = serde_json::json!({"key": "value"});
        let hints = ExecutionHints::default();

        let plan1 = build_plan(&item, &params, &hints, &ctx, &executors, &kinds, &parsers, &roots, "fp:test", &TrustStore::empty()).unwrap();
        let plan2 = build_plan(&item, &params, &hints, &ctx, &executors, &kinds, &parsers, &roots, "fp:test", &TrustStore::empty()).unwrap();

        assert_eq!(plan1.cache_key, plan2.cache_key);
    }

    #[test]
    fn cache_key_changes_with_params() {
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();
        let mut executors = ExecutorRegistry::new();
        executors.register(
            "@primitive_chain",
            SubprocessDispatch {
                interpreter: Some("python3".into()),
            },
        );

        let item = make_verified_item(
            "tool:my_tool",
            "tool",
            PathBuf::from("/tmp/test.py"),
            Some("@primitive_chain"),
            None,
        );

        let ctx = test_plan_context(None);
        let roots = ResolutionRoots::from_flat(None, None, vec![]);
        let hints = ExecutionHints::default();

        let plan1 = build_plan(&item, &serde_json::json!({"a": 1}), &hints, &ctx, &executors, &kinds, &parsers, &roots, "fp:test", &TrustStore::empty()).unwrap();
        let plan2 = build_plan(&item, &serde_json::json!({"a": 2}), &hints, &ctx, &executors, &kinds, &parsers, &roots, "fp:test", &TrustStore::empty()).unwrap();

        assert_ne!(plan1.cache_key, plan2.cache_key);
    }

    #[test]
    fn subprocess_no_params_no_args() {
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();
        let mut executors = ExecutorRegistry::new();
        executors.register(
            "@primitive_chain",
            SubprocessDispatch {
                interpreter: Some("python3".into()),
            },
        );

        let item = make_verified_item(
            "tool:my_tool",
            "tool",
            PathBuf::from("/tmp/test.py"),
            Some("@primitive_chain"),
            None,
        );

        let ctx = test_plan_context(None);
        let roots = ResolutionRoots::from_flat(None, None, vec![]);

        let plan = build_plan(
            &item,
            &serde_json::Value::Null,
            &ExecutionHints::default(),
            &ctx,
            &executors,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &TrustStore::empty(),
        )
        .unwrap();

        match &plan.nodes[0] {
            PlanNode::DispatchSubprocess { arguments, .. } => {
                assert!(arguments.is_empty());
            }
            other => panic!("expected DispatchSubprocess, got: {other:?}"),
        }
    }

    #[test]
    fn chain_tampered_intermediate_fails() {
        use base64::Engine as _;
        use lillux::crypto::Signer;
        use sha2::{Digest, Sha256};

        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();

        write_tool_schema(&kinds_dir);
        write_directive_schema(&kinds_dir, Some("tool:exec/directive"));

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();
        let mut executors = ExecutorRegistry::new();
        executors.register(
            "@primitive_chain",
            SubprocessDispatch {
                interpreter: Some("python3".into()),
            },
        );

        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let fp = crate::trust::compute_fingerprint(&signing_key.verifying_key());

        let body = "__executor_id__ = \"@primitive_chain\"\n";
        let body_hash = {
            let h = Sha256::digest(body.as_bytes());
            let mut out = String::with_capacity(64);
            for byte in h.iter() {
                use std::fmt::Write;
                let _ = write!(&mut out, "{byte:02x}");
            }
            out
        };
        let sig = signing_key.sign(body_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        let tampered_body = "__executor_id__ = \"@primitive_chain\"\n# INJECTED\n";
        let content = format!(
            "# rye:signed:2026-04-10T00:00:00Z:{body_hash}:{sig_b64}:{fp}\n{tampered_body}"
        );

        let tool_dir = project_dir.join(AI_DIR).join("tools").join("exec");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("directive.py"), &content).unwrap();

        let directive_dir = project_dir.join(AI_DIR).join("directives");
        fs::create_dir_all(&directive_dir).unwrap();
        let directive_path = directive_dir.join("init.md");
        fs::write(&directive_path, "# Init\n").unwrap();

        let item = make_verified_item(
            "directive:init",
            "directive",
            directive_path,
            None,
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir.clone()));
        let roots = ResolutionRoots::from_flat(Some(project_dir.join(AI_DIR)), None, vec![]);

        let err = build_plan(
            &item,
            &serde_json::Value::Null,
            &ExecutionHints::default(),
            &ctx,
            &executors,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &TrustStore::empty(),
        )
        .unwrap_err();

        assert!(
            matches!(err, EngineError::ContentHashMismatch { .. }),
            "expected ContentHashMismatch, got: {err:?}"
        );
    }
}
