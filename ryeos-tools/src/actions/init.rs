//! Operator-side `ryeos init` (Model B) — bootstraps user space, node space,
//! pins the official publisher key into the operator's trust store, installs
//! both core and standard bundles under `<system_space_dir>/.ai/bundles/`,
//! and writes signed registration records at
//! `<system_space_dir>/.ai/node/bundles/{core,standard}.yaml`.
//!
//! Idempotent. Re-running keeps existing keys; only fills in missing pieces.
//! Refuses on inconsistent state — e.g. trust-doc fingerprint doesn't match
//! the key on disk. Refusing means a wipe-and-reinit recovery is required.
//!
//! `ryeos init` does NOT auto-import trust docs from any bundle. The official
//! publisher key is hardcoded in this source ([`OFFICIAL_PUBLISHER_PUBKEY`]) and
//! pinned explicitly. Third-party bundle authors are pinned via
//! `ryeos trust pin <fingerprint>` or `--trust-file <PUBLISHER_TRUST.toml>`.

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

/// SHA-256 fingerprint of the official publisher Ed25519 public key.
///
/// This is the long-lived release key under which all official `core` and
/// `standard` bundles are signed in the public registry. Hardcoded here
/// so `ryeos init` can pin it without trusting any on-disk source. Rotation
/// is rare and requires a coordinated release of a new `ryeos` binary.
///
/// For local development, bundles are signed with the dev publisher key
/// (`.dev-keys/PUBLISHER_DEV.pem`), and `--trust-file` is used to pin it.
pub const OFFICIAL_PUBLISHER_FP: &str =
    "c9d7301fba468b669d91a6000e9b6a4158c0e615dea4fe1f99906b8c9214bc28";

/// Raw 32-byte Ed25519 public key for the official publisher.
///
/// Encoded inline so `ryeos init` does NOT need to read any bundle file to
/// pin trust. The fingerprint over these bytes MUST equal
/// [`OFFICIAL_PUBLISHER_FP`] — verified at init time.
pub const OFFICIAL_PUBLISHER_PUBKEY: [u8; 32] = [
    0xe7, 0x68, 0x9b, 0x49, 0x7f, 0xd5, 0x92, 0x57,
    0x10, 0x2b, 0x97, 0x86, 0x68, 0x2d, 0x74, 0x10,
    0xb4, 0x35, 0xf2, 0x1b, 0x16, 0x81, 0x44, 0x2d,
    0x3b, 0xfb, 0x4a, 0xcd, 0xe6, 0x25, 0x36, 0x03,
];

#[derive(Debug)]
pub struct InitOptions {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    /// Contains mutable node state and installed bundle content.
    pub system_space_dir: PathBuf,
    /// User space root (parent of `~/.ai/`). Defaults to `$HOME`.
    pub user_root: PathBuf,
    /// Source tree to copy `core` from. Required — the operator points this
    /// at the packaged core (e.g. `/usr/share/ryeos/core`) or at the dev
    /// tree `ryeos-bundles/core`.
    pub core_source: PathBuf,
    /// Source tree to copy `standard` from. Required unless `core_only`.
    pub standard_source: Option<PathBuf>,
    /// Skip installing standard. Positive framing — opt-in to bare core.
    pub core_only: bool,
    /// Force-regenerate the node signing key. Does NOT touch the user key.
    pub force_node_key: bool,
    /// Additional PUBLISHER_TRUST.tomL files to pin before verifying bundles.
    /// Each file contains `public_key`, `fingerprint`, and `owner` fields.
    pub trust_files: Vec<PathBuf>,
    /// Skip preflight verification of source bundles (trust + signatures).
    /// Used in dev/test when source bundles are not yet signed and populated.
    /// DO NOT expose this as a CLI flag — production installs always verify.
    pub skip_preflight: bool,
}

#[derive(Debug, Serialize)]
pub struct InitReport {
    pub user_key_fingerprint: String,
    pub node_key_fingerprint: String,
    pub official_publisher_pinned: String,
    pub core_installed_at: PathBuf,
    pub standard_installed_at: Option<PathBuf>,
    pub vault_dir: PathBuf,
    /// SHA-256 fingerprint of the X25519 vault public key. Surfaced
    /// so operators can sanity-check that subsequent vault writes are
    /// being sealed to the right key (and so audit logs can pin it).
    pub vault_pubkey_fingerprint: String,
    pub trust_dir: PathBuf,
    pub next_steps: Vec<String>,
}

