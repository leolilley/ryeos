pub mod keypair;
pub mod signing;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum IdentityAction {
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
        action: keypair::KeypairAction,
    },
}

pub fn run(action: IdentityAction) -> serde_json::Value {
    match action {
        IdentityAction::Sign { key_dir, hash } => signing::sign(&key_dir, &hash),
        IdentityAction::Verify {
            hash,
            signature,
            public_key,
        } => signing::verify(&hash, &signature, &public_key),
        IdentityAction::Keypair { action } => keypair::run(action),
    }
}
