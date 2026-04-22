//! rye-sign — sign items with Ed25519 inline comment signatures

use anyhow::{bail, Context, Result};
use clap::Parser;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::SigningKey;
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

/// Signature report structure
#[derive(Debug, Serialize)]
struct SignatureReport {
    file: String,
    signer_fingerprint: String,
    signature_line: String,
    updated_at: String,
}

#[derive(Parser)]
#[command(name = "rye-sign")]
#[command(about = "Sign items with Ed25519 inline comment signatures")]
struct Args {
    /// File to sign (YAML, JSON, or Markdown)
    input: PathBuf,

    /// Signing key path (PEM format) or use RYE_SIGNING_KEY env var
    #[arg(short, long)]
    key: Option<PathBuf>,

    /// Output format (json or text)
    #[arg(short, long, default_value = "json")]
    format: String,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Read input file
    let content = fs::read_to_string(&args.input)
        .with_context(|| format!("failed to read input file: {}", args.input.display()))?;

    // Load signing key
    let key_path = args
        .key
        .or_else(|| std::env::var("RYE_SIGNING_KEY").ok().map(PathBuf::from))
        .context("No signing key provided via --key or RYE_SIGNING_KEY env var")?;

    let key_pem = fs::read_to_string(&key_path)
        .with_context(|| format!("failed to read signing key: {}", key_path.display()))?;

    let signing_key = SigningKey::from_pkcs8_pem(&key_pem)
        .context("failed to parse signing key (must be Ed25519 PKCS8 PEM)")?;

    // Compute fingerprint (SHA256 of verifying key)
    let verifying_key = signing_key.verifying_key();
    let fingerprint = lillux::sha256_hex(verifying_key.as_bytes());

    // Determine signature envelope based on file extension
    let (sig_prefix, sig_suffix) = determine_envelope(&args.input)?;

    // Sign the content with inline comment signature
    let signed_content = lillux::signature::sign_content(
        &content,
        &signing_key,
        &sig_prefix,
        sig_suffix.as_deref(),
    );

    // Write signed content back to the file (atomic write)
    let tmp_path = args.input.with_extension("signed.tmp");
    fs::write(&tmp_path, &signed_content)
        .with_context(|| format!("failed to write temporary file: {}", tmp_path.display()))?;

    fs::rename(&tmp_path, &args.input)
        .with_context(|| format!("failed to rename signed file: {}", args.input.display()))?;

    // Extract signature line for reporting
    let signature_line = extract_signature_line(&signed_content, &sig_prefix)
        .unwrap_or_else(|| "signature applied".to_string());

    // Build report
    let report = SignatureReport {
        file: args.input.display().to_string(),
        signer_fingerprint: fingerprint,
        signature_line,
        updated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };

    // Output report
    match args.format.as_str() {
        "json" => {
            let json_report = serde_json::to_string_pretty(&report)?;
            println!("{}", json_report);
        }
        "text" => {
            println!("Signed: {}", report.file);
            println!("Signer: {}", report.signer_fingerprint);
            println!("Updated: {}", report.updated_at);
        }
        other => bail!("unsupported output format: {}", other),
    }

    Ok(())
}

/// Determine signature envelope (prefix/suffix) based on file extension
fn determine_envelope(path: &PathBuf) -> Result<(String, Option<String>)> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("yaml") | Some("yml") => Ok(("# rye:signed".to_string(), None)),
        Some("json") => Ok(("// rye:signed".to_string(), None)),
        Some("md") | Some("markdown") => Ok(("<!-- rye:signed".to_string(), Some("-->".to_string()))),
        Some("py") => Ok(("# rye:signed".to_string(), None)),
        Some("rs") => Ok(("// rye:signed".to_string(), None)),
        Some("ts") | Some("js") => Ok(("// rye:signed".to_string(), None)),
        Some(ext) => bail!(
            "unsupported file type: .{}. Supported: yaml, json, md, py, rs, ts, js",
            ext
        ),
        None => bail!("file has no extension"),
    }
}

/// Extract the first signature line from signed content
fn extract_signature_line(content: &str, _prefix: &str) -> Option<String> {
    content
        .lines()
        .find(|line| line.contains("rye:signed"))
        .map(|line| line.trim().to_string())
}
