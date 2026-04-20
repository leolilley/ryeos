use std::path::PathBuf;

use anyhow::{bail, Result};
use serde_json::Value;

use super::callback_token::{compute_ttl, uds_allowed_primaries};
use super::launch_envelope::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, EnvelopeTarget,
    LaunchEnvelope, ENVELOPE_VERSION,
};
use super::limits::{compute_effective_limits, load_limits_config};
use super::thread_meta::ThreadMeta;
use crate::state::AppState;

pub struct LaunchResult {
    pub runtime_result: super::launch_envelope::RuntimeResult,
    pub callback_token: String,
}

pub fn build_and_launch(
    state: &AppState,
    thread_id: &str,
    _chain_root_id: &str,
    project_path: &PathBuf,
    item_ref: &str,
    kind: &str,
    target_path: &str,
    digest: &str,
    inputs: Value,
    previous_thread_id: Option<String>,
    parent_thread_id: Option<String>,
    parent_capabilities: Option<Vec<String>>,
    depth: u32,
    effective_caps: Vec<String>,
    identity: &crate::identity::NodeIdentity,
) -> Result<LaunchResult> {
    let callback_store = &state.callback_tokens;

    let limits_config = load_limits_config(project_path);
    let parent_hard = parent_thread_id.as_ref().and_then(|_| None);
    let hard_limits = compute_effective_limits(
        None,
        &limits_config.defaults,
        &limits_config.caps,
        parent_hard,
        depth,
    );

    let ttl = compute_ttl(Some(hard_limits.duration_seconds));
    let cap = callback_store.generate(
        thread_id,
        project_path.clone(),
        uds_allowed_primaries(),
        ttl,
    );

    let meta = ThreadMeta {
        thread_id: thread_id.to_string(),
        status: "running".to_string(),
        item_ref: item_ref.to_string(),
        capabilities: effective_caps.clone(),
        limits: serde_json::to_value(&hard_limits)?,
        model: None,
        started_at: chrono::Utc::now().to_rfc3339(),
        completed_at: None,
        cost: None,
        outputs: None,
    };

    super::thread_meta::write_thread_meta(project_path, thread_id, &meta, identity)?;

    let engine_roots = state.engine.resolution_roots(Some(project_path.clone()));
    let runtime_binary = resolve_runtime_binary(state, kind)?;

    let envelope = LaunchEnvelope {
        envelope_version: ENVELOPE_VERSION,
        invocation_id: cap.invocation_id.clone(),
        thread_id: thread_id.to_string(),
        roots: EnvelopeRoots {
            project_root: project_path.clone(),
            user_root: engine_roots.user.map(|p| {
                p.parent().map(|pp| pp.to_path_buf()).unwrap_or(p)
            }),
            system_roots: engine_roots.system.into_iter().map(|p| {
                p.parent().map(|pp| pp.to_path_buf()).unwrap_or(p)
            }).collect(),
        },
        target: EnvelopeTarget {
            item_id: item_ref.to_string(),
            kind: kind.to_string(),
            path: target_path.to_string(),
            digest: digest.to_string(),
        },
        request: EnvelopeRequest {
            inputs,
            previous_thread_id,
            parent_thread_id,
            parent_capabilities,
            depth,
        },
        policy: EnvelopePolicy {
            effective_caps: effective_caps.clone(),
            hard_limits: hard_limits.clone(),
        },
        callback: EnvelopeCallback {
            socket_path: state.config.uds_path.clone(),
            token: cap.token.clone(),
            allowed_primaries: cap.allowed_primaries.clone(),
        },
    };

    let envelope_json = serde_json::to_string(&envelope)?;
    let result = spawn_runtime(&runtime_binary, project_path, &envelope_json, hard_limits.duration_seconds)?;

    callback_store.invalidate(&cap.token);

    let updated_meta = ThreadMeta {
        status: if result.success { "completed" } else { "failed" }.to_string(),
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        cost: result.cost.as_ref().map(|c| serde_json::json!({
            "input_tokens": c.input_tokens,
            "output_tokens": c.output_tokens,
            "total_usd": c.total_usd,
        })),
        outputs: if result.outputs.is_null() { None } else { Some(result.outputs.clone()) },
        ..meta
    };
    super::thread_meta::write_thread_meta(project_path, thread_id, &updated_meta, identity)?;

    Ok(LaunchResult {
        runtime_result: result,
        callback_token: cap.token,
    })
}

fn resolve_runtime_binary(state: &AppState, kind: &str) -> Result<String> {
    let kinds = &state.engine.kinds;
    let schema = kinds.get(kind).ok_or_else(|| {
        anyhow::anyhow!("unknown kind '{}' — no kind schema found", kind)
    })?;

    if let Some(ref executor_id) = schema.default_executor_id {
        if let Some(binary) = executor_id.strip_prefix("native:") {
            return Ok(binary.to_string());
        }
    }

    match kind {
        "directive" => Ok("directive-runtime".to_string()),
        "graph" => Ok("graph-runtime".to_string()),
        other => bail!("no runtime binary for kind '{}'", other),
    }
}

fn spawn_runtime(
    binary: &str,
    project_path: &PathBuf,
    envelope_json: &str,
    timeout_secs: u64,
) -> Result<super::launch_envelope::RuntimeResult> {
    let request = lillux::SubprocessRequest {
        cmd: binary.to_string(),
        args: vec![
            "--project-path".to_string(),
            project_path.to_string_lossy().to_string(),
        ],
        cwd: Some(project_path.to_string_lossy().to_string()),
        envs: vec![],
        stdin_data: Some(envelope_json.to_string()),
        timeout: timeout_secs as f64,
    };

    let result = lillux::run(request);

    if !result.success {
        let runtime_result = super::launch_envelope::RuntimeResult {
            success: false,
            status: "failed".to_string(),
            thread_id: String::new(),
            result: Some(result.stderr.clone()),
            outputs: Value::Null,
            cost: None,
        };
        return Ok(runtime_result);
    }

    let parsed: super::launch_envelope::RuntimeResult = serde_json::from_str(&result.stdout)
        .map_err(|e| anyhow::anyhow!("failed to parse runtime result: {}", e))?;

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    #[test]
    fn fallback_kind_name_mapping() {
        assert_eq!(fallback_binary_for_kind("directive"), Some("directive-runtime"));
        assert_eq!(fallback_binary_for_kind("graph"), Some("graph-runtime"));
        assert_eq!(fallback_binary_for_kind("tool"), None);
    }

    fn fallback_binary_for_kind(kind: &str) -> Option<&'static str> {
        match kind {
            "directive" => Some("directive-runtime"),
            "graph" => Some("graph-runtime"),
            _ => None,
        }
    }
}
