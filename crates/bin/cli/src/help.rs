//! CLI help — static lifecycle section + dynamic alias discovery.
//!
//! `ryeos help` prints lifecycle verbs (always available) and discovers
//! the rest from installed bundle descriptors on disk. No daemon required.
//! `ryeos help <verb>` queries the daemon for alias info via the same
//! token dispatch path with `validate_only: true`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use crate::error::CliError;

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
    let discovered = discover_aliases_from_disk(&system_space_dir);

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

/// Check whether the service descriptor backing an alias declares
/// `availability: offline`. Tries: verb execute ref → service, then
/// direct service lookup by verb name.
fn check_service_offline(system_space_dir: &Path, alias_tokens: &[String]) -> bool {
    let verb_name = alias_tokens.join("-");

    // Try 1: verb → execute service ref → service descriptor
    if let Some(offline) = check_via_verb_execute_ref(system_space_dir, &verb_name) {
        return offline;
    }

    // Try 2: direct service descriptor by verb name (e.g. services/sign.yaml)
    check_via_service_name(system_space_dir, &verb_name)
}

fn check_via_verb_execute_ref(system_space_dir: &Path, verb_name: &str) -> Option<bool> {
    let verb = read_verb_help(system_space_dir, verb_name)?;
    let service_rel = verb.execute.strip_prefix("service:")?;
    let path = find_service_path(system_space_dir, service_rel)?;
    let content = std::fs::read_to_string(&path).ok()?;
    Some(read_availability(&content))
}

fn check_via_service_name(system_space_dir: &Path, name: &str) -> bool {
    let path = match find_service_path(system_space_dir, name) {
        Some(p) => p,
        None => return false,
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    read_availability(&content)
}

fn read_availability(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("# ryeos:signed:") {
            continue;
        }
        if let Some(val) = trimmed.strip_prefix("availability:") {
            let val = val.trim();
            return val == "offline" || val == "both";
        }
    }
    false
}

