use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context as _, Result};
use rand::Rng;
use serde_json::{Value, json};

use super::arch_check;
use super::launch_claim::{ThreadLaunchClaim, ThreadLaunchClaimOutcome};
use super::launch_envelope::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, HardLimits, LaunchEnvelope,
    LaunchEnvelopeBuilder, RuntimeResult,
};

use super::limits::{
    LimitsConfigSnapshot, apply_caller_limit_overrides, apply_execution_policy_defaults,
    apply_execution_policy_item_overrides, compute_effective_limits, load_limits_config_snapshot,
    load_limits_config_snapshot_under_project_authority, merge_header_limits, policy_item_override,
};
use super::thread_meta::ThreadMeta;
use crate::dispatch_error::DispatchError;
use ryeos_app::callback_token::{
    HookDispatchAuthorization, effective_bundle_id_for_request, launch_token_ttl,
};
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::{ResolvedExecutionRequest, ThreadFinalizeParams};
use ryeos_app::vault::VaultReadError;
use ryeos_runtime::RuntimeJsonArrayBudget;
use ryeos_runtime::checkpoint::{
    FanoutItemStatus, checkpoint_shape_limits, validate_checkpoint_shape,
};
use ryeos_runtime::events::RuntimeEventType;

mod runtime_request;
mod terminal;

use runtime_request::{SpawnRuntimeParams, spawn_runtime};
use terminal::{
    fallback_finalization, is_thread_terminal_status, reconcile_terminal_finalization,
    runtime_terminal_status,
};

const MAX_SIGNED_EXECUTOR_MANIFEST_REF_BYTES: u64 = 256 * 1024;
const MAX_EXECUTOR_MANIFEST_OBJECT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EXECUTOR_ITEM_SOURCE_OBJECT_BYTES: u64 = 1024 * 1024;
const MAX_NATIVE_EXECUTOR_BYTES: u64 = 512 * 1024 * 1024;

const fn native_executor_size_is_admissible(bytes: u64) -> bool {
    bytes <= MAX_NATIVE_EXECUTOR_BYTES
}

