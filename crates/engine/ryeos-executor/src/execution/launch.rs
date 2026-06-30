use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use rand::Rng;
use serde_json::{json, Value};

use super::arch_check;
use super::launch_envelope::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, LaunchEnvelope,
    LaunchEnvelopeBuilder, RuntimeResult,
};
use super::limits::{
    apply_caller_limit_overrides, apply_execution_policy_overrides, compute_effective_limits,
    load_limits_config,
};
use super::thread_meta::ThreadMeta;
use ryeos_app::callback_token::{effective_bundle_id_for_request, launch_token_ttl};
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::{ResolvedExecutionRequest, ThreadFinalizeParams};
use ryeos_app::vault::VaultReadError;

/// Typed error for native executor materialization failures.
///
/// Raised by [`resolve_native_executor_path`] when the bundle CAS
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
pub enum SecretSource {
    Metadata,
    Provider { provider_id: String },
}

impl SecretSource {
    pub fn kind_for_wire(&self) -> &'static str {
        match self {
            SecretSource::Metadata => "declared",
            SecretSource::Provider { .. } => "provider",
        }
    }

    pub fn name_for_wire(&self) -> String {
        match self {
            SecretSource::Metadata => "item metadata".to_string(),
            SecretSource::Provider { provider_id } => provider_id.clone(),
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
        self.sources
            .iter()
            .find(|source| matches!(source, SecretSource::Provider { .. }))
            .unwrap_or(&self.sources[0])
    }
}

#[derive(Debug, Clone)]
pub struct ProviderPreflight {
    pub snapshot: ryeos_runtime::ResolvedProviderSnapshot,
    pub env_var: Option<String>,
    pub provider_id: String,
}

const ENVELOPE_FIELD_PROVIDER_SNAPSHOT: &str = "provider_snapshot";

pub(crate) fn requires_provider_snapshot(required_envelope_fields: &[String]) -> bool {
    required_envelope_fields
        .iter()
        .any(|field| field == ENVELOPE_FIELD_PROVIDER_SNAPSHOT)
}