/// Run `ryeos init` end-to-end (Model B).
///
/// Order:
///   1. Layout: create `<system_space_dir>/.ai/{node,state,bundles}` + user space
///   2. User key (load-or-create at `<user>/.ai/config/keys/signing/private_key.pem`)
///   3. Node key (load-or-create at `<system_space_dir>/.ai/node/identity/private_key.pem`)
///   4. Self-trust both keys (write signed `<fp>.toml` into user trust dir)
///   5. Pin official publisher key into user trust dir + additional trust files
///   6. Install core bundle at `<system_space_dir>/.ai/bundles/core/` + registration
///   7. Install standard bundle at `<system_space_dir>/.ai/bundles/standard/` + registration
///   8. Vault X25519 keypair
///   9. Post-init trust verification
pub fn run_init(opts: &InitOptions) -> Result<InitReport> {
    // ── 1. Layout ──
    create_layout(&opts.system_space_dir, &opts.user_root)?;

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
        .system_space_dir
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

    // ── 5. Pin official publisher key ──
    let official_publisher_vk = decode_official_publisher_pubkey()?;
    let pinned_fp = pin_key(&official_publisher_vk, "official-publisher", &trust_dir, None)
        .map_err(|e| anyhow!("pin official publisher trust doc: {e}"))?;
    if pinned_fp != OFFICIAL_PUBLISHER_FP {
        bail!(
            "official publisher fingerprint mismatch: hardcoded {} but \
             public key bytes hash to {}",
            OFFICIAL_PUBLISHER_FP,
            pinned_fp
        );
    }

    // ── 5b. Pin additional trust files (--trust-file) ──
    for trust_file in &opts.trust_files {
        pin_trust_file(trust_file, &trust_dir)
            .with_context(|| format!("pin trust file {}", trust_file.display()))?;
    }

    // ── 6. Install core bundle ──
    // Model B: core installs to <system_space_dir>/.ai/bundles/core/
    // (NOT directly to system_space_dir root).
    if !opts.core_source.is_dir() {
        bail!(
            "core_source is not a directory: {}",
            opts.core_source.display()
        );
    }
    let core_target = opts
        .system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("bundles")
        .join("core");

    if core_target.exists() {
        // Core bundle already installed — verify structure and update.
        verify_bundle_structure(&core_target)?;

        // Verify the source before re-copying.
        if !opts.skip_preflight {
            crate::actions::install::preflight_verify_bundle(
                &opts.core_source,
                &opts.core_source,
                Some(opts.user_root.as_path()),
            )
            .context("verify core source against pinned publisher key")?;
        }

        // Re-copy to bring it up to date.
        copy_dir_recursive(&opts.core_source, &core_target).with_context(|| {
            format!(
                "update core: {} -> {}",
                opts.core_source.display(),
                core_target.display()
            )
        })?;

        // Ensure registration exists and is valid.
        let core_canonical = core_target.canonicalize()?;
        ensure_node_bundle_registration(
            &opts.system_space_dir,
            "core",
            &core_canonical,
            &node_key,
        )
        .context("verify/recreate node/bundles/core.yaml")?;
    } else {
        // Fresh install.
        if !opts.skip_preflight {
            crate::actions::install::preflight_verify_bundle(
                &opts.core_source,
                &opts.core_source,
                Some(opts.user_root.as_path()),
            )
            .context("verify core source against pinned publisher key")?;
        }

        fs::create_dir_all(core_target.parent().unwrap())
            .with_context(|| format!("create bundles parent for {}", core_target.display()))?;
        copy_dir_recursive(&opts.core_source, &core_target).with_context(|| {
            format!(
                "install core: {} -> {}",
                opts.core_source.display(),
                core_target.display()
            )
        })?;

        let core_canonical = core_target.canonicalize()?;
        let node_dir = opts.system_space_dir.join(ryeos_engine::AI_DIR).join("node");
        write_node_bundle_registration(&node_dir, "core", &core_canonical, &node_key)?;
    }

    let core_installed_at = core_target.canonicalize()?;

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
            .system_space_dir
            .join(ryeos_engine::AI_DIR)
            .join("bundles")
            .join("standard");

        if target.exists() {
            // Standard bundle already installed.
            verify_bundle_structure(&target)?;

            if !opts.skip_preflight {
                crate::actions::install::preflight_verify_bundle(
                    &target,
                    &opts.system_space_dir,
                    Some(opts.user_root.as_path()),
                )
                .context("verify installed standard bundle against operator trust")?;

                crate::actions::install::preflight_verify_bundle(
                    standard_source,
                    &opts.system_space_dir,
                    Some(opts.user_root.as_path()),
                )
                .context("verify standard source signatures")?;
            }

            copy_dir_recursive(standard_source, &target).with_context(|| {
                format!(
                    "update standard: {} -> {}",
                    standard_source.display(),
                    target.display()
                )
            })?;

            ensure_node_bundle_registration(
                &opts.system_space_dir,
                "standard",
                &target.canonicalize()?,
                &node_key,
            )
            .context("verify/recreate node/bundles/standard.yaml")?;

            Some(target.canonicalize()?)
        } else {
            install_bundle(
                &opts.system_space_dir,
                "standard",
                standard_source,
                &node_key,
                &opts.system_space_dir,
                opts.user_root.as_path(),
                opts.skip_preflight,
            )?
        }
    };

    // ── 8. Vault X25519 keypair ──
    let vault_dir = opts.system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("vault");
    fs::create_dir_all(&vault_dir)
        .with_context(|| format!("create vault dir {}", vault_dir.display()))?;
    // Separate from Ed25519 node identity so node-key rotation does NOT
    // brick the vault. Idempotent: load if present, generate otherwise.
    let vault_secret_path = vault_dir.join("private_key.pem");
    let vault_public_path = vault_dir.join("public_key.pem");
    let vault_sk = if vault_secret_path.exists() {
        lillux::vault::read_secret_key(&vault_secret_path)
            .with_context(|| format!("load vault key {}", vault_secret_path.display()))?
    } else {
        let sk = lillux::vault::VaultSecretKey::generate();
        lillux::vault::write_secret_key(&vault_secret_path, &sk)
            .with_context(|| format!("write vault key {}", vault_secret_path.display()))?;
        sk
    };
    lillux::vault::write_public_key(&vault_public_path, &vault_sk.public_key())
        .with_context(|| format!("write vault pubkey {}", vault_public_path.display()))?;

    // ── 9. Post-init trust verification ──
    let post_trust = TrustStore::load_three_tier(
        None,
        Some(opts.user_root.as_path()),
        std::slice::from_ref(&opts.system_space_dir),
    )
    .context("load post-init trust store")?;
    if !post_trust.is_trusted(OFFICIAL_PUBLISHER_FP) {
        bail!(
            "post-init self-check failed: official publisher key {} is \
             not in the loaded trust store — trust dir at {}",
            OFFICIAL_PUBLISHER_FP,
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

    let mut next_steps = vec![
        format!(
            "Start the daemon: ryeosd --system-space-dir {}",
            opts.system_space_dir.display()
        ),
        "Try a verb: ryeos status".to_string(),
    ];
    if opts.core_only {
        next_steps.push(
            "Install the standard bundle later: ryeos bundle install --name standard \
             --source-path <path>"
                .to_string(),
        );
    }

    Ok(InitReport {
        user_key_fingerprint: user_fp,
        node_key_fingerprint: node_fp,
        official_publisher_pinned: OFFICIAL_PUBLISHER_FP.to_string(),
        core_installed_at,
        standard_installed_at,
        vault_dir,
        vault_pubkey_fingerprint: vault_sk.public_key().fingerprint(),
        trust_dir,
        next_steps,
    })
}

/// Decode the hardcoded official publisher public key into a `VerifyingKey`,
/// guaranteeing the fingerprint matches [`OFFICIAL_PUBLISHER_FP`].
fn decode_official_publisher_pubkey() -> Result<VerifyingKey> {
    let vk = VerifyingKey::from_bytes(&OFFICIAL_PUBLISHER_PUBKEY)
        .map_err(|e| anyhow!("hardcoded official publisher key invalid: {e}"))?;
    let fp = compute_fingerprint(&vk);
    if fp != OFFICIAL_PUBLISHER_FP {
        bail!(
            "internal error: hardcoded official publisher fingerprint {} does \
             not match SHA-256 over OFFICIAL_PUBLISHER_PUBKEY ({})",
            OFFICIAL_PUBLISHER_FP,
            fp
        );
    }
    Ok(vk)
}

/// Parse a `PUBLISHER_TRUST.toml` and pin its key into the trust store.
fn pin_trust_file(trust_file: &Path, trust_dir: &Path) -> Result<()> {
    let content = fs::read_to_string(trust_file)
        .with_context(|| format!("read trust file {}", trust_file.display()))?;

    let doc = ryeos_engine::trust::PublisherTrustDoc::parse(&content)
        .map_err(|e| anyhow!("{e}"))?;

    let vk = doc.decode_verifying_key()
        .map_err(|e| anyhow!("{e}"))?;

    pin_key(&vk, &doc.owner, trust_dir, None)
        .map_err(|e| anyhow!("pin trust doc for {}: {e}", doc.owner))?;

    Ok(())
}

/// Create the Model B directory layout.
///
/// System space contains:
/// - `node/` — mutable daemon state (identity, vault, config, bundle registrations)
/// - `state/` — CAS and runtime state
/// - `bundles/` — installed bundle content (populated by bundle installs)
fn create_layout(system_space_dir: &Path, user_root: &Path) -> Result<()> {
    let dirs = [
        // Node tier (daemon-owned)
        system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("identity"),
        system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("auth").join("authorized_keys"),
        system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("vault"),
        system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("config"),
        system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("bundles"),
        system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("engine").join("kinds"),
        // CAS state
        system_space_dir.join(ryeos_engine::AI_DIR).join("state").join("objects"),
        system_space_dir.join(ryeos_engine::AI_DIR).join("state").join("refs"),
        // Installed bundles directory
        system_space_dir.join(ryeos_engine::AI_DIR).join("bundles"),
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

/// Verify that an existing bundle directory has the expected `.ai/` structure.
fn verify_bundle_structure(target: &Path) -> Result<()> {
    if !target.join(ryeos_engine::AI_DIR).is_dir() {
        bail!(
            "{} exists but is not a Rye bundle — refusing to clobber",
            target.display()
        );
    }
    Ok(())
}

/// Install a bundle by copy + signed `kind: node` registration.
///
/// Mirrors `service:bundle/install` semantics but runs in-process (no daemon
/// required). The official publisher trust must already be pinned so
/// preflight verification passes.
fn install_bundle(
    system_space_dir: &Path,
    name: &str,
    source: &Path,
    node_key: &SigningKey,
    system_space_dir_for_kinds: &Path,
    user_root: &Path,
    skip_preflight: bool,
) -> Result<Option<PathBuf>> {
    if !skip_preflight {
        // Preflight: load trust store from operator state.
        let trust_store = TrustStore::load_three_tier(
            None,
            Some(user_root),
            &[system_space_dir_for_kinds.to_path_buf()],
        )
        .context("preflight: load trust store")?;
        if !trust_store.is_trusted(OFFICIAL_PUBLISHER_FP) {
            bail!(
                "internal error: official publisher key {} not in trust store \
                 after `ryeos init` pinned it — trust dir at {}",
                OFFICIAL_PUBLISHER_FP,
                user_root.join(".ai/config/keys/trusted").display()
            );
        }

        // Verify every signable item in the source bundle against the trust store.
        crate::actions::install::preflight_verify_bundle(
            source,
            system_space_dir_for_kinds,
            Some(user_root),
        )
        .with_context(|| format!("preflight verification of {} bundle", name))?;
    }

    // Copy bundle into <system_space_dir>/.ai/bundles/<name>/
    let target = system_space_dir.join(ryeos_engine::AI_DIR).join("bundles").join(name);
    fs::create_dir_all(target.parent().unwrap())
        .with_context(|| format!("create bundles parent for {}", target.display()))?;
    copy_dir_recursive(source, &target)
        .with_context(|| format!("copy {} to {}", name, target.display()))?;
    let canonical = target
        .canonicalize()
        .with_context(|| format!("canonicalize {} install path", name))?;

    // Write signed kind: node bundle registration record.
    let node_dir = system_space_dir.join(ryeos_engine::AI_DIR).join("node");
    write_node_bundle_registration(&node_dir, name, &canonical, node_key)?;

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

/// Ensure a node bundle registration record exists and is valid.
///
/// - If missing → write + sign it (idempotent repair).
/// - If present and signature-valid with correct path → no-op.
/// - If present but signed by a different key (e.g. after node-key rotation)
///   → re-write with current key.
/// - If present but invalid (broken signature, mismatched path) → hard fail.
fn ensure_node_bundle_registration(
    system_space_dir: &Path,
    name: &str,
    bundle_path: &Path,
    node_key: &SigningKey,
) -> Result<()> {
    let reg_path = system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("bundles")
        .join(format!("{name}.yaml"));

    if !reg_path.exists() {
        // Missing — write a fresh registration.
        let node_dir = system_space_dir.join(ryeos_engine::AI_DIR).join("node");
        write_node_bundle_registration(&node_dir, name, bundle_path, node_key)?;
        return Ok(());
    }

    // Existing record — verify signature and content.
    let content = fs::read_to_string(&reg_path)
        .with_context(|| format!("read {}", reg_path.display()))?;

    let node_vk = node_key.verifying_key();
    let node_fp = compute_fingerprint(&node_vk);

    let sig_header = lillux::signature::parse_signature_line(
        content.lines().next().unwrap_or(""),
        "#",
        None,
    )
    .ok_or_else(|| anyhow!(
        "node bundle registration {} has no valid signature line",
        reg_path.display()
    ))?;

    let body = lillux::signature::strip_signature_lines(&content);
    let actual_hash = lillux::signature::content_hash(&body);
    if actual_hash != sig_header.content_hash {
        bail!(
            "node bundle registration {} has corrupted content (hash mismatch)",
            reg_path.display()
        );
    }

    // If signed by a different key (e.g. after node-key rotation),
    // re-write with the current key.
    if sig_header.signer_fingerprint != node_fp {
        tracing::info!(
            name,
            old_signer = %sig_header.signer_fingerprint,
            new_signer = %node_fp,
            "re-signing bundle registration after node-key change"
        );
        let node_dir = system_space_dir.join(ryeos_engine::AI_DIR).join("node");
        write_node_bundle_registration(&node_dir, name, bundle_path, node_key)?;
        return Ok(());
    }

    if !lillux::signature::verify_signature(
        &sig_header.content_hash,
        &sig_header.signature_b64,
        &node_vk,
    ) {
        bail!(
            "node bundle registration {} has invalid Ed25519 signature",
            reg_path.display()
        );
    }

    // Signature valid — check the path field matches.
    if !body.contains(&format!("path: {}", bundle_path.display())) {
        bail!(
            "node bundle registration {} references wrong path — \
             expected {} but record contains a different path. \
             Wipe and re-init to repair",
            reg_path.display(),
            bundle_path.display()
        );
    }

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
pub fn _decode_official_publisher_pubkey_for_tests() -> Result<VerifyingKey> {
    decode_official_publisher_pubkey()
}

/// Compile-time-ish self-check: encode the platform pubkey for inclusion
/// in error messages or audit logs.
pub fn official_publisher_pubkey_b64() -> String {
    base64::engine::general_purpose::STANDARD.encode(OFFICIAL_PUBLISHER_PUBKEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_publisher_fingerprint_matches_hardcoded_pubkey() {
        let vk = decode_official_publisher_pubkey().expect("decode pubkey");
        assert_eq!(compute_fingerprint(&vk), OFFICIAL_PUBLISHER_FP);
    }

    fn dev_trust_file() -> PathBuf {
        workspace_root().join(".dev-keys/PUBLISHER_DEV_TRUST.toml")
    }

    fn make_opts_core_only(state: &Path, user: &Path) -> InitOptions {
        InitOptions {
            system_space_dir: state.to_path_buf(),
            user_root: user.to_path_buf(),
            core_source: workspace_root().join("ryeos-bundles/core"),
            standard_source: None,
            core_only: true,
            force_node_key: false,
            trust_files: vec![dev_trust_file()],
            skip_preflight: true,
        }
    }

    fn make_opts_force(state: &Path, user: &Path) -> InitOptions {
        InitOptions {
            system_space_dir: state.to_path_buf(),
            user_root: user.to_path_buf(),
            core_source: workspace_root().join("ryeos-bundles/core"),
            standard_source: None,
            core_only: true,
            force_node_key: true,
            trust_files: vec![dev_trust_file()],
            skip_preflight: true,
        }
    }

    #[test]
    fn run_installs_core_to_bundles_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let opts = make_opts_core_only(&state, &user);
        let report = run_init(&opts).expect("init");
        assert_eq!(report.official_publisher_pinned, OFFICIAL_PUBLISHER_FP);

        // Core should be at .ai/bundles/core/.ai/ (Model B)
        assert!(
            state.join(".ai/bundles/core/.ai").is_dir(),
            "core should be installed at .ai/bundles/core/.ai/"
        );
        // Core registration should exist
        assert!(
            state.join(".ai/node/bundles/core.yaml").exists(),
            "core bundle registration must exist"
        );
        // Kind schemas should be inside the core bundle
        assert!(
            state.join(".ai/bundles/core/.ai/node/engine/kinds").is_dir(),
            "core kind schemas must be inside the installed bundle"
        );
        // No standard installed
        assert!(report.standard_installed_at.is_none());
        assert!(state.join(".ai/node/identity/private_key.pem").exists());
        assert!(state.join(".ai/node/vault").is_dir());
        assert!(user.join(".ai/config/keys/signing/private_key.pem").exists());
    }

    #[test]
    fn run_init_creates_keys_and_pins_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let opts = make_opts_core_only(&state, &user);
        let report = run_init(&opts).expect("init");
        assert_eq!(report.official_publisher_pinned, OFFICIAL_PUBLISHER_FP);
        assert!(report.standard_installed_at.is_none());
        assert!(state.join(".ai/node/identity/private_key.pem").exists());
        assert!(state.join(".ai/node/vault").is_dir());
        assert!(user.join(".ai/config/keys/signing/private_key.pem").exists());
        assert!(user
            .join(".ai/config/keys/trusted")
            .join(format!("{}.toml", OFFICIAL_PUBLISHER_FP))
            .exists());
    }

    #[test]
    fn run_init_generates_vault_keypair() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let opts = make_opts_core_only(&state, &user);
        let report = run_init(&opts).expect("init");
        let vault_priv = state.join(".ai/node/vault/private_key.pem");
        let vault_pub = state.join(".ai/node/vault/public_key.pem");
        assert!(vault_priv.exists(), "vault private key must exist");
        assert!(vault_pub.exists(), "vault public key must exist");
        let sk = lillux::vault::read_secret_key(&vault_priv).unwrap();
        assert_eq!(report.vault_pubkey_fingerprint, sk.public_key().fingerprint());
        assert_eq!(report.vault_pubkey_fingerprint.len(), 64);
        let env = lillux::vault::seal(&sk.public_key(), b"hello").unwrap();
        let out = lillux::vault::open(&sk, &env).unwrap();
        assert_eq!(out, b"hello");
    }

    #[test]
    fn run_init_vault_key_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let opts = make_opts_core_only(&state, &user);
        let r1 = run_init(&opts).expect("init #1");
        let r2 = run_init(&opts).expect("init #2");
        assert_eq!(
            r1.vault_pubkey_fingerprint, r2.vault_pubkey_fingerprint,
            "vault key must persist across reinits"
        );
    }

    #[test]
    fn run_init_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let opts = make_opts_core_only(&state, &user);
        let r1 = run_init(&opts).expect("init #1");
        let r2 = run_init(&opts).expect("init #2");
        assert_eq!(r1.user_key_fingerprint, r2.user_key_fingerprint);
        assert_eq!(r1.node_key_fingerprint, r2.node_key_fingerprint);
    }

    #[test]
    fn run_init_force_regenerates_node_key_only() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let r1 = run_init(&make_opts_core_only(&state, &user)).expect("init #1");
        let r2 = run_init(&make_opts_force(&state, &user)).expect("init #2 (force)");
        assert_eq!(r1.user_key_fingerprint, r2.user_key_fingerprint, "user key must persist");
        assert_ne!(r1.node_key_fingerprint, r2.node_key_fingerprint, "node key must rotate");
    }

    #[test]
    fn run_init_with_both_bundles() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");

        let opts = InitOptions {
            system_space_dir: state.clone(),
            user_root: user.clone(),
            core_source: workspace_root().join("ryeos-bundles/core"),
            standard_source: Some(workspace_root().join("ryeos-bundles/standard")),
            core_only: false,
            force_node_key: false,
            trust_files: vec![dev_trust_file()],
            skip_preflight: true,
        };
        let report = run_init(&opts).expect("init");

        // Both bundles installed
        assert!(state.join(".ai/bundles/core/.ai").is_dir(), "core must be installed");
        assert!(state.join(".ai/bundles/standard/.ai").is_dir(), "standard must be installed");

        // Both registrations exist
        assert!(state.join(".ai/node/bundles/core.yaml").exists(), "core registration");
        assert!(state.join(".ai/node/bundles/standard.yaml").exists(), "standard registration");

        // Report paths
        assert!(report.core_installed_at.exists());
        assert!(report.standard_installed_at.is_some());
        assert!(report.standard_installed_at.unwrap().exists());
    }

    fn workspace_root() -> PathBuf {
        // ryeos-tools/Cargo.toml is at workspace_root/ryeos-tools/
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }
}
