//! Operator-side `ryeos init` (Model B) — bootstraps operator config, node space,
//! pins the official publisher key into the operator's trust store, discovers
//! bundles from a source directory, installs them under
//! `<app_root>/.ai/bundles/`, and writes signed registration records
//! at `<app_root>/.ai/node/bundles/<name>.yaml`.
//!
//! # Bundle discovery
//!
//! `--source` points to a directory containing bundle subdirectories.
//! Each immediate child directory that contains a `.ai/` subdirectory
//! is recognized as a bundle. The bundle name is the directory name.
//!
//! Source layout (e.g. `/usr/share/ryeos`):
//! ```text
//! .ai/
//!   PUBLISHER_TRUST.toml
//!   node/init/command-registration/default.yaml
//!   node/init/bundle-registration-grants/default.yaml
//! core/
//!   .ai/
//!     handlers/ parsers/ services/ tools/ config/ knowledge/
//!     node/engine/kinds/ node/commands/ node/routes/
//!     bin/<triple>/
//!     PUBLISHER_TRUST.toml
//! standard/
//!   .ai/
//!     ...same shape...
//! ```
//!
//! After init, installed at `<app_root>/.ai/bundles/`:
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
//! created by init — they belong to the app-root runtime layout.
//!
//! Directories that are NOT immediate children of `--source`, or that
//! lack a `.ai/` subdirectory, are silently skipped. Hidden directories
//! (starting with `.`) are also skipped.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use lillux::crypto::{
    DecodePrivateKey, EncodePrivateKey, Signature, Signer, SigningKey, Verifier, VerifyingKey,
};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use ryeos_engine::contracts::{SignatureEnvelope, TrustClass};
use ryeos_engine::trust::{compute_fingerprint, pin_key, TrustStore};

mod bundle_install;
mod default_policy;

#[cfg(test)]
pub(crate) use bundle_install::copy_dir_recursive;
use bundle_install::{
    bundle_registration_value, install_bundle, replace_bundle, verify_bundle_structure,
};
use default_policy::materialize_node_defaults;

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
    0xe7, 0x68, 0x9b, 0x49, 0x7f, 0xd5, 0x92, 0x57, 0x10, 0x2b, 0x97, 0x86, 0x68, 0x2d, 0x74, 0x10,
    0xb4, 0x35, 0xf2, 0x1b, 0x16, 0x81, 0x44, 0x2d, 0x3b, 0xfb, 0x4a, 0xcd, 0xe6, 0x25, 0x36, 0x03,
];

