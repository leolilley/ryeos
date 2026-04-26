mod cmd;
mod exec;

use clap::Parser;

use cmd::ClientCmd;

#[derive(Parser)]
#[command(name = "rye", about = "Minimal CLI for Rye OS")]
struct Cli {
    #[command(subcommand)]
    command: ClientCmd,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let state_dir = discover_state_dir();
    match exec::dispatch(&state_dir, cli.command).await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Discover the daemon's state directory.
///
/// Checks:
/// 1. `RYEOS_STATE_DIR` environment variable
/// 2. XDG state dir / ryeosd
fn discover_state_dir() -> std::path::PathBuf {
    if let Ok(state_dir) = std::env::var("RYEOS_STATE_DIR") {
        return std::path::PathBuf::from(state_dir);
    }

    dirs::state_dir()
        .expect("failed to determine XDG state directory")
        .join("ryeosd")
}
