mod cas;
mod exec;
mod identity;
mod time;

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
    /// Sign a hash with Ed25519
    Sign {
        /// Directory containing private_key.pem
        #[arg(long)]
        key_dir: String,
        /// SHA256 hex digest to sign
        #[arg(long)]
        hash: String,
    },
    /// Verify an Ed25519 signature
    Verify {
        /// SHA256 hex digest that was signed
        #[arg(long)]
        hash: String,
        /// Base64url-encoded signature
        #[arg(long, allow_hyphen_values = true)]
        signature: String,
        /// Path to public_key.pem
        #[arg(long)]
        public_key: String,
    },
    /// Keypair management
    Keypair {
        #[command(subcommand)]
        action: identity::KeypairAction,
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
        Command::Sign { key_dir, hash } => identity::sign(&key_dir, &hash),
        Command::Verify { hash, signature, public_key } => {
            identity::verify(&hash, &signature, &public_key)
        }
        Command::Keypair { action } => identity::run(action),
        Command::Time { action } => time::run(action),
    };

    println!("{}", result);
}
