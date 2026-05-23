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
/// Receives validated parameters as a JSON Value, returns a JSON Value result.
/// This is the implementation table — NOT the source of truth for which
/// commands may run offline (that's the service descriptor's job).
type OfflineHandler = fn(Value) -> Result<Value>;

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
    m
}

// ── In-process handlers ────────────────────────────────────────────

fn offline_verify(params: Value) -> Result<Value> {
    let p: ryeos_tools::actions::inspect::verify::VerifyParams =
        serde_json::from_value(params).context("invalid verify params")?;
    let engine = ryeos_tools::actions::inspect::boot(
        p.project_path.as_deref().map(Path::new),
    )
    .context("boot offline engine for verify")?;
    ryeos_tools::actions::inspect::verify::run_verify(p, &engine)
        .context("offline verify failed")
}

fn offline_fetch(params: Value) -> Result<Value> {
    let p: ryeos_tools::actions::inspect::fetch::FetchParams =
        serde_json::from_value(params).context("invalid fetch params")?;
    let engine = ryeos_tools::actions::inspect::boot(
        p.project_path.as_deref().map(Path::new),
    )
    .context("boot offline engine for fetch")?;
    ryeos_tools::actions::inspect::fetch::run_fetch(p, &engine)
        .context("offline fetch failed")
}

fn offline_sign(params: Value) -> Result<Value> {
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
    let params = bind_params(tail, &alias, &service, project_path).map_err(|e| CliError::Local {
        detail: format!("{e:#}"),
    })?;

    // 7. Run the handler
    let result = (endpoint.handler)(params).map_err(|e| CliError::Local {
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
