//! Descriptor-driven offline command dispatch.
//!
//! Commands that declare `availability: offline` in their service descriptor
//! can run in-process without a daemon. This module:
//!
//! 1. Resolves alias tokens → verb → service descriptor from disk YAMLs
//! 2. Checks the service descriptor's `availability` field
//! 3. Looks up an in-process handler in the offline endpoint registry
//! 4. Runs the handler and returns the result
//!
//! The service descriptor is the source of truth for whether a command
//! may run offline. The offline endpoint registry only answers whether
//! this CLI binary has an implementation for that endpoint.
//!
//! Dispatch requires both:
//!   - service.availability == offline (descriptor declares it)
//!   - offline_endpoints.contains(endpoint) (CLI has an implementation)
//!
//! If descriptor says offline but binary lacks handler → clear error.
//! If descriptor does not say offline → returns None (fall through to daemon).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::error::CliError;

// ── Service descriptor (subset of fields we need) ──────────────────

/// Parsed subset of a service YAML descriptor.
#[derive(Debug, serde::Deserialize)]
struct ServiceDescriptor {
    /// Service endpoint name (e.g. "verify", "fetch", "sign").
    endpoint: String,
    /// Whether this service may run offline: "offline", "daemon", or "both".
    #[serde(default)]
    availability: Option<String>,
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

#[derive(Debug, serde::Deserialize)]
struct VerbDescriptor {
    /// Execution target ref: `service:...` or `tool:...`.
    #[allow(dead_code)]
    execute: String,
}

// ── Offline endpoint registry ──────────────────────────────────────

/// In-process handler for an offline-capable endpoint.
///
/// Receives validated parameters as a JSON Value and context about the
/// system space, returns a JSON Value result. For most endpoints this is
/// a simple request/response. For `client.open`, this handler execs the
/// client binary and does not return.
type OfflineHandler = fn(Value, &Path) -> Result<Value>;

struct OfflineEndpoint {
    handler: OfflineHandler,
}

/// Returns the CLI's offline endpoint registry.
///
/// Key = endpoint name from the service descriptor (e.g. "verify", "fetch", "sign").
fn offline_endpoints() -> HashMap<&'static str, OfflineEndpoint> {
    let mut m = HashMap::new();
    m.insert(
        "verify",
        OfflineEndpoint {
            handler: offline_verify,
        },
    );
    m.insert(
        "fetch",
        OfflineEndpoint {
            handler: offline_fetch,
        },
    );
    m.insert(
        "sign",
        OfflineEndpoint {
            handler: offline_sign,
        },
    );
    m.insert(
        "client.open",
        OfflineEndpoint {
            handler: offline_client_open,
        },
    );
    m
}

// ── In-process handlers ────────────────────────────────────────────

fn offline_verify(params: Value, _system_space_dir: &Path) -> Result<Value> {
    let p: ryeos_tools::actions::inspect::verify::VerifyParams =
        serde_json::from_value(params).context("invalid verify params")?;
    let engine = ryeos_tools::actions::inspect::boot(
        p.project_path.as_deref().map(Path::new),
    )
    .context("boot offline engine for verify")?;
    ryeos_tools::actions::inspect::verify::run_verify(p, &engine)
        .context("offline verify failed")
}

fn offline_fetch(params: Value, _system_space_dir: &Path) -> Result<Value> {
    let p: ryeos_tools::actions::inspect::fetch::FetchParams =
        serde_json::from_value(params).context("invalid fetch params")?;
    let engine = ryeos_tools::actions::inspect::boot(
        p.project_path.as_deref().map(Path::new),
    )
    .context("boot offline engine for fetch")?;
    ryeos_tools::actions::inspect::fetch::run_fetch(p, &engine)
        .context("offline fetch failed")
}

fn offline_sign(params: Value, _system_space_dir: &Path) -> Result<Value> {
    let item_ref = params
        .get("item_ref")
        .and_then(|v| v.as_str())
        .context("item_ref required")?
        .to_string();
    let source = params
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("project");
    let project = params
        .get("project")
        .and_then(|v| v.as_str())
        .map(|s| std::path::PathBuf::from(s))
        .or_else(|| std::env::current_dir().ok())
        .context("resolve project directory")?;

    let source =
        ryeos_tools::actions::sign::SignSource::parse(source).context("invalid source")?;
    let report = ryeos_tools::actions::sign::run_sign(&item_ref, Some(&project), source)
        .context("offline sign failed")?;
    serde_json::to_value(report).context("serialize sign result")
}

// ── client.open handler ────────────────────────────────────────────

