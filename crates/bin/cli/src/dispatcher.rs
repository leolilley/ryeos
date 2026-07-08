use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use ryeos_runtime::{
    CommandDef, CommandDispatch, CommandParameterBindingMode, CommandProjectDefault,
    CommandProjectResolution, CommandRegistry, InvocationInputContract,
};
use serde_json::Value;

use crate::error::CliError;
use crate::lifecycle_commands;

/// CLI struct for clap argument parsing.
#[derive(clap::Parser)]
#[command(
    name = "ryeos",
    about = "CLI for Rye OS",
    version,
    disable_help_flag = true,
    disable_help_subcommand = true,
    trailing_var_arg = true
)]
pub struct Cli {
    /// Project root (overrides cwd).
    #[arg(short, long)]
    project: Option<PathBuf>,

    /// Verbose tracing output.
    #[arg(long)]
    pub debug: bool,

    /// Command tokens + tail (everything after globals).
    #[arg(trailing_var_arg = true)]
    pub rest: Vec<String>,
}

/// Main dispatch flow.
pub async fn run(cli: Cli) -> Result<(), CliError> {
    // 1. Project root
    let body_project_path = cli
        .project
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());

    // 2. App root
    let app_root = discover_app_root();

    // 3. Hardcoded lifecycle/bootstrap commands (must work before daemon exists):
    //      ryeos identity                   — print local node identity
    //      ryeos init                       — bootstrap operator state
    //      ryeos start/stop                 — manage the local daemon
    //      ryeos node status                — inspect lifecycle state
    if lifecycle_commands::try_dispatch(&cli.rest).await? {
        return Ok(());
    }

    if should_show_tty_screen(&cli.rest, std::io::stdout().is_terminal()) {
        return crate::tty::run(
            &app_root,
            cli.project.as_deref(),
            tty_screen_for(&cli.rest),
            cli.debug,
        )
        .await;
    }

    // Load the verified node-config snapshot once per invocation; help,
    // offline dispatch, and daemon resolution all reuse it instead of
    // re-reading and re-verifying node config from disk. Local commands
    // above must not need it so `ryeos init` works before any node
    // config exists. Help degrades gracefully on load failure; dispatch
    // paths fail hard below.
    let snapshot = crate::node_descriptors::load_verified_snapshot(&app_root);

    // 4. No command = help
    if cli.rest.is_empty() {
        crate::help::print_help(std::io::stdout(), &app_root, &snapshot)?;
        return Ok(());
    }

    // Non-TTY `ryeos help` keeps the plain top-level help path. TTY stdout
    // is intercepted above and rendered through the compact TTY help screen.
    if cli.rest == ["help"] {
        crate::help::print_help(std::io::stdout(), &app_root, &snapshot)?;
        return Ok(());
    }

    // `ryeos help --all` / `ryeos commands` → exhaustive command reference.
    if cli.rest == ["help", "--all"] || cli.rest == ["commands"] {
        crate::help::print_help(std::io::stdout(), &app_root, &snapshot)?;
        return Ok(());
    }

    // `ryeos help <command...>` → command help (queries daemon for alias info)
    if cli.rest.len() > 1 && cli.rest[0] == "help" {
        crate::help::print_command_help(
            &cli.rest[1..],
            &app_root,
            body_project_path.as_deref().unwrap_or("."),
            &snapshot,
        )
        .await?;
        return Ok(());
    }

    // `ryeos <command...> --help` / `-h` should feel like a normal CLI.
    // Without this guard the trailing help flag is bound as service
    // input and strict service schemas return noisy "unknown field help".
    if let Some(help_idx) = cli.rest.iter().position(|t| t == "--help" || t == "-h") {
        let command_tokens = &cli.rest[..help_idx];
        if command_tokens.is_empty() {
            crate::help::print_help(std::io::stdout(), &app_root, &snapshot)?;
        } else {
            crate::help::print_command_help(
                command_tokens,
                &app_root,
                body_project_path.as_deref().unwrap_or("."),
                &snapshot,
            )
            .await?;
        }
        return Ok(());
    }

    // Past help: offline and daemon dispatch require verified node config.
    let snapshot = match &snapshot {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return Err(CliError::Local {
                detail: format!("load verified node config: {err:#}"),
            });
        }
    };

    // 5. Descriptor-driven offline dispatch.
    //    For commands whose service descriptor declares availability: offline,
    //    run the in-process handler. Returns None to fall through to daemon.
    if let Some(outcome) = crate::offline_dispatch::try_offline_dispatch(
        &cli.rest,
        &app_root,
        body_project_path.as_deref().unwrap_or("."),
        snapshot,
    )? {
        if let crate::offline_dispatch::OfflineDispatchOutcome::Json(result) = outcome {
            print_result(result);
        }
        return Ok(());
    }

    // 6. Token dispatch — send tokens to daemon, it resolves the command
    //    registry and binds tail parameters server-side.
    //
    //    For remote commands that take a project root, CLI-side rewrite injects a canonical
    //    `--project <abs>` or `--no-project` into the tail. The daemon
    //    cannot do this — its cwd is irrelevant to the caller. Accepting
    //    `--project` here is deliberate: project-aware aliases expose a
    //    service-schema `project` field, while global `-p/--project` before
    //    the command remains supported by clap above.

    let mut resolved = resolve_command_for_daemon(&cli.rest, snapshot, cli.project.as_deref())?;
    let item_ref_for_contract = resolved.item_ref.clone();
    normalize_resolved_parameters(
        &app_root,
        snapshot,
        &item_ref_for_contract,
        resolved.project_path.as_deref(),
        &mut resolved.parameters,
    )?;

    let mut body = serde_json::json!({
        "item_ref": resolved.item_ref,
        "parameters": resolved.parameters,
    });
    if resolved.async_launch {
        body["launch_mode"] = Value::String("accepted".to_string());
    }
    if let Some(project_path) = &resolved.project_path {
        body["project_path"] = Value::String(project_path.to_string_lossy().into_owned());
    }
    if resolved.debug_raw {
        body["debug_raw"] = Value::Bool(true);
    }
    if let Some(state_root) = &resolved.state_root {
        body["state_root"] = Value::String(state_root.to_string_lossy().into_owned());
    }
    // Method dispatch: a `--method`/`--args` selector lands in `call`, the
    // control-plane block distinct from data-plane `parameters`.
    if resolved.call_method.is_some() || resolved.call_args.is_some() {
        let mut call = serde_json::Map::new();
        if let Some(method) = &resolved.call_method {
            call.insert("method".to_string(), Value::String(method.clone()));
        }
        if let Some(args) = &resolved.call_args {
            call.insert("args".to_string(), args.clone());
        }
        body["call"] = Value::Object(call);
    }

    // Stream a live execution log for `execute` runs on a terminal, unless the
    // caller opted out. Piped/redirected output and `--no-stream`/`--json` get
    // the buffered JSON result (machine-friendly, unchanged behavior).
    // `/execute/stream` requires a project_path (unlike `/execute`, which falls
    // back to the app root), so fall back to the buffered path when none was
    // resolved (`--no-project` / outside a project). `--stream`/`--no-stream`
    // force the choice; otherwise auto-detect a terminal.
    // A state-root override is carried only by the buffered `/execute` route
    // (whose response also echoes both roots as execution diagnostics), so it
    // forces the buffered path; forcing the stream on alongside it is a
    // contradiction worth failing loudly.
    if resolved.state_root.is_some() && resolved.stream == Some(true) {
        return Err(CliError::Local {
            detail: "--state-root is not supported with --stream (the override rides the \
                     buffered /execute route)"
                .into(),
        });
    }
    let want_stream = resolved
        .stream
        .unwrap_or_else(|| std::io::IsTerminal::is_terminal(&std::io::stdout()));
    let stream_live = resolved.direct_execute
        && !resolved.async_launch
        && resolved.project_path.is_some()
        && resolved.state_root.is_none()
        && want_stream;
    if stream_live {
        return post_to_daemon_streaming(&app_root, &body).await;
    }

    let route_path = if resolved.async_launch {
        "/execute/launch"
    } else {
        "/execute"
    };
    let result = post_to_daemon(&app_root, route_path, &body).await?;

    // A service may resolve to a live stream rather than a buffered value: the
    // daemon mediates by returning a stream descriptor (the signed SSE route to
    // open), keeping itself out of the byte path. Any such command (e.g.
    // `thread tail`) is followed here by opening the stream and rendering it. A
    // descriptor that is present but unsupported/unsafe errors loudly rather than
    // being misread as an ordinary JSON result and exiting zero.
    if let Some((path, braid)) = stream_descriptor_path(&result)? {
        return follow_stream_descriptor(&app_root, &path, braid).await;
    }

    print_result(result);
    Ok(())
}

