use serde::Serialize;
use std::io::{self, Read};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    success: bool,
    error: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            println!(
                "{}",
                serde_json::to_string(&ErrorEnvelope {
                    success: false,
                    error: format!("{err:#}")
                })
                .unwrap()
            );
            ExitCode::SUCCESS
        }
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        anyhow::bail!("missing command; expected `browser --stdin-json`");
    };
    if !args.any(|arg| arg == "--stdin-json") {
        anyhow::bail!("{command} requires --stdin-json");
    }
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw)?;
    match command.as_str() {
        "browser" => println!(
            "{}",
            serde_json::to_string(&ryeos_browser_tools::browser::execute_json(&raw)?)?
        ),
        _ => anyhow::bail!("unknown command `{command}`; expected `browser`"),
    }
    Ok(())
}
