//! Operator-side `ryeos init` — bootstraps user space, node space, pins the
//! official publisher key into the operator's trust store, lays down the core
//! bundle, and (unless `--core-only`) installs the standard bundle.
//!
//! Idempotent. Re-running keeps existing keys; only fills in missing pieces.
//! Refuses on inconsistent state — e.g. trust-doc fingerprint doesn't match
//! the key on disk. Refusing means a wipe-and-reinit recovery is required;
//! see `docs/operator-init-recipe.md` (NOT something `ryeos init` does).
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
    "09674c8998e9dd01bfc40ec9f8c4b6b2c1bd01333842582a9c34b3c7db5aa86c";

/// Raw 32-byte Ed25519 public key for the official publisher.
///
/// Encoded inline so `ryeos init` does NOT need to read any bundle file to
/// pin trust. The fingerprint over these bytes MUST equal
/// [`OFFICIAL_PUBLISHER_FP`] — verified at init time.
pub const OFFICIAL_PUBLISHER_PUBKEY: [u8; 32] = [
    0x01, 0x16, 0x95, 0xa5, 0x8f, 0x1d, 0xd6, 0x20,
    0x0a, 0x84, 0xab, 0x8b, 0xb8, 0x36, 0xc4, 0x3d,
    0x92, 0x29, 0x75, 0x19, 0x9b, 0xd7, 0x41, 0xfa,
    0x42, 0x4b, 0xae, 0x5e, 0xa3, 0x69, 0x64, 0x0e,
];

#[derive(Debug)]
pub struct InitOptions {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    /// Contains both runtime state (identity, vault, CAS) and bundle content.
    pub system_space_dir: PathBuf,
    /// User space root (parent of `~/.ai/`). Defaults to `$HOME`.
    pub user_root: PathBuf,
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
    /// Additional PUBLISHER_TRUST.toml files to pin before verifying bundles.
    /// Each file contains `public_key`, `fingerprint`, and `owner` fields.
    pub trust_files: Vec<PathBuf>,
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

/// Run `ryeos init` end-to-end.
///
/// Order:
///   1. Layout: create `<system_space_dir>/.ai/{node,state,bundles}` + vault placeholder
///   2. User key (load-or-create at `<user>/.ai/config/keys/signing/private_key.pem`)
///   3. Node key (load-or-create at `<system_space_dir>/.ai/node/identity/private_key.pem`)
///   4. Self-trust both keys (write signed `<fp>.toml` into user trust dir)
///   5. Pin official publisher key into user trust dir
///   6. Lay down core at `system_space_dir` (copy from `core_source`)
///   7. Install standard bundle (unless `core_only`) — copy + signed
///      registration record
///   8. Verify post-init trust store contains all required keys; refuse if not
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

