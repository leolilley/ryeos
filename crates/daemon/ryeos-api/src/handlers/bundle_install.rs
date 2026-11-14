//! `bundle.install` — install a downstream bundle via node-config writer.
//!
//! Copies source to `<app_root>/.ai/bundles/<name>/`, writes a signed
//! `kind: node` bundle registration item at `<app_root>/.ai/node/bundles/<name>.yaml`.
//!
//! Any bundle name is accepted — no special treatment for any name.
//!
//! Stopped-node only: the daemon must be stopped (engine reload not implemented).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use ryeos_bundle::plan::{
    BundlePlan, BundlePlanMode, BundleSource, PlanInput, VerificationSubjectKind, build_plan,
};
use ryeos_bundle::preflight::{
    preflight_verify_bundle_staging_in_context, preflight_verify_named_bundle_in_context,
};
use ryeos_engine::trust::TrustStore;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Bundle name; becomes the install directory name.
    pub name: String,
    /// Source directory to copy from.
    pub source_path: PathBuf,
    /// Replace an existing installed bundle after preflight verification.
    #[serde(default)]
    pub replace: bool,
    /// Preserve installed runtime artifacts when replacing from a sparse source.
    ///
    /// This keeps `.ai/bin`, `.ai/objects`, and `.ai/refs` from the existing
    /// installation when the replacement source does not provide them. Source
    /// files always win when present.
    #[serde(default)]
    pub preserve_runtime_artifacts: bool,
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("invalid bundle name '{}': must be 1–64 characters", name);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        bail!(
            "invalid bundle name '{}': must contain only lowercase letters, digits, underscore, or hyphen",
            name
        );
    }
    Ok(())
}

/// What a bundle install acts under. The live or standalone service takes it
/// from its state; the daemon's startup, applying a signed
/// `node.bundle.replace`, composes it from the stopped node's configuration.
pub struct InstallAuthority<'a> {
    pub app_root: &'a Path,
    /// The node's signing key, which signs the bundle registration.
    pub node_signing_key: &'a lillux::crypto::SigningKey,
    pub node_trust_store: &'a TrustStore,
    pub prospective_validator: &'a ryeos_app::prospective_admission::ProspectiveNodeConfigValidator,
    pub isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
    /// The state lock the registration reseal is published under.
    pub state_lock: &'a ryeos_node_runtime::state_lock::StateLockLease,
    /// Called whenever a new bundle generation may be visible, so a process
    /// that caches engines drops them.
    pub generation_changed: &'a dyn Fn(&str),
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let prospective_validator = state
        .extensions
        .get::<ryeos_app::prospective_admission::ProspectiveNodeConfigValidator>()
        .context("prospective node-config validator is not installed at the composition root")?;
    let bump = |reason: &str| {
        let new_gen = state.engine_cache.bump_system_install_generation();
        tracing::info!(
            bundle = %req.name,
            engine_cache_generation = new_gen,
            reason,
            "bumped engine cache generation"
        );
    };
    install(
        &req,
        &InstallAuthority {
            app_root: &state.config.app_root,
            node_signing_key: state.identity.signing_key(),
            node_trust_store: &state.engine.node_trust_store,
            prospective_validator: &prospective_validator,
            isolation: Arc::clone(&state.isolation),
            state_lock: &state.controller_lifetime,
            generation_changed: &bump,
        },
    )
}

