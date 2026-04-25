//! Chain builder — turns a `VerifiedItem` into an `ExecutionPlan`.
//!
//! The builder follows the executor chain: starting from the root item's
//! `executor_id`, it walks through tool items on disk until hitting one
//! with `executor_id: null` (the terminal). Every chain element is a real
//! tool resolved from the filesystem. The terminal tool's `config` block
//! (if it has one) provides the runtime config describing how to spawn
//! the subprocess.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::canonical_ref::CanonicalRef;
use crate::contracts::{
    ExecutionHints, ExecutionPlan, PlanCapabilities, PlanContext, PlanNode, PlanNodeId,
    SubprocessSpec, TrustClass, VerifiedItem,
};
use crate::error::EngineError;
use crate::kind_registry::KindRegistry;
use crate::metadata::MetadataParserRegistry;
use crate::resolution::ResolutionRoots;
use crate::trust::TrustStore;

/// Maximum executor chain depth before we assume a cycle or misconfiguration.
const MAX_CHAIN_DEPTH: usize = 16;

/// Reserved env key prefix — runtime configs may not override daemon-injected bindings.
const RESERVED_ENV_PREFIX: &str = "RYE_";

// ── Chain data types ─────────────────────────────────────────────────────

/// Result of resolving the executor chain to a terminal.
/// Contains all resolved intermediates — the plan builder compiles these
/// into a SubprocessSpec.
struct ChainTerminal {
    root_source_path: PathBuf,
    chain: Vec<String>,
    verified_chain: Vec<(String, TrustClass)>,
    chain_content_hashes: Vec<String>,
    intermediates: Vec<ChainIntermediate>,
}

/// One resolved hop in the executor chain.
struct ChainIntermediate {
    executor_id: String,
    resolved_ref: String,
    kind: String,
    source_path: PathBuf,
    parsed: serde_json::Value,
}

// ── Runtime config deserialization ───────────────────────────────────────

/// Execution config extracted from a tool with a `config` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    input_data: Option<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default)]
    env: HashMap<String, String>,
}

fn default_timeout_secs() -> u64 {
    300
}

