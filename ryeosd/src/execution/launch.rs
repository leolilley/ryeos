use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};

use super::arch_check;
use super::callback_token::compute_ttl;
use super::launch_envelope::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, LaunchEnvelope,
    RuntimeResult,
};
use super::limits::{compute_effective_limits, load_limits_config};
use super::thread_meta::ThreadMeta;
use crate::services::thread_lifecycle::{ResolvedExecutionRequest, ThreadFinalizeParams};
use crate::state::AppState;

/// Typed error for native executor materialization failures.
///
/// Raised by [`resolve_native_executor_path`] when the bundle CAS
/// cannot supply the requested binary. The daemon's `dispatch.rs`
/// maps this to `DispatchError::RuntimeMaterializationFailed` with
/// a 502 status — no string-classifier anywhere.
#[derive(Debug, thiserror::Error)]
pub enum MaterializationError {
    #[error("native executor '{executor_ref}' not available: {detail}")]
    ExecutorUnavailable { executor_ref: String, detail: String },
    #[error("bundle manifest error: {0}")]
    ManifestError(String),
    #[error("executor resolution failed for '{executor_ref}': {detail}")]
    ResolutionFailed { executor_ref: String, detail: String },
    #[error("binary blob '{hash}' not found in system CAS")]
    BlobNotFound { hash: String },
    #[error("arch check failed for '{executor_ref}': {detail}")]
    ArchCheckFailed { executor_ref: String, detail: String },
    #[error("executor materialization failed for '{executor_ref}': {detail}")]
    MaterializationFailed { executor_ref: String, detail: String },
}