fn should_show_tty_screen(rest: &[String], stdout_is_tty: bool) -> bool {
    stdout_is_tty
        && (rest.is_empty() || rest == ["help"] || rest == ["--help"] || rest == ["-h"])
}

fn tty_screen_for(rest: &[String]) -> crate::tty::TtyScreen {
    if rest == ["help"] || rest == ["--help"] || rest == ["-h"] {
        crate::tty::TtyScreen::Help
    } else {
        crate::tty::TtyScreen::Home
    }
}

/// Parse a stream descriptor from an `/execute` response into `(path, braid)`.
///
/// The service envelope nests the handler's value under `result`, so the
/// descriptor is `result.stream` = `{ transport, method, path, follow }`.
/// Returns `Ok(None)` when there is no descriptor (an ordinary result),
/// `Ok(Some((path, braid)))` for a valid, followable, safe descriptor (`braid`
/// = chain follow), and `Err` when a descriptor is present but malformed, an
/// unsupported transport/method/follow, or an unsafe path — so a broken stream
/// contract never silently prints as JSON and exits zero.
fn stream_descriptor_path(response: &Value) -> Result<Option<(String, bool)>, CliError> {
    let Some(stream) = response.get("result").and_then(|r| r.get("stream")) else {
        return Ok(None);
    };
    let transport = stream
        .get("transport")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::Local {
            detail: "stream descriptor missing 'transport'".into(),
        })?;
    if transport != "sse" {
        return Err(CliError::Local {
            detail: format!("unsupported stream transport '{transport}' (expected 'sse')"),
        });
    }
    let method = stream
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::Local {
            detail: "stream descriptor missing 'method'".into(),
        })?;
    if !method.eq_ignore_ascii_case("GET") {
        return Err(CliError::Local {
            detail: format!("unsupported stream method '{method}' (expected 'GET')"),
        });
    }
    let path = stream
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::Local {
            detail: "stream descriptor missing 'path'".into(),
        })?;
    validate_descriptor_path(path)?;
    // `follow` selects the completion policy: a single thread stops at its
    // terminal; a chain (braid) follows continuations until the stream closes.
    // Absent defaults to `thread`; an unknown mode errors loudly.
    let braid = match stream.get("follow").and_then(Value::as_str) {
        None | Some("thread") => false,
        Some("chain") => true,
        Some(other) => {
            return Err(CliError::Local {
                detail: format!(
                    "unsupported stream follow mode '{other}' (expected 'thread' or 'chain')"
                ),
            })
        }
    };
    Ok(Some((path.to_string(), braid)))
}

/// Reject any descriptor path that is not a safe, node-relative path before it is
/// signed and opened. The daemon currently only emits safe paths, but this is a
/// generic descriptor consumer, so it must be self-protecting against a scheme,
/// authority, fragment, control/whitespace chars, or a missing leading slash.
fn validate_descriptor_path(path: &str) -> Result<(), CliError> {
    let reject = |why: &str| {
        Err(CliError::Local {
            detail: format!("unsafe stream descriptor path '{path}': {why}"),
        })
    };
    if !path.starts_with('/') {
        return reject("must be node-relative (start with '/')");
    }
    if path.starts_with("//") {
        return reject("must not carry an authority");
    }
    if path.contains("://") {
        return reject("must not be a full URL");
    }
    if path.contains('#') {
        return reject("must not contain a fragment");
    }
    if path.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return reject("must not contain control or whitespace characters");
    }
    Ok(())
}

/// Sign and follow a daemon-issued stream descriptor: GET the SSE route and
/// render events with the shared renderer. Completion differs from
/// `execute --stream`: a tail can end on a clean EOF with no terminal event
/// (e.g. a running thread the user Ctrl-Cs), so only a failing terminal — or a
/// transport/auth error — is non-zero; Done / clean EOF exit 0.
///
/// `braid` follows the whole chain across continuations: it renders each
/// thread's terminal as it passes but never stops on one — the braid keeps
/// going to the next turn — ending only on clean EOF / interrupt (exit 0).
async fn follow_stream_descriptor(
    app_root: &Path,
    path: &str,
    braid: bool,
) -> Result<(), CliError> {
    use crate::exec_stream::StreamOutcome;

    let daemon_url = crate::transport::http::resolve_daemon_url(app_root).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;
    let discovered = crate::transport::discovery::discover_audience(&daemon_url).await?;

    let headers = signer.sign("GET", path, &[], &discovered.principal_id)?;
    let url = format!(
        "{}{path}",
        discovered.effective_base_url.trim_end_matches('/')
    );

    let mut terminal: Option<Result<(), String>> = None;
    crate::transport::http::get_streaming(&url, &headers, |ev| {
        match crate::exec_stream::render_event(ev) {
            StreamOutcome::Continue => false,
            // A braid follows continuations: a per-thread terminal is rendered
            // but does not stop the stream — only EOF / interrupt ends it.
            StreamOutcome::Done if braid => false,
            StreamOutcome::Failed(_) if braid => false,
            StreamOutcome::Done => {
                terminal = Some(Ok(()));
                true
            }
            StreamOutcome::Failed(detail) => {
                terminal = Some(Err(detail));
                true
            }
        }
    })
    .await?;

    match terminal {
        Some(Err(detail)) => Err(CliError::Local { detail }),
        Some(Ok(())) | None => Ok(()),
    }
}

struct CliResolvedExecute {
    item_ref: String,
    parameters: Value,
    project_path: Option<PathBuf>,
    async_launch: bool,
    /// True when this is the `execute` command (`direct_execute_item_ref`) —
    /// the only path that streams a live execution log.
    direct_execute: bool,
    /// Streaming preference: `None` = auto (TTY-detect), `Some(true)` =
    /// `--stream` (force on), `Some(false)` = `--no-stream`/`--json` (force off).
    stream: Option<bool>,
    /// `--debug-raw`: request the debug block on the execution result.
    debug_raw: bool,
    /// Method selector for method-dispatch kinds → request `call.method`.
    call_method: Option<String>,
    /// Method args (parsed JSON) → request `call.args`.
    call_args: Option<Value>,
    /// `--state-root <path>`: runtime state-root override → request
    /// `state_root` (absolutized against the CLI's cwd).
    state_root: Option<PathBuf>,
}

/// Outcome of stripping a command's declared control flags from its tail.
/// Each field is a routing destination from the generic `ControlFlagBinding`
/// vocabulary; the dispatcher applies these to the request body / display.
#[derive(Default)]
struct ResolvedControlFlags {
    async_launch: bool,
    stream: Option<bool>,
    debug_raw: bool,
    call_method: Option<String>,
    call_args: Option<Value>,
    state_root: Option<String>,
}

fn resolve_command_for_daemon(
    rest: &[String],
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
    default_project: Option<&Path>,
) -> Result<CliResolvedExecute, CliError> {
    resolve_command_for_daemon_with_commands(
        rest,
        &snapshot.commands,
        &snapshot.command_registration_policy.policy,
        default_project,
    )
}

