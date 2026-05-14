//! Operator-side `ryeos init` (Model B) — bootstraps user space, node space,
//! pins the official publisher key into the operator's trust store, discovers
//! bundles from a source directory, installs them under
//! `<system_space_dir>/.ai/bundles/`, and writes signed registration records
//! at `<system_space_dir>/.ai/node/bundles/<name>.yaml`.
//!
//! # Bundle discovery
//!
//! `--source` points to a directory containing bundle subdirectories.
//! Each immediate child directory that contains a `.ai/` subdirectory
//! is recognized as a bundle. The bundle name is the directory name.
//!
//! Source layout (e.g. `/usr/share/ryeos`):
//! ```text
//! core/
//!   .ai/
//!     handlers/ parsers/ services/ tools/ config/ knowledge/
//!     node/engine/kinds/ node/verbs/ node/aliases/ node/routes/
//!     bin/<triple>/
//!     PUBLISHER_TRUST.toml
//! standard/
//!   .ai/
//!     ...same shape...
//! ```
//!
//! After init, installed at `<system_space_dir>/.ai/bundles/`:
//! ```text
//! core/.ai/...      ← copied from source/core/
//! standard/.ai/...  ← copied from source/standard/
//! ```
//!
//! The entire source bundle directory is copied as-is. Source bundles
//! are expected to contain only bundle content (handlers, parsers, kinds,
//! tools, config, knowledge, binaries). Runtime-only state directories
//! (`state/objects/`, `state/refs/`, `node/identity/`, `node/vault/`,
//! `node/bundles/`) are never present in source bundles and are not
//! created by init — they belong to the system space runtime layout.
//!
//! Directories that are NOT immediate children of `--source`, or that
//! lack a `.ai/` subdirectory, are silently skipped. Hidden directories
//! (starting with `.`) are also skipped.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use lillux::crypto::{
    DecodePrivateKey, EncodePrivateKey, SigningKey, VerifyingKey,
};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

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
    /// Source directory containing one or more bundle subdirectories.
    /// Each immediate child that contains a `.ai/` directory is a bundle;
    /// the bundle name is its directory name.
    ///
    /// Examples:
    ///   - `/usr/share/ryeos` (packaged install)
    ///   - `ryeos-bundles` (dev tree)
    ///   - `/opt/ryeos` (docker)
    pub source_dir: PathBuf,
    /// Additional PUBLISHER_TRUST.toml files to pin before verifying bundles.
    /// Each file contains `public_key`, `fingerprint`, and `owner` fields.
    pub trust_files: Vec<PathBuf>,
    /// Skip preflight verification of source bundles (trust + signatures).
    /// Used in dev/test when source bundles are not yet signed and populated.
    /// DO NOT expose this as a CLI flag — production installs always verify.
    pub skip_preflight: bool,
}

#[derive(Debug, Serialize)]
pub struct InitReport {
    pub system_space_dir: PathBuf,
    pub user_key_fingerprint: String,
    pub node_key_fingerprint: String,
    pub official_publisher_pinned: String,
    /// SHA-256 fingerprint of the X25519 vault public key. Surfaced
    /// so operators can sanity-check that subsequent vault writes are
    /// being sealed to the right key (and so audit logs can pin it).
    pub vault_pubkey_fingerprint: String,
    /// Names of bundles discovered and installed from `source_dir`.
    pub bundles_installed: Vec<String>,
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
///   6. Discover bundles in `source_dir` — scan for child dirs containing `.ai/`
///   7. Install each discovered bundle + write registration record
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
    let node_key = load_or_create_key(&node_key_path, false)
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

    // ── 6. Discover bundles in source_dir ──
    let discovered = discover_bundles(&opts.source_dir)?;
    if discovered.is_empty() {
        bail!(
            "no bundles found in {} — expected immediate child directories \
             containing a `.ai/` subdirectory",
            opts.source_dir.display()
        );
    }
    tracing::info!(
        source = %opts.source_dir.display(),
        bundles = ?discovered.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        "discovered bundles"
    );

    // ── 6b. Validate bundle manifest dependencies ──
    validate_manifest_dependencies(&discovered)
        .context("bundle manifest dependency check")?;

