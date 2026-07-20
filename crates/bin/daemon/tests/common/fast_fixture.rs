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

/// Install one executable into a synthetic test bundle with the same complete
/// provenance chain required from published bundles: installed bytes, CAS
/// blob, canonical ItemSource object and signed sidecar, source-manifest
/// object, and signed manifest ref. Repeated calls extend the existing
/// manifest only when it is signed by the same fixture authority.
///
/// Returns the canonical path-style binary ref for use in runtime or handler
/// descriptors.
pub fn install_signed_bundle_binary(
    bundle_root: &Path,
    binary_name: &str,
    bytes: &[u8],
    signer: &SigningKey,
) -> Result<String> {
    anyhow::ensure!(
        !binary_name.is_empty()
            && Path::new(binary_name)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(binary_name),
        "synthetic bundle binary name must be one safe path segment"
    );

    let triple = env!("RYEOSD_HOST_TRIPLE");
    let ai_dir = bundle_root.join(AI_DIR);
    let binary_path = ai_dir.join("bin").join(triple).join(binary_name);
    fs::create_dir_all(
        binary_path
            .parent()
            .context("synthetic binary path has no parent")?,
    )?;
    fs::write(&binary_path, bytes)
        .with_context(|| format!("write synthetic binary {}", binary_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&binary_path, fs::Permissions::from_mode(0o755))?;
    }

    let cas = lillux::cas::CasStore::new(ai_dir.join("objects"));
    let expected_content_hash = lillux::sha256_hex(bytes);
    let content_blob_hash = cas.store_blob(bytes)?;
    anyhow::ensure!(
        content_blob_hash == expected_content_hash,
        "fixture CAS returned {content_blob_hash} for bytes hashing to {expected_content_hash}"
    );

    let item_ref = format!("bin/{triple}/{binary_name}");
    let item_source = serde_json::json!({
        "kind": "item_source",
        "item_ref": item_ref,
        "content_blob_hash": content_blob_hash,
        "integrity": format!("sha256:{content_blob_hash}"),
        "signature_info": null,
        "mode": 0o755,
    });
    let item_source_hash = cas.store_object(&item_source)?;
    let sidecar_body = lillux::cas::canonical_json(&item_source)?;
    let signed_sidecar =
        lillux::signature::sign_content_at(&sidecar_body, signer, "#", None, FAST_FIXTURE_TIME);
    fs::write(
        binary_path.with_file_name(format!("{binary_name}.item_source.json")),
        signed_sidecar,
    )?;

    let manifest_ref_path = ai_dir.join("refs").join("bundles").join("manifest");
    let signer_key = signer.verifying_key();
    let signer_fingerprint = lillux::signature::compute_fingerprint(&signer_key);
    let mut item_source_hashes = match fs::read_to_string(&manifest_ref_path) {
        Ok(signed_ref) => {
            let verified = ryeos_engine::executor_resolution::verify_signed_executor_manifest_ref(
                &signed_ref,
                |candidate| (candidate == signer_fingerprint.as_str()).then_some(signer_key),
                ryeos_engine::resolution::TrustClass::TrustedBundle,
            )?;
            anyhow::ensure!(
                verified.signer_fingerprint == signer_fingerprint,
                "existing synthetic executor manifest has the wrong signer"
            );
            let manifest = cas.get_object(&verified.manifest_hash)?.with_context(|| {
                format!(
                    "synthetic source manifest {} is missing from CAS",
                    verified.manifest_hash
                )
            })?;
            ryeos_engine::executor_resolution::verify_executor_manifest_object(
                &manifest,
                &verified.manifest_hash,
            )?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::collections::HashMap::new()
        }
        Err(error) => return Err(error.into()),
    };
    item_source_hashes.insert(item_ref.clone(), item_source_hash);

    let manifest = serde_json::json!({
        "kind": "source_manifest",
        "item_source_hashes": item_source_hashes,
    });
    let manifest_hash = cas.store_object(&manifest)?;
    let ref_body = format!(
        "{}\n{manifest_hash}\n",
        ryeos_engine::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN
    );
    let signed_ref =
        lillux::signature::sign_content_at(&ref_body, signer, "#", None, FAST_FIXTURE_TIME);
    fs::create_dir_all(
        manifest_ref_path
            .parent()
            .context("synthetic manifest ref has no parent")?,
    )?;
    fs::write(&manifest_ref_path, signed_ref)?;

    Ok(item_ref)
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
/// <state>/.ai/node/isolation.yaml                        (node isolation policy)
/// <state>/.ai/node/command_registration/default.yaml   (deterministic node-signed seed)
/// <state>/.ai/node/vault/private_key.pem               (deterministic vault X25519)
/// <state>/.ai/node/vault/public_key.pem
/// <state>/.ai/node/auth/authorized_keys/               (empty dir)
/// <state>/.ai/state/objects/                           (empty dir)
/// <state>/.ai/state/locators/                          (empty dir)
/// <state>/.ai/state/refs/                              (empty dir)
/// <state>/.ai/state/recovery/thread-projection/        (empty dir)
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
        state_path.join(AI_DIR).join("state").join("locators"),
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
    let runtime_state_path = state_path.join(AI_DIR).join("state");
    let runtime_state = lillux::PinnedDirectory::open_or_create(&runtime_state_path)
        .context("pin fast-fixture runtime-state directory")?;
    let recovery = runtime_state
        .open_or_create_child(std::ffi::OsStr::new("recovery"), 0o700)
        .context("create fast-fixture recovery authority")?;
    recovery
        .open_or_create_child(std::ffi::OsStr::new("thread-projection"), 0o700)
        .context("create fast-fixture thread-projection recovery authority")?;

    fs::write(
        state_path.join(AI_DIR).join("node").join("isolation.yaml"),
        "version: 1\nmode: disabled\nbackend: null\nfilesystem:\n  readable:\n    - \"{node_public_identity}\"\n    - \"{daemon_socket}\"\n    - \"{bundle_roots}\"\n    - \"{node_trusted_keys}\"\n    - \"{verified_code}\"\n  writable:\n    - \"{project}\"\n    - \"{checkpoint_dir}\"\n\
         network:\n  mode: host\nenvironment:\n  allow:\n    - \"*\"\n\
         limits:\n  open_files: 1024\n  stdout_bytes: 8388608\n  stderr_bytes: 8388608\n  verified_artifact_file_bytes: 67108864\n  verified_artifact_total_bytes: 268435456\n  verified_artifact_files: 4096\n",
    )
    .context("write node isolation policy")?;

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

/// Write a `kind: node` bundle record registering the core
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
    let body = node_bundle_record_body("core", &abs)?;
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(dir.join("core.yaml"), signed)?;
    Ok(())
}

