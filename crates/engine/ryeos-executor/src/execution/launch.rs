use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use rand::Rng;
use serde_json::{json, Value};

use super::arch_check;
use super::launch_claim::{ThreadLaunchClaim, ThreadLaunchClaimOutcome};
use super::launch_envelope::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, HardLimits, LaunchEnvelope,
    LaunchEnvelopeBuilder, RuntimeResult,
};
use super::limits::{
    apply_caller_limit_overrides, apply_execution_policy_overrides, compute_effective_limits,
    load_limits_config_from_loader, merge_header_limits,
};
use super::thread_meta::ThreadMeta;
use crate::dispatch_error::DispatchError;
use ryeos_app::callback_token::{effective_bundle_id_for_request, launch_token_ttl};
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::{ResolvedExecutionRequest, ThreadFinalizeParams};
use ryeos_app::vault::VaultReadError;
use ryeos_runtime::checkpoint::{
    checkpoint_shape_limits, validate_checkpoint_shape, FanoutItemStatus,
};
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::RuntimeJsonArrayBudget;

mod runtime_request;
mod terminal;

use runtime_request::{spawn_runtime, SpawnRuntimeParams};
use terminal::{
    fallback_finalization, is_thread_terminal_status, reconcile_terminal_finalization,
    runtime_terminal_status,
};

/// Typed error for native executor materialization failures.
///
/// Raised by [`materialize_native_executor`] when the bundle CAS
/// cannot supply the requested binary. The daemon's `dispatch.rs`
/// maps this to `DispatchError::RuntimeMaterializationFailed` with
/// a 502 status — no string-classifier anywhere.
#[derive(Debug, thiserror::Error)]
pub enum MaterializationError {
    #[error("native executor '{executor_ref}' not available: {detail}")]
    ExecutorUnavailable {
        executor_ref: String,
        detail: String,
    },
    #[error("bundle manifest error: {0}")]
    ManifestError(String),
    #[error("executor resolution failed for '{executor_ref}': {detail}")]
    ResolutionFailed {
        executor_ref: String,
        detail: String,
    },
    #[error("binary blob '{hash}' not found in system CAS")]
    BlobNotFound { hash: String },
    #[error("arch check failed for '{executor_ref}': {detail}")]
    ArchCheckFailed {
        executor_ref: String,
        detail: String,
    },
    #[error("executor materialization failed for '{executor_ref}': {detail}")]
    MaterializationFailed {
        executor_ref: String,
        detail: String,
    },
    #[error(
        "executor '{executor_ref}' failed trust check (class={trust_class:?}, fp={fingerprint:?})"
    )]
    ExecutorUntrusted {
        executor_ref: String,
        trust_class: ryeos_engine::resolution::TrustClass,
        fingerprint: Option<String>,
    },
    #[error("{0}")]
    Internal(String),
}

#[derive(Debug, Clone)]
pub struct MaterializedExecutor {
    pub path: PathBuf,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub enum SecretSource {
    Metadata,
    LaunchPreparation { origin: String },
}

impl SecretSource {
    pub fn kind_for_wire(&self) -> &'static str {
        match self {
            SecretSource::Metadata => "declared",
            SecretSource::LaunchPreparation { .. } => "launch_preparation",
        }
    }

    pub fn name_for_wire(&self) -> String {
        match self {
            SecretSource::Metadata => "item metadata".to_string(),
            SecretSource::LaunchPreparation { origin } => origin.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SecretRequirement {
    pub name: String,
    pub sources: Vec<SecretSource>,
}

#[derive(Debug, Clone)]
pub struct MissingSecret {
    pub name: String,
    pub sources: Vec<SecretSource>,
}

impl MissingSecret {
    pub fn primary_source(&self) -> &SecretSource {
        &self.sources[0]
    }
}

pub(crate) fn build_secret_requirements(
    metadata_required_secrets: &[String],
) -> Vec<SecretRequirement> {
    metadata_required_secrets
        .iter()
        .map(|name| SecretRequirement {
            name: name.clone(),
            sources: vec![SecretSource::Metadata],
        })
        .collect()
}

fn merge_prepared_secret_requirements(
    requirements: &mut Vec<SecretRequirement>,
    prepared: &[super::launch_preparation::PreparedSecret],
) -> Result<(), BuildAndLaunchError> {
    for secret in prepared {
        let origin_value = serde_json::to_value(&secret.origin)?;
        let source = SecretSource::LaunchPreparation {
            origin: lillux::canonical_json(&origin_value).map_err(|error| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "canonicalize prepared secret origin: {error}"
                ))
            })?,
        };
        if let Some(existing) = requirements
            .iter_mut()
            .find(|item| item.name == secret.name)
        {
            existing.sources.push(source);
        } else {
            requirements.push(SecretRequirement {
                name: secret.name.clone(),
                sources: vec![source],
            });
        }
    }
    Ok(())
}

pub(crate) fn missing_secrets_from_requirements(
    missing_names: &[String],
    requirements: &[SecretRequirement],
) -> Vec<MissingSecret> {
    missing_names
        .iter()
        .filter_map(|name| {
            requirements
                .iter()
                .find(|req| &req.name == name)
                .map(|req| MissingSecret {
                    name: req.name.clone(),
                    sources: req.sources.clone(),
                })
        })
        .collect()
}

pub(crate) fn required_secret_missing_payload(
    item_ref: &str,
    missing: &MissingSecret,
) -> serde_json::Value {
    let source = missing.primary_source();
    crate::structured_error::StructuredErrorPayload::required_secret_missing(
        format!(
            "missing required secret `{}` for `{}`",
            missing.name, item_ref
        ),
        missing.name.clone(),
        source.kind_for_wire(),
        source.name_for_wire(),
        crate::dispatch_error::required_secret_remediation(&missing.name),
    )
    .to_value()
}

/// Typed error returned by [`build_and_launch`]. Materialization
/// failures carry a stable variant; everything else is `Internal`.
#[derive(Debug, thiserror::Error)]
pub enum BuildAndLaunchError {
    #[error("materialization failed: {0}")]
    Materialization(#[from] MaterializationError),
    #[error("missing required secret(s) for `{item_ref}`")]
    MissingSecrets {
        item_ref: String,
        secrets: Vec<MissingSecret>,
    },
    /// A composed permission tried to self-grant manifest runtime authority
    /// (bundle events / vault). Mapped to `DispatchError::CapabilityRejected`.
    #[error("{reason}")]
    CapabilityRejected { reason: String },
    #[error("{0}")]
    LaunchPreparation(#[source] Box<DispatchError>),
    #[error("{0}")]
    Internal(#[from] anyhow::Error),
}

impl From<DispatchError> for BuildAndLaunchError {
    fn from(error: DispatchError) -> Self {
        Self::LaunchPreparation(Box::new(error))
    }
}

impl BuildAndLaunchError {
    /// Whether a launch failure is an infrastructure interruption that is safe
    /// to re-drive without changing the authored execution. Keep this deliberately
    /// narrow: capability, secret, materialization, and unknown failures are
    /// deterministic until proven otherwise.
    fn retryable_launch_interruption(&self) -> bool {
        match self {
            Self::Internal(error) => error.chain().any(|cause| {
                cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                    )
                })
            }),
            Self::Materialization(_)
            | Self::MissingSecrets { .. }
            | Self::CapabilityRejected { .. } => false,
            Self::LaunchPreparation(error) => error.retryable(),
        }
    }
}

impl From<serde_json::Error> for BuildAndLaunchError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(anyhow::anyhow!(e))
    }
}

impl From<std::io::Error> for BuildAndLaunchError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(anyhow::anyhow!(e))
    }
}

/// Host triple for native executor resolution.
///
/// Returns the rustc target triple this daemon was compiled for (e.g.
/// `x86_64-unknown-linux-gnu`), as captured at build time by `crates/bin/daemon/build.rs`
/// from Cargo's `TARGET` environment variable. This is identical to
/// `rustc -vV | grep ^host:` for a native build, which is the convention the
/// build-bundle pipeline uses when writing `bin/<triple>/<name>` into bundle
/// manifests (see `crates/tools/core-tools/tests/build_bundle_smoke.rs` and
/// `bundles/standard/.ai/bin/<triple>/`).
///
/// Using the compile-time `TARGET` (as opposed to a hand-built
/// `ARCH-VENDOR-OS` string) guarantees the daemon's lookup key matches the
/// path the bundle was built for — including the ABI segment (`gnu`, `musl`,
/// `msvc`) that hand-coding would otherwise omit.
fn host_triple() -> String {
    env!("RYEOSD_HOST_TRIPLE").to_string()
}

/// Ref path under `.ai/` that stores the system bundle manifest hash.
/// PR1b2 writes this ref during bundle build.
const BUNDLE_MANIFEST_REF: &str = "refs/bundles/manifest";

/// Content-addressed cache target for a native executor binary.
///
/// Returns `<cache_root>/cache/executors/<blob_hash>/<bare>`.
fn executor_cache_target(cache_root: &Path, blob_hash: &str, bare: &str) -> PathBuf {
    cache_root
        .join("cache")
        .join("executors")
        .join(blob_hash)
        .join(bare)
}

fn verify_materialized_executor_artifact(
    target_path: &Path,
    expected_hash: &str,
    expected_mode: u32,
    executor_ref: &str,
) -> Result<(), MaterializationError> {
    let metadata = std::fs::symlink_metadata(target_path).map_err(|error| {
        MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!(
                "failed to stat materialized executor {}: {error}",
                target_path.display()
            ),
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!(
                "materialized executor {} must be a regular, non-symlink file",
                target_path.display()
            ),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let actual_mode = metadata.permissions().mode() & 0o7777;
        if actual_mode & !0o777 != 0 {
            return Err(MaterializationError::MaterializationFailed {
                executor_ref: executor_ref.to_string(),
                detail: format!(
                    "materialized executor {} has forbidden special permission bits ({actual_mode:#o})",
                    target_path.display()
                ),
            });
        }
        if actual_mode != expected_mode {
            return Err(MaterializationError::MaterializationFailed {
                executor_ref: executor_ref.to_string(),
                detail: format!(
                    "materialized executor {} has Unix mode {actual_mode:#o}, expected signed mode {expected_mode:#o}",
                    target_path.display()
                ),
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = expected_mode;
        return Err(MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: "native executor Unix mode validation is unavailable on this platform"
                .to_string(),
        });
    }

    let actual_hash = std::fs::read(target_path)
        .map(|bytes| lillux::cas::sha256_hex(&bytes))
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!(
                "failed to verify materialized executor {}: {error}",
                target_path.display()
            ),
        })?;
    if actual_hash != expected_hash {
        return Err(MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!(
                "materialized executor {} failed its content-address check",
                target_path.display()
            ),
        });
    }
    Ok(())
}

/// Resolve a native executor from the system bundle's CAS.
///
/// Looks up the system bundle manifest via `refs/bundles/manifest`,
/// verifies the trusted signature over that exact manifest hash, resolves
/// `bin/<host_triple>/<bare>` through hash-checked manifest and ItemSource
/// objects, verifies the blob bytes, checks architecture,
/// and materializes the binary to a content-addressed cache under
/// `cache_root/cache/executors/<blob_hash>/<bare>`.
///
/// Content-addressed: a given blob hash always lands at the same path.
/// Extract once per (binary version, host), re-exec from cache forever
/// after. Cache lives under daemon-owned app-root state, not under the
/// project tree — read-only project mounts work.
///
/// Returns the materialized path and the verified raw-byte SHA-256 that every
/// enforced launch must carry into the isolation boundary.
pub fn materialize_native_executor(
    bundle_roots: &[PathBuf],
    executor_ref: &str,
    cache_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    root_trust_class: ryeos_engine::resolution::TrustClass,
) -> Result<MaterializedExecutor, MaterializationError> {
    let bare = executor_ref.strip_prefix("native:").ok_or_else(|| {
        MaterializationError::ExecutorUnavailable {
            executor_ref: executor_ref.to_string(),
            detail: "executor_ref is not a native executor".into(),
        }
    })?;
    let mut components = Path::new(bare).components();
    if bare.is_empty()
        || !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(MaterializationError::ExecutorUnavailable {
            executor_ref: executor_ref.to_string(),
            detail: "native executor id must be one normal filename component".to_string(),
        });
    }

    let triple = host_triple();

    // Iterate every bundle root that ships a manifest. A requested native
    // executor must resolve from exactly one root: even if admission was
    // bypassed, root ordering must never decide which executable runs.
    let mut tried_roots: Vec<PathBuf> = Vec::new();
    let mut last_resolution_error: Option<String> = None;
    let mut resolved_with: Option<(
        PathBuf,
        lillux::cas::CasStore,
        ryeos_engine::executor_resolution::ResolvedExecutor,
        ryeos_engine::executor_resolution::VerifiedExecutorManifestRef,
    )> = None;

    for system_root in bundle_roots {
        let ai_dir = system_root.join(ryeos_engine::AI_DIR);
        let objects_dir = ai_dir.join("objects");

        if !objects_dir.join("blobs").is_dir() || !objects_dir.join("objects").is_dir() {
            continue;
        }

        let ref_path = ai_dir.join(BUNDLE_MANIFEST_REF);
        let signed_ref = match std::fs::read_to_string(&ref_path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(MaterializationError::ManifestError(format!(
                    "failed to read signed bundle executor manifest ref {}: {error}",
                    ref_path.display()
                )))
            }
        };

        tried_roots.push(system_root.clone());

        let verified_ref =
            match ryeos_engine::executor_resolution::verify_signed_executor_manifest_ref(
                &signed_ref,
                |fingerprint| {
                    trust_store
                        .get(fingerprint)
                        .map(|signer| signer.verifying_key)
                },
                root_trust_class,
            ) {
                Ok(verified) => verified,
                Err(
                    ryeos_engine::executor_resolution::ExecutorResolutionError::ManifestSignerUntrusted {
                        fingerprint,
                    },
                ) => {
                    return Err(MaterializationError::ExecutorUntrusted {
                        executor_ref: bare.to_string(),
                        trust_class: ryeos_engine::resolution::TrustClass::UntrustedProject,
                        fingerprint: Some(fingerprint),
                    })
                }
                Err(error) => {
                    return Err(MaterializationError::ManifestError(format!(
                        "{}: {error}",
                        ref_path.display()
                    )))
                }
            };
        let mhash = verified_ref.manifest_hash.clone();

        if !matches!(
            verified_ref.trust_class,
            ryeos_engine::resolution::TrustClass::TrustedBundle
                | ryeos_engine::resolution::TrustClass::TrustedProject
        ) {
            return Err(MaterializationError::ExecutorUntrusted {
                executor_ref: bare.to_string(),
                trust_class: verified_ref.trust_class,
                fingerprint: Some(verified_ref.signer_fingerprint.clone()),
            });
        }

        let cas = lillux::cas::CasStore::new(objects_dir);

        let manifest_value = cas
            .get_object(&mhash)
            .map_err(|e| {
                MaterializationError::ManifestError(format!(
                    "failed to read bundle manifest object {mhash}: {e}"
                ))
            })?
            .ok_or_else(|| {
                MaterializationError::ManifestError(format!(
                    "bundle manifest object {mhash} not found in system CAS"
                ))
            })?;

        let manifest_item_source_hashes =
            ryeos_engine::executor_resolution::verify_executor_manifest_object(
                &manifest_value,
                &mhash,
            )
            .map_err(|error| {
                MaterializationError::ManifestError(format!(
                    "bundle executor manifest {mhash} failed verification: {error}"
                ))
            })?;

        tracing::debug!(
            executor_ref,
            host_triple = %triple,
            bundle_root = %system_root.display(),
            manifest_entries = manifest_item_source_hashes.len(),
            "scanning bundle manifest for native executor"
        );

        match ryeos_engine::executor_resolution::resolve_native_executor(
            &manifest_item_source_hashes,
            executor_ref,
            &triple,
            |h| cas.get_object(h).map_err(|e| e.to_string()),
        ) {
            Ok(resolved) => {
                if let Some((first_root, ..)) = &resolved_with {
                    return Err(MaterializationError::ResolutionFailed {
                        executor_ref: bare.to_string(),
                        detail: format!(
                            "native executor identity `bin/{triple}/{bare}` is published by both {} and {}; bundle root order cannot select an executor",
                            first_root.display(),
                            system_root.display(),
                        ),
                    });
                }
                resolved_with = Some((system_root.clone(), cas, resolved, verified_ref));
            }
            Err(
                error @ ryeos_engine::executor_resolution::ExecutorResolutionError::NotInManifest {
                    ..
                },
            ) => {
                last_resolution_error = Some(error.to_string());
                continue;
            }
            Err(error) => {
                return Err(MaterializationError::ResolutionFailed {
                    executor_ref: bare.to_string(),
                    detail: error.to_string(),
                })
            }
        }
    }

    if tried_roots.is_empty() {
        return Err(MaterializationError::ExecutorUnavailable {
            executor_ref: bare.to_string(),
            detail: format!(
                "no system bundle manifest found ({BUNDLE_MANIFEST_REF}). \
                 The bundle pipeline must ship binaries for host triple '{triple}'."
            ),
        });
    }

    let (_bundle_root, cas, resolved, verified_ref) =
        resolved_with.ok_or_else(|| MaterializationError::ResolutionFailed {
            executor_ref: bare.to_string(),
            detail: last_resolution_error.unwrap_or_else(|| {
                format!(
                    "no manifest among {} system bundle root(s) lists '{executor_ref}' for triple '{triple}'",
                    tried_roots.len()
                )
            }),
        })?;

    tracing::info!(
        executor_ref,
        host_triple = %triple,
        manifest_hash = %verified_ref.manifest_hash,
        item_source_hash = %resolved.item_source_hash,
        blob_hash = %resolved.blob_hash,
        signer_fp = %verified_ref.signer_fingerprint,
        trust_class = ?verified_ref.trust_class,
        "native executor CAS chain cryptographically verified"
    );

    // 5. Fetch the binary blob from CAS
    let blob_bytes = cas
        .get_blob(&resolved.blob_hash)
        .map_err(|e| MaterializationError::BlobNotFound {
            hash: format!("{} (read error: {e})", resolved.blob_hash),
        })?
        .ok_or_else(|| MaterializationError::BlobNotFound {
            hash: resolved.blob_hash.clone(),
        })?;
    let blob_content_hash = lillux::cas::sha256_hex(&blob_bytes);
    if blob_content_hash != resolved.blob_hash {
        return Err(MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!(
                "CAS binary blob hash mismatch: expected {}, got {blob_content_hash}",
                resolved.blob_hash
            ),
        });
    }

    // 6. Architecture check
    arch_check::check_arch(&blob_bytes, std::env::consts::ARCH).map_err(|e| {
        MaterializationError::ArchCheckFailed {
            executor_ref: bare.to_string(),
            detail: e.to_string(),
        }
    })?;

    // 7. Materialize to content-addressed cache under daemon state.
    //    Path: <cache_root>/cache/executors/<blob_hash>/<bare>
    //    Content-addressed → extract once, re-exec forever.
    let target_path = executor_cache_target(cache_root, &resolved.blob_hash, bare);

    match std::fs::symlink_metadata(&target_path) {
        Ok(_) => {
            verify_materialized_executor_artifact(
                &target_path,
                &blob_content_hash,
                resolved.mode,
                bare,
            )?;
            tracing::debug!(
                executor_ref,
                target = %target_path.display(),
                "native executor cache hit"
            );
            return Ok(MaterializedExecutor {
                path: target_path,
                content_hash: blob_content_hash,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(MaterializationError::MaterializationFailed {
                executor_ref: bare.to_string(),
                detail: format!(
                    "failed to inspect executor cache target {}: {error}",
                    target_path.display()
                ),
            });
        }
    }

    // Stage atomically — first writer wins.
    let staging_dir = target_path.parent().unwrap().with_file_name(format!(
        "{}.staging.{}.{}",
        resolved.blob_hash,
        std::process::id(),
        rand::thread_rng().gen::<u32>()
    ));
    std::fs::create_dir_all(&staging_dir).map_err(|e| {
        MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to create staging dir: {e}"),
        }
    })?;
    let staged_bin = staging_dir.join(bare);
    lillux::cas::materialize_executable(&staged_bin, &blob_bytes, resolved.mode).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging_dir);
        MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to materialize executable: {e}"),
        }
    })?;

    // Atomic publish — first writer wins.
    //
    // We rename the staging directory to its final content-addressed
    // location. If `rename` fails, we MUST verify the target exists
    // before returning Ok — otherwise a permissions error / dirty
    // cache dir / FS corruption would silently produce a path that
    // has no binary in it and the caller's exec would later fail
    // with a confusing ENOENT.
    let target_parent = target_path.parent().unwrap();
    if let Some(grandparent) = target_parent.parent() {
        std::fs::create_dir_all(grandparent).map_err(|e| {
            MaterializationError::MaterializationFailed {
                executor_ref: bare.to_string(),
                detail: format!("failed to create cache root dir: {e}"),
            }
        })?;
    }
    match std::fs::rename(&staging_dir, target_parent) {
        Ok(_) => {
            tracing::info!(
                executor_ref,
                target = %target_path.display(),
                "native executor published to cache"
            );
        }
        Err(rename_err) => {
            let _ = std::fs::remove_dir_all(&staging_dir);
            if let Err(winner_error) = verify_materialized_executor_artifact(
                &target_path,
                &blob_content_hash,
                resolved.mode,
                bare,
            ) {
                return Err(MaterializationError::MaterializationFailed {
                    executor_ref: bare.to_string(),
                    detail: format!(
                        "failed to publish executor to cache at {} \
                         (rename error: {rename_err}; winner validation failed: {winner_error})",
                        target_path.display(),
                    ),
                });
            }
            tracing::debug!(
                executor_ref,
                target = %target_path.display(),
                "native executor publish lost benign race; using winner's binary"
            );
        }
    }

    verify_materialized_executor_artifact(&target_path, &blob_content_hash, resolved.mode, bare)?;
    Ok(MaterializedExecutor {
        path: target_path,
        content_hash: blob_content_hash,
    })
}

/// Build the verified config loader used by generic execution-limit policy.
/// Launch-preparer snapshots use the stricter engine-owned loader instead.
fn build_verified_loader_for_thread(
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    node_trusted_keys_dir: &Path,
) -> anyhow::Result<ryeos_runtime::verified_loader::VerifiedLoader> {
    let project_root = engine_roots
        .ordered
        .iter()
        .find(|root| root.space == ryeos_engine::contracts::ItemSpace::Project)
        .and_then(|root| root.ai_root.parent().map(Path::to_path_buf))
        .ok_or_else(|| anyhow::anyhow!("no project root in engine resolution roots"))?;
    let bundle_roots = engine_roots
        .ordered
        .iter()
        .filter(|root| root.space == ryeos_engine::contracts::ItemSpace::Bundle)
        .filter_map(|root| root.ai_root.parent().map(Path::to_path_buf))
        .collect();
    Ok(ryeos_runtime::verified_loader::VerifiedLoader::new(
        project_root,
        bundle_roots,
        node_trusted_keys_dir,
    ))
}

pub struct NativeLaunchResult {
    pub thread: Value,
    pub result: Value,
}

/// Spawn-gate: refuse to spawn an effective item whose composed trust class
/// is `Unsigned`. The rejection remains a typed dispatch-policy error all the
/// way to the HTTP boundary; it must never collapse into an opaque 500.
pub(crate) fn enforce_effective_trust(
    trust_class: ryeos_engine::resolution::TrustClass,
    item_ref: &str,
    kind: &str,
) -> std::result::Result<(), DispatchError> {
    if matches!(trust_class, ryeos_engine::resolution::TrustClass::Unsigned) {
        return Err(effective_trust_unsigned_error(item_ref, kind));
    }
    Ok(())
}

/// Construct the single typed policy rejection used when either the composed
/// resolution pipeline or a direct verified-root gate proves unsigned launch
/// authority. Keeping this shape centralized prevents method and envelope
/// dispatch from drifting at the HTTP boundary.
pub(crate) fn effective_trust_unsigned_error(item_ref: &str, kind: &str) -> DispatchError {
    DispatchError::LaunchPolicyForbidden {
        code: "effective_trust_unsigned".to_owned(),
        message: format!(
            "refusing to spawn `{item_ref}` ({kind}): effective_trust_class is Unsigned — \
             root or one of its ancestors lacks a valid signature from a trusted signer"
        ),
        binding: None,
    }
}

