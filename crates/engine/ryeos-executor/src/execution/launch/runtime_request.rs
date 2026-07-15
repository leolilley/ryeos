use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ryeos_engine::canonical_ref::CanonicalRef;

use super::{EnvelopeCallback, LaunchEnvelope, RuntimeResult};
use ryeos_runtime::envelope::RuntimeResultStatus;

pub(super) struct SpawnRuntimeParams<'a> {
    pub state: &'a ryeos_app::state::AppState,
    pub descriptor: &'a ryeos_engine::protocols::ProtocolDescriptor,
    /// Exact verified runtime item selected by the runtime registry.
    pub item_ref: &'a CanonicalRef,
    pub acting_principal: &'a str,
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
    pub thread_auth_token: &'a str,
    pub roots: ryeos_app::env_contract::DaemonRootEnv,
    pub sandbox: &'a ryeos_engine::sandbox::SandboxRuntime,
    pub verified_command: &'a ryeos_engine::sandbox::SandboxVerifiedCode,
    pub cas_root: &'a Path,
    /// Daemon-allocated checkpoint dir for a replay-aware runtime.
    pub checkpoint_dir: Option<&'a Path>,
    /// Whether the replay-aware runtime should load that checkpoint.
    pub is_resume: bool,
}

pub(super) struct SpawnedRuntime {
    process: Option<lillux::RunningProcess>,
    attached_process: Option<AttachedProcessGuard>,
    workspace_lifeline: Option<std::sync::Arc<ryeos_app::temp_dir_guard::TempDirGuard>>,
    immediate_result: Option<RuntimeResult>,
}

impl SpawnedRuntime {
    pub(super) fn wait(mut self) -> Result<RuntimeResult> {
        if let Some(result) = self.immediate_result.take() {
            return Ok(result);
        }
        let process = self
            .process
            .take()
            .ok_or_else(|| anyhow::anyhow!("spawned runtime has no process or immediate result"))?;
        let result = process.wait();
        drop(self.attached_process.take());
        drop(self.workspace_lifeline.take());
        if !result.success {
            return Ok(runtime_failure_result(
                &result.stderr,
                result.timed_out,
                result.output_limit_exceeded.map(|limit| limit.as_str()),
            ));
        }
        decode_runtime_stdout(&result.stdout)
    }
}

struct AttachedProcessGuard {
    state: ryeos_app::state::AppState,
    thread_id: String,
    identity: ryeos_app::process::ExecutionProcessIdentity,
}

impl Drop for AttachedProcessGuard {
    fn drop(&mut self) {
        match self
            .state
            .state_store
            .clear_thread_process_if_matches(&self.thread_id, &self.identity)
        {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                thread_id = self.thread_id,
                "managed runtime identity changed before compare-and-clear"
            ),
            Err(error) => tracing::error!(
                thread_id = self.thread_id,
                error = %error,
                "failed to clear managed runtime identity after owned wait"
            ),
        }
    }
}