/// Typed error for native executor materialization failures.
///
/// Raised by [`materialize_native_executor_for_engine`] when the bundle CAS
/// cannot supply the requested binary. The daemon's `dispatch.rs`
/// maps this to `DispatchError::RuntimeMaterializationFailed` with
/// a 502 status — no string-classifier anywhere.
#[derive(Debug, thiserror::Error)]
pub enum MaterializationError {
    #[error("{0}")]
    Shared(Arc<MaterializationError>),
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
    #[error(
        "native executor resource limit reached for {resource}: requested {requested}, \
         available {available}, limit {limit}"
    )]
    ResourceLimit {
        resource: &'static str,
        requested: u64,
        available: u64,
        limit: u64,
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

impl From<ryeos_engine::error::EngineError> for BuildAndLaunchError {
    fn from(error: ryeos_engine::error::EngineError) -> Self {
        // A signed kind validator rejecting the composed definition is an
        // authoring error in launched content, not an internal fault.
        if let ryeos_engine::error::EngineError::EffectiveValidationRejected {
            canonical_ref,
            code,
            message,
        } = &error
        {
            return Self::LaunchPreparation(Box::new(DispatchError::LaunchPreparationFailed {
                code: format!("effective_validation_rejected:{code}"),
                message: format!("effective validator rejected `{canonical_ref}`: {message}"),
                classification: "configuration".to_owned(),
                binding: None,
                details: Box::new(std::collections::BTreeMap::new()),
            }));
        }
        Self::Internal(anyhow::anyhow!(error))
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
const EXECUTOR_VERIFICATION_MAX_RESIDENT_BLOB_BYTES: u64 = 1024 * 1024 * 1024;

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

/// Opaque proof of the exact signed native-executor chain selected across the
/// complete registered bundle-root generation.
///
/// Callers may bind a cache key to its digest, but cannot construct or alter
/// the underlying verified chain. Materialization can therefore happen only
/// on a cache miss without reducing an early hit to registry `binary_ref`
/// metadata.
#[derive(Clone)]
pub struct VerifiedExecutorChainAttestation {
    verified: Arc<VerifiedNativeExecutorChain>,
}

impl std::fmt::Debug for VerifiedExecutorChainAttestation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedExecutorChainAttestation")
            .field("executor_ref", &self.verified.key.probe.executor_ref)
            .field("host_triple", &self.verified.key.probe.host_triple)
            .field("blob_hash", &self.verified.key.blob_hash)
            .finish_non_exhaustive()
    }
}

impl VerifiedExecutorChainAttestation {
    pub fn identity_digest(&self) -> Result<String, MaterializationError> {
        let key = &self.verified.key;
        let selected_root_index = key
            .probe
            .manifest_refs
            .iter()
            .position(|candidate| candidate.bundle_root == key.bundle_root)
            .ok_or_else(|| {
                MaterializationError::Internal(
                    "verified executor root is absent from its ambiguity proof".to_string(),
                )
            })?;
        let value = serde_json::json!({
            "schema_version": 1,
            "bundle_generation_fingerprint": &key.probe.bundle_generation_fingerprint,
            "node_trust_fingerprint": &key.probe.node_trust_fingerprint,
            "root_trust_class": key.probe.root_trust_class,
            "host_triple": &key.probe.host_triple,
            "executor_ref": &key.probe.executor_ref,
            "eligible_roots": key.probe.manifest_refs.iter().enumerate().map(|(index, candidate)| {
                serde_json::json!({
                    "index": index,
                    "cas_ready": candidate.cas_ready,
                    "signed_ref_digest": &candidate.signed_ref_digest,
                })
            }).collect::<Vec<_>>(),
            "selected_root_index": selected_root_index,
            "signed_manifest_ref_digest": &key.signed_manifest_ref_digest,
            "manifest_object_hash": &key.manifest_object_hash,
            "item_source_object_hash": &key.item_source_object_hash,
            "blob_hash": &key.blob_hash,
            "blob_len": key.blob_len,
            "mode": key.mode,
            "signer_fingerprint": &key.signer_fingerprint,
        });
        let canonical = lillux::canonical_json(&value)
            .map_err(|error| MaterializationError::Internal(error.to_string()))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    pub fn matches_materialized(&self, materialized: &MaterializedExecutor) -> bool {
        let key = &self.verified.key;
        let command_identity = materialized.verified_command.identity();
        let descriptor_identity = materialized.verified_command.file_identity();
        materialized.content_hash == key.blob_hash
            && materialized.bundle_manifest_hash == key.manifest_object_hash
            && materialized.bundle_signer_fingerprint == key.signer_fingerprint
            && command_identity.source_path == materialized.path
            && command_identity.content_hash == key.blob_hash
            && lillux::matches_regular_file_identity(
                descriptor_identity.size,
                descriptor_identity.mode,
                descriptor_identity.file_type,
                key.blob_len,
                key.mode,
            )
    }
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
    in_flight: HashMap<ExecutorVerificationProbe, Arc<PendingExecutorVerification>>,
    tick: u64,
    metadata_bytes: usize,
}

struct ExecutorVerificationCache {
    state: Mutex<ExecutorVerificationCacheState>,
    blob_budget: Arc<ExecutorVerificationBlobBudget>,
}

static EXECUTOR_VERIFICATION_CACHE: OnceLock<ExecutorVerificationCache> = OnceLock::new();

fn executor_verification_cache() -> &'static ExecutorVerificationCache {
    EXECUTOR_VERIFICATION_CACHE.get_or_init(|| ExecutorVerificationCache {
        state: Mutex::new(ExecutorVerificationCacheState::default()),
        blob_budget: Arc::new(ExecutorVerificationBlobBudget::default()),
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
    if let Some(key) = state.by_probe.remove(probe)
        && let Some(entry) = state.entries.remove(&key)
    {
        state.metadata_bytes = state.metadata_bytes.saturating_sub(entry.metadata_bytes);
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
    Wait(Arc<PendingExecutorVerification>),
    Owner(ExecutorVerificationFlight),
    Saturated,
}

struct ExecutorVerificationBlob {
    bytes: Vec<u8>,
    reserved_bytes: u64,
    budget: Arc<ExecutorVerificationBlobBudget>,
}

impl ExecutorVerificationBlob {
    fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for ExecutorVerificationBlob {
    fn drop(&mut self) {
        self.budget.release(self.reserved_bytes);
    }
}

type ExecutorVerificationResult = Result<
    (
        Arc<VerifiedNativeExecutorChain>,
        Arc<ExecutorVerificationBlob>,
    ),
    Arc<MaterializationError>,
>;

struct PendingExecutorVerification {
    result: Mutex<Option<ExecutorVerificationResult>>,
    ready: Condvar,
}

impl Default for PendingExecutorVerification {
    fn default() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }
}

struct ExecutorVerificationFlight {
    probe: ExecutorVerificationProbe,
    pending: Arc<PendingExecutorVerification>,
    blob_budget: Arc<ExecutorVerificationBlobBudget>,
    reserved_blob_bytes: Option<u64>,
    complete: bool,
}

fn checked_executor_blob_reservation(current: u64, requested: u64) -> Option<u64> {
    current
        .checked_add(requested)
        .filter(|total| *total <= EXECUTOR_VERIFICATION_MAX_RESIDENT_BLOB_BYTES)
}

#[derive(Default)]
struct ExecutorVerificationBlobBudget {
    resident_bytes: Mutex<u64>,
}

impl ExecutorVerificationBlobBudget {
    fn reserve(&self, bytes: u64) -> Result<(), MaterializationError> {
        let mut resident_bytes = self
            .resident_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let available =
            EXECUTOR_VERIFICATION_MAX_RESIDENT_BLOB_BYTES.saturating_sub(*resident_bytes);
        let Some(reserved_total) = checked_executor_blob_reservation(*resident_bytes, bytes) else {
            return Err(MaterializationError::ResourceLimit {
                resource: "verification_blob_resident_bytes",
                requested: bytes,
                available,
                limit: EXECUTOR_VERIFICATION_MAX_RESIDENT_BLOB_BYTES,
            });
        };
        *resident_bytes = reserved_total;
        Ok(())
    }

    fn release(&self, bytes: u64) {
        let mut resident_bytes = self
            .resident_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *resident_bytes = resident_bytes.saturating_sub(bytes);
    }

    #[cfg(test)]
    fn resident_bytes(&self) -> u64 {
        *self
            .resident_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ExecutorVerificationFlight {
    fn reserve_blob_bytes(&mut self, bytes: u64) -> Result<(), MaterializationError> {
        if self.reserved_blob_bytes.is_some() {
            return Err(MaterializationError::Internal(
                "executor verification attempted a second blob reservation".to_owned(),
            ));
        }
        self.blob_budget.reserve(bytes)?;
        self.reserved_blob_bytes = Some(bytes);
        Ok(())
    }

    fn release_blob_reservation(&mut self) {
        self.blob_budget
            .release(self.reserved_blob_bytes.take().unwrap_or(0));
    }

    fn take_blob(&mut self, bytes: Vec<u8>) -> Arc<ExecutorVerificationBlob> {
        let reserved_bytes = self
            .reserved_blob_bytes
            .take()
            .expect("executor blob publication requires a reservation");
        debug_assert_eq!(
            u64::try_from(bytes.len())
                .ok()
                .and_then(|bytes| bytes.checked_add(1)),
            Some(reserved_bytes)
        );
        Arc::new(ExecutorVerificationBlob {
            bytes,
            reserved_bytes,
            budget: Arc::clone(&self.blob_budget),
        })
    }

    fn publish(
        mut self,
        verified: VerifiedNativeExecutorChain,
        blob_bytes: Vec<u8>,
    ) -> Result<
        (
            Arc<VerifiedNativeExecutorChain>,
            Arc<ExecutorVerificationBlob>,
        ),
        Arc<MaterializationError>,
    > {
        let expected_reservation = u64::try_from(blob_bytes.len())
            .ok()
            .and_then(|bytes| bytes.checked_add(1));
        if self.reserved_blob_bytes != expected_reservation {
            return Err(self.fail(MaterializationError::Internal(
                "executor verification blob contradicts its bounded reservation".to_owned(),
            )));
        }
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
            let blob_bytes = self.take_blob(blob_bytes);
            self.set_outcome(Ok((verified.clone(), blob_bytes.clone())));
            state.in_flight.remove(&self.probe);
            drop(state);
            self.pending.ready.notify_all();
            self.complete = true;
            return Ok((verified, blob_bytes));
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
        let blob_bytes = self.take_blob(blob_bytes);
        self.set_outcome(Ok((verified.clone(), blob_bytes.clone())));
        state.in_flight.remove(&self.probe);
        drop(state);
        self.pending.ready.notify_all();
        self.complete = true;
        Ok((verified, blob_bytes))
    }

    fn fail(mut self, error: MaterializationError) -> Arc<MaterializationError> {
        let error = Arc::new(error);
        let cache = executor_verification_cache();
        let mut state = cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.release_blob_reservation();
        self.set_outcome(Err(error.clone()));
        if state
            .in_flight
            .get(&self.probe)
            .is_some_and(|pending| Arc::ptr_eq(pending, &self.pending))
        {
            state.in_flight.remove(&self.probe);
        }
        drop(state);
        self.pending.ready.notify_all();
        self.complete = true;
        error
    }

    fn set_outcome(&self, outcome: ExecutorVerificationResult) {
        *self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
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
        self.release_blob_reservation();
        self.set_outcome(Err(Arc::new(MaterializationError::Internal(
            "executor verification fill ended without publishing its result".to_owned(),
        ))));
        if state
            .in_flight
            .get(&self.probe)
            .is_some_and(|pending| Arc::ptr_eq(pending, &self.pending))
        {
            state.in_flight.remove(&self.probe);
        }
        drop(state);
        self.pending.ready.notify_all();
    }
}

impl PendingExecutorVerification {
    fn wait(&self) -> ExecutorVerificationResult {
        let mut result = self
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while result.is_none() {
            result = self
                .ready
                .wait(result)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        result
            .as_ref()
            .expect("completed executor verification has an outcome")
            .clone()
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
    retire_other_executor_generations(&mut state, &probe.bundle_generation_fingerprint);
    if force_reverify {
        // A repair needs authenticated blob bytes, which reusable metadata
        // entries deliberately do not retain.
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
    if let Some(pending) = state.in_flight.get(probe) {
        return ExecutorVerificationCacheLookup::Wait(pending.clone());
    }
    if state.in_flight.len() >= EXECUTOR_VERIFICATION_CACHE_MAX_IN_FLIGHT {
        return ExecutorVerificationCacheLookup::Saturated;
    }
    let pending = Arc::new(PendingExecutorVerification::default());
    state.in_flight.insert(probe.clone(), pending.clone());
    ExecutorVerificationCacheLookup::Owner(ExecutorVerificationFlight {
        probe: probe.clone(),
        pending,
        blob_budget: Arc::clone(&cache.blob_budget),
        reserved_blob_bytes: None,
        complete: false,
    })
}

fn read_canonical_cas_object_bounded(
    cas: &lillux::CasStore,
    hash: &str,
    max_bytes: u64,
    label: &str,
) -> Result<Option<Value>, MaterializationError> {
    let Some((file, size)) = cas.open_object(hash).map_err(|error| {
        MaterializationError::ManifestError(format!("failed to open {label} {hash}: {error}"))
    })?
    else {
        return Ok(None);
    };
    if size > max_bytes {
        return Err(MaterializationError::ManifestError(format!(
            "{label} {hash} exceeds {max_bytes} bytes"
        )));
    }
    let bytes =
        lillux::read_open_regular_file_exact_bounded(file, size, max_bytes).map_err(|error| {
            MaterializationError::ManifestError(format!("failed to read {label} {hash}: {error}"))
        })?;
    if u64::try_from(bytes.len()).ok() != Some(size) || lillux::sha256_hex(&bytes) != hash {
        return Err(MaterializationError::ManifestError(format!(
            "{label} {hash} failed content-address verification"
        )));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        MaterializationError::ManifestError(format!("failed to decode {label} {hash}: {error}"))
    })?;
    let canonical = lillux::canonical_json(&value).map_err(|error| {
        MaterializationError::ManifestError(format!(
            "failed to canonicalize {label} {hash}: {error}"
        ))
    })?;
    if canonical.as_bytes() != bytes {
        return Err(MaterializationError::ManifestError(format!(
            "{label} {hash} violates the canonical JSON contract"
        )));
    }
    Ok(Some(value))
}

fn read_cas_blob_bounded(
    cas: &lillux::CasStore,
    hash: &str,
    max_bytes: u64,
    executor_ref: &str,
    reservation: &mut ExecutorVerificationFlight,
) -> Result<Option<Vec<u8>>, MaterializationError> {
    let Some((file, size)) =
        cas.open_blob(hash)
            .map_err(|error| MaterializationError::BlobNotFound {
                hash: format!("{hash} (open error: {error})"),
            })?
    else {
        return Ok(None);
    };
    if size > max_bytes {
        return Err(MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_owned(),
            detail: format!("native executor blob {hash} exceeds {max_bytes} bytes"),
        });
    }
    let allocation_bytes =
        size.checked_add(1)
            .ok_or_else(|| MaterializationError::MaterializationFailed {
                executor_ref: executor_ref.to_owned(),
                detail: "native executor blob allocation size overflow".to_owned(),
            })?;
    reservation.reserve_blob_bytes(allocation_bytes)?;
    let bytes =
        lillux::read_open_regular_file_exact_bounded(file, size, max_bytes).map_err(|error| {
            MaterializationError::MaterializationFailed {
                executor_ref: executor_ref.to_owned(),
                detail: format!("failed to read native executor blob {hash}: {error}"),
            }
        })?;
    if u64::try_from(bytes.len()).ok() != Some(size) || lillux::sha256_hex(&bytes) != hash {
        return Err(MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_owned(),
            detail: format!("native executor blob {hash} failed content-address verification"),
        });
    }
    Ok(Some(bytes))
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
        let signed_ref_digest = lillux::read_optional_regular_file_bounded_no_follow(
            &ref_path,
            MAX_SIGNED_EXECUTOR_MANIFEST_REF_BYTES,
        )
        .map_err(|error| {
            MaterializationError::ManifestError(format!(
                "failed to read signed bundle executor manifest ref {}: {error}",
                ref_path.display()
            ))
        })?
        .map(|bytes| lillux::sha256_hex(&bytes));
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
    reservation: &mut ExecutorVerificationFlight,
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
        let signed_ref_bytes = lillux::read_optional_regular_file_bounded_no_follow(
            &ref_path,
            MAX_SIGNED_EXECUTOR_MANIFEST_REF_BYTES,
        )
        .map_err(|error| {
            MaterializationError::ManifestError(format!(
                "failed to re-read signed bundle executor manifest ref {}: {error}",
                ref_path.display()
            ))
        })?
        .ok_or_else(|| {
            MaterializationError::ManifestError(format!(
                "signed bundle executor manifest ref {} disappeared",
                ref_path.display()
            ))
        })?;
        let live_ref_digest = lillux::sha256_hex(&signed_ref_bytes);
        if &live_ref_digest != expected_ref_digest {
            return Err(MaterializationError::ManifestError(format!(
                "signed bundle executor manifest ref {} changed during generation-checked verification",
                ref_path.display()
            )));
        }
        let signed_ref = std::str::from_utf8(&signed_ref_bytes).map_err(|error| {
            MaterializationError::ManifestError(format!(
                "signed bundle executor manifest ref {} is not UTF-8: {error}",
                ref_path.display()
            ))
        })?;
        tried_roots.push(system_root.clone());

        let verified_ref =
            match ryeos_engine::executor_resolution::verify_signed_executor_manifest_ref(
                signed_ref,
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
        let manifest_value = read_canonical_cas_object_bounded(
            &cas,
            &manifest_hash,
            MAX_EXECUTOR_MANIFEST_OBJECT_BYTES,
            "bundle manifest object",
        )?
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
            |hash| {
                read_canonical_cas_object_bounded(
                    &cas,
                    hash,
                    MAX_EXECUTOR_ITEM_SOURCE_OBJECT_BYTES,
                    "executor item-source object",
                )
                .map_err(|error| error.to_string())
            },
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
                });
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
    let blob_bytes = read_cas_blob_bounded(
        &cas,
        &resolved.blob_hash,
        MAX_NATIVE_EXECUTOR_BYTES,
        bare,
        reservation,
    )?
    .ok_or_else(|| MaterializationError::BlobNotFound {
        hash: resolved.blob_hash.clone(),
    })?;
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
) -> Result<
    (
        Arc<VerifiedNativeExecutorChain>,
        Option<Arc<ExecutorVerificationBlob>>,
    ),
    MaterializationError,
> {
    match lookup_or_claim_executor_verification(probe, force_reverify) {
        ExecutorVerificationCacheLookup::Hit(verified) => {
            emit_executor_verification_cache_metric(
                ExecutorVerificationCacheOutcome::Hit,
                ExecutorVerificationCacheReason::Ready,
            );
            tracing::debug!(
                executor_ref = %probe.executor_ref,
                bundle_generation = %probe.bundle_generation_fingerprint,
                "native executor verified-chain cache hit"
            );
            Ok((verified, None))
        }
        ExecutorVerificationCacheLookup::Owner(mut flight) => {
            emit_executor_verification_cache_metric(
                ExecutorVerificationCacheOutcome::Miss,
                ExecutorVerificationCacheReason::Cold,
            );
            let (verified, blob_bytes) = match verify_native_executor_chain(
                probe,
                bare,
                trust_store,
                launch_timings,
                &mut flight,
            ) {
                Ok(verified) => verified,
                Err(error) => return Err(MaterializationError::Shared(flight.fail(error))),
            };
            let (verified, blob_bytes) = flight
                .publish(verified, blob_bytes)
                .map_err(MaterializationError::Shared)?;
            Ok((verified, Some(blob_bytes)))
        }
        ExecutorVerificationCacheLookup::Wait(pending) => {
            emit_executor_verification_cache_metric(
                ExecutorVerificationCacheOutcome::Hit,
                ExecutorVerificationCacheReason::SingleFlight,
            );
            match pending.wait() {
                Ok((verified, blob_bytes)) => Ok((verified, Some(blob_bytes))),
                Err(error) => Err(MaterializationError::Shared(error)),
            }
        }
        ExecutorVerificationCacheLookup::Saturated => {
            emit_executor_verification_cache_metric(
                ExecutorVerificationCacheOutcome::Refusal,
                ExecutorVerificationCacheReason::PendingCapacity,
            );
            Err(MaterializationError::ResourceLimit {
                resource: "verification_in_flight",
                requested: u64::try_from(EXECUTOR_VERIFICATION_CACHE_MAX_IN_FLIGHT)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
                available: 0,
                limit: u64::try_from(EXECUTOR_VERIFICATION_CACHE_MAX_IN_FLIGHT).unwrap_or(u64::MAX),
            })
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ExecutorVerificationCacheOutcome {
    Hit,
    Miss,
    Refusal,
}

impl ExecutorVerificationCacheOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Refusal => "refusal",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ExecutorVerificationCacheReason {
    Ready,
    SingleFlight,
    Cold,
    PendingCapacity,
}

impl ExecutorVerificationCacheReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::SingleFlight => "single_flight",
            Self::Cold => "cold",
            Self::PendingCapacity => "pending_capacity",
        }
    }
}

fn emit_executor_verification_cache_metric(
    outcome: ExecutorVerificationCacheOutcome,
    reason: ExecutorVerificationCacheReason,
) {
    tracing::info!(
        target: "ryeos.metrics",
        metric = "native_executor_verification_cache",
        outcome = outcome.as_str(),
        reason = reason.as_str(),
        "native executor verification cache metric"
    );
}

/// Cache namespace identity for a native executor binary.
///
/// The executor name is part of the key even when two executors have identical
/// bytes. Otherwise materializing the second name would treat the first name's
/// `<blob_hash>` directory as corrupt and quarantine the recovery artifact.
fn executor_cache_entry_key(blob_hash: &str, bare: &str) -> String {
    lillux::cas::sha256_hex(
        format!("ryeos-native-executor-cache-v2\0{blob_hash}\0{bare}").as_bytes(),
    )
}

/// Content-addressed cache target for a native executor binary.
///
/// Returns `<cache_root>/cache/executors/<tuple_hash>/<bare>`, where the tuple
/// commits to both the verified blob hash and executor name.
fn executor_cache_target(cache_root: &Path, blob_hash: &str, bare: &str) -> PathBuf {
    cache_root
        .join("cache")
        .join("executors")
        .join(executor_cache_entry_key(blob_hash, bare))
        .join(bare)
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

struct ExecutorCacheLayout {
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
    // hierarchy exists.
    Ok(ExecutorCacheLayout {
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
) -> Result<VerifiedOpenedExecutor, String> {
    if !native_executor_size_is_admissible(expected_len) {
        return Err(format!(
            "opened executor length {expected_len} exceeds {MAX_NATIVE_EXECUTOR_BYTES} bytes"
        ));
    }
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

    let (actual_hash, after_metadata) =
        lillux::digest_open_regular_file_stable_exact(&mut file, expected_len)
            .map_err(|error| format!("failed to hash opened executor: {error}"))?;
    if actual_hash != expected_hash {
        return Err(format!(
            "opened executor failed its content-address check for {executor_ref}"
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

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
        Ok(VerifiedOpenedExecutor {
            handle: Arc::new(file),
            identity: after_identity,
        })
    }
}

fn inspect_materialized_executor(
    layout: &ExecutorCacheLayout,
    verified: &VerifiedNativeExecutorChain,
    bare: &str,
) -> MaterializedArtifactInspection {
    let entry_key = executor_cache_entry_key(&verified.key.blob_hash, bare);
    let blob_dir = match layout
        .executors
        .open_child_directory(OsStr::new(&entry_key))
    {
        Ok(Some(directory)) => directory,
        Ok(None) => {
            return match layout.executors.open_entry(OsStr::new(&entry_key), false) {
                Ok(None) => MaterializedArtifactInspection::Missing,
                Ok(Some(_)) => MaterializedArtifactInspection::Invalid(
                    "content-addressed executor entry is not a directory".to_string(),
                ),
                Err(error) => MaterializedArtifactInspection::Invalid(format!(
                    "content-addressed executor entry is malformed: {error}"
                )),
            };
        }
        Err(error) => {
            return MaterializedArtifactInspection::Invalid(format!(
                "failed to securely open content-addressed executor directory: {error}"
            ));
        }
    };
    if let Err(error) = validate_executor_cache_ancestors(layout, &blob_dir, bare) {
        return MaterializedArtifactInspection::Invalid(error.to_string());
    }
    let file = match blob_dir.open_regular(OsStr::new(bare), false) {
        Ok(Some(file)) => file,
        Ok(None) => {
            return MaterializedArtifactInspection::Invalid(
                "materialized executor file is missing".to_string(),
            );
        }
        Err(error) => {
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
                ));
            }
        };
        let daemon_uid = unsafe { libc::geteuid() };
        let actual_mode = metadata.mode() & 0o7777;
        if !metadata.file_type().is_file()
            || metadata.uid() != daemon_uid
            || actual_mode & 0o022 != 0
        {
            return MaterializedArtifactInspection::Invalid(format!(
                "materialized executor descriptor is not a daemon-owned, non-group/other-writable regular file (uid={}, mode={actual_mode:#o})",
                metadata.uid()
            ));
        }
    }
    let verification = verify_opened_executor_file(
        file,
        &verified.key.blob_hash,
        verified.key.blob_len,
        verified.key.mode,
        bare,
    );
    match verification {
        Ok(opened) => MaterializedArtifactInspection::Valid(opened),
        Err(detail) => MaterializedArtifactInspection::Invalid(detail),
    }
}

/// Reopen the exact native executor already admitted into a managed launch
/// capsule. The capsule's content hash is the authority: recovery must not
/// resolve the executor name through a newer installed bundle generation.
///
/// The executor cache is populated only after full bundle-chain verification
/// and is content-addressed by the admitted blob hash. We nevertheless reopen
/// it through the descriptor-pinned hierarchy and hash every byte again before
/// returning a launch handle.
fn materialize_admitted_native_executor(
    executor_ref: &str,
    cas_root: &Path,
    isolation: &ryeos_engine::isolation::IsolationRuntime,
    content_hash: &str,
    bundle_manifest_hash: &str,
    bundle_signer_fingerprint: &str,
) -> Result<MaterializedExecutor, MaterializationError> {
    let bare = executor_ref.strip_prefix("native:").ok_or_else(|| {
        MaterializationError::ExecutorUnavailable {
            executor_ref: executor_ref.to_string(),
            detail: "executor_ref is not a native executor".to_string(),
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
    for (label, hash) in [
        ("admitted executor content hash", content_hash),
        (
            "admitted executor bundle manifest hash",
            bundle_manifest_hash,
        ),
    ] {
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(MaterializationError::Internal(format!(
                "{label} is not a canonical SHA-256 hash"
            )));
        }
    }

    let cas_directory = lillux::PinnedDirectory::open(cas_root)
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!("failed to open admitted executor CAS root: {error}"),
        })?
        .ok_or_else(|| MaterializationError::BlobNotFound {
            hash: content_hash.to_string(),
        })?;
    let cas = lillux::CasStore::from_pinned_root(cas_directory);
    let (blob, blob_size) = cas
        .open_blob(content_hash)
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!("failed to open admitted executor blob: {error}"),
        })?
        .ok_or_else(|| MaterializationError::BlobNotFound {
            hash: content_hash.to_string(),
        })?;
    if !native_executor_size_is_admissible(blob_size) {
        return Err(MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!(
                "admitted native executor blob exceeds {MAX_NATIVE_EXECUTOR_BYTES} bytes"
            ),
        });
    }
    let path = lillux::cas::shard_path(cas.root(), "blobs", content_hash, "");
    let verified_command = isolation
        .bind_admitted_verified_command(
            ryeos_engine::isolation::IsolationVerifiedCode {
                source_path: path.clone(),
                content_hash: content_hash.to_string(),
            },
            blob,
        )
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!("failed to bind admitted executor blob: {error}"),
        })?;
    Ok(MaterializedExecutor {
        path,
        content_hash: content_hash.to_string(),
        bundle_manifest_hash: bundle_manifest_hash.to_string(),
        bundle_signer_fingerprint: bundle_signer_fingerprint.to_string(),
        verified_command,
    })
}

fn stage_managed_executor_blob(
    state: &AppState,
    executor: &MaterializedExecutor,
) -> Result<(String, super::PendingCasPublication), BuildAndLaunchError> {
    #[cfg(not(unix))]
    {
        let _ = (state, executor);
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "managed executor admission requires Unix descriptor identity"
        )));
    }
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::fs::FileExt as _;

        let descriptor = executor.verified_command.executable();
        let before = descriptor.metadata().map_err(|error| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "inspect managed executor before CAS admission: {error}"
            ))
        })?;
        let admitted = executor.verified_command.file_identity();
        let admitted = ExecutorFileIdentity {
            device: admitted.device,
            inode: admitted.inode,
            size: admitted.size,
            modified_seconds: admitted.modified_seconds,
            modified_nanoseconds: admitted.modified_nanoseconds,
            changed_seconds: admitted.changed_seconds,
            changed_nanoseconds: admitted.changed_nanoseconds,
            mode: admitted.mode,
            file_type: admitted.file_type,
        };
        if executor_file_identity(&before) != admitted {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed executor descriptor identity changed before CAS admission"
            )));
        }
        if !native_executor_size_is_admissible(before.len()) {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed executor exceeds {MAX_NATIVE_EXECUTOR_BYTES} bytes"
            )));
        }
        let len = usize::try_from(before.len()).map_err(|_| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed executor is too large to admit on this platform"
            ))
        })?;
        let mut bytes = vec![0_u8; len];
        let mut offset = 0_usize;
        while offset < bytes.len() {
            let read = descriptor
                .read_at(&mut bytes[offset..], offset as u64)
                .map_err(|error| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "read managed executor for CAS admission: {error}"
                    ))
                })?;
            if read == 0 {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "managed executor ended before its verified size"
                )));
            }
            offset += read;
        }
        let after = descriptor.metadata().map_err(|error| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "reinspect managed executor after CAS admission read: {error}"
            ))
        })?;
        if executor_file_identity(&before) != executor_file_identity(&after)
            || lillux::sha256_hex(&bytes) != executor.content_hash
        {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed executor changed while entering the admitted CAS closure"
            )));
        }
        bytes
    };

    let authority = super::pinned_state_authority(state).map_err(BuildAndLaunchError::Internal)?;
    let guard = authority
        .acquire_shared_guard()
        .map_err(BuildAndLaunchError::Internal)?;
    authority
        .ensure_guard(&guard)
        .map_err(BuildAndLaunchError::Internal)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "cannot acquire managed executor CAS write permit: {error}"
            ))
        })?;
    let cas = authority
        .cas_store()
        .map_err(BuildAndLaunchError::Internal)?;
    let mut staged_roots = authority
        .require_recovery()
        .map_err(BuildAndLaunchError::Internal)?
        .begin_staged_cas_roots_admitted(&guard, "managed-executor-admission")
        .map_err(BuildAndLaunchError::Internal)?;
    let hash = staged_roots
        .store_blob_admitted(&guard, &cas, &bytes)
        .map_err(BuildAndLaunchError::Internal)?;
    if hash != executor.content_hash {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "managed executor CAS hash contradicts verified executable identity"
        )));
    }
    Ok((
        hash,
        super::PendingCasPublication::new(authority, staged_roots),
    ))
}

fn ensure_admitted_executor_signer_trusted(
    trust_store: &ryeos_engine::trust::TrustStore,
    executor_ref: &str,
    signer_fingerprint: &str,
) -> Result<(), MaterializationError> {
    if trust_store.is_trusted(signer_fingerprint) {
        return Ok(());
    }
    Err(MaterializationError::ExecutorUntrusted {
        executor_ref: executor_ref.to_string(),
        trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
        fingerprint: Some(signer_fingerprint.to_string()),
    })
}