fn resolve_command_for_daemon_with_commands(
    rest: &[String],
    commands: &[CommandDef],
    policy: &ryeos_runtime::CommandRegistrationPolicy,
    default_project: Option<&Path>,
) -> Result<CliResolvedExecute, CliError> {
    let registry =
        CommandRegistry::from_records(commands, policy).map_err(|error| CliError::Local {
            detail: format!("load verified node commands: {error:#}"),
        })?;
    let initial_match = registry.resolve(rest).map_err(|error| CliError::Local {
        detail: error.to_string(),
    })?;
    let tokens = if matches!(
        initial_match.command.dispatch,
        CommandDispatch::DirectExecuteItemRef { .. }
    ) {
        rest.to_vec()
    } else {
        canonicalize_tokens_with_commands_policy_and_project(
            rest,
            commands,
            policy,
            default_project,
        )?
    };
    let matched = registry.resolve(&tokens).map_err(|error| CliError::Local {
        detail: error.to_string(),
    })?;
    let mut tail = tokens[matched.consumed..].to_vec();
    let direct_execute = matches!(
        matched.command.dispatch,
        CommandDispatch::DirectExecuteItemRef { .. }
    );
    // Strip the command's DECLARED control flags (from data) and route them.
    // Commands that declare none get a no-op, so non-execute commands are
    // unaffected; the execute command declares --async/--method/--args/etc.
    let control = strip_declared_control_flags(&mut tail, &matched.command.control_flags)?;
    let item_ref = match &matched.command.dispatch {
        CommandDispatch::ExecuteRef { execute, .. } => execute.clone(),
        CommandDispatch::DirectExecuteItemRef { item_ref_arg, .. } => tail
            .first()
            .filter(|token| !token.starts_with('-'))
            .cloned()
            .ok_or_else(|| CliError::Local {
                detail: format!(
                    "command '{}' requires argument '{}'",
                    matched.command.name, item_ref_arg
                ),
            })?,
        CommandDispatch::Group | CommandDispatch::LocalHandler { .. } => {
            return Err(CliError::Local {
                detail: format!(
                    "command '{}' does not dispatch to an executable item ref",
                    matched.command.name
                ),
            });
        }
    };
    let parameter_tail = match matched.command.dispatch {
        CommandDispatch::DirectExecuteItemRef { .. } => &tail[1..],
        _ => &tail,
    };
    let mut parameters = bind_command_parameters_for_daemon(parameter_tail, &matched.command)?;
    let project_path = apply_project_policy(&matched.command, &mut parameters, default_project)?;
    // Absolutize (not canonicalize — the daemon creates it on demand) the
    // state-root override against the CLI's cwd, which the daemon cannot see.
    let state_root = control
        .state_root
        .map(|raw| -> Result<PathBuf, CliError> {
            let path = PathBuf::from(&raw);
            if path.is_absolute() {
                return Ok(path);
            }
            let cwd = std::env::current_dir()
                .map_err(|e| CliError::ProjectResolution(format!("cwd: {e}")))?;
            Ok(cwd.join(path))
        })
        .transpose()?;
    Ok(CliResolvedExecute {
        item_ref,
        parameters,
        project_path,
        async_launch: control.async_launch,
        direct_execute,
        stream: control.stream,
        debug_raw: control.debug_raw,
        call_method: control.call_method,
        call_args: control.call_args,
        state_root,
    })
}

/// Strip a command's DECLARED control flags (`command.control_flags`) from the
/// tail and route each into [`ResolvedControlFlags`] per its generic
/// `ControlFlagBinding`. No flag spellings or destinations are hardcoded — the
/// command data names the flags and the runtime knows only the binding
/// vocabulary. Presence flags accept `--flag`, `--flag=true`, `--flag=false`;
/// value flags (`call_method`/`call_args`) take `--flag value` or `--flag=v`.
fn strip_declared_control_flags(
    tail: &mut Vec<String>,
    declared: &[ryeos_runtime::CommandControlFlag],
) -> Result<ResolvedControlFlags, CliError> {
    use ryeos_runtime::ControlFlagBinding as Bind;
    let mut routes: std::collections::HashMap<&str, Bind> = std::collections::HashMap::new();
    for cf in declared {
        routes.insert(cf.flag.as_str(), cf.binding);
        for alias in &cf.aliases {
            routes.insert(alias.as_str(), cf.binding);
        }
    }

    let mut flags = ResolvedControlFlags::default();
    let mut out: Vec<String> = Vec::with_capacity(tail.len());
    let mut iter = std::mem::take(tail).into_iter();
    while let Some(token) = iter.next() {
        let Some(rest) = token.strip_prefix("--") else {
            out.push(token);
            continue;
        };
        let (name, inline) = match rest.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (rest, None),
        };
        let Some(&binding) = routes.get(name) else {
            out.push(token);
            continue;
        };
        if binding.takes_value() {
            let value = match inline {
                Some(value) => value,
                None => iter.next().ok_or_else(|| CliError::Local {
                    detail: format!("flag --{name} requires a value"),
                })?,
            };
            match binding {
                Bind::CallMethod => flags.call_method = Some(value),
                Bind::CallArgs => {
                    flags.call_args =
                        Some(serde_json::from_str(&value).map_err(|e| CliError::Local {
                            detail: format!("--{name} must be a JSON value: {e}"),
                        })?);
                }
                Bind::StateRoot => flags.state_root = Some(value),
                _ => {}
            }
        } else {
            let on = match inline.as_deref() {
                None | Some("true") => true,
                Some("false") => false,
                Some(other) => {
                    return Err(CliError::Local {
                        detail: format!("invalid value for --{name}: {other}"),
                    })
                }
            };
            if !on {
                continue;
            }
            match binding {
                Bind::LaunchModeAccepted => flags.async_launch = true,
                Bind::DebugRaw => flags.debug_raw = true,
                Bind::StreamOn => {
                    if flags.stream == Some(false) {
                        return Err(CliError::Local {
                            detail: "conflicting flags: stream on and off".into(),
                        });
                    }
                    flags.stream = Some(true);
                }
                Bind::StreamOff => {
                    if flags.stream == Some(true) {
                        return Err(CliError::Local {
                            detail: "conflicting flags: stream on and off".into(),
                        });
                    }
                    flags.stream = Some(false);
                }
                _ => {}
            }
        }
    }
    *tail = out;
    Ok(flags)
}

fn bind_command_parameters_for_daemon(
    tail: &[String],
    command: &CommandDef,
) -> Result<Value, CliError> {
    let direct_command;
    let command = if matches!(
        command.dispatch,
        CommandDispatch::DirectExecuteItemRef { .. }
    ) {
        direct_command = CommandDef {
            forms: Vec::new(),
            ..command.clone()
        };
        &direct_command
    } else {
        command
    };

    if let Some(params) = crate::arg_bind::bind_declared_shortcuts(tail, command)? {
        return Ok(params);
    }

    let binding = command.parameter_binding.as_ref();
    match binding.map(|binding| binding.mode).unwrap_or_default() {
        CommandParameterBindingMode::None
        | CommandParameterBindingMode::TailObject
        | CommandParameterBindingMode::SchemaObject => {
            ryeos_runtime::arg_binder::bind_argv_with_command(tail, Some(command))
                .map_err(CliError::ProjectResolution)
        }
    }
}

fn normalize_resolved_parameters(
    app_root: &Path,
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
    item_ref: &str,
    project_path: Option<&Path>,
    parameters: &mut Value,
) -> Result<(), CliError> {
    let Some(contract) = resolve_invocation_contract(app_root, snapshot, item_ref, project_path)?
    else {
        return Ok(());
    };
    let normalized = ryeos_runtime::arg_binder::normalize_params_with_contract(
        std::mem::take(parameters),
        Some(&contract),
    )
    .map_err(CliError::ProjectResolution)?;
    *parameters = normalized;
    Ok(())
}

fn resolve_invocation_contract(
    app_root: &Path,
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
    item_ref: &str,
    project_path: Option<&Path>,
) -> Result<Option<InvocationInputContract>, CliError> {
    let bundle_roots = crate::effective_metadata::snapshot_bundle_roots(snapshot);
    if bundle_roots.is_empty() {
        return Err(CliError::ProjectResolution(
            "resolve invocation schema: installed node config has no bundle roots".into(),
        ));
    }
    let engine = crate::effective_metadata::build_effective_item_engine(
        app_root,
        project_path,
        &bundle_roots,
    )
    .map_err(|err| CliError::ProjectResolution(format!("resolve invocation schema: {err:#}")))?;
    let Some(composed) = crate::effective_metadata::resolve_effective_composed_value(
        &engine,
        item_ref,
        project_path,
    )
    .map_err(|err| CliError::ProjectResolution(format!("resolve invocation schema: {err:#}")))?
    else {
        return Ok(None);
    };
    let Some(schema) = composed.get("schema") else {
        return Ok(None);
    };
    InvocationInputContract::from_lightweight_schema_value(schema)
        .map_err(CliError::ProjectResolution)
}

