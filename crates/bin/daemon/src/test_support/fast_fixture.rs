//! Fast, deterministic fixture for ryeosd integration tests.
//!
//! Drop-in replacement for the daemon's real `` path
//! using fixed keys + a fixed signing timestamp so the produced state
//! is **byte-stable across runs of the fixture**.
//!
//! ## What is byte-stable
//!
//! After [`populate_initialized_state_with_profile`] returns, the following files
//! have identical bytes across every run of the fixture:
//!
//!   * `<state>/.ai/node/identity/private_key.pem`   (deterministic Ed25519)
//!   * `<state>/.ai/node/identity/public-identity.json`
//!   * `<state>/.ai/node/substrate-identity.json`
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
//! The fast fixture is structurally a **superset** of `init`:
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
//!   * The harness writes `<state>/.ai/node/config.yaml` after selecting
//!     the exact temporary UDS path and bind. Those paths vary across
//!     runs and are outside the fixture's byte-stability assertions.
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
/// [`populate_initialized_state_with_profile`]. Tests use `fixture.publisher` to
/// sign their own bundle/directive content.
pub struct FastFixture {
    pub publisher: SigningKey,
    pub node: SigningKey,
    pub user: SigningKey,
    pub vault: lillux::vault::VaultSecretKey,
}

const FULL_BUNDLE_INVENTORY: &[&str] = &[
    "browser",
    "central-auth",
    "codex",
    "core",
    "directive",
    "environment-qualification",
    "graph",
    "knowledge",
    "opencode",
    "ryeos-docs",
    "ryeos-ui",
    "scheduling",
    "web",
];

/// The workflow bundles layered over Core by the Standard composition.
const WORKFLOW_BUNDLES: &[&str] = &["knowledge", "directive", "graph", "ryeos-docs"];

const STANDARD_BUNDLE_INVENTORY: &[&str] = &[
    "core",
    "directive",
    "graph",
    "knowledge",
    "ryeos-docs",
    "scheduling",
];

#[derive(Clone, Copy)]
pub enum FastFixtureProfile {
    Core,
    CoreScheduling,
    Standard,
    Full,
}

impl FastFixtureProfile {
    fn name(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::CoreScheduling => "core-scheduling",
            Self::Standard => "standard",
            Self::Full => "full",
        }
    }

    pub(super) fn exact_bundles(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Core => Some(&["core"]),
            Self::CoreScheduling => Some(&["core", "scheduling"]),
            Self::Standard => Some(STANDARD_BUNDLE_INVENTORY),
            Self::Full => Some(FULL_BUNDLE_INVENTORY),
        }
    }
}