/// Install or replace one bundle from a source directory under `authority`.
pub fn install(req: &Request, authority: &InstallAuthority<'_>) -> Result<Value> {
    validate_name(&req.name)?;

    if !req.source_path.is_dir() {
        bail!(
            "source_path is not a directory: {}",
            req.source_path.display()
        );
    }

    let app_root = authority.app_root;
    let bundles_root = app_root.join(".ai").join("bundles");
    let target = bundles_root.join(&req.name);
    // Keep the exact installed-set read, admission, and namespace mutation in
    // one node-wide critical section. The registry lock always precedes the
    // per-name transaction lock.
    let registry_lock =
        ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(app_root)?;
    let transaction = ryeos_app::bundle_transaction::BundleMutationTransaction::acquire(
        &registry_lock,
        &req.name,
    )?;
    let recovered = match transaction.reconcile(authority.node_signing_key) {
        Ok(recovered) => recovered,
        Err(error) => {
            // Reconciliation may finish the tree/registration mutation and
            // then fail while durably removing its journal. Invalidate before
            // returning so a namespace-visible repair never leaves old engines
            // cached in this daemon process.
            (authority.generation_changed)("reconcile failed");
            return Err(error).context("reconcile interrupted bundle transaction");
        }
    };
    if recovered.is_some() {
        (authority.generation_changed)("reconciled bundle transaction");
    }
    if matches!(
        recovered,
        Some(
            ryeos_app::bundle_transaction::BundleOperation::Install
                | ryeos_app::bundle_transaction::BundleOperation::RemoteInstall
        )
    ) && transaction.target().is_dir()
        && !req.replace
    {
        return Ok(serde_json::json!({
            "name": req.name,
            "path": transaction.target(),
            "recovered": true,
        }));
    }

    let replaced = target.exists();
    if replaced && !req.replace {
        bail!(
            "bundle '{}' already installed at {}; use --replace to update it from source",
            req.name,
            target.display()
        );
    }

    // Trust comes only from the node's persistent trust store. Admission runs
    // against the completed staging tree below, never the mutable source tree.

    fs::create_dir_all(&bundles_root)
        .with_context(|| format!("failed to create bundles root {}", bundles_root.display()))?;

    let admit = |staging: &Path, replace: bool| {
        admit_completed_staging(
            app_root,
            &req.name,
            staging,
            replace,
            authority.node_trust_store,
            authority.prospective_validator,
            Arc::clone(&authority.isolation),
        )
    };
    let registration = serde_json::json!({ "kind": "node", "path": target });
    let activation = if replaced {
        replace_dir_atomic(
            &req.source_path,
            &target,
            req.preserve_runtime_artifacts,
            &transaction,
            registration.clone(),
            |staging| {
                admit(staging, true).context("admission refused completed replacement staging tree")
            },
        )
        .with_context(|| {
            format!(
                "failed to replace bundle from {} to {}",
                req.source_path.display(),
                target.display()
            )
        })
    } else {
        install_dir_atomic(
            &req.source_path,
            &target,
            &transaction,
            registration.clone(),
            |staging| {
                admit(staging, false).context("admission refused completed install staging tree")
            },
        )
        .with_context(|| {
            format!(
                "failed to install bundle from {} to {}",
                req.source_path.display(),
                target.display()
            )
        })
    };

    // Invalidate cached engines as soon as a generation may be visible. This
    // deliberately happens before propagating a post-rename/exchange journal
    // error: activation can succeed even when `mark_activated` or its durable
    // journal write fails. A conservative bump for a failed replacement that
    // left the old target unchanged is harmless.
    if target.is_dir() {
        (authority.generation_changed)("bundle namespace observed after activation attempt");
    }
    activation?;

    let canonical_target = target
        .canonicalize()
        .context("failed to canonicalize installed bundle path")?;

    // Write signed kind: node bundle registration
    let config_item_path = transaction
        .commit_present(authority.node_signing_key)
        .context("commit bundle registration")?;
    // The registration set is pinned by the signed init completion record;
    // reseal it so the node with this bundle is startable and the next
    // stopped-node operation is admitted.
    ryeos_node::seal_init_completion_after_bundle_registration_change(
        app_root,
        authority.state_lock,
    )
    .context("commit bundle registration into initialized state")?;

    let report = serde_json::json!({
        "name": req.name,
        "path": canonical_target.display().to_string(),
        "config_item": config_item_path.display().to_string(),
        "replaced": replaced,
        "preserve_runtime_artifacts": req.preserve_runtime_artifacts,
    });
    Ok(report)
}

/// Most bytes one bundle archive may expand to.
pub const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 64 * 1024 * 1024;
/// Most entries one bundle archive may hold.
pub const MAX_ARCHIVE_ENTRIES: usize = 8192;