/// Interpreter resolution config from a tool with an `env_config` block.
#[derive(Debug, Clone, Deserialize)]
struct EnvConfig {
    #[serde(default)]
    interpreter: Option<InterpreterConfig>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct InterpreterConfig {
    binary: String,
    #[serde(default)]
    candidates: Vec<String>,
    #[serde(default)]
    search_paths: Vec<String>,
    var: Option<String>,
}

// ── Template expansion ──────────────────────────────────────────────────

struct TemplateContext {
    tool_path: PathBuf,
    project_path: Option<PathBuf>,
    params_json: String,
    interpreter: Option<String>,
}

fn expand_template(
    template: &str,
    ctx: &TemplateContext,
) -> Result<String, EngineError> {
    // Extract all {token} placeholders
    let mut result = template.to_string();
    let mut start = 0;
    while let Some(open) = result[start..].find('{') {
        let abs_open = start + open;
        let Some(close) = result[abs_open..].find('}') else {
            break;
        };
        let abs_close = abs_open + close;
        let token = &result[abs_open + 1..abs_close];

        let value = match token {
            "tool_path" => ctx.tool_path.to_string_lossy().to_string(),
            "project_path" => ctx.project_path.as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .ok_or_else(|| EngineError::TemplateMissingContext {
                    token: "project_path".into(),
                })?,
            "params_json" => ctx.params_json.clone(),
            "interpreter" => ctx.interpreter.clone()
                .ok_or_else(|| EngineError::TemplateMissingContext {
                    token: "interpreter".into(),
                })?,
            _ => return Err(EngineError::UnknownTemplateToken {
                token: token.to_string(),
            }),
        };
        result.replace_range(abs_open..abs_close + 1, &value);
        // Move past the replacement
        start = abs_open + value.len();
    }
    if result.is_empty() {
        return Err(EngineError::ExpandedEmpty {
            template: template.to_string(),
        });
    }
    Ok(result)
}

// ── Interpreter resolution ──────────────────────────────────────────────

fn resolve_interpreter(
    config: &InterpreterConfig,
    project_root: Option<&PathBuf>,
) -> Result<String, EngineError> {
    // 1. Check env var override
    if let Some(var) = &config.var {
        if let Ok(val) = std::env::var(var) {
            return Ok(val);
        }
    }
    // 2. Search project-local paths for binary/candidates
    if let Some(root) = project_root {
        let binaries = std::iter::once(&config.binary)
            .chain(config.candidates.iter());
        for search_path in &config.search_paths {
            for binary in binaries.clone() {
                let candidate = root.join(search_path).join(binary);
                if candidate.exists() {
                    return Ok(candidate.to_string_lossy().to_string());
                }
            }
        }
    }
    Err(EngineError::RuntimeBinaryNotFound {
        binary: config.binary.clone(),
    })
}

// ── Chain walker ────────────────────────────────────────────────────────

/// Resolve the executor chain from a starting executor_id to a terminal.
///
/// The chain walks `executor_id` on each resolved tool until hitting one
/// with `executor_id: null` (the terminal). Every element is a real tool
/// resolved from the filesystem. No registry lookup.
///
/// `@`-prefixed executor IDs are resolved as aliases via the kind schema
/// of the *previous* intermediate (or `root_kind` for the first hop).
fn resolve_executor_chain(
    starting_executor_id: &str,
    root_source_path: &PathBuf,
    root_kind: &str,
    kinds: &KindRegistry,
    parsers: &MetadataParserRegistry,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
) -> Result<ChainTerminal, EngineError> {
    let mut current_id = starting_executor_id.to_owned();
    let mut visited: Vec<String> = Vec::new();
    let mut verified_chain: Vec<(String, TrustClass)> = Vec::new();
    let mut chain_content_hashes: Vec<String> = Vec::new();
    let mut intermediates: Vec<ChainIntermediate> = Vec::new();

    loop {
        // Cycle detection
        if visited.contains(&current_id) {
            visited.push(current_id.clone());
            return Err(EngineError::CycleDetected { cycle: visited });
        }

        // Depth limit
        if visited.len() >= MAX_CHAIN_DEPTH {
            return Err(EngineError::ChainTooDeep {
                max_depth: MAX_CHAIN_DEPTH,
                chain: visited,
            });
        }

        visited.push(current_id.clone());

        // Resolve @ aliases via kind schema
        let resolved_id = if current_id.starts_with('@') {
            // Determine which kind schema to look up the alias in.
            // The previous intermediate tells us the kind context.
            // For the first hop, use the root item's kind.
            let kind_for_alias = intermediates.last()
                .map(|i| i.kind.as_str())
                .unwrap_or(root_kind);
            let kind_schema = kinds.get(kind_for_alias).ok_or_else(|| {
                EngineError::UnsupportedKind {
                    kind: kind_for_alias.to_string(),
                }
            })?;
            kind_schema.resolve_alias(&current_id).ok_or_else(|| {
                EngineError::UnknownAlias {
                    alias: current_id.clone(),
                    kind: kind_for_alias.to_string(),
                }
            })?.to_owned()
        } else {
            current_id.clone()
        };

        // Resolve as canonical ref → tool on disk
        let ref_ = CanonicalRef::parse(&resolved_id).map_err(|e| {
            EngineError::ExecutorNotFound {
                executor_id: format!(
                    "{current_id} → {resolved_id} (not a valid canonical ref: {e})"
                ),
            }
        })?;

        let kind_schema = kinds.get(&ref_.kind).ok_or_else(|| {
            EngineError::UnsupportedKind {
                kind: ref_.kind.clone(),
            }
        })?;

        let (source_path, _space, matched_ext) =
            crate::resolution::resolve_item(roots, kind_schema, &ref_)?;

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

        // Verify trust/integrity of this chain hop
        let sig_header = crate::resolution::parse_signature_header(&content, &source_format.signature);
        let trust_class = match &sig_header {
            Some(header) => {
                if let Some(actual_hash) = crate::trust::content_hash_after_signature(&content, &source_format.signature) {
                    if actual_hash != header.content_hash {
                        return Err(EngineError::ContentHashMismatch {
                            canonical_ref: resolved_id.clone(),
                            expected: header.content_hash.clone(),
                            actual: actual_hash,
                        });
                    }
                }
                if trust_store.is_trusted(&header.signer_fingerprint) {
                    TrustClass::Trusted
                } else {
                    TrustClass::Untrusted
                }
            }
            None => TrustClass::Unsigned,
        };
        verified_chain.push((current_id.clone(), trust_class));

        let content_hash = crate::resolution::content_hash(&content);
        chain_content_hashes.push(content_hash);

        let parsed = parsers.extract(&content, &source_format.parser_id)?;
        let metadata = crate::metadata::apply_extraction_rules(
            &parsed,
            &kind_schema.extraction_rules,
            &source_path,
        );

        // Accumulate this intermediate
        intermediates.push(ChainIntermediate {
            executor_id: current_id.clone(),
            resolved_ref: resolved_id.clone(),
            kind: ref_.kind.clone(),
            source_path: source_path.clone(),
            parsed: parsed.clone(),
        });

        // Terminal check: no executor_id → stop
        let next_id = metadata.executor_id.as_deref();
        match next_id {
            None => break, // terminal
            Some(id) => current_id = id.to_owned(),
        }
    }

    Ok(ChainTerminal {
        root_source_path: root_source_path.clone(),
        chain: visited,
        verified_chain,
        chain_content_hashes,
        intermediates,
    })
}

// ── Spec compilation ────────────────────────────────────────────────────

/// Compile the chain intermediates into a SubprocessSpec.
///
/// Scans intermediates for runtime config and env config, resolves
/// interpreter, expands templates.
fn compile_subprocess_spec(
    terminal: &ChainTerminal,
    project_root: Option<&PathBuf>,
    params: &serde_json::Value,
    plan_env: &HashMap<String, String>,
) -> Result<SubprocessSpec, EngineError> {
    let mut runtime_config: Option<RuntimeConfig> = None;
    let mut env_config: Option<EnvConfig> = None;

    for intermediate in &terminal.intermediates {
        tracing::debug!(
            executor_id = %intermediate.executor_id,
            resolved_ref = %intermediate.resolved_ref,
            path = %intermediate.source_path.display(),
            "scanning intermediate for runtime config"
        );
        if let Some(config) = intermediate.parsed.get("config") {
            if runtime_config.is_some() {
                return Err(EngineError::MultipleRuntimeConfigs {
                    chain: terminal.chain.clone(),
                });
            }
            runtime_config = Some(serde_json::from_value(config.clone())
                .map_err(|e| EngineError::InvalidRuntimeConfig {
                    path: intermediate.source_path.display().to_string(),
                    reason: format!("{e}"),
                })?);
        }
        if let Some(ec) = intermediate.parsed.get("env_config") {
            env_config = Some(serde_json::from_value(ec.clone())
                .map_err(|e| EngineError::InvalidRuntimeConfig {
                    path: intermediate.source_path.display().to_string(),
                    reason: format!("invalid env_config: {e}"),
                })?);
        }
    }

    let config = runtime_config.ok_or_else(|| EngineError::NoRuntimeConfig {
        chain: terminal.chain.clone(),
    })?;

    // Resolve interpreter from env_config
    let interpreter = env_config
        .as_ref()
        .and_then(|ec| ec.interpreter.as_ref())
        .map(|ic| resolve_interpreter(ic, project_root))
        .transpose()?;

    let params_json = if params.is_null() { "".to_string() } else { params.to_string() };

    let tmpl_ctx = TemplateContext {
        tool_path: terminal.root_source_path.clone(),
        project_path: project_root.cloned(),
        params_json,
        interpreter: interpreter.clone(),
    };

    // Expand command
    let cmd = expand_template(&config.command, &tmpl_ctx)?;

    // Expand args
    let args: Result<Vec<String>, EngineError> = config.args.iter()
        .map(|a| expand_template(a, &tmpl_ctx))
        .collect();
    let args = args?;

    // Expand input_data
    let stdin_data = config.input_data
        .as_deref()
        .map(|t| expand_template(t, &tmpl_ctx))
        .transpose()?;

    // Build env: plan env + env_config.env + config.env
    let mut env: HashMap<String, String> = plan_env.clone();

    // Merge env_config.env
    if let Some(ref ec) = env_config {
        for (k, v) in &ec.env {
            if k.starts_with(RESERVED_ENV_PREFIX) {
                return Err(EngineError::ReservedEnvKey { key: k.clone() });
            }
            let expanded = expand_template(v, &tmpl_ctx)?;
            env.insert(k.clone(), expanded);
        }
    }

    // Merge config.env
    for (k, v) in &config.env {
        if k.starts_with(RESERVED_ENV_PREFIX) {
            return Err(EngineError::ReservedEnvKey { key: k.clone() });
        }
        let expanded = expand_template(v, &tmpl_ctx)?;
        env.insert(k.clone(), expanded);
    }

    // Inject interpreter path as env var if resolved
    if let Some(ref interp) = interpreter {
        // The interpreter config may declare a var name — use it if present
        if let Some(ref ec) = env_config {
            if let Some(ref ic) = ec.interpreter {
                if let Some(ref var) = ic.var {
                    env.insert(var.clone(), interp.clone());
                }
            }
        }
    }

    Ok(SubprocessSpec {
        cmd,
        args,
        cwd: project_root.map(|p| p.clone()),
        env,
        stdin_data,
        timeout_secs: config.timeout_secs,
    })
}

// ── Plan builder ────────────────────────────────────────────────────────

/// Build an execution plan from a verified item.
///
/// This is the core chain builder logic. It:
/// 1. Resolves the effective executor ID (metadata → kind default)
/// 2. Follows the executor chain to a terminal (executor_id: null)
/// 3. Compiles intermediates into a SubprocessSpec
/// 4. Emits the DispatchSubprocess plan node
/// 5. Computes a cache key
pub fn build_plan(
    item: &VerifiedItem,
    parameters: &serde_json::Value,
    hints: &ExecutionHints,
    ctx: &PlanContext,
    kinds: &KindRegistry,
    parsers: &MetadataParserRegistry,
    roots: &ResolutionRoots,
    registry_fingerprint: &str,
    trust_store: &TrustStore,
) -> Result<ExecutionPlan, EngineError> {
    let resolved = &item.resolved;
    let canonical_ref = resolved.canonical_ref.to_string();

    // Step 1: Item MUST declare executor_id — no default, no fallback
    let executor_id = resolved
        .metadata
        .executor_id
        .as_deref()
        .ok_or_else(|| EngineError::MissingExecutorId {
            item_ref: canonical_ref.clone(),
        })?
        .to_owned();

    // Step 2: Follow the executor chain to a terminal
    let terminal = resolve_executor_chain(
        &executor_id,
        &resolved.source_path,
        &resolved.kind,
        kinds,
        parsers,
        roots,
        trust_store,
    )?;

    // Log chain trust status
    for (id, trust) in &terminal.verified_chain {
        tracing::debug!(executor_id = %id, trust = ?trust, "chain hop trust");
    }

    // Step 3: Build plan environment
    let mut plan_env = HashMap::new();
    plan_env.insert(
        "RYE_ITEM_PATH".to_owned(),
        resolved.source_path.to_string_lossy().to_string(),
    );
    plan_env.insert("RYE_ITEM_KIND".to_owned(), resolved.kind.clone());
    plan_env.insert(
        "RYE_ITEM_REF".to_owned(),
        canonical_ref.clone(),
    );
    if let Some(ref root) = resolved.materialized_project_root {
        plan_env.insert(
            "RYE_PROJECT_ROOT".to_owned(),
            root.to_string_lossy().to_string(),
        );
    }
    plan_env.insert("RYE_SITE_ID".to_owned(), ctx.current_site_id.clone());
    plan_env.insert("RYE_ORIGIN_SITE_ID".to_owned(), ctx.origin_site_id.clone());

    // Step 4: Compile intermediates into SubprocessSpec
    let project_root = match &ctx.project_context {
        crate::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
        _ => None,
    };
    let spec = compile_subprocess_spec(
        &terminal,
        project_root.as_ref(),
        parameters,
        &plan_env,
    )?;

    // Step 5: Build plan node
    let entrypoint_id = PlanNodeId(format!("entry:{canonical_ref}"));

    let entry_node = PlanNode::DispatchSubprocess {
        id: entrypoint_id.clone(),
        spec,
        tool_path: Some(resolved.source_path.clone()),
        executor_chain: terminal.chain.clone(),
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

    // Step 6: Compute cache key
    let cache_key = compute_cache_key(
        &canonical_ref,
        &resolved.content_hash,
        &terminal.chain_content_hashes,
        registry_fingerprint,
        parameters,
        hints,
    );

    // Step 7: Build plan ID
    let plan_id = format!("plan:{cache_key}");

    Ok(ExecutionPlan {
        plan_id,
        root_executor_id: terminal.chain.last().cloned().unwrap_or_default(),
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
    chain_content_hashes: &[String],
    registry_fingerprint: &str,
    parameters: &serde_json::Value,
    hints: &ExecutionHints,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rye:plan:v2:");
    hasher.update(canonical_ref.as_bytes());
    hasher.update(b":");
    hasher.update(content_hash.as_bytes());
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
    let result = hasher.finalize();
    format!("{result:x}")
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::*;
    use crate::kind_registry::KindRegistry;
    use crate::trust::{TrustedSigner, TrustStore};
    use lillux::crypto::SigningKey;
    use serde_json::json;
    use std::fs;
    use std::path::Path;

    const AI_DIR: &str = crate::AI_DIR;

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

    fn test_ts() -> TrustStore {
        let sk = test_signing_key();
        let vk = sk.verifying_key();
        let fp = crate::trust::compute_fingerprint(&vk);
        TrustStore::from_signers(vec![crate::trust::TrustedSigner {
            fingerprint: fp,
            verifying_key: vk,
            label: None,
        }])
    }

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    fn sign_yaml(yaml: &str) -> String {
        lillux::signature::sign_content(yaml, &test_signing_key(), "#", None)
    }

    const TOOL_SCHEMA_YAML: &str = "\
location:
  directory: tools
execution:
  aliases:
    \"@subprocess\": \"tool:rye/core/subprocess/execute\"
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
  - extensions: [\".yaml\", \".yml\"]
    parser_id: yaml/yaml
    signature:
      prefix: \"#\"
metadata:
  rules:
    executor_id:
      from: path
      key: __executor_id__
";

    fn write_tool_schema(kinds_dir: &Path) {
        let dir = kinds_dir.join("tool");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tool.kind-schema.yaml"), sign_yaml(TOOL_SCHEMA_YAML)).unwrap();
    }

    fn make_verified_item(
        canonical_ref: &str,
        kind: &str,
        source_path: PathBuf,
        executor_id: Option<&str>,
        project_dir: Option<PathBuf>,
    ) -> VerifiedItem {
        use crate::contracts::{ItemMetadata, ResolvedItem, ResolvedSourceFormat, SignatureEnvelope};

        let metadata = ItemMetadata {
            executor_id: executor_id.map(String::from),
            ..Default::default()
        };

        let resolved = ResolvedItem {
            canonical_ref: CanonicalRef::parse(canonical_ref).unwrap(),
            kind: kind.to_string(),
            source_path,
            source_space: ItemSpace::Project,
            resolved_from: "test".to_string(),
            shadowed: vec![],
            materialized_project_root: project_dir,
            content_hash: "test_hash".to_string(),
            signature_header: None,
            source_format: ResolvedSourceFormat {
                extension: ".py".to_string(),
                parser_id: "python/ast".to_string(),
                signature: SignatureEnvelope {
                    prefix: "#".to_string(),
                    suffix: None,
                    after_shebang: false,
                },
            },
            metadata,
        };

        VerifiedItem {
            resolved,
            trust_class: TrustClass::Trusted,
            signer: None,
            pinned_version: None,
        }
    }

    fn test_plan_context(project_dir: Option<PathBuf>) -> PlanContext {
        PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: match project_dir {
                Some(p) => ProjectContext::LocalPath { path: p },
                None => ProjectContext::None,
            },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        }
    }

    // ── Helper: write a tool with __executor_id__ on disk ──────────────

    fn write_chain_tool(dir: &Path, name: &str, executor_id: Option<&str>) -> PathBuf {
        let tool_dir = dir.join(AI_DIR).join("tools");
        let parts: Vec<&str> = name.split('/').collect();
        let mut d = tool_dir.clone();
        for part in &parts[..parts.len().saturating_sub(1)] {
            d = d.join(part);
        }
        fs::create_dir_all(&d).unwrap();
        let file_path = d.join(format!("{}.py", parts.last().unwrap()));
        let content = match executor_id {
            Some(id) => format!("__executor_id__ = \"{id}\"\n"),
            None => "# terminal — no executor_id\n".to_string(),
        };
        fs::write(&file_path, &content).unwrap();
        file_path
    }

    // ── Helper: write a terminal tool with config (executor_id: null) ────

    fn write_terminal_with_config(dir: &Path, name: &str) -> PathBuf {
        let tool_dir = dir.join(AI_DIR).join("tools");
        let parts: Vec<&str> = name.split('/').collect();
        let mut d = tool_dir.clone();
        for part in &parts[..parts.len().saturating_sub(1)] {
            d = d.join(part);
        }
        fs::create_dir_all(&d).unwrap();
        let file_path = d.join(format!("{}.yaml", parts.last().unwrap()));
        let content = "\
tool_type: subprocess
executor_id: null
config:
  command: /bin/sh
  args: [\"-c\", \"{tool_path}\"]
  timeout_secs: 300
";
        fs::write(&file_path, content).unwrap();
        file_path
    }

    // ── Helper: write a terminal tool (executor_id: null, no config) ────

    fn write_terminal(dir: &Path, name: &str) -> PathBuf {
        let tool_dir = dir.join(AI_DIR).join("tools");
        let parts: Vec<&str> = name.split('/').collect();
        let mut d = tool_dir.clone();
        for part in &parts[..parts.len().saturating_sub(1)] {
            d = d.join(part);
        }
        fs::create_dir_all(&d).unwrap();
        let file_path = d.join(format!("{}.yaml", parts.last().unwrap()));
        let content = "tool_type: subprocess\nexecutor_id: null\n";
        fs::write(&file_path, content).unwrap();
        file_path
    }

    // ── Test: chain walks to terminal with executor_id null ─────────────

    #[test]
    fn chain_walks_to_null_terminal() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();

        // Write chain: tool → @subprocess (alias → tool:rye/core/subprocess/execute, null terminal with config)
        let _term = write_terminal_with_config(&project_dir, "rye/core/subprocess/execute");
        let tool_path = write_chain_tool(&project_dir, "my_tool", Some("@subprocess"));

        let item = make_verified_item(
            "tool:my_tool",
            "tool",
            tool_path,
            Some("@subprocess"),
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir.clone()));
        let roots = ResolutionRoots::from_flat(
            Some(project_dir.join(AI_DIR)),
            None,
            vec![],
        );

        let plan = build_plan(
            &item,
            &json!({"key": "value"}),
            &ExecutionHints::default(),
            &ctx,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &ts,
        )
        .unwrap();

        assert_eq!(plan.root_ref, "tool:my_tool");
        // Chain should include @subprocess and the resolved terminal
        assert!(plan.executor_chain.iter().any(|id| id.contains("subprocess")));
    }

    // ── Test: chain cycle detected ─────────────────────────────────────

    #[test]
    fn chain_cycle_detected() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();

        write_chain_tool(&project_dir, "a", Some("tool:b"));
        write_chain_tool(&project_dir, "b", Some("tool:a"));

        let item = make_verified_item(
            "tool:a",
            "tool",
            project_dir.join(AI_DIR).join("tools").join("a.py"),
            Some("tool:b"),
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir.clone()));
        let roots = ResolutionRoots::from_flat(
            Some(project_dir.join(AI_DIR)),
            None,
            vec![],
        );

        let err = build_plan(
            &item,
            &json!(null),
            &ExecutionHints::default(),
            &ctx,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &ts,
        )
        .unwrap_err();

        assert!(matches!(err, EngineError::CycleDetected { .. }));
    }

    // ── Test: @subprocess alias resolves ────────────────────────────────

    #[test]
    fn subprocess_alias_resolves() {
        let _project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();

        // Verify the alias is loaded
        let tool_schema = kinds.get("tool").unwrap();
        assert_eq!(
            tool_schema.resolve_alias("@subprocess"),
            Some("tool:rye/core/subprocess/execute")
        );
        assert!(tool_schema.is_executable());
    }

    // ── Test: unknown alias errors ──────────────────────────────────────

    #[test]
    fn unknown_alias_errors() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();

        // Tool points to @nonexistent alias
        let tool_path = write_chain_tool(&project_dir, "my_tool", Some("@nonexistent"));

        let item = make_verified_item(
            "tool:my_tool",
            "tool",
            tool_path,
            Some("@nonexistent"),
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir.clone()));
        let roots = ResolutionRoots::from_flat(
            Some(project_dir.join(AI_DIR)),
            None,
            vec![],
        );

