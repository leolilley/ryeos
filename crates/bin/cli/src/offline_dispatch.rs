//! Engine-backed offline command dispatch.
//!
//! Commands whose service descriptor declares `availability: offline` (or `both`)
//! run locally without a daemon. Client and tool descriptors are also dispatchable
//! offline when the command's `execute` field targets them directly.
//!
//! The engine resolves items (tools, services, clients) through the same pipeline
//! the daemon uses: kind-agnostic resolution, trust verification, composition.
//! The CLI reads dispatch fields generically from the composed value — no
//! schema-specific structs needed.
//!
//! Command descriptors are node configuration (not engine kinds), so they are
//! loaded from the verified node-config snapshot. Unsigned, tampered, or untrusted
//! node config fails before any offline execution path is selected.

mod params;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::EffectiveItem;
use ryeos_runtime::{CommandDef, CommandDispatch, CommandRegistry};
use serde_json::Value;

use crate::error::CliError;
use params::{bind_params_minimal, bind_params_with_schema, expand_template};

#[derive(Debug)]
pub enum OfflineDispatchOutcome {
    Json(Value),
    Silent,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Try to dispatch a command through the offline descriptor path.
///
/// Returns `Ok(Some(result))` if the command was handled offline.
/// Returns `Ok(None)` if the command is not offline-capable (caller should fall
/// through to daemon dispatch). Returns `Err` if the command is offline-capable
/// but something went wrong.
pub async fn try_offline_dispatch(
    argv: &[String],
    app_root: &Path,
    project_path: &str,
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
) -> Result<Option<OfflineDispatchOutcome>, CliError> {
    // 1. The verified node config comes from the caller, loaded once per
    //    invocation in `dispatcher::run` from signed installed bundle
    //    registrations.
    let bundle_roots: Vec<PathBuf> = snapshot
        .bundles
        .iter()
        .map(|record| record.path.clone())
        .collect();
    if bundle_roots.is_empty() {
        return Ok(None);
    }

    // 2. Resolve the command from the verified snapshot.
    let registry = CommandRegistry::from_records(
        &snapshot.commands,
        &snapshot.command_registration_policy.policy,
    )
    .map_err(|error| CliError::Local {
        detail: format!("load verified node commands: {error:#}"),
    })?;
    let Ok(matched) = registry.resolve(argv) else {
        return Ok(None);
    };
    let CommandDispatch::ExecuteRef { execute, .. } = &matched.command.dispatch else {
        return Ok(None);
    };

    // 3. Parse the command's execute ref for the engine. Kind semantics stay in
    //    the engine; dispatch below is based on composed fields.
    let execute_ref = execute;
    let canonical = CanonicalRef::parse(execute_ref).map_err(|e| CliError::Local {
        detail: format!(
            "command '{}' has invalid execute ref '{}': {e}",
            matched.command.name, execute_ref
        ),
    })?;

    // 4. Boot engine (lazy — only reached when we know we have a match)
    let node_config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        app_root: Some(app_root.to_path_buf()),
        ..Default::default()
    })
    .map_err(local_err)?;
    let engine = boot_engine(&node_config, project_path, &bundle_roots)?;

    // 5. Resolve once through the engine, then dispatch by composed fields.
    let item = effective_item(&engine, canonical, project_path, execute_ref)?;
    let tail = &argv[matched.consumed..];

    if has_launch_binary_ref(&item.composed_value) {
        return exec_client(
            &engine,
            item,
            &matched.command,
            tail,
            app_root,
            project_path,
        )
        .await
        .map(Some);
    }

    if has_service_offline_dispatch(&item.composed_value) {
        return dispatch_service(
            &engine,
            item,
            &matched.command,
            tail,
            app_root,
            project_path,
            node_config.sandbox_enabled,
        );
    }

    if has_tool_command(&item.composed_value) {
        let params = bind_params_minimal(tail, &matched.command, project_path)?;
        return exec_tool(
            &engine,
            &item,
            execute_ref,
            params,
            app_root,
            project_path,
            node_config.sandbox_enabled,
        )
        .map(|result| result.map(OfflineDispatchOutcome::Json));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Engine boot
// ---------------------------------------------------------------------------

fn boot_engine(
    config: &ryeos_app::config::Config,
    project_path: &str,
    bundle_roots: &[PathBuf],
) -> Result<ryeos_engine::engine::Engine, CliError> {
    let project_root = if project_path == "." {
        None
    } else {
        Some(PathBuf::from(project_path))
    };

    ryeos_app::engine_init::build_engine_for_roots(
        config,
        bundle_roots,
        project_root.as_deref(),
        None, // no trust overlay
    )
    .map_err(local_err)
}

fn effective_item(
    engine: &ryeos_engine::engine::Engine,
    canonical: CanonicalRef,
    project_path: &str,
    execute_ref: &str,
) -> Result<EffectiveItem, CliError> {
    effective_item_from_project_root(engine, canonical, project_root(project_path), execute_ref)
}

fn effective_item_from_project_root(
    engine: &ryeos_engine::engine::Engine,
    canonical: CanonicalRef,
    project_root: Option<PathBuf>,
    execute_ref: &str,
) -> Result<EffectiveItem, CliError> {
    let request = ryeos_engine::engine::EffectiveItemRequest {
        item_ref: canonical,
        expected_kind: None,
        project_root,
    };

    engine.effective_item(request).map_err(|e| CliError::Local {
        detail: format!("resolve '{execute_ref}': {e}"),
    })
}

fn project_root(project_path: &str) -> Option<PathBuf> {
    if project_path == "." {
        None
    } else {
        Some(PathBuf::from(project_path))
    }
}

fn has_launch_binary_ref(value: &Value) -> bool {
    value
        .get("launch")
        .and_then(|v| v.get("binary_ref"))
        .and_then(|v| v.as_str())
        .is_some()
}

fn has_service_offline_dispatch(value: &Value) -> bool {
    value
        .get("offline_execute")
        .and_then(|v| v.as_str())
        .is_some()
        || matches!(
            value.get("availability").and_then(|v| v.as_str()),
            Some("offline" | "both")
        )
}

fn has_tool_command(value: &Value) -> bool {
    value
        .get("config")
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
        .is_some()
}

// ---------------------------------------------------------------------------
// Client dispatch
// ---------------------------------------------------------------------------

async fn exec_client(
    engine: &ryeos_engine::engine::Engine,
    item: EffectiveItem,
    command_def: &CommandDef,
    tail: &[String],
    app_root: &Path,
    project_path: &str,
) -> Result<OfflineDispatchOutcome, CliError> {
    let item_ref = item.canonical_ref.clone();

    if client_requires_daemon(&item.composed_value) {
        crate::daemon_preflight::lifecycle_preflight(app_root).await?;
    }

    let launch = item
        .composed_value
        .get("launch")
        .ok_or_else(|| CliError::Local {
            detail: format!("item '{item_ref}' composed value missing launch block"),
        })?;

    let mode = launch
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("cli_exec");
    if mode != "cli_exec" {
        return Err(CliError::Local {
            detail: format!(
                "offline launch for '{item_ref}' only supports cli_exec mode, got {mode:?}"
            ),
        });
    }

    // Read launch.binary_ref from composed value.
    let binary_ref = launch
        .get("binary_ref")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CliError::Local {
            detail: format!("item '{item_ref}' composed value missing launch.binary_ref"),
        })?;

    // Resolve binary via engine's binary resolver
    let bundle_root = item
        .source
        .bundle_root
        .as_ref()
        .ok_or_else(|| CliError::Local {
            detail: format!("item '{item_ref}' not from an installed bundle"),
        })?;

    let resolved = ryeos_engine::binary_resolver::resolve_bundle_binary_ref(
        binary_ref,
        bundle_root,
        |fp| {
            engine
                .trust_store
                .get(fp)
                .map(|signer| signer.verifying_key)
        },
        ryeos_engine::resolution::TrustClass::TrustedBundle,
    )
    .map_err(|e| CliError::Local {
        detail: format!("resolve client binary '{binary_ref}': {e}"),
    })?;

    let args = client_args_from_launch(launch, command_def, tail, project_path)?;

    // Exec: client replaces the process (inherited stdio)
    let mut command = std::process::Command::new(&resolved.absolute_path);
    command.args(&args);
    command
        .env("RYEOS_PROJECT_PATH", project_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let status = command
        .status()
        .with_context(|| {
            format!(
                "run client '{}' ({})",
                item_ref,
                resolved.absolute_path.display()
            )
        })
        .map_err(local_err)?;

    if !status.success() {
        return Err(CliError::Local {
            detail: format!("client '{}' failed with exit {:?}", item_ref, status.code()),
        });
    }

    Ok(OfflineDispatchOutcome::Silent)
}

fn client_requires_daemon(value: &Value) -> bool {
    value
        .get("capabilities")
        .and_then(|v| v.get("requires_daemon"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn client_args_from_launch(
    launch: &Value,
    command_def: &CommandDef,
    tail: &[String],
    project_path: &str,
) -> Result<Vec<String>, CliError> {
    let Some(arg_map) = launch.get("args").and_then(|value| value.as_object()) else {
        return Ok(tail.to_vec());
    };
    let params = bind_params_minimal(tail, command_def, project_path)?;
    let Some(params) = params.as_object() else {
        return Ok(Vec::new());
    };

    let mut args = Vec::new();
    for (field, flag) in arg_map {
        let Some(flag) = flag.as_str().filter(|flag| !flag.is_empty()) else {
            continue;
        };
        let Some(value) = params.get(field) else {
            continue;
        };
        append_client_arg(&mut args, flag, value);
    }
    Ok(args)
}

fn append_client_arg(args: &mut Vec<String>, flag: &str, value: &Value) {
    match value {
        Value::Null | Value::Bool(false) => {}
        Value::Bool(true) => args.push(flag.to_string()),
        Value::String(value) => {
            args.push(flag.to_string());
            args.push(value.clone());
        }
        Value::Number(value) => {
            args.push(flag.to_string());
            args.push(value.to_string());
        }
        Value::Array(values) => {
            for value in values {
                append_client_arg(args, flag, value);
            }
        }
        Value::Object(_) => {
            args.push(flag.to_string());
            args.push(value.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

fn exec_tool(
    engine: &ryeos_engine::engine::Engine,
    item: &EffectiveItem,
    tool_ref_str: &str,
    params: Value,
    app_root: &Path,
    project_path: &str,
    sandbox_enabled: bool,
) -> Result<Option<Value>, CliError> {
    // Check executor_id
    let executor_id = item
        .composed_value
        .get("executor_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match executor_id {
        "@subprocess" | "tool:ryeos/core/subprocess/execute" => {}
        other => {
            return Err(CliError::Local {
                detail: format!(
                    "offline tool `{tool_ref_str}` must use @subprocess executor, got {other:?}"
                ),
            });
        }
    }

    let config = item
        .composed_value
        .get("config")
        .ok_or_else(|| CliError::Local {
            detail: format!("offline tool `{tool_ref_str}` missing config block"),
        })?;

    let command_template = config
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CliError::Local {
            detail: format!("offline tool `{tool_ref_str}` missing config.command"),
        })?;

    if !command_template.starts_with("bin:") && !command_template.starts_with("bin/") {
        return Err(CliError::Local {
            detail: format!(
                "offline subprocess tools may only execute trusted `bin:` or `bin/` commands, got `{command_template}`"
            ),
        });
    }

    let bundle_root = item
        .source
        .bundle_root
        .as_ref()
        .ok_or_else(|| CliError::Local {
            detail: format!("tool '{tool_ref_str}' not from an installed bundle"),
        })?;

    let params_json = serde_json::to_string(&params).map_err(|e| CliError::Local {
        detail: e.to_string(),
    })?;

    // Resolve binary
    let cmd = expand_template(command_template, &params_json, project_path)?;
    let resolved = ryeos_engine::binary_resolver::resolve_bundle_binary_ref(
        &cmd,
        bundle_root,
        |fp| {
            engine
                .trust_store
                .get(fp)
                .map(|signer| signer.verifying_key)
        },
        ryeos_engine::resolution::TrustClass::TrustedBundle,
    )
    .map_err(|e| CliError::Local {
        detail: format!("resolve offline binary `{cmd}`: {e}"),
    })?;

    // Build args
    let args = config
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    v.as_str()
                        .map(|s| expand_template(s, &params_json, project_path))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();

    let stdin_data = config
        .get("input_data")
        .and_then(|v| v.as_str())
        .map(|t| expand_template(t, &params_json, project_path))
        .transpose()?;

    let cwd = match config.get("cwd").and_then(|v| v.as_str()) {
        Some(cwd) => Some(expand_template(cwd, &params_json, project_path)?),
        None => Some(project_path.to_string()),
    };

    let inherit_stdio = config
        .get("inherit_stdio")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let inherit_env = config
        .get("inherit_env")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let timeout = config
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60);

    let mut envs: Vec<(String, String)> = config
        .get("env")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| {
                    v.as_str().map(|s| {
                        expand_template(s, &params_json, project_path).map(|v| (k.clone(), v))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();

    envs.push(("RYEOS_APP_ROOT".to_string(), app_root.display().to_string()));

    if inherit_env {
        for (key, value) in std::env::vars() {
            if !envs.iter().any(|(configured, _)| configured == &key) {
                envs.push((key, value));
            }
        }
    }

    let request = ryeos_engine::subprocess_spec::sandbox_lillux_request(
        lillux::SubprocessRequest {
            cmd: resolved.absolute_path.to_string_lossy().into_owned(),
            args,
            cwd,
            envs,
            stdin_data,
            timeout: timeout as f64,
            limits: None,
        },
        sandbox_enabled,
        app_root,
        Path::new(project_path),
        tool_ref_str,
        "offline-cli",
    )
    .map_err(|error| CliError::Local {
        detail: format!("offline tool sandbox refused execution: {error}"),
    })?;

    if inherit_stdio {
        return exec_inherited(
            tool_ref_str,
            Path::new(&request.cmd),
            &request.args,
            request.cwd.as_deref(),
            &request.envs,
            request.limits.as_ref(),
            false,
        );
    }

    let result = lillux::run(request);

    if !result.success {
        return Err(CliError::Local {
            detail: format!(
                "offline tool `{tool_ref_str}` failed with exit {:?}\nstdout:\n{}\nstderr:\n{}",
                result.exit_code, result.stdout, result.stderr
            ),
        });
    }

    Ok(Some(
        serde_json::from_str(&result.stdout)
            .unwrap_or_else(|_| Value::String(result.stdout.clone())),
    ))
}

// ---------------------------------------------------------------------------
// Service dispatch
// ---------------------------------------------------------------------------

fn dispatch_service(
    engine: &ryeos_engine::engine::Engine,
    item: EffectiveItem,
    command: &CommandDef,
    tail: &[String],
    app_root: &Path,
    project_path: &str,
    sandbox_enabled: bool,
) -> Result<Option<OfflineDispatchOutcome>, CliError> {
    // Check availability
    let availability = item
        .composed_value
        .get("availability")
        .and_then(|v| v.as_str())
        .unwrap_or("daemon_only");

    let is_offline = availability == "offline" || availability == "both";
    if !is_offline {
        // A descriptor that names an offline tool while staying daemon-only
        // is contradicting itself; routing to the daemon here would silently
        // ignore the declared offline dispatch. Fail at the source instead.
        // (An explicit null is "absent", matching the resolution below.)
        if item
            .composed_value
            .get("offline_execute")
            .and_then(Value::as_str)
            .is_some()
        {
            return Err(CliError::Local {
                detail: format!(
                    "service '{}' declares offline_execute but availability is \
                     '{availability}'; set availability to offline|both or drop \
                     offline_execute",
                    item.canonical_ref
                ),
            });
        }
        return Ok(None);
    }

    // Resolve offline tool ref
    let offline_execute = item
        .composed_value
        .get("offline_execute")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| match &command.dispatch {
            CommandDispatch::ExecuteRef { execute, .. } if execute.starts_with("tool:") => {
                Some(execute.clone())
            }
            _ => None,
        });

    // Get service schema for param validation
    let service_schema = item
        .composed_value
        .get("schema")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    // Bind params with service schema
    let mut params = bind_params_with_schema(tail, command, &service_schema, project_path)?;

    // Strip internal routing fields before passing to the subprocess tool.
    if let Some(obj) = params.as_object_mut() {
        obj.retain(|key, _| !key.starts_with('_'));
    }

    let Some(tool_ref) = offline_execute else {
        return run_standalone_service(&item.canonical_ref, params, app_root).map(Some);
    };

    // Dispatch tool
    let canonical = CanonicalRef::parse(&tool_ref).map_err(|e| CliError::Local {
        detail: format!("invalid offline tool ref '{tool_ref}': {e}"),
    })?;
    let tool_item = effective_item(engine, canonical, project_path, &tool_ref)?;
    exec_tool(
        engine,
        &tool_item,
        &tool_ref,
        params,
        app_root,
        project_path,
        sandbox_enabled,
    )
    .map(|result| result.map(OfflineDispatchOutcome::Json))
}

fn run_standalone_service(
    service_ref: &str,
    params: Value,
    app_root: &Path,
) -> Result<OfflineDispatchOutcome, CliError> {
    ensure_daemon_stopped_for_standalone(app_root)?;

    let params_json = serde_json::to_string(&params).map_err(|error| CliError::Local {
        detail: format!("encode standalone service params: {error}"),
    })?;
    let ryeosd = resolve_ryeosd_binary();
    let output = std::process::Command::new(&ryeosd)
        .arg("--app-root")
        .arg(app_root)
        .arg("run-service")
        .arg(service_ref)
        .arg("--params")
        .arg(params_json)
        .output()
        .with_context(|| {
            format!(
                "run standalone service `{service_ref}` via {}",
                ryeosd.display()
            )
        })
        .map_err(local_err)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(CliError::Local {
            detail: format!(
                "standalone service `{service_ref}` failed with exit {:?}: {detail}",
                output.status.code()
            ),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Ok(OfflineDispatchOutcome::Silent);
    }
    match serde_json::from_str::<Value>(&stdout) {
        Ok(value) => Ok(OfflineDispatchOutcome::Json(value)),
        Err(_) => {
            println!("{stdout}");
            Ok(OfflineDispatchOutcome::Silent)
        }
    }
}

fn ensure_daemon_stopped_for_standalone(app_root: &Path) -> Result<(), CliError> {
    let lock_path = ryeos_app::state_lock::default_lock_path(app_root);
    match ryeos_app::state_lock::StateLock::acquire(&lock_path) {
        Ok(lock) => {
            drop(lock);
            Ok(())
        }
        Err(error) => Err(CliError::Local {
            detail: format!(
                "this command is offline-only and the daemon appears to be running. Run `ryeos stop`, then retry. ({error:#})"
            ),
        }),
    }
}

fn resolve_ryeosd_binary() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let sibling = parent.join("ryeosd");
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from("ryeosd")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn local_err(error: anyhow::Error) -> CliError {
    CliError::Local {
        detail: format!("{error:#}"),
    }
}

fn exec_inherited(
    tool_ref: &str,
    cmd: &Path,
    args: &[String],
    cwd: Option<&str>,
    envs: &[(String, String)],
    limits: Option<&lillux::SubprocessLimits>,
    inherit_env: bool,
) -> Result<Option<Value>, CliError> {
    let mut command = std::process::Command::new(cmd);
    command.args(args);
    if !inherit_env {
        command.env_clear();
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    lillux::configure_subprocess_limits(&mut command, limits).map_err(|error| CliError::Local {
        detail: format!(
            "offline tool `{tool_ref}` has invalid or unsupported resource limits: {error}"
        ),
    })?;

    let status = command
        .status()
        .with_context(|| format!("run inherited offline tool `{tool_ref}`"))
        .map_err(local_err)?;
    if !status.success() {
        return Err(CliError::Local {
            detail: format!(
                "offline tool `{tool_ref}` failed with exit {:?}",
                status.code()
            ),
        });
    }
    Ok(Some(serde_json::json!({ "status": "ok" })))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::{EncodePrivateKey, SigningKey};
    use rand::rngs::OsRng;

    #[test]
    fn inherited_exec_rejects_invalid_limits_before_spawn() {
        let limits = lillux::SubprocessLimits {
            max_open_files: Some(u64::MAX),
        };

        let error = exec_inherited(
            "tool:test/inherited",
            Path::new("unused"),
            &[],
            None,
            &[],
            Some(&limits),
            false,
        )
        .unwrap_err();

        assert!(error.to_string().contains("resource limits"));
    }

    fn expect_json(outcome: OfflineDispatchOutcome) -> Value {
        match outcome {
            OfflineDispatchOutcome::Json(value) => value,
            OfflineDispatchOutcome::Silent => panic!("expected JSON offline dispatch outcome"),
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        _env_guard: std::sync::MutexGuard<'static, ()>,
        system: PathBuf,
        project: PathBuf,
        bundle: PathBuf,
        key: SigningKey,
    }

    impl Fixture {
        fn new() -> Self {
            let env_guard = crate::test_env::lock();
            let tmp = tempfile::tempdir().unwrap();
            let system = tmp.path().join("system");
            let project = tmp.path().join("project");
            std::fs::create_dir_all(project.join(ryeos_engine::AI_DIR)).unwrap();
            let trust_dir = system
                .join(ryeos_engine::AI_DIR)
                .join("config")
                .join("keys")
                .join("trusted");
            std::fs::create_dir_all(&trust_dir).unwrap();
            let key = SigningKey::generate(&mut OsRng);
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();
            let node_identity_dir = system
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("identity");
            std::fs::create_dir_all(&node_identity_dir).unwrap();
            std::fs::write(
                node_identity_dir.parent().unwrap().join("sandbox.yaml"),
                "version: 1\nbackend_path: /usr/bin/bwrap\nallow_network: false\n\
                 writable_paths:\n  - \"{project}\"\nallowed_env:\n  - \"*\"\n\
                 max_open_files: 128\nmax_processes: 32\n",
            )
            .unwrap();
            let pem = key.to_pkcs8_pem(Default::default()).unwrap();
            std::fs::write(node_identity_dir.join("private_key.pem"), pem.as_bytes()).unwrap();
            let dev_trust = std::fs::read_to_string(
                workspace_root()
                    .join(".dev-keys")
                    .join("PUBLISHER_DEV_TRUST.toml"),
            )
            .unwrap();
            let dev_trust = ryeos_engine::trust::PublisherTrustDoc::parse(&dev_trust).unwrap();
            let dev_key = dev_trust.decode_verifying_key().unwrap();
            ryeos_engine::trust::pin_key(&dev_key, "ryeos-dev", &trust_dir, None).unwrap();
            std::env::set_var("RYEOS_APP_ROOT", &system);

            let bundle = system
                .join(ryeos_engine::AI_DIR)
                .join("bundles")
                .join("test");
            std::fs::create_dir_all(bundle.join(ryeos_engine::AI_DIR)).unwrap();
            let core_bundle = system
                .join(ryeos_engine::AI_DIR)
                .join("bundles")
                .join("core");
            copy_dir_all(&workspace_root().join("bundles").join("core"), &core_bundle).unwrap();

            let this = Self {
                _tmp: tmp,
                _env_guard: env_guard,
                system,
                project,
                bundle,
                key,
            };
            this.resign_node_commands(&core_bundle);
            this.write_signed(
                &core_bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("protocols")
                    .join("ryeos")
                    .join("core")
                    .join("cli_exec.yaml"),
                "kind: protocol\nname: cli_exec\ncategory: ryeos/core\nabi_version: v1\ndescription: Direct exec with argv flags and inherited stdio.\nstdin:\n  shape: opaque\nstdout:\n  shape: opaque_bytes\n  mode: terminal\nenv_injections:\n  - { name: RYEOS_PROJECT_PATH, source: project_path }\ncapabilities:\n  allows_pushed_head: false\n  allows_target_site: false\n  allows_detached: false\nlifecycle:\n  mode: managed\ncallback_channel: none\n",
            );
            this.write_manifest();
            this.write_command_registration_policy();
            this.write_registration();
            this.write_core_bundle_registration(&core_bundle);
            for handler_bin in [
                "rye-composer-identity",
                "rye-parser-regex-kv",
                "rye-parser-yaml-document",
                "rye-parser-yaml-header-document",
            ] {
                this.write_bin_in_bundle(
                    &core_bundle,
                    handler_bin,
                    fixture_handler_script().as_bytes(),
                );
            }
            this.write_echo_bin();
            this.write_standard_descriptors(Some("offline_execute: tool:custom/echo\n"));
            this
        }

        fn resign_node_commands(&self, bundle: &Path) {
            let root = bundle
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("commands");
            if !root.is_dir() {
                return;
            }
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(dir).unwrap() {
                    let entry = entry.unwrap();
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().and_then(|ext| ext.to_str()) == Some("yaml") {
                        let content = std::fs::read_to_string(&path).unwrap();
                        let body = lillux::signature::strip_signature_lines(&content);
                        self.write_signed(&path, &body);
                    }
                }
            }
        }

        fn write_standard_descriptors(&self, offline_execute_line: Option<&str>) {
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("node")
                    .join("commands")
                    .join("custom.yaml"),
                "tokens: [\"custom\"]\ndescription: Custom offline command\nforms:\n  - slots:\n      - field: name\ndispatch:\n  kind: execute_ref\n  execute: service:custom\n",
            );
            let offline_execute_line = offline_execute_line.unwrap_or("");
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("services")
                    .join("custom.yaml"),
                &format!(
                    "kind: service\nendpoint: custom\navailability: offline\n{offline_execute_line}schema:\n  name: string?\n  project_path: string?\n"
                ),
            );
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("tools")
                    .join("custom")
                    .join("echo.yaml"),
                "category: custom\nname: echo\nexecutor_id: \"@subprocess\"\nconfig:\n  command: \"bin:echo-json\"\n  input_data: \"{params_json}\"\n",
            );
        }

        fn write_manifest(&self) {
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("manifest.yaml"),
                "name: test\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n",
            );
        }

        fn write_registration(&self) {
            self.write_bundle_registration("test", &self.bundle);
        }

        fn write_command_registration_policy(&self) {
            self.write_signed(
                &self
                    .system
                    .join(ryeos_engine::AI_DIR)
                    .join("node")
                    .join("command_registration")
                    .join("default.yaml"),
                "claim_rules:\n  - claim:\n      kind: command.root\n      value: execute\n    required_caps:\n      - ryeos.register.command.root.execute\n  - claim:\n      kind: command.dispatch.kind\n      value: direct_execute_item_ref\n    required_caps:\n      - ryeos.register.command.dispatch.direct_execute_item_ref\nsystem_source_caps:\n  - ryeos.register.command.root.execute\n  - ryeos.register.command.dispatch.direct_execute_item_ref\n",
            );
        }

        fn write_core_bundle_registration(&self, core_bundle: &Path) {
            self.write_bundle_registration_with_caps(
                "core",
                core_bundle,
                &[
                    "ryeos.register.command.root.execute",
                    "ryeos.register.command.dispatch.direct_execute_item_ref",
                ],
            );
        }

        fn write_bundle_registration(&self, id: &str, bundle_root: &Path) {
            self.write_bundle_registration_with_caps(id, bundle_root, &[]);
        }

        fn write_bundle_registration_with_caps(
            &self,
            id: &str,
            bundle_root: &Path,
            command_registration_caps: &[&str],
        ) {
            let path = self
                .system
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("bundles")
                .join(format!("{id}.yaml"));
            let mut body = format!("kind: node\npath: {}\n", bundle_root.display());
            if !command_registration_caps.is_empty() {
                body.push_str("command_registration_caps:\n");
                for cap in command_registration_caps {
                    body.push_str("  - ");
                    body.push_str(cap);
                    body.push('\n');
                }
            }
            self.write_signed(&path, &body);
        }

        fn write_echo_bin(&self) {
            self.write_bin("echo-json", b"#!/bin/sh\ncat\n");
        }

        fn write_capture_bin(&self, capture_file: &Path) {
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$RYEOS_PROJECT_PATH\" > {}\nprintf '%s\\n' \"$@\" >> {}\n",
                capture_file.display(),
                capture_file.display()
            );
            self.write_bin("capture-client", script.as_bytes());
        }

        fn write_bin(&self, name: &str, script: &[u8]) {
            self.write_bin_in_bundle(&self.bundle, name, script);
        }

        fn write_bin_in_bundle(&self, bundle: &Path, name: &str, script: &[u8]) {
            let triple = host_triple();
            let ai_dir = bundle.join(ryeos_engine::AI_DIR);
            let bin_path = ai_dir.join("bin").join(triple).join(name);
            std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
            std::fs::write(&bin_path, script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }

            let cas = lillux::CasStore::new(ai_dir.join("objects"));
            let content_blob_hash = lillux::sha256_hex(script);
            let item_ref = format!("bin/{triple}/{name}");
            let item_source = serde_json::json!({
                "item_ref": item_ref,
                "content_blob_hash": content_blob_hash,
                "signature_info": {
                    "fingerprint": lillux::signature::compute_fingerprint(&self.key.verifying_key())
                }
            });
            let sidecar_body = lillux::cas::canonical_json(&item_source);
            let sidecar = lillux::signature::sign_content(&sidecar_body, &self.key, "#", None);
            std::fs::write(
                bin_path.with_file_name(format!("{name}.item_source.json")),
                sidecar,
            )
            .unwrap();
            let item_source_hash = cas.store_object(&item_source).unwrap();
            let ref_path = ai_dir.join("refs").join("bundles").join("manifest");
            let mut item_source_hashes = if ref_path.exists() {
                let manifest_hash = std::fs::read_to_string(&ref_path).unwrap();
                cas.get_object(manifest_hash.trim())
                    .unwrap()
                    .and_then(|manifest| manifest.get("item_source_hashes").cloned())
                    .and_then(|value| {
                        serde_json::from_value::<serde_json::Map<String, Value>>(value).ok()
                    })
                    .unwrap_or_default()
            } else {
                serde_json::Map::new()
            };
            item_source_hashes.insert(item_ref, Value::String(item_source_hash));
            let manifest = serde_json::json!({ "item_source_hashes": item_source_hashes });
            let manifest_hash = cas.store_object(&manifest).unwrap();
            std::fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
            std::fs::write(ref_path, manifest_hash).unwrap();
        }

        fn write_client_kind_schema(&self) {
            let path = workspace_root()
                .join("bundles")
                .join("standard")
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("engine")
                .join("kinds")
                .join("client")
                .join("client.kind-schema.yaml");
            let content = std::fs::read_to_string(path).unwrap();
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("node")
                    .join("engine")
                    .join("kinds")
                    .join("client")
                    .join("client.kind-schema.yaml"),
                &lillux::signature::strip_signature_lines(&content),
            );
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("manifest.yaml"),
                "name: test\nversion: '1.0'\nprovides_kinds: [client]\nrequires_kinds: []\nuses_kinds: []\n",
            );
        }

        fn write_signed(&self, path: &Path, body: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let signed = lillux::signature::sign_content(body, &self.key, "#", None);
            std::fs::write(path, signed).unwrap();
        }

        fn project_str(&self) -> String {
            self.project.to_string_lossy().into_owned()
        }
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .to_path_buf()
    }

    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let dst_path = dst.join(entry.file_name());
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &dst_path)?;
            } else if ty.is_file() {
                std::fs::copy(entry.path(), dst_path)?;
            }
        }
        Ok(())
    }

    fn fixture_handler_script() -> &'static str {
        r#"#!/usr/bin/python3