fn apply_project_policy(
    command: &CommandDef,
    parameters: &mut Value,
    default_project: Option<&Path>,
) -> Result<Option<PathBuf>, CliError> {
    let Some(project) = command.project.as_ref() else {
        return Ok(None);
    };

    let obj = parameters.as_object_mut().ok_or_else(|| {
        CliError::ProjectResolution("command parameters must be a JSON object".into())
    })?;
    let no_project = obj
        .remove("no_project")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if no_project && !project.no_project_flag {
        return Err(CliError::ProjectResolution(format!(
            "command '{}' does not accept --no-project",
            command.name
        )));
    }

    let mut project_path = obj
        .remove("project")
        .and_then(|value| value.as_str().map(PathBuf::from));
    if no_project && project_path.is_some() {
        return Err(CliError::ProjectResolution(
            "cannot pass both --no-project and --project: choose one".into(),
        ));
    }
    if project_path.is_none() && !no_project {
        project_path = default_project.map(PathBuf::from);
    }
    if let Some(path) = project_path.take() {
        project_path = Some(canonicalize_project_path(&path)?);
    }
    if project_path.is_none()
        && !no_project
        && project.default == CommandProjectDefault::DiscoverUpwardAi
    {
        project_path = discover_upward_ai_project()?;
    }

    if project.resolution == CommandProjectResolution::Required
        && project_path.is_none()
        && !no_project
    {
        return Err(CliError::ProjectResolution(format!(
            "command '{}' requires a project",
            command.name
        )));
    }

    if let (Some(bind_parameter), Some(path)) = (&project.bind_parameter, &project_path) {
        obj.insert(
            bind_parameter.clone(),
            Value::String(path.to_string_lossy().into_owned()),
        );
    }
    if no_project && project.bind_parameter.is_none() && !project.request_project_path {
        obj.insert("no_project".to_string(), Value::Bool(true));
    }

    if project.request_project_path {
        Ok(project_path)
    } else {
        Ok(None)
    }
}

fn canonicalize_project_path(path: &Path) -> Result<PathBuf, CliError> {
    let cwd =
        std::env::current_dir().map_err(|e| CliError::ProjectResolution(format!("cwd: {e}")))?;
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    abs.canonicalize().map_err(|e| {
        CliError::ProjectResolution(format!(
            "cannot canonicalize project path '{}': {e}. \
             Ensure the path exists and is accessible.",
            abs.display()
        ))
    })
}

fn discover_upward_ai_project() -> Result<Option<PathBuf>, CliError> {
    let cwd =
        std::env::current_dir().map_err(|e| CliError::ProjectResolution(format!("cwd: {e}")))?;
    for ancestor in cwd.ancestors() {
        if ancestor.join(ryeos_engine::AI_DIR).is_dir() {
            return ancestor.canonicalize().map(Some).map_err(|e| {
                CliError::ProjectResolution(format!(
                    "cannot canonicalize project path '{}': {e}",
                    ancestor.display()
                ))
            });
        }
    }
    Ok(None)
}

#[cfg(test)]
fn canonicalize_tokens_with_commands(
    rest: &[String],
    commands: &[CommandDef],
) -> Result<Vec<String>, CliError> {
    canonicalize_tokens_with_commands_and_project(rest, commands, None)
}

#[cfg(test)]
fn canonicalize_tokens_with_commands_and_project(
    rest: &[String],
    commands: &[CommandDef],
    default_project: Option<&std::path::Path>,
) -> Result<Vec<String>, CliError> {
    let policy = ryeos_runtime::CommandRegistrationPolicy::default();
    canonicalize_tokens_with_commands_policy_and_project(rest, commands, &policy, default_project)
}

fn canonicalize_tokens_with_commands_policy_and_project(
    rest: &[String],
    commands: &[CommandDef],
    policy: &ryeos_runtime::CommandRegistrationPolicy,
    default_project: Option<&std::path::Path>,
) -> Result<Vec<String>, CliError> {
    let registry =
        CommandRegistry::from_records(commands, policy).map_err(|error| CliError::Local {
            detail: format!("load verified node commands: {error:#}"),
        })?;
    let Ok(matched) = registry.resolve(rest) else {
        return Ok(rest.to_vec());
    };
    let tail = &rest[matched.consumed..];
    let resolution = command_project_resolution(&matched.command);

    if matched.command.forms.is_empty() && resolution == CommandProjectResolution::None {
        // This command declares no args and takes no project, but a global
        // `-p/--project/--no-project` may still be placed *after* the verb
        // (e.g. `scheduler list -p /path`). Those are the project selector, not
        // arguments to this command — strip them so they never leak to the
        // handler as stray positionals. Accepting them here (rather than only
        // before the verb) is what makes `-p` consistent around the verb.
        let cleaned_tail = strip_project_control_flags(tail);
        let mut out = rest[..matched.consumed].to_vec();
        out.extend(cleaned_tail);
        return Ok(out);
    }

    let bound = ryeos_runtime::arg_binder::bind_argv_with_command(tail, Some(&matched.command))
        .map_err(CliError::ProjectResolution)?;
    let mut canonical_tail = params_to_tail(&bound);

    match resolution {
        CommandProjectResolution::None => {}
        CommandProjectResolution::Optional => {
            canonical_tail = crate::project_resolve::rewrite_project_tail_with_default(
                &canonical_tail,
                default_project,
            )?;
        }
        CommandProjectResolution::Required => {
            if canonical_tail.iter().any(|t| t == "--no-project") {
                return Err(CliError::ProjectResolution(
                    "this command requires a project; do not pass --no-project".into(),
                ));
            }
            canonical_tail = crate::project_resolve::rewrite_project_tail_with_default(
                &canonical_tail,
                default_project,
            )?;
            if canonical_tail.iter().any(|t| t == "--no-project") {
                return Err(CliError::ProjectResolution(
                    "this command requires a project; run it from a directory containing .ai/ \
                     or pass --project <path>"
                        .into(),
                ));
            }
        }
    }

    let mut out = rest[..matched.consumed].to_vec();
    out.extend(canonical_tail);
    Ok(out)
}

/// Remove `-p`/`--project`/`--project=…`/`-p=…`/`--no-project` (and the value
/// following the bare `-p`/`--project` form) from a command tail. Used for
/// commands that take no project, so a project selector placed after the verb
/// is accepted and dropped rather than leaking to the handler.
fn strip_project_control_flags(tail: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(tail.len());
    let mut i = 0;
    while i < tail.len() {
        let tok = &tail[i];
        if tok == "--no-project" || tok.starts_with("--project=") || tok.starts_with("-p=") {
            i += 1;
            continue;
        }
        if tok == "--project" || tok == "-p" {
            // Skip the flag and its value (if a value is present).
            i += if i + 1 < tail.len() { 2 } else { 1 };
            continue;
        }
        out.push(tok.clone());
        i += 1;
    }
    out
}

fn command_project_resolution(command: &CommandDef) -> CommandProjectResolution {
    command
        .project
        .as_ref()
        .map(|p| p.resolution)
        .unwrap_or_default()
}

fn params_to_tail(params: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = params.as_object() else {
        return out;
    };
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    for key in keys {
        emit_param(&mut out, key, &obj[key]);
    }
    out
}

fn emit_param(out: &mut Vec<String>, key: &str, value: &Value) {
    match value {
        Value::Bool(true) => out.push(format!("--{}", key.replace('_', "-"))),
        Value::Bool(false) | Value::Null => {}
        Value::Array(values) => {
            for v in values {
                emit_param(out, key, v);
            }
        }
        other => {
            out.push(format!("--{}", key.replace('_', "-")));
            out.push(match other {
                Value::String(s) => s.clone(),
                _ => other.to_string(),
            });
        }
    }
}

/// POST a JSON body to a daemon execute route and return the response.
async fn post_to_daemon(
    app_root: &std::path::Path,
    route_path: &str,
    body: &Value,
) -> Result<Value, CliError> {
    lifecycle_preflight(app_root).await?;
    let daemon_url = crate::transport::http::resolve_daemon_url(app_root).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;

    // Discover the daemon's principal_id (audience) and the effective base URL
    // after any redirects. Signed dispatch targets that base directly — never
    // a redirect, which could downgrade the POST and break the signature.
    let discovered = crate::transport::discovery::discover_audience(&daemon_url).await?;

    let body_bytes = serde_json::to_vec(body).expect("infallible: Value serialization");
    let headers = signer.sign("POST", route_path, &body_bytes, &discovered.principal_id)?;

    let url = format!("{}{}", discovered.effective_base_url, route_path);
    let payload = crate::transport::http::post_json(&url, &headers, &body_bytes).await?;
    Ok(payload)
}

