//! CLI help — static lifecycle section + dynamic alias discovery.
//!
//! `ryeos help` prints lifecycle verbs (always available) and discovers
//! the rest from installed bundle descriptors on disk. No daemon required.
//! `ryeos help <verb>` uses installed descriptors first and only queries the
//! daemon when no local descriptor help is available.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::CliError;
use anyhow::Context;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::{EffectiveItemRequest, Engine};
use serde_json::Value;

/// Print top-level help. Best-effort: includes dynamic alias discovery
/// from the system space if accessible, no daemon required.
pub fn print_help(mut out: impl Write) -> std::io::Result<()> {
    writeln!(out, "ryeos — CLI for Rye OS")?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "  ryeos [-p PROJECT] [--debug] <verb...> [args...]")?;
    writeln!(out)?;
    writeln!(out, "LIFECYCLE:")?;
    writeln!(
        out,
        "  {:<30} {}",
        "init", "Bootstrap local node state and packaged bundles"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "start", "Bring the local node runtime online"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "stop", "Gracefully stop the local node runtime"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "status", "Show local node lifecycle status"
    )?;
    writeln!(out)?;
    writeln!(out, "UNIVERSAL ESCAPE HATCH:")?;
    writeln!(
        out,
        "  {:<30} {}",
        "execute <item_ref>", "Execute any canonical item ref directly"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "  --input <file>", "  pass JSON parameters from file (or - for stdin)"
    )?;
    writeln!(out)?;

    // ── Dynamic alias discovery from installed bundles ──
    let system_space_dir = discover_system_space_dir();
    let bundle_roots = help_bundle_roots(&system_space_dir);
    let engine = build_help_engine(&system_space_dir, ".", &bundle_roots).ok();
    let discovered = discover_aliases_from_disk(&bundle_roots, engine.as_ref(), ".");

    if !discovered.is_empty() {
        let mut offline_cmds: Vec<(&str, &str)> = Vec::new();
        let mut daemon_cmds: Vec<(&str, &str)> = Vec::new();

        for (tokens_str, description, is_offline) in &discovered {
            if *is_offline {
                offline_cmds.push((tokens_str, description));
            } else {
                daemon_cmds.push((tokens_str, description));
            }
        }

        if !offline_cmds.is_empty() {
            writeln!(out, "OFFLINE (no daemon required):")?;
            offline_cmds.sort_by_key(|c| c.0);
            for (tokens_str, description) in &offline_cmds {
                writeln!(out, "    {:<28} {}", tokens_str, description)?;
            }
            writeln!(out)?;
        }

        if !daemon_cmds.is_empty() {
            writeln!(out, "DAEMON (requires running daemon):")?;
            daemon_cmds.sort_by_key(|c| c.0);
            for (tokens_str, description) in &daemon_cmds {
                writeln!(out, "    {:<28} {}", tokens_str, description)?;
            }
            writeln!(out)?;
        }
    }

    writeln!(out, "Run `ryeos help <verb>` for verb-specific help.")?;
    Ok(())
}

/// Scan installed bundles on disk for alias definitions.
/// Returns (token_string, description, is_offline) tuples.
fn discover_aliases_from_disk(
    bundle_roots: &[PathBuf],
    engine: Option<&Engine>,
    project_path: &str,
) -> Vec<(String, String, bool)> {
    let mut results = Vec::new();

    for bundle_root in bundle_roots {
        let aliases_dir = bundle_root.join(".ai").join("node").join("aliases");
        if !aliases_dir.is_dir() {
            continue;
        }
        let Ok(alias_files) = std::fs::read_dir(aliases_dir) else {
            continue;
        };

        for alias_file in alias_files.flatten() {
            let path = alias_file.path();
            if !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("yaml") | Some("yml")
            ) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(alias) = serde_yaml::from_str::<AliasHelpRecord>(&content) else {
                continue;
            };

            if alias.tokens == ["status"] {
                continue;
            }
            // Skip short aliases (s, f) — they're abbreviations.
            if alias.tokens.len() == 1 && alias.tokens[0].len() <= 1 {
                continue;
            }

            let metadata = read_verb_help_from_roots(bundle_roots, &alias.verb).and_then(|verb| {
                engine
                    .and_then(|engine| resolve_effective_help(engine, &verb.execute, project_path))
            });
            let is_offline = metadata
                .as_ref()
                .is_some_and(ItemHelpMetadata::is_offline_dispatch);
            results.push((alias.tokens.join(" "), alias.description, is_offline));
        }
    }

    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

