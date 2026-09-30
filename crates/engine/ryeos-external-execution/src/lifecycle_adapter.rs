//! Descriptor-bound execution boundary for signed lifecycle adapters.
//!
//! Application code owns durable operation intent and provider-specific
//! settings. This module constructs the admitted descriptor-bound lifecycle
//! protocol invocation. Lillux owns process acquisition, execution,
//! observation and settlement.

use anyhow::{Result, ensure};

use ryeos_external_execution_contract::{
    LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV, LIFECYCLE_REQUEST_FD_ENV, MAX_LIFECYCLE_RESPONSE_BYTES,
};

const MAX_LIFECYCLE_ADAPTER_STDERR_BYTES: u64 = 64 * 1024;
// A maximum guest projection carries 64 mounts and 64 separate exact product
// manifests, plus the sealed request, bootstrap and admitted artifacts.
const MAX_LIFECYCLE_ADAPTER_OPEN_FILES: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAdapterInvocation {
    Inspect,
    VerifyRuntimeProbe,
    Operate,
    ProduceSnapshot,
    ProduceSnapshotUpload,
    ProduceSnapshotCreate,
    BootstrapSourceCreate,
    BootstrapSourceReadiness,
    BootstrapSourceTerminate,
    BootstrapSourceObserveTermination,
    ObserveSnapshotReadiness,
    QualifySnapshotCreate,
    QualifySnapshotVerify,
    QualifySnapshotTerminate,
    ObserveSnapshotTermination,
}

/// Complete bounded protocol observation, not permission to continue startup.
/// A late operation may carry exact lifecycle evidence that must be retained
/// before its owner refuses further execution. Inspection never returns late.
pub struct LifecycleAdapterOutput {
    pub bytes: Vec<u8>,
    pub deadline_exceeded: bool,
}

impl LifecycleAdapterInvocation {
    fn maximum_response_bytes(self) -> usize {
        if self == Self::QualifySnapshotVerify {
            ryeos_external_execution_contract::restored_runtime_measurement::MAX_RESTORED_VERIFIER_ADAPTER_RESPONSE_BYTES
        } else {
            MAX_LIFECYCLE_RESPONSE_BYTES
        }
    }

    fn argument(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::VerifyRuntimeProbe => "verify-runtime-probe",
            Self::Operate => "operate",
            Self::ProduceSnapshot => "produce-snapshot",
            Self::ProduceSnapshotUpload => "produce-snapshot-upload",
            Self::ProduceSnapshotCreate => "produce-snapshot-create",
            Self::BootstrapSourceCreate => "bootstrap-source-create",
            Self::BootstrapSourceReadiness => "bootstrap-source-readiness",
            Self::BootstrapSourceTerminate => "bootstrap-source-terminate",
            Self::BootstrapSourceObserveTermination => "bootstrap-source-observe-termination",
            Self::ObserveSnapshotReadiness => "observe-snapshot-readiness",
            Self::QualifySnapshotCreate => "qualify-snapshot-create",
            Self::QualifySnapshotVerify => "qualify-snapshot-verify",
            Self::QualifySnapshotTerminate => "qualify-snapshot-terminate",
            Self::ObserveSnapshotTermination => "observe-snapshot-termination",
        }
    }

    fn requires_no_process_creation(self) -> bool {
        matches!(
            self,
            Self::ProduceSnapshot | Self::ProduceSnapshotUpload | Self::ProduceSnapshotCreate
        )
    }
}

