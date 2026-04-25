//! One-shot tool to re-sign a YAML file using the user's local signing
//! key, producing the long-form fingerprint the engine expects.
//!
//! Usage: cargo run --example resign_yaml -p ryeos-engine -- <path-to-yaml>

use std::env;
use std::fs;

fn main() {
    let path = env::args().nth(1).expect("usage: resign_yaml <path>");
    let key_path = std::path::PathBuf::from(env::var("HOME").unwrap())
        .join(".ai/config/keys/signing/private_key.pem");
    let sk = lillux::crypto::load_signing_key(&key_path).expect("load key");

    let body = fs::read_to_string(&path).unwrap();
    // Strip existing signature line(s) at top of file
    let body_stripped: String = body
        .lines()
        .skip_while(|l| l.starts_with("# rye:signed:"))
        .collect::<Vec<_>>()
        .join("\n");

    let signed = lillux::signature::sign_content(&body_stripped, &sk, "#", None);
    fs::write(&path, &signed).unwrap();
    println!("re-signed: {path}");
}