/// Print verb-specific help by querying the daemon with validate_only.
///
/// This sends a `validate_only: true` request which resolves the alias
/// and returns metadata without executing. If the daemon is unreachable
/// or the tokens don't resolve, prints a descriptive error.
pub async fn print_verb_help(
    verb_tokens: &[String],
    system_space_dir: &std::path::Path,
    project_path: &str,
) -> Result<(), CliError> {
    // Prefer installed descriptor help. This keeps help kind-agnostic in the
    // CLI: aliases/verbs are node config, and the help renderer does not need
    // to classify the execute ref before deciding whether it can print usage.
    if print_installed_verb_help(verb_tokens, system_space_dir, project_path)? {
        return Ok(());
    }

    // Try to reach the daemon. If unavailable, fall back to a helpful
    // message rather than a raw connection error.
    let daemon_url = match crate::transport::http::resolve_daemon_url(system_space_dir).await {
        Ok(url) => url,
        Err(e) => {
            // Daemon not running — show what we can from local knowledge
            eprintln!("note: daemon not reachable, showing limited help");
            eprintln!("  detail: {e:#}");
            eprintln!();
            if !print_installed_verb_help(verb_tokens, system_space_dir, project_path)? {
                print_local_verb_help(verb_tokens)?;
            }
            return Ok(());
        }
    };

    let signer = match crate::transport::signing::Signer::resolve(system_space_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("note: cannot sign help request (user key not found)");
            eprintln!("  detail: {e:#}");
            eprintln!();
            if !print_installed_verb_help(verb_tokens, system_space_dir, project_path)? {
                print_local_verb_help(verb_tokens)?;
            }
            return Ok(());
        }
    };

    let audience = crate::transport::discovery::discover_audience(&daemon_url).await?;

    let body = serde_json::json!({
        "tokens": verb_tokens,
        "project_path": project_path,
        "parameters": {},
        "validate_only": true,
    });

    let body_bytes = serde_json::to_vec(&body).expect("infallible: Value serialization");
    let headers = signer.sign("POST", "/execute", &body_bytes, &audience)?;

    let url = format!("{}/execute", daemon_url);
    let payload = crate::transport::http::post_json(&url, &headers, &body_bytes).await?;

    // If the daemon resolved it, show the result
    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    println!("{pretty}");

    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct AliasHelpRecord {
    tokens: Vec<String>,
    verb: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    positional_field: Option<String>,
    #[serde(default)]
    positional_forms: Vec<ryeos_runtime::PositionalForm>,
    #[serde(default)]
    project_resolution: ryeos_runtime::ProjectResolution,
}

#[derive(Debug, serde::Deserialize)]
struct VerbHelpRecord {
    #[serde(default)]
    description: String,
    execute: String,
}

#[derive(Debug, serde::Deserialize)]
struct ItemHelpMetadata {
    #[serde(default)]
    description: String,
    #[serde(default)]
    required_caps: Vec<String>,
    #[serde(default)]
    schema: BTreeMap<String, String>,
    #[serde(default)]
    availability: Option<String>,
    #[serde(default)]
    has_launch_binary_ref: bool,
    #[serde(default)]
    has_tool_command: bool,
    #[serde(default)]
    has_offline_execute: bool,
}