/// The `command_registration_caps` the node-init bundle-registration grants
/// assign to `bundle_name`, read from the same signed source the real install
/// materializes from (`bundles/.ai/node/init/bundle-registration-grants/
/// default.yaml`). Empty when the bundle declares no grants. Mirroring
/// production here is what lets a fixture-registered bundle register a command
/// whose dispatch kind is gated behind a registration capability — e.g.
/// standard's `graph validate`, which dispatches `direct_execute_item_ref`.
fn bundle_registration_caps(bundle_name: &str) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Grants {
        #[serde(default)]
        bundles: std::collections::BTreeMap<String, BundleGrant>,
    }
    #[derive(serde::Deserialize)]
    struct BundleGrant {
        #[serde(default)]
        command_registration_caps: Vec<String>,
    }
    let source = super::workspace_root()
        .join("bundles")
        .join(AI_DIR)
        .join("node")
        .join("init")
        .join("bundle-registration-grants")
        .join("default.yaml");
    let raw = fs::read_to_string(&source)
        .with_context(|| format!("read bundle registration grants {}", source.display()))?;
    let grants: Grants = serde_yaml::from_str(&raw)
        .with_context(|| format!("parse bundle registration grants {}", source.display()))?;
    Ok(grants
        .bundles
        .get(bundle_name)
        .map(|g| g.command_registration_caps.clone())
        .unwrap_or_default())
}

