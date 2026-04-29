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
/// Creates the node space layout, generates or loads BOTH the node signing
/// key (daemon-internal state) and the user signing key (operator edits),
/// writes the public identity document for the node, and bootstraps
/// self-trust for both keys.
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

    // 4. Generate or load the NODE signing key (daemon-internal state)
    let node_key_path = &config.node_signing_key_path;
    let node_identity = if options.force && node_key_path.exists() {
        tracing::info!(path = %node_key_path.display(), "regenerating node signing key (--force)");
        fs::remove_file(node_key_path)
            .with_context(|| format!("failed to remove old node key {}", node_key_path.display()))?;
        NodeIdentity::create(node_key_path)?
    } else if node_key_path.exists() {
        NodeIdentity::load(node_key_path)?
    } else {
        NodeIdentity::create(node_key_path)?
    };

    tracing::info!(
        fingerprint = %node_identity.fingerprint(),
        path = %node_key_path.display(),
        "node signing key ready"
    );

    // 5. Generate or load the USER signing key (operator edits in project/user space)
    let user_key_path = &config.user_signing_key_path;
    let user_identity = if options.force && user_key_path.exists() {
        tracing::info!(path = %user_key_path.display(), "regenerating user signing key (--force)");
        fs::remove_file(user_key_path)
            .with_context(|| format!("failed to remove old user key {}", user_key_path.display()))?;
        NodeIdentity::create(user_key_path)?
    } else if user_key_path.exists() {
        NodeIdentity::load(user_key_path)?
    } else {
        NodeIdentity::create(user_key_path)?
    };

    tracing::info!(
        fingerprint = %user_identity.fingerprint(),
        path = %user_key_path.display(),
        "user signing key ready"
    );

    // 6. Write public identity document (node only)
    let identity_path = config.state_dir.join(".ai").join("node").join("identity").join("public-identity.json");
    if options.force || !identity_path.exists() {
        node_identity.write_public_identity(&identity_path)?;
        tracing::info!(path = %identity_path.display(), "wrote node public identity");
    }

    // 7. Bootstrap self-trust: write verifying keys as trusted key docs
    let user_space = discover_user_root().unwrap_or_else(|| PathBuf::from("/tmp/missing-home"));
    let trust_dir = user_space.join(".ai").join("config").join("keys").join("trusted");

    // Node key trust doc
    let node_trust_entry = trust_dir.join(format!("{}.toml", node_identity.fingerprint()));
    if options.force || !node_trust_entry.exists() {
        write_self_trust(&trust_dir, &node_trust_entry, node_identity.verifying_key(), node_identity.signing_key())?;
    }

    // User key trust doc
    let user_trust_entry = trust_dir.join(format!("{}.toml", user_identity.fingerprint()));
    if options.force || !user_trust_entry.exists() {
        write_self_trust(&trust_dir, &user_trust_entry, user_identity.verifying_key(), user_identity.signing_key())?;
    }

    // NOTE: We intentionally do NOT write a node-config registration for the
    // system bundle here. `engine_init::build_engine` always adds
    // `config.system_data_dir` to its system roots unconditionally, so a
    // `bundles` registration pointing back at it would cause Phase 1 to
    // return that same path, which `engine_init` would then add a second
    // time (producing duplicate parsers / kinds at boot). The `bundles`
    // section is reserved for ADDITIONAL bundles installed via
    // `bundle.install`.

    Ok(())
}

/// Write a self-signed trusted-key TOML entry so the key's own signed items verify.
///
/// The document is signed by the key it declares (self-signature), using the
/// `# rye:signed:...` envelope format consumed by `TrustStore::load_three_tier`.
fn write_self_trust(
    trust_dir: &Path,
    trust_entry: &Path,
    verifying_key: &lillux::crypto::VerifyingKey,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<()> {
    fs::create_dir_all(trust_dir)
        .with_context(|| format!("failed to create trust dir {}", trust_dir.display()))?;

    let fingerprint = lillux::cas::sha256_hex(verifying_key.as_bytes());
    let key_b64 = base64::engine::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        verifying_key.as_bytes(),
    );

    let body = format!(
        r#"fingerprint = "{fingerprint}"
owner = "self"
version = "1.0.0"
attestation = ""

[public_key]
pem = "ed25519:{key_b64}"
"#
    );

    // Self-sign using the `# rye:signed:...` envelope
    let signed = lillux::signature::sign_content(&body, signing_key, "#", None);

    let tmp = trust_entry.with_extension("tmp");
    fs::write(&tmp, signed.as_bytes())
        .with_context(|| format!("failed to write trust entry {}", trust_entry.display()))?;
    fs::rename(&tmp, trust_entry)
        .with_context(|| format!("failed to rename {} → {}", tmp.display(), trust_entry.display()))?;

    tracing::info!(
        path = %trust_entry.display(),
        fingerprint = %fingerprint,
        "wrote self-signed trust entry"
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
    if !config.node_signing_key_path.exists() {
        tracing::warn!("no node signing key found — signed items will fail to verify");
    }
    if !config.user_signing_key_path.exists() {
        tracing::warn!("no user signing key found — operator-signed items will fail to verify");
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
            .load_full(&section_table, &bundle_records)
            .context("Phase 2: failed to load full node config")?,
    );
    tracing::info!(
        bundle_count = snapshot.bundles.len(),
        route_count = snapshot.routes.len(),
        "Phase 2: node config loaded"
    );

    Ok((engine, snapshot))
}