enum QuarantinedExecutorEntry {
    Directory {
        name: String,
        directory: lillux::secure_fs::PinnedDirectory,
    },
    Regular {
        name: String,
        file: std::fs::File,
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
            Self::Regular { name, file } => {
                executors
                    .remove_if_same(OsStr::new(&name), &file)
                    .map_err(|error| MaterializationError::MaterializationFailed {
                        executor_ref: executor_ref.to_string(),
                        detail: format!("failed to remove executor quarantine {name}: {error}"),
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
    let entry_key = executor_cache_entry_key(blob_hash, executor_ref);
    let source = layout
        .executors
        .open_entry(OsStr::new(&entry_key), false)
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: executor_ref.to_string(),
            detail: format!("failed to pin corrupt executor cache entry: {error}"),
        })?;
    let Some(source) = source else {
        return Ok(None);
    };
    let quarantine_name = format!(
        ".quarantine.{blob_hash}.{}.{}",
        std::process::id(),
        rand::thread_rng().r#gen::<u64>()
    );
    let quarantined = match source {
        lillux::PinnedDirectoryEntry::Directory(directory) => {
            match layout.executors.rename_child_directory_noreplace(
                OsStr::new(&entry_key),
                OsStr::new(&quarantine_name),
                &directory,
            ) {
                Ok(()) => {}
                Err(error) if error.namespace_committed() => {}
                Err(error) => {
                    return Err(MaterializationError::MaterializationFailed {
                        executor_ref: executor_ref.to_string(),
                        detail: format!(
                            "failed to quarantine corrupt executor cache entry: {error}"
                        ),
                    });
                }
            }
            QuarantinedExecutorEntry::Directory {
                name: quarantine_name,
                directory,
            }
        }
        lillux::PinnedDirectoryEntry::Regular(file) => {
            match layout.executors.rename_regular_child_noreplace_atomic(
                OsStr::new(&entry_key),
                OsStr::new(&quarantine_name),
                &file,
            ) {
                Ok(()) => {}
                Err(error) if error.namespace_committed() => {}
                Err(error) => {
                    return Err(MaterializationError::MaterializationFailed {
                        executor_ref: executor_ref.to_string(),
                        detail: format!(
                            "failed to quarantine corrupt executor cache entry: {error}"
                        ),
                    });
                }
            }
            QuarantinedExecutorEntry::Regular {
                name: quarantine_name,
                file,
            }
        }
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
        || !native_executor_size_is_admissible(verified.key.blob_len)
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
        rand::thread_rng().r#gen::<u64>()
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
    lillux::set_open_regular_file_mode(&staged_file, verified.key.mode).map_err(|error| {
        MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to apply signed executor mode: {error}"),
        }
    })?;
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
    let verified_staged_file = verify_opened_executor_file(
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
    validate_executor_cache_ancestors(layout, &staging, bare)?;
    staging
        .sync_tree()
        .map_err(|error| MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to sync staged executor tree: {error}"),
        })?;

    let publication = layout.executors.rename_child_directory_noreplace(
        OsStr::new(&staging_name),
        OsStr::new(&executor_cache_entry_key(&verified.key.blob_hash, bare)),
        &staging,
    );
    match publication {
        // The staged file was fully hashed through its open descriptor, and
        // this primitive proves the same pinned staging-directory inode was
        // moved without replacement. No second target-path read is needed on
        // the won branch.
        Ok(()) => Ok(verified_staged_file),
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
    mut blob_bytes: Option<Arc<ExecutorVerificationBlob>>,
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
            .as_ref()
            .ok_or_else(|| MaterializationError::MaterializationFailed {
                executor_ref: bare.to_string(),
                detail:
                    "single-flight full executor re-verification produced no trusted blob bytes"
                        .to_string(),
            })?;
    let opened = publish_verified_executor_blob(layout, &verified, bare, blob_bytes.as_slice())?;
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

/// Verify the exact executor chain without opening or repairing the
/// materialized executable cache entry.
pub fn verify_native_executor_chain_attestation_for_engine(
    engine: &ryeos_engine::engine::Engine,
    bundle_roots: &[PathBuf],
    executor_ref: &str,
    root_trust_class: ryeos_engine::resolution::TrustClass,
    launch_timings: Option<&ryeos_app::launch_stage_timings::LaunchStageTimings>,
) -> Result<VerifiedExecutorChainAttestation, MaterializationError> {
    engine.debug_assert_executor_cache_generation_identity();
    engine.with_checked_bundle_generation(|_| {
        if bundle_roots != engine.bundle_roots.as_slice() {
            return Err(MaterializationError::Internal(
                "executor verification requires the complete registered bundle-root generation"
                    .to_string(),
            ));
        }
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
        let bundle_generation_fingerprint = engine.registered_bundle_generation_fingerprint();
        let node_trust_fingerprint = engine.node_trust_store.fingerprint();
        let triple = host_triple();
        let probe = manifest_ref_probe(
            bundle_roots,
            &bundle_generation_fingerprint,
            &node_trust_fingerprint,
            executor_ref,
            &triple,
            root_trust_class,
        )?;
        let (verified, _) = cached_or_verified_executor_chain(
            &probe,
            bare,
            &engine.node_trust_store,
            false,
            launch_timings,
        )?;
        Ok(VerifiedExecutorChainAttestation { verified })
    })
}

/// Build the verified config loader used by generic execution-limit policy.
/// Launch-preparer snapshots use the stricter engine-owned loader instead.
fn build_verified_loader_for_thread(
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    node_config_root: Option<&Path>,
    node_trusted_keys_dir: &Path,
) -> anyhow::Result<ryeos_runtime::verified_loader::VerifiedLoader> {
    let project_root = engine_roots
        .authoritative_project_root()?
        .map(Path::to_path_buf);
    let bundle_roots = engine_roots
        .authoritative_bundle_roots()?
        .into_iter()
        .map(Path::to_path_buf)
        .collect();
    match project_root {
        Some(project_root) => ryeos_runtime::verified_loader::VerifiedLoader::new_with_node_config(
            project_root,
            node_config_root.map(Path::to_path_buf),
            bundle_roots,
            node_trusted_keys_dir,
        ),
        None => ryeos_runtime::verified_loader::VerifiedLoader::new_projectless_with_node_config(
            node_config_root.map(Path::to_path_buf),
            bundle_roots,
            node_trusted_keys_dir,
        ),
    }
}

fn build_verified_loader_for_thread_under_project_authority(
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    node_config_root: Option<&Path>,
    node_trusted_keys_dir: &Path,
    project_materialization: &ryeos_state::PinnedProjectMaterialization,
) -> anyhow::Result<ryeos_runtime::verified_loader::VerifiedLoader> {
    let project_root = engine_roots
        .authoritative_project_root()?
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("admitted execution controls have no project root"))?;
    if project_root != project_materialization.path() {
        anyhow::bail!(
            "execution-control project root differs from admitted project materialization"
        );
    }
    let bundle_roots = engine_roots
        .authoritative_bundle_roots()?
        .into_iter()
        .map(Path::to_path_buf)
        .collect();
    ryeos_runtime::verified_loader::VerifiedLoader::new_with_node_config_under_project_authority(
        project_root,
        project_materialization,
        node_config_root.map(Path::to_path_buf),
        bundle_roots,
        node_trusted_keys_dir,
    )
}

fn load_limits_snapshot_under_current_authority(
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    node_config_root: Option<&Path>,
    node_trusted_keys_dir: &Path,
    project_materialization: Option<&ryeos_state::PinnedProjectMaterialization>,
    declaration: &ryeos_engine::runtime_registry::RuntimeLimitsDecl,
) -> anyhow::Result<LimitsConfigSnapshot> {
    match project_materialization {
        Some(materialization) => {
            let loader = build_verified_loader_for_thread_under_project_authority(
                roots,
                node_config_root,
                node_trusted_keys_dir,
                materialization,
            )?;
            load_limits_config_snapshot_under_project_authority(
                &loader,
                materialization,
                declaration,
            )
        }
        None => {
            let loader =
                build_verified_loader_for_thread(roots, node_config_root, node_trusted_keys_dir)?;
            load_limits_config_snapshot(&loader, declaration)
        }
    }
}

#[derive(Debug, Clone)]
struct ExecutionControlSnapshot {
    policy: ryeos_engine::execution_policy::ResolvedExecutionPolicy,
    limits: LimitsConfigSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionControlProofStatus {
    Current,
    MutableAuthorityChanged,
    ImmutableAuthorityMismatch,
}

impl ExecutionControlSnapshot {
    fn estimated_bytes(&self) -> usize {
        self.policy
            .dependency_proof()
            .estimated_bytes()
            .saturating_add(
                self.policy
                    .loaded_layers
                    .iter()
                    .map(|layer| layer.path.as_os_str().as_encoded_bytes().len())
                    .sum::<usize>(),
            )
            .saturating_add(
                serde_json::to_vec(&self.limits.config)
                    .map(|bytes| bytes.len())
                    .unwrap_or(usize::MAX),
            )
            .saturating_add(
                self.limits
                    .dependency_proof
                    .as_ref()
                    .map(|proof| proof.estimated_bytes())
                    .unwrap_or(0),
            )
    }
}

fn execution_control_cache()
-> &'static crate::resolved_config_cache::SnapshotCache<ExecutionControlSnapshot> {
    static CACHE: OnceLock<crate::resolved_config_cache::SnapshotCache<ExecutionControlSnapshot>> =
        OnceLock::new();
    CACHE.get_or_init(crate::resolved_config_cache::SnapshotCache::default)
}

fn execution_control_snapshot_status(
    snapshot: &ExecutionControlSnapshot,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    node_config_root: Option<&Path>,
    node_trusted_keys_dir: &Path,
    project_materialization: Option<&ryeos_state::PinnedProjectMaterialization>,
) -> ExecutionControlProofStatus {
    let project = project_materialization.map(|materialization| {
        (
            materialization.path(),
            materialization as &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
        )
    });
    let policy_status = match snapshot
        .policy
        .dependency_proof()
        .revalidate_under_authority_status(roots, project)
    {
        ryeos_engine::execution_policy::ExecutionPolicyProofStatus::Current => {
            ExecutionControlProofStatus::Current
        }
        ryeos_engine::execution_policy::ExecutionPolicyProofStatus::MutableAuthorityChanged => {
            ExecutionControlProofStatus::MutableAuthorityChanged
        }
        ryeos_engine::execution_policy::ExecutionPolicyProofStatus::ImmutableAuthorityMismatch => {
            ExecutionControlProofStatus::ImmutableAuthorityMismatch
        }
    };
    if policy_status == ExecutionControlProofStatus::ImmutableAuthorityMismatch {
        return policy_status;
    }
    let Ok(current_node_trust) =
        ryeos_runtime::verified_loader::VerifiedLoader::node_trust_identity_from(
            node_trusted_keys_dir,
        )
    else {
        return combine_execution_control_status(
            policy_status,
            ExecutionControlProofStatus::MutableAuthorityChanged,
        );
    };
    let Some(limits_proof) = snapshot.limits.dependency_proof.as_ref() else {
        return policy_status;
    };
    let limits_status = if let Some(materialization) = project_materialization {
        if !limits_proof.trust_identities_match(None, &current_node_trust) {
            ExecutionControlProofStatus::MutableAuthorityChanged
        } else {
            match limits_proof.revalidate_under_project_authority_status(
                    Some(materialization.path()),
                    node_config_root,
                    project.map(|(_, content)| content),
                ) {
                ryeos_runtime::verified_loader::ConfigDependencyProofStatus::Current => {
                    ExecutionControlProofStatus::Current
                }
                ryeos_runtime::verified_loader::ConfigDependencyProofStatus::MutableAuthorityChanged => {
                    ExecutionControlProofStatus::MutableAuthorityChanged
                }
                ryeos_runtime::verified_loader::ConfigDependencyProofStatus::ImmutableAuthorityMismatch => {
                    ExecutionControlProofStatus::ImmutableAuthorityMismatch
                }
            }
        }
    } else {
        let Ok(loader) =
            build_verified_loader_for_thread(roots, node_config_root, node_trusted_keys_dir)
        else {
            return combine_execution_control_status(
                policy_status,
                ExecutionControlProofStatus::MutableAuthorityChanged,
            );
        };
        if limits_proof.trust_identities_match(
            Some(&loader.effective_trust_identity()),
            &current_node_trust,
        ) && limits_proof.revalidate_mutable_against(
            roots.authoritative_project_root().ok().flatten(),
            node_config_root,
            true,
        ) {
            ExecutionControlProofStatus::Current
        } else {
            ExecutionControlProofStatus::MutableAuthorityChanged
        }
    };
    combine_execution_control_status(policy_status, limits_status)
}

fn combine_execution_control_status(
    left: ExecutionControlProofStatus,
    right: ExecutionControlProofStatus,
) -> ExecutionControlProofStatus {
    use ExecutionControlProofStatus::{
        Current, ImmutableAuthorityMismatch, MutableAuthorityChanged,
    };
    match (left, right) {
        (ImmutableAuthorityMismatch, _) | (_, ImmutableAuthorityMismatch) => {
            ImmutableAuthorityMismatch
        }
        (MutableAuthorityChanged, _) | (_, MutableAuthorityChanged) => MutableAuthorityChanged,
        (Current, Current) => Current,
    }
}

fn execution_control_snapshot_is_current(
    snapshot: &ExecutionControlSnapshot,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    node_config_root: Option<&Path>,
    node_trusted_keys_dir: &Path,
    project_materialization: Option<&ryeos_state::PinnedProjectMaterialization>,
) -> anyhow::Result<bool> {
    match execution_control_snapshot_status(
        snapshot,
        roots,
        node_config_root,
        node_trusted_keys_dir,
        project_materialization,
    ) {
        ExecutionControlProofStatus::Current => Ok(true),
        ExecutionControlProofStatus::MutableAuthorityChanged => Ok(false),
        ExecutionControlProofStatus::ImmutableAuthorityMismatch => {
            anyhow::bail!("execution control contradicts its immutable admitted authority")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn load_execution_control_snapshot_cached(
    engine: &ryeos_engine::engine::Engine,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    request_snapshot: &ryeos_engine::engine::EffectiveRequestSnapshot,
    item_ref: &ryeos_engine::canonical_ref::CanonicalRef,
    subject_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
    node_config_root: Option<&Path>,
    node_trusted_keys_dir: &Path,
    project_materialization: Option<&ryeos_state::PinnedProjectMaterialization>,
    runtime_limits: &ryeos_engine::runtime_registry::RuntimeLimitsDecl,
) -> anyhow::Result<Arc<ExecutionControlSnapshot>> {
    let generation = engine.registered_bundle_generation_fingerprint();
    let generation_epoch = engine.registered_bundle_generation_epoch();
    let root_identity = roots
        .ordered
        .iter()
        .map(|root| {
            serde_json::json!({
                "label": root.label,
                "space": root.space,
                "live_path": matches!(
                    subject_authority,
                    ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs
                )
                .then(|| root.ai_root.clone()),
            })
        })
        .collect::<Vec<_>>();
    let node_trust_identity =
        ryeos_runtime::verified_loader::VerifiedLoader::node_trust_identity_from(
            node_trusted_keys_dir,
        )?;
    let key_value = serde_json::json!({
        "schema_version": 1,
        "item_ref": item_ref.to_string(),
        "subject_authority": subject_authority,
        "request_engine_generation_identity": request_snapshot.request_engine_generation_identity,
        "registry_fingerprint": request_snapshot.registry_fingerprint,
        "effective_trust_identity": request_snapshot.effective_trust_identity,
        "node_trust_identity": node_trust_identity,
        "node_config_layer": node_config_root.is_some(),
        "runtime_limits": runtime_limits,
        "roots": root_identity,
    });
    let canonical = lillux::canonical_json(&key_value)?;
    let key = crate::resolved_config_cache::SnapshotCacheKey {
        namespace: "execution_control",
        retirement_scope: item_ref.to_string(),
        generation,
        generation_epoch,
        identity: lillux::sha256_hex(canonical.as_bytes()),
    };
    let mut retries = 0_usize;
    loop {
        match execution_control_cache().begin(key.clone()) {
            crate::resolved_config_cache::Lookup::Hit { value, entry_bytes } => {
                if execution_control_snapshot_is_current(
                    &value,
                    roots,
                    node_config_root,
                    node_trusted_keys_dir,
                    project_materialization,
                )? {
                    crate::resolved_config_cache::emit_metric(
                        key.namespace,
                        crate::resolved_config_cache::CacheOutcome::Hit,
                        crate::resolved_config_cache::CacheReason::Ready,
                        entry_bytes,
                        0,
                    );
                    return Ok(value);
                }
                crate::resolved_config_cache::emit_metric(
                    key.namespace,
                    crate::resolved_config_cache::CacheOutcome::Eviction,
                    crate::resolved_config_cache::CacheReason::StaleProof,
                    entry_bytes,
                    0,
                );
                execution_control_cache().discard_if_same(&key, &value);
            }
            crate::resolved_config_cache::Lookup::Wait { pending } => {
                let wait_started = Instant::now();
                let Some(value) = pending.wait().await.map_err(|error| {
                    anyhow::Error::new(crate::dispatch_error::DispatchError::Shared(error))
                })?
                else {
                    retries = retries.saturating_add(1);
                    if retries >= 3 {
                        anyhow::bail!(
                            "execution control authority changed repeatedly while waiting"
                        );
                    }
                    continue;
                };
                if execution_control_snapshot_is_current(
                    &value,
                    roots,
                    node_config_root,
                    node_trusted_keys_dir,
                    project_materialization,
                )? {
                    crate::resolved_config_cache::emit_metric(
                        key.namespace,
                        crate::resolved_config_cache::CacheOutcome::Hit,
                        crate::resolved_config_cache::CacheReason::SingleFlight,
                        0,
                        wait_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    );
                    return Ok(value);
                }
                crate::resolved_config_cache::emit_metric(
                    key.namespace,
                    crate::resolved_config_cache::CacheOutcome::Eviction,
                    crate::resolved_config_cache::CacheReason::StaleProof,
                    0,
                    wait_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                );
                execution_control_cache().discard_if_same(&key, &value);
            }
            crate::resolved_config_cache::Lookup::Build(fill) => {
                let load_roots = roots.clone();
                let load_item_ref = item_ref.clone();
                let load_parsers = request_snapshot.parser_dispatcher.clone();
                let load_kinds = engine.kinds.clone();
                let load_trust = request_snapshot.trust_store.clone();
                let load_project_materialization = project_materialization.cloned();
                let load_node_config_root = node_config_root.map(Path::to_path_buf);
                let load_node_trusted_keys_dir = node_trusted_keys_dir.to_path_buf();
                let load_runtime_limits = runtime_limits.clone();
                let loaded = tokio::task::spawn_blocking(move || {
                    let policy = ryeos_engine::execution_policy::ExecutionPolicyResolver::new(
                        ryeos_engine::config_loading::ConfigLoadContext {
                            roots: &load_roots,
                            parsers: &load_parsers,
                            kinds: &load_kinds,
                            trust_store: &load_trust,
                            project_authority: load_project_materialization.as_ref().map(
                                |materialization| {
                                    (
                                        materialization.path(),
                                        materialization
                                            as &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
                                    )
                                },
                            ),
                        },
                    )
                    .resolve_for_item(&load_item_ref)?;
                    let limits = load_limits_snapshot_under_current_authority(
                        &load_roots,
                        load_node_config_root.as_deref(),
                        &load_node_trusted_keys_dir,
                        load_project_materialization.as_ref(),
                        &load_runtime_limits,
                    )?;
                    Ok::<_, anyhow::Error>(ExecutionControlSnapshot { policy, limits })
                })
                .await
                .map_err(|error| {
                    anyhow::anyhow!("execution control snapshot worker failed: {error}")
                })
                .and_then(|result| result);
                let snapshot = match loaded {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        let error =
                            fill.fail(crate::dispatch_error::DispatchError::Internal(error));
                        return Err(anyhow::Error::new(
                            crate::dispatch_error::DispatchError::Shared(error),
                        ));
                    }
                };
                let snapshot_is_current = match execution_control_snapshot_is_current(
                    &snapshot,
                    roots,
                    node_config_root,
                    node_trusted_keys_dir,
                    project_materialization,
                ) {
                    Ok(current) => current,
                    Err(error) => {
                        let error =
                            fill.fail(crate::dispatch_error::DispatchError::Internal(error));
                        return Err(anyhow::Error::new(
                            crate::dispatch_error::DispatchError::Shared(error),
                        ));
                    }
                };
                if !snapshot_is_current {
                    retries = retries.saturating_add(1);
                    if retries >= 3 {
                        let error = fill.fail(crate::dispatch_error::DispatchError::Internal(
                            anyhow::anyhow!(
                                "execution control authority changed repeatedly while loading"
                            ),
                        ));
                        return Err(anyhow::Error::new(
                            crate::dispatch_error::DispatchError::Shared(error),
                        ));
                    }
                    fill.cancel();
                    continue;
                }
                let estimated_bytes = snapshot.estimated_bytes();
                crate::resolved_config_cache::emit_metric(
                    key.namespace,
                    crate::resolved_config_cache::CacheOutcome::Miss,
                    crate::resolved_config_cache::CacheReason::Cold,
                    estimated_bytes,
                    0,
                );
                return Ok(fill.complete(snapshot, estimated_bytes));
            }
            crate::resolved_config_cache::Lookup::Bypass => {
                crate::resolved_config_cache::emit_metric(
                    key.namespace,
                    crate::resolved_config_cache::CacheOutcome::Bypass,
                    crate::resolved_config_cache::CacheReason::PendingCapacity,
                    0,
                    0,
                );
                let policy = ryeos_engine::execution_policy::ExecutionPolicyResolver::new(
                    ryeos_engine::config_loading::ConfigLoadContext {
                        roots,
                        parsers: &request_snapshot.parser_dispatcher,
                        kinds: &engine.kinds,
                        trust_store: &request_snapshot.trust_store,
                        project_authority: project_materialization.map(|materialization| {
                            (
                                materialization.path(),
                                materialization
                                    as &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
                            )
                        }),
                    },
                )
                .resolve_for_item(item_ref)?;
                let limits = load_limits_snapshot_under_current_authority(
                    roots,
                    node_config_root,
                    node_trusted_keys_dir,
                    project_materialization,
                    runtime_limits,
                )?;
                let snapshot = Arc::new(ExecutionControlSnapshot { policy, limits });
                if execution_control_snapshot_is_current(
                    &snapshot,
                    roots,
                    node_config_root,
                    node_trusted_keys_dir,
                    project_materialization,
                )? {
                    return Ok(snapshot);
                }
            }
        }
        retries = retries.saturating_add(1);
        if retries >= 3 {
            anyhow::bail!("execution control authority changed repeatedly while loading");
        }
    }
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

fn admitted_hook_dispatch_authorizations(
    plan: &ryeos_engine::hooks::EffectiveHookPlan,
) -> Result<Vec<HookDispatchAuthorization>> {
    plan.validate().map_err(|error| anyhow::anyhow!(error))?;
    let mut authorizations = Vec::new();
    for (layer, body) in plan.iter_layers() {
        for hook in &body.hooks {
            let contract = plan.event_contracts.get(&hook.event).ok_or_else(|| {
                anyhow::anyhow!("hook `{}` has no captured event contract", hook.id)
            })?;
            authorizations.push(HookDispatchAuthorization {
                owner_kind: plan.owner_kind.clone(),
                hook_id: hook.id.clone(),
                event: hook.event.clone(),
                layer,
                result_mode: hook.result,
                context_contract: contract.context_contract.clone(),
                dispatch_caps: body.dispatch_caps.clone(),
            });
        }
    }
    Ok(authorizations)
}

fn admitted_effect_dispatch_authorizations(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    effective_definition_digest: &str,
) -> Result<Vec<ryeos_effect_contract::AdmittedEffectAuthorization>> {
    let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_effect_contract::EFFECT_AUTHORIZATIONS_DERIVED_KEY)
    else {
        return Ok(Vec::new());
    };
    let projections = serde_json::from_value::<
        Vec<ryeos_effect_contract::EffectAuthorizationProjection>,
    >(value.clone())
    .context("decode admitted effect authorization projections")?;
    ryeos_effect_contract::validate_authorization_projections(&projections)?;
    projections
        .into_iter()
        .map(|projection| {
            let authorization = ryeos_effect_contract::AdmittedEffectAuthorization {
                authorization_id: projection.authorization_id,
                source_definition_ref: resolution.root.resolved_ref.clone(),
                source_effective_definition_digest: effective_definition_digest.to_string(),
                policy_digest: projection.policy_digest,
                action_contract_digest: projection.action_contract_digest,
                class: projection.class,
            };
            authorization.validate()?;
            Ok(authorization)
        })
        .collect()
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

/// How first admission reconciles freshly resolved capability sources, plus any
/// extra equality assertion a recovery caller carries. Once an admitted capsule
/// exists, its exact capability closure wins and live sources are never reopened.
#[derive(Clone, Copy)]
pub enum CapabilityPolicy<'a> {
    /// At first admission, run with exactly the union of live composed sources.
    /// Recovery/continuation instead preserves the admitted capsule verbatim.
    AdmissionDefault,
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
        CapabilityPolicy::AdmissionDefault => Ok(union_cap_sources(declared, runtime_manifest)),
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

/// Recover the exact capability closure sealed at first admission.
///
/// `ExactPinned` is an additional consistency assertion supplied by native
/// resume/continuation metadata. `AdmissionDefault` and `FollowChildHybrid`
/// have already been evaluated before the capsule was persisted, so replaying
/// either policy would re-enter mutable item/manifest authority and is
/// deliberately forbidden.
fn recover_admitted_effective_caps(
    admitted: &[String],
    policy: CapabilityPolicy<'_>,
    item_ref: &str,
) -> Result<Vec<String>, BuildAndLaunchError> {
    if let CapabilityPolicy::ExactPinned(captured) = policy {
        let admitted_set: BTreeSet<&str> = admitted.iter().map(String::as_str).collect();
        let captured_set: BTreeSet<&str> = captured.iter().map(String::as_str).collect();
        if admitted_set != captured_set {
            return Err(BuildAndLaunchError::CapabilityRejected {
                reason: format!(
                    "recovery capability identity for `{item_ref}` differs from its admitted \
                     execution capsule"
                ),
            });
        }
    }
    Ok(admitted.to_vec())
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
    effective_program: ryeos_engine::effective_program::FinalizedEffectiveProgram,
    prepared_launch: super::launch_preparation::PreparedRuntimeLaunch,
    effective_vault: HashMap<String, String>,
    effective_caps: Vec<String>,
    selected_runtime: ryeos_engine::runtime_registry::VerifiedRuntime,
    verified_protocol: ryeos_engine::protocols::VerifiedProtocol,
    materialized_executor: MaterializedExecutor,
    checkpoint_dir: Option<PathBuf>,
    is_resume: bool,
    launch_metadata: Option<ryeos_app::launch_metadata::RuntimeLaunchMetadata>,
    pending_project_snapshot: Option<super::CapturedProjectGeneration>,
    pending_executor_blob: Option<super::PendingCasPublication>,
    pending_external_realization: Option<super::PendingCasPublication>,
    pending_session_publications: Option<super::persistent_session::AdmittedSessionPublications>,
    bound_external_realizations: Option<super::external_content::BoundExternalRealizations>,
    augmentation_audits: Vec<crate::augmentations::LaunchAugmentationAudit>,
    /// True when this preparation minted the accounting scope (fresh
    /// admission) rather than copying a frozen one forward. Only a freshly
    /// minted scope may create ledger accounts — a missing account for a
    /// recovered scope is fail-closed, never re-created from limits.
    freshly_minted_accounting_scope: bool,
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

fn mint_budget_id(prefix: &str) -> String {
    let random_bytes: [u8; 16] = rand::random();
    let hex = lillux::sha256_hex(&random_bytes);
    format!("{prefix}-{}", &hex[..16])
}

/// Resolve the immutable accounting scope for a launch whose runtime declares
/// a financial authority.
///
/// - An already-admitted continuation or recovery carries its frozen scope
///   forward exactly (no allowance reset).
/// - A paid callback-dispatched descendant inherits the parent's execution
///   budget authority and mints only its own narrower directive-item scope;
///   a scope from another accounting authority site or ledger epoch is
///   rejected before child admission.
/// - A fresh top-level execution mints a new execution budget identity from
///   the local accounting authority.
///
/// Runtimes without a financial authority get no scope; a paid launch with no
/// available accounting ledger fails closed here.
fn resolve_accounting_scope(
    params: &BuildAndLaunchParams<'_>,
    metadata_template: Option<&ryeos_app::launch_metadata::RuntimeLaunchMetadata>,
    prepared_launch: &super::launch_preparation::PreparedRuntimeLaunch,
) -> Result<(Option<ryeos_state::objects::AdmittedAccountingScope>, bool), BuildAndLaunchError> {
    if let Some(existing) = metadata_template.and_then(|template| template.accounting_scope.clone())
    {
        // A recovered/continuation scope still requires the ledger whenever
        // the launch carries financial authority — otherwise the refusal
        // shifts from admission to the first reserve, burning a full
        // launch/relaunch cycle for the identical fail-closed outcome a
        // fresh launch gets right here.
        if let (None, Some(financial_authority)) = (
            params.state.accounting.as_ref(),
            prepared_launch.financial_authority.as_ref(),
        ) {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "accounting ledger is unavailable; a recovered launch carrying financial \
                 authority {} cannot be admitted",
                financial_authority.authority_digest
            )));
        }
        return Ok((Some(existing), false));
    }
    // EVERY managed execution owns an execution budget scope (plan §5.1) —
    // not just paying runtimes. A graph parent performs no direct paid work
    // yet its execution account is exactly what its paid descendants must
    // share; without it two parallel children each observe the full parent
    // maximum. The financial authority decides only whether a narrower
    // directive-item scope exists and whether a missing ledger is fatal.
    let accounting = match (
        params.state.accounting.as_ref(),
        &prepared_launch.financial_authority,
    ) {
        (Some(accounting), _) => accounting,
        (None, Some(financial_authority)) => {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "accounting ledger is unavailable; a launch carrying financial authority {} \
                 cannot be admitted",
                financial_authority.authority_digest
            )));
        }
        // No ledger and no direct paid work: run unscoped (a paid descendant
        // would fail its own admission on this node anyway).
        (None, None) => return Ok((None, false)),
    };
    let (site_id, ledger_epoch) = accounting.site_identity();
    let execution_budget_id = match params
        .parent_execution_context
        .and_then(|context| context.accounting_scope.as_ref())
    {
        Some(parent) => {
            if parent.budget_authority_site_id != site_id || parent.ledger_epoch != ledger_epoch {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "paid descendant is bound to accounting authority {}/{} but this node \
                     serves {}/{}; cross-site or cross-epoch paid fan-out is rejected before \
                     child admission",
                    parent.budget_authority_site_id,
                    parent.ledger_epoch,
                    site_id,
                    ledger_epoch
                )));
            }
            parent.execution_budget_id.clone()
        }
        None => mint_budget_id("B"),
    };
    // Only a runtime that pays directly gets a narrower directive-item
    // account; scope-owning non-paying runtimes (graph, knowledge) carry the
    // execution identity alone.
    let directive_budget_id = prepared_launch
        .financial_authority
        .as_ref()
        .map(|_| mint_budget_id("D"));
    Ok((
        Some(ryeos_state::objects::AdmittedAccountingScope {
            budget_authority_site_id: site_id,
            ledger_epoch,
            execution_budget_id,
            directive_budget_id,
        }),
        true,
    ))
}