/// Parameters for the client.open offline service.
#[derive(Debug, serde::Deserialize)]
struct ClientOpenParams {
    client_ref: Option<String>,
    #[allow(dead_code)]
    renderer: Option<String>,
    surface: Option<String>,
    surface_file: Option<String>,
    mock: Option<bool>,
    #[allow(dead_code)]
    read_only: Option<bool>,
    #[allow(dead_code)]
    project: Option<String>,
    /// Injected by dispatcher — the verb name that triggered this handler.
    #[serde(rename = "_verb")]
    _verb: Option<String>,
}

/// Offline handler for `client.open` — resolves a descriptor from
/// installed bundles and execs the declared binary.
///
/// Fully generic: reads the descriptor as raw value, pulls launch config,
/// and execs. No typed structs — the CLI does not define what a "client" is.
fn offline_client_open(params: Value, system_space_dir: &Path) -> Result<Value> {
    let p: ClientOpenParams =
        serde_json::from_value(params).context("invalid client.open params")?;

    // 1. Determine client_ref from params
    //    If not explicitly provided, derive from the verb: verb "tui" → "client:ryeos/tui"
    let client_ref = match &p.client_ref {
        Some(r) => r.clone(),
        None => {
            let verb = p._verb.as_deref().unwrap_or("unknown");
            format!("client:ryeos/{}", verb)
        }
    };

    // 2. Parse the canonical ref
    let parsed = ryeos_engine::canonical_ref::CanonicalRef::parse(&client_ref)
        .map_err(|e| anyhow::anyhow!("invalid client ref '{client_ref}': {e}"))?;

    if parsed.kind != "client" {
        anyhow::bail!(
            "ref '{client_ref}' is kind '{}', expected 'client'",
            parsed.kind
        );
    }

    // 3. Find the descriptor YAML in installed bundles and read as raw value
    let descriptor = find_bundle_item(system_space_dir, "clients", &parsed.bare_id, ".yaml")
        .context(format!("failed to resolve '{client_ref}'"))?;

    // 4. Read launch config from the descriptor
    let launch = descriptor
        .get("launch")
        .context("descriptor missing 'launch' block")?;
    let mode = launch
        .get("mode")
        .and_then(|v| v.as_str())
        .context("launch missing 'mode' field")?;

    match mode {
        "cli_exec" => {
            let binary_ref = launch
                .get("binary_ref")
                .and_then(|v| v.as_str())
                .context("cli_exec launch missing 'binary_ref'")?;
            let args_map = launch
                .get("args")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();

            exec_binary(binary_ref, &args_map, &p, system_space_dir, &parsed)
        }
        other => anyhow::bail!("launch mode '{other}' not yet implemented"),
    }
}

/// Find an item file in installed bundles by kind directory and bare_id.
///
/// Searches `<bundle>/.ai/<kind_dir>/<bare_id>.<ext>` across all bundles.
fn find_bundle_item(
    system_space_dir: &Path,
    kind_dir: &str,
    bare_id: &str,
    ext: &str,
) -> Result<Value> {
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let entries = std::fs::read_dir(&bundles_dir)
        .context(format!("no bundles directory at {}", bundles_dir.display()))?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str.ends_with(".backup.prev") {
            continue;
        }

        let item_path = entry
            .path()
            .join(".ai")
            .join(kind_dir)
            .join(bare_id)
            .with_extension(ext.trim_start_matches('.'));

        if item_path.is_file() {
            let content = std::fs::read_to_string(&item_path)
                .context(format!("read {}", item_path.display()))?;
            let value: Value = serde_yaml::from_str(&content)
                .context(format!("parse {}", item_path.display()))?;
            return Ok(value);
        }
    }

    anyhow::bail!("'{}' not found in any installed bundle", bare_id)
}

/// Resolve a binary_ref relative to the bundle containing the descriptor.
fn resolve_binary_in_bundle(
    binary_ref: &str,
    system_space_dir: &Path,
    kind_dir: &str,
    bare_id: &str,
) -> Result<std::path::PathBuf> {
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let entries = std::fs::read_dir(&bundles_dir)
        .context("no bundles directory")?;

    for entry in entries.flatten() {
        let item_path = entry
            .path()
            .join(".ai")
            .join(kind_dir)
            .join(bare_id)
            .with_extension("yaml");

        if item_path.is_file() {
            let binary_path = entry.path().join(binary_ref);
            if binary_path.is_file() {
                return Ok(binary_path);
            } else {
                anyhow::bail!("binary '{}' not found at {}", binary_ref, binary_path.display());
            }
        }
    }

    anyhow::bail!("binary '{}' not found in any bundle", binary_ref)
}

