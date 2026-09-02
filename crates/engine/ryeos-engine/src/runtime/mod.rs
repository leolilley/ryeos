//! Runtime handler dispatch — generic, schema-driven compilation of an
//! executor chain into a `SubprocessSpec`.
//!
//! Mirrors the composer pattern (`crate::composers`):
//!   * Each top-level YAML block on a chain intermediate (e.g. `config`,
//!     `env_config`, `runtime_config`) is claimed by
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

pub use handlers::runtime_config::{LiteralRuntimeArgument, RuntimeArgument};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::contracts::{
    ExecutionDecorations, PlanArgument, PlanStdin, PlanSubprocessSpec, RuntimeEnvSource,
};
use crate::error::EngineError;
use crate::item_resolution::ResolutionRoots;
use crate::kind_registry::KindRegistry;
use crate::parsers::ParserDispatcher;
use crate::resolution::TrustClass;
use crate::trust::TrustStore;

/// Reserved env key prefix — runtime configs may not override
/// daemon-injected bindings.
pub const RESERVED_ENV_PREFIX: &str = "RYEOS_";
pub const RESERVED_DAEMON_ENV_PREFIX: &str = "RYEOSD_";

pub fn is_reserved_env_name(name: &str) -> bool {
    name.starts_with(RESERVED_ENV_PREFIX) || name.starts_with(RESERVED_DAEMON_ENV_PREFIX)
}

// ── Host env passthrough ────────────────────────────────────────────────

/// Operator-supplied allowlist + values for host-env passthrough into
/// subprocess `env_config.env` values. The daemon builds this once at
/// bootstrap from the `RYEOS_TOOL_ENV_PASSTHROUGH` env var (a comma-
/// separated list of allowed names) and the daemon's current process
/// environment.
///
/// Tools request a host var with `"${VAR}"` in an env value. The
/// engine resolves the request only if `VAR` is in `allowed` AND has
/// a value in `values`. Otherwise plan-build fails with a typed
/// `EngineError` (see `HostEnvPassthroughNotAllowed`,
/// `HostEnvPassthroughMissing`).
///
/// **Vault is for secrets.** `RYEOS_TOOL_ENV_PASSTHROUGH` is for
/// non-secret deployment config (hostnames, URLs, ports). Secrets
/// like API keys continue to use the vault + `required_secrets` path.
#[derive(Debug, Clone, Default)]
pub struct HostEnvBindings {
    pub allowed: std::collections::HashSet<String>,
    pub values: std::collections::HashMap<String, String>,
}

impl HostEnvBindings {
    /// Build from an explicit allowlist (e.g. parsed from
    /// `RYEOS_TOOL_ENV_PASSTHROUGH`), snapshotting current host env
    /// values for each allowed key. Rejects reserved names.
    pub fn from_allowlist(allowed: impl IntoIterator<Item = String>) -> Result<Self, EngineError> {
        let mut out = Self::default();
        for name in allowed {
            if is_reserved_env_name(&name) {
                return Err(EngineError::ReservedHostEnvPassthrough { var: name });
            }
            if let Ok(v) = std::env::var(&name) {
                out.values.insert(name.clone(), v);
            }
            out.allowed.insert(name);
        }
        Ok(out)
    }
}

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
    pub source_space: crate::contracts::ItemSpace,
    pub source_root: crate::contracts::ItemSourceRoot,
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
    /// Handler-owned runtime context roots such as `tool_dir` and
    /// `runtime_dir`. Fixed roots above take precedence.
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
    render_runtime_template(template, ctx, None)
}

/// Render an env-config value through the same bounded rye-expr/1 template
/// language as every other runtime field. Uppercase roots retain the existing
/// allowlisted host-environment contract; runtime roots come from
/// `TemplateContext`. `$${...}` is the rye-expr/1 literal escape.
pub fn expand_env_value(
    raw: &str,
    template_ctx: &TemplateContext,
    host_env: &HostEnvBindings,
) -> Result<String, EngineError> {
    render_runtime_template(raw, template_ctx, Some(host_env))
}

fn render_runtime_template(
    source: &str,
    ctx: &TemplateContext,
    host_env: Option<&HostEnvBindings>,
) -> Result<String, EngineError> {
    let compiled = compile_runtime_template(source, ctx)?;
    render_compiled_runtime_template(&compiled, ctx, host_env)
}