/// Conventional name of the launcher-facing capability list inside
/// `KindComposedView::policy_facts`. Kinds wire this name through
/// their `composer_config.policy_facts[].name` so the launcher reads
/// caps without naming the underlying field path. Adding a new
/// policy fact = adding a new constant here AND a matching
/// `policy_facts` entry in the kind schema; no engine algorithm
/// change required.
pub const POLICY_FACT_EFFECTIVE_CAPS: &str = "effective_caps";

/// Derive effective capabilities from the composed view by reading
/// the conventional `effective_caps` policy fact. Kinds without a
/// permission model leave the fact unset → empty caps (deny-all),
/// which is the correct posture for kinds the launcher should never
/// be granting tool access on its behalf.
pub(crate) fn derive_effective_caps(
    composed: &ryeos_engine::resolution::KindComposedView,
) -> Vec<String> {
    composed.policy_fact_string_seq(POLICY_FACT_EFFECTIVE_CAPS)
}

/// How a managed runtime launch should treat checkpoint state. One axis (distinct
/// from `reconcile::ResumeKind`, which is the dispatch route). Encoding the three
/// legal cases as an enum makes the illegal "both machine-continuation AND
/// same-thread" state unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointResumeMode {
    /// Fresh launch / operator follow-up: cold start, no resume env.
    None,
    /// Autonomous machine continuation successor: copy the PREDECESSOR's
    /// checkpoint into this (new) thread's dir, then inject `RYEOS_RESUME=1`.
    MachineContinuation,
    /// Same-thread crash recovery: resume from this thread's OWN checkpoint
    /// (already in its dir — no copy), then inject `RYEOS_RESUME=1`.
    SameThread,
}

impl CheckpointResumeMode {
    fn injects_resume_env(self) -> bool {
        matches!(self, Self::MachineContinuation | Self::SameThread)
    }

    fn copies_predecessor_checkpoint(self) -> bool {
        matches!(self, Self::MachineContinuation)
    }
}

/// How the run-half reconciles the freshly-resolved (live composed) caps against
/// any captured or bounding authority. The live composition arrives as two
/// distinct sources — caller-delegated `declared` grants and daemon-minted
/// manifest `runtime_manifest` authority — because the follow-child policy treats
/// them differently; every other policy reasons over their union.
#[derive(Clone, Copy)]
pub enum CapabilityPolicy<'a> {
    /// Fresh launch: run with exactly the live composed caps.
    Fresh,
    /// Continuation / native-resume: the live composed caps MUST equal the caps
    /// the predecessor captured (no silent privilege drift); run with them.
    ExactPinned(&'a [String]),
    /// Detached follow child: source-aware bounding against the parent's
    /// authority. Each child-*declared* (caller-delegated) grant must be implied
    /// by `parent_effective_caps` and is kept at the child's own exact shape;
    /// child-owned *manifest runtime* authority is preserved verbatim (the parent
    /// need not hold it); and the parent must imply the child's execute cap
    /// (admission). A follow child is a delegated deputy of the parent, so it may
    /// never hold delegated authority the parent lacks — but it keeps the runtime
    /// authority its own signed manifest grants.
    FollowChildHybrid { parent_effective_caps: &'a [String] },
}

/// Union the two live cap sources into the single set a non-source-aware policy
/// reasons over (sorted + de-duplicated).
fn union_cap_sources(declared: Vec<String>, runtime_manifest: Vec<String>) -> Vec<String> {
    declared
        .into_iter()
        .chain(runtime_manifest)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Apply a [`CapabilityPolicy`] to the freshly-resolved cap sources, returning the
/// caps the launch should actually run with (callback token + envelope + launch
/// metadata all consume the result). `child_execute_cap` is the canonical execute
/// cap for the item being launched (`ryeos.execute.<kind>.<bare_id>`); only the
/// follow-child policy consults it (admission gate).
fn apply_capability_policy(
    declared: Vec<String>,
    runtime_manifest: Vec<String>,
    policy: CapabilityPolicy<'_>,
    item_ref: &str,
    child_execute_cap: &str,
) -> Result<Vec<String>, BuildAndLaunchError> {
    match policy {
        CapabilityPolicy::Fresh => Ok(union_cap_sources(declared, runtime_manifest)),
        CapabilityPolicy::ExactPinned(captured) => {
            let composed = union_cap_sources(declared, runtime_manifest);
            let recomputed: BTreeSet<&str> = composed.iter().map(String::as_str).collect();
            let captured_set: BTreeSet<&str> = captured.iter().map(String::as_str).collect();
            if recomputed != captured_set {
                return Err(BuildAndLaunchError::CapabilityRejected {
                    reason: format!(
                        "continuation capability drift for `{item_ref}`: the live item resolves \
                         to a different capability set than the predecessor captured — refusing \
                         to launch with changed authority (snapshot-pinned continuation not yet \
                         implemented)"
                    ),
                });
            }
            Ok(composed)
        }
        CapabilityPolicy::FollowChildHybrid {
            parent_effective_caps,
        } => apply_follow_child_hybrid(
            parent_effective_caps,
            declared,
            runtime_manifest,
            item_ref,
            child_execute_cap,
        ),
    }
}

/// Source-aware capability bounding for a detached follow child (see
/// [`CapabilityPolicy::FollowChildHybrid`]).
///
/// Parent coverage uses grant-side wildcard matching
/// (`cap_matches(parent_grant, required)`): a parent `ryeos.execute.tool.*` covers
/// a child-declared `ryeos.execute.tool.echo`, but the child keeps its own exact
/// `tool.echo` shape — the parent's wildcard is never copied onto the child.
fn apply_follow_child_hybrid(
    parent_effective_caps: &[String],
    declared: Vec<String>,
    runtime_manifest: Vec<String>,
    item_ref: &str,
    child_execute_cap: &str,
) -> Result<Vec<String>, BuildAndLaunchError> {
    let parent_implies = |required: &str| {
        parent_effective_caps
            .iter()
            .any(|grant| ryeos_runtime::authorizer::cap_matches(grant, required))
    };

    // Admission: the parent must itself hold execute authority over the child
    // item — a follow child may only run what the parent could have dispatched.
    if !parent_implies(child_execute_cap) {
        return Err(BuildAndLaunchError::CapabilityRejected {
            reason: format!(
                "follow-child admission denied for `{item_ref}`: parent lacks execute authority \
                 `{child_execute_cap}` — refusing to launch a child the parent cannot itself \
                 dispatch"
            ),
        });
    }

    let mut effective: BTreeSet<String> = BTreeSet::new();

    // Delegated authority: every child-declared grant must be covered by the
    // parent, and is kept at the child's exact shape (never widened to the
    // parent's wildcard).
    for cap in declared {
        if !parent_implies(&cap) {
            return Err(BuildAndLaunchError::CapabilityRejected {
                reason: format!(
                    "follow-child capability escalation for `{item_ref}`: child declares delegated \
                     cap `{cap}` not covered by the parent's authority — a follow child cannot \
                     hold delegated authority the parent lacks"
                ),
            });
        }
        effective.insert(cap);
    }

    // Child-owned manifest runtime authority (bundle-events / runtime-vault),
    // minted from the child's OWN signed manifest, is preserved verbatim — the
    // parent need not (and usually does not) hold it.
    effective.extend(runtime_manifest);

    Ok(effective.into_iter().collect())
}

pub struct BuildAndLaunchParams<'a> {
    pub state: &'a AppState,
    /// The serving runtime's canonical ref (`runtime:<name>`) for a managed
    /// runtime-registry launch (directive / graph); `None` for direct subprocess
    /// launches. Persisted into the `ResumeContext` so a continuation successor
    /// reattaches the same runtime identity rather than re-resolving the default.
    pub runtime_ref: Option<&'a str>,
    pub acting_principal: &'a str,
    pub resolved: &'a ResolvedExecutionRequest,
    pub project_path: &'a Path,
    pub provenance: &'a ryeos_app::execution_provenance::ExecutionProvenance,
    pub parameters: &'a Value,
    pub metadata_required_secrets: &'a [String],
    pub pre_minted_thread_id: Option<&'a str>,
    /// Chained-resume turn (see `DispatchRequest::previous_thread_id`).
    pub previous_thread_id: Option<&'a str>,
    /// Trusted parent execution context carried out-of-band from schema-driven
    /// dispatch. Present for callback-dispatched child launches; absent for
    /// roots and same-braid continuations.
    pub parent_execution_context: Option<&'a crate::dispatch::ParentExecutionContext>,
    /// Machine continuation: fold the chain and resume with NO new stimulus.
    /// `false` for fresh launches and operator follow-ups (which inject their
    /// `parameters` as the opening stimulus); `true` only for an autonomous
    /// limit-cutoff successor, whose `parameters` are the source's originals and
    /// are already in the folded chain.
    pub suppress_stimulus: bool,
    /// How the run-half reconciles the freshly-resolved caps against any captured
    /// authority — see [`CapabilityPolicy`].
    pub capability_policy: CapabilityPolicy<'a>,
    /// How this managed launch treats checkpoint state — see
    /// [`CheckpointResumeMode`]. Drives `RYEOS_RESUME=1` injection and predecessor
    /// copy-forward, and only for replay-aware (`native_resume`) kinds.
    pub checkpoint_resume_mode: CheckpointResumeMode,
    /// Optional acknowledgement seam for launch surfaces that must not expose
    /// a thread ID until the frozen authority has crossed into a successfully
    /// scheduled spawn task. Synchronous and reconcile paths leave this absent.
    pub launch_handoff: Option<&'a LaunchHandoff>,
}

/// One-shot readiness signal for an acknowledged subprocess launch.
///
/// Pre-handoff failures publish a structured error; cancellation/panic closes
/// the receiver. The dispatch task's typed error remains authoritative.
/// Managed-envelope, method-runtime, and terminal-subprocess launchers publish
/// success only after their exact execution authority is owned by a scheduled
/// task.
#[derive(Debug, Clone)]
pub struct LaunchHandoff {
    sender: Arc<Mutex<Option<tokio::sync::oneshot::Sender<LaunchHandoffResult>>>>,
}

#[derive(Debug, Clone)]
pub struct LaunchHandoffFailure {
    pub code: String,
    pub message: String,
    pub status: u16,
    pub body: Value,
}

pub type LaunchHandoffResult = std::result::Result<String, LaunchHandoffFailure>;

impl LaunchHandoff {
    pub fn channel() -> (Self, tokio::sync::oneshot::Receiver<LaunchHandoffResult>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (
            Self {
                sender: Arc::new(Mutex::new(Some(sender))),
            },
            receiver,
        )
    }

    fn publish_result(&self, result: LaunchHandoffResult) {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
    }

    pub(crate) fn publish(&self, thread_id: String) {
        self.publish_result(Ok(thread_id));
    }

    pub(crate) fn publish_failure(
        &self,
        code: impl Into<String>,
        message: impl Into<String>,
        status: u16,
        retryable: bool,
    ) {
        let code = code.into();
        let message = message.into();
        self.publish_result(Err(LaunchHandoffFailure {
            body: json!({
                "code": code.clone(),
                "error": message.clone(),
                "retryable": retryable,
            }),
            code,
            message,
            status,
        }));
    }

    pub(crate) fn publish_dispatch_failure(&self, error: &DispatchError) {
        self.publish_result(Err(LaunchHandoffFailure {
            code: error.code().to_owned(),
            message: error.to_string(),
            status: error.http_status().as_u16(),
            body: crate::structured_error::StructuredErrorPayload::from(error).to_value(),
        }));
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }
}

/// Per-attempt launch authority produced immediately before persistence.
///
/// This value is deliberately in-memory only. Admission preparation is a
/// separate pass and can never construct this type; restart/reconcile paths
/// recompute it from reconstructed provenance instead of loading persisted
/// runtime behavior.
struct PreparedManagedLaunchAuthority {
    resolution: ryeos_engine::resolution::ResolutionOutput,
    prepared_launch: super::launch_preparation::PreparedRuntimeLaunch,
    effective_vault: HashMap<String, String>,
    effective_caps: Vec<String>,
    selected_runtime: ryeos_engine::runtime_registry::VerifiedRuntime,
    executor_ref: String,
    checkpoint_dir: Option<PathBuf>,
    is_resume: bool,
    launch_metadata: Option<ryeos_app::launch_metadata::RuntimeLaunchMetadata>,
    pending_project_snapshot: Option<super::CapturedProjectGeneration>,
}

/// Whether the exact authority audit for this launch is already part of the
/// thread's signed birth commit or must be appended for this claimed attempt.
/// Keeping this typed prevents an existing `created` successor from silently
/// taking the fresh-birth path (or a fresh root from duplicating its audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchAuditDisposition {
    CommittedAtBirth,
    AppendForAttempt,
}

fn launch_audit_records(
    resolved: &ResolvedExecutionRequest,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    prepared_launch: &super::launch_preparation::PreparedRuntimeLaunch,
) -> Result<Vec<ryeos_app::state_store::NewEventRecord>, BuildAndLaunchError> {
    Ok([
        (
            RuntimeEventType::AsLaunchedResolution,
            serde_json::to_value(resolution.as_launched_digest())?,
        ),
        (
            RuntimeEventType::AsLaunchedRefBindings,
            json!({
                "ref_bindings": resolved.ref_bindings.clone(),
                "records": prepared_launch.binding_records.clone(),
            }),
        ),
        (
            RuntimeEventType::RuntimeLaunchFacts,
            json!({"facts": prepared_launch.runtime_facts.clone()}),
        ),
    ]
    .into_iter()
    .map(
        |(event_type, payload)| ryeos_app::state_store::NewEventRecord {
            event_type: event_type.as_str().to_owned(),
            storage_class: event_type.storage_class().as_str().to_owned(),
            payload,
        },
    )
    .collect())
}

async fn prepare_managed_launch_authority(
    params: &BuildAndLaunchParams<'_>,
    thread_id: &str,
    metadata_template: Option<&ryeos_app::launch_metadata::RuntimeLaunchMetadata>,
    capture_project_snapshot: bool,
) -> Result<PreparedManagedLaunchAuthority, BuildAndLaunchError> {
    let engine = params.provenance.request_engine();
    let engine_roots = engine.resolution_roots(Some(params.project_path.to_path_buf()));
    let effective_parsers = engine
        .effective_parser_dispatcher(Some(params.project_path))
        .map_err(|error| anyhow::anyhow!("effective parser dispatcher: {error}"))?;

    // Resolve the primary exactly once for this authoritative pass. Launch
    // augmentations complete that resolution before the same exact output is
    // prepared, audited, persisted, and moved into the LaunchEnvelope.
    let mut resolution = ryeos_engine::resolution::run_resolution_pipeline(
        &params.resolved.resolved_item.canonical_ref,
        &engine.kinds,
        &effective_parsers,
        &engine_roots,
        &engine.trust_store,
        &engine.composers,
    )
    .map_err(|error| anyhow::anyhow!("resolution pipeline failed: {error}"))?;
    // Admission bound this launch to the exact verified signature-stripped
    // root bytes. The authoritative preparation pass resolves again against
    // current sources; refuse drift before augmentation, preparation, audit,
    // persistence, or callback-capability minting can observe another item.
    if resolution.root.raw_content_digest != params.resolved.root_raw_content_digest {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "launch root raw-content digest drift for `{}`: resolved={}, launch={}",
            params.resolved.item_ref,
            params.resolved.root_raw_content_digest,
            resolution.root.raw_content_digest,
        )));
    }
    enforce_effective_trust(
        resolution.effective_trust_class,
        &params.resolved.item_ref,
        &params.resolved.resolved_item.kind,
    )?;

    // Augmentation is part of the authoritative resolution, not a mutation of
    // already-audited launch state. Its internal worker is an independent,
    // lifecycle-guarded root, so the prospective managed thread need not exist.
    let launching_kind_schema = engine
        .kinds
        .get(&params.resolved.resolved_item.kind)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "build_and_launch: launching kind `{}` is not registered",
                params.resolved.resolved_item.kind
            )
        })?;
    if let Some(exec) = launching_kind_schema.execution() {
        if !exec.launch_augmentations.is_empty() {
            crate::augmentations::run_augmentations(
                exec,
                &mut resolution,
                thread_id,
                params.project_path,
                engine,
                params.provenance,
                &params.resolved.plan_context,
                params.acting_principal,
                params.state,
            )
            .await
            .map_err(|error| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "launch augmentation failed: {error}"
                ))
            })?;
        }
    }

    let selected_runtime = engine
        .runtimes
        .resolve_for_launch(params.runtime_ref, &params.resolved.resolved_item.kind)
        .map_err(|error| {
            BuildAndLaunchError::from(DispatchError::LaunchPreparationFailed {
                code: "runtime_launch_contract_unavailable".to_owned(),
                message: error.to_string(),
                classification: "configuration".to_owned(),
                binding: None,
                details: Box::new(BTreeMap::new()),
            })
        })?
        .clone();
    let prepared_launch = super::launch_preparation::prepare_runtime_launch(
        super::launch_preparation::PrepareRuntimeLaunchRequest {
            engine,
            runtime: &selected_runtime,
            primary: &resolution,
            ref_bindings: &params.resolved.ref_bindings,
            roots: &engine_roots,
            parsers: &effective_parsers,
            principal: &params.resolved.plan_context.requested_by,
        },
    )
    .map_err(BuildAndLaunchError::from)?;
    let dotenv_dirs =
        ryeos_app::vault::dotenv_search_dirs(Some(params.provenance.original_project_path()));
    let mut secret_requirements = build_secret_requirements(params.metadata_required_secrets);
    merge_prepared_secret_requirements(
        &mut secret_requirements,
        &prepared_launch.required_secrets,
    )?;
    let secret_names: Vec<String> = secret_requirements
        .iter()
        .map(|requirement| requirement.name.clone())
        .collect();
    let effective_vault = ryeos_app::vault::read_required_secrets(
        params.state.vault.as_ref(),
        params.acting_principal,
        &secret_names,
        &dotenv_dirs,
    )
    .map_err(|error| match error {
        VaultReadError::MissingSecrets { names, .. } => BuildAndLaunchError::MissingSecrets {
            item_ref: params.resolved.item_ref.clone(),
            secrets: missing_secrets_from_requirements(&names, &secret_requirements),
        },
        VaultReadError::Internal(error) => {
            BuildAndLaunchError::Internal(anyhow::anyhow!("vault read failed: {error:#}"))
        }
    })?;
    let composed_effective_caps = derive_effective_caps(&resolution.composed);
    ryeos_bundle::runtime_authority::reject_disallowed_composed_grants(&composed_effective_caps)
        .map_err(|error| BuildAndLaunchError::CapabilityRejected {
            reason: error.to_string(),
        })?;
    let runtime_capability_caps = crate::dispatch::mint_runtime_capability_caps(
        resolution.composed.composed.get("requires"),
        &params.resolved.resolved_item,
        resolution.effective_trust_class,
        engine,
    )
    .map_err(|reason| BuildAndLaunchError::CapabilityRejected { reason })?;
    let child_execute_cap = ryeos_runtime::authorizer::canonical_cap(
        &params.resolved.resolved_item.canonical_ref.kind,
        &params.resolved.resolved_item.canonical_ref.bare_id,
        "execute",
    );
    let effective_caps = apply_capability_policy(
        composed_effective_caps,
        runtime_capability_caps,
        params.capability_policy,
        &params.resolved.item_ref,
        &child_execute_cap,
    )?;
    let runtime_binary =
        crate::dispatch::strip_binary_ref_prefix(&selected_runtime.yaml.binary_ref)
            .map_err(|error| BuildAndLaunchError::Internal(anyhow::anyhow!(error)))?;
    let executor_ref = format!("native:{runtime_binary}");
    let native_resume = selected_runtime.yaml.native_resume.clone();
    let checkpoint_dir = if native_resume.is_some() {
        let dir = ryeos_app::launch_metadata::daemon_checkpoint_dir(
            &params.state.config.app_root,
            thread_id,
        );
        std::fs::create_dir_all(&dir).map_err(|error| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "failed to allocate checkpoint dir for replay-aware runtime `{}`: {error}",
                params.resolved.item_ref
            ))
        })?;
        Some(dir)
    } else {
        None
    };
    let is_resume = params.checkpoint_resume_mode.injects_resume_env() && native_resume.is_some();
    if params
        .checkpoint_resume_mode
        .copies_predecessor_checkpoint()
        && native_resume.is_some()
    {
        let previous = params.previous_thread_id.ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "machine continuation of `{}` has no predecessor thread",
                params.resolved.item_ref
            ))
        })?;
        let successor_dir = checkpoint_dir.as_deref().ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "machine continuation of `{}` has no checkpoint dir",
                params.resolved.item_ref
            ))
        })?;
        let previous_dir = ryeos_app::launch_metadata::daemon_checkpoint_dir(
            &params.state.config.app_root,
            previous,
        );
        if !ryeos_runtime::CheckpointWriter::copy_latest(&previous_dir, successor_dir).map_err(
            |error| {
                BuildAndLaunchError::Internal(anyhow::anyhow!("copy-forward checkpoint: {error}"))
            },
        )? {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "machine continuation of `{}`: predecessor `{previous}` has no checkpoint to resume from",
                params.resolved.item_ref
            )));
        }
    }
    let supports_continuation = params
        .state
        .threads
        .kind_profiles()
        .get(&params.resolved.kind)
        .is_some_and(|profile| profile.supports_continuation);
    let force_resume_context = metadata_template
        .and_then(|metadata| metadata.resume_context.as_ref())
        .is_some();
    let should_capture_resume_context =
        supports_continuation || native_resume.is_some() || force_resume_context;
    let mut pending_project_snapshot = None;
    let launch_metadata = if should_capture_resume_context {
        let original_pushed_head_ref =
            ryeos_app::launch_metadata::OriginalPushedHeadRef::from_provenance(params.provenance);
        let has_local_project_context = matches!(
            &params.resolved.plan_context.project_context,
            ryeos_engine::contracts::ProjectContext::LocalPath { .. }
        );
        let must_pin_local_snapshot = original_pushed_head_ref.is_none()
            && has_local_project_context
            && params.provenance.captured_snapshot_hash().is_none()
            && (native_resume.is_some() || params.provenance.workspace_lifeline().is_some())
            && capture_project_snapshot;
        if must_pin_local_snapshot {
            pending_project_snapshot = Some(
                super::capture_live_project_snapshot(
                    params.state,
                    params.project_path,
                    &params.resolved.origin_site_id,
                    "managed_runtime_resume_pin",
                )
                .map_err(|error| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "failed to pin project snapshot for durable runtime `{}`: {error:#}",
                        params.resolved.item_ref
                    ))
                })?,
            );
            let generation = pending_project_snapshot
                .as_ref()
                .expect("captured generation was assigned above");
            tracing::debug!(
                snapshot_hash = %generation.snapshot_hash,
                tree_hash = %generation.tree_hash,
                policy_hash = %generation.policy_hash,
                "captured exact managed-launch project generation"
            );
        }
        let mut metadata = metadata_template.cloned().unwrap_or_default();
        let inherited_stable_project_identity = pending_project_snapshot
            .as_ref()
            .map(|generation| generation.stable_project_identity.clone())
            .or_else(|| {
                metadata_template
                    .and_then(|template| template.resume_context.as_ref())
                    .and_then(|resume| resume.stable_project_identity.clone())
            });
        let stable_project_identity = match inherited_stable_project_identity {
            Some(identity) => Some(identity),
            None if matches!(
                &params.resolved.plan_context.project_context,
                ryeos_engine::contracts::ProjectContext::None
            ) =>
            {
                None
            }
            None => Some(
                ryeos_app::launch_metadata::StableProjectIdentity::from_path(
                    params.provenance.original_project_path(),
                    &params.resolved.origin_site_id,
                )
                .map_err(BuildAndLaunchError::Internal)?,
            ),
        };
        let local_overlay_root = pending_project_snapshot
            .as_ref()
            .and_then(|generation| generation.local_overlay_root.clone())
            .or_else(|| {
                metadata_template
                    .and_then(|template| template.resume_context.as_ref())
                    .and_then(|resume| resume.local_overlay_root.clone())
            })
            .or_else(|| {
                matches!(
                    &params.provenance,
                    ryeos_app::execution_provenance::ExecutionProvenance::RootLiveFs { .. }
                        | ryeos_app::execution_provenance::ExecutionProvenance::BorrowedChildLiveFs { .. }
                )
                .then(|| params.provenance.original_project_path().to_path_buf())
            });
        metadata = metadata.with_resume_context(ryeos_app::launch_metadata::ResumeContext {
            kind: params.resolved.kind.clone(),
            item_ref: params.resolved.item_ref.clone(),
            ref_bindings: params.resolved.ref_bindings.clone(),
            launch_mode: params.resolved.launch_mode.clone(),
            parameters: params.parameters.clone(),
            project_context: params.resolved.plan_context.project_context.clone(),
            stable_project_identity,
            local_overlay_root,
            original_snapshot_hash: pending_project_snapshot
                .as_ref()
                .map(|publication| publication.snapshot_hash.clone())
                .or_else(|| {
                    params
                        .provenance
                        .captured_snapshot_hash()
                        .map(str::to_owned)
                })
                .or_else(|| {
                    metadata_template
                        .and_then(|template| template.resume_context.as_ref())
                        .and_then(|resume| resume.original_snapshot_hash.clone())
                }),
            original_pushed_head_ref,
            state_root: params
                .provenance
                .state_root_override()
                .map(Path::to_path_buf),
            current_site_id: params.resolved.current_site_id.clone(),
            origin_site_id: params.resolved.origin_site_id.clone(),
            requested_by: params.resolved.plan_context.requested_by.clone(),
            execution_hints: params.resolved.plan_context.execution_hints.clone(),
            effective_caps: effective_caps.clone(),
            executor_ref: Some(executor_ref.clone()),
            runtime_ref: Some(selected_runtime.canonical_ref.to_string()),
        });
        if let Some(native_resume) = native_resume {
            metadata = metadata.with_native_resume(native_resume);
        }
        if let Some(checkpoint_dir) = checkpoint_dir.clone() {
            metadata = metadata.with_checkpoint_dir(checkpoint_dir);
        }
        Some(metadata)
    } else {
        None
    };
    Ok(PreparedManagedLaunchAuthority {
        resolution,
        prepared_launch,
        effective_vault,
        effective_caps,
        selected_runtime,
        executor_ref,
        checkpoint_dir,
        is_resume,
        launch_metadata,
        pending_project_snapshot,
    })
}

