use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use ryeos_engine::engine::Engine;
use ryeos_engine::trust::TrustStore;

use crate::config::Config;
use crate::identity::NodeIdentity;
use crate::node_config::{NodeConfigSnapshot, SectionTable};

/// Bootstrap options.
///
/// `force` controls node-key regeneration ONLY. The user signing key is
/// never overwritten by init — it is always load-or-create. Node-key
/// rotation is a daemon-internal operation; user-key rotation requires
/// explicit operator action.
#[derive(Debug)]
pub struct InitOptions {
    /// If true, regenerate the node signing key even if one already exists.
    /// Does NOT affect the user signing key.
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
    let config_path = config.state_dir.join(".ai").join("node").join("config.yaml");
    if options.force || !config_path.exists() {
        write_default_config(&config_path, config)?;
        tracing::info!(path = %config_path.display(), "wrote default config");
    }

    // 3. Create auth directory
    fs::create_dir_all(&config.authorized_keys_dir)?;

    // Discover trust directory early — needed for stale-entry cleanup during
    // node-key regeneration.
    let user_space = discover_user_root().unwrap_or_else(|| PathBuf::from("/tmp/missing-home"));
    let trust_dir = user_space.join(".ai").join("config").join("keys").join("trusted");

    // 4. Generate or load the NODE signing key (daemon-internal state)
    let node_key_path = &config.node_signing_key_path;
    let node_identity = if options.force && node_key_path.exists() {
        // Before regenerating, clean up stale trust entries from old keys.
        // The trust entry path includes the fingerprint, so old entries become
        // orphans. We remove the known file; any other orphan cleanup is
        // deferred to explicit operator action.
        let old_identity = NodeIdentity::load(node_key_path)?;
        let old_trust = trust_dir.join(format!("{}.toml", old_identity.fingerprint()));
        if old_trust.exists() {
            tracing::info!(
                path = %old_trust.display(),
                old_fingerprint = %old_identity.fingerprint(),
                "removing stale node trust entry before regeneration"
            );
            fs::remove_file(&old_trust)
                .with_context(|| format!("failed to remove stale trust entry {}", old_trust.display()))?;
        }
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

    // 5. Generate or load the USER signing key (operator edits in project/user space).
    //    NEVER force-regenerate: the user key is the operator's persistent identity.
    //    Rotation is an explicit out-of-band action.
    let user_key_path = &config.user_signing_key_path;
    let user_identity = if user_key_path.exists() {
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
    // (trust_dir computed above, before node-key regeneration)

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
    // Node root layout: config, auth, vault, and CAS state live under .ai/.
    //
    // The vault directory is currently a placeholder — keypair generation
    // and sealed-envelope encryption land in a later step (see
    // .tmp/POST-KINDS-FLIP-PLAN.md §7). Reserved here so the layout is
    // stable across the v1 → vault upgrade.
    let dirs = [
        config.state_dir.join(".ai").join("node").join("auth").join("authorized_keys"),
        config.state_dir.join(".ai").join("node").join("vault"),
        config.state_dir.join(".ai").join("state").join("objects"),
        config.state_dir.join(".ai").join("state").join("refs"),
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
/// Trust continuity: the trust store used for node-config verification is
/// loaded via the engine's `TrustStore::load_three_tier`, which sources trust
/// from operator tiers ONLY (project + user). The daemon's identity must
/// have its trust doc pinned in the user tier (created by `rye init` /
/// daemon bootstrap) for daemon-written `kind: node` items to verify on
/// next boot.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::NodeIdentity;

    /// Build a minimal Config for testing with all paths under a tempdir.
    fn test_config(tmp: &std::path::Path) -> Config {
        let state_dir = tmp.join("state");
        let user_keys = tmp.join("user_keys");
        std::fs::create_dir_all(state_dir.join(".ai").join("node").join("auth").join("authorized_keys")).unwrap();
        std::fs::create_dir_all(state_dir.join(".ai").join("state")).unwrap();
        std::fs::create_dir_all(user_keys.join("signing")).unwrap();
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: state_dir.join(".ai").join("state").join("runtime.sqlite3"),
            uds_path: state_dir.join("ryeosd.sock"),
            state_dir: state_dir.clone(),
            node_signing_key_path: state_dir
                .join(".ai")
                .join("node")
                .join("identity")
                .join("private_key.pem"),
            user_signing_key_path: user_keys.join("signing").join("private_key.pem"),
            authorized_keys_dir: state_dir.join(".ai").join("node").join("auth").join("authorized_keys"),
            system_data_dir: tmp.join("system"),
            require_auth: false,
        }
    }

    /// Helper: extract the fingerprint from a PEM key file.
    fn fingerprint_at(path: &std::path::Path) -> String {
        let id = NodeIdentity::load(path).unwrap();
        id.fingerprint().to_string()
    }

    /// Process-wide mutex for tests that mutate the `USER_SPACE` env var.
    /// Without this, parallel tests in this module race on the shared env
    /// and observe each other's `USER_SPACE` values, producing trust-dir
    /// paths under the wrong tempdir and bogus "stale entry not removed"
    /// failures.
    static USER_SPACE_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that holds `USER_SPACE_MUTEX`, sets `USER_SPACE` for the
    /// duration of the test, and unsets it on drop (not restore — avoids
    /// inheriting stale values from previous tests).
    struct UserSpaceGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl UserSpaceGuard {
        fn new(tmp: &std::path::Path) -> Self {
            // Recover from prior panics: PoisonError still exposes the guard.
            let lock = USER_SPACE_MUTEX
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::env::set_var("USER_SPACE", tmp);
            Self { _lock: lock }
        }
    }

    impl Drop for UserSpaceGuard {
        fn drop(&mut self) {
            std::env::remove_var("USER_SPACE");
        }
    }

    #[test]
    fn init_creates_both_keys_on_fresh_state() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let _guard = UserSpaceGuard::new(tmp.path());

        init(&config, &InitOptions { force: false }).unwrap();

        assert!(config.node_signing_key_path.exists(), "node key should be created");
        assert!(config.user_signing_key_path.exists(), "user key should be created");

        // Trust entries for both keys — trust_dir = <USER_SPACE>/.ai/config/keys/trusted/
        let trust_dir = tmp.path().join(".ai").join("config").join("keys").join("trusted");
        let node_fp = fingerprint_at(&config.node_signing_key_path);
        let user_fp = fingerprint_at(&config.user_signing_key_path);
        assert!(trust_dir.join(format!("{}.toml", node_fp)).exists(), "node trust entry");
        assert!(trust_dir.join(format!("{}.toml", user_fp)).exists(), "user trust entry");
    }

    #[test]
    fn init_idempotent_reuses_existing_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let _guard = UserSpaceGuard::new(tmp.path());

        init(&config, &InitOptions { force: false }).unwrap();
        let node_fp1 = fingerprint_at(&config.node_signing_key_path);
        let user_fp1 = fingerprint_at(&config.user_signing_key_path);

        // Second init without force — keys should be the same
        init(&config, &InitOptions { force: false }).unwrap();
        let node_fp2 = fingerprint_at(&config.node_signing_key_path);
        let user_fp2 = fingerprint_at(&config.user_signing_key_path);

        assert_eq!(node_fp1, node_fp2, "node key should not change on idempotent init");
        assert_eq!(user_fp1, user_fp2, "user key should not change on idempotent init");
    }

