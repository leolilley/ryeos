//! Descriptor-driven offline command dispatch.
//!
//! Commands that declare `availability: offline` in their service descriptor
//! can run in-process without a daemon. This module:
//!
//! 1. Resolves alias tokens → verb → service descriptor from disk YAMLs
//! 2. Checks the service descriptor's `availability` field
//! 3. Resolves the descriptor-declared offline tool implementation
//! 4. Executes that tool descriptor locally and returns the result
//!
//! Service descriptors are the source of truth for both whether a command
//! may run offline and which tool implements the offline path. The CLI does
//! not keep an endpoint → handler table; adding an offline service means
//! adding/updating descriptors that point at a local tool.
//!
//! Dispatch requires:
//!   - service.availability == offline|both
//!   - service.offline_execute or verb.execute resolves to tool:<id>
//!   - that tool descriptor declares a subprocess implementation
//!
//! If descriptor says offline but has no local tool implementation → clear error.
//! If descriptor does not say offline → returns None (fall through to daemon).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::error::CliError;

// ── Service descriptor (subset of fields we need) ──────────────────

/// Parsed subset of a service YAML descriptor.
#[derive(Debug, serde::Deserialize)]
struct ServiceDescriptor {
    /// Service endpoint name (e.g. "verify", "fetch", "sign").
    #[allow(dead_code)]
    endpoint: String,
    /// Whether this service may run offline: "offline", "daemon", or "both".
    #[serde(default)]
    availability: Option<String>,
    /// Descriptor-declared local implementation for offline dispatch.
    ///
    /// Shape: `tool:<id>`, e.g. `tool:ryeos/core/bundle/publish`.
    /// If absent, a verb that already executes a `tool:<id>` can be used
    /// directly. Service-backed offline commands should always declare this.
    #[serde(default)]
    offline_execute: Option<String>,
    /// Input schema (field name → type string).
    #[serde(default)]
    schema: HashMap<String, String>,
}

// ── Alias descriptor (subset) ──────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct AliasDescriptor {
    tokens: Vec<String>,
    verb: String,
    /// Field to bind from the first positional arg.
    #[serde(default)]
    positional_field: Option<String>,
    #[allow(dead_code)]
    /// How --project is resolved.
    #[serde(default)]
    project_resolution: String,
}

// ── Verb descriptor (subset) ───────────────────────────────────────

/// Parsed subset of a verb YAML descriptor.
#[derive(Debug, serde::Deserialize)]
struct VerbDescriptor {
    /// Execution target ref: `service:...` or `tool:...`.
    execute: String,
}

// ── Tool descriptor (subset of fields we need) ─────────────────────

#[derive(Debug, serde::Deserialize)]
struct ToolDescriptor {
    #[serde(default)]
    executor_id: Option<String>,
    config: ToolConfig,
}

#[derive(Debug, serde::Deserialize)]
struct ToolConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    input_data: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

// ── Main entry point ───────────────────────────────────────────────

