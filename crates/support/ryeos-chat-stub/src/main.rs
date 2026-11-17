//! Standalone runner: serve a script file until interrupted.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use ryeos_chat_stub::{Script, StubServer};

#[derive(Parser)]
#[command(about = "Replay a scripted chat-completions conversation over HTTP")]
struct Args {
    /// Script file (YAML or JSON).
    script: PathBuf,
    /// Address to bind.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    /// Port to bind (0 picks a free one).
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Append every received request as one JSON line to this file.
    #[arg(long)]
    record: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let script = Script::from_file(&args.script)?;
    let addr: SocketAddr = format!("{}:{}", args.bind, args.port).parse()?;
    let server = StubServer::bind(addr, script, args.record).await?;
    println!("ryeos-chat-stub listening on {}", server.base_url);
    tokio::signal::ctrl_c().await?;
    Ok(())
}