/// Drop guard that finalizes a created thread as `failed` if `build_and_launch`
/// returns before the thread reached a terminal status. This covers the
/// post-create `?` paths (execution policy, limits, resolution pipeline,
/// effective trust, capability mint) that would otherwise leave the row stuck
/// at `created` — the sync `/execute` counterpart of the accepted-launch
/// finalize-on-error net. It no-ops when the thread is already terminal —
/// normal success (the runtime self-finalized), or a path that finalized
/// explicitly — so it never overrides a real outcome.
struct FinalizeFailedOnDrop<'a> {
    state: &'a AppState,
    thread_id: String,
    launch_owner: String,
    /// The launch failure, captured by the wrapper before the guard drops so
    /// the terminal `thread_failed` event carries the cause. `None` only on a
    /// panic/cancellation mid-launch, where no error value exists to record.
    error: Option<Value>,
}

fn current_launch_owner(state: &AppState, thread_id: &str) -> Result<String> {
    state
        .state_store
        .get_launch_claim(thread_id)?
        .map(|claim| claim.claimed_by)
        .ok_or_else(|| anyhow::anyhow!("thread {thread_id} has no current launch owner"))
}

impl Drop for FinalizeFailedOnDrop<'_> {
    fn drop(&mut self) {
        match super::process_attachment::finalize_requested_stop_if_present(
            self.state,
            &self.thread_id,
        ) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => tracing::error!(
                thread_id = %self.thread_id,
                error = %error,
                "failed to settle durable stop while unwinding managed launch"
            ),
        }
        if !self
            .state
            .state_store
            .process_attachment_admission_is_open()
        {
            let _ = self
                .state
                .state_store
                .reset_resume_attempts(&self.thread_id);
            tracing::info!(
                thread_id = %self.thread_id,
                "preserving managed runtime row after shutdown-owned interruption"
            );
            return;
        }
        if let Err(error) = crate::dispatch::finalize_method_thread_if_needed(
            self.state,
            &self.thread_id,
            &self.launch_owner,
            "failed",
            self.error.take(),
        ) {
            tracing::error!(
                thread_id = %self.thread_id,
                error = %error,
                "failed to persist terminal cleanup while unwinding managed launch"
            );
        }
    }
}

pub async fn build_and_launch(
    params: BuildAndLaunchParams<'_>,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    // Allocate identity in memory, then complete the authoritative pass before
    // creating a fresh root or continuation row. A caller-provided ID remains
    // unobservable until its higher-level acknowledgement path receives spawn
    // handoff readiness.
    let thread_id = params
        .pre_minted_thread_id
        .map(str::to_owned)
        .unwrap_or_else(ryeos_app::thread_lifecycle::new_thread_id);
    let mut authority = prepare_managed_launch_authority(&params, &thread_id, None, true).await?;

    let initial_events = launch_audit_records(
        params.resolved,
        &authority.resolution,
        &authority.prepared_launch,
    )?;
    // Reserve the pre-minted ID before publishing the row. The reservation is
    // moved through the whole launch and drops automatically if creation or
    // preparation fails.
    let _launch_claim = ThreadLaunchClaim::acquire_fresh(params.state, &thread_id)
        .map_err(BuildAndLaunchError::Internal)?;
    let thread = match params.previous_thread_id {
        Some(source) => params
            .state
            .threads
            .create_continuation_with_id_and_launch_metadata(
                &thread_id,
                source,
                params.resolved,
                Some("chained_resume"),
                initial_events,
                authority.launch_metadata.as_ref(),
            )?,
        None => params
            .state
            .threads
            .create_root_thread_with_events_and_launch_metadata(
                &thread_id,
                params.resolved,
                initial_events,
                authority.launch_metadata.as_ref(),
            )?,
    };
    drop(authority.pending_project_snapshot.take());
    run_claimed_thread_row_with_authority(
        params,
        thread,
        authority,
        LaunchAuditDisposition::CommittedAtBirth,
    )
    .await
}

/// Run an already-created `created` thread row to completion: resolve, spawn its
/// runtime subprocess, wait, and finalize.
///
/// Split out of `build_and_launch` so a
/// **continuation successor** — an existing `created` row carrying a captured
/// launch identity — can be launched through the SAME path. The successor is
/// re-resolved as **its own kind** (from `resolved.resolved_item.kind`, never
/// assumed directive), and `previous_thread_id` is carried in the envelope so
/// the runtime folds the chain. Behavior-preserving for fresh launches: the
/// body is the original run-half verbatim.
async fn run_claimed_thread_row(
    params: BuildAndLaunchParams<'_>,
    thread: ryeos_app::state_store::ThreadDetail,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    let launch_owner = current_launch_owner(params.state, &thread.thread_id)?;
    // Existing-row paths (native resume/reconcile and rows created by their
    // dedicated lifecycle) must recompute launch authority for every attempt.
    // No persisted runtime data or admission output is accepted here.
    let authority = match prepare_managed_launch_authority(&params, &thread.thread_id, None, false)
        .await
    {
        Ok(authority) => authority,
        Err(error) => {
            let terminal_error = match &error {
                BuildAndLaunchError::LaunchPreparation(dispatch_error) => {
                    crate::structured_error::StructuredErrorPayload::from(dispatch_error.as_ref())
                        .to_value()
                }
                other => json!({
                    "code": "launch_preparation_failed",
                    "message": format!("{other:#}"),
                    "retryable": other.retryable_launch_interruption(),
                }),
            };
            if let Err(cleanup_error) = crate::dispatch::finalize_method_thread_if_needed(
                params.state,
                &thread.thread_id,
                &launch_owner,
                "failed",
                Some(terminal_error),
            ) {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "authoritative launch preparation failed: {error}; terminal cleanup also failed: {cleanup_error:#}"
                )));
            }
            return Err(error);
        }
    };
    run_claimed_thread_row_with_authority(
        params,
        thread,
        authority,
        LaunchAuditDisposition::AppendForAttempt,
    )
    .await
}

async fn run_claimed_thread_row_with_authority(
    params: BuildAndLaunchParams<'_>,
    thread: ryeos_app::state_store::ThreadDetail,
    authority: PreparedManagedLaunchAuthority,
    launch_audit: LaunchAuditDisposition,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    let state = params.state;
    let thread_id = thread.thread_id.clone();
    let launch_owner = current_launch_owner(state, &thread_id)?;
    // Persistence-first net: any failure below finalizes the thread `failed`
    // WITH its cause on the terminal event — a spawn-phase death must never
    // settle as an empty `thread_failed` the operator cannot diagnose. Paths
    // that finalize explicitly (with richer outcome codes) run first; the
    // guard no-ops once the thread is terminal.
    let mut guard = FinalizeFailedOnDrop {
        state,
        thread_id: thread_id.clone(),
        launch_owner: launch_owner.clone(),
        error: None,
    };
    // Declared after the persistence guard so reverse drop order exact-stops
    // and settles any live process tree before the generic finalizer runs.
    let mut lifecycle_owner =
        super::process_attachment::LifecycleOwnerGuard::new(state, &thread_id);
    let result = run_claimed_thread_row_inner(
        params,
        thread,
        authority,
        launch_audit,
        &launch_owner,
        &mut lifecycle_owner,
    )
    .await;
    if let Err(ref err) = result {
        guard.error = Some(json!({
            "code": "launch_failure",
            "message": format!("{err:#}"),
        }));
    }
    result
}

