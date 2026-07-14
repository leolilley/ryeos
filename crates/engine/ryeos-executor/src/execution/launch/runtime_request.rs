use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use super::{EnvelopeCallback, LaunchEnvelope, RuntimeResult};

pub(super) struct SpawnRuntimeParams<'a> {
    pub descriptor: &'a ryeos_engine::protocols::ProtocolDescriptor,
    pub binary: &'a str,
    pub project_path: &'a Path,
    pub project_authority: ryeos_engine::sandbox::SandboxProjectAuthority,
    pub state_root: Option<&'a Path>,
    pub workspace_lifeline: Option<std::sync::Arc<ryeos_app::temp_dir_guard::TempDirGuard>>,
    pub envelope: &'a LaunchEnvelope,
    pub timeout_secs: u64,
    pub callback: &'a EnvelopeCallback,
    pub thread_id: &'a str,
    pub vault_bindings: &'a [(String, String)],
    pub provider_secret_name: Option<&'a str>,
    pub thread_auth_token: &'a str,
    pub roots: ryeos_app::env_contract::DaemonRootEnv,
    pub app_root: &'a Path,
    pub sandbox: &'a ryeos_engine::sandbox::SandboxRuntime,
    pub verified_command: &'a ryeos_engine::sandbox::SandboxVerifiedCode,
    pub cas_root: &'a Path,
    /// Daemon-allocated checkpoint dir for a replay-aware runtime.
    pub checkpoint_dir: Option<&'a Path>,
    /// Whether the replay-aware runtime should load that checkpoint.
    pub is_resume: bool,
}

pub(super) fn spawn_runtime(params: SpawnRuntimeParams<'_>) -> Result<RuntimeResult> {
    let SpawnRuntimeParams {
        descriptor,
        binary,
        project_path,
        project_authority,
        state_root,
        workspace_lifeline,
        envelope,
        timeout_secs,
        callback,
        thread_id,
        vault_bindings,
        provider_secret_name,
        thread_auth_token,
        roots,
        app_root,
        sandbox,
        verified_command,
        cas_root,
        checkpoint_dir,
        is_resume,
    } = params;
    let secret_map: BTreeMap<String, String> = vault_bindings.iter().cloned().collect();

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
        acting_principal: "",
        cas_root,
        app_root,
        thread_auth_token,
    };
    let mut spec = ryeos_engine::protocols::build_subprocess_spec(descriptor, &build_request)
        .map_err(|error| anyhow::anyhow!("builder failed: {error}"))?;

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
    if let Some(checkpoint_dir) = checkpoint_dir {
        protocol_bindings.push(ryeos_app::env_contract::EnvBinding::new(
            "RYEOS_CHECKPOINT_DIR",
            checkpoint_dir.display().to_string(),
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

    let request = super::super::lillux_bridge::to_lillux_request(&spec);
    let request = sandbox
        .apply(
            request,
            ryeos_engine::sandbox::SandboxLaunchContext {
                project_path: &spec.project_path,
                project_authority,
                state_root,
                checkpoint_dir,
                bundle_roots: &envelope.roots.bundle_roots,
                node_trusted_keys_dir: Some(&envelope.roots.node_trusted_keys_dir),
                verified_code: std::slice::from_ref(verified_command),
                item_ref: &envelope.resolution.root.resolved_ref,
                thread_id,
            },
        )
        .map_err(|error| anyhow::anyhow!("sandbox apply failed: {error}"))?;
    let result = lillux::run(request);
    drop(workspace_lifeline);

    if !result.success {
        return Ok(runtime_failure_result(
            &result.stderr,
            result.timed_out,
            result.output_limit_exceeded.map(|limit| limit.as_str()),
        ));
    }

    decode_runtime_stdout(&result.stdout)
}

fn runtime_failure_result(
    stderr: &str,
    timed_out: bool,
    output_limit: Option<&str>,
) -> RuntimeResult {
    RuntimeResult {
        success: false,
        status: if output_limit.is_some() {
            "failed"
        } else if timed_out {
            "timed_out"
        } else {
            "failed"
        }
        .to_string(),
        thread_id: String::new(),
        result: Some(match output_limit {
            Some(stream) => json!({
                "code": format!("output_limit:{stream}"),
                "message": stderr,
                "stream": stream,
            }),
            None => json!(stderr),
        }),
        outputs: Value::Null,
        cost: None,
        warnings: Vec::new(),
    }
}

fn decode_runtime_stdout(stdout: &str) -> Result<RuntimeResult> {
    serde_json::from_str(stdout).map_err(|error| {
        anyhow::anyhow!(
            "failed to parse runtime stdout: {}\nstdout: {}",
            error,
            &stdout[..stdout.len().min(500)]
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subprocess_failure_preserves_stderr_and_failed_status() {
        let result = runtime_failure_result("permission denied", false, None);

        assert!(!result.success);
        assert_eq!(result.status, "failed");
        assert_eq!(result.result, Some(json!("permission denied")));
        assert_eq!(result.outputs, Value::Null);
    }

    #[test]
    fn subprocess_timeout_uses_timed_out_status() {
        assert_eq!(
            runtime_failure_result("deadline", true, None).status,
            "timed_out"
        );
    }

    #[test]
    fn subprocess_output_limit_preserves_the_explicit_reason() {
        let result = runtime_failure_result("retention exceeded", false, Some("stdout"));

        assert_eq!(result.status, "failed");
        assert_eq!(
            result.result.as_ref().unwrap()["code"],
            "output_limit:stdout"
        );
        assert_eq!(result.result.as_ref().unwrap()["stream"], "stdout");
    }

    #[test]
    fn stdout_decode_error_keeps_runtime_context() {
        let error = decode_runtime_stdout("not-json").unwrap_err().to_string();

        assert!(error.contains("failed to parse runtime stdout"));
        assert!(error.contains("stdout: not-json"));
    }
}
