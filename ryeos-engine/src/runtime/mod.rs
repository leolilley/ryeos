//! Runtime handler dispatch — generic, schema-driven compilation of an
//! executor chain into a `SubprocessSpec`.
//!
//! Mirrors the composer pattern (`crate::composers`):
//!   * Each top-level YAML block on a chain intermediate (e.g. `config`,
//!     `env_config`, `verify_deps`, `runtime_config`) is claimed by
//!     exactly one `RuntimeHandler` registered under a string key.
//!   * `compile_with_handlers` walks the chain in order; for each block
//!     it dispatches to the registered handler, which owns
//!     deserialization of ITS OWN typed config and writes into a shared
//!     mutable `CompileContext`.
//!   * Keys not in `ignored_keys` and not claimed by a handler are a
//!     hard error (`EngineError::UnknownRuntimeBlock`). No silent
//!     ignores.

pub mod config_schema;
pub mod handlers;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::contracts::{ExecutionDecorations, SubprocessSpec};
use crate::error::EngineError;
use crate::item_resolution::ResolutionRoots;
use crate::kind_registry::KindRegistry;
use crate::parsers::ParserDispatcher;
use crate::trust::TrustStore;

/// Reserved env key prefix — runtime configs may not override
/// daemon-injected bindings.
pub const RESERVED_ENV_PREFIX: &str = "RYE_";

// ── Chain hop (input to compilation) ─────────────────────────────────────

/// One resolved hop in the executor chain. Identical shape to the
/// internal type used by `plan_builder`; re-exported here so handlers
/// can be passed a borrow without a circular dep.
#[derive(Debug, Clone)]
pub struct ChainIntermediate {
    pub executor_id: String,
    pub resolved_ref: String,
    pub kind: String,
    pub source_path: PathBuf,
    pub parsed: Value,
}

// ── Template expansion ───────────────────────────────────────────────────

/// Tokens that handlers can populate; consumed by `expand_template`.
/// Only `tool_path` is mandatory; everything else is optional and
/// fail-loud when referenced from a template without a value.
#[derive(Debug, Clone)]
pub struct TemplateContext {
    pub tool_path: PathBuf,
    pub project_path: Option<PathBuf>,
    pub params_json: String,
    pub interpreter: Option<String>,
    /// Forward-compat: handlers may add their own tokens here without
    /// the engine knowing about them. Lookup is checked AFTER the
    /// known tokens, so handlers can shadow nothing.
    pub extra: HashMap<String, String>,
}

impl TemplateContext {
    pub fn new(tool_path: PathBuf) -> Self {
        Self {
            tool_path,
            project_path: None,
            params_json: String::new(),
            interpreter: None,
            extra: HashMap::new(),
        }
    }
}

pub fn expand_template(template: &str, ctx: &TemplateContext) -> Result<String, EngineError> {
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
            "project_path" => ctx
                .project_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .ok_or_else(|| EngineError::TemplateMissingContext {
                    token: "project_path".into(),
                })?,
            "params_json" => ctx.params_json.clone(),
            "interpreter" => ctx
                .interpreter
                .clone()
                .ok_or_else(|| EngineError::TemplateMissingContext {
                    token: "interpreter".into(),
                })?,
            other => match ctx.extra.get(other) {
                Some(v) => v.clone(),
                None => {
                    return Err(EngineError::UnknownTemplateToken {
                        token: other.to_string(),
                    });
                }
            },
        };
        result.replace_range(abs_open..abs_close + 1, &value);
        start = abs_open + value.len();
    }
    Ok(result)
}

// ── Handler-side mutable state ───────────────────────────────────────────

/// Subprocess spec fields a handler can write into. The final
/// `SubprocessSpec` is built by `compile_with_handlers` after all
/// handlers have run; templates are expanded at that point.
#[derive(Debug, Default, Clone)]
pub struct SpecOverrides {
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub stdin_data: Option<String>,
    pub timeout_secs: Option<u64>,
    pub cwd: Option<PathBuf>,
    /// Accumulator for `DecorateSpec`-phase handler output. Each
    /// handler claims one field on `ExecutionDecorations` (e.g.
    /// `native_async`) and sets it. Default = empty.
    pub execution: ExecutionDecorations,
}