impl ItemHelpMetadata {
    fn from_composed(value: &Value) -> Self {
        Self {
            description: value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            required_caps: value
                .get("required_caps")
                .and_then(Value::as_array)
                .map(|caps| {
                    caps.iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            schema: value
                .get("schema")
                .and_then(Value::as_object)
                .map(|schema| {
                    schema
                        .iter()
                        .map(|(field, ty)| {
                            (
                                field.clone(),
                                ty.as_str()
                                    .map(ToString::to_string)
                                    .unwrap_or_else(|| ty.to_string()),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            availability: value
                .get("availability")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            has_launch_binary_ref: value
                .get("launch")
                .and_then(|launch| launch.get("binary_ref"))
                .and_then(Value::as_str)
                .is_some(),
            has_tool_command: value
                .get("config")
                .and_then(|config| config.get("command"))
                .and_then(Value::as_str)
                .is_some(),
            has_offline_execute: value
                .get("offline_execute")
                .and_then(Value::as_str)
                .is_some(),
        }
    }

    fn is_offline_dispatch(&self) -> bool {
        matches!(self.availability.as_deref(), Some("offline" | "both"))
            || self.has_launch_binary_ref
            || self.has_tool_command
            || self.has_offline_execute
    }
}

fn print_installed_verb_help(
    verb_tokens: &[String],
    system_space_dir: &std::path::Path,
    project_path: &str,
) -> std::io::Result<bool> {
    let bundle_roots = help_bundle_roots(system_space_dir);
    let Some(alias) = find_alias_help_from_roots(verb_tokens, &bundle_roots) else {
        return Ok(false);
    };
    let verb = read_verb_help_from_roots(&bundle_roots, &alias.verb);
    let engine = build_help_engine(system_space_dir, project_path, &bundle_roots).ok();
    let item = verb.as_ref().and_then(|v| {
        engine
            .as_ref()
            .and_then(|engine| resolve_effective_help(engine, &v.execute, project_path))
    });

    let mut out = std::io::stdout();
    let command = alias.tokens.join(" ");
    let description = item
        .as_ref()
        .map(|s| s.description.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            verb.as_ref()
                .map(|v| v.description.as_str())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or(&alias.description);

    writeln!(out, "ryeos {command} — {description}")?;
    writeln!(out)?;
    if let Some(item) = &item {
        if item.is_offline_dispatch() {
            writeln!(out, "DISPATCH: offline (no daemon required)")?;
            writeln!(out)?;
        }
    } else if let Some(verb) = &verb {
        writeln!(out, "EXECUTE: {}", verb.execute)?;
        writeln!(out)?;
    }
    writeln!(out, "USAGE:")?;
    writeln!(
        out,
        "  ryeos {command}{}",
        usage_tail(&alias, item.as_ref())
    )?;

    if alias.project_resolution != ryeos_runtime::ProjectResolution::None {
        writeln!(out)?;
        writeln!(out, "PROJECT:")?;
        writeln!(
            out,
            "  --project <DIR>       Project root; accepted before or after the verb"
        )?;
        if alias.project_resolution == ryeos_runtime::ProjectResolution::Optional {
            writeln!(
                out,
                "  --no-project          Resolve against user/system space only"
            )?;
        }
    }

    if let Some(item) = &item {
        if !item.schema.is_empty() {
            writeln!(out)?;
            writeln!(out, "FIELDS:")?;
            for (field, ty) in &item.schema {
                let flag = field.replace('_', "-");
                writeln!(out, "  --{:<20} {}", flag, ty)?;
            }
        }
        if !item.required_caps.is_empty() {
            writeln!(out)?;
            writeln!(out, "REQUIRED CAPABILITIES:")?;
            for cap in &item.required_caps {
                writeln!(out, "  {cap}")?;
            }
        }
    }

    Ok(true)
}

fn usage_tail(alias: &AliasHelpRecord, item: Option<&ItemHelpMetadata>) -> String {
    let mut parts = Vec::new();
    if !alias.positional_forms.is_empty() {
        for form in &alias.positional_forms {
            let shape = form
                .slots
                .iter()
                .map(|slot| format!("<{}>", slot.field.replace('_', "-")))
                .collect::<Vec<_>>()
                .join(" ");
            if !shape.is_empty() {
                parts.push(shape);
            }
        }
    } else if let Some(field) = &alias.positional_field {
        parts.push(format!("<{}>", field.replace('_', "-")));
    }

    if let Some(item) = item {
        for (field, ty) in &item.schema {
            let required = !ty.ends_with('?');
            if field == "project" || parts.iter().any(|p| p.contains(&field.replace('_', "-"))) {
                continue;
            }
            let flag = format!(
                "--{} <{}>",
                field.replace('_', "-"),
                field.replace('_', "-")
            );
            if required {
                parts.push(flag);
            } else {
                parts.push(format!("[{flag}]"));
            }
        }
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" "))
    }
}

fn find_alias_help_from_roots(
    verb_tokens: &[String],
    bundle_roots: &[PathBuf],
) -> Option<AliasHelpRecord> {
    for bundle_root in bundle_roots {
        let aliases_dir = bundle_root.join(".ai/node/aliases");
        let Ok(alias_files) = std::fs::read_dir(aliases_dir) else {
            continue;
        };
        for alias_file in alias_files.flatten() {
            let path = alias_file.path();
            if !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("yaml") | Some("yml")
            ) {
                continue;
            }
            let Some(alias) = read_yaml::<AliasHelpRecord>(&path) else {
                continue;
            };
            if alias.tokens == verb_tokens {
                return Some(alias);
            }
        }
    }
    None
}

fn read_verb_help_from_roots(bundle_roots: &[PathBuf], verb: &str) -> Option<VerbHelpRecord> {
    for bundle_root in bundle_roots {
        let path = bundle_root
            .join(".ai/node/verbs")
            .join(format!("{verb}.yaml"));
        if let Some(record) = read_yaml::<VerbHelpRecord>(&path) {
            return Some(record);
        }
    }
    None
}

fn read_yaml<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&content).ok()
}

fn help_bundle_roots(system_space_dir: &Path) -> Vec<PathBuf> {
    let user_root = ryeos_engine::roots::user_root().ok();
    match ryeos_bundle::installed::load_installed_bundle_records(
        system_space_dir,
        user_root.as_deref(),
    ) {
        Ok(records) if !records.is_empty() => records
            .into_iter()
            .map(|record| record.bundle_root)
            .collect(),
        _ => discover_bundle_roots_direct(system_space_dir),
    }
}

fn discover_bundle_roots_direct(system_space_dir: &Path) -> Vec<PathBuf> {
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let Ok(entries) = std::fs::read_dir(&bundles_dir) else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name.ends_with(".backup.prev") {
            continue;
        }
        let path = entry.path();
        if path.join(".ai").is_dir() {
            roots.push(path);
        }
    }
    roots.sort();
    roots
}

fn build_help_engine(
    system_space_dir: &Path,
    project_path: &str,
    bundle_roots: &[PathBuf],
) -> anyhow::Result<Engine> {
    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        system_space_dir: Some(system_space_dir.to_path_buf()),
        ..Default::default()
    })?;
    let project_root = (project_path != ".").then(|| PathBuf::from(project_path));
    let user_root = ryeos_engine::roots::user_root().ok();