/// Verify every authored profile member is registered to its exact prepared
/// bundle root after caller fixtures have been installed.
///
/// A registration outside the authored inventory is admitted only when it
/// names a synthetic test bundle (one with no published bundle of that name
/// in the workspace), as written by [`register_fixture_bundle`],
/// [`register_presigned_fixture_bundle`] or [`register_config_fixture_bundle`].
/// A published bundle the selected profile does not include is refused, so a
/// test cannot silently run a composition no authored profile selects.
pub fn validate_profile_bundle_records(
    state_path: &Path,
    fixture: &FastFixture,
    profile: FastFixtureProfile,
) -> Result<()> {
    let expected = profile
        .exact_bundles()
        .context("selected fast-fixture profile has no exact bundle inventory")?;
    let records = state_path.join(AI_DIR).join("node/bundles");
    let expected_names = expected
        .iter()
        .map(|name| format!("{name}.yaml"))
        .collect::<std::collections::BTreeSet<_>>();
    let mut observed_names = std::collections::BTreeSet::new();
    for entry in fs::read_dir(&records).with_context(|| {
        format!(
            "enumerate selected profile bundle registrations at {}",
            records.display()
        )
    })? {
        anyhow::ensure!(
            observed_names.len() < 4096,
            "selected profile registration directory exceeds its bounded entry count"
        );
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .as_os_str()
            .to_str()
            .context("selected profile bundle registration name is not UTF-8")?
            .to_owned();
        let metadata = fs::symlink_metadata(entry.path())?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "selected profile registration entry {name} is not a regular non-symlink file"
        );
        anyhow::ensure!(
            observed_names.insert(name.clone()),
            "selected profile registration directory contains duplicate entry {name}"
        );
    }
    let missing = expected_names
        .difference(&observed_names)
        .cloned()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        missing.is_empty(),
        "selected profile registration inventory lacks authored members {missing:?}"
    );
    for extra in observed_names.difference(&expected_names) {
        let bundle_name = extra
            .strip_suffix(".yaml")
            .with_context(|| format!("bundle registration entry {extra} is not a YAML record"))?;
        let published = super::workspace_root().join("bundles").join(bundle_name);
        anyhow::ensure!(
            !published.exists(),
            "published bundle {bundle_name} is registered but the selected profile does not include it"
        );
    }
    for bundle_name in expected {
        let root = if *bundle_name == "core" {
            state_path.join(AI_DIR).join("bundles/core")
        } else {
            super::workspace_root().join("bundles").join(bundle_name)
        };
        let root = root
            .canonicalize()
            .with_context(|| format!("resolve selected profile bundle {bundle_name}"))?;
        let body = node_bundle_record_body(bundle_name, &root)?;
        let expected_signed = lillux::signature::sign_content_at(
            &body,
            &fixture.publisher,
            "#",
            None,
            FAST_FIXTURE_TIME,
        );
        let record = records.join(format!("{bundle_name}.yaml"));
        let metadata = fs::symlink_metadata(&record)
            .with_context(|| format!("read selected profile registration {bundle_name}"))?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "selected profile registration {bundle_name} is not a regular non-symlink file"
        );
        anyhow::ensure!(
            fs::read(&record)? == expected_signed.as_bytes(),
            "selected profile registration {bundle_name} does not bind its expected bundle root"
        );
    }
    Ok(())
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
    pub fn site_id(&self) -> String {
        format!("site:{}", self.node_fp())
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
/// After this returns the daemon can boot without running node initialization:
/// node identity, vault keypair, layout dirs, user signing key, and
/// self-signed trust docs are all in place.
///
/// What it writes:
///
/// ```text
/// <state>/.ai/node/identity/private_key.pem            (deterministic node Ed25519)
/// <state>/.ai/node/policies/*.yaml                    (complete node-signed generation)
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
/// Populate fast-fixture state and node policies from an explicit authored
/// profile. Bundle registration remains a separate caller-owned step.
pub fn populate_initialized_state_with_profile(
    state_path: &Path,
    _home_dir: &Path,
    profile: FastFixtureProfile,
) -> Result<FastFixture> {
    populate_initialized_state_with_profile_and_identities(
        state_path,
        _home_dir,
        node_signing_key(),
        user_signing_key(),
        profile,
    )
}

pub fn populate_initialized_state_with_profile_and_identities(
    state_path: &Path,
    _home_dir: &Path,
    node: SigningKey,
    user: SigningKey,
    profile: FastFixtureProfile,
) -> Result<FastFixture> {
    let publisher = publisher_signing_key();
    let vault = vault_secret_key();

    // ── Layout dirs (mirrors bootstrap::create_directory_layout) ──
    for d in [
        state_path
            .join(AI_DIR)
            .join("node")
            .join("auth")
            .join("authorized_keys"),
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

    // Init completion binds the immutable substrate coordinate and selected
    // composition profile, matching the authored policy generation below.
    let substrate = ryeos_node::SubstrateIdentity::new(
        format!("sha256:{}", "1".repeat(64)),
        1,
        Some(profile.name().to_owned()),
        Some(FAST_FIXTURE_HOST_CLASS.to_owned()),
    )
    .context("construct fast-fixture substrate identity")?;
    let substrate_path = state_path.join(AI_DIR).join("node/substrate-identity.json");
    lillux::atomic_write(&substrate_path, &serde_json::to_vec_pretty(&substrate)?)
        .with_context(|| format!("write {}", substrate_path.display()))?;

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
    ryeos_node_runtime::identity::NodeIdentity::load(&node_identity_dir.join("private_key.pem"))
        .context("re-load node identity to write public doc")?
        .write_public_identity_at(&public_identity_path, FAST_FIXTURE_TIME)
        .context("write node public identity")?;

    // Mirror first publication from the explicitly selected init profile.
    materialize_seed_node_policy_generation(state_path, &node, profile)
        .context("materialize complete node policy generation")?;

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

/// Seal the fixture's final policy and bundle-registration set using the same
/// signed init-completion contract as the real node initializer. The test-only
/// node helper also invokes the real capacity issuer under the stopped-node
/// lock; a parsed native-capacity policy alone cannot make this fixture live.
/// Capacity files bind real inode identities and are not byte-stability inputs.
pub fn seal_initialized_state(state_path: &Path) -> Result<()> {
    ryeos_node::init::seal_test_fixture_init_completion(state_path)
        .context("seal fast-fixture initialization transaction")
}

/// Write a `kind: node` bundle record registering the core
/// bundle that lives at `state_path` itself (the daemon harness copies
/// `bundles/core` into the test tempdir and uses that as
/// `app_root`). `bootstrap::verify_initialized` requires at
/// least one registered bundle, so the harness calls this before
/// spawning the daemon.
pub fn register_core_bundle_at_state(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    let abs = state_path
        .join(AI_DIR)
        .join("bundles")
        .join("core")
        .canonicalize()
        .with_context(|| format!("canonicalize installed Core under {}", state_path.display()))?;
    let dir = state_path.join(AI_DIR).join("node").join("bundles");
    fs::create_dir_all(&dir)?;
    let body = node_bundle_record_body("core", &abs)?;
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(dir.join("core.yaml"), signed)?;
    Ok(())
}

/// Render the exact current bundle-registration shape. Command-registration
/// authority belongs only to the node policy generation.
fn node_bundle_record_body(_bundle_name: &str, path: &Path) -> Result<String> {
    Ok(format!("kind: node\npath: {}\n", path.display()))
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

    register_presigned_fixture_bundle(state_path, bundle_name, &abs, fixture)
}

/// Register an already signed test bundle without replacing its manifest.
/// External lifecycle fixtures declare capabilities in that manifest, so the
/// generic fixture writer above must not erase them during daemon setup.
pub fn register_presigned_fixture_bundle(
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
    anyhow::ensure!(
        abs.join(AI_DIR).join("manifest.yaml").is_file(),
        "presigned fixture bundle has no manifest"
    );
    let dir = state_path.join(AI_DIR).join("node").join("bundles");
    fs::create_dir_all(&dir)?;
    let body = node_bundle_record_body(bundle_name, &abs)?;
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(dir.join(format!("{bundle_name}.yaml")), signed)?;
    authorize_fixture_bundle(state_path, bundle_name, &fixture.node)?;
    Ok(())
}

/// Write the init-owned bootstrap config after the harness has selected its
/// exact endpoints. A fabricated UDS path would conflict with the startup
/// command and would test `--force` instead of ordinary admission.
pub fn write_harness_bootstrap_config(
    state_path: &Path,
    bind: std::net::SocketAddr,
    uds_path: &Path,
) -> Result<()> {
    let config = crate::config::Config {
        bind,
        db_path: state_path.join(AI_DIR).join("state/runtime.sqlite3"),
        uds_path: uds_path.to_path_buf(),
        app_root: state_path.to_path_buf(),
        node_signing_key_path: state_path
            .join(AI_DIR)
            .join("node/identity/private_key.pem"),
        operator_signing_key_path: state_path
            .join(AI_DIR)
            .join("config/keys/signing/private_key.pem"),
        authorized_keys_dir: state_path.join(AI_DIR).join("node/auth/authorized_keys"),
    };
    fs::write(
        state_path.join(AI_DIR).join("node/config.yaml"),
        serde_yaml::to_string(&config)?,
    )
    .context("write exact fast-fixture daemon bootstrap config")
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
    authorize_fixture_bundle(state_path, bundle_name, &fixture.node)?;
    Ok(())
}

/// Register the central-auth component from the already prepared bundle set.
pub fn register_central_auth_bundle(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    super::require_bundles_prepared(&["central-auth"])?;
    register_bundle_record(state_path, fixture, "central-auth")
}

/// Write `kind: node` bundle records for the knowledge, directive, graph and
/// ryeos-docs bundles.
pub fn register_workflow_bundles(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    super::require_bundles_prepared(&["core", "knowledge", "directive", "graph", "ryeos-docs"])?;
    for bundle_name in WORKFLOW_BUNDLES {
        register_bundle_record(state_path, fixture, bundle_name)?;
    }
    Ok(())
}

/// Register the exact authored Standard composition: Core, Scheduling, and
/// the workflow bundles. Its policy retains Standard's maintenance
/// obligations while its physical bundle set matches `standard.yaml`.
pub fn register_standard_composition(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    super::require_bundles_prepared(STANDARD_BUNDLE_INVENTORY)?;
    register_scheduling_bundle(state_path, fixture)?;
    register_workflow_bundles(state_path, fixture)
}

/// Register every bundle selected by the authored Full profile. The caller's
/// prepared-artifact gate and the profile inventory share the same source list.
pub fn register_full_composition(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    super::require_bundles_prepared(FULL_BUNDLE_INVENTORY)?;
    for bundle_name in FULL_BUNDLE_INVENTORY {
        // Core was already registered against the copied state-root bundle
        // by the base fixture. Preserve that exact root rather than replacing
        // it with the workspace source tree.
        if *bundle_name == "core" {
            continue;
        }
        register_bundle_record(state_path, fixture, bundle_name)?;
    }
    Ok(())
}

/// Register the separately selected Scheduling bundle without refreshing,
/// building, or signing workspace artifacts. The caller must first prepare
/// the published Core+Scheduling bundle set.
pub fn register_scheduling_bundle(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    super::require_bundles_prepared(&["core", "scheduling"])?;
    register_bundle_record(state_path, fixture, "scheduling")
}

fn register_bundle_record(
    state_path: &Path,
    fixture: &FastFixture,
    bundle_name: &str,
) -> Result<()> {
    let bundle = super::workspace_root().join("bundles").join(bundle_name);
    if !bundle.is_dir() {
        anyhow::bail!("prepared bundle does not exist at {}", bundle.display());
    }
    let abs = bundle.canonicalize()?;
    let dir = state_path.join(AI_DIR).join("node").join("bundles");
    fs::create_dir_all(&dir)?;
    let body = node_bundle_record_body(bundle_name, &abs)?;
    let signed =
        lillux::signature::sign_content_at(&body, &fixture.publisher, "#", None, FAST_FIXTURE_TIME);
    fs::write(dir.join(format!("{bundle_name}.yaml")), signed)?;
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
    ryeos_node_runtime::identity::write_authorized_key_toml(
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
        ryeos_node_runtime::identity::WildcardPolicy::AllowBootstrap,
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

fn materialize_seed_node_policy_generation(
    state_path: &Path,
    node: &SigningKey,
    selected_profile: FastFixtureProfile,
) -> Result<()> {
    let source = super::workspace_root()
        .join("bundles")
        .join(AI_DIR)
        .join("node")
        .join("init")
        .join("profiles")
        .join(format!("{}.yaml", selected_profile.name()));
    let raw = fs::read_to_string(&source)
        .with_context(|| format!("read node init profile {}", source.display()))?;
    let body = lillux::signature::strip_signature_lines(&raw);
    let profile: ryeos_node_runtime::node_policy::generation::NodeInitProfile =
        serde_yaml::from_str(&body)
            .with_context(|| format!("parse node init profile {}", source.display()))?;
    profile
        .validate(
            &ryeos_node_runtime::node_policy::NodePolicyTable::new(),
            &source,
        )
        .with_context(|| format!("validate selected node init profile {}", source.display()))?;
    if let Some(expected) = selected_profile.exact_bundles() {
        anyhow::ensure!(
            profile.exact_bundles()
                == expected
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect::<Vec<_>>(),
            "selected profile {} exact bundle inventory differs from the fast-fixture selection",
            selected_profile.name()
        );
    }
    // Every fast-fixture node runs under its profile's local class.
    let mut profile = profile
        .for_host_class(FAST_FIXTURE_HOST_CLASS)
        .with_context(|| {
            format!(
                "select the local class of profile {}",
                selected_profile.name()
            )
        })?;
    let isolation = profile
        .policies
        .get_mut("isolation")
        .context("selected node init profile has no isolation policy")?;
    direct_host_isolation(isolation).with_context(|| {
        format!(
            "derive direct-host isolation fixture from profile {}",
            selected_profile.name()
        )
    })?;
    profile
        .validate(
            &ryeos_node_runtime::node_policy::NodePolicyTable::new(),
            &source,
        )
        .with_context(|| {
            format!(
                "validate direct-host fixture policy generation for profile {}",
                selected_profile.name()
            )
        })?;
    let target_dir = state_path.join(AI_DIR).join("node").join("policies");
    fs::create_dir_all(&target_dir).with_context(|| format!("create {}", target_dir.display()))?;
    for (name, policy) in profile.policies() {
        let body = serde_yaml::to_string(policy)
            .with_context(|| format!("serialize `{name}` test node policy"))?;
        let signed = lillux::signature::sign_content_at(&body, node, "#", None, FAST_FIXTURE_TIME);
        fs::write(target_dir.join(format!("{name}.yaml")), signed)
            .with_context(|| format!("write `{name}` test node policy"))?;
    }
    Ok(())
}

/// Host class every fast-fixture node is initialized under.
const FAST_FIXTURE_HOST_CLASS: &str = "local";

/// Author a profile source root for switching a fast-fixture node to
/// `profile` through the real `ryeos node reset policy-generation`.
///
/// The authored profile keeps a per-scope floor that a directly spawned
/// daemon cannot prove, so the root holds the profile narrowed exactly as
/// the fixture narrows its initial generation and signed by the fixture
/// publisher, whose key the node already trusts. The CLI then installs a
/// complete, consistently sealed generation. Every bundle the profile names
/// is copied from `authored_source`, along with its publisher trust document.
pub fn author_direct_host_profile_source(
    authored_source: &Path,
    profile: &str,
    destination: &Path,
) -> Result<()> {
    let profile_dir = destination
        .join(AI_DIR)
        .join("node")
        .join("init")
        .join("profiles");
    fs::create_dir_all(&profile_dir)
        .with_context(|| format!("create {}", profile_dir.display()))?;
    let authored = authored_source
        .join(AI_DIR)
        .join("node")
        .join("init")
        .join("profiles")
        .join(format!("{profile}.yaml"));
    let raw = fs::read_to_string(&authored)
        .with_context(|| format!("read node init profile {}", authored.display()))?;
    let mut document: serde_json::Value =
        serde_yaml::from_str(&lillux::signature::strip_signature_lines(&raw))
            .with_context(|| format!("parse node init profile {}", authored.display()))?;
    let isolation = document
        .pointer_mut("/policies/isolation")
        .context("node init profile has no isolation policy")?;
    direct_host_isolation(isolation)
        .with_context(|| format!("derive direct-host isolation for profile {profile}"))?;
    let bundles: Vec<String> = document
        .get("exact_bundles")
        .and_then(serde_json::Value::as_array)
        .context("node init profile has no exact_bundles list")?
        .iter()
        .map(|name| {
            name.as_str()
                .map(str::to_owned)
                .context("exact_bundles entry is not a name")
        })
        .collect::<Result<_>>()?;
    let body = serde_yaml::to_string(&document).context("serialize narrowed node init profile")?;
    let signed = lillux::signature::sign_content_at(
        &body,
        &publisher_signing_key(),
        "#",
        None,
        FAST_FIXTURE_TIME,
    );
    fs::write(profile_dir.join(format!("{profile}.yaml")), signed)
        .with_context(|| format!("write narrowed node init profile {profile}"))?;
    fs::copy(
        authored_source.join(AI_DIR).join("PUBLISHER_TRUST.toml"),
        destination.join(AI_DIR).join("PUBLISHER_TRUST.toml"),
    )
    .context("copy publisher trust document")?;
    for bundle in bundles {
        copy_tree_preserving_mtimes(&authored_source.join(&bundle), &destination.join(&bundle))
            .with_context(|| format!("copy bundle {bundle} into the profile source"))?;
    }
    Ok(())
}

/// Copy a directory tree keeping every file's modification time. Bundle
/// preflight refuses a manifest older than its source, so a copy that
/// restamped files in arbitrary order would change what the bundle proves.
fn copy_tree_preserving_mtimes(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination).with_context(|| format!("create {}", destination.display()))?;
    for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree_preserving_mtimes(&from, &to)?;
            continue;
        }
        fs::copy(&from, &to).with_context(|| format!("copy {}", from.display()))?;
        let modified = fs::metadata(&from)?.modified()?;
        fs::File::options()
            .write(true)
            .open(&to)
            .with_context(|| format!("open {}", to.display()))?
            .set_modified(modified)
            .with_context(|| format!("restore modification time of {}", to.display()))?;
    }
    Ok(())
}

/// The isolation mode (`enforce` or `disabled`) of the host class a
/// fast-fixture node at `state_path` runs under, read from its signed node
/// policy. Under `enforce` a launched tool runs in the sandbox; under
/// `disabled` it runs directly on the host with the node account's file
/// access, so tests that judge a tool's file reach branch on this.
pub fn node_isolation_mode(state_path: &Path) -> Result<String> {
    let path = state_path
        .join(AI_DIR)
        .join("node")
        .join("policies")
        .join("isolation.yaml");
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read node isolation policy {}", path.display()))?;
    let document: serde_json::Value =
        serde_yaml::from_str(&lillux::signature::strip_signature_lines(&raw))
            .with_context(|| format!("parse node isolation policy {}", path.display()))?;
    document
        .pointer(&format!(
            "/policy/host_classes/{FAST_FIXTURE_HOST_CLASS}/mode"
        ))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .with_context(|| {
            format!(
                "node isolation policy {} names no mode for the {FAST_FIXTURE_HOST_CLASS} class",
                path.display()
            )
        })
}

/// Derive the fixture isolation member for a node started directly by this
/// harness.
///
/// A direct `ryeosd` child has no protected host-runtime association: no host
/// service or external controller delegates a writable cgroup subtree to it,
/// so no per-scope capability (`limits:scope`, `recovery:scope`,
/// `termination:scope`, ...) can ever qualify and an authored profile that
/// requires process scopes is refused at startup by its capability floor.
/// The fixture keeps every other authored isolation field and removes only
/// the per-scope contract:
///
///   * `process_scopes` becomes `unconfigured`;
///   * every per-scope name leaves `requires_capabilities`, whether it is a
///     bare string or the `name` of an object entry (an `any_of` entry that
///     names only per-scope alternatives is refused rather than dropped;
///     one that keeps other alternatives keeps its `accept`/`providers`);
///   * `isolation:namespaces` leaves with them and `pid_namespace_nested`
///     proc narrows to `pid_namespace`: together they are the scoped nested
///     sandbox, which has no scope here to belong to.
///
/// The result is signed by the fixture node key and is never shipped. The
/// authored profiles keep their floors; tests of per-scope containment need
/// an administrator-prepared host-backed node, not this harness.
fn direct_host_isolation(isolation: &mut serde_json::Value) -> Result<()> {
    let policy = isolation
        .pointer_mut(&format!("/policy/host_classes/{FAST_FIXTURE_HOST_CLASS}"))
        .and_then(serde_json::Value::as_object_mut)
        .context("isolation member has no fast-fixture host class")?;
    let scoped = matches!(
        policy
            .get("process_scopes")
            .and_then(|scopes| scopes.get("mode"))
            .and_then(serde_json::Value::as_str),
        Some("required")
    );
    if !scoped {
        return Ok(());
    }
    policy.insert(
        "process_scopes".to_owned(),
        serde_json::json!({ "mode": "unconfigured" }),
    );
    let floor = policy
        .get_mut("requires_capabilities")
        .and_then(serde_json::Value::as_array_mut)
        .context("isolation policy has no requires_capabilities list")?;
    let scoped_only = |name: &str| {
        lillux::process_control::is_scope_capability(name)
            || name == ryeos_engine::isolation::ISOLATION_NAMESPACES
    };
    let mut retained = Vec::with_capacity(floor.len());
    for entry in floor.drain(..) {
        match &entry {
            serde_json::Value::String(name) => {
                if !scoped_only(name) {
                    retained.push(entry);
                }
            }
            serde_json::Value::Object(spec) => {
                if let Some(name) = spec.get("name").and_then(serde_json::Value::as_str) {
                    if !scoped_only(name) {
                        retained.push(entry);
                    }
                    continue;
                }
                let names = spec
                    .get("any_of")
                    .and_then(serde_json::Value::as_array)
                    .context("isolation floor entry names neither a name nor an any_of set")?;
                let kept = names
                    .iter()
                    .filter(|name| name.as_str().is_none_or(|name| !scoped_only(name)))
                    .cloned()
                    .collect::<Vec<_>>();
                anyhow::ensure!(
                    !kept.is_empty(),
                    "isolation floor entry {entry} can be met only by per-scope capabilities"
                );
                let mut narrowed = spec.clone();
                narrowed.insert("any_of".to_owned(), serde_json::Value::Array(kept));
                retained.push(serde_json::Value::Object(narrowed));
            }
            other => anyhow::bail!("isolation floor entry {other} has an unknown shape"),
        }
    }
    *floor = retained;
    if policy
        .get("proc_filesystem")
        .and_then(serde_json::Value::as_str)
        == Some("pid_namespace_nested")
    {
        policy.insert(
            "proc_filesystem".to_owned(),
            serde_json::Value::String("pid_namespace".to_owned()),
        );
    }
    let typed: ryeos_engine::isolation::IsolationPolicyDocument = serde_json::from_value(
        isolation
            .get("policy")
            .cloned()
            .context("isolation member has no policy mapping")?,
    )
    .context("parse direct-host fixture isolation policy")?;
    ryeos_engine::isolation::IsolationRuntime::validate_policy_document(&typed)
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("validate direct-host fixture isolation policy")?;
    Ok(())
}

/// Grant `bundle_name` an explicit (empty) command-registration authority in
/// the node policy. The policy generation is sealed by initialization, so a
/// bundle a test installs later must be granted here, before the seal.
pub fn authorize_fixture_bundle(
    state_path: &Path,
    bundle_name: &str,
    node: &SigningKey,
) -> Result<()> {
    let path = state_path
        .join(AI_DIR)
        .join("node")
        .join("policies")
        .join("command_registration.yaml");
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read test command-registration policy {}", path.display()))?;
    let body = lillux::signature::strip_signature_lines(&raw);
    let mut policy: serde_json::Value =
        serde_yaml::from_str(&body).context("parse test command-registration policy")?;
    let bundle_caps = policy
        .get_mut("bundle_source_caps")
        .and_then(serde_json::Value::as_object_mut)
        .context("test command-registration policy has no bundle_source_caps mapping")?;
    bundle_caps
        .entry(bundle_name.to_owned())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let body = serde_yaml::to_string(&policy).context("serialize test command policy")?;
    let signed = lillux::signature::sign_content_at(&body, node, "#", None, FAST_FIXTURE_TIME);
    fs::write(&path, signed).context("publish test command policy")
}