        let err = build_plan(
            &item,
            &json!(null),
            &ExecutionHints::default(),
            &ctx,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &ts,
        )
        .unwrap_err();

        assert!(
            matches!(err, EngineError::UnknownAlias { .. }),
            "expected UnknownAlias, got: {err:?}"
        );
    }

    // ── Test: no executor_id fails ──────────────────────────────────────

    #[test]
    fn no_executor_id_fails() {
        let _project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();

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
            &json!(null),
            &ExecutionHints::default(),
            &ctx,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &TrustStore::empty(),
        )
        .unwrap_err();

        assert!(matches!(err, EngineError::MissingExecutorId { .. }));
    }

    // ── Test: template expansion ────────────────────────────────────────

    #[test]
    fn template_expansion_basic() {
        let ctx = TemplateContext {
            tool_path: PathBuf::from("/path/to/echo.py"),
            project_path: Some(PathBuf::from("/project")),
            params_json: r#"{"message":"hi"}"#.to_string(),
            interpreter: Some("python3".to_string()),
        };
        assert_eq!(expand_template("{tool_path}", &ctx).unwrap(), "/path/to/echo.py");
        assert_eq!(expand_template("{interpreter}", &ctx).unwrap(), "python3");
        assert_eq!(expand_template("{params_json}", &ctx).unwrap(), r#"{"message":"hi"}"#);
        assert_eq!(expand_template("{project_path}", &ctx).unwrap(), "/project");
    }

    #[test]
    fn template_unknown_token_errors() {
        let ctx = TemplateContext {
            tool_path: PathBuf::from("/t.py"),
            project_path: None,
            params_json: String::new(),
            interpreter: None,
        };
        let err = expand_template("{unknown_thing}", &ctx).unwrap_err();
        assert!(matches!(err, EngineError::UnknownTemplateToken { .. }));
    }

    #[test]
    fn template_missing_context_errors() {
        let ctx = TemplateContext {
            tool_path: PathBuf::from("/t.py"),
            project_path: None,
            params_json: String::new(),
            interpreter: None,
        };
        let err = expand_template("{project_path}", &ctx).unwrap_err();
        assert!(matches!(err, EngineError::TemplateMissingContext { .. }));
    }

    // ── Test: interpreter resolution ────────────────────────────────────

    #[test]
    fn interpreter_from_env_var() {
        std::env::set_var("RYE_TEST_PYTHON", "/custom/python3");
        let config = InterpreterConfig {
            binary: "python3".into(),
            candidates: vec![],
            search_paths: vec![],
            var: Some("RYE_TEST_PYTHON".into()),
        };
        let result = resolve_interpreter(&config, None).unwrap();
        assert_eq!(result, "/custom/python3");
        std::env::remove_var("RYE_TEST_PYTHON");
    }

    #[test]
    fn interpreter_from_search_path() {
        let project_dir = tempdir();
        let venv_bin = project_dir.join(".venv").join("bin");
        fs::create_dir_all(&venv_bin).unwrap();
        fs::write(venv_bin.join("python3"), "#!/bin/bash\necho python").unwrap();

        let config = InterpreterConfig {
            binary: "python3".into(),
            candidates: vec![],
            search_paths: vec![".venv/bin".into()],
            var: Some("RYE_PYTHON".into()),
        };
        let result = resolve_interpreter(&config, Some(&project_dir)).unwrap();
        assert!(result.contains(".venv/bin/python3"));
    }

    #[test]
    fn interpreter_not_found_errors() {
        let config = InterpreterConfig {
            binary: "nonexistent_interp".into(),
            candidates: vec![],
            search_paths: vec![],
            var: None,
        };
        let err = resolve_interpreter(&config, None).unwrap_err();
        assert!(matches!(err, EngineError::RuntimeBinaryNotFound { .. }));
    }

    // ── Test: no runtime config errors ──────────────────────────────────

    #[test]
    fn no_runtime_config_errors() {
        let intermediates = vec![ChainIntermediate {
            executor_id: "test".into(),
            resolved_ref: "tool:test".into(),
            kind: "tool".into(),
            source_path: PathBuf::from("/test.py"),
            parsed: json!({}), // no "config" key
        }];
        let terminal = ChainTerminal {
            root_source_path: PathBuf::from("/test.py"),
            chain: vec!["test".into()],
            verified_chain: vec![],
            chain_content_hashes: vec![],
            intermediates,
        };
        let err = compile_subprocess_spec(&terminal, None, &json!(null), &HashMap::new()).unwrap_err();
        assert!(matches!(err, EngineError::NoRuntimeConfig { .. }));
    }

    // ── Test: multiple runtime configs error ────────────────────────────

    #[test]
    fn multiple_runtime_configs_error() {
        let config_block = json!({
            "command": "python3",
            "args": ["{tool_path}"],
            "timeout_secs": 300
        });
        let intermediates = vec![
            ChainIntermediate {
                executor_id: "a".into(),
                resolved_ref: "tool:a".into(),
                kind: "tool".into(),
                source_path: PathBuf::from("/a.yaml"),
                parsed: json!({ "config": config_block }),
            },
            ChainIntermediate {
                executor_id: "b".into(),
                resolved_ref: "tool:b".into(),
                kind: "tool".into(),
                source_path: PathBuf::from("/b.yaml"),
                parsed: json!({ "config": config_block }),
            },
        ];
        let terminal = ChainTerminal {
            root_source_path: PathBuf::from("/test.py"),
            chain: vec!["a".into(), "b".into()],
            verified_chain: vec![],
            chain_content_hashes: vec![],
            intermediates,
        };
        let err = compile_subprocess_spec(&terminal, None, &json!(null), &HashMap::new()).unwrap_err();
        assert!(matches!(err, EngineError::MultipleRuntimeConfigs { .. }));
    }

    // ── Test: reserved env key rejected ─────────────────────────────────

    #[test]
    fn reserved_env_key_rejected() {
        let config_block = json!({
            "command": "/bin/echo",
            "args": [],
            "timeout_secs": 10,
            "env": { "RYE_THREAD_ID": "evil" }
        });
        let intermediates = vec![ChainIntermediate {
            executor_id: "test".into(),
            resolved_ref: "tool:test".into(),
            kind: "tool".into(),
            source_path: PathBuf::from("/test.yaml"),
            parsed: json!({ "config": config_block }),
        }];
        let terminal = ChainTerminal {
            root_source_path: PathBuf::from("/test.py"),
            chain: vec!["test".into()],
            verified_chain: vec![],
            chain_content_hashes: vec![],
            intermediates,
        };
        let err = compile_subprocess_spec(&terminal, None, &json!(null), &HashMap::new()).unwrap_err();
        assert!(matches!(err, EngineError::ReservedEnvKey { .. }));
    }

    // ── Test: runtime config compiles to spec ───────────────────────────

    #[test]
    fn runtime_config_compiles_to_spec() {
        let config_block = json!({
            "command": "python3",
            "args": ["{tool_path}", "--project-path", "{project_path}"],
            "input_data": "{params_json}",
            "timeout_secs": 60,
            "env": { "PYTHONUNBUFFERED": "1" }
        });
        let intermediates = vec![ChainIntermediate {
            executor_id: "runtime".into(),
            resolved_ref: "tool:runtime".into(),
            kind: "tool".into(),
            source_path: PathBuf::from("/runtime.yaml"),
            parsed: json!({ "config": config_block }),
        }];
        let terminal = ChainTerminal {
            root_source_path: PathBuf::from("/project/.ai/tools/echo.py"),
            chain: vec!["runtime".into()],
            verified_chain: vec![],
            chain_content_hashes: vec![],
            intermediates,
        };
        let spec = compile_subprocess_spec(
            &terminal,
            Some(&PathBuf::from("/project")),
            &json!({"message": "hello"}),
            &HashMap::new(),
        ).unwrap();

        assert_eq!(spec.cmd, "python3");
        assert_eq!(spec.args, vec!["/project/.ai/tools/echo.py", "--project-path", "/project"]);
        assert_eq!(spec.stdin_data, Some(r#"{"message":"hello"}"#.to_string()));
        assert_eq!(spec.timeout_secs, 60);
        assert_eq!(spec.env.get("PYTHONUNBUFFERED").unwrap(), "1");
    }

    // ── Test: content hash mismatch detected on chain hop ────────────────

    #[test]
    fn content_hash_mismatch_detected() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();

        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fp = crate::trust::compute_fingerprint(&verifying_key);
        let ts = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key,
            label: None,
        }]);

        write_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();

        // Write a chain hop tool with a valid signature, then tamper it.
        // The root tool points to tool:runtimes/python/script as executor.
        let runtime_dir = project_dir.join(AI_DIR).join("tools").join("runtimes").join("python");
        fs::create_dir_all(&runtime_dir).unwrap();

        // Sign the runtime YAML
        let body = "tool_type: runtime\nexecutor_id: \"@subprocess\"\n";
        let hash: String = {
            let h = sha2::Sha256::digest(body.as_bytes());
            let mut out = String::with_capacity(64);
            for byte in h.iter() {
                use std::fmt::Write;
                let _ = write!(&mut out, "{byte:02x}");
            }
            out
        };
        use base64::Engine;
        use lillux::crypto::Signer;
        let sig: lillux::crypto::Signature = signing_key.sign(hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        let signed_content = format!(
            "# rye:signed:2026-04-10T00:00:00Z:{hash}:{sig_b64}:{fp}\n{body}"
        );
        // Tamper after signing
        let tampered = signed_content.replace("@subprocess", "@tampered");
        fs::write(runtime_dir.join("script.yaml"), &tampered).unwrap();

        // Also need the terminal for the alias resolution (even though we won't reach it)
        let _term = write_terminal(&project_dir, "rye/core/subprocess/execute");

        // Root tool
        let tool_path = write_chain_tool(&project_dir, "my_tool", Some("tool:runtimes/python/script"));

        let item = make_verified_item(
            "tool:my_tool",
            "tool",
            tool_path,
            Some("tool:runtimes/python/script"),
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir.clone()));
        let roots = ResolutionRoots::from_flat(
            Some(project_dir.join(AI_DIR)),
            None,
            vec![],
        );

        let err = build_plan(
            &item,
            &json!(null),
            &ExecutionHints::default(),
            &ctx,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &ts,
        )
        .unwrap_err();

        assert!(
            matches!(err, EngineError::ContentHashMismatch { .. }),
            "expected ContentHashMismatch, got: {err:?}"
        );
    }

    // ── Test: cache key is deterministic ────────────────────────────────

    #[test]
    fn cache_key_deterministic() {
        let k1 = compute_cache_key("tool:a", "hash1", &[], "fp1", &json!(1), &ExecutionHints::default());
        let k2 = compute_cache_key("tool:a", "hash1", &[], "fp1", &json!(1), &ExecutionHints::default());
        assert_eq!(k1, k2);

        let k3 = compute_cache_key("tool:b", "hash1", &[], "fp1", &json!(1), &ExecutionHints::default());
        assert_ne!(k1, k3);
    }

    // ── E2E: full 3-hop chain with interpreter resolution ───────────────
    //
    // Chain: root tool → runtime (with @subprocess alias + interpreter) → terminal
    //
    //   my_tool.py         __executor_id__ = "tool:runtimes/python/script"
    //     → script.yaml    __executor_id__ = "@subprocess"   (has config + env_config)
    //       → execute.yaml __executor_id__ = null             (terminal)
    //
    // Verifies that the final SubprocessSpec has the correct expanded
    // {interpreter} template, tool_path, project_path, and timeout_secs.

    #[test]
    fn e2e_full_chain_with_interpreter() {
        let project_dir = tempdir();
        let kinds_dir = tempdir();
        let ts = test_ts();
        write_tool_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).unwrap();
        let parsers = MetadataParserRegistry::with_builtins();

        // 1. Create a fake python binary in project/.venv/bin/
        let venv_bin = project_dir.join(".venv").join("bin");
        fs::create_dir_all(&venv_bin).unwrap();
        let fake_python = venv_bin.join("python3");
        fs::write(&fake_python, "#!/bin/sh\necho fake-python").unwrap();

        // 2. Write runtime YAML: runtimes/python/script.yaml
        //    - __executor_id__: "@subprocess"
        //    - env_config with interpreter pointing to .venv/bin
        //    - config with {interpreter} template
        let runtime_dir = project_dir
            .join(AI_DIR)
            .join("tools")
            .join("runtimes")
            .join("python");
        fs::create_dir_all(&runtime_dir).unwrap();

        let runtime_content = format!(r#"__executor_id__: "@subprocess"
tool_type: runtime
category: rye/core/runtimes/python
env_config:
  interpreter:
    binary: python3
    candidates: [python3]
    search_paths: [".venv/bin"]
    var: RYE_PYTHON
  env:
    PYTHONUNBUFFERED: "1"
config:
  command: "{{interpreter}}"
  args:
    - "{{tool_path}}"
    - "--project-path"
    - "{{project_path}}"
  input_data: "{{params_json}}"
  timeout_secs: 120
"#);
        let runtime_path = runtime_dir.join("script.yaml");
        fs::write(&runtime_path, &runtime_content).unwrap();

        // 3. Write terminal: rye/core/subprocess/execute.yaml (executor_id: null)
        let terminal_dir = project_dir
            .join(AI_DIR)
            .join("tools")
            .join("rye")
            .join("core")
            .join("subprocess");
        fs::create_dir_all(&terminal_dir).unwrap();
        let terminal_content = "\
__executor_id__: null\n\
tool_type: subprocess\n\
category: rye/core/subprocess\n";
        fs::write(terminal_dir.join("execute.yaml"), &terminal_content).unwrap();

        // 4. Write root tool: my_tool.py
        let tool_path = write_chain_tool(
            &project_dir,
            "my_tool",
            Some("tool:runtimes/python/script"),
        );

        let item = make_verified_item(
            "tool:my_tool",
            "tool",
            tool_path.clone(),
            Some("tool:runtimes/python/script"),
            Some(project_dir.clone()),
        );

        let ctx = test_plan_context(Some(project_dir.clone()));
        let roots = ResolutionRoots::from_flat(
            Some(project_dir.join(AI_DIR)),
            None,
            vec![],
        );

        // 5. Build plan — this walks the full 3-hop chain
        let plan = build_plan(
            &item,
            &json!({"message": "hello"}),
            &ExecutionHints::default(),
            &ctx,
            &kinds,
            &parsers,
            &roots,
            "fp:test",
            &ts,
        )
        .expect("build_plan should succeed for valid 3-hop chain");

        // 6. Verify the plan structure
        assert_eq!(plan.root_ref, "tool:my_tool");
        assert_eq!(plan.item_kind, "tool");

        // Chain should include the runtime hop and the resolved terminal
        assert!(
            plan.executor_chain.iter().any(|id| id.contains("python/script")),
            "executor_chain should include the runtime: {:?}",
            plan.executor_chain,
        );
        assert!(
            plan.executor_chain.iter().any(|id| id.contains("subprocess")),
            "executor_chain should include the terminal alias resolution: {:?}",
            plan.executor_chain,
        );

        // 7. Verify the DispatchSubprocess node
        let dispatch = plan.nodes.iter().find_map(|n| match n {
            PlanNode::DispatchSubprocess { spec, .. } => Some(spec.clone()),
            _ => None,
        }).expect("plan should have a DispatchSubprocess node");

        // Command should be the resolved interpreter path (fake python in .venv)
        assert!(
            dispatch.cmd.contains("python3"),
            "cmd should contain python3, got: {:?}",
            dispatch.cmd,
        );
        assert!(
            dispatch.cmd.contains(".venv"),
            "cmd should resolve to .venv/bin/python3, got: {:?}",
            dispatch.cmd,
        );

        // Args should have tool_path and --project-path expanded
        assert_eq!(dispatch.args.len(), 3);
        // args[0] = {tool_path} → root tool source path
        assert!(
            dispatch.args[0].contains("my_tool"),
            "args[0] should contain my_tool, got: {:?}",
            dispatch.args[0],
        );
        assert_eq!(dispatch.args[1], "--project-path");
        // args[2] = {project_path} → project root
        assert!(
            dispatch.args[2].contains("rye_plan_test"),
            "args[2] should be project path, got: {:?}",
            dispatch.args[2],
        );

        // stdin_data should have the params JSON
        assert_eq!(dispatch.stdin_data.as_deref(), Some(r#"{"message":"hello"}"#));

        // timeout_secs from the runtime config
        assert_eq!(dispatch.timeout_secs, 120);

        // Env should include PYTHONUNBUFFERED and the RYE_PYTHON var injection
        assert_eq!(dispatch.env.get("PYTHONUNBUFFERED").unwrap(), "1");
        assert!(dispatch.env.contains_key("RYE_PYTHON"));
    }
}
