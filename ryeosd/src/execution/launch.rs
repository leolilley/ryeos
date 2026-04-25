use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};

use super::callback_token::compute_ttl;
use super::launch_envelope::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, LaunchEnvelope,
    RuntimeResult,
};
use super::limits::{compute_effective_limits, load_limits_config};
use super::thread_meta::ThreadMeta;
use crate::services::thread_lifecycle::{ResolvedExecutionRequest, ThreadFinalizeParams};
use crate::state::AppState;

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

/// Extract native runtime binary from an executor ref.
/// Returns None for non-native executors (use old inline path).
pub fn native_runtime_binary(executor_ref: &str) -> Option<String> {
    executor_ref
        .strip_prefix("native:")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

pub fn build_and_launch(
    state: &AppState,
    runtime_binary: &str,
    acting_principal: &str,
    resolved: &ResolvedExecutionRequest,
    project_path: &Path,
    parameters: &Value,
    vault_bindings: &HashMap<String, String>,
) -> Result<NativeLaunchResult> {
    tracing::info!(
        runtime_binary,
        acting_principal,
        item_ref = %resolved.item_ref,
        kind = %resolved.resolved_item.kind,
        vault_count = vault_bindings.len(),
        "launching native runtime"
    );
    // 1. Create DB thread (status = created)
    let thread = state.threads.create_root_thread(resolved)?;
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

    // EnvelopeTarget is gone. The runtime reads the root path / digest /
    // kind / id from `resolution.root` directly. There is now exactly one
    // root snapshot in the envelope, eliminating the split-brain where
    // `envelope.target` and `envelope.resolution.root` could disagree.
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
    };

    // 6. Write thread.json (status = created, pre-execution audit).
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

    // 7. Spawn runtime (env vars + stdin envelope)
    let envelope_json = serde_json::to_string(&envelope)?;
    let spawn_result = spawn_runtime(
        runtime_binary, project_path, &envelope_json,
        hard_limits.duration_seconds,
        &envelope.callback,
        thread_id,
    );

    // 8. ALWAYS invalidate callback token (cleanup guard)
    state.callback_tokens.invalidate(&cap.token);
    state.callback_tokens.invalidate_for_thread(thread_id);

    // Prune stale capabilities from other completed threads
    let pruned = state.callback_tokens.prune_expired();
    if pruned > 0 {
        tracing::debug!(pruned, "cleaned up expired callback capabilities");
    }

    // 9. Handle spawn result
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
            return Err(err);
        }
    };

    // 10. Build response from DB thread (runtime already finalized via callback)
    let thread_detail = state.threads.get_thread(thread_id)?
        .unwrap_or(thread);

    Ok(NativeLaunchResult {
        thread: serde_json::to_value(&thread_detail)?,
        result: json!({
            "success": runtime_result.success,
            "status": runtime_result.status,
            "outputs": runtime_result.outputs,
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
    fn native_runtime_binary_extracts_native_prefix() {
        assert_eq!(native_runtime_binary("native:directive-runtime"), Some("directive-runtime".to_string()));
        assert_eq!(native_runtime_binary("native:graph-runtime"), Some("graph-runtime".to_string()));
    }

    #[test]
    fn native_runtime_binary_rejects_empty() {
        assert_eq!(native_runtime_binary("native:"), None);
    }

    #[test]
    fn native_runtime_binary_returns_none_for_non_native() {
        assert_eq!(native_runtime_binary("tool:rye/core/bash/bash"), None);
        assert_eq!(native_runtime_binary("inline"), None);
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
        // Identity-style view with no `effective_caps` policy fact —
        // the launcher must treat this as deny-all rather than panic.
        let view = KindComposedView::identity(serde_json::json!({}));
        let caps = derive_effective_caps(&view);
        assert!(caps.is_empty(), "expected deny-all, got: {caps:?}");
    }
}