/// Mutable compilation state passed to every handler.
///
/// The borrows are split into "shared read-only context" (registries,
/// roots, trust store) and "per-compile mutable scratch" (template
/// ctx, env, spec overrides, params). `chain` and `current_index`
/// expose the chain shape so handlers like `config_resolve` (which
/// reads sibling chain elements) can navigate it.
pub struct CompileContext<'a> {
    pub template_ctx: TemplateContext,
    pub env: HashMap<String, String>,
    pub spec_overrides: SpecOverrides,
    pub params: Value,
    /// Original caller-supplied parameters BEFORE any handler
    /// mutation. `ValidateInput` phase handlers (e.g. `config_schema`)
    /// must validate against this — not `params` — so they see what
    /// the user actually passed in.
    pub original_params: &'a Value,
    pub chain: &'a [ChainIntermediate],
    pub current_index: usize,
    pub roots: &'a ResolutionRoots,
    pub parsers: &'a ParserDispatcher,
    pub kinds: &'a KindRegistry,
    pub trust_store: &'a TrustStore,
    pub project_root: Option<&'a PathBuf>,
}

// ── Handler phasing & cardinality ────────────────────────────────────────

/// Pipeline phase a handler belongs to. Handlers run in `phase()`
/// order; within a phase they run in chain order subject to
/// `cardinality()`. Earlier phases see only what previous phases
/// wrote; `ValidateInput` sees the unmutated `original_params`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HandlerPhase {
    /// Pre-pass over `ctx.original_params` BEFORE any other handler
    /// mutates `ctx.params`. Used by `config_schema`.
    ValidateInput,
    /// Build template extras / env / shared context vars. Runs
    /// before any spec mutation. Used by `env_config`.
    ResolveContext,
    /// Mutate `SpecOverrides` (cmd, args, cwd, timeout, stdin).
    /// Used by `config` (RuntimeConfigHandler) and any handler that
    /// derives spec from resolved context.
    BuildSpec,
    /// Attach metadata flags to the spec
    /// (cancellation_mode, resume_mode, execution_owner). Used by
    /// `native_async`, `native_resume`, `execution_owner`.
    DecorateSpec,
    /// Post-build integrity / safety checks. Used by `verify_deps`.
    Verify,
}

/// Multiplicity semantics: how many chain elements may declare this
/// block, and how the engine resolves multiplicity. Cardinality is
/// enforced by `compile_with_handlers` BEFORE dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerCardinality {
    /// Run on every chain element that declares the block (current
    /// default). Per-element semantics live in the handler.
    All,
    /// Run only on the FIRST chain element that declares it. Mirrors
    /// Python `for element in chain: if element.X: ... break`.
    FirstWins,
    /// Run only on the LAST chain element that declares it.
    LastWins,
    /// Hard error if more than one chain element declares the block.
    /// Used for global runtime configs (e.g. `config`, `config_schema`).
    Singleton,
}

// ── The handler trait ────────────────────────────────────────────────────

/// A runtime handler owns a single top-level YAML key on tool/runtime
/// items (e.g. `"config"`, `"env_config"`). It deserializes its own
/// typed config from the JSON `Value` of that block and mutates the
/// shared `CompileContext`. No other handler touches the same key.
pub trait RuntimeHandler: Send + Sync {
    /// Top-level YAML key this handler claims.
    fn key(&self) -> &'static str;

    /// Pipeline phase. Defaults to `BuildSpec` (most common).
    fn phase(&self) -> HandlerPhase {
        HandlerPhase::BuildSpec
    }

    /// Multiplicity semantics. Defaults to `All` (run on every
    /// declaring chain element — preserves pre-refactor behavior).
    fn cardinality(&self) -> HandlerCardinality {
        HandlerCardinality::All
    }

    /// Run the handler against its block. The handler is responsible
    /// for `deny_unknown_fields`-style strict deserialization and for
    /// returning a structured `EngineError` on misconfiguration.
    fn apply(&self, block: &Value, ctx: &mut CompileContext<'_>) -> Result<(), EngineError>;
}

// ── Registry ─────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct RuntimeHandlerRegistry {
    handlers: HashMap<String, Arc<dyn RuntimeHandler>>,
}

