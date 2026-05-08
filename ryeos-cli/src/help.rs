//! Hardcoded CLI help — no daemon dependency for top-level help.
//!
//! `ryeos help` prints a static overview of built-in verbs + hints.
//! `ryeos help <verb>` queries the daemon for alias info via the same
//! token dispatch path.

use std::io::Write;

use crate::error::CliError;

/// Print top-level help. No daemon round-trip.
pub fn print_help(mut out: impl Write) -> std::io::Result<()> {
    writeln!(out, "ryeos — CLI for Rye OS")?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "  ryos [-p PROJECT] [--debug] <verb...> [args...]")?;
    writeln!(out)?;
    writeln!(out, "LOCAL COMMANDS (no daemon required):")?;
    writeln!(out, "  {:<30} {}", "init", "Bootstrap operator keys and core bundle")?;
    writeln!(out, "  {:<30} {}", "trust pin --from <trust.toml>", "Pin a publisher key from PUBLISHER_TRUST.toml")?;
    writeln!(out, "  {:<30} {}", "publish <src>", "Sign and publish a bundle")?;
    writeln!(out, "  {:<30} {}", "vault put <K=V>...", "Add entries to sealed secret store")?;
    writeln!(out, "  {:<30} {}", "vault list", "List sealed secret keys")?;
    writeln!(out, "  {:<30} {}", "vault remove <K>...", "Remove sealed secret keys")?;
    writeln!(out, "  {:<30} {}", "vault rewrap", "Rotate vault keypair")?;
    writeln!(out)?;
    writeln!(out, "UNIVERSAL ESCAPE HATCH:")?;
    writeln!(out, "  {:<30} {}", "execute <item_ref>", "Execute any canonical item ref directly")?;
    writeln!(out)?;
    writeln!(out, "DAEMON COMMANDS (require running daemon):")?;
    writeln!(out, "  {:<30} {}", "status", "Show daemon status")?;
    writeln!(out, "  {:<30} {}", "sign", "Sign a bundle item")?;
    writeln!(out, "  {:<30} {}", "verify", "Verify a bundle item")?;
    writeln!(out, "  {:<30} {}", "fetch", "Fetch an item")?;
    writeln!(out, "  {:<30} {}", "rebuild", "Rebuild the bundle manifest")?;
    writeln!(out, "  {:<30} {}", "bundle install", "Install a bundle")?;
    writeln!(out, "  {:<30} {}", "bundle list", "List installed bundles")?;
    writeln!(out, "  {:<30} {}", "bundle remove", "Remove a bundle")?;
    writeln!(out)?;
    writeln!(out, "Run `ryeos help <verb>` for verb-specific help.")?;
    Ok(())
}

/// Print verb-specific help by querying the daemon.
pub async fn print_verb_help(
    verb_tokens: &[String],
    system_space_dir: &std::path::Path,
    project_path: &str,
) -> Result<(), CliError> {
    // We send the tokens to the daemon — if it resolves, we get back
    // the execution result which shows what the verb does. If not,
    // we get an error with the unmatched tokens.
    let bind = crate::transport::http::read_daemon_bind(system_space_dir).await?;
    let signer = crate::transport::signing::Signer::resolve(system_space_dir)?;

    let body = serde_json::json!({
        "tokens": verb_tokens,
        "project_path": project_path,
        "parameters": {},
        "validate_only": true,
    });

    let body_bytes = serde_json::to_vec(&body).expect("infallible: Value serialization");
    let headers = signer.sign("POST", "/execute", &body_bytes)?;
    let payload = crate::transport::http::post_json(&bind, &headers, &body_bytes).await?;

    // If the daemon resolved it, show the result
    let pretty = serde_json::to_string_pretty(&payload)
        .unwrap_or_else(|_| payload.to_string());
    println!("{pretty}");

    Ok(())
}