fn compile_runtime_template(
    source: &str,
    ctx: &TemplateContext,
) -> Result<ryeos_expression::CompiledTemplate, EngineError> {
    let compilation_limits = ryeos_expression::CompilationLimits::default();
    let compiled = ryeos_expression::compile_template_for(
        source,
        "runtime subprocess template",
        &compilation_limits,
    )
    .map_err(|error| EngineError::RuntimeTemplateExpression {
        reason: error.to_string(),
    })?;
    let fixed_roots = [
        "tool_path",
        "tool_dir",
        "tool_parent",
        "project_path",
        "params_json",
        "interpreter",
        "runtime_dir",
    ];
    ryeos_expression::reject_removed_single_brace_interpolation(
        &compiled,
        fixed_roots
            .into_iter()
            .chain(ctx.extra.keys().map(String::as_str)),
    )
    .map_err(|error| EngineError::RuntimeTemplateExpression {
        reason: error.to_string(),
    })?;
    Ok(compiled)
}

fn render_compiled_runtime_template(
    compiled: &ryeos_expression::CompiledTemplate,
    ctx: &TemplateContext,
    host_env: Option<&HostEnvBindings>,
) -> Result<String, EngineError> {
    let mut roots = Map::new();
    for (name, value) in &ctx.extra {
        roots.insert(name.clone(), Value::String(value.clone()));
    }
    roots.insert(
        "tool_path".to_owned(),
        Value::String(ctx.tool_path.to_string_lossy().into_owned()),
    );
    roots.insert(
        "params_json".to_owned(),
        Value::String(ctx.params_json.clone()),
    );
    if let Some(project_path) = &ctx.project_path {
        roots.insert(
            "project_path".to_owned(),
            Value::String(project_path.to_string_lossy().into_owned()),
        );
    }
    if let Some(interpreter) = &ctx.interpreter {
        roots.insert("interpreter".to_owned(), Value::String(interpreter.clone()));
    }

    if let Some(host_env) = host_env {
        for root in compiled.references().roots() {
            if roots.contains_key(root) || !is_host_env_root(root) {
                continue;
            }
            if is_reserved_env_name(root) {
                return Err(EngineError::ReservedHostEnvPassthrough {
                    var: root.to_owned(),
                });
            }
            if !host_env.allowed.contains(root) {
                return Err(EngineError::HostEnvPassthroughNotAllowed {
                    var: root.to_owned(),
                });
            }
            let value = host_env.values.get(root).ok_or_else(|| {
                EngineError::HostEnvPassthroughMissing {
                    var: root.to_owned(),
                }
            })?;
            roots.insert(root.to_owned(), Value::String(value.clone()));
        }
    }

    let context = Value::Object(roots);
    let evaluation_limits = ryeos_expression::EvaluationLimits::default();
    let rendered = ryeos_expression::render_template(compiled, &context, &evaluation_limits)
        .map_err(|error| EngineError::RuntimeTemplateExpression {
            reason: error.to_string(),
        })?;
    rendered
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| EngineError::RuntimeTemplateExpression {
            reason: format!(
                "runtime subprocess template produced {}; expected string",
                json_type_name(&rendered)
            ),
        })
}

fn compile_stdin_template(
    template: &str,
    ctx: &TemplateContext,
    parameters: &Value,
    project_root: Option<&Path>,
) -> Result<PlanStdin, EngineError> {
    let compiled = compile_runtime_template(template, ctx)?;
    if compiled.whole_direct_root_reference() == Some("params_json") {
        let mut parameters = parameters.clone();
        let project_path = match (parameters.as_object_mut(), project_root) {
            (Some(object), Some(project_root)) => object
                .remove("project_path")
                .map(|_| project_root.to_path_buf()),
            _ => None,
        };
        return Ok(PlanStdin::RuntimeParameters {
            parameters,
            project_path,
        });
    }
    let data = render_compiled_runtime_template(&compiled, ctx, None)?;
    Ok(PlanStdin::Opaque { data })
}