#[derive(Debug)]
pub struct InitOptions {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    /// Contains operator config, mutable node state, and installed bundle
    /// content — there is no separate user-space tier.
    pub app_root: PathBuf,
    /// Source directory containing one or more bundle subdirectories.
    /// Each immediate child that contains a `.ai/` directory is a bundle;
    /// the bundle name is its directory name.
    ///
    /// Examples:
    ///   - `/usr/share/ryeos` (packaged install)
    ///   - `bundles` (dev tree)
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitOperatorProfile {
    pub display_name: Option<String>,
    pub identity_statement: Option<String>,
}

pub struct InitOperatorCeremony {
    pub profile: InitOperatorProfile,
    pub entropy_contribution: Option<Zeroizing<Vec<u8>>>,
}

impl std::fmt::Debug for InitOperatorCeremony {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InitOperatorCeremony")
            .field("profile", &self.profile)
            .field("entropy_contribution", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Serialize)]
pub struct InitReport {
    pub app_root: PathBuf,
    pub user_key_fingerprint: String,
    pub node_key_fingerprint: String,
    pub official_publisher_pinned: String,
    /// SHA-256 fingerprint of the X25519 vault public key. Surfaced
    /// so operators can sanity-check that subsequent vault writes are
    /// being sealed to the right key (and so audit logs can pin it).
    pub vault_pubkey_fingerprint: String,
    /// Names of bundles discovered and installed from `source_dir`.
    pub bundles_installed: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
}

const INIT_COMPLETION_SCHEMA: &str = "ryeos/init-completion/v1";
const INIT_COMPLETION_MAX_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitCompletionBody {
    schema: String,
    operator_fingerprint: String,
    node_fingerprint: String,
    vault_fingerprint: String,
    registration_digests: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitCompletionDocument {
    body: InitCompletionBody,
    signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitCompletionReport {
    pub operator_fingerprint: String,
    pub node_fingerprint: String,
    pub vault_fingerprint: String,
    pub bundles_verified: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitPhase {
    PreparingLayout,
    InitializingIdentity,
    PinningTrust,
    DiscoveringBundles,
    VerifyingBundles,
    InstallingBundles,
    InitializingVault,
    Finalizing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitProgress {
    pub phase: InitPhase,
    pub completed: Option<usize>,
    pub total: Option<usize>,
    pub detail: Option<String>,
}

/// Run `ryeos init` end-to-end (Model B).
///
/// Order:
///   1. Layout: create `<app_root>/.ai/{node,state,bundles,config}`
///   2. Operator key (load-or-create at `<app_root>/.ai/config/keys/signing/private_key.pem`)
///   3. Node key (load-or-create at `<app_root>/.ai/node/identity/private_key.pem`)
///   4. Self-trust both keys (write signed `<fp>.toml` into user trust dir)
///   5. Pin official publisher key into user trust dir + additional trust files
///   6. Discover bundles in `source_dir` — scan for child dirs containing `.ai/`
///   7. Install each discovered bundle + write registration record
///   8. Vault X25519 keypair
///   9. Post-init trust verification
pub fn run_init(opts: &InitOptions) -> Result<InitReport> {
    run_init_with_progress(opts, |_| Ok(()))
}

pub fn run_init_with_progress(
    opts: &InitOptions,
    mut observe: impl FnMut(&InitProgress) -> Result<()>,
) -> Result<InitReport> {
    run_init_internal(opts, None, &mut observe)
}

pub fn run_init_with_operator_ceremony(
    opts: &InitOptions,
    ceremony: InitOperatorCeremony,
    mut observe: impl FnMut(&InitProgress) -> Result<()>,
) -> Result<InitReport> {
    run_init_internal(opts, Some(ceremony), &mut observe)
}

fn run_init_internal(
    opts: &InitOptions,
    mut ceremony: Option<InitOperatorCeremony>,
    mut observe: impl FnMut(&InitProgress) -> Result<()>,
) -> Result<InitReport> {
    let mut progress = |phase, completed, total, detail: Option<String>| {
        observe(&InitProgress {
            phase,
            completed,
            total,
            detail,
        })
    };
    progress(InitPhase::PreparingLayout, None, None, None)?;
    // ── 0. Source exists? ──
    if !opts.source_dir.is_dir() {
        bail!(
            "bundle source directory not found: {}\n\
             \n\
             If you installed from a package, the default source is /usr/share/ryeos.\n\
             For development, use: ryeos init --source bundles\n\
             For Docker, use: ryeos init --source /opt/ryeos",
            opts.source_dir.display()
        );
    }

    // ── 1. Layout ──
    create_layout(&opts.app_root)?;

    // Operator config root (`<app_root>/.ai/config`) — the single trust
    // source for `ryeos init`. Bundles are never a trust source.
    let operator_config_root = opts.app_root.join(ryeos_engine::AI_DIR).join("config");

    let trust_dir = opts
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("keys")
        .join("trusted");
    fs::create_dir_all(&trust_dir)
        .with_context(|| format!("failed to create trust dir {}", trust_dir.display()))?;

    // ── 2. User key ──
    progress(InitPhase::InitializingIdentity, None, None, None)?;
    let user_key_path = opts
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("keys")
        .join("signing")
        .join("private_key.pem");
    if let Some(ceremony) = ceremony.as_ref() {
        validate_operator_ceremony(ceremony)?;
    }
    let contribution = ceremony
        .as_mut()
        .and_then(|ceremony| ceremony.entropy_contribution.take());
    let (user_key, user_key_created, contribution_digest) =
        load_or_create_operator_key(&user_key_path, contribution)
        .with_context(|| format!("user key at {}", user_key_path.display()))?;
    let user_fp = compute_fingerprint(&user_key.verifying_key());
    ensure_operator_genesis(
        &opts.app_root,
        &user_key,
        &user_fp,
        ceremony.as_ref().map(|ceremony| &ceremony.profile),
        user_key_created,
        contribution_digest.as_deref(),
    )?;

    // ── 3. Node key ──
    let node_key_path = opts
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("identity")
        .join("private_key.pem");
    let node_key = load_or_create_key(&node_key_path, false)
        .with_context(|| format!("node key at {}", node_key_path.display()))?;
    let node_fp = compute_fingerprint(&node_key.verifying_key());
    ryeos_app::identity::NodeIdentity::load(&node_key_path)
        .with_context(|| format!("load node identity {}", node_key_path.display()))?
        .write_public_identity(&node_key_path.with_file_name("public-identity.json"))
        .with_context(|| {
            format!(
                "write node public identity {}",
                node_key_path
                    .with_file_name("public-identity.json")
                    .display()
            )
        })?;

    // ── 4. Self-trust both keys ──
    pin_key(
        &user_key.verifying_key(),
        "user",
        &trust_dir,
        Some(&user_key),
    )
    .map_err(|e| anyhow!("pin user trust doc: {e}"))?;
    pin_key(
        &node_key.verifying_key(),
        "node",
        &trust_dir,
        Some(&node_key),
    )
    .map_err(|e| anyhow!("pin node trust doc: {e}"))?;

    // ── 5. Pin official publisher key ──
    progress(InitPhase::PinningTrust, None, None, None)?;
    // Owner label must match the bundle pipeline (`populate-bundles.sh --owner
    // ryeos-official`, all release Dockerfiles). The label is informational, but
    // bundle preflight compares it, so a divergence used to brick boot (the
    // mismatch is now a warning, not fatal — see ryeos-bundle preflight).
    let official_publisher_vk = decode_official_publisher_pubkey()?;
    let pinned_fp = pin_key(&official_publisher_vk, "ryeos-official", &trust_dir, None)
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
    progress(InitPhase::DiscoveringBundles, None, None, None)?;
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

    // Source-root seed data owns node registration grants. Init reads this
    // declarative data and materializes signed node registrations; it does not
    // infer command registration authority from bundle names or discovery.
    let seed_trust_store = TrustStore::load(None, &operator_config_root)
        .context("load trust store for source-root seed data")?;
    let command_registration_grants =
        load_init_bundle_registration_grants(&opts.source_dir, &seed_trust_store).with_context(
            || {
                format!(
                    "load init bundle registration grants from {}",
                    opts.source_dir.display()
                )
            },
        )?;
    materialize_seed_command_registration_policy(
        &opts.source_dir,
        &opts.app_root,
        &seed_trust_store,
        &node_key,
    )
    .with_context(|| "materialize seed command registration policy")?;

    // ── 6b. Build source bundle plan ──
    // Planning is always performed, even when `skip_preflight` is true: the
    // flag skips verification jobs only, not manifest policy, ordering,
    // duplicate-provider checks, or cycle checks.
    let candidates: Vec<PlanInput> = discovered
        .iter()
        .map(|(name, source_path)| PlanInput {
            name: name.clone(),
            source: BundleSource::SourceDir(source_path.clone()),
        })
        .collect();
    let plan = build_plan(BundlePlanMode::InitSourceSet, &candidates, &[])
        .context("bundle source-set planning")?;
    tracing::info!(
        order = ?plan.install_order,
        "installation order determined"
    );

    // A re-init inherits the existing immutable node policy. On first init the
    // policy is materialized later in this transaction and its defined default
    // is disabled, so authoring preflight uses the matching compiled snapshot.
    let isolation_policy = opts
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join(ryeos_engine::isolation::ISOLATION_POLICY_RELATIVE_PATH);
    let isolation = match fs::symlink_metadata(&isolation_policy) {
        Ok(_) => ryeos_app::engine_init::load_registered_isolation(&opts.app_root)
            .context("load existing node isolation policy for init preflight")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Arc::new(ryeos_engine::isolation::IsolationRuntime::default())
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspect existing node isolation policy {}",
                    isolation_policy.display()
                )
            });
        }
    };

    // Prove the complete source generation can become the next immutable
    // runtime before the first installed bundle is touched. This privileged
    // admission is mandatory even for test-only `skip_preflight`: that flag
    // may skip ordinary candidate execution, never selected-backend capture or
    // next-boot registry construction.
    let prospective_roots = plan
        .bundles
        .values()
        .map(|bundle| bundle.source.root_path().clone())
        .collect::<Vec<_>>();
    let prospective_isolation = ryeos_app::engine_init::admit_node_bundle_roots(
        &opts.app_root,
        &prospective_roots,
        &seed_trust_store,
    )
    .context("prospective init source set would fail node boot")?;

    if !opts.skip_preflight {
        progress(
            InitPhase::VerifyingBundles,
            Some(0),
            Some(plan.verification_jobs.len()),
            None,
        )?;
        for (index, job) in plan.verification_jobs.iter().enumerate() {
            progress(
                InitPhase::VerifyingBundles,
                Some(index),
                Some(plan.verification_jobs.len()),
                Some(job.subject.clone()),
            )?;
            ryeos_bundle::preflight::preflight_verify_bundle_in_context(
                &job.subject_root,
                &job.dependency_roots,
                &operator_config_root,
                Arc::clone(&isolation),
            )
            .with_context(|| {
                format!("verify {} source against pinned publisher key", job.subject)
            })?;
        }
    }

    // ── 7. Install each bundle (atomic stage → swap) ──
    // Serialize the entire installed-set mutation. Each per-name transaction
    // below is acquired through this guard, preserving global -> per-name lock
    // order while dependency generations are activated in plan order.
    let bundle_registry_lock =
        ryeos_app::bundle_transaction::BundleRegistryMutationLock::acquire(&opts.app_root)?;
    let mut bundles_installed = Vec::new();
    for (index, name) in plan.install_order.iter().enumerate() {
        progress(
            InitPhase::InstallingBundles,
            Some(index),
            Some(plan.install_order.len()),
            Some(name.clone()),
        )?;
        let planned = plan
            .bundles
            .get(name)
            .with_context(|| format!("planned bundle {}", name))?;
        let BundleSource::SourceDir(source_path) = &planned.source else {
            bail!("init source-set plan unexpectedly included installed bundle {name}");
        };
        let target = opts
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("bundles")
            .join(name);
        let transaction = bundle_registry_lock.acquire_bundle(name)?;
        transaction.reconcile(&node_key)?;
        let grants = command_registration_grants
            .get(name)
            .cloned()
            .unwrap_or_default();
        let registration = bundle_registration_value(&target, &grants);
        // Source-set preflight above validates the complete graph before init
        // mutates any installed bundle. Re-verify each completed staging tree
        // against the dependency generations already installed in plan order,
        // so a source mutation racing the copy cannot reach activation.
        let installed_dependency_roots: Vec<PathBuf> = plan
            .install_order
            .iter()
            .filter(|dependency_name| {
                planned
                    .dependency_closure
                    .contains(dependency_name.as_str())
            })
            .map(|dependency_name| {
                opts.app_root
                    .join(ryeos_engine::AI_DIR)
                    .join("bundles")
                    .join(dependency_name)
            })
            .collect();

        if target.exists() {
            // Bundle already installed. Registration continues to name the
            // same canonical path, so publish it before the atomic tree
            // exchange; every observable generation remains registered.
            verify_bundle_structure(&target)?;
            replace_bundle(
                source_path,
                &target,
                &transaction,
                registration.clone(),
                |staging| {
                    validate_selected_backend_staging(
                        &opts.app_root,
                        name,
                        staging,
                        &plan,
                        &prospective_isolation,
                        &seed_trust_store,
                    )?;
                    if opts.skip_preflight {
                        return Ok(());
                    }
                    ryeos_bundle::preflight::preflight_verify_bundle_staging_in_context(
                        staging,
                        name,
                        &installed_dependency_roots,
                        &operator_config_root,
                        Arc::clone(&isolation),
                    )
                    .with_context(|| {
                        format!(
                            "preflight verification refused completed {} replacement staging tree",
                            name
                        )
                    })
                },
            )
            .with_context(|| {
                format!(
                    "atomic replace {}: {} -> {}",
                    name,
                    source_path.display(),
                    target.display()
                )
            })?;
        } else {
            install_bundle(
                &opts.app_root,
                name,
                source_path,
                &transaction,
                registration.clone(),
                |staging| {
                    validate_selected_backend_staging(
                        &opts.app_root,
                        name,
                        staging,
                        &plan,
                        &prospective_isolation,
                        &seed_trust_store,
                    )?;
                    if opts.skip_preflight {
                        return Ok(());
                    }
                    ryeos_bundle::preflight::preflight_verify_bundle_staging_in_context(
                        staging,
                        name,
                        &installed_dependency_roots,
                        &operator_config_root,
                        Arc::clone(&isolation),
                    )
                    .with_context(|| {
                        format!(
                            "preflight verification refused completed {} install staging tree",
                            name
                        )
                    })
                },
            )?;
        }
        transaction.commit_present(&node_key)?;

        bundles_installed.push(name.clone());
    }
    drop(bundle_registry_lock);

    // ── 8. Vault X25519 keypair ──
    progress(InitPhase::InitializingVault, None, None, None)?;
    let vault_dir = opts
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault");
    fs::create_dir_all(&vault_dir)
        .with_context(|| format!("create vault dir {}", vault_dir.display()))?;
    let vault_secret_path = vault_dir.join("private_key.pem");
    let vault_public_path = vault_dir.join("public_key.pem");
    let vault_sk = lillux::with_exclusive_file_lock(&vault_secret_path, || {
        match fs::symlink_metadata(&vault_secret_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                bail!("refusing unsafe vault key path {}", vault_secret_path.display())
            }
            Ok(_) => lillux::vault::read_secret_key(&vault_secret_path)
                .with_context(|| format!("load vault key {}", vault_secret_path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let sk = lillux::vault::VaultSecretKey::generate();
                lillux::vault::write_secret_key(&vault_secret_path, &sk)
                    .with_context(|| format!("write vault key {}", vault_secret_path.display()))?;
                Ok(sk)
            }
            Err(error) => Err(error)
                .with_context(|| format!("inspect vault key {}", vault_secret_path.display())),
        }
    })
    .with_context(|| format!("initialize vault key {}", vault_secret_path.display()))?;
    if let Ok(metadata) = fs::symlink_metadata(&vault_public_path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("refusing unsafe vault public key path {}", vault_public_path.display());
        }
    }
    lillux::vault::write_public_key(&vault_public_path, &vault_sk.public_key())
        .with_context(|| format!("write vault pubkey {}", vault_public_path.display()))?;

