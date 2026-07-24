use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

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
    apply_caller_limit_overrides, apply_execution_policy_defaults,
    apply_execution_policy_item_overrides, compute_effective_limits,
    load_limits_config_from_loader, merge_header_limits, policy_item_override,
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
/// Raised by [`materialize_native_executor_for_engine`] when the bundle CAS
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
    pub bundle_manifest_hash: String,
    pub bundle_signer_fingerprint: String,
    /// Exact no-follow descriptor whose identity passed materialization
    /// verification. Native launch paths must carry this handle through the
    /// isolation boundary instead of reopening `path`.
    pub verified_command: ryeos_engine::isolation::IsolationDescriptorBoundCommand,
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

/// Typed error returned by [`build_and_launch`]. Materialization,
/// cancellation, admission, and launch-preparation failures retain stable
/// variants; unexpected infrastructure failures use `Internal`.
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
    #[error("launch `{thread_id}` was cancelled before {stage}: {detail}")]
    LaunchCancelled {
        thread_id: String,
        stage: &'static str,
        detail: String,
    },
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
    pub fn retryable_launch_interruption(&self) -> bool {
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
            | Self::CapabilityRejected { .. }
            | Self::LaunchCancelled { .. } => false,
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

impl From<ryeos_engine::error::EngineError> for MaterializationError {
    fn from(error: ryeos_engine::error::EngineError) -> Self {
        Self::Internal(error.to_string())
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

const BUNDLE_MANIFEST_REF: &str = "refs/bundles/manifest";
const EXECUTOR_VERIFICATION_CACHE_MAX_ENTRIES: usize = 64;
const EXECUTOR_VERIFICATION_CACHE_MAX_IN_FLIGHT: usize = 64;
const EXECUTOR_VERIFICATION_CACHE_MAX_METADATA_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ManifestRefProbe {
    bundle_root: PathBuf,
    cas_ready: bool,
    signed_ref_digest: Option<String>,
}

/// Cheap lookup identity read before deciding whether the expensive signed CAS
/// chain may be reused. Every registered bundle root participates, including
/// roots that do not publish the requested executor, preserving the mandatory
/// all-roots ambiguity check.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExecutorVerificationProbe {
    bundle_generation_fingerprint: String,
    node_trust_fingerprint: String,
    root_trust_class: ryeos_engine::resolution::TrustClass,
    host_triple: String,
    executor_ref: String,
    manifest_refs: Vec<ManifestRefProbe>,
}

/// Full verified-chain identity retained by the cache. The probe is the lookup
/// index; this key additionally binds every authenticated object/content edge
/// that selected the executable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VerifiedExecutorChainKey {
    probe: ExecutorVerificationProbe,
    bundle_root: PathBuf,
    signed_manifest_ref_digest: String,
    manifest_object_hash: String,
    item_source_object_hash: String,
    blob_hash: String,
    blob_len: u64,
    mode: u32,
    signer_fingerprint: String,
}

#[derive(Debug)]
struct VerifiedNativeExecutorChain {
    key: VerifiedExecutorChainKey,
}

struct ExecutorVerificationCacheEntry {
    verified: Arc<VerifiedNativeExecutorChain>,
    last_used: u64,
    metadata_bytes: usize,
}

#[derive(Default)]
struct ExecutorVerificationCacheState {
    by_probe: HashMap<ExecutorVerificationProbe, VerifiedExecutorChainKey>,
    entries: HashMap<VerifiedExecutorChainKey, ExecutorVerificationCacheEntry>,
    in_flight: HashSet<ExecutorVerificationProbe>,
    tick: u64,
    metadata_bytes: usize,
}

struct ExecutorVerificationCache {
    state: Mutex<ExecutorVerificationCacheState>,
    ready: Condvar,
}

static EXECUTOR_VERIFICATION_CACHE: OnceLock<ExecutorVerificationCache> = OnceLock::new();

fn executor_verification_cache() -> &'static ExecutorVerificationCache {
    EXECUTOR_VERIFICATION_CACHE.get_or_init(|| ExecutorVerificationCache {
        state: Mutex::new(ExecutorVerificationCacheState::default()),
        ready: Condvar::new(),
    })
}

fn verified_chain_metadata_bytes(key: &VerifiedExecutorChainKey) -> usize {
    let mut total = key.probe.bundle_generation_fingerprint.len()
        + key.probe.node_trust_fingerprint.len()
        + key.probe.host_triple.len()
        + key.probe.executor_ref.len()
        + key.bundle_root.as_os_str().as_encoded_bytes().len()
        + key.signed_manifest_ref_digest.len()
        + key.manifest_object_hash.len()
        + key.item_source_object_hash.len()
        + key.blob_hash.len()
        + key.signer_fingerprint.len()
        + std::mem::size_of::<VerifiedExecutorChainKey>();
    for manifest_ref in &key.probe.manifest_refs {
        total += manifest_ref
            .bundle_root
            .as_os_str()
            .as_encoded_bytes()
            .len()
            + manifest_ref
                .signed_ref_digest
                .as_ref()
                .map_or(0, String::len)
            + std::mem::size_of::<ManifestRefProbe>();
    }
    total
}

fn remove_cached_probe(
    state: &mut ExecutorVerificationCacheState,
    probe: &ExecutorVerificationProbe,
) {
    if let Some(key) = state.by_probe.remove(probe) {
        if let Some(entry) = state.entries.remove(&key) {
            state.metadata_bytes = state.metadata_bytes.saturating_sub(entry.metadata_bytes);
        }
    }
}

fn retire_other_executor_generations(
    state: &mut ExecutorVerificationCacheState,
    current_generation: &str,
) {
    let stale = state
        .entries
        .keys()
        .filter(|key| key.probe.bundle_generation_fingerprint.as_str() != current_generation)
        .cloned()
        .collect::<Vec<_>>();
    for key in stale {
        if let Some(entry) = state.entries.remove(&key) {
            state.metadata_bytes = state.metadata_bytes.saturating_sub(entry.metadata_bytes);
        }
        state.by_probe.retain(|_, indexed| indexed != &key);
    }
}

enum ExecutorVerificationCacheLookup {
    Hit(Arc<VerifiedNativeExecutorChain>),
    Owner(ExecutorVerificationFlight),
    Bypass,
}

struct ExecutorVerificationFlight {
    probe: ExecutorVerificationProbe,
    complete: bool,
}

impl ExecutorVerificationFlight {
    fn publish(
        mut self,
        verified: VerifiedNativeExecutorChain,
    ) -> Arc<VerifiedNativeExecutorChain> {
        let cache = executor_verification_cache();
        let mut state = cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        remove_cached_probe(&mut state, &self.probe);
        retire_other_executor_generations(&mut state, &self.probe.bundle_generation_fingerprint);
        state.tick = state.tick.wrapping_add(1);
        let last_used = state.tick;
        let key = verified.key.clone();
        let metadata_bytes = verified_chain_metadata_bytes(&key);
        if metadata_bytes > EXECUTOR_VERIFICATION_CACHE_MAX_METADATA_BYTES {
            let verified = Arc::new(verified);
            state.in_flight.remove(&self.probe);
            self.complete = true;
            cache.ready.notify_all();
            return verified;
        }
        while !state.entries.is_empty()
            && (state.entries.len() >= EXECUTOR_VERIFICATION_CACHE_MAX_ENTRIES
                || state.metadata_bytes.saturating_add(metadata_bytes)
                    > EXECUTOR_VERIFICATION_CACHE_MAX_METADATA_BYTES)
        {
            let Some(oldest) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = state.entries.remove(&oldest) {
                state.metadata_bytes = state.metadata_bytes.saturating_sub(entry.metadata_bytes);
            }
            state.by_probe.retain(|_, indexed| indexed != &oldest);
        }
        let verified = Arc::new(verified);
        state.metadata_bytes = state.metadata_bytes.saturating_add(metadata_bytes);
        state.entries.insert(
            key.clone(),
            ExecutorVerificationCacheEntry {
                verified: verified.clone(),
                last_used,
                metadata_bytes,
            },
        );
        state.by_probe.insert(self.probe.clone(), key);
        state.in_flight.remove(&self.probe);
        self.complete = true;
        cache.ready.notify_all();
        verified
    }
}

impl Drop for ExecutorVerificationFlight {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        let cache = executor_verification_cache();
        let mut state = cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.in_flight.remove(&self.probe);
        cache.ready.notify_all();
    }
}

fn lookup_or_claim_executor_verification(
    probe: &ExecutorVerificationProbe,
    force_reverify: bool,
) -> ExecutorVerificationCacheLookup {
    let cache = executor_verification_cache();
    let mut state = cache
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        retire_other_executor_generations(&mut state, &probe.bundle_generation_fingerprint);
        if force_reverify {
            // A repair needs the authenticated blob bytes, which ordinary
            // metadata-cache hits deliberately do not retain. Re-apply the
            // invalidation after every single-flight wake so a concurrent
            // verifier cannot publish a metadata-only hit into this forced
            // path.
            remove_cached_probe(&mut state, probe);
        } else if let Some(key) = state.by_probe.get(probe).cloned() {
            state.tick = state.tick.wrapping_add(1);
            let last_used = state.tick;
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.last_used = last_used;
                return ExecutorVerificationCacheLookup::Hit(entry.verified.clone());
            }
            state.by_probe.remove(probe);
        }
        if !state.in_flight.contains(probe)
            && state.in_flight.len() >= EXECUTOR_VERIFICATION_CACHE_MAX_IN_FLIGHT
        {
            return ExecutorVerificationCacheLookup::Bypass;
        }
        if state.in_flight.insert(probe.clone()) {
            return ExecutorVerificationCacheLookup::Owner(ExecutorVerificationFlight {
                probe: probe.clone(),
                complete: false,
            });
        }
        state = cache
            .ready
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

