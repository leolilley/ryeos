use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::identity::NodeIdentity;

pub struct InitOptions {
    pub force: bool,
}

/// One-time idempotent filesystem bootstrap.
pub fn init(config: &Config, options: &InitOptions) -> Result<()> {
    // 1. Create directory layout
    create_directory_layout(config)?;

    // 2. Generate node signing key
    let key_path = &config.signing_key_path;
    let identity = if key_path.exists() && !options.force {
        tracing::info!(path = %key_path.display(), "signing key already exists");
        NodeIdentity::load(key_path)?
    } else {
        if key_path.exists() {
            fs::remove_file(key_path)?;
        }
        let id = NodeIdentity::create(key_path)?;
        tracing::info!(path = %key_path.display(), "generated signing key");
        id
    };

    // 3. Write stable public identity document
    let identity_dir = config.state_dir.join("identity");
    let public_identity_path = identity_dir.join("public-identity.json");
    if !public_identity_path.exists() || options.force {
        identity.write_public_identity(&public_identity_path)?;
        tracing::info!(path = %public_identity_path.display(), "wrote public identity");
    }

    // 4. Write default config file if missing
    let config_path = config.state_dir.join("config.yaml");
    if !config_path.exists() {
        write_default_config(&config_path, config)?;
        tracing::info!(path = %config_path.display(), "wrote default config");
    }

    // 5. Create auth directory
    fs::create_dir_all(&config.authorized_keys_dir)?;

    // 6. Seed trust store with node's own public key
    //
    // Write to system_data_dir/.ai/config/keys/trusted/ — the canonical
    // path that TrustStore::load_three_tier scans. This ensures bootstrap
    // trust participates in engine verification.
    let trust_keys_dir = config
        .system_data_dir
        .join(rye_engine::AI_DIR)
        .join(rye_engine::TRUST_KEYS_DIR);
    fs::create_dir_all(&trust_keys_dir)?;
    let verifying_key = identity.verifying_key();
    rye_engine::trust::pin_key(
        verifying_key,
        &identity.principal_id(),
        &trust_keys_dir,
        Some(identity.signing_key()),
    )
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to seed trust store with node key");
        String::new()
    });

    tracing::info!(
        principal = %identity.principal_id(),
        state_dir = %config.state_dir.display(),
        "bootstrap complete"
    );

    Ok(())
}

fn create_directory_layout(config: &Config) -> Result<()> {
    let dirs = [
        config.state_dir.join("identity"),
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
    let key = &config.signing_key_path;
    if !key.exists() {
        anyhow::bail!(
            "ryeosd not initialized: signing key missing at {}\n\
             Run: rye daemon init",
            key.display()
        );
    }
    let identity_doc = config.state_dir.join("identity").join("public-identity.json");
    if !identity_doc.exists() {
        anyhow::bail!(
            "ryeosd not initialized: public identity missing at {}\n\
             Run: rye daemon init",
            identity_doc.display()
        );
    }
    Ok(())
}