pub(crate) fn build_secret_requirements(
    metadata_required_secrets: &[String],
    provider_preflight: Option<&ProviderPreflight>,
) -> Vec<SecretRequirement> {
    let mut requirements: Vec<SecretRequirement> = metadata_required_secrets
        .iter()
        .map(|name| SecretRequirement {
            name: name.clone(),
            sources: vec![SecretSource::Metadata],
        })
        .collect();

    if let Some(preflight) = provider_preflight {
        if let Some(env_var) = preflight.env_var.as_ref() {
            let provider_source = SecretSource::Provider {
                provider_id: preflight.provider_id.clone(),
            };
            if let Some(existing) = requirements.iter_mut().find(|req| req.name == *env_var) {
                existing.sources.push(provider_source);
            } else {
                requirements.push(SecretRequirement {
                    name: env_var.clone(),
                    sources: vec![provider_source],
                });
            }
        }
    }

    requirements
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

fn finalize_missing_secret_launch(
    state: &AppState,
    thread_id: &str,
    item_ref: &str,
    secrets: &[MissingSecret],
) {
    let Some(first) = secrets.first() else {
        return;
    };
    let payload = required_secret_missing_payload(item_ref, first);
    let _ = state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: "failed".to_string(),
        outcome_code: Some("required_secret_missing".to_string()),
        result: Some(payload.clone()),
        error: Some(payload),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    });
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
    Internal(#[from] anyhow::Error),
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

/// Resolve a native executor from the system bundle's CAS.
///
/// Looks up the system bundle manifest via `refs/bundles/manifest`,
/// resolves `bin/<host_triple>/<bare>` in the manifest, verifies
/// trust on the binary's `item_source` record, checks architecture,
/// and materializes the binary to a content-addressed cache under
/// `cache_root/cache/executors/<blob_hash>/<bare>`.
///
/// Content-addressed: a given blob hash always lands at the same path.
/// Extract once per (binary version, host), re-exec from cache forever
/// after. Cache lives under daemon-owned app-root state, not under the
/// project tree — read-only project mounts work.
///
/// Returns the path to the materialized binary.
pub fn resolve_native_executor_path(
    bundle_roots: &[PathBuf],
    executor_ref: &str,
    cache_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    root_trust_class: ryeos_engine::resolution::TrustClass,
) -> Result<PathBuf, MaterializationError> {
    let bare = executor_ref.strip_prefix("native:").ok_or_else(|| {
        MaterializationError::ExecutorUnavailable {
            executor_ref: executor_ref.to_string(),
            detail: "executor_ref is not a native executor".into(),
        }
    })?;

    let triple = host_triple();

    // Iterate every bundle root that ships a manifest, and use the
    // first one whose manifest contains the requested executor. This
    // matches the kind/parser-discovery model: each bundle owns a
    // disjoint slice of the executor namespace (core ships utility
    // bins like `ryeos-core-tools`; standard ships runtime drivers like
    // `ryeos-directive-runtime`). Picking the first manifest blindly
    // would cause core to shadow standard for runtimes that only
    // standard ships.
    let mut tried_roots: Vec<PathBuf> = Vec::new();
    let mut last_resolution_error: Option<String> = None;
    let mut resolved_with: Option<(
        lillux::cas::CasStore,
        ryeos_engine::executor_resolution::ResolvedExecutor,
    )> = None;

    for system_root in bundle_roots {
        let ai_dir = system_root.join(ryeos_engine::AI_DIR);
        let objects_dir = ai_dir.join("objects");

        if !objects_dir.join("blobs").is_dir() || !objects_dir.join("objects").is_dir() {
            continue;
        }

        let ref_path = ai_dir.join(BUNDLE_MANIFEST_REF);
        let ref_content = match std::fs::read_to_string(&ref_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let hash = ref_content.trim().lines().next().unwrap_or("").trim();
        if !lillux::cas::valid_hash(hash) {
            continue;
        }
        let mhash = hash.to_string();

        tried_roots.push(system_root.clone());

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

        let manifest =
            ryeos_state::objects::SourceManifest::from_value(&manifest_value).map_err(|e| {
                MaterializationError::ManifestError(format!("failed to parse bundle manifest: {e}"))
            })?;

        tracing::debug!(
            executor_ref,
            host_triple = %triple,
            bundle_root = %system_root.display(),
            manifest_entries = manifest.item_source_hashes.len(),
            "scanning bundle manifest for native executor"
        );

        match ryeos_engine::executor_resolution::resolve_native_executor(
            &manifest.item_source_hashes,
            executor_ref,
            &triple,
            |h| cas.get_object(h).map_err(|e| e.to_string()),
        ) {
            Ok(resolved) => {
                resolved_with = Some((cas, resolved));
                break;
            }
            Err(e) => {
                last_resolution_error = Some(e.to_string());
                continue;
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

    let (cas, resolved) = resolved_with.ok_or_else(|| MaterializationError::ResolutionFailed {
        executor_ref: bare.to_string(),
        detail: last_resolution_error.unwrap_or_else(|| {
            format!(
                "no manifest among {} system bundle root(s) lists '{executor_ref}' for triple '{triple}'",
                tried_roots.len()
            )
        }),
    })?;

    // 4. Verify trust on the binary's item_source record
    let (trust_class, fingerprint) = ryeos_engine::executor_resolution::verify_executor_trust(
        &resolved.item_source,
        |fp| trust_store.get(fp).is_some(),
        root_trust_class,
    );

    match trust_class {
        ryeos_engine::resolution::TrustClass::TrustedBundle
        | ryeos_engine::resolution::TrustClass::TrustedProject => {
            tracing::info!(
                executor_ref,
                host_triple = %triple,
                blob_hash = %resolved.blob_hash,
                signer_fp = ?fingerprint,
                trust_class = ?trust_class,
                "native executor resolved and trust-verified"
            );
        }
        ryeos_engine::resolution::TrustClass::UntrustedProject
        | ryeos_engine::resolution::TrustClass::Unsigned => {
            return Err(MaterializationError::ExecutorUntrusted {
                executor_ref: bare.to_string(),
                trust_class,
                fingerprint,
            });
        }
    }

    // 5. Fetch the binary blob from CAS
    let blob_bytes = cas
        .get_blob(&resolved.blob_hash)
        .map_err(|e| MaterializationError::BlobNotFound {
            hash: format!("{} (read error: {e})", resolved.blob_hash),
        })?
        .ok_or_else(|| MaterializationError::BlobNotFound {
            hash: resolved.blob_hash.clone(),
        })?;

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

    if target_path.is_file() {
        // Cache hit — skip extraction.
        tracing::debug!(
            executor_ref,
            target = %target_path.display(),
            "native executor cache hit"
        );
        return Ok(target_path);
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
            let winner_present = target_path.is_file();
            let _ = std::fs::remove_dir_all(&staging_dir);
            if !winner_present {
                return Err(MaterializationError::MaterializationFailed {
                    executor_ref: bare.to_string(),
                    detail: format!(
                        "failed to publish executor to cache at {} \
                         (rename error: {rename_err}; no winner present)",
                        target_path.display()
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

    Ok(target_path)
}

/// Extract the model spec from the composed view produced by the
/// engine's resolution pipeline. The composed view contains the
/// directive's parsed header; we pull the `model` key out without
/// re-parsing the directive YAML.
fn extract_model_spec_from_resolved(
    composed: &ryeos_engine::resolution::KindComposedView,
) -> anyhow::Result<Option<ryeos_runtime::model_resolution::ModelSpec>> {
    let model_value = composed.composed.get("model");
    match model_value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => {
            let spec: ryeos_runtime::model_resolution::ModelSpec =
                serde_json::from_value(v.clone()).map_err(|e| {
                    anyhow::anyhow!("failed to parse model spec from composed view: {e}")
                })?;
            Ok(Some(spec))
        }
    }
}

/// Build a `VerifiedLoader` over the same root ordering the spawned
/// runtime will see. This ensures the daemon's preflight resolve
/// produces the same answer the runtime would get. Trust context is
/// the explicit operator trusted-keys dir plus the project root —
/// matching the engine trust store's sources, not the bundle roots.
fn build_verified_loader_for_thread(
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    operator_trusted_keys_dir: &Path,
) -> anyhow::Result<ryeos_runtime::verified_loader::VerifiedLoader> {
    let project_root = engine_roots
        .ordered
        .iter()
        .find(|r| r.space == ryeos_engine::contracts::ItemSpace::Project)
        .map(|r| {
            r.ai_root
                .parent()
                .map(|pp| pp.to_path_buf())
                .unwrap_or(r.ai_root.clone())
        })
        .ok_or_else(|| anyhow::anyhow!("no project root in engine resolution roots"))?;

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

    Ok(ryeos_runtime::verified_loader::VerifiedLoader::new(
        project_root,
        bundle_roots,
        operator_trusted_keys_dir,
    ))
}

pub(crate) fn resolve_provider_preflight(
    composed: &ryeos_engine::resolution::KindComposedView,
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    operator_trusted_keys_dir: &Path,
) -> Result<ProviderPreflight, MaterializationError> {
    let header = ryeos_runtime::model_resolution::DirectiveModelHeader {
        model: extract_model_spec_from_resolved(composed)
            .map_err(|e| MaterializationError::Internal(e.to_string()))?,
    };
    let loader = build_verified_loader_for_thread(engine_roots, operator_trusted_keys_dir)
        .map_err(|e| MaterializationError::Internal(e.to_string()))?;
    let resolved_target = ryeos_runtime::model_resolution::preflight_resolve(&header, &loader)
        .map_err(|e| MaterializationError::Internal(e.to_string()))?;

    Ok(ProviderPreflight {
        env_var: resolved_target.provider.auth.env_var.clone(),
        provider_id: resolved_target.provider_id.clone(),
        snapshot: resolved_target,
    })
}

pub struct NativeLaunchResult {
    pub thread: Value,
    pub result: Value,
}

/// Spawn-gate: refuse to spawn an effective item whose composed trust class
/// is `Unsigned`. Pulled out of `build_and_launch` so the policy is
/// independently unit-testable.
pub(crate) fn enforce_effective_trust(
    trust_class: ryeos_engine::resolution::TrustClass,
    item_ref: &str,
    kind: &str,
) -> Result<()> {
    if matches!(trust_class, ryeos_engine::resolution::TrustClass::Unsigned) {
        anyhow::bail!(
            "refusing to spawn `{}` ({}): effective_trust_class is Unsigned — \
             root or one of its ancestors lacks a valid signature from a trusted signer",
            item_ref,
            kind
        );
    }
    Ok(())
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
/// any captured authority.
pub enum CapabilityPolicy<'a> {
    /// Fresh launch: run with exactly the live composed caps.
    Fresh,
    /// Continuation / native-resume: the live composed caps MUST equal the caps
    /// the predecessor captured (no silent privilege drift); run with them.
    ExactPinned(&'a [String]),
    /// Follow child: the (parent-bounded) caps MUST be a subset of the live
    /// composed caps (no escalation beyond what the item can compose); run with
    /// the bounded set, narrowing authority to the parent's bound.
    BoundedOverride(&'a [String]),
}

/// Apply a [`CapabilityPolicy`] to the freshly-resolved `composed` caps, returning
/// the caps the launch should actually run with (callback token + envelope +
/// launch metadata all consume the result).
fn apply_capability_policy(
    composed: Vec<String>,
    policy: CapabilityPolicy<'_>,
    item_ref: &str,
) -> Result<Vec<String>, BuildAndLaunchError> {
    match policy {
        CapabilityPolicy::Fresh => Ok(composed),
        CapabilityPolicy::ExactPinned(captured) => {
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
        CapabilityPolicy::BoundedOverride(bounded) => {
            // Each bounded cap must be *implied* by a live composed grant, using
            // grant-side wildcard matching (`cap_matches(grant, required)`): a
            // composed grant `ryeos.execute.tool.*` covers a bounded
            // `ryeos.execute.tool.echo`, but an exact composed grant never covers
            // a wider bounded wildcard. Raw string subset would reject the former.
            if let Some(uncovered) = bounded.iter().find(|cap| {
                !composed
                    .iter()
                    .any(|grant| ryeos_runtime::authorizer::cap_matches(grant, cap))
            }) {
                return Err(BuildAndLaunchError::CapabilityRejected {
                    reason: format!(
                        "follow-child capability escalation for `{item_ref}`: bounded cap \
                         `{uncovered}` is not covered by any live composed grant — refusing to \
                         grant authority the item cannot compose"
                    ),
                });
            }
            Ok(bounded.to_vec())
        }
    }
}

pub struct BuildAndLaunchParams<'a> {
    pub state: &'a AppState,
    pub executor_ref: &'a str,
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
    pub required_envelope_fields: &'a [String],
    pub pre_minted_thread_id: Option<&'a str>,
    /// Chained-resume turn (see `DispatchRequest::previous_thread_id`).
    pub previous_thread_id: Option<&'a str>,
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
}

impl Drop for FinalizeFailedOnDrop<'_> {
    fn drop(&mut self) {
        crate::dispatch::finalize_method_thread_if_needed(
            self.state,
            &self.thread_id,
            "failed",
            None,
        );
    }
}

pub async fn build_and_launch(
    params: BuildAndLaunchParams<'_>,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    // 1. Create DB thread (status = created). A chained-resume turn
    //    (`previous_thread_id` present) braids a successor onto the source's
    //    chain — lineage persisted at spawn — instead of a fresh root. All the
    //    row-creation inputs are `Copy` references, so `params` is untouched and
    //    flows whole into the run-half.
    let thread = match (params.pre_minted_thread_id, params.previous_thread_id) {
        (Some(id), Some(source)) => params.state.threads.create_continuation_with_id(
            id,
            source,
            params.resolved,
            Some("chained_resume"),
        )?,
        (Some(id), None) => params
            .state
            .threads
            .create_root_thread_with_id(id, params.resolved)?,
        (None, Some(source)) => {
            let id = ryeos_app::thread_lifecycle::new_thread_id();
            params.state.threads.create_continuation_with_id(
                &id,
                source,
                params.resolved,
                Some("chained_resume"),
            )?
        }
        (None, None) => params.state.threads.create_root_thread(params.resolved)?,
    };
    run_claimed_thread_row(params, thread).await
}

/// Run an already-created `created` thread row to completion: resolve, spawn its
/// runtime subprocess, wait, and finalize.
///
/// Split out of `build_and_launch` (which now only mints the row first) so a
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
    let BuildAndLaunchParams {
        state,
        executor_ref,
        runtime_ref,
        acting_principal,
        resolved,
        project_path,
        provenance,
        parameters,
        metadata_required_secrets,
        required_envelope_fields,
        pre_minted_thread_id: _,
        previous_thread_id,
        suppress_stimulus,
        capability_policy,
        checkpoint_resume_mode,
    } = params;
    let engine = provenance.request_engine();
    tracing::info!(
        executor_ref,
        acting_principal,
        item_ref = %resolved.item_ref,
        kind = %resolved.resolved_item.kind,
        required_secret_count = metadata_required_secrets.len(),
        "launching native runtime"
    );
    let thread_id = thread.thread_id.clone();
    // Authoritative chain root from the freshly-created thread row (a successor
    // inherits its source's root; a fresh launch is its own root). Used to set
    // the callback cap's chain root.
    let chain_root_id = thread.chain_root_id.clone();

    // Arm the persistence-first guard: any post-create failure below finalizes
    // the thread `failed` instead of leaving it stuck at `created` (no-op once
    // the thread is terminal).
    let _finalize_guard = FinalizeFailedOnDrop {
        state,
        thread_id: thread_id.clone(),
    };

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
    let limits_config = load_limits_config(project_path).with_context(|| {
        format!(
            "loading limits config for project {}",
            project_path.display()
        )
    })?;
    let limits_config = limits_config.unwrap_or_default();
    let requested_limits =
        apply_execution_policy_overrides(&limits_config.defaults, &execution_policy);
    let requested_limits = apply_caller_limit_overrides(requested_limits, parameters)?;
    let hard_limits = compute_effective_limits(
        Some(&requested_limits),
        &limits_config.defaults,
        &limits_config.caps,
        None,
        0,
    );
    let duration_source = if parameters.get("timeout").is_some() {
        "caller param `timeout`".to_string()
    } else {
        execution_policy
            .timeout
            .as_ref()
            .map(|policy| policy.source.describe())
            .unwrap_or_else(|| "ryeos-runtime/limits.yaml defaults or built-in default".to_string())
    };
    let turns_source = if parameters.get("max_steps").is_some() {
        "caller param `max_steps`".to_string()
    } else {
        execution_policy
            .max_steps
            .as_ref()
            .map(|policy| policy.source.describe())
            .unwrap_or_else(|| "ryeos-runtime/limits.yaml defaults or built-in default".to_string())
    };
    tracing::info!(
        item_ref = %resolved.item_ref,
        duration_seconds = hard_limits.duration_seconds,
        duration_source,
        duration_cap = ?limits_config.caps.duration_seconds,
        turns = hard_limits.turns,
        turns_source = %turns_source,
        turns_cap = ?limits_config.caps.turns,
        execution_policy_override = execution_policy.timeout.is_some() || execution_policy.max_steps.is_some(),
        caller_limit_override = parameters.get("timeout").is_some() || parameters.get("max_steps").is_some(),
        "native launch execution policy resolved"
    );

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

    // Run the resolution pipeline (extends/references DAGs etc.) so the
    // runtime receives pre-resolved data and never reimplements traversal.
    // Hard fail on any pipeline error — partial pipelines never reach the
    // runtime.
    // The composer registry is owned by the engine — boot built it
    // once via `ComposerRegistry::from_kinds(&kinds, &native)`,
    // validated against it, and persisted it on `Engine::composers`.
    // Pulling it back out here guarantees launcher and boot use the
    // **same** instance (no split-brain).
    let composers = &engine.composers;

    let mut resolution = ryeos_engine::resolution::run_resolution_pipeline(
        &resolved.resolved_item.canonical_ref,
        &engine.kinds,
        &effective_parsers,
        &engine_roots,
        &engine.trust_store,
        composers,
    )
    .map_err(|e| anyhow::anyhow!("resolution pipeline failed: {e}"))?;

    tracing::info!(
        item_ref = %resolved.item_ref,
        ancestors = resolution.ancestors.len(),
        references_edges = resolution.references_edges.len(),
        effective_trust_class = ?resolution.effective_trust_class,
        "resolution pipeline complete"
    );

    // ── Launch augmentations ──────────────────────────────────────
    // Walk any launch_augmentations declared on the kind's schema.
    // Augmentations run between resolution and parent spawn, mutating
    // resolution.composed.derived in place. On failure, abort the
    // parent launch with a structured error.
    {
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
        if let Some(exec) = launching_kind_schema.execution() {
            if !exec.launch_augmentations.is_empty() {
                if let Err(e) = crate::augmentations::run_augmentations(
                    exec,
                    &mut resolution,
                    &thread.thread_id,
                    project_path,
                    engine,
                    provenance,
                    &resolved.plan_context,
                    acting_principal,
                    state,
                )
                .await
                {
                    // The thread was created (status `created`) above. An
                    // augmentation failure (e.g. an unresolved context ref)
                    // must finalize it `failed` rather than orphan it at
                    // `created` with no `thread_failed` event — otherwise the
                    // async launch path leaves the thread stuck and silent.
                    // Mirrors the pre-runtime spawn-failure finalize below;
                    // best-effort so a finalize error never masks the cause.
                    let reason = format!("launch augmentation failed: {e}");
                    // Put the reason in `error` (not `result`) + a stable
                    // outcome_code so the `thread_failed` event payload carries
                    // it (state_store surfaces error_json), and the timeline can
                    // show *why* it failed, not just that it did.
                    let _ = state.threads.finalize_thread(&ThreadFinalizeParams {
                        thread_id: thread_id.clone(),
                        status: "failed".to_string(),
                        outcome_code: Some("launch_augmentation_failed".to_string()),
                        result: None,
                        error: Some(json!({ "message": reason })),
                        metadata: None,
                        artifacts: Vec::new(),
                        final_cost: None,
                        summary_json: None,
                    });
                    return Err(anyhow::anyhow!(reason).into());
                }
            }
        }
    }

    // Active trust enforcement: hard-fail before spawn if the daemon
    // resolved an `Unsigned` effective item for ANY kind. The trust posture is
    // the *weakest* of root + every ancestor (`effective_trust`) — a
    // single unsigned link in an extends chain taints the whole
    // executor. There is no per-kind opt-out; the launcher always
    // refuses to spawn an unsigned effective item.
    let effective_trust_class = resolution.effective_trust_class;
    let kind = resolved.resolved_item.kind.as_str();
    enforce_effective_trust(effective_trust_class, &resolved.item_ref, kind)?;

    // Composed effective caps are the daemon-side single source of
    // truth, exposed via `policy_facts` on the composed view. Kinds
    // without a permission model surface no `effective_caps` fact →
    // empty caps (deny-all). Runtimes consume `resolution.composed`
    // directly and never re-derive.
    // Composed permissions are caller-declared input; the runtime callback caps
    // minted below are daemon-derived from the signed manifest. A composed grant
    // must not overlap the manifest runtime-authority namespace (bundle events /
    // vault): that authority is minted only from a signed manifest, never
    // self-granted via `permissions:`. Reject while the two sources are still
    // distinguishable, before they are unioned.
    let composed_effective_caps = derive_effective_caps(&resolution.composed);
    ryeos_bundle::runtime_authority::reject_disallowed_composed_grants(&composed_effective_caps)
        .map_err(|err| BuildAndLaunchError::CapabilityRejected {
            reason: err.to_string(),
        })?;

    // Manifest-backed runtime callback caps (bundle-events / runtime-vault) are
    // minted from the *composed* `requires` block: for directives the
    // extends-chain composer has already narrowed a child against its parent, so
    // a child can never request more than the parent template. The signed
    // bundle manifest remains the final upper bound (checked inside the minter).
    let runtime_capability_caps = crate::dispatch::mint_runtime_capability_caps(
        resolution.composed.composed.get("requires"),
        &resolved.resolved_item,
        &engine.trust_store,
    )
    .map_err(|reason| BuildAndLaunchError::CapabilityRejected { reason })?;

    let composed_caps: Vec<String> = composed_effective_caps
        .into_iter()
        .chain(runtime_capability_caps)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    // Reconcile the freshly-resolved caps against any captured authority per the
    // launch's policy: fresh uses them as-is; a continuation/native-resume pins
    // them to EXACTLY the predecessor's captured set (no silent drift); a follow
    // child narrows them to the parent-bounded subset.
    let effective_caps = apply_capability_policy(composed_caps, capability_policy, &resolved.item_ref)?;

    // The serving runtime (captured ref, else the kind's default). A runtime that
    // declares `native_resume` is replay-aware: allocate its per-thread checkpoint
    // dir so the spawn can inject `RYEOS_CHECKPOINT_DIR` and reconcile can reason
    // about restart-safe state.
    // Resolve via the REQUEST engine (a malformed/unregistered captured
    // runtime_ref is an error, never silently the kind default — that could
    // switch binary / envelope / native_resume policy).
    let native_resume = engine
        .runtimes
        // ITEM kind (`graph`), not the thread-profile kind (`graph_run`):
        // runtimes are registered by the kind they serve and resolve_for_launch
        // verifies serves == kind.
        .resolve_for_launch(runtime_ref, &resolved.resolved_item.kind)
        .map_err(|e| BuildAndLaunchError::Internal(anyhow::anyhow!(e)))?
        .yaml
        .native_resume
        .clone();
    // Replay-aware runtimes get a per-thread checkpoint dir. If allocation fails,
    // FAIL the launch rather than spawn a replay-aware process with no checkpoint
    // env (the finalize-on-drop guard records a failed thread).
    let checkpoint_dir: Option<std::path::PathBuf> = if native_resume.is_some() {
        let dir = state
            .config
            .app_root
            .join("threads")
            .join(&thread_id)
            .join("checkpoints");
        std::fs::create_dir_all(&dir).map_err(|e| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "failed to allocate checkpoint dir for replay-aware runtime `{}`: {e}",
                resolved.item_ref
            ))
        })?;
        Some(dir)
    } else {
        None
    };

    // A machine continuation of a replay-aware kind resumes from the
    // predecessor's checkpoint: copy it into this successor's dir and flag the
    // spawn so `RYEOS_RESUME=1` is injected and the runtime resumes mid-run.
    // A machine continuation with no predecessor checkpoint cannot resume — fail
    // rather than cold-start (which would re-run the graph from the beginning).
    // Both a machine-continuation successor and a same-thread crash recovery
    // resume from a checkpoint (RYEOS_RESUME=1 — `injects_resume_env`). They
    // differ on WHICH checkpoint: a machine successor copies the PREDECESSOR's
    // forward into its own dir (`copies_predecessor_checkpoint`); a same-thread
    // resume reads its OWN dir (already populated) — so copy-forward is gated on
    // the mode, not on `is_resume`.
    let is_resume = checkpoint_resume_mode.injects_resume_env() && native_resume.is_some();
    if checkpoint_resume_mode.copies_predecessor_checkpoint() && native_resume.is_some() {
        let prev = previous_thread_id.ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "machine continuation of `{}` has no predecessor thread",
                resolved.item_ref
            ))
        })?;
        let succ_dir = checkpoint_dir.as_deref().ok_or_else(|| {
            BuildAndLaunchError::Internal(anyhow::anyhow!(
                "machine continuation of `{}` has no checkpoint dir",
                resolved.item_ref
            ))
        })?;
        let prev_dir = state
            .config
            .app_root
            .join("threads")
            .join(prev)
            .join("checkpoints");
        let copied = ryeos_runtime::CheckpointWriter::copy_latest(&prev_dir, succ_dir)
            .map_err(|e| {
                BuildAndLaunchError::Internal(anyhow::anyhow!("copy-forward checkpoint: {e}"))
            })?;
        if !copied {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "machine continuation of `{}`: predecessor `{prev}` has no checkpoint to resume from",
                resolved.item_ref
            )));
        }
    }

    // Seed launch metadata for continuation-capable kinds (the launch identity a
    // successor relaunches from — REAL composed `effective_caps`, unlike
    // `spawn_item`) AND for replay-aware runtimes (the `native_resume` +
    // `checkpoint_dir` reconcile reads on restart). These are independent: a graph
    // is replay-aware before its continuation flag is set. Warn-not-fail: a fresh
    // launch still works; the relevant path fails loudly later if identity is absent.
    let supports_continuation = state
        .threads
        .kind_profiles()
        .get(&resolved.kind)
        .is_some_and(|p| p.supports_continuation);
    if supports_continuation || native_resume.is_some() {
        let mut metadata = ryeos_app::launch_metadata::RuntimeLaunchMetadata::default();
        if supports_continuation {
            metadata = metadata.with_resume_context(ryeos_app::launch_metadata::ResumeContext {
                kind: resolved.kind.clone(),
                item_ref: resolved.item_ref.clone(),
                launch_mode: resolved.launch_mode.clone(),
                parameters: parameters.clone(),
                project_context: resolved.plan_context.project_context.clone(),
                original_snapshot_hash: None,
                current_site_id: resolved.current_site_id.clone(),
                origin_site_id: resolved.origin_site_id.clone(),
                requested_by: resolved.plan_context.requested_by.clone(),
                execution_hints: resolved.plan_context.execution_hints.clone(),
                effective_caps: effective_caps.clone(),
                // Capture the actual launch identity so a continuation successor of
                // a delegate kind (directive / graph) can reconstruct how to launch
                // without a per-item `executor_id`.
                executor_ref: Some(executor_ref.to_string()),
                runtime_ref: runtime_ref.map(str::to_string),
            });
        }
        if let Some(nr) = native_resume.clone() {
            metadata = metadata.with_native_resume(nr);
        }
        if let Some(ckpt) = checkpoint_dir.clone() {
            metadata = metadata.with_checkpoint_dir(ckpt);
        }
        if let Err(e) = state.state_store.seed_launch_metadata(&thread_id, &metadata) {
            tracing::warn!(
                thread_id = %thread_id,
                error = %e,
                "failed to seed launch metadata"
            );
        }
    }

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

    // Resolve the protocol descriptor from the **runtime** kind schema
    // (always `runtime`), not from the launching item's kind.
    // build_and_launch is only ever called for managed-lifecycle
    // subprocess spawns where a runtime hosts the launching item; the
    // subprocess terminator + protocol_ref live on the runtime kind.
    let runtime_kind_schema = engine.kinds.get("runtime").ok_or_else(|| {
        anyhow::anyhow!("build_and_launch: `runtime` kind schema is not registered")
    })?;

    let protocol_ref = runtime_kind_schema
        .execution()
        .and_then(|ex| ex.terminator.as_ref())
        .and_then(|t| match t {
            ryeos_engine::kind_registry::TerminatorDecl::Subprocess { protocol_ref } => {
                Some(protocol_ref.clone())
            }
            // InProcess terminators don't carry a protocol ref.
            ryeos_engine::kind_registry::TerminatorDecl::InProcess { .. } => None,
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "build_and_launch: `runtime` kind has no subprocess terminator with protocol ref"
            )
        })?;

    let verified_protocol = engine
        .protocols
        .require(&protocol_ref)
        .map_err(|e| anyhow::anyhow!("protocol lookup failed for `{protocol_ref}`: {e}"))?;

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
    let cap = state.callback_tokens.generate_with_context(
        &thread_id,
        project_path.to_path_buf(),
        ttl,
        effective_caps.clone(),
        child_provenance,
        // Same bundle identity the runtime-cap minter used (resolved canonical
        // ref), so token-claimed caps and minted caps cannot diverge.
        effective_bundle_id_for_request(resolved),
        Some(resolved.item_ref.clone()),
    );
    // Carry the thread's authoritative chain root on the cap (it defaults to
    // thread_id / root until set here).
    if !state.callback_tokens.set_chain_root(&cap.token, &chain_root_id) {
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

    // 6c. Runtime envelope requirements can add derived secrets. The engine
    //     only carries opaque envelope field names; executor owns the
    //     `provider_snapshot` LaunchEnvelope contract and resolves it here.
    let dotenv_dirs = ryeos_app::vault::dotenv_search_dirs(Some(project_path));
    let operator_trusted_keys_dir = state.config.runtime_root().trusted_keys_dir();
    let provider_preflight = if requires_provider_snapshot(required_envelope_fields) {
        Some(resolve_provider_preflight(
            &resolution.composed,
            &engine_roots,
            &operator_trusted_keys_dir,
        )?)
    } else {
        None
    };
    let secret_requirements =
        build_secret_requirements(metadata_required_secrets, provider_preflight.as_ref());
    let secret_names: Vec<String> = secret_requirements
        .iter()
        .map(|req| req.name.clone())
        .collect();
    let effective_vault = ryeos_app::vault::read_required_secrets(
        state.vault.as_ref(),
        acting_principal,
        &secret_names,
        &dotenv_dirs,
    )
    .map_err(|e| match e {
        VaultReadError::MissingSecrets { names, .. } => {
            let secrets = missing_secrets_from_requirements(&names, &secret_requirements);
            finalize_missing_secret_launch(state, &thread_id, &resolved.item_ref, &secrets);
            BuildAndLaunchError::MissingSecrets {
                item_ref: resolved.item_ref.clone(),
                secrets,
            }
        }
        VaultReadError::Internal(e) => {
            BuildAndLaunchError::Internal(anyhow::anyhow!("vault read failed: {e:#}"))
        }
    })?;

    // 7. Resolve the native executor from the system bundle's CAS.
    //    Materialized to content-addressed cache under app-root state,
    //    not the project tree (works with read-only mounts).
    let cache_root = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state");
    let materialized_binary = resolve_native_executor_path(
        &bundle_roots,
        executor_ref,
        &cache_root,
        &engine.trust_store,
        ryeos_engine::resolution::TrustClass::TrustedBundle, // executor binaries ship in system bundles
    )?;

    // 8. Build envelope
    //    Using LaunchEnvelopeBuilder to centralize construction and
    //    prevent future field drift. New fields on LaunchEnvelope
    //    only need updating in the builder, not at every call site.
    let mut envelope_builder = LaunchEnvelopeBuilder::new(
        cap.invocation_id.clone(),
        thread_id.clone(),
        EnvelopeRoots {
            project_root: project_path.to_path_buf(),
            bundle_roots,
            operator_trusted_keys_dir,
        },
        EnvelopeRequest {
            inputs: parameters.clone(),
            previous_thread_id: previous_thread_id.map(str::to_string),
            parent_thread_id: None,
            parent_capabilities: None,
            depth: 0,
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
    .inventory(inventory);
    if let Some(preflight) = provider_preflight.as_ref() {
        envelope_builder = envelope_builder.provider_snapshot(
            serde_json::to_value(&preflight.snapshot)
                .expect("ResolvedProviderSnapshot serializable"),
        );
    }
    let envelope = envelope_builder.build();

    // 8. Write thread.json (status = created, pre-execution audit).
    //    `effective_trust_class` is recorded so the on-disk audit trail
    //    matches what the launcher used for spawn-gating.
    let meta = ThreadMeta {
        thread_id: thread_id.clone(),
        status: "created".to_string(),
        item_ref: resolved.item_ref.clone(),
        capabilities: envelope.policy.effective_caps.clone(),
        limits: serde_json::to_value(&hard_limits)?,
        model: None,
        started_at: lillux::time::iso8601_now(),
        completed_at: None,
        cost: None,
        outputs: None,
        effective_trust_class: Some(effective_trust_class),
    };
    let identity = &state.identity;
    super::thread_meta::write_thread_meta(project_path, &thread_id, &meta, identity)?;

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
    let binary_path = materialized_binary.to_string_lossy().to_string();
    let project_owned = project_path.to_path_buf();
    let callback_owned = envelope.callback.clone();
    let thread_id_owned = thread_id.to_string();
    let duration = hard_limits.duration_seconds;
    let descriptor_clone = verified_protocol.descriptor.clone();
    // The native-runtime spawn pipe must include vault_bindings the
    // same way `services::thread_lifecycle::spawn_item` does for
    // generic plan-node subprocesses. Without this, operator secrets
    // never reach the runtime — the trait machinery in `vault.rs`
    // gets called and discarded.
    let vault_owned: Vec<(String, String)> = effective_vault
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let thread_auth = state.thread_auth.mint(
        &thread_id,
        acting_principal.to_string(),
        vec!["execute".to_string()],
        ttl,
    );
    let tat_owned = thread_auth.token.clone();
    let runtime_roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
        &engine_roots,
        &state.config.app_root,
    );
    let provider_secret_name = provider_preflight
        .as_ref()
        .and_then(|preflight| preflight.env_var.clone());
    let app_root_owned = state.config.app_root.clone();
    let cas_root_owned = state.config.app_root.join("cas");
    let checkpoint_dir_owned = checkpoint_dir.clone();

    let spawn_result = tokio::task::spawn_blocking(move || {
        spawn_runtime(SpawnRuntimeParams {
            descriptor: &descriptor_clone,
            binary: &binary_path,
            project_path: &project_owned,
            envelope: &envelope,
            timeout_secs: duration,
            callback: &callback_owned,
            thread_id: &thread_id_owned,
            vault_bindings: &vault_owned,
            provider_secret_name: provider_secret_name.as_deref(),
            thread_auth_token: &tat_owned,
            roots: runtime_roots,
            app_root: &app_root_owned,
            cas_root: &cas_root_owned,
            checkpoint_dir: checkpoint_dir_owned.as_deref(),
            // A machine continuation of a replay-aware kind resumes from the
            // predecessor's copied-forward checkpoint; a fresh launch writes a
            // cold one.
            is_resume,
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_runtime join error: {e}"))?;

    // 10. ALWAYS invalidate callback token (cleanup guard)
    state.callback_tokens.invalidate(&cap.token);
    state.callback_tokens.invalidate_for_thread(&thread_id);
    state.thread_auth.invalidate(&thread_auth.token);
    state.thread_auth.invalidate_for_thread(&thread_id);

    // Prune stale capabilities from other completed threads
    let pruned = state.callback_tokens.prune_expired();
    state.thread_auth.prune_expired();
    if pruned > 0 {
        tracing::debug!(pruned, "cleaned up expired callback capabilities");
    }

    // 11. Handle spawn result
    let runtime_result = match spawn_result {
        Ok(result) => result,
        Err(err) => {
            // Pre-runtime failure (provider/secret resolution, materialization,
            // builder): record the real cause into `error` — the ONLY field the
            // terminal `thread_failed` braid event persists — not `result`,
            // which is dropped. Without this the operator only ever sees a bare
            // "failed" and is locked out of why the thread died. `{err:#}` keeps
            // the full cause chain (e.g. "missing required secret …").
            let _ = state.threads.finalize_thread(&ThreadFinalizeParams {
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
            });
            let failed_meta = ThreadMeta {
                status: "failed".to_string(),
                completed_at: Some(lillux::time::iso8601_now()),
                ..meta
            };
            let _ = super::thread_meta::write_thread_meta(
                project_path,
                &thread_id,
                &failed_meta,
                identity,
            );
            return Err(BuildAndLaunchError::Internal(err));
        }
    };

    // 12. Build response from DB thread. Normally the runtime already
    // finalized via callback. If the subprocess exits before it can do that
    // (for example a hard timeout/SIGKILL), fail closed by finalizing here;
    // otherwise streaming callers tailing until terminal degrade into a
    // misleading `thread_not_terminal` error.
    let mut thread_detail = state.threads.get_thread(&thread_id)?.unwrap_or(thread);
    if !is_runtime_terminal_status(&thread_detail.status) {
        let terminal_status = normalize_runtime_terminal_status(&runtime_result.status);
        let terminal_error = if terminal_status == "completed" {
            None
        } else {
            Some(json!({
                "reason": "runtime_exited_without_callback_finalization",
                "runtime_status": runtime_result.status.clone(),
                "result": runtime_result.result.clone(),
            }))
        };
        let final_cost =
            runtime_result
                .cost
                .as_ref()
                .map(|cost| ryeos_engine::contracts::FinalCost {
                    turns: 0,
                    input_tokens: cost.input_tokens as i64,
                    output_tokens: cost.output_tokens as i64,
                    spend: cost.total_usd,
                    provider: None,
                    metadata: None,
                });
        let finalized = state.threads.finalize_thread(&ThreadFinalizeParams {
            thread_id: thread_id.clone(),
            status: terminal_status.to_string(),
            outcome_code: if terminal_status == "completed" {
                Some("success".to_string())
            } else {
                Some(terminal_status.to_string())
            },
            result: runtime_result.result.clone(),
            error: terminal_error,
            metadata: None,
            artifacts: Vec::new(),
            final_cost,
            summary_json: None,
        })?;
        thread_detail = finalized;
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

/// Lease duration for a successor launch claim. Generous enough to cover the
/// create→`running` window (the subprocess marks running early in its run); the
/// thread's own status is the authoritative guard against relaunching a live
/// successor — the claim just prevents two launchers racing the same `created`
/// row and lets a crashed launcher's claim be reclaimed after it expires.
const SUCCESSOR_LAUNCH_LEASE_MS: i64 = 300_000;

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
    launch_successor_inner(state, successor_id, SuccessorMode::Machine).await
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
    launch_successor_inner(state, successor_id, SuccessorMode::Operator).await
}

async fn launch_successor_inner(
    state: AppState,
    successor_id: &str,
    mode: SuccessorMode,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    // Claim the launch FIRST — the sole authorization to spawn, and the
    // serialization point for the status + budget guards below.
    let claim_id = ryeos_app::thread_lifecycle::new_thread_id();
    let claimed_by = format!("daemon:{}", std::process::id());
    match state.state_store.claim_thread_launch(
        successor_id,
        &claim_id,
        &claimed_by,
        SUCCESSOR_LAUNCH_LEASE_MS,
    )? {
        ryeos_app::runtime_db::LaunchClaimOutcome::Claimed => {}
        // Another launcher (live dispatch or a concurrent reconcile) owns the
        // window. Benign no-op — must NOT burn the attempt budget or finalize.
        ryeos_app::runtime_db::LaunchClaimOutcome::AlreadyClaimed => {
            return Ok(SuccessorLaunchOutcome::Skipped("already_claimed"));
        }
    }

    // Status guard under the claim: ONLY a `created` row is launchable. A
    // successor already `running`/terminal (a duplicate trigger, or a stale-lease
    // reclaim of a still-live launch) must never be relaunched — release the
    // claim and skip WITHOUT finalizing (the row is fine, just not ours to run).
    let successor = match state.threads.get_thread(successor_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = state
                .state_store
                .release_thread_launch_claim(successor_id, &claim_id);
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "launch_successor: thread not found: {successor_id}"
            )));
        }
        Err(e) => {
            let _ = state
                .state_store
                .release_thread_launch_claim(successor_id, &claim_id);
            return Err(e.into());
        }
    };
    if successor.status != ryeos_state::objects::ThreadStatus::Created.as_str() {
        let _ = state
            .state_store
            .release_thread_launch_claim(successor_id, &claim_id);
        return Ok(SuccessorLaunchOutcome::Skipped("not_created"));
    }

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
            Err(e) => {
                let _ = state
                    .state_store
                    .release_thread_launch_claim(successor_id, &claim_id);
                return Err(e.into());
            }
        };
        let max = ryeos_app::thread_lifecycle::MAX_CONTINUATION_AUTO_ATTEMPTS;
        if attempts >= max {
            crate::dispatch::finalize_method_thread_if_needed(
                &state,
                successor_id,
                "failed",
                Some(json!({
                    "error": format!("continuation auto-launch budget exhausted ({attempts}/{max})")
                })),
            );
            let _ = state
                .state_store
                .release_thread_launch_claim(successor_id, &claim_id);
            return Ok(SuccessorLaunchOutcome::Skipped("budget_exhausted"));
        }
        if let Err(e) = state.state_store.bump_resume_attempts(successor_id) {
            let _ = state
                .state_store
                .release_thread_launch_claim(successor_id, &claim_id);
            return Err(e.into());
        }
    }

    // Rebuild + run under the held claim; always release it afterwards.
    let result = launch_claimed_successor(&state, successor, mode).await;
    let _ = state
        .state_store
        .release_thread_launch_claim(successor_id, &claim_id);

    match result {
        Ok(native) => Ok(SuccessorLaunchOutcome::Launched(native)),
        Err(e) => {
            // A pre-run launch DEFECT (absent ResumeContext, snapshot-pinned
            // source, capability drift, envelope rebuild) would otherwise leave
            // the successor stuck at `created`. `run_claimed_thread_row` already
            // finalizes in-run failures, and finalize-if-needed is idempotent, so
            // finalizing here covers the pre-run case too without double-finalizing.
            crate::dispatch::finalize_method_thread_if_needed(
                &state,
                successor_id,
                "failed",
                Some(json!({ "error": e.to_string() })),
            );
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

    // Snapshot guard FIRST — before reconstruction — so a snapshot-pinned source
    // fails with an explicit unsupported error rather than a generic resolution
    // error downstream. This slice folds against a live working tree; a
    // `SnapshotHash` source needs a CAS checkout that is out of scope here.
    let project_path = match &resume.project_context {
        ryeos_engine::contracts::ProjectContext::LocalPath { path } => path.clone(),
        other => {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "launch_successor: snapshot-pinned continuation not supported yet \
                 (project_context = {other:?})"
            )));
        }
    };

    // Rebuild ExecutionParams from the captured identity (re-resolves the item as
    // its own kind, restores principal / hints / sites verbatim).
    let params = crate::execution::runner::execution_params_from_resume_context(state, &resume)?;

    // Envelope-field requirements come from the runtime entry. Prefer the
    // predecessor's captured `runtime_ref` (by-ref) so a continued thread keeps
    // the exact runtime it launched under; a captured-but-bad ref is an error,
    // never a silent switch to the kind default.
    let required_envelope_fields = state
        .engine
        .runtimes
        .resolve_for_launch(
            resume.runtime_ref.as_deref(),
            &params.resolved.resolved_item.kind,
        )
        .map_err(|e| BuildAndLaunchError::Internal(anyhow::anyhow!(e)))?
        .yaml
        .required_envelope_fields
        .clone();

    // Machine: fold the chain with NO new stimulus, and pin authority to the
    // predecessor's captured caps. Operator: inject the seeded input as the
    // opening stimulus, and re-derive caps fresh (an explicit launch, not a
    // relaunch of the same authority).
    let (suppress_stimulus, capability_policy) = match mode {
        SuccessorMode::Machine => (
            true,
            CapabilityPolicy::ExactPinned(resume.effective_caps.as_slice()),
        ),
        SuccessorMode::Operator => (false, CapabilityPolicy::Fresh),
    };

    run_claimed_thread_row(
        BuildAndLaunchParams {
            state,
            executor_ref: &params.resolved.executor_ref,
            // Propagate the predecessor's runtime identity so this successor
            // re-seeds the same runtime for the NEXT continuation turn.
            runtime_ref: resume.runtime_ref.as_deref(),
            acting_principal: &params.acting_principal,
            resolved: &params.resolved,
            project_path: &project_path,
            provenance: &params.provenance,
            parameters: &params.parameters,
            metadata_required_secrets: &params.resolved.resolved_item.metadata.required_secrets,
            required_envelope_fields: &required_envelope_fields,
            pre_minted_thread_id: None,
            previous_thread_id: Some(&previous_thread_id),
            suppress_stimulus,
            capability_policy,
            checkpoint_resume_mode: match mode {
                SuccessorMode::Machine => CheckpointResumeMode::MachineContinuation,
                SuccessorMode::Operator => CheckpointResumeMode::None,
            },
        },
        successor,
    )
    .await
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

    let project_path = match &resume.project_context {
        ryeos_engine::contracts::ProjectContext::LocalPath { path } => path.clone(),
        other => {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "native resume: snapshot-pinned resume not supported yet \
                 (project_context = {other:?})"
            )));
        }
    };

    let params = crate::execution::runner::execution_params_from_resume_context(state, &resume)?;

    let required_envelope_fields = state
        .engine
        .runtimes
        .resolve_for_launch(
            resume.runtime_ref.as_deref(),
            &params.resolved.resolved_item.kind,
        )
        .map_err(|e| BuildAndLaunchError::Internal(anyhow::anyhow!(e)))?
        .yaml
        .required_envelope_fields
        .clone();

    run_claimed_thread_row(
        BuildAndLaunchParams {
            state,
            executor_ref: &params.resolved.executor_ref,
            runtime_ref: resume.runtime_ref.as_deref(),
            acting_principal: &params.acting_principal,
            resolved: &params.resolved,
            project_path: &project_path,
            provenance: &params.provenance,
            parameters: &params.parameters,
            metadata_required_secrets: &params.resolved.resolved_item.metadata.required_secrets,
            required_envelope_fields: &required_envelope_fields,
            pre_minted_thread_id: None,
            // SAME thread, not a successor — no chain braid.
            previous_thread_id: None,
            // Crash resume folds no new stimulus; it reloads its own checkpoint.
            suppress_stimulus: true,
            // Pin the captured authority verbatim (same as a machine relaunch).
            capability_policy: CapabilityPolicy::ExactPinned(resume.effective_caps.as_slice()),
            checkpoint_resume_mode: CheckpointResumeMode::SameThread,
        },
        thread,
    )
    .await
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
    let claim_id = ryeos_app::thread_lifecycle::new_thread_id();
    let claimed_by = format!("daemon:{}", std::process::id());
    match state.state_store.claim_thread_launch(
        thread_id,
        &claim_id,
        &claimed_by,
        SUCCESSOR_LAUNCH_LEASE_MS,
    )? {
        ryeos_app::runtime_db::LaunchClaimOutcome::Claimed => {}
        ryeos_app::runtime_db::LaunchClaimOutcome::AlreadyClaimed => {
            return Ok(SuccessorLaunchOutcome::Skipped("already_claimed"));
        }
    }

    let thread = match state.threads.get_thread(thread_id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            let _ = state
                .state_store
                .release_thread_launch_claim(thread_id, &claim_id);
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "native resume: thread not found: {thread_id}"
            )));
        }
        Err(e) => {
            let _ = state
                .state_store
                .release_thread_launch_claim(thread_id, &claim_id);
            return Err(e.into());
        }
    };

    // A terminal thread is already done (a duplicate trigger or a stale-lease
    // reclaim of a settled row) — release and skip without finalizing. A
    // non-terminal (crashed `running`/`created`) row is the resume target.
    if ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
        .is_some_and(|s| s.is_terminal())
    {
        let _ = state
            .state_store
            .release_thread_launch_claim(thread_id, &claim_id);
        return Ok(SuccessorLaunchOutcome::Skipped("terminal"));
    }

    // A still-`running` row whose process group is ALIVE is not crashed — this is
    // a duplicate trigger or a stale-lease reclaim of a live launch. Skip rather
    // than spawn a duplicate runtime. (reconcile already gates on liveness; this
    // keeps the launcher safe if called twice or if the lease expires under a
    // long-running resume.)
    if thread.status == ryeos_state::objects::ThreadStatus::Running.as_str() {
        if let Some(pgid) = thread.runtime.pgid {
            if ryeos_app::process::pgid_alive(pgid) {
                let _ = state
                    .state_store
                    .release_thread_launch_claim(thread_id, &claim_id);
                return Ok(SuccessorLaunchOutcome::Skipped("live_process"));
            }
        }
    }

    let result = launch_claimed_native_resume(&state, thread).await;
    let _ = state
        .state_store
        .release_thread_launch_claim(thread_id, &claim_id);

    match result {
        Ok(native) => Ok(SuccessorLaunchOutcome::Launched(native)),
        Err(e) => {
            crate::dispatch::finalize_method_thread_if_needed(
                &state,
                thread_id,
                "failed",
                Some(json!({ "error": e.to_string() })),
            );
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
/// chain root, so `previous_thread_id` is `None`. Caps are pinned to the
/// (parent-bounded) set captured on the row at create time.
async fn launch_claimed_follow_child(
    state: &AppState,
    thread: ryeos_app::state_store::ThreadDetail,
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
    let identity = state
        .state_store
        .get_launch_metadata(&thread_id)?
        .and_then(|m| m.resume_context)
        .ok_or_else(|| anyhow::anyhow!("follow child: {thread_id} has no seeded launch identity"))?;

    let project_path = match &identity.project_context {
        ryeos_engine::contracts::ProjectContext::LocalPath { path } => path.clone(),
        other => {
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "follow child: snapshot-pinned launch not supported yet \
                 (project_context = {other:?})"
            )));
        }
    };

    let params = crate::execution::runner::execution_params_from_resume_context(state, &identity)?;

    let required_envelope_fields = state
        .engine
        .runtimes
        .resolve_for_launch(identity.runtime_ref.as_deref(), &params.resolved.resolved_item.kind)
        .map_err(|e| BuildAndLaunchError::Internal(anyhow::anyhow!(e)))?
        .yaml
        .required_envelope_fields
        .clone();

    run_claimed_thread_row(
        BuildAndLaunchParams {
            state,
            executor_ref: &params.resolved.executor_ref,
            runtime_ref: identity.runtime_ref.as_deref(),
            acting_principal: &params.acting_principal,
            resolved: &params.resolved,
            project_path: &project_path,
            provenance: &params.provenance,
            parameters: &params.parameters,
            metadata_required_secrets: &params.resolved.resolved_item.metadata.required_secrets,
            required_envelope_fields: &required_envelope_fields,
            pre_minted_thread_id: None,
            // A follow child is its OWN root chain, never a continuation braid.
            previous_thread_id: None,
            // A fresh launch injects its opening stimulus.
            suppress_stimulus: false,
            // Narrow to the (parent-bounded) caps captured on the row at create
            // time — must be a subset of what the child item can compose.
            capability_policy: CapabilityPolicy::BoundedOverride(identity.effective_caps.as_slice()),
            // Fresh launch, not a checkpoint resume.
            checkpoint_resume_mode: CheckpointResumeMode::None,
        },
        thread,
    )
    .await
}

