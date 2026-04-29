//! Fast, deterministic fixture for ryeosd integration tests.
//!
//! Drop-in replacement for the daemon's real `--init-if-missing` path.
//! Pre-populates `state_dir` and `user_space` with byte-equivalent
//! state to what `ryeosd::bootstrap::init` would produce, but using
//! deterministic keys so tests are reproducible and faster.
//!
//! ## Why
//!
//! Most e2e tests don't care about init or bundle install — they only
//! need a daemon that can boot, verify trust, and dispatch items. The
//! fast fixture writes the keys, trust docs, and vault keypair directly
//! and skips real `rye init`. The slow `start_with_pre_init` path stays
//! around for the 1-3 smoke tests that exercise real init/install.
//!
//! ## Three-key role split
//!
//! Three deterministic Ed25519 keys, mirroring the publisher / user /
//! node split documented in `docs/future/key-rotation-and-trust-policy.md`:
//!
//!   * [`publisher_signing_key`] — signs test bundle / directive /
//!     route content. Most tests use this one.
//!   * [`node_signing_key`]      — daemon's persistent identity.
//!   * [`user_signing_key`]      — operator's persistent identity.
//!
//! Plus an X25519 vault keypair distinct from the Ed25519 keys (so node
//! key rotation doesn't brick the vault) — see [`vault_secret_key`].
//!
//! Returned by [`populate_initialized_state`] inside a [`FastFixture`].

#![allow(dead_code)] // helpers are only used by some integration test bins

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine as _;
use lillux::crypto::{EncodePrivateKey, SigningKey};

use ryeos_engine::AI_DIR;

/// Deterministic publisher signing key. Used for signing test bundle
/// content (directives, routes, providers, …). Bytes pattern: `[42; 32]`.
pub fn publisher_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

/// Deterministic node signing key — separate domain from publisher per
/// the locked three-key role split. Bytes pattern: `[43; 32]`.
pub fn node_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[43u8; 32])
}

/// Deterministic user signing key. Bytes pattern: `[44; 32]`.
pub fn user_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[44u8; 32])
}

/// Deterministic X25519 vault secret. Distinct from the Ed25519 keys
/// above so node-key rotation doesn't brick the vault. Bytes
/// pattern: `[45; 32]`.
pub fn vault_secret_key() -> lillux::vault::VaultSecretKey {
    lillux::vault::VaultSecretKey::from_bytes([45u8; 32])
}

/// Bundle of deterministic test keys returned by
/// [`populate_initialized_state`]. Tests use `fixture.publisher` to
/// sign their own bundle/directive content.
pub struct FastFixture {
    pub publisher: SigningKey,
    pub node: SigningKey,
    pub user: SigningKey,
    pub vault: lillux::vault::VaultSecretKey,
}

impl FastFixture {
    pub fn publisher_fp(&self) -> String {
        lillux::signature::compute_fingerprint(&self.publisher.verifying_key())
    }
    pub fn node_fp(&self) -> String {
        lillux::signature::compute_fingerprint(&self.node.verifying_key())
    }
    pub fn user_fp(&self) -> String {
        lillux::signature::compute_fingerprint(&self.user.verifying_key())
    }
}

