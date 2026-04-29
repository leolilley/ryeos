//! Operator-side `rye init` — bootstraps user space, node space, pins the
//! platform author key into the operator's trust store, lays down the core
//! bundle, and (unless `--core-only`) installs the standard bundle.
//!
//! Idempotent. Re-running keeps existing keys; only fills in missing pieces.
//! Refuses on inconsistent state — e.g. trust-doc fingerprint doesn't match
//! the key on disk. Refusing means a wipe-and-reinit recovery is required;
//! see `docs/operator-init-recipe.md` (NOT something `rye init` does).
//!
//! `rye init` does NOT auto-import trust docs from any bundle. The platform
//! author key is hardcoded in this source ([`PLATFORM_AUTHOR_PUBKEY`]) and
//! pinned explicitly. Third-party bundle authors are pinned via
//! `rye trust pin <fingerprint>`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use lillux::crypto::{
    DecodePrivateKey, EncodePrivateKey, SigningKey, VerifyingKey,
};
use rand::rngs::OsRng;
use serde::Serialize;

use ryeos_engine::trust::{compute_fingerprint, pin_key, TrustStore};

/// SHA-256 fingerprint of the platform author Ed25519 public key.
///
/// This is the long-lived release key under which all official `core` and
/// `standard` bundles are signed in the public registry. Hardcoded here
/// so `rye init` can pin it without trusting any on-disk source. Rotation
/// is rare and requires a coordinated release of a new `rye` binary.
pub const PLATFORM_AUTHOR_FP: &str =
    "09674c8998e9dd01bfc40ec9f8c4b6b2c1bd01333842582a9c34b3c7db5aa86c";

/// Raw 32-byte Ed25519 public key for the platform author.
///
/// Encoded inline so `rye init` does NOT need to read any bundle file to
/// pin trust. The fingerprint over these bytes MUST equal
/// [`PLATFORM_AUTHOR_FP`] — verified at init time.
pub const PLATFORM_AUTHOR_PUBKEY: [u8; 32] = [
    0x01, 0x16, 0x95, 0xa5, 0x8f, 0x1d, 0xd6, 0x20,
    0x0a, 0x84, 0xab, 0x8b, 0xb8, 0x36, 0xc4, 0x3d,
    0x92, 0x29, 0x75, 0x19, 0x9b, 0xd7, 0x41, 0xfa,
    0x42, 0x4b, 0xae, 0x5e, 0xa3, 0x69, 0x64, 0x0e,
];

#[derive(Debug)]
pub struct InitOptions {
    /// Daemon state root (parent of `.ai/`). Defaults to XDG state dir.
    pub state_dir: PathBuf,
    /// User space root (parent of `~/.ai/`). Defaults to `$HOME`.
    pub user_root: PathBuf,
    /// Where the core bundle should live (system data dir). The CLI copies
    /// `core_source` here when first laying down the platform.
    pub system_data_dir: PathBuf,
    /// Source tree to copy `core` from. Required — the operator points this
    /// at the bundled `core` from their package install (e.g.
    /// `/usr/share/ryeos/bundles/core`) or at the dev tree
    /// `ryeos-bundles/core`.
    pub core_source: PathBuf,
    /// Source tree to copy `standard` from. Required unless `core_only`.
    pub standard_source: Option<PathBuf>,
    /// Skip installing standard. Positive framing — opt-in to bare core.
    pub core_only: bool,
    /// Force-regenerate the node signing key. Does NOT touch the user key.
    pub force_node_key: bool,
}

#[derive(Debug, Serialize)]
pub struct InitReport {
    pub user_key_fingerprint: String,
    pub node_key_fingerprint: String,
    pub platform_author_pinned: String,
    pub core_installed_at: PathBuf,
    pub standard_installed_at: Option<PathBuf>,
    pub vault_dir: PathBuf,
    pub trust_dir: PathBuf,
    pub next_steps: Vec<String>,
}