/// Render a signed-ready `kind: node` bundle record body for `bundle_name`,
/// including any `command_registration_caps` the node-init grants assign it —
/// the same shape the real install writes to `.ai/node/bundles/<name>.yaml`.
fn node_bundle_record_body(bundle_name: &str, path: &Path) -> Result<String> {
    let mut body = format!("kind: node\npath: {}\n", path.display());
    let caps = bundle_registration_caps(bundle_name)?;
    if !caps.is_empty() {
        body.push_str("command_registration_caps:\n");
        for cap in &caps {
            body.push_str(&format!("  - {cap}\n"));
        }
    }
    Ok(body)
}

/// Register a synthetic bundle root using the exact current signed node-bundle
/// record. Tests that author runtime or handler descriptors must register the
/// bundle containing those descriptors rather than adding them to the copied
/// core bundle (whose executor manifest belongs to a different authority).
pub fn register_fixture_bundle(
    state_path: &Path,
    bundle_name: &str,
    bundle_root: &Path,
    fixture: &FastFixture,
) -> Result<()> {
    anyhow::ensure!(
        !bundle_name.is_empty()
            && Path::new(bundle_name)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(bundle_name),
        "synthetic bundle name must be one safe path segment"
    );
    let abs = bundle_root
        .canonicalize()
        .with_context(|| format!("canonicalize synthetic bundle {}", bundle_root.display()))?;

    // Registered bundles are current bundle roots, not loose descriptor
    // directories. Give synthetic fixtures the same signed manifest boundary
    // as a published bundle. Custom kind schemas in the fixture are the kinds
    // it provides; its runtime descriptors and schema references consume the
    // core-owned handler/parser/runtime kinds.
    let kinds_root = abs.join(AI_DIR).join("node/engine/kinds");
    let mut provides_kinds = Vec::new();
    match fs::read_dir(&kinds_root) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    provides_kinds.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    provides_kinds.sort();
    provides_kinds.dedup();

    let provides_kinds_yaml = if provides_kinds.is_empty() {
        "provides_kinds: []\n".to_string()
    } else {
        format!(
            "provides_kinds:\n{}",
            provides_kinds
                .iter()
                .map(|kind| format!("  - {kind}\n"))
                .collect::<String>()
        )
    };
    let manifest_body = format!(
        "name: {bundle_name}\nversion: 1.0.0\ndescription: synthetic daemon integration fixture\n{provides_kinds_yaml}requires_kinds:\n  - handler\n  - parser\n  - runtime\nuses_kinds: []\n"
    );
    let manifest = lillux::signature::sign_content_at(
        &manifest_body,
        &fixture.publisher,
        "#",
        None,
        FAST_FIXTURE_TIME,
    );
    fs::create_dir_all(abs.join(AI_DIR))?;
    fs::write(abs.join(AI_DIR).join("manifest.yaml"), manifest)?;

    let dir = state_path.join(AI_DIR).join("node").join("bundles");
    fs::create_dir_all(&dir)?;
    let body = node_bundle_record_body(bundle_name, &abs)?;
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(dir.join(format!("{bundle_name}.yaml")), signed)?;
    Ok(())
}