/// POST to the gateway `/execute/stream`, render the live execution log to
/// stdout, then (on success) fetch and print the final thread result so the
/// terminal path keeps parity with `/execute`. Same signing/audience flow as
/// [`post_to_daemon`]. Returns a non-zero error on a failing/errored run.
async fn post_to_daemon_streaming(
    app_root: &std::path::Path,
    body: &Value,
) -> Result<(), CliError> {
    use crate::exec_stream::StreamOutcome;

    lifecycle_preflight(app_root).await?;
    let daemon_url = crate::transport::http::resolve_daemon_url(app_root).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;
    let discovered = crate::transport::discovery::discover_audience(&daemon_url).await?;

    let route_path = "/execute/stream";
    let body_bytes = serde_json::to_vec(body).expect("infallible: Value serialization");
    let headers = signer.sign("POST", route_path, &body_bytes, &discovered.principal_id)?;
    let url = format!("{}{route_path}", discovered.effective_base_url);

    // Track the explicit terminal outcome: `Some(Ok)` success, `Some(Err)`
    // failure, `None` means the stream ended without any terminal event.
    let mut terminal: Option<Result<(), String>> = None;
    let mut thread_id: Option<String> = None;
    crate::transport::http::post_json_streaming(&url, &headers, &body_bytes, |ev| {
        if ev.event == "stream_started" {
            if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                thread_id = v
                    .get("thread_id")
                    .and_then(|t| t.as_str())
                    .map(String::from);
            }
        }
        match crate::exec_stream::render_event(ev) {
            StreamOutcome::Continue => false,
            StreamOutcome::Done => {
                terminal = Some(Ok(()));
                true
            }
            StreamOutcome::Failed(detail) => {
                terminal = Some(Err(detail));
                true
            }
        }
    })
    .await?;

    match terminal {
        Some(Err(detail)) => return Err(CliError::Local { detail }),
        None => {
            return Err(CliError::Local {
                detail: "execute stream ended before a terminal event".into(),
            })
        }
        Some(Ok(())) => {}
    }

    // Parity with `/execute`: print the final result after a successful run.
    // Missing thread_id or a failed fetch is an error, not a silent exit-0.
    let tid = thread_id.ok_or_else(|| CliError::Local {
        detail: "execute stream completed but stream_started carried no thread_id".into(),
    })?;
    let result_path = format!("/threads/{tid}");
    let headers = signer.sign("GET", &result_path, &[], &discovered.principal_id)?;
    let url = format!("{}{result_path}", discovered.effective_base_url);
    let payload = crate::transport::http::get_json(&url, &headers)
        .await
        .map_err(|e| CliError::Local {
            detail: format!(
                "execute stream completed but final result fetch failed for {tid}: {e}"
            ),
        })?;
    print_result(thread_get_payload_to_execute_result(payload));
    Ok(())
}

/// Normalize a `GET /threads/{id}` payload into the `/execute` result envelope
/// (`{ "result": { outcome_code, result, error, artifacts } }`) so the streamed
/// TTY path prints the same shape `/execute` does — including `artifacts`, which
/// `threads.get` returns as a sibling of `result` (a `ThreadResultRecord`).
fn thread_get_payload_to_execute_result(payload: Value) -> Value {
    let result = payload.get("result").cloned().unwrap_or(Value::Null);
    let artifacts = payload
        .get("artifacts")
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]));
    serde_json::json!({
        "result": {
            "outcome_code": result.get("outcome_code").cloned().unwrap_or(Value::Null),
            "result": result.get("result").cloned().unwrap_or(Value::Null),
            "error": result.get("error").cloned().unwrap_or(Value::Null),
            "artifacts": artifacts,
        }
    })
}

async fn lifecycle_preflight(app_root: &std::path::Path) -> Result<(), CliError> {
    // A deliberate remote override is still valid for normal daemon-backed
    // dispatch. Lifecycle reads/mutations themselves ignore this env var.
    if std::env::var_os("RYEOSD_URL").is_some() {
        return Ok(());
    }

    let env = ryeos_node::LocalLifecycleEnv::load(Some(app_root.to_path_buf())).map_err(|e| {
        CliError::Local {
            detail: format!("resolve local node lifecycle env: {e:#}"),
        }
    })?;
    let controller = ryeos_node::LifecycleController::from_env(env);

    // A busy daemon (e.g. absorbing a launch burst) times out the status
    // probe transiently; that congestion is self-clearing, so give it a few
    // bounded retries before failing the launch. Every other outcome is
    // settled on the first probe.
    const BUSY_ATTEMPTS: u32 = 3;
    let mut busy_message = String::new();
    for attempt in 1..=BUSY_ATTEMPTS {
        match controller.status().await.map_err(|e| CliError::Local {
            detail: format!("read lifecycle status: {e:#}"),
        })? {
            ryeos_node::LifecycleStatus::Running { .. } => return Ok(()),
            ryeos_node::LifecycleStatus::NotInitialized { diagnostics } => {
                return Err(CliError::Local {
                    detail: format!(
                        "RyeOS is not initialized. Run: ryeos init\nDetail: {}",
                        diagnostics.message
                    ),
                })
            }
            ryeos_node::LifecycleStatus::Stopped { .. } => {
                return Err(CliError::Local {
                    detail: "RyeOS is initialized but not running. Run: ryeos start".into(),
                })
            }
            ryeos_node::LifecycleStatus::Stale { diagnostics, .. } => {
                return Err(CliError::Local {
                    detail: format!(
                        "RyeOS daemon metadata is stale: {}\nRun: ryeos start",
                        diagnostics.message
                    ),
                })
            }
            ryeos_node::LifecycleStatus::Starting { pid, .. } => {
                // Boot (projection catch-up after a deploy) runs for minutes,
                // far past the busy-retry budget — settle immediately with
                // the actual remediation: wait, don't start a second daemon.
                return Err(CliError::Local {
                    detail: format!(
                        "RyeOS daemon (pid {pid}) is starting up; its control socket is \
                         not available yet — wait for `ryeos node status` to report \
                         running, then retry"
                    ),
                });
            }
            ryeos_node::LifecycleStatus::Unresponsive { diagnostics, .. } => {
                busy_message = diagnostics.message;
                if attempt < BUSY_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                }
            }
        }
    }
    Err(CliError::Local {
        detail: format!(
            "RyeOS daemon is running but did not answer the control probe within the \
             timeout ({busy_message}); likely busy — retry shortly rather than starting \
             a replacement daemon"
        ),
    })
}

fn print_result(payload: serde_json::Value) {
    // A state-root override echoes both roots as top-level `execution`
    // diagnostics; surface them on stderr so stdout stays the bare result
    // for scripts.
    if let Some(execution) = payload.get("execution") {
        if let (Some(source), Some(state)) = (
            execution.get("source_root").and_then(|v| v.as_str()),
            execution.get("state_root").and_then(|v| v.as_str()),
        ) {
            eprintln!("source_root: {source}");
            eprintln!("state_root:  {state}");
        }
    }
    let result = payload.get("result").cloned().unwrap_or(payload);
    let pretty = serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
    println!("{pretty}");
}