/// Claim-guarded entry to launch a pre-created, pre-seeded follow CHILD row
/// through the managed runtime path. Fire-and-forget from the daemon: the follow
/// path `tokio::spawn`s this so the child runs detached while the parent suspends.
/// Idempotent + crash-safe like `launch_existing_native_resume`: claims the lease
/// (a dead launcher's claim is reclaimable), skips a terminal or live-process row,
/// and finalizes on a pre-run defect.
pub async fn launch_follow_child(
    state: AppState,
    child_id: &str,
) -> Result<SuccessorLaunchOutcome, BuildAndLaunchError> {
    let claim_id = ryeos_app::thread_lifecycle::new_thread_id();
    let claimed_by = format!("daemon:{}", std::process::id());
    match state.state_store.claim_thread_launch(
        child_id,
        &claim_id,
        &claimed_by,
        SUCCESSOR_LAUNCH_LEASE_MS,
    )? {
        ryeos_app::runtime_db::LaunchClaimOutcome::Claimed => {}
        ryeos_app::runtime_db::LaunchClaimOutcome::AlreadyClaimed => {
            return Ok(SuccessorLaunchOutcome::Skipped("already_claimed"));
        }
    }

    let thread = match state.threads.get_thread(child_id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            let _ = state.state_store.release_thread_launch_claim(child_id, &claim_id);
            return Err(BuildAndLaunchError::Internal(anyhow::anyhow!(
                "follow child: thread not found: {child_id}"
            )));
        }
        Err(e) => {
            let _ = state.state_store.release_thread_launch_claim(child_id, &claim_id);
            return Err(e.into());
        }
    };

    // A terminal row is already done (a duplicate trigger or a stale-lease reclaim
    // of a settled child) — release and skip without finalizing.
    if ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
        .is_some_and(|s| s.is_terminal())
    {
        let _ = state.state_store.release_thread_launch_claim(child_id, &claim_id);
        return Ok(SuccessorLaunchOutcome::Skipped("terminal"));
    }

    // A still-`running` row whose process group is ALIVE is not crashed — a
    // duplicate trigger or a stale-lease reclaim of a live launch. Skip rather than
    // spawn a duplicate runtime.
    if thread.status == ryeos_state::objects::ThreadStatus::Running.as_str() {
        if let Some(pgid) = thread.runtime.pgid {
            if ryeos_app::process::pgid_alive(pgid) {
                let _ = state.state_store.release_thread_launch_claim(child_id, &claim_id);
                return Ok(SuccessorLaunchOutcome::Skipped("live_process"));
            }
        }
    }

    let result = launch_claimed_follow_child(&state, thread).await;
    let _ = state.state_store.release_thread_launch_claim(child_id, &claim_id);

    match result {
        Ok(native) => Ok(SuccessorLaunchOutcome::Launched(native)),
        Err(e) => {
            crate::dispatch::finalize_method_thread_if_needed(
                &state,
                child_id,
                "failed",
                Some(json!({ "error": e.to_string() })),
            );
            Err(e)
        }
    }
}

