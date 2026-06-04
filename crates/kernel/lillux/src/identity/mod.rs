pub mod envelope;
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
    /// Sealed secret envelope operations
    Envelope {
        #[command(subcommand)]
        action: EnvelopeAction,
    },
}

#[derive(Subcommand)]
pub enum EnvelopeAction {
    /// Decrypt a sealed envelope (reads envelope JSON from stdin)
    Open {
        /// Directory containing box_key.pem
        #[arg(long)]
        key_dir: String,
    },
    /// Seal an env map to a recipient (reads env map JSON from stdin)
    Seal {
        /// Path to recipient's box_pub.pem file
        #[arg(long, group = "recipient")]
        box_pub: Option<String>,
        /// Raw base64url-encoded X25519 public key
        #[arg(long, group = "recipient")]
        box_pub_inline: Option<String>,
        /// Path to identity document JSON with x25519:... box_key field
        #[arg(long, group = "recipient")]
        identity_doc: Option<String>,
    },
    /// Validate an env map for safety (reads env map JSON from stdin)
    Validate,
    /// Inspect envelope metadata without decrypting (reads envelope JSON from stdin)
    Inspect,
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
        IdentityAction::Envelope { action } => match action {
            EnvelopeAction::Open { key_dir } => envelope::cli_open(&key_dir),
            EnvelopeAction::Seal {
                box_pub,
                box_pub_inline,
                identity_doc,
            } => envelope::cli_seal(
                box_pub.as_deref(),
                box_pub_inline.as_deref(),
                identity_doc.as_deref(),
            ),
            EnvelopeAction::Validate => envelope::cli_validate(),
            EnvelopeAction::Inspect => envelope::cli_inspect(),
        },
    }
}