/// Typed error returned by [`build_and_launch`]. Materialization
/// failures carry a stable variant; everything else is `Internal`.
#[derive(Debug, thiserror::Error)]
pub enum BuildAndLaunchError {
    #[error("materialization failed: {0}")]
    Materialization(#[from] MaterializationError),
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
/// `x86_64-unknown-linux-gnu`), as captured at build time by `ryeosd/build.rs`
/// from Cargo's `TARGET` environment variable. This is identical to
/// `rustc -vV | grep ^host:` for a native build, which is the convention the
/// build-bundle pipeline uses when writing `bin/<triple>/<name>` into bundle
/// manifests (see `ryeos-tools/tests/build_bundle_smoke.rs` and
/// `ryeos-bundles/standard/.ai/bin/<triple>/`).
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

/// Resolve a native executor from the system bundle's CAS.
///
/// Looks up the system bundle manifest via `refs/bundles/manifest`,
/// resolves `bin/<host_triple>/<bare>` in the manifest, verifies
/// trust on the binary's `item_source` record, checks architecture,
/// and materializes the binary to the target directory.
///
/// Returns the path to the materialized binary.
///
/// This function implements the full verified-chain path per the
/// PR1b1 design: no PATH lookup, no fallback. If the bundle manifest
/// doesn't exist (pre-PR1b2), resolution fails with a clear error.
pub fn resolve_native_executor_path(
    system_roots: &[PathBuf],
    executor_ref: &str,
    materialize_dir: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<PathBuf, MaterializationError> {
    let bare = executor_ref
        .strip_prefix("native:")
        .ok_or_else(|| MaterializationError::ExecutorUnavailable {
            executor_ref: executor_ref.to_string(),
            detail: "executor_ref is not a native executor".into(),
        })?;

    let triple = host_triple();

    // 1. Find a system root with a valid CAS
    let mut found_root: Option<PathBuf> = None;
    let mut manifest_hash: Option<String> = None;

    for system_root in system_roots {
        let ai_dir = system_root.join(".ai");
        let objects_dir = ai_dir.join("objects");

        // Check CAS exists
        if !objects_dir.join("blobs").is_dir() || !objects_dir.join("objects").is_dir() {
            continue;
        }

        // Read the bundle manifest ref
        let ref_path = ai_dir.join(BUNDLE_MANIFEST_REF);
        if let Ok(ref_content) = std::fs::read_to_string(&ref_path) {
            // Ref files contain the hash as the first line
            let hash = ref_content.trim().lines().next().unwrap_or("").trim();
            if lillux::cas::valid_hash(hash) {
                found_root = Some(objects_dir);
                manifest_hash = Some(hash.to_string());
                break;
            }
        }
    }

    let (cas_root, mhash) = match (found_root, manifest_hash) {
        (Some(root), Some(hash)) => (root, hash),
        _ => {
            return Err(MaterializationError::ExecutorUnavailable {
                executor_ref: bare.to_string(),
                detail: format!(
                    "system bundle manifest not found ({BUNDLE_MANIFEST_REF}). \
                     The bundle pipeline (PR1b2) must ship binaries for host triple '{triple}'."
                ),
            })
        }
    };

    let cas = lillux::cas::CasStore::new(cas_root);

    // 2. Load the manifest
    let manifest_value = cas
        .get_object(&mhash)
        .map_err(|e| MaterializationError::ManifestError(format!(
            "failed to read bundle manifest object {mhash}: {e}"
        )))?
        .ok_or_else(|| MaterializationError::ManifestError(
            format!("bundle manifest object {mhash} not found in system CAS")
        ))?;

    let manifest =
        ryeos_state::objects::SourceManifest::from_value(&manifest_value)
            .map_err(|e| MaterializationError::ManifestError(format!(
                "failed to parse bundle manifest: {e}"
            )))?;

    tracing::debug!(
        executor_ref,
        host_triple = %triple,
        manifest_entries = manifest.item_source_hashes.len(),
        "loaded bundle manifest for native executor resolution"
    );

    // 3. Resolve the executor from the manifest
    let resolved = ryeos_engine::executor_resolution::resolve_native_executor(
        &manifest.item_source_hashes,
        executor_ref,
        &triple,
        |hash| {
            cas.get_object(hash)
                .map_err(|e| e.to_string())
        },
    )
    .map_err(|e| MaterializationError::ResolutionFailed {
        executor_ref: bare.to_string(),
        detail: e.to_string(),
    })?;

    // 4. Verify trust on the binary's item_source record
    let (_trust_class, _fingerprint) =
        ryeos_engine::executor_resolution::verify_executor_trust(
            &resolved.item_source,
            |fp| trust_store.get(fp).is_some(),
        );

    tracing::info!(
        executor_ref,
        host_triple = %triple,
        blob_hash = %resolved.blob_hash,
        "native executor resolved and trust-verified"
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

    // 6. Architecture check
    arch_check::check_arch(&blob_bytes, std::env::consts::ARCH)
        .map_err(|e| MaterializationError::ArchCheckFailed {
            executor_ref: bare.to_string(),
            detail: e.to_string(),
        })?;

    // 7. Materialize to target directory
    let bin_dir = materialize_dir.join("bin");
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to create bin dir: {e}"),
        })?;
    let target_path = bin_dir.join(bare);

    lillux::cas::materialize_executable(&target_path, &blob_bytes, resolved.mode)
        .map_err(|e| MaterializationError::MaterializationFailed {
            executor_ref: bare.to_string(),
            detail: format!("failed to materialize executable: {e}"),
        })?;

    tracing::info!(
        executor_ref,
        target = %target_path.display(),
        mode = format!("{:o}", resolved.mode),
        "native executor materialized"
    );

    Ok(target_path)
}

pub struct NativeLaunchResult {
    pub thread: Value,
    pub result: Value,
}

/// Spawn-gate: refuse to spawn an executor whose composed trust class
/// is `Unsigned`. Pulled out of `build_and_launch` so the policy is
/// independently unit-testable.
pub(crate) fn enforce_executor_trust(
    trust_class: ryeos_engine::resolution::TrustClass,
    item_ref: &str,
    kind: &str,
) -> Result<()> {
    if matches!(trust_class, ryeos_engine::resolution::TrustClass::Unsigned) {
        anyhow::bail!(
            "refusing to spawn `{}` ({}): executor_trust_class is Unsigned — \
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

pub async fn build_and_launch(
    state: &AppState,
    executor_ref: &str,
    acting_principal: &str,
    resolved: &ResolvedExecutionRequest,
    project_path: &Path,
    parameters: &Value,
    vault_bindings: &HashMap<String, String>,
    pre_minted_thread_id: Option<&str>,
) -> Result<NativeLaunchResult, BuildAndLaunchError> {
    tracing::info!(
        executor_ref,
        acting_principal,
        item_ref = %resolved.item_ref,
        kind = %resolved.resolved_item.kind,
        vault_count = vault_bindings.len(),
        "launching native runtime"
    );
    // 1. Create DB thread (status = created)
    let thread = match pre_minted_thread_id {
        Some(id) => state.threads.create_root_thread_with_id(id, resolved)?,
        None => state.threads.create_root_thread(resolved)?,
    };
    let thread_id = &thread.thread_id;

    // 2. Compute limits (root execution: depth = 0)
    let limits_config = load_limits_config(&project_path.to_path_buf());
    let hard_limits = compute_effective_limits(
        None,
        &limits_config.defaults,
        &limits_config.caps,
        None,
        0,
    );

    // 3. Effective capabilities derivation happens below — sourced
    //    from `resolution.composed.effective_caps` so callback
    //    enforcement and the runtime see the *same* composed capability
    //    set.

    // 4. Mint callback capability
    let ttl = compute_ttl(Some(hard_limits.duration_seconds));
    let cap = state.callback_tokens.generate(
        thread_id,
        project_path.to_path_buf(),
        ttl,
    );

    // 5. Build envelope
    let engine_roots = state.engine.resolution_roots(Some(project_path.to_path_buf()));

    let user_root = engine_roots.ordered.iter()
        .find(|r| r.space == ryeos_engine::contracts::ItemSpace::User)
        .map(|r| r.ai_root.parent().map(|pp| pp.to_path_buf()).unwrap_or(r.ai_root.clone()));

    let system_roots: Vec<PathBuf> = engine_roots.ordered.iter()
        .filter(|r| r.space == ryeos_engine::contracts::ItemSpace::System)
        .map(|r| r.ai_root.parent().map(|pp| pp.to_path_buf()).unwrap_or(r.ai_root.clone()))
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
    let composers = &state.engine.composers;

    // Per-request: build the effective parser dispatcher so any
    // project-local `.ai/parsers/` overlay applies for this request.
    let effective_parsers = state
        .engine
        .effective_parser_dispatcher(Some(project_path))
        .map_err(|e| anyhow::anyhow!("effective parser dispatcher: {e}"))?;

    let resolution = ryeos_engine::resolution::run_resolution_pipeline(
        &resolved.resolved_item.canonical_ref,
        &state.engine.kinds,
        &effective_parsers,
        &engine_roots,
        &state.engine.trust_store,
        composers,
    )
    .map_err(|e| anyhow::anyhow!("resolution pipeline failed: {e}"))?;

    tracing::info!(
        item_ref = %resolved.item_ref,
        ancestors = resolution.ancestors.len(),
        references_edges = resolution.references_edges.len(),
        executor_trust_class = ?resolution.executor_trust_class,
        "resolution pipeline complete"
    );

    // Active trust enforcement: hard-fail before spawn if the daemon
    // resolved an `Unsigned` executor for ANY kind. The trust posture is
    // the *weakest* of root + every ancestor (`execution_trust`) — a
    // single unsigned link in an extends chain taints the whole
    // executor. There is no per-kind opt-out; the launcher always
    // refuses to spawn an unsigned executor.
    let executor_trust_class = resolution.executor_trust_class;
    let kind = resolved.resolved_item.kind.as_str();
    enforce_executor_trust(executor_trust_class, &resolved.item_ref, kind)?;

    // Composed effective caps are the daemon-side single source of
    // truth, exposed via `policy_facts` on the composed view. Kinds
    // without a permission model surface no `effective_caps` fact →
    // empty caps (deny-all). Runtimes consume `resolution.composed`
    // directly and never re-derive.
    let effective_caps: Vec<String> = derive_effective_caps(&resolution.composed);

    tracing::info!(
        item_ref = %resolved.item_ref,
        kind = kind,
        executor_trust_class = ?executor_trust_class,
        effective_caps_count = effective_caps.len(),
        "launcher policy resolved from composed view"
    );

    // 6b. Build inventory the launching kind asked for. The engine
    //     enumerates + parses every inventoried item once here so the
    //     runtime is a pure consumer of `envelope.inventory`.
    let launching_kind_schema = state
        .engine
        .kinds
        .get(&resolved.resolved_item.kind)
        .ok_or_else(|| anyhow::anyhow!(
            "build_and_launch: launching kind `{}` is not registered",
            resolved.resolved_item.kind
        ))?;
    let inventory = ryeos_engine::inventory::build_inventory_for_launching_kind(
        launching_kind_schema,
        &state.engine.kinds,
        &engine_roots,
        &effective_parsers,
    )
    .map_err(|e| anyhow::anyhow!("inventory build failed: {e}"))?;

    // 7. Resolve the native executor from the system bundle's CAS.
    //    This is the verified-chain path: the binary is materialized from
    //    CAS, trust-verified, arch-checked — no PATH lookup.
    let materialized_binary = resolve_native_executor_path(
        &system_roots,
        executor_ref,
        project_path,
        &state.engine.trust_store,
    )?;

    // 8. Build envelope
    //    EnvelopeTarget is gone. The runtime reads the root path / digest /
    //    kind / id from `resolution.root` directly. There is now exactly one
    //    root snapshot in the envelope, eliminating the split-brain where
    //    `envelope.target` and `envelope.resolution.root` could disagree.
    let envelope = LaunchEnvelope {
        invocation_id: cap.invocation_id.clone(),
        thread_id: thread_id.clone(),
        roots: EnvelopeRoots {
            project_root: project_path.to_path_buf(),
            user_root,
            system_roots,
        },
        request: EnvelopeRequest {
            inputs: parameters.clone(),
            previous_thread_id: None,
            parent_thread_id: None,
            parent_capabilities: None,
            depth: 0,
        },
        policy: EnvelopePolicy {
            effective_caps,
            hard_limits: hard_limits.clone(),
        },
        callback: EnvelopeCallback {
            socket_path: state.config.uds_path.clone(),
            token: cap.token.clone(),
        },
        resolution,
        inventory,
    };

    // 8. Write thread.json (status = created, pre-execution audit).
    //    `executor_trust_class` is recorded so the on-disk audit trail
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
        executor_trust_class: Some(executor_trust_class),
    };
    let identity = &state.identity;
    super::thread_meta::write_thread_meta(
        &project_path.to_path_buf(), thread_id, &meta, identity,
    )?;

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
    let envelope_json = serde_json::to_string(&envelope)?;
    let binary_path = materialized_binary.to_string_lossy().to_string();
    let project_owned = project_path.to_path_buf();
    let callback_owned = envelope.callback.clone();
    let thread_id_owned = thread_id.to_string();
    let duration = hard_limits.duration_seconds;
    let spawn_result = tokio::task::spawn_blocking(move || {
        spawn_runtime(
            &binary_path,
            &project_owned,
            &envelope_json,
            duration,
            &callback_owned,
            &thread_id_owned,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_runtime join error: {e}"))?;

    // 10. ALWAYS invalidate callback token (cleanup guard)
    state.callback_tokens.invalidate(&cap.token);
    state.callback_tokens.invalidate_for_thread(thread_id);

    // Prune stale capabilities from other completed threads
    let pruned = state.callback_tokens.prune_expired();
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
                &project_path.to_path_buf(), thread_id, &failed_meta, identity,
            );
            return Err(BuildAndLaunchError::Internal(err));
        }
    };

    // 12. Build response from DB thread (runtime already finalized via callback)
    let thread_detail = state.threads.get_thread(thread_id)?
        .unwrap_or(thread);

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
            "warnings": runtime_result.warnings,
        }),
    })
}