fn capture_managed_descriptor_document(
    path: &Path,
    expected_content_hash: &str,
    expected_signer: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<String, BuildAndLaunchError> {
    let bytes = lillux::read_regular_file_bounded_no_follow(
        path,
        ryeos_state::objects::admitted_launch_capsule::MAX_ADMITTED_DESCRIPTOR_BYTES,
    )
    .map_err(|error| {
        BuildAndLaunchError::Internal(anyhow::anyhow!(
            "read admitted descriptor {}: {error}",
            path.display()
        ))
    })?;
    let document = String::from_utf8(bytes).map_err(|error| {
        BuildAndLaunchError::Internal(anyhow::anyhow!(
            "admitted descriptor {} is not UTF-8: {error}",
            path.display()
        ))
    })?;
    let header =
        lillux::signature::parse_signature_line(document.lines().next().unwrap_or(""), "#", None)
            .ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "admitted descriptor {} has no valid signature header",
                path.display()
            ))
        })?;
    let body = lillux::signature::strip_signature_lines(&document);
    let observed_hash = lillux::signature::content_hash(&body);
    if observed_hash != expected_content_hash
        || header.content_hash != expected_content_hash
        || header.signer_fingerprint != expected_signer
    {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "admitted descriptor {} contradicts its verified identity",
            path.display()
        )));
    }
    let signer = trust_store.get(expected_signer).ok_or_else(|| {
        BuildAndLaunchError::Internal(anyhow::anyhow!(
            "admitted descriptor signer is no longer trusted: {expected_signer}"
        ))
    })?;
    if !lillux::signature::verify_signature(
        expected_content_hash,
        &header.signature_b64,
        &signer.verifying_key,
    ) {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "admitted descriptor signature does not verify"
        )));
    }
    Ok(document)
}

fn verify_admitted_signed_descriptor_document(
    document: &str,
    expected_content_hash: &str,
    expected_signer: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<String, BuildAndLaunchError> {
    let header =
        lillux::signature::parse_signature_line(document.lines().next().unwrap_or(""), "#", None)
            .ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "admitted descriptor has no valid signature header"
            ))
        })?;
    let body = lillux::signature::strip_signature_lines(document);
    let observed_hash = lillux::signature::content_hash(&body);
    if observed_hash != expected_content_hash
        || header.content_hash != expected_content_hash
        || header.signer_fingerprint != expected_signer
    {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "admitted descriptor document contradicts its sealed identity"
        )));
    }
    let signer = trust_store.get(expected_signer).ok_or_else(|| {
        BuildAndLaunchError::Internal(anyhow::anyhow!(
            "admitted descriptor signer is no longer trusted: {expected_signer}"
        ))
    })?;
    if !lillux::signature::verify_signature(
        expected_content_hash,
        &header.signature_b64,
        &signer.verifying_key,
    ) {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "admitted descriptor signature no longer verifies"
        )));
    }
    Ok(body)
}