/// Run `rye init` end-to-end.
///
/// Order:
///   1. Layout: create `<state>/.ai/{node,state,bundles}` + vault placeholder
///   2. User key (load-or-create at `<user>/.ai/config/keys/signing/private_key.pem`)
///   3. Node key (load-or-create at `<state>/.ai/node/identity/private_key.pem`)
///   4. Self-trust both keys (write signed `<fp>.toml` into user trust dir)
///   5. Pin platform author key into user trust dir
///   6. Lay down core at `system_data_dir` (copy from `core_source`)
///   7. Install standard bundle (unless `core_only`) — copy + signed
///      registration record
///   8. Verify post-init trust store contains all required keys; refuse if not
pub fn run_init(opts: &InitOptions) -> Result<InitReport> {
    // ── 1. Layout ──
    create_layout(&opts.state_dir, &opts.user_root)?;

    let trust_dir = opts
        .user_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("keys")
        .join("trusted");
    fs::create_dir_all(&trust_dir).with_context(|| {
        format!("failed to create trust dir {}", trust_dir.display())
    })?;

    // ── 2. User key ──
    let user_key_path = opts
        .user_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("keys")
        .join("signing")
        .join("private_key.pem");
    let user_key = load_or_create_key(&user_key_path, false)
        .with_context(|| format!("user key at {}", user_key_path.display()))?;
    let user_fp = compute_fingerprint(&user_key.verifying_key());

    // ── 3. Node key ──
    let node_key_path = opts
        .state_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("identity")
        .join("private_key.pem");
    let node_key = load_or_create_key(&node_key_path, opts.force_node_key)
        .with_context(|| format!("node key at {}", node_key_path.display()))?;
    let node_fp = compute_fingerprint(&node_key.verifying_key());

    // ── 4. Self-trust both keys ──
    pin_key(&user_key.verifying_key(), "user", &trust_dir, Some(&user_key))
        .map_err(|e| anyhow!("pin user trust doc: {e}"))?;
    pin_key(&node_key.verifying_key(), "node", &trust_dir, Some(&node_key))
        .map_err(|e| anyhow!("pin node trust doc: {e}"))?;

    // ── 5. Pin platform author key ──
    let platform_vk = decode_platform_author_pubkey()?;
    let pinned_fp = pin_key(&platform_vk, "platform-author", &trust_dir, None)
        .map_err(|e| anyhow!("pin platform author trust doc: {e}"))?;
    if pinned_fp != PLATFORM_AUTHOR_FP {
        bail!(
            "platform author fingerprint mismatch: hardcoded {} but \
             public key bytes hash to {}",
            PLATFORM_AUTHOR_FP,
            pinned_fp
        );
    }

    // ── 6. Lay down core bundle ──
    if !opts.core_source.is_dir() {
        bail!(
            "core_source is not a directory: {}",
            opts.core_source.display()
        );
    }
    if opts.system_data_dir.exists() {
        verify_core_already_consistent(&opts.system_data_dir, &opts.core_source)?;
        // Verify the on-disk core is still signature-valid against the
        // trust we just pinned. Any drift here is a hard failure — we
        // refuse to proceed rather than risk handing an unverified core
        // to the daemon.
        crate::actions::install::preflight_verify_bundle(
            &opts.system_data_dir,
            &opts.system_data_dir,
            Some(opts.user_root.as_path()),
        )
        .context("verify on-disk core against pinned platform author key")?;
    } else {
        // Verify the source BEFORE copying so a tampered core never
        // touches the operator's system_data_dir.
        crate::actions::install::preflight_verify_bundle(
            &opts.core_source,
            &opts.core_source,
            Some(opts.user_root.as_path()),
        )
        .context("verify core source against pinned platform author key")?;
        copy_dir_recursive(&opts.core_source, &opts.system_data_dir).with_context(
            || {
                format!(
                    "lay down core: {} -> {}",
                    opts.core_source.display(),
                    opts.system_data_dir.display()
                )
            },
        )?;
    }

    // ── 7. Install standard bundle ──
    let standard_installed_at = if opts.core_only {
        None
    } else {
        let standard_source = opts
            .standard_source
            .as_ref()
            .ok_or_else(|| anyhow!("standard_source is required unless --core-only"))?;
        if !standard_source.is_dir() {
            bail!(
                "standard_source is not a directory: {}",
                standard_source.display()
            );
        }
        let target = opts
            .state_dir
            .join(ryeos_engine::AI_DIR)
            .join("bundles")
            .join("standard");
        if target.exists() {
            verify_bundle_already_installed(&target, standard_source)?;
            Some(target.canonicalize()?)
        } else {
            install_standard_bundle(
                &opts.state_dir,
                standard_source,
                &node_key,
                &opts.system_data_dir,
                opts.user_root.as_path(),
            )?
        }
    };

    // ── 8. Post-init trust verification ──
    let post_trust = TrustStore::load_three_tier(
        None,
        Some(opts.user_root.as_path()),
        &[opts.system_data_dir.clone()],
    )
    .context("load post-init trust store")?;
    if !post_trust.is_trusted(PLATFORM_AUTHOR_FP) {
        bail!(
            "post-init self-check failed: platform author key {} is \
             not in the loaded trust store — trust dir at {}",
            PLATFORM_AUTHOR_FP,
            trust_dir.display()
        );
    }
    if !post_trust.is_trusted(&user_fp) {
        bail!(
            "post-init self-check failed: user key {} not loadable — \
             trust dir at {}",
            user_fp,
            trust_dir.display()
        );
    }
    if !post_trust.is_trusted(&node_fp) {
        bail!(
            "post-init self-check failed: node key {} not loadable — \
             trust dir at {}",
            node_fp,
            trust_dir.display()
        );
    }

    let vault_dir = opts.state_dir.join(ryeos_engine::AI_DIR).join("node").join("vault");
    fs::create_dir_all(&vault_dir)
        .with_context(|| format!("create vault dir {}", vault_dir.display()))?;

    let mut next_steps = vec![
        format!(
            "Start the daemon: ryeosd --state-dir {} --system-data-dir {}",
            opts.state_dir.display(),
            opts.system_data_dir.display()
        ),
        "Try a verb: rye status".to_string(),
    ];
    if opts.core_only {
        next_steps.push(
            "Install the standard bundle later: rye bundle install --name standard \
             --source-path <path>"
                .to_string(),
        );
    }

    Ok(InitReport {
        user_key_fingerprint: user_fp,
        node_key_fingerprint: node_fp,
        platform_author_pinned: PLATFORM_AUTHOR_FP.to_string(),
        core_installed_at: opts.system_data_dir.clone(),
        standard_installed_at,
        vault_dir,
        trust_dir,
        next_steps,
    })
}