async fn run_claimed_thread_row_inner(
    params: BuildAndLaunchParams<'_>,
    thread: ryeos_app::state_store::ThreadDetail,
    authority: PreparedManagedLaunchAuthority,
    launch_audit: LaunchAuditDisposition,
    launch_owner: &str,
    lifecycle_owner: &mut super::process_attachment::LifecycleOwnerGuard,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    let BuildAndLaunchParams {
        state,
        runtime_ref: _,
        acting_principal,
        resolved,
        project_path,
        provenance,
        parameters,
        metadata_required_secrets,
        pre_minted_thread_id: _,
        previous_thread_id,
        parent_execution_context,
        suppress_stimulus,
        capability_policy: _,
        checkpoint_resume_mode: _,
        launch_handoff,
    } = params;
    let PreparedManagedLaunchAuthority {
        resolution,
        prepared_launch,
        effective_vault,
        effective_caps,
        selected_runtime,
        executor_ref,
        checkpoint_dir,
        is_resume,
        launch_metadata: _,
        pending_project_snapshot,
    } = authority;
    let engine = provenance.request_engine();
    // Runtime-state root: the deliberate `state_root` override when one was
    // requested, otherwise the project path. Resolution stays anchored at
    // `project_path`; only state writes (thread.json here, and the runtime's
    // own writes via `envelope.roots.state_root`) move.
    let runtime_state_root = provenance.state_root_override().unwrap_or(project_path);
    tracing::info!(
        acting_principal,
        item_ref = %resolved.item_ref,
        kind = %resolved.resolved_item.kind,
        required_secret_count = metadata_required_secrets.len(),
        source_root = %project_path.display(),
        state_root = %runtime_state_root.display(),
        "launching native runtime"
    );
    let thread_id = thread.thread_id.clone();
    // Authoritative chain root from the freshly-created thread row (a successor
    // inherits its source's root; a fresh launch is its own root). Used to set
    // the callback cap's chain root.
    let chain_root_id = thread.chain_root_id.clone();
    // Recovery identity is a birth invariant. Fresh roots and continuations
    // seed it atomically before becoming visible; existing-row attempts may
    // recompute launch authority and append a new audit, but never rewrite the
    // persisted identity that selected this row.
    drop(pending_project_snapshot);

    // Record operational lineage the instant we commit to launching a child, so a
    // cancel/kill of the parent can cascade to it. Only a launch carrying a parent
    // execution context is a child — inline-dispatched and follow children both
    // flow through here; a fresh root launch and a continuation successor carry no
    // parent context and are (correctly) not linked. This is fail-closed: the
    // store atomically inherits an already-durable parent stop onto the child.
    if let Some(parent_ctx) = parent_execution_context {
        let inherited_stop = state.state_store.record_child_link(
            &parent_ctx.parent_thread_id,
            &thread_id,
            "dispatch",
        )?;
        if inherited_stop.is_some() {
            super::process_attachment::finalize_requested_stop_if_present(state, &thread_id)?;
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "parent {} was stop-requested before child launch",
                parent_ctx.parent_thread_id
            )));
        }
    }

    // A machine-continuation successor continues its predecessor's work under a
    // fresh thread id and carries no parent execution context, so the block above
    // does not link it. Link it to its immediate predecessor: on continuation the
    // predecessor goes terminal and is a dead end in the descendant walk, so
    // without this a cancel/kill of an ancestor would stop at the (terminal)
    // predecessor and miss the live successor still running — and authoring — the
    // work. (`previous_thread_id` and a parent context are mutually exclusive, so
    // this never contends with the link above.)
    if let Some(previous) = previous_thread_id {
        let inherited_stop =
            state
                .state_store
                .record_child_link(previous, &thread_id, "continuation")?;
        if inherited_stop.is_some() {
            super::process_attachment::finalize_requested_stop_if_present(state, &thread_id)?;
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "predecessor {previous} was stop-requested before continuation launch"
            )));
        }
    }

    let engine_roots = engine.resolution_roots(Some(project_path.to_path_buf()));
    let effective_parsers = engine
        .effective_parser_dispatcher(Some(project_path))
        .map_err(|e| anyhow::anyhow!("effective parser dispatcher: {e}"))?;

    // 2. Compute limits (root execution: depth = 0)
    let root_item_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(&resolved.item_ref)
        .map_err(|e| anyhow::anyhow!("build_and_launch: invalid root item ref: {e}"))?;
    let execution_policy = ryeos_engine::execution_policy::ExecutionPolicyResolver::new(
        ryeos_engine::config_loading::ConfigLoadContext {
            roots: &engine_roots,
            parsers: &effective_parsers,
            kinds: &engine.kinds,
            trust_store: &engine.trust_store,
        },
    )
    .resolve_for_item(&root_item_ref)
    .with_context(|| {
        format!(
            "loading execution policy for item {} in project {}",
            resolved.item_ref,
            project_path.display()
        )
    })?;
    let node_trusted_keys_dir = state.config.runtime_root().trusted_keys_dir();
    let config_loader = build_verified_loader_for_thread(&engine_roots, &node_trusted_keys_dir)
        .context("building verified loader for execution limits config")?;
    let limits_config = load_limits_config_from_loader(&config_loader).with_context(|| {
        format!(
            "loading limits config for project {}",
            project_path.display()
        )
    })?;
    let limits_config = limits_config.unwrap_or_default();
    // Hard limits are computed AFTER the resolution pipeline below (see
    // "compute effective limits"), so the directive-authored header `limits:`
    // can be overlaid onto defaults BELOW execution-policy/caller overrides.
    // The composed header is not available until resolution runs; `hard_limits`
    // is still produced before the TTL / envelope consumers further down.

    // 3. Effective capabilities derivation happens below — sourced
    //    from `resolution.composed.effective_caps` so callback
    //    enforcement and the runtime see the *same* composed capability
    //    set. The callback capability is minted AFTER caps derivation
    //    (V5.5 P2) so the daemon-side dispatcher can enforce caps from
    //    the token instead of trusting the runtime to self-police.

    // 4. Build envelope
    let bundle_roots: Vec<PathBuf> = engine_roots
        .ordered
        .iter()
        .filter(|r| r.space == ryeos_engine::contracts::ItemSpace::Bundle)
        .map(|r| {
            r.ai_root
                .parent()
                .map(|pp| pp.to_path_buf())
                .unwrap_or(r.ai_root.clone())
        })
        .collect();

    tracing::info!(
        item_ref = %resolved.item_ref,
        ancestors = resolution.ancestors.len(),
        references_edges = resolution.references_edges.len(),
        effective_trust_class = ?resolution.effective_trust_class,
        "resolution pipeline complete"
    );

    // Compute effective limits now that the composed header is resolved.
    // The item's authored `limits:` (from the composed view, any kind) overlays
    // its named fields onto the project defaults; omitted fields inherit. The
    // merge is at the JSON level, so the executor names no limit field here.
    // Precedence: defaults → header → execution policy → caller → caps → parent.
    let base_limits = match resolution.composed.composed.get("limits") {
        Some(v) if !v.is_null() => merge_header_limits(&limits_config.defaults, v)?,
        _ => limits_config.defaults.clone(),
    };
    let requested_limits = apply_execution_policy_overrides(&base_limits, &execution_policy);
    let requested_limits = apply_caller_limit_overrides(requested_limits, parameters)?;
    // Parent budget/depth inheritance is trusted control-plane data carried
    // out-of-band (callback token → DispatchRequest). It is never read from
    // action parameters, so runtimes and graph authors cannot spoof it.
    // Missing/empty/null parent limits means "no parent clamp" — never
    // deserialize `{}` into a zero-valued HardLimits, since 0 reads as "no
    // limit" and `min(x, 0)` would erase the child's limits.
    let parent_limits = parent_limits_from_context(parent_execution_context)?;
    // Current launch depth (position in the spawn tree). Callback children use
    // trusted parent depth + 1; roots and same-braid continuations launch at 0.
    let current_depth = launch_depth_from_context(parent_execution_context);
    let hard_limits = compute_effective_limits(
        Some(&requested_limits),
        &limits_config.defaults,
        &limits_config.caps,
        parent_limits.as_ref(),
    );
    let duration_source = if parameters.get("timeout").is_some() {
        "caller param `timeout`".to_string()
    } else {
        execution_policy
            .timeout
            .as_ref()
            .map(|policy| policy.source.describe())
            .unwrap_or_else(|| {
                "directive-runtime/limits.yaml defaults or built-in default".to_string()
            })
    };
    let turns_source = if parameters.get("max_steps").is_some() {
        "caller param `max_steps`".to_string()
    } else {
        execution_policy
            .max_steps
            .as_ref()
            .map(|policy| policy.source.describe())
            .unwrap_or_else(|| {
                "directive-runtime/limits.yaml defaults or built-in default".to_string()
            })
    };
    tracing::info!(
        item_ref = %resolved.item_ref,
        duration_seconds = hard_limits.duration_seconds,
        duration_source,
        duration_cap = ?limits_config.caps.duration_seconds,
        turns = hard_limits.turns,
        turns_source = %turns_source,
        turns_cap = ?limits_config.caps.turns,
        header_limits_present = resolution.composed.composed.get("limits").is_some_and(|v| !v.is_null()),
        execution_policy_override = execution_policy.timeout.is_some() || execution_policy.max_steps.is_some(),
        caller_limit_override = parameters.get("timeout").is_some() || parameters.get("max_steps").is_some(),
        "native launch execution policy resolved"
    );

    // Active trust enforcement: hard-fail before spawn if the daemon
    // resolved an `Unsigned` effective item for ANY kind. The trust posture is
    // the *weakest* of root + every ancestor (`effective_trust`) — a
    // single unsigned link in an extends chain taints the whole
    // executor. There is no per-kind opt-out; the launcher always
    // refuses to spawn an unsigned effective item.
    let effective_trust_class = resolution.effective_trust_class;
    let kind = resolved.resolved_item.kind.as_str();
    enforce_effective_trust(effective_trust_class, &resolved.item_ref, kind)?;

    // The launching kind schema (e.g. `directive`, `graph`) drives
    // inventory build below; it does NOT carry the subprocess
    // terminator — those kinds run in-process inside a runtime.
    let launching_kind_schema =
        engine
            .kinds
            .get(&resolved.resolved_item.kind)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "build_and_launch: launching kind `{}` is not registered",
                    resolved.resolved_item.kind
                )
            })?;

    // The exact verified runtime selected above owns this launch boundary.
    // Its canonical kind selects the schema and signed subprocess protocol;
    // managed runtimes must expose the exact callback/runtime wire contract.
    let verified_protocol =
        crate::dispatch::require_callback_runtime_protocol(engine, &selected_runtime, "managed")
            .map_err(|error| BuildAndLaunchError::Internal(anyhow::anyhow!(error)))?;

    tracing::info!(
        item_ref = %resolved.item_ref,
        kind = kind,
        effective_trust_class = ?effective_trust_class,
        effective_caps_count = effective_caps.len(),
        "launcher policy resolved from composed view"
    );

    // V5.5 P2: mint the callback capability AFTER `effective_caps` is
    // derived so the daemon-side dispatcher can enforce the same set
    // the runtime sees. This closes the trust gap where the runtime
    // was the only entity gating its own callback dispatches.
    // Run-scoped token: must outlive the run's hard timeout + finalization, so
    // a `duration > 3600s` run does not lose callback authority mid-run.
    let ttl = launch_token_ttl(Some(hard_limits.duration_seconds));
    let child_provenance = provenance.clone_for_borrowed_child();
    // The token's project identity is the run's state/callback anchor: the
    // deliberate state-root override when one is in play, else the project.
    // The runtime advertises exactly `envelope.roots.state_root()` on every
    // callback and validation is equality — minting the source root here
    // would reject every dispatch of an overridden run.
    let token_project = provenance
        .state_root_override()
        .unwrap_or(project_path)
        .to_path_buf();
    let cap = state.callback_tokens.generate_with_context(
        &thread_id,
        token_project,
        ttl,
        effective_caps.clone(),
        child_provenance,
        // Same bundle identity the runtime-cap minter used (resolved canonical
        // ref), so token-claimed caps and minted caps cannot diverge.
        effective_bundle_id_for_request(resolved),
        Some(resolved.item_ref.clone()),
        resolution.root.raw_content_digest.clone(),
        serde_json::to_value(&hard_limits).unwrap_or(Value::Null),
        current_depth,
    );
    lifecycle_owner.track_callback_token(cap.token.clone());
    let launch_owner = state
        .state_store
        .get_launch_claim(&thread_id)
        .map_err(BuildAndLaunchError::Internal)?
        .ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed launch has no durable launch owner"
            ))
        })?
        .claimed_by;
    if !state
        .callback_tokens
        .set_launch_owner(&cap.token, launch_owner)
    {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "callback capability disappeared before launch-owner binding"
        )));
    }
    // Carry the thread's authoritative chain root on the cap (it defaults to
    // thread_id / root until set here).
    if !state
        .callback_tokens
        .set_chain_root(&cap.token, &chain_root_id)
    {
        tracing::warn!(
            thread_id = %thread_id,
            "set_chain_root found no cap for the just-minted token; chain root left at default"
        );
    }

    // 6b. Build inventory the launching kind asked for. The engine
    //     enumerates + parses every inventoried item once here so the
    //     runtime is a pure consumer of `envelope.inventory`.
    let inventory = ryeos_engine::inventory::build_inventory_for_launching_kind(
        launching_kind_schema,
        &engine.kinds,
        &engine_roots,
        &effective_parsers,
    )
    .map_err(|e| anyhow::anyhow!("inventory build failed: {e}"))?;

    // 7. Resolve the native executor from the system bundle's CAS.
    //    Materialized to content-addressed cache under app-root state,
    //    not the project tree (works with read-only mounts).
    let cache_root = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state");
    let materialized_binary = materialize_native_executor(
        &bundle_roots,
        &executor_ref,
        &cache_root,
        // Executor manifests authorize node-installed host binaries, so their
        // signer must come from the daemon's persistent node trust store.
        // A project-local key or caller-scoped trust overlay may authorize
        // project content, but it cannot expand node executable authority.
        &engine.node_trust_store,
        ryeos_engine::resolution::TrustClass::TrustedBundle, // executor binaries ship in system bundles
    )?;

    // Fresh roots and continuations committed this audit with `thread_created`
    // in their birth transaction. Existing-row retry/recovery paths append the
    // recomputed trio atomically before handoff.
    match launch_audit {
        LaunchAuditDisposition::CommittedAtBirth => {}
        LaunchAuditDisposition::AppendForAttempt => {
            let launch_audit = launch_audit_records(resolved, &resolution, &prepared_launch)?;
            state
                .threads
                .append_launch_attempt_audit(&chain_root_id, &thread_id, &launch_audit)
                .map_err(|error| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "atomic durable launch audit append failed: {error}"
                    ))
                })?;
        }
    }

    // 8. Build envelope
    //    Using LaunchEnvelopeBuilder to centralize construction and
    //    prevent future field drift. New fields on LaunchEnvelope
    //    only need updating in the builder, not at every call site.
    let envelope = LaunchEnvelopeBuilder::new(
        cap.invocation_id.clone(),
        thread_id.clone(),
        EnvelopeRoots {
            project_root: project_path.to_path_buf(),
            bundle_roots,
            node_trusted_keys_dir,
            // Deliberate runtime state-root override, carried so the runtime
            // can target its state writes (thread state, transcripts, thread
            // knowledge) away from the source project.
            state_root: provenance.state_root_override().map(Path::to_path_buf),
        },
        EnvelopeRequest {
            // Strip runtime-control fields from prompt inputs. Parent
            // budget/depth now travels out-of-band, but rejecting prompt leaks
            // here keeps forged caller/action fields from becoming model input.
            inputs: prompt_inputs_from_parameters(parameters),
            previous_thread_id: previous_thread_id.map(str::to_string),
            parent_thread_id: parent_execution_context.map(|ctx| ctx.parent_thread_id.clone()),
            parent_capabilities: None,
            depth: current_depth,
            suppress_stimulus,
        },
        EnvelopePolicy {
            effective_caps,
            hard_limits: hard_limits.clone(),
        },
        EnvelopeCallback {
            socket_path: state.config.uds_path.clone(),
            token: cap.token.clone(),
        },
        resolution,
    )
    .runtime_data(prepared_launch.runtime_data.clone())
    .inventory(inventory)
    .build();

    // 8. Write thread.json (status = created, pre-execution audit).
    //    `effective_trust_class` is recorded so the on-disk audit trail
    //    matches what the launcher used for spawn-gating. The record is
    //    rewritten twice more: to `running` at the exec boundary inside the
    //    blocking spawn task, and to its settled status (+completion time,
    //    cost, outputs) after finalization below — so the file tracks the
    //    execution instead of reading `created` forever.
    let meta = ThreadMeta {
        thread_id: thread_id.clone(),
        status: "created".to_string(),
        item_ref: resolved.item_ref.clone(),
        capabilities: envelope.policy.effective_caps.clone(),
        limits: serde_json::to_value(&hard_limits)?,
        ref_bindings: resolved.ref_bindings.clone(),
        binding_launch_records: prepared_launch.binding_records.clone(),
        runtime_facts: prepared_launch.runtime_facts.clone(),
        started_at: lillux::time::iso8601_now(),
        completed_at: None,
        cost: None,
        outputs: None,
        effective_trust_class,
    };
    let identity = &state.identity;
    super::thread_meta::write_thread_meta(runtime_state_root, &thread_id, &meta, identity)?;

    // 9. Spawn runtime (env vars + stdin envelope)
    //
    // `spawn_runtime` calls `lillux::run` which is a fully synchronous
    // subprocess wait (std::process + blocking pipe drains). Calling
    // it directly inside an async fn pins the current Tokio worker
    // for the entire runtime lifetime — and the runtime's first action
    // is a `runtime.mark_running` UDS callback. If the daemon's UDS
    // server task is scheduled on the same worker, the runtime
    // deadlocks waiting for a response that never comes (oracle review
    // of P3b.2 hang). `spawn_blocking` moves the wait onto Tokio's
    // dedicated blocking pool so async workers stay free to service
    // UDS callbacks.
    let materialized_binary_path = materialized_binary.path;
    let binary_path = materialized_binary_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("materialized runtime path is not valid UTF-8"))?
        .to_owned();
    let isolation_verified_command = ryeos_engine::isolation::IsolationVerifiedCode {
        source_path: materialized_binary_path,
        content_hash: materialized_binary.content_hash,
    };
    let project_owned = project_path.to_path_buf();
    let acting_principal_owned = acting_principal.to_string();
    let callback_owned = envelope.callback.clone();
    let thread_id_owned = thread_id.to_string();
    let duration = hard_limits.duration_seconds;
    let descriptor_clone = verified_protocol.descriptor.clone();
    let runtime_item_ref = selected_runtime.canonical_ref.clone();
    // The native-runtime spawn pipe must include vault_bindings the
    // same way `services::thread_lifecycle::spawn_item` does for
    // generic plan-node subprocesses. Without this, operator secrets
    // never reach the runtime — the trait machinery in `vault.rs`
    // gets called and discarded.
    let vault_owned: Vec<(String, String)> = effective_vault
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let thread_auth = descriptor_clone
        .env_injections
        .iter()
        .any(|injection| {
            injection.source
                == ryeos_engine::protocol_vocabulary::EnvInjectionSource::ThreadAuthToken
        })
        .then(|| {
            state.thread_auth.mint(
                &thread_id,
                acting_principal.to_string(),
                vec!["execute".to_string()],
                ttl,
            )
        });
    let tat_owned = thread_auth
        .as_ref()
        .map(|auth| auth.token.clone())
        .ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "verified managed runtime protocol does not request thread auth"
            ))
        })?;
    lifecycle_owner.track_thread_auth_token(tat_owned.clone());
    let runtime_roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
        &engine_roots,
        &state.config.app_root,
    )?;
    let isolation = state.isolation.clone();
    let isolation_project_authority = provenance.isolation_project_authority();
    let isolation_state_root = provenance
        .state_root_override()
        .map(std::path::Path::to_path_buf);
    let isolation_workspace_lifeline = provenance.workspace_lifeline();
    let cas_root_owned = state
        .state_store
        .cas_root()
        .map_err(BuildAndLaunchError::Internal)?;
    let checkpoint_dir_owned = checkpoint_dir.clone();
    // Execution starts at the exec boundary inside the blocking task, and the
    // launcher then blocks for the runtime's whole lifetime — so the flip of
    // the audit record from its pre-execution `created` posture to `running`
    // must happen in there, not out here. Best-effort: the audit file never
    // blocks a launch.
    let running_meta = ThreadMeta {
        status: "running".to_string(),
        ..meta.clone()
    };
    let state_root_for_spawn = runtime_state_root.to_path_buf();
    let identity_for_spawn = state.identity.clone();
    let state_for_spawn = (*state).clone();

    let spawn_handle = tokio::task::spawn_blocking(move || {
        if let Err(e) = super::thread_meta::write_thread_meta(
            &state_root_for_spawn,
            &thread_id_owned,
            &running_meta,
            &identity_for_spawn,
        ) {
            tracing::warn!(
                thread_id = %thread_id_owned,
                error = %e,
                "failed to update thread.json audit record to running"
            );
        }
        spawn_runtime(SpawnRuntimeParams {
            state: &state_for_spawn,
            descriptor: &descriptor_clone,
            item_ref: &runtime_item_ref,
            acting_principal: &acting_principal_owned,
            binary: &binary_path,
            project_path: &project_owned,
            project_authority: isolation_project_authority,
            state_root: isolation_state_root.as_deref(),
            workspace_lifeline: isolation_workspace_lifeline,
            envelope: &envelope,
            timeout_secs: duration,
            callback: &callback_owned,
            thread_id: &thread_id_owned,
            vault_bindings: &vault_owned,
            thread_auth_token: &tat_owned,
            roots: runtime_roots,
            isolation: isolation.as_ref(),
            verified_command: &isolation_verified_command,
            cas_root: &cas_root_owned,
            checkpoint_dir: checkpoint_dir_owned.as_deref(),
            // A machine continuation of a replay-aware kind resumes from the
            // predecessor's copied-forward checkpoint; a fresh launch writes a
            // cold one.
            is_resume,
        })
    });

    // The row and complete launch audit are durable, and the exact in-memory
    // authority (envelope runtime_data + resolved secret injection set) is now
    // owned by the scheduled spawn task. This is the acknowledgement boundary;
    // actual child start may race with network delivery by design.
    if let Some(handoff) = launch_handoff {
        handoff.publish(thread_id.clone());
    }

    let spawned_runtime = spawn_handle
        .await
        .map_err(|e| anyhow::anyhow!("spawn_runtime join error: {e}"))??;
    let spawn_result = tokio::task::spawn_blocking(move || spawned_runtime.wait())
        .await
        .map_err(|e| anyhow::anyhow!("runtime wait join error: {e}"))?;
    // The owned wait has completed and compare-cleared the exact attached
    // identity. Revoke callback and thread-auth authority before result handling.
    lifecycle_owner.disarm();

    // Prune stale capabilities from other completed threads
    let pruned = state.callback_tokens.prune_expired();
    state.thread_auth.prune_expired();
    if pruned > 0 {
        tracing::debug!(pruned, "cleaned up expired callback capabilities");
    }

    // 11. Handle spawn result
    let mut runtime_result = match spawn_result {
        Ok(result) => result,
        Err(err) => {
            if super::process_attachment::finalize_requested_stop_if_present(state, &thread_id)? {
                return Err(BuildAndLaunchError::Internal(err));
            }
            if !state.state_store.process_attachment_admission_is_open() {
                let _ = state.state_store.reset_resume_attempts(&thread_id);
                return Err(BuildAndLaunchError::Internal(err));
            }
            // Pre-runtime failure (launch preparation, secret resolution, materialization,
            // builder): record the real cause into `error` — the ONLY field the
            // terminal `thread_failed` braid event persists — not `result`,
            // which is dropped. Without this the operator only ever sees a bare
            // "failed" and is locked out of why the thread died. `{err:#}` keeps
            // the full cause chain (e.g. "missing required secret …").
            let _ = state.threads.finalize_thread_owned(
                &ThreadFinalizeParams {
                    thread_id: thread_id.clone(),
                    status: "failed".to_string(),
                    outcome_code: Some("pre_runtime_failure".to_string()),
                    result: None,
                    error: Some(json!({
                        "code": "pre_runtime_failure",
                        "message": format!("{err:#}"),
                    })),
                    metadata: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    summary_json: None,
                },
                launch_owner,
            );
            let failed_meta = ThreadMeta {
                status: "failed".to_string(),
                completed_at: Some(lillux::time::iso8601_now()),
                ..meta
            };
            let _ = super::thread_meta::write_thread_meta(
                runtime_state_root,
                &thread_id,
                &failed_meta,
                identity,
            );
            return Err(BuildAndLaunchError::Internal(err));
        }
    };

    if !state.state_store.process_attachment_admission_is_open() {
        let _ = state.state_store.reset_resume_attempts(&thread_id);
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "managed runtime interrupted by daemon shutdown; row preserved for recovery"
        )));
    }

    // 12. Build response from DB thread. Normally the runtime already
    // finalized via callback. If the subprocess exits before it can do that
    // (for example a hard timeout/SIGKILL), fail closed by finalizing here;
    // otherwise streaming callers tailing until terminal degrade into a
    // misleading `thread_not_terminal` error.
    let mut thread_detail = state.threads.get_thread(&thread_id)?.unwrap_or(thread);
    let already_finalized = is_thread_terminal_status(&thread_detail.status);
    if !already_finalized {
        let mut terminal_status = runtime_terminal_status(runtime_result.status);
        // Kill-intent: a subprocess SIGKILLed by a daemon-issued `kill` exits
        // abnormally with no self-finalization, which maps to `failed`. If
        // a kill was requested for this thread, that stop was intentional —
        // settle `killed`, not `failed`, so the terminal reflects the operator's
        // action instead of looking like a crash.
        if terminal_status == ryeos_state::objects::ThreadStatus::Failed
            && state.state_store.thread_has_kill_command(&thread_id)?
        {
            terminal_status = ryeos_state::objects::ThreadStatus::Killed;
        }
        let fallback = fallback_finalization(&thread_id, &runtime_result, terminal_status);
        runtime_result = fallback.runtime_result;
        let finalized = state
            .threads
            .finalize_thread_with_managed_envelope(&fallback.params, fallback.managed_envelope)?;
        // Live parent-resume kick: a followed child finalized on this fallback
        // (abnormal exit, no self-finalize over the callback) still flips its waiter
        // to `ready`, so wake the parent now instead of waiting for a restart.
        kick_follow_resume_if_ready(state, &finalized.chain_root_id);
        kick_launch_window_for_terminal(state, &finalized.chain_root_id);
        thread_detail = finalized;
    } else {
        let authority = state
            .threads
            .get_thread_terminal_authority(&thread_id)?
            .ok_or_else(|| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "already-finalized thread {thread_id} has no authoritative terminal snapshot"
                ))
            })?;
        runtime_result = reconcile_terminal_finalization(&authority, &runtime_result)
            .map_err(BuildAndLaunchError::Internal)?;
    }

    // The audit record follows the execution to its settled state: the real
    // status (terminal, or `continued` on a handoff), completion time, and
    // cost land beside the launch-time posture — instead of `created`/
    // `running` sitting on disk forever. Best-effort like every audit write.
    let settled_meta = ThreadMeta {
        status: thread_detail.status.clone(),
        completed_at: (thread_detail.status
            != ryeos_state::objects::ThreadStatus::Continued.as_str())
        .then(lillux::time::iso8601_now),
        cost: runtime_result
            .cost
            .as_ref()
            .and_then(|c| serde_json::to_value(c).ok()),
        outputs: (!runtime_result.outputs.is_null()).then(|| runtime_result.outputs.clone()),
        ..meta
    };
    if let Err(e) = super::thread_meta::write_thread_meta(
        runtime_state_root,
        &thread_id,
        &settled_meta,
        identity,
    ) {
        tracing::warn!(
            thread_id = %thread_id,
            error = %e,
            "failed to update thread.json audit record to its settled status"
        );
    }

    // The runtime returns terminal text in `result` (Option<String>) and any
    // non-fatal callback drift in `warnings`. Both must be visible to the
    // HTTP caller — dropping `result` would silently lose the assistant's
    // last message; dropping `warnings` would silently lose contract-drift
    // diagnostics surfaced via `record_callback_warning`.
    Ok(NativeLaunchResult {
        thread: serde_json::to_value(&thread_detail)?,
        result: json!({
            "success": runtime_result.success,
            "status": runtime_result.status,
            "result": runtime_result.result,
            "outputs": runtime_result.outputs,
            "cost": runtime_result.cost,
            "warnings": runtime_result.warnings,
        }),
    })
}

/// Outcome of a successor launch attempt.
///
/// `Launched` ran the successor to terminal. `Skipped` is a **benign no-op** —
/// another launcher owns the claim (`already_claimed`), the row is no longer
/// `created` (`not_created`), or the per-successor attempt budget was exhausted
/// and the row finalized (`budget_exhausted`). Callers log `Skipped` at debug,
/// not error. A real launch defect is still `Err`.
pub enum SuccessorLaunchOutcome {
    Launched(NativeLaunchResult),
    Skipped(&'static str),
}

/// Startup recovery preparation result.
///
/// `Enqueued` means this daemon first persisted the launch claim and then moved
/// that owned claim into a detached runtime task. `Skipped` is a classified
/// benign no-op; no unowned in-memory work is reported as queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryLaunchOutcome {
    Enqueued,
    Skipped(&'static str),
}

/// Which kind of successor launch this is — they share the claim/run machinery
/// but differ on stimulus and capability/budget policy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SuccessorMode {
    /// Autonomous limit cut-off: fold the chain, inject NO new stimulus, pin
    /// authority to the predecessor's captured caps, and enforce the per-successor
    /// auto-launch attempt budget.
    Machine,
    /// Explicit operator follow-up: inject the operator's input as the opening
    /// stimulus, re-derive caps fresh (no pin), and skip the auto-launch budget
    /// (an operator action is not an autonomous relaunch).
    Operator,
    /// Follow-resume: fold the chain with NO new stimulus and pin authority like
    /// Machine, but resume from the successor's OWN checkpoint dir — the follow-
    /// resume launcher has already copied the predecessor's checkpoint in and
    /// spliced the child's result — so no predecessor re-copy, and skip the
    /// autonomous auto-launch budget (this relaunch is child-terminal-driven).
    Follow,
}

/// Launch a continuation successor: an existing `created` thread row carrying a
/// captured `ResumeContext` and an `upstream_thread_id`.
///
/// Claims the launch lease (so only one launcher acts, and a dead launcher's
/// claim is reclaimable), reconstructs the execution from the captured identity
/// — re-resolved as the successor's OWN kind, never assumed directive — and runs
/// it through [`run_claimed_thread_row`] with `previous_thread_id` set so the
/// runtime folds the chain. A MACHINE continuation injects no new stimulus.
///
/// Fire-and-forget from the daemon: the machine path `tokio::spawn`s this after
/// the source is settled `continued`, and reconcile calls it for crash recovery.
/// Takes `state` by value so the spawned task can own it. Blocks until the
/// successor reaches terminal (inside its detached task).
pub async fn launch_successor(
    state: AppState,
    successor_id: &str,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    launch_successor_inner(state, successor_id, SuccessorMode::Machine, None, None).await
}

/// Launch a pre-created OPERATOR follow-up successor (an existing `created` row
/// with a seeded `ResumeContext`). Claim-guarded like [`launch_successor`] but
/// injects the operator's input as the opening stimulus and does not pin caps or
/// consume the auto-launch budget. Used by the `threads/input` path after a
/// synchronous create-or-get, and to "ensure launch" a stranded `created`
/// operator successor on a duplicate submit.
pub async fn launch_operator_successor(
    state: AppState,
    successor_id: &str,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    launch_successor_inner(state, successor_id, SuccessorMode::Operator, None, None).await
}

/// Consumable authoritative pass for one exact successor ID. Fresh creation and
/// existing-row retries share the carrier, but differ explicitly in whether the
/// audit was committed at birth. It contains secret values and must never be
/// persisted or cloned.
struct PreparedSuccessorLaunch {
    thread_id: String,
    mode: SuccessorMode,
    source_thread_id: Option<String>,
    resume_context: ryeos_app::launch_metadata::ResumeContext,
    execution: crate::execution::runner::ExecutionParams,
    launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    authority: PreparedManagedLaunchAuthority,
    launch_audit: LaunchAuditDisposition,
}

pub struct PreparedOperatorSuccessorLaunch {
    prepared: PreparedSuccessorLaunch,
}

impl PreparedOperatorSuccessorLaunch {
    pub fn initial_audit_events(
        &self,
    ) -> Result<Vec<ryeos_app::state_store::NewEventRecord>, BuildAndLaunchError> {
        launch_audit_records(
            &self.prepared.execution.resolved,
            &self.prepared.authority.resolution,
            &self.prepared.authority.prepared_launch,
        )
    }

    pub fn launch_metadata(&self) -> &ryeos_app::launch_metadata::RuntimeLaunchMetadata {
        &self.prepared.launch_metadata
    }

    /// Mark that the authoritative audit committed with a newly-created
    /// successor. Existing-row retry preparations deliberately retain
    /// `AppendForAttempt` so their recomputed audit is appended before handoff.
    pub fn with_persisted_birth_audit(mut self) -> Self {
        self.prepared.launch_audit = LaunchAuditDisposition::CommittedAtBirth;
        drop(self.prepared.authority.pending_project_snapshot.take());
        self
    }
}

pub struct PreparedMachineSuccessorLaunch {
    prepared: PreparedSuccessorLaunch,
}

impl PreparedMachineSuccessorLaunch {
    pub fn with_persisted_birth_audit(mut self) -> Self {
        self.prepared.launch_audit = LaunchAuditDisposition::CommittedAtBirth;
        drop(self.prepared.authority.pending_project_snapshot.take());
        self
    }
}

/// Consumable authoritative launch pass for a fresh lineage-linked child.
///
/// The value deliberately owns the borrowed-child provenance, resolved launch
/// authority, and secret values. It can cross only the in-process spawn-task
/// boundary; only its explicit `launch_metadata` projection is persisted.
pub struct PreparedFollowChildLaunch {
    thread_id: String,
    resume_context: ryeos_app::launch_metadata::ResumeContext,
    parent_context: crate::dispatch::ParentExecutionContext,
    execution: crate::execution::runner::ExecutionParams,
    launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    authority: PreparedManagedLaunchAuthority,
    launch_audit: LaunchAuditDisposition,
}

impl PreparedFollowChildLaunch {
    pub fn resolved_request(&self) -> &ResolvedExecutionRequest {
        &self.execution.resolved
    }

    pub fn initial_audit_events(
        &self,
    ) -> Result<Vec<ryeos_app::state_store::NewEventRecord>, BuildAndLaunchError> {
        launch_audit_records(
            &self.execution.resolved,
            &self.authority.resolution,
            &self.authority.prepared_launch,
        )
    }

    pub fn launch_metadata(&self) -> &ryeos_app::launch_metadata::RuntimeLaunchMetadata {
        &self.launch_metadata
    }

    /// Mark that `initial_audit_events` committed atomically with this fresh
    /// root. Re-driven pre-existing rows retain `AppendForAttempt` so the
    /// recomputed audit is appended before their spawn handoff.
    pub fn with_persisted_birth_audit(mut self) -> Self {
        self.launch_audit = LaunchAuditDisposition::CommittedAtBirth;
        drop(self.authority.pending_project_snapshot.take());
        self
    }
}

/// Perform the complete generic authority pass for a fresh follow/detached
/// child before its row becomes observable.
pub async fn prepare_follow_child_launch(
    state: &AppState,
    thread_id: &str,
    launch_metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    parent_context: crate::dispatch::ParentExecutionContext,
) -> Result<PreparedFollowChildLaunch, BuildAndLaunchError> {
    prepare_follow_child_launch_inner(
        state,
        thread_id,
        launch_metadata,
        provenance,
        parent_context,
        true,
    )
    .await
}

/// Recompute one launch attempt for an already-persisted child. The birth
/// identity is immutable: preparation starts from and returns the exact stored
/// metadata, and snapshot publication is disabled because the existing row is
/// already the authoritative GC root.
pub async fn prepare_existing_follow_child_launch(
    state: &AppState,
    thread_id: &str,
    launch_metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    parent_context: crate::dispatch::ParentExecutionContext,
) -> Result<PreparedFollowChildLaunch, BuildAndLaunchError> {
    let persisted_parent = launch_metadata
        .follow_parent_context
        .as_ref()
        .ok_or_else(|| {
            anyhow::anyhow!("follow child {thread_id} has no persisted parent execution context")
        })?;
    if persisted_parent.parent_thread_id != parent_context.parent_thread_id
        || persisted_parent.hard_limits != parent_context.hard_limits
        || persisted_parent.depth != parent_context.depth
    {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow child {thread_id} re-drive parent context differs from its persisted birth identity"
        )));
    }
    prepare_follow_child_launch_inner(
        state,
        thread_id,
        launch_metadata,
        provenance,
        parent_context,
        false,
    )
    .await
}