async fn prepare_managed_launch_authority(
    params: &BuildAndLaunchParams<'_>,
    thread_id: &str,
    metadata_template: Option<&ryeos_app::launch_metadata::RuntimeLaunchMetadata>,
) -> Result<PreparedManagedLaunchAuthority, BuildAndLaunchError> {
    let engine = params.provenance.request_engine();
    let subject_resolution_authority = params.provenance.subject_resolution_authority();
    let resolution_project_root = (!matches!(
        subject_resolution_authority,
        ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
    ))
    .then_some(params.project_path);
    let engine_roots = engine.resolution_roots(resolution_project_root.map(Path::to_path_buf));
    let bundle_roots: Vec<PathBuf> = engine_roots
        .authoritative_bundle_roots()
        .map_err(|error| BuildAndLaunchError::Internal(anyhow::anyhow!(error)))?
        .into_iter()
        .map(Path::to_path_buf)
        .collect();
    let persisted_admitted_capsule = params
        .state
        .state_store
        .admitted_launch_capsule(thread_id)
        .map_err(BuildAndLaunchError::Internal)?;
    // A continuation can be prepared before its successor row exists. Its
    // immutable execution authority comes from the predecessor's CAS capsule,
    // never from the operational metadata seed. The seed is checked against
    // that authority before descriptor validation or credential access.
    let continuation_source = metadata_template
        .and_then(|metadata| metadata.continuation_source_thread_id.as_deref())
        .or(params.previous_thread_id);
    let inherited_admitted_capsule = if persisted_admitted_capsule.is_none() {
        continuation_source
            .map(|source_thread_id| {
                params
                    .state
                    .state_store
                    .admitted_launch_capsule(source_thread_id)
                    .map_err(BuildAndLaunchError::Internal)?
                    .ok_or_else(|| {
                        BuildAndLaunchError::Internal(anyhow::anyhow!(
                            "continuation source {source_thread_id} has no authoritative admitted launch capsule"
                        ))
                    })
            })
            .transpose()?
    } else {
        None
    };
    let authoritative_admitted_capsule = persisted_admitted_capsule
        .as_ref()
        .or(inherited_admitted_capsule.as_ref());
    if let (Some(authoritative), Some(template)) =
        (authoritative_admitted_capsule, metadata_template)
    {
        let template_capsule = template
            .admitted_launch_capsule()
            .map_err(BuildAndLaunchError::Internal)?
            .ok_or_else(|| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "continuation metadata seed has no admitted launch capsule"
                ))
            })?;
        if !authoritative
            .same_continuation_admission(&template_capsule)
            .map_err(BuildAndLaunchError::Internal)?
        {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "operational launch metadata differs from its authoritative CAS admission"
            )));
        }
    }
    let admitted_capsule = authoritative_admitted_capsule;
    // Inheritance: a fresh child seals its dispatching parent's exact
    // realization set unless it authors its own declaration. Resolved from
    // the parent's durable capsule — never from operational metadata — and
    // fail-closed: a dispatching parent without an admitted capsule is
    // broken lineage, not an empty inheritance. Recovered launches read
    // their own sealed set instead, so nothing is resolved here for them.
    let inherited_external_realizations = if admitted_capsule.is_none() {
        params
            .parent_execution_context
            .map(|parent| {
                params
                    .state
                    .state_store
                    .admitted_launch_capsule(&parent.parent_thread_id)
                    .map_err(BuildAndLaunchError::Internal)?
                    .ok_or_else(|| {
                        BuildAndLaunchError::Internal(anyhow::anyhow!(
                            "dispatching parent {} has no authoritative admitted launch capsule",
                            parent.parent_thread_id
                        ))
                    })?
                    .external_realization_set()
                    .map_err(BuildAndLaunchError::Internal)
            })
            .transpose()?
            .flatten()
    } else {
        None
    };
    let root_admission = params.resolved.root_admission.as_ref().ok_or_else(|| {
        BuildAndLaunchError::Internal(anyhow::anyhow!(
            "managed launch is missing exact admitted resolution authority"
        ))
    })?;
    let recovery_trust_store = admitted_capsule
        .is_some()
        .then(|| root_admission.current_policy_trust_store())
        .transpose()
        .map_err(BuildAndLaunchError::Internal)?;
    let effective_request_snapshot = if admitted_capsule.is_some() {
        None
    } else {
        Some(
            match root_admission.admitted_request_snapshot() {
                Some(admitted) => engine.effective_request_snapshot_under_admitted_authority(
                    resolution_project_root.ok_or_else(|| {
                        anyhow::anyhow!(
                            "admitted pinned request snapshot has no execution project root"
                        )
                    })?,
                    admitted,
                ),
                None if subject_resolution_authority
                    .operational_generation()
                    .is_some() =>
                {
                    return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "fresh content-addressed managed launch has no admitted request snapshot"
                    )));
                }
                None => engine
                    .effective_request_snapshot(
                        resolution_project_root,
                        &subject_resolution_authority,
                    )
                    .map(Arc::new),
            }
            .map_err(|error| anyhow::anyhow!("effective request snapshot: {error}"))?,
        )
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

    let (selected_runtime, verified_protocol, admitted_prepared_launch) = if let Some(capsule) =
        admitted_capsule.as_ref()
    {
        let ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
            runtime_ref,
            runtime_content_hash,
            runtime_signer_fingerprint,
            protocol_ref,
            protocol_content_hash,
            protocol_signer_fingerprint,
            ..
        } = &capsule.artifact_identity
        else {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed recovery found a non-managed admitted artifact identity"
            )));
        };
        let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
            prepared_runtime_launch,
            runtime_descriptor_document,
            protocol_descriptor_document,
            executor_blob_hash: _,
        } = &capsule.execution_closure
        else {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed recovery found a non-managed admitted execution closure"
            )));
        };
        for (label, signer) in [
            ("runtime", runtime_signer_fingerprint),
            ("protocol", protocol_signer_fingerprint),
        ] {
            if !engine.node_trust_store.is_trusted(signer) {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "admitted managed {label} signer is no longer trusted: {signer}"
                )));
            }
        }
        let runtime_body = verify_admitted_signed_descriptor_document(
            runtime_descriptor_document,
            runtime_content_hash,
            runtime_signer_fingerprint,
            &engine.node_trust_store,
        )?;
        let runtime_yaml: ryeos_engine::runtime_registry::RuntimeYaml =
            serde_yaml::from_str(&runtime_body).map_err(|error| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "decode admitted runtime descriptor: {error}"
                ))
            })?;
        let canonical_runtime_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(runtime_ref)
            .map_err(|error| {
            BuildAndLaunchError::Internal(anyhow::anyhow!("decode admitted runtime ref: {error}"))
        })?;
        ryeos_engine::runtime_registry::validate_admitted_runtime_descriptor(
            &canonical_runtime_ref,
            &runtime_yaml,
        )
        .map_err(|error| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "validate admitted runtime descriptor: {error}"
            ))
        })?;
        let selected_runtime = ryeos_engine::runtime_registry::VerifiedRuntime {
            canonical_ref: canonical_runtime_ref,
            raw_content_digest: runtime_content_hash.clone(),
            signer_fingerprint: runtime_signer_fingerprint.clone(),
            yaml: runtime_yaml,
            trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
            bundle_root: PathBuf::new(),
            descriptor_path: PathBuf::new(),
        };
        let protocol_body = verify_admitted_signed_descriptor_document(
            protocol_descriptor_document,
            protocol_content_hash,
            protocol_signer_fingerprint,
            &engine.node_trust_store,
        )?;
        let descriptor: ryeos_engine::protocols::ProtocolDescriptor =
            serde_yaml::from_str(&protocol_body).map_err(|error| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "decode admitted protocol descriptor: {error}"
                ))
            })?;
        ryeos_engine::protocols::validate_admitted_protocol_descriptor(protocol_ref, &descriptor)
            .map_err(|error| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "validate admitted protocol descriptor: {error}"
            ))
        })?;
        let verified_protocol = ryeos_engine::protocols::VerifiedProtocol {
            canonical_ref: protocol_ref.clone(),
            raw_content_digest: protocol_content_hash.clone(),
            signer_fingerprint: protocol_signer_fingerprint.clone(),
            descriptor,
            trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
            bundle_root: PathBuf::new(),
            descriptor_path: PathBuf::new(),
        };
        crate::dispatch::validate_admitted_callback_runtime_protocol(
            &verified_protocol,
            &selected_runtime.canonical_ref,
        )
        .map_err(BuildAndLaunchError::from)?;
        let prepared = serde_json::from_value::<super::launch_preparation::PreparedRuntimeLaunch>(
            prepared_runtime_launch.clone(),
        )
        .map_err(|error| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "decode admitted prepared runtime launch: {error}"
            ))
        })?;
        (selected_runtime, verified_protocol, Some(prepared))
    } else {
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
        let verified_protocol = crate::dispatch::require_callback_runtime_protocol(
            engine,
            &selected_runtime,
            "managed",
        )
        .map_err(|error| BuildAndLaunchError::Internal(anyhow::anyhow!(error)))?
        .clone();
        (selected_runtime, verified_protocol, None)
    };
    let runtime_binary =
        crate::dispatch::strip_binary_ref_prefix(&selected_runtime.yaml.binary_ref)
            .map_err(|error| BuildAndLaunchError::Internal(anyhow::anyhow!(error)))?;
    if selected_runtime.yaml.serves != params.resolved.resolved_item.kind {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "admitted runtime '{}' serves kind '{}', not launched kind '{}'",
            selected_runtime.canonical_ref,
            selected_runtime.yaml.serves,
            params.resolved.resolved_item.kind,
        )));
    }
    let executor_ref = format!("native:{runtime_binary}");
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
    let admitted_executor_cas_root = params
        .state
        .state_store
        .cas_root()
        .map_err(BuildAndLaunchError::Internal)?;
    let materialization_isolation = Arc::clone(&params.state.isolation);
    let materialization_timings = params.launch_timings.clone();
    let admitted_executor_identity = admitted_capsule
        .as_ref()
        .map(|capsule| match &capsule.artifact_identity {
            ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
                executor_ref,
                executor_content_hash,
                executor_bundle_manifest_hash,
                executor_bundle_signer_fingerprint,
                ..
            } => {
                let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
                    executor_blob_hash,
                    ..
                } = &capsule.execution_closure
                else {
                    return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "managed recovery found a non-managed admitted execution closure"
                    )));
                };
                Ok((
                    executor_ref.clone(),
                    executor_content_hash.clone(),
                    executor_bundle_manifest_hash.clone(),
                    executor_bundle_signer_fingerprint.clone(),
                    executor_blob_hash.clone(),
                ))
            }
            ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor { .. } => {
                Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "managed recovery found a non-managed admitted artifact identity"
                )))
            }
        })
        .transpose()?;
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
        if let Some((
            admitted_executor_ref,
            admitted_content_hash,
            admitted_manifest_hash,
            admitted_signer_fingerprint,
            admitted_blob_hash,
        )) = admitted_executor_identity
        {
            if admitted_executor_ref != materialization_executor_ref {
                return Err(MaterializationError::ResolutionFailed {
                    executor_ref: materialization_executor_ref,
                    detail: format!(
                        "installed executor ref differs from admitted managed executor {admitted_executor_ref}"
                    ),
                });
            }
            ensure_admitted_executor_signer_trusted(
                &materialization_engine.node_trust_store,
                &admitted_executor_ref,
                &admitted_signer_fingerprint,
            )?;
            if admitted_blob_hash != admitted_content_hash {
                return Err(MaterializationError::MaterializationFailed {
                    executor_ref: admitted_executor_ref,
                    detail: "admitted executor blob contradicts artifact identity".to_string(),
                });
            }
            materialize_admitted_native_executor(
                &admitted_executor_ref,
                &admitted_executor_cas_root,
                materialization_isolation.as_ref(),
                &admitted_content_hash,
                &admitted_manifest_hash,
                &admitted_signer_fingerprint,
            )
            .map(|materialized| (materialized, None))
        } else {
            let attestation = verify_native_executor_chain_attestation_for_engine(
                &materialization_engine,
                &materialization_bundle_roots,
                &materialization_executor_ref,
                ryeos_engine::resolution::TrustClass::TrustedBundle,
                materialization_timings.as_ref(),
            )?;
            let materialized = materialize_native_executor_for_engine(
                &materialization_engine,
                &materialization_bundle_roots,
                &materialization_executor_ref,
                &materialization_cache_root,
                ryeos_engine::resolution::TrustClass::TrustedBundle,
                materialization_timings.as_ref(),
            )?;
            if !attestation.matches_materialized(&materialized) {
                return Err(MaterializationError::MaterializationFailed {
                    executor_ref: materialization_executor_ref,
                    detail: "materialized executor differs from its verified chain attestation"
                        .to_string(),
                });
            }
            Ok((materialized, Some(attestation)))
        }
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
            if let Some(exec) = launching_kind_schema.execution()
                && !exec.launch_augmentations.is_empty()
            {
                let augmentation_timer = params
                    .launch_timings
                    .as_ref()
                    .map(|timings| timings.nested("background_dispatch", "launch_augmentation"));
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
                    params
                        .resolved
                        .root_admission
                        .as_ref()
                        .and_then(|admission| admission.admitted_request_snapshot()),
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
        Ok::<_, BuildAndLaunchError>(materialized)
    };
    let (augmentation_result, materialization_result) = tokio::join!(augmentation, materialization);
    let concurrent_prerequisites_succeeded =
        augmentation_result.is_ok() && materialization_result.is_ok();
    let augmentation_audits = augmentation_result?;
    let (materialized_executor, executor_chain_attestation) = materialization_result?;
    debug_assert!(
        concurrent_prerequisites_succeeded,
        "runtime preparation must remain strictly after augmentation and executor materialization join"
    );

    if let Some(timings) = params.launch_timings.as_ref() {
        timings.mark("runtime_prep_started");
    }
    let mut prepared_launch = if let Some(prepared) = admitted_prepared_launch {
        prepared
    } else {
        let runtime_preparation_timer = params
            .launch_timings
            .as_ref()
            .map(|timings| timings.nested("background_dispatch", "runtime_preparation"));
        let request_snapshot = effective_request_snapshot.as_deref().ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "fresh managed launch has no effective request snapshot"
            ))
        })?;
        let executor_chain_identity = executor_chain_attestation
            .as_ref()
            .ok_or_else(|| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "fresh managed launch has no verified executor-chain attestation"
                ))
            })?
            .identity_digest()
            .map_err(BuildAndLaunchError::from)?;
        let binding_generation_identity = engine.registered_bundle_generation_fingerprint();
        let binding_plan_context_identity = [
            request_snapshot.request_engine_generation_identity.as_str(),
            request_snapshot.registry_fingerprint.as_str(),
            request_snapshot.effective_trust_identity.as_str(),
        ]
        .join("\u{1f}");
        let binding_materialization = params
            .resolved
            .root_admission
            .as_ref()
            .ok_or_else(|| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "fresh managed launch has no admitted project materialization"
                ))
            })?
            .resolution_materialization_binding()
            .map_err(BuildAndLaunchError::Internal)?;
        let prepared = super::launch_preparation::prepare_runtime_launch_cached(
            super::launch_preparation::PrepareRuntimeLaunchRequest {
                engine,
                runtime: &selected_runtime,
                primary: &resolution,
                ref_bindings: &params.resolved.ref_bindings,
                roots: &engine_roots,
                parsers: &request_snapshot.parser_dispatcher,
                trust_store: &request_snapshot.trust_store,
                principal: &params.resolved.plan_context.requested_by,
                subject_resolution_authority: &subject_resolution_authority,
                resolution_cache: Some(super::launch_preparation::PreparedResolutionCacheContext {
                    cache: &params.state.resolution_cache,
                    materialization: &binding_materialization,
                    generation_identity: &binding_generation_identity,
                    plan_context_identity: &binding_plan_context_identity,
                }),
                ref_binding_resolution_timings: params.launch_timings.as_ref(),
            },
            super::launch_preparation::PreparedLaunchSkeletonAuthority {
                subject_resolution_authority: &subject_resolution_authority,
                execution_project_authority: params.provenance.project_authority(),
                lifecycle_authority: &params.lifecycle_authority,
                protocol: &verified_protocol,
                executor_chain_identity: &executor_chain_identity,
                request_engine_generation_identity: &request_snapshot
                    .request_engine_generation_identity,
                effective_trust_identity: &request_snapshot.effective_trust_identity,
            },
        )
        .await
        .map_err(BuildAndLaunchError::from);
        drop(runtime_preparation_timer);
        prepared?
    };
    let current_trust_store = match (
        recovery_trust_store.as_ref(),
        effective_request_snapshot.as_deref(),
    ) {
        (Some(trust), None) => trust,
        (None, Some(snapshot)) => &snapshot.trust_store,
        _ => {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed launch current-policy trust authority is ambiguous"
            )));
        }
    };
    super::admitted_trust::validate_managed_current_trust(
        engine,
        current_trust_store,
        &resolution,
        &prepared_launch,
    )
    .map_err(BuildAndLaunchError::Internal)?;
    let pending_session_publications =
        super::persistent_session::admit_or_verify_prepared_sessions(
            params.state,
            engine,
            &mut prepared_launch,
            admitted_capsule.is_some(),
        )
        .map_err(BuildAndLaunchError::Internal)?;
    let effective_caps = if let Some(capsule) = admitted_capsule.as_ref() {
        // Capability authority is part of the admitted execution closure.
        // Recovery must not reopen the composed item or its runtime-authority
        // manifest: either could have changed or disappeared after admission.
        // The launch mode may assert an exact captured set, but it cannot
        // derive, mint, or widen the capsule's sealed authority.
        recover_admitted_effective_caps(
            &capsule.effective_caps,
            params.capability_policy,
            &params.resolved.item_ref,
        )?
    } else {
        let composed_effective_caps = derive_effective_caps(&resolution.composed);
        ryeos_bundle::runtime_authority::reject_disallowed_composed_grants(
            &composed_effective_caps,
        )
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
        apply_capability_policy(
            composed_effective_caps,
            runtime_capability_caps,
            params.capability_policy,
            &params.resolved.item_ref,
            &child_execute_cap,
        )?
    };

    // Capture the complete hook policy after every declared augmentation and
    // capability derivation, then lock/validate/finalize the exact resolution
    // before any capsule, callback token, or runtime envelope can exist.
    let (effective_program, pending_external_realization) = if admitted_capsule.is_some() {
        let hooks = engine
            .kinds
            .get(&params.resolved.resolved_item.kind)
            .and_then(|schema| schema.execution.as_ref())
            .and_then(|execution| execution.hooks.as_ref())
            .ok_or_else(|| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "recovered hook-capable launch has no signed kind hook contract"
                ))
            })?;
        let plan_value = resolution
            .composed
            .derived
            .get(&hooks.plan_derived)
            .ok_or_else(|| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "recovered effective program has no captured `{}` plan",
                    hooks.plan_derived
                ))
            })?;
        let plan = ryeos_engine::hooks::EffectiveHookPlan::from_value(plan_value)
            .map_err(|error| BuildAndLaunchError::Internal(anyhow::anyhow!(error)))?;
        super::effective_program_projection::validate_captured_hook_plan_pre_spawn(&plan)
            .map_err(BuildAndLaunchError::from)?;
        super::admitted_trust::validate_hook_plan_current_trust(engine, current_trust_store, &plan)
            .map_err(BuildAndLaunchError::Internal)?;

        let recovered_external =
            ryeos_app::external_content_admission::recover_external_realizations(
                params.state,
                &resolution,
            )
            .map_err(BuildAndLaunchError::Internal)?;
        let validation = engine
            .effective_validators
            .validate(&params.resolved.resolved_item.kind, &resolution)
            .map_err(BuildAndLaunchError::from)?;
        let candidate = ryeos_engine::effective_program::lock_validated_effective_program(
            resolution, validation,
        )
        .map_err(BuildAndLaunchError::from)?;
        let finalization_materialization = params
            .resolved
            .root_admission
            .as_ref()
            .map(|admission| admission.resolution_materialization_binding())
            .transpose()
            .map_err(BuildAndLaunchError::Internal)?;
        let finalization_project = finalization_materialization
            .as_ref()
            .map(|binding| binding.authoritative_project_content())
            .transpose()
            .map_err(BuildAndLaunchError::Internal)?
            .flatten()
            .map(|(root, content)| {
                (
                    root,
                    content as &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
                )
            });
        let finalization_proof = ryeos_engine::effective_program::prove_finalization_authority(
            &candidate,
            &[],
            &engine_roots,
            finalization_project,
            recovered_external
                .as_ref()
                .map(|captured| captured.finalization_evidence()),
            None,
        )
        .map_err(BuildAndLaunchError::from)?;
        (
            ryeos_engine::effective_program::finalize_effective_program(
                candidate,
                finalization_proof,
            )
            .map_err(BuildAndLaunchError::from)?,
            None,
        )
    } else {
        let request_snapshot = effective_request_snapshot.as_deref().ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "fresh hook capture has no effective request snapshot"
            ))
        })?;
        let materialization = params
            .resolved
            .root_admission
            .as_ref()
            .ok_or_else(|| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "fresh hook capture has no root admission"
                ))
            })?
            .resolution_materialization_binding()
            .map_err(BuildAndLaunchError::Internal)?;
        super::effective_program_projection::capture_and_finalize_fresh_effective_program(
            params.state,
            engine,
            &params.resolved.resolved_item.kind,
            resolution,
            &effective_caps,
            &engine_roots,
            &request_snapshot.parser_dispatcher,
            &request_snapshot.trust_store,
            Some(&materialization),
            inherited_external_realizations.as_ref(),
        )
        .map_err(BuildAndLaunchError::from)?
    };
    let bound_external_realizations =
        super::external_content::bind_external_realizations_for_execution(
            params.state,
            effective_program.resolution(),
            params.project_path,
            params.provenance.project_authority(),
        )
        .map_err(BuildAndLaunchError::Internal)?;
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
    if persisted_admitted_capsule.is_some() {
        params
            .state
            .state_store
            .verify_admitted_artifact_identity(thread_id, &admitted_artifact_identity)
            .map_err(BuildAndLaunchError::Internal)?;
    } else if let Some(persisted) = metadata_template
        && let Some(expected) = persisted.admitted_artifact_identity.as_ref()
        && expected != &admitted_artifact_identity
    {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "installed runtime/protocol/executor identity no longer matches the admitted launch capsule: admitted={expected:?}, installed={admitted_artifact_identity:?}"
        )));
    }
    let (executor_blob_hash, pending_executor_blob) =
        if let Some(capsule) = admitted_capsule.as_ref() {
            let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
                executor_blob_hash,
                ..
            } = &capsule.execution_closure
            else {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "managed recovery found a non-managed admitted execution closure"
                )));
            };
            (executor_blob_hash.clone(), None)
        } else {
            let (hash, publication) =
                stage_managed_executor_blob(params.state, &materialized_executor)?;
            (hash, Some(publication))
        };
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
        error @ VaultReadError::AuthorityViolation(_) => {
            BuildAndLaunchError::Internal(anyhow::anyhow!("vault read refused: {error}"))
        }
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
    let freshly_minted_accounting_scope;
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
            .with_admitted_execution_closure(if let Some(capsule) = admitted_capsule.as_ref() {
                capsule.execution_closure.clone()
            } else {
                ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
                    prepared_runtime_launch: serde_json::to_value(&prepared_launch).map_err(
                        |error| {
                            BuildAndLaunchError::Internal(anyhow::anyhow!(
                                "serialize admitted prepared launch: {error}"
                            ))
                        },
                    )?,
                    runtime_descriptor_document: capture_managed_descriptor_document(
                        &selected_runtime.descriptor_path,
                        &selected_runtime.raw_content_digest,
                        &selected_runtime.signer_fingerprint,
                        &engine.node_trust_store,
                    )?,
                    protocol_descriptor_document: capture_managed_descriptor_document(
                        &verified_protocol.descriptor_path,
                        &verified_protocol.raw_content_digest,
                        &verified_protocol.signer_fingerprint,
                        &engine.node_trust_store,
                    )?,
                    executor_blob_hash: executor_blob_hash.clone(),
                }
            })
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
        let (scope, minted) =
            resolve_accounting_scope(params, metadata_template, &prepared_launch)?;
        if let Some(scope) = scope {
            metadata = metadata.with_accounting_scope(scope);
        }
        freshly_minted_accounting_scope = minted;
        Some(metadata)
    };
    Ok(PreparedManagedLaunchAuthority {
        effective_program,
        prepared_launch,
        effective_vault,
        effective_caps,
        selected_runtime,
        verified_protocol,
        materialized_executor,
        checkpoint_dir,
        is_resume,
        launch_metadata,
        pending_project_snapshot,
        pending_executor_blob,
        pending_external_realization,
        pending_session_publications: Some(pending_session_publications),
        bound_external_realizations,
        augmentation_audits,
        freshly_minted_accounting_scope,
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

/// Admission-evidence wrapper: exactly one emission seam for every real
/// managed-launch attempt. Success appends `admission_recorded` (implies a
/// sealed capsule and a spawned thread); failure appends `admission_refused`
/// with a closed stage. Emission failure is an evidence gap logged as a
/// warning — it never alters the launch outcome. Preview/projection paths do
/// not pass through here and never append.
pub async fn build_and_launch(
    params: BuildAndLaunchParams<'_>,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    let state = params.state;
    let project_path = params.project_path.to_path_buf();
    let canonical_ref = params.resolved.item_ref.clone();
    let acting_principal = params.acting_principal.to_string();
    let outcome = build_and_launch_inner(params).await;
    let emission = match &outcome {
        Ok(result) => {
            let text = |key: &str| {
                result
                    .thread
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            ryeos_app::admission_events::append_admission_recorded(
                &state.state_store,
                ryeos_app::admission_events::AdmissionRecorded {
                    project_path: &project_path,
                    canonical_ref: &canonical_ref,
                    thread_id: &text("thread_id"),
                    chain_root_id: &text("chain_root_id"),
                    root_raw_content_digest: &text("root_raw_content_digest"),
                    effective_definition_digest: &text("effective_definition_digest"),
                    admitted_launch_capsule_hash: &text("admitted_launch_capsule_hash"),
                    acting_principal: &acting_principal,
                },
            )
        }
        Err(error) => {
            let (stage, reason_code) = admission_stage_for(error);
            ryeos_app::admission_events::append_admission_refused(
                &state.state_store,
                ryeos_app::admission_events::AdmissionRefused {
                    project_path: &project_path,
                    canonical_ref: &canonical_ref,
                    stage,
                    reason_code: &reason_code,
                    detail: &error.to_string(),
                    acting_principal: &acting_principal,
                },
            )
        }
    };
    if let Err(error) = emission {
        tracing::warn!(%error, "admission evidence emission failed; launch outcome unaffected");
    }
    outcome
}

/// Exhaustive mapping from launch failure variants to admission refusal
/// stages; adding a variant requires choosing its stage here.
fn admission_stage_for(
    error: &BuildAndLaunchError,
) -> (ryeos_app::admission_events::AdmissionStage, String) {
    use ryeos_app::admission_events::AdmissionStage as Stage;
    match error {
        BuildAndLaunchError::Materialization(_) => {
            (Stage::Materialization, "materialization_failed".to_string())
        }
        BuildAndLaunchError::MissingSecrets { .. } => {
            (Stage::Secrets, "missing_secrets".to_string())
        }
        BuildAndLaunchError::CapabilityRejected { .. } => {
            (Stage::Authority, "capability_rejected".to_string())
        }
        BuildAndLaunchError::LaunchPreparation(inner) => {
            let code = match inner.as_ref() {
                DispatchError::LaunchPreparationFailed { code, .. } => code.clone(),
                _ => "launch_preparation_failed".to_string(),
            };
            (Stage::Preparation, code)
        }
        BuildAndLaunchError::LaunchCancelled { stage, .. } => {
            (Stage::Cancelled, format!("cancelled_before_{stage}"))
        }
        BuildAndLaunchError::Internal(_) => (Stage::Internal, "internal".to_string()),
    }
}

async fn build_and_launch_inner(
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
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::capture_finalized(
            params.resolved,
            authority.selected_runtime.canonical_ref.to_string(),
            &authority.effective_program,
        )?;
    authority
        .launch_metadata
        .get_or_insert_with(Default::default)
        .set_sealed_root_request(sealed_request);
    let realization_contract_ref = authority.selected_runtime.canonical_ref.to_string();
    let realization_contract_digest = authority.selected_runtime.raw_content_digest.clone();
    let realization_admission = super::execution_realization::admit_or_verify(
        params.state,
        authority.launch_metadata.as_ref().ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "managed launch lost its admitted metadata"
            ))
        })?,
        authority.effective_program.resolution(),
        authority
            .effective_program
            .effective_definition_digest()
            .as_str(),
        &realization_contract_ref,
        &realization_contract_digest,
        authority.pending_external_realization.as_mut(),
    )
    .map_err(BuildAndLaunchError::Internal)?;
    if authority.pending_external_realization.is_none() {
        authority.pending_external_realization = realization_admission.publication;
    }
    authority.launch_metadata = authority
        .launch_metadata
        .take()
        .map(|metadata| metadata.with_execution_realization_hash(realization_admission.hash));

    let initial_events = launch_audit_records(
        params.resolved,
        authority.effective_program.resolution(),
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
    drop(authority.pending_executor_blob.take());
    drop(authority.pending_external_realization.take());
    if let Some(publications) = authority.pending_session_publications.take() {
        publications
            .publish()
            .map_err(BuildAndLaunchError::Internal)?;
    }
    let result = run_claimed_thread_row_with_authority(
        params,
        thread,
        authority,
        LaunchAuditDisposition::CommittedAtBirth,
    )
    .await;
    drop(_launch_claim);
    result
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

fn inventory_ref_is_authorized(
    item_ref: &ryeos_engine::canonical_ref::CanonicalRef,
    admission: Option<&ryeos_engine::kind_registry::InventoryAdmissionPolicy>,
    effective_caps: &[String],
) -> bool {
    let Some(admission) = admission else {
        return true;
    };
    let required = admission.required_capability(item_ref);
    effective_caps
        .iter()
        .any(|granted| ryeos_runtime::cap_matches(granted, &required))
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
        effective_program,
        prepared_launch,
        effective_vault,
        effective_caps,
        selected_runtime,
        verified_protocol,
        materialized_executor: materialized_binary,
        checkpoint_dir,
        is_resume,
        launch_metadata,
        pending_project_snapshot,
        pending_executor_blob,
        pending_external_realization,
        pending_session_publications,
        bound_external_realizations,
        augmentation_audits,
        freshly_minted_accounting_scope,
    } = authority;
    let thread_id = thread.thread_id.clone();
    let owns_workspace = !provenance.is_borrowed_child()
        && provenance.project_authority().requires_project_foldback();
    super::runner::bind_owned_workspace_after_thread_birth(
        state,
        provenance,
        &thread_id,
        launch_owner,
    )
    .map_err(BuildAndLaunchError::Internal)?;
    let resolution = effective_program.resolution();
    let post_publication_timer = launch_timings
        .as_ref()
        .map(|timings| timings.top_level("post_publication_launch_setup"));
    let accounting_scope = launch_metadata
        .as_ref()
        .and_then(|metadata| metadata.accounting_scope.clone());
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
    // Authoritative chain root from the freshly-created thread row (a successor
    // inherits its source's root; a fresh launch is its own root). Used to set
    // the callback cap's chain root.
    let chain_root_id = thread.chain_root_id.clone();
    // Recovery identity is a birth invariant. Fresh roots and continuations
    // seed it atomically before becoming visible; existing-row attempts may
    // recompute launch authority and append a new audit, but never rewrite the
    // persisted identity that selected this row.
    drop(pending_project_snapshot);
    drop(pending_executor_blob);
    drop(pending_external_realization);
    drop(pending_session_publications);

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

    let subject_resolution_authority = provenance.subject_resolution_authority();
    let resolution_project_root = (!matches!(
        subject_resolution_authority,
        ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
    ))
    .then_some(project_path);
    let engine_roots = engine.resolution_roots(resolution_project_root.map(Path::to_path_buf));
    let effective_request_snapshot = match resolved
        .root_admission
        .as_ref()
        .and_then(|admission| admission.admitted_request_snapshot())
    {
        Some(admitted) => engine.effective_request_snapshot_under_admitted_authority(
            resolution_project_root.ok_or_else(|| {
                anyhow::anyhow!("admitted pinned request snapshot has no execution project root")
            })?,
            admitted,
        ),
        None => engine
            .effective_request_snapshot(resolution_project_root, &subject_resolution_authority)
            .map(Arc::new),
    }
    .map_err(|e| anyhow::anyhow!("effective request snapshot: {e}"))?;

    // 2. Compute limits (root execution: depth = 0)
    let root_item_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(&resolved.item_ref)
        .map_err(|e| anyhow::anyhow!("build_and_launch: invalid root item ref: {e}"))?;
    let node_trusted_keys_dir = state.config.runtime_root().trusted_keys_dir();
    let node_config_root = engine.node_config_root();
    let execution_controls = load_execution_control_snapshot_cached(
        engine,
        &engine_roots,
        &effective_request_snapshot,
        &root_item_ref,
        &subject_resolution_authority,
        node_config_root.as_deref(),
        &node_trusted_keys_dir,
        provenance.pinned_materialization(),
        &selected_runtime.yaml.limits,
    )
    .await
    .with_context(|| {
        format!(
            "loading execution controls for item {} in project {}",
            resolved.item_ref,
            project_path.display()
        )
    })?;
    let execution_policy = &execution_controls.policy;
    let limits_config = &execution_controls.limits.config;
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
        .authoritative_bundle_roots()?
        .into_iter()
        .map(Path::to_path_buf)
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
        apply_execution_policy_defaults(&limits_config.defaults, execution_policy);
    let base_limits = match limits_header {
        Some(v) if !v.is_null() => {
            merge_header_limits(&execution_defaults, v, &selected_runtime.yaml.limits)?
        }
        _ => execution_defaults,
    };
    let requested_limits = apply_execution_policy_item_overrides(&base_limits, execution_policy);
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
        &selected_runtime.yaml.limits,
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
                selected_runtime
                    .yaml
                    .limits
                    .config_identity
                    .as_deref()
                    .map(|identity| format!("{identity} config defaults or built-in default"))
                    .unwrap_or_else(|| "built-in default".to_string())
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
                selected_runtime
                    .yaml
                    .limits
                    .config_identity
                    .as_deref()
                    .map(|identity| format!("{identity} config defaults or built-in default"))
                    .unwrap_or_else(|| "built-in default".to_string())
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
        runtime_limits = %serde_json::to_string(&hard_limits.runtime)
            .unwrap_or_else(|_| "{}".to_string()),
        runtime_limit_caps = %serde_json::to_string(&limits_config.caps.runtime)
            .unwrap_or_else(|_| "{}".to_string()),
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
    let hook_contract = launching_kind_schema
        .execution
        .as_ref()
        .and_then(|execution| execution.hooks.as_ref())
        .ok_or_else(|| anyhow::anyhow!("managed kind `{kind}` has no hook contract"))?;
    let hook_plan = resolution
        .composed
        .derived
        .get(&hook_contract.plan_derived)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "finalized program omitted captured `{}` hook plan",
                hook_contract.plan_derived
            )
        })
        .and_then(|value| {
            ryeos_engine::hooks::EffectiveHookPlan::from_value(value)
                .map_err(|error| anyhow::anyhow!(error))
        })?;
    let hook_dispatch_authorizations = admitted_hook_dispatch_authorizations(&hook_plan)
        .context("project finalized hook plan into callback authority")?;
    let effect_dispatch_authorizations = admitted_effect_dispatch_authorizations(
        resolution,
        effective_program.effective_definition_digest().as_str(),
    )
    .context("project finalized effect grants into callback authority")?;
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
        Some(effective_program.effective_definition_digest().to_string()),
        serde_json::to_value(&hard_limits).unwrap_or(Value::Null),
        current_depth,
    );
    if !state
        .callback_tokens
        .set_hook_dispatch_authorizations(&cap.token, hook_dispatch_authorizations)
    {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "callback capability disappeared before hook authorization binding"
        )));
    }
    if !state
        .callback_tokens
        .set_effect_dispatch_authorizations(&cap.token, effect_dispatch_authorizations)
        .map_err(BuildAndLaunchError::Internal)?
    {
        return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
            "callback capability disappeared before effect authorization binding"
        )));
    }
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

    // 6a½. Financial admission. A finite hard spend limit is enforceable only
    // with a proven spend bound: paid-with-certificate or explicitly free.
    // An advisory-only route under a finite hard limit rejects before any
    // provider contact; a settled post-attempt threshold is never described
    // as hard.
    if !hard_limits.spend_usd.is_zero() {
        // A runtime that pays providers directly must hold a proven bound; a
        // non-paying runtime (graph/knowledge) may carry a finite limit that
        // its paid descendants enforce through the shared execution account —
        // each descendant's own admission re-checks eligibility.
        if let Some(authority) = prepared_launch.financial_authority.as_ref()
            && !authority.spend_bound.hard_spend_eligible()
        {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "hard spend limit {} requires a mechanically proven spend bound; this \
                 route's sealed financial authority is `{}` and is ineligible for hard \
                 spend",
                hard_limits.spend_usd.to_canonical_string(),
                authority.spend_bound.as_str()
            )));
        }
        if accounting_scope.is_none() || state.accounting.is_none() {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "hard spend limit {} requires the accounting ledger and a sealed accounting \
                 scope; refusing to launch",
                hard_limits.spend_usd.to_canonical_string()
            )));
        }
    }

    // 6a¾. Journaled account birth and the launch accounting gate. Only a
    // freshly minted scope may create accounts; a recovered or continued
    // scope requires its accounts to already exist (reservation fails closed
    // on a missing account — allowance is never re-minted from limits).
    // The gate must be open before the runtime can spawn; reserve/issue
    // callbacks require it and terminal fencing closes it atomically.
    if let (Some(scope), Some(accounting)) = (accounting_scope.as_ref(), state.accounting.as_ref())
    {
        let account_limit = (!hard_limits.spend_usd.is_zero()).then_some(hard_limits.spend_usd);
        // Journaled, crash-recoverable account birth. The scope is sealed in
        // launch metadata at thread birth, so a crash can land between seal
        // and birth; recovery may re-run birth ONLY while the identity has
        // zero ledger history — an identity WITH history and a missing
        // account is fail-closed (allowance is never re-minted from limits).
        let execution_exists = accounting
            .account_exists(
                &scope.execution_budget_id,
                "execution",
                &scope.execution_budget_id,
            )
            .map_err(|error| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "execution budget account lookup failed: {error:#}"
                ))
            })?;
        if !execution_exists {
            let may_create = freshly_minted_accounting_scope
                || !accounting
                    .execution_budget_has_history(&scope.execution_budget_id)
                    .map_err(|error| {
                        BuildAndLaunchError::Internal(anyhow::anyhow!(
                            "execution budget history lookup failed: {error:#}"
                        ))
                    })?;
            if !may_create {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "execution budget account {} is absent but its identity has ledger \
                     history; refusing to re-mint allowance",
                    scope.execution_budget_id
                )));
            }
            accounting
                .create_execution_account_prepared(
                    &scope.execution_budget_id,
                    &chain_root_id,
                    account_limit,
                )
                .map_err(|error| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "execution budget account birth failed: {error:#}"
                    ))
                })?;
        }
        accounting
            .activate_account(
                &scope.execution_budget_id,
                "execution",
                &scope.execution_budget_id,
            )
            .map_err(|error| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "execution budget account activation failed: {error:#}"
                ))
            })?;
        if let Some(directive_budget_id) = scope.directive_budget_id.as_ref() {
            let directive_exists = accounting
                .account_exists(
                    &scope.execution_budget_id,
                    "directive_item",
                    directive_budget_id,
                )
                .map_err(|error| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "directive budget account lookup failed: {error:#}"
                    ))
                })?;
            if !directive_exists {
                let may_create = freshly_minted_accounting_scope
                    || !accounting
                        .directive_budget_has_history(directive_budget_id)
                        .map_err(|error| {
                            BuildAndLaunchError::Internal(anyhow::anyhow!(
                                "directive budget history lookup failed: {error:#}"
                            ))
                        })?;
                if !may_create {
                    return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "directive budget account {directive_budget_id} is absent but its \
                         identity has ledger history; refusing to re-mint allowance"
                    )));
                }
                accounting
                    .create_directive_account_prepared(
                        &scope.execution_budget_id,
                        directive_budget_id,
                        account_limit,
                    )
                    .map_err(|error| {
                        BuildAndLaunchError::Internal(anyhow::anyhow!(
                            "directive budget account birth failed: {error:#}"
                        ))
                    })?;
            }
            accounting
                .activate_account(
                    &scope.execution_budget_id,
                    "directive_item",
                    directive_budget_id,
                )
                .map_err(|error| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "directive budget account activation failed: {error:#}"
                    ))
                })?;
        }
        // Bind the open gate to the exact daemon-resolved credential values
        // that will be handed to this paid runtime. The runtime never sees or
        // supplies this digest. `mark_issued` re-resolves the same declared
        // names and must reproduce it before the irreversible provider-contact
        // boundary.
        let credential_binding = prepared_launch
            .financial_authority
            .as_ref()
            .map(|financial| {
                let authority: ryeos_accounting::ProviderAccountingAuthority =
                    serde_json::from_value(financial.authority.clone()).map_err(|error| {
                        BuildAndLaunchError::Internal(anyhow::anyhow!(
                            "decode sealed financial authority for credential binding: {error}"
                        ))
                    })?;
                authority.validate().map_err(|error| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "validate sealed financial authority for credential binding: {error}"
                    ))
                })?;
                let secrets = prepared_launch
                    .required_secrets
                    .iter()
                    .map(|required| {
                        effective_vault
                            .get(&required.name)
                            .cloned()
                            .map(|value| (required.name.clone(), value))
                            .ok_or_else(|| {
                                BuildAndLaunchError::Internal(anyhow::anyhow!(
                                    "prepared financial launch secret `{}` disappeared before \
                                     accounting admission",
                                    required.name
                                ))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                ryeos_accounting::credential_binding_digest(
                    accounting.credential_binding_key(),
                    &authority,
                    &secrets,
                )
                .map_err(|error| {
                    BuildAndLaunchError::Internal(anyhow::anyhow!(
                        "seal launch credential binding: {error}"
                    ))
                })
            })
            .transpose()?;
        accounting
            .open_launch_gate_with_credential_binding(
                &thread_id,
                launch_owner,
                &scope.execution_budget_id,
                &chain_root_id,
                credential_binding.as_ref().map(|digest| digest.as_str()),
            )
            .map_err(|error| {
                BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "launch accounting gate could not be opened: {error:#}"
                ))
            })?;
        if !state
            .callback_tokens
            .set_accounting_scope(&cap.token, scope.clone())
        {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "callback capability disappeared before accounting-scope binding"
            )));
        }
    }

    // 6b. Build the authorized inventory the launching kind asked for. Refuse
    //     tools outside the already-sealed effective capability set before
    //     their descriptors are read or parsed. The runtime remains a pure
    //     consumer of `envelope.inventory` and keeps its defensive filter.
    let inventory_timer = launch_timings
        .as_ref()
        .map(|timings| timings.nested("post_publication_launch_setup", "inventory_build"));
    let inventory = ryeos_engine::inventory::build_inventory_for_launching_kind_filtered(
        launching_kind_schema,
        &engine.kinds,
        &engine_roots,
        &effective_request_snapshot.parser_dispatcher,
        |item_ref| {
            let admission = engine
                .kinds
                .get(&item_ref.kind)
                .and_then(|schema| schema.inventory_policy.admission.as_ref());
            inventory_ref_is_authorized(item_ref, admission, &effective_caps)
        },
    )
    .map_err(|e| anyhow::anyhow!("inventory build failed: {e}"))?;
    drop(inventory_timer);

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
        effective_program,
    )
    .runtime_data(prepared_launch.runtime_data.clone())
    .inventory(inventory)
    .financial_authority(
        prepared_launch
            .financial_authority
            .as_ref()
            .map(|authority| authority.authority.clone()),
    )
    .accounting_scope(accounting_scope.as_ref().map(|scope| {
        super::launch_envelope::EnvelopeAccountingScope {
            budget_authority_site_id: scope.budget_authority_site_id.clone(),
            ledger_epoch: scope.ledger_epoch,
            execution_budget_id: scope.execution_budget_id.clone(),
            directive_budget_id: scope.directive_budget_id.clone(),
        }
    }))
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
    let observation_declarations = selected_runtime.yaml.observability.child_records.clone();
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
    drop(post_publication_timer);
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
            observation_declarations: &observation_declarations,
            acting_principal: &acting_principal_owned,
            binary: &binary_path,
            project_path: &project_owned,
            project_authority: isolation_project_authority,
            live_access: isolation_live_access,
            state_root: isolation_state_root.as_deref(),
            workspace_lifeline: isolation_workspace_lifeline,
            owns_workspace,
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
            external_realizations: bound_external_realizations,
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
            return Ok(RecoveryRefusalOutcome::AlreadyClaimed);
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
    /// stimulus, preserve the execution's sealed admitted capability closure,
    /// and skip the auto-launch budget (an operator action is not autonomous).
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
/// it through `run_claimed_thread_row` with `previous_thread_id` set so the
/// runtime folds the chain. A MACHINE continuation injects no new stimulus.
///
/// Fire-and-forget from the daemon: the machine path `tokio::spawn`s this after
/// the source is settled `continued`, and reconcile calls it for crash recovery.
/// Takes `state` by value so the spawned task can own it. Blocks until the
/// successor reaches terminal (inside its detached task).
pub fn launch_successor<'a>(
    state: AppState,
    successor_id: &'a str,
) -> impl std::future::Future<Output = Result<SuccessorLaunchOutcome, BuildAndLaunchError>> + 'a {
    launch_successor_inner(state, successor_id, SuccessorMode::Machine, None, None)
}

