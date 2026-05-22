//! CLI help — static + dynamic alias discovery.
//!
//! `ryeos help` prints a static overview of built-in verbs and, if the
//! system space is accessible, appends a summary of installed aliases.
//! `ryeos help <verb>` queries the daemon for alias info via the same
//! token dispatch path with `validate_only: true`.

use std::io::Write;

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
    writeln!(out, "LOCAL TOOLS (no daemon required):")?;
    writeln!(
        out,
        "  {:<30} {}",
        "authorize-key", "Authorize a public key to call the daemon"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "trust pin --from <trust.toml>", "Pin a publisher key from PUBLISHER_TRUST.toml"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "publish <src>", "Sign and publish a bundle"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "vault put --name K", "Add a secret to the sealed secret store"
    )?;
    writeln!(out, "  {:<30} {}", "vault list", "List sealed secret keys")?;
    writeln!(
        out,
        "  {:<30} {}",
        "vault remove <K>...", "Remove sealed secret keys"
    )?;
    writeln!(out, "  {:<30} {}", "vault rewrap", "Rotate vault keypair")?;
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
        // Group by prefix
        let mut groups: std::collections::BTreeMap<String, Vec<(String, String)>> =
            std::collections::BTreeMap::new();

        for (tokens_str, description) in &discovered {
            let tokens: Vec<&str> = tokens_str.split(' ').collect();
            let prefix = if tokens.len() > 1 {
                tokens[0].to_string()
            } else {
                "(general)".to_string()
            };
            groups
                .entry(prefix)
                .or_default()
                .push((tokens_str.clone(), description.clone()));
        }

        writeln!(out, "INSTALLED COMMANDS (from bundles):")?;
        for (prefix, aliases) in &groups {
            writeln!(out, "  [{}]", prefix)?;
            for (tokens_str, description) in aliases {
                writeln!(out, "    {:<28} {}", tokens_str, description)?;
            }
        }
        writeln!(out)?;
    } else {
        // Fallback: static list when no bundles are discovered
        writeln!(out, "DAEMON COMMANDS (require running daemon):")?;
        writeln!(
            out,
            "  {:<30} {}",
            "identity public-key", "Show node public identity"
        )?;
        writeln!(out, "  {:<30} {}", "sign", "Sign a bundle item")?;
        writeln!(out, "  {:<30} {}", "verify", "Verify a bundle item")?;
        writeln!(out, "  {:<30} {}", "fetch", "Fetch an item")?;
        writeln!(out, "  {:<30} {}", "rebuild", "Rebuild the bundle manifest")?;
        writeln!(out, "  {:<30} {}", "bundle install", "Install a bundle")?;
        writeln!(out, "  {:<30} {}", "bundle list", "List installed bundles")?;
        writeln!(out, "  {:<30} {}", "bundle remove", "Remove a bundle")?;
        writeln!(out)?;
    }

    writeln!(out, "Run `ryeos help <verb>` for verb-specific help.")?;
    Ok(())
}

/// Scan installed bundles on disk for alias definitions.
/// Returns (token_string, description) pairs.
fn discover_aliases_from_disk(system_space_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let bundles_dir = system_space_dir.join(".ai").join("bundles");

    if !bundles_dir.is_dir() {
        return results;
    }

    let Ok(bundle_entries) = std::fs::read_dir(&bundles_dir) else {
        return results;
    };

    for bundle_entry in bundle_entries.flatten() {
        let aliases_dir = bundle_entry.path().join(".ai").join("node").join("aliases");
        if !aliases_dir.is_dir() {
            continue;
        }
        let Ok(alias_files) = std::fs::read_dir(&aliases_dir) else {
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
                results.push((tokens.join(" "), description));
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
            print_local_verb_help(verb_tokens)?;
            return Ok(());
        }
    };

    let signer = match crate::transport::signing::Signer::resolve(system_space_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("note: cannot sign help request (user key not found)");
            eprintln!("  detail: {e:#}");
            eprintln!();
            print_local_verb_help(verb_tokens)?;
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
        Some("authorize-key") => {
            writeln!(
                out,
                "ryeos authorize-key — Authorize a public key to call the daemon"
            )?;
            writeln!(out)?;
            writeln!(
                out,
                "USAGE: ryeos authorize-key --public-key <KEY> [OPTIONS]"
            )?;
            writeln!(out)?;
            writeln!(out, "OPTIONS:")?;
            writeln!(
                out,
                "  --public-key <KEY>  Ed25519 public key in 'ed25519:<base64>' format (required)"
            )?;
            writeln!(
                out,
                "  --label <LABEL>     Human-readable label (default: cli-authorized)"
            )?;
            writeln!(
                out,
                "  --scopes <SCOPES>   Comma-separated capabilities in canonical form"
            )?;
            writeln!(
                out,
                "                      ryeos.<verb>.<kind>.<subject> (required)"
            )?;
            writeln!(
                out,
                "                      e.g. ryeos.execute.service.remote.admin"
            )?;
            writeln!(
                out,
                "  --allow-wildcard    Allow wildcard scope '*' (bootstrap only)"
            )?;
            writeln!(out, "  --system-space-dir  System space root")?;
        }
        Some("trust") => {
            writeln!(out, "ryeos trust pin — Pin a publisher key")?;
            writeln!(out)?;
            writeln!(out, "USAGE:")?;
            writeln!(out, "  ryeos trust pin --from <trust.toml>")?;
            writeln!(out, "  ryeos trust pin <fingerprint> --pubkey-file <file>")?;
        }
        Some("vault") => {
            writeln!(out, "ryeos vault — Manage sealed secrets")?;
            writeln!(out)?;
            writeln!(out, "COMMANDS:")?;
            writeln!(
                out,
                "  vault put --name <KEY>              Add a secret (reads value from stdin)"
            )?;
            writeln!(
                out,
                "  vault put --name <KEY> --value-string <VAL>  (insecure, for scripts)"
            )?;
            writeln!(
                out,
                "  vault list                           List sealed secret keys"
            )?;
            writeln!(
                out,
                "  vault remove <KEY>...                Remove sealed secret keys"
            )?;
            writeln!(
                out,
                "  vault rewrap                         Rotate vault keypair"
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