fn manifest_ref_probe(
    bundle_roots: &[PathBuf],
    bundle_generation_fingerprint: &str,
    node_trust_fingerprint: &str,
    executor_ref: &str,
    triple: &str,
    root_trust_class: ryeos_engine::resolution::TrustClass,
) -> Result<ExecutorVerificationProbe, MaterializationError> {
    let mut manifest_refs = Vec::with_capacity(bundle_roots.len());
    for bundle_root in bundle_roots {
        let ai_dir = bundle_root.join(ryeos_engine::AI_DIR);
        let objects = ai_dir.join("objects");
        let cas_ready = objects.join("blobs").is_dir() && objects.join("objects").is_dir();
        let ref_path = ai_dir.join(BUNDLE_MANIFEST_REF);
        let signed_ref_digest = match std::fs::read(&ref_path) {
            Ok(bytes) => Some(lillux::cas::sha256_hex(&bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(MaterializationError::ManifestError(format!(
                    "failed to read signed bundle executor manifest ref {}: {error}",
                    ref_path.display()
                )))
            }
        };
        manifest_refs.push(ManifestRefProbe {
            bundle_root: bundle_root.clone(),
            cas_ready,
            signed_ref_digest,
        });
    }
    Ok(ExecutorVerificationProbe {
        bundle_generation_fingerprint: bundle_generation_fingerprint.to_owned(),
        node_trust_fingerprint: node_trust_fingerprint.to_owned(),
        root_trust_class,
        host_triple: triple.to_owned(),
        executor_ref: executor_ref.to_owned(),
        manifest_refs,
    })
}

fn verify_native_executor_chain(
    probe: &ExecutorVerificationProbe,
    bare: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
    launch_timings: Option<&ryeos_app::launch_stage_timings::LaunchStageTimings>,
) -> Result<(VerifiedNativeExecutorChain, Vec<u8>), MaterializationError> {
    let manifest_verification_timer = launch_timings.map(|timings| {
        timings.nested(
            "background_dispatch",
            "executor_manifest_chain_verification",
        )
    });
    let mut tried_roots: Vec<PathBuf> = Vec::new();
    let mut last_resolution_error: Option<String> = None;
    let mut resolved_with: Option<(
        PathBuf,
        String,
        lillux::cas::CasStore,
        ryeos_engine::executor_resolution::ResolvedExecutor,
        ryeos_engine::executor_resolution::VerifiedExecutorManifestRef,
    )> = None;

    for manifest_probe in &probe.manifest_refs {
        if !manifest_probe.cas_ready {
            continue;
        }
        let Some(expected_ref_digest) = manifest_probe.signed_ref_digest.as_ref() else {
            continue;
        };
        let system_root = &manifest_probe.bundle_root;
        let ai_dir = system_root.join(ryeos_engine::AI_DIR);
        let objects_dir = ai_dir.join("objects");
        let ref_path = ai_dir.join(BUNDLE_MANIFEST_REF);
        let signed_ref = std::fs::read_to_string(&ref_path).map_err(|error| {
            MaterializationError::ManifestError(format!(
                "failed to re-read signed bundle executor manifest ref {}: {error}",
                ref_path.display()
            ))
        })?;
        let live_ref_digest = lillux::cas::sha256_hex(signed_ref.as_bytes());
        if &live_ref_digest != expected_ref_digest {
            return Err(MaterializationError::ManifestError(format!(
                "signed bundle executor manifest ref {} changed during generation-checked verification",
                ref_path.display()
            )));
        }
        tried_roots.push(system_root.clone());

        let verified_ref =
            match ryeos_engine::executor_resolution::verify_signed_executor_manifest_ref(
                &signed_ref,
                |fingerprint| {
                    trust_store
                        .get(fingerprint)
                        .map(|signer| signer.verifying_key)
                },
                probe.root_trust_class,
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
        let manifest_hash = verified_ref.manifest_hash.clone();
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
            .get_object(&manifest_hash)
            .map_err(|error| {
                MaterializationError::ManifestError(format!(
                    "failed to read bundle manifest object {manifest_hash}: {error}"
                ))
            })?
            .ok_or_else(|| {
                MaterializationError::ManifestError(format!(
                    "bundle manifest object {manifest_hash} not found in system CAS"
                ))
            })?;
        let manifest_item_source_hashes =
            ryeos_engine::executor_resolution::verify_executor_manifest_object(
                &manifest_value,
                &manifest_hash,
            )
            .map_err(|error| {
                MaterializationError::ManifestError(format!(
                    "bundle executor manifest {manifest_hash} failed verification: {error}"
                ))
            })?;

        tracing::debug!(
            executor_ref = %probe.executor_ref,
            host_triple = %probe.host_triple,
            bundle_root = %system_root.display(),
            manifest_entries = manifest_item_source_hashes.len(),
            "scanning bundle manifest for native executor"
        );

        match ryeos_engine::executor_resolution::resolve_native_executor(
            &manifest_item_source_hashes,
            &probe.executor_ref,
            &probe.host_triple,
            |hash| cas.get_object(hash).map_err(|error| error.to_string()),
        ) {
            Ok(resolved) => {
                if resolved.mode & 0o022 != 0 {
                    return Err(MaterializationError::ResolutionFailed {
                        executor_ref: bare.to_string(),
                        detail: format!(
                            "signed executor mode {:#o} is group/other writable",
                            resolved.mode
                        ),
                    });
                }
                if let Some((first_root, ..)) = &resolved_with {
                    return Err(MaterializationError::ResolutionFailed {
                        executor_ref: bare.to_string(),
                        detail: format!(
                            "native executor identity `bin/{}/{bare}` is published by both {} and {}; bundle root order cannot select an executor",
                            probe.host_triple,
                            first_root.display(),
                            system_root.display(),
                        ),
                    });
                }
                resolved_with = Some((
                    system_root.clone(),
                    live_ref_digest,
                    cas,
                    resolved,
                    verified_ref,
                ));
            }
            Err(
                error @ ryeos_engine::executor_resolution::ExecutorResolutionError::NotInManifest {
                    ..
                },
            ) => {
                last_resolution_error = Some(error.to_string());
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
                 The bundle pipeline must ship binaries for host triple '{}'.",
                probe.host_triple
            ),
        });
    }

    let (bundle_root, signed_ref_digest, cas, resolved, verified_ref) =
        resolved_with.ok_or_else(|| MaterializationError::ResolutionFailed {
            executor_ref: bare.to_string(),
            detail: last_resolution_error.unwrap_or_else(|| {
                format!(
                    "no manifest among {} system bundle root(s) lists '{}' for triple '{}'",
                    tried_roots.len(),
                    probe.executor_ref,
                    probe.host_triple
                )
            }),
        })?;
    drop(manifest_verification_timer);

    let blob_fetch_timer = launch_timings.map(|timings| {
        timings.nested(
            "background_dispatch",
            "executor_blob_fetch_hash_and_arch_check",
        )
    });
    let blob_bytes = cas
        .get_blob(&resolved.blob_hash)
        .map_err(|error| MaterializationError::BlobNotFound {
            hash: format!("{} (read error: {error})", resolved.blob_hash),
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
    arch_check::check_arch(&blob_bytes, std::env::consts::ARCH).map_err(|error| {
        MaterializationError::ArchCheckFailed {
            executor_ref: bare.to_string(),
            detail: error.to_string(),
        }
    })?;
    drop(blob_fetch_timer);

    tracing::info!(
        executor_ref = %probe.executor_ref,
        host_triple = %probe.host_triple,
        manifest_hash = %verified_ref.manifest_hash,
        item_source_hash = %resolved.item_source_hash,
        blob_hash = %resolved.blob_hash,
        signer_fp = %verified_ref.signer_fingerprint,
        trust_class = ?verified_ref.trust_class,
        "native executor CAS chain cryptographically verified"
    );

    let blob_len = u64::try_from(blob_bytes.len()).map_err(|_| {
        MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: "native executor blob length does not fit u64".to_string(),
        }
    })?;
    Ok((
        VerifiedNativeExecutorChain {
            key: VerifiedExecutorChainKey {
                probe: probe.clone(),
                bundle_root,
                signed_manifest_ref_digest: signed_ref_digest,
                manifest_object_hash: verified_ref.manifest_hash,
                item_source_object_hash: resolved.item_source_hash,
                blob_hash: resolved.blob_hash,
                blob_len,
                mode: resolved.mode,
                signer_fingerprint: verified_ref.signer_fingerprint,
            },
        },
        blob_bytes,
    ))
}

fn cached_or_verified_executor_chain(
    probe: &ExecutorVerificationProbe,
    bare: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
    force_reverify: bool,
    launch_timings: Option<&ryeos_app::launch_stage_timings::LaunchStageTimings>,
) -> Result<(Arc<VerifiedNativeExecutorChain>, Option<Vec<u8>>), MaterializationError> {
    match lookup_or_claim_executor_verification(probe, force_reverify) {
        ExecutorVerificationCacheLookup::Hit(verified) => {
            if executor_cache_verify_hits_enabled() {
                let (cold, cold_blob) =
                    verify_native_executor_chain(probe, bare, trust_store, launch_timings)?;
                let cold_blob_hash = lillux::cas::sha256_hex(&cold_blob);
                let cold_blob_len = u64::try_from(cold_blob.len()).ok();
                if cold.key != verified.key
                    || cold_blob_hash != verified.key.blob_hash
                    || cold_blob_len != Some(verified.key.blob_len)
                {
                    return Err(MaterializationError::MaterializationFailed {
                        executor_ref: probe.executor_ref.clone(),
                        detail: format!(
                            "verified-chain cache diagnostic diverged from cold verification (hot={:?}, cold={:?}, cold_blob_hash={cold_blob_hash}, cold_blob_len={cold_blob_len:?})",
                            verified.key, cold.key,
                        ),
                    });
                }
            }
            tracing::debug!(
                executor_ref = %probe.executor_ref,
                bundle_generation = %probe.bundle_generation_fingerprint,
                "native executor verified-chain cache hit"
            );
            Ok((verified, None))
        }
        ExecutorVerificationCacheLookup::Owner(flight) => {
            let (verified, blob_bytes) =
                verify_native_executor_chain(probe, bare, trust_store, launch_timings)?;
            Ok((flight.publish(verified), Some(blob_bytes)))
        }
        ExecutorVerificationCacheLookup::Bypass => {
            let (verified, blob_bytes) =
                verify_native_executor_chain(probe, bare, trust_store, launch_timings)?;
            Ok((Arc::new(verified), Some(blob_bytes)))
        }
    }
}

const EXECUTOR_CACHE_VERIFY_HITS_ENV: &str = "RYEOS_EXECUTOR_CACHE_VERIFY_HITS";
const EXECUTOR_STAT_PIN_ENV: &str = "RYEOS_EXECUTOR_STAT_PIN";

fn exact_env_opt_in(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

fn executor_cache_verify_hits_enabled() -> bool {
    exact_env_opt_in(std::env::var_os(EXECUTOR_CACHE_VERIFY_HITS_ENV).as_deref())
}

fn executor_stat_pin_fast_path_enabled_for(
    stat_pin: Option<&OsStr>,
    verify_hits: Option<&OsStr>,
) -> bool {
    exact_env_opt_in(stat_pin) && !exact_env_opt_in(verify_hits)
}

fn executor_stat_pin_fast_path_enabled() -> bool {
    let stat_pin = std::env::var_os(EXECUTOR_STAT_PIN_ENV);
    let verify_hits = std::env::var_os(EXECUTOR_CACHE_VERIFY_HITS_ENV);
    executor_stat_pin_fast_path_enabled_for(stat_pin.as_deref(), verify_hits.as_deref())
}

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExecutorPinKey {
    cache_root: PathBuf,
    blob_hash: String,
    bare: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecutorFileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    mode: u32,
    file_type: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecutorStatPin {
    identity: ExecutorFileIdentity,
    capture_granule_seconds: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutorStatPinState {
    Unpinned,
    Pinned(ExecutorStatPin),
    PermanentlyDisabled,
}

const EXECUTOR_STAT_PIN_MAX_ENTRIES: usize = 256;

#[derive(Default)]
struct ExecutorStatPinRegistryState {
    entries: HashMap<ExecutorPinKey, Arc<Mutex<ExecutorStatPinState>>>,
    /// Saturation is sticky. Once the bounded registry reaches its cap, every
    /// entry remains on the full-hash path for the daemon lifetime instead of
    /// evicting a permanent-disable decision.
    saturated: bool,
}

struct ExecutorStatPinRegistry {
    state: Mutex<ExecutorStatPinRegistryState>,
}

static EXECUTOR_STAT_PIN_REGISTRY: OnceLock<ExecutorStatPinRegistry> = OnceLock::new();

fn executor_stat_pin_registry() -> &'static ExecutorStatPinRegistry {
    EXECUTOR_STAT_PIN_REGISTRY.get_or_init(|| ExecutorStatPinRegistry {
        state: Mutex::new(ExecutorStatPinRegistryState::default()),
    })
}

impl ExecutorStatPinRegistry {
    fn entry(&self, key: ExecutorPinKey) -> Option<Arc<Mutex<ExecutorStatPinState>>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.saturated {
            return None;
        }
        if let Some(entry) = state.entries.get(&key) {
            return Some(Arc::clone(entry));
        }
        if state.entries.len() >= EXECUTOR_STAT_PIN_MAX_ENTRIES {
            state.saturated = true;
            let entries = state.entries.values().cloned().collect::<Vec<_>>();
            drop(state);
            for entry in entries {
                *entry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    ExecutorStatPinState::PermanentlyDisabled;
            }
            return None;
        }
        let entry = Arc::new(Mutex::new(ExecutorStatPinState::Unpinned));
        state.entries.insert(key, Arc::clone(&entry));
        Some(entry)
    }
}

struct ExecutorCacheLayout {
    cache_root: PathBuf,
    state_root: lillux::secure_fs::PinnedDirectory,
    cache: lillux::secure_fs::PinnedDirectory,
    executors: lillux::secure_fs::PinnedDirectory,
}

struct VerifiedOpenedExecutor {
    handle: Arc<std::fs::File>,
    identity: ExecutorFileIdentity,
}

fn validate_secure_cache_directory(
    directory: &lillux::secure_fs::PinnedDirectory,
    executor_ref: &str,
) -> Result<(), MaterializationError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = directory;
        return Err(MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: "descriptor-pinned native executor cache validation requires Linux".to_string(),
        });
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = std::fs::metadata(directory.descriptor_path().map_err(|error| {
            MaterializationError::MaterializationFailed {
                executor_ref: executor_ref.to_string(),
                detail: format!(
                    "failed to address pinned executor cache directory {}: {error}",
                    directory.path().display()
                ),
            }
        })?)
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!(
                "failed to inspect pinned executor cache directory {}: {error}",
                directory.path().display()
            ),
        })?;
        let daemon_uid = unsafe { libc::geteuid() };
        let mode = metadata.mode() & 0o7777;
        if metadata.uid() != daemon_uid || mode & 0o022 != 0 {
            return Err(MaterializationError::MaterializationFailed {
                executor_ref: executor_ref.to_string(),
                detail: format!(
                    "executor cache directory {} must be owned by daemon uid {} and not group/other writable (uid={}, mode={mode:#o})",
                    directory.path().display(),
                    daemon_uid,
                    metadata.uid(),
                ),
            });
        }
        Ok(())
    }
}

#[cfg(unix)]
fn executor_file_identity(metadata: &std::fs::Metadata) -> ExecutorFileIdentity {
    use std::os::unix::fs::MetadataExt as _;

    ExecutorFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        mode: metadata.mode(),
        file_type: metadata.mode() & libc::S_IFMT,
    }
}

fn reusable_executor_stat_pin(
    identity: ExecutorFileIdentity,
    capture_time: SystemTime,
) -> Option<ExecutorStatPin> {
    let capture_granule_seconds = capture_time
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())?;
    // Git-style racy-clean protection: a timestamp in the capture clock
    // second may not advance for a subsequent same-granule mutation.
    if identity.modified_seconds >= capture_granule_seconds
        || identity.changed_seconds >= capture_granule_seconds
    {
        return None;
    }
    Some(ExecutorStatPin {
        identity,
        capture_granule_seconds,
    })
}

fn remember_executor_stat_pin(
    state: Option<&Arc<Mutex<ExecutorStatPinState>>>,
    pin: Option<ExecutorStatPin>,
) {
    let Some(state) = state else {
        return;
    };
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *state != ExecutorStatPinState::PermanentlyDisabled {
        *state = pin.map_or(ExecutorStatPinState::Unpinned, ExecutorStatPinState::Pinned);
    }
}

fn executor_pin_state(
    layout: &ExecutorCacheLayout,
    verified: &VerifiedNativeExecutorChain,
    bare: &str,
) -> Option<Arc<Mutex<ExecutorStatPinState>>> {
    executor_stat_pin_registry().entry(ExecutorPinKey {
        cache_root: layout.cache_root.clone(),
        blob_hash: verified.key.blob_hash.clone(),
        bare: bare.to_string(),
    })
}

fn validate_executor_cache_ancestors(
    layout: &ExecutorCacheLayout,
    blob_dir: &lillux::secure_fs::PinnedDirectory,
    executor_ref: &str,
) -> Result<(), MaterializationError> {
    validate_secure_cache_directory(&layout.state_root, executor_ref)?;
    validate_secure_cache_directory(&layout.cache, executor_ref)?;
    validate_secure_cache_directory(&layout.executors, executor_ref)?;
    validate_secure_cache_directory(blob_dir, executor_ref)
}

fn open_executor_cache_layout(
    cache_root: &Path,
    executor_ref: &str,
) -> Result<ExecutorCacheLayout, MaterializationError> {
    let state_root =
        lillux::secure_fs::PinnedDirectory::open_or_create(cache_root).map_err(|error| {
            MaterializationError::MaterializationFailed {
                executor_ref: executor_ref.to_string(),
                detail: format!(
                    "failed to securely open executor cache root {}: {error}",
                    cache_root.display()
                ),
            }
        })?;
    let cache = state_root
        .open_or_create_child(OsStr::new("cache"), 0o700)
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!("failed to securely open executor cache directory: {error}"),
        })?;
    let executors = cache
        .open_or_create_child(OsStr::new("executors"), 0o700)
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!("failed to securely open native executor cache: {error}"),
        })?;
    // Security eligibility is checked only after the complete descriptor
    // hierarchy and per-entry pin key exist. That ordering lets any ancestor
    // anomaly permanently disable this logical entry instead of returning
    // before the daemon-lifetime disable decision can be recorded.
    Ok(ExecutorCacheLayout {
        cache_root: cache_root.to_path_buf(),
        state_root,
        cache,
        executors,
    })
}

