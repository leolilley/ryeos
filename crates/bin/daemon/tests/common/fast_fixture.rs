//! Fast, deterministic fixture for ryeosd integration tests.
//!
//! Drop-in replacement for the daemon's real `` path
//! using fixed keys + a fixed signing timestamp so the produced state
//! is **byte-stable across runs of the fixture**.
//!
//! ## What is byte-stable
//!
//! After [`populate_initialized_state`] returns, the following files
//! have identical bytes across every run of the fixture:
//!
//!   * `<state>/.ai/node/identity/private_key.pem`   (deterministic Ed25519)
//!   * `<state>/.ai/node/identity/public-identity.json`
//!   * `<state>/.ai/node/vault/private_key.pem`      (deterministic X25519)
//!   * `<state>/.ai/node/vault/public_key.pem`
//!   * `<user>/.ai/config/keys/signing/private_key.pem`
//!   * `<user>/.ai/config/keys/trusted/<fp>.toml` for publisher / node / user
//!
//! Determinism is achieved by signing all fixture-authored content with
//! [`FAST_FIXTURE_TIME`] via `lillux::signature::sign_content_at` and
//! `NodeIdentity::write_public_identity_at`.
//!
//! ## Differences from `ryeosd::bootstrap::init`
//!
//! The fast fixture is structurally a **superset** of `init` with one
//! intentional omission:
//!
//!   * **Adds** publisher self-trust: tests sign their own bundle /
//!     directive / route content with `FastFixture::publisher`, and the
//!     daemon's trust store needs that key pinned. Real `init` doesn't
//!     pin publisher keys — the operator does that via `ryeos trust pin`.
//!   * **Adds** system-bundle signer trust (via
//!     `super::populate_trusted_keys`): without this the daemon refuses
//!     to load `node:engine/kinds/...` items in the core bundle and
//!     bootstrap aborts. Real `init` doesn't seed this either; the
//!     operator pins the platform author key manually or via the
//!     standard install flow.
//!   * **Omits** `<state>/.ai/node/config.yaml`. Real `init` writes
//!     this for next-boot persistence, but its content is per-run
//!     dependent (tempdir paths, picked port). Fast-path tests pass
//!     all settings via CLI/env which take precedence in
//!     `Config::load`, and `bootstrap::verify_initialized` does not
//!     require the file. Writing it would either lie about real values
//!     or break byte-stability; we skip it.
//!
//! ## Three-key role split
//!
//! Mirrors the locked publisher / user / node split:
//!
//!   * [`FastFixture::publisher`] — signs test bundle / directive /
//!     route content. Most tests use this.
//!   * [`FastFixture::node`]      — daemon's persistent identity.
//!     Signs daemon-state artifacts (e.g. authorized-key files).
//!   * [`FastFixture::user`]      — operator's persistent identity.
//!     Signs HTTP requests when the test exercises authenticated routes.
//!
//! Plus an X25519 vault keypair distinct from all Ed25519 keys (so node
//! key rotation doesn't brick the vault) — see [`vault_secret_key`].

#![allow(dead_code)]

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine as _;
use lillux::crypto::{EncodePrivateKey, SigningKey};

use ryeos_engine::AI_DIR;

/// Fixed signing timestamp used by all fixture-authored content.
/// Every call to `sign_content_at` / `write_public_identity_at` in this
/// module passes this value so the output is byte-identical across runs.
pub const FAST_FIXTURE_TIME: &str = "2026-01-01T00:00:00Z";

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