    // ── 7. Install each bundle (atomic stage → swap) ──
    let mut bundles_installed = Vec::new();
    for (name, source_path) in &discovered {
        let target = opts
            .system_space_dir
            .join(ryeos_engine::AI_DIR)
            .join("bundles")
            .join(name);

        if target.exists() {
            // Bundle already installed — replace atomically.
            verify_bundle_structure(&target)?;

            if !opts.skip_preflight {
                crate::actions::install::preflight_verify_bundle(
                    source_path,
                    &opts.system_space_dir,
                    Some(opts.user_root.as_path()),
                )
                .with_context(|| format!("verify {} source against pinned publisher key", name))?;
            }

            replace_bundle(source_path, &target).with_context(|| {
                format!(
                    "atomic replace {}: {} -> {}",
                    name,
                    source_path.display(),
                    target.display()
                )
            })?;

            ensure_node_bundle_registration(
                &opts.system_space_dir,
                name,
                &target.canonicalize()?,
                &node_key,
            )
            .with_context(|| format!("verify/recreate node/bundles/{}.yaml", name))?;
        } else {
            install_bundle(
                &opts.system_space_dir,
                name,
                source_path,
                &node_key,
                &opts.system_space_dir,
                opts.user_root.as_path(),
                opts.skip_preflight,
            )?;
        }

        bundles_installed.push(name.clone());
    }

    // ── 8. Vault X25519 keypair ──
    let vault_dir = opts.system_space_dir.join(ryeos_engine::AI_DIR).join("node").join("vault");
    fs::create_dir_all(&vault_dir)
        .with_context(|| format!("create vault dir {}", vault_dir.display()))?;
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

    let next_steps = vec![
        format!(
            "Start the daemon: ryeosd --system-space-dir {}",
            opts.system_space_dir.display()
        ),
        "Try a verb: ryeos status".to_string(),
    ];

    Ok(InitReport {
        system_space_dir: opts.system_space_dir.clone(),
        user_key_fingerprint: user_fp,
        node_key_fingerprint: node_fp,
        official_publisher_pinned: OFFICIAL_PUBLISHER_FP.to_string(),
        vault_pubkey_fingerprint: vault_sk.public_key().fingerprint(),
        bundles_installed,
        next_steps,
    })
}

/// Discover bundles in a source directory.
///
/// Scans immediate children of `source_dir` for directories containing
/// a `.ai/` subdirectory. Hidden directories (starting with `.`) and
/// names that don't pass [`is_valid_bundle_name`] are skipped.
/// Returns `(name, source_path)` pairs sorted by name for
/// deterministic registration order.
fn discover_bundles(source_dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    if !source_dir.is_dir() {
        bail!("source directory does not exist: {}", source_dir.display());
    }

    let mut bundles = Vec::new();
    let entries = fs::read_dir(source_dir)
        .with_context(|| format!("read source directory {}", source_dir.display()))?;

    for entry in entries {
        let entry = entry.context("read source dir entry")?;
        let file_type = entry.file_type().context("read source dir entry type")?;
        if !file_type.is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden directories (e.g. .staging, .git)
        if name_str.starts_with('.') {
            continue;
        }

        // Skip names that don't meet bundle naming rules
        if !is_valid_bundle_name(&name_str) {
            tracing::warn!(
                name = %name_str,
                "skipping directory with invalid bundle name (must be lowercase alphanumeric, underscore, or hyphen, 1–64 chars)"
            );
            continue;
        }

        let child_path = entry.path();
        if child_path.join(ryeos_engine::AI_DIR).is_dir() {
            bundles.push((name_str.into_owned(), child_path));
        }
    }

    bundles.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(bundles)
}

/// Check whether a bundle name is valid.
///
/// Rules: 1–64 chars, lowercase ASCII letters, digits, underscore, or hyphen.
/// This must match the validation in `bundle_install` and `bundle_remove`
/// service handlers so that any name discoverable by init can also be
/// managed via the service endpoints.
pub fn is_valid_bundle_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

// ── Bundle manifest (v2: generated + signed) ───────────────────────

/// Hand-authored manifest source, written by bundle authors.
///
/// Located at `<bundle>/.ai/manifest.source.yaml`. Contains only fields
/// that humans must provide — the system derives `provides_kinds` from
/// actual kind schemas on disk.
///
/// The publish pipeline reads this, derives provides_kinds, and generates
/// the final signed `.ai/manifest.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifestSource {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub requires_kinds: Vec<String>,
    // NO provides_kinds — derived from actual content by the system.
}