    // ── 8b. Node-owned default policies ──
    materialize_node_defaults(&opts.app_root)?;

    // ── 9. Post-init trust verification ──
    progress(InitPhase::Finalizing, None, None, None)?;
    let post_trust =
        TrustStore::load(None, &operator_config_root).context("load post-init trust store")?;
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

    let vault_pubkey_fingerprint = vault_sk.public_key().fingerprint();
    write_init_completion(
        &opts.app_root,
        &user_key,
        &user_fp,
        &node_fp,
        &vault_pubkey_fingerprint,
    )?;
    let next_steps = Vec::new();

    Ok(InitReport {
        app_root: opts.app_root.clone(),
        user_key_fingerprint: user_fp,
        node_key_fingerprint: node_fp,
        official_publisher_pinned: OFFICIAL_PUBLISHER_FP.to_string(),
        vault_pubkey_fingerprint,
        bundles_installed,
        next_steps,
    })
}

fn init_completion_path(app_root: &Path) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("config/onboarding/init-completion-v1.json")
}

fn registration_digests(app_root: &Path) -> Result<BTreeMap<String, String>> {
    let directory = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node/bundles");
    let paths = lillux::collect_regular_files_no_follow(&directory, false)?
        .ok_or_else(|| anyhow!("bundle registration directory is absent"))?;
    let mut digests = BTreeMap::new();
    for path in paths {
        if !matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("yaml" | "yml")
        ) {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("bundle registration filename is not UTF-8"))?
            .to_string();
        let bytes = lillux::read_regular_file_bounded_no_follow(&path, 1024 * 1024)?;
        digests.insert(name, lillux::sha256_hex(&bytes));
    }
    if digests.is_empty() {
        bail!("no bundle registrations are present");
    }
    Ok(digests)
}

fn write_init_completion(
    app_root: &Path,
    signing_key: &SigningKey,
    operator_fingerprint: &str,
    node_fingerprint: &str,
    vault_fingerprint: &str,
) -> Result<()> {
    let body = InitCompletionBody {
        schema: INIT_COMPLETION_SCHEMA.to_string(),
        operator_fingerprint: operator_fingerprint.to_string(),
        node_fingerprint: node_fingerprint.to_string(),
        vault_fingerprint: vault_fingerprint.to_string(),
        registration_digests: registration_digests(app_root)?,
    };
    let canonical = lillux::canonical_json(&serde_json::to_value(&body)?)?;
    let signature = signing_key.sign(canonical.as_bytes());
    let document = InitCompletionDocument {
        body,
        signature: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
        ),
    };
    let bytes = serde_json::to_vec_pretty(&document)?;
    if bytes.len() as u64 > INIT_COMPLETION_MAX_BYTES {
        bail!("init completion record exceeds its size limit");
    }
    let path = init_completion_path(app_root);
    lillux::with_exclusive_file_lock(&path, || {
        lillux::atomic_write(&path, &bytes)
            .map_err(|error| anyhow!("write init completion {}: {error}", path.display()))
    })
}

