use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result};
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
    pub project_authority: ryeos_engine::isolation::IsolationProjectAuthority,
    pub live_access: Option<ryeos_engine::isolation::IsolationLiveAccessAuthority>,
    pub state_root: Option<&'a Path>,
    pub workspace_lifeline: Option<std::sync::Arc<ryeos_app::temp_dir_guard::TempDirGuard>>,
    pub envelope: &'a LaunchEnvelope,
    pub timeout_secs: u64,
    pub callback: &'a EnvelopeCallback,
    pub thread_id: &'a str,
    pub launch_owner: &'a str,
    pub vault_bindings: &'a [(String, String)],
    pub thread_auth_token: &'a str,
    pub roots: ryeos_app::env_contract::DaemonRootEnv,
    pub isolation: &'a ryeos_engine::isolation::IsolationRuntime,
    pub verified_command: &'a ryeos_engine::isolation::IsolationDescriptorBoundCommand,
    pub cas_root: &'a Path,
    /// Daemon-allocated checkpoint dir for a replay-aware runtime.
    pub checkpoint_dir: Option<&'a Path>,
    /// Whether the replay-aware runtime should load that checkpoint.
    pub is_resume: bool,
}

pub(super) struct SpawnedRuntime {
    thread_id: String,
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
        emit_captured_child_timing_records(
            &self.thread_id,
            &result.stderr,
            result.stderr_truncated,
        );
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
    launch_owner: String,
    identity: ryeos_app::process::ExecutionProcessIdentity,
}