fn spawn_runtime(
    binary: &str,
    project_path: &Path,
    envelope_json: &str,
    timeout_secs: u64,
    callback: &EnvelopeCallback,
    thread_id: &str,
) -> Result<RuntimeResult> {
    let request = lillux::SubprocessRequest {
        cmd: binary.to_string(),
        args: vec![
            "--project-path".to_string(),
            project_path.to_string_lossy().to_string(),
        ],
        cwd: Some(project_path.to_string_lossy().to_string()),
        envs: vec![
            ("RYEOSD_SOCKET_PATH".to_string(), callback.socket_path.to_string_lossy().to_string()),
            ("RYEOSD_CALLBACK_TOKEN".to_string(), callback.token.clone()),
            ("RYEOSD_THREAD_ID".to_string(), thread_id.to_string()),
            ("RYEOSD_PROJECT_PATH".to_string(),
             project_path.to_string_lossy().to_string()),
        ],
        stdin_data: Some(envelope_json.to_string()),
        timeout: timeout_secs as f64,
    };

    let result = lillux::run(request);

    if !result.success {
        return Ok(RuntimeResult {
            success: false,
            status: "failed".to_string(),
            thread_id: String::new(),
            result: Some(result.stderr.clone()),
            outputs: Value::Null,
            cost: None,
            warnings: Vec::new(),
        });
    }

    serde_json::from_str(&result.stdout)
        .map_err(|e| anyhow::anyhow!(
            "failed to parse runtime stdout: {}\nstdout: {}",
            e, &result.stdout[..result.stdout.len().min(500)]
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_triple_matches_rustc_host() {
        // The bundle build pipeline writes binaries under
        // `bin/<triple>/<name>` where `<triple>` is `rustc -vV | grep ^host:`
        // (see `ryeos-tools/tests/build_bundle_smoke.rs::host_triple`). The
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
             fails, check ryeosd/build.rs forwards Cargo's TARGET env var.",
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
                segs, 4,
                "linux rustc triples include an ABI segment (gnu/musl); got {:?}",
                host_triple(),
            );
        }
    }

    use ryeos_engine::resolution::{KindComposedView, TrustClass};
    use std::collections::HashMap;

    #[test]
    fn enforce_trust_blocks_unsigned() {
        let err = enforce_executor_trust(
            TrustClass::Unsigned,
            "directive:my/agent",
            "directive",
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("refusing to spawn"));
        assert!(msg.contains("Unsigned"));
        assert!(msg.contains("directive:my/agent"));
    }

    #[test]
    fn enforce_trust_allows_trusted_classes() {
        for cls in [
            TrustClass::TrustedSystem,
            TrustClass::TrustedUser,
            TrustClass::UntrustedUserSpace,
        ] {
            enforce_executor_trust(cls, "directive:x", "directive")
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
        let view = view_with_caps(vec!["rye.execute.tool.bash", "rye.execute.tool.read"]);
        let caps = derive_effective_caps(&view);
        assert_eq!(caps, vec!["rye.execute.tool.bash", "rye.execute.tool.read"]);
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
}