    // ── 6. Lay down core bundle ──
    if !opts.core_source.is_dir() {
        bail!(
            "core_source is not a directory: {}",
            opts.core_source.display()
        );
    }
    let core_kinds_dir = opts.system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("engine")
        .join("kinds");
    if core_kinds_dir.is_dir() {
        // Core bundle already laid down — verify the on-disk structure
        // is consistent, then re-copy to bring it up to date with the source.
        verify_core_structure(&opts.system_space_dir)?;

        // Verify the source BEFORE re-copying so a tampered core never
        // touches the operator's system_space_dir.
        crate::actions::install::preflight_verify_bundle(
            &opts.core_source,
            &opts.core_source,
            Some(opts.user_root.as_path()),
        )
        .context("verify core source against pinned official publisher key")?;

        // Re-copy over the top of the existing installation. This brings
        // new binaries, kind schemas, handlers, etc. forward while
        // preserving the layout. Derived CAS artifacts (objects/, refs/)
        // are left in place — the daemon will rebuild them if needed.
        copy_dir_recursive(&opts.core_source, &opts.system_space_dir).with_context(|| {
            format!(
                "update core: {} -> {}",
                opts.core_source.display(),
                opts.system_space_dir.display()
            )
        })?;

        // No post-copy verification: the source was already preflighted
        // before the copy (line 184 above), and walking the on-disk
        // tree now would include daemon-runtime artifacts (signed
        // `node/config.yaml`, `node/bundles/*.yaml`, identity files)
        // that the bundle preflight schema doesn't recognize. The
        // source was trusted and copied verbatim — the post-copy state
        // is correct by construction.
    } else {
        // Verify the source BEFORE copying so a tampered core never
        // touches the operator's system_space_dir.
        crate::actions::install::preflight_verify_bundle(
            &opts.core_source,
            &opts.core_source,
            Some(opts.user_root.as_path()),
        )
        .context("verify core source against pinned official publisher key")?;
        copy_dir_recursive(&opts.core_source, &opts.system_space_dir).with_context(
            || {
                format!(
                    "lay down core: {} -> {}",
                    opts.core_source.display(),
                    opts.system_space_dir.display()
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
            .system_space_dir
            .join(ryeos_engine::AI_DIR)
            .join("bundles")
            .join("standard");
        if target.exists() {
            // Standard bundle already installed.
            // 1. Verify the installed target against operator trust.
            //    Any drift here is a hard failure — we will not silently
            //    repair-by-recopy.
            verify_bundle_structure(&target)?;
            crate::actions::install::preflight_verify_bundle(
                &target,
                &opts.system_space_dir,
                Some(opts.user_root.as_path()),
            )
            .context("verify installed standard bundle against operator trust")?;

            // 2. Verify the source so a tampered source never touches disk.
            //    Kind schemas live in core, which is `system_space_dir` —
            //    the standard source has no kinds of its own.
            crate::actions::install::preflight_verify_bundle(
                standard_source,
                &opts.system_space_dir,
                Some(opts.user_root.as_path()),
            )
            .context("verify standard source signatures")?;

            // 3. Re-copy to bring it up to date with the source.
            copy_dir_recursive(standard_source, &target).with_context(|| {
                format!(
                    "update standard: {} -> {}",
                    standard_source.display(),
                    target.display()
                )
            })?;

            // 4. Ensure the signed node bundle registration record exists.
            ensure_node_bundle_registration(
                &opts.system_space_dir,
                "standard",
                &target,
                &node_key,
            )
            .context("verify/recreate node/bundles/standard.yaml")?;

            Some(target.canonicalize()?)
        } else {
            install_standard_bundle(
                &opts.system_space_dir,
                standard_source,
                &node_key,
                &opts.system_space_dir,
                opts.user_root.as_path(),
            )?
        }
    };

    // ── 8. Post-init trust verification ──
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

    let vault_dir = opts.system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("vault");
    fs::create_dir_all(&vault_dir)
        .with_context(|| format!("create vault dir {}", vault_dir.display()))?;
    // Vault X25519 keypair — separate from the Ed25519 node identity
    // so that node-key rotation does NOT brick the vault. Idempotent:
    // load if present, generate otherwise. Public sidecar is regenerated
    // from the secret on every run for resilience against drift.
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
        core_installed_at: opts.system_space_dir.clone(),
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

/// Create the directory layout `ryeos init` is responsible for.
///
/// Reserves `<state>/.ai/state/` even though only the daemon writes here in
/// v1 — keeps the layout consistent with the documented contract.
fn create_layout(system_space_dir: &Path, user_root: &Path) -> Result<()> {
    let dirs = [
        // Node tier (daemon-owned)
        system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("identity"),
        system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("auth").join("authorized_keys"),
        system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("vault"),
        // CAS state
        system_space_dir.join(ryeos_engine::AI_DIR).join("state").join("objects"),
        system_space_dir.join(ryeos_engine::AI_DIR).join("state").join("refs"),
        // Bundles dir (populated by bundle.install or ryeos init)
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

/// If `system_space_dir` already exists, sanity-check that it contains a
/// recognisable `core` bundle so we don't clobber something else and we
/// don't overwrite a previously-laid-down core (idempotency).
/// Verify that an existing core bundle installation has the expected
/// directory structure. Refuses to proceed if the layout is wrong.
fn verify_core_structure(system_space_dir: &Path) -> Result<()> {
    let kinds = system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("engine")
        .join("kinds");
    if !kinds.is_dir() {
        bail!(
            "system_space_dir exists at {} but is not a core bundle \
             (no .ai/node/engine/kinds/) — refusing to clobber. \
             Wipe it manually if intentional, or point --system-space-dir \
             elsewhere.",
            system_space_dir.display()
        );
    }
    Ok(())
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

/// Install the standard bundle by copy + signed `kind: node` registration.
///
/// Mirrors `service:bundle/install` semantics but runs in-process (no daemon
/// required). The official publisher trust must already be pinned (we just
/// did so in [`run_init`]) so preflight verification passes.
fn install_standard_bundle(
    system_space_dir: &Path,
    standard_source: &Path,
    node_key: &SigningKey,
    system_space_dir_for_kinds: &Path,
    user_root: &Path,
) -> Result<Option<PathBuf>> {
    // Preflight: load trust store from operator state (user + system_space_dir
    // for kind schemas only — trust comes from user-tier).
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
        standard_source,
        system_space_dir_for_kinds,
        Some(user_root),
    )
    .context("preflight verification of standard bundle")?;

    // Copy bundle into <system_space_dir>/.ai/bundles/standard/
    let target = system_space_dir.join(ryeos_engine::AI_DIR).join("bundles").join("standard");
    fs::create_dir_all(target.parent().unwrap())
        .with_context(|| format!("create bundles parent for {}", target.display()))?;
    copy_dir_recursive(standard_source, &target)
        .with_context(|| format!("copy standard to {}", target.display()))?;
    let canonical = target
        .canonicalize()
        .context("canonicalize standard install path")?;

    // Write signed kind: node bundle registration record.
    let node_dir = system_space_dir.join(ryeos_engine::AI_DIR).join("node");
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

/// Ensure a node bundle registration record exists and is valid.
///
/// - If missing → write + sign it (idempotent repair).
/// - If present and signature-valid with correct path → no-op.
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

    // Verify the node key's signature on this file.
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

    if sig_header.signer_fingerprint != node_fp {
        bail!(
            "node bundle registration {} signed by {} but node key is {} — \
             wipe and re-init to repair",
            reg_path.display(),
            sig_header.signer_fingerprint,
            node_fp
        );
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

    #[test]
    fn run_init_creates_keys_and_pins_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let core_src = workspace_root().join("ryeos-bundles/core");
        let standard_src = workspace_root().join("ryeos-bundles/standard");

        let opts = InitOptions {
            system_space_dir: state.clone(),
            user_root: user.clone(),
            core_source: core_src,
            standard_source: Some(standard_src),
            core_only: true, // skip standard for unit-test speed
            force_node_key: false,
            trust_files: vec![dev_trust_file()],
        };
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
        let opts = InitOptions {
            system_space_dir: state.clone(),
            user_root: user,
            core_source: workspace_root().join("ryeos-bundles/core"),
            standard_source: None,
            core_only: true,
            force_node_key: false,
        trust_files: vec![dev_trust_file()],
        };
        let report = run_init(&opts).expect("init");
        let vault_priv = state.join(".ai/node/vault/private_key.pem");
        let vault_pub = state.join(".ai/node/vault/public_key.pem");
        assert!(vault_priv.exists(), "vault private key must exist");
        assert!(vault_pub.exists(), "vault public key must exist");
        // Fingerprint surfaced in report matches the one derivable from the
        // on-disk secret key.
        let sk = lillux::vault::read_secret_key(&vault_priv).unwrap();
        assert_eq!(report.vault_pubkey_fingerprint, sk.public_key().fingerprint());
        assert_eq!(report.vault_pubkey_fingerprint.len(), 64);
        // Sealed envelope round-trip with the on-disk key works.
        let env = lillux::vault::seal(&sk.public_key(), b"hello").unwrap();
        let out = lillux::vault::open(&sk, &env).unwrap();
        assert_eq!(out, b"hello");
    }

    #[test]
    fn run_init_vault_key_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let opts = InitOptions {
            system_space_dir: state.clone(),
            user_root: tmp.path().join("home"),
            core_source: workspace_root().join("ryeos-bundles/core"),
            standard_source: None,
            core_only: true,
            force_node_key: false,
        trust_files: vec![dev_trust_file()],
        };
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
        let core_src = workspace_root().join("ryeos-bundles/core");
        let opts = InitOptions {
            system_space_dir: state.clone(),
            user_root: user.clone(),
            core_source: core_src,
            standard_source: None,
            core_only: true,
            force_node_key: false,
        trust_files: vec![dev_trust_file()],
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
            system_space_dir: tmp.path().join("state"),
            user_root: tmp.path().join("home"),
            core_source: workspace_root().join("ryeos-bundles/core"),
            standard_source: None,
            core_only: true,
            force_node_key: false,
        trust_files: vec![dev_trust_file()],
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
