//! `ryeos-web-launcher` — mints a launch token and opens the browser.
//!
//! This binary is the `cli_exec` target for `client:ryeos/web`. It:
//! 1. Resolves the daemon URL from `daemon.json` or `RYEOSD_URL`.
//! 2. Calls the daemon's session minting endpoint to get a launch token.
//! 3. Opens the browser at `/ui/launch/<token>`.
//!
//! The daemon consumes the token, sets a session cookie, and redirects
//! the browser to `/ui`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(name = "ryeos-web-launcher", about = "Launch RyeOS in the browser")]
struct Cli {
    /// Surface ref to open (e.g. surface:ryeos/cockpit/base)
    #[arg(long = "surface")]
    surface: Option<String>,

    /// Project path
    #[arg(long = "project")]
    project: Option<PathBuf>,

    /// Read-only mode
    #[arg(long = "read-only")]
    read_only: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let _cli = Cli::parse();

    // Resolve daemon URL.
    let daemon_url = resolve_daemon_url()?;

    // Mint a launch token via the daemon.
    let token = mint_launch_token(&daemon_url).await?;

    // Build the launch URL.
    let launch_url = format!("{}/ui/launch/{}", daemon_url.trim_end_matches('/'), token);

    // Open the browser.
    eprintln!("Opening browser: {launch_url}");
    open_browser(&launch_url)?;

    Ok(())
}

fn resolve_daemon_url() -> Result<String> {
    // Check RYEOSD_URL env var first.
    if let Ok(url) = std::env::var("RYEOSD_URL") {
        return Ok(url.trim_end_matches('/').to_string());
    }

    // Try daemon.json discovery.
    let system_space_dir = discover_system_space_dir()?;
    let daemon_json = system_space_dir.join("daemon.json");
    if daemon_json.exists() {
        let raw = std::fs::read_to_string(&daemon_json)
            .context("read daemon.json")?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .context("parse daemon.json")?;
        if let Some(bind) = v.get("bind").and_then(|b| b.as_str()) {
            return Ok(format!("http://{bind}"));
        }
    }

    bail!("cannot resolve daemon URL: set RYEOSD_URL or ensure daemon is running")
}

fn discover_system_space_dir() -> Result<PathBuf> {
    // Match the CLI's discovery logic.
    if let Ok(dir) = std::env::var("RYEOS_SYSTEM_SPACE") {
        return Ok(PathBuf::from(dir));
    }
    let base = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .context("cannot discover local data dir")?;
    Ok(base.join("ryeos"))
}

async fn mint_launch_token(_daemon_url: &str) -> Result<String> {
    // For now, create a session directly by calling the daemon's
    // internal minting flow. The daemon has the session store;
    // we need to get a token through the existing bootstrap endpoint.
    //
    // Minimal approach: call the bootstrap endpoint to get a session,
    // then construct a launch URL. In a full implementation, the
    // launcher would call a dedicated `ui.launch.mint` endpoint.
    //
    // For now, generate a random token and register it with the daemon.
    // This requires the daemon to have a `ui/launch/mint` endpoint.
    // Since we don't have that yet, use a simpler approach:
    // just open the browser directly (the web client will bootstrap
    // itself via the /ui/api/bootstrap endpoint once auth is sorted).
    //
    // Phase 5 placeholder: generate a UUID-based token and return it.
    // The daemon's session minting will be wired in a follow-up.
    let token = uuid::Uuid::new_v4().to_string();
    Ok(token)
}

fn open_browser(url: &str) -> Result<()> {
    let result = if cfg!(target_os = "linux") {
        Command::new("xdg-open").arg(url).spawn()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else {
        bail!("unsupported OS for browser launch");
    };

    result.context("failed to open browser")?;
    Ok(())
}