impl RuntimeHandlerRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register a handler. Panics on duplicate key — boot-time
    /// misuse, not a runtime path.
    pub fn register(&mut self, h: Arc<dyn RuntimeHandler>) {
        let key = h.key().to_owned();
        if self.handlers.insert(key.clone(), h).is_some() {
            panic!("RuntimeHandlerRegistry: duplicate handler for key `{key}`");
        }
    }

    pub fn get(&self, key: &str) -> Option<&dyn RuntimeHandler> {
        self.handlers.get(key).map(|a| a.as_ref())
    }

    /// Iterator over registered handler keys (for dispatch loops).
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.handlers.keys().map(String::as_str)
    }

    /// Construct the registry pre-populated with the engine's
    /// built-in handlers (currently `config` and `env_config`).
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register(Arc::new(handlers::runtime_config::RuntimeConfigHandler));
        reg.register(Arc::new(handlers::env_config::EnvConfigHandler));
        reg.register(Arc::new(handlers::config_resolve::ConfigResolveHandler));
        reg.register(Arc::new(handlers::verify_deps::VerifyDepsHandler));
        reg.register(Arc::new(handlers::execution_params::ExecutionParamsHandler));
        reg.register(Arc::new(handlers::native_async::NativeAsyncHandler));
        reg.register(Arc::new(handlers::native_resume::NativeResumeHandler));
        reg
    }
}

impl std::fmt::Debug for RuntimeHandlerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHandlerRegistry")
            .field("handlers", &self.handlers.keys().collect::<Vec<_>>())
            .finish()
    }
}

// ── Top-level compile entrypoint ─────────────────────────────────────────

