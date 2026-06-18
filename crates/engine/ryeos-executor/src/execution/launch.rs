use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use rand::Rng;
use serde_json::{Value, json};

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
use ryeos_app::callback_token::{compute_ttl, effective_bundle_id_from_item_ref};
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

pub struct BuildAndLaunchParams<'a> {
    pub state: &'a AppState,
    pub executor_ref: &'a str,
    pub acting_principal: &'a str,
    pub resolved: &'a ResolvedExecutionRequest,
    pub project_path: &'a Path,
    pub provenance: &'a ryeos_app::execution_provenance::ExecutionProvenance,
    pub parameters: &'a Value,
    pub metadata_required_secrets: &'a [String],
    pub required_envelope_fields: &'a [String],
    pub extra_effective_caps: &'a [String],
    pub pre_minted_thread_id: Option<&'a str>,
    /// Chained-resume turn (see `DispatchRequest::previous_thread_id`).
    pub previous_thread_id: Option<&'a str>,
}

pub async fn build_and_launch(
    params: BuildAndLaunchParams<'_>,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    let BuildAndLaunchParams {
        state,
        executor_ref,
        acting_principal,
        resolved,
        project_path,
        provenance,
        parameters,
        metadata_required_secrets,
        required_envelope_fields,
        extra_effective_caps,
        pre_minted_thread_id,
        previous_thread_id,
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
    // 1. Create DB thread (status = created)
    let thread = match pre_minted_thread_id {
        Some(id) => state.threads.create_root_thread_with_id(id, resolved)?,
        None => state.threads.create_root_thread(resolved)?,
    };
    let thread_id = thread.thread_id.clone();

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
                crate::augmentations::run_augmentations(
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
                .map_err(|e| anyhow::anyhow!("launch augmentation failed: {e}"))?;
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
    let effective_caps: Vec<String> = derive_effective_caps(&resolution.composed)
        .into_iter()
        .chain(extra_effective_caps.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

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
    let ttl = compute_ttl(Some(hard_limits.duration_seconds));
    let child_provenance = provenance.clone_for_borrowed_child();
    let cap = state.callback_tokens.generate_with_context(
        &thread_id,
        project_path.to_path_buf(),
        ttl,
        effective_caps.clone(),
        child_provenance,
        effective_bundle_id_from_item_ref(&resolved.item_ref),
        Some(resolved.item_ref.clone()),
    );

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
            // Pre-runtime failure: launcher finalizes as failed
            let _ = state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread_id.clone(),
                status: "failed".to_string(),
                outcome_code: None,
                result: Some(json!({"error": err.to_string()})),
                error: None,
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
                None
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
    let protocol_bindings: Vec<_> = protocol_bindings.collect::<Result<Vec<_>>>()?;

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