async fn prepare_follow_child_launch_inner(
    state: &AppState,
    thread_id: &str,
    launch_metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    parent_context: crate::dispatch::ParentExecutionContext,
    capture_project_snapshot: bool,
) -> Result<PreparedFollowChildLaunch, BuildAndLaunchError> {
    let resume = launch_metadata
        .resume_context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("follow-child launch metadata has no ResumeContext"))?;
    // Resolve directly through the borrowed provenance. Going through resume
    // reconstruction first could consult the daemon's current engine or create
    // a second snapshot checkout, neither of which is the admitted child source.
    let engine = provenance.request_engine();
    let admitted_request = launch_metadata
        .sealed_root_request
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("follow-child launch metadata has no sealed root request"))?
        .restore(engine)
        .context("restore follow-child sealed root request")?;
    if admitted_request.kind != resume.kind
        || admitted_request.item_ref != resume.item_ref
        || admitted_request.launch_mode != resume.launch_mode
        || admitted_request.parameters != resume.parameters
        || admitted_request.ref_bindings != resume.ref_bindings
        || admitted_request.current_site_id != resume.current_site_id
        || admitted_request.origin_site_id != resume.origin_site_id
        || admitted_request.requested_by.as_deref() != Some(resume.principal_identifier())
        || admitted_request.plan_context.requested_by != resume.requested_by
        || admitted_request.plan_context.project_context != resume.project_context
        || admitted_request.plan_context.execution_hints != resume.execution_hints
        || resume.executor_ref.as_deref() != Some(admitted_request.executor_ref.as_str())
        || resume.runtime_ref.as_deref()
            != launch_metadata
                .sealed_root_request
                .as_ref()
                .map(|sealed| sealed.runtime_ref())
    {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow-child launch envelope does not match its sealed root request for {}",
            resume.item_ref
        )));
    }
    let canonical =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&resume.item_ref).map_err(|error| {
            anyhow::anyhow!(
                "child launch: invalid item ref {}: {error}",
                resume.item_ref
            )
        })?;
    let plan_context = ryeos_engine::contracts::PlanContext {
        requested_by: resume.requested_by.clone(),
        project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
            path: provenance.effective_path().to_path_buf(),
        },
        current_site_id: resume.current_site_id.clone(),
        origin_site_id: resume.origin_site_id.clone(),
        execution_hints: resume.execution_hints.clone(),
        validate_only: false,
    };
    let admission_primary = engine.resolve(&plan_context, &canonical).map_err(|error| {
        BuildAndLaunchError::from(map_follow_child_resolution_error(
            "admission",
            &resume.item_ref,
            error,
        ))
    })?;
    let executor_ref = resume.executor_ref.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "child launch: {} has no captured executor identity",
            resume.item_ref
        )
    })?;
    let acting_principal = resume.principal_identifier().to_string();
    let caller_scopes = match &resume.requested_by {
        ryeos_engine::contracts::EffectivePrincipal::Local(principal) => principal.scopes.clone(),
        ryeos_engine::contracts::EffectivePrincipal::Delegated(principal) => {
            principal.delegated_scopes.clone()
        }
    };
    let admission_context = crate::executor::ExecutionContext {
        principal_fingerprint: acting_principal.clone(),
        caller_scopes,
        engine: engine.clone(),
        plan_ctx: plan_context.clone(),
        requested_call: None,
    };
    let applicability =
        crate::dispatch::launch_contract_applicability(&resume.item_ref, &admission_context)
            .map_err(BuildAndLaunchError::from)?;
    crate::dispatch::admit_launch_contract(
        &applicability,
        &admission_primary,
        &resume.ref_bindings,
        &provenance,
        &admission_context,
        state,
    )
    .map_err(BuildAndLaunchError::from)?;

    // The authoritative pass begins from a fresh primary resolution against the
    // exact borrowed-provenance engine. No admission output crosses this seam.
    let resolved_item = engine.resolve(&plan_context, &canonical).map_err(|error| {
        BuildAndLaunchError::from(map_follow_child_resolution_error(
            "authority",
            &resume.item_ref,
            error,
        ))
    })?;
    let root_raw_content_digest = resolved_item.raw_content_digest.clone();
    let execution = crate::execution::runner::ExecutionParams {
        resolved: ResolvedExecutionRequest {
            kind: resume.kind.clone(),
            item_ref: resume.item_ref.clone(),
            executor_ref,
            launch_mode: resume.launch_mode.clone(),
            current_site_id: resume.current_site_id.clone(),
            origin_site_id: resume.origin_site_id.clone(),
            target_site_id: None,
            requested_by: Some(resume.principal_identifier().to_string()),
            usage_subject: None,
            usage_subject_asserted_by: None,
            parameters: resume.parameters.clone(),
            ref_bindings: resume.ref_bindings.clone(),
            root_raw_content_digest,
            resolved_item,
            plan_context,
            root_admission: admitted_request.root_admission,
        },
        acting_principal,
        vault_bindings: HashMap::new(),
        parameters: resume.parameters.clone(),
        pre_minted_thread_id: None,
        effective_caps: resume.effective_caps.clone(),
        provenance,
        runtime_ref: resume.runtime_ref.clone(),
        parent_thread_id: None,
    };

    let project_path = execution.provenance.effective_path().to_path_buf();
    let authority = prepare_managed_launch_authority(
        &BuildAndLaunchParams {
            state,
            runtime_ref: resume.runtime_ref.as_deref(),
            acting_principal: &execution.acting_principal,
            resolved: &execution.resolved,
            project_path: &project_path,
            provenance: &execution.provenance,
            parameters: &execution.parameters,
            metadata_required_secrets: &execution.resolved.resolved_item.metadata.required_secrets,
            pre_minted_thread_id: None,
            previous_thread_id: None,
            parent_execution_context: Some(&parent_context),
            suppress_stimulus: false,
            capability_policy: CapabilityPolicy::FollowChildHybrid {
                parent_effective_caps: resume.effective_caps.as_slice(),
            },
            checkpoint_resume_mode: CheckpointResumeMode::None,
            launch_handoff: None,
        },
        thread_id,
        Some(launch_metadata),
        capture_project_snapshot,
    )
    .await?;
    let mut launch_metadata =
        if capture_project_snapshot {
            authority.launch_metadata.as_ref().cloned().ok_or_else(|| {
                anyhow::anyhow!("follow-child authority produced no launch metadata")
            })?
        } else {
            launch_metadata.clone()
        };
    // The prepared authority carries the CHILD's composed capabilities for the
    // actual launch. The durable pre-launch ResumeContext has a different,
    // explicit role: it must retain the PARENT's capabilities so a hot launch
    // and a crash/reconcile launch apply the same FollowChildHybrid bound.
    // `prepare_managed_launch_authority` refreshes ResumeContext from the child
    // resolution, so restore only this overloaded birth-identity field while
    // preserving its newly pinned snapshot and all other prepared metadata.
    if capture_project_snapshot {
        launch_metadata
            .resume_context
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("follow-child authority produced no ResumeContext"))?
            .effective_caps
            .clone_from(&resume.effective_caps);
    }
    let prepared_resume = launch_metadata
        .resume_context
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("follow-child authority produced no ResumeContext"))?;

    Ok(PreparedFollowChildLaunch {
        thread_id: thread_id.to_string(),
        resume_context: prepared_resume,
        parent_context,
        execution,
        launch_metadata,
        authority,
        launch_audit: LaunchAuditDisposition::AppendForAttempt,
    })
}

fn map_follow_child_resolution_error(
    phase: &'static str,
    item_ref: &str,
    error: ryeos_engine::error::EngineError,
) -> DispatchError {
    use ryeos_engine::error::EngineError;

    let detail = error.to_string();
    match error {
        EngineError::ItemNotFound { .. }
        | EngineError::PinnedVersionNotFound { .. }
        | EngineError::EffectiveItemNotFound { .. } => DispatchError::LaunchResourceNotFound {
            code: "follow_child_item_not_found".to_owned(),
            message: format!("follow-child {phase} item `{item_ref}` was not found"),
            binding: None,
        },
        EngineError::ItemResolutionUnavailable { .. }
        | EngineError::ProjectContextMaterializationFailed { .. }
        | EngineError::BundleDiscoveryFailed { .. } => DispatchError::LaunchPreparationFailed {
            code: "follow_child_resolution_failed".to_owned(),
            message: format!(
                "follow-child {phase} resolution dependency is unavailable for `{item_ref}`: {detail}"
            ),
            classification: "unavailable".to_owned(),
            binding: None,
            details: Box::new(BTreeMap::new()),
        },
        _ => {
            DispatchError::LaunchPreparationFailed {
                code: "follow_child_resolution_failed".to_owned(),
                message: format!(
                    "follow-child {phase} item `{item_ref}` has an invalid definition: {detail}"
                ),
                classification: "configuration".to_owned(),
                binding: None,
                details: Box::new(BTreeMap::new()),
            }
        }
    }
}

impl PreparedMachineSuccessorLaunch {
    pub fn initial_audit_events(
        &self,
    ) -> Result<Vec<ryeos_app::state_store::NewEventRecord>, BuildAndLaunchError> {
        launch_audit_records(
            &self.prepared.execution.resolved,
            &self.prepared.authority.resolution,
            &self.prepared.authority.prepared_launch,
        )
    }

    pub fn launch_metadata(&self) -> &ryeos_app::launch_metadata::RuntimeLaunchMetadata {
        &self.prepared.launch_metadata
    }
}

async fn prepare_successor_launch(
    state: &AppState,
    successor_thread_id: &str,
    resume: &ryeos_app::launch_metadata::ResumeContext,
    mode: SuccessorMode,
    previous_thread_id: Option<&str>,
    metadata_template: Option<&ryeos_app::launch_metadata::RuntimeLaunchMetadata>,
    capture_project_snapshot: bool,
) -> Result<PreparedSuccessorLaunch, BuildAndLaunchError> {
    let execution = crate::execution::runner::execution_params_from_resume_context(state, resume)?;
    let project_path = execution.provenance.effective_path().to_path_buf();
    let (suppress_stimulus, capability_policy, checkpoint_resume_mode) = match mode {
        SuccessorMode::Machine => (
            true,
            CapabilityPolicy::ExactPinned(resume.effective_caps.as_slice()),
            CheckpointResumeMode::MachineContinuation,
        ),
        SuccessorMode::Operator => (false, CapabilityPolicy::Fresh, CheckpointResumeMode::None),
        SuccessorMode::Follow => {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "follow successor authority cannot be prepared by this path"
            )));
        }
    };
    let authority = prepare_managed_launch_authority(
        &BuildAndLaunchParams {
            state,
            runtime_ref: resume.runtime_ref.as_deref(),
            acting_principal: &execution.acting_principal,
            resolved: &execution.resolved,
            project_path: &project_path,
            provenance: &execution.provenance,
            parameters: &execution.parameters,
            metadata_required_secrets: &execution.resolved.resolved_item.metadata.required_secrets,
            pre_minted_thread_id: None,
            previous_thread_id,
            parent_execution_context: None,
            suppress_stimulus,
            capability_policy,
            checkpoint_resume_mode,
            launch_handoff: None,
        },
        successor_thread_id,
        metadata_template,
        capture_project_snapshot,
    )
    .await?;
    let launch_metadata = if let Some(persisted) = metadata_template {
        persisted.clone()
    } else {
        authority
            .launch_metadata
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("successor authority produced no launch metadata"))?
    };
    let prepared_resume = launch_metadata
        .resume_context
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("successor authority produced no ResumeContext"))?;
    Ok(PreparedSuccessorLaunch {
        thread_id: successor_thread_id.to_string(),
        mode,
        source_thread_id: previous_thread_id.map(str::to_owned),
        resume_context: prepared_resume,
        execution,
        launch_metadata,
        authority,
        launch_audit: LaunchAuditDisposition::AppendForAttempt,
    })
}

pub async fn prepare_operator_successor_launch(
    state: &AppState,
    successor_thread_id: &str,
    resume: &ryeos_app::launch_metadata::ResumeContext,
) -> Result<PreparedOperatorSuccessorLaunch, BuildAndLaunchError> {
    Ok(PreparedOperatorSuccessorLaunch {
        prepared: prepare_successor_launch(
            state,
            successor_thread_id,
            resume,
            SuccessorMode::Operator,
            None,
            None,
            true,
        )
        .await?,
    })
}

/// Reprepare a stranded operator successor against its actual durable ID and
/// birth identity. This produces a fresh attempt audit but never captures or
/// publishes a replacement snapshot.
pub async fn prepare_existing_operator_successor_launch(
    state: &AppState,
    successor_thread_id: &str,
    launch_metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
) -> Result<PreparedOperatorSuccessorLaunch, BuildAndLaunchError> {
    let resume = launch_metadata.resume_context.as_ref().ok_or_else(|| {
        anyhow::anyhow!("operator successor {successor_thread_id} has no persisted ResumeContext")
    })?;
    Ok(PreparedOperatorSuccessorLaunch {
        prepared: prepare_successor_launch(
            state,
            successor_thread_id,
            resume,
            SuccessorMode::Operator,
            None,
            Some(launch_metadata),
            false,
        )
        .await?,
    })
}

pub async fn prepare_machine_successor_launch(
    state: &AppState,
    successor_thread_id: &str,
    resume: &ryeos_app::launch_metadata::ResumeContext,
    source_thread_id: &str,
) -> Result<PreparedMachineSuccessorLaunch, BuildAndLaunchError> {
    let mut prepared = prepare_successor_launch(
        state,
        successor_thread_id,
        resume,
        SuccessorMode::Machine,
        Some(source_thread_id),
        None,
        false,
    )
    .await?;

    // A machine continuation is another segment of the same admitted launch,
    // not a fresh launch identity. Preparing it may materialize a pinned
    // project snapshot into a request-owned checkout, but that ephemeral path
    // must never replace the source's durable ResumeContext. The state boundary
    // verifies exact equality before committing the continuation edge.
    prepared.resume_context = resume.clone();
    prepared.launch_metadata =
        std::mem::take(&mut prepared.launch_metadata).with_resume_context(resume.clone());
    prepared.authority.launch_metadata = Some(prepared.launch_metadata.clone());

    Ok(PreparedMachineSuccessorLaunch { prepared })
}

/// Launch a newly persisted operator successor with the exact authoritative
/// output computed before its row and ResumeContext were created.
pub async fn launch_prepared_operator_successor(
    state: AppState,
    successor_id: &str,
    prepared: PreparedOperatorSuccessorLaunch,
    launch_handoff: &LaunchHandoff,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    let result = launch_successor_inner(
        state,
        successor_id,
        SuccessorMode::Operator,
        Some(launch_handoff),
        Some(prepared.prepared),
    )
    .await;
    match &result {
        Err(BuildAndLaunchError::LaunchPreparation(error)) => {
            launch_handoff.publish_dispatch_failure(error.as_ref())
        }
        Err(error) => launch_handoff.publish_failure(
            "operator_successor_launch_failed",
            error.to_string(),
            500,
            error.retryable_launch_interruption(),
        ),
        // Another task owns the exact successor. Do not claim launch success
        // and do not manufacture an internal error: report truthful transient
        // contention so the caller may retry after the owner crosses handoff.
        Ok(SuccessorLaunchOutcome::Skipped("already_claimed")) if launch_handoff.is_pending() => {
            launch_handoff.publish_failure(
                "operator_successor_launch_in_progress",
                format!("operator successor {successor_id} launch is already in progress"),
                409,
                true,
            );
        }
        Ok(SuccessorLaunchOutcome::Skipped(reason)) if launch_handoff.is_pending() => {
            launch_handoff.publish_failure(
                "operator_successor_not_handed_off",
                format!("operator successor launch skipped: {reason}"),
                409,
                true,
            );
        }
        Ok(_) => {}
    }
    result
}

/// Launch a newly persisted machine successor with the exact authoritative
/// output computed before its row and ResumeContext became observable.
pub async fn launch_prepared_machine_successor(
    state: AppState,
    successor_id: &str,
    prepared: PreparedMachineSuccessorLaunch,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    launch_successor_inner(
        state,
        successor_id,
        SuccessorMode::Machine,
        None,
        Some(prepared.prepared),
    )
    .await
}

/// Persist ownership of a stranded MACHINE successor and enqueue its terminal
/// launch work. Unlike [`launch_successor`], this recovery boundary returns as
/// soon as the owned claim has been transferred into the detached task.
pub fn prepare_and_spawn_successor_recovery(
    state: AppState,
    successor_id: &str,
) -> Result<RecoveryLaunchOutcome, BuildAndLaunchError> {
    prepare_and_spawn_successor_recovery_inner(state, successor_id, SuccessorMode::Machine)
}

/// Operator-continuation counterpart of
/// [`prepare_and_spawn_successor_recovery`]. The detached run still injects the
/// persisted operator stimulus and retains the live API's terminal semantics.
pub fn prepare_and_spawn_operator_successor_recovery(
    state: AppState,
    successor_id: &str,
) -> Result<RecoveryLaunchOutcome, BuildAndLaunchError> {
    prepare_and_spawn_successor_recovery_inner(state, successor_id, SuccessorMode::Operator)
}

fn prepare_and_spawn_successor_recovery_inner(
    state: AppState,
    successor_id: &str,
    mode: SuccessorMode,
) -> Result<RecoveryLaunchOutcome, BuildAndLaunchError> {
    let claim = match ThreadLaunchClaim::acquire(&state, successor_id)? {
        ThreadLaunchClaimOutcome::Claimed(claim) => *claim,
        ThreadLaunchClaimOutcome::AlreadyClaimed => {
            return Ok(RecoveryLaunchOutcome::Skipped("already_claimed"));
        }
    };
    let successor_id = successor_id.to_string();
    tokio::spawn(async move {
        if !ryeos_app::recovery_execution_gate::wait_if_armed().await {
            return;
        }
        match launch_successor_inner_with_claim(state, &successor_id, mode, None, None, Some(claim))
            .await
        {
            Ok(SuccessorLaunchOutcome::Launched(_)) => {}
            Ok(SuccessorLaunchOutcome::Skipped(reason)) => tracing::debug!(
                thread_id = %successor_id,
                reason,
                "prepared successor recovery skipped"
            ),
            Err(error) => tracing::error!(
                thread_id = %successor_id,
                error = %error,
                "prepared successor recovery failed"
            ),
        }
    });
    Ok(RecoveryLaunchOutcome::Enqueued)
}

async fn launch_successor_inner(
    state: AppState,
    successor_id: &str,
    mode: SuccessorMode,
    launch_handoff: Option<&LaunchHandoff>,
    prepared_successor: Option<PreparedSuccessorLaunch>,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    launch_successor_inner_with_claim(
        state,
        successor_id,
        mode,
        launch_handoff,
        prepared_successor,
        None,
    )
    .await
}

async fn launch_successor_inner_with_claim(
    state: AppState,
    successor_id: &str,
    mode: SuccessorMode,
    launch_handoff: Option<&LaunchHandoff>,
    prepared_successor: Option<PreparedSuccessorLaunch>,
    prepared_claim: Option<ThreadLaunchClaim>,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    // Claim the launch FIRST — the sole authorization to spawn, and the
    // serialization point for the status + budget guards below.
    let claim = match prepared_claim {
        Some(claim) => claim,
        None => match ThreadLaunchClaim::acquire(&state, successor_id)? {
            ThreadLaunchClaimOutcome::Claimed(claim) => *claim,
            // Another launcher (live dispatch or a concurrent reconcile) owns the
            // window. Benign no-op — must NOT burn the attempt budget or finalize.
            ThreadLaunchClaimOutcome::AlreadyClaimed => {
                return Ok(SuccessorLaunchOutcome::Skipped("already_claimed"));
            }
        },
    };
    let launch_owner = claim
        .canonical_owner()
        .map_err(BuildAndLaunchError::Internal)?;

    // Status guard under the claim: ONLY a `created` row is launchable. A
    // successor already `running`/terminal (a duplicate trigger, or a stale-lease
    // reclaim of a still-live launch) must never be relaunched — release the
    // claim and skip WITHOUT finalizing (the row is fine, just not ours to run).
    let successor = match state.threads.get_thread(successor_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "launch_successor: thread not found: {successor_id}"
            )));
        }
        Err(e) => return Err(e.into()),
    };
    if successor.status != ryeos_state::objects::ThreadStatus::Created.as_str() {
        return Ok(SuccessorLaunchOutcome::Skipped("not_created"));
    }
    if let Some(reason) = attached_identity_launch_blocker(&state, &successor)? {
        return Ok(SuccessorLaunchOutcome::Skipped(reason));
    }

    // Refusal guard (defense-in-depth): a follow-resume successor is driven ONLY by
    // the follow-resume path, which first copies the parent's checkpoint in and
    // splices the child's result. A machine/operator relaunch of it here would run
    // it WITHOUT that result — corrupting the resume. Refuse. Fail closed if the
    // marker read errors: never machine-launch a possibly-follow successor.
    if let Some(source) = successor.upstream_thread_id.as_deref() {
        match state
            .state_store
            .is_follow_resume_successor(source, successor_id)
        {
            Ok(true) => {
                return Ok(SuccessorLaunchOutcome::Skipped("follow_resume_successor"));
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    successor_id,
                    error = %e,
                    "follow-resume marker read failed; refusing successor launch"
                );
                return Ok(SuccessorLaunchOutcome::Skipped("follow_marker_error"));
            }
        }
    }

    // Chain root captured BEFORE `successor` moves into launch_claimed_successor: a
    // continuation successor can itself sit in a followed child chain, so a failed
    // launch (budget-exhausted or pre-run defect) that finalizes it must wake the
    // followed parent — same liveness class as the follow-child / native-resume
    // paths. `finalize_failed_and_kick_follow` is a no-op kick for non-follow chains.
    let successor_chain_root_id = successor.chain_root_id.clone();

    // Attempt budget — MACHINE path only. Enforced HERE, after a successful claim
    // and the `created` check, so a lost claim (`AlreadyClaimed`) or a
    // non-launchable row never burns it. Bounds the TOTAL auto-launch attempts per
    // successor (live + reconcile combined); on exhaustion the successor is
    // finalized rather than relaunched forever. This is a per-successor relaunch
    // cap, NOT a chain-depth cap (a separate, open concern, which is why auto
    // machine continuation stays opt-in). The OPERATOR path skips this: an
    // operator follow-up is an explicit user action, not an autonomous relaunch.
    if mode == SuccessorMode::Machine {
        let attempts = match state.state_store.get_resume_attempts(successor_id) {
            Ok(n) => n,
            Err(e) => return Err(e.into()),
        };
        let max = ryeos_app::thread_lifecycle::MAX_CONTINUATION_AUTO_ATTEMPTS;
        if attempts >= max {
            if let Err(error) = finalize_failed_and_kick_follow(
                &state,
                successor_id,
                &successor_chain_root_id,
                &launch_owner,
                json!({
                    "error": format!("continuation auto-launch budget exhausted ({attempts}/{max})")
                }),
            ) {
                return Err(BuildAndLaunchError::Internal(error.context(
                    "finalize continuation after auto-launch budget exhaustion",
                )));
            }
            return Ok(SuccessorLaunchOutcome::Skipped("budget_exhausted"));
        }
        if let Err(e) = state.state_store.bump_resume_attempts(successor_id) {
            return Err(e.into());
        }
    }

    // Rebuild + run while the owned claim guard remains in this future. It is
    // released on every return, including cancellation and panic unwind.
    let result =
        launch_claimed_successor(&state, successor, mode, launch_handoff, prepared_successor).await;

    match result {
        Ok(native) => Ok(SuccessorLaunchOutcome::Launched(native)),
        Err(e) => {
            // A pre-run launch DEFECT (absent ResumeContext, snapshot-pinned
            // source, capability drift, envelope rebuild) would otherwise leave
            // the successor stuck at `created`. `run_claimed_thread_row` already
            // finalizes in-run failures, and finalize-if-needed is idempotent, so
            // finalizing here covers the pre-run case too without double-finalizing.
            // Kick too: this successor may sit in a followed child chain.
            if let Err(cleanup_error) = finalize_failed_and_kick_follow(
                &state,
                successor_id,
                &successor_chain_root_id,
                &launch_owner,
                json!({ "error": e.to_string() }),
            ) {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "successor launch failed: {e}; terminal cleanup also failed: {cleanup_error}"
                )));
            }
            Err(e)
        }
    }
}

