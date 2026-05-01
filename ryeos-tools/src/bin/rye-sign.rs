//! `rye-sign` — operator-side validated sign binary.
//!
//! Reachable via `tool:rye/core/sign` (subprocess tool YAML) or
//! directly from the CLI. Resolves a canonical ref
//! (`<kind>:<bare-id>`) through the engine's kind registry, runs the
//! path-anchoring validator, then signs the resolved file in place
//! using the user signing key.
//!
//! Two input modes:
//!   * argv (clap) — direct CLI invocation:
//!       `rye-sign <ITEM_REF> [--project PATH] [--source project|user]`
//!   * stdin JSON — used by `tool:rye/core/sign` which passes
//!       `{params_json}` on stdin: an object with keys
//!       `item_ref`, optional `project_path`, optional `source`.
//!     Selected by `--stdin-json` so argv parsing is never ambiguous.
//!
//! Distinct from the daemon's `service:node-sign` (node-key, no
//! validator) — see `ryeosd/src/services/handlers/node_sign.rs`.

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use ryeos_tools::actions::sign::{run_sign, SignSource};

#[derive(Parser, Debug)]
#[command(
    name = "rye-sign",
    about = "Sign a Rye item by canonical ref after path-anchoring validation."
)]
struct Args {
    /// Canonical ref of the item to sign, e.g. `directive:hello`,
    /// `tool:rye/core/sign`, `config:cli/sign`. Required unless
    /// `--stdin-json` is set.
    #[arg(value_name = "ITEM_REF")]
    item_ref: Option<String>,

    /// Project root (parent of `.ai/`). Defaults to the current
    /// working directory.
    #[arg(long, value_name = "PATH")]
    project: Option<PathBuf>,

    /// Where to look for the item. `system` is rejected — bundle
    /// items are signed by their author key during bundle authoring.
    #[arg(long, value_name = "SOURCE", default_value = "project")]
    source: String,

    /// Read a JSON object from stdin instead of using argv.
    /// Object keys: `item_ref` (required), `project_path` (optional),
    /// `source` (optional). When set, the positional `ITEM_REF`
    /// argument is rejected to avoid ambiguity.
    #[arg(long)]
    stdin_json: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StdinParams {
    item_ref: String,
    #[serde(default)]
    project_path: Option<PathBuf>,
    #[serde(default = "default_source")]
    source: String,
}

fn default_source() -> String {
    "project".to_string()
}

fn main() -> ExitCode {
    let args = Args::parse();

    let (item_ref, project_arg, source_str) = if args.stdin_json {
        if args.item_ref.is_some() {
            eprintln!(
                "✗ --stdin-json is mutually exclusive with positional ITEM_REF"
            );
            return ExitCode::from(2);
        }
        let mut buf = String::new();
        if let Err(e) = io::stdin().read_to_string(&mut buf) {
            eprintln!("✗ read stdin: {e}");
            return ExitCode::from(2);
        }
        let parsed: StdinParams = match serde_json::from_str(&buf) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("✗ parse stdin JSON: {e}");
                return ExitCode::from(2);
            }
        };
        (parsed.item_ref, parsed.project_path, parsed.source)
    } else {
        match args.item_ref {
            Some(r) => (r, args.project, args.source),
            None => {
                eprintln!("✗ ITEM_REF required (or pass --stdin-json with stdin JSON)");
                return ExitCode::from(2);
            }
        }
    };

    let source = match SignSource::parse(&source_str) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("✗ {e}");
            return ExitCode::from(2);
        }
    };

    let project = match project_arg {
        Some(p) => Some(p),
        None => match std::env::current_dir() {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("✗ cannot read current directory: {e}");
                return ExitCode::from(2);
            }
        },
    };

    match run_sign(&item_ref, project.as_deref(), source) {
        Ok(batch) => match serde_json::to_string_pretty(&batch) {
            Ok(s) => {
                println!("{s}");
                if batch.is_total_success() {
                    ExitCode::SUCCESS
                } else {
                    eprintln!(
                        "✗ {}/{} items failed validation or signing",
                        batch.failed.len(),
                        batch.total()
                    );
                    ExitCode::FAILURE
                }
            }
            Err(e) => {
                eprintln!("✗ failed to serialize report: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("✗ sign failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}