struct SpawnRuntimeParams<'a> {
    descriptor: &'a ryeos_engine::protocols::ProtocolDescriptor,
    binary: &'a str,
    project_path: &'a Path,
    envelope: &'a LaunchEnvelope,
    timeout_secs: u64,
    callback: &'a EnvelopeCallback,
    thread_id: &'a str,
    vault_bindings: &'a [(String, String)],
    provider_secret_name: Option<&'a str>,
    thread_auth_token: &'a str,
    roots: ryeos_app::env_contract::DaemonRootEnv,
    app_root: &'a Path,
    cas_root: &'a Path,
    /// Daemon-allocated checkpoint dir for a replay-aware (`native_resume`)
    /// runtime; `Some` injects `RYEOS_CHECKPOINT_DIR` so the runtime writes
    /// restart-safe state. `None` for runtimes that declare no `native_resume`.
    checkpoint_dir: Option<&'a Path>,
    /// Inject `RYEOS_RESUME=1` so a replay-aware runtime loads its checkpoint
    /// instead of cold-starting.
    is_resume: bool,
}

fn spawn_runtime(params: SpawnRuntimeParams<'_>) -> Result<RuntimeResult> {
    let SpawnRuntimeParams {
        descriptor,
        binary,
        project_path,
        envelope,
        timeout_secs,
        callback,
        thread_id,
        vault_bindings,
        provider_secret_name,
        thread_auth_token,
        roots,
        app_root,
        cas_root,
        checkpoint_dir,
        is_resume,
    } = params;
    let secret_map: std::collections::BTreeMap<String, String> =
        vault_bindings.iter().cloned().collect();

    // Use the protocol builder to produce the SubprocessSpec.
    let item_ref = ryeos_engine::canonical_ref::CanonicalRef::parse("runtime:spawn")
        .expect("hardcoded runtime:spawn ref is valid");
    let callback_bindings = ryeos_engine::protocols::CallbackBindings {
        socket_path: callback.socket_path.to_string_lossy().to_string(),
        token: callback.token.clone(),
    };
    let build_request = ryeos_engine::protocols::BuildRequest {
        item_ref: &item_ref,
        binary_path: Path::new(binary),
        args: &[
            "--project-path".to_string(),
            project_path.to_string_lossy().to_string(),
        ],
        cwd: project_path,
        project_path,
        thread_id,
        callback: Some(&callback_bindings),
        vault_bindings,
        launch_envelope: Some(envelope),
        timeout: std::time::Duration::from_secs(timeout_secs),
        acting_principal: "", // not needed for env injection in runtime path
        cas_root,
        app_root,
        thread_auth_token,
    };

    let mut spec = ryeos_engine::protocols::build_subprocess_spec(descriptor, &build_request)
        .map_err(|e| anyhow::anyhow!("builder failed: {e}"))?;

    let protocol_bindings = spec.env.iter().map(|(key, value)| {
        let source = descriptor
            .env_injections
            .iter()
            .find(|injection| injection.name == *key)
            .map(|injection| injection.source)
            .ok_or_else(|| anyhow::anyhow!("protocol builder emitted undeclared env `{key}`"))?;
        Ok(ryeos_app::env_contract::EnvBinding::new(
            key.clone(),
            value.clone(),
            ryeos_app::env_contract::EnvSourceDetail::ProtocolInjection { source },
        ))
    });
    let mut protocol_bindings: Vec<_> = protocol_bindings.collect::<Result<Vec<_>>>()?;
    // Replay-aware runtimes: surface the daemon-allocated checkpoint dir (and the
    // resume flag) so the runtime writes / reloads restart-safe state via
    // `ryeos_runtime::CheckpointWriter`.
    if let Some(ckpt) = checkpoint_dir {
        protocol_bindings.push(ryeos_app::env_contract::EnvBinding::new(
            "RYEOS_CHECKPOINT_DIR",
            ckpt.display().to_string(),
            ryeos_app::env_contract::EnvSourceDetail::DaemonResume,
        ));
        if is_resume {
            protocol_bindings.push(ryeos_app::env_contract::EnvBinding::new(
                "RYEOS_RESUME",
                "1",
                ryeos_app::env_contract::EnvSourceDetail::DaemonResume,
            ));
        }
    }

    let declared_secret_bindings = secret_map
        .iter()
        .filter(|(key, _)| Some(key.as_str()) != provider_secret_name)
        .map(|(key, value)| (key.clone(), value.clone()));
    let provider_secret_bindings = secret_map
        .iter()
        .filter(|(key, _)| Some(key.as_str()) == provider_secret_name)
        .map(|(key, value)| (key.clone(), value.clone()));
    spec.env = ryeos_app::env_contract::EnvContractBuilder::new()
        .with_base_allowlist(std::env::vars_os().map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        }))?
        .with_daemon_roots(roots)?
        .with_bindings(
            ryeos_app::env_contract::EnvSourceKind::DeclaredSecret,
            declared_secret_bindings,
        )?
        .with_bindings(
            ryeos_app::env_contract::EnvSourceKind::ProviderSecret,
            provider_secret_bindings,
        )?
        .with_typed_bindings(protocol_bindings)?
        .build();

    // sandbox_wrap is identity today; the sandbox wave fills it in.
    let spec = ryeos_engine::subprocess_spec::sandbox_wrap(spec)
        .map_err(|e| anyhow::anyhow!("sandbox_wrap failed: {e}"))?;

    let request = super::lillux_bridge::to_lillux_request(&spec);
    let result = lillux::run(request);

    if !result.success {
        return Ok(RuntimeResult {
            success: false,
            status: if result.timed_out {
                "timed_out".to_string()
            } else {
                "failed".to_string()
            },
            thread_id: String::new(),
            result: Some(json!(result.stderr.clone())),
            outputs: Value::Null,
            cost: None,
            warnings: Vec::new(),
        });
    }

    serde_json::from_str(&result.stdout).map_err(|e| {
        anyhow::anyhow!(
            "failed to parse runtime stdout: {}\nstdout: {}",
            e,
            &result.stdout[..result.stdout.len().min(500)]
        )
    })
}

