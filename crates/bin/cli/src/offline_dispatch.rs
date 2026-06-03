//! Engine-backed offline command dispatch.
//!
//! Commands whose service descriptor declares `availability: offline` (or `both`)
//! run locally without a daemon. Client and tool descriptors are also dispatchable
//! offline when the verb's `execute` field targets them directly.
//!
//! The engine resolves items (tools, services, clients) through the same pipeline
//! the daemon uses: kind-agnostic resolution, trust verification, composition.
//! The CLI reads dispatch fields generically from the composed value — no
//! schema-specific structs needed.
//!
//! Alias and verb descriptors are node configuration (not engine kinds), so they
//! are loaded from the verified node-config snapshot. Unsigned, tampered, or
//! untrusted node config fails before any offline execution path is selected.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::EffectiveItem;
use ryeos_runtime::alias_registry::{AliasDef, ProjectResolution};
use serde_json::Value;

use crate::error::CliError;
use crate::node_descriptors::{LoadedAliasDescriptor, LoadedVerbDescriptor};

// ---------------------------------------------------------------------------
// Alias / verb types (node configuration, not engine kinds)
// ---------------------------------------------------------------------------

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
pub fn try_offline_dispatch(
    argv: &[String],
    system_space_dir: &Path,
    project_path: &str,
) -> Result<Option<OfflineDispatchOutcome>, CliError> {
    // 1. Load verified node config from signed installed bundle registrations.
    let snapshot =
        crate::node_descriptors::load_verified_snapshot(system_space_dir).map_err(local_err)?;
    let bundle_roots: Vec<PathBuf> = snapshot
        .bundles
        .iter()
        .map(|record| record.path.clone())
        .collect();
    if bundle_roots.is_empty() {
        return Ok(None);
    }

    // 2. Load aliases + verbs from the verified snapshot.
    let aliases = crate::node_descriptors::load_alias_descriptors_from_snapshot(&snapshot);
    let Some((alias, consumed)) = match_alias(argv, &aliases).map_err(local_err)? else {
        return Ok(None);
    };
    let Some(verb) =
        crate::node_descriptors::load_verb_descriptor_from_snapshot(&snapshot, &alias.def.verb)
    else {
        return Ok(None);
    };

    // 3. Parse the verb's execute ref for the engine. Kind semantics stay in
    //    the engine; dispatch below is based on composed fields.
    let execute_ref = &verb.execute;
    let canonical = CanonicalRef::parse(execute_ref).map_err(|e| CliError::Local {
        detail: format!(
            "verb '{}' has invalid execute ref '{}': {e}",
            alias.def.verb, execute_ref
        ),
    })?;

    // 4. Boot engine (lazy — only reached when we know we have a match)
    let engine = boot_engine(system_space_dir, project_path, &bundle_roots)?;

    // 5. Resolve once through the engine, then dispatch by composed fields.
    let item = effective_item(&engine, canonical, project_path, execute_ref)?;
    let tail = &argv[consumed..];

    if has_launch_binary_ref(&item.composed_value) {
        return exec_client(&engine, item, tail, project_path).map(Some);
    }

    if has_service_offline_dispatch(&item.composed_value) {
        return dispatch_service(
            &engine,
            item,
            &alias.def,
            &verb,
            tail,
            system_space_dir,
            project_path,
        )
        .map(|result| result.map(OfflineDispatchOutcome::Json));
    }

    if has_tool_command(&item.composed_value) {
        let params = bind_params_minimal(tail, &alias.def, project_path)?;
        return exec_tool(
            &engine,
            &item,
            execute_ref,
            params,
            system_space_dir,
            project_path,
        )
        .map(|result| result.map(OfflineDispatchOutcome::Json));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Engine boot
// ---------------------------------------------------------------------------

fn boot_engine(
    system_space_dir: &Path,
    project_path: &str,
    bundle_roots: &[PathBuf],
) -> Result<ryeos_engine::engine::Engine, CliError> {
    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        system_space_dir: Some(system_space_dir.to_path_buf()),
        ..Default::default()
    })
    .map_err(local_err)?;

    let project_root = if project_path == "." {
        None
    } else {
        Some(PathBuf::from(project_path))
    };

    let user_root = ryeos_engine::roots::user_root().ok();

    ryeos_app::engine_init::build_engine_for_roots(
        &config,
        bundle_roots,
        project_root.as_deref(),
        user_root.as_deref(),
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

fn exec_client(
    engine: &ryeos_engine::engine::Engine,
    item: EffectiveItem,
    tail: &[String],
    project_path: &str,
) -> Result<OfflineDispatchOutcome, CliError> {
    let item_ref = item.canonical_ref.clone();

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
        ryeos_engine::resolution::TrustClass::TrustedSystem,
    )
    .map_err(|e| CliError::Local {
        detail: format!("resolve client binary '{binary_ref}': {e}"),
    })?;

    // Clients own their argv surface. The descriptor may document structured
    // args for later protocol-aware binding, but offline CLI dispatch forwards
    // the caller's tail unchanged for now.
    let args = tail.to_vec();

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

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

fn exec_tool(
    engine: &ryeos_engine::engine::Engine,
    item: &EffectiveItem,
    tool_ref_str: &str,
    params: Value,
    system_space_dir: &Path,
    project_path: &str,
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
        ryeos_engine::resolution::TrustClass::TrustedSystem,
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
        None => Some(std::env::current_dir()?.to_string_lossy().into_owned()),
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

    envs.push((
        "RYEOS_SYSTEM_SPACE_DIR".to_string(),
        system_space_dir.display().to_string(),
    ));

    if inherit_stdio {
        return exec_inherited(
            tool_ref_str,
            &resolved.absolute_path,
            &args,
            cwd.as_deref(),
            &envs,
            inherit_env,
        );
    }

    let result = lillux::run(lillux::SubprocessRequest {
        cmd: resolved.absolute_path.to_string_lossy().into_owned(),
        args,
        cwd,
        envs,
        stdin_data,
        timeout: timeout as f64,
    });

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
    alias_def: &AliasDef,
    verb: &LoadedVerbDescriptor,
    tail: &[String],
    system_space_dir: &Path,
    project_path: &str,
) -> Result<Option<Value>, CliError> {
    // Check availability
    let availability = item
        .composed_value
        .get("availability")
        .and_then(|v| v.as_str())
        .unwrap_or("daemon_only");

    let is_offline = availability == "offline" || availability == "both";
    if !is_offline {
        return Ok(None);
    }

    // Resolve offline tool ref
    let offline_execute = item
        .composed_value
        .get("offline_execute")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            verb.execute
                .starts_with("tool:")
                .then(|| verb.execute.clone())
        });

    let Some(tool_ref) = offline_execute else {
        return Err(CliError::Local {
            detail: format!(
                "service `{}` is declared offline-capable, but its descriptor \
                 does not declare a local tool implementation (`offline_execute: tool:<id>`)",
                alias_def.verb
            ),
        });
    };

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
    let mut params = bind_params_with_schema(tail, alias_def, &service_schema, project_path)?;
    if let Some(obj) = params.as_object_mut() {
        obj.insert("_verb".to_string(), Value::String(alias_def.verb.clone()));
    }

    // Strip internal routing fields before passing to the subprocess tool.
    if let Some(obj) = params.as_object_mut() {
        obj.retain(|key, _| !key.starts_with('_'));
    }

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
        system_space_dir,
        project_path,
    )
}