/// Try to dispatch a command through the offline descriptor path.
///
/// Returns `Ok(Some(result))` if the command was handled offline.
/// Returns `Ok(None)` if the command is not offline-capable (caller
/// should fall through to daemon dispatch).
/// Returns `Err` if the command is offline-capable but something went wrong.
pub fn try_offline_dispatch(
    argv: &[String],
    system_space_dir: &Path,
    project_path: &str,
) -> Result<Option<Value>, CliError> {
    // 1. Match alias from disk
    let aliases = load_aliases(system_space_dir);
    let Some((alias, consumed)) = match_alias(argv, &aliases) else {
        return Ok(None);
    };

    // 2. Look up verb — confirm the verb descriptor exists
    let Some(verb) = load_verb(system_space_dir, &alias.verb) else {
        return Ok(None);
    };

    // 3. Resolve service descriptor — prefer verb's execute ref, fall back
    //    to verb-name-as-service lookup.
    //    Verb names use dashes ("bundle-verify") but service paths use
    //    slashes ("bundle/verify.yaml"), so the execute ref is authoritative.
    let service_path = resolve_service_path(system_space_dir, &alias.verb, &verb);
    let Some(service_path) = service_path else {
        // No service descriptor for this verb — not our concern
        return Ok(None);
    };
    let Some(service) = read_yaml::<ServiceDescriptor>(&service_path) else {
        return Ok(None);
    };

    // 4. Check availability — only proceed if descriptor declares offline
    let is_offline = service
        .availability
        .as_deref()
        .map(|a| a == "offline" || a == "both")
        .unwrap_or(false);

    if !is_offline {
        return Ok(None);
    }

    // 5. Resolve the descriptor-declared local implementation.
    let Some(offline_execute) = resolve_offline_execute(&service, &verb) else {
        return Err(CliError::Local {
            detail: format!(
                "service `{}` is declared offline-capable, but its descriptor \
                 does not declare a local tool implementation (`offline_execute: tool:<id>`)",
                alias.verb
            ),
        });
    };

    // 6. Bind parameters from tail args
    let tail = &argv[consumed..];
    let params =
        bind_params(tail, &alias, &service, project_path).map_err(|e| CliError::Local {
            detail: format!("{e:#}"),
        })?;

    // 7. Run the descriptor-declared tool.
    let result = execute_offline_tool(&offline_execute, params, system_space_dir, project_path)
        .map_err(|e| CliError::Local {
            detail: format!("{e:#}"),
        })?;

    Ok(Some(result))
}

fn resolve_offline_execute(service: &ServiceDescriptor, verb: &VerbDescriptor) -> Option<String> {
    service.offline_execute.clone().or_else(|| {
        verb.execute
            .starts_with("tool:")
            .then(|| verb.execute.clone())
    })
}

fn execute_offline_tool(
    tool_ref: &str,
    params: Value,
    system_space_dir: &Path,
    project_path: &str,
) -> Result<Value> {
    let tool_path = find_tool_path(system_space_dir, tool_ref)
        .with_context(|| format!("resolve offline tool descriptor `{tool_ref}`"))?;
    let tool: ToolDescriptor = read_yaml(&tool_path)
        .with_context(|| format!("parse offline tool descriptor `{}`", tool_path.display()))?;

    match tool.executor_id.as_deref() {
        Some("@subprocess") | Some("tool:ryeos/core/subprocess/execute") => {}
        other => bail!(
            "offline tool `{tool_ref}` must use @subprocess executor, got {:?}",
            other
        ),
    }

    let params_json = serde_json::to_string(&params).context("serialize offline params")?;
    let cmd_template = expand_template(&tool.config.command, &params_json, project_path)?;
    let cmd = resolve_command(&cmd_template, &tool_path, system_space_dir, project_path)?;
    let args = tool
        .config
        .args
        .iter()
        .map(|arg| expand_template(arg, &params_json, project_path))
        .collect::<Result<Vec<_>>>()?;
    let stdin_data = tool
        .config
        .input_data
        .as_deref()
        .map(|input| expand_template(input, &params_json, project_path))
        .transpose()?;
    let cwd = match tool.config.cwd.as_deref() {
        Some(cwd) => Some(expand_template(cwd, &params_json, project_path)?),
        None => Some(std::env::current_dir()?.to_string_lossy().into_owned()),
    };
    let mut envs: Vec<(String, String)> = tool
        .config
        .env
        .iter()
        .map(|(k, v)| expand_template(v, &params_json, project_path).map(|v| (k.clone(), v)))
        .collect::<Result<Vec<_>>>()?;
    envs.push((
        "RYEOS_SYSTEM_SPACE_DIR".to_string(),
        system_space_dir.to_string_lossy().into_owned(),
    ));

    let result = lillux::run(lillux::SubprocessRequest {
        cmd: cmd.to_string_lossy().into_owned(),
        args,
        cwd,
        envs,
        stdin_data,
        timeout: tool.config.timeout_secs.unwrap_or(60) as f64,
    });

    if !result.success {
        bail!(
            "offline tool `{tool_ref}` failed with exit {:?}\nstdout:\n{}\nstderr:\n{}",
            result.exit_code,
            result.stdout,
            result.stderr
        );
    }

    serde_json::from_str(&result.stdout).or_else(|_| Ok(Value::String(result.stdout)))
}

