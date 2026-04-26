use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use ryeos_engine::engine::Engine;
use ryeos_engine::trust::TrustStore;

use crate::config::Config;
use crate::identity::NodeIdentity;
use crate::node_config::{NodeConfigSnapshot, SectionTable};

#[derive(Debug)]
pub struct InitOptions {
    pub force: bool,
}

/// One-time idempotent filesystem bootstrap.
///
/// Creates the node space layout, generates or loads the signing key,
/// writes the public identity document, and bootstraps self-trust.
#[tracing::instrument(name = "engine:lifecycle", skip(config), fields(event = "bootstrap"))]
pub fn init(config: &Config, options: &InitOptions) -> Result<()> {
    // 1. Create directory layout
    create_directory_layout(config)?;

    // 2. Write default config file if missing (or force rewrite)
    let config_path = config.state_dir.join("config.yaml");
    if options.force || !config_path.exists() {
        write_default_config(&config_path, config)?;
        tracing::info!(path = %config_path.display(), "wrote default config");
    }

    // 3. Create auth directory
    fs::create_dir_all(&config.authorized_keys_dir)?;

    // 4. Generate or load the user signing key
    let key_path = &config.signing_key_path;
    let identity = if options.force && key_path.exists() {
        // Force: regenerate the signing key
        tracing::info!(path = %key_path.display(), "regenerating signing key (--force)");
        fs::remove_file(key_path)
            .with_context(|| format!("failed to remove old key {}", key_path.display()))?;
        NodeIdentity::create(key_path)?
    } else if key_path.exists() {
        NodeIdentity::load(key_path)?
    } else {
        NodeIdentity::create(key_path)?
    };

    tracing::info!(
        fingerprint = %identity.fingerprint(),
        path = %key_path.display(),
        "signing key ready"
    );

    // 5. Write public identity document
    let identity_path = config.state_dir.join(".ai").join("identity").join("public-identity.json");
    if options.force || !identity_path.exists() {
        identity.write_public_identity(&identity_path)?;
        tracing::info!(path = %identity_path.display(), "wrote public identity");
    }

    // 6. Bootstrap self-trust: write the user's verifying key as a trusted key
    let user_space = discover_user_root().unwrap_or_else(|| PathBuf::from("/tmp/missing-home"));
    let trust_dir = user_space.join(".ai").join("config").join("keys").join("trusted");
    let trust_entry = trust_dir.join(format!("{}.toml", identity.fingerprint()));
    if options.force || !trust_entry.exists() {
        write_self_trust(&trust_dir, &trust_entry, identity.verifying_key())?;
    }

    // Write signed core bundle registration so Phase 1 bootstrap can discover it.
    let node_dir = config.state_dir.join(".ai").join("node").join("bundles");
    std::fs::create_dir_all(&node_dir).with_context(|| {
        format!("failed to create node config bundles dir {}", node_dir.display())
    })?;
    crate::node_config::loader::write_core_bundle_registration(
        &config.state_dir,
        &config.system_data_dir,
        &identity,
    )?;

    Ok(())
}

/// Write a self-trust TOML entry so the user's own signed items verify.
fn write_self_trust(
    trust_dir: &Path,
    trust_entry: &Path,
    verifying_key: &lillux::crypto::VerifyingKey,
) -> Result<()> {
    fs::create_dir_all(trust_dir)
        .with_context(|| format!("failed to create trust dir {}", trust_dir.display()))?;

    let fingerprint = lillux::cas::sha256_hex(verifying_key.as_bytes());
    // Use single-line `ed25519:<base64>` form — matches `TrustedKeyDoc::to_toml`
    // in ryeos-engine and avoids fragile multiline TOML quoting at write time.
    let key_b64 = base64::engine::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        verifying_key.as_bytes(),
    );

    let toml_content = format!(
        r#"version = "1.0.0"
category = "keys/trusted"
fingerprint = "{fingerprint}"
owner = "self"
attestation = ""

[public_key]
pem = "ed25519:{key_b64}"
"#
    );

    fs::write(trust_entry, toml_content.as_bytes())
        .with_context(|| format!("failed to write trust entry {}", trust_entry.display()))?;

    tracing::info!(
        path = %trust_entry.display(),
        fingerprint = %fingerprint,
        "wrote self-trust entry"
    );

    Ok(())
}