/// Pre-populate `state_path` with everything
/// `bootstrap::init` would produce, using deterministic keys.
///
/// After this returns the daemon can boot WITHOUT ``:
/// node identity, vault keypair, layout dirs, user signing key, and
/// self-signed trust docs are all in place.
///
/// What it writes:
///
/// ```text
/// <state>/.ai/node/identity/private_key.pem            (deterministic node Ed25519)
/// <state>/.ai/node/command_registration/default.yaml   (deterministic node-signed seed)
/// <state>/.ai/node/vault/private_key.pem               (deterministic vault X25519)
/// <state>/.ai/node/vault/public_key.pem
/// <state>/.ai/node/auth/authorized_keys/               (empty dir)
/// <state>/.ai/state/objects/                           (empty dir)
/// <state>/.ai/state/refs/                              (empty dir)
/// <state>/.ai/config/keys/signing/private_key.pem      (deterministic operator Ed25519)
/// <state>/.ai/config/keys/trusted/<publisher_fp>.toml  (self-signed trust doc)
/// <state>/.ai/config/keys/trusted/<node_fp>.toml       (self-signed trust doc)
/// <state>/.ai/config/keys/trusted/<user_fp>.toml       (self-signed trust doc)
/// ```
///
/// Does NOT install bundles. Tests that need the standard bundle
/// installed call `register_standard_bundle()` separately.
pub fn populate_initialized_state(state_path: &Path, _home_dir: &Path) -> Result<FastFixture> {
    let publisher = publisher_signing_key();
    let node = node_signing_key();
    let user = user_signing_key();
    let vault = vault_secret_key();

    // ── Layout dirs (mirrors bootstrap::create_directory_layout) ──
    for d in [
        state_path
            .join(AI_DIR)
            .join("node")
            .join("auth")
            .join("authorized_keys"),
        state_path
            .join(AI_DIR)
            .join("node")
            .join("command_registration"),
        state_path.join(AI_DIR).join("node").join("vault"),
        state_path.join(AI_DIR).join("node").join("identity"),
        state_path.join(AI_DIR).join("state").join("objects"),
        state_path.join(AI_DIR).join("state").join("refs"),
        state_path
            .join(AI_DIR)
            .join("config")
            .join("keys")
            .join("signing"),
        state_path
            .join(AI_DIR)
            .join("config")
            .join("keys")
            .join("trusted"),
    ] {
        fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }

    // ── Node Ed25519 identity ──
    let node_identity_dir = state_path.join(AI_DIR).join("node").join("identity");
    write_pem_signing_key(&node_identity_dir.join("private_key.pem"), &node)
        .context("write node signing key")?;

    // Public identity doc — read by the `tool:ryeos/core/identity/public_key`
    // tool. Daemon startup itself doesn't need this file (it loads the
    // private key directly), but tests that exercise the public_key tool
    // would otherwise see a null result. Mirrors what
    // `bootstrap::init` writes after generating the node key.
    let public_identity_path = node_identity_dir.join("public-identity.json");
    ryeos_app::identity::NodeIdentity::load(&node_identity_dir.join("private_key.pem"))
        .context("re-load node identity to write public doc")?
        .write_public_identity_at(&public_identity_path, FAST_FIXTURE_TIME)
        .context("write node public identity")?;

    // ── Node-owned command registration policy ──
    //
    // Real `ryeos init` verifies the publisher-signed seed policy from
    // bundles/.ai/node/init/command-registration and re-signs it with the node
    // identity. The node-config loader intentionally requires this section and
    // requires the node signer, so the fast fixture mirrors that fail-closed
    // boot contract rather than making command registration optional.
    materialize_seed_command_registration_policy(state_path, &node)
        .context("materialize command registration policy")?;

    // ── Operator Ed25519 identity ──
    write_pem_signing_key(
        &state_path
            .join(AI_DIR)
            .join("config")
            .join("keys")
            .join("signing")
            .join("private_key.pem"),
        &user,
    )
    .with_context(|| format!("write operator signing key under {}", state_path.display()))?;

    // ── Vault X25519 keypair ──
    let vault_dir = state_path.join(AI_DIR).join("node").join("vault");
    lillux::vault::write_secret_key(&vault_dir.join("private_key.pem"), &vault)
        .context("write vault secret key")?;
    lillux::vault::write_public_key(&vault_dir.join("public_key.pem"), &vault.public_key())
        .context("write vault public key")?;

    // ── Ingest ignore config (mirrors ryeos init step 8b) ──
    let ignore_dir = state_path.join(AI_DIR).join("node").join("ingest");
    fs::create_dir_all(&ignore_dir).with_context(|| format!("create {}", ignore_dir.display()))?;
    let builtin = ryeos_app::ignore::builtin_patterns();
    let patterns_yaml = builtin
        .iter()
        .map(|p| format!("  - {:?}", p))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        ignore_dir.join("ignore.yaml"),
        format!("patterns:\n{}\n", patterns_yaml),
    )
    .context("write ignore config")?;

    // ── Self-signed trust docs (publisher + node + operator) ──
    let trust_dir = state_path
        .join(AI_DIR)
        .join("config")
        .join("keys")
        .join("trusted");
    for sk in [&publisher, &node, &user] {
        write_self_signed_trust_doc(&trust_dir, sk).with_context(|| {
            format!("write self-signed trust doc under {}", state_path.display())
        })?;
    }

    // ── Trust the system-bundle signer (signs `bundles/core` items) ──
    //
    // Without this the daemon refuses to load the kind schemas inside
    // the core bundle and bootstrap aborts with `untrusted signer` for
    // every `node:engine/kinds/...` item. Mirrors what the existing
    // `populate_trusted_keys()` helper does on the slow path.
    super::populate_trusted_keys(state_path);

    Ok(FastFixture {
        publisher,
        node,
        user,
        vault,
    })
}

