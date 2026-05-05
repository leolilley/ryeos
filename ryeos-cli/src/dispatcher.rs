use std::path::PathBuf;

use crate::error::CliError;
use crate::help;
use crate::local_verbs;
use crate::verbs;

// ── Routing status ───────────────────────────────────────────────
//
// The daemon has a surface-aware `AliasRegistry` (loaded from
// `node/aliases/*.yaml`) with `(surface, tokens)` keying and
// longest-prefix matching via `match_argv()`. The CLI does NOT yet
// use it — this file still dispatches via the local `verbs` module
// and sends raw tokens to the daemon's `/execute` endpoint.
//
// Unification path:
//   1. CLI queries daemon for alias registry (or loads from bundle)
//   2. CLI uses `match_argv("cli", argv)` for routing
//   3. Delete this file's local verb table (`verbs` module)
//   4. CLI becomes a thin pipe: match argv → send to daemon
//
// Until then, the CLI and daemon have separate routing tables that
// must be kept in sync manually.

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
        let _canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref).map_err(|_| {
            crate::error::CliConfigError::InvalidExecuteRef {
                path: "<cli>".into(),
                item_ref: item_ref.clone(),
                detail: "not a valid canonical ref".into(),
            }
        })?;

        let parameters = crate::arg_bind::bind_tail(&cli.rest[2..])?;
        let body = serde_json::json!({
            "item_ref": item_ref,
            "project_path": body_project_path,
            "parameters": parameters,
        });

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