fn expand_template(template: &str, params_json: &str, project_path: &str) -> Result<String> {
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        if let Some(end) = rest[start + 1..].find('}') {
            let token = &rest[start + 1..start + 1 + end];
            if token != "params_json" && token != "project_path" {
                bail!("offline tool template references unsupported token {{{token}}}");
            }
            rest = &rest[start + 1 + end + 1..];
        } else {
            bail!("offline tool template contains an unterminated token");
        }
    }

    let mut out = template.replace("{params_json}", params_json);
    out = out.replace("{project_path}", project_path);
    Ok(out)
}

fn resolve_command(
    command: &str,
    tool_path: &Path,
    system_space_dir: &Path,
    project_path: &str,
) -> Result<PathBuf> {
    if !command.starts_with("bin:") {
        return Ok(PathBuf::from(command));
    }

    let bundle_root = find_bundle_root(tool_path).with_context(|| {
        format!(
            "resolve bundle root for offline tool descriptor {}",
            tool_path.display()
        )
    })?;
    let user_root = ryeos_engine::roots::user_root().ok();
    let system_roots = installed_bundle_roots(system_space_dir);
    let trust_store = ryeos_engine::trust::TrustStore::load_three_tier(
        Some(Path::new(project_path)),
        user_root.as_deref(),
        &system_roots,
    )
    .context("load trust store for offline binary dispatch")?;
    let resolved = ryeos_engine::binary_resolver::resolve_bundle_binary_ref(
        command,
        &bundle_root,
        |fp| trust_store.is_trusted(fp),
        ryeos_engine::resolution::TrustClass::TrustedSystem,
    )
    .with_context(|| format!("resolve offline binary `{command}`"))?;
    Ok(resolved.absolute_path)
}

fn find_bundle_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.join(ryeos_engine::AI_DIR).is_dir())
        .map(Path::to_path_buf)
}

fn installed_bundle_roots(system_space_dir: &Path) -> Vec<PathBuf> {
    let bundles_dir = system_space_dir.join(ryeos_engine::AI_DIR).join("bundles");
    let mut roots = Vec::new();
    if let Ok(entries) = std::fs::read_dir(bundles_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                roots.push(path);
            }
        }
    }
    ryeos_engine::roots::system_roots(&roots)
}

// ── Parameter binding ──────────────────────────────────────────────