/// Decode the hardcoded platform author public key into a `VerifyingKey`,
/// guaranteeing the fingerprint matches [`PLATFORM_AUTHOR_FP`].
fn decode_platform_author_pubkey() -> Result<VerifyingKey> {
    let vk = VerifyingKey::from_bytes(&PLATFORM_AUTHOR_PUBKEY)
        .map_err(|e| anyhow!("hardcoded platform author key invalid: {e}"))?;
    let fp = compute_fingerprint(&vk);
    if fp != PLATFORM_AUTHOR_FP {
        bail!(
            "internal error: hardcoded platform author fingerprint {} does \
             not match SHA-256 over PLATFORM_AUTHOR_PUBKEY ({})",
            PLATFORM_AUTHOR_FP,
            fp
        );
    }
    Ok(vk)
}

/// Create the directory layout `rye init` is responsible for.
///
/// Reserves `<state>/.ai/state/` even though only the daemon writes here in
/// v1 — keeps the layout consistent with the documented contract.
fn create_layout(state_dir: &Path, user_root: &Path) -> Result<()> {
    let dirs = [
        // Node tier (daemon-owned)
        state_dir.join(ryeos_engine::AI_DIR).join("node").join("identity"),
        state_dir.join(ryeos_engine::AI_DIR).join("node").join("auth").join("authorized_keys"),
        state_dir.join(ryeos_engine::AI_DIR).join("node").join("vault"),
        // CAS state
        state_dir.join(ryeos_engine::AI_DIR).join("state").join("objects"),
        state_dir.join(ryeos_engine::AI_DIR).join("state").join("refs"),
        // Bundles dir (populated by bundle.install or rye init)
        state_dir.join(ryeos_engine::AI_DIR).join("bundles"),
        // User tier (operator-edited)
        user_root.join(ryeos_engine::AI_DIR).join("config").join("keys").join("signing"),
        user_root.join(ryeos_engine::AI_DIR).join("config").join("keys").join("trusted"),
    ];
    for d in &dirs {
        fs::create_dir_all(d)
            .with_context(|| format!("create {}", d.display()))?;
    }
    Ok(())
}