/// Launch a pre-created OPERATOR follow-up successor (an existing `created` row
/// with a seeded `ResumeContext`). Claim-guarded like [`launch_successor`] but
/// injects the operator's input as the opening stimulus and does not pin caps or
/// consume the auto-launch budget. Used by the `threads/input` path after a
/// synchronous create-or-get, and to "ensure launch" a stranded `created`
/// operator successor on a duplicate submit.
pub fn launch_operator_successor<'a>(
    state: AppState,
    successor_id: &'a str,
) -> impl std::future::Future<Output = Result<SuccessorLaunchOutcome, BuildAndLaunchError>> + 'a {
    launch_successor_inner(state, successor_id, SuccessorMode::Operator, None, None)
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
            self.prepared.authority.effective_program.resolution(),
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
        drop(self.prepared.authority.pending_executor_blob.take());
        drop(self.prepared.authority.pending_external_realization.take());
        drop(self.prepared.authority.pending_session_publications.take());
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
        drop(self.prepared.authority.pending_executor_blob.take());
        drop(self.prepared.authority.pending_external_realization.take());
        drop(self.prepared.authority.pending_session_publications.take());
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
    fresh_launch_authority_digest: Option<String>,
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
            self.authority.effective_program.resolution(),
            &self.authority.prepared_launch,
            &self.authority.augmentation_audits,
        )
    }

    pub fn launch_metadata(&self) -> &ryeos_app::launch_metadata::RuntimeLaunchMetadata {
        &self.launch_metadata
    }

    pub fn verify_fresh_launch_authority_unchanged(&self) -> anyhow::Result<()> {
        let Some(expected) = self.fresh_launch_authority_digest.as_deref() else {
            return Ok(());
        };
        let observed = self
            .launch_metadata
            .admitted_launch_authority()?
            .ok_or_else(|| anyhow::anyhow!("fresh follow child lost its launch authority"))?
            .digest()?;
        if observed != expected {
            anyhow::bail!("fresh follow-child launch authority changed before durable birth");
        }
        Ok(())
    }

    /// Mark that `initial_audit_events` committed atomically with this fresh
    /// root. Re-driven pre-existing rows retain `AppendForAttempt` so the
    /// recomputed audit is appended before their spawn handoff.
    pub fn with_persisted_birth_audit(mut self) -> Self {
        self.launch_audit = LaunchAuditDisposition::CommittedAtBirth;
        drop(self.authority.pending_project_snapshot.take());
        drop(self.authority.pending_executor_blob.take());
        drop(self.authority.pending_external_realization.take());
        drop(self.authority.pending_session_publications.take());
        self
    }
}