pub fn verify_init_completion(app_root: &Path) -> Result<Option<InitCompletionReport>> {
    let path = init_completion_path(app_root);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("unsafe init completion record at {}", path.display())
        }
        Ok(metadata) if metadata.len() > INIT_COMPLETION_MAX_BYTES => {
            bail!("init completion record exceeds its size limit")
        }
        Ok(_) => {}
    }
    let bytes = lillux::read_regular_file_bounded_no_follow(&path, INIT_COMPLETION_MAX_BYTES)?;
    let document: InitCompletionDocument = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse init completion {}", path.display()))?;
    if document.body.schema != INIT_COMPLETION_SCHEMA {
        bail!("unsupported init completion schema");
    }
    let operator_key_path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("config/keys/signing/private_key.pem");
    let operator_pem = Zeroizing::new(String::from_utf8(
        lillux::read_regular_file_bounded_no_follow(&operator_key_path, 32 * 1024)?,
    )?);
    let operator_key = SigningKey::from_pkcs8_pem(operator_pem.as_str())?;
    let operator_fingerprint = compute_fingerprint(&operator_key.verifying_key());
    if operator_fingerprint != document.body.operator_fingerprint {
        bail!("init completion operator fingerprint does not match the current key");
    }
    let signature = document
        .signature
        .strip_prefix("ed25519:")
        .ok_or_else(|| anyhow!("init completion signature has an invalid encoding"))?;
    let signature = Signature::from_slice(
        &base64::engine::general_purpose::STANDARD.decode(signature)?,
    )?;
    let canonical = lillux::canonical_json(&serde_json::to_value(&document.body)?)?;
    operator_key
        .verifying_key()
        .verify(canonical.as_bytes(), &signature)
        .context("verify init completion signature")?;

    let node_key_path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    let node_pem = Zeroizing::new(String::from_utf8(
        lillux::read_regular_file_bounded_no_follow(&node_key_path, 32 * 1024)?,
    )?);
    let node_key = SigningKey::from_pkcs8_pem(node_pem.as_str())?;
    if compute_fingerprint(&node_key.verifying_key()) != document.body.node_fingerprint {
        bail!("init completion node fingerprint does not match the current key");
    }
    let vault_path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node/vault/public_key.pem");
    let vault_fingerprint = lillux::vault::read_public_key(&vault_path)?.fingerprint();
    if vault_fingerprint != document.body.vault_fingerprint {
        bail!("init completion vault fingerprint does not match the current key");
    }
    let registrations = registration_digests(app_root)?;
    if registrations != document.body.registration_digests {
        bail!("bundle registrations differ from the signed init completion record");
    }
    Ok(Some(InitCompletionReport {
        operator_fingerprint,
        node_fingerprint: document.body.node_fingerprint,
        vault_fingerprint,
        bundles_verified: registrations.len(),
    }))
}