/// Create and register a synthetic bundle that owns runtime configuration.
///
/// Launch contracts deliberately accept model-provider catalogs only from
/// bundle space. Integration fixtures therefore install dynamic provider
/// endpoints in a real signed bundle instead of weakening that authority
/// boundary or pretending project configuration is bundle-owned.
pub fn register_config_fixture_bundle(
    state_path: &Path,
    bundle_name: &str,
    fixture: &FastFixture,
    populate: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    anyhow::ensure!(
        !bundle_name.is_empty()
            && Path::new(bundle_name)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(bundle_name),
        "synthetic config bundle name must be one safe path segment"
    );

    let bundle_root = state_path.join(AI_DIR).join("bundles").join(bundle_name);
    fs::create_dir_all(bundle_root.join(AI_DIR))?;
    populate(&bundle_root)?;

    let manifest_body = format!(
        "name: {bundle_name}\nversion: 1.0.0\ndescription: synthetic runtime config fixture\nprovides_kinds: []\nrequires_kinds:\n  - config\nuses_kinds: []\n"
    );
    let manifest = lillux::signature::sign_content_at(
        &manifest_body,
        &fixture.publisher,
        "#",
        None,
        FAST_FIXTURE_TIME,
    );
    fs::write(bundle_root.join(AI_DIR).join("manifest.yaml"), manifest)?;

    let absolute_root = bundle_root
        .canonicalize()
        .with_context(|| format!("canonicalize config fixture {}", bundle_root.display()))?;
    let registration_dir = state_path.join(AI_DIR).join("node").join("bundles");
    fs::create_dir_all(&registration_dir)?;
    let body = node_bundle_record_body(bundle_name, &absolute_root)?;
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(registration_dir.join(format!("{bundle_name}.yaml")), signed)?;
    Ok(())
}

/// Write a `kind: node` bundle record pointing at
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
    let body = node_bundle_record_body("standard", &abs)?;
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(dir.join("standard.yaml"), signed)?;
    // RyeOS UI bundle was split from standard — tests that register
    // standard also need RyeOS UI for the UI service catalog self-check.
    register_ryeos_ui_bundle(state_path, fixture)?;
    Ok(())
}

/// Write a `kind: node` bundle record pointing at
/// `bundles/ryeos-ui`, signed with the publisher key. Use this when
/// a test needs the RyeOS UI bundle's UI routes and services.
pub fn register_ryeos_ui_bundle(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    let ryeos_ui = super::workspace_root().join("bundles/ryeos-ui");
    if !ryeos_ui.is_dir() {
        anyhow::bail!("bundles/ryeos-ui does not exist at {}", ryeos_ui.display());
    }
    let abs = ryeos_ui.canonicalize()?;
    let dir = state_path.join(AI_DIR).join("node").join("bundles");
    fs::create_dir_all(&dir)?;
    let body = node_bundle_record_body("ryeos-ui", &abs)?;
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(dir.join("ryeos-ui.yaml"), signed)?;
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
    write_authorized_key_with_scopes(state_path, subject_sk, signer_sk, &["*"])
}

/// Like [`write_authorized_key_signed_by`] but with explicit `scopes`, for
/// tests that need a capability-restricted principal (e.g. asserting a
/// runtime-cap rejection). `signer_sk` MUST be the node identity.
pub fn write_authorized_key_with_scopes(
    state_path: &Path,
    subject_sk: &SigningKey,
    signer_sk: &SigningKey,
    scopes: &[&str],
) -> Result<()> {
    let vk = subject_sk.verifying_key();
    let fp = lillux::signature::compute_fingerprint(&vk);
    let auth_dir = state_path
        .join(AI_DIR)
        .join("node")
        .join("auth")
        .join("authorized_keys");
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
    ryeos_app::identity::write_authorized_key_toml(
        &auth_dir,
        &fp,
        &key_b64,
        &scopes
            .iter()
            .map(|scope| (*scope).to_string())
            .collect::<Vec<_>>(),
        "fast-fixture-authorized-key",
        &lillux::signature::compute_fingerprint(&signer_sk.verifying_key()),
        FAST_FIXTURE_TIME,
        signer_sk,
        ryeos_app::identity::WildcardPolicy::AllowBootstrap,
    )?;
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
