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
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

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

    let CompileContext {
        template_ctx,
        env,
        spec_overrides,
        ..
    } = ctx;

    let command = spec_overrides.command.ok_or_else(|| EngineError::NoRuntimeConfig {
        chain: chain_str.to_vec(),
    })?;
    let cmd = expand_template(&command, &template_ctx)?;

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
