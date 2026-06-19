use std::path::{Path, PathBuf};

use ryeos_runtime::{
    CommandDef, CommandDispatch, CommandParameterBindingMode, CommandProjectDefault,
    CommandProjectResolution, CommandRegistry,
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

    // `ryeos help` → top-level help
    if cli.rest == ["help"] {
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

    let resolved = resolve_command_for_daemon(&cli.rest, snapshot, cli.project.as_deref())?;

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

    // Stream a live execution log for `execute` runs on a terminal, unless the
    // caller opted out. Piped/redirected output and `--no-stream`/`--json` get
    // the buffered JSON result (machine-friendly, unchanged behavior).
    // `/execute/stream` requires a project_path (unlike `/execute`, which falls
    // back to the app root), so fall back to the buffered path when none was
    // resolved (`--no-project` / outside a project). `--stream`/`--no-stream`
    // force the choice; otherwise auto-detect a terminal.
    let want_stream = resolved
        .stream
        .unwrap_or_else(|| std::io::IsTerminal::is_terminal(&std::io::stdout()));
    let stream_live = resolved.direct_execute
        && !resolved.async_launch
        && resolved.project_path.is_some()
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
    print_result(result);
    Ok(())
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
}

/// Control flags stripped from an `execute` command tail before parameter bind.
#[derive(Default)]
struct ExecuteControlFlags {
    async_launch: bool,
    /// See `CliResolvedExecute::stream`.
    stream: Option<bool>,
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
    let control = if direct_execute {
        strip_execute_control_flags(&mut tail)?
    } else {
        ExecuteControlFlags::default()
    };
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
    Ok(CliResolvedExecute {
        item_ref,
        parameters,
        project_path,
        async_launch: control.async_launch,
        direct_execute,
        stream: control.stream,
    })
}

fn strip_execute_control_flags(tail: &mut Vec<String>) -> Result<ExecuteControlFlags, CliError> {
    let mut flags = ExecuteControlFlags::default();
    let mut out = Vec::with_capacity(tail.len());
    for token in tail.drain(..) {
        match token.as_str() {
            "--async" | "--async=true" => {
                flags.async_launch = true;
            }
            "--async=false" => {}
            token if token.starts_with("--async=") => {
                return Err(CliError::Local {
                    detail: format!("invalid execute control flag value: {token}"),
                });
            }
            // Force the live stream on (useful when piping to a pager).
            "--stream" => {
                if flags.stream == Some(false) {
                    return Err(CliError::Local {
                        detail: "conflicting flags: --stream and --no-stream/--json".into(),
                    });
                }
                flags.stream = Some(true);
            }
            // Both opt out of the live stream and print the final JSON result.
            "--no-stream" | "--json" => {
                if flags.stream == Some(true) {
                    return Err(CliError::Local {
                        detail: "conflicting flags: --stream and --no-stream/--json".into(),
                    });
                }
                flags.stream = Some(false);
            }
            _ => out.push(token),
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

    // Discover the daemon's principal_id for audience binding.
    let audience = crate::transport::discovery::discover_audience(&daemon_url).await?;

    let body_bytes = serde_json::to_vec(body).expect("infallible: Value serialization");
    let headers = signer.sign("POST", route_path, &body_bytes, &audience)?;

    let url = format!("{}{}", daemon_url, route_path);
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
    let audience = crate::transport::discovery::discover_audience(&daemon_url).await?;

    let route_path = "/execute/stream";
    let body_bytes = serde_json::to_vec(body).expect("infallible: Value serialization");
    let headers = signer.sign("POST", route_path, &body_bytes, &audience)?;
    let url = format!("{daemon_url}{route_path}");

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
    let headers = signer.sign("GET", &result_path, &[], &audience)?;
    let url = format!("{daemon_url}{result_path}");
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
    match ryeos_node::LifecycleController::from_env(env)
        .status()
        .await
        .map_err(|e| CliError::Local {
            detail: format!("read lifecycle status: {e:#}"),
        })? {
        ryeos_node::LifecycleStatus::Running { .. } => Ok(()),
        ryeos_node::LifecycleStatus::NotInitialized { diagnostics } => Err(CliError::Local {
            detail: format!(
                "RyeOS is not initialized. Run: ryeos init\nDetail: {}",
                diagnostics.message
            ),
        }),
        ryeos_node::LifecycleStatus::Stopped { .. } => Err(CliError::Local {
            detail: "RyeOS is initialized but not running. Run: ryeos start".into(),
        }),
        ryeos_node::LifecycleStatus::Stale { diagnostics, .. } => Err(CliError::Local {
            detail: format!(
                "RyeOS daemon metadata is stale: {}\nRun: ryeos start",
                diagnostics.message
            ),
        }),
    }
}

fn print_result(payload: serde_json::Value) {
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
        let mut tail = vec![
            "item:x".to_string(),
            "--async".to_string(),
            "--no-stream".to_string(),
            "--keep".to_string(),
        ];
        let flags = strip_execute_control_flags(&mut tail).unwrap();
        assert!(flags.async_launch);
        assert_eq!(flags.stream, Some(false));
        // Non-control tokens survive untouched.
        assert_eq!(tail, vec!["item:x".to_string(), "--keep".to_string()]);

        // --json is an alias for force-off.
        let mut json_tail = vec!["--json".to_string()];
        assert_eq!(
            strip_execute_control_flags(&mut json_tail).unwrap().stream,
            Some(false)
        );

        // --stream forces on.
        let mut stream_tail = vec!["--stream".to_string()];
        assert_eq!(
            strip_execute_control_flags(&mut stream_tail)
                .unwrap()
                .stream,
            Some(true)
        );

        // Default: auto (None), nothing set.
        let mut plain = vec!["--other".to_string()];
        let flags = strip_execute_control_flags(&mut plain).unwrap();
        assert_eq!(flags.stream, None);
        assert!(!flags.async_launch);
        assert_eq!(plain, vec!["--other".to_string()]);

        // Conflicting flags error.
        let mut conflict = vec!["--stream".to_string(), "--no-stream".to_string()];
        assert!(strip_execute_control_flags(&mut conflict).is_err());
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
                assert!(format!("{err}").contains("invalid execute control flag value"));
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
}
