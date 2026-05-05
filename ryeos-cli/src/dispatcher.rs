use std::path::PathBuf;

use ryeos_runtime::authorizer::{canonical_cap, Authorizer, AuthorizationPolicy};

use crate::error::CliError;
use crate::help;
use crate::local_verbs;
use crate::verbs;

/// CLI struct for clap argument parsing.
#[derive(clap::Parser)]
#[command(
    name = "rye",
    about = "CLI for Rye OS",
    disable_help_subcommand = true,
    trailing_var_arg = true
)]
pub struct Cli {
    /// Project root (overrides cwd).
    #[arg(short, long)]
    project: Option<PathBuf>,

    /// Verbose tracing output.
    #[arg(long)]
    pub debug: bool,

    /// Verb tokens + tail (everything after globals).
    #[arg(trailing_var_arg = true)]
    pub rest: Vec<String>,
}

/// Main dispatch flow.
pub async fn run(cli: Cli) -> Result<(), CliError> {
    // 1. Project root
    //
    // `body_project_path` is what we send the daemon: the literal `--project`
    // value if supplied, else "." (relative to the daemon's view). The
    // resolved `project_root` is only used locally for verb-table loading.
    let body_project_path = match &cli.project {
        Some(p) => p.to_string_lossy().into_owned(),
        None => ".".to_string(),
    };
    let project_root = crate::project_root::effective_project_root(cli.project)?;

    // 2. State dir
    let state_dir = discover_state_dir();

    // 3. Hardcoded LOCAL verbs (must work on a fresh checkout before
    //    keys / core bundle exist). These run in-process; no daemon
    //    round-trip and no verb-table dependency:
    //      rye init             — bootstrap operator state
    //      rye trust pin <fp>   — pin a publisher key (cap-gated rye.trust.pin)
    //      rye publish <src>    — bundle author publish dance
    if local_verbs::try_dispatch(&cli.rest)? {
        return Ok(());
    }

    // 4. Load verb table
    let table = verbs::load_verbs(&project_root)?;

    // 4. No verb = help
    if cli.rest.is_empty() {
        help::print_table_help(&table, std::io::stdout())?;
        return Ok(());
    }

    // `rye help` → top-level help
    if cli.rest == ["help"] {
        help::print_table_help(&table, std::io::stdout())?;
        return Ok(());
    }

    // `rye help <verb...>` → verb help
    if cli.rest.len() > 1 && cli.rest[0] == "help" {
        help::print_verb_help(&table, &cli.rest[1..], std::io::stdout())?;
        return Ok(());
    }

    // 5. Hardcoded `rye execute <item_ref>` — the universal escape hatch
    if cli.rest.first().map(|s| s.as_str()) == Some("execute") {
        if cli.rest.len() < 2 {
            return Err(CliError::UnknownVerb {
                argv: cli.rest.clone(),
            });
        }
        let item_ref = &cli.rest[1];
        // Validate it parses as a canonical ref
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref).map_err(|_| {
            crate::error::CliConfigError::InvalidExecuteRef {
                path: "<cli>".into(),
                item_ref: item_ref.clone(),
                detail: "not a valid canonical ref".into(),
            }
        })?;
        let required_cap = canonical_cap(&canonical.kind, &canonical.bare_id, "execute");

        let parameters = crate::arg_bind::bind_tail(&cli.rest[2..])?;
        let body = serde_json::json!({
            "item_ref": item_ref,
            "project_path": body_project_path,
            "parameters": parameters,
        });

        // Local cap pre-check: fail fast if we can determine the caller lacks the cap.
        precheck_cap(&state_dir, &required_cap)?;

        let bind = crate::transport::http::read_daemon_bind(&state_dir).await?;
        let signer = crate::transport::signing::Signer::resolve(&state_dir)?;
        let body_bytes = serde_json::to_vec(&body)
            .expect("infallible: Value serialization");
        let headers = signer.sign("POST", "/execute", &body_bytes)?;
        let payload = crate::transport::http::post_json(&bind, &headers, &body_bytes).await?;

        let result = payload
            .get("result")
            .cloned()
            .unwrap_or(payload);
        let pretty = serde_json::to_string_pretty(&result)
            .unwrap_or_else(|_| result.to_string());
        println!("{pretty}");

        return Ok(());
    }

    // 6. Match argv
    let (entry, tail) = match table.match_argv(&cli.rest) {
        Some(x) => x,
        None => {
            return Err(CliError::UnknownVerb {
                argv: cli.rest.clone(),
            });
        }
    };

    // 6. Build execute payload: { item_ref, parameters }
    let parameters = crate::arg_bind::bind_tail(tail)?;

    let body = serde_json::json!({
        "item_ref": entry.execute,
        "project_path": body_project_path,
        "parameters": parameters,
    });

    // Local cap pre-check: fail fast if we can determine the caller lacks the cap.
    precheck_cap(&state_dir, &entry.required_cap)?;

    // 7. Sign + POST to daemon /execute
    let bind = crate::transport::http::read_daemon_bind(&state_dir).await?;
    let signer = crate::transport::signing::Signer::resolve(&state_dir)?;
    let body_bytes = serde_json::to_vec(&body)
        .expect("infallible: Value serialization");
    let headers = signer.sign("POST", "/execute", &body_bytes)?;
    let payload = crate::transport::http::post_json(&bind, &headers, &body_bytes).await?;

    // 8. Print result
    let result = payload
        .get("result")
        .cloned()
        .unwrap_or(payload);
    let pretty = serde_json::to_string_pretty(&result)
        .unwrap_or_else(|_| result.to_string());
    println!("{pretty}");

    Ok(())
}

