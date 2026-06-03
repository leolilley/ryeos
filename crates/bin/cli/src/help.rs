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
use ryeos_app::node_config::NodeConfigSnapshot;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::{EffectiveItemRequest, Engine};
use serde_json::Value;

use crate::node_descriptors::LoadedAliasDescriptor;

/// Print top-level help. Static lifecycle help always renders. Dynamic alias
/// discovery is included only after installed node config verifies; verification
/// failures are surfaced as warnings instead of being treated as absent config.
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
        "node status", "Show local node lifecycle status"
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
    let snapshot = match crate::node_descriptors::load_verified_snapshot(&system_space_dir) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            eprintln!("warning: installed node config failed verification: {err:#}");
            None
        }
    };
    let bundle_roots = snapshot
        .as_ref()
        .map(snapshot_bundle_roots)
        .unwrap_or_default();
    let engine = (!bundle_roots.is_empty())
        .then(|| build_help_engine(&system_space_dir, ".", &bundle_roots).ok())
        .flatten();
    let discovered = snapshot
        .as_ref()
        .map(|snapshot| discover_aliases_from_snapshot(snapshot, engine.as_ref(), "."))
        .unwrap_or_default();

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

/// Discover aliases from verified installed node config.
/// Returns (token_string, description, is_offline) tuples.
fn discover_aliases_from_snapshot(
    snapshot: &NodeConfigSnapshot,
    engine: Option<&Engine>,
    project_path: &str,
) -> Vec<(String, String, bool)> {
    let mut results = Vec::new();
    let aliases = crate::node_descriptors::load_alias_descriptors_from_snapshot(snapshot);

    for alias in aliases {
        // Skip short aliases (s, f) — they're abbreviations.
        if alias.def.tokens.len() == 1 && alias.def.tokens[0].len() <= 1 {
            continue;
        }

        let metadata =
            crate::node_descriptors::load_verb_descriptor_from_snapshot(snapshot, &alias.def.verb)
                .and_then(|verb| {
                    engine.and_then(|engine| {
                        resolve_effective_help(engine, &verb.execute, project_path)
                    })
                });
        let is_offline = metadata
            .as_ref()
            .is_some_and(ItemHelpMetadata::is_offline_dispatch);
        results.push((alias.def.tokens.join(" "), alias.description, is_offline));
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
    let snapshot =
        crate::node_descriptors::load_verified_snapshot(system_space_dir).map_err(|err| {
            std::io::Error::other(format!(
                "installed node config failed verification: {err:#}"
            ))
        })?;
    let bundle_roots = snapshot_bundle_roots(&snapshot);
    let Some(alias) = crate::node_descriptors::find_alias(&snapshot, verb_tokens) else {
        return Ok(false);
    };
    let verb =
        crate::node_descriptors::load_verb_descriptor_from_snapshot(&snapshot, &alias.def.verb);
    let engine = build_help_engine(system_space_dir, project_path, &bundle_roots).ok();
    let item = verb.as_ref().and_then(|v| {
        engine
            .as_ref()
            .and_then(|engine| resolve_effective_help(engine, &v.execute, project_path))
    });

    let mut out = std::io::stdout();
    let command = alias.def.tokens.join(" ");
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

    if alias.def.project_resolution != ryeos_runtime::ProjectResolution::None {
        writeln!(out)?;
        writeln!(out, "PROJECT:")?;
        writeln!(
            out,
            "  --project <DIR>       Project root; accepted before or after the verb"
        )?;
        if alias.def.project_resolution == ryeos_runtime::ProjectResolution::Optional {
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

fn usage_tail(alias: &LoadedAliasDescriptor, item: Option<&ItemHelpMetadata>) -> String {
    let mut parts = Vec::new();
    if !alias.def.positional_forms.is_empty() {
        for form in &alias.def.positional_forms {
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

fn snapshot_bundle_roots(snapshot: &NodeConfigSnapshot) -> Vec<PathBuf> {
    snapshot
        .bundles
        .iter()
        .map(|record| record.path.clone())
        .collect()
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
        Some("node") if verb_tokens.get(1).map(String::as_str) == Some("status") => {
            writeln!(out, "ryeos node status — Show local node lifecycle status")?;
            writeln!(out)?;
            writeln!(
                out,
                "USAGE: ryeos node status [--json] [--system-space-dir <DIR>]"
            )?;
        }
        Some("system") if verb_tokens.get(1).map(String::as_str) == Some("status") => {
            writeln!(
                out,
                "ryeos system status — Show local node lifecycle status"
            )?;
            writeln!(out)?;
            writeln!(
                out,
                "USAGE: ryeos system status [--json] [--system-space-dir <DIR>]"
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
    use ryeos_app::node_config::sections::alias as node_alias;
    use ryeos_app::node_config::NodeConfigSnapshot;

    #[test]
    fn installed_help_reads_alias_verb_and_effective_item_metadata() {
        let snapshot = NodeConfigSnapshot {
            bundles: vec![],
            routes: vec![],
            verbs: vec![ryeos_app::node_config::sections::verb::VerbRecord {
                category: "verbs".into(),
                section: "verbs".into(),
                name: "remote-doctor".into(),
                description: "Diagnose remote setup".into(),
                execute: Some("tool:remote/doctor".into()),
                aliases: vec![],
                source_file: PathBuf::from("/tmp/remote-doctor.yaml"),
            }],
            aliases: vec![node_alias::AliasRecord {
                category: "aliases".into(),
                section: "aliases".into(),
                tokens: vec!["remote".into(), "doctor".into()],
                verb: "remote-doctor".into(),
                description: "Diagnose remote setup".into(),
                deprecated: None,
                replacement_tokens: None,
                removed_in: None,
                positional_forms: vec![node_alias::PositionalForm {
                    slots: vec![node_alias::PositionalSlot {
                        field: "remote".into(),
                        matcher: node_alias::PositionalMatcher::Any,
                    }],
                }],
                project_resolution: node_alias::ProjectResolution::Optional,
                source_file: PathBuf::from("/tmp/remote-doctor.yaml"),
            }],
            hosted_node_policies: vec![],
        };
        let tokens = vec!["remote".to_string(), "doctor".to_string()];
        let alias = crate::node_descriptors::find_alias(&snapshot, &tokens).unwrap();
        assert_eq!(alias.def.verb, "remote-doctor");
        assert_eq!(
            alias.def.project_resolution,
            ryeos_runtime::ProjectResolution::Optional
        );
        let verb =
            crate::node_descriptors::load_verb_descriptor_from_snapshot(&snapshot, &alias.def.verb)
                .unwrap();
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