/// Compile a resolved chain into a `SubprocessSpec` by dispatching
/// every top-level block on every chain intermediate to its registered
/// handler.
///
/// `ignored_keys` is the set of metadata keys the engine deliberately
/// does NOT route through the handler registry (e.g. `version`,
/// `__executor_id__`). Any other key that is not registered is a hard
/// `EngineError::UnknownRuntimeBlock`.
#[allow(clippy::too_many_arguments)]
pub fn compile_with_handlers(
    chain: &[ChainIntermediate],
    root_source_path: &PathBuf,
    chain_str: &[String],
    ignored_keys: &[String],
    registry: &RuntimeHandlerRegistry,
    params: &Value,
    plan_env: &HashMap<String, String>,
    project_root: Option<&PathBuf>,
    parsers: &ParserDispatcher,
    kinds: &KindRegistry,
    trust_store: &TrustStore,
    roots: &ResolutionRoots,
) -> Result<SubprocessSpec, EngineError> {
    let mut ctx = CompileContext {
        template_ctx: TemplateContext::new(root_source_path.clone()),
        env: plan_env.clone(),
        spec_overrides: SpecOverrides::default(),
        params: params.clone(),
        original_params: params,
        chain,
        current_index: 0,
        roots,
        parsers,
        kinds,
        trust_store,
        project_root,
    };
    ctx.template_ctx.project_path = project_root.cloned();
    ctx.template_ctx.params_json = params.to_string();

    // Seed always-present template tokens computed from the chain
    // shape itself (no handler ownership). `tool_dir` is the parent
    // of chain[0]'s source path; `tool_parent` is one level above
    // that. Both are guaranteed present so templates that reference
    // them never need a handler to have run first.
    if let Some(first) = chain.first() {
        if let Some(tool_dir) = first.source_path.parent() {
            ctx.template_ctx
                .extra
                .insert("tool_dir".to_owned(), tool_dir.to_string_lossy().into_owned());
            let tool_parent = tool_dir.parent().unwrap_or(tool_dir);
            ctx.template_ctx.extra.insert(
                "tool_parent".to_owned(),
                tool_parent.to_string_lossy().into_owned(),
            );
        }
    }

    // 1. Validate every key up-front: must be ignored or claimed by a
    //    registered handler. This is a single pass over the chain
    //    that fails loud BEFORE any handler runs (no partial
    //    mutations on misconfiguration).
    for intermediate in chain {
        let Some(obj) = intermediate.parsed.as_object() else {
            continue;
        };
        for key in obj.keys() {
            if ignored_keys.iter().any(|k| k == key) {
                continue;
            }
            if registry.get(key).is_none() {
                return Err(EngineError::UnknownRuntimeBlock {
                    key: key.clone(),
                    kind: intermediate.kind.clone(),
                    source_path: intermediate.source_path.clone(),
                });
            }
        }
    }

    // 2. Group handlers by phase, then dispatch in phase order with
    //    cardinality enforcement.
    let phases = [
        HandlerPhase::ValidateInput,
        HandlerPhase::ResolveContext,
        HandlerPhase::BuildSpec,
        HandlerPhase::DecorateSpec,
        HandlerPhase::Verify,
    ];

    for phase in phases {
        // Stable iteration order over registered handler keys.
        let mut keys: Vec<&str> = registry.keys().collect();
        keys.sort();
        for key in keys {
            let handler = registry.get(key).expect("listed key resolves");
            if handler.phase() != phase {
                continue;
            }

            // Find every chain element that declares this block.
            let declarers: Vec<usize> = chain
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.parsed
                        .as_object()
                        .map(|o| o.contains_key(key))
                        .unwrap_or(false)
                })
                .map(|(i, _)| i)
                .collect();

            if declarers.is_empty() {
                continue;
            }

            // Filter declarers by cardinality.
            let to_run: Vec<usize> = match handler.cardinality() {
                HandlerCardinality::All => declarers.clone(),
                HandlerCardinality::FirstWins => vec![declarers[0]],
                HandlerCardinality::LastWins => vec![*declarers.last().unwrap()],
                HandlerCardinality::Singleton => {
                    if declarers.len() > 1 {
                        let paths: Vec<PathBuf> = declarers
                            .iter()
                            .map(|i| chain[*i].source_path.clone())
                            .collect();
                        return Err(EngineError::DuplicateSingletonBlock {
                            key: key.to_owned(),
                            paths,
                        });
                    }
                    declarers
                }
            };

            for idx in to_run {
                ctx.current_index = idx;
                let block = chain[idx]
                    .parsed
                    .as_object()
                    .and_then(|o| o.get(key))
                    .expect("declarer always has the block");
                handler.apply(block, &mut ctx)?;
            }
        }
    }

    // Now build the spec. command/args/stdin must come from a handler
    // (currently the `config` handler). Templates expanded against
    // the populated template context.
    //
    // Re-derive `params_json` AFTER all handlers have run — handlers
    // may have mutated `ctx.params` to inject additional context vars
    // that must appear in the params JSON passed to the subprocess.
    ctx.template_ctx.params_json = ctx.params.to_string();

    let trust_ref = ctx.trust_store;

    let CompileContext {
        template_ctx,
        env,
        spec_overrides,
        ..
    } = ctx;

    let command = spec_overrides.command.ok_or_else(|| EngineError::NoRuntimeConfig {
        chain: chain_str.to_vec(),
    })?;
    let cmd_expanded = expand_template(&command, &template_ctx)?;

    // Resolve `bin:` prefix — look up the binary from the bundle's
    // `.ai/bin/<triple>/` directory instead of PATH.
    let cmd = resolve_bin_prefix(&cmd_expanded, root_source_path, |fp| trust_ref.get(fp).is_some())?;

    let args_template = spec_overrides.args.unwrap_or_default();
    let args: Result<Vec<String>, EngineError> = args_template
        .iter()
        .map(|a| expand_template(a, &template_ctx))
        .collect();
    let args = args?;

    let stdin_data = spec_overrides
        .stdin_data
        .as_deref()
        .map(|t| expand_template(t, &template_ctx))
        .transpose()?;

    let timeout_secs = spec_overrides.timeout_secs.unwrap_or(300);

    // Expand env values now that the template context is final.
    let mut expanded_env = HashMap::with_capacity(env.len());
    for (k, v) in env {
        let expanded = expand_template(&v, &template_ctx)?;
        expanded_env.insert(k, expanded);
    }

    Ok(SubprocessSpec {
        cmd,
        args,
        cwd: spec_overrides.cwd.or_else(|| project_root.cloned()),
        env: expanded_env,
        stdin_data,
        timeout_secs,
        execution: spec_overrides.execution,
    })
}

