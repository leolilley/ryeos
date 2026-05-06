//! `rye-inspect` — unified inspection binary for fetch, verify, identity.
//!
//! Subcommands:
//!   fetch    — resolve, optionally verify, and read an item
//!   verify   — resolve and trust-verify an item
//!   identity — return the node's public identity document
//!
//! Each subcommand reads a JSON object from stdin (when `--stdin-json` is
//! set) or from command-line arguments, performs the operation, and
//! writes a JSON report to stdout. This is the binary that
//! `tool:rye/core/fetch`, `tool:rye/core/verify`, and
//! `tool:rye/core/identity/public_key` invoke via `bin:rye-inspect`.

use std::io::{self, Read};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rye-inspect",
    about = "Unified inspection binary (fetch, verify, identity)",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// Read params as JSON from stdin instead of using CLI flags.
    #[arg(long, global = true)]
    stdin_json: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Resolve and read an item through the engine.
    Fetch {
        /// Canonical ref to fetch (e.g. `tool:rye/core/fetch`).
        #[arg(long)]
        item_ref: Option<String>,

        /// Include the full file content in the report.
        #[arg(long)]
        with_content: bool,

        /// Also verify trust status.
        #[arg(long)]
        verify: bool,

        /// Project root path.
        #[arg(long)]
        project_path: Option<String>,
    },

    /// Resolve and trust-verify an item.
    Verify {
        /// Canonical ref to verify.
        #[arg(long)]
        item_ref: Option<String>,

        /// Project root path.
        #[arg(long)]
        project_path: Option<String>,
    },

    /// Return the node's public identity document.
    Identity {
        /// System space directory (defaults to XDG data dir / ryeos).
        #[arg(long)]
        system_space_dir: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rye-inspect: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Fetch {
            item_ref,
            with_content,
            verify,
            project_path,
        } => {
            let params = if cli.stdin_json {
                read_stdin_json()?
            } else {
                let ir = item_ref.ok_or_else(|| anyhow::anyhow!("--item-ref required"))?;
                let mut obj = serde_json::json!({
                    "item_ref": ir,
                    "with_content": with_content,
                    "verify": verify,
                });
                if let Some(p) = project_path {
                    obj["project_path"] = serde_json::json!(p);
                }
                obj
            };
            let params: ryeos_tools::actions::inspect::fetch::FetchParams =
                serde_json::from_value(params)?;
            let engine = ryeos_tools::actions::inspect::boot(params.project_path.as_deref().map(std::path::Path::new))?;
            let report = ryeos_tools::actions::inspect::fetch::run_fetch(params, &engine)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::Verify {
            item_ref,
            project_path,
        } => {
            let params = if cli.stdin_json {
                read_stdin_json()?
            } else {
                let ir = item_ref.ok_or_else(|| anyhow::anyhow!("--item-ref required"))?;
                let mut obj = serde_json::json!({ "item_ref": ir });
                if let Some(p) = project_path {
                    obj["project_path"] = serde_json::json!(p);
                }
                obj
            };
            let params: ryeos_tools::actions::inspect::verify::VerifyParams =
                serde_json::from_value(params)?;
            let engine = ryeos_tools::actions::inspect::boot(params.project_path.as_deref().map(std::path::Path::new))?;
            let report = ryeos_tools::actions::inspect::verify::run_verify(params, &engine)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::Identity { system_space_dir } => {
            let params = if cli.stdin_json {
                read_stdin_json()?
            } else {
                let mut obj = serde_json::json!({});
                if let Some(s) = system_space_dir {
                    obj["system_space_dir"] = serde_json::json!(s);
                }
                obj
            };
            let params: ryeos_tools::actions::inspect::identity::IdentityParams =
                serde_json::from_value(params)?;
            let report = ryeos_tools::actions::inspect::identity::run_identity(params)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

fn read_stdin_json() -> anyhow::Result<serde_json::Value> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    serde_json::from_str(&buf).map_err(|e| anyhow::anyhow!("parse stdin JSON: {e}"))
}