enum MaterializedArtifactInspection {
    Valid(VerifiedOpenedExecutor),
    Missing,
    Invalid(String),
}

fn verify_opened_executor_file(
    mut file: std::fs::File,
    expected_hash: &str,
    expected_len: u64,
    expected_mode: u32,
    executor_ref: &str,
) -> Result<(VerifiedOpenedExecutor, Option<ExecutorStatPin>), String> {
    #[cfg(unix)]
    let before_identity = {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = file
            .metadata()
            .map_err(|error| format!("failed to inspect opened executor: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err("opened executor is not a regular file".to_string());
        }
        let daemon_uid = unsafe { libc::geteuid() };
        if metadata.uid() != daemon_uid {
            return Err(format!(
                "opened executor is owned by uid {}, expected daemon uid {daemon_uid}",
                metadata.uid()
            ));
        }
        let actual_mode = metadata.mode() & 0o7777;
        if actual_mode & !0o777 != 0 {
            return Err(format!(
                "opened executor has forbidden special permission bits ({actual_mode:#o})"
            ));
        }
        if actual_mode != expected_mode {
            return Err(format!(
                "opened executor has Unix mode {actual_mode:#o}, expected signed mode {expected_mode:#o}"
            ));
        }
        if actual_mode & 0o022 != 0 {
            return Err(format!(
                "opened executor has group/other writable mode {actual_mode:#o}"
            ));
        }
        if metadata.len() != expected_len {
            return Err(format!(
                "opened executor has length {}, expected signed blob length {expected_len}",
                metadata.len()
            ));
        }
        executor_file_identity(&metadata)
    };
    #[cfg(not(unix))]
    {
        let _ = (
            file,
            expected_hash,
            expected_len,
            expected_mode,
            executor_ref,
        );
        return Err("native executor Unix validation is unavailable on this platform".to_string());
    }

    let mut bytes = Vec::with_capacity(usize::try_from(expected_len).unwrap_or(0));
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read opened executor: {error}"))?;
    let actual_hash = lillux::cas::sha256_hex(&bytes);
    if actual_hash != expected_hash {
        return Err(format!(
            "opened executor failed its content-address check for {executor_ref}"
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let after_metadata = file
            .metadata()
            .map_err(|error| format!("failed to re-inspect opened executor: {error}"))?;
        let daemon_uid = unsafe { libc::geteuid() };
        if !after_metadata.file_type().is_file()
            || after_metadata.uid() != daemon_uid
            || after_metadata.mode() & 0o7777 != expected_mode
            || after_metadata.mode() & 0o022 != 0
            || after_metadata.len() != expected_len
        {
            return Err("opened executor security metadata changed while hashing".to_string());
        }
        let after_identity = executor_file_identity(&after_metadata);
        if before_identity != after_identity {
            return Err("opened executor identity changed while hashing".to_string());
        }
        let pin = reusable_executor_stat_pin(after_identity, SystemTime::now());
        Ok((
            VerifiedOpenedExecutor {
                handle: Arc::new(file),
                identity: after_identity,
            },
            pin,
        ))
    }
}

fn inspect_materialized_executor(
    layout: &ExecutorCacheLayout,
    verified: &VerifiedNativeExecutorChain,
    bare: &str,
) -> MaterializedArtifactInspection {
    inspect_materialized_executor_with_pin_policy(
        layout,
        verified,
        bare,
        executor_stat_pin_fast_path_enabled(),
    )
}

fn inspect_materialized_executor_with_pin_policy(
    layout: &ExecutorCacheLayout,
    verified: &VerifiedNativeExecutorChain,
    bare: &str,
    stat_pin_fast_path_enabled: bool,
) -> MaterializedArtifactInspection {
    let pin_state = executor_pin_state(layout, verified, bare);
    let mut pin_state_guard = pin_state.as_ref().map(|state| {
        state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    });
    let blob_dir = match layout
        .executors
        .open_child_directory(OsStr::new(&verified.key.blob_hash))
    {
        Ok(Some(directory)) => directory,
        Ok(None) => {
            return match layout
                .executors
                .open_entry(OsStr::new(&verified.key.blob_hash), false)
            {
                Ok(None) => MaterializedArtifactInspection::Missing,
                Ok(Some(_)) => {
                    if let Some(state) = pin_state_guard.as_deref_mut() {
                        *state = ExecutorStatPinState::PermanentlyDisabled;
                    }
                    MaterializedArtifactInspection::Invalid(
                        "content-addressed executor entry is not a directory".to_string(),
                    )
                }
                Err(error) => {
                    if let Some(state) = pin_state_guard.as_deref_mut() {
                        *state = ExecutorStatPinState::PermanentlyDisabled;
                    }
                    MaterializedArtifactInspection::Invalid(format!(
                        "content-addressed executor entry is malformed: {error}"
                    ))
                }
            }
        }
        Err(error) => {
            if let Some(state) = pin_state_guard.as_deref_mut() {
                *state = ExecutorStatPinState::PermanentlyDisabled;
            }
            return MaterializedArtifactInspection::Invalid(format!(
                "failed to securely open content-addressed executor directory: {error}"
            ));
        }
    };
    if let Err(error) = validate_executor_cache_ancestors(layout, &blob_dir, bare) {
        if let Some(state) = pin_state_guard.as_deref_mut() {
            *state = ExecutorStatPinState::PermanentlyDisabled;
        }
        return MaterializedArtifactInspection::Invalid(error.to_string());
    }
    let file = match blob_dir.open_regular(OsStr::new(bare), false) {
        Ok(Some(file)) => file,
        Ok(None) => {
            return MaterializedArtifactInspection::Invalid(
                "materialized executor file is missing".to_string(),
            )
        }
        Err(error) => {
            if let Some(state) = pin_state_guard.as_deref_mut() {
                *state = ExecutorStatPinState::PermanentlyDisabled;
            }
            return MaterializedArtifactInspection::Invalid(format!(
                "materialized executor is not a regular non-symlink file: {error}"
            ));
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                return MaterializedArtifactInspection::Invalid(format!(
                    "failed to stat materialized executor descriptor: {error}"
                ))
            }
        };
        let daemon_uid = unsafe { libc::geteuid() };
        let actual_mode = metadata.mode() & 0o7777;
        if !metadata.file_type().is_file()
            || metadata.uid() != daemon_uid
            || actual_mode & 0o022 != 0
        {
            if let Some(state) = pin_state_guard.as_deref_mut() {
                *state = ExecutorStatPinState::PermanentlyDisabled;
            }
            return MaterializedArtifactInspection::Invalid(format!(
                "materialized executor descriptor is not a daemon-owned, non-group/other-writable regular file (uid={}, mode={actual_mode:#o})",
                metadata.uid()
            ));
        }
        let observed = executor_file_identity(&metadata);
        if stat_pin_fast_path_enabled
            && pin_state_guard.as_deref().is_some_and(
                |state| matches!(state, ExecutorStatPinState::Pinned(pin) if pin.identity == observed),
            )
        {
            tracing::debug!(
                executor_ref = bare,
                device = observed.device,
                inode = observed.inode,
                "native executor materialized-file stat-pin hit"
            );
            return MaterializedArtifactInspection::Valid(VerifiedOpenedExecutor {
                handle: Arc::new(file),
                identity: observed,
            });
        }
    }
    match verify_opened_executor_file(
        file,
        &verified.key.blob_hash,
        verified.key.blob_len,
        verified.key.mode,
        bare,
    ) {
        Ok((opened, pin)) => {
            if let Some(state) = pin_state_guard.as_deref_mut() {
                if *state != ExecutorStatPinState::PermanentlyDisabled {
                    *state =
                        pin.map_or(ExecutorStatPinState::Unpinned, ExecutorStatPinState::Pinned);
                }
            }
            MaterializedArtifactInspection::Valid(opened)
        }
        Err(detail) => MaterializedArtifactInspection::Invalid(detail),
    }
}

enum QuarantinedExecutorEntry {
    Directory {
        name: String,
        directory: lillux::secure_fs::PinnedDirectory,
    },
    Other {
        name: String,
    },
}

impl QuarantinedExecutorEntry {
    fn remove(
        self,
        executors: &lillux::secure_fs::PinnedDirectory,
        executor_ref: &str,
    ) -> Result<(), MaterializationError> {
        match self {
            Self::Directory { name, directory } => {
                directory.remove_contents_recursive().map_err(|error| {
                    MaterializationError::MaterializationFailed {
                        executor_ref: executor_ref.to_string(),
                        detail: format!("failed to empty executor quarantine {name}: {error}"),
                    }
                })?;
                if !executors
                    .remove_empty_child_if_same(OsStr::new(&name), &directory)
                    .map_err(|error| MaterializationError::MaterializationFailed {
                        executor_ref: executor_ref.to_string(),
                        detail: format!("failed to remove executor quarantine {name}: {error}"),
                    })?
                {
                    return Err(MaterializationError::MaterializationFailed {
                        executor_ref: executor_ref.to_string(),
                        detail: format!("executor quarantine {name} remained non-empty"),
                    });
                }
            }
            Self::Other { name } => {
                let quarantine_path =
                    executors
                        .descriptor_child_path(OsStr::new(&name))
                        .map_err(|error| MaterializationError::MaterializationFailed {
                            executor_ref: executor_ref.to_string(),
                            detail: format!(
                                "failed to address executor quarantine {name}: {error}"
                            ),
                        })?;
                std::fs::remove_file(&quarantine_path).map_err(|error| {
                    MaterializationError::MaterializationFailed {
                        executor_ref: executor_ref.to_string(),
                        detail: format!("failed to remove executor quarantine {name}: {error}"),
                    }
                })?;
                executors.sync_tree().map_err(|error| {
                    MaterializationError::MaterializationFailed {
                        executor_ref: executor_ref.to_string(),
                        detail: format!(
                            "failed to durably remove executor quarantine {name}: {error}"
                        ),
                    }
                })?;
            }
        }
        Ok(())
    }
}

fn quarantine_materialized_executor(
    layout: &ExecutorCacheLayout,
    blob_hash: &str,
    executor_ref: &str,
) -> Result<Option<QuarantinedExecutorEntry>, MaterializationError> {
    let source = layout
        .executors
        .descriptor_child_path(OsStr::new(blob_hash))
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!("failed to address corrupt executor cache entry: {error}"),
        })?;
    match std::fs::symlink_metadata(&source) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(MaterializationError::MaterializationFailed {
                executor_ref: executor_ref.to_string(),
                detail: format!("failed to inspect corrupt executor cache entry: {error}"),
            })
        }
    }
    let quarantine_name = format!(
        ".quarantine.{blob_hash}.{}.{}",
        std::process::id(),
        rand::thread_rng().gen::<u64>()
    );
    let destination = layout
        .executors
        .descriptor_child_path(OsStr::new(&quarantine_name))
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!("failed to address executor quarantine: {error}"),
        })?;
    match lillux::rename_path_noreplace_durable(&source, &destination) {
        Ok(()) => {}
        Err(error) if error.namespace_committed() => {}
        Err(error) => {
            return Err(MaterializationError::MaterializationFailed {
                executor_ref: executor_ref.to_string(),
                detail: format!("failed to quarantine corrupt executor cache entry: {error}"),
            })
        }
    }
    let quarantined = match layout
        .executors
        .open_child_directory(OsStr::new(&quarantine_name))
    {
        Ok(Some(directory)) => QuarantinedExecutorEntry::Directory {
            name: quarantine_name,
            directory,
        },
        Ok(None) | Err(_) => QuarantinedExecutorEntry::Other {
            name: quarantine_name,
        },
    };
    Ok(Some(quarantined))
}

fn remove_staging_directory(
    executors: &lillux::secure_fs::PinnedDirectory,
    staging_name: &str,
    staging: &lillux::secure_fs::PinnedDirectory,
) -> anyhow::Result<()> {
    staging.remove_contents_recursive()?;
    if !executors.remove_empty_child_if_same(OsStr::new(staging_name), staging)? {
        anyhow::bail!("executor staging directory remained non-empty");
    }
    Ok(())
}

