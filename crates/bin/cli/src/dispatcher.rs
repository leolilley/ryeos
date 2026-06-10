use std::path::{Path, PathBuf};

use ryeos_runtime::{
    CommandDef, CommandDispatch, CommandParameterBindingMode, CommandProjectDefault,
    CommandProjectResolution, CommandRegistry,
};
use serde_json::Value;

use crate::error::CliError;
use crate::local_verbs;

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

    /// Verb tokens + tail (everything after globals).
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

    // 3. Hardcoded LOCAL verbs (must work before daemon exists):
    //      ryeos init                       — bootstrap operator state
    //      ryeos trust pin --from <trust>   — pin a publisher key
    //      ryeos publish <src>              — bundle author publish dance
    //      ryeos vault {put,list,remove,rewrap} — sealed secret management
    if local_verbs::try_dispatch(&cli.rest).await? {
        return Ok(());
    }

    // 4. No verb = help
    if cli.rest.is_empty() {
        crate::help::print_help(std::io::stdout())?;
        return Ok(());
    }

    // `ryeos help` → top-level help
    if cli.rest == ["help"] {
        crate::help::print_help(std::io::stdout())?;
        return Ok(());
    }

    // `ryeos help <verb...>` → verb help (queries daemon for alias info)
    if cli.rest.len() > 1 && cli.rest[0] == "help" {
        crate::help::print_verb_help(
            &cli.rest[1..],
            &app_root,
            body_project_path.as_deref().unwrap_or("."),
        )
        .await?;
        return Ok(());
    }

    // `ryeos <verb...> --help` / `-h` should feel like a normal CLI.
    // Without this guard the trailing help flag is bound as service
    // input and strict service schemas return noisy "unknown field help".
    if let Some(help_idx) = cli.rest.iter().position(|t| t == "--help" || t == "-h") {
        let verb_tokens = &cli.rest[..help_idx];
        if verb_tokens.is_empty() {
            crate::help::print_help(std::io::stdout())?;
        } else {
            crate::help::print_verb_help(
                verb_tokens,
                &app_root,
                body_project_path.as_deref().unwrap_or("."),
            )
            .await?;
        }
        return Ok(());
    }

    // 5. Descriptor-driven offline dispatch.
    //    For commands whose service descriptor declares availability: offline,
    //    run the in-process handler. Returns None to fall through to daemon.
    if let Some(outcome) = crate::offline_dispatch::try_offline_dispatch(
        &cli.rest,
        &app_root,
        body_project_path.as_deref().unwrap_or("."),
    )? {
        if let crate::offline_dispatch::OfflineDispatchOutcome::Json(result) = outcome {
            print_result(result);
        }
        return Ok(());
    }

    // 6. Token dispatch — send tokens to daemon, it resolves the command
    //    registry and binds tail parameters server-side.
    //
    //    For remote verbs that take a project root, CLI-side rewrite injects a canonical
    //    `--project <abs>` or `--no-project` into the tail. The daemon
    //    cannot do this — its cwd is irrelevant to the caller. Accepting
    //    `--project` here is deliberate: project-aware aliases expose a
    //    service-schema `project` field, while global `-p/--project` before
    //    the verb remains supported by clap above.

    let resolved = resolve_command_for_daemon(&cli.rest, &app_root, cli.project.as_deref())?;

    let mut body = serde_json::json!({
        "item_ref": resolved.item_ref,
        "parameters": resolved.parameters,
    });
    if resolved.async_launch {
        body["launch_mode"] = Value::String("accepted".to_string());
    }
    if let Some(project_path) = resolved.project_path {
        body["project_path"] = Value::String(project_path.to_string_lossy().into_owned());
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
}

fn resolve_command_for_daemon(
    rest: &[String],
    app_root: &Path,
    default_project: Option<&Path>,
) -> Result<CliResolvedExecute, CliError> {
    let snapshot = load_verified_snapshot(app_root)?;
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
    let async_launch = if matches!(
        matched.command.dispatch,
        CommandDispatch::DirectExecuteItemRef { .. }
    ) {
        strip_execute_control_flags(&mut tail)?
    } else {
        false
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
    let project_path = apply_project_policy(&matched.command, &mut parameters)?;
    Ok(CliResolvedExecute {
        item_ref,
        parameters,
        project_path,
        async_launch,
    })
}

fn strip_execute_control_flags(tail: &mut Vec<String>) -> Result<bool, CliError> {
    let mut async_launch = false;
    let mut out = Vec::with_capacity(tail.len());
    for token in tail.drain(..) {
        match token.as_str() {
            "--async" | "--async=true" => {
                async_launch = true;
            }
            "--async=false" => {}
            token if token.starts_with("--async=") => {
                return Err(CliError::Local {
                    detail: format!("invalid execute control flag value: {token}"),
                });
            }
            _ => out.push(token),
        }
    }
    *tail = out;
    Ok(async_launch)
}

fn bind_command_parameters_for_daemon(
    tail: &[String],
    command: &CommandDef,
) -> Result<Value, CliError> {
    let binding = command.parameter_binding.as_ref();
    if binding.is_some_and(|binding| binding.input_flag.is_some()) {
        if let Some(input) = crate::arg_bind::parse_input_arg(tail)? {
            return Ok(input);
        }
    }

    if binding.is_some_and(|binding| binding.single_json_object_arg) && tail.len() == 1 {
        if let Ok(value) = serde_json::from_str::<Value>(&tail[0]) {
            if value.is_object() {
                return Ok(value);
            }
        }
    }

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
    if project_path.is_none() && project.default == CommandProjectDefault::DiscoverUpwardAi {
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
    if no_project && project.bind_parameter.is_none() {
        obj.insert("no_project".to_string(), Value::Bool(true));
    }

    if project.request_project_path {
        Ok(project_path)
    } else {
        Ok(None)
    }
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
        return Ok(rest.to_vec());
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

fn load_verified_snapshot(
    app_root: &std::path::Path,
) -> Result<ryeos_app::node_config::NodeConfigSnapshot, CliError> {
    crate::node_descriptors::load_verified_snapshot(app_root).map_err(|error| CliError::Local {
        detail: format!("load verified node commands: {error:#}"),
    })
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
            category: "commands".into(),
            section: "commands".into(),
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

    fn direct_execute_command() -> CommandDef {
        CommandDef {
            category: "commands".into(),
            section: "commands".into(),
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
                default: ryeos_runtime::CommandProjectDefault::None,
                no_project_flag: true,
                request_project_path: false,
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
    fn remote_bind_project_accepts_project_after_verb() {
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
    fn remote_doctor_accepts_optional_project_after_verb() {
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