fn validate_selected_backend_staging(
    app_root: &Path,
    bundle_name: &str,
    staging: &Path,
    plan: &ryeos_bundle::plan::BundlePlan,
    prospective_isolation: &ryeos_engine::isolation::IsolationRuntime,
    node_trust_store: &TrustStore,
) -> Result<()> {
    let selected_bundle = prospective_isolation
        .inspection()
        .backend
        .selection
        .as_ref()
        .map(|selection| selection.bundle.as_str());
    if !prospective_isolation.is_enforced() || selected_bundle != Some(bundle_name) {
        return Ok(());
    }
    let roots = plan
        .bundles
        .iter()
        .map(|(name, bundle)| {
            if name == bundle_name {
                staging.to_path_buf()
            } else {
                bundle.source.root_path().clone()
            }
        })
        .collect::<Vec<_>>();
    ryeos_app::engine_init::load_prospective_isolation(app_root, &roots, node_trust_store)
        .with_context(|| {
            format!("selected isolation backend `{bundle_name}` staging tree would fail next boot")
        })?;
    Ok(())
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct InitBundleRegistrationGrantsFile {
    #[serde(default)]
    bundles: HashMap<String, InitBundleRegistrationGrant>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct InitBundleRegistrationGrant {
    #[serde(default)]
    command_registration_caps: Vec<String>,
}

fn source_node_init_dir(source_dir: &Path) -> PathBuf {
    source_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("init")
}

fn load_init_bundle_registration_grants(
    source_dir: &Path,
    trust_store: &TrustStore,
) -> Result<HashMap<String, Vec<String>>> {
    let path = source_node_init_dir(source_dir)
        .join("bundle-registration-grants")
        .join("default.yaml");
    if !path.exists() {
        bail!(
            "missing init bundle registration grants seed file {}",
            path.display()
        );
    }
    let raw = read_trusted_seed_yaml(&path, trust_store)
        .with_context(|| format!("verify init bundle registration grants {}", path.display()))?;
    let body = lillux::signature::strip_signature_lines(&raw);
    let parsed: InitBundleRegistrationGrantsFile = serde_yaml::from_str(&body)
        .with_context(|| format!("parse init bundle registration grants {}", path.display()))?;
    Ok(parsed
        .bundles
        .into_iter()
        .map(|(name, grant)| (name, grant.command_registration_caps))
        .collect())
}

fn materialize_seed_command_registration_policy(
    source_dir: &Path,
    app_root: &Path,
    trust_store: &TrustStore,
    node_key: &SigningKey,
) -> Result<()> {
    let source = source_node_init_dir(source_dir).join("command-registration");
    let source_meta = fs::symlink_metadata(&source).with_context(|| {
        format!(
            "missing command registration policy seed dir {}",
            source.display()
        )
    })?;
    if source_meta.file_type().is_symlink() || !source_meta.file_type().is_dir() {
        bail!(
            "command registration policy seed at {} must be a real directory",
            source.display()
        );
    }
    let target = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("command_registration");

    let mut policies = Vec::new();
    for entry in fs::read_dir(&source)
        .with_context(|| format!("read command registration policy dir {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("read seed file type {}", source_path.display()))?;
        if file_type.is_symlink() {
            bail!(
                "command registration policy seed {} must not be a symlink",
                source_path.display()
            );
        }
        if !file_type.is_file() {
            continue;
        }
        let ext = source_path.extension().and_then(|ext| ext.to_str());
        if ext == Some("yaml") || ext == Some("yml") {
            policies.push(source_path);
        }
    }
    policies.sort();
    if policies.len() != 1 {
        bail!(
            "command registration policy seed at {} must contain exactly one .yaml/.yml file, found {}",
            source.display(),
            policies.len()
        );
    }

    let source_path = &policies[0];
    let verified = read_trusted_seed_yaml(source_path, trust_store).with_context(|| {
        format!(
            "verify command registration policy seed {}",
            source_path.display()
        )
    })?;
    let body = lillux::signature::strip_signature_lines(&verified);
    validate_command_registration_seed_body(source_path, &body)?;

    if target.exists() {
        fs::remove_dir_all(&target).with_context(|| {
            format!(
                "remove stale command registration policy dir {}",
                target.display()
            )
        })?;
    }
    fs::create_dir_all(&target).with_context(|| {
        format!(
            "create command registration policy dir {}",
            target.display()
        )
    })?;
    let file_name = source_path
        .file_name()
        .context("command registration policy seed has no filename")?;
    let target_path = target.join(file_name);
    let signed = lillux::signature::sign_content(&body, node_key, "#", None);
    let tmp = target_path.with_extension("tmp");
    fs::write(&tmp, signed.as_bytes())
        .with_context(|| format!("write command registration policy temp {}", tmp.display()))?;
    fs::rename(&tmp, &target_path).with_context(|| {
        format!(
            "rename command registration policy {} -> {}",
            tmp.display(),
            target_path.display()
        )
    })?;
    Ok(())
}

fn read_trusted_seed_yaml(path: &Path, trust_store: &TrustStore) -> Result<String> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("read seed YAML metadata {}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        bail!("seed YAML {} must be a regular file", path.display());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("read seed YAML {}", path.display()))?;
    let envelope = SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    };
    let header = ryeos_engine::item_resolution::parse_signature_header(&content, &envelope)
        .with_context(|| format!("seed YAML {} has no valid signature", path.display()))?;
    let (trust_class, _) =
        ryeos_engine::trust::verify_item_signature(&content, &header, &envelope, trust_store)
            .with_context(|| format!("verify seed YAML signature {}", path.display()))?;
    if trust_class != TrustClass::Trusted {
        bail!(
            "seed YAML {} is not trusted (trust_class: {:?})",
            path.display(),
            trust_class
        );
    }
    Ok(content)
}

fn validate_command_registration_seed_body(path: &Path, body: &str) -> Result<()> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawClaimPattern {
        #[allow(dead_code)]
        kind: String,
        #[allow(dead_code)]
        value: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawClaimRule {
        #[allow(dead_code)]
        claim: RawClaimPattern,
        #[allow(dead_code)]
        #[serde(default)]
        required_caps: Vec<String>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawPolicy {
        #[serde(default)]
        claim_rules: Vec<RawClaimRule>,
        #[serde(default)]
        system_source_caps: Vec<String>,
    }

    let value: serde_yaml::Value = serde_yaml::from_str(body)
        .with_context(|| format!("parse command registration policy seed {}", path.display()))?;
    if let Some(mapping) = value.as_mapping() {
        for forbidden in ["category", "section", "name"] {
            if mapping.contains_key(serde_yaml::Value::String(forbidden.to_string())) {
                bail!(
                    "command registration policy seed {} declares legacy structural field '{}'",
                    path.display(),
                    forbidden
                );
            }
        }
    }
    let policy: RawPolicy = serde_yaml::from_str(body)
        .with_context(|| format!("parse command registration policy seed {}", path.display()))?;
    if policy.claim_rules.is_empty() {
        bail!(
            "command registration policy seed {} must declare at least one claim rule",
            path.display()
        );
    }
    if policy.system_source_caps.is_empty() {
        bail!(
            "command registration policy seed {} must declare non-empty system_source_caps",
            path.display()
        );
    }
    Ok(())
}

/// Discover bundles in a source directory.
///
/// Scans immediate children of `source_dir` for published bundle trees
/// containing `.ai/manifest.yaml`. A directory containing only release source
/// material (for example `manifest.source.yaml`) is not installable and is not
/// part of the initialized generation. Hidden directories (starting with `.`)
/// and names that don't pass [`is_valid_bundle_name`] are skipped.
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
        if child_path
            .join(ryeos_engine::AI_DIR)
            .join("manifest.yaml")
            .is_file()
        {
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

// ── Bundle manifest (generated + signed) ───────────────────────────
pub use ryeos_bundle::manifest::{
    derive_provides_kinds, materialize_manifest, parse_manifest, sort_bundles_by_dependency,
    validate_manifest_dependencies, BundleManifest, BundleManifestSource,
};
use ryeos_bundle::plan::{build_plan, BundlePlanMode, BundleSource, PlanInput};

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

    let doc =
        ryeos_engine::trust::PublisherTrustDoc::parse(&content).map_err(|e| anyhow!("{e}"))?;

    let vk = doc.decode_verifying_key().map_err(|e| anyhow!("{e}"))?;

    pin_key(&vk, &doc.owner, trust_dir, None)
        .map_err(|e| anyhow!("pin trust doc for {}: {e}", doc.owner))?;

    Ok(())
}

/// Create the Model B directory layout.
///
/// The app root contains:
/// - `node/` — mutable daemon state (identity, vault, config, bundle registrations)
/// - `state/` — CAS and runtime state
/// - `bundles/` — installed bundle content (populated by bundle installs)
fn create_layout(app_root: &Path) -> Result<()> {
    let dirs = [
        // Node tier (daemon-owned)
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("identity"),
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("auth")
            .join("authorized_keys"),
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("vault"),
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("config"),
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("bundles"),
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("engine")
            .join("kinds"),
        // CAS state
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("state")
            .join("objects"),
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("state")
            .join("locators"),
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("state")
            .join("refs"),
        // Installed bundles directory
        app_root.join(ryeos_engine::AI_DIR).join("bundles"),
        // Operator config (operator-edited)
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("config")
            .join("keys")
            .join("signing"),
        app_root
            .join(ryeos_engine::AI_DIR)
            .join("config")
            .join("keys")
            .join("trusted"),
    ];
    for d in &dirs {
        fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
    }
    let runtime_state_path = app_root.join(ryeos_engine::AI_DIR).join("state");
    let runtime_state = lillux::PinnedDirectory::open_or_create(&runtime_state_path)
        .context("pin initialized runtime-state directory")?;
    let recovery = runtime_state
        .open_or_create_child(std::ffi::OsStr::new("recovery"), 0o700)
        .context("create initialized recovery authority")?;
    recovery
        .open_or_create_child(std::ffi::OsStr::new("thread-projection"), 0o700)
        .context("create initialized thread-projection recovery authority")?;
    Ok(())
}

/// Load an existing key, or create one. Refuses to overwrite unless `force`.
fn load_or_create_key(path: &Path, force: bool) -> Result<SigningKey> {
    lillux::with_exclusive_file_lock(path, || load_or_create_key_locked(path, force))
}

fn load_or_create_key_locked(path: &Path, force: bool) -> Result<SigningKey> {
    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("refusing unsafe signing key path {}", path.display())
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("inspect key {}", path.display()))
        }
    };
    if force && existing {
        fs::remove_file(path).with_context(|| format!("remove old key {}", path.display()))?;
    }
    if existing && !force {
        let pem = Zeroizing::new(String::from_utf8(
            lillux::read_regular_file_bounded_no_follow(path, 32 * 1024)
                .with_context(|| format!("read existing key {}", path.display()))?,
        )?);
        let key = SigningKey::from_pkcs8_pem(pem.as_str())
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
    lillux::atomic_write_private(path, pem.as_bytes())
        .map_err(|error| anyhow!("write generated key {}: {error}", path.display()))?;
    Ok(signing_key)
}

fn load_or_create_operator_key(
    path: &Path,
    contribution: Option<Zeroizing<Vec<u8>>>,
) -> Result<(SigningKey, bool, Option<String>)> {
    lillux::with_exclusive_file_lock(path, || {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                bail!("refusing unsafe operator key path {}", path.display())
            }
            Ok(_) => {
                let pem = Zeroizing::new(String::from_utf8(
                    lillux::read_regular_file_bounded_no_follow(path, 32 * 1024)
                        .with_context(|| format!("read existing key {}", path.display()))?,
                )?);
                let key = SigningKey::from_pkcs8_pem(pem.as_str())
                    .with_context(|| format!("parse existing key {}", path.display()))?;
                return Ok((key, false, None));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspect key {}", path.display()))
            }
        }
        let mut os_random = Zeroizing::new([0_u8; 32]);
        OsRng.fill_bytes(&mut *os_random);
        let contribution_digest = contribution.as_ref().map(|contribution| {
            let mut digest = Sha256::new();
            digest.update(b"ryeos/operator-contribution/v1\0");
            digest.update(contribution.as_slice());
            digest.finalize()
        });
        let mut input = Zeroizing::new(Vec::with_capacity(64));
        input.extend_from_slice(&*os_random);
        if let Some(digest) = &contribution_digest {
            input.extend_from_slice(digest.as_slice());
        }
        let hkdf = hkdf::Hkdf::<Sha256>::new(
            Some(b"ryeos/operator-ed25519-seed/v1"),
            input.as_slice(),
        );
        let mut seed = Zeroizing::new([0_u8; 32]);
        hkdf.expand(b"ed25519 signing seed", &mut *seed)
            .map_err(|_| anyhow!("derive operator signing seed"))?;
        let signing_key = SigningKey::from_bytes(&*seed);
        let pem = signing_key
            .to_pkcs8_pem(Default::default())
            .map_err(|error| anyhow!("encode generated operator key: {error}"))?;
        lillux::atomic_write_private(path, pem.as_bytes())
            .map_err(|error| anyhow!("write generated operator key {}: {error}", path.display()))?;
        Ok((
            signing_key,
            true,
            contribution_digest.map(hex::encode),
        ))
    })
}

