use lillux::cas;
use lillux::exec;
use lillux::identity;
use lillux::time;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "lillux", about = "Lillux microkernel — Execute, Memory, Identity, Time")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Execute primitive — process lifecycle
    Exec {
        #[command(subcommand)]
        action: exec::ExecAction,
    },
    /// Memory primitive — content-addressed storage
    Cas {
        #[command(subcommand)]
        action: cas::CasAction,
    },
    /// Identity primitive — signing, verification, keypairs, sealed envelopes
    Identity {
        #[command(subcommand)]
        action: identity::IdentityAction,
    },
    /// Time primitive
    Time {
        #[command(subcommand)]
        action: time::TimeAction,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Exec { action } => exec::run(action),
        Command::Cas { action } => cas::run(action),
        Command::Identity { action } => identity::run(action),
        Command::Time { action } => time::run(action),
    };

    println!("{}", result);
}