    ryeos_app::engine_init::build_engine_for_roots(
        &config,
        bundle_roots,
        project_root.as_deref(),
        user_root.as_deref(),
        None,
    )
    .context("build help effective-item engine")
}

fn resolve_effective_help(
    engine: &Engine,
    execute_ref: &str,
    project_path: &str,
) -> Option<ItemHelpMetadata> {
    let item_ref = CanonicalRef::parse(execute_ref).ok()?;
    let project_root = (project_path != ".").then(|| PathBuf::from(project_path));
    let item = engine
        .effective_item(EffectiveItemRequest {
            item_ref,
            expected_kind: None,
            project_root,
        })
        .ok()?;
    Some(ItemHelpMetadata::from_composed(&item.composed_value))
}

/// Print help for local verbs when the daemon is unavailable.
fn print_local_verb_help(verb_tokens: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout();
    match verb_tokens.first().map(|s| s.as_str()) {
        Some("init") => {
            writeln!(out, "ryeos init — Bootstrap operator keys and core bundle")?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos init [OPTIONS]")?;
            writeln!(out)?;
            writeln!(out, "OPTIONS:")?;
            writeln!(
                out,
                "  --source <DIR>           Bundle source directory (default: /usr/share/ryeos)"
            )?;
            writeln!(
                out,
                "  --trust-file <FILE>      Additional publisher trust doc (repeatable)"
            )?;
            writeln!(out, "  --system-space-dir <DIR> System space root")?;
            writeln!(out, "  --user-root <DIR>        User space root")?;
        }
        Some("status") => {
            writeln!(out, "ryeos status — Show local node lifecycle status")?;
            writeln!(out)?;
            writeln!(
                out,
                "USAGE: ryeos status [--json] [--system-space-dir <DIR>]"
            )?;
        }
        Some("start") => {
            writeln!(out, "ryeos start — Bring the local node runtime online")?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos start [--system-space-dir <DIR>]")?;
        }
        Some("stop") => {
            writeln!(out, "ryeos stop — Gracefully stop the local node runtime")?;
            writeln!(out)?;
            writeln!(
                out,
                "USAGE: ryeos stop [--force] [--system-space-dir <DIR>]"
            )?;
        }
        Some("execute") => {
            writeln!(out, "ryeos execute — Universal escape hatch")?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos execute <item_ref> [flags...]")?;
            writeln!(out)?;
            writeln!(out, "PARAMETER INPUT:")?;
            writeln!(out, "  --input <FILE>   Read JSON parameters from a file")?;
            writeln!(out, "  --input -        Read JSON parameters from stdin")?;
            writeln!(
                out,
                "  --key value      Heuristic flag binding (hyphens normalised to underscores)"
            )?;
        }
        Some("sign") => {
            writeln!(out, "ryeos sign — Sign a RyeOS item by canonical ref")?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos sign <item_ref> [OPTIONS]")?;
            writeln!(out)?;
            writeln!(out, "OPTIONS:")?;
            writeln!(
                out,
                "  --project <DIR>       Project root (parent of .ai/); default: cwd"
            )?;
            writeln!(
                out,
                "  --source <SOURCE>     Where to look: project (default) or user"
            )?;
        }
        Some("identity") => {
            writeln!(out, "ryeos identity — Print the local node public identity")?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos identity [--system-space-dir <DIR>]")?;
        }
        Some(other) => {
            writeln!(out, "no local help available for '{}'", other)?;
            writeln!(out, "run `ryeos init` if Rye OS has not been initialized")?;
        }
        None => {}
    }
    Ok(())
}