fn ensure_operator_genesis(
    app_root: &Path,
    signing_key: &SigningKey,
    operator_fingerprint: &str,
    profile: Option<&InitOperatorProfile>,
    key_created: bool,
    contribution_digest: Option<&str>,
) -> Result<()> {
    let path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("identity")
        .join("operator-genesis.json");
    lillux::with_exclusive_file_lock(&path, || {
        ensure_operator_genesis_locked(
            &path,
            signing_key,
            operator_fingerprint,
            profile,
            key_created,
            contribution_digest,
        )
    })
}

fn ensure_operator_genesis_locked(
    path: &Path,
    signing_key: &SigningKey,
    operator_fingerprint: &str,
    profile: Option<&InitOperatorProfile>,
    key_created: bool,
    contribution_digest: Option<&str>,
) -> Result<()> {
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("refusing unsafe operator genesis path {}", path.display())
        }
        Ok(_) => {
            let source = String::from_utf8(lillux::read_regular_file_bounded_no_follow(
                path,
                64 * 1024,
            )?)
                .with_context(|| format!("read operator genesis {}", path.display()))?;
            let mut value: serde_json::Value = serde_json::from_str(&source)
                .with_context(|| format!("parse operator genesis {}", path.display()))?;
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("operator genesis must be a JSON object"))?;
            let allowed = [
                "schema",
                "operator_fingerprint",
                "created_at",
                "ceremony_version",
                "display_name",
                "identity_statement",
                "contribution_digest",
                "signature",
            ];
            if object.keys().any(|key| !allowed.contains(&key.as_str()))
                || object.get("schema").and_then(serde_json::Value::as_str)
                    != Some("ryeos/operator-genesis/v1")
                || object
                    .get("ceremony_version")
                    .and_then(serde_json::Value::as_str)
                    != Some("1")
                || object
                    .get("created_at")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
            {
                bail!("operator genesis has an invalid v1 schema");
            }
            if object.get("display_name").and_then(serde_json::Value::as_str).is_some_and(
                |value| value.chars().count() > 80 || value.chars().any(char::is_control),
            ) || object
                .get("identity_statement")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| {
                    value.chars().count() > 280 || value.chars().any(char::is_control)
                })
            {
                bail!("operator genesis contains unsafe semantic identity fields");
            }
            let stored_fingerprint = value
                .get("operator_fingerprint")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow!("operator genesis missing operator_fingerprint"))?;
            if stored_fingerprint != operator_fingerprint {
                bail!(
                    "operator genesis fingerprint {} does not match operator key {}",
                    stored_fingerprint,
                    operator_fingerprint
                );
            }
            let signature = value
                .as_object_mut()
                .and_then(|object| object.remove("signature"))
                .and_then(|value| value.as_str().map(str::to_string))
                .and_then(|value| value.strip_prefix("ed25519:").map(str::to_string))
                .ok_or_else(|| anyhow!("operator genesis missing Ed25519 signature"))?;
            let signature_bytes = base64::engine::general_purpose::STANDARD
                .decode(signature)
                .context("decode operator genesis signature")?;
            let signature = Signature::from_slice(&signature_bytes)
                .context("parse operator genesis signature")?;
            let canonical = lillux::canonical_json(&value)
                .map_err(|error| anyhow!("canonicalize operator genesis: {error}"))?;
            signing_key
                .verifying_key()
                .verify(canonical.as_bytes(), &signature)
                .context("verify operator genesis signature")?;
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect operator genesis {}", path.display()))
        }
    }

    let profile = profile.cloned().unwrap_or_default();
    let mut document = serde_json::json!({
        "schema": "ryeos/operator-genesis/v1",
        "operator_fingerprint": operator_fingerprint,
        "created_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "ceremony_version": "1",
    });
    if let Some(display_name) = profile.display_name.filter(|value| !value.trim().is_empty()) {
        document["display_name"] = serde_json::Value::String(display_name);
    }
    if let Some(statement) = profile
        .identity_statement
        .filter(|value| !value.trim().is_empty())
    {
        document["identity_statement"] = serde_json::Value::String(statement);
    }
    if key_created {
        if let Some(digest) = contribution_digest {
            document["contribution_digest"] = serde_json::Value::String(digest.to_string());
        }
    }
    let canonical = lillux::canonical_json(&document)
        .map_err(|error| anyhow!("canonicalize operator genesis: {error}"))?;
    let signature = signing_key.sign(canonical.as_bytes());
    document["signature"] = serde_json::Value::String(format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
    ));
    let bytes = serde_json::to_vec_pretty(&document)?;
    lillux::atomic_write(&path, &bytes)
        .map_err(|error| anyhow!("write operator genesis {}: {error}", path.display()))?;
    Ok(())
}