pub fn run_lifecycle_adapter(
    adapter: &lillux::InheritedDescriptorAuthority,
    invocation: LifecycleAdapterInvocation,
    request: &lillux::InheritedDescriptorAuthority,
    mut inherited: Vec<lillux::InheritedDescriptorAuthority>,
    mut environment: Vec<(String, String)>,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<LifecycleAdapterOutput> {
    ensure!(
        !deadline.has_elapsed(),
        "external lifecycle adapter deadline expired"
    );
    let descriptors = lillux::retain_fork_sensitive_descriptors_until(deadline)?;
    adapter.require_owned_executable()?;
    let request_descriptor = request.inherited_descriptor().map_err(anyhow::Error::msg)?;
    ensure!(
        environment
            .iter()
            .all(|(name, _)| name != LIFECYCLE_REQUEST_FD_ENV
                && name != LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV),
        "external lifecycle adapter protected environment is duplicated"
    );
    environment.push((
        LIFECYCLE_REQUEST_FD_ENV.into(),
        request_descriptor.to_string(),
    ));
    inherited.push(request.clone());

    let mut process = lillux::SubprocessRequest {
        cmd: String::new(),
        argv0: Some("ryeos-external-lifecycle-adapter".into()),
        args: vec![invocation.argument().into()],
        cwd: Some("/".into()),
        envs: environment,
        stdin_data: None,
        timeout: deadline.remaining().as_secs_f64(),
        limits: Some(lillux::SubprocessLimits {
            max_open_files: Some(MAX_LIFECYCLE_ADAPTER_OPEN_FILES),
            max_stdout_bytes: Some(invocation.maximum_response_bytes() as u64),
            max_stderr_bytes: Some(MAX_LIFECYCLE_ADAPTER_STDERR_BYTES),
            deny_process_creation: invocation.requires_no_process_creation(),
            ..lillux::SubprocessLimits::default()
        }),
        inherited_fds: inherited,
        inherited_fd_mappings: Vec::new(),
        supervised_status: None,
    };
    let adapter_descriptor = adapter
        .bind_as_subprocess_executable_at_source(&mut process)
        .map_err(anyhow::Error::msg)?;
    process.envs.push((
        LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV.into(),
        adapter_descriptor.to_string(),
    ));
    // Refresh only the remaining fraction of the SAME absolute authority;
    // descriptor preparation may not restart a full contact allowance.
    process.timeout = deadline.remaining().as_secs_f64();
    ensure!(
        process.timeout > 0.0,
        "external lifecycle adapter deadline expired before spawn"
    );
    drop(descriptors);
    let result = match lillux::spawn_until(process, deadline) {
        Ok(process) => process.wait(),
        Err(result) => result,
    };
    lifecycle_adapter_output(invocation, result)
}

fn lifecycle_adapter_output(
    invocation: LifecycleAdapterInvocation,
    result: lillux::SubprocessResult,
) -> Result<LifecycleAdapterOutput> {
    // The adapter receives sealed credentials and bootstrap material. Neither
    // its diagnostics nor a launcher diagnostic is public error authority.
    // Errors retain only typed host observations, never adapter-controlled
    // diagnostics. Complete bounded response bytes go solely to the trusted
    // protocol decoder below this invocation boundary.
    let failure = if result.timed_out {
        "deadline_exceeded"
    } else if result.output_limit_exceeded.is_some() {
        "output_limit_exceeded"
    } else {
        "unsuccessful_exit"
    };
    // This private runner uses direct spawn_until: no process scope or
    // supervised attachment is admitted. Snapshot production uses Lillux's
    // fail-closed no-process-creation limit, which excludes local descendants
    // but does not settle remote provider work. A complete late
    // exit is protocol observation, never timely host success.
    let complete = result.exit_code == 0
        && result.output_limit_exceeded.is_none()
        && !result.stdout_truncated
        && !result.stderr_truncated
        && result.launcher_refusal.is_none()
        && result.aborted_before_attachment.is_none()
        && !result.stdout.is_empty()
        && result.stdout.len() <= invocation.maximum_response_bytes();
    ensure!(
        complete
            && ((result.success && !result.timed_out)
                || (result.timed_out
                    && matches!(
                        invocation,
                        LifecycleAdapterInvocation::Operate
                            | LifecycleAdapterInvocation::ProduceSnapshot
                            | LifecycleAdapterInvocation::ProduceSnapshotUpload
                            | LifecycleAdapterInvocation::ProduceSnapshotCreate
                            | LifecycleAdapterInvocation::BootstrapSourceCreate
                            | LifecycleAdapterInvocation::BootstrapSourceTerminate
                            | LifecycleAdapterInvocation::BootstrapSourceObserveTermination
                            | LifecycleAdapterInvocation::ObserveSnapshotReadiness
                            | LifecycleAdapterInvocation::QualifySnapshotCreate
                            | LifecycleAdapterInvocation::QualifySnapshotVerify
                            | LifecycleAdapterInvocation::QualifySnapshotTerminate
                            | LifecycleAdapterInvocation::ObserveSnapshotTermination
                    ))),
        "external lifecycle adapter failed: {failure} (exit_code={})",
        result.exit_code
    );
    Ok(LifecycleAdapterOutput {
        bytes: result.stdout.into_bytes(),
        deadline_exceeded: result.timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verifier_observation_has_the_larger_evidence_envelope() {
        assert!(
            LifecycleAdapterInvocation::QualifySnapshotVerify.maximum_response_bytes()
                > MAX_LIFECYCLE_RESPONSE_BYTES
        );
        for invocation in [
            LifecycleAdapterInvocation::Inspect,
            LifecycleAdapterInvocation::VerifyRuntimeProbe,
            LifecycleAdapterInvocation::Operate,
            LifecycleAdapterInvocation::QualifySnapshotCreate,
            LifecycleAdapterInvocation::QualifySnapshotTerminate,
        ] {
            assert_eq!(
                invocation.maximum_response_bytes(),
                MAX_LIFECYCLE_RESPONSE_BYTES
            );
        }
    }

    #[test]
    fn no_process_creation_is_scoped_to_snapshot_mutations() {
        assert!(LifecycleAdapterInvocation::ProduceSnapshot.requires_no_process_creation());
        assert!(LifecycleAdapterInvocation::ProduceSnapshotUpload.requires_no_process_creation());
        assert!(LifecycleAdapterInvocation::ProduceSnapshotCreate.requires_no_process_creation());
        for invocation in [
            LifecycleAdapterInvocation::Operate,
            LifecycleAdapterInvocation::BootstrapSourceCreate,
            LifecycleAdapterInvocation::BootstrapSourceTerminate,
            LifecycleAdapterInvocation::QualifySnapshotCreate,
            LifecycleAdapterInvocation::QualifySnapshotTerminate,
            LifecycleAdapterInvocation::ObserveSnapshotReadiness,
        ] {
            assert!(!invocation.requires_no_process_creation());
        }
    }

    fn late_result() -> lillux::SubprocessResult {
        lillux::SubprocessResult {
            success: false,
            stdout: "{\"observation\":true}".into(),
            stderr: "PRIVATE_DIAGNOSTIC".into(),
            exit_code: 0,
            duration_ms: 50.0,
            pid: 7,
            timed_out: true,
            launcher_refusal: None,
            aborted_before_attachment: None,
            output_limit_exceeded: None,
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn late_complete_operation_is_only_an_observation_and_never_inspection() {
        let observation =
            lifecycle_adapter_output(LifecycleAdapterInvocation::Operate, late_result()).unwrap();
        assert!(observation.deadline_exceeded);
        assert_eq!(observation.bytes, b"{\"observation\":true}");
        let snapshot =
            lifecycle_adapter_output(LifecycleAdapterInvocation::ProduceSnapshot, late_result())
                .unwrap();
        assert!(snapshot.deadline_exceeded);
        assert_eq!(snapshot.bytes, observation.bytes);
        for success in [false, true] {
            let mut result = late_result();
            result.success = success;
            assert!(lifecycle_adapter_output(LifecycleAdapterInvocation::Inspect, result).is_err());
        }
    }

    #[test]
    fn incomplete_or_refused_late_adapter_output_is_not_observation_authority() {
        for fault in [
            "exit", "stdout", "stderr", "overflow", "launcher", "aborted", "empty", "oversize",
        ] {
            let mut result = late_result();
            match fault {
                "exit" => result.exit_code = 1,
                "stdout" => result.stdout_truncated = true,
                "stderr" => result.stderr_truncated = true,
                "overflow" => {
                    result.output_limit_exceeded = Some(lillux::OutputLimitExceeded::Stdout)
                }
                "launcher" => result.launcher_refusal = Some("PRIVATE_DIAGNOSTIC".into()),
                "aborted" => {
                    result.aborted_before_attachment =
                        Some(lillux::AbortedProcess { pid: 7, pgid: 7 })
                }
                "empty" => result.stdout.clear(),
                "oversize" => result.stdout = "x".repeat(MAX_LIFECYCLE_RESPONSE_BYTES + 1),
                _ => unreachable!(),
            }
            let error = lifecycle_adapter_output(LifecycleAdapterInvocation::Operate, result)
                .err()
                .unwrap();
            assert!(!format!("{error:#}").contains("PRIVATE_DIAGNOSTIC"));
            assert!(!format!("{error:#}").contains("observation"));
        }
        // Even a physically complete stdout is not a valid protocol response
        // until the app's strict decoder and validate_for(request) accept it.
        let mut partial = late_result();
        partial.stdout = "{\"schema\":".into();
        let output =
            lifecycle_adapter_output(LifecycleAdapterInvocation::Operate, partial).unwrap();
        assert!(
            ryeos_external_execution_contract::from_json_slice_strict::<
                ryeos_external_execution_contract::LifecycleAdapterResponse,
            >(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
            .is_err()
        );
    }

    #[test]
    fn failure_diagnostics_never_disclose_adapter_output() {
        for timed_out in [false, true] {
            let secret = "SEALED_ADAPTER_CREDENTIAL_SENTINEL";
            let result = lillux::SubprocessResult {
                success: false,
                stdout: secret.into(),
                stderr: secret.into(),
                exit_code: 17,
                duration_ms: 1.0,
                pid: 1,
                timed_out,
                launcher_refusal: Some(secret.into()),
                aborted_before_attachment: None,
                output_limit_exceeded: None,
                stdout_truncated: false,
                stderr_truncated: false,
            };
            let error = lifecycle_adapter_output(LifecycleAdapterInvocation::Operate, result)
                .err()
                .unwrap();
            assert!(!format!("{error:#}").contains(secret));
            assert!(!format!("{error:?}").contains(secret));
            assert!(error.to_string().contains("exit_code=17"));
            assert!(error.to_string().contains(if timed_out {
                "deadline_exceeded"
            } else {
                "unsuccessful_exit"
            }));
        }
    }
}
