//! One-shot tool to sign or re-sign a YAML file using the user's local
//! signing key, producing the long-form fingerprint the engine expects.
//!
//! Usage:
//!     cargo run --example resign_yaml -p ryeos-engine -- <path-to-yaml> [--force]
//!
//! Safety defaults (V5.2-CLOSEOUT):
//! - If the file is unsigned: signs it.
//! - If the file is already signed by the SAME key: re-signs (timestamp refresh).
//! - If the file is signed by a DIFFERENT key: REFUSES unless `--force` is passed.
//!   Rationale: silently overwriting another signer's authorship is the failure
//!   mode that polluted `ryeos-bundles/core/` during V5.2-CLOSEOUT debugging.
//!   Make it explicit.

use std::env;
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let path = match args.next() {
        Some(p) => p,
        None => {
            eprintln!("usage: resign_yaml <path-to-yaml> [--force]");
            return ExitCode::from(2);
        }
    };
    let force = args.any(|a| a == "--force");

    let key_path = std::path::PathBuf::from(env::var("HOME").expect("HOME"))
        .join(".ai/config/keys/signing/private_key.pem");
    let sk = lillux::crypto::load_signing_key(&key_path).expect("load signing key");
    let our_fp = lillux::signature::compute_fingerprint(&sk.verifying_key());

    let body = fs::read_to_string(&path).expect("read file");

    // Inspect the existing signature line, if any.
    let first_line = body.lines().next().unwrap_or("");
    if let Some(header) = lillux::signature::parse_signature_line(first_line, "#", None) {
        if header.signer_fingerprint != our_fp && !force {
            eprintln!(
                "refusing to re-sign {path}: existing signer is {}, but our key is {}.\n\
                 If you really intend to take over authorship, re-run with --force.",
                header.signer_fingerprint, our_fp
            );
            return ExitCode::from(3);
        }
    }

    let body_stripped: String = body
        .lines()
        .skip_while(|l| l.starts_with("# rye:signed:"))
        .collect::<Vec<_>>()
        .join("\n");

    let signed = lillux::signature::sign_content(&body_stripped, &sk, "#", None);
    fs::write(&path, &signed).expect("write file");
    println!("re-signed: {path}");
    ExitCode::SUCCESS
}