/// Load an existing key, or create one. Refuses to overwrite unless `force`.
fn load_or_create_key(path: &Path, force: bool) -> Result<SigningKey> {
    if force && path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("remove old key {}", path.display()))?;
    }
    if path.exists() {
        let pem = fs::read_to_string(path)
            .with_context(|| format!("read existing key {}", path.display()))?;
        let key = SigningKey::from_pkcs8_pem(&pem)
            .with_context(|| format!("parse existing key {}", path.display()))?;
        return Ok(key);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create key parent {}", parent.display()))?;
    }
    let signing_key = SigningKey::generate(&mut OsRng);
    let pem = signing_key
        .to_pkcs8_pem(Default::default())
        .map_err(|e| anyhow!("encode generated key: {e}"))?;
    fs::write(path, pem.as_bytes())
        .with_context(|| format!("write generated key {}", path.display()))?;
    Ok(signing_key)
}

/// If `system_data_dir` already exists, sanity-check that it contains a
/// recognisable `core` bundle so we don't clobber something else and we
/// don't overwrite a previously-laid-down core (idempotency).
fn verify_core_already_consistent(system_data_dir: &Path, core_source: &Path) -> Result<()> {
    let kinds = system_data_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("engine")
        .join("kinds");
    if !kinds.is_dir() {
        bail!(
            "system_data_dir exists at {} but is not a core bundle \
             (no .ai/node/engine/kinds/) — refusing to clobber. \
             Wipe it manually if intentional, or point --system-data-dir \
             elsewhere.",
            system_data_dir.display()
        );
    }
    tracing::info!(
        existing = %system_data_dir.display(),
        source = %core_source.display(),
        "core bundle already laid down — keeping existing"
    );
    Ok(())
}

/// If a bundle directory already exists at the install target, sanity-check
/// it looks like a Rye bundle (`.ai/` exists). Idempotent: don't reinstall.
fn verify_bundle_already_installed(target: &Path, source: &Path) -> Result<()> {
    if !target.join(ryeos_engine::AI_DIR).is_dir() {
        bail!(
            "{} exists but is not a Rye bundle — refusing to clobber",
            target.display()
        );
    }
    tracing::info!(
        existing = %target.display(),
        source = %source.display(),
        "bundle already installed — keeping existing"
    );
    Ok(())
}

/// Install the standard bundle by copy + signed `kind: node` registration.
///
/// Mirrors `service:bundle/install` semantics but runs in-process (no daemon
/// required). The platform author trust must already be pinned (we just
/// did so in [`run_init`]) so preflight verification passes.
fn install_standard_bundle(
    state_dir: &Path,
    standard_source: &Path,
    node_key: &SigningKey,
    system_data_dir: &Path,
    user_root: &Path,
) -> Result<Option<PathBuf>> {
    // Preflight: load trust store from operator state (user + system_data_dir
    // for kind schemas only — trust comes from user-tier).
    let trust_store = TrustStore::load_three_tier(
        None,
        Some(user_root),
        &[system_data_dir.to_path_buf()],
    )
    .context("preflight: load trust store")?;
    if !trust_store.is_trusted(PLATFORM_AUTHOR_FP) {
        bail!(
            "internal error: platform author key {} not in trust store \
             after `rye init` pinned it — trust dir at {}",
            PLATFORM_AUTHOR_FP,
            user_root.join(".ai/config/keys/trusted").display()
        );
    }

    // Verify every signable item in the source bundle against the trust store.
    crate::actions::install::preflight_verify_bundle(
        standard_source,
        system_data_dir,
        Some(user_root),
    )
    .context("preflight verification of standard bundle")?;

    // Copy bundle into <state>/.ai/bundles/standard/
    let target = state_dir.join(ryeos_engine::AI_DIR).join("bundles").join("standard");
    fs::create_dir_all(target.parent().unwrap())
        .with_context(|| format!("create bundles parent for {}", target.display()))?;
    copy_dir_recursive(standard_source, &target)
        .with_context(|| format!("copy standard to {}", target.display()))?;
    let canonical = target
        .canonicalize()
        .context("canonicalize standard install path")?;

    // Write signed kind: node bundle registration record.
    let node_dir = state_dir.join(ryeos_engine::AI_DIR).join("node");
    write_node_bundle_registration(&node_dir, "standard", &canonical, node_key)?;

    Ok(Some(canonical))
}