/// Discover the user-space root (parent of `~/.ai/`).
fn discover_user_root() -> Option<PathBuf> {
    std::env::var_os("USER_SPACE")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
}

// V5.2-CLOSEOUT: sign_unsigned_items + walk helpers deleted.
// Daemon bootstrap is bootstrap-only — must NEVER mutate
// system_data_dir or any operator/publisher-managed bundle.
// To sign bundle items use: cargo run --example resign_yaml -p ryeos-engine -- <path>


fn create_directory_layout(config: &Config) -> Result<()> {
    // Canonical paths — one CAS root under .ai/state/objects
    let state_root = config.state_dir.join(".ai").join("state");
    let dirs = [
        config.state_dir.join("auth").join("authorized_keys"),
        config.state_dir.join("db"),
        state_root.join("objects"),
        state_root.join("refs"),
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
             Run: rye init",
            state_dir.display()
        );
    }
    if !config.signing_key_path.exists() {
        tracing::warn!("no user signing key found — signed items will fail to verify");
    }
    Ok(())
}

/// Two-phase node-config bootstrap: shared by daemon-start and standalone paths.
///
/// 1. Phase 1: load bundle section from `system_data_dir` + `state_dir`
///    to determine effective bundle roots.
/// 2. Build the engine with those roots.
/// 3. Phase 2: full node-config scan across all sections → snapshot.
///
/// Trust continuity: the trust store used for node-config verification includes
/// the same sources the engine's `TrustStore::load_three_tier` uses (system +
/// user tiers). This ensures daemon-written `kind: node` items (signed by daemon
/// identity, whose trust lives in user-tier) verify on next boot.
///
/// Returns `(engine, node_config_snapshot)`.
pub fn load_node_config_two_phase(
    config: &Config,
) -> Result<(Arc<Engine>, Arc<NodeConfigSnapshot>)> {
    let system_data_dir = &config.system_data_dir;
    let state_dir = &config.state_dir;

    // Discover user root (same logic as engine_init)
    let user_root = discover_user_root();
    let system_roots_phase1 = vec![system_data_dir.to_path_buf()];

    // ── Phase 1: bootstrap trust store + bundle section ──
    // Use three-tier trust (same as engine_init) so daemon-written items verify.
    let bootstrap_trust_store = TrustStore::load_three_tier(
        None, // project root unknown at startup
        user_root.as_deref(),
        &system_roots_phase1,
    )
    .context("failed to load bootstrap trust store for node-config verification")?;

    let bootstrap_loader = crate::node_config::loader::BootstrapLoader {
        system_data_dir,
        state_dir,
        trust_store: &bootstrap_trust_store,
    };

    let bundle_records = bootstrap_loader
        .load_bundle_section()
        .context("Phase 1: failed to load bundle section from node config")?;

    let effective_bundle_roots: Vec<PathBuf> = bundle_records
        .iter()
        .map(|b| b.path.clone())
        .collect();

    tracing::info!(
        system_data_dir = %system_data_dir.display(),
        bundle_count = effective_bundle_roots.len(),
        trust_signers = bootstrap_trust_store.len(),
        "Phase 1: effective bundle roots determined"
    );

    // ── Build engine ──
    let engine = Arc::new(
        crate::engine_init::build_engine(config, &effective_bundle_roots)?,
    );

    // ── Phase 2: full node-config scan ──
    let section_table = SectionTable::new();
    let full_loader = crate::node_config::loader::BootstrapLoader {
        system_data_dir,
        state_dir,
        trust_store: &bootstrap_trust_store,
    };
    let snapshot = Arc::new(
        full_loader
            .load_full(&section_table)
            .context("Phase 2: failed to load full node config")?,
    );
    tracing::info!(
        bundle_count = snapshot.bundles.len(),
        "Phase 2: node config loaded"
    );

    Ok((engine, snapshot))
}