    #[test]
    fn force_regenerates_node_key_but_preserves_user_key() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let _guard = UserSpaceGuard::new(tmp.path());

        init(&config, &InitOptions { force: false }).unwrap();
        let user_fp_before = fingerprint_at(&config.user_signing_key_path);
        let node_fp_before = fingerprint_at(&config.node_signing_key_path);

        // Force regenerate
        init(&config, &InitOptions { force: true }).unwrap();
        let user_fp_after = fingerprint_at(&config.user_signing_key_path);
        let node_fp_after = fingerprint_at(&config.node_signing_key_path);

        assert_ne!(
            node_fp_before, node_fp_after,
            "node key SHOULD change with --force"
        );
        assert_eq!(
            user_fp_before, user_fp_after,
            "user key MUST NOT change with --force"
        );

        // Trust dir
        let trust_dir = tmp.path().join(".ai").join("config").join("keys").join("trusted");

        // Old node trust entry should be cleaned up
        assert!(
            !trust_dir.join(format!("{}.toml", node_fp_before)).exists(),
            "stale node trust entry should be removed after force regeneration"
        );
        // New node trust entry should exist
        assert!(
            trust_dir.join(format!("{}.toml", node_fp_after)).exists(),
            "new node trust entry should exist after force regeneration"
        );
    }

    #[test]
    fn force_creates_fresh_keys_when_none_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let _guard = UserSpaceGuard::new(tmp.path());

        init(&config, &InitOptions { force: true }).unwrap();

        assert!(config.node_signing_key_path.exists(), "node key should be created even with --force on fresh state");
        assert!(config.user_signing_key_path.exists(), "user key should be created on fresh state");
    }
}