fn discover_app_root() -> PathBuf {
    if let Ok(p) = std::env::var("RYEOS_APP_ROOT") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .expect("could not determine XDG data directory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::CommandArgumentKind;

    #[test]
    fn strip_execute_control_flags_parses_stream_prefs() {
        let cf = execute_control_flags();
        let mut tail = vec![
            "item:x".to_string(),
            "--async".to_string(),
            "--no-stream".to_string(),
            "--keep".to_string(),
        ];
        let flags = strip_declared_control_flags(&mut tail, &cf).unwrap();
        assert!(flags.async_launch);
        assert_eq!(flags.stream, Some(false));
        // Non-control tokens survive untouched.
        assert_eq!(tail, vec!["item:x".to_string(), "--keep".to_string()]);

        // --json is an alias for force-off.
        let mut json_tail = vec!["--json".to_string()];
        assert_eq!(
            strip_declared_control_flags(&mut json_tail, &cf)
                .unwrap()
                .stream,
            Some(false)
        );

        // --stream forces on.
        let mut stream_tail = vec!["--stream".to_string()];
        assert_eq!(
            strip_declared_control_flags(&mut stream_tail, &cf)
                .unwrap()
                .stream,
            Some(true)
        );

        // Undeclared flags pass through untouched; nothing set.
        let mut plain = vec!["--other".to_string()];
        let flags = strip_declared_control_flags(&mut plain, &cf).unwrap();
        assert_eq!(flags.stream, None);
        assert!(!flags.async_launch);
        assert_eq!(plain, vec!["--other".to_string()]);

        // Conflicting flags error.
        let mut conflict = vec!["--stream".to_string(), "--no-stream".to_string()];
        assert!(strip_declared_control_flags(&mut conflict, &cf).is_err());

        // Value flags route to call.method / call.args (both `--flag value`
        // and `--flag=value` forms).
        let mut method_tail = vec![
            "knowledge:notes".to_string(),
            "--method".to_string(),
            "query".to_string(),
            "--args".to_string(),
            r#"{"query":"needle"}"#.to_string(),
        ];
        let flags = strip_declared_control_flags(&mut method_tail, &cf).unwrap();
        assert_eq!(flags.call_method.as_deref(), Some("query"));
        assert_eq!(
            flags.call_args,
            Some(serde_json::json!({"query": "needle"}))
        );
        assert_eq!(method_tail, vec!["knowledge:notes".to_string()]);

        // A value flag with no value is an error.
        let mut dangling = vec!["--method".to_string()];
        assert!(strip_declared_control_flags(&mut dangling, &cf).is_err());

        // Malformed JSON for --args is an error.
        let mut bad_args = vec!["--args".to_string(), "not-json".to_string()];
        assert!(strip_declared_control_flags(&mut bad_args, &cf).is_err());
    }

    #[test]
    fn thread_get_payload_normalizes_to_execute_result_shape() {
        let threads_get = serde_json::json!({
            "thread": {"thread_id": "T-1"},
            "result": {
                "outcome_code": "success",
                "result": {"text": "hi"},
                "error": null,
                "metadata": {}
            },
            "artifacts": [{"uri": "cas://x"}],
            "facets": []
        });
        let normalized = thread_get_payload_to_execute_result(threads_get);
        let result = normalized.get("result").expect("result envelope");
        assert_eq!(result.get("outcome_code").unwrap(), "success");
        assert_eq!(
            result.get("result").unwrap(),
            &serde_json::json!({"text": "hi"})
        );
        // artifacts (a sibling in threads.get) are carried into the envelope.
        assert_eq!(
            result.get("artifacts").unwrap(),
            &serde_json::json!([{"uri": "cas://x"}])
        );
        assert!(result.get("error").unwrap().is_null());
    }

    fn with_app_root<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::test_env::lock();
        let saved = std::env::var_os("RYEOS_APP_ROOT");
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(ryeos_engine::AI_DIR)).unwrap();
        std::env::set_var("RYEOS_APP_ROOT", tmp.path());
        let result = f();
        if let Some(v) = saved {
            std::env::set_var("RYEOS_APP_ROOT", v);
        } else {
            std::env::remove_var("RYEOS_APP_ROOT");
        }
        result
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn command(
        tokens: &[&str],
        forms: Vec<Vec<(&str, CommandArgumentKind)>>,
        project_resolution: CommandProjectResolution,
    ) -> CommandDef {
        CommandDef {
            name: tokens.join("-"),
            tokens: s(tokens),
            description: String::new(),
            aliases: Vec::new(),
            help: None,
            arguments: Vec::new(),
            forms: forms
                .into_iter()
                .map(|slots| ryeos_runtime::CommandArgumentForm {
                    slots: slots
                        .into_iter()
                        .map(|(field, matcher)| ryeos_runtime::CommandArgumentSlot {
                            field: field.to_string(),
                            matcher,
                        })
                        .collect(),
                })
                .collect(),
            defaults: Default::default(),
            parameter_binding: None,
            control_flags: Vec::new(),
            project: Some(ryeos_runtime::CommandProjectPolicy {
                resolution: project_resolution,
                default: ryeos_runtime::CommandProjectDefault::None,
                no_project_flag: false,
                request_project_path: false,
                bind_parameter: None,
            }),
            dispatch: ryeos_runtime::CommandDispatch::ExecuteRef {
                execute: "tool:test/command".into(),
                availability: ryeos_runtime::CommandAvailability::Auto,
            },
            source_file: PathBuf::new(),
            provenance: ryeos_runtime::CommandProvenance::default(),
        }
    }

    /// Guard: `--input` must NOT short-circuit binding unless the
    /// command's parameter_binding declares an input flag. The old
    /// offline path honored it unconditionally; both paths now share
    /// `arg_bind::bind_declared_shortcuts`, which is descriptor-gated.
    #[test]
    fn undeclared_input_flag_does_not_short_circuit_binding() {
        let cmd = command(&["custom"], vec![], CommandProjectResolution::None);
        assert!(cmd.parameter_binding.is_none());
        let tail = s(&["--input", "/nonexistent/params.json"]);
        let result = crate::arg_bind::bind_declared_shortcuts(&tail, &cmd)
            .expect("undeclared --input must not be parsed at all");
        assert!(
            result.is_none(),
            "--input short-circuited without a declared input flag"
        );
    }

    #[test]
    fn strip_project_control_flags_removes_all_forms() {
        // bare `-p`/`--project` consume their following value
        assert_eq!(
            super::strip_project_control_flags(&s(&["-p", "/proj", "extra"])),
            s(&["extra"])
        );
        assert!(super::strip_project_control_flags(&s(&["--project", "/proj"])).is_empty());
        // `=` forms and `--no-project` are bare; other tokens pass through
        assert_eq!(
            super::strip_project_control_flags(&s(&[
                "--project=/p",
                "x",
                "-p=/q",
                "--no-project",
                "y"
            ])),
            s(&["x", "y"])
        );
        // trailing bare `-p` with no value
        assert!(super::strip_project_control_flags(&s(&["-p"])).is_empty());
    }

    #[test]
    fn no_project_command_strips_trailing_dash_p() {
        // A forms-empty, no-project command (e.g. `scheduler list`) must accept
        // `-p <path>` after the verb and not forward it to the handler.
        let cmd = command(
            &["scheduler", "list"],
            vec![],
            CommandProjectResolution::None,
        );
        let rest = s(&["scheduler", "list", "-p", "/data/projects/snap-track"]);
        let out = canonicalize_tokens_with_commands(&rest, std::slice::from_ref(&cmd))
            .expect("dispatch should accept -p after the verb");
        assert_eq!(out, s(&["scheduler", "list"]));
    }

    /// The execute command's control flags, mirroring
    /// `bundles/core/.ai/node/commands/execute.yaml`. Kept in sync with that
    /// data file; both declare the same flag→binding routing.
    fn execute_control_flags() -> Vec<ryeos_runtime::CommandControlFlag> {
        use ryeos_runtime::{CommandControlFlag as F, ControlFlagBinding as B};
        vec![
            F {
                flag: "async".into(),
                help: "Accepted/background launch; returns a thread_id".into(),
                binding: B::LaunchModeAccepted,
                aliases: vec![],
            },
            F {
                flag: "stream".into(),
                help: "Force the live execution stream on".into(),
                binding: B::StreamOn,
                aliases: vec![],
            },
            F {
                flag: "no-stream".into(),
                help: "Print the buffered JSON result instead of streaming".into(),
                binding: B::StreamOff,
                aliases: vec!["json".into()],
            },
            F {
                flag: "debug-raw".into(),
                help: "Attach a debug block to the result".into(),
                binding: B::DebugRaw,
                aliases: vec![],
            },
            F {
                flag: "method".into(),
                help: "Method selector for method-dispatch kinds (call.method)".into(),
                binding: B::CallMethod,
                aliases: vec![],
            },
            F {
                flag: "args".into(),
                help: "Method args as a JSON object (call.args)".into(),
                binding: B::CallArgs,
                aliases: vec![],
            },
        ]
    }

    fn direct_execute_command() -> CommandDef {
        CommandDef {
            name: "execute".into(),
            tokens: s(&["execute"]),
            description: "Execute an item".into(),
            aliases: Vec::new(),
            help: None,
            arguments: Vec::new(),
            forms: vec![ryeos_runtime::CommandArgumentForm {
                slots: vec![ryeos_runtime::CommandArgumentSlot {
                    field: "item_ref".into(),
                    matcher: ryeos_runtime::CommandArgumentKind::CanonicalRef,
                }],
            }],
            defaults: Default::default(),
            parameter_binding: Some(ryeos_runtime::CommandParameterBinding {
                mode: ryeos_runtime::CommandParameterBindingMode::TailObject,
                input_flag: Some("input".into()),
                single_json_object_arg: true,
                flag_key_normalization: ryeos_runtime::FlagKeyNormalization::HyphenToUnderscore,
            }),
            control_flags: execute_control_flags(),
            project: Some(ryeos_runtime::CommandProjectPolicy {
                resolution: ryeos_runtime::CommandProjectResolution::Optional,
                default: ryeos_runtime::CommandProjectDefault::DiscoverUpwardAi,
                no_project_flag: true,
                request_project_path: true,
                bind_parameter: None,
            }),
            dispatch: ryeos_runtime::CommandDispatch::DirectExecuteItemRef {
                item_ref_arg: "item_ref".into(),
                availability: ryeos_runtime::CommandAvailability::Both,
            },
            source_file: PathBuf::new(),
            provenance: ryeos_runtime::CommandProvenance::default(),
        }
    }

    #[test]
    fn direct_execute_async_before_item_is_control_flag() {
        let commands = vec![direct_execute_command()];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&["execute", "--async", "tool:test/run", "--batch-size", "50"]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            None,
        )
        .unwrap();

        assert!(resolved.async_launch);
        assert_eq!(resolved.item_ref, "tool:test/run");
        assert_eq!(resolved.parameters["batch_size"], serde_json::json!("50"));
        assert!(resolved.parameters.get("async").is_none());
    }

    #[test]
    fn direct_execute_async_after_item_is_control_flag() {
        let commands = vec![direct_execute_command()];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&["execute", "tool:test/run", "--async", "--provider", "zen"]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            None,
        )
        .unwrap();

        assert!(resolved.async_launch);
        assert_eq!(resolved.item_ref, "tool:test/run");
        assert_eq!(resolved.parameters["provider"], serde_json::json!("zen"));
        assert!(resolved.parameters.get("async").is_none());
    }

    #[test]
    fn direct_execute_without_async_uses_foreground_execute() {
        let commands = vec![direct_execute_command()];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&["execute", "tool:test/run", "--provider", "zen"]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            None,
        )
        .unwrap();

        assert!(!resolved.async_launch);
        assert_eq!(resolved.item_ref, "tool:test/run");
        assert_eq!(resolved.parameters["provider"], serde_json::json!("zen"));
    }

    #[test]
    fn direct_execute_uses_global_project_default() {
        let tmp = tempfile::tempdir().unwrap();
        let commands = vec![direct_execute_command()];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&["execute", "tool:test/run"]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            Some(tmp.path()),
        )
        .unwrap();

        assert_eq!(resolved.item_ref, "tool:test/run");
        assert_eq!(resolved.project_path.as_deref(), Some(tmp.path()));
    }

    #[test]
    fn direct_execute_canonicalizes_relative_global_project_default() {
        let _g = crate::test_env::lock();
        let saved = std::env::current_dir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let commands = vec![direct_execute_command()];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&["execute", "tool:test/run"]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            Some(Path::new(".")),
        )
        .unwrap();
        std::env::set_current_dir(saved).unwrap();

        assert_eq!(
            resolved.project_path,
            Some(tmp.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn direct_execute_tail_project_wins_over_global_project_default() {
        let global = tempfile::tempdir().unwrap();
        let tail = tempfile::tempdir().unwrap();
        let commands = vec![direct_execute_command()];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&[
                "execute",
                "tool:test/run",
                "--project",
                &tail.path().to_string_lossy(),
            ]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            Some(global.path()),
        )
        .unwrap();

        assert_eq!(resolved.project_path.as_deref(), Some(tail.path()));
    }

    #[test]
    fn direct_execute_input_preserves_tail_project_flag() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("params.json");
        std::fs::write(&input, r#"{"provider":"zen"}"#).unwrap();
        let commands = vec![direct_execute_command()];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&[
                "execute",
                "tool:test/run",
                "--project",
                &dir.path().to_string_lossy(),
                "--input",
                &input.to_string_lossy(),
            ]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            None,
        )
        .unwrap();

        assert_eq!(resolved.project_path.as_deref(), Some(dir.path()));
        assert_eq!(resolved.parameters["provider"], serde_json::json!("zen"));
        assert!(resolved.parameters.get("project").is_none());
    }

    #[test]
    fn direct_execute_input_canonicalizes_relative_tail_project_flag() {
        let _g = crate::test_env::lock();
        let saved = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let input = dir.path().join("params.json");
        std::fs::write(&input, r#"{"provider":"zen"}"#).unwrap();
        let commands = vec![direct_execute_command()];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&[
                "execute",
                "tool:test/run",
                "--project",
                ".",
                "--input",
                &input.to_string_lossy(),
            ]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            None,
        )
        .unwrap();
        std::env::set_current_dir(saved).unwrap();

        assert_eq!(
            resolved.project_path,
            Some(dir.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn direct_execute_input_rejects_tail_project_without_value() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("params.json");
        std::fs::write(&input, r#"{"provider":"zen"}"#).unwrap();
        let commands = vec![direct_execute_command()];
        let result = resolve_command_for_daemon_with_commands(
            &s(&[
                "execute",
                "tool:test/run",
                "--input",
                &input.to_string_lossy(),
                "--project",
            ]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            None,
        );
        let Err(err) = result else {
            panic!("expected --project without value to fail");
        };

        assert!(format!("{err}").contains("--project requires a value"));
    }

    #[test]
    fn direct_execute_input_no_project_suppresses_global_project_default() {
        let global = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("params.json");
        std::fs::write(&input, r#"{"provider":"zen"}"#).unwrap();
        let commands = vec![direct_execute_command()];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&[
                "execute",
                "tool:test/run",
                "--input",
                &input.to_string_lossy(),
                "--no-project",
            ]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            Some(global.path()),
        )
        .unwrap();

        assert_eq!(resolved.project_path, None);
        assert!(
            resolved.parameters.get("no_project").is_none(),
            "--no-project is execute control data, not a service parameter"
        );
    }

    #[test]
    fn direct_execute_async_false_keeps_foreground_execute() {
        let commands = vec![direct_execute_command()];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&["execute", "--async=false", "tool:test/run"]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            None,
        )
        .unwrap();

        assert!(!resolved.async_launch);
        assert_eq!(resolved.item_ref, "tool:test/run");
        assert!(resolved.parameters.get("async").is_none());
    }

    #[test]
    fn direct_execute_async_invalid_value_errors() {
        let commands = vec![direct_execute_command()];
        let result = resolve_command_for_daemon_with_commands(
            &s(&["execute", "--async=maybe", "tool:test/run"]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            None,
        );

        match result {
            Ok(_) => panic!("invalid --async value unexpectedly succeeded"),
            Err(err) => {
                assert!(format!("{err}").contains("invalid value for --async"));
            }
        }
    }

    #[test]
    fn non_direct_command_async_remains_parameter() {
        let commands = vec![command(
            &["demo"],
            Vec::new(),
            CommandProjectResolution::None,
        )];
        let resolved = resolve_command_for_daemon_with_commands(
            &s(&["demo", "--async"]),
            &commands,
            &ryeos_runtime::CommandRegistrationPolicy::default(),
            None,
        )
        .unwrap();

        assert!(!resolved.async_launch);
        assert_eq!(resolved.item_ref, "tool:test/command");
        assert_eq!(resolved.parameters["async"], serde_json::json!(true));
    }

    #[test]
    fn remote_threads_positional_remote_is_normalized() {
        let commands = vec![command(
            &["remote", "threads"],
            vec![vec![("remote", CommandArgumentKind::String)]],
            CommandProjectResolution::None,
        )];
        let out =
            canonicalize_tokens_with_commands(&s(&["remote", "threads", "railway"]), &commands)
                .unwrap();
        assert_eq!(out, s(&["remote", "threads", "--remote", "railway"]));
    }

    #[test]
    fn remote_project_status_positional_remote_is_normalized() {
        let commands = vec![command(
            &["remote", "project-status"],
            vec![vec![("remote", CommandArgumentKind::String)]],
            CommandProjectResolution::Required,
        )];
        let out = canonicalize_tokens_with_commands(
            &s(&["remote", "project-status", "railway", "--project", "/tmp"]),
            &commands,
        )
        .unwrap();
        assert_eq!(
            out,
            s(&[
                "remote",
                "project-status",
                "--remote",
                "railway",
                "--project",
                "/tmp",
            ])
        );
    }

    #[test]
    fn remote_bind_project_accepts_project_after_command() {
        let tmp = tempfile::tempdir().unwrap();
        let commands = vec![command(
            &["remote", "bind-project"],
            vec![vec![("remote", CommandArgumentKind::String)]],
            CommandProjectResolution::Required,
        )];
        let out = canonicalize_tokens_with_commands(
            &s(&[
                "remote",
                "bind-project",
                "prod",
                "--project",
                &tmp.path().to_string_lossy(),
                "--remote-project",
                "/data/app",
                "--sync-scope",
                "ai_only",
            ]),
            &commands,
        )
        .unwrap();
        assert_eq!(
            out[0..4],
            s(&["remote", "bind-project", "--remote", "prod"])
        );
        assert!(out
            .windows(2)
            .any(|w| w[0] == "--project" && w[1] == tmp.path().to_string_lossy()));
    }

    #[test]
    fn remote_doctor_accepts_optional_project_after_command() {
        with_app_root(|| {
            let tmp = tempfile::tempdir().unwrap();
            let commands = vec![command(
                &["remote", "doctor"],
                vec![vec![("remote", CommandArgumentKind::String)]],
                CommandProjectResolution::Optional,
            )];
            let out = canonicalize_tokens_with_commands(
                &s(&[
                    "remote",
                    "doctor",
                    "prod",
                    "--project",
                    &tmp.path().to_string_lossy(),
                ]),
                &commands,
            )
            .unwrap();
            assert_eq!(out[0..4], s(&["remote", "doctor", "--remote", "prod"]));
            assert!(out
                .windows(2)
                .any(|w| w[0] == "--project" && w[1] == tmp.path().to_string_lossy()));
        });
    }

    #[test]
    fn project_aware_alias_uses_global_project_default() {
        let tmp = tempfile::tempdir().unwrap();
        let commands = vec![command(
            &["remote", "bind-project"],
            vec![vec![("remote", CommandArgumentKind::String)]],
            CommandProjectResolution::Required,
        )];
        let out = canonicalize_tokens_with_commands_and_project(
            &s(&[
                "remote",
                "bind-project",
                "prod",
                "--remote-project",
                "/data/app",
            ]),
            &commands,
            Some(tmp.path()),
        )
        .unwrap();
        assert!(out.windows(2).any(|w| {
            w[0] == "--project" && w[1] == tmp.path().canonicalize().unwrap().to_string_lossy()
        }));
    }

    #[test]
    fn remote_execute_remote_then_item_is_normalized() {
        with_app_root(|| {
            let commands = vec![command(
                &["remote", "execute"],
                vec![
                    vec![
                        ("remote", CommandArgumentKind::String),
                        ("item_ref", CommandArgumentKind::CanonicalRef),
                    ],
                    vec![("item_ref", CommandArgumentKind::CanonicalRef)],
                ],
                CommandProjectResolution::Optional,
            )];
            let out = canonicalize_tokens_with_commands(
                &s(&[
                    "remote",
                    "execute",
                    "railway",
                    "service:health/status",
                    "--no-project",
                ]),
                &commands,
            )
            .unwrap();
            assert_eq!(
                out,
                s(&[
                    "remote",
                    "execute",
                    "--item-ref",
                    "service:health/status",
                    "--remote",
                    "railway",
                    "--no-project",
                ])
            );
        });
    }

    #[test]
    fn remote_execute_item_only_is_left_for_default_remote() {
        with_app_root(|| {
            let commands = vec![command(
                &["remote", "execute"],
                vec![
                    vec![
                        ("remote", CommandArgumentKind::String),
                        ("item_ref", CommandArgumentKind::CanonicalRef),
                    ],
                    vec![("item_ref", CommandArgumentKind::CanonicalRef)],
                ],
                CommandProjectResolution::Optional,
            )];
            let input = s(&["remote", "execute", "service:health/status", "--no-project"]);
            let out = canonicalize_tokens_with_commands(&input, &commands).unwrap();
            assert_eq!(
                out,
                s(&[
                    "remote",
                    "execute",
                    "--item-ref",
                    "service:health/status",
                    "--no-project",
                ])
            );
        });
    }

    #[test]
    fn explicit_remote_forms_are_not_rewritten() {
        let commands = vec![command(
            &["remote", "threads"],
            vec![vec![("remote", CommandArgumentKind::String)]],
            CommandProjectResolution::None,
        )];
        let flag = s(&["remote", "threads", "--remote", "railway"]);
        assert_eq!(
            canonicalize_tokens_with_commands(&flag, &commands).unwrap(),
            flag
        );

        let equals_flag = s(&["remote", "threads", "--remote=railway"]);
        assert_eq!(
            canonicalize_tokens_with_commands(&equals_flag, &commands).unwrap(),
            s(&["remote", "threads", "--remote", "railway"])
        );
    }

    #[test]
    fn aliases_without_metadata_preserve_positional_tail() {
        let commands = vec![command(
            &["status"],
            Vec::new(),
            CommandProjectResolution::None,
        )];
        let input = s(&["status", "extra-arg"]);
        let out = canonicalize_tokens_with_commands(&input, &commands).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn stream_descriptor_path_extracts_followable_sse_get() {
        // Service envelope: descriptor nested under `result`. No `follow` field
        // → defaults to the single-thread completion policy (braid = false).
        let resp = serde_json::json!({
            "thread": { "thread_id": "audit-1" },
            "result": { "stream": {
                "transport": "sse",
                "method": "GET",
                "path": "/threads/abc/events/stream"
            }}
        });
        assert_eq!(
            stream_descriptor_path(&resp).unwrap(),
            Some(("/threads/abc/events/stream".to_string(), false))
        );
    }

    #[test]
    fn stream_descriptor_path_honors_follow_mode() {
        // follow=chain → braid follow (true); follow=thread → single (false).
        let chain = serde_json::json!({
            "result": { "stream": {
                "transport": "sse",
                "method": "GET",
                "path": "/chains/root/events/stream",
                "follow": "chain"
            }}
        });
        assert_eq!(
            stream_descriptor_path(&chain).unwrap(),
            Some(("/chains/root/events/stream".to_string(), true))
        );
        let thread = serde_json::json!({
            "result": { "stream": {
                "transport": "sse",
                "method": "GET",
                "path": "/threads/abc/events/stream",
                "follow": "thread"
            }}
        });
        assert_eq!(
            stream_descriptor_path(&thread).unwrap(),
            Some(("/threads/abc/events/stream".to_string(), false))
        );
        // Unknown follow mode errors loudly.
        let bad = serde_json::json!({
            "result": { "stream": {
                "transport": "sse",
                "method": "GET",
                "path": "/x/stream",
                "follow": "everything"
            }}
        });
        assert!(stream_descriptor_path(&bad).is_err());
    }

    #[test]
    fn stream_descriptor_path_none_for_ordinary_result() {
        // A normal buffered result has no descriptor → Ok(None), prints as JSON.
        let normal = serde_json::json!({ "result": { "outcome_code": "success" } });
        assert!(stream_descriptor_path(&normal).unwrap().is_none());
        // A descriptor is only read from `result.stream`, never top-level.
        let top = serde_json::json!({
            "stream": { "transport": "sse", "method": "GET", "path": "/x/stream" }
        });
        assert!(stream_descriptor_path(&top).unwrap().is_none());
    }

    #[test]
    fn stream_descriptor_path_errors_on_malformed_or_unsafe() {
        // Present but unsupported transport → loud error (not silent exit-0).
        let ws = serde_json::json!({
            "result": { "stream": { "transport": "ws", "method": "GET", "path": "/x" } }
        });
        assert!(stream_descriptor_path(&ws).is_err());

        // Unsupported method.
        let post = serde_json::json!({
            "result": { "stream": { "transport": "sse", "method": "POST", "path": "/x" } }
        });
        assert!(stream_descriptor_path(&post).is_err());

        // Missing fields — including method, which must not default to GET.
        let no_path = serde_json::json!({
            "result": { "stream": { "transport": "sse", "method": "GET" } }
        });
        assert!(stream_descriptor_path(&no_path).is_err());
        let no_transport = serde_json::json!({
            "result": { "stream": { "method": "GET", "path": "/x" } }
        });
        assert!(stream_descriptor_path(&no_transport).is_err());
        let no_method = serde_json::json!({
            "result": { "stream": { "transport": "sse", "path": "/x/stream" } }
        });
        assert!(stream_descriptor_path(&no_method).is_err());

        // Unsafe paths: full URL, authority, fragment, non-rooted, whitespace.
        for bad in [
            "https://evil.example.com/x/stream",
            "//evil.example.com/x",
            "/x/stream#frag",
            "x/stream",
            "/x /stream",
        ] {
            let resp = serde_json::json!({
                "result": { "stream": { "transport": "sse", "method": "GET", "path": bad } }
            });
            assert!(
                stream_descriptor_path(&resp).is_err(),
                "expected error for unsafe path {bad:?}"
            );
        }
    }

    #[test]
    fn thread_tail_resolves_positional_and_flag_to_thread_id() {
        // A `thread tail` command: positional `thread_id` form, plain execute_ref.
        let commands = vec![command(
            &["thread", "tail"],
            vec![vec![("thread_id", CommandArgumentKind::String)]],
            CommandProjectResolution::None,
        )];

        for argv in [
            s(&["thread", "tail", "T-abc"]),
            s(&["thread", "tail", "--thread-id", "T-abc"]),
        ] {
            let resolved = resolve_command_for_daemon_with_commands(
                &argv,
                &commands,
                &ryeos_runtime::CommandRegistrationPolicy::default(),
                None,
            )
            .unwrap();
            assert_eq!(resolved.parameters["thread_id"], serde_json::json!("T-abc"));
        }
    }
}
