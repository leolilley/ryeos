use std::io::{self, Read};
use std::process::ExitCode;

use serde::Serialize;

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    success: bool,
    error: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let envelope = ErrorEnvelope {
                success: false,
                error: format!("{err:#}"),
            };
            println!(
                "{}",
                serde_json::to_string(&envelope).unwrap_or_else(|_| {
                    "{\"success\":false,\"error\":\"failed to serialize error\"}".to_string()
                })
            );
            ExitCode::SUCCESS
        }
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        anyhow::bail!("missing command; expected `search --stdin-json` or `fetch --stdin-json`");
    };
    if !args.any(|arg| arg == "--stdin-json") {
        anyhow::bail!("{command} requires --stdin-json");
    }
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw)?;
    match command.as_str() {
        "search" => println!(
            "{}",
            serde_json::to_string(&ryeos_web_tools::search::execute_json(&raw)?)?
        ),
        "fetch" => println!(
            "{}",
            serde_json::to_string(&ryeos_web_tools::fetch::execute_json(&raw)?)?
        ),
        _ => anyhow::bail!("unknown command `{command}`; expected `search` or `fetch`"),
    }
    Ok(())
}
