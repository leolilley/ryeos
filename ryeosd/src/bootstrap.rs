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
        eprintln!(
            "ryeosd init: signing key already exists at {}",
            key_path.display()
        );
        NodeIdentity::load(key_path)?
    } else {
        if key_path.exists() {
            fs::remove_file(key_path)?;
        }
        let id = NodeIdentity::create(key_path)?;
        eprintln!(
            "ryeosd init: generated signing key at {}",
            key_path.display()
        );
        id
    };

    // 3. Write stable public identity document
    let identity_dir = config.state_dir.join("identity");
    let public_identity_path = identity_dir.join("public-identity.json");
    if !public_identity_path.exists() || options.force {
        identity.write_public_identity(&public_identity_path)?;
        eprintln!(
            "ryeosd init: wrote public identity to {}",
            public_identity_path.display()
        );
    }

    // 4. Write default config file if missing
    let config_path = config.state_dir.join("config.yaml");
    if !config_path.exists() {
        write_default_config(&config_path, config)?;
        eprintln!(
            "ryeosd init: wrote default config to {}",
            config_path.display()
        );
    }

    // 5. Create auth and trust directories
    fs::create_dir_all(&config.authorized_keys_dir)?;
    fs::create_dir_all(config.state_dir.join("trust").join("trusted_keys"))?;

    eprintln!("ryeosd init: bootstrap complete");
    eprintln!("  principal: {}", identity.principal_id());
    eprintln!("  state_dir: {}", config.state_dir.display());

    Ok(())
}

fn create_directory_layout(config: &Config) -> Result<()> {
    let dirs = [
        config.state_dir.join("identity"),
        config.state_dir.join("auth").join("authorized_keys"),
        config.state_dir.join("trust").join("trusted_keys"),
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
    let yaml = format!(
        "bind: \"{bind}\"\n\
         db_path: \"{db}\"\n\
         uds_path: \"{uds}\"\n\
         state_dir: \"{state_dir}\"\n\
         signing_key_path: \"{key}\"\n\
         cas_root: \"{cas}\"\n\
         require_auth: false\n\
         authorized_keys_dir: \"{auth_dir}\"\n",
        bind = config.bind,
        db = config.db_path.display(),
        uds = config.uds_path.display(),
        state_dir = config.state_dir.display(),
        key = config.signing_key_path.display(),
        cas = config.cas_root.display(),
        auth_dir = config.authorized_keys_dir.display(),
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
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
