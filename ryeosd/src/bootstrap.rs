use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ed25519_dalek::pkcs8::DecodePrivateKey;

use crate::config::Config;

/// User signing key path: `~/.ai/config/keys/signing/private_key.pem`
const USER_KEY_REL: &str = ".ai/config/keys/signing/private_key.pem";

pub struct InitOptions {
    pub force: bool,
}

/// Resolve the user's signing key.
fn resolve_user_key() -> Option<PathBuf> {
    let user_space: PathBuf = std::env::var("USER_SPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            directories::BaseDirs::new()
                .map(|d| d.home_dir().to_path_buf())
                .unwrap_or_default()
        });
    let key_path = user_space.join(USER_KEY_REL);
    if key_path.exists() {
        tracing::info!(path = %key_path.display(), "found user signing key");
        Some(key_path)
    } else {
        tracing::info!(path = %key_path.display(), "no user signing key found");
        None
    }
}

/// One-time idempotent filesystem bootstrap.
///
/// Sets up the daemon's runtime directories (state, db, auth).
/// Does NOT generate keys — uses the user's existing key from ~/.ai/.
pub fn init(config: &Config, _options: &InitOptions) -> Result<()> {
    // 1. Create directory layout
    create_directory_layout(config)?;

    // 2. Write default config file if missing
    let config_path = config.state_dir.join("config.yaml");
    if !config_path.exists() {
        write_default_config(&config_path, config)?;
        tracing::info!(path = %config_path.display(), "wrote default config");
    }

    // 3. Create auth directory
    fs::create_dir_all(&config.authorized_keys_dir)?;

    tracing::info!(
        state_dir = %config.state_dir.display(),
        "bootstrap complete"
    );

    Ok(())
}

/// Sign all unsigned items in system bundle roots using the user's signing key.
///
/// Runs on every daemon start. If no user key exists, logs a warning and
/// returns (items will fail to verify at load time).
pub fn sign_unsigned_items(config: &Config) {
    let key_path = match resolve_user_key() {
        Some(p) => p,
        None => {
            tracing::warn!("no user signing key — cannot sign unsigned items");
            return;
        }
    };

    let sk = match ed25519_dalek::SigningKey::from_pkcs8_pem(
        &fs::read_to_string(&key_path).unwrap_or_default(),
    ) {
        Ok(sk) => sk,
        Err(e) => {
            tracing::error!(path = %key_path.display(), error = %e, "failed to load user signing key");
            return;
        }
    };

    let all_roots = config.all_system_roots();
    let mut signed = 0u32;
    let mut skipped = 0u32;

    for root in &all_roots {
        // Sign kind schemas
        let kinds_dir = root.join(rye_engine::AI_DIR).join("config").join("engine").join("kinds");
        if kinds_dir.is_dir() {
            signed += walk_and_sign(&kinds_dir, &sk, "#", &mut skipped);
        }

        // Sign bundle items (directives, tools, knowledge, configs)
        let ai_dir = root.join(rye_engine::AI_DIR);
        if ai_dir.is_dir() {
            signed += walk_and_sign_items(&ai_dir, &sk, &mut skipped);
        }
    }

    tracing::info!(signed, skipped, "bundle item signing");
}

/// Walk a directory and sign any unsigned .kind-schema.yaml files.
fn walk_and_sign(dir: &Path, sk: &ed25519_dalek::SigningKey, sig_prefix: &str, skipped: &mut u32) -> u32 {
    let mut count = 0u32;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += walk_and_sign(&path, sk, sig_prefix, skipped);
            continue;
        }
        if path.extension().map_or(false, |e| e == "yaml") {
            match sign_file_if_unsigned(&path, sk, sig_prefix) {
                Ok(true) => {
                    count += 1;
                    tracing::info!(path = %path.display(), "signed file");
                }
                Ok(false) => {
                    *skipped += 1;
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "failed to sign file");
                }
            }
        }
    }
    count
}

/// Walk a directory tree and sign any unsigned or wrong-key-signed items.
///
/// Signs .md, .py, .yaml/.yml files with the appropriate
/// signature prefix for each type.
fn walk_and_sign_items(dir: &Path, sk: &ed25519_dalek::SigningKey, skipped: &mut u32) -> u32 {
    let mut count = 0u32;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip config/ subdirectory (kind schemas handled separately)
            if path.file_name().map_or(false, |n| n == "config") {
                continue;
            }
            count += walk_and_sign_items(&path, sk, skipped);
            continue;
        }
        let sig_prefix = match path.extension().and_then(|e| e.to_str()) {
            Some("md") => "<!--",
            Some("yaml") | Some("yml") | Some("py") | Some("toml") => "#",
            _ => continue,
        };
        match sign_file_if_unsigned(&path, sk, sig_prefix) {
            Ok(true) => {
                count += 1;
            }
            Ok(false) => {
                *skipped += 1;
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to sign file");
            }
        }
    }
    count
}

/// Sign a file if it's not already signed by the current key.
/// Returns Ok(true) if signed (or re-signed), Ok(false) if skipped.
fn sign_file_if_unsigned(path: &Path, sk: &ed25519_dalek::SigningKey, sig_prefix: &str) -> Result<bool> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let our_fp = lillux::signature::compute_fingerprint(&sk.verifying_key());

    // Check if already signed by our key
    if let Some(first_line) = content.lines().next() {
        if let Some(header) = lillux::signature::parse_signature_line(first_line, sig_prefix, None) {
            if header.signer_fingerprint == our_fp {
                return Ok(false); // already signed by us
            }
            // Signed by a different key — strip old signature and re-sign
            let body = lillux::signature::strip_signature_lines(&content);
            let signed = lillux::signature::sign_content(&body, sk, sig_prefix, None);
            fs::write(path, &signed)
                .with_context(|| format!("failed to write {}", path.display()))?;
            tracing::info!(
                path = %path.display(),
                old_fp = %header.signer_fingerprint,
                new_fp = %our_fp,
                "re-signed file (was signed by different key)"
            );
            return Ok(true);
        }
    }

    // Not signed at all — sign it
    let signed = lillux::signature::sign_content(&content, sk, sig_prefix, None);
    fs::write(path, &signed)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(true)
}

fn create_directory_layout(config: &Config) -> Result<()> {
    let dirs = [
        config.state_dir.join("auth").join("authorized_keys"),
        config.state_dir.join("db"),
        config.cas_root.clone(),
    ];
    for dir in &dirs {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create directory {}", dir.display()))?;
    }
    Ok(())
}

fn write_default_config(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let yaml = serde_yaml::to_string(config)
        .context("failed to serialize default config")?;
    fs::write(path, yaml.as_bytes())?;
    Ok(())
}

/// Check if the daemon has been initialized.
pub fn verify_initialized(config: &Config) -> Result<()> {
    let state_dir = &config.state_dir;
    if !state_dir.exists() {
        anyhow::bail!(
            "ryeosd not initialized: state dir missing at {}\n\
             Run: rye daemon init",
            state_dir.display()
        );
    }
    if resolve_user_key().is_none() {
        tracing::warn!("no user signing key found — signed items will fail to verify");
    }
    Ok(())
}