/// Install the bundle a gzip-compressed tar holds under `name`, replacing the
/// installed bundle of that name, with the same verification as
/// `bundle install --replace`. The archive holds one top-level directory
/// named after the bundle (`tar czf site.tgz site`), and only directories and
/// regular files below it.
pub fn replace_from_archive(
    name: &str,
    archive: &[u8],
    preserve_runtime_artifacts: bool,
    authority: &InstallAuthority<'_>,
) -> Result<Value> {
    validate_name(name)?;
    let bundles_root = authority
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("bundles");
    fs::create_dir_all(&bundles_root)
        .with_context(|| format!("failed to create bundles root {}", bundles_root.display()))?;
    let extraction = bundles_root.join(format!(".{name}.archive-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&extraction)
        .with_context(|| format!("create archive extraction {}", extraction.display()))?;
    let result = extract_bundle_archive(archive, name, &extraction).and_then(|()| {
        install(
            &Request {
                name: name.to_owned(),
                source_path: extraction.join(name),
                replace: true,
                preserve_runtime_artifacts,
            },
            authority,
        )
    });
    if let Err(error) = fs::remove_dir_all(&extraction) {
        tracing::warn!(
            path = %extraction.display(),
            %error,
            "bundle archive extraction was not removed"
        );
    }
    result
}

/// Extract a bundle archive below `into`, refusing anything but directories
/// and regular files at plain relative paths under one top-level directory
/// named `name`, and anything past the entry and size bounds.
pub fn extract_bundle_archive(archive: &[u8], name: &str, into: &Path) -> Result<()> {
    use std::io::Read as _;
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder.take(MAX_ARCHIVE_EXPANDED_BYTES + 1));
    let mut entries = 0usize;
    let mut expanded = 0u64;
    let mut saw_root = false;
    for entry in tar
        .entries()
        .context("bundle archive is not a gzip-compressed tar")?
    {
        let mut entry = entry.context("read a bundle archive entry")?;
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            bail!("bundle archive holds more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        let path = entry
            .path()
            .context("bundle archive entry path is not valid")?
            .into_owned();
        let mut components = Vec::new();
        for component in path.components() {
            match component {
                std::path::Component::Normal(part) => components.push(part.to_owned()),
                std::path::Component::CurDir if components.is_empty() => {}
                _ => bail!(
                    "bundle archive entry `{}` is not a plain relative path",
                    path.display()
                ),
            }
        }
        if components.first().and_then(|first| first.to_str()) != Some(name) {
            bail!(
                "bundle archive entry `{}` is not under the top-level directory `{name}`",
                path.display()
            );
        }
        let relative: PathBuf = components.iter().collect();
        let destination = into.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            saw_root |= components.len() == 1;
            fs::create_dir_all(&destination)
                .with_context(|| format!("create {}", destination.display()))?;
        } else if kind.is_file() {
            if components.len() == 1 {
                bail!("bundle archive top-level entry `{name}` is not a directory");
            }
            let size = entry.header().size().context("bundle archive entry size")?;
            expanded = expanded.saturating_add(size);
            if expanded > MAX_ARCHIVE_EXPANDED_BYTES {
                bail!("bundle archive expands past {MAX_ARCHIVE_EXPANDED_BYTES} bytes");
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .with_context(|| {
                    format!(
                        "bundle archive names `{}` twice or it cannot be written",
                        relative.display()
                    )
                })?;
            let copied = std::io::copy(&mut (&mut entry).take(size), &mut file)
                .with_context(|| format!("write {}", destination.display()))?;
            if copied != size {
                bail!("bundle archive entry `{}` is truncated", relative.display());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = entry.header().mode().unwrap_or(0o644);
                let mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                fs::set_permissions(&destination, fs::Permissions::from_mode(mode))?;
            }
        } else {
            bail!(
                "bundle archive entry `{}` is a link or special file; a bundle archive holds only directories and regular files",
                path.display()
            );
        }
    }
    if !saw_root && !into.join(name).is_dir() {
        bail!("bundle archive holds no top-level directory `{name}`");
    }
    Ok(())
}

/// Build the exact post-operation bundle graph used for dependency closure and
/// prospective boot admission.
pub(crate) fn build_prospective_bundle_plan(
    app_root: &Path,
    bundle_name: &str,
    candidate_root: &Path,
    replace: bool,
) -> Result<BundlePlan> {
    let candidate_root = candidate_root.canonicalize().with_context(|| {
        format!(
            "canonicalize prospective bundle root {}",
            candidate_root.display()
        )
    })?;
    let installed = ryeos_bundle::installed::load_installed_plan_inputs(app_root)
        .context("load verified installed bundle graph")?;
    let candidate = PlanInput {
        name: bundle_name.to_string(),
        source: BundleSource::SourceDir(candidate_root),
    };
    let mode = if replace {
        BundlePlanMode::Replace
    } else {
        BundlePlanMode::Install
    };
    build_plan(mode, &[candidate], &installed).context("resolve prospective bundle graph")
}

/// Verify only the candidate using the planner's exact transitive dependency
/// closure, never the ambient set of every installed bundle.
pub(crate) fn verify_planned_candidate(
    plan: &BundlePlan,
    bundle_name: &str,
    app_root: &Path,
    completed_staging: bool,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
) -> Result<()> {
    let job = plan
        .verification_jobs
        .iter()
        .find(|job| {
            job.subject == bundle_name
                && job.subject_kind == VerificationSubjectKind::CandidateSource
        })
        .with_context(|| {
            format!(
                "prospective plan has no candidate verification job for '{}'",
                bundle_name
            )
        })?;

    if completed_staging {
        preflight_verify_bundle_staging_in_context(
            &job.subject_root,
            bundle_name,
            &job.dependency_roots,
            app_root,
            isolation,
        )
    } else {
        preflight_verify_named_bundle_in_context(
            &job.subject_root,
            bundle_name,
            &job.dependency_roots,
            app_root,
            isolation,
        )
    }
}

/// Re-plan the completed staging generation, verify it with the exact closure,
/// then run the same node-owned registry/executable admission used at boot.
// This admission boundary deliberately keeps every verified authority explicit.
pub(crate) fn admit_completed_staging(
    app_root: &Path,
    bundle_name: &str,
    staging: &Path,
    replace: bool,
    node_trust_store: &TrustStore,
    prospective_validator: &ryeos_app::prospective_admission::ProspectiveNodeConfigValidator,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
) -> Result<()> {
    let plan = build_prospective_bundle_plan(app_root, bundle_name, staging, replace)?;
    verify_planned_candidate(&plan, bundle_name, app_root, true, Arc::clone(&isolation))?;
    // Exercise the second boot phase too: bundle-contributed node config is
    // scanned from the prospective roots and policy-directory collisions are
    // rejected before activation. Command grants come only from the exact
    // node-policy generation, never from bundle registrations.
    let loader = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
        app_root,
        trust_store: node_trust_store,
    };
    let mut current_records: std::collections::BTreeMap<
        String,
        ryeos_node_runtime::node_config_source::BundleRecord,
    > = loader
        .load_bundle_section()
        .context("load current node bundle registrations for prospective admission")?
        .into_iter()
        .map(|record| (record.name.clone(), record))
        .collect();
    let prospective_records = plan
        .bundles
        .iter()
        .map(|(name, bundle)| {
            if name == bundle_name {
                if replace {
                    current_records.remove(name).with_context(|| {
                        format!(
                            "replacement bundle '{}' has no verified current registration",
                            name
                        )
                    })?;
                }
                Ok(ryeos_node_runtime::node_config_source::BundleRecord {
                    name: name.clone(),
                    path: bundle.source.root_path().clone(),
                    source_file: app_root
                        .join(ryeos_engine::AI_DIR)
                        .join("node/bundles")
                        .join(format!("{name}.yaml")),
                })
            } else {
                current_records.remove(name).with_context(|| {
                    format!(
                        "prospective bundle '{}' has no verified current registration",
                        name
                    )
                })
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let config_table = ryeos_app::node_config::NodeConfigTable::new();
    let policy_table = ryeos_node_runtime::node_policy::NodePolicyTable::new();
    let policy_snapshot =
        ryeos_node_runtime::node_policy::load_snapshot(app_root, node_trust_store, &policy_table)
            .context("load exact node policy generation for prospective admission")?;
    let command_registration = policy_snapshot.require::<
        ryeos_node_runtime::node_policy::sections::command_registration::CommandRegistrationAuthority,
    >()?;
    let snapshot = ryeos_app::node_config::loader::NodeConfigLoader {
        app_root,
        trust_store: node_trust_store,
    }
    .load_full(
        &config_table,
        &prospective_records,
        command_registration,
        &policy_table,
    )
    .context("prospective bundle set would fail full node-config boot")?;
    prospective_validator
        .validate(&snapshot)
        .context("prospective bundle set would fail composed node-config admission")?;
    let prospective_roots: Vec<PathBuf> = plan
        .bundles
        .values()
        .map(|bundle| bundle.source.root_path().clone())
        .collect();
    ryeos_node_runtime::engine_init::admit_node_bundle_roots(
        app_root,
        &prospective_roots,
        node_trust_store,
        policy_snapshot.require::<ryeos_engine::isolation::IsolationPolicy>()?,
        policy_snapshot.generation_digest(),
    )
    .context("prospective bundle set failed node definition admission")?;
    Ok(())
}

fn install_dir_atomic<F>(
    src: &Path,
    target: &Path,
    transaction: &ryeos_app::bundle_transaction::BundleTransaction,
    registration: serde_json::Value,
    verify_staged: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let source = src
        .canonicalize()
        .with_context(|| format!("canonicalize source path {}", src.display()))?;
    let parent = target
        .parent()
        .context("installed bundle target has no parent")?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("installed bundle target has no valid directory name")?;
    let staging = parent.join(format!(".{name}.staging"));
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("canonicalize bundle parent {}", parent.display()))?;
    let canonical_target = canonical_parent.join(name);
    let canonical_staging = canonical_parent.join(format!(".{name}.staging"));
    if source == canonical_target
        || source.starts_with(&canonical_target)
        || canonical_target.starts_with(&source)
    {
        bail!(
            "source_path {} must be outside installed bundle target {}",
            source.display(),
            canonical_target.display()
        );
    }
    if source == canonical_staging
        || source.starts_with(&canonical_staging)
        || canonical_staging.starts_with(&source)
    {
        bail!(
            "source_path {} must be separate from bundle install staging path {}",
            source.display(),
            canonical_staging.display()
        );
    }
    let stale_staging = match fs::symlink_metadata(&staging) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "bundle install staging path is not a real directory: {}",
                    staging.display()
                );
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect bundle install staging {}", staging.display()));
        }
    };
    (|| {
        match fs::symlink_metadata(target) {
            Ok(_) => {
                bail!(
                    "bundle target appeared during install: {}",
                    target.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect bundle target {}", target.display()));
            }
        }
        if stale_staging {
            fs::remove_dir_all(&staging)
                .with_context(|| format!("remove stale staging dir {}", staging.display()))?;
        }
        copy_dir_recursive(&source, &staging).with_context(|| {
            format!(
                "copy bundle from {} to staging {}",
                source.display(),
                staging.display()
            )
        })?;
        lillux::sync_tree_durable(&staging)
            .with_context(|| format!("flush staged bundle {}", staging.display()))?;
        verify_staged(&staging)?;
        transaction.begin_present(
            ryeos_app::bundle_transaction::BundleOperation::Install,
            &staging,
            registration,
        )?;
        if let Err(error) = lillux::rename_path_durable(&staging, target) {
            if error.namespace_committed() {
                tracing::warn!(
                    %error,
                    target = %target.display(),
                    "bundle install is visible but its parent durability barrier failed"
                );
            } else {
                return Err(error).with_context(|| {
                    format!(
                        "activate staged bundle {} at {}",
                        staging.display(),
                        target.display()
                    )
                });
            }
        }
        transaction.mark_activated()
    })()
}

fn replace_dir_atomic<F>(
    src: &Path,
    target: &Path,
    preserve_runtime_artifacts: bool,
    transaction: &ryeos_app::bundle_transaction::BundleTransaction,
    registration: serde_json::Value,
    verify_staged: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let source = src
        .canonicalize()
        .with_context(|| format!("canonicalize source path {}", src.display()))?;
    let canonical_target = target
        .canonicalize()
        .with_context(|| format!("canonicalize target path {}", target.display()))?;
    let parent = target
        .parent()
        .context("installed bundle target has no parent")?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("installed bundle target has no valid directory name")?;
    let staging = parent.join(format!(".{name}.staging"));
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("canonicalize bundle parent {}", parent.display()))?;
    let canonical_staging = canonical_parent.join(format!(".{name}.staging"));
    if source == canonical_target
        || source.starts_with(&canonical_target)
        || canonical_target.starts_with(&source)
    {
        bail!(
            "source_path {} must be outside installed bundle target {} when using --replace",
            source.display(),
            canonical_target.display()
        );
    }
    if source == canonical_staging
        || source.starts_with(&canonical_staging)
        || canonical_staging.starts_with(&source)
    {
        bail!(
            "source_path {} must be separate from bundle replacement staging path {}",
            source.display(),
            canonical_staging.display()
        );
    }
    let stale_staging = match fs::symlink_metadata(&staging) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "bundle replacement staging path is not a real directory: {}",
                    staging.display()
                );
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect bundle replacement staging {}", staging.display())
            });
        }
    };

    (|| {
        if stale_staging {
            fs::remove_dir_all(&staging)
                .with_context(|| format!("remove stale staging dir {}", staging.display()))?;
        }
        copy_dir_recursive(&source, &staging).with_context(|| {
            format!(
                "copy replacement bundle from {} to staging {}",
                source.display(),
                staging.display(),
            )
        })?;
        if preserve_runtime_artifacts {
            preserve_runtime_artifact_dirs(&canonical_target, &staging)?;
        }
        lillux::sync_tree_durable(&staging)
            .with_context(|| format!("flush staged bundle {}", staging.display()))?;
        verify_staged(&staging)?;
        transaction.begin_present(
            ryeos_app::bundle_transaction::BundleOperation::Replace,
            &staging,
            registration,
        )?;
        if let Err(error) = lillux::atomic_exchange_paths(target, &staging) {
            if error.namespace_committed() {
                tracing::warn!(
                    %error,
                    target = %target.display(),
                    "bundle replacement is visible but its parent durability barrier failed"
                );
            } else {
                return Err(error).with_context(|| {
                    format!(
                        "atomically exchange installed bundle {} with {}",
                        target.display(),
                        staging.display()
                    )
                });
            }
        }
        transaction.mark_activated()?;
        if let Err(error) = lillux::remove_dir_all_durable(&staging) {
            tracing::warn!(
                path = %staging.display(),
                error = %format!("{error:#}"),
                "bundle replacement committed but previous generation cleanup failed"
            );
        }
        Ok(())
    })()
}