/// Write a signed `kind: node` `section: bundles` registration record.
///
/// Mirrors what `bundle.install` does in the daemon, but uses the local
/// node signing key rather than the daemon's identity (they're the same
/// key when both paths run on the same node).
fn write_node_bundle_registration(
    node_dir: &Path,
    name: &str,
    path: &Path,
    node_key: &SigningKey,
) -> Result<()> {
    let bundles_dir = node_dir.join("bundles");
    fs::create_dir_all(&bundles_dir)
        .with_context(|| format!("create node bundles dir {}", bundles_dir.display()))?;
    let body = format!(
        "kind: node\nsection: bundles\nid: {name}\npath: {}\n",
        path.display()
    );
    let signed = lillux::signature::sign_content(&body, node_key, "#", None);
    let target = bundles_dir.join(format!("{name}.yaml"));
    let tmp = target.with_extension("tmp");
    fs::write(&tmp, signed.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &target)
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
    Ok(())
}

/// Recursive directory copy with symlink preservation (Unix only).
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    for entry in fs::read_dir(src)
        .with_context(|| format!("read {}", src.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link_target, &to)
                .with_context(|| format!("symlink {}", to.display()))?;
            #[cfg(not(unix))]
            {
                let _ = link_target;
                bail!("symlinks unsupported on this platform: {}", from.display());
            }
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Sanity check helper exposed for tests.
#[doc(hidden)]
pub fn _decode_platform_author_pubkey_for_tests() -> Result<VerifyingKey> {
    decode_platform_author_pubkey()
}

/// Compile-time-ish self-check: encode the platform pubkey for inclusion
/// in error messages or audit logs.
pub fn platform_author_pubkey_b64() -> String {
    base64::engine::general_purpose::STANDARD.encode(PLATFORM_AUTHOR_PUBKEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_author_fingerprint_matches_hardcoded_pubkey() {
        let vk = decode_platform_author_pubkey().expect("decode pubkey");
        assert_eq!(compute_fingerprint(&vk), PLATFORM_AUTHOR_FP);
    }

    #[test]
    fn run_init_creates_keys_and_pins_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let core_src = workspace_root().join("ryeos-bundles/core");
        let standard_src = workspace_root().join("ryeos-bundles/standard");

        let opts = InitOptions {
            state_dir: state.clone(),
            user_root: user.clone(),
            system_data_dir: tmp.path().join("system"),
            core_source: core_src,
            standard_source: Some(standard_src),
            core_only: true, // skip standard for unit-test speed
            force_node_key: false,
        };
        let report = run_init(&opts).expect("init");
        assert_eq!(report.platform_author_pinned, PLATFORM_AUTHOR_FP);
        assert!(report.standard_installed_at.is_none());
        assert!(state.join(".ai/node/identity/private_key.pem").exists());
        assert!(state.join(".ai/node/vault").is_dir());
        assert!(user.join(".ai/config/keys/signing/private_key.pem").exists());
        assert!(user
            .join(".ai/config/keys/trusted")
            .join(format!("{}.toml", PLATFORM_AUTHOR_FP))
            .exists());
    }

    #[test]
    fn run_init_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let core_src = workspace_root().join("ryeos-bundles/core");
        let opts = InitOptions {
            state_dir: state.clone(),
            user_root: user.clone(),
            system_data_dir: tmp.path().join("system"),
            core_source: core_src,
            standard_source: None,
            core_only: true,
            force_node_key: false,
        };
        let r1 = run_init(&opts).expect("init #1");
        let r2 = run_init(&opts).expect("init #2");
        assert_eq!(r1.user_key_fingerprint, r2.user_key_fingerprint);
        assert_eq!(r1.node_key_fingerprint, r2.node_key_fingerprint);
    }

    #[test]
    fn run_init_force_regenerates_node_key_only() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = InitOptions {
            state_dir: tmp.path().join("state"),
            user_root: tmp.path().join("home"),
            system_data_dir: tmp.path().join("system"),
            core_source: workspace_root().join("ryeos-bundles/core"),
            standard_source: None,
            core_only: true,
            force_node_key: false,
        };
        let r1 = run_init(&opts).expect("init #1");
        opts.force_node_key = true;
        let r2 = run_init(&opts).expect("init #2 (force)");
        assert_eq!(r1.user_key_fingerprint, r2.user_key_fingerprint, "user key must persist");
        assert_ne!(r1.node_key_fingerprint, r2.node_key_fingerprint, "node key must rotate");
    }

    fn workspace_root() -> PathBuf {
        // ryeos-tools/Cargo.toml is at workspace_root/ryeos-tools/
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }
}