fn discover_system_space_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("RYEOS_SYSTEM_SPACE_DIR") {
        return std::path::PathBuf::from(p);
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_help_reads_alias_verb_and_effective_item_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle_root = tmp.path().join(".ai/bundles/core");
        let bundle = bundle_root.join(".ai");
        std::fs::create_dir_all(bundle.join("node/aliases")).unwrap();
        std::fs::create_dir_all(bundle.join("node/verbs")).unwrap();
        std::fs::write(
            bundle.join("node/aliases/remote-doctor.yaml"),
            r#"
category: aliases
section: aliases
tokens: ["remote", "doctor"]
verb: remote-doctor
description: Diagnose remote setup
project_resolution: optional
positional_forms:
  - slots:
      - field: remote
"#,
        )
        .unwrap();
        std::fs::write(
            bundle.join("node/verbs/remote-doctor.yaml"),
            r#"
category: verbs
section: verbs
name: remote-doctor
description: Diagnose remote setup
execute: tool:remote/doctor
"#,
        )
        .unwrap();

        let tokens = vec!["remote".to_string(), "doctor".to_string()];
        let roots = vec![bundle_root];
        let alias = find_alias_help_from_roots(&tokens, &roots).unwrap();
        assert_eq!(alias.verb, "remote-doctor");
        assert_eq!(
            alias.project_resolution,
            ryeos_runtime::ProjectResolution::Optional
        );
        let verb = read_verb_help_from_roots(&roots, &alias.verb).unwrap();
        assert_eq!(verb.execute, "tool:remote/doctor");

        let item = ItemHelpMetadata::from_composed(&serde_json::json!({
            "required_caps": ["ryeos.execute.tool.remote.doctor"],
            "schema": {
                "remote": "string?",
                "project": "string?"
            },
            "description": "Diagnose remote node authorization and project setup",
            "availability": "offline"
        }));
        assert!(item.is_offline_dispatch());
        assert_eq!(item.schema.get("project").unwrap(), "string?");
        assert_eq!(usage_tail(&alias, Some(&item)), " <remote>");
    }
}