/// Build the protocol subprocess request and start the child, returning as
/// soon as stdin has been handed to the successfully spawned process. Waiting
/// and result decoding are deliberately separate so accepted launch surfaces
/// can acknowledge the durable spawn-task handoff without waiting for runtime
/// completion.
pub(super) fn spawn_runtime(params: SpawnRuntimeParams<'_>) -> Result<SpawnedRuntime> {
    let SpawnRuntimeParams {
        state,
        descriptor,
        item_ref,
        acting_principal,
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
        thread_auth_token,
        roots,
        sandbox,
        verified_command,
        cas_root,
        checkpoint_dir,
        is_resume,
    } = params;
    let secret_map: BTreeMap<String, String> = vault_bindings.iter().cloned().collect();
    let callback_socket_requested = descriptor.env_injections.iter().any(|injection| {
        injection.source
            == ryeos_engine::protocol_vocabulary::EnvInjectionSource::CallbackSocketPath
    });
    let callback_ipc_requested = descriptor.callback_channel
        != ryeos_engine::protocol_vocabulary::CallbackChannel::None
        || callback_socket_requested;
    let sandbox_daemon_socket_path =
        callback_ipc_requested.then_some(callback.socket_path.as_path());

    let callback_socket_path = callback
        .socket_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("runtime callback socket path is not valid UTF-8"))?
        .to_owned();
    let project_path_string = project_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("runtime project path is not valid UTF-8"))?
        .to_owned();
    let callback_bindings = ryeos_engine::protocols::CallbackBindings {
        socket_path: callback_socket_path,
        token: callback.token.clone(),
    };
    let build_request = ryeos_engine::protocols::BuildRequest {
        item_ref,
        binary_path: Path::new(binary),
        args: &["--project-path".to_string(), project_path_string],
        cwd: project_path,
        project_path,
        callback_project_path: state_root.unwrap_or(project_path),
        thread_id,
        callback: Some(&callback_bindings),
        launch_envelope: Some(envelope),
        timeout: std::time::Duration::from_secs(timeout_secs),
        acting_principal,
        cas_root,
        thread_auth_token: Some(thread_auth_token),
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
        let checkpoint_dir = checkpoint_dir
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("runtime checkpoint path is not valid UTF-8"))?;
        protocol_bindings.push(ryeos_app::env_contract::EnvBinding::new(
            "RYEOS_CHECKPOINT_DIR",
            checkpoint_dir,
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
        .with_typed_bindings(protocol_bindings)?
        .build();

    let request = super::super::lillux_bridge::to_lillux_request(&spec)?;
    let sandbox_item_ref = item_ref.to_string();
    let request = sandbox
        .apply(
            request,
            ryeos_engine::sandbox::SandboxLaunchContext {
                project_path: &spec.project_path,
                project_authority,
                state_root,
                checkpoint_dir,
                daemon_socket_path: sandbox_daemon_socket_path,
                bundle_roots: &envelope.roots.bundle_roots,
                node_trusted_keys_dir: Some(&envelope.roots.node_trusted_keys_dir),
                verified_code: std::slice::from_ref(verified_command),
                item_ref: &sandbox_item_ref,
                thread_id,
            },
        )
        .map_err(|error| anyhow::anyhow!("sandbox apply failed: {error}"))?;
    let spawned = match lillux::spawn(request) {
        Ok(spawned) => spawned,
        Err(result) => {
            return Ok(SpawnedRuntime {
                process: None,
                attached_process: None,
                workspace_lifeline,
                immediate_result: Some(runtime_failure_result(
                    &result.stderr,
                    result.timed_out,
                    result.output_limit_exceeded.map(|limit| limit.as_str()),
                )),
            });
        }
    };
    let process_identity =
        match crate::execution::process_attachment::capture_or_adopt_owned_identity(
            state,
            thread_id,
            spawned.pid as i64,
            spawned.pgid,
        ) {
            Ok(identity) => identity,
            Err(error) => {
                spawned.abort();
                drop(workspace_lifeline);
                return Err(error.context("capture managed runtime process identity"));
            }
        };
    // Install compare-clear ownership before the in-process attach. The runtime
    // can win the UDS self-attach race, then stop/finalize before this call.
    let attached_process = AttachedProcessGuard {
        state: state.clone(),
        thread_id: thread_id.to_string(),
        identity: process_identity.clone(),
    };
    if let Err(error) =
        state
            .threads
            .attach_process(&ryeos_app::thread_lifecycle::ThreadAttachProcessParams {
                thread_id: thread_id.to_string(),
                pid: spawned.pid as i64,
                pgid: spawned.pgid,
                process_identity: Some(process_identity.clone()),
                metadata: None,
                // Spawn metadata was seeded before launch. An empty self-attach
                // preserves it while establishing the immutable process identity.
                launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata::default(),
            })
    {
        spawned.abort();
        drop(workspace_lifeline);
        return Err(error.context("attach managed runtime process identity"));
    }
    Ok(SpawnedRuntime {
        process: Some(spawned),
        attached_process: Some(attached_process),
        workspace_lifeline,
        immediate_result: None,
    })
}

fn runtime_failure_result(
    stderr: &str,
    timed_out: bool,
    output_limit: Option<&str>,
) -> RuntimeResult {
    RuntimeResult {
        success: false,
        status: if timed_out {
            RuntimeResultStatus::TimedOut
        } else {
            RuntimeResultStatus::Failed
        },
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
            stdout_prefix(stdout, 500)
        )
    })
}

fn stdout_prefix(stdout: &str, max_bytes: usize) -> &str {
    let mut end = stdout.len().min(max_bytes);
    while !stdout.is_char_boundary(end) {
        end -= 1;
    }
    &stdout[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subprocess_failure_preserves_stderr_and_failed_status() {
        let result = runtime_failure_result("permission denied", false, None);

        assert!(!result.success);
        assert_eq!(result.status, RuntimeResultStatus::Failed);
        assert_eq!(result.result, Some(json!("permission denied")));
        assert_eq!(result.outputs, Value::Null);
    }

    #[test]
    fn subprocess_timeout_uses_timed_out_status() {
        assert_eq!(
            runtime_failure_result("deadline", true, None).status,
            RuntimeResultStatus::TimedOut
        );
    }

    #[test]
    fn subprocess_output_limit_preserves_the_explicit_reason() {
        let result = runtime_failure_result("retention exceeded", false, Some("stdout"));

        assert_eq!(result.status, RuntimeResultStatus::Failed);
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

    #[test]
    fn stdout_decode_rejects_success_status_contradiction() {
        let stdout = serde_json::json!({
            "success": false,
            "status": "completed",
            "thread_id": "T-test",
            "outputs": null,
            "warnings": [],
        })
        .to_string();

        let error = decode_runtime_stdout(&stdout).unwrap_err().to_string();
        assert!(error.contains("failed to parse runtime stdout"));
        assert!(error.contains("contradicts `status` `completed`"));
    }

    #[test]
    fn stdout_decode_error_truncates_on_utf8_boundary() {
        let stdout = format!("{}é", "x".repeat(499));

        let error = decode_runtime_stdout(&stdout).unwrap_err().to_string();
        assert!(error.contains("failed to parse runtime stdout"));
        assert!(error.contains(&"x".repeat(499)));
    }
}
