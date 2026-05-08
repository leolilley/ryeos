use std::path::PathBuf;

use serde_json::Value;

use crate::error::CliError;
use crate::local_verbs;

/// CLI struct for clap argument parsing.
#[derive(clap::Parser)]
#[command(
    name = "ryeos",
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
    let body_project_path = match &cli.project {
        Some(p) => p.to_string_lossy().into_owned(),
        None => ".".to_string(),
    };

    // 2. System space dir
    let system_space_dir = discover_system_space_dir();

    // 3. Hardcoded LOCAL verbs (must work before daemon exists):
    //      ryos init                       — bootstrap operator state
    //      ryos trust pin --from <trust>   — pin a publisher key
    //      ryos publish <src>              — bundle author publish dance
    //      ryos vault {put,list,remove,rewrap} — sealed secret management
    if local_verbs::try_dispatch(&cli.rest)? {
        return Ok(());
    }

    // 4. No verb = help
    if cli.rest.is_empty() {
        crate::help::print_help(std::io::stdout())?;
        return Ok(());
    }

    // `ryeos help` → top-level help
    if cli.rest == ["help"] {
        crate::help::print_help(std::io::stdout())?;
        return Ok(());
    }

    // `ryeos help <verb...>` → verb help (queries daemon for alias info)
    if cli.rest.len() > 1 && cli.rest[0] == "help" {
        crate::help::print_verb_help(&cli.rest[1..], &system_space_dir, &body_project_path).await?;
        return Ok(());
    }

    // 5. Hardcoded `ryeos execute <item_ref>` — the universal escape hatch
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

        let result = post_to_daemon(&system_space_dir, &body).await?;
        print_result(result);
        return Ok(());
    }

    // 6. Token dispatch — send tokens to daemon, it resolves via alias
    //    registry and binds tail parameters server-side.
    let body = serde_json::json!({
        "tokens": cli.rest,
        "project_path": body_project_path,
    });

    let result = post_to_daemon(&system_space_dir, &body).await?;
    print_result(result);
    Ok(())
}

/// POST a JSON body to the daemon's /execute endpoint and return the response.
async fn post_to_daemon(
    system_space_dir: &std::path::Path,
    body: &Value,
) -> Result<Value, CliError> {
    let bind = crate::transport::http::read_daemon_bind(system_space_dir).await?;
    let signer = crate::transport::signing::Signer::resolve(system_space_dir)?;
    let body_bytes = serde_json::to_vec(body).expect("infallible: Value serialization");
    let headers = signer.sign("POST", "/execute", &body_bytes)?;
    let payload = crate::transport::http::post_json(&bind, &headers, &body_bytes).await?;
    Ok(payload)
}

fn print_result(payload: serde_json::Value) {
    let result = payload
        .get("result")
        .cloned()
        .unwrap_or(payload);
    let pretty = serde_json::to_string_pretty(&result)
        .unwrap_or_else(|_| result.to_string());
    println!("{pretty}");
}

fn discover_system_space_dir() -> PathBuf {
    if let Ok(p) = std::env::var("RYEOS_SYSTEM_SPACE_DIR") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .expect("could not determine XDG data directory")
}