/// Inner half of the successor launch, run once the claim is held and the
/// successor is confirmed `created`: rebuild the execution from the seeded
/// `ResumeContext` and run the existing row. `mode` selects stimulus + caps
/// policy (machine folds with no stimulus + caps pin; operator injects input +
/// fresh caps).
async fn launch_claimed_successor(
    state: &AppState,
    successor: ryeos_app::state_store::ThreadDetail,
    mode: SuccessorMode,
    launch_handoff: Option<&LaunchHandoff>,
    prepared_successor: Option<PreparedSuccessorLaunch>,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    let successor_id = successor.thread_id.clone();
    // A continuation successor must link upstream (chain-fold) and carry the
    // predecessor's captured launch identity (the create path guarantees both;
    // absence is a hard defect, not a silent skip).
    let previous_thread_id = successor.upstream_thread_id.clone().ok_or_else(|| {
        anyhow::anyhow!("launch_successor: {successor_id} has no upstream_thread_id")
    })?;
    let resume = state
        .state_store
        .get_launch_metadata(&successor_id)?
        .and_then(|m| m.resume_context)
        .ok_or_else(|| {
            anyhow::anyhow!("launch_successor: {successor_id} has no captured ResumeContext")
        })?;

    // Rebuild ExecutionParams from the captured identity (re-resolves the item as
    // its own kind, restores principal / hints / sites verbatim). Provenance
    // selection happens inside — a pushed-head record rebuilds the pinned
    // checkout + overlay engine, a snapshot-scoped record without a pushed-head
    // ref fails loudly before any resolution runs.
    let (params, prepared_authority) = match prepared_successor {
        Some(prepared) => {
            if prepared.thread_id != successor_id {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "precomputed authority names successor {}, not persisted successor {successor_id}",
                    prepared.thread_id
                )));
            }
            if mode != prepared.mode {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "precomputed successor authority supplied to the wrong launch mode"
                )));
            }
            if prepared
                .source_thread_id
                .as_deref()
                .is_some_and(|source| source != previous_thread_id.as_str())
            {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "precomputed authority does not match persisted successor source"
                )));
            }
            if !prepared.resume_context.eq(&resume) {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "precomputed authority does not match persisted successor identity"
                )));
            }
            (
                prepared.execution,
                Some((prepared.authority, prepared.launch_audit)),
            )
        }
        None => (
            crate::execution::runner::execution_params_from_resume_context(state, &resume)?,
            None,
        ),
    };
    // The managed run path takes the working dir separately from the provenance;
    // derive it FROM the provenance so a pushed-head successor runs in its
    // re-materialised checkout, never the (ephemeral) spawn-time path.
    let project_path = params.provenance.effective_path().to_path_buf();

    // Machine: fold the chain with NO new stimulus, and pin authority to the
    // predecessor's captured caps. Operator: inject the seeded input as the
    // opening stimulus, and re-derive caps fresh (an explicit launch, not a
    // relaunch of the same authority).
    let (suppress_stimulus, capability_policy) = match mode {
        // Machine and Follow both fold the chain (no stimulus) and pin authority to
        // the predecessor's captured caps; they differ only in checkpoint sourcing
        // (below). Operator injects the seeded input + re-derives caps fresh.
        SuccessorMode::Machine | SuccessorMode::Follow => (
            true,
            CapabilityPolicy::ExactPinned(resume.effective_caps.as_slice()),
        ),
        SuccessorMode::Operator => (false, CapabilityPolicy::Fresh),
    };

    let launch_params = BuildAndLaunchParams {
        state,
        // Propagate the predecessor's runtime identity so this successor
        // re-seeds the same runtime for the NEXT continuation turn.
        runtime_ref: resume.runtime_ref.as_deref(),
        acting_principal: &params.acting_principal,
        resolved: &params.resolved,
        project_path: &project_path,
        provenance: &params.provenance,
        parameters: &params.parameters,
        metadata_required_secrets: &params.resolved.resolved_item.metadata.required_secrets,
        pre_minted_thread_id: None,
        previous_thread_id: Some(&previous_thread_id),
        parent_execution_context: None,
        suppress_stimulus,
        capability_policy,
        checkpoint_resume_mode: match mode {
            SuccessorMode::Machine => CheckpointResumeMode::MachineContinuation,
            SuccessorMode::Operator => CheckpointResumeMode::None,
            // The follow-resume launcher already copied the predecessor's
            // checkpoint into this successor's dir and spliced the child
            // result, so resume from its OWN dir — do NOT re-copy.
            SuccessorMode::Follow => CheckpointResumeMode::SameThread,
        },
        launch_handoff,
    };
    match prepared_authority {
        Some((authority, launch_audit)) => {
            run_claimed_thread_row_with_authority(launch_params, successor, authority, launch_audit)
                .await
        }
        None => run_claimed_thread_row(launch_params, successor).await,
    }
}

/// Inner half of a SAME-THREAD native-resume crash recovery, run once the claim
/// is held: rebuild the execution from this thread's own seeded `ResumeContext`
/// and re-run the existing row through the managed runtime path (which builds the
/// `LaunchEnvelope` the runtime needs — `spawn_item` cannot). Mirrors
/// `launch_claimed_successor`, but it is the SAME thread (no upstream/braid), so
/// `previous_thread_id` is `None`, there is no copy-forward, and `RYEOS_RESUME=1`
/// makes the runtime load its OWN checkpoint.
async fn launch_claimed_native_resume(
    state: &AppState,
    thread: ryeos_app::state_store::ThreadDetail,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    let thread_id = thread.thread_id.clone();
    let resume = state
        .state_store
        .get_launch_metadata(&thread_id)?
        .and_then(|m| m.resume_context)
        .ok_or_else(|| {
            anyhow::anyhow!("native resume: {thread_id} has no captured ResumeContext")
        })?;

    // Provenance selection (pushed-head rebuild / live-fs / loud refusal)
    // happens inside; working dir + runtime registry then follow the
    // provenance so the resumed run resolves against the pinned overlay
    // engine when the original spawn was pushed-head.
    let params = crate::execution::runner::execution_params_from_resume_context(state, &resume)?;
    let project_path = params.provenance.effective_path().to_path_buf();

    run_claimed_thread_row(
        BuildAndLaunchParams {
            state,
            runtime_ref: resume.runtime_ref.as_deref(),
            acting_principal: &params.acting_principal,
            resolved: &params.resolved,
            project_path: &project_path,
            provenance: &params.provenance,
            parameters: &params.parameters,
            metadata_required_secrets: &params.resolved.resolved_item.metadata.required_secrets,
            pre_minted_thread_id: None,
            // SAME thread, not a successor — no chain braid.
            previous_thread_id: None,
            parent_execution_context: None,
            // Crash resume folds no new stimulus; it reloads its own checkpoint.
            suppress_stimulus: true,
            // Pin the captured authority verbatim (same as a machine relaunch).
            capability_policy: CapabilityPolicy::ExactPinned(resume.effective_caps.as_slice()),
            checkpoint_resume_mode: CheckpointResumeMode::SameThread,
            launch_handoff: None,
        },
        thread,
    )
    .await
}

fn attached_identity_launch_blocker(
    state: &AppState,
    thread: &ryeos_app::state_store::ThreadDetail,
) -> anyhow::Result<Option<&'static str>> {
    if thread.runtime.stop_intent.is_some() {
        return Ok(Some("stop_requested"));
    }
    let Some(identity) = thread.runtime.process_identity.as_ref() else {
        return Ok(None);
    };
    use ryeos_app::process::IdentityLiveness;
    match ryeos_app::process::execution_group_liveness(identity) {
        IdentityLiveness::Alive => return Ok(Some("live_process")),
        IdentityLiveness::Unavailable => return Ok(Some("process_liveness_unavailable")),
        IdentityLiveness::DeadOrStale => {}
    }
    match ryeos_app::process::execution_liveness(identity) {
        IdentityLiveness::Alive => return Ok(Some("group_identity_lost")),
        IdentityLiveness::Unavailable => return Ok(Some("process_liveness_unavailable")),
        IdentityLiveness::DeadOrStale => {}
    }
    // A vanished same-boot group leader does not prove that every descendant
    // left the process group. Only startup's exact live-group teardown, which
    // compare-clears before collecting a launch intent, or a boot boundary may
    // remove the attachment. Generic launch paths must never bypass quarantine.
    match ryeos_app::process::execution_identity_is_current_boot(identity) {
        Ok(true) => return Ok(Some("same_boot_process_identity_quarantined")),
        Ok(false) => {}
        Err(_) => return Ok(Some("process_identity_boot_unavailable")),
    }
    if state
        .state_store
        .clear_thread_process_if_matches(&thread.thread_id, identity)?
    {
        Ok(None)
    } else {
        Ok(Some("process_identity_changed"))
    }
}

/// Claim-guarded entry for a SAME-THREAD native-resume crash recovery (the
/// reconciler's `NativeResume` for a runtime-registry kind, e.g. graph). Claims
/// the launch lease (so only one launcher acts), skips a thread that already
/// reached a terminal status, then rebuilds + re-runs through the managed path.
/// The resume-attempt budget is enforced upstream by `reconcile::decide_resume`.
pub async fn launch_existing_native_resume(
    state: AppState,
    thread_id: &str,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    launch_existing_native_resume_with_claim(state, thread_id, None).await
}

/// Persist ownership of a managed same-thread resume and enqueue its terminal
/// runtime work. The returned `Enqueued` boundary is safe for startup readiness:
/// the detached future owns the SQLite claim before this function returns.
pub fn prepare_and_spawn_existing_native_resume_recovery(
    state: AppState,
    thread_id: &str,
) -> Result<RecoveryLaunchOutcome, BuildAndLaunchError> {
    let claim = match ThreadLaunchClaim::acquire(&state, thread_id)? {
        ThreadLaunchClaimOutcome::Claimed(claim) => *claim,
        ThreadLaunchClaimOutcome::AlreadyClaimed => {
            return Ok(RecoveryLaunchOutcome::Skipped("already_claimed"));
        }
    };
    let thread_id = thread_id.to_string();
    tokio::spawn(async move {
        if !ryeos_app::recovery_execution_gate::wait_if_armed().await {
            return;
        }
        match launch_existing_native_resume_with_claim(state, &thread_id, Some(claim)).await {
            Ok(SuccessorLaunchOutcome::Launched(_)) => {}
            Ok(SuccessorLaunchOutcome::Skipped(reason)) => tracing::debug!(
                thread_id = %thread_id,
                reason,
                "prepared managed native resume skipped"
            ),
            Err(error) => tracing::error!(
                thread_id = %thread_id,
                error = %error,
                "prepared managed native resume failed"
            ),
        }
    });
    Ok(RecoveryLaunchOutcome::Enqueued)
}

async fn launch_existing_native_resume_with_claim(
    state: AppState,
    thread_id: &str,
    prepared_claim: Option<ThreadLaunchClaim>,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    let claim = match prepared_claim {
        Some(claim) => claim,
        None => match ThreadLaunchClaim::acquire(&state, thread_id)? {
            ThreadLaunchClaimOutcome::Claimed(claim) => *claim,
            ThreadLaunchClaimOutcome::AlreadyClaimed => {
                return Ok(SuccessorLaunchOutcome::Skipped("already_claimed"));
            }
        },
    };
    let launch_owner = claim
        .canonical_owner()
        .map_err(BuildAndLaunchError::Internal)?;

    let thread = match state.threads.get_thread(thread_id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "native resume: thread not found: {thread_id}"
            )));
        }
        Err(e) => return Err(e.into()),
    };

    // A terminal thread is already done (a duplicate trigger or a stale-lease
    // reclaim of a settled row) — release and skip without finalizing. A
    // non-terminal (crashed `running`/`created`) row is the resume target.
    if ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
        .is_some_and(|s| s.is_terminal())
    {
        return Ok(SuccessorLaunchOutcome::Skipped("terminal"));
    }

    // Any attached identity blocks or is exact-cleared before relaunch,
    // regardless of lifecycle status (`created` can already be attached).
    if let Some(reason) = attached_identity_launch_blocker(&state, &thread)? {
        return Ok(SuccessorLaunchOutcome::Skipped(reason));
    }

    // Capture the chain root BEFORE `thread` moves into the launcher: a native-
    // resume target can itself be a follow child, and a failed relaunch finalizes it
    // (flipping the awaiting waiter to `ready`) — so the parent must be kicked here
    // too, not left for the next restart.
    let child_chain_root_id = thread.chain_root_id.clone();
    let result = launch_claimed_native_resume(&state, thread).await;

    match result {
        Ok(native) => Ok(SuccessorLaunchOutcome::Launched(native)),
        Err(e) => {
            if let Err(cleanup_error) = finalize_failed_and_kick_follow(
                &state,
                thread_id,
                &child_chain_root_id,
                &launch_owner,
                json!({ "error": e.to_string() }),
            ) {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "native resume launch failed: {e}; terminal cleanup also failed: \
                     {cleanup_error}"
                )));
            }
            Err(e)
        }
    }
}

/// Inner half of a follow-child launch (claim held): rebuild the execution from
/// the child's seeded launch identity and run the FRESH child row through the
/// managed runtime path (which builds the `LaunchEnvelope` the runtime needs —
/// `spawn_item` cannot). Mirrors `launch_claimed_native_resume`, but the child is
/// a FRESH root launch, not a resume: it injects its opening stimulus
/// (`suppress_stimulus = false`) and is not a checkpoint resume. It is its own
/// chain root, so `previous_thread_id` is `None`. For an unlaunched follow-child
/// row ONLY, the seeded `ResumeContext.effective_caps` carries the PARENT's
/// effective caps (the bounding authority for `FollowChildHybrid`), not the
/// child's own — `run_claimed_thread_row` overwrites launch metadata with the
/// child's actual composed caps once policy resolution succeeds.
async fn launch_claimed_follow_child(
    state: &AppState,
    thread: ryeos_app::state_store::ThreadDetail,
    provenance_override: Option<ryeos_app::execution_provenance::ExecutionProvenance>,
    parent_context: Option<crate::dispatch::ParentExecutionContext>,
    launch_handoff: Option<&LaunchHandoff>,
    prepared_child: Option<PreparedFollowChildLaunch>,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    let thread_id = thread.thread_id.clone();
    // A follow child is a FRESH ROOT: no upstream braid, its own chain root.
    // Reject a continuation-shaped row (a sign the caller created it wrong).
    if thread.upstream_thread_id.is_some() {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow child {thread_id} must be a fresh root but has upstream {:?}",
            thread.upstream_thread_id
        )));
    }
    if thread.chain_root_id != thread.thread_id {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow child {thread_id} must be its own chain root (chain_root = {})",
            thread.chain_root_id
        )));
    }
    let metadata = state
        .state_store
        .get_launch_metadata(&thread_id)?
        .ok_or_else(|| {
            anyhow::anyhow!("follow child: {thread_id} has no seeded launch identity")
        })?;
    let persisted_parent_context =
        metadata
            .follow_parent_context
            .map(|p| crate::dispatch::ParentExecutionContext {
                parent_thread_id: p.parent_thread_id,
                hard_limits: p.hard_limits,
                depth: p.depth,
            });
    let sealed_root_request = metadata.sealed_root_request.ok_or_else(|| {
        anyhow::anyhow!("follow child: {thread_id} has no sealed root execution request")
    })?;
    let identity = metadata.resume_context.ok_or_else(|| {
        anyhow::anyhow!("follow child: {thread_id} has no seeded launch identity")
    })?;
    let (params, parent_context, prepared_authority, launch_audit) = match prepared_child {
        Some(prepared) => {
            if prepared.thread_id != thread_id {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "precomputed child authority names {}, not persisted child {thread_id}",
                    prepared.thread_id
                )));
            }
            if prepared.resume_context != identity {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "precomputed child authority does not match persisted launch identity"
                )));
            }
            let launch_audit = prepared.launch_audit;
            (
                prepared.execution,
                prepared.parent_context,
                Some(prepared.authority),
                launch_audit,
            )
        }
        None => {
            let parent_context = parent_context.or(persisted_parent_context).ok_or_else(|| {
                anyhow::anyhow!("follow child: {thread_id} has no persisted parent context")
            })?;
            // Recovery reconstructs the exact admitted root identity from
            // the sealed request. A live caller may still override only the
            // borrowed workspace provenance.
            let params = crate::execution::runner::execution_params_from_sealed_root_request(
                state,
                &identity,
                &sealed_root_request,
                provenance_override,
            )?;
            (
                params,
                parent_context,
                None,
                LaunchAuditDisposition::AppendForAttempt,
            )
        }
    };
    // Working dir + runtime registry follow the FINAL provenance (post-
    // override), so the hot path runs in the parent's workspace with the
    // parent's request engine.
    let project_path = params.provenance.effective_path().to_path_buf();

    // For an unlaunched follow-child row the seeded `effective_caps` is the
    // PARENT's authority (see the fn header) — name it as such at the use site so
    // the overload is explicit and F5 seeds parent caps, never child-bounded ones.
    let parent_effective_caps = identity.effective_caps.as_slice();

    let launch_params = BuildAndLaunchParams {
        state,
        runtime_ref: identity.runtime_ref.as_deref(),
        acting_principal: &params.acting_principal,
        resolved: &params.resolved,
        project_path: &project_path,
        provenance: &params.provenance,
        parameters: &params.parameters,
        metadata_required_secrets: &params.resolved.resolved_item.metadata.required_secrets,
        pre_minted_thread_id: None,
        // A follow child is its OWN root chain, never a continuation braid.
        previous_thread_id: None,
        // A fresh launch injects its opening stimulus.
        suppress_stimulus: false,
        // Source-aware bounding against the parent: child-declared grants are
        // bounded against the parent's effective caps; the child keeps its own
        // manifest runtime authority.
        capability_policy: CapabilityPolicy::FollowChildHybrid {
            parent_effective_caps,
        },
        // Fresh launch, not a checkpoint resume.
        checkpoint_resume_mode: CheckpointResumeMode::None,
        // Clamp the child to the parent's hard limits + launch at parent depth
        // + 1 on the hot path; reconcile reconstructs the persisted parent
        // execution context below rather than silently granting root limits.
        parent_execution_context: Some(&parent_context),
        launch_handoff,
    };
    match prepared_authority {
        Some(authority) => {
            run_claimed_thread_row_with_authority(launch_params, thread, authority, launch_audit)
                .await
        }
        None => run_claimed_thread_row(launch_params, thread).await,
    }
}

/// Claim-guarded entry to launch a pre-created, pre-seeded follow CHILD row
/// through the managed runtime path. Reconcile uses this unacknowledged form;
/// live child creation uses [`launch_prepared_follow_child`] and waits for its
/// explicit spawn-task handoff while the runtime continues detached.
/// Idempotent + crash-safe like `launch_existing_native_resume`: claims the lease
/// (a dead launcher's claim is reclaimable), skips a terminal or live-process row,
/// and finalizes on a pre-run defect.
pub async fn launch_follow_child(
    state: AppState,
    child_id: &str,
    provenance_override: Option<ryeos_app::execution_provenance::ExecutionProvenance>,
    // Parent execution ceiling, built from the parent's live callback cap on the
    // hot launch so the child is clamped to the parent's hard limits and launched
    // at parent depth + 1 — the same context a normal callback-dispatched child
    // gets. `None` on a reconcile relaunch (like `provenance_override`): a crashed
    // follow child recovers through the general native-resume sweep as a root, the
    // documented reconcile limit shared with every native-resume child.
    parent_context: Option<crate::dispatch::ParentExecutionContext>,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    launch_follow_child_with_claim(
        state,
        child_id,
        provenance_override,
        parent_context,
        None,
        None,
        None,
    )
    .await
}

/// Launch a just-created child with the exact authority prepared before birth.
/// The handoff is published only after the runtime spawn task owns that
/// authority and its secret values.
pub async fn launch_prepared_follow_child(
    state: AppState,
    child_id: &str,
    prepared: PreparedFollowChildLaunch,
    launch_handoff: &LaunchHandoff,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    let result = launch_follow_child_with_claim(
        state,
        child_id,
        None,
        None,
        Some(launch_handoff),
        Some(prepared),
        None,
    )
    .await;
    match &result {
        Err(BuildAndLaunchError::LaunchPreparation(error)) => {
            launch_handoff.publish_dispatch_failure(error.as_ref())
        }
        Err(error) => {
            launch_handoff.publish_failure("child_launch_failed", error.to_string(), 500, false)
        }
        Ok(SuccessorLaunchOutcome::Skipped(reason)) if launch_handoff.is_pending() => {
            launch_handoff.publish_failure(
                "child_launch_not_handed_off",
                format!("child launch skipped: {reason}"),
                409,
                true,
            );
        }
        Ok(_) => {}
    }
    result
}

/// Persist ownership of a stranded follow child and enqueue the reconcile-parity
/// launch (captured provenance and parent execution context, no live overrides).
pub fn prepare_and_spawn_follow_child_recovery(
    state: AppState,
    child_id: &str,
) -> Result<RecoveryLaunchOutcome, BuildAndLaunchError> {
    prepare_and_spawn_follow_child(state, child_id, None, None)
}

/// Claim and detach a follow-child launch while preserving any live borrowed
/// provenance and parent ceiling. This is the durable handoff used by both the
/// callback hot path and the no-override recovery wrapper above.
pub fn prepare_and_spawn_follow_child(
    state: AppState,
    child_id: &str,
    provenance_override: Option<ryeos_app::execution_provenance::ExecutionProvenance>,
    parent_context: Option<crate::dispatch::ParentExecutionContext>,
) -> Result<RecoveryLaunchOutcome, BuildAndLaunchError> {
    let claim = match ThreadLaunchClaim::acquire(&state, child_id)? {
        ThreadLaunchClaimOutcome::Claimed(claim) => *claim,
        ThreadLaunchClaimOutcome::AlreadyClaimed => {
            return Ok(RecoveryLaunchOutcome::Skipped("already_claimed"));
        }
    };
    let child_id = child_id.to_string();
    tokio::spawn(async move {
        if !ryeos_app::recovery_execution_gate::wait_if_armed().await {
            return;
        }
        match launch_follow_child_with_claim(
            state,
            &child_id,
            provenance_override,
            parent_context,
            None,
            None,
            Some(claim),
        )
        .await
        {
            Ok(SuccessorLaunchOutcome::Launched(_)) => {}
            Ok(SuccessorLaunchOutcome::Skipped(reason)) => tracing::debug!(
                child_thread_id = %child_id,
                reason,
                "prepared follow-child recovery skipped"
            ),
            Err(error) => tracing::error!(
                child_thread_id = %child_id,
                error = %error,
                "prepared follow-child recovery failed"
            ),
        }
    });
    Ok(RecoveryLaunchOutcome::Enqueued)
}

async fn launch_follow_child_with_claim(
    state: AppState,
    child_id: &str,
    provenance_override: Option<ryeos_app::execution_provenance::ExecutionProvenance>,
    parent_context: Option<crate::dispatch::ParentExecutionContext>,
    launch_handoff: Option<&LaunchHandoff>,
    prepared_child: Option<PreparedFollowChildLaunch>,
    prepared_claim: Option<ThreadLaunchClaim>,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    let claim = match prepared_claim {
        Some(claim) => claim,
        None => match ThreadLaunchClaim::acquire(&state, child_id)? {
            ThreadLaunchClaimOutcome::Claimed(claim) => *claim,
            ThreadLaunchClaimOutcome::AlreadyClaimed => {
                return Ok(SuccessorLaunchOutcome::Skipped("already_claimed"));
            }
        },
    };
    let launch_owner = claim
        .canonical_owner()
        .map_err(BuildAndLaunchError::Internal)?;

    let thread = match state.threads.get_thread(child_id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "follow child: thread not found: {child_id}"
            )));
        }
        Err(e) => return Err(e.into()),
    };

    // Cancellation tombstones are checked after claiming, so admission and an
    // ancestor cancel cannot race into a spawn. Finalize this never-launched row
    // and wake its own follow chain immediately.
    if state
        .state_store
        .launch_window_is_cancelled(&thread.chain_root_id)?
    {
        let chain_root = thread.chain_root_id.clone();
        let cancelled = state.threads.finalize_thread_owned(
            &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                thread_id: thread.thread_id.clone(),
                status: "cancelled".into(),
                outcome_code: Some("cancelled".into()),
                result: None,
                error: Some(json!({"reason":"ancestor_cancelled_before_launch"})),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            },
            &launch_owner,
        );
        cancelled?;
        state.state_store.discard_window_member(&chain_root)?;
        kick_follow_resume_if_ready(&state, &chain_root);
        return Ok(SuccessorLaunchOutcome::Skipped("cancelled"));
    }

    // A terminal row is already done (a duplicate trigger or a stale-lease reclaim
    // of a settled child) — release and skip without finalizing.
    if ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
        .is_some_and(|s| s.is_terminal())
    {
        return Ok(SuccessorLaunchOutcome::Skipped("terminal"));
    }

    // Exact process identity supersedes the old pgid-only liveness check and
    // covers the created-but-already-attached launch window as well.
    if let Some(reason) = attached_identity_launch_blocker(&state, &thread)? {
        return Ok(SuccessorLaunchOutcome::Skipped(reason));
    }

    // This entry point owns only the never-launched created-root window. Once a
    // child has started, ordinary native-resume recovery owns it; replaying the
    // opening stimulus here would turn a crash resume into a second fresh run.
    if thread.status != ryeos_state::objects::ThreadStatus::Created.as_str() {
        return Ok(SuccessorLaunchOutcome::Skipped("already_started"));
    }

    let result = launch_claimed_follow_child(
        &state,
        thread,
        provenance_override,
        parent_context,
        launch_handoff,
        prepared_child,
    )
    .await;

    match result {
        Ok(native) => Ok(SuccessorLaunchOutcome::Launched(native)),
        Err(e) => {
            // A pre-run failure flips the waiter to `ready` (degraded failure);
            // finalize + kick so the parent resumes live. The child is its own chain
            // root, so its id is the chain root the waiter keys on.
            if let Err(cleanup_error) = finalize_failed_and_kick_follow(
                &state,
                child_id,
                child_id,
                &launch_owner,
                json!({ "error": e.to_string() }),
            ) {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "follow-child launch failed: {e}; terminal cleanup also failed: \
                     {cleanup_error}"
                )));
            }
            Err(e)
        }
    }
}