// ---------------------------------------------------------------------------
// Parameter binding
// ---------------------------------------------------------------------------

fn bind_params_minimal(
    tail: &[String],
    alias: &AliasDef,
    project_path: &str,
) -> Result<Value, CliError> {
    if let Some(input) = crate::arg_bind::parse_input_arg(tail)? {
        return Ok(input);
    }

    let mut params = ryeos_runtime::arg_binder::bind_argv_with_alias(tail, Some(alias))
        .map_err(|e| CliError::Local { detail: e })?;

    // Project resolution
    if alias.project_resolution != ProjectResolution::None {
        let mut canonical_tail = params_to_tail(&params);
        let default_project = (project_path != ".").then(|| Path::new(project_path));
        match alias.project_resolution {
            ProjectResolution::None => {}
            ProjectResolution::Optional => {
                canonical_tail = crate::project_resolve::rewrite_project_tail_with_default(
                    &canonical_tail,
                    default_project,
                )?;
            }
            ProjectResolution::Required => {
                if canonical_tail.iter().any(|t| t == "--no-project") {
                    return Err(CliError::Local {
                        detail: "this command requires a project; do not pass --no-project".into(),
                    });
                }
                canonical_tail = crate::project_resolve::rewrite_project_tail_with_default(
                    &canonical_tail,
                    default_project,
                )?;
                if canonical_tail.iter().any(|t| t == "--no-project") {
                    return Err(CliError::Local {
                        detail: format!(
                            "this command requires a project; run it from a directory containing \
                             {} or pass --project <path>",
                            ryeos_engine::AI_DIR
                        ),
                    });
                }
            }
        }
        params = ryeos_runtime::bind_argv(&canonical_tail);
    }

    Ok(params)
}