/// Resolve the `bin:` executor prefix.
///
/// When `cmd` starts with `bin:`, the remainder is the binary name
/// (must be a single token — subcommand args go in the YAML's `args`
/// list, not in `command`). We walk up from `root_source_path` to
/// find the bundle root, load the CAS manifest, verify the binary's
/// hash and trust record, and return the absolute path.
///
/// If `cmd` does not start with `bin:`, it is returned unchanged.
fn resolve_bin_prefix(
    cmd: &str,
    root_source_path: &Path,
    trust_store_has_fingerprint: impl Fn(&str) -> bool,
) -> Result<String, EngineError> {
    let bin_name = match cmd.strip_prefix("bin:") {
        Some(r) => r.trim(),
        None => return Ok(cmd.to_string()),
    };

    if bin_name.is_empty() {
        return Err(EngineError::InvalidBinPrefix {
            raw: cmd.to_string(),
            detail: "no binary name after `bin:`".into(),
        });
    }
    if bin_name.contains(' ') {
        return Err(EngineError::InvalidBinPrefix {
            raw: cmd.to_string(),
            detail: "binary name must not contain spaces — put subcommand args in the YAML's `args` list".into(),
        });
    }

    let bundle_root = find_bundle_root(root_source_path).ok_or_else(|| {
        EngineError::InvalidBinPrefix {
            raw: cmd.to_string(),
            detail: format!(
                "cannot find bundle root (no .ai/ ancestor of {})",
                root_source_path.display()
            ),
        }
    })?;

    let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
    let bin_dir = bundle_root
        .join(crate::AI_DIR)
        .join("bin")
        .join(triple);
    if !bin_dir.is_dir() {
        return Err(EngineError::BinNotFound {
            bin: bin_name.to_string(),
            searched: format!("expected triple dir {}", bin_dir.display()),
        });
    }

    let bin_path = bin_dir.join(bin_name);
    if !bin_path.exists() {
        return Err(EngineError::BinNotFound {
            bin: bin_name.to_string(),
            searched: bin_path.display().to_string(),
        });
    }

    let manifest_ref_path = bundle_root
        .join(crate::AI_DIR)
        .join("refs")
        .join("bundles")
        .join("manifest");

    // Manifest is part of the bundle's contract. A bundle that ships
    // `bin:` items MUST also ship `refs/bundles/manifest`; absence is
    // a hard error, never a fallback to an exists-only check. This
    // closes the soft-fallback that violated the wave's "no migration
    // shims" rule.
    if !manifest_ref_path.exists() {
        return Err(EngineError::BinManifestMissing {
            bundle_root: bundle_root.display().to_string(),
        });
    }

    let manifest_hash = std::fs::read_to_string(&manifest_ref_path)
        .map_err(|_| EngineError::BinManifestMissing {
            bundle_root: bundle_root.display().to_string(),
        })?
        .trim()
        .to_string();

    let objects_dir = bundle_root.join(crate::AI_DIR).join("objects");
    let cas = lillux::cas::CasStore::new(objects_dir);

    let manifest_value = cas
        .get_object(&manifest_hash)
        .map_err(|e| {
            EngineError::Internal(format!(
                "CAS read error for manifest {manifest_hash}: {e}"
            ))
        })?
        .ok_or_else(|| EngineError::BinManifestMissing {
            bundle_root: bundle_root.display().to_string(),
        })?;

    let item_source_hashes: HashMap<String, String> = manifest_value
        .get("item_source_hashes")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();

    let item_ref = format!("bin/{triple}/{bin_name}");
    let item_source_hash = item_source_hashes
        .get(&item_ref)
        .ok_or_else(|| EngineError::BinNotInManifest {
            bin: bin_name.to_string(),
            triple: triple.to_string(),
        })?;

    let item_source = cas
        .get_object(item_source_hash)
        .map_err(|e| {
            EngineError::Internal(format!(
                "CAS read error for item_source {item_source_hash}: {e}"
            ))
        })?
        .ok_or_else(|| {
            EngineError::Internal(format!(
                "item_source {item_source_hash} for {item_ref} not found in CAS"
            ))
        })?;

    let content_blob_hash = item_source
        .get("content_blob_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let bin_bytes = std::fs::read(&bin_path).map_err(|e| {
        EngineError::Internal(format!(
            "failed to read binary {}: {e}",
            bin_path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bin_bytes);
    let computed_hash = format!("{:x}", hasher.finalize());

    if computed_hash != content_blob_hash {
        return Err(EngineError::BinHashMismatch {
            bin: bin_name.to_string(),
            declared: content_blob_hash,
            computed: computed_hash,
        });
    }

    let (trust_class, fingerprint) =
        crate::executor_resolution::verify_executor_trust(&item_source, trust_store_has_fingerprint);

    match trust_class {
        crate::resolution::TrustClass::TrustedSystem => {}
        crate::resolution::TrustClass::TrustedUser
        | crate::resolution::TrustClass::UntrustedUserSpace
        | crate::resolution::TrustClass::Unsigned => {
            return Err(EngineError::BinUntrusted {
                bin: bin_name.to_string(),
                fingerprint: fingerprint.unwrap_or_default(),
            });
        }
    }

    Ok(bin_path.to_string_lossy().into_owned())
}

/// Walk up from `path` to find the first ancestor containing `.ai/`.
fn find_bundle_root(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if current.join(".ai").is_dir() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