/// Finalize a thread as failed on a pre-run / relaunch defect, then wake any follow
/// parent waiting on its chain. A no-op kick for non-follow threads. Used by EVERY
/// launch error path that can finalize a follow child (fresh follow-child launch,
/// native-resume relaunch) so a child that dies during (re)launch never leaves its
/// parent suspended until the next restart. Pass `child_chain_root_id` captured
/// BEFORE the `ThreadDetail` is moved into the launcher.
pub fn finalize_failed_and_kick_follow(
    state: &AppState,
    thread_id: &str,
    child_chain_root_id: &str,
    launch_owner: &str,
    error: Value,
) -> anyhow::Result<()> {
    let outcome = crate::dispatch::finalize_method_thread_if_needed(
        state,
        thread_id,
        launch_owner,
        "failed",
        Some(error),
    )?;
    if outcome != crate::dispatch::MethodFinalizeOutcome::PreservedForShutdown {
        kick_follow_resume_if_ready(state, child_chain_root_id);
        kick_launch_window_for_terminal(state, child_chain_root_id);
    }
    Ok(())
}

static GLOBAL_LIVE_FANOUT_LIMIT: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();

/// Arm the node-wide ceiling on launched-and-live window members across ALL
/// fanouts — the cross-project load valve. The daemon arms it once at boot
/// from the node-scoped execution config (`config/execution/execution.yaml`,
/// `node.max_live_fanout`); unarmed or 0 means no ceiling.
pub fn arm_global_live_fanout_limit(limit: Option<u32>) {
    let _ = GLOBAL_LIVE_FANOUT_LIMIT.set(limit.filter(|n| *n > 0));
}

pub(crate) fn global_live_fanout_limit() -> Option<u32> {
    GLOBAL_LIVE_FANOUT_LIMIT.get().copied().flatten()
}

/// Launch a window-admitted child on the reconcile-parity path. Preparation
/// persists the claim before detaching, so releasing/admitting a window member
/// never leaves only an unclaimed in-memory spawn request behind.
pub(crate) fn launch_admitted_window_member(state: &AppState, child_thread_id: &str) {
    match prepare_and_spawn_follow_child_recovery(state.clone(), child_thread_id) {
        Ok(RecoveryLaunchOutcome::Enqueued) => {}
        Ok(RecoveryLaunchOutcome::Skipped(reason)) => tracing::debug!(
            child_thread_id,
            reason,
            "window-admitted child launch skipped"
        ),
        Err(error) => tracing::error!(
            child_thread_id,
            error = %error,
            "window-admitted child launch preparation failed"
        ),
    }
}

/// Whether a chain has settled for good: walk `continued` links to the tip
/// and report a HARD terminal there. `continued` itself never counts — the
/// chain lives on in its successor — and a `continued` tip with no recorded
/// successor is a handoff in flight, not an end.
fn chain_tip_hard_terminal(state: &AppState, chain_root_id: &str) -> anyhow::Result<bool> {
    use ryeos_state::objects::ThreadStatus;
    let mut cursor = chain_root_id.to_string();
    for _ in 0..1024 {
        let Some(t) = state.state_store.get_thread(&cursor)? else {
            return Ok(false);
        };
        if t.status == ThreadStatus::Continued.as_str() {
            match t.successor_thread_id {
                Some(next) => {
                    cursor = next;
                    continue;
                }
                None => return Ok(false),
            }
        }
        return Ok(ThreadStatus::from_str_lossy(&t.status).is_some_and(|s| s.is_terminal()));
    }
    Ok(false)
}

/// Release a launch-window slot when a member CHAIN reaches a hard terminal
/// and launch the queued members admitted in its place. `thread_continued`
/// keeps the slot. Called from every live finalize seam (alongside
/// `kick_follow_resume_if_ready`); a chain holding no window row is the
/// common case and returns immediately.
pub fn kick_launch_window_for_terminal(state: &AppState, chain_root_id: &str) {
    match state.state_store.launch_window_is_member(chain_root_id) {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::warn!(chain_root_id, error = %e, "launch-window membership check failed");
            return;
        }
    }
    match chain_tip_hard_terminal(state, chain_root_id) {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::warn!(chain_root_id, error = %e, "launch-window terminal check failed");
            return;
        }
    }
    match state.state_store.launch_window_release(
        chain_root_id,
        global_live_fanout_limit(),
        lillux::time::timestamp_millis(),
    ) {
        Ok(admitted) => {
            for id in admitted {
                tracing::info!(
                    child_thread_id = %id,
                    freed_by = %chain_root_id,
                    "launch-window slot freed — launching queued member",
                );
                launch_admitted_window_member(state, &id);
            }
        }
        Err(e) => {
            tracing::warn!(chain_root_id, error = %e, "launch-window release failed");
        }
    }
}

/// Startup/maintenance sweep for launch windows: release members whose
/// chain settled without a kick landing (the crash window), then admit and
/// launch queued members up to each window's width and the global ceiling.
/// Idempotent — every launch is claim-guarded, so a double-drive is a
/// benign skip. Run post-listener (launched runtimes call back immediately).
pub fn sweep_launch_windows(state: &AppState) {
    let now_ms = lillux::time::timestamp_millis();
    match state.state_store.launch_window_launched_members() {
        Ok(members) => {
            for chain in members {
                match chain_tip_hard_terminal(state, &chain) {
                    Ok(true) => match state.state_store.launch_window_release(
                        &chain,
                        global_live_fanout_limit(),
                        now_ms,
                    ) {
                        Ok(admitted) => {
                            for id in admitted {
                                launch_admitted_window_member(state, &id);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(chain_root_id = %chain, error = %e, "launch-window sweep release failed")
                        }
                    },
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(chain_root_id = %chain, error = %e, "launch-window sweep terminal check failed")
                    }
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "launch-window sweep member listing failed"),
    }
    match state.state_store.launch_window_keys_with_queue() {
        Ok(keys) => {
            for key in keys {
                match state.state_store.launch_window_admit(
                    &key,
                    global_live_fanout_limit(),
                    now_ms,
                ) {
                    Ok(admitted) => {
                        for id in admitted {
                            tracing::info!(
                                child_thread_id = %id,
                                window_key = %key,
                                "launch-window sweep admission — launching queued member",
                            );
                            launch_admitted_window_member(state, &id);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(window_key = %key, error = %e, "launch-window sweep admission failed")
                    }
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "launch-window sweep queue listing failed"),
    }
}

/// Strict startup counterpart to [`sweep_launch_windows`].
///
/// The periodic live sweep is deliberately best-effort, but startup must not
/// publish Ready until every launch-window mutation and admitted launch has
/// either acquired durable ownership or returned a benign classification.
/// Consequently this variant propagates every listing, terminality, admission,
/// and claim error to the startup coordinator.
pub fn prepare_launch_window_recovery(
    state: &AppState,
) -> Result<Vec<(String, RecoveryLaunchOutcome)>> {
    let now_ms = lillux::time::timestamp_millis();
    let mut outcomes = Vec::new();

    for chain_root_id in state.state_store.launch_window_launched_members()? {
        if !chain_tip_hard_terminal(state, &chain_root_id)? {
            continue;
        }
        let admitted = state.state_store.launch_window_release(
            &chain_root_id,
            global_live_fanout_limit(),
            now_ms,
        )?;
        for child_thread_id in admitted {
            let outcome = prepare_and_spawn_follow_child_recovery(state.clone(), &child_thread_id)
                .with_context(|| {
                    format!(
                    "prepare launch-window child {child_thread_id} admitted after {chain_root_id}"
                )
                })?;
            outcomes.push((child_thread_id, outcome));
        }
    }

    for window_key in state.state_store.launch_window_keys_with_queue()? {
        let admitted = state.state_store.launch_window_admit(
            &window_key,
            global_live_fanout_limit(),
            now_ms,
        )?;
        for child_thread_id in admitted {
            let outcome = prepare_and_spawn_follow_child_recovery(state.clone(), &child_thread_id)
                .with_context(|| {
                    format!(
                        "prepare launch-window child {child_thread_id} admitted from {window_key}"
                    )
                })?;
            outcomes.push((child_thread_id, outcome));
        }
    }

    Ok(outcomes)
}

/// If `child_chain_root_id`'s just-recorded terminal flipped a follow waiter to
/// `ready`, fire the parent-resume launch NOW (claim-guarded; a no-op otherwise).
/// Called from EVERY live finalize path a follow child can reach — the self-finalize
/// UDS handler, the executor-supervised fallback, the operator-cancel handler, and
/// the pre-run launch-failure arm — so a followed parent wakes live regardless of
/// how the child terminated, not only at the next startup `reconcile_follow`. Spawns
/// the launch detached so the finalize path (and its held locks) is never blocked on
/// the parent's whole resume. The waiter's `ready` state is the signal, so a
/// redundant call is a cheap claim-guarded no-op.
pub fn kick_follow_resume_if_ready(state: &AppState, child_chain_root_id: &str) {
    let waiter = match state
        .state_store
        .get_follow_waiter_by_child_chain(child_chain_root_id)
    {
        Ok(Some(w)) => w,
        // The common case: no parent awaits this chain.
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(
                child_chain_root_id,
                error = %e,
                "follow-resume kick: waiter lookup failed"
            );
            return;
        }
    };
    // Only a `ready` waiter has a stored result to resume with. `waiting` (an
    // intermediate `continued` link) or `resuming`/cleared → no kick here.
    if waiter.phase != ryeos_app::runtime_db::follow_phase::READY {
        return;
    }
    let follow_key = waiter.follow_key;
    match prepare_and_spawn_follow_resume_recovery(state.clone(), &follow_key) {
        Ok(RecoveryLaunchOutcome::Enqueued) => {}
        Ok(RecoveryLaunchOutcome::Skipped(reason)) => {
            tracing::debug!(follow_key = %follow_key, reason, "follow-resume kick skipped");
        }
        Err(error) => {
            tracing::error!(follow_key = %follow_key, error = %error, "follow-resume kick failed");
        }
    }
}

/// Validate that `successor` really is the graph-follow-resume successor of
/// `parent_thread_id`: it must link upstream to the parent AND carry the
/// graph-follow-resume continuation marker. Returns `None` when valid, or the
/// fail-closed skip reason otherwise. Shared by the claimed launch path AND the
/// `AlreadyClaimed` waiter cleanup, so neither ever splices/launches — nor clears a
/// waiter — for a stale/corrupt row that is not this parent's follow successor.
fn follow_resume_successor_refusal(
    state: &AppState,
    parent_thread_id: &str,
    successor: &ryeos_app::state_store::ThreadDetail,
) -> Option<&'static str> {
    if successor.upstream_thread_id.as_deref() != Some(parent_thread_id) {
        tracing::warn!(
            parent = %parent_thread_id,
            successor_id = %successor.thread_id,
            upstream = ?successor.upstream_thread_id,
            "follow-resume: successor does not link back to the waiter's parent — refusing"
        );
        return Some("successor_mismatch");
    }
    match state
        .state_store
        .is_follow_resume_successor(parent_thread_id, &successor.thread_id)
    {
        Ok(true) => None,
        Ok(false) => {
            tracing::warn!(
                parent = %parent_thread_id,
                successor_id = %successor.thread_id,
                "follow-resume: successor lacks the graph-follow-resume marker — refusing"
            );
            Some("not_follow_successor")
        }
        Err(e) => {
            tracing::warn!(
                parent = %parent_thread_id,
                successor_id = %successor.thread_id,
                error = %e,
                "follow-resume: marker read failed — refusing"
            );
            Some("follow_marker_error")
        }
    }
}

/// Launch a suspended parent's follow-resume successor once the followed child's
/// terminal envelope is stored on the waiter (`ready`, or `resuming` when re-driven
/// after a crash). Claim-guarded and crash-safe: copies the parent's checkpoint
/// into the successor's dir and splices the child's canonical envelope as
/// `follow_result`, then runs the successor folding the chain (Follow mode). Clears
/// the waiter once the successor is durably launched — its own checkpoint now
/// carries the result, so reconcile can native-resume it independently. Idempotent
/// by `follow_key`: a re-drive of an already-launched successor skips.
pub async fn launch_follow_resume_successor(
    state: AppState,
    follow_key: &str,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    launch_follow_resume_successor_with_claim(state, follow_key, None).await
}

/// Persist ownership of a ready follow-resume successor and enqueue the splice
/// and terminal launch. A waiter that is no longer ready is classified before
/// enqueue; `Enqueued` always transfers an owned SQLite claim into the task.
pub fn prepare_and_spawn_follow_resume_recovery(
    state: AppState,
    follow_key: &str,
) -> Result<RecoveryLaunchOutcome, BuildAndLaunchError> {
    use ryeos_app::runtime_db::follow_phase;

    let waiter = state
        .state_store
        .get_follow_waiter_by_key(follow_key)?
        .ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "follow-resume: waiter not found: {follow_key}"
            ))
        })?;
    if waiter.phase != follow_phase::READY && waiter.phase != follow_phase::RESUMING {
        return Ok(RecoveryLaunchOutcome::Skipped("not_ready"));
    }
    let successor_id = waiter.parent_successor_thread_id.clone().ok_or_else(|| {
        BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow-resume: waiter {follow_key} has no parent successor"
        ))
    })?;
    let claim = match ThreadLaunchClaim::acquire(&state, &successor_id)? {
        ThreadLaunchClaimOutcome::Claimed(claim) => *claim,
        ThreadLaunchClaimOutcome::AlreadyClaimed => {
            // Preserve the live launcher's waiter-cleanup semantics: if this is
            // provably the right successor and it already advanced, the owning
            // launcher has durably consumed the waiter even though we did not
            // win its claim.
            if let Some(successor) = state.threads.get_thread(&successor_id)? {
                if follow_resume_successor_refusal(&state, &waiter.parent_thread_id, &successor)
                    .is_none()
                    && successor.status != ryeos_state::objects::ThreadStatus::Created.as_str()
                {
                    let _ = state.state_store.clear_follow_waiter(follow_key);
                }
            }
            return Ok(RecoveryLaunchOutcome::Skipped("already_claimed"));
        }
    };
    let follow_key = follow_key.to_string();
    tokio::spawn(async move {
        if !ryeos_app::recovery_execution_gate::wait_if_armed().await {
            return;
        }
        match launch_follow_resume_successor_with_claim(state, &follow_key, Some(claim)).await {
            Ok(SuccessorLaunchOutcome::Launched(_)) => {}
            Ok(SuccessorLaunchOutcome::Skipped(reason)) => tracing::debug!(
                follow_key = %follow_key,
                reason,
                "prepared follow-resume recovery skipped"
            ),
            Err(error) => tracing::error!(
                follow_key = %follow_key,
                error = %error,
                "prepared follow-resume recovery failed"
            ),
        }
    });
    Ok(RecoveryLaunchOutcome::Enqueued)
}

async fn launch_follow_resume_successor_with_claim(
    state: AppState,
    follow_key: &str,
    prepared_claim: Option<ThreadLaunchClaim>,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    use ryeos_app::runtime_db::follow_phase;

    let waiter = state
        .state_store
        .get_follow_waiter_by_key(follow_key)?
        .ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "follow-resume: waiter not found: {follow_key}"
            ))
        })?;

    // Only a waiter whose child has reached terminal (`ready`) — or one already
    // mid-resume (`resuming`, re-driven after a crash) — has a result to resume.
    if waiter.phase != follow_phase::READY && waiter.phase != follow_phase::RESUMING {
        return Ok(SuccessorLaunchOutcome::Skipped("not_ready"));
    }
    let successor_id = waiter.parent_successor_thread_id.clone().ok_or_else(|| {
        BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow-resume: waiter {follow_key} has no parent successor"
        ))
    })?;

    // Claim the successor launch — the serialization point (concurrent reconcile +
    // live drives) and the sole authorization to run it.
    let claim = match prepared_claim {
        Some(claim) => claim,
        None => match ThreadLaunchClaim::acquire(&state, &successor_id)? {
            ThreadLaunchClaimOutcome::Claimed(claim) => *claim,
            ThreadLaunchClaimOutcome::AlreadyClaimed => {
                // Another launcher holds the claim. Retire the waiter ONLY if the
                // successor is a VALID follow-resume successor of THIS parent (upstream +
                // marker) that has already advanced past `created` (the resume ran) — so
                // it does not sit `resuming` until a future restart. Fail closed: a
                // stale/corrupt waiter pointing at an unrelated claimed row is never
                // cleared blindly. Still `created` → a concurrent follow launcher is
                // mid-splice/launch and owns the clear.
                match state.threads.get_thread(&successor_id) {
                    Ok(Some(s)) => {
                        if follow_resume_successor_refusal(&state, &waiter.parent_thread_id, &s)
                            .is_none()
                            && s.status != ryeos_state::objects::ThreadStatus::Created.as_str()
                        {
                            let _ = state.state_store.clear_follow_waiter(follow_key);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        follow_key,
                        successor_id,
                        error = %e,
                        "follow-resume: claim held; failed to inspect successor for waiter cleanup"
                    ),
                }
                return Ok(SuccessorLaunchOutcome::Skipped("already_claimed"));
            }
        },
    };
    let launch_owner = claim
        .canonical_owner()
        .map_err(BuildAndLaunchError::Internal)?;

    let result = launch_follow_resume_claimed(&state, &waiter, &successor_id).await;

    match result {
        Ok(SuccessorLaunchOutcome::Launched(native)) => {
            // Durably launched: the successor's own checkpoint now carries the
            // spliced result, so it is independently reconcile-recoverable. Retire
            // the waiter.
            let _ = state.state_store.clear_follow_waiter(follow_key);
            Ok(SuccessorLaunchOutcome::Launched(native))
        }
        // Skips leave the waiter for a later drive (or it was already cleared by the
        // not-created branch below).
        Ok(skipped) => Ok(skipped),
        Err(e) => {
            // A transient filesystem/CAS interruption leaves the successor
            // `created` and the waiter `resuming`. Preserve both so the periodic
            // follow reconciler can safely re-drive the idempotent checkpoint
            // splice and launch. Deterministic defects still terminalize below.
            if e.retryable_launch_interruption() {
                tracing::warn!(
                    follow_key,
                    successor_id,
                    error = %e,
                    "follow-resume launch interrupted; leaving waiter for reconcile"
                );
                return Err(e);
            }
            // A failed parent-resume finalizes the successor. If THIS parent chain is
            // itself the child of an OUTER follow (nested follow), that finalize flips
            // the outer waiter to ready — so kick it. The follow-resume successor
            // lives in the parent's chain, so the parent chain root IS its chain root.
            // No-op for a non-nested resume.
            if let Err(cleanup_error) = finalize_failed_and_kick_follow(
                &state,
                &successor_id,
                &waiter.parent_chain_root_id,
                &launch_owner,
                json!({ "error": e.to_string() }),
            ) {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "follow-resume launch failed: {e}; terminal cleanup also failed: \
                     {cleanup_error}"
                )));
            }
            Err(e)
        }
    }
}

fn append_follow_terminal_envelope(
    budget: &mut RuntimeJsonArrayBudget,
    envelopes: &mut Vec<Value>,
    envelope: Value,
    index: u32,
) -> Result<(), BuildAndLaunchError> {
    budget.append(&envelope).map_err(|error| {
        BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow-resume: terminal-envelope cohort exceeded runtime JSON bounds at child index {index}: {error}"
        ))
    })?;
    envelopes.push(envelope);
    Ok(())
}

fn validate_follow_waiter_cardinality(
    fanout: bool,
    expected_children: u32,
) -> Result<(), BuildAndLaunchError> {
    if expected_children == 0 {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow-resume: waiter must declare at least one child"
        )));
    }
    if !fanout && expected_children != 1 {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow-resume: non-fanout waiter must declare exactly one child, received {expected_children}"
        )));
    }
    Ok(())
}

fn follow_resume_payload(
    fanout: bool,
    mut envelopes: Vec<Value>,
) -> Result<Value, BuildAndLaunchError> {
    if !fanout {
        if envelopes.len() != 1 {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "follow-resume: non-fanout cohort must contain exactly one terminal envelope, received {}",
                envelopes.len()
            )));
        }
        let envelope = envelopes.pop().expect("cardinality checked above");
        validate_checkpoint_shape(&envelope, "follow terminal envelope").map_err(|error| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "follow-resume: terminal envelope exceeded runtime JSON bounds: {error}"
            ))
        })?;
        return Ok(envelope);
    }
    let statuses: Vec<FanoutItemStatus> = envelopes
        .iter()
        .map(|envelope| {
            if ryeos_runtime::envelope::envelope_succeeded(envelope) {
                FanoutItemStatus::Completed
            } else {
                FanoutItemStatus::Failed
            }
        })
        .collect();
    let failed = statuses
        .iter()
        .filter(|status| **status == FanoutItemStatus::Failed)
        .count();
    let expected = envelopes.len();
    let mut fields = serde_json::Map::with_capacity(5);
    fields.insert("fanout".to_string(), Value::Bool(true));
    fields.insert("items".to_string(), Value::Array(envelopes));
    fields.insert("statuses".to_string(), serde_json::to_value(statuses)?);
    fields.insert("failed".to_string(), serde_json::to_value(failed)?);
    fields.insert("expected".to_string(), serde_json::to_value(expected)?);
    let payload = Value::Object(fields);
    validate_checkpoint_shape(&payload, "follow fanout resume payload").map_err(|error| {
        BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow-resume: fanout payload exceeded runtime JSON bounds: {error}"
        ))
    })?;
    Ok(payload)
}