fn is_runtime_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
    )
}

fn normalize_runtime_terminal_status(status: &str) -> &'static str {
    match status {
        "completed" => "completed",
        "cancelled" => "cancelled",
        "killed" => "killed",
        "timed_out" => "timed_out",
        "continued" => "continued",
        // RuntimeResult historically used "errored" internally. The thread
        // lifecycle vocabulary uses "failed" for that terminal state.
        "failed" | "errored" => "failed",
        _ => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn capability_policy_fresh_uses_composed() {
        let composed = caps(&["a", "b"]);
        let out = apply_capability_policy(composed.clone(), CapabilityPolicy::Fresh, "i").unwrap();
        assert_eq!(out, composed);
    }

    #[test]
    fn capability_policy_exact_pinned_requires_equality() {
        let composed = caps(&["a", "b"]);
        // Equal set (order-insensitive) → ok, returns composed.
        let pinned = caps(&["b", "a"]);
        let out = apply_capability_policy(
            composed.clone(),
            CapabilityPolicy::ExactPinned(&pinned),
            "i",
        )
        .unwrap();
        assert_eq!(out, composed);
        // Drift (narrower OR wider) → rejected.
        let narrower = caps(&["a"]);
        assert!(apply_capability_policy(
            composed.clone(),
            CapabilityPolicy::ExactPinned(&narrower),
            "i"
        )
        .is_err());
        let wider = caps(&["a", "b", "c"]);
        assert!(
            apply_capability_policy(composed, CapabilityPolicy::ExactPinned(&wider), "i").is_err()
        );
    }

    #[test]
    fn capability_policy_bounded_override_narrows_to_subset() {
        let composed = caps(&["a", "b", "c"]);
        // A subset is accepted and BECOMES the effective set (narrowed).
        let bounded = caps(&["a", "b"]);
        let out = apply_capability_policy(
            composed.clone(),
            CapabilityPolicy::BoundedOverride(&bounded),
            "i",
        )
        .unwrap();
        assert_eq!(out, bounded);
        // A cap not present in the composed set is an escalation → rejected.
        let escalated = caps(&["a", "d"]);
        assert!(apply_capability_policy(
            composed,
            CapabilityPolicy::BoundedOverride(&escalated),
            "i"
        )
        .is_err());
    }

    #[test]
    fn capability_policy_bounded_override_grant_side_wildcards() {
        // A composed wildcard grant covers a narrower bounded exact cap.
        let composed = caps(&["ryeos.execute.tool.*"]);
        let bounded = caps(&["ryeos.execute.tool.echo"]);
        let out = apply_capability_policy(
            composed,
            CapabilityPolicy::BoundedOverride(&bounded),
            "i",
        )
        .unwrap();
        assert_eq!(out, bounded);

        // A broader composed wildcard covers a narrower bounded wildcard.
        let composed = caps(&["ryeos.execute.*"]);
        let bounded = caps(&["ryeos.execute.tool.*"]);
        let out = apply_capability_policy(
            composed,
            CapabilityPolicy::BoundedOverride(&bounded),
            "i",
        )
        .unwrap();
        assert_eq!(out, bounded);

        // An exact composed grant does NOT cover a wider bounded wildcard.
        let composed = caps(&["ryeos.execute.tool.echo"]);
        let bounded = caps(&["ryeos.execute.tool.*"]);
        assert!(apply_capability_policy(
            composed,
            CapabilityPolicy::BoundedOverride(&bounded),
            "i"
        )
        .is_err());

        // The global wildcard covers any valid child cap.
        let composed = caps(&["*"]);
        let bounded = caps(&["ryeos.execute.tool.echo", "ryeos.execute.service.threads/get"]);
        let out = apply_capability_policy(
            composed,
            CapabilityPolicy::BoundedOverride(&bounded),
            "i",
        )
        .unwrap();
        assert_eq!(out, bounded);
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
    fn runtime_terminal_status_normalization_matches_thread_vocabulary() {
        assert_eq!(normalize_runtime_terminal_status("completed"), "completed");
        assert_eq!(normalize_runtime_terminal_status("timed_out"), "timed_out");
        assert_eq!(normalize_runtime_terminal_status("cancelled"), "cancelled");
        assert_eq!(normalize_runtime_terminal_status("errored"), "failed");
        assert_eq!(normalize_runtime_terminal_status("unexpected"), "failed");
    }

    #[test]
    fn runtime_terminal_status_detection_rejects_running_states() {
        assert!(is_runtime_terminal_status("completed"));
        assert!(is_runtime_terminal_status("failed"));
        assert!(is_runtime_terminal_status("timed_out"));
        assert!(!is_runtime_terminal_status("created"));
        assert!(!is_runtime_terminal_status("running"));
    }
}