/// Generated bundle manifest, produced by the publish pipeline.
///
/// Located at `<bundle>/.ai/manifest.yaml` (signed with `# ryeos:signed:...`).
/// `provides_kinds` is auto-derived from kind schemas present in
/// `.ai/node/engine/kinds/`. The full manifest is signed by the publisher
/// and verified during preflight.
///
/// Bundles without a manifest (no `manifest.source.yaml`) still install
/// and function — they're treated as providing/requiring nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub provides_kinds: Vec<String>,
    #[serde(default)]
    pub requires_kinds: Vec<String>,
}

/// Derive `provides_kinds` from actual kind schemas on disk.
///
/// Scans `<ai_dir>/node/engine/kinds/<name>/<name>.kind-schema.yaml`.
/// If the schema file exists, the kind is provided. Returns sorted, deduped.
pub fn derive_provides_kinds(ai_dir: &Path) -> Result<Vec<String>> {
    let kinds_dir = ai_dir.join("node").join("engine").join("kinds");
    if !kinds_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut kinds: Vec<String> = Vec::new();
    for entry in fs::read_dir(&kinds_dir)
        .with_context(|| format!("read kinds dir {}", kinds_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let schema = kinds_dir.join(&name).join(format!("{name}.kind-schema.yaml"));
        if schema.exists() {
            kinds.push(name);
        }
    }
    kinds.sort();
    kinds.dedup();
    Ok(kinds)
}

/// Materialize a full `BundleManifest` from a source + actual content.
///
/// Validates identity (name matches expected) and derives `provides_kinds`
/// from kind schemas on disk. Used by both publish pipeline and dev-mode
/// fallback — ensures `provides_kinds` is always derived, never defaulted.
pub fn materialize_manifest(
    source: BundleManifestSource,
    ai_dir: &Path,
    expected_name: &str,
) -> Result<BundleManifest> {
    if source.name != expected_name {
        bail!(
            "manifest identity mismatch: source.name is '{}' but expected '{}' — \
             update manifest.source.yaml name to match the directory",
            source.name,
            expected_name
        );
    }
    let provides_kinds = derive_provides_kinds(ai_dir)?;
    Ok(BundleManifest {
        name: source.name,
        version: source.version,
        description: source.description,
        provides_kinds,
        requires_kinds: source.requires_kinds,
    })
}

/// Parse a bundle manifest from a source directory.
///
/// Tries in order:
/// 1. `.ai/manifest.yaml` — generated + signed (from publish pipeline).
///    Strips signature header, parses body.
/// 2. `.ai/manifest.source.yaml` — hand-authored source (dev mode).
///    Parses source, materializes provides_kinds from actual content.
/// 3. Neither exists — returns `None` (manifests are optional).
///
/// In all cases, validates identity (`name` matches `expected_name`).
pub fn parse_manifest(source: &Path, expected_name: &str) -> Result<Option<BundleManifest>> {
    let ai_dir = source.join(".ai");

    // 1. Try generated + signed manifest (published mode)
    let manifest_path = ai_dir.join("manifest.yaml");
    if manifest_path.exists() {
        let raw = fs::read_to_string(&manifest_path)
            .with_context(|| format!("read manifest {}", manifest_path.display()))?;
        let body = lillux::signature::strip_signature_lines(&raw);
        let manifest: BundleManifest = serde_yaml::from_str(&body)
            .with_context(|| format!("parse manifest {}", manifest_path.display()))?;
        if manifest.name != expected_name {
            bail!(
                "manifest identity mismatch: manifest.yaml name is '{}' but expected '{}' — \
                 regenerate the manifest",
                manifest.name,
                expected_name
            );
        }
        return Ok(Some(manifest));
    }

    // 2. Fallback: dev mode — materialize from source + actual content
    let source_path = ai_dir.join("manifest.source.yaml");
    if source_path.exists() {
        let raw = fs::read_to_string(&source_path)
            .with_context(|| format!("read manifest source {}", source_path.display()))?;
        let src: BundleManifestSource = serde_yaml::from_str(&raw)
            .with_context(|| format!("parse manifest source {}", source_path.display()))?;
        let manifest = materialize_manifest(src, &ai_dir, expected_name)?;
        return Ok(Some(manifest));
    }

    // 3. No manifest — optional
    Ok(None)
}

/// Validate that the discovered bundles' kind dependencies are satisfiable.
///
/// For every bundle that declares `requires_kinds`, each required kind must
/// appear in at least one other (or the same) bundle's `provides_kinds`.
///
/// Returns `Ok(())` if all dependencies are satisfied, or an error listing
/// the unsatisfied kinds and which bundles need them.
fn validate_manifest_dependencies(
    bundles: &[(String, PathBuf)],
) -> Result<()> {
    // Collect manifests
    let mut manifests: Vec<(String, Option<BundleManifest>)> = Vec::new();
    for (name, path) in bundles {
        let mf = parse_manifest(path, name)
            .with_context(|| format!("parse manifest for bundle {}", name))?;
        manifests.push((name.clone(), mf));
    }

    // Union of all provides_kinds
    let mut all_provides: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for (_, mf) in &manifests {
        if let Some(m) = mf {
            for k in &m.provides_kinds {
                all_provides.insert(k.clone());
            }
        }
    }

    // Check every requires_kinds is covered
    let mut missing: Vec<(String, Vec<String>)> = Vec::new(); // (bundle_name, missing_kinds)
    for (name, mf) in &manifests {
        let Some(m) = mf else { continue };
        let mut unsatisfied: Vec<String> = Vec::new();
        for req in &m.requires_kinds {
            if !all_provides.contains(req) {
                unsatisfied.push(req.clone());
            }
        }
        if !unsatisfied.is_empty() {
            missing.push((name.clone(), unsatisfied));
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    let mut msg = "bundle dependency check failed:\n".to_string();
    for (name, kinds) in &missing {
        msg.push_str(&format!(
            "  bundle '{}' requires kinds not provided by any bundle: {}\n",
            name,
            kinds.join(", ")
        ));
    }
    msg.push_str(&format!(
        "\n  all provided kinds across bundles: {}",
        all_provides.iter().cloned().collect::<Vec<_>>().join(", ")
    ));
    bail!("{}", msg)
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

/// Atomically replace an installed bundle with a new version.
///
/// Instead of copying on top (which leaves stale files), this:
/// 1. Copies source to a staging directory
/// 2. Moves old bundle to `.backup.prev`
/// 3. Moves staging to final location
///
/// If step 3 fails, the old bundle is still at `.backup.prev` for recovery.
/// One previous generation is kept for rollback; older backups are cleaned up.
fn replace_bundle(source: &Path, target: &Path) -> Result<()> {
    let parent = target.parent().ok_or_else(|| anyhow!("bundle path has no parent"))?;
    let name = target
        .file_name()
        .ok_or_else(|| anyhow!("bundle path has no name"))?
        .to_string_lossy();

    let staging = parent.join(format!(".{name}.staging"));
    let backup = parent.join(format!("{name}.backup.prev"));

    // Clean up any leftover staging from a previous failed attempt
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .with_context(|| format!("clean up stale staging {}", staging.display()))?;
    }

    // 1. Copy source to staging
    copy_dir_recursive(source, &staging)
        .with_context(|| format!("stage {} -> {}", source.display(), staging.display()))?;

    // 2. Move old to backup (one generation)
    if target.exists() {
        if backup.exists() {
            // Clean up previous backup
            fs::remove_dir_all(&backup)
                .with_context(|| format!("remove old backup {}", backup.display()))?;
        }
        fs::rename(target, &backup)
            .with_context(|| format!("backup {} -> {}", target.display(), backup.display()))?;
    }

    // 3. Move staging to final
    fs::rename(&staging, target)
        .with_context(|| format!("swap {} -> {}", staging.display(), target.display()))?;

    Ok(())
}

/// Install a bundle by copy + signed `kind: node` registration.
///
/// Mirrors `service:bundle/install` semantics but runs in-process (no daemon
/// required). The official publisher trust must already be pinned so
/// preflight verification passes.
///
/// Returns the canonical path of the installed bundle.
fn install_bundle(
    system_space_dir: &Path,
    name: &str,
    source: &Path,
    node_key: &SigningKey,
    system_space_dir_for_kinds: &Path,
    user_root: &Path,
    skip_preflight: bool,
) -> Result<PathBuf> {
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

    Ok(canonical)
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

    fn make_opts(state: &Path, user: &Path) -> InitOptions {
        InitOptions {
            system_space_dir: state.to_path_buf(),
            user_root: user.to_path_buf(),
            source_dir: workspace_root().join("ryeos-bundles"),
            trust_files: vec![dev_trust_file()],
            skip_preflight: true,
        }
    }

    #[test]
    fn run_installs_discovered_bundles() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let report = run_init(&make_opts(&state, &user)).expect("init");
        assert_eq!(report.official_publisher_pinned, OFFICIAL_PUBLISHER_FP);

        // Both bundles discovered and installed
        assert!(
            report.bundles_installed.contains(&"core".to_string()),
            "core must be in installed list: {:?}",
            report.bundles_installed
        );
        assert!(
            report.bundles_installed.contains(&"standard".to_string()),
            "standard must be in installed list: {:?}",
            report.bundles_installed
        );

        // Core at .ai/bundles/core/.ai/
        assert!(
            state.join(".ai/bundles/core/.ai").is_dir(),
            "core should be installed at .ai/bundles/core/.ai/"
        );
        // Standard at .ai/bundles/standard/.ai/
        assert!(
            state.join(".ai/bundles/standard/.ai").is_dir(),
            "standard should be installed at .ai/bundles/standard/.ai/"
        );
        // Registrations
        assert!(state.join(".ai/node/bundles/core.yaml").exists());
        assert!(state.join(".ai/node/bundles/standard.yaml").exists());
        // Kind schemas inside core
        assert!(
            state.join(".ai/bundles/core/.ai/node/engine/kinds").is_dir(),
            "core kind schemas must be inside the installed bundle"
        );
        assert!(state.join(".ai/node/identity/private_key.pem").exists());
        assert!(state.join(".ai/node/vault").is_dir());
        assert!(user.join(".ai/config/keys/signing/private_key.pem").exists());
    }

    #[test]
    fn run_init_creates_keys_and_pins_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let report = run_init(&make_opts(&state, &user)).expect("init");
        assert_eq!(report.official_publisher_pinned, OFFICIAL_PUBLISHER_FP);
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
        let report = run_init(&make_opts(&state, &user)).expect("init");
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
        let opts = make_opts(&state, &user);
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
        let opts = make_opts(&state, &user);
        let r1 = run_init(&opts).expect("init #1");
        let r2 = run_init(&opts).expect("init #2");
        assert_eq!(r1.user_key_fingerprint, r2.user_key_fingerprint);
        assert_eq!(r1.node_key_fingerprint, r2.node_key_fingerprint);
        assert_eq!(r1.bundles_installed, r2.bundles_installed);
    }

    #[test]
    fn discover_bundles_finds_core_and_standard() {
        let bundles = discover_bundles(&workspace_root().join("ryeos-bundles")).unwrap();
        let names: Vec<&str> = bundles.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"core"), "must find core: {:?}", names);
        assert!(names.contains(&"standard"), "must find standard: {:?}", names);
    }

    #[test]
    fn discover_bundles_skips_hidden_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a hidden dir with .ai/ — should be skipped
        let hidden = tmp.path().join(".hidden");
        fs::create_dir_all(hidden.join(".ai")).unwrap();
        // Create a valid bundle
        let valid = tmp.path().join("my-bundle");
        fs::create_dir_all(valid.join(".ai")).unwrap();

        let bundles = discover_bundles(tmp.path()).unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].0, "my-bundle");
    }

    #[test]
    fn discover_bundles_skips_non_bundle_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        // Dir without .ai/ — not a bundle
        fs::create_dir_all(tmp.path().join("not-a-bundle")).unwrap();
        // Valid bundle
        let valid = tmp.path().join("real-bundle");
        fs::create_dir_all(valid.join(".ai")).unwrap();

        let bundles = discover_bundles(tmp.path()).unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].0, "real-bundle");
    }

    #[test]
    fn discover_bundles_fails_on_missing_dir() {
        let result = discover_bundles(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }

    #[test]
    fn discover_bundles_skips_invalid_names() {
        let tmp = tempfile::tempdir().unwrap();
        // Invalid names
        for invalid in &["has.dot", "UPPER", "has space", "has/slash"] {
            fs::create_dir_all(tmp.path().join(invalid).join(".ai")).unwrap();
        }
        // Valid name
        fs::create_dir_all(tmp.path().join("valid-bundle").join(".ai")).unwrap();

        let bundles = discover_bundles(tmp.path()).unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].0, "valid-bundle");
    }

    #[test]
    fn valid_bundle_name_rules() {
        assert!(is_valid_bundle_name("core"));
        assert!(is_valid_bundle_name("standard"));
        assert!(is_valid_bundle_name("my-bundle_v2"));
        assert!(is_valid_bundle_name("a"));
        assert!(!is_valid_bundle_name(""));
        assert!(!is_valid_bundle_name("has.dot"));
        assert!(!is_valid_bundle_name("UPPER"));
        assert!(!is_valid_bundle_name("has space"));
        assert!(!is_valid_bundle_name(&"x".repeat(65)));
    }

    // ── Manifest tests ──────────────────────────────────────────────

    #[test]
    fn parse_manifest_reads_core() {
        let mf = parse_manifest(&workspace_root().join("ryeos-bundles/core"), "core")
            .expect("parse core manifest")
            .expect("core has a manifest");
        assert_eq!(mf.name, "core");
        assert_eq!(mf.version, "0.5.0");
        assert!(!mf.provides_kinds.is_empty());
        assert!(mf.provides_kinds.contains(&"config".to_string()));
        assert!(mf.provides_kinds.contains(&"handler".to_string()));
        assert!(mf.provides_kinds.contains(&"parser".to_string()));
        assert!(mf.provides_kinds.contains(&"runtime".to_string()));
        assert!(mf.provides_kinds.contains(&"service".to_string()));
        assert!(mf.provides_kinds.contains(&"tool".to_string()));
        assert!(mf.requires_kinds.is_empty(), "core should have no requires");
    }

    #[test]
    fn parse_manifest_reads_standard() {
        let mf = parse_manifest(&workspace_root().join("ryeos-bundles/standard"), "standard")
            .expect("parse standard manifest")
            .expect("standard has a manifest");
        assert_eq!(mf.name, "standard");
        assert!(mf.provides_kinds.contains(&"directive".to_string()));
        assert!(mf.provides_kinds.contains(&"graph".to_string()));
        assert!(mf.provides_kinds.contains(&"knowledge".to_string()));
        assert!(
            mf.requires_kinds.contains(&"config".to_string()),
            "standard requires config from core"
        );
        assert!(
            mf.requires_kinds.contains(&"handler".to_string()),
            "standard requires handler from core"
        );
    }

    #[test]
    fn parse_manifest_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("no-manifest");
        fs::create_dir_all(bundle.join(".ai")).unwrap();
        assert!(parse_manifest(&bundle, "no-manifest").unwrap().is_none());
    }

    #[test]
    fn parse_manifest_rejects_invalid_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bad-manifest");
        fs::create_dir_all(bundle.join(".ai")).unwrap();
        fs::write(bundle.join(".ai/manifest.source.yaml"), "not: [valid\nyaml").unwrap();
        assert!(parse_manifest(&bundle, "bad-manifest").is_err());
    }

    #[test]
    fn validate_dependencies_core_and_standard_ok() {
        let bundles = discover_bundles(&workspace_root().join("ryeos-bundles")).unwrap();
        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "core + standard should satisfy all dependencies"
        );
    }

    #[test]
    fn validate_dependencies_fails_with_missing_provider() {
        let tmp = tempfile::tempdir().unwrap();

        // Create a bundle that requires "magic" kind — nothing provides it
        let needy = tmp.path().join("needy");
        fs::create_dir_all(needy.join(".ai")).unwrap();
        fs::write(
            needy.join(".ai/manifest.source.yaml"),
            "name: needy\nversion: '1.0'\nrequires_kinds:\n  - magic\n",
        )
        .unwrap();

        let bundles = vec![("needy".to_string(), needy)];
        let err = validate_manifest_dependencies(&bundles).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("magic"),
            "error should mention missing kind 'magic': {msg}"
        );
        assert!(
            msg.contains("needy"),
            "error should mention bundle 'needy': {msg}"
        );
    }

    #[test]
    fn validate_dependencies_skips_bundles_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        // No manifest.yaml — should be fine
        let bare = tmp.path().join("bare");
        fs::create_dir_all(bare.join(".ai")).unwrap();

        let bundles = vec![("bare".to_string(), bare)];
        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "bundles without manifests should pass"
        );
    }

    #[test]
    fn validate_dependencies_self_provide_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        // Bundle provides and requires the same kind — self-sufficient
        let selfish = tmp.path().join("selfish");
        fs::create_dir_all(selfish.join(".ai/node/engine/kinds/foo")).unwrap();
        fs::write(
            selfish.join(".ai/node/engine/kinds/foo/foo.kind-schema.yaml"),
            "kind: config\ndirectory: foo\nextensions: []\n",
        )
        .unwrap();
        fs::write(
            selfish.join(".ai/manifest.source.yaml"),
            "name: selfish\nversion: '1.0'\nrequires_kinds:\n  - foo\n",
        )
        .unwrap();

        let bundles = vec![("selfish".to_string(), selfish)];
        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "self-providing bundle should pass"
        );
    }

    #[test]
    fn validate_dependencies_cross_bundle_satisfies() {
        let tmp = tempfile::tempdir().unwrap();

        // Provider bundle — has a kind schema for "alpha"
        let provider = tmp.path().join("provider");
        fs::create_dir_all(provider.join(".ai/node/engine/kinds/alpha")).unwrap();
        fs::write(
            provider.join(".ai/node/engine/kinds/alpha/alpha.kind-schema.yaml"),
            "kind: config\ndirectory: alpha\nextensions: []\n",
        )
        .unwrap();
        fs::write(
            provider.join(".ai/manifest.source.yaml"),
            "name: provider\nversion: '1.0'\nrequires_kinds: []\n",
        )
        .unwrap();

        // Consumer bundle
        let consumer = tmp.path().join("consumer");
        fs::create_dir_all(consumer.join(".ai")).unwrap();
        fs::write(
            consumer.join(".ai/manifest.source.yaml"),
            "name: consumer\nversion: '1.0'\nrequires_kinds:\n  - alpha\n",
        )
        .unwrap();

        let bundles = vec![
            ("consumer".to_string(), consumer),
            ("provider".to_string(), provider),
        ];
        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "cross-bundle dependency should be satisfied"
        );
    }

    // ── Follow-up tests (2.1, 2.2, 2.3) ───────────────────────────

    #[test]
    fn manifest_name_must_match_bundle_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("real-name");
        fs::create_dir_all(bundle.join(".ai")).unwrap();
        fs::write(
            bundle.join(".ai/manifest.source.yaml"),
            "name: wrong-name\nversion: '1.0'\nrequires_kinds: []\n",
        )
        .unwrap();

        let err = parse_manifest(&bundle, "real-name").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("mismatch"),
            "error should mention mismatch: {msg}"
        );
        assert!(
            msg.contains("real-name") && msg.contains("wrong-name"),
            "error should name both: {msg}"
        );
    }

    #[test]
    fn manifest_rejects_unknown_fields() {
        let yaml = r#"
name: test
version: "1.0"
provides_kinds: []
requires_kinds: []
typo_field: oops
"#;
        let result: Result<BundleManifest, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "unknown field should be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unknown field"),
            "error should mention unknown field: {msg}"
        );
    }

    #[test]
    fn run_init_aborts_before_install_on_unsatisfied_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");

        // Create a source dir with a bundle that requires an unsatisfied kind
        let source = tmp.path().join("source");
        let needy = source.join("needy");
        fs::create_dir_all(needy.join(".ai")).unwrap();
        // Need a PUBLISHER_TRUST.toml for the bundle (copy from dev keys)
        let dev_trust = workspace_root().join(".dev-keys/PUBLISHER_DEV_TRUST.toml");
        if dev_trust.exists() {
            fs::copy(&dev_trust, needy.join("PUBLISHER_TRUST.toml")).unwrap();
        }
        fs::write(
            needy.join(".ai/manifest.source.yaml"),
            "name: needy\nversion: '1.0'\nrequires_kinds:\n  - nonexistent-kind\n",
        )
        .unwrap();

        let opts = InitOptions {
            system_space_dir: state.clone(),
            user_root: user,
            source_dir: source,
            trust_files: vec![],
            skip_preflight: true,
        };

        let result = run_init(&opts);
        assert!(result.is_err(), "init should fail with unsatisfied deps");
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("nonexistent-kind"),
            "error should mention the missing kind: {err_msg}"
        );

        // Critical: no bundles should be installed (non-mutating failure)
        let bundles_dir = state.join(".ai/bundles");
        if bundles_dir.exists() {
            let installed: Vec<_> = fs::read_dir(&bundles_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert!(
                installed.is_empty(),
                "no bundles should be installed on dep failure, found: {:?}",
                installed.iter().map(|e| e.path().display().to_string()).collect::<Vec<_>>()
            );
        }

        // No registrations written
        let regs_dir = state.join(".ai/node/bundles");
        if regs_dir.exists() {
            let regs: Vec<_> = fs::read_dir(&regs_dir)
                .unwrap()
                .filter_map(|e| {
                    let e = e.ok()?;
                    if e.path().extension().map(|ext| ext == "yaml").unwrap_or(false) {
                        Some(e)
                    } else {
                        None
                    }
                })
                .collect();
            assert!(
                regs.is_empty(),
                "no registrations should exist on dep failure"
            );
        }
    }

    // ── v2 tests: derive, materialize, source split ────────────────

    #[test]
    fn derive_provides_kinds_scans_core_schemas() {
        let ai_dir = workspace_root().join("ryeos-bundles/core/.ai");
        let kinds = derive_provides_kinds(&ai_dir).expect("derive core provides_kinds");
        assert!(
            kinds.contains(&"config".to_string()),
            "core must provide config: {kinds:?}"
        );
        assert!(
            kinds.contains(&"handler".to_string()),
            "core must provide handler: {kinds:?}"
        );
        assert!(
            kinds.contains(&"tool".to_string()),
            "core must provide tool: {kinds:?}"
        );
        // Standard kinds are NOT in core
        assert!(
            !kinds.contains(&"directive".to_string()),
            "directive is a standard kind, not core: {kinds:?}"
        );
    }

    #[test]
    fn derive_provides_kinds_scans_standard_schemas() {
        let ai_dir = workspace_root().join("ryeos-bundles/standard/.ai");
        let kinds = derive_provides_kinds(&ai_dir).expect("derive standard provides_kinds");
        assert!(
            kinds.contains(&"directive".to_string()),
            "standard must provide directive: {kinds:?}"
        );
        assert!(
            kinds.contains(&"graph".to_string()),
            "standard must provide graph: {kinds:?}"
        );
        assert!(
            kinds.contains(&"knowledge".to_string()),
            "standard must provide knowledge: {kinds:?}"
        );
    }

    #[test]
    fn derive_provides_kinds_returns_empty_without_kinds_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let kinds = derive_provides_kinds(tmp.path()).unwrap();
        assert!(kinds.is_empty());
    }

    #[test]
    fn materialize_manifest_derives_provides_from_schemas() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("test-bundle");
        let ai_dir = bundle.join(".ai");
        fs::create_dir_all(ai_dir.join("node/engine/kinds/mykind")).unwrap();
        fs::write(
            ai_dir.join("node/engine/kinds/mykind/mykind.kind-schema.yaml"),
            "kind: config\ndirectory: mykind\nextensions: []\n",
        )
        .unwrap();

        let source = BundleManifestSource {
            name: "test-bundle".to_string(),
            version: "1.0".to_string(),
            description: "test".to_string(),
            requires_kinds: vec![],
        };
        let manifest = materialize_manifest(source, &ai_dir, "test-bundle").unwrap();
        assert_eq!(manifest.provides_kinds, vec!["mykind"]);
        assert_eq!(manifest.name, "test-bundle");
    }

    #[test]
    fn parse_manifest_dev_mode_materializes_from_source() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("dev-bundle");
        let ai_dir = bundle.join(".ai");
        fs::create_dir_all(ai_dir.join("node/engine/kinds/custom")).unwrap();
        fs::write(
            ai_dir.join("node/engine/kinds/custom/custom.kind-schema.yaml"),
            "kind: config\ndirectory: custom\nextensions: []\n",
        )
        .unwrap();
        // No .ai/manifest.yaml (not published), only source
        fs::write(
            ai_dir.join("manifest.source.yaml"),
            "name: dev-bundle\nversion: '0.1'\ndescription: 'dev test'\nrequires_kinds: []\n",
        )
        .unwrap();

        let mf = parse_manifest(&bundle, "dev-bundle")
            .unwrap()
            .expect("should find manifest via source fallback");
        assert_eq!(mf.name, "dev-bundle");
        assert_eq!(mf.provides_kinds, vec!["custom"]);
        assert!(mf.requires_kinds.is_empty());
    }

    #[test]
    fn parse_manifest_prefers_generated_over_source() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("pub-bundle");
        let ai_dir = bundle.join(".ai");
        fs::create_dir_all(&ai_dir).unwrap();
        // Generated manifest (simulates publish output — not signed here, just the body)
        fs::write(
            ai_dir.join("manifest.yaml"),
            "name: pub-bundle\nversion: '2.0'\nprovides_kinds:\n  - published-kind\nrequires_kinds: []\n",
        )
        .unwrap();
        // Source exists too but should be ignored
        fs::write(
            ai_dir.join("manifest.source.yaml"),
            "name: pub-bundle\nversion: '1.0'\ndescription: 'old source'\nrequires_kinds: []\n",
        )
        .unwrap();

        let mf = parse_manifest(&bundle, "pub-bundle")
            .unwrap()
            .expect("should find manifest");
        assert_eq!(mf.version, "2.0", "should read generated, not source");
        assert_eq!(mf.provides_kinds, vec!["published-kind"]);
    }

    #[test]
    fn source_rejects_unknown_fields() {
        let yaml = r#"
name: test
version: "1.0"
typo_field: oops
"#;
        let result: Result<BundleManifestSource, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "unknown field in source should be rejected");
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }
}