impl Drop for AttachedProcessGuard {
    fn drop(&mut self) {
        match self
            .state
            .state_store
            .clear_thread_process_if_matches_owned(
                &self.thread_id,
                &self.identity,
                &self.launch_owner,
            ) {
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
        live_access,
        state_root,
        workspace_lifeline,
        envelope,
        timeout_secs,
        callback,
        thread_id,
        launch_owner,
        vault_bindings,
        thread_auth_token,
        roots,
        isolation,
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
    let isolation_daemon_socket_path =
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
    let isolation_item_ref = item_ref.to_string();
    let applied = isolation
        .apply_awaiting_attachment_with_provenance(
            request,
            ryeos_engine::isolation::IsolationLaunchContext {
                project_path: &spec.project_path,
                project_authority,
                live_access: live_access.as_ref(),
                state_root,
                checkpoint_dir,
                daemon_socket_path: isolation_daemon_socket_path,
                bundle_roots: &envelope.roots.bundle_roots,
                node_trusted_keys_dir: Some(&envelope.roots.node_trusted_keys_dir),
                verified_code: &[],
                verified_command: Some(verified_command),
                item_ref: &isolation_item_ref,
                thread_id,
            },
        )
        .map_err(|error| anyhow::anyhow!("isolation apply failed: {error}"))?;
    state
        .state_store
        .seed_isolation_provenance(thread_id, applied.provenance)
        .context("persist managed-runtime isolation provenance")?;
    let request = applied.request;
    let spawned = match request.spawn() {
        Ok(spawned) => spawned,
        Err(result) => {
            emit_captured_child_timing_records(thread_id, &result.stderr, result.stderr_truncated);
            return Ok(SpawnedRuntime {
                thread_id: thread_id.to_string(),
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
    #[cfg(target_os = "linux")]
    let process_identity_result =
        ryeos_app::process::capture_execution_process_identity_from_pidfd(
            spawned.pid() as i64,
            Some(spawned.pgid()),
            spawned.pidfd(),
        )
        .context("capture held managed-runtime identity from Lillux pidfd");
    #[cfg(not(target_os = "linux"))]
    let process_identity_result = ryeos_app::process::capture_execution_process_identity(
        spawned.pid() as i64,
        Some(spawned.pgid()),
    )
    .context("capture held managed-runtime identity");
    let process_identity = match process_identity_result {
        Ok(identity) => identity,
        Err(error) => {
            let cleanup = spawned.abort_and_reap().err();
            drop(workspace_lifeline);
            return Err(match cleanup {
                Some(cleanup) => {
                    error.context(format!("pending-process cleanup failed: {cleanup}"))
                }
                None => error,
            });
        }
    };
    // The runtime cannot self-attach before release. An existing identity at
    // this boundary is therefore an invariant violation, not an adoption race.
    if let Err(error) = state.threads.attach_new_process_owned(
        &ryeos_app::thread_lifecycle::ThreadAttachProcessParams {
            thread_id: thread_id.to_string(),
            pid: spawned.pid() as i64,
            pgid: spawned.pgid(),
            process_identity: Some(process_identity.clone()),
            metadata: None,
            // Spawn metadata was seeded before launch. An empty self-attach
            // preserves it while establishing the immutable process identity.
            launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata::default(),
        },
        launch_owner,
    ) {
        let cleanup = spawned.abort_and_reap().err();
        drop(workspace_lifeline);
        let error = error.context("attach held managed runtime process identity");
        return match cleanup {
            Some(cleanup) => {
                Err(error.context(format!("pending-process cleanup failed: {cleanup}")))
            }
            None => Err(error),
        };
    }
    let attached_process = AttachedProcessGuard {
        state: state.clone(),
        thread_id: thread_id.to_string(),
        launch_owner: launch_owner.to_string(),
        identity: process_identity.clone(),
    };
    if let Err(error) =
        state
            .threads
            .authorize_process_release_owned(thread_id, &process_identity, launch_owner)
    {
        let cleanup = spawned.abort_and_reap().err();
        let stop_settlement =
            super::super::process_attachment::finalize_requested_stop_if_present(state, thread_id);
        drop(workspace_lifeline);
        let error = match stop_settlement {
            Ok(true) => {
                anyhow::anyhow!("managed runtime stopped before attachment release: {error}")
            }
            Ok(false) => error.context("authorize managed runtime release after durable attachment"),
            Err(stop_error) => error.context(format!(
                "authorize managed runtime release after durable attachment; stop settlement also failed: {stop_error:#}"
            )),
        };
        return match cleanup {
            Some(cleanup) => {
                Err(error.context(format!("pending-process cleanup failed: {cleanup}")))
            }
            None => Err(error),
        };
    }
    let spawned = spawned
        .release_after_attachment()
        .context("release managed runtime after durable process attachment")?;
    Ok(SpawnedRuntime {
        thread_id: thread_id.to_string(),
        process: Some(spawned),
        attached_process: Some(attached_process),
        workspace_lifeline,
        immediate_result: None,
    })
}

const MAX_CAPTURED_CHILD_TIMING_RECORDS: usize = 128;
const MAX_CAPTURED_CHILD_TIMING_RECORD_BYTES: usize = 64 * 1024;
const MAX_CAPTURED_CHILD_TIMING_ID_BYTES: usize = 256;
const MAX_CAPTURED_CHILD_PROVIDER_ID_BYTES: usize = 256;
const MAX_CAPTURED_CHILD_HTTP_VERSION_BYTES: usize = 32;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CapturedProviderCallTiming {
    call_id: u64,
    provider_id: String,
    turn: u32,
    attempt: u32,
    call_started_us: u64,
    request_submitted_us: Option<u64>,
    response_headers_us: Option<u64>,
    http_version: Option<String>,
    first_provider_event_us: Option<u64>,
    dns_lookup_first_started_us: Option<u64>,
    dns_lookup_last_done_us: Option<u64>,
    dns_lookup_count: u32,
    dns_lookup_completed_count: u32,
    dns_lookup_total_us: u64,
    dns_lookup_failures: u32,
    connection_establishment_first_started_us: Option<u64>,
    connection_establishment_last_done_us: Option<u64>,
    connection_establishment_count: u32,
    connection_establishment_completed_count: u32,
    connection_establishment_total_us: u64,
    connection_establishment_failures: u32,
    call_finished_us: Option<u64>,
    completion: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CapturedDirectiveStageTiming {
    invocation_id: Option<String>,
    thread_id: Option<String>,
    envelope_parsed_us: Option<u64>,
    attach_process_started_us: Option<u64>,
    attach_process_done_us: Option<u64>,
    mark_running_started_us: Option<u64>,
    mark_running_done_us: Option<u64>,
    bootstrap_done_us: Option<u64>,
    provider_request_submitted_us: Option<u64>,
    provider_response_headers_us: Option<u64>,
    provider_http_version: Option<String>,
    first_provider_event_us: Option<u64>,
    first_non_whitespace_text_us: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", deny_unknown_fields)]
enum CapturedChildTimingRecord {
    #[serde(rename = "directive_provider_call_timing")]
    ProviderCall {
        schema_version: u32,
        clock_domain: String,
        invocation_id: Option<String>,
        thread_id: Option<String>,
        item_ref_kind: String,
        dns_lookup_scope: String,
        connection_establishment_scope: String,
        exact_tcp_tls_split_available: bool,
        timing: CapturedProviderCallTiming,
    },
    #[serde(rename = "directive_runtime_stage_timing")]
    RuntimeStage {
        schema_version: u32,
        clock_domain: String,
        invocation_id: Option<String>,
        thread_id: Option<String>,
        item_ref_kind: String,
        connection_establishment_scope: String,
        exact_tcp_tls_split_available: bool,
        timing: CapturedDirectiveStageTiming,
        first_provider_call: Option<Box<CapturedProviderCallTiming>>,
        summary_emitted_us: u64,
        completion: String,
    },
}

impl CapturedProviderCallTiming {
    fn validate(&self) -> bool {
        self.call_id > 0
            && self.attempt > 0
            && bounded_nonempty(&self.provider_id, MAX_CAPTURED_CHILD_PROVIDER_ID_BYTES)
            && self
                .http_version
                .as_deref()
                .is_none_or(|value| bounded_nonempty(value, MAX_CAPTURED_CHILD_HTTP_VERSION_BYTES))
            && self.completion.as_deref().is_none_or(|completion| {
                matches!(
                    completion,
                    "completed" | "interrupted" | "cancelled" | "error"
                )
            })
    }
}

impl CapturedChildTimingRecord {
    fn validate(&self, expected_thread_id: &str) -> bool {
        match self {
            Self::ProviderCall {
                schema_version,
                clock_domain,
                invocation_id,
                thread_id,
                item_ref_kind,
                dns_lookup_scope,
                connection_establishment_scope,
                exact_tcp_tls_split_available,
                timing,
            } => {
                *schema_version == 1
                    && clock_domain == "directive_process_monotonic"
                    && item_ref_kind == "directive"
                    && dns_lookup_scope == "exact_resolver_future"
                    && connection_establishment_scope
                        == "aggregate_reqwest_connector_may_include_dns_tcp_proxy_tls"
                    && !*exact_tcp_tls_split_available
                    && valid_identity(invocation_id.as_deref())
                    && thread_id.as_deref() == Some(expected_thread_id)
                    && timing.validate()
            }
            Self::RuntimeStage {
                schema_version,
                clock_domain,
                invocation_id,
                thread_id,
                item_ref_kind,
                connection_establishment_scope,
                exact_tcp_tls_split_available,
                timing,
                first_provider_call,
                completion,
                ..
            } => {
                *schema_version == 2
                    && clock_domain == "directive_process_monotonic"
                    && item_ref_kind == "directive"
                    && connection_establishment_scope
                        == "aggregate_reqwest_connector_may_include_dns_tcp_proxy_tls"
                    && !*exact_tcp_tls_split_available
                    && valid_identity(invocation_id.as_deref())
                    && thread_id.as_deref() == Some(expected_thread_id)
                    && timing.invocation_id.as_deref() == invocation_id.as_deref()
                    && timing.thread_id.as_deref() == thread_id.as_deref()
                    && timing.provider_http_version.as_deref().is_none_or(|value| {
                        bounded_nonempty(value, MAX_CAPTURED_CHILD_HTTP_VERSION_BYTES)
                    })
                    && first_provider_call
                        .as_ref()
                        .is_none_or(|timing| timing.validate())
                    && matches!(
                        completion.as_str(),
                        "first_non_whitespace_text_published" | "process_exit"
                    )
            }
        }
    }
}

fn bounded_nonempty(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes
}

fn valid_identity(identity: Option<&str>) -> bool {
    identity.is_some_and(|value| bounded_nonempty(value, MAX_CAPTURED_CHILD_TIMING_ID_BYTES))
}

fn decode_captured_child_timing_record(
    expected_thread_id: &str,
    encoded: &str,
) -> Option<CapturedChildTimingRecord> {
    if encoded.len() > MAX_CAPTURED_CHILD_TIMING_RECORD_BYTES {
        return None;
    }
    let record = serde_json::from_str::<CapturedChildTimingRecord>(encoded).ok()?;
    record.validate(expected_thread_id).then_some(record)
}

fn emit_captured_child_timing_records(
    expected_thread_id: &str,
    stderr: &str,
    stderr_truncated: bool,
) {
    let mut observed = 0usize;
    let mut emitted = 0usize;
    let mut record_limit_exceeded = false;
    for line in stderr.lines() {
        let Some(encoded) = line.strip_prefix(ryeos_runtime::events::CAPTURED_CHILD_TIMING_PREFIX)
        else {
            continue;
        };
        if observed == MAX_CAPTURED_CHILD_TIMING_RECORDS {
            record_limit_exceeded = true;
            break;
        }
        observed = observed.saturating_add(1);
        let Some(record) = decode_captured_child_timing_record(expected_thread_id, encoded) else {
            tracing::warn!(
                thread_id = expected_thread_id,
                "discarding invalid captured child timing record"
            );
            continue;
        };
        let normalized = match serde_json::to_string(&record) {
            Ok(normalized) => normalized,
            Err(error) => {
                tracing::warn!(
                    thread_id = expected_thread_id,
                    %error,
                    "failed to normalize validated captured child timing record"
                );
                continue;
            }
        };
        emitted = emitted.saturating_add(1);
        match record {
            CapturedChildTimingRecord::ProviderCall {
                schema_version,
                invocation_id,
                timing,
                ..
            } => tracing::info!(
                event = "runtime_child_timing_record",
                child_event = "directive_provider_call_timing",
                child_schema_version = schema_version,
                invocation_id = invocation_id.as_deref(),
                thread_id = expected_thread_id,
                provider_call_id = timing.call_id,
                provider_id = timing.provider_id.as_str(),
                turn = timing.turn,
                attempt = timing.attempt,
                completion = timing.completion.as_deref(),
                request_submitted_us = timing.request_submitted_us,
                response_headers_us = timing.response_headers_us,
                first_provider_event_us = timing.first_provider_event_us,
                dns_lookup_total_us = timing.dns_lookup_total_us,
                connection_establishment_total_us = timing.connection_establishment_total_us,
                exact_tcp_tls_split_available = false,
                child_timing_json = normalized.as_str(),
                "captured runtime child provider timing record"
            ),
            CapturedChildTimingRecord::RuntimeStage {
                schema_version,
                invocation_id,
                timing,
                completion,
                ..
            } => tracing::info!(
                event = "runtime_child_timing_record",
                child_event = "directive_runtime_stage_timing",
                child_schema_version = schema_version,
                invocation_id = invocation_id.as_deref(),
                thread_id = expected_thread_id,
                completion = completion.as_str(),
                envelope_parsed_us = timing.envelope_parsed_us,
                attach_process_done_us = timing.attach_process_done_us,
                mark_running_done_us = timing.mark_running_done_us,
                bootstrap_done_us = timing.bootstrap_done_us,
                provider_request_submitted_us = timing.provider_request_submitted_us,
                provider_response_headers_us = timing.provider_response_headers_us,
                first_provider_event_us = timing.first_provider_event_us,
                first_non_whitespace_text_us = timing.first_non_whitespace_text_us,
                exact_tcp_tls_split_available = false,
                child_timing_json = normalized.as_str(),
                "captured runtime child stage timing record"
            ),
        }
    }
    if record_limit_exceeded {
        tracing::warn!(
            thread_id = expected_thread_id,
            accepted_timing_record_limit = MAX_CAPTURED_CHILD_TIMING_RECORDS,
            "runtime emitted more child timing records than the daemon accepts"
        );
    }
    if stderr_truncated {
        tracing::warn!(
            thread_id = expected_thread_id,
            emitted_timing_records = emitted,
            "runtime stderr was truncated; child timing records may be incomplete"
        );
    }
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

    #[test]
    fn captured_child_timing_is_exact_schema_and_thread_bound() {
        let stage = CapturedDirectiveStageTiming {
            invocation_id: Some("invocation-1".to_string()),
            thread_id: Some("T-expected".to_string()),
            envelope_parsed_us: Some(1),
            attach_process_started_us: Some(2),
            attach_process_done_us: Some(3),
            mark_running_started_us: Some(4),
            mark_running_done_us: Some(5),
            bootstrap_done_us: Some(6),
            provider_request_submitted_us: Some(7),
            provider_response_headers_us: Some(8),
            provider_http_version: Some("HTTP/2.0".to_string()),
            first_provider_event_us: Some(9),
            first_non_whitespace_text_us: Some(10),
        };
        let record = CapturedChildTimingRecord::RuntimeStage {
            schema_version: 2,
            clock_domain: "directive_process_monotonic".to_string(),
            invocation_id: Some("invocation-1".to_string()),
            thread_id: Some("T-expected".to_string()),
            item_ref_kind: "directive".to_string(),
            connection_establishment_scope:
                "aggregate_reqwest_connector_may_include_dns_tcp_proxy_tls".to_string(),
            exact_tcp_tls_split_available: false,
            timing: stage,
            first_provider_call: None,
            summary_emitted_us: 11,
            completion: "first_non_whitespace_text_published".to_string(),
        };
        let encoded = serde_json::to_string(&record).expect("encode timing fixture");

        assert!(decode_captured_child_timing_record("T-expected", &encoded).is_some());
        assert!(decode_captured_child_timing_record("T-other", &encoded).is_none());

        let mut unknown = serde_json::to_value(&record).expect("timing fixture value");
        unknown
            .as_object_mut()
            .expect("record object")
            .insert("content".to_string(), json!("must never be accepted"));
        assert!(
            decode_captured_child_timing_record("T-expected", &unknown.to_string()).is_none(),
            "unknown content-bearing fields must fail closed"
        );

        let mut wrong_version = serde_json::to_value(&record).expect("timing fixture value");
        wrong_version["schema_version"] = json!(1);
        assert!(
            decode_captured_child_timing_record("T-expected", &wrong_version.to_string()).is_none(),
            "predecessor timing schemas are not accepted"
        );
    }
}