fn publish_verified_executor_blob(
    layout: &ExecutorCacheLayout,
    verified: &VerifiedNativeExecutorChain,
    bare: &str,
    blob_bytes: &[u8],
) -> Result<VerifiedOpenedExecutor, MaterializationError> {
    if lillux::cas::sha256_hex(blob_bytes) != verified.key.blob_hash
        || u64::try_from(blob_bytes.len()).ok() != Some(verified.key.blob_len)
    {
        return Err(MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: "verified executor bytes changed before cache publication".to_string(),
        });
    }
    let staging_name = format!(
        ".staging.{}.{}.{}",
        verified.key.blob_hash,
        std::process::id(),
        rand::thread_rng().gen::<u64>()
    );
    let staging = layout
        .executors
        .create_child(OsStr::new(&staging_name), 0o700)
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to create executor staging directory: {error}"),
        })?;
    validate_secure_cache_directory(&staging, bare)?;
    let mut staged_file = staging
        .open_regular_create(OsStr::new(bare), true, true, verified.key.mode)
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to create staged executor: {error}"),
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        staged_file
            .set_permissions(std::fs::Permissions::from_mode(verified.key.mode))
            .map_err(|error| MaterializationError::MaterializationFailed {
                executor_ref: bare.to_string(),
                detail: format!("failed to apply signed executor mode: {error}"),
            })?;
    }
    staged_file
        .write_all(blob_bytes)
        .and_then(|()| staged_file.sync_all())
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to write staged executor: {error}"),
        })?;
    drop(staged_file);
    let staged_file = staging
        .open_regular(OsStr::new(bare), false)
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to reopen staged executor: {error}"),
        })?
        .ok_or_else(|| MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: "staged executor disappeared before verification".to_string(),
        })?;
    let (verified_staged_file, pin) = verify_opened_executor_file(
        staged_file,
        &verified.key.blob_hash,
        verified.key.blob_len,
        verified.key.mode,
        bare,
    )
    .map_err(|detail| MaterializationError::MaterializationFailed {
        executor_ref: bare.to_string(),
        detail: format!("staged executor verification failed: {detail}"),
    })?;
    if let Err(error) = validate_executor_cache_ancestors(layout, &staging, bare) {
        let pin_state = executor_pin_state(layout, verified, bare);
        if let Some(pin_state) = pin_state {
            *pin_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                ExecutorStatPinState::PermanentlyDisabled;
        }
        return Err(error);
    }
    staging
        .sync_tree()
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to sync staged executor tree: {error}"),
        })?;

    match layout.executors.rename_child_directory_noreplace(
        OsStr::new(&staging_name),
        OsStr::new(&verified.key.blob_hash),
        &staging,
    ) {
        // The staged file was fully hashed through its open descriptor, and
        // this primitive proves the same pinned staging-directory inode was
        // moved without replacement. No second target-path read is needed on
        // the won branch.
        Ok(()) => {
            let pin_state = executor_pin_state(layout, verified, bare);
            remember_executor_stat_pin(pin_state.as_ref(), pin);
            Ok(verified_staged_file)
        }
        Err(error) => {
            if !error.namespace_committed() {
                let _ = remove_staging_directory(&layout.executors, &staging_name, &staging);
            }
            match inspect_materialized_executor(layout, verified, bare) {
                MaterializedArtifactInspection::Valid(opened) => {
                    tracing::debug!(
                        executor_ref = bare,
                        "native executor publish lost benign race; verified winner"
                    );
                    Ok(opened)
                }
                MaterializedArtifactInspection::Missing => {
                    Err(MaterializationError::MaterializationFailed {
                        executor_ref: bare.to_string(),
                        detail: format!(
                            "executor publication failed and no race winner exists: {error}"
                        ),
                    })
                }
                MaterializedArtifactInspection::Invalid(winner_error) => {
                    Err(MaterializationError::MaterializationFailed {
                        executor_ref: bare.to_string(),
                        detail: format!(
                            "executor publication failed and race winner was invalid: {error}; {winner_error}"
                        ),
                    })
                }
            }
        }
    }
}

fn repair_materialized_executor(
    layout: &ExecutorCacheLayout,
    mut verified: Arc<VerifiedNativeExecutorChain>,
    bare: &str,
    mut blob_bytes: Option<Vec<u8>>,
    probe: &ExecutorVerificationProbe,
    trust_store: &ryeos_engine::trust::TrustStore,
    launch_timings: Option<&ryeos_app::launch_stage_timings::LaunchStageTimings>,
) -> Result<(Arc<VerifiedNativeExecutorChain>, VerifiedOpenedExecutor), MaterializationError> {
    let namespace_lock = layout.executors.lock_exclusive().map_err(|error| {
        MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to lock executor cache namespace: {error}"),
        }
    })?;
    namespace_lock
        .ensure_protects(&layout.executors)
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("executor cache lock identity mismatch: {error}"),
        })?;
    if let MaterializedArtifactInspection::Valid(opened) =
        inspect_materialized_executor(layout, &verified, bare)
    {
        return Ok((verified, opened));
    }
    // Remove the suspect namespace entry before any expensive fallback work.
    // If chain verification or publication fails, the bad target remains
    // quarantined and therefore cannot be selected by a later launch.
    let quarantine = quarantine_materialized_executor(layout, &verified.key.blob_hash, bare)?;
    if blob_bytes.is_none() {
        // The namespace lock serializes repairs before the cache is invalidated,
        // so exactly one repairer performs this mandatory full-chain fallback.
        let refreshed =
            cached_or_verified_executor_chain(probe, bare, trust_store, true, launch_timings)?;
        verified = refreshed.0;
        blob_bytes = refreshed.1;
    }
    let blob_bytes =
        blob_bytes
            .as_deref()
            .ok_or_else(|| MaterializationError::MaterializationFailed {
                executor_ref: bare.to_string(),
                detail:
                    "single-flight full executor re-verification produced no trusted blob bytes"
                        .to_string(),
            })?;
    let opened = publish_verified_executor_blob(layout, &verified, bare, blob_bytes)?;
    if let Some(quarantine) = quarantine {
        quarantine.remove(&layout.executors, bare)?;
    }
    tracing::info!(
        executor_ref = bare,
        blob_hash = %verified.key.blob_hash,
        "native executor cache entry repaired from fully verified CAS chain"
    );
    Ok((verified, opened))
}

struct NativeExecutorMaterializationContext<'a> {
    bundle_roots: &'a [PathBuf],
    cache_root: &'a Path,
    trust_store: &'a ryeos_engine::trust::TrustStore,
    root_trust_class: ryeos_engine::resolution::TrustClass,
    bundle_generation_fingerprint: &'a str,
    node_trust_fingerprint: &'a str,
    launch_timings: Option<&'a ryeos_app::launch_stage_timings::LaunchStageTimings>,
}

fn materialize_native_executor_in_generation(
    executor_ref: &str,
    context: NativeExecutorMaterializationContext<'_>,
) -> Result<MaterializedExecutor, MaterializationError> {
    let NativeExecutorMaterializationContext {
        bundle_roots,
        cache_root,
        trust_store,
        root_trust_class,
        bundle_generation_fingerprint,
        node_trust_fingerprint,
        launch_timings,
    } = context;
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
    let manifest_probe_timer = launch_timings
        .map(|timings| timings.nested("background_dispatch", "executor_manifest_ref_probe"));
    let probe = manifest_ref_probe(
        bundle_roots,
        bundle_generation_fingerprint,
        node_trust_fingerprint,
        executor_ref,
        &triple,
        root_trust_class,
    )?;
    drop(manifest_probe_timer);
    let (mut verified, verified_blob_bytes) =
        cached_or_verified_executor_chain(&probe, bare, trust_store, false, launch_timings)?;
    let materialized_file_timer = launch_timings.map(|timings| {
        timings.nested(
            "background_dispatch",
            "executor_materialized_file_verify_or_repair",
        )
    });
    let layout = open_executor_cache_layout(cache_root, bare)?;
    let target_path = executor_cache_target(cache_root, &verified.key.blob_hash, bare);
    let opened = match inspect_materialized_executor(&layout, &verified, bare) {
        MaterializedArtifactInspection::Valid(opened) => {
            tracing::debug!(
                executor_ref,
                target = %target_path.display(),
                "native executor materialized-file identity verified"
            );
            opened
        }
        MaterializedArtifactInspection::Missing | MaterializedArtifactInspection::Invalid(_) => {
            let repaired = repair_materialized_executor(
                &layout,
                verified,
                bare,
                verified_blob_bytes,
                &probe,
                trust_store,
                launch_timings,
            )?;
            verified = repaired.0;
            repaired.1
        }
    };
    drop(materialized_file_timer);

    Ok(MaterializedExecutor {
        path: executor_cache_target(cache_root, &verified.key.blob_hash, bare),
        content_hash: verified.key.blob_hash.clone(),
        bundle_manifest_hash: verified.key.manifest_object_hash.clone(),
        bundle_signer_fingerprint: verified.key.signer_fingerprint.clone(),
        verified_command: ryeos_engine::isolation::IsolationDescriptorBoundCommand::new(
            ryeos_engine::isolation::IsolationVerifiedCode {
                source_path: executor_cache_target(cache_root, &verified.key.blob_hash, bare),
                content_hash: verified.key.blob_hash.clone(),
            },
            opened.handle,
            ryeos_engine::isolation::IsolationDescriptorFileIdentity {
                device: opened.identity.device,
                inode: opened.identity.inode,
                size: opened.identity.size,
                modified_seconds: opened.identity.modified_seconds,
                modified_nanoseconds: opened.identity.modified_nanoseconds,
                changed_seconds: opened.identity.changed_seconds,
                changed_nanoseconds: opened.identity.changed_nanoseconds,
                mode: opened.identity.mode,
                file_type: opened.identity.file_type,
            },
        ),
    })
}