/// Write a `kind: node, section: bundles` record registering the core
/// bundle that lives at `state_path` itself (the daemon harness copies
/// `bundles/core` into the test tempdir and uses that as
/// `app_root`). `bootstrap::verify_initialized` requires at
/// least one registered bundle, so the harness calls this before
/// spawning the daemon.
pub fn register_core_bundle_at_state(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    let abs = state_path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", state_path.display()))?;
    let dir = state_path.join(AI_DIR).join("node").join("bundles");
    fs::create_dir_all(&dir)?;
    let body = format!(
        "kind: node\nsection: bundles\nid: core\npath: {}\n",
        abs.display()
    );
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(dir.join("core.yaml"), signed)?;
    Ok(())
}

/// Write a `kind: node, section: bundles` record pointing at
/// `bundles/standard`, signed with the publisher key. Use this
/// when a test needs the standard bundle's runtime/directive YAMLs in
/// the daemon's effective bundle roots.
pub fn register_standard_bundle(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    super::ensure_bundles_fresh();
    let standard = super::workspace_root().join("bundles/standard");
    if !standard.is_dir() {
        anyhow::bail!("bundles/standard does not exist at {}", standard.display());
    }
    let abs = standard.canonicalize()?;
    let dir = state_path.join(AI_DIR).join("node").join("bundles");
    fs::create_dir_all(&dir)?;
    let body = format!(
        "kind: node\nsection: bundles\nid: standard\npath: {}\n",
        abs.display()
    );
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(dir.join("standard.yaml"), signed)?;
    // Studio bundle was split from standard — tests that register
    // standard also need Studio for the UI service catalog self-check.
    register_studio_bundle(state_path, fixture)?;
    Ok(())
}

/// Write a `kind: node, section: bundles` record pointing at
/// `bundles/studio`, signed with the publisher key. Use this when
/// a test needs the Studio bundle's UI routes and services.
pub fn register_studio_bundle(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    let studio = super::workspace_root().join("bundles/studio");
    if !studio.is_dir() {
        anyhow::bail!("bundles/studio does not exist at {}", studio.display());
    }
    let abs = studio.canonicalize()?;
    let dir = state_path.join(AI_DIR).join("node").join("bundles");
    fs::create_dir_all(&dir)?;
    let body = format!(
        "kind: node\nsection: bundles\nid: studio\npath: {}\n",
        abs.display()
    );
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(dir.join("studio.yaml"), signed)?;
    Ok(())
}

/// Write a signed authorized-key TOML for the daemon's HTTP auth path.
///
/// `subject_sk` is the public key the daemon will accept on
/// `x-ryeos-key-id`-signed HTTP requests (typically [`FastFixture::user`]).
///
/// `signer_sk` MUST be the node identity ([`FastFixture::node`]) — the
/// daemon's auth loader requires authorized-key files to be signed by
/// the node key. Passing a non-node signer is a test bug.
pub fn write_authorized_key_signed_by(
    state_path: &Path,
    subject_sk: &SigningKey,
    signer_sk: &SigningKey,
) -> Result<()> {
    let vk = subject_sk.verifying_key();
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
    let signed =
        lillux::signature::sign_content_at(&toml_body, signer_sk, "#", None, FAST_FIXTURE_TIME);
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
    fs::write(path, pem.as_bytes()).with_context(|| format!("write {}", path.display()))?;
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
    let signed = lillux::signature::sign_content_at(&body, sk, "#", None, FAST_FIXTURE_TIME);
    fs::write(trust_dir.join(format!("{fp}.toml")), signed)
        .with_context(|| format!("write trust doc for {fp}"))?;
    Ok(())
}

fn materialize_seed_command_registration_policy(
    state_path: &Path,
    node: &SigningKey,
) -> Result<()> {
    let source = super::workspace_root()
        .join("bundles")
        .join(AI_DIR)
        .join("node")
        .join("init")
        .join("command-registration")
        .join("default.yaml");
    let raw = fs::read_to_string(&source)
        .with_context(|| format!("read command registration seed {}", source.display()))?;
    let body = lillux::signature::strip_signature_lines(&raw);
    let signed = lillux::signature::sign_content_at(&body, node, "#", None, FAST_FIXTURE_TIME);
    let target_dir = state_path
        .join(AI_DIR)
        .join("node")
        .join("command_registration");
    fs::create_dir_all(&target_dir).with_context(|| format!("create {}", target_dir.display()))?;
    fs::write(target_dir.join("default.yaml"), signed)
        .context("write command registration policy")?;
    Ok(())
}