fn bind_params_with_schema(
    tail: &[String],
    alias: &AliasDef,
    service_schema: &HashMap<String, String>,
    project_path: &str,
) -> Result<Value, CliError> {
    let mut params = bind_params_minimal(tail, alias, project_path)?;

    // Normalize project → project_path
    params = normalize_project_param(params, service_schema, project_path);

    // Reject unknown flags
    if let Some(obj) = params.as_object() {
        for key in obj.keys() {
            if key.starts_with('_') {
                continue;
            }
            let normalized_key = key.replace('_', "-");
            if !service_schema.contains_key(key.as_str())
                && !service_schema.contains_key(&normalized_key)
                && key != "input"
            {
                return Err(CliError::Local {
                    detail: format!(
                        "unknown parameter --{normalized_key} for this command{}",
                        if service_schema.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " (expected: {})",
                                service_schema
                                    .keys()
                                    .map(|k| format!("--{}", k.replace('_', "-")))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        }
                    ),
                });
            }
        }
    }

    Ok(params)
}

fn normalize_project_param(
    mut params: Value,
    service_schema: &HashMap<String, String>,
    default_project_path: &str,
) -> Value {
    let Some(obj) = params.as_object_mut() else {
        return params;
    };

    if service_schema.contains_key("project_path") && !service_schema.contains_key("project") {
        if let Some(project) = obj.remove("project") {
            obj.entry("project_path".to_string()).or_insert(project);
        }
    }

    if !obj.contains_key("project")
        && !obj.contains_key("project_path")
        && !obj.contains_key("no_project")
    {
        if service_schema.contains_key("project_path") {
            obj.insert(
                "project_path".to_string(),
                Value::String(default_project_path.to_string()),
            );
        } else if service_schema.contains_key("project") {
            obj.insert(
                "project".to_string(),
                Value::String(default_project_path.to_string()),
            );
        }
    }

    params
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn local_err(error: anyhow::Error) -> CliError {
    CliError::Local {
        detail: format!("{error:#}"),
    }
}

fn expand_template(
    template: &str,
    params_json: &str,
    project_path: &str,
) -> Result<String, CliError> {
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        if let Some(end) = rest[start + 1..].find('}') {
            let token = &rest[start + 1..start + 1 + end];
            if token != "params_json" && token != "project_path" && token != "triple" {
                return Err(CliError::Local {
                    detail: format!(
                        "offline tool template references unsupported token {{{token}}}"
                    ),
                });
            }
            rest = &rest[start + 1 + end + 1..];
        } else {
            return Err(CliError::Local {
                detail: "offline tool template contains an unterminated token".into(),
            });
        }
    }

    let mut out = template.replace("{params_json}", params_json);
    out = out.replace("{project_path}", project_path);
    Ok(out)
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

fn exec_inherited(
    tool_ref: &str,
    cmd: &Path,
    args: &[String],
    cwd: Option<&str>,
    envs: &[(String, String)],
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
// Alias / verb loading from bundle roots
// ---------------------------------------------------------------------------

fn match_alias<'a>(
    argv: &[String],
    aliases: &'a [LoadedAliasDescriptor],
) -> anyhow::Result<Option<(&'a LoadedAliasDescriptor, usize)>> {
    for len in (1..=argv.len()).rev() {
        let prefix = &argv[..len];
        let matches: Vec<&LoadedAliasDescriptor> = aliases
            .iter()
            .filter(|alias| alias.def.tokens == prefix)
            .collect();
        match matches.len() {
            0 => {}
            1 => return Ok(Some((matches[0], len))),
            _ => {
                bail!("duplicate alias descriptor matches for tokens {:?}", prefix)
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;

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
            let user = tmp.path().join("user");
            let project = tmp.path().join("project");
            std::fs::create_dir_all(project.join(ryeos_engine::AI_DIR)).unwrap();
            let trust_dir = user
                .join(ryeos_engine::AI_DIR)
                .join("config")
                .join("keys")
                .join("trusted");
            std::fs::create_dir_all(&trust_dir).unwrap();
            let key = SigningKey::generate(&mut OsRng);
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();
            let dev_trust = std::fs::read_to_string(
                workspace_root()
                    .join(".dev-keys")
                    .join("PUBLISHER_DEV_TRUST.toml"),
            )
            .unwrap();
            let dev_trust = ryeos_engine::trust::PublisherTrustDoc::parse(&dev_trust).unwrap();
            let dev_key = dev_trust.decode_verifying_key().unwrap();
            ryeos_engine::trust::pin_key(&dev_key, "ryeos-dev", &trust_dir, None).unwrap();
            std::env::set_var("USER_SPACE", &user);

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
            this.write_registration();
            this.write_bundle_registration("core", &core_bundle);
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

        fn write_standard_descriptors(&self, offline_execute_line: Option<&str>) {
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("node")
                    .join("verbs")
                    .join("custom.yaml"),
                "category: verbs\nsection: verbs\nname: custom\ndescription: Custom offline command\nexecute: service:custom\naliases:\n  - tokens: [\"custom\"]\n    description: Custom offline command\n    positional_forms:\n      - slots:\n          - field: name\n",
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

        fn write_bundle_registration(&self, id: &str, bundle_root: &Path) {
            let path = self
                .system
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("bundles")
                .join(format!("{id}.yaml"));
            self.write_signed(
                &path,
                &format!(
                    "kind: node\nsection: bundles\nid: {id}\npath: {}\n",
                    bundle_root.display()
                ),
            );
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

    #[test]
    fn offline_dispatch_executes_descriptor_declared_tool() {
        let fixture = Fixture::new();
        let argv = vec!["custom".to_string(), "leo".to_string()];

        let result = expect_json(
            try_offline_dispatch(&argv, &fixture.system, &fixture.project_str())
                .unwrap()
                .expect("handled offline"),
        );

        assert_eq!(result["name"], "leo");
        assert_eq!(result["project_path"], fixture.project_str());
    }

    #[test]
    fn offline_service_without_tool_impl_errors_loudly() {
        let fixture = Fixture::new();
        fixture.write_standard_descriptors(None);

        let err = try_offline_dispatch(&["custom".to_string()], &fixture.system, ".").unwrap_err();

        match err {
            CliError::Local { detail } => {
                assert!(detail.contains("offline_execute: tool:<id>"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn offline_dispatch_maps_project_flag_to_project_path_schema() {
        let fixture = Fixture::new();

        let result = expect_json(
            try_offline_dispatch(
                &[
                    "custom".to_string(),
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
    fn duplicate_alias_matches_error_loudly() {
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
            &format!(
                "kind: node\nsection: bundles\nid: second\npath: {}\n",
                second_bundle.display()
            ),
        );
        fixture.write_signed(
            &second_bundle
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("verbs")
                .join("custom.yaml"),
            "category: verbs\nsection: verbs\nname: custom\ndescription: Other offline command\nexecute: service:other\naliases:\n  - tokens: [\"custom\"]\n    description: Other offline command\n",
        );

        let err = try_offline_dispatch(&["custom".to_string()], &fixture.system, ".").unwrap_err();
        match err {
            CliError::Local { detail } => {
                assert!(
                    detail.contains("node config aliases have duplicate tokens"),
                    "{detail}"
                );
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

        let err = try_offline_dispatch(&["custom".to_string()], &fixture.system, ".").unwrap_err();
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
            try_offline_dispatch(
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
                .join("verbs")
                .join("capture.yaml"),
            "category: verbs\nsection: verbs\nname: capture\ndescription: Capture offline client command\nexecute: client:custom/capture\naliases:\n  - tokens: [\"capture\"]\n    description: Capture offline client command\n",
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

        let outcome = try_offline_dispatch(
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
        assert_eq!(lines[1..], ["--surface", "main", "--mock"]);
    }
}