/// Resolve and materialize an executor while holding the exact installed
/// bundle-generation guard owned by `engine`.
pub fn materialize_native_executor_for_engine(
    engine: &ryeos_engine::engine::Engine,
    bundle_roots: &[PathBuf],
    executor_ref: &str,
    cache_root: &Path,
    root_trust_class: ryeos_engine::resolution::TrustClass,
    launch_timings: Option<&ryeos_app::launch_stage_timings::LaunchStageTimings>,
) -> Result<MaterializedExecutor, MaterializationError> {
    engine.debug_assert_executor_cache_generation_identity();
    engine.with_checked_bundle_generation(|_| {
        if bundle_roots != engine.bundle_roots.as_slice() {
            return Err(MaterializationError::Internal(
                "executor verification requires the complete registered bundle-root generation"
                    .to_string(),
            ));
        }
        let bundle_generation_fingerprint = engine.registered_bundle_generation_fingerprint();
        let node_trust_fingerprint = engine.node_trust_store.fingerprint();
        materialize_native_executor_in_generation(
            executor_ref,
            NativeExecutorMaterializationContext {
                bundle_roots,
                cache_root,
                trust_store: &engine.node_trust_store,
                root_trust_class,
                bundle_generation_fingerprint: &bundle_generation_fingerprint,
                node_trust_fingerprint: &node_trust_fingerprint,
                launch_timings,
            },
        )
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
    pub lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    /// Optional request-local daemon timing trace. Observability only: this is
    /// neither persisted nor part of launch authority.
    pub launch_timings: Option<ryeos_app::launch_stage_timings::LaunchStageTimings>,
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
            body: crate::structured_error::dispatch_error_value(error),
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
    materialized_executor: MaterializedExecutor,
    checkpoint_dir: Option<PathBuf>,
    is_resume: bool,
    launch_metadata: Option<ryeos_app::launch_metadata::RuntimeLaunchMetadata>,
    pending_project_snapshot: Option<super::CapturedProjectGeneration>,
    augmentation_audits: Vec<crate::augmentations::LaunchAugmentationAudit>,
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
    augmentation_audits: &[crate::augmentations::LaunchAugmentationAudit],
) -> Result<Vec<ryeos_app::state_store::NewEventRecord>, BuildAndLaunchError> {
    let mut records = [
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
    .collect::<Vec<_>>();
    records.extend(augmentation_audits.iter().map(|audit| {
        ryeos_app::state_store::NewEventRecord {
            event_type: audit.event_type.as_str().to_owned(),
            storage_class: audit.event_type.storage_class().as_str().to_owned(),
            payload: audit.payload.clone(),
        }
    }));
    Ok(records)
}

async fn prepare_managed_launch_authority(
    params: &BuildAndLaunchParams<'_>,
    thread_id: &str,
    metadata_template: Option<&ryeos_app::launch_metadata::RuntimeLaunchMetadata>,
) -> Result<PreparedManagedLaunchAuthority, BuildAndLaunchError> {
    let engine = params.provenance.request_engine();
    let engine_roots = engine.resolution_roots(Some(params.project_path.to_path_buf()));
    let bundle_roots: Vec<PathBuf> = engine_roots
        .ordered
        .iter()
        .filter(|root| root.space == ryeos_engine::contracts::ItemSpace::Bundle)
        .map(|root| {
            root.ai_root
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.ai_root.clone())
        })
        .collect();
    let admitted_capsule = params
        .state
        .state_store
        .admitted_launch_capsule(thread_id)
        .map_err(BuildAndLaunchError::Internal)?;
    let effective_request_snapshot = if admitted_capsule.is_none() {
        Some(
            engine
                .effective_request_snapshot(Some(params.project_path))
                .map_err(|error| anyhow::anyhow!("effective request snapshot: {error}"))?,
        )
    } else {
        None
    };

    // Launch preparation begins from the exact admitted resolution closure.
    // Re-reading the canonical ref here would let an edit between admission
    // and spawn replace, reject, or otherwise reinterpret an already-admitted
    // program. Current trust/revocation policy may narrow the sealed authority,
    // but source bytes never substitute after this boundary.
    let mut resolution = params
        .resolved
        .root_admission
        .as_ref()
        .ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed launch is missing exact admitted resolution authority"
            ))
        })?
        .resolution_output()
        .clone();
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
    let runtime_binary =
        crate::dispatch::strip_binary_ref_prefix(&selected_runtime.yaml.binary_ref)
            .map_err(|error| BuildAndLaunchError::Internal(anyhow::anyhow!(error)))?;
    let executor_ref = format!("native:{runtime_binary}");
    let verified_protocol =
        crate::dispatch::require_callback_runtime_protocol(engine, &selected_runtime, "managed")
            .map_err(|error| BuildAndLaunchError::Internal(anyhow::anyhow!(error)))?;
    if selected_runtime.trust_class != ryeos_engine::resolution::TrustClass::TrustedBundle
        || verified_protocol.trust_class != ryeos_engine::resolution::TrustClass::TrustedBundle
    {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "managed runtime and protocol artifact identity requires installed TrustedBundle provenance"
        )));
    }

    // Parent executor verification/materialization does not consume the
    // augmentation projection. Start it on the blocking pool before polling
    // augmentation, then join both legs. Runtime preparation below reads
    // `resolution.composed.derived`, so it remains strictly post-join.
    let materialization_engine = (*engine).clone();
    let materialization_bundle_roots = bundle_roots.clone();
    let materialization_executor_ref = executor_ref.clone();
    let materialization_cache_root = params
        .state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state");
    let materialization_timings = params.launch_timings.clone();
    let materialization_queue_timer = materialization_timings.as_ref().map(|timings| {
        timings.nested(
            "background_dispatch",
            "executor_materialization_blocking_queue_wait",
        )
    });
    let materialization_handle = tokio::task::spawn_blocking(move || {
        drop(materialization_queue_timer);
        let _materialization_work_timer = materialization_timings.as_ref().map(|timings| {
            timings.nested(
                "background_dispatch",
                "executor_materialization_blocking_work",
            )
        });
        materialize_native_executor_for_engine(
            &materialization_engine,
            &materialization_bundle_roots,
            &materialization_executor_ref,
            &materialization_cache_root,
            ryeos_engine::resolution::TrustClass::TrustedBundle,
            materialization_timings.as_ref(),
        )
    });

    let augmentation = async {
        // Augmentation is part of the authoritative resolution, not a mutation
        // of already-audited launch state. Its internal worker is an
        // independent, lifecycle-guarded root, so the prospective managed
        // thread need not exist.
        if admitted_capsule.is_none() {
            let launching_kind_schema = engine
                .kinds
                .get(&params.resolved.resolved_item.kind)
                .ok_or_else(|| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "build_and_launch: launching kind `{}` is not registered",
                        params.resolved.resolved_item.kind
                    ))
                })?;
            if let Some(exec) = launching_kind_schema.execution() {
                if !exec.launch_augmentations.is_empty() {
                    let augmentation_timer = params.launch_timings.as_ref().map(|timings| {
                        timings.nested("background_dispatch", "launch_augmentation")
                    });
                    let audits = crate::augmentations::run_augmentations(
                        exec,
                        &mut resolution,
                        thread_id,
                        params.project_path,
                        engine,
                        params.provenance,
                        &params.resolved.plan_context,
                        params.acting_principal,
                        params.state,
                        params.launch_timings.as_ref(),
                    )
                    .await
                    .map_err(|error| {
                        BuildAndLaunchError::Internal(anyhow::anyhow!(
                            "launch augmentation failed: {error}"
                        ))
                    })?;
                    drop(augmentation_timer);
                    return Ok(audits);
                }
            }
        }
        Ok::<Vec<crate::augmentations::LaunchAugmentationAudit>, BuildAndLaunchError>(Vec::new())
    };
    let materialization = async {
        let materialized = materialization_handle
            .await
            .map_err(|error| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "executor materialization blocking worker failed: {error}"
                ))
            })?
            .map_err(BuildAndLaunchError::from)?;
        Ok::<MaterializedExecutor, BuildAndLaunchError>(materialized)
    };
    let (augmentation_result, materialization_result) = tokio::join!(augmentation, materialization);
    let concurrent_prerequisites_succeeded =
        augmentation_result.is_ok() && materialization_result.is_ok();
    let augmentation_audits = augmentation_result?;
    let materialized_executor = materialization_result?;
    debug_assert!(
        concurrent_prerequisites_succeeded,
        "runtime preparation must remain strictly after augmentation and executor materialization join"
    );

    if let Some(timings) = params.launch_timings.as_ref() {
        timings.mark("runtime_prep_started");
    }
    let prepared_launch = if let Some(capsule) = admitted_capsule.as_ref() {
        if capsule.launch_driver != ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed recovery found a non-managed admitted launch capsule"
            )));
        }
        serde_json::from_value::<super::launch_preparation::PreparedRuntimeLaunch>(
            capsule.prepared_launch.clone().ok_or_else(|| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "managed admitted launch capsule has no prepared launch authority"
                ))
            })?,
        )
        .map_err(|error| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "decode admitted prepared launch authority: {error}"
            ))
        })?
    } else {
        let runtime_preparation_timer = params
            .launch_timings
            .as_ref()
            .map(|timings| timings.nested("background_dispatch", "runtime_preparation"));
        let prepared = super::launch_preparation::prepare_runtime_launch(
            super::launch_preparation::PrepareRuntimeLaunchRequest {
                engine,
                runtime: &selected_runtime,
                primary: &resolution,
                ref_bindings: &params.resolved.ref_bindings,
                roots: &engine_roots,
                parsers: &effective_request_snapshot
                    .as_ref()
                    .ok_or_else(|| {
                        BuildAndLaunchError::Internal(anyhow::anyhow!(
                            "fresh managed launch has no parser authority"
                        ))
                    })?
                    .parser_dispatcher,
                trust_store: &effective_request_snapshot
                    .as_ref()
                    .ok_or_else(|| {
                        BuildAndLaunchError::Internal(anyhow::anyhow!(
                            "fresh managed launch has no trust authority"
                        ))
                    })?
                    .trust_store,
                principal: &params.resolved.plan_context.requested_by,
                ref_binding_resolution_timings: params.launch_timings.as_ref(),
            },
        )
        .map_err(BuildAndLaunchError::from);
        drop(runtime_preparation_timer);
        prepared?
    };
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
    let mut effective_caps = apply_capability_policy(
        composed_effective_caps,
        runtime_capability_caps,
        params.capability_policy,
        &params.resolved.item_ref,
        &child_execute_cap,
    )?;
    if let Some(capsule) = admitted_capsule.as_ref() {
        // Recovery may observe stricter current policy, but it may never mint
        // authority outside the exact capability ceiling rooted at admission.
        let admitted_ceiling: BTreeSet<&str> =
            capsule.effective_caps.iter().map(String::as_str).collect();
        effective_caps.retain(|capability| admitted_ceiling.contains(capability.as_str()));
    }
    let admitted_artifact_identity =
        ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
            runtime_ref: selected_runtime.canonical_ref.to_string(),
            runtime_content_hash: selected_runtime.raw_content_digest.clone(),
            runtime_signer_fingerprint: selected_runtime.signer_fingerprint.clone(),
            protocol_ref: verified_protocol.canonical_ref.clone(),
            protocol_content_hash: verified_protocol.raw_content_digest.clone(),
            protocol_signer_fingerprint: verified_protocol.signer_fingerprint.clone(),
            executor_ref: executor_ref.clone(),
            executor_content_hash: materialized_executor.content_hash.clone(),
            executor_bundle_manifest_hash: materialized_executor.bundle_manifest_hash.clone(),
            executor_bundle_signer_fingerprint: materialized_executor
                .bundle_signer_fingerprint
                .clone(),
        };
    admitted_artifact_identity
        .validate()
        .map_err(BuildAndLaunchError::Internal)?;
    if admitted_capsule.is_some() {
        params
            .state
            .state_store
            .verify_admitted_artifact_identity(thread_id, &admitted_artifact_identity)
            .map_err(BuildAndLaunchError::Internal)?;
    } else if let Some(persisted) = metadata_template {
        if let Some(expected) = persisted.admitted_artifact_identity.as_ref() {
            if expected != &admitted_artifact_identity {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "installed runtime/protocol/executor identity no longer matches the admitted launch capsule: admitted={expected:?}, installed={admitted_artifact_identity:?}"
                )));
            }
        }
    }
    // Credentials are read only after the installed runtime closure has been
    // matched to the authoritative capsule. A failed recovery attempt cannot
    // obtain secrets for substituted code.
    let mut secret_requirements = build_secret_requirements(params.metadata_required_secrets);
    merge_prepared_secret_requirements(
        &mut secret_requirements,
        &prepared_launch.required_secrets,
    )?;
    let secret_names: Vec<String> = secret_requirements
        .iter()
        .map(|requirement| requirement.name.clone())
        .collect();
    let effective_vault = ryeos_app::vault::read_required_secrets_with_authority(
        params.state.vault.as_ref(),
        params.acting_principal,
        &secret_names,
        params.provenance.project_authority(),
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
    let pending_project_snapshot: Option<super::CapturedProjectGeneration> = None;
    let launch_metadata = {
        let original_pushed_head_ref =
            ryeos_app::launch_metadata::OriginalPushedHeadRef::from_provenance(params.provenance);
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
        let project_authority = params.provenance.project_authority().clone();
        let local_overlay_root = matches!(
            project_authority.environment(),
            ryeos_state::objects::EnvironmentAuthority::ProjectOverlay { .. }
        )
        .then(|| params.provenance.original_project_path().to_path_buf());
        metadata = metadata
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime)
            .with_admitted_artifact_identity(admitted_artifact_identity)
            .with_admitted_prepared_launch(serde_json::to_value(&prepared_launch).map_err(
                |error| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "serialize admitted prepared launch: {error}"
                    ))
                },
            )?)
            .with_resume_context(ryeos_app::launch_metadata::ResumeContext {
                kind: params.resolved.kind.clone(),
                item_ref: params.resolved.item_ref.clone(),
                ref_bindings: params.resolved.ref_bindings.clone(),
                launch_mode: params.resolved.launch_mode.clone(),
                parameters: params.parameters.clone(),
                project_context: params.resolved.plan_context.project_context.clone(),
                project_authority,
                lifecycle_authority: params.lifecycle_authority,
                stable_project_identity,
                local_overlay_root,
                original_snapshot_hash: pending_project_snapshot
                    .as_ref()
                    .map(|publication| publication.snapshot_hash.clone())
                    .or_else(|| params.provenance.pinned_snapshot_hash().map(str::to_owned))
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
                parent_delegation_caps: metadata_template
                    .and_then(|template| template.resume_context.as_ref())
                    .and_then(|resume| resume.parent_delegation_caps.clone()),
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
    };
    Ok(PreparedManagedLaunchAuthority {
        resolution,
        prepared_launch,
        effective_vault,
        effective_caps,
        selected_runtime,
        materialized_executor,
        checkpoint_dir,
        is_resume,
        launch_metadata,
        pending_project_snapshot,
        augmentation_audits,
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
    if let Some(timings) = params.launch_timings.as_ref() {
        timings.bind_thread_id(&thread_id);
        timings.set_launch_dimensions(&params.resolved.resolved_item.kind, "managed_runtime");
    }
    if params.pre_minted_thread_id.is_some() {
        params
            .state
            .state_store
            .ensure_launch_planning_active(&thread_id)
            .map_err(|error| {
                map_launch_planning_check_error(error, &thread_id, "authoritative planning")
            })?;
    }
    let mut authority = prepare_managed_launch_authority(&params, &thread_id, None).await?;
    if params.pre_minted_thread_id.is_some() {
        params
            .state
            .state_store
            .ensure_launch_planning_active(&thread_id)
            .map_err(|error| {
                map_launch_planning_check_error(error, &thread_id, "irreversible thread handoff")
            })?;
    }
    let sealed_request =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::capture_with_resolution(
            params.resolved,
            authority.selected_runtime.canonical_ref.to_string(),
            authority.resolution.clone(),
        )?;
    authority
        .launch_metadata
        .get_or_insert_with(Default::default)
        .set_sealed_root_request(sealed_request);

    let initial_events = launch_audit_records(
        params.resolved,
        &authority.resolution,
        &authority.prepared_launch,
        &authority.augmentation_audits,
    )?;
    // Reserve the pre-minted ID before publishing the row. The reservation is
    // moved through the whole launch and drops automatically if creation or
    // preparation fails.
    let row_publication_timer = params
        .launch_timings
        .as_ref()
        .map(|timings| timings.nested("background_dispatch", "thread_row_publication"));
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
            )
            .map_err(|error| {
                map_launch_planning_check_error(
                    error,
                    &thread_id,
                    "authoritative thread publication",
                )
            })?,
        None => params
            .state
            .threads
            .create_root_thread_with_events_and_launch_metadata(
                &thread_id,
                params.resolved,
                authority
                    .launch_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.resume_context.as_ref())
                    .map(|resume| resume.project_authority.clone())
                    .ok_or_else(|| {
                        BuildAndLaunchError::Internal(anyhow::anyhow!(
                            "managed root launch has no sealed project authority"
                        ))
                    })?,
                initial_events,
                authority.launch_metadata.as_ref(),
            )
            .map_err(|error| {
                map_launch_planning_check_error(
                    error,
                    &thread_id,
                    "authoritative thread publication",
                )
            })?,
    };
    drop(row_publication_timer);
    if let Some(timings) = params.launch_timings.as_ref() {
        timings.record_nested_from_milestone(
            "background_dispatch",
            "runtime_prep_to_row_publication",
            "runtime_prep_started",
        );
        timings
            .record_top_level_from_milestone("background_dispatch", "background_dispatch_entered");
    }
    drop(authority.pending_project_snapshot.take());
    run_claimed_thread_row_with_authority(
        params,
        thread,
        authority,
        LaunchAuditDisposition::CommittedAtBirth,
    )
    .await
}