fn preserve_runtime_artifact_dirs(existing: &Path, staging: &Path) -> Result<()> {
    for rel in [".ai/bin", ".ai/objects", ".ai/refs"] {
        let from = existing.join(rel);
        if !from.exists() {
            continue;
        }
        let to = staging.join(rel);
        if to.exists() {
            continue;
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create runtime artifact parent {}", parent.display()))?;
        }
        copy_dir_recursive(&from, &to).with_context(|| {
            format!(
                "preserve runtime artifact directory {} -> {}",
                from.display(),
                to.display()
            )
        })?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("failed to create {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))? {
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
                .with_context(|| format!("failed to symlink {}", to.display()))?;
            #[cfg(not(unix))]
            {
                let _ = link_target;
                bail!("symlinks unsupported on this platform: {}", from.display());
            }
        } else {
            fs::copy(&from, &to).with_context(|| {
                format!("failed to copy {} -> {}", from.display(), to.display())
            })?;
        }
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/install",
    endpoint: "bundle.install",
    availability: ServiceAvailability::StoppedNodeOnly,
    required_caps: &["ryeos.execute.service.bundle/install"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .context("bundle.install requires { name, source_path, replace? }")?;
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    fn replace_for_test(src: &Path, target: &Path, preserve: bool) {
        let app_root = target.ancestors().nth(3).unwrap();
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(app_root).unwrap();
        let transaction = ryeos_app::bundle_transaction::BundleMutationTransaction::acquire(
            &registry_lock,
            target.file_name().unwrap().to_str().unwrap(),
        )
        .unwrap();
        replace_dir_atomic(
            src,
            target,
            preserve,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |_| Ok(()),
        )
        .unwrap();
    }

    fn tar_gz(entries: &[(&str, Option<&[u8]>, tar::EntryType)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (path, content, kind) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(0o644);
            let bytes = content.unwrap_or_default();
            header.set_size(bytes.len() as u64);
            if *kind == tar::EntryType::Symlink {
                header.set_link_name("../../etc/passwd").unwrap();
            }
            // Written raw so a hostile path reaches the extractor unchanged.
            let name = path.as_bytes();
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_cksum();
            builder.append(&header, bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn a_bundle_archive_extracts_its_one_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tar_gz(&[
            ("site/", None, tar::EntryType::Directory),
            (
                "site/.ai/manifest.yaml",
                Some(b"name: site\n"),
                tar::EntryType::Regular,
            ),
        ]);
        extract_bundle_archive(&archive, "site", tmp.path()).unwrap();
        assert_eq!(
            fs::read(tmp.path().join("site/.ai/manifest.yaml")).unwrap(),
            b"name: site\n"
        );
    }

    #[test]
    fn bundle_replace_operation_refuses_unverified_archive() {
        for (entries, needle) in [
            (
                vec![("site/../escape", Some(&b"x"[..]), tar::EntryType::Regular)],
                "not a plain relative path",
            ),
            (
                vec![("/etc/site", Some(&b"x"[..]), tar::EntryType::Regular)],
                "not a plain relative path",
            ),
            (
                vec![("other/file", Some(&b"x"[..]), tar::EntryType::Regular)],
                "not under the top-level directory",
            ),
            (
                vec![("site/link", None, tar::EntryType::Symlink)],
                "link or special file",
            ),
            (
                vec![("site", Some(&b"x"[..]), tar::EntryType::Regular)],
                "not a directory",
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let error = extract_bundle_archive(&tar_gz(&entries), "site", tmp.path()).unwrap_err();
            assert!(format!("{error:#}").contains(needle), "{error:#}");
            assert!(!tmp.path().join("escape").exists());
        }
        let tmp = tempfile::tempdir().unwrap();
        let error = extract_bundle_archive(b"not gzip", "site", tmp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("bundle archive"), "{error:#}");
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_slashes() {
        assert!(validate_name("foo/bar").is_err());
    }

    #[test]
    fn validate_name_rejects_dots() {
        assert!(validate_name("foo.bar").is_err());
    }

    #[test]
    fn validate_name_rejects_uppercase() {
        assert!(validate_name("Foo").is_err());
    }

    #[test]
    fn validate_name_rejects_spaces() {
        assert!(validate_name("foo bar").is_err());
    }

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_name("my-bundle_v2").is_ok());
        assert!(validate_name("core").is_ok());
        assert!(validate_name("knowledge").is_ok());
    }

    #[test]
    fn copy_dir_copies_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("top.txt"), b"top").unwrap();
        fs::write(src.join("a/mid.txt"), b"mid").unwrap();
        fs::write(src.join("a/b/leaf.txt"), b"leaf").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"top");
        assert_eq!(fs::read(dst.join("a/mid.txt")).unwrap(), b"mid");
        assert_eq!(fs::read(dst.join("a/b/leaf.txt")).unwrap(), b"leaf");
    }

    #[test]
    fn replace_dir_replaces_existing_tree_and_cleans_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join(".ai/bundles");
        let target = bundles.join("ryeos-ui");
        fs::create_dir_all(src.join("new")).unwrap();
        fs::create_dir_all(target.join("old")).unwrap();
        fs::write(src.join("new/file.txt"), b"new").unwrap();
        fs::write(target.join("old/file.txt"), b"old").unwrap();

        replace_for_test(&src, &target, false);

        assert_eq!(fs::read(target.join("new/file.txt")).unwrap(), b"new");
        assert!(!target.join("old/file.txt").exists());
        assert!(!bundles.join(".ryeos-ui.staging").exists());
    }

    #[test]
    fn replace_dir_preserves_runtime_artifacts_when_source_is_sparse() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join(".ai/bundles");
        let target = bundles.join("ryeos-ui");
        fs::create_dir_all(src.join(".ai/node/commands")).unwrap();
        fs::create_dir_all(target.join(".ai/bin/x86_64-unknown-linux-gnu")).unwrap();
        fs::create_dir_all(target.join(".ai/objects/blobs")).unwrap();
        fs::create_dir_all(target.join(".ai/refs/bundles")).unwrap();
        fs::write(src.join(".ai/node/commands/web.yaml"), b"web").unwrap();
        fs::write(target.join(".ai/bin/x86_64-unknown-linux-gnu/web"), b"bin").unwrap();
        fs::write(target.join(".ai/objects/blobs/blob"), b"blob").unwrap();
        fs::write(target.join(".ai/refs/bundles/manifest"), b"ref").unwrap();

        replace_for_test(&src, &target, true);

        assert_eq!(
            fs::read(target.join(".ai/node/commands/web.yaml")).unwrap(),
            b"web"
        );
        assert_eq!(
            fs::read(target.join(".ai/bin/x86_64-unknown-linux-gnu/web")).unwrap(),
            b"bin"
        );
        assert_eq!(
            fs::read(target.join(".ai/objects/blobs/blob")).unwrap(),
            b"blob"
        );
        assert_eq!(
            fs::read(target.join(".ai/refs/bundles/manifest")).unwrap(),
            b"ref"
        );
    }

    #[test]
    fn replacement_validates_completed_staging_before_activation() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join(".ai/bundles");
        let target = bundles.join("demo");
        fs::create_dir_all(src.join(".ai/node/commands")).unwrap();
        fs::create_dir_all(target.join(".ai/refs/bundles")).unwrap();
        fs::write(src.join(".ai/node/commands/new.yaml"), b"new").unwrap();
        fs::write(target.join(".ai/refs/bundles/manifest"), b"preserved").unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(tmp.path()).unwrap();
        let transaction = ryeos_app::bundle_transaction::BundleMutationTransaction::acquire(
            &registry_lock,
            "demo",
        )
        .unwrap();
        let validation_called = std::cell::Cell::new(false);

        let error = replace_dir_atomic(
            &src,
            &target,
            true,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |staging| {
                validation_called.set(true);
                assert_eq!(
                    fs::read(staging.join(".ai/refs/bundles/manifest")).unwrap(),
                    b"preserved"
                );
                anyhow::bail!("completed staging rejected")
            },
        )
        .expect_err("failed completed-staging preflight must prevent activation");

        assert!(validation_called.get());
        assert!(error.to_string().contains("completed staging rejected"));
        assert_eq!(fs::read(target.join("old.txt")).unwrap(), b"old");
        assert!(!target.join(".ai/node/commands/new.yaml").exists());
    }

    #[test]
    fn replace_dir_does_not_overwrite_source_runtime_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join(".ai/bundles");
        let target = bundles.join("ryeos-ui");
        fs::create_dir_all(src.join(".ai/bin/x86_64-unknown-linux-gnu")).unwrap();
        fs::create_dir_all(target.join(".ai/bin/x86_64-unknown-linux-gnu")).unwrap();
        fs::write(src.join(".ai/bin/x86_64-unknown-linux-gnu/web"), b"new").unwrap();
        fs::write(target.join(".ai/bin/x86_64-unknown-linux-gnu/web"), b"old").unwrap();

        replace_for_test(&src, &target, true);

        assert_eq!(
            fs::read(target.join(".ai/bin/x86_64-unknown-linux-gnu/web")).unwrap(),
            b"new"
        );
    }
}
