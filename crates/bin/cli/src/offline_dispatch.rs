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

/// Parsed subset of a verb YAML descriptor.
#[derive(Debug, serde::Deserialize)]
struct VerbDescriptor {
    /// Execution target ref: `service:...` or `tool:...`.
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
/// Key = endpoint name from the service descriptor (e.g. "verify", "fetch", "sign",
/// "bundle.verify", "bundle.publish").
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
        "bundle.verify",
        OfflineEndpoint {
            handler: offline_bundle_verify,
        },
    );
    m.insert(
        "bundle.publish",
        OfflineEndpoint {
            handler: offline_bundle_publish,
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

fn offline_bundle_verify(params: Value) -> Result<Value> {
    let source = params
        .get("source")
        .and_then(|v| v.as_str())
        .context("--source <bundle-path> required")?;
    let source_path = std::fs::canonicalize(Path::new(source))
        .context("resolve --source path")?;

    if !source_path.is_dir() {
        anyhow::bail!("--source is not a directory: {}", source_path.display());
    }

    let system_space_dir = ryeos_engine::roots::system_roots(&[])
        .into_iter()
        .next()
        .context("resolve system space dir")?;

    // When verifying a source tree, use installed bundles as dependency
    // roots but exclude any installed bundle with the same directory name
    // as the source (avoids duplicate handler/parser refs between the
    // installed copy and the source copy).
    let source_name = source_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let user_root = ryeos_engine::roots::user_root().ok();
    let installed_roots: Vec<std::path::PathBuf> =
        ryeos_bundle::installed::load_installed_bundle_records(&system_space_dir, user_root.as_deref())
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                r.bundle_root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|name| name != source_name)
                    .unwrap_or(true)
            })
            .map(|r| r.bundle_root)
            .collect();

    ryeos_bundle::preflight::preflight_verify_bundle_in_context(
        &source_path,
        &installed_roots,
        user_root.as_deref(),
    )
    .context("bundle verify failed")?;

    Ok(serde_json::json!({
        "source": source,
        "status": "verified",
        "detail": "all items pass signature and metadata validation"
    }))
}

fn offline_bundle_publish(params: Value) -> Result<Value> {
    use ryeos_tools::actions::publish::{run_publish, PublishOptions};

    let source = params
        .get("source")
        .and_then(|v| v.as_str())
        .context("--source <bundle-path> required")?;
    let bundle_source = Path::new(source).to_path_buf();

    if !bundle_source.is_dir() {
        anyhow::bail!("--source is not a directory: {}", source);
    }

    let registry_root = params
        .get("registry_root")
        .and_then(|v| v.as_str())
        .map(|s| Path::new(s).to_path_buf())
        .unwrap_or_else(|| bundle_source.clone());

    let owner = params
        .get("owner")
        .and_then(|v| v.as_str())
        .unwrap_or("local-dev")
        .to_string();

    let no_trust_doc = params
        .get("no_trust_doc")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Load signing key from user root (same path as ryeos-core-tools build)
    let user_root = ryeos_engine::roots::user_root()
        .context("resolve user root — run `ryeos init` first")?;
    let key_path = user_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("keys")
        .join("signing")
        .join("private_key.pem");

    if !key_path.exists() {
        anyhow::bail!(
            "user signing key not found at {} — run `ryeos init` first",
            key_path.display()
        );
    }

    let signing_key = ryeos_tools::actions::build_bundle::load_signing_key(&key_path)
        .context(format!("load signing key from {}", key_path.display()))?;

    let report = run_publish(&PublishOptions {
        bundle_source,
        registry_root,
        signing_key,
        owner,
        emit_trust_doc: !no_trust_doc,
    })
    .context("bundle publish failed")?;

    serde_json::to_value(report).context("serialize publish report")
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
    let service_path = resolve_service_path(system_space_dir, &verb);
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
    verb: &VerbDescriptor,
) -> Option<std::path::PathBuf> {
    if let Some(service_ref) = verb.execute.strip_prefix("service:") {
        // Authoritative path via execute ref
        find_service_path(system_space_dir, service_ref)
    } else {
        // Fallback: try verb name as-is, then with dash→slash
        None.or_else(|| find_service_path(system_space_dir, &verb.execute))
    }
}

/// Find a service descriptor file by relative service name.
/// Looks for `.ai/services/{name}.yaml` in each installed bundle.
/// Appends `.yaml` if the name doesn't already end with it.
fn find_service_path(system_space_dir: &Path, service_rel: &str) -> Option<std::path::PathBuf> {
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
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
            .join(".ai")
            .join("services")
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