fn map_launch_planning_check_error(
    error: anyhow::Error,
    thread_id: &str,
    stage: &'static str,
) -> BuildAndLaunchError {
    if error
        .chain()
        .any(|cause| cause.is::<ryeos_app::state_store::LaunchPlanningInactive>())
    {
        BuildAndLaunchError::LaunchCancelled {
            thread_id: thread_id.to_string(),
            stage,
            detail: error.to_string(),
        }
    } else {
        BuildAndLaunchError::Internal(
            error.context(format!("read launch planning state during {stage}")),
        )
    }
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
    let persisted_metadata = params
        .state
        .state_store
        .get_launch_metadata(&thread.thread_id)?
        .ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "existing managed thread {} has no admitted launch metadata",
                thread.thread_id
            ))
        })?;
    // Existing-row paths (native resume/reconcile and rows created by their
    // dedicated lifecycle) must recompute launch authority for every attempt.
    // No persisted runtime data or admission output is accepted here.
    let authority = match prepare_managed_launch_authority(
        &params,
        &thread.thread_id,
        Some(&persisted_metadata),
    )
    .await
    {
        Ok(authority) => authority,
        Err(error) => {
            let terminal_error = match &error {
                BuildAndLaunchError::LaunchPreparation(dispatch_error) => {
                    crate::structured_error::dispatch_error_value(dispatch_error.as_ref())
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
        lifecycle_authority: _,
        launch_timings,
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
        materialized_executor: materialized_binary,
        checkpoint_dir,
        is_resume,
        launch_metadata: _,
        pending_project_snapshot,
        augmentation_audits,
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
    let effective_request_snapshot = engine
        .effective_request_snapshot(Some(project_path))
        .map_err(|e| anyhow::anyhow!("effective request snapshot: {e}"))?;

    // 2. Compute limits (root execution: depth = 0)
    let root_item_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(&resolved.item_ref)
        .map_err(|e| anyhow::anyhow!("build_and_launch: invalid root item ref: {e}"))?;
    let execution_policy = ryeos_engine::execution_policy::ExecutionPolicyResolver::new(
        ryeos_engine::config_loading::ConfigLoadContext {
            roots: &engine_roots,
            parsers: &effective_request_snapshot.parser_dispatcher,
            kinds: &engine.kinds,
            trust_store: &effective_request_snapshot.trust_store,
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
    // "compute effective limits"), once the composed header is available.
    // Execution-policy defaults are applied before that authored header;
    // explicit item policy and caller parameters are applied after it.
    // `hard_limits` is still produced before the TTL / envelope consumers
    // further down.

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
    // Execution defaults are fallbacks, not overrides of item-authored limits.
    // Explicit kind/item execution policy remains authoritative above the
    // authored item. Precedence:
    // limit defaults → execution defaults → header → item policy → caller
    // → caps → parent.
    let limits_header = resolution.composed.composed.get("limits");
    let execution_defaults =
        apply_execution_policy_defaults(&limits_config.defaults, &execution_policy);
    let base_limits = match limits_header {
        Some(v) if !v.is_null() => merge_header_limits(&execution_defaults, v)?,
        _ => execution_defaults,
    };
    let requested_limits = apply_execution_policy_item_overrides(&base_limits, &execution_policy);
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
    let header_has_limit = |field: &str| {
        limits_header
            .and_then(Value::as_object)
            .is_some_and(|limits| limits.contains_key(field))
    };
    let duration_source = if parameters.get("timeout").is_some() {
        "caller param `timeout`".to_string()
    } else if policy_item_override(execution_policy.timeout.as_ref()) {
        execution_policy
            .timeout
            .as_ref()
            .expect("item override checked above")
            .source
            .describe()
    } else if header_has_limit("duration_seconds") {
        "composed item `limits.duration_seconds`".to_string()
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
    } else if policy_item_override(execution_policy.max_steps.as_ref()) {
        execution_policy
            .max_steps
            .as_ref()
            .expect("item override checked above")
            .source
            .describe()
    } else if header_has_limit("turns") {
        "composed item `limits.turns`".to_string()
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
        header_limits_present = limits_header.is_some_and(|v| !v.is_null()),
        execution_policy_item_override = policy_item_override(execution_policy.timeout.as_ref())
            || policy_item_override(execution_policy.max_steps.as_ref()),
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
    if super::process_attachment::finalize_requested_stop_if_present(state, &thread_id)? {
        return Err(BuildAndLaunchError::LaunchCancelled {
            thread_id: thread_id.clone(),
            stage: "callback capability mint",
            detail: "durable stop intent won after authoritative thread creation".to_string(),
        });
    }
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
    if !state
        .callback_tokens
        .set_launch_owner(&cap.token, launch_owner.to_owned())
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
        &effective_request_snapshot.parser_dispatcher,
    )
    .map_err(|e| anyhow::anyhow!("inventory build failed: {e}"))?;

    // 7. The exact native executor was verified and materialized before birth,
    //    and its content identity is now part of the admitted capsule. Never
    //    perform a fresh name-based executor selection after that boundary.

    // Fresh roots and continuations committed this audit with `thread_created`
    // in their birth transaction. Existing-row retry/recovery paths append the
    // recomputed trio atomically before handoff.
    match launch_audit {
        LaunchAuditDisposition::CommittedAtBirth => {}
        LaunchAuditDisposition::AppendForAttempt => {
            let launch_audit = launch_audit_records(
                resolved,
                &resolution,
                &prepared_launch,
                &augmentation_audits,
            )?;
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
    // Process preparation, attachment, release, and result handling use
    // blocking process and pipe operations. Keep their owner on Tokio's
    // blocking pool so async workers remain free to service runtime UDS
    // callbacks.
    let isolation_verified_command = materialized_binary.verified_command;
    let materialized_binary_path = materialized_binary.path;
    let binary_path = materialized_binary_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("materialized runtime path is not valid UTF-8"))?
        .to_owned();
    // The ambient cache pathname is argv/provenance only. The exact no-follow
    // descriptor and its verified stat identity cross the isolation boundary
    // in `isolation_verified_command`.
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

    if super::process_attachment::finalize_requested_stop_if_present(state, &thread_id)? {
        return Err(BuildAndLaunchError::LaunchCancelled {
            thread_id: thread_id.clone(),
            stage: "thread credential mint",
            detail: "durable stop intent won after authoritative thread creation".to_string(),
        });
    }
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
    let isolation_live_access = provenance
        .isolation_live_access_authority()
        .map_err(BuildAndLaunchError::Internal)?;
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
    let launch_owner_owned = launch_owner.to_string();

    if super::process_attachment::finalize_requested_stop_if_present(state, &thread_id)? {
        return Err(BuildAndLaunchError::LaunchCancelled {
            thread_id: thread_id.clone(),
            stage: "isolation and runtime spawn",
            detail: "durable stop intent won after authoritative thread creation".to_string(),
        });
    }
    let spawn_handoff_timer = launch_timings
        .as_ref()
        .map(|timings| timings.top_level("spawn_scheduled_to_handoff"));
    let spawn_worker_total_timer = launch_timings
        .as_ref()
        .map(|timings| timings.top_level("runtime_spawn_worker"));
    let spawn_queue_timer = launch_timings
        .as_ref()
        .map(|timings| timings.nested("runtime_spawn_worker", "spawn_blocking_queue_wait"));
    let spawn_work_timings = launch_timings.clone();
    let spawn_handle = tokio::task::spawn_blocking(move || {
        let spawn_worker_total_timer = spawn_worker_total_timer;
        drop(spawn_queue_timer);
        let spawn_work_timer = spawn_work_timings
            .as_ref()
            .map(|timings| timings.nested("runtime_spawn_worker", "spawn_blocking_work"));
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
        let result = spawn_runtime(SpawnRuntimeParams {
            state: &state_for_spawn,
            descriptor: &descriptor_clone,
            item_ref: &runtime_item_ref,
            acting_principal: &acting_principal_owned,
            binary: &binary_path,
            project_path: &project_owned,
            project_authority: isolation_project_authority,
            live_access: isolation_live_access,
            state_root: isolation_state_root.as_deref(),
            workspace_lifeline: isolation_workspace_lifeline,
            envelope: &envelope,
            timeout_secs: duration,
            callback: &callback_owned,
            thread_id: &thread_id_owned,
            launch_owner: &launch_owner_owned,
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
        });
        drop(spawn_work_timer);
        drop(spawn_worker_total_timer);
        if let Some(timings) = spawn_work_timings.as_ref() {
            timings.emit("runtime_spawn_completed");
        }
        result
    });

    // The row and complete launch audit are durable, and the exact in-memory
    // authority (envelope runtime_data + resolved secret injection set) is now
    // owned by the scheduled spawn task. This is the acknowledgement boundary;
    // actual child start may race with network delivery by design.
    if let Some(handoff) = launch_handoff {
        handoff.publish(thread_id.clone());
    }
    drop(spawn_handoff_timer);
    if let Some(timings) = launch_timings.as_ref() {
        timings.mark("runtime_handoff_published");
        timings.emit("runtime_handoff_published");
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

/// Result of an owner-fenced permanent recovery refusal. The helper acquires
/// the exact launch claim before writing a terminal disposition, so a competing
/// launcher can win only by making this operation a benign `AlreadyClaimed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryRefusalOutcome {
    Finalized,
    AlreadyTerminal,
    AlreadyClaimed,
    PreservedForShutdown,
}

pub fn settle_recovery_preparation_refusal(
    state: &AppState,
    thread_id: &str,
    outcome_code: &str,
    stage: &str,
    message: &str,
) -> anyhow::Result<RecoveryRefusalOutcome> {
    let claim = match ThreadLaunchClaim::acquire(state, thread_id)? {
        ThreadLaunchClaimOutcome::Claimed(claim) => *claim,
        ThreadLaunchClaimOutcome::AlreadyClaimed => {
            return Ok(RecoveryRefusalOutcome::AlreadyClaimed)
        }
    };
    let launch_owner = claim.canonical_owner()?;
    let params = ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: "failed".to_string(),
        outcome_code: Some(outcome_code.to_string()),
        result: None,
        error: Some(json!({
            "code": outcome_code,
            "stage": stage,
            "message": message,
        })),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    };
    match state
        .threads
        .finalize_if_nonterminal_owned(&params, &launch_owner)?
    {
        ryeos_app::thread_lifecycle::FinalizeIfNonterminalOutcome::Finalized(thread) => {
            let chain_root_id = thread.chain_root_id.clone();
            kick_follow_resume_if_ready(state, &chain_root_id);
            kick_launch_window_for_terminal(state, &chain_root_id);
            Ok(RecoveryRefusalOutcome::Finalized)
        }
        ryeos_app::thread_lifecycle::FinalizeIfNonterminalOutcome::AlreadyTerminal { .. } => {
            Ok(RecoveryRefusalOutcome::AlreadyTerminal)
        }
        ryeos_app::thread_lifecycle::FinalizeIfNonterminalOutcome::PreservedForShutdown => {
            Ok(RecoveryRefusalOutcome::PreservedForShutdown)
        }
    }
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
    /// Fresh successors reserve their launch owner before the state-store
    /// birth makes the `created` row observable. The owned claim then crosses
    /// the daemon task queue inside this carrier, so live reconciliation can
    /// never mistake queue latency for an abandoned launch. Existing-row retry
    /// preparations leave this empty and claim the already-published row at
    /// launch time.
    launch_claim: Option<ThreadLaunchClaim>,
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
            &self.prepared.authority.augmentation_audits,
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
            &self.authority.augmentation_audits,
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
    let sealed_request = launch_metadata
        .sealed_root_request
        .as_ref()
        .ok_or_else(|| {
            anyhow::anyhow!("follow-child launch metadata has no sealed root request")
        })?;
    if sealed_request.project_context() != &resume.project_context
        || sealed_request.project_authority() != &resume.project_authority
        || sealed_request.project_authority() != provenance.project_authority()
    {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "follow-child sealed, resume, and reconstructed project identities disagree"
        )));
    }
    let admitted_request = sealed_request
        .restore_for_reconstructed_provenance(
            engine,
            &ryeos_app::launch_metadata::daemon_thread_state_dir(&state.config.app_root, thread_id)
                .join("launch-capsule"),
            &provenance,
        )
        .context("restore follow-child sealed root request")?;
    let mut operational_resume = resume.clone();
    operational_resume.project_context = admitted_request.plan_context.project_context.clone();
    if admitted_request.kind != operational_resume.kind
        || admitted_request.item_ref != operational_resume.item_ref
        || admitted_request.launch_mode != operational_resume.launch_mode
        || admitted_request.parameters != operational_resume.parameters
        || admitted_request.ref_bindings != operational_resume.ref_bindings
        || admitted_request.current_site_id != operational_resume.current_site_id
        || admitted_request.origin_site_id != operational_resume.origin_site_id
        || admitted_request.requested_by.as_deref()
            != Some(operational_resume.principal_identifier())
        || admitted_request.plan_context.requested_by != operational_resume.requested_by
        || admitted_request.plan_context.project_context != operational_resume.project_context
        || admitted_request.plan_context.execution_hints != operational_resume.execution_hints
        || operational_resume.executor_ref.as_deref()
            != Some(admitted_request.executor_ref.as_str())
        || operational_resume.runtime_ref.as_deref()
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
    let acting_principal = operational_resume.principal_identifier().to_string();
    let execution = crate::execution::runner::ExecutionParams {
        resolved: admitted_request,
        acting_principal,
        vault_bindings: HashMap::new(),
        parameters: resume.parameters.clone(),
        pre_minted_thread_id: None,
        effective_caps: operational_resume.effective_caps.clone(),
        provenance,
        lifecycle_authority: operational_resume.lifecycle_authority,
        runtime_ref: operational_resume.runtime_ref.clone(),
        parent_thread_id: None,
    };

    let project_path = execution.provenance.effective_path().to_path_buf();
    let authority = prepare_managed_launch_authority(
        &BuildAndLaunchParams {
            state,
            lifecycle_authority: resume.lifecycle_authority,
            launch_timings: None,
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
                parent_effective_caps: resume.parent_delegation_caps.as_deref().ok_or_else(
                    || {
                        anyhow::anyhow!(
                            "follow-child launch metadata has no parent delegation authority"
                        )
                    },
                )?,
            },
            checkpoint_resume_mode: CheckpointResumeMode::None,
            launch_handoff: None,
        },
        thread_id,
        Some(launch_metadata),
    )
    .await?;
    let (launch_metadata, prepared_resume) = if capture_project_snapshot {
        let mut prepared =
            authority.launch_metadata.as_ref().cloned().ok_or_else(|| {
                anyhow::anyhow!("follow-child authority produced no launch metadata")
            })?;
        let prepared_resume = prepared.resume_context.as_mut().ok_or_else(|| {
            anyhow::anyhow!("follow-child launch metadata lost its ResumeContext")
        })?;
        // The separately materialized launch workspace is operational only.
        // Persist the admission workspace named by the original sealed pair;
        // recovery reconstructs and transiently rebinds it from provenance.
        prepared_resume.project_context = resume.project_context.clone();
        let prepared_resume = prepared_resume.clone();
        prepared.set_sealed_root_request(sealed_request.clone());
        (prepared, prepared_resume)
    } else {
        // An existing child already has an immutable durable birth record.
        // Re-drive may re-materialize its workspace, but it must not rewrite
        // either persisted copy of that identity.
        (launch_metadata.clone(), resume.clone())
    };

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

impl PreparedMachineSuccessorLaunch {
    pub fn initial_audit_events(
        &self,
    ) -> Result<Vec<ryeos_app::state_store::NewEventRecord>, BuildAndLaunchError> {
        launch_audit_records(
            &self.prepared.execution.resolved,
            &self.prepared.authority.resolution,
            &self.prepared.authority.prepared_launch,
            &self.prepared.authority.augmentation_audits,
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
) -> Result<PreparedSuccessorLaunch, BuildAndLaunchError> {
    let sealed_request = metadata_template
        .and_then(|metadata| metadata.sealed_root_request.as_ref())
        .ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "continuation successor {successor_thread_id} has no exact admitted launch capsule"
            ))
        })?;
    let execution = crate::execution::runner::execution_params_from_sealed_root_request(
        state,
        successor_thread_id,
        resume,
        sealed_request,
        None,
    )?;
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
            lifecycle_authority: resume.lifecycle_authority,
            launch_timings: None,
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
        launch_claim: None,
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
    source_thread_id: &str,
) -> Result<PreparedOperatorSuccessorLaunch, BuildAndLaunchError> {
    let source_metadata = state
        .state_store
        .get_launch_metadata(source_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("source {source_thread_id} has no launch metadata"))?;
    let sealed = source_metadata
        .sealed_root_request
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("source {source_thread_id} has no admitted launch capsule"))?
        .for_continuation_invocation(resume)?;
    let successor_metadata = source_metadata
        .continuation_successor_seed(resume.clone())
        .with_continuation_source(source_thread_id)
        .with_sealed_root_request(sealed);
    let mut prepared = prepare_successor_launch(
        state,
        successor_thread_id,
        resume,
        SuccessorMode::Operator,
        Some(source_thread_id),
        Some(&successor_metadata),
    )
    .await?;
    prepared.launch_claim = Some(
        ThreadLaunchClaim::acquire_fresh(state, successor_thread_id)
            .map_err(BuildAndLaunchError::Internal)?,
    );
    Ok(PreparedOperatorSuccessorLaunch { prepared })
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
    let source_metadata = state
        .state_store
        .get_launch_metadata(source_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("source {source_thread_id} has no launch metadata"))?;
    let sealed = source_metadata
        .sealed_root_request
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("source {source_thread_id} has no admitted launch capsule"))?
        .for_continuation_invocation(resume)?;
    let successor_metadata = source_metadata
        .continuation_successor_seed(resume.clone())
        .with_continuation_source(source_thread_id)
        .with_sealed_root_request(sealed);
    let mut prepared = prepare_successor_launch(
        state,
        successor_thread_id,
        resume,
        SuccessorMode::Machine,
        Some(source_thread_id),
        Some(&successor_metadata),
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
    prepared.launch_claim = Some(
        ThreadLaunchClaim::acquire_fresh(state, successor_thread_id)
            .map_err(BuildAndLaunchError::Internal)?,
    );

    Ok(PreparedMachineSuccessorLaunch { prepared })
}