fn render_runtime_argument(
    argument: RuntimeArgument,
    ctx: &TemplateContext,
) -> Result<PlanArgument, EngineError> {
    match argument {
        RuntimeArgument::Template(template) if template == "${source.entry}" => {
            Ok(PlanArgument::AdmittedSourceEntry)
        }
        RuntimeArgument::Template(template) if template.contains("${source.") => {
            Err(EngineError::InvalidRuntimeConfig {
                path: "config.args".to_owned(),
                reason: "source entry must occupy one complete argument".to_owned(),
            })
        }
        RuntimeArgument::Template(template) => {
            expand_template(&template, ctx).map(PlanArgument::literal)
        }
        RuntimeArgument::Literal(literal) => Ok(PlanArgument::literal(literal.literal)),
    }
}

fn is_host_env_root(root: &str) -> bool {
    root.bytes()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        && root.bytes().any(|byte| byte.is_ascii_uppercase())
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ── Handler-side mutable state ───────────────────────────────────────────

/// Subprocess spec fields a handler can write into. The final
/// `SubprocessSpec` is built by `compile_with_handlers` after all
/// handlers have run; templates are expanded at that point.
#[derive(Debug, Default, Clone)]
pub struct SpecOverrides {
    pub command: Option<String>,
    pub args: Option<Vec<RuntimeArgument>>,
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
    pub env_sources: HashMap<String, RuntimeEnvSource>,
    pub spec_overrides: SpecOverrides,
    /// Mutable planner parameters. Runtime handlers may add resolved
    /// execution-policy values here, but those values are not invocation
    /// input and must never cross the subprocess stdin boundary implicitly.
    pub params: Value,
    /// Original caller-supplied invocation parameters BEFORE any handler
    /// mutation. `ValidateInput` phase handlers (e.g. `config_schema`) must
    /// validate against this, and subprocess input is projected from this
    /// authority plus only root-owned resolved configuration.
    pub original_params: &'a Value,
    pub chain: &'a [ChainIntermediate],
    pub current_index: usize,
    pub roots: &'a ResolutionRoots,
    pub parsers: &'a ParserDispatcher,
    pub kinds: &'a KindRegistry,
    pub trust_store: &'a TrustStore,
    pub node_trust_store: &'a TrustStore,
    pub project_root: Option<&'a Path>,
    pub project_authority: Option<(
        &'a Path,
        &'a dyn crate::project_content::AuthoritativeProjectContent,
    )>,
    /// Sealed bytes for dependencies covered by an admitted realization
    /// mount. Verification consults this before the live filesystem, so a
    /// realized dependency is judged by the bytes the runtime will execute.
    pub sealed_content: Option<&'a dyn crate::project_content::SealedDependencyBytes>,
    pub root_trust_class: TrustClass,
    /// Operator-supplied allowlist + snapshot for host-env passthrough.
    /// Populated once at daemon bootstrap from `RYEOS_TOOL_ENV_PASSTHROUGH`.
    pub host_env: &'a HostEnvBindings,
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
}

/// Project the exact tool-facing invocation payload from the two parameter
/// authorities used during runtime compilation.
///
/// `resolved_params` is planner scratch: non-root runtime handlers place
/// timeout and cancellation policy there so later handlers can compile the
/// process-control contract. Only a root `config_resolve` contribution is
/// tool input. Project authority is not invented here; an explicitly supplied
/// `project_path` remains in the invocation until `compile_stdin_template`
/// converts that one field into a relocatable typed binding.
fn subprocess_invocation_params(original_params: &Value, resolved_params: &Value) -> Value {
    let mut invocation = original_params.clone();
    if let Some(resolved_config) = resolved_params.get("resolved_config") {
        if !invocation.is_object() {
            invocation = Value::Object(Map::new());
        }
        invocation
            .as_object_mut()
            .expect("invocation converted to object")
            .insert("resolved_config".to_owned(), resolved_config.clone());
    }
    invocation
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
/// `executor_id`). Any other key that is not registered is a hard
/// `EngineError::UnknownRuntimeBlock`.
#[allow(clippy::too_many_arguments)]
pub fn compile_with_handlers(
    chain: &[ChainIntermediate],
    root_source_path: &Path,
    chain_str: &[String],
    ignored_keys: &[String],
    registry: &RuntimeHandlerRegistry,
    params: &Value,
    plan_env: &HashMap<String, String>,
    host_env: &HostEnvBindings,
    project_root: Option<&Path>,
    parsers: &ParserDispatcher,
    kinds: &KindRegistry,
    trust_store: &TrustStore,
    node_trust_store: &TrustStore,
    roots: &ResolutionRoots,
    root_trust_class: TrustClass,
    project_authority: Option<(
        &Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
    sealed_content: Option<&dyn crate::project_content::SealedDependencyBytes>,
) -> Result<PlanSubprocessSpec, EngineError> {
    let mut ctx = CompileContext {
        template_ctx: TemplateContext::new(root_source_path.to_path_buf()),
        env: plan_env.clone(),
        env_sources: plan_env
            .keys()
            .map(|key| (key.clone(), RuntimeEnvSource::EnginePlan))
            .collect(),
        spec_overrides: SpecOverrides::default(),
        params: params.clone(),
        original_params: params,
        chain,
        current_index: 0,
        roots,
        parsers,
        kinds,
        trust_store,
        node_trust_store,
        project_root,
        project_authority,
        sealed_content,
        root_trust_class,
        host_env,
    };
    ctx.template_ctx.project_path = project_root.map(|p| p.to_path_buf());

    ctx.template_ctx.params_json = params.to_string();

    // Seed always-present template tokens computed from the chain
    // shape itself (no handler ownership). `tool_dir` is the parent
    // of chain[0]'s source path; `tool_parent` is one level above
    // that. Both are guaranteed present so templates that reference
    // them never need a handler to have run first.
    if let Some(first) = chain.first()
        && let Some(tool_dir) = first.source_path.parent()
    {
        ctx.template_ctx.extra.insert(
            "tool_dir".to_owned(),
            tool_dir.to_string_lossy().into_owned(),
        );
        let tool_parent = tool_dir.parent().unwrap_or(tool_dir);
        ctx.template_ctx.extra.insert(
            "tool_parent".to_owned(),
            tool_parent.to_string_lossy().into_owned(),
        );
    }

    // 1. Validate every key up-front against the schema of the chain
    //    element that authored it. Executor chains may cross kinds (for
    //    example, a worker whose terminal executor is a tool); borrowing the
    //    root kind's ignored keys or handler declarations would either reject
    //    valid terminal metadata or, worse, let one kind smuggle a block that
    //    only another kind owns. The explicit registry remains the mechanical
    //    implementation set. Empty test registries without loaded kind
    //    schemas retain the uniform-policy path used by the focused compiler
    //    fixtures below.
    for intermediate in chain {
        let Some(obj) = intermediate.parsed.as_object() else {
            continue;
        };
        let kind_runtime = kinds
            .get(&intermediate.kind)
            .map(|schema| {
                schema.runtime().ok_or_else(|| EngineError::SchemaLoaderError {
                reason: format!(
                    "kind `{}` has no runtime block while compiling executor-chain item `{}`",
                    intermediate.kind, intermediate.resolved_ref
                ),
            })
            })
            .transpose()?;
        if let Some(spec) = kind_runtime {
            for declaration in &spec.handlers {
                if registry.get(&declaration.type_).is_none() {
                    return Err(EngineError::SchemaLoaderError {
                        reason: format!(
                            "kind `{}` declares runtime handler `{}` which is not registered",
                            intermediate.kind, declaration.type_
                        ),
                    });
                }
            }
        }
        for key in obj.keys() {
            let (ignored, claimed) = match kind_runtime {
                Some(spec) => (
                    spec.ignored_keys.iter().any(|candidate| candidate == key),
                    spec.handlers
                        .iter()
                        .any(|declaration| declaration.type_ == *key),
                ),
                None => (
                    ignored_keys.iter().any(|candidate| candidate == key),
                    registry.get(key).is_some(),
                ),
            };
            if ignored {
                continue;
            }
            if !claimed || registry.get(key).is_none() {
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
                    let present = c
                        .parsed
                        .as_object()
                        .map(|o| o.contains_key(key))
                        .unwrap_or(false);
                    if !present {
                        return false;
                    }
                    kinds
                        .get(&c.kind)
                        .and_then(|schema| schema.runtime())
                        .map(|spec| {
                            spec.handlers
                                .iter()
                                .any(|declaration| declaration.type_ == key)
                        })
                        .unwrap_or(true)
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
    // Re-derive the subprocess payload AFTER handlers have run, while keeping
    // runtime execution policy on the planner side of the ABI. Root-owned
    // resolved configuration remains an explicit tool input; non-root timeout
    // and cancellation values do not become caller parameters.
    let invocation_params = subprocess_invocation_params(ctx.original_params, &ctx.params);
    ctx.template_ctx.params_json = invocation_params.to_string();

    let node_trust_ref = ctx.node_trust_store;

    let CompileContext {
        template_ctx,
        env,
        env_sources,
        spec_overrides,
        host_env: ctx_host_env,
        ..
    } = ctx;

    let command = spec_overrides
        .command
        .ok_or_else(|| EngineError::NoRuntimeConfig {
            chain: chain_str.to_vec(),
        })?;
    let cmd_expanded = expand_template(&command, &template_ctx)?;

    // Resolve `bin:` prefix — look up the binary from signed bundle `.ai/bin/`
    // material instead of PATH. Unqualified refs stay wrapper-local;
    // qualified refs (`bin:<bundle>/<name>`) resolve from a registered bundle
    // while keeping runtime authority on the wrapper item.
    let (cmd, verified_command) = if cmd_expanded.starts_with("bin:") {
        let resolved = crate::binary_resolver::resolve_runtime_binary_command_ref(
            &cmd_expanded,
            &chain
                .first()
                .ok_or_else(|| EngineError::NoRuntimeConfig {
                    chain: chain_str.to_vec(),
                })?
                .source_root,
            chain[0].source_space,
            root_source_path,
            roots,
            node_trust_ref,
            ctx.root_trust_class,
        )?;
        (
            resolved.absolute_path.to_string_lossy().into_owned(),
            Some(crate::contracts::PlanVerifiedCommand::BundleExecutor {
                code: crate::isolation::IsolationVerifiedCode {
                    source_path: resolved.absolute_path,
                    content_hash: resolved.content_hash,
                },
                provider: crate::contracts::PlanBundleExecutorIdentity {
                    manifest_hash: resolved.manifest_hash,
                    signer_fingerprint: resolved.signer_fingerprint,
                },
            }),
        )
    } else {
        (cmd_expanded, None)
    };

    let authored_args = spec_overrides.args.unwrap_or_default();
    let args: Result<Vec<PlanArgument>, EngineError> = authored_args
        .into_iter()
        .map(|argument| render_runtime_argument(argument, &template_ctx))
        .collect();
    let args = args?;

    let stdin = spec_overrides
        .stdin_data
        .as_deref()
        .map(|template| {
            compile_stdin_template(template, &template_ctx, &invocation_params, project_root)
        })
        .transpose()?;

    let timeout_secs = spec_overrides.timeout_secs.unwrap_or(300);

    // Expand env values now that the template context is final.
    let mut expanded_env = HashMap::with_capacity(env.len());
    let mut expanded_env_sources = HashMap::with_capacity(env.len());
    for (k, v) in env {
        let expanded = expand_env_value(&v, &template_ctx, ctx_host_env)?;
        if let Some(source) = env_sources.get(&k).copied() {
            expanded_env_sources.insert(k.clone(), source);
        }
        expanded_env.insert(k, expanded);
    }

    Ok(PlanSubprocessSpec {
        cmd,
        verified_command,
        args,
        cwd: spec_overrides
            .cwd
            .or_else(|| project_root.map(|p| p.to_path_buf())),
        env: expanded_env,
        env_sources: expanded_env_sources,
        stdin,
        timeout_secs,
        execution: spec_overrides.execution,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn env_value_expands_allowed_host_env_passthrough() {
        let mut host_env = HostEnvBindings::default();
        host_env.allowed.insert("BACKEND_API_URL".into());
        host_env.values.insert(
            "BACKEND_API_URL".into(),
            "http://host.docker.internal:4000".into(),
        );
        let ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        let got = expand_env_value("${BACKEND_API_URL}/api/x", &ctx, &host_env).unwrap();
        assert_eq!(got, "http://host.docker.internal:4000/api/x");
    }

    #[test]
    fn env_value_rejects_unlisted_host_env_passthrough() {
        let host_env = HostEnvBindings::default();
        let ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        let err = expand_env_value("${SECRET_KEY}", &ctx, &host_env).unwrap_err();
        assert!(
            matches!(
                err,
                EngineError::HostEnvPassthroughNotAllowed { ref var } if var == "SECRET_KEY"
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn env_value_rejects_reserved_host_env_passthrough() {
        let mut host_env = HostEnvBindings::default();
        // Simulate misconfigured allowlist that somehow includes a
        // reserved name — the expansion must still reject it.
        host_env.allowed.insert("RYEOS_THREAD_ID".into());
        let ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        let err = expand_env_value("${RYEOS_THREAD_ID}", &ctx, &host_env).unwrap_err();
        assert!(
            matches!(err, EngineError::ReservedHostEnvPassthrough { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn env_value_reports_allowed_but_missing() {
        let mut host_env = HostEnvBindings::default();
        host_env.allowed.insert("BACKEND_API_URL".into());
        // No value populated.
        let ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        let err = expand_env_value("${BACKEND_API_URL}", &ctx, &host_env).unwrap_err();
        assert!(
            matches!(
                err,
                EngineError::HostEnvPassthroughMissing { ref var } if var == "BACKEND_API_URL"
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn host_env_bindings_constructor_rejects_reserved_names() {
        let err = HostEnvBindings::from_allowlist(["RYEOS_FOO".into()]).unwrap_err();
        assert!(
            matches!(err, EngineError::ReservedHostEnvPassthrough { .. }),
            "got {err:?}"
        );

        let err = HostEnvBindings::from_allowlist(["RYEOSD_CALLBACK_TOKEN".into()]).unwrap_err();
        assert!(
            matches!(err, EngineError::ReservedHostEnvPassthrough { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn env_value_passthrough_dollar_without_brace() {
        // A lone `$` not followed by `{` must pass through untouched.
        let host_env = HostEnvBindings::default();
        let ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        let got = expand_env_value("price: $5", &ctx, &host_env).unwrap();
        assert_eq!(got, "price: $5");
    }

    #[test]
    fn env_value_expands_both_host_and_template() {
        // Host and runtime roots share one rye-expr/1 evaluation.
        let mut host_env = HostEnvBindings::default();
        host_env.allowed.insert("MY_HOST".into());
        host_env.values.insert("MY_HOST".into(), "hello".into());
        let ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        let got = expand_env_value("${MY_HOST}-${tool_path}", &ctx, &host_env).unwrap();
        assert_eq!(got, "hello-/tool.yaml");
    }

    #[test]
    fn template_leaves_empty_braces_literal() {
        let ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        let got = expand_template("print('{}') ${tool_path}", &ctx).unwrap();
        assert_eq!(got, "print('{}') /tool.yaml");
    }

    #[test]
    fn template_rejects_removed_runtime_root_interpolation() {
        let mut ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        ctx.extra
            .insert("custom_runtime_root".into(), "/runtime".into());

        for source in [
            "{tool_path}",
            "prefix/{project_path}",
            "{custom_runtime_root}/bin",
        ] {
            let error = expand_template(source, &ctx).unwrap_err();
            assert!(
                error.to_string().contains("removed") && error.to_string().contains("rye-expr/1"),
                "unexpected error for {source}: {error}"
            );
        }
    }

    #[test]
    fn template_leaves_unrelated_brace_grammars_literal() {
        let ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        let got = expand_template("bin/{triple}/tool", &ctx).unwrap();
        assert_eq!(got, "bin/{triple}/tool");
    }

    #[test]
    fn literal_runtime_argument_bypasses_expression_compilation() {
        let ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        let source = "print(f'{tool_path} ${not_a_rye_expression')";
        let argument = RuntimeArgument::Literal(LiteralRuntimeArgument {
            literal: source.into(),
        });

        assert_eq!(
            render_runtime_argument(argument, &ctx)
                .unwrap()
                .literal_value(),
            Some(source)
        );
    }

    #[test]
    fn source_entry_argument_remains_typed_until_admitted_materialization() {
        let ctx = TemplateContext::new(PathBuf::from("/worker.yaml"));
        let argument = RuntimeArgument::Template("${source.entry}".to_owned());
        assert_eq!(
            render_runtime_argument(argument, &ctx).unwrap(),
            PlanArgument::AdmittedSourceEntry
        );
    }

    #[test]
    fn source_entry_refuses_partial_string_interpolation() {
        let ctx = TemplateContext::new(PathBuf::from("/worker.yaml"));
        let argument = RuntimeArgument::Template("--entry=${source.entry}".to_owned());
        let error = render_runtime_argument(argument, &ctx).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("source entry must occupy one complete argument")
        );
    }

    #[test]
    fn template_uses_rye_expr_literal_escape() {
        let ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        let got = expand_template("$${tool_path} ${tool_path}", &ctx).unwrap();
        assert_eq!(got, "${tool_path} /tool.yaml");
    }

    #[test]
    fn direct_params_json_stdin_remains_typed() {
        let mut ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        ctx.params_json = r#"{"message":"hello"}"#.to_owned();
        let parameters = json!({"message": "hello"});

        let stdin = compile_stdin_template("${ (params_json) }", &ctx, &parameters, None).unwrap();
        let PlanStdin::RuntimeParameters {
            parameters: actual,
            project_path,
        } = stdin
        else {
            panic!("direct params_json must remain structured");
        };
        assert_eq!(actual, parameters);
        assert_eq!(project_path, None);
    }

    #[test]
    fn runtime_policy_params_do_not_become_subprocess_input() {
        let original = json!({"message": "hello"});
        let resolved = json!({
            "message": "hello",
            "timeout": 86400,
            "cancellation_mode": "graceful",
            "cancellation_grace_secs": 5,
        });

        assert_eq!(subprocess_invocation_params(&original, &resolved), original);
    }

    #[test]
    fn explicit_project_path_and_root_resolved_config_remain_typed_input() {
        let project = PathBuf::from("/admitted/project");
        let invocation = subprocess_invocation_params(
            &json!({"message": "hello", "project_path": "/caller-controlled"}),
            &json!({
                "message": "hello",
                "resolved_config": {"model": "qualified"},
                "timeout": 86400,
            }),
        );
        let mut ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        ctx.params_json = invocation.to_string();
        let stdin =
            compile_stdin_template("${params_json}", &ctx, &invocation, Some(&project)).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stdin.materialize().unwrap()).unwrap(),
            json!({
                "message": "hello",
                "project_path": "/admitted/project",
                "resolved_config": {"model": "qualified"},
            })
        );
        let PlanStdin::RuntimeParameters {
            parameters,
            project_path,
        } = stdin
        else {
            panic!("direct params_json must remain structured");
        };

        assert_eq!(
            parameters,
            json!({
                "message": "hello",
                "resolved_config": {"model": "qualified"},
            })
        );
        assert_eq!(project_path.as_deref(), Some(project.as_path()));
    }

    #[test]
    fn project_backed_runtime_does_not_invent_project_path_input() {
        let project = PathBuf::from("/admitted/project");
        let invocation = subprocess_invocation_params(
            &json!({}),
            &json!({
                "timeout": 86400,
                "cancellation_mode": "graceful",
                "cancellation_grace_secs": 5,
            }),
        );
        let mut ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        ctx.params_json = invocation.to_string();
        let stdin =
            compile_stdin_template("${params_json}", &ctx, &invocation, Some(&project)).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stdin.materialize().unwrap()).unwrap(),
            json!({})
        );
        let PlanStdin::RuntimeParameters {
            parameters,
            project_path,
        } = stdin
        else {
            panic!("direct params_json must remain structured");
        };

        assert_eq!(parameters, json!({}));
        assert_eq!(project_path, None);
    }

    #[test]
    fn embedded_params_json_stdin_is_opaque() {
        let mut ctx = TemplateContext::new(PathBuf::from("/tool.yaml"));
        ctx.params_json = r#"{"message":"hello"}"#.to_owned();

        let stdin =
            compile_stdin_template("payload=${params_json}", &ctx, &json!({}), None).unwrap();
        let PlanStdin::Opaque { data } = stdin else {
            panic!("embedded params_json must remain opaque");
        };
        assert_eq!(data, r#"payload={"message":"hello"}"#);
    }
}