fn validate_operator_ceremony(ceremony: &InitOperatorCeremony) -> Result<()> {
    if ceremony.profile.display_name.as_deref().is_some_and(|value| {
        value.chars().count() > 80 || value.chars().any(char::is_control)
    }) {
        bail!("operator display name exceeds 80 characters or contains control characters");
    }
    if ceremony
        .profile
        .identity_statement
        .as_deref()
        .is_some_and(|value| {
            value.chars().count() > 280 || value.chars().any(char::is_control)
        })
    {
        bail!("operator identity statement exceeds 280 characters or contains control characters");
    }
    if ceremony
        .entropy_contribution
        .as_ref()
        .is_some_and(|value| value.len() > 4096)
    {
        bail!("operator entropy contribution exceeds 4096 bytes");
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
    fn operator_creation_keeps_os_randomness_and_never_replaces_an_existing_key() {
        let tmp = tempfile::tempdir().expect("temporary directory");
        let first_path = tmp.path().join("first/private_key.pem");
        let second_path = tmp.path().join("second/private_key.pem");
        fs::create_dir_all(first_path.parent().expect("first parent")).expect("first key dir");
        fs::create_dir_all(second_path.parent().expect("second parent")).expect("second key dir");
        let contribution = || Zeroizing::new(b"same human contribution".to_vec());

        let (first, first_created, first_digest) =
            load_or_create_operator_key(&first_path, Some(contribution())).expect("first key");
        let (second, second_created, second_digest) =
            load_or_create_operator_key(&second_path, Some(contribution())).expect("second key");
        assert!(first_created && second_created);
        assert_eq!(first_digest, second_digest);
        assert_ne!(
            first.verifying_key().as_bytes(),
            second.verifying_key().as_bytes(),
            "equal human input must not reproduce an operator key"
        );

        let original = fs::read(&first_path).expect("read original key");
        let (reloaded, created, digest) = load_or_create_operator_key(
            &first_path,
            Some(Zeroizing::new(b"different contribution".to_vec())),
        )
        .expect("reload key");
        assert!(!created);
        assert!(digest.is_none());
        assert_eq!(first.verifying_key(), reloaded.verifying_key());
        assert_eq!(original, fs::read(&first_path).expect("read preserved key"));
    }

    #[test]
    fn official_publisher_fingerprint_matches_hardcoded_pubkey() {
        let vk = decode_official_publisher_pubkey().expect("decode pubkey");
        assert_eq!(compute_fingerprint(&vk), OFFICIAL_PUBLISHER_FP);
    }

    fn dev_trust_file() -> PathBuf {
        workspace_root().join(".dev-keys/PUBLISHER_DEV_TRUST.toml")
    }

    fn make_opts(state: &Path, _user: &Path) -> InitOptions {
        InitOptions {
            app_root: state.to_path_buf(),
            source_dir: workspace_root().join("bundles"),
            trust_files: vec![dev_trust_file()],
            skip_preflight: true,
        }
    }

    fn copy_source_seed(target_source: &Path) {
        copy_dir_recursive(
            &workspace_root().join("bundles/.ai"),
            &target_source.join(".ai"),
        )
        .expect("copy source-root seed data");
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
        let core_registration = fs::read_to_string(state.join(".ai/node/bundles/core.yaml"))
            .expect("read core registration");
        assert!(
            core_registration.contains("ryeos.register.command.root.help"),
            "core registration should include source-root grant caps: {core_registration}"
        );
        // Kind schemas inside core
        assert!(
            state
                .join(".ai/bundles/core/.ai/node/engine/kinds")
                .is_dir(),
            "core kind schemas must be inside the installed bundle"
        );
        assert!(state.join(".ai/node/identity/private_key.pem").exists());
        assert!(state.join(".ai/node/vault").is_dir());
        assert!(state
            .join(".ai/config/keys/signing/private_key.pem")
            .exists());
    }

    #[test]
    fn run_init_installs_core_and_hosted_node_without_standard() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        copy_dir_recursive(&workspace_root().join("bundles/core"), &source.join("core"))
            .expect("copy core bundle");
        copy_dir_recursive(
            &workspace_root().join("bundles/hosted-node"),
            &source.join("hosted-node"),
        )
        .expect("copy hosted-node bundle");
        copy_source_seed(&source);

        let state = tmp.path().join("state");
        let opts = InitOptions {
            app_root: state.to_path_buf(),
            source_dir: source,
            trust_files: vec![dev_trust_file()],
            skip_preflight: true,
        };

        let report = run_init(&opts).expect("init core + hosted-node");
        assert_eq!(
            report.bundles_installed,
            vec!["core".to_string(), "hosted-node".to_string()]
        );
        assert!(state.join(".ai/bundles/core/.ai").is_dir());
        assert!(state.join(".ai/bundles/hosted-node/.ai").is_dir());
        assert!(state.join(".ai/node/bundles/core.yaml").exists());
        assert!(state.join(".ai/node/bundles/hosted-node.yaml").exists());
        assert!(
            !state.join(".ai/bundles/standard").exists(),
            "hosted-node init proof must not install standard"
        );
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
        assert!(state
            .join(".ai/config/keys/signing/private_key.pem")
            .exists());
        assert!(state
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
        assert_eq!(
            report.vault_pubkey_fingerprint,
            sk.public_key().fingerprint()
        );
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
        let genesis_path = state.join(".ai/config/identity/operator-genesis.json");
        let genesis = fs::read(&genesis_path).expect("operator genesis #1");
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
        assert_eq!(
            genesis,
            fs::read(genesis_path).expect("operator genesis #2"),
            "reinitialization must not rewrite operator genesis"
        );
    }

    #[test]
    fn run_init_replaces_stale_command_registration_policy_and_node_signs_it() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let opts = make_opts(&state, &user);
        let _ = run_init(&opts).expect("init #1");

        let policy_dir = state.join(".ai/node/command_registration");
        fs::write(policy_dir.join("stale.yaml"), "claim_rules: []\n").expect("write stale policy");

        let report = run_init(&opts).expect("init #2");
        let mut policies = fs::read_dir(&policy_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        policies.sort();
        assert_eq!(policies, vec!["default.yaml"]);

        let content = fs::read_to_string(policy_dir.join("default.yaml")).unwrap();
        let envelope = SignatureEnvelope {
            prefix: "#".into(),
            suffix: None,
            after_shebang: false,
        };
        let header = ryeos_engine::item_resolution::parse_signature_header(&content, &envelope)
            .expect("materialized policy should be signed");
        assert_eq!(header.signer_fingerprint, report.node_key_fingerprint);
    }

    #[test]
    fn run_init_fails_when_command_registration_seed_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        copy_dir_recursive(&workspace_root().join("bundles/core"), &source.join("core"))
            .expect("copy core bundle");
        copy_source_seed(&source);
        fs::remove_dir_all(source.join(".ai/node/init/command-registration"))
            .expect("remove command-registration seed");

        let state = tmp.path().join("state");
        let opts = InitOptions {
            app_root: state,
            source_dir: source,
            trust_files: vec![dev_trust_file()],
            skip_preflight: true,
        };

        let err = run_init(&opts).expect_err("missing seed must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing command registration policy seed dir"),
            "got: {msg}"
        );
    }

    #[test]
    fn run_init_fails_when_bundle_registration_grants_seed_missing() {
        let tmp_source = tempfile::tempdir().unwrap();
        let source = tmp_source.path().join("source");
        fs::create_dir_all(&source).unwrap();
        copy_dir_recursive(&workspace_root().join("bundles/core"), &source.join("core"))
            .expect("copy core bundle");
        copy_source_seed(&source);
        fs::remove_file(
            source
                .join(".ai/node/init/bundle-registration-grants")
                .join("default.yaml"),
        )
        .expect("remove grants seed");

        let tmp_state = tempfile::tempdir().unwrap();
        let state = tmp_state.path().join("state");
        let opts = InitOptions {
            app_root: state,
            source_dir: source,
            trust_files: vec![dev_trust_file()],
            skip_preflight: true,
        };

        let err = run_init(&opts).expect_err("missing grants seed must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing init bundle registration grants seed file"),
            "got: {msg}"
        );
    }

    #[test]
    fn run_init_requires_explicit_trust_for_dev_signed_source() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let opts = InitOptions {
            app_root: state,
            source_dir: workspace_root().join("bundles"),
            trust_files: vec![],
            skip_preflight: true,
        };

        let err = run_init(&opts).expect_err("dev source without explicit trust must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not trusted") || msg.contains("signature"),
            "got: {msg}"
        );
    }

    #[test]
    fn run_init_accepts_explicit_trust_for_dev_signed_source() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let user = tmp.path().join("home");
        let report = run_init(&make_opts(&state, &user)).expect("init with explicit trust");
        assert!(
            !report.bundles_installed.is_empty(),
            "bundles should install"
        );
    }

    #[test]
    fn discover_bundles_finds_core_and_standard() {
        let bundles = discover_bundles(&workspace_root().join("bundles")).unwrap();
        let names: Vec<&str> = bundles.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"core"), "must find core: {:?}", names);
        assert!(
            names.contains(&"standard"),
            "must find standard: {:?}",
            names
        );
    }

    fn create_discoverable_bundle(path: &Path) {
        let ai_dir = path.join(ryeos_engine::AI_DIR);
        fs::create_dir_all(&ai_dir).unwrap();
        fs::write(ai_dir.join("manifest.yaml"), "name: fixture\n").unwrap();
    }

    #[test]
    fn discover_bundles_skips_hidden_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        // A hidden published tree is still skipped.
        let hidden = tmp.path().join(".hidden");
        create_discoverable_bundle(&hidden);
        // Create a valid published bundle.
        let valid = tmp.path().join("my-bundle");
        create_discoverable_bundle(&valid);

        let bundles = discover_bundles(tmp.path()).unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].0, "my-bundle");
    }

    #[test]
    fn discover_bundles_skips_non_bundle_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        // Dir without a published manifest — not a bundle.
        fs::create_dir_all(tmp.path().join("not-a-bundle")).unwrap();
        // Valid published bundle.
        let valid = tmp.path().join("real-bundle");
        create_discoverable_bundle(&valid);

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
            create_discoverable_bundle(&tmp.path().join(invalid));
        }
        // Valid name
        create_discoverable_bundle(&tmp.path().join("valid-bundle"));

        let bundles = discover_bundles(tmp.path()).unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].0, "valid-bundle");
    }

    #[test]
    fn discover_bundles_skips_release_source_only_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let ai_dir = tmp.path().join("prototype").join(ryeos_engine::AI_DIR);
        fs::create_dir_all(&ai_dir).unwrap();
        fs::write(ai_dir.join("manifest.source.yaml"), "name: prototype\n").unwrap();

        assert!(discover_bundles(tmp.path()).unwrap().is_empty());
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

    fn materialize_test_manifest(bundle: &Path, name: &str) {
        let ai_dir = bundle.join(ryeos_engine::AI_DIR);
        let source: BundleManifestSource =
            serde_yaml::from_str(&fs::read_to_string(ai_dir.join("manifest.source.yaml")).unwrap())
                .unwrap();
        let manifest = materialize_manifest(source, &ai_dir, name).unwrap();
        fs::write(
            ai_dir.join("manifest.yaml"),
            serde_yaml::to_string(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn parse_manifest_reads_core() {
        let mf = parse_manifest(&workspace_root().join("bundles/core"), "core")
            .expect("parse core manifest");
        assert_eq!(mf.name, "core");
        assert_eq!(mf.version, "0.5.0");
        assert!(!mf.provides_kinds.is_empty());
        assert!(mf.provides_kinds.contains(&"config".to_string()));
        assert!(mf.provides_kinds.contains(&"handler".to_string()));
        assert!(mf.provides_kinds.contains(&"parser".to_string()));
        assert!(mf.provides_kinds.contains(&"runtime".to_string()));
        assert!(mf.provides_kinds.contains(&"service".to_string()));
        assert!(mf.provides_kinds.contains(&"tool".to_string()));
        assert!(
            !mf.provides_kinds.contains(&"knowledge".to_string()),
            "core must NOT provide knowledge after schema move to standard: {:?}",
            mf.provides_kinds
        );
        assert!(mf.requires_kinds.is_empty(), "core should have no requires");
    }

    #[test]
    fn parse_manifest_reads_standard() {
        let mf = parse_manifest(&workspace_root().join("bundles/standard"), "standard")
            .expect("parse standard manifest");
        assert_eq!(mf.name, "standard");
        assert!(mf.provides_kinds.contains(&"directive".to_string()));
        assert!(mf.provides_kinds.contains(&"graph".to_string()));
        assert!(
            mf.provides_kinds.contains(&"knowledge".to_string()),
            "standard must provide knowledge after schema move from core"
        );
        assert!(
            !mf.uses_kinds.contains(&"knowledge".to_string()),
            "standard must not use knowledge externally since it now provides it"
        );
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
    fn parse_manifest_fails_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("no-manifest");
        fs::create_dir_all(bundle.join(".ai")).unwrap();
        let error = parse_manifest(&bundle, "no-manifest").unwrap_err();
        assert!(error.to_string().contains("required generated"));
    }

    #[test]
    fn parse_manifest_rejects_invalid_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bad-manifest");
        fs::create_dir_all(bundle.join(".ai")).unwrap();
        fs::write(bundle.join(".ai/manifest.yaml"), "not: [valid\nyaml").unwrap();
        assert!(parse_manifest(&bundle, "bad-manifest").is_err());
    }

    #[test]
    fn validate_dependencies_core_and_standard_ok() {
        let root = workspace_root();
        let bundles = vec![
            ("core".to_string(), root.join("bundles/core")),
            ("standard".to_string(), root.join("bundles/standard")),
        ];
        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "all bundle dependencies should be satisfied"
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
        materialize_test_manifest(&needy, "needy");

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
    fn validate_dependencies_rejects_bundles_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        // No generated manifest: dependency validation must fail closed.
        let bare = tmp.path().join("bare");
        fs::create_dir_all(bare.join(".ai")).unwrap();

        let bundles = vec![("bare".to_string(), bare)];
        let error = validate_manifest_dependencies(&bundles).unwrap_err();
        assert!(format!("{error:#}").contains("required generated"));
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
        materialize_test_manifest(&selfish, "selfish");

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
        materialize_test_manifest(&provider, "provider");

        // Consumer bundle
        let consumer = tmp.path().join("consumer");
        fs::create_dir_all(consumer.join(".ai")).unwrap();
        fs::write(
            consumer.join(".ai/manifest.source.yaml"),
            "name: consumer\nversion: '1.0'\nrequires_kinds:\n  - alpha\n",
        )
        .unwrap();
        materialize_test_manifest(&consumer, "consumer");

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
            bundle.join(".ai/manifest.yaml"),
            "name: wrong-name\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\n",
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

        // Create a source dir with a bundle that requires an unsatisfied kind
        let source = tmp.path().join("source");
        copy_source_seed(&source);
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
        materialize_test_manifest(&needy, "needy");

        let opts = InitOptions {
            app_root: state.clone(),
            source_dir: source,
            trust_files: vec![dev_trust_file()],
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
                installed
                    .iter()
                    .map(|e| e.path().display().to_string())
                    .collect::<Vec<_>>()
            );
        }

        // No registrations written
        let regs_dir = state.join(".ai/node/bundles");
        if regs_dir.exists() {
            let regs: Vec<_> = fs::read_dir(&regs_dir)
                .unwrap()
                .filter_map(|e| {
                    let e = e.ok()?;
                    if e.path()
                        .extension()
                        .map(|ext| ext == "yaml")
                        .unwrap_or(false)
                    {
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

    // ── Generated-manifest tests: derive, materialize, source split ──

    #[test]
    fn derive_provides_kinds_scans_core_schemas() {
        let ai_dir = workspace_root().join("bundles/core/.ai");
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
        assert!(
            !kinds.contains(&"knowledge".to_string()),
            "core must NOT provide knowledge after schema move to standard: {kinds:?}"
        );
    }

    #[test]
    fn derive_provides_kinds_scans_standard_schemas() {
        let ai_dir = workspace_root().join("bundles/standard/.ai");
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
            "standard must provide knowledge after schema move from core: {kinds:?}"
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
            uses_kinds: vec![],
            runtime_authority: Default::default(),
            smoke: vec![],
            shadows: vec![],
            isolation_backends: vec![],
        };
        let manifest = materialize_manifest(source, &ai_dir, "test-bundle").unwrap();
        assert_eq!(manifest.provides_kinds, vec!["mykind"]);
        assert_eq!(manifest.name, "test-bundle");
    }

    #[test]
    fn parse_manifest_rejects_source_only_bundle() {
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

        let error = parse_manifest(&bundle, "dev-bundle").unwrap_err();
        assert!(error.to_string().contains("required generated"));
    }

    #[test]
    fn parse_manifest_reads_generated_and_ignores_source() {
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

        let mf = parse_manifest(&bundle, "pub-bundle").expect("should find generated manifest");
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
        assert!(
            result.is_err(),
            "unknown field in source should be rejected"
        );
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("bundles").is_dir())
            .expect("workspace root with bundles/ directory")
            .to_path_buf()
    }
}