/// Pre-populate `state_path` + `user_space` with everything
/// `bootstrap::init` would produce, using deterministic keys.
///
/// After this returns the daemon can boot WITHOUT `--init-if-missing`:
/// node identity, vault keypair, layout dirs, user signing key, and
/// self-signed trust docs are all in place.
///
/// What it writes:
///
/// ```text
/// <state>/.ai/node/identity/private_key.pem            (deterministic node Ed25519)
/// <state>/.ai/node/vault/private_key.pem               (deterministic vault X25519)
/// <state>/.ai/node/vault/public_key.pem
/// <state>/.ai/node/auth/authorized_keys/               (empty dir)
/// <state>/.ai/state/objects/                           (empty dir)
/// <state>/.ai/state/refs/                              (empty dir)
/// <user>/.ai/config/keys/signing/private_key.pem       (deterministic user Ed25519)
/// <user>/.ai/config/keys/trusted/<publisher_fp>.toml   (self-signed trust doc)
/// <user>/.ai/config/keys/trusted/<node_fp>.toml        (self-signed trust doc)
/// <user>/.ai/config/keys/trusted/<user_fp>.toml        (self-signed trust doc)
/// ```
///
/// Does NOT install bundles. Tests that need the standard bundle
/// installed call `register_standard_bundle()` separately.
pub fn populate_initialized_state(state_path: &Path, user_space: &Path) -> Result<FastFixture> {
    let publisher = publisher_signing_key();
    let node = node_signing_key();
    let user = user_signing_key();
    let vault = vault_secret_key();

    // ── Layout dirs (mirrors bootstrap::create_directory_layout) ──
    for d in [
        state_path.join(AI_DIR).join("node").join("auth").join("authorized_keys"),
        state_path.join(AI_DIR).join("node").join("vault"),
        state_path.join(AI_DIR).join("node").join("identity"),
        state_path.join(AI_DIR).join("state").join("objects"),
        state_path.join(AI_DIR).join("state").join("refs"),
        user_space.join(AI_DIR).join("config").join("keys").join("signing"),
        user_space.join(AI_DIR).join("config").join("keys").join("trusted"),
    ] {
        fs::create_dir_all(&d)
            .with_context(|| format!("create {}", d.display()))?;
    }

    // ── Node Ed25519 identity ──
    let node_identity_dir = state_path.join(AI_DIR).join("node").join("identity");
    write_pem_signing_key(&node_identity_dir.join("private_key.pem"), &node)
        .context("write node signing key")?;

    // Public identity doc — read by the `tool:rye/core/identity/public_key`
    // tool. Daemon startup itself doesn't need this file (it loads the
    // private key directly), but tests that exercise the public_key tool
    // would otherwise see a null result. Mirrors what
    // `bootstrap::init` writes after generating the node key.
    let public_identity_path = node_identity_dir.join("public-identity.json");
    ryeosd::identity::NodeIdentity::load(&node_identity_dir.join("private_key.pem"))
        .context("re-load node identity to write public doc")?
        .write_public_identity(&public_identity_path)
        .context("write node public identity")?;

    // ── User Ed25519 identity ──
    write_pem_signing_key(
        &user_space
            .join(AI_DIR)
            .join("config")
            .join("keys")
            .join("signing")
            .join("private_key.pem"),
        &user,
    )
    .context("write user signing key")?;

    // ── Vault X25519 keypair ──
    let vault_dir = state_path.join(AI_DIR).join("node").join("vault");
    lillux::vault::write_secret_key(&vault_dir.join("private_key.pem"), &vault)
        .context("write vault secret key")?;
    lillux::vault::write_public_key(&vault_dir.join("public_key.pem"), &vault.public_key())
        .context("write vault public key")?;

    // ── Self-signed trust docs (publisher + node + user) ──
    let trust_dir = user_space.join(AI_DIR).join("config").join("keys").join("trusted");
    for sk in [&publisher, &node, &user] {
        write_self_signed_trust_doc(&trust_dir, sk)
            .context("write self-signed trust doc")?;
    }

    // ── Trust the system-bundle signer (signs `ryeos-bundles/core` items) ──
    //
    // Without this the daemon refuses to load the kind schemas inside
    // the core bundle and bootstrap aborts with `untrusted signer` for
    // every `node:engine/kinds/...` item. Mirrors what the existing
    // `populate_user_space()` helper does on the slow path.
    super::populate_user_space(user_space);

    Ok(FastFixture { publisher, node, user, vault })
}

/// Write a `kind: node, section: bundles` record pointing at
/// `ryeos-bundles/standard`, signed with the publisher key. Use this
/// when a test needs the standard bundle's runtime/directive YAMLs in
/// the daemon's effective bundle roots.
pub fn register_standard_bundle(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    let standard = super::workspace_root().join("ryeos-bundles/standard");
    if !standard.is_dir() {
        anyhow::bail!(
            "ryeos-bundles/standard does not exist at {}",
            standard.display()
        );
    }
    let abs = standard.canonicalize()?;
    let dir = state_path.join(AI_DIR).join("node").join("bundles");
    fs::create_dir_all(&dir)?;
    let body = format!(
        "kind: node\nsection: bundles\nid: standard\npath: {}\n",
        abs.display()
    );
    let signed = lillux::signature::sign_content(&body, &fixture.publisher, "#", None);
    fs::write(dir.join("standard.yaml"), signed)?;
    Ok(())
}

/// Write a signed authorized-key TOML for the daemon's HTTP auth path.
/// Used by tests that POST signed `/execute*` requests.
pub fn write_authorized_key(state_path: &Path, sk: &SigningKey) -> Result<()> {
    let vk = sk.verifying_key();
    let fp = lillux::signature::compute_fingerprint(&vk);
    let auth_dir = state_path
        .join(AI_DIR)
        .join("node")
        .join("auth")
        .join("authorized_keys");
    fs::create_dir_all(&auth_dir)?;

    let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
    let toml_body = format!(
        r#"fingerprint = "{fp}"
public_key = "ed25519:{key_b64}"
scopes = ["*"]
label = "fast-fixture-authorized-key"
"#
    );
    let signed = lillux::signature::sign_content(&toml_body, sk, "#", None);
    fs::write(auth_dir.join(format!("{fp}.toml")), signed)?;
    Ok(())
}

// ── Internals ───────────────────────────────────────────────────────

fn write_pem_signing_key(path: &Path, sk: &SigningKey) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pem = sk
        .to_pkcs8_pem(Default::default())
        .context("encode signing key to PKCS8 PEM")?;
    fs::write(path, pem.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn write_self_signed_trust_doc(trust_dir: &Path, sk: &SigningKey) -> Result<()> {
    let vk = sk.verifying_key();
    let fp = lillux::signature::compute_fingerprint(&vk);
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
    let body = format!(
        r#"fingerprint = "{fp}"
owner = "self"
version = "1.0.0"
attestation = ""

[public_key]
pem = "ed25519:{key_b64}"
"#
    );
    let signed = lillux::signature::sign_content(&body, sk, "#", None);
    fs::write(trust_dir.join(format!("{fp}.toml")), signed)
        .with_context(|| format!("write trust doc for {fp}"))?;
    Ok(())
}