/// Translate params to argv using the descriptor's args mapping and exec.
fn exec_binary(
    binary_ref: &str,
    args_map: &serde_json::Map<String, Value>,
    params: &ClientOpenParams,
    system_space_dir: &Path,
    canonical_ref: &ryeos_engine::canonical_ref::CanonicalRef,
) -> Result<Value> {
    let binary_path = resolve_binary_in_bundle(
        binary_ref,
        system_space_dir,
        "clients",
        &canonical_ref.bare_id,
    )?;

    // Build argv from args mapping + params
    let mut argv: Vec<String> = Vec::new();
    let param_fields: Vec<(&str, Option<&String>, bool)> = vec![
        ("surface", params.surface.as_ref(), false),
        ("surface_file", params.surface_file.as_ref(), false),
        ("mock", None, params.mock.unwrap_or(false)),
        ("read_only", None, params.read_only.unwrap_or(false)),
    ];

    for (field, value, is_bool) in param_fields {
        let flag = match args_map.get(field).and_then(|v| v.as_str()) {
            Some(f) => f.to_string(),
            None => continue,
        };
        if is_bool {
            argv.push(flag);
        } else if let Some(val) = value {
            argv.push(flag);
            argv.push(val.clone());
        }
    }

    // Exec, replacing the current process
    eprintln!("info: launching {} via {}", canonical_ref, binary_path.display());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let result = std::process::Command::new(&binary_path)
            .args(&argv)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .exec();
        // exec only returns on error
        anyhow::bail!("failed to exec '{}': {}", binary_path.display(), result);
    }

    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(&binary_path)
            .args(&argv)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .map_err(|e| anyhow::anyhow!("failed to exec '{}': {}", binary_path.display(), e))?;
        std::process::exit(status.code().unwrap_or(1));
    }
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
    let Some(_verb) = load_verb(system_space_dir, &alias.verb) else {
        return Ok(None);
    };

    // 3. Resolve service descriptor by verb name
    //    Service files are at .ai/services/{name}.yaml with endpoint: {name}
    let service_path = find_service_path(system_space_dir, &alias.verb);
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

    // 5. Check handler exists in this binary
    let endpoints = offline_endpoints();
    let Some(endpoint) = endpoints.get(service.endpoint.as_str()) else {
        return Err(CliError::Local {
            detail: format!(
                "service `{}` is declared offline-capable, but this ryeos binary \
                 has no offline handler for endpoint `{}`",
                alias.verb, service.endpoint
            ),
        });
    };

    // 6. Bind parameters from tail args
    let tail = &argv[consumed..];
    let mut params = bind_params(tail, &alias, &service, project_path).map_err(|e| CliError::Local {
        detail: format!("{e:#}"),
    })?;

    // Inject verb name so handlers can derive defaults from it
    if let Some(obj) = params.as_object_mut() {
        obj.entry("_verb".to_string())
            .or_insert_with(|| Value::String(alias.verb.clone()));
    }

    // 7. Run the handler
    let result = (endpoint.handler)(params, system_space_dir).map_err(|e| CliError::Local {
        detail: format!("{e:#}"),
    })?;

    Ok(Some(result))
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
            let key = key.replace('-', "_");
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
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let Ok(entries) = std::fs::read_dir(&bundles_dir) else {
        return out;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str.ends_with(".backup.prev") {
            continue;
        }
        let aliases_dir = entry.path().join(".ai").join("node").join("aliases");
        let Ok(files) = std::fs::read_dir(aliases_dir) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if !matches!(path.extension().and_then(|s| s.to_str()), Some("yaml") | Some("yml")) {
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
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let Ok(entries) = std::fs::read_dir(&bundles_dir) else {
        return None;
    };

    for entry in entries.flatten() {
        let path = entry
            .path()
            .join(".ai")
            .join("node")
            .join("verbs")
            .join(format!("{verb_name}.yaml"));
        if let Some(verb) = read_yaml::<VerbDescriptor>(&path) {
            return Some(verb);
        }
    }
    None
}

/// Find a service descriptor file by verb name.
/// Looks for `.ai/services/{verb_name}.yaml` in each bundle.
fn find_service_path(system_space_dir: &Path, verb_name: &str) -> Option<std::path::PathBuf> {
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let Ok(entries) = std::fs::read_dir(&bundles_dir) else {
        return None;
    };

    for entry in entries.flatten() {
        let path = entry
            .path()
            .join(".ai")
            .join("services")
            .join(format!("{verb_name}.yaml"));
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
            a.tokens.len() == prefix.len() && a.tokens.iter().zip(prefix.iter()).all(|(t, p)| t == p)
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