/// Perform the complete generic authority pass for a fresh follow/detached
/// child before its row becomes observable.
pub async fn prepare_follow_child_launch(
    state: &AppState,
    thread_id: &str,
    launch_metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    admitted_request: ResolvedExecutionRequest,
    provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    parent_context: crate::dispatch::ParentExecutionContext,
) -> Result<PreparedFollowChildLaunch, BuildAndLaunchError> {
    prepare_follow_child_launch_inner(
        state,
        thread_id,
        launch_metadata,
        Some(admitted_request),
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
        None,
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
    fresh_admitted_request: Option<ResolvedExecutionRequest>,
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
    let (admitted_request, admitted_runtime_ref) = match fresh_admitted_request {
        Some(request) => {
            let request_authority = request
                .root_admission
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!("fresh follow child has no root admission authority")
                })?
                .project_authority();
            if request_authority != &resume.project_authority
                || request_authority != provenance.project_authority()
            {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "fresh follow-child admission, resume, and reconstructed project identities disagree"
                )));
            }
            let runtime_ref = resume.runtime_ref.clone().ok_or_else(|| {
                anyhow::anyhow!("fresh follow-child resume has no admitted runtime ref")
            })?;
            (request, runtime_ref)
        }
        None => {
            let sealed_request = launch_metadata
                .sealed_root_request
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "existing follow-child launch metadata has no finalized sealed root request"
                    )
                })?;
            if sealed_request.project_context() != &resume.project_context
                || sealed_request.project_authority() != &resume.project_authority
                || sealed_request.project_authority() != provenance.project_authority()
            {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "follow-child sealed, resume, and reconstructed project identities disagree"
                )));
            }
            let request = sealed_request
                .restore_for_reconstructed_provenance(
                    engine,
                    &ryeos_app::launch_metadata::daemon_thread_state_dir(
                        &state.config.app_root,
                        thread_id,
                    )
                    .join("launch-capsule"),
                    &provenance,
                )
                .context("restore follow-child sealed root request")?;
            (request, sealed_request.runtime_ref().to_string())
        }
    };
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
        || operational_resume.runtime_ref.as_deref() != Some(admitted_runtime_ref.as_str())
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
        effect_authority: None,
        finalized_direct: None,
    };

    let project_path = execution.provenance.effective_path().to_path_buf();
    let mut authority = prepare_managed_launch_authority(
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
    let (launch_metadata, prepared_resume, fresh_launch_authority_digest) =
        if capture_project_snapshot {
            let mut prepared = authority.launch_metadata.as_ref().cloned().ok_or_else(|| {
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
            let admitted_project_authority = execution
                .resolved
                .root_admission
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("fresh follow child lost its root admission"))?
                .project_authority();
            if admitted_project_authority != &prepared_resume.project_authority {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "fresh follow-child prepared resume authority differs from its admitted request"
                )));
            }
            // The inherited request is only the child's initial identity. A fresh
            // child may add launch-augmentation outputs (for example
            // `rendered_contexts`) during its authoritative pass; seal that
            // augmented resolution so a retry/relaunch receives the same runtime
            // envelope instead of reusing the parent's pre-augmentation view.
            let augmented_sealed_request =
                ryeos_app::thread_lifecycle::SealedRootExecutionRequest::capture_finalized(
                    &execution.resolved,
                    authority.selected_runtime.canonical_ref.to_string(),
                    &authority.effective_program,
                )?;
            prepared.set_sealed_root_request(augmented_sealed_request);
            let realization_contract_ref = authority.selected_runtime.canonical_ref.to_string();
            let realization_contract_digest = authority.selected_runtime.raw_content_digest.clone();
            let realization_admission = super::execution_realization::admit_or_verify(
                state,
                &prepared,
                authority.effective_program.resolution(),
                authority
                    .effective_program
                    .effective_definition_digest()
                    .as_str(),
                &realization_contract_ref,
                &realization_contract_digest,
                authority.pending_external_realization.as_mut(),
            )
            .map_err(BuildAndLaunchError::Internal)?;
            if authority.pending_external_realization.is_none() {
                authority.pending_external_realization = realization_admission.publication;
            }
            prepared = prepared.with_execution_realization_hash(realization_admission.hash);
            let prepared_launch_authority_digest = prepared
                .admitted_launch_authority()?
                .ok_or_else(|| anyhow::anyhow!("fresh follow child lost its launch authority"))?
                .digest()?;
            if prepared_launch_authority_digest != realization_admission.launch_authority_digest {
                return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                    "fresh follow-child metadata changed after execution-realization admission"
                )));
            }
            (
                prepared,
                prepared_resume,
                Some(realization_admission.launch_authority_digest),
            )
        } else {
            // An existing child already has an immutable durable birth record.
            // Re-drive may re-materialize its workspace, but it must not rewrite
            // either persisted copy of that identity.
            (launch_metadata.clone(), resume.clone(), None)
        };

    Ok(PreparedFollowChildLaunch {
        thread_id: thread_id.to_string(),
        resume_context: prepared_resume,
        parent_context,
        execution,
        launch_metadata,
        fresh_launch_authority_digest,
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
            self.prepared.authority.effective_program.resolution(),
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
        SuccessorMode::Operator => (
            false,
            CapabilityPolicy::AdmissionDefault,
            CheckpointResumeMode::None,
        ),
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
    let launch_metadata = authority
        .launch_metadata
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("successor authority produced no launch metadata"))?;
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

fn launch_successor_inner<'a>(
    state: AppState,
    successor_id: &'a str,
    mode: SuccessorMode,
    launch_handoff: Option<&'a LaunchHandoff>,
    prepared_successor: Option<PreparedSuccessorLaunch>,
) -> impl std::future::Future<Output = Result<SuccessorLaunchOutcome, BuildAndLaunchError>> + 'a {
    launch_successor_inner_with_claim(
        state,
        successor_id,
        mode,
        launch_handoff,
        prepared_successor,
        None,
    )
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
/// `ResumeContext` and run the existing row. `mode` selects stimulus and an
/// optional exact-capability assertion; every successor keeps the capability
/// closure sealed by the admitted capsule.
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

    // Machine: fold the chain with NO new stimulus and assert the predecessor's
    // captured caps equal the capsule. Operator: inject the seeded input while
    // preserving that same sealed capability closure.
    let (suppress_stimulus, capability_policy) = match mode {
        // Machine and Follow both fold the chain and assert captured capability
        // identity; they differ only in checkpoint sourcing. Operator injects
        // input but still consumes the admitted capsule unchanged.
        SuccessorMode::Machine | SuccessorMode::Follow => (
            true,
            CapabilityPolicy::ExactPinned(resume.effective_caps.as_slice()),
        ),
        SuccessorMode::Operator => (false, CapabilityPolicy::AdmissionDefault),
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

    let result = run_claimed_thread_row(
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
    .await;
    drop(params);
    result
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
        let launch_result = launch_admitted_root_with_claim(state, &thread_id, claim).await;
        match launch_result {
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
                accounting_scope: p.accounting_scope,
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
                let terminal = chain_tip_hard_terminal(state, &chain);
                match terminal {
                    Ok(true) => {
                        let release = state.state_store.launch_window_release(
                            &chain,
                            global_live_fanout_limit(),
                            now_ms,
                        );
                        match release {
                            Ok(admitted) => {
                                for id in admitted {
                                    launch_admitted_window_member(state, &id);
                                }
                            }
                            Err(e) => {
                                tracing::warn!(chain_root_id = %chain, error = %e, "launch-window sweep release failed")
                            }
                        }
                    }
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
                let admission =
                    state
                        .state_store
                        .launch_window_admit(&key, global_live_fanout_limit(), now_ms);
                match admission {
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
            if let Some(successor) = state.threads.get_thread(&successor_id)?
                && follow_resume_successor_refusal(&state, &waiter.parent_thread_id, &successor)
                    .is_none()
                && successor.status != ryeos_state::objects::ThreadStatus::Created.as_str()
            {
                let _ = state.state_store.clear_follow_waiter(follow_key);
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

    fn executor_verification_probe(label: &str) -> ExecutorVerificationProbe {
        ExecutorVerificationProbe {
            bundle_generation_fingerprint: format!(
                "test-generation-{label}-{}",
                rand::random::<u64>()
            ),
            node_trust_fingerprint: format!("test-trust-{label}"),
            root_trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
            host_triple: "test-host".to_owned(),
            executor_ref: format!("native:{label}"),
            manifest_refs: Vec::new(),
        }
    }

    fn executor_verification_chain(
        probe: &ExecutorVerificationProbe,
        blob_len: u64,
    ) -> VerifiedNativeExecutorChain {
        VerifiedNativeExecutorChain {
            key: VerifiedExecutorChainKey {
                probe: probe.clone(),
                bundle_root: PathBuf::from("/test/bundle"),
                signed_manifest_ref_digest: "a".repeat(64),
                manifest_object_hash: "b".repeat(64),
                item_source_object_hash: "c".repeat(64),
                blob_hash: "d".repeat(64),
                blob_len,
                mode: 0o500,
                signer_fingerprint: "e".repeat(64),
            },
        }
    }

    #[test]
    fn executor_verification_waiter_shares_exact_leader_blob_and_chain() {
        let probe = executor_verification_probe("shared-success");
        let mut owner = match lookup_or_claim_executor_verification(&probe, false) {
            ExecutorVerificationCacheLookup::Owner(owner) => owner,
            _ => panic!("first lookup must own the verification"),
        };
        let pending = match lookup_or_claim_executor_verification(&probe, false) {
            ExecutorVerificationCacheLookup::Wait(pending) => pending,
            _ => panic!("concurrent lookup must wait"),
        };
        owner.reserve_blob_bytes(4).unwrap();
        let (leader_chain, leader_blob) = owner
            .publish(executor_verification_chain(&probe, 3), vec![1_u8, 2, 3])
            .unwrap();
        let (waiter_chain, waiter_blob) = pending.wait().unwrap();
        assert!(Arc::ptr_eq(&leader_chain, &waiter_chain));
        assert!(Arc::ptr_eq(&leader_blob, &waiter_blob));
        assert_eq!(waiter_blob.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn executor_verification_waiter_receives_exact_leader_failure_without_caching_it() {
        let probe = executor_verification_probe("shared-failure");
        let owner = match lookup_or_claim_executor_verification(&probe, false) {
            ExecutorVerificationCacheLookup::Owner(owner) => owner,
            _ => panic!("first lookup must own the verification"),
        };
        let pending = match lookup_or_claim_executor_verification(&probe, false) {
            ExecutorVerificationCacheLookup::Wait(pending) => pending,
            _ => panic!("concurrent lookup must wait"),
        };
        let leader_error = owner.fail(MaterializationError::Internal(
            "shared leader failure".to_owned(),
        ));
        let waiter_error = match pending.wait() {
            Err(error) => error,
            Ok(_) => panic!("waiter must receive the leader failure"),
        };
        assert!(Arc::ptr_eq(&leader_error, &waiter_error));
        drop(pending);

        match lookup_or_claim_executor_verification(&probe, false) {
            ExecutorVerificationCacheLookup::Owner(owner) => drop(owner),
            _ => panic!("terminal failure must not become a reusable cache entry"),
        }
    }

    #[test]
    fn executor_verification_failure_releases_its_blob_reservation() {
        let probe = executor_verification_probe("reservation-failure");
        let budget = Arc::new(ExecutorVerificationBlobBudget::default());
        let pending = Arc::new(PendingExecutorVerification::default());
        let mut owner = ExecutorVerificationFlight {
            probe,
            pending,
            blob_budget: Arc::clone(&budget),
            reserved_blob_bytes: None,
            complete: false,
        };
        owner.reserve_blob_bytes(17).unwrap();
        assert_eq!(budget.resident_bytes(), 17);
        drop(owner.fail(MaterializationError::Internal(
            "release reservation".to_owned(),
        )));
        assert_eq!(budget.resident_bytes(), 0);
    }

    #[test]
    fn executor_verification_resident_byte_boundary_is_exact() {
        let max = EXECUTOR_VERIFICATION_MAX_RESIDENT_BLOB_BYTES;
        assert_eq!(checked_executor_blob_reservation(max - 1, 1), Some(max));
        assert_eq!(checked_executor_blob_reservation(max - 1, 2), None);
        assert_eq!(checked_executor_blob_reservation(max, 1), None);
        assert_eq!(checked_executor_blob_reservation(u64::MAX, 1), None);
    }

    #[test]
    fn native_executor_blob_size_boundary_is_exact() {
        assert!(native_executor_size_is_admissible(
            MAX_NATIVE_EXECUTOR_BYTES
        ));
        assert!(!native_executor_size_is_admissible(
            MAX_NATIVE_EXECUTOR_BYTES + 1
        ));
    }

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

    #[test]
    fn inventory_filter_uses_schema_declared_capability_template() {
        let admission = ryeos_engine::kind_registry::InventoryAdmissionPolicy {
            capability_template: "ryeos.execute.{kind}.{bare_id}".to_string(),
        };
        let allowed =
            ryeos_engine::canonical_ref::CanonicalRef::parse("action:example/api/read").unwrap();
        let sibling =
            ryeos_engine::canonical_ref::CanonicalRef::parse("action:example/api/write").unwrap();
        let knowledge =
            ryeos_engine::canonical_ref::CanonicalRef::parse("knowledge:example/context").unwrap();

        assert!(inventory_ref_is_authorized(
            &allowed,
            Some(&admission),
            &caps(&["ryeos.execute.action.example/api/read"])
        ));
        assert!(!inventory_ref_is_authorized(
            &sibling,
            Some(&admission),
            &caps(&["ryeos.execute.action.example/api/read"])
        ));
        assert!(inventory_ref_is_authorized(
            &sibling,
            Some(&admission),
            &caps(&["ryeos.execute.action.example/api/*"])
        ));
        assert!(
            inventory_ref_is_authorized(&knowledge, None, &[]),
            "an inventoried kind without an admission declaration remains visible"
        );
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
            CapabilityPolicy::AdmissionDefault,
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
        assert!(
            apply_policy(
                &["a", "b"],
                &[],
                CapabilityPolicy::ExactPinned(&narrower),
                ""
            )
            .is_err()
        );
        let wider = caps(&["a", "b", "c"]);
        assert!(apply_policy(&["a", "b"], &[], CapabilityPolicy::ExactPinned(&wider), "").is_err());
    }

    #[test]
    fn recovery_uses_exact_admitted_capability_closure() {
        let admitted = caps(&["ryeos.execute.tool.echo", "ryeos.get.vault.child/oauth"]);
        assert_eq!(
            recover_admitted_effective_caps(
                &admitted,
                CapabilityPolicy::AdmissionDefault,
                "tool:echo"
            )
            .unwrap(),
            admitted
        );

        let parent = caps(&["ryeos.execute.tool.*"]);
        assert_eq!(
            recover_admitted_effective_caps(
                &admitted,
                CapabilityPolicy::FollowChildHybrid {
                    parent_effective_caps: &parent,
                },
                "tool:echo",
            )
            .unwrap(),
            admitted
        );

        let mismatched = caps(&["ryeos.execute.tool.echo"]);
        assert!(
            recover_admitted_effective_caps(
                &admitted,
                CapabilityPolicy::ExactPinned(&mismatched),
                "tool:echo",
            )
            .is_err()
        );
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

    static MATERIALIZER_TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn materializer_test_guard() -> std::sync::MutexGuard<'static, ()> {
        MATERIALIZER_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(target_os = "linux")]
    fn write_admitted_executor_blob(cas_root: &Path, bytes: &[u8]) -> String {
        lillux::cas::CasStore::new(cas_root.to_path_buf())
            .store_blob(bytes)
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_recovery_reopens_exact_admitted_executor_from_cas() {
        let _guard = materializer_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let bare = "admitted-recovery-executor";
        let content_hash = write_admitted_executor_blob(tmp.path(), b"previous signed executor");
        let manifest_hash = "b".repeat(64);
        let isolation = ryeos_engine::isolation::IsolationRuntime::default();

        let materialized = materialize_admitted_native_executor(
            &format!("native:{bare}"),
            tmp.path(),
            &isolation,
            &content_hash,
            &manifest_hash,
            "trusted-signer",
        )
        .unwrap();

        assert_eq!(materialized.content_hash, content_hash);
        assert_eq!(materialized.bundle_manifest_hash, manifest_hash);
        assert_eq!(materialized.bundle_signer_fingerprint, "trusted-signer");
        assert_eq!(
            materialized.verified_command.identity().content_hash,
            materialized.content_hash
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_recovery_rejects_admitted_executor_blob_with_wrong_bytes() {
        let _guard = materializer_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let bare = "tampered-recovery-executor";
        let admitted_bytes = b"admitted executor bytes";
        let admitted_hash = write_admitted_executor_blob(tmp.path(), admitted_bytes);
        std::fs::write(
            lillux::cas::shard_path(tmp.path(), "blobs", &admitted_hash, ""),
            vec![b'x'; admitted_bytes.len()],
        )
        .unwrap();
        let isolation = ryeos_engine::isolation::IsolationRuntime::default();

        let error = materialize_admitted_native_executor(
            &format!("native:{bare}"),
            tmp.path(),
            &isolation,
            &admitted_hash,
            &"c".repeat(64),
            "trusted-signer",
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("failed its content check"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn managed_recovery_requires_admitted_executor_signer_to_remain_trusted() {
        let key = lillux::crypto::SigningKey::from_bytes(&[76u8; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
        let empty = ryeos_engine::trust::TrustStore::from_signers(Vec::new());
        let error = ensure_admitted_executor_signer_trusted(
            &empty,
            "native:revoked-executor",
            &fingerprint,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            MaterializationError::ExecutorUntrusted { .. }
        ));

        let trusted = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: fingerprint.clone(),
                verifying_key: key.verifying_key(),
                label: None,
            },
        ]);
        ensure_admitted_executor_signer_trusted(&trusted, "native:trusted-executor", &fingerprint)
            .unwrap();
    }

    #[test]
    fn managed_recovery_verifies_the_exact_signed_descriptor_document() {
        let key = lillux::crypto::SigningKey::from_bytes(&[77u8; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: fingerprint.clone(),
                verifying_key: key.verifying_key(),
                label: None,
            },
        ]);
        let body = "kind: runtime\nserves: directive\n";
        let document = lillux::signature::sign_content(body, &key, "#", None);
        let content_hash = lillux::signature::content_hash(body);

        assert_eq!(
            verify_admitted_signed_descriptor_document(
                &document,
                &content_hash,
                &fingerprint,
                &trust,
            )
            .unwrap(),
            body
        );

        let tampered = document.replace("directive", "graph");
        assert!(
            verify_admitted_signed_descriptor_document(
                &tampered,
                &content_hash,
                &fingerprint,
                &trust,
            )
            .is_err()
        );
        let revoked = ryeos_engine::trust::TrustStore::from_signers(Vec::new());
        assert!(
            verify_admitted_signed_descriptor_document(
                &document,
                &content_hash,
                &fingerprint,
                &revoked,
            )
            .is_err()
        );
    }

    #[test]
    fn materializer_keeps_distinct_names_with_identical_executor_bytes() {
        let _guard = materializer_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        let first_bundle = tmp.path().join("first-bundle");
        let second_bundle = tmp.path().join("second-bundle");
        let cache_root = tmp.path().join("state");
        let key = lillux::crypto::SigningKey::from_bytes(&[75u8; 32]);
        let first_fixture =
            write_executable_materializer_fixture(&first_bundle, "first-executor", &key);
        let second_fixture =
            write_executable_materializer_fixture(&second_bundle, "second-executor", &key);
        assert_eq!(first_fixture.blob_hash, second_fixture.blob_hash);
        let trust_store = materializer_trust_store(&first_fixture, &key);
        let roots = vec![first_bundle, second_bundle];

        let first =
            materialize_test_executor(&roots, "native:first-executor", &cache_root, &trust_store)
                .unwrap();
        let second =
            materialize_test_executor(&roots, "native:second-executor", &cache_root, &trust_store)
                .unwrap();

        assert_ne!(first.path, second.path);
        assert_eq!(std::fs::read(&first.path).unwrap(), first_fixture.bytes);
        assert_eq!(std::fs::read(&second.path).unwrap(), second_fixture.bytes);
    }

    #[test]
    fn materializer_rejects_duplicate_native_executor_instead_of_first_root_wins() {
        let _guard = materializer_test_guard();
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
        let _guard = materializer_test_guard();
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
        let _guard = materializer_test_guard();
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

    #[cfg(unix)]
    #[test]
    fn weak_cache_directory_permissions_force_repair() {
        let _guard = materializer_test_guard();
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
    }

    #[cfg(unix)]
    #[test]
    fn full_hash_detects_same_size_rewrite_with_restored_mtime() {
        let _guard = materializer_test_guard();
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
        let _guard = materializer_test_guard();
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
        let _guard = materializer_test_guard();
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

        let executor_cache = cache_root.join("cache").join("executors");
        let remaining_entries = std::fs::read_dir(&executor_cache)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(
            !materialized.path.exists(),
            "failed repair left the original target; cache entries: {remaining_entries:?}"
        );
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
            (
                MaterializationError::ResourceLimit {
                    resource: "verification_in_flight",
                    requested: 129,
                    available: 0,
                    limit: 128,
                },
                "verification_in_flight",
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
            spend_usd: ryeos_accounting::UsdNanos::parse_canonical("0.25").unwrap(),
            spawns: 2,
            depth: 3,
            duration_seconds: 45,
            runtime: BTreeMap::from([
                ("actions".to_string(), 4),
                ("payload_bytes".to_string(), 8_192),
            ]),
            runtime_contract: Some("example-runtime/v1".to_string()),
        };
        let ctx = crate::dispatch::ParentExecutionContext {
            parent_thread_id: "T-parent".to_string(),
            hard_limits: serde_json::to_value(&parent_hard_limits).unwrap(),
            depth: 4,
            accounting_scope: None,
        };

        let parent_limits = parent_limits_from_context(Some(&ctx))
            .expect("parent hard limits parse")
            .expect("parent hard limits present");
        let requested = LimitValues {
            turns: 20,
            tokens: 20_000,
            spend_usd: ryeos_accounting::UsdNanos::parse_canonical("2").unwrap(),
            spawns: 10,
            depth: 8,
            duration_seconds: 300,
            runtime: BTreeMap::from([
                ("actions".to_string(), 12),
                ("payload_bytes".to_string(), 65_536),
            ]),
        };
        let hard = compute_effective_limits(
            Some(&requested),
            &LimitValues::default(),
            &LimitCaps::default(),
            Some(&parent_limits),
            &ryeos_engine::runtime_registry::RuntimeLimitsDecl {
                config_identity: Some("example-runtime/limits".to_string()),
                contract: Some("example-runtime/v1".to_string()),
                dimensions: BTreeMap::from([
                    ("actions".to_string(), u32::MAX.into()),
                    ("payload_bytes".to_string(), 1_073_741_824),
                ]),
            },
        );

        assert_eq!(hard.turns, 6);
        assert_eq!(hard.runtime_limit("actions"), 4);
        assert_eq!(hard.tokens, 1_000);
        assert_eq!(hard.runtime_limit("payload_bytes"), 8_192);
        assert_eq!(
            hard.spend_usd,
            ryeos_accounting::UsdNanos::parse_canonical("0.25").unwrap()
        );
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
            accounting_scope: None,
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
            accounting_scope: None,
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