import json
import sys

req = json.load(sys.stdin)
cmd = req.get("command")

if cmd in ("validate_parser_config", "validate_composer_config"):
    print(json.dumps({"result": "validate_ok"}))
elif cmd == "parse":
    try:
        import yaml
        value = yaml.safe_load(req.get("content") or "")
        if value is None:
            value = {}
        print(json.dumps({"result": "parse_ok", "value": value}))
    except Exception as exc:
        print(json.dumps({"result": "parse_err", "kind": "syntax", "message": str(exc)}))
elif cmd == "compose":
    root = req.get("root", {})
    print(json.dumps({
        "result": "compose_ok",
        "composed": root.get("parsed", {}),
        "derived": {},
        "policy_facts": {},
    }))
else:
    print(json.dumps({"result": "validate_err", "message": "unknown command"}))
"#
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"))]
    fn host_triple() -> &'static str {
        "x86_64-unknown-linux-gnu"
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "musl"))]
    fn host_triple() -> &'static str {
        "x86_64-unknown-linux-musl"
    }

    #[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"))]
    fn host_triple() -> &'static str {
        "aarch64-unknown-linux-gnu"
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn host_triple() -> &'static str {
        "aarch64-apple-darwin"
    }

    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    fn host_triple() -> &'static str {
        "x86_64-apple-darwin"
    }

    /// Tests exercise the public entry point with the snapshot loaded
    /// the same way `dispatcher::run` does.
    fn try_offline_dispatch_for_test(
        argv: &[String],
        app_root: &std::path::Path,
        project_path: &str,
    ) -> Result<Option<OfflineDispatchOutcome>, CliError> {
        let snapshot =
            crate::node_descriptors::load_verified_snapshot(app_root).map_err(local_err)?;
        tokio::runtime::Runtime::new()
            .expect("create test runtime")
            .block_on(try_offline_dispatch(
                argv,
                app_root,
                project_path,
                &snapshot,
            ))
    }

    #[test]
    fn offline_dispatch_executes_descriptor_declared_tool() {
        let fixture = Fixture::new();
        let argv = vec!["custom".to_string(), "leo".to_string()];

        let result = expect_json(
            try_offline_dispatch_for_test(&argv, &fixture.system, &fixture.project_str())
                .unwrap()
                .expect("handled offline"),
        );

        assert_eq!(result["name"], "leo");
        assert_eq!(result["project_path"], fixture.project_str());
    }

    #[test]
    fn offline_service_without_tool_impl_dispatches_standalone_service() {
        let fixture = Fixture::new();
        fixture.write_standard_descriptors(None);

        let err = try_offline_dispatch_for_test(
            &["custom".to_string(), "leo".to_string()],
            &fixture.system,
            ".",
        )
        .unwrap_err();

        match err {
            CliError::Local { detail } => {
                assert!(
                    detail.contains("standalone service `service:custom`"),
                    "{detail}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn offline_dispatch_maps_project_flag_to_project_path_schema() {
        let fixture = Fixture::new();

        let result = expect_json(
            try_offline_dispatch_for_test(
                &[
                    "custom".to_string(),
                    "leo".to_string(),
                    "--project".to_string(),
                    "/tmp/project".to_string(),
                ],
                &fixture.system,
                ".",
            )
            .unwrap()
            .expect("handled offline"),
        );

        assert_eq!(result["project_path"], "/tmp/project");
        assert!(result.get("project").is_none());
    }

    #[test]
    fn duplicate_command_tokens_error_loudly() {
        let fixture = Fixture::new();
        let second_bundle = fixture
            .system
            .join(ryeos_engine::AI_DIR)
            .join("bundles")
            .join("second");
        std::fs::create_dir_all(second_bundle.join(ryeos_engine::AI_DIR)).unwrap();
        fixture.write_signed(
            &second_bundle
                .join(ryeos_engine::AI_DIR)
                .join("manifest.yaml"),
            "name: second\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n",
        );
        fixture.write_signed(
            &fixture
                .system
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("bundles")
                .join("second.yaml"),
            &format!("kind: node\npath: {}\n", second_bundle.display()),
        );
        fixture.write_signed(
            &second_bundle
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("commands")
                .join("other-custom.yaml"),
            "tokens: [\"custom\"]\ndescription: Other offline command\ndispatch:\n  kind: execute_ref\n  execute: service:other\n",
        );

        let err = try_offline_dispatch_for_test(&["custom".to_string()], &fixture.system, ".")
            .unwrap_err();
        match err {
            CliError::Local { detail } => {
                assert!(detail.contains("command token collision"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn offline_tool_rejects_non_bin_command() {
        let fixture = Fixture::new();
        fixture.write_signed(
            &fixture
                .bundle
                .join(ryeos_engine::AI_DIR)
                .join("tools")
                .join("custom")
                .join("echo.yaml"),
            "category: custom\nname: echo\nexecutor_id: \"@subprocess\"\nconfig:\n  command: \"cat\"\n  input_data: \"{params_json}\"\n",
        );

        let err = try_offline_dispatch_for_test(
            &["custom".to_string(), "leo".to_string()],
            &fixture.system,
            ".",
        )
        .unwrap_err();
        match err {
            CliError::Local { detail } => {
                assert!(
                    detail.contains("trusted `bin:` or `bin/` commands"),
                    "{detail}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn offline_tool_accepts_path_style_binary_ref() {
        let fixture = Fixture::new();
        fixture.write_signed(
            &fixture
                .bundle
                .join(ryeos_engine::AI_DIR)
                .join("tools")
                .join("custom")
                .join("echo.yaml"),
            "category: custom\nname: echo\nexecutor_id: \"@subprocess\"\nconfig:\n  command: \"bin/{triple}/echo-json\"\n  input_data: \"{params_json}\"\n",
        );

        let result = expect_json(
            try_offline_dispatch_for_test(
                &["custom".to_string(), "leo".to_string()],
                &fixture.system,
                ".",
            )
            .unwrap()
            .expect("handled offline"),
        );

        assert_eq!(result["name"], "leo");
    }

    #[test]
    fn offline_client_forwards_tail_and_returns_silent() {
        let fixture = Fixture::new();
        fixture.write_client_kind_schema();
        let capture_file = fixture._tmp.path().join("client-capture.txt");
        fixture.write_capture_bin(&capture_file);
        fixture.write_signed(
            &fixture
                .bundle
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("commands")
                .join("capture.yaml"),
            "tokens: [\"capture\"]\ndescription: Capture offline client command\ndispatch:\n  kind: execute_ref\n  execute: client:custom/capture\n",
        );
        fixture.write_signed(
            &fixture
                .bundle
                .join(ryeos_engine::AI_DIR)
                .join("clients")
                .join("custom")
                .join("capture.yaml"),
            "launch:\n  mode: cli_exec\n  binary_ref: bin/{triple}/capture-client\n  args:\n    surface: \"--surface\"\n    mock: \"--mock\"\nserves:\n  kind: surface\n",
        );

        let outcome = try_offline_dispatch_for_test(
            &[
                "capture".to_string(),
                "--surface".to_string(),
                "main".to_string(),
                "--mock".to_string(),
            ],
            &fixture.system,
            &fixture.project_str(),
        )
        .unwrap()
        .expect("handled offline");

        assert!(matches!(outcome, OfflineDispatchOutcome::Silent));
        let captured = std::fs::read_to_string(capture_file).unwrap();
        let lines: Vec<&str> = captured.lines().collect();
        assert_eq!(lines[0], fixture.project_str());
        assert!(lines[1..]
            .windows(2)
            .any(|pair| pair == ["--surface", "main"]));
        assert!(lines[1..].contains(&"--mock"));
    }

    #[test]
    fn offline_client_maps_command_defaults_through_launch_args() {
        let fixture = Fixture::new();
        fixture.write_client_kind_schema();
        let capture_file = fixture._tmp.path().join("client-default-capture.txt");
        fixture.write_capture_bin(&capture_file);
        fixture.write_signed(
            &fixture
                .bundle
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("commands")
                .join("capture-base.yaml"),
            "tokens: [\"capture\", \"base\"]\ndescription: Capture base client command\ndefaults:\n  surface: surface:demo/base\ndispatch:\n  kind: execute_ref\n  execute: client:custom/capture\n",
        );
        fixture.write_signed(
            &fixture
                .bundle
                .join(ryeos_engine::AI_DIR)
                .join("clients")
                .join("custom")
                .join("capture.yaml"),
            "launch:\n  mode: cli_exec\n  binary_ref: bin/{triple}/capture-client\n  args:\n    surface: \"--surface\"\nserves:\n  kind: surface\n",
        );

        let outcome = try_offline_dispatch_for_test(
            &["capture".to_string(), "base".to_string()],
            &fixture.system,
            &fixture.project_str(),
        )
        .unwrap()
        .expect("handled offline");

        assert!(matches!(outcome, OfflineDispatchOutcome::Silent));
        let captured = std::fs::read_to_string(capture_file).unwrap();
        let lines: Vec<&str> = captured.lines().collect();
        assert_eq!(lines[0], fixture.project_str());
        assert_eq!(lines[1..], ["--surface", "surface:demo/base"]);
    }
}