/// Launch a newly persisted operator successor with the exact authoritative
/// output computed before its row and ResumeContext were created.
pub async fn launch_prepared_operator_successor(
    state: AppState,
    successor_id: &str,
    mut prepared: PreparedOperatorSuccessorLaunch,
    launch_handoff: &LaunchHandoff,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    let prepared_claim = prepared.prepared.launch_claim.take();
    let result = launch_successor_inner_with_claim(
        state,
        successor_id,
        SuccessorMode::Operator,
        Some(launch_handoff),
        Some(prepared.prepared),
        prepared_claim,
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
    mut prepared: PreparedMachineSuccessorLaunch,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    let prepared_claim = prepared.prepared.launch_claim.take();
    launch_successor_inner_with_claim(
        state,
        successor_id,
        SuccessorMode::Machine,
        None,
        Some(prepared.prepared),
        prepared_claim,
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
    let launch_metadata = state
        .state_store
        .get_launch_metadata(&successor_id)?
        .ok_or_else(|| {
            anyhow::anyhow!("launch_successor: {successor_id} has no launch metadata")
        })?;
    let resume = launch_metadata.resume_context.clone().ok_or_else(|| {
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
        None => {
            let sealed = launch_metadata
                .sealed_root_request
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "launch_successor: {successor_id} has no sealed admitted request"
                    )
                })?;
            (
                crate::execution::runner::execution_params_from_sealed_root_request(
                    state,
                    &successor_id,
                    &resume,
                    sealed,
                    None,
                )?,
                None,
            )
        }
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
        lifecycle_authority: resume.lifecycle_authority,
        launch_timings: None,
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
    let launch_metadata = state
        .state_store
        .get_launch_metadata(&thread_id)?
        .ok_or_else(|| anyhow::anyhow!("native resume: {thread_id} has no launch metadata"))?;
    let resume = launch_metadata.resume_context.clone().ok_or_else(|| {
        anyhow::anyhow!("native resume: {thread_id} has no captured ResumeContext")
    })?;
    let sealed = launch_metadata
        .sealed_root_request
        .as_ref()
        .ok_or_else(|| {
            anyhow::anyhow!("native resume: {thread_id} has no sealed admitted request")
        })?;

    // Provenance selection (pushed-head rebuild / live-fs / loud refusal)
    // happens inside; working dir + runtime registry then follow the
    // provenance so the resumed run resolves against the pinned overlay
    // engine when the original spawn was pushed-head.
    let params = crate::execution::runner::execution_params_from_sealed_root_request(
        state, &thread_id, &resume, sealed, None,
    )?;
    let project_path = params.provenance.effective_path().to_path_buf();

    run_claimed_thread_row(
        BuildAndLaunchParams {
            state,
            lifecycle_authority: resume.lifecycle_authority,
            launch_timings: None,
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

/// Enqueue the first launch of a fresh root whose atomic birth committed before
/// its process attached. This consumes the sealed admitted request and never
/// sets checkpoint-resume semantics because the item has not executed yet.
pub fn prepare_and_spawn_admitted_root_recovery(
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
        let launch_owner = match claim.canonical_owner() {
            Ok(owner) => owner,
            Err(error) => {
                tracing::error!(thread_id = %thread_id, %error, "serialize admitted-root recovery owner");
                return;
            }
        };
        let launch_state = state.clone();
        match launch_admitted_root_with_claim(state, &thread_id, claim).await {
            Ok(SuccessorLaunchOutcome::Launched(_)) => {}
            Ok(SuccessorLaunchOutcome::Skipped(reason)) => tracing::debug!(
                thread_id = %thread_id,
                reason,
                "prepared admitted-root recovery skipped"
            ),
            Err(error) if error.retryable_launch_interruption() => tracing::warn!(
                thread_id = %thread_id,
                error = %error,
                "prepared admitted-root recovery hit a transient interruption; retaining created admission"
            ),
            Err(error) => {
                tracing::error!(
                    thread_id = %thread_id,
                    error = %error,
                    "prepared admitted-root recovery is permanently invalid; finalizing"
                );
                if let Err(finalize_error) = launch_state.threads.finalize_if_nonterminal_owned(
                    &ThreadFinalizeParams {
                        thread_id: thread_id.clone(),
                        status: "failed".to_string(),
                        outcome_code: Some("admitted_root_recovery_invalid".to_string()),
                        result: None,
                        error: Some(json!({
                            "code": "admitted_root_recovery_invalid",
                            "message": error.to_string(),
                        })),
                        metadata: None,
                        artifacts: Vec::new(),
                        final_cost: None,
                        summary_json: None,
                    },
                    &launch_owner,
                ) {
                    tracing::error!(
                        thread_id = %thread_id,
                        error = %finalize_error,
                        "failed to settle invalid admitted-root recovery"
                    );
                }
            }
        }
    });
    Ok(RecoveryLaunchOutcome::Enqueued)
}

async fn launch_admitted_root_with_claim(
    state: AppState,
    thread_id: &str,
    claim: ThreadLaunchClaim,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    let launch_owner = claim
        .canonical_owner()
        .map_err(BuildAndLaunchError::Internal)?;
    let thread = state
        .threads
        .get_thread(thread_id)?
        .ok_or_else(|| anyhow::anyhow!("admitted root not found: {thread_id}"))?;
    if thread.status != ryeos_state::objects::ThreadStatus::Created.as_str()
        || thread.upstream_thread_id.is_some()
    {
        return Ok(SuccessorLaunchOutcome::Skipped("not_fresh_created_root"));
    }
    if let Some(reason) = attached_identity_launch_blocker(&state, &thread)? {
        return Ok(SuccessorLaunchOutcome::Skipped(reason));
    }
    let metadata = state
        .state_store
        .get_launch_metadata(thread_id)?
        .ok_or_else(|| anyhow::anyhow!("admitted root {thread_id} has no launch metadata"))?;
    let resume = metadata
        .resume_context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("admitted root {thread_id} has no resume authority"))?;
    let sealed = metadata.sealed_root_request.as_ref().ok_or_else(|| {
        anyhow::anyhow!("admitted root {thread_id} has no sealed admitted request")
    })?;
    let execution = crate::execution::runner::execution_params_from_sealed_root_request(
        &state, thread_id, resume, sealed, None,
    )?;
    let project_path = execution.provenance.effective_path().to_path_buf();
    let chain_root_id = thread.chain_root_id.clone();
    let result = run_claimed_thread_row(
        BuildAndLaunchParams {
            state: &state,
            lifecycle_authority: resume.lifecycle_authority,
            launch_timings: None,
            runtime_ref: resume.runtime_ref.as_deref(),
            acting_principal: &execution.acting_principal,
            resolved: &execution.resolved,
            project_path: &project_path,
            provenance: &execution.provenance,
            parameters: &execution.parameters,
            metadata_required_secrets: &execution.resolved.resolved_item.metadata.required_secrets,
            pre_minted_thread_id: None,
            previous_thread_id: None,
            parent_execution_context: None,
            suppress_stimulus: false,
            capability_policy: CapabilityPolicy::ExactPinned(resume.effective_caps.as_slice()),
            checkpoint_resume_mode: CheckpointResumeMode::None,
            launch_handoff: None,
        },
        thread,
    )
    .await;
    match result {
        Ok(native) => Ok(SuccessorLaunchOutcome::Launched(native)),
        Err(error) => {
            if let Err(cleanup_error) = finalize_failed_and_kick_follow(
                &state,
                thread_id,
                &chain_root_id,
                &launch_owner,
                json!({ "error": error.to_string() }),
            ) {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "admitted root launch failed: {error}; terminal cleanup also failed: {cleanup_error}"
                )));
            }
            Err(error)
        }
    }
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
                &thread_id,
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

    let parent_effective_caps = identity.parent_delegation_caps.as_deref().ok_or_else(|| {
        anyhow::anyhow!("follow child {thread_id} has no parent delegation authority")
    })?;

    let launch_params = BuildAndLaunchParams {
        state,
        lifecycle_authority: identity.lifecycle_authority,
        launch_timings: None,
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
            state
                .state_store
                .clear_follow_waiter(follow_key)
                .map_err(BuildAndLaunchError::Internal)?;
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
        .enumerate()
        .map(|(index, envelope)| {
            let status = ryeos_runtime::envelope::follow_envelope_terminal_status(envelope)
                .map_err(|error| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "follow-resume: invalid terminal envelope at child index {index}: {error}"
                    ))
                })?;
            Ok(if status.is_success() {
                FanoutItemStatus::Completed
            } else {
                FanoutItemStatus::Failed
            })
        })
        .collect::<Result<Vec<_>, BuildAndLaunchError>>()?;
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
                    "child_thread_id": "T-follow-child-1",
                    "status": "completed",
                    "result": {"answer": 1},
                    "outputs": null,
                    "warnings": [],
                    "cost": null,
                }),
                json!({
                    "success": false,
                    "child_thread_id": "T-follow-child-2",
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
            "item_ref": item_ref.clone(),
            "content_blob_hash": blob_hash.clone(),
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

    struct ExecutableMaterializerFixture {
        fingerprint: String,
        blob_hash: String,
        blob_path: PathBuf,
        bytes: Vec<u8>,
    }

    fn write_executable_materializer_fixture(
        bundle_root: &Path,
        bare_name: &str,
        signing_key: &lillux::crypto::SigningKey,
    ) -> ExecutableMaterializerFixture {
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let ai_dir = bundle_root.join(ryeos_engine::AI_DIR);
        let objects_root = ai_dir.join("objects");
        let cas = lillux::cas::CasStore::new(objects_root.clone());
        let blob_hash = cas.store_blob(&bytes).unwrap();
        let item_ref = format!("bin/{}/{bare_name}", host_triple());
        let item_source = serde_json::json!({
            "kind": "item_source",
            "item_ref": item_ref.clone(),
            "content_blob_hash": blob_hash.clone(),
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
        ExecutableMaterializerFixture {
            fingerprint: lillux::signature::compute_fingerprint(&signing_key.verifying_key()),
            blob_path: lillux::cas::shard_path(&objects_root, "blobs", &blob_hash, ""),
            blob_hash,
            bytes,
        }
    }

    fn materializer_trust_store(
        fixture: &ExecutableMaterializerFixture,
        signing_key: &lillux::crypto::SigningKey,
    ) -> ryeos_engine::trust::TrustStore {
        ryeos_engine::trust::TrustStore::from_signers(vec![ryeos_engine::trust::TrustedSigner {
            fingerprint: fixture.fingerprint.clone(),
            verifying_key: signing_key.verifying_key(),
            label: None,
        }])
    }

    fn focused_test_generation_fingerprint(bundle_roots: &[PathBuf]) -> String {
        let mut generation_identity = Vec::new();
        for root in bundle_roots {
            generation_identity.extend_from_slice(root.as_os_str().as_encoded_bytes());
            generation_identity.push(0);
        }
        format!(
            "focused-test-generation:{}",
            lillux::cas::sha256_hex(&generation_identity)
        )
    }

    fn materialize_test_executor(
        bundle_roots: &[PathBuf],
        executor_ref: &str,
        cache_root: &Path,
        trust_store: &ryeos_engine::trust::TrustStore,
    ) -> Result<MaterializedExecutor, MaterializationError> {
        let bundle_generation_fingerprint = focused_test_generation_fingerprint(bundle_roots);
        let node_trust_fingerprint = trust_store.fingerprint();
        materialize_native_executor_in_generation(
            executor_ref,
            NativeExecutorMaterializationContext {
                bundle_roots,
                cache_root,
                trust_store,
                root_trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
                bundle_generation_fingerprint: &bundle_generation_fingerprint,
                node_trust_fingerprint: &node_trust_fingerprint,
                launch_timings: None,
            },
        )
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

        let error = materialize_test_executor(
            &[first.clone(), second.clone()],
            "native:shared-executor",
            tmp.path(),
            &trust_store,
        )
        .expect_err("root order must not select between duplicate executor identities");
        let message = error.to_string();
        assert!(message.contains("published by both"));
        assert!(message.contains(&first.display().to_string()));
        assert!(message.contains(&second.display().to_string()));
    }

    #[test]
    fn materializer_repairs_corrupt_target_from_a_fully_verified_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache_root = tmp.path().join("state");
        let key = lillux::crypto::SigningKey::from_bytes(&[72u8; 32]);
        let fixture = write_executable_materializer_fixture(&bundle, "repair-executor", &key);
        let trust_store = materializer_trust_store(&fixture, &key);
        let materialized = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:repair-executor",
            &cache_root,
            &trust_store,
        )
        .unwrap();
        std::fs::write(&materialized.path, b"corrupt materialized target").unwrap();

        let repaired = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:repair-executor",
            &cache_root,
            &trust_store,
        )
        .unwrap();

        assert_eq!(repaired.content_hash, fixture.blob_hash);
        assert_eq!(std::fs::read(&repaired.path).unwrap(), fixture.bytes);
        let executor_cache = cache_root.join("cache").join("executors");
        assert!(!std::fs::read_dir(executor_cache).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".quarantine.")
        }));
    }

    #[test]
    fn verified_chain_cache_hit_skips_redundant_cas_blob_read_and_reuses_pinned_target() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache_root = tmp.path().join("state");
        let key = lillux::crypto::SigningKey::from_bytes(&[74u8; 32]);
        let fixture = write_executable_materializer_fixture(&bundle, "cached-executor", &key);
        let trust_store = materializer_trust_store(&fixture, &key);
        let first = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:cached-executor",
            &cache_root,
            &trust_store,
        )
        .unwrap();
        std::fs::remove_file(&fixture.blob_path).unwrap();

        let second = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:cached-executor",
            &cache_root,
            &trust_store,
        )
        .expect("exact generation/trust/manifest identity may reuse the verified chain");

        assert_eq!(first.path, second.path);
        assert_eq!(std::fs::read(&second.path).unwrap(), fixture.bytes);
    }

    #[test]
    fn same_granule_executor_identity_is_never_pinned() {
        let capture_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let capture_seconds = i64::try_from(capture_seconds).unwrap();
        let identity = ExecutorFileIdentity {
            device: 1,
            inode: 2,
            size: 3,
            modified_seconds: capture_seconds,
            modified_nanoseconds: 0,
            changed_seconds: capture_seconds,
            changed_nanoseconds: 0,
            mode: libc::S_IFREG | 0o755,
            file_type: libc::S_IFREG,
        };
        assert!(reusable_executor_stat_pin(identity, SystemTime::now()).is_none());
    }

    #[test]
    fn stat_pin_fast_path_requires_current_opt_in_and_verify_hits_forces_full_verification() {
        let enabled = OsStr::new("1");
        let disabled = OsStr::new("0");
        let noncanonical = OsStr::new("true");

        assert!(!executor_stat_pin_fast_path_enabled_for(None, None));
        assert!(!executor_stat_pin_fast_path_enabled_for(
            Some(disabled),
            None
        ));
        assert!(!executor_stat_pin_fast_path_enabled_for(
            Some(noncanonical),
            None
        ));
        assert!(executor_stat_pin_fast_path_enabled_for(Some(enabled), None));
        assert!(!executor_stat_pin_fast_path_enabled_for(
            Some(enabled),
            Some(enabled)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn enabled_stat_pin_skips_unchanged_hash_and_mismatch_falls_back_to_tamper_rejection() {
        use std::io::Seek as _;
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache_root = tmp.path().join("state");
        let bare = "pin-branch-executor";
        let key = lillux::crypto::SigningKey::from_bytes(&[78u8; 32]);
        let fixture = write_executable_materializer_fixture(&bundle, bare, &key);
        let trust_store = materializer_trust_store(&fixture, &key);
        let materialized = materialize_test_executor(
            std::slice::from_ref(&bundle),
            &format!("native:{bare}"),
            &cache_root,
            &trust_store,
        )
        .unwrap();

        let bundle_roots = std::slice::from_ref(&bundle);
        let generation = focused_test_generation_fingerprint(bundle_roots);
        let trust_fingerprint = trust_store.fingerprint();
        let probe = manifest_ref_probe(
            bundle_roots,
            &generation,
            &trust_fingerprint,
            &format!("native:{bare}"),
            &host_triple(),
            ryeos_engine::resolution::TrustClass::TrustedBundle,
        )
        .unwrap();
        let (verified, _) = verify_native_executor_chain(&probe, bare, &trust_store, None).unwrap();
        let layout = open_executor_cache_layout(&cache_root, bare).unwrap();
        let original_identity =
            executor_file_identity(&std::fs::metadata(&materialized.path).unwrap());
        let pin_state = executor_pin_state(&layout, &verified, bare).unwrap();
        let seed_original_pin = || {
            *pin_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                ExecutorStatPinState::Pinned(ExecutorStatPin {
                    identity: original_identity,
                    capture_granule_seconds: original_identity.changed_seconds.saturating_add(1),
                });
        };

        seed_original_pin();
        let pinned =
            match inspect_materialized_executor_with_pin_policy(&layout, &verified, bare, true) {
                MaterializedArtifactInspection::Valid(opened) => opened,
                MaterializedArtifactInspection::Missing => {
                    panic!("materialized executor disappeared before pin inspection")
                }
                MaterializedArtifactInspection::Invalid(detail) => {
                    panic!("unchanged pinned executor was rejected: {detail}")
                }
            };
        let mut pinned_handle = pinned.handle.try_clone().unwrap();
        assert_eq!(
            pinned_handle.stream_position().unwrap(),
            0,
            "stat-pin hit must return before the full-file read"
        );
        drop(pinned_handle);
        drop(pinned);

        seed_original_pin();
        let fully_verified =
            match inspect_materialized_executor_with_pin_policy(&layout, &verified, bare, false) {
                MaterializedArtifactInspection::Valid(opened) => opened,
                MaterializedArtifactInspection::Missing => {
                    panic!("materialized executor disappeared before full verification")
                }
                MaterializedArtifactInspection::Invalid(detail) => {
                    panic!("valid executor failed full verification: {detail}")
                }
            };
        let mut fully_verified_handle = fully_verified.handle.try_clone().unwrap();
        assert_eq!(
            fully_verified_handle.stream_position().unwrap(),
            u64::try_from(fixture.bytes.len()).unwrap(),
            "disabled pin policy must consume the complete file hash"
        );
        drop(fully_verified_handle);
        drop(fully_verified);

        seed_original_pin();
        let mut corrupt = fixture.bytes.clone();
        corrupt[0] ^= 0xff;
        let replacement = materialized.path.with_extension("tampered");
        std::fs::write(&replacement, &corrupt).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&replacement, &materialized.path).unwrap();
        let replacement_identity =
            executor_file_identity(&std::fs::metadata(&materialized.path).unwrap());
        assert_ne!(replacement_identity, original_identity);

        match inspect_materialized_executor_with_pin_policy(&layout, &verified, bare, true) {
            MaterializedArtifactInspection::Invalid(detail) => {
                assert!(detail.contains("content-addressed check"));
            }
            MaterializedArtifactInspection::Missing => {
                panic!("tampered replacement disappeared before inspection")
            }
            MaterializedArtifactInspection::Valid(_) => {
                panic!("pin identity mismatch must fall back to hash and reject tampered bytes")
            }
        }
    }

    #[test]
    fn bounded_pin_registry_saturation_permanently_disables_all_fast_paths() {
        let registry = ExecutorStatPinRegistry {
            state: Mutex::new(ExecutorStatPinRegistryState::default()),
        };
        let key = |index: usize| ExecutorPinKey {
            cache_root: PathBuf::from("/test/executor-cache"),
            blob_hash: format!("{index:064x}"),
            bare: format!("executor-{index}"),
        };
        let first = registry.entry(key(0)).unwrap();
        for index in 1..EXECUTOR_STAT_PIN_MAX_ENTRIES {
            assert!(registry.entry(key(index)).is_some());
        }
        assert!(registry.entry(key(EXECUTOR_STAT_PIN_MAX_ENTRIES)).is_none());
        assert_eq!(
            *first.lock().unwrap(),
            ExecutorStatPinState::PermanentlyDisabled
        );
        assert!(registry.entry(key(0)).is_none());
        assert!(registry
            .entry(key(EXECUTOR_STAT_PIN_MAX_ENTRIES + 1))
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn weak_cache_directory_permissions_permanently_disable_entry_pin() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache_root = tmp.path().join("state");
        let key = lillux::crypto::SigningKey::from_bytes(&[77u8; 32]);
        let fixture = write_executable_materializer_fixture(&bundle, "weak-cache-executor", &key);
        let trust_store = materializer_trust_store(&fixture, &key);
        let materialized = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:weak-cache-executor",
            &cache_root,
            &trust_store,
        )
        .unwrap();
        let blob_dir = materialized.path.parent().unwrap();
        std::fs::set_permissions(blob_dir, std::fs::Permissions::from_mode(0o777)).unwrap();

        let repaired = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:weak-cache-executor",
            &cache_root,
            &trust_store,
        )
        .unwrap();
        assert_eq!(std::fs::read(&repaired.path).unwrap(), fixture.bytes);
        let layout = open_executor_cache_layout(&cache_root, "weak-cache-executor").unwrap();
        let state = executor_stat_pin_registry()
            .entry(ExecutorPinKey {
                cache_root: layout.cache_root.clone(),
                blob_hash: fixture.blob_hash,
                bare: "weak-cache-executor".to_string(),
            })
            .unwrap();
        assert_eq!(
            *state.lock().unwrap(),
            ExecutorStatPinState::PermanentlyDisabled
        );
    }

    #[cfg(unix)]
    #[test]
    fn full_hash_detects_same_size_rewrite_with_restored_mtime() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::MetadataExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache_root = tmp.path().join("state");
        let key = lillux::crypto::SigningKey::from_bytes(&[75u8; 32]);
        let fixture = write_executable_materializer_fixture(&bundle, "ctime-executor", &key);
        let trust_store = materializer_trust_store(&fixture, &key);
        let first = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:ctime-executor",
            &cache_root,
            &trust_store,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let pinned = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:ctime-executor",
            &cache_root,
            &trust_store,
        )
        .unwrap();
        let before = std::fs::metadata(&pinned.path).unwrap();
        let mut corrupt = fixture.bytes.clone();
        corrupt[0] ^= 0xff;
        std::fs::write(&pinned.path, &corrupt).unwrap();
        let path = std::ffi::CString::new(pinned.path.as_os_str().as_bytes()).unwrap();
        let times = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            libc::timespec {
                tv_sec: before.mtime(),
                tv_nsec: before.mtime_nsec(),
            },
        ];
        assert_eq!(
            unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) },
            0
        );
        let tampered = std::fs::metadata(&pinned.path).unwrap();
        assert_eq!(tampered.len(), before.len());
        assert_eq!(tampered.mtime(), before.mtime());
        assert_eq!(tampered.mtime_nsec(), before.mtime_nsec());
        assert_ne!(
            (tampered.ctime(), tampered.ctime_nsec()),
            (before.ctime(), before.ctime_nsec())
        );

        let repaired = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:ctime-executor",
            &cache_root,
            &trust_store,
        )
        .unwrap();
        assert_eq!(std::fs::read(&repaired.path).unwrap(), fixture.bytes);
        assert_eq!(
            first.verified_command.identity().content_hash,
            repaired.verified_command.identity().content_hash
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialized_descriptor_survives_path_substitution_without_inode_rebinding() {
        use std::io::{Read as _, Seek as _};
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache_root = tmp.path().join("state");
        let key = lillux::crypto::SigningKey::from_bytes(&[76u8; 32]);
        let fixture = write_executable_materializer_fixture(&bundle, "descriptor-executor", &key);
        let trust_store = materializer_trust_store(&fixture, &key);
        let materialized = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:descriptor-executor",
            &cache_root,
            &trust_store,
        )
        .unwrap();
        let held_inode = materialized
            .verified_command
            .executable()
            .metadata()
            .unwrap()
            .ino();
        let displaced = materialized.path.with_extension("displaced");
        std::fs::rename(&materialized.path, &displaced).unwrap();
        std::fs::write(&materialized.path, vec![0u8; fixture.bytes.len()]).unwrap();
        std::fs::set_permissions(&materialized.path, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_ne!(
            std::fs::metadata(&materialized.path).unwrap().ino(),
            held_inode
        );

        let mut exact = materialized
            .verified_command
            .executable()
            .try_clone()
            .unwrap();
        exact.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        exact.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, fixture.bytes);
        assert_eq!(exact.metadata().unwrap().ino(), held_inode);
    }

    #[test]
    fn materializer_quarantines_bad_target_when_full_chain_repair_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache_root = tmp.path().join("state");
        let key = lillux::crypto::SigningKey::from_bytes(&[73u8; 32]);
        let fixture =
            write_executable_materializer_fixture(&bundle, "failed-repair-executor", &key);
        let trust_store = materializer_trust_store(&fixture, &key);
        let materialized = materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:failed-repair-executor",
            &cache_root,
            &trust_store,
        )
        .unwrap();
        std::fs::write(&materialized.path, b"corrupt materialized target").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&fixture.blob_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        std::fs::write(&fixture.blob_path, b"corrupt CAS blob").unwrap();

        materialize_test_executor(
            std::slice::from_ref(&bundle),
            "native:failed-repair-executor",
            &cache_root,
            &trust_store,
        )
        .expect_err("corrupt CAS bytes must prevent repair");

        assert!(!materialized.path.exists());
        let executor_cache = cache_root.join("cache").join("executors");
        assert!(std::fs::read_dir(executor_cache).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".quarantine.")
        }));
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
    fn planning_check_only_maps_the_typed_inactive_marker_to_cancellation() {
        let cancelled = map_launch_planning_check_error(
            ryeos_app::state_store::LaunchPlanningInactive.into(),
            "T-internal",
            "authoritative thread publication",
        );
        assert!(matches!(cancelled, BuildAndLaunchError::LaunchCancelled {
            thread_id,
            stage: "authoritative thread publication",
            ..
        } if thread_id == "T-internal"));

        let internal = map_launch_planning_check_error(
            anyhow::anyhow!("runtime database unavailable"),
            "T-internal",
            "authoritative thread publication",
        );
        assert!(matches!(internal, BuildAndLaunchError::Internal(_)));
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