fn discover_state_dir() -> PathBuf {
    if let Ok(p) = std::env::var("RYEOS_STATE_DIR") {
        return PathBuf::from(p);
    }
    dirs::state_dir()
        .map(|d| d.join("ryeosd"))
        .unwrap_or_else(|| PathBuf::from(".ryeosd"))
}

/// Local capability pre-check.
///
/// Tries to load the authorized key file for the node's fingerprint.
/// If found, checks the derived `required_cap` against the key's scopes
/// using the unified `Authorizer`. If not found (auth disabled, operator
/// key), silently passes — the daemon is the final enforcement point.
///
/// This provides fail-fast UX: the CLI tells the operator what cap they
/// need *before* hitting the daemon, with clear error messages.
fn precheck_cap(state_dir: &std::path::Path, required_cap: &str) -> Result<(), CliError> {
    // Resolve the node signing key to get the fingerprint
    let signer = match crate::transport::signing::Signer::resolve(state_dir) {
        Ok(s) => s,
        Err(_) => return Ok(()), // No signing key yet (pre-init) — can't check
    };
    let fingerprint = &signer.fingerprint;

    // Try to load the authorized key file for this fingerprint.
    // Path matches ryeosd/src/auth.rs:load_authorized_key.
    let auth_dir = state_dir.join(".ai").join("node").join("authorized_keys");
    let key_file = auth_dir.join(format!("{fingerprint}.toml"));
    if !key_file.exists() {
        // No authorized key file → auth is disabled → daemon grants ["*"].
        // Nothing to check locally.
        return Ok(());
    }

    // Parse the TOML body to extract scopes.
    let raw = match std::fs::read_to_string(&key_file) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let scopes = parse_scopes_from_toml(&raw);

    // If the key has wildcard, always passes.
    if scopes.iter().any(|s| s == "*") {
        return Ok(());
    }

    // Check using the unified Authorizer.
    let registry = std::sync::Arc::new(ryeos_runtime::verb_registry::VerbRegistry::with_builtins());
    let authorizer = Authorizer::new(registry);
    let policy = AuthorizationPolicy::require_all(&[required_cap]);
    if authorizer.authorize(&scopes, &policy).is_err() {
        return Err(CliError::InsufficientCapabilities {
            required: required_cap.to_string(),
            fingerprint: fingerprint.clone(),
            scopes,
        });
    }

    Ok(())
}

/// Extract `scopes = [...]` from a TOML body.
fn parse_scopes_from_toml(raw: &str) -> Vec<String> {
    for line in raw.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() != "scopes" {
                continue;
            }
            let v = v.trim();
            if v.starts_with('[') && v.ends_with(']') {
                let inner = &v[1..v.len() - 1];
                return inner
                    .split(',')
                    .map(|s| {
                        let s = s.trim();
                        if (s.starts_with('"') && s.ends_with('"'))
                            || (s.starts_with('\'') && s.ends_with('\''))
                        {
                            s[1..s.len() - 1].to_string()
                        } else {
                            s.to_string()
                        }
                    })
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
    }
    Vec::new()
}