/// Find a service descriptor file by relative path.
/// Appends `.yaml` extension if not already present.
fn find_service_path(system_space_dir: &Path, service_rel: &str) -> Option<std::path::PathBuf> {
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let Ok(entries) = std::fs::read_dir(&bundles_dir) else {
        return None;
    };
    // Service names may or may not include .yaml extension
    let file_name = if service_rel.ends_with(".yaml") || service_rel.ends_with(".yml") {
        service_rel.to_string()
    } else {
        format!("{}.yaml", service_rel)
    };
    for entry in entries.flatten() {
        let path = entry.path().join(".ai").join("services").join(&file_name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Scan installed bundles on disk for alias definitions.
/// Returns (token_string, description, is_offline) tuples.
fn discover_aliases_from_disk(system_space_dir: &std::path::Path) -> Vec<(String, String, bool)> {
    let mut results = Vec::new();
    let bundles_dir = system_space_dir.join(".ai").join("bundles");

    if !bundles_dir.is_dir() {
        return results;
    }

    let Ok(bundle_entries) = std::fs::read_dir(&bundles_dir) else {
        return results;
    };

    for bundle_entry in bundle_entries.flatten() {
        let name = bundle_entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip non-bundle artifacts: hidden dirs (e.g. .staging),
        // backup dirs (e.g. core.backup.prev), and staging dirs.
        if name_str.starts_with('.') || name_str.ends_with(".backup.prev") {
            continue;
        }

        let aliases_dir = bundle_entry.path().join(".ai").join("node").join("aliases");
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

            // Parse tokens and description from the YAML.
            // Simple line-by-line extraction — avoids pulling in a YAML parser.
            let mut tokens: Option<Vec<String>> = None;
            let mut description = String::new();

            for line in content.lines() {
                let trimmed = line.trim();
                // Skip signed envelope header
                if trimmed.starts_with("# ryeos:signed:") {
                    continue;
                }
                if let Some(val) = trimmed.strip_prefix("tokens:") {
                    // tokens: ["remote", "exec"]  or tokens: ["status"]
                    let val = val.trim();
                    if val.starts_with('[') {
                        let parsed = parse_yaml_string_array(val);
                        if !parsed.is_empty() {
                            tokens = Some(parsed);
                        }
                    }
                } else if let Some(val) = trimmed.strip_prefix("description:") {
                    let val = val.trim().trim_matches('"');
                    description = val.to_string();
                }
            }

            if let Some(tokens) = tokens {
                if tokens == ["status"] {
                    continue;
                }
                // Skip short aliases (s, f) — they're abbreviations
                if tokens.len() == 1 && tokens[0].len() <= 1 {
                    continue;
                }

                // Check if the corresponding service declares offline availability
                let is_offline = check_service_offline(system_space_dir, &tokens);
                results.push((tokens.join(" "), description, is_offline));
            }
        }
    }

    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

/// Very small YAML string-array parser for `["a", "b", "c"]`.
fn parse_yaml_string_array(s: &str) -> Vec<String> {
    let s = s.trim_start_matches('[').trim_end_matches(']');
    s.split(',')
        .map(|item| item.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
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
    // Try to reach the daemon. If unavailable, fall back to a helpful
    // message rather than a raw connection error.
    let daemon_url = match crate::transport::http::resolve_daemon_url(system_space_dir).await {
        Ok(url) => url,
        Err(e) => {
            // Daemon not running — show what we can from local knowledge
            eprintln!("note: daemon not reachable, showing limited help");
            eprintln!("  detail: {e:#}");
            eprintln!();
            if !print_installed_verb_help(verb_tokens, system_space_dir)? {
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
            if !print_installed_verb_help(verb_tokens, system_space_dir)? {
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
struct ServiceHelpRecord {
    #[serde(default)]
    description: String,
    #[serde(default)]
    required_caps: Vec<String>,
    #[serde(default)]
    schema: BTreeMap<String, String>,
    #[serde(default)]
    availability: Option<String>,
}

fn print_installed_verb_help(
    verb_tokens: &[String],
    system_space_dir: &std::path::Path,
) -> std::io::Result<bool> {
    let Some(alias) = find_alias_help(verb_tokens, system_space_dir) else {
        return Ok(false);
    };
    let verb = read_verb_help(system_space_dir, &alias.verb);
    let service = verb
        .as_ref()
        .and_then(|v| service_ref_to_path(system_space_dir, &v.execute))
        .and_then(|path| read_yaml::<ServiceHelpRecord>(&path));

    let mut out = std::io::stdout();
    let command = alias.tokens.join(" ");
    let description = service
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
    if let Some(service) = &service {
        let avail = service.availability.as_deref().unwrap_or("daemon");
        if avail == "offline" || avail == "both" {
            writeln!(out, "DISPATCH: offline (no daemon required)")?;
            writeln!(out)?;
        }
    }
    writeln!(out, "USAGE:")?;
    writeln!(
        out,
        "  ryeos {command}{}",
        usage_tail(&alias, service.as_ref())
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

    if let Some(service) = &service {
        if !service.schema.is_empty() {
            writeln!(out)?;
            writeln!(out, "FIELDS:")?;
            for (field, ty) in &service.schema {
                let flag = field.replace('_', "-");
                writeln!(out, "  --{:<20} {}", flag, ty)?;
            }
        }
        if !service.required_caps.is_empty() {
            writeln!(out)?;
            writeln!(out, "REQUIRED CAPABILITIES:")?;
            for cap in &service.required_caps {
                writeln!(out, "  {cap}")?;
            }
        }
    }

    Ok(true)
}

fn usage_tail(alias: &AliasHelpRecord, service: Option<&ServiceHelpRecord>) -> String {
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

    if let Some(service) = service {
        for (field, ty) in &service.schema {
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

fn find_alias_help(
    verb_tokens: &[String],
    system_space_dir: &std::path::Path,
) -> Option<AliasHelpRecord> {
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let bundle_entries = std::fs::read_dir(&bundles_dir).ok()?;
    for bundle_entry in bundle_entries.flatten() {
        let aliases_dir = bundle_entry.path().join(".ai/node/aliases");
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

fn read_verb_help(system_space_dir: &std::path::Path, verb: &str) -> Option<VerbHelpRecord> {
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let bundle_entries = std::fs::read_dir(&bundles_dir).ok()?;
    for bundle_entry in bundle_entries.flatten() {
        let path = bundle_entry
            .path()
            .join(".ai/node/verbs")
            .join(format!("{verb}.yaml"));
        if let Some(record) = read_yaml::<VerbHelpRecord>(&path) {
            return Some(record);
        }
    }
    None
}

fn service_ref_to_path(
    system_space_dir: &std::path::Path,
    service_ref: &str,
) -> Option<std::path::PathBuf> {
    let rel = service_ref.strip_prefix("service:")?;
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let bundle_entries = std::fs::read_dir(&bundles_dir).ok()?;
    for bundle_entry in bundle_entries.flatten() {
        let path = bundle_entry
            .path()
            .join(".ai/services")
            .join(format!("{rel}.yaml"));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn read_yaml<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&content).ok()
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
    fn installed_help_reads_alias_verb_and_service_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join(".ai/bundles/core/.ai");
        std::fs::create_dir_all(bundle.join("node/aliases")).unwrap();
        std::fs::create_dir_all(bundle.join("node/verbs")).unwrap();
        std::fs::create_dir_all(bundle.join("services/remote")).unwrap();
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
execute: service:remote/doctor
"#,
        )
        .unwrap();
        std::fs::write(
            bundle.join("services/remote/doctor.yaml"),
            r#"
kind: service
endpoint: remote.doctor
required_caps: ["ryeos.execute.service.remote.doctor"]
schema:
  remote: string?
  project: string?
description: Diagnose remote node authorization and project setup
"#,
        )
        .unwrap();

        let tokens = vec!["remote".to_string(), "doctor".to_string()];
        let alias = find_alias_help(&tokens, tmp.path()).unwrap();
        assert_eq!(alias.verb, "remote-doctor");
        assert_eq!(
            alias.project_resolution,
            ryeos_runtime::ProjectResolution::Optional
        );
        let verb = read_verb_help(tmp.path(), &alias.verb).unwrap();
        assert_eq!(verb.execute, "service:remote/doctor");
        let service_path = service_ref_to_path(tmp.path(), &verb.execute).unwrap();
        let service: ServiceHelpRecord = read_yaml(&service_path).unwrap();
        assert_eq!(service.schema.get("project").unwrap(), "string?");
        assert_eq!(usage_tail(&alias, Some(&service)), " <remote>");
    }
}
