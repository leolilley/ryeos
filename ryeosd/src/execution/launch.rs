use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};

use super::callback_token::compute_ttl;
use super::launch_envelope::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, EnvelopeTarget,
    LaunchEnvelope, RuntimeResult, ENVELOPE_VERSION,
};
use super::limits::{compute_effective_limits, load_limits_config};
use super::thread_meta::ThreadMeta;
use crate::services::thread_lifecycle::{ResolvedExecutionRequest, ThreadFinalizeParams};
use crate::state::AppState;

pub struct NativeLaunchResult {
    pub thread: Value,
    pub result: Value,
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

    // 3. Derive effective capabilities from resolved item metadata
    let effective_caps = derive_effective_caps(&resolved.resolved_item.metadata.extra);

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

    let envelope = LaunchEnvelope {
        envelope_version: ENVELOPE_VERSION,
        invocation_id: cap.invocation_id.clone(),
        thread_id: thread_id.clone(),
        roots: EnvelopeRoots {
            project_root: project_path.to_path_buf(),
            user_root,
            system_roots,
        },
        target: EnvelopeTarget {
            item_id: resolved.item_ref.clone(),
            kind: resolved.resolved_item.kind.clone(),
            path: resolved.resolved_item.source_path.to_string_lossy().to_string(),
            digest: resolved.resolved_item.content_hash.clone(),
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
    };

    // 6. Write thread.json (status = created, pre-execution audit)
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

/// Derive effective capabilities from item metadata.
///
/// Reads the `permissions` field from item metadata (set by the directive's
/// YAML frontmatter). No permissions declared = empty caps (deny all).
fn derive_effective_caps(metadata_extra: &std::collections::HashMap<String, serde_json::Value>) -> Vec<String> {
    if let Some(perms) = metadata_extra.get("permissions").and_then(|v| v.as_array()) {
        perms.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        // No permissions declared = deny all tool dispatch from within the runtime.
        // The runtime can still execute its own logic, but callback-mediated
        // actions (execute, fetch, sign) require explicit permission grants.
        Vec::new()
    }
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
}