async fn launch_follow_resume_claimed(
    state: &AppState,
    waiter: &ryeos_app::runtime_db::FollowWaiter,
    successor_id: &str,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    let successor = state.threads.get_thread(successor_id)?.ok_or_else(|| {
        BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow-resume: successor not found: {successor_id}"
        ))
    })?;

    // Marker validation BEFORE mutating anything: prove this successor really is the
    // graph-follow-resume successor of the waiter's parent. A splice + fold-the-
    // chain launch of the wrong row would run someone else's thread with the child's
    // result. Fail closed — a mismatch or marker-read error skips WITHOUT launching
    // (and without clearing the waiter: suspected corruption is left for inspection).
    if let Some(reason) =
        follow_resume_successor_refusal(state, &waiter.parent_thread_id, &successor)
    {
        return Ok(SuccessorLaunchOutcome::Skipped(reason));
    }

    // Only a `created` successor is launchable. A running/terminal row means the
    // resume already fired (or is live) — the waiter's job is done, so retire it and
    // skip WITHOUT re-splicing a live successor's checkpoint (which could corrupt an
    // in-flight resume).
    if successor.status != ryeos_state::objects::ThreadStatus::Created.as_str() {
        let _ = state.state_store.clear_follow_waiter(&waiter.follow_key);
        return Ok(SuccessorLaunchOutcome::Skipped("not_created"));
    }
    if let Some(reason) = attached_identity_launch_blocker(state, &successor)? {
        return Ok(SuccessorLaunchOutcome::Skipped(reason));
    }

    validate_follow_waiter_cardinality(waiter.fanout, waiter.expected_children)?;

    // Do not reserve from database cardinality or retain independently-valid
    // envelopes into an unbounded cohort. Each fanout child is admitted against
    // the aggregate checkpoint shape before it enters the vector.
    let mut envelopes = Vec::new();
    let mut fanout_budget = waiter.fanout.then(|| {
        RuntimeJsonArrayBudget::with_limits(
            "follow fanout terminal-envelope cohort",
            checkpoint_shape_limits(),
        )
    });
    for index in 0..waiter.expected_children {
        let child = state
            .state_store
            .get_follow_child(&waiter.follow_key, index)?
            .ok_or_else(|| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "follow-resume: missing child index {index}"
                ))
            })?;
        let envelope = child.terminal_envelope.ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "follow-resume: child index {index} has no terminal envelope"
            ))
        })?;
        if let Some(budget) = fanout_budget.as_mut() {
            append_follow_terminal_envelope(budget, &mut envelopes, envelope, index)?;
        } else {
            validate_checkpoint_shape(&envelope, "follow terminal envelope").map_err(|error| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "follow-resume: terminal envelope at child index {index} exceeded runtime JSON bounds: {error}"
                ))
            })?;
            envelopes.push(envelope);
        }
    }
    let terminal_envelope = follow_resume_payload(waiter.fanout, envelopes)?;

    // Mark resuming (ready→resuming; idempotent on resuming) BEFORE mutating the
    // successor's checkpoint, so a crash mid-resume is re-driven by reconcile.
    state
        .state_store
        .mark_follow_resuming(&waiter.follow_key)
        .map_err(|e| BuildAndLaunchError::Internal(anyhow::anyhow!(e)))?;

    // Seed the successor's checkpoint = parent's checkpoint + the child's canonical
    // envelope spliced under `follow_result`. The successor is `created` (not yet
    // running), so writing its checkpoint here races nothing.
    let prev_dir = ryeos_app::launch_metadata::daemon_checkpoint_dir(
        &state.config.app_root,
        &waiter.parent_thread_id,
    );
    let succ_dir =
        ryeos_app::launch_metadata::daemon_checkpoint_dir(&state.config.app_root, successor_id);
    let spliced = ryeos_runtime::checkpoint::CheckpointWriter::copy_latest_with_splice(
        &prev_dir,
        &succ_dir,
        ryeos_runtime::checkpoint::FOLLOW_RESULT_KEY,
        terminal_envelope,
    )
    .map_err(|e| BuildAndLaunchError::Internal(anyhow::anyhow!("follow-resume splice: {e}")))?;
    if !spliced {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow-resume: predecessor {} has no checkpoint to resume from",
            waiter.parent_thread_id
        )));
    }

    launch_claimed_successor(state, successor, SuccessorMode::Follow, None, None)
        .await
        .map(SuccessorLaunchOutcome::Launched)
}

fn parent_limits_from_context(
    parent_execution_context: Option<&crate::dispatch::ParentExecutionContext>,
) -> anyhow::Result<Option<HardLimits>> {
    let parent_limits_value = parent_execution_context.map(|ctx| &ctx.hard_limits);
    parent_limits_value
        .filter(|v| match v {
            Value::Null => false,
            Value::Object(m) => !m.is_empty(),
            _ => true,
        })
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(|e| anyhow::anyhow!("failed to parse parent_limits: {e}"))
}

fn launch_depth_from_context(
    parent_execution_context: Option<&crate::dispatch::ParentExecutionContext>,
) -> u32 {
    parent_execution_context
        .map(|ctx| ctx.depth.saturating_add(1))
        .unwrap_or(0)
}

fn prompt_inputs_from_parameters(parameters: &Value) -> Value {
    let mut inputs = parameters.clone();
    if let Some(obj) = inputs.as_object_mut() {
        for k in ryeos_runtime::callback::RESERVED_CONTROL_KEYS {
            obj.remove(*k);
        }
    }
    inputs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::limits::{LimitCaps, LimitValues};

    #[test]
    fn follow_fanout_payload_uses_closed_item_statuses() {
        let payload = follow_resume_payload(
            true,
            vec![
                json!({
                    "success": true,
                    "status": "completed",
                    "result": {"answer": 1},
                    "outputs": null,
                    "warnings": [],
                    "cost": null,
                }),
                json!({
                    "success": false,
                    "status": "failed",
                    "result": {"error": "boom"},
                    "outputs": null,
                    "warnings": [],
                    "cost": null,
                }),
            ],
        )
        .unwrap();
        let statuses: Vec<FanoutItemStatus> =
            serde_json::from_value(payload["statuses"].clone()).unwrap();
        assert_eq!(
            statuses,
            vec![FanoutItemStatus::Completed, FanoutItemStatus::Failed,]
        );
        assert_eq!(payload["failed"], 1);
    }

    #[test]
    fn follow_cohort_rejects_aggregate_before_retaining_child() {
        let limits = ryeos_runtime::EvaluationLimits {
            max_result_bytes: 20,
            ..ryeos_runtime::EvaluationLimits::default()
        };
        let mut budget = RuntimeJsonArrayBudget::with_limits("follow cohort", limits);
        let mut envelopes = Vec::new();

        append_follow_terminal_envelope(&mut budget, &mut envelopes, json!("first"), 0).unwrap();
        let error = append_follow_terminal_envelope(
            &mut budget,
            &mut envelopes,
            json!("second-is-too-large"),
            1,
        )
        .unwrap_err();

        assert!(error.to_string().contains("child index 1"));
        assert_eq!(envelopes, vec![json!("first")]);
        assert_eq!(budget.elements(), 1);
    }

    #[test]
    fn non_fanout_waiter_requires_exactly_one_child_before_collection() {
        let error = validate_follow_waiter_cardinality(false, 0).unwrap_err();
        assert!(error.to_string().contains("at least one child"));
        for expected_children in [2, u32::MAX] {
            let error = validate_follow_waiter_cardinality(false, expected_children).unwrap_err();
            assert!(error.to_string().contains("exactly one child"));
        }
        validate_follow_waiter_cardinality(false, 1).unwrap();
        let error = validate_follow_waiter_cardinality(true, 0).unwrap_err();
        assert!(error.to_string().contains("at least one child"));
        validate_follow_waiter_cardinality(true, 2).unwrap();
    }

    fn caps(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Test shim over the two-source [`apply_capability_policy`]. `child_execute_cap`
    /// is irrelevant to the non-follow policies, so they pass a placeholder.
    fn apply_policy(
        declared: &[&str],
        runtime_manifest: &[&str],
        policy: CapabilityPolicy<'_>,
        child_execute_cap: &str,
    ) -> Result<Vec<String>, BuildAndLaunchError> {
        apply_capability_policy(
            caps(declared),
            caps(runtime_manifest),
            policy,
            "i",
            child_execute_cap,
        )
    }

    #[test]
    fn capability_policy_fresh_unions_both_sources() {
        // Fresh runs with the union of caller-delegated and manifest caps.
        let out = apply_policy(
            &["ryeos.execute.tool.echo"],
            &["ryeos.get.vault.child/oauth"],
            CapabilityPolicy::Fresh,
            "",
        )
        .unwrap();
        assert_eq!(
            out,
            caps(&["ryeos.execute.tool.echo", "ryeos.get.vault.child/oauth"])
        );
    }

    #[test]
    fn capability_policy_exact_pinned_requires_equality() {
        // Equal set (order-insensitive, across both sources) → ok.
        let pinned = caps(&["b", "a"]);
        let out = apply_policy(&["a"], &["b"], CapabilityPolicy::ExactPinned(&pinned), "").unwrap();
        assert_eq!(out, caps(&["a", "b"]));
        // Drift (narrower OR wider) → rejected.
        let narrower = caps(&["a"]);
        assert!(apply_policy(
            &["a", "b"],
            &[],
            CapabilityPolicy::ExactPinned(&narrower),
            ""
        )
        .is_err());
        let wider = caps(&["a", "b", "c"]);
        assert!(apply_policy(&["a", "b"], &[], CapabilityPolicy::ExactPinned(&wider), "").is_err());
    }

    // ── Follow-child hybrid: source-aware bounding ──────────────────────
    // Declared (caller-delegated) caps must be covered by the parent and keep the
    // child's exact shape; manifest runtime caps are preserved without parent
    // coverage; the parent must imply the child's execute cap (admission).

    const CHILD_EXEC: &str = "ryeos.execute.tool.echo";

    fn hybrid(parent: &[String]) -> CapabilityPolicy<'_> {
        CapabilityPolicy::FollowChildHybrid {
            parent_effective_caps: parent,
        }
    }

    #[test]
    fn follow_hybrid_parent_wildcard_narrows_to_child_exact() {
        // parent execute.tool.* covers child-declared execute.tool.echo; the child
        // keeps its exact shape, NOT the parent wildcard.
        let parent = caps(&["ryeos.execute.tool.*"]);
        let out = apply_policy(
            &["ryeos.execute.tool.echo"],
            &[],
            hybrid(&parent),
            CHILD_EXEC,
        )
        .unwrap();
        assert_eq!(out, caps(&["ryeos.execute.tool.echo"]));
    }

    #[test]
    fn follow_hybrid_broad_parent_wildcard_does_not_leak() {
        // parent execute.* covers the child cap, but the result is still the
        // child's exact cap — the broad parent grant is never copied in.
        let parent = caps(&["ryeos.execute.*"]);
        let out = apply_policy(
            &["ryeos.execute.tool.echo"],
            &[],
            hybrid(&parent),
            CHILD_EXEC,
        )
        .unwrap();
        assert_eq!(out, caps(&["ryeos.execute.tool.echo"]));
    }

    #[test]
    fn follow_hybrid_child_wildcard_requires_parent_coverage() {
        // parent has only the exact execute.tool.echo; a child-declared wildcard
        // execute.tool.* is wider than the parent grant → rejected.
        let parent = caps(&["ryeos.execute.tool.echo"]);
        assert!(apply_policy(&["ryeos.execute.tool.*"], &[], hybrid(&parent), CHILD_EXEC).is_err());
    }

    #[test]
    fn follow_hybrid_admission_separate_from_run_set() {
        // parent can execute the child AND holds the delegated tool.echo; only the
        // child's declared cap lands in the run-set (admission cap is not added).
        let parent = caps(&["ryeos.execute.tool.echo", "ryeos.execute.tool.echo"]);
        let out = apply_policy(
            &["ryeos.execute.tool.echo"],
            &[],
            hybrid(&parent),
            "ryeos.execute.tool.echo",
        )
        .unwrap();
        assert_eq!(out, caps(&["ryeos.execute.tool.echo"]));
    }

    #[test]
    fn follow_hybrid_admission_cap_is_not_added_to_run_set() {
        // Parent may execute the child (admission cap `directive.child`) AND holds
        // the delegated `tool.echo` the child declares. The run-set is exactly the
        // child's declared cap — the execute-child admission grant is NOT inherited.
        let parent = caps(&["ryeos.execute.directive.child", "ryeos.execute.tool.echo"]);
        let out = apply_policy(
            &["ryeos.execute.tool.echo"],
            &[],
            hybrid(&parent),
            "ryeos.execute.directive.child",
        )
        .unwrap();
        assert_eq!(out, caps(&["ryeos.execute.tool.echo"]));
    }

    #[test]
    fn follow_hybrid_missing_delegated_cap_rejected() {
        // parent may execute the child but does NOT hold the delegated tool.echo
        // the child declares → rejected (confused-deputy guard).
        let parent = caps(&["ryeos.execute.tool.echo"]);
        // Parent's execute authority is over the child item itself, but it lacks
        // the *delegated* grant the child declares.
        let out = apply_policy(
            &["ryeos.execute.service.threads/get"],
            &[],
            hybrid(&parent),
            CHILD_EXEC,
        );
        assert!(out.is_err());
    }

    #[test]
    fn follow_hybrid_admission_denied_when_parent_cannot_execute_child() {
        // parent holds no execute authority over the child item → admission denied
        // before any run-set is computed.
        let parent = caps(&["ryeos.execute.tool.other"]);
        assert!(apply_policy(&[], &[], hybrid(&parent), CHILD_EXEC).is_err());
    }

    fn write_materializer_fixture(
        bundle_root: &Path,
        bare_name: &str,
        signing_key: &lillux::crypto::SigningKey,
    ) -> String {
        let ai_dir = bundle_root.join(ryeos_engine::AI_DIR);
        let cas = lillux::cas::CasStore::new(ai_dir.join("objects"));
        let blob_hash = cas.store_blob(b"not reached by duplicate check").unwrap();
        let item_ref = format!("bin/{}/{bare_name}", host_triple());
        let item_source = serde_json::json!({
            "kind": "item_source",
            "item_ref": item_ref,
            "content_blob_hash": blob_hash,
            "integrity": format!("sha256:{blob_hash}"),
            "mode": 0o755,
            "signature_info": null,
        });
        let item_source_hash = cas.store_object(&item_source).unwrap();
        let manifest = serde_json::json!({
            "kind": "source_manifest",
            "item_source_hashes": {
                item_ref: item_source_hash,
            },
        });
        let manifest_hash = cas.store_object(&manifest).unwrap();
        let ref_path = ai_dir.join(BUNDLE_MANIFEST_REF);
        std::fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
        let signed_ref = lillux::signature::sign_content(
            &format!(
                "{}\n{manifest_hash}\n",
                ryeos_engine::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN,
            ),
            signing_key,
            "#",
            None,
        );
        std::fs::write(ref_path, signed_ref).unwrap();

        lillux::signature::compute_fingerprint(&signing_key.verifying_key())
    }

    #[test]
    fn materializer_rejects_duplicate_native_executor_instead_of_first_root_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        let key = lillux::crypto::SigningKey::from_bytes(&[71u8; 32]);
        let fingerprint = write_materializer_fixture(&first, "shared-executor", &key);
        write_materializer_fixture(&second, "shared-executor", &key);
        let trust_store = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint,
                verifying_key: key.verifying_key(),
                label: None,
            },
        ]);

        let error = materialize_native_executor(
            &[first.clone(), second.clone()],
            "native:shared-executor",
            tmp.path(),
            &trust_store,
            ryeos_engine::resolution::TrustClass::TrustedBundle,
        )
        .expect_err("root order must not select between duplicate executor identities");
        let message = error.to_string();
        assert!(message.contains("published by both"));
        assert!(message.contains(&first.display().to_string()));
        assert!(message.contains(&second.display().to_string()));
    }

    #[test]
    fn follow_hybrid_preserves_child_manifest_runtime_caps() {
        // A manifest-minted runtime cap the parent does NOT hold is preserved —
        // it's the child's own signed authority, not delegated from the parent.
        let parent = caps(&["ryeos.execute.tool.*"]);
        let out = apply_policy(
            &["ryeos.execute.tool.echo"],
            &["ryeos.get.vault.child-bundle/oauth"],
            hybrid(&parent),
            CHILD_EXEC,
        )
        .unwrap();
        assert_eq!(
            out,
            caps(&[
                "ryeos.execute.tool.echo",
                "ryeos.get.vault.child-bundle/oauth"
            ])
        );
    }

    #[test]
    fn host_triple_matches_rustc_host() {
        // The bundle build pipeline writes binaries under
        // `bin/<triple>/<name>` where `<triple>` is `rustc -vV | grep ^host:`
        // (see `crates/tools/core-tools/tests/build_bundle_smoke.rs::host_triple`). The
        // daemon's `host_triple()` MUST produce the same string or
        // materialization will silently fail to find the binary.
        let output = std::process::Command::new("rustc")
            .args(["-vV"])
            .output()
            .expect("rustc -vV");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let rustc_host = stdout
            .lines()
            .find_map(|l| l.strip_prefix("host:"))
            .expect("rustc -vV must report host:")
            .trim()
            .to_string();

        assert_eq!(
            host_triple(),
            rustc_host,
            "daemon host_triple() must match `rustc -vV | grep ^host:` so that \
             bundle binaries written at `bin/<triple>/<name>` resolve. If this \
             fails, check crates/bin/daemon/build.rs forwards Cargo's TARGET env var.",
        );

        // Format sanity: rustc host triples have either 3 segments (e.g.
        // x86_64-apple-darwin) or 4 (e.g. x86_64-unknown-linux-gnu). The
        // V5.1 bug produced 3-segment Linux triples missing the ABI.
        let segs = host_triple().split('-').count();
        assert!(
            (3..=4).contains(&segs),
            "host_triple() {:?} should have 3 or 4 dash-separated segments, got {}",
            host_triple(),
            segs,
        );
        if cfg!(target_os = "linux") {
            assert_eq!(
                segs,
                4,
                "linux rustc triples include an ABI segment (gnu/musl); got {:?}",
                host_triple(),
            );
        }
    }

    use ryeos_engine::resolution::{KindComposedView, TrustClass};
    use std::collections::HashMap;

    #[test]
    fn enforce_trust_blocks_unsigned() {
        let err = enforce_effective_trust(TrustClass::Unsigned, "directive:my/agent", "directive")
            .unwrap_err();
        assert_eq!(err.code(), "effective_trust_unsigned");
        assert_eq!(err.http_status(), axum::http::StatusCode::FORBIDDEN);
        assert!(matches!(
            &err,
            DispatchError::LaunchPolicyForbidden { binding: None, .. }
        ));
        let msg = format!("{err}");
        assert!(msg.contains("refusing to spawn"));
        assert!(msg.contains("Unsigned"));
        assert!(msg.contains("directive:my/agent"));
    }

    #[test]
    fn enforce_trust_allows_trusted_classes() {
        for cls in [
            TrustClass::TrustedBundle,
            TrustClass::TrustedProject,
            TrustClass::UntrustedProject,
        ] {
            enforce_effective_trust(cls, "directive:x", "directive")
                .unwrap_or_else(|e| panic!("{cls:?} should pass, got: {e}"));
        }
    }

    fn view_with_caps(caps: Vec<&str>) -> KindComposedView {
        let mut policy_facts = HashMap::new();
        policy_facts.insert(
            POLICY_FACT_EFFECTIVE_CAPS.to_string(),
            serde_json::Value::Array(
                caps.into_iter()
                    .map(|c| serde_json::Value::String(c.to_string()))
                    .collect(),
            ),
        );
        KindComposedView {
            composed: serde_json::json!({}),
            derived: HashMap::new(),
            policy_facts,
        }
    }

    #[test]
    fn caps_passed_through_from_policy_fact() {
        let view = view_with_caps(vec!["ryeos.execute.tool.bash", "ryeos.execute.tool.read"]);
        let caps = derive_effective_caps(&view);
        assert_eq!(
            caps,
            vec!["ryeos.execute.tool.bash", "ryeos.execute.tool.read"]
        );
    }

    #[test]
    fn missing_policy_fact_yields_empty_caps() {
        let view = KindComposedView::identity(serde_json::json!({}));
        let caps = derive_effective_caps(&view);
        assert!(caps.is_empty(), "expected deny-all, got: {caps:?}");
    }

    #[test]
    fn materialization_error_messages_are_descriptive() {
        let cases: Vec<(MaterializationError, &str)> = vec![
            (
                MaterializationError::ExecutorUnavailable {
                    executor_ref: "tool:my/bash".into(),
                    detail: "not in manifest".into(),
                },
                "tool:my/bash",
            ),
            (
                MaterializationError::ManifestError("bad json".into()),
                "bad json",
            ),
            (
                MaterializationError::ResolutionFailed {
                    executor_ref: "tool:x/y".into(),
                    detail: "no such ref".into(),
                },
                "tool:x/y",
            ),
            (
                MaterializationError::BlobNotFound {
                    hash: "sha256:abc123".into(),
                },
                "sha256:abc123",
            ),
            (
                MaterializationError::ArchCheckFailed {
                    executor_ref: "tool:x/y".into(),
                    detail: "x86_64 vs aarch64".into(),
                },
                "x86_64",
            ),
            (
                MaterializationError::MaterializationFailed {
                    executor_ref: "tool:x/y".into(),
                    detail: "disk full".into(),
                },
                "disk full",
            ),
        ];
        for (err, expected_substr) in cases {
            let msg = format!("{err}");
            assert!(
                msg.contains(expected_substr),
                "expected {:?} to contain {:?}",
                msg,
                expected_substr,
            );
        }
    }

    #[test]
    fn build_and_launch_error_from_serde_json() {
        let json_err = serde_json::from_str::<Value>("{bad").unwrap_err();
        let err = BuildAndLaunchError::from(json_err);
        let msg = format!("{err}");
        assert!(!msg.is_empty());
    }

    #[test]
    fn build_and_launch_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
        let err = BuildAndLaunchError::from(io_err);
        let msg = format!("{err}");
        assert!(msg.contains("file gone"));
    }

    #[test]
    fn parent_context_clamps_child_limits_and_increments_spawn_depth() {
        let parent_hard_limits = HardLimits {
            turns: 6,
            tokens: 1_000,
            spend_usd: 0.25,
            spawns: 2,
            depth: 3,
            duration_seconds: 45,
        };
        let ctx = crate::dispatch::ParentExecutionContext {
            parent_thread_id: "T-parent".to_string(),
            hard_limits: serde_json::to_value(&parent_hard_limits).unwrap(),
            depth: 4,
        };

        let parent_limits = parent_limits_from_context(Some(&ctx))
            .expect("parent hard limits parse")
            .expect("parent hard limits present");
        let requested = LimitValues {
            turns: 20,
            tokens: 20_000,
            spend_usd: 2.0,
            spawns: 10,
            depth: 8,
            duration_seconds: 300,
        };
        let hard = compute_effective_limits(
            Some(&requested),
            &LimitValues::default(),
            &LimitCaps::default(),
            Some(&parent_limits),
        );

        assert_eq!(hard.turns, 6);
        assert_eq!(hard.tokens, 1_000);
        assert_eq!(hard.spend_usd, 0.25);
        assert_eq!(hard.spawns, 2);
        assert_eq!(hard.depth, 3);
        assert_eq!(hard.duration_seconds, 45);
        assert_eq!(launch_depth_from_context(Some(&ctx)), 5);
    }

    #[test]
    fn absent_parent_context_is_root_or_same_braid_launch() {
        assert!(parent_limits_from_context(None).unwrap().is_none());
        assert_eq!(launch_depth_from_context(None), 0);
    }

    #[test]
    fn empty_parent_limits_do_not_zero_erase_child_limits() {
        let ctx = crate::dispatch::ParentExecutionContext {
            parent_thread_id: "T-parent".to_string(),
            hard_limits: json!({}),
            depth: 2,
        };

        assert!(parent_limits_from_context(Some(&ctx)).unwrap().is_none());
        assert_eq!(launch_depth_from_context(Some(&ctx)), 3);
    }

    #[test]
    fn malformed_parent_limits_fail_loudly() {
        let ctx = crate::dispatch::ParentExecutionContext {
            parent_thread_id: "T-parent".to_string(),
            hard_limits: json!({"turns": "not-a-number"}),
            depth: 0,
        };

        let err = parent_limits_from_context(Some(&ctx)).unwrap_err();
        assert!(
            err.to_string().contains("failed to parse parent_limits"),
            "got: {err}"
        );
    }

    #[test]
    fn forged_parent_control_params_are_not_launch_context_or_prompt_input() {
        let params = json!({
            "task": "keep this",
            "parent_limits": {"turns": 1},
            "parent_thread_id": "T-forged",
            "depth": 99,
            "continuation": {"seed": "forged"}
        });

        assert!(
            parent_limits_from_context(None).unwrap().is_none(),
            "parent clamp must come only from trusted ParentExecutionContext"
        );
        assert_eq!(
            launch_depth_from_context(None),
            0,
            "forged params must not affect launch depth"
        );
        assert_eq!(
            prompt_inputs_from_parameters(&params),
            json!({"task": "keep this"})
        );
    }
}