/// Bind the tail argv into a JSON object based on alias + service schema.
fn bind_params(
    tail: &[String],
    alias: &AliasDescriptor,
    service: &ServiceDescriptor,
    project_path: &str,
) -> Result<Value> {
    let mut obj = serde_json::Map::new();

    // Bind positional arg (e.g. `ryeos sign knowledge:foo` → item_ref)
    if let Some(pos_field) = &alias.positional_field {
        if !tail.is_empty() {
            obj.insert(pos_field.clone(), Value::String(tail[0].clone()));
        }
    }

    // Parse --flag value pairs from the tail
    let mut i = 0;
    while i < tail.len() {
        let tok = &tail[i];
        if let Some(key) = tok.strip_prefix("--") {
            let mut key = key.replace('-', "_");
            if key == "project"
                && service.schema.contains_key("project_path")
                && !service.schema.contains_key("project")
            {
                key = "project_path".to_string();
            }
            if i + 1 < tail.len() && !tail[i + 1].starts_with('-') {
                let val = &tail[i + 1];
                obj.insert(key, Value::String(val.clone()));
                i += 2;
            } else {
                // Boolean flag
                obj.insert(key, Value::Bool(true));
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    // Inject project_path from --project flag or CLI default
    if !obj.contains_key("project") && !obj.contains_key("project_path") {
        // Use the service schema to determine the field name
        let project_key = if service.schema.contains_key("project_path") {
            "project_path"
        } else if service.schema.contains_key("project") {
            "project"
        } else {
            // Don't inject if the schema doesn't declare it
            project_path
        };
        if service.schema.contains_key(project_key) {
            obj.insert(
                project_key.to_string(),
                Value::String(project_path.to_string()),
            );
        }
    }

    Ok(Value::Object(obj))
}

// ── Disk descriptor loading ────────────────────────────────────────

fn load_aliases(system_space_dir: &Path) -> Vec<AliasDescriptor> {
    let mut out = Vec::new();
    let bundles_dir = system_space_dir.join(ryeos_engine::AI_DIR).join("bundles");
    let Ok(entries) = std::fs::read_dir(&bundles_dir) else {
        return out;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str.ends_with(".backup.prev") {
            continue;
        }
        let aliases_dir = entry
            .path()
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("aliases");
        let Ok(files) = std::fs::read_dir(aliases_dir) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("yaml") | Some("yml")
            ) {
                continue;
            }
            if let Some(alias) = read_yaml::<AliasDescriptor>(&path) {
                out.push(alias);
            }
        }
    }
    out
}

fn load_verb(system_space_dir: &Path, verb_name: &str) -> Option<VerbDescriptor> {
    let bundles_dir = system_space_dir.join(ryeos_engine::AI_DIR).join("bundles");
    let Ok(entries) = std::fs::read_dir(&bundles_dir) else {
        return None;
    };

    for entry in entries.flatten() {
        let path = entry
            .path()
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("verbs")
            .join(format!("{verb_name}.yaml"));
        if let Some(verb) = read_yaml::<VerbDescriptor>(&path) {
            return Some(verb);
        }
    }
    None
}

/// Resolve the service descriptor path for a verb.
///
/// Strategy:
///  1. If the verb's `execute` field starts with `service:`, use that ref
///     (e.g. `service:bundle/verify` → `services/bundle/verify.yaml`).
///  2. Otherwise, try the verb name directly (e.g. `sign` → `services/sign.yaml`),
///     and also try converting dashes to slashes (e.g. `bundle-verify` →
///     `services/bundle/verify.yaml`).
fn resolve_service_path(
    system_space_dir: &Path,
    verb_name: &str,
    verb: &VerbDescriptor,
) -> Option<std::path::PathBuf> {
    if let Some(service_ref) = verb.execute.strip_prefix("service:") {
        // Authoritative path via execute ref
        find_service_path(system_space_dir, service_ref)
    } else {
        // Fallback: try verb name as-is, then with dash→slash
        find_service_path(system_space_dir, verb_name).or_else(|| {
            let service_rel = verb_name.replace('-', "/");
            find_service_path(system_space_dir, &service_rel)
        })
    }
}

/// Find a service descriptor file by relative service name.
/// Looks for `.ai/services/{name}.yaml` in each installed bundle.
/// Appends `.yaml` if the name doesn't already end with it.
fn find_service_path(system_space_dir: &Path, service_rel: &str) -> Option<std::path::PathBuf> {
    let bundles_dir = system_space_dir.join(ryeos_engine::AI_DIR).join("bundles");
    let Ok(entries) = std::fs::read_dir(&bundles_dir) else {
        return None;
    };

    let file_name = if service_rel.ends_with(".yaml") || service_rel.ends_with(".yml") {
        service_rel.to_string()
    } else {
        format!("{}.yaml", service_rel)
    };

    for entry in entries.flatten() {
        let path = entry
            .path()
            .join(ryeos_engine::AI_DIR)
            .join("services")
            .join(&file_name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn find_tool_path(system_space_dir: &Path, tool_ref: &str) -> Option<PathBuf> {
    let rel = tool_ref.strip_prefix("tool:")?;
    let bundles_dir = system_space_dir.join(ryeos_engine::AI_DIR).join("bundles");
    let Ok(entries) = std::fs::read_dir(&bundles_dir) else {
        return None;
    };

    let file_name = if rel.ends_with(".yaml") || rel.ends_with(".yml") {
        rel.to_string()
    } else {
        format!("{}.yaml", rel)
    };

    for entry in entries.flatten() {
        let path = entry
            .path()
            .join(ryeos_engine::AI_DIR)
            .join("tools")
            .join(&file_name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn match_alias<'a>(
    argv: &[String],
    aliases: &'a [AliasDescriptor],
) -> Option<(&'a AliasDescriptor, usize)> {
    for len in (1..=argv.len()).rev() {
        let prefix: Vec<&str> = argv[..len].iter().map(|s| s.as_str()).collect();
        if let Some(alias) = aliases.iter().find(|a| {
            a.tokens.len() == prefix.len()
                && a.tokens.iter().zip(prefix.iter()).all(|(t, p)| t == p)
        }) {
            return Some((alias, len));
        }
    }
    None
}

fn read_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&content).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp
            .path()
            .join(ryeos_engine::AI_DIR)
            .join("bundles")
            .join("test");

        write(
            &root
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("aliases")
                .join("custom.yaml"),
            r#"
tokens: ["custom"]
verb: custom
description: test custom offline command
"#,
        );
        write(
            &root
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("verbs")
                .join("custom.yaml"),
            r#"
name: custom
execute: service:custom
"#,
        );
        write(
            &root
                .join(ryeos_engine::AI_DIR)
                .join("services")
                .join("custom.yaml"),
            r#"
kind: service
endpoint: custom
availability: offline
offline_execute: tool:custom/echo
schema:
  name: string?
"#,
        );
        write(
            &root
                .join(ryeos_engine::AI_DIR)
                .join("tools")
                .join("custom")
                .join("echo.yaml"),
            r#"
executor_id: "@subprocess"
config:
  command: "cat"
  input_data: "{params_json}"
"#,
        );

        tmp
    }

    #[test]
    fn offline_dispatch_executes_descriptor_declared_tool() {
        let tmp = fixture();
        let argv = vec![
            "custom".to_string(),
            "--name".to_string(),
            "leo".to_string(),
        ];

        let result = try_offline_dispatch(&argv, tmp.path(), ".")
            .unwrap()
            .expect("handled offline");

        assert_eq!(result["name"], "leo");
    }

    #[test]
    fn offline_service_without_tool_impl_errors_loudly() {
        let tmp = fixture();
        let service_path = tmp
            .path()
            .join(ryeos_engine::AI_DIR)
            .join("bundles")
            .join("test")
            .join(ryeos_engine::AI_DIR)
            .join("services")
            .join("custom.yaml");
        write(
            &service_path,
            r#"
kind: service
endpoint: custom
availability: offline
schema: {}
"#,
        );

        let err = try_offline_dispatch(&["custom".to_string()], tmp.path(), ".").unwrap_err();

        match err {
            CliError::Local { detail } => {
                assert!(detail.contains("offline_execute: tool:<id>"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn offline_dispatch_maps_project_flag_to_project_path_schema() {
        let tmp = fixture();
        let service_path = tmp
            .path()
            .join(ryeos_engine::AI_DIR)
            .join("bundles")
            .join("test")
            .join(ryeos_engine::AI_DIR)
            .join("services")
            .join("custom.yaml");
        write(
            &service_path,
            r#"
kind: service
endpoint: custom
availability: offline
offline_execute: tool:custom/echo
schema:
  project_path: string?
"#,
        );

        let result = try_offline_dispatch(
            &[
                "custom".to_string(),
                "--project".to_string(),
                "/tmp/project".to_string(),
            ],
            tmp.path(),
            ".",
        )
        .unwrap()
        .expect("handled offline");

        assert_eq!(result["project_path"], "/tmp/project");
        assert!(result.get("project").is_none());
    }
}
