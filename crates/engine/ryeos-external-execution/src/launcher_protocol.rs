//! Exact inherited-channel protocol between the protected supervisor and one
//! dedicated native candidate launcher.

use std::io::{Read as _, Write as _};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_state::external_execution::guest_journal::AuthenticatedLauncherReady;
use ryeos_state::external_execution::guest_journal::{
    DurableNativeCandidateCapture, LauncherOccurrenceEvidence, PreparedGuestJournal,
};
use ryeos_state::external_execution::{
    AuthenticatedExecutionFrame, ExecutionChannelBinding, ExecutionChannelPayload,
    ExternalCommandOutputCommitment, ExternalCommandOutputStream, ExternalCommandTermination,
    ExternalCommandTerminationReason, ExternalExecutionMode, ExternalTargetExit,
    SignedExecutionFrame,
};
use serde::{Deserialize, Serialize};

use crate::backends::linux::NativeExternalCandidate;
use crate::launcher::{LAUNCHER_CONTROL_FD, PreparedExternalCandidateLauncherRequest};
use crate::supervisor::{
    CandidateExecutionOutput, ExternalCandidateLauncherClient,
    SerializedExternalCandidateSupervisor, SupervisorApplicationOutcome,
};

const PROTOCOL_SCHEMA: u32 = 1;
const MAX_MESSAGE_BYTES: usize = ryeos_state::external_execution::MAX_FRAME_BYTES + 16 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LauncherMessage {
    Bootstrap {
        schema: u32,
        binding: ExecutionChannelBinding,
        challenge: String,
        occurrence_digest: String,
    },
    Ready {
        schema: u32,
        binding_digest: String,
        challenge_digest: String,
    },
    Apply {
        schema: u32,
        frame_json: String,
    },
    Applied {
        schema: u32,
        frame_digest: String,
        written_bytes: Option<usize>,
    },
    Captured {
        schema: u32,
        capture: DurableNativeCandidateCapture,
    },
    AcknowledgeFinish {
        schema: u32,
        frame_digest: String,
    },
    FinishAcknowledged {
        schema: u32,
        frame_digest: String,
    },
    PollAppliedLaunch {
        schema: u32,
    },
    AppliedLaunch {
        schema: u32,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        receipt: Option<lillux::LinuxSandboxAppliedLaunchReceipt>,
    },
    PollOutput {
        schema: u32,
        maximum_bytes: usize,
    },
    Output {
        schema: u32,
        bytes_base64: String,
        closed: bool,
    },
    CommandOutput {
        schema: u32,
        stream: ExternalCommandOutputStream,
        offset: u64,
        bytes_base64: String,
    },
    CommandTerminated {
        schema: u32,
        observation: ExternalCommandTermination,
    },
    Failed {
        schema: u32,
        frame_digest: String,
        detail: String,
    },
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub struct InheritedExternalCandidateLauncherClient {
    channel: lillux::InheritedDuplexChannel,
    binding: ExecutionChannelBinding,
    deadline: lillux::time::MonotonicDeadline,
}

/// Exact live composition retained after launch intent, held-process binding,
/// inherited-channel authentication and journal readiness all agree.
pub struct LiveInheritedExternalCandidateSupervisor {
    supervisor: SerializedExternalCandidateSupervisor<InheritedExternalCandidateLauncherClient>,
    launcher_process: lillux::RunningProcess,
    process_identity: lillux::ExactProcessIdentity,
    occurrence_digest: String,
    authority: ryeos_state::PinnedStateAuthority,
    supervisor_signing_key: lillux::crypto::SigningKey,
    bootstrap_digest: String,
    ready_frame: String,
    applied_runtime_recorded: bool,
    runtime_applied_frame_authored: bool,
}

pub enum ExternalOwnerFrameOutcome {
    Application(SupervisorApplicationOutcome),
    Capture(crate::supervisor::SupervisorCaptureOutcome),
    Acknowledgement,
}

pub struct ExternalOwnerFrameDispatch {
    pub outcome: ExternalOwnerFrameOutcome,
    /// Exact durable signed supervisor frame. Reconnect resends these bytes;
    /// it never rebuilds the acknowledgement from request state.
    pub acknowledgement_frame: String,
}

impl LiveInheritedExternalCandidateSupervisor {
    pub fn binding(&self) -> &ExecutionChannelBinding {
        self.supervisor.journal().binding()
    }

    pub fn process_identity(&self) -> &lillux::ExactProcessIdentity {
        &self.process_identity
    }

    pub fn occurrence_digest(&self) -> &str {
        &self.occurrence_digest
    }

    pub fn ready_frame(&self) -> &str {
        &self.ready_frame
    }

    pub fn has_durable_capture(&self) -> Result<bool> {
        self.supervisor.journal().has_retained_export()
    }

    pub fn dispatch_release(&mut self, wire: &[u8]) -> Result<SupervisorApplicationOutcome> {
        self.supervisor.dispatch_release(wire)
    }

    pub fn dispatch_protocol_chunk(&mut self, wire: &[u8]) -> Result<SupervisorApplicationOutcome> {
        self.supervisor.dispatch_protocol_chunk(wire)
    }

    pub fn dispatch_cancel(&mut self, wire: &[u8]) -> Result<SupervisorApplicationOutcome> {
        self.supervisor.dispatch_cancel(wire)
    }

    pub fn dispatch_quiesce(
        &mut self,
        wire: &[u8],
    ) -> Result<crate::supervisor::SupervisorCaptureOutcome> {
        self.supervisor.dispatch_quiesce(
            wire,
            &self.authority,
            &self.supervisor_signing_key,
            &self.occurrence_digest,
            &self.bootstrap_digest,
        )
    }

    /// Apply one controller frame through the live guest journal and native
    /// launcher, then durably author the exact resulting application state.
    /// A partial protocol write returns a signed `claimed` acknowledgement;
    /// the caller must continue this same frame locally until `applied`.
    pub fn dispatch_owner_frame(&mut self, wire: &[u8]) -> Result<ExternalOwnerFrameDispatch> {
        let verified = SignedExecutionFrame::decode_and_verify(
            wire,
            self.supervisor.journal().binding(),
            lillux::time::timestamp_millis(),
        )?;
        ensure!(
            verified.frame().direction
                == ryeos_state::external_execution::ChannelDirection::OwnerToSupervisor,
            "external supervisor accepts only controller-authored frames"
        );
        let sequence = verified.frame().sequence;
        let digest = verified.digest().to_owned();
        let outcome = match &verified.frame().payload {
            ExecutionChannelPayload::Release => {
                ExternalOwnerFrameOutcome::Application(self.dispatch_release(wire)?)
            }
            ExecutionChannelPayload::ProtocolBytes { .. } => {
                ExternalOwnerFrameOutcome::Application(self.dispatch_protocol_chunk(wire)?)
            }
            ExecutionChannelPayload::Cancel => {
                ExternalOwnerFrameOutcome::Application(self.dispatch_cancel(wire)?)
            }
            ExecutionChannelPayload::Quiesce { .. } => {
                ExternalOwnerFrameOutcome::Capture(self.dispatch_quiesce(wire)?)
            }
            ExecutionChannelPayload::Acknowledge { .. } => {
                self.supervisor.record_owner_acknowledgement(wire)?;
                ExternalOwnerFrameOutcome::Acknowledgement
            }
            _ => anyhow::bail!("controller sent a supervisor-only external payload"),
        };
        let acknowledgement = self
            .supervisor
            .ensure_supervisor_acknowledgement(&self.supervisor_signing_key, sequence, &digest)?
            .context("external owner frame has no supervisor acknowledgement")?;
        Ok(ExternalOwnerFrameDispatch {
            outcome,
            acknowledgement_frame: acknowledgement.canonical().to_owned(),
        })
    }

    pub fn poll_execution_output(&mut self) -> Result<Option<String>> {
        // Transport may poll before the controller has released the target.
        // Do not ask Lillux for a pre-exec receipt until release is durably
        // applied, and do not forward any output before the exact local
        // occurrence row is retained. This still is not qualification.
        if !self.supervisor.journal().has_applied_release()? {
            return Ok(None);
        }
        if !self.applied_runtime_recorded && !self.poll_applied_runtime()? {
            return Ok(None);
        }
        if !self.runtime_applied_frame_authored {
            let newly_authored = self
                .supervisor
                .ensure_runtime_applied_frame(&self.supervisor_signing_key)?;
            self.runtime_applied_frame_authored = true;
            // A previously committed frame is already in the journal's
            // pending transport queue. Do not enqueue it a second time after
            // an ambiguous authoring response.
            return Ok(newly_authored.map(|frame| frame.canonical().to_owned()));
        }
        Ok(self
            .supervisor
            .poll_execution_output(&self.supervisor_signing_key)?
            .map(|frame| frame.canonical().to_owned()))
    }

    /// Retain the exact native receipt in the occurrence journal before any
    /// later signed projection can expose it to the controller.
    pub fn poll_applied_runtime(&mut self) -> Result<bool> {
        if self.applied_runtime_recorded {
            return Ok(true);
        }
        if !self.supervisor.journal().has_applied_release()? {
            return Ok(false);
        }
        let recorded = self
            .supervisor
            .poll_applied_runtime(&self.occurrence_digest)?;
        self.applied_runtime_recorded = recorded;
        Ok(recorded)
    }

    /// Recover the next exact journal-authored frame rather than relying on a
    /// process-local queue. This is what makes a multi-frame candidate export
    /// reconnectable without rebuilding or rereading candidate content.
    pub fn next_pending_transport_frame_after(
        &self,
        after_sequence: u64,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .supervisor
            .journal()
            .pending_supervisor_transport_frames_after(
                after_sequence,
                1,
                ryeos_state::external_execution::MAX_FRAME_BYTES,
            )?
            .into_iter()
            .next()
            .map(|frame| frame.wire().to_vec()))
    }

    /// Authoritative local cleanup. Returning `Ok` proves the exact launcher
    /// wrapper and all descendants owned by Lillux were reaped.
    pub fn abort_and_reap(self) -> Result<()> {
        match self
            .launcher_process
            .wait_for_natural_exit(lillux::time::Duration::from_millis(200))
        {
            Ok(settled) if settled.success => Ok(()),
            Ok(settled) => anyhow::bail!(
                "dedicated launcher exited before cleanup; exit={}; timed_out={}; stderr={:?}",
                settled.exit_code,
                settled.timed_out,
                bounded_diagnostic_tail(&settled.stderr)
            ),
            Err(running) => {
                let stderr = running.stderr_diagnostic_tail();
                let cleanup = running.abort_and_reap_checked();
                if stderr.is_some() {
                    anyhow::bail!(
                        "dedicated launcher reported stderr before cleanup; stderr={stderr:?}; cleanup={cleanup:?}"
                    );
                }
                cleanup.map_err(anyhow::Error::msg)
            }
        }
    }
}

pub fn launch_external_candidate_supervisor(
    prepared_journal: PreparedGuestJournal,
    mut request: lillux::SubprocessRequest,
    authority: ryeos_state::PinnedStateAuthority,
    bootstrap_digest: String,
    supervisor_signing_key: lillux::crypto::SigningKey,
    launcher_artifact_digest: &str,
    channel_env_name: &str,
    channel_target_fd: u32,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<LiveInheritedExternalCandidateSupervisor> {
    ensure!(
        prepared_journal.bootstrap_digest() == bootstrap_digest,
        "launcher request changed guest bootstrap identity"
    );
    let binding = prepared_journal.binding().clone();
    let challenge = lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>());
    let challenge_digest = lillux::sha256_hex(challenge.as_bytes());
    let (parent_channel, child_channel) =
        lillux::inherited_duplex_channel_pair().map_err(anyhow::Error::msg)?;
    child_channel
        .bind_to_subprocess_request(&mut request, channel_env_name, channel_target_fd)
        .map_err(anyhow::Error::msg)?;
    // The request now owns the only parent-side launch retention for the child
    // endpoint. Keeping this construction handle alive would mask EOF when a
    // launcher exits before authentication and strand the supervisor until
    // its full execution deadline.
    drop(child_channel);
    let pending = lillux::spawn_awaiting_attachment(request).map_err(|failure| {
        anyhow::anyhow!("dedicated launcher spawn failed: {}", failure.stderr)
    })?;
    let process_identity = pending
        .exact_process_identity()
        .map_err(anyhow::Error::msg)?;
    let evidence = LauncherOccurrenceEvidence::from_held_launcher(
        process_identity.clone(),
        launcher_artifact_digest,
        &challenge_digest,
    )?;
    let occurrence_digest = evidence.digest()?;
    let bound_journal = match prepared_journal.bind_launcher(evidence) {
        Ok(bound) => bound,
        Err(error) => {
            let cleanup = pending.abort_and_reap();
            return Err(error.context(format!(
                "persist exact launcher occurrence; held cleanup={cleanup:?}"
            )));
        }
    };
    let running = pending
        .release_after_attachment()
        .map_err(|error| anyhow::anyhow!("release attached dedicated launcher: {error}"))?;
    let (client, ready) = match InheritedExternalCandidateLauncherClient::authenticate(
        parent_channel,
        binding,
        &challenge,
        &occurrence_digest,
        deadline,
    ) {
        Ok(result) => result,
        Err(error) => {
            // Authentication failure commonly means the launcher refused
            // before Ready. Give that sole process owner a short natural-exit
            // window so its refusal reaches the bounded capture. Interrupting
            // immediately would race teardown and replace the actual cause
            // with our own observation-cancel diagnostic.
            return match running.wait_for_natural_exit(lillux::time::Duration::from_secs(1)) {
                Ok(settled) => Err(error.context(format!(
                    "authenticate dedicated launcher; settled_exit={}; timed_out={}; stderr={:?}",
                    settled.exit_code,
                    settled.timed_out,
                    bounded_diagnostic_tail(&settled.stderr)
                ))),
                Err(running) => {
                    let stderr = running.stderr_diagnostic_tail();
                    let cleanup = running.abort_and_reap_checked();
                    Err(error.context(format!(
                        "authenticate dedicated launcher; natural settlement absent; stderr={stderr:?}; cleanup={cleanup:?}"
                    )))
                }
            };
        }
    };
    let live_journal = match bound_journal.mark_launcher_ready(ready) {
        Ok(live) => live,
        Err(error) => {
            let cleanup = running.abort_and_reap_checked();
            return Err(error.context(format!(
                "persist authenticated launcher readiness; cleanup={cleanup:?}"
            )));
        }
    };
    let supervisor = SerializedExternalCandidateSupervisor::new(live_journal, client);
    let ready_frame = match supervisor.publish_ready(&supervisor_signing_key) {
        Ok(ready) => ready.canonical().to_owned(),
        Err(error) => {
            let cleanup = running.abort_and_reap_checked();
            return Err(error.context(format!(
                "publish dedicated launcher readiness; cleanup={cleanup:?}"
            )));
        }
    };
    Ok(LiveInheritedExternalCandidateSupervisor {
        supervisor,
        launcher_process: running,
        process_identity,
        occurrence_digest,
        authority,
        supervisor_signing_key,
        bootstrap_digest,
        ready_frame,
        applied_runtime_recorded: false,
        runtime_applied_frame_authored: false,
    })
}

fn bounded_diagnostic_tail(value: &str) -> String {
    const MAX_BYTES: usize = 2 * 1024;

    if value.len() <= MAX_BYTES {
        return value.to_owned();
    }
    let bytes = value.as_bytes();
    let tail = String::from_utf8_lossy(&bytes[bytes.len() - MAX_BYTES..]);
    format!("… (bounded stderr tail; earlier bytes omitted)\n{tail}")
}

pub fn launch_prepared_external_candidate_supervisor(
    prepared_journal: PreparedGuestJournal,
    prepared: PreparedExternalCandidateLauncherRequest,
    supervisor_signing_key: lillux::crypto::SigningKey,
    launcher_artifact_digest: &str,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<LiveInheritedExternalCandidateSupervisor> {
    launch_external_candidate_supervisor(
        prepared_journal,
        prepared.request,
        prepared.authority,
        prepared.bootstrap_digest,
        supervisor_signing_key,
        launcher_artifact_digest,
        "RYEOS_EXTERNAL_CANDIDATE_CONTROL_FD",
        LAUNCHER_CONTROL_FD,
        deadline,
    )
}

impl InheritedExternalCandidateLauncherClient {
    /// Exact local point read from the authenticated, already-bound launcher.
    /// This does not journal the observation or authorize a qualification
    /// claim; the protected supervisor must retain it separately.
    pub fn poll_applied_launch(
        &mut self,
    ) -> Result<Option<lillux::LinuxSandboxAppliedLaunchReceipt>> {
        write_message(
            &mut self.channel,
            self.deadline,
            &LauncherMessage::PollAppliedLaunch {
                schema: PROTOCOL_SCHEMA,
            },
        )?;
        match read_message(&mut self.channel, self.deadline)
            .context("read dedicated launcher applied-launch response")?
        {
            LauncherMessage::AppliedLaunch { schema, receipt } if schema == PROTOCOL_SCHEMA => {
                Ok(receipt)
            }
            _ => anyhow::bail!("dedicated launcher returned a mismatched applied-launch response"),
        }
    }

    pub fn authenticate(
        mut channel: lillux::InheritedDuplexChannel,
        binding: ExecutionChannelBinding,
        challenge: &str,
        occurrence_digest: &str,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<(Self, AuthenticatedLauncherReady)> {
        ensure!(
            challenge.len() == 64
                && challenge
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "launcher challenge is not a canonical 256-bit value"
        );
        binding.validate()?;
        ensure!(
            occurrence_digest.len() == 64
                && occurrence_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "launcher occurrence digest is not canonical"
        );
        let bootstrap = LauncherMessage::Bootstrap {
            schema: PROTOCOL_SCHEMA,
            binding: binding.clone(),
            challenge: challenge.to_owned(),
            occurrence_digest: occurrence_digest.to_owned(),
        };
        write_message(&mut channel, deadline, &bootstrap)?;
        let ready = read_message(&mut channel, deadline)
            .context("read dedicated launcher readiness response")?;
        let (binding_digest, challenge_digest) = match &ready {
            LauncherMessage::Ready {
                schema,
                binding_digest,
                challenge_digest,
            } if *schema == PROTOCOL_SCHEMA => (binding_digest, challenge_digest),
            _ => anyhow::bail!("dedicated launcher did not return exact readiness"),
        };
        ensure!(
            binding_digest == &binding.digest()?
                && challenge_digest == &lillux::sha256_hex(challenge.as_bytes()),
            "dedicated launcher readiness changed binding or challenge"
        );
        let transcript = lillux::canonical_json(&serde_json::json!({
            "bootstrap": bootstrap,
            "ready": ready,
        }))?;
        let authenticated = AuthenticatedLauncherReady::from_handshake_transcript(
            &lillux::sha256_hex(transcript.as_bytes()),
        )?;
        Ok((
            Self {
                channel,
                binding,
                deadline,
            },
            authenticated,
        ))
    }

    fn apply(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<Option<usize>> {
        ensure!(
            frame.frame().binding_digest == self.binding.digest()?,
            "launcher client action changed its bound occurrence"
        );
        let request = LauncherMessage::Apply {
            schema: PROTOCOL_SCHEMA,
            frame_json: frame.canonical().to_owned(),
        };
        write_message(&mut self.channel, self.deadline, &request)?;
        match read_message(&mut self.channel, self.deadline)
            .context("read dedicated launcher application response")?
        {
            LauncherMessage::Applied {
                schema,
                frame_digest,
                written_bytes,
            } if schema == PROTOCOL_SCHEMA && frame_digest == frame.digest() => Ok(written_bytes),
            LauncherMessage::Failed {
                schema,
                frame_digest,
                detail,
            } if schema == PROTOCOL_SCHEMA && frame_digest == frame.digest() => {
                anyhow::bail!("dedicated launcher action failed: {detail}")
            }
            _ => anyhow::bail!("dedicated launcher returned a mismatched action response"),
        }
    }
}

impl ExternalCandidateLauncherClient for InheritedExternalCandidateLauncherClient {
    fn poll_applied_launch(&mut self) -> Result<Option<lillux::LinuxSandboxAppliedLaunchReceipt>> {
        InheritedExternalCandidateLauncherClient::poll_applied_launch(self)
    }

    fn release(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<()> {
        ensure!(
            self.apply(frame)?.is_none(),
            "launcher release returned protocol progress"
        );
        Ok(())
    }

    fn apply_protocol_chunk(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<usize> {
        self.apply(frame)?
            .context("launcher protocol action omitted written byte count")
    }

    fn cancel(&mut self, frame: &AuthenticatedExecutionFrame) -> Result<()> {
        ensure!(
            self.apply(frame)?.is_none(),
            "launcher cancellation returned protocol progress"
        );
        Ok(())
    }

    fn capture(
        &mut self,
        frame: &AuthenticatedExecutionFrame,
    ) -> Result<DurableNativeCandidateCapture> {
        ensure!(
            frame.frame().binding_digest == self.binding.digest()?,
            "launcher capture changed its bound occurrence"
        );
        write_message(
            &mut self.channel,
            self.deadline,
            &LauncherMessage::Apply {
                schema: PROTOCOL_SCHEMA,
                frame_json: frame.canonical().to_owned(),
            },
        )?;
        match read_message(&mut self.channel, self.deadline)
            .context("read dedicated launcher capture response")?
        {
            LauncherMessage::Captured { schema, capture }
                if schema == PROTOCOL_SCHEMA && capture.quiesce_frame_digest == frame.digest() =>
            {
                Ok(capture)
            }
            LauncherMessage::Failed {
                schema,
                frame_digest,
                detail,
            } if schema == PROTOCOL_SCHEMA && frame_digest == frame.digest() => {
                anyhow::bail!("dedicated launcher capture failed: {detail}")
            }
            _ => anyhow::bail!("dedicated launcher returned a mismatched capture response"),
        }
    }

    fn acknowledge_finish(&mut self, frame_digest: &str) -> Result<()> {
        let request = LauncherMessage::AcknowledgeFinish {
            schema: PROTOCOL_SCHEMA,
            frame_digest: frame_digest.to_owned(),
        };
        write_message(&mut self.channel, self.deadline, &request)?;
        ensure!(
            matches!(
                read_message(&mut self.channel, self.deadline)
                    .context("read dedicated launcher finish acknowledgement")?,
                LauncherMessage::FinishAcknowledged {
                    schema: PROTOCOL_SCHEMA,
                    frame_digest: response,
                } if response == frame_digest
            ),
            "dedicated launcher changed finish acknowledgement"
        );
        Ok(())
    }

    fn poll_execution_output(&mut self) -> Result<CandidateExecutionOutput> {
        write_message(
            &mut self.channel,
            self.deadline,
            &LauncherMessage::PollOutput {
                schema: PROTOCOL_SCHEMA,
                maximum_bytes: ryeos_state::external_execution::MAX_CHUNK_BYTES,
            },
        )?;
        decode_execution_output(
            read_message(&mut self.channel, self.deadline)
                .context("read dedicated launcher output response")?,
            self.binding.execution_mode,
        )
    }
}

fn decode_execution_output(
    message: LauncherMessage,
    mode: ExternalExecutionMode,
) -> Result<CandidateExecutionOutput> {
    match message {
        LauncherMessage::Output {
            schema,
            bytes_base64,
            closed,
        } if schema == PROTOCOL_SCHEMA => {
            let bytes = STANDARD
                .decode(&bytes_base64)
                .map_err(|_| anyhow::anyhow!("dedicated launcher output is invalid base64"))?;
            ensure!(
                bytes.len() <= ryeos_state::external_execution::MAX_CHUNK_BYTES
                    && STANDARD.encode(&bytes) == bytes_base64,
                "dedicated launcher output changed its bounded canonical bytes"
            );
            match (bytes.is_empty(), closed) {
                (false, false) if mode == (ExternalExecutionMode::StructuredSession {}) => {
                    Ok(CandidateExecutionOutput::Bytes(bytes))
                }
                (true, false) => Ok(CandidateExecutionOutput::Idle),
                (true, true) if mode == (ExternalExecutionMode::StructuredSession {}) => {
                    Ok(CandidateExecutionOutput::Closed)
                }
                _ => anyhow::bail!("launcher output changed execution mode or EOF framing"),
            }
        }
        LauncherMessage::CommandOutput {
            schema: PROTOCOL_SCHEMA,
            stream,
            offset,
            bytes_base64,
        } => {
            let limit = mode.output_limit(stream)?;
            let bytes = STANDARD.decode(&bytes_base64)?;
            ensure!(
                !bytes.is_empty()
                    && bytes.len() <= ryeos_state::external_execution::MAX_CHUNK_BYTES
                    && STANDARD.encode(&bytes) == bytes_base64
                    && offset
                        .checked_add(u64::try_from(bytes.len())?)
                        .is_some_and(|end| end <= limit),
                "launcher command output exceeds exact canonical stream bound"
            );
            Ok(CandidateExecutionOutput::CommandOutput {
                stream,
                offset,
                bytes,
            })
        }
        LauncherMessage::CommandTerminated {
            schema: PROTOCOL_SCHEMA,
            observation,
        } => {
            observation.validate(mode)?;
            Ok(CandidateExecutionOutput::CommandTerminated { observation })
        }
        _ => anyhow::bail!("dedicated launcher returned a mismatched output poll response"),
    }
}

struct BoundedCandidateExecutionOutput {
    output: std::fs::File,
    maximum_bytes: u64,
    observed_bytes: u64,
    closed: bool,
}

struct BoundedCandidateStderr {
    input: std::fs::File,
    maximum_bytes: u64,
    observed_bytes: u64,
    closed: bool,
}

/// Stream commitments belong to the command protocol; process observation and
/// termination remain owned by Lillux through NativeExternalCandidate. At most
/// the admitted aggregate output bound is retained here (64 MiB).
struct DirectCommandObservation {
    stdout: BoundedCandidateExecutionOutput,
    stderr: BoundedCandidateExecutionOutput,
    stdout_bytes: Vec<u8>,
    stderr_bytes: Vec<u8>,
    stderr_first: bool,
    target_exit: Option<ExternalTargetExit>,
    reason: Option<ExternalCommandTerminationReason>,
    stopped: bool,
    terminal_sent: bool,
}

impl DirectCommandObservation {
    fn service(
        &mut self,
        candidate: &mut NativeExternalCandidate,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        if self.stopped || self.target_exit.is_some() {
            return Ok(());
        }
        if candidate.execution_expired() {
            self.reason = Some(ExternalCommandTerminationReason::Deadline);
            self.stopped = true;
            return candidate.stop(deadline);
        }
        candidate.pump_direct_input()?;
        self.target_exit = candidate.try_observe_target_exit()?;
        if self.target_exit.is_some() {
            self.reason = Some(if candidate.require_direct_input_complete().is_ok() {
                ExternalCommandTerminationReason::TargetExited
            } else {
                ExternalCommandTerminationReason::Fault
            });
        }
        Ok(())
    }

    fn poll(&mut self, maximum_bytes: usize) -> Result<CandidateExecutionOutput> {
        if self.terminal_sent {
            return Ok(CandidateExecutionOutput::Idle);
        }
        // Neither a busy stdout nor a busy stderr can starve its peer.
        let order = if self.stderr_first {
            [
                ExternalCommandOutputStream::Stderr,
                ExternalCommandOutputStream::Stdout,
            ]
        } else {
            [
                ExternalCommandOutputStream::Stdout,
                ExternalCommandOutputStream::Stderr,
            ]
        };
        for stream in order {
            let (output, retained) = match stream {
                ExternalCommandOutputStream::Stdout => (&mut self.stdout, &mut self.stdout_bytes),
                ExternalCommandOutputStream::Stderr => (&mut self.stderr, &mut self.stderr_bytes),
            };
            if let CandidateExecutionOutput::Bytes(bytes) = output.poll(maximum_bytes)? {
                let offset = u64::try_from(retained.len())?;
                retained.extend_from_slice(&bytes);
                self.stderr_first = stream == ExternalCommandOutputStream::Stdout;
                return Ok(CandidateExecutionOutput::CommandOutput {
                    stream,
                    offset,
                    bytes,
                });
            }
        }
        if self.stdout.closed && self.stderr.closed {
            if let Some(target_exit) = self.target_exit {
                let commitment = |bytes: &[u8]| ExternalCommandOutputCommitment {
                    bytes: bytes.len() as u64,
                    sha256: lillux::sha256_hex(bytes),
                    truncated: false,
                };
                let observation = ExternalCommandTermination {
                    target_exit,
                    reason: self
                        .reason
                        .context("direct target exit has no observation reason")?,
                    stdout: commitment(&self.stdout_bytes),
                    stderr: commitment(&self.stderr_bytes),
                };
                self.terminal_sent = true;
                return Ok(CandidateExecutionOutput::CommandTerminated { observation });
            }
        }
        // EOF, including EOF following forced cleanup, supplies no target exit.
        Ok(CandidateExecutionOutput::Idle)
    }
}

enum NativeCandidateOutputOwner {
    Structured {
        stdout: BoundedCandidateExecutionOutput,
        stderr: BoundedCandidateStderr,
    },
    Direct(DirectCommandObservation),
}

impl NativeCandidateOutputOwner {
    fn service(
        &mut self,
        candidate: &mut NativeExternalCandidate,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        match self {
            Self::Structured { stderr, .. } => {
                if candidate.execution_expired() && !candidate.has_native_settlement() {
                    candidate.stop(deadline)?;
                    anyhow::bail!("structured candidate execution deadline elapsed");
                }
                stderr.drain_available()
            }
            Self::Direct(output) => output.service(candidate, deadline),
        }
    }

    fn cancelled(&mut self) {
        if let Self::Direct(output) = self {
            output.reason = Some(ExternalCommandTerminationReason::Cancelled);
            output.stopped = true;
        }
    }

    fn poll(&mut self, maximum_bytes: usize) -> Result<CandidateExecutionOutput> {
        match self {
            Self::Structured { stdout, .. } => stdout.poll(maximum_bytes),
            Self::Direct(output) => output.poll(maximum_bytes),
        }
    }
}

impl BoundedCandidateStderr {
    fn new(input: std::fs::File, maximum_bytes: u64) -> Result<Self> {
        ensure!(
            (1..=64 * 1024 * 1024).contains(&maximum_bytes),
            "candidate stderr bound is invalid"
        );
        Ok(Self {
            input,
            maximum_bytes,
            observed_bytes: 0,
            closed: false,
        })
    }

    fn drain_available(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let mut buffer = [0_u8; 64 * 1024];
        // Yield after one bounded read so a continuously writable stderr cannot
        // defer cancellation or the next absolute execution-deadline check.
        match self.input.read(&mut buffer) {
            Ok(0) => {
                self.closed = true;
                Ok(())
            }
            Ok(count) => {
                self.observed_bytes = self
                    .observed_bytes
                    .checked_add(u64::try_from(count)?)
                    .context("candidate stderr byte count overflow")?;
                ensure!(
                    self.observed_bytes <= self.maximum_bytes,
                    "external candidate exceeded stderr bound"
                );
                Ok(())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error).context("read candidate stderr"),
        }
    }
}

#[derive(Default)]
struct NonblockingLauncherMessageReader {
    bytes: Vec<u8>,
    frame_bytes: Option<usize>,
}

impl NonblockingLauncherMessageReader {
    fn read_available(
        &mut self,
        channel: &mut lillux::InheritedDuplexChannel,
    ) -> Result<Option<LauncherMessage>> {
        loop {
            let target = self.frame_bytes.unwrap_or(4);
            if self.bytes.len() == target {
                if self.frame_bytes.is_none() {
                    let length = usize::try_from(u32::from_be_bytes(
                        self.bytes
                            .as_slice()
                            .try_into()
                            .expect("four-byte launcher frame prefix"),
                    ))?;
                    ensure!(
                        length > 0 && length <= MAX_MESSAGE_BYTES,
                        "invalid launcher message length"
                    );
                    self.frame_bytes = Some(
                        4_usize
                            .checked_add(length)
                            .context("launcher message length overflow")?,
                    );
                    continue;
                }
                let body = &self.bytes[4..];
                let message: LauncherMessage = serde_json::from_slice(body)?;
                ensure!(
                    lillux::canonical_json(&serde_json::to_value(&message)?)?.as_bytes() == body,
                    "launcher message is not canonical"
                );
                self.bytes.clear();
                self.frame_bytes = None;
                return Ok(Some(message));
            }

            let remaining = target
                .checked_sub(self.bytes.len())
                .context("launcher message reader crossed its exact frame bound")?;
            let mut chunk = [0_u8; 16 * 1024];
            let read_limit = remaining.min(chunk.len());
            match channel.read(&mut chunk[..read_limit]) {
                Ok(0) => anyhow::bail!("dedicated launcher control channel closed"),
                Ok(count) => self.bytes.extend_from_slice(&chunk[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error).context("read dedicated launcher control channel"),
            }
        }
    }
}

fn next_launcher_message(
    channel: &mut lillux::InheritedDuplexChannel,
    reader: &mut NonblockingLauncherMessageReader,
    output: &mut NativeCandidateOutputOwner,
    candidate: &mut NativeExternalCandidate,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<LauncherMessage> {
    loop {
        ensure!(!deadline.has_elapsed(), "launcher channel deadline elapsed");
        output.service(candidate, deadline)?;
        if let Some(message) = reader.read_available(channel)? {
            return Ok(message);
        }
        // A quiet or partially framed control channel must not defer execution
        // expiry or one-shot input progress until its longer drain deadline.
        let tick = deadline.min(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_millis(10),
        ));
        let ready = match output {
            NativeCandidateOutputOwner::Structured { stderr, .. } if !stderr.closed => channel
                .with_deadline(tick)
                .wait_readable_with(&stderr.input)
                .map(|_| ()),
            _ => channel.with_deadline(tick).wait_readable(),
        };
        match ready {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error).context("wait for bounded launcher control"),
        }
    }
}

impl BoundedCandidateExecutionOutput {
    fn new(output: std::fs::File, maximum_bytes: u64) -> Result<Self> {
        ensure!(
            (1..=64 * 1024 * 1024).contains(&maximum_bytes),
            "candidate protocol output bound is invalid"
        );
        Ok(Self {
            output,
            maximum_bytes,
            observed_bytes: 0,
            closed: false,
        })
    }

    fn poll(&mut self, maximum_bytes: usize) -> Result<CandidateExecutionOutput> {
        ensure!(
            (1..=ryeos_state::external_execution::MAX_CHUNK_BYTES).contains(&maximum_bytes),
            "candidate protocol output poll bound is invalid"
        );
        if self.closed {
            return Ok(CandidateExecutionOutput::Closed);
        }
        let remaining = self
            .maximum_bytes
            .checked_sub(self.observed_bytes)
            .context("candidate protocol output exceeded its aggregate bound")?;
        let probe_only = remaining == 0;
        let limit = if probe_only {
            1
        } else {
            maximum_bytes.min(usize::try_from(remaining).unwrap_or(usize::MAX))
        };
        let mut bytes = vec![0_u8; limit];
        let count = match self.output.read(&mut bytes) {
            Ok(0) => {
                self.closed = true;
                return Ok(CandidateExecutionOutput::Closed);
            }
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(CandidateExecutionOutput::Idle);
            }
            Err(error) => return Err(error).context("read candidate protocol output"),
        };
        ensure!(
            !probe_only,
            "candidate protocol output exceeded its aggregate bound"
        );
        bytes.truncate(count);
        self.observed_bytes = self
            .observed_bytes
            .checked_add(u64::try_from(count)?)
            .context("candidate protocol output byte count overflow")?;
        Ok(CandidateExecutionOutput::Bytes(bytes))
    }
}

/// Run the bounded local protocol after a dedicated launcher has prepared its
/// native candidate. This loop owns no signing key or remote transport.
pub fn serve_native_candidate_launcher(
    mut channel: lillux::InheritedDuplexChannel,
    mut candidate: NativeExternalCandidate,
    protocol_output: std::fs::File,
    maximum_output_bytes: u64,
    candidate_stderr: std::fs::File,
    maximum_stderr_bytes: u64,
    authority: ryeos_state::PinnedStateAuthority,
    bootstrap_digest: String,
    expected_binding: ExecutionChannelBinding,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    let result = (|| -> Result<()> {
        let stdout = BoundedCandidateExecutionOutput::new(protocol_output, maximum_output_bytes)?;
        let mut output = match expected_binding.execution_mode {
            ExternalExecutionMode::StructuredSession {} => NativeCandidateOutputOwner::Structured {
                stdout,
                stderr: BoundedCandidateStderr::new(candidate_stderr, maximum_stderr_bytes)?,
            },
            ExternalExecutionMode::DirectCommand {
                stdout_max_bytes,
                stderr_max_bytes,
            } => {
                ensure!(
                    maximum_output_bytes == stdout_max_bytes
                        && maximum_stderr_bytes == stderr_max_bytes,
                    "launcher command stream limits changed admitted execution mode"
                );
                NativeCandidateOutputOwner::Direct(DirectCommandObservation {
                    stdout,
                    stderr: BoundedCandidateExecutionOutput::new(
                        candidate_stderr,
                        maximum_stderr_bytes,
                    )?,
                    stdout_bytes: Vec::new(),
                    stderr_bytes: Vec::new(),
                    stderr_first: false,
                    target_exit: None,
                    reason: None,
                    stopped: false,
                    terminal_sent: false,
                })
            }
        };
        let bootstrap = read_message(&mut channel, candidate.control_io_deadline(deadline))?;
        let (challenge, occurrence_digest) = match &bootstrap {
            LauncherMessage::Bootstrap {
                schema,
                binding,
                challenge,
                occurrence_digest,
            } if *schema == PROTOCOL_SCHEMA
                && binding == &expected_binding
                && challenge.len() == 64
                && challenge
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                && occurrence_digest.len() == 64
                && occurrence_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) =>
            {
                (challenge, occurrence_digest)
            }
            _ => anyhow::bail!("dedicated launcher bootstrap changed exact authority"),
        };
        write_message(
            &mut channel,
            candidate.control_io_deadline(deadline),
            &LauncherMessage::Ready {
                schema: PROTOCOL_SCHEMA,
                binding_digest: expected_binding.digest()?,
                challenge_digest: lillux::sha256_hex(challenge.as_bytes()),
            },
        )?;
        channel.set_nonblocking(true)?;
        let mut message_reader = NonblockingLauncherMessageReader::default();
        loop {
            let message = next_launcher_message(
                &mut channel,
                &mut message_reader,
                &mut output,
                &mut candidate,
                deadline,
            )?;
            match message {
                LauncherMessage::Apply { schema, frame_json } if schema == PROTOCOL_SCHEMA => {
                    let decoded = SignedExecutionFrame::decode_and_verify(
                        frame_json.as_bytes(),
                        &expected_binding,
                        lillux::time::timestamp_millis(),
                    )?;
                    let digest = decoded.digest().to_owned();
                    if matches!(
                        decoded.frame().payload,
                        ExecutionChannelPayload::Quiesce { .. }
                    ) {
                        let guard = authority.acquire_shared_guard()?;
                        let publication_key =
                            ryeos_state::DurableCasPublicationKey::external_candidate_occurrence(
                                &expected_binding.digest()?,
                                occurrence_digest,
                            )?;
                        let mut stage = authority
                            .require_recovery()?
                            .begin_durable_cas_upload_admitted(
                                &guard,
                                &bootstrap_digest,
                                "external-candidate-native-capture",
                                &publication_key,
                                None,
                            )?;
                        let export = candidate.capture(
                            &decoded,
                            &authority,
                            &guard,
                            &mut stage,
                            occurrence_digest,
                            deadline,
                        );
                        let response = match export {
                            Ok(export) => LauncherMessage::Captured {
                                schema: PROTOCOL_SCHEMA,
                                capture: DurableNativeCandidateCapture {
                                    quiesce_frame_digest: digest,
                                    occurrence_digest: export.occurrence_digest().to_owned(),
                                    durable_stage_id: export.durable_stage_id().to_owned(),
                                    snapshot_hash: export.snapshot_hash().to_owned(),
                                    output_capture_hash: export
                                        .output_capture_hash()
                                        .map(str::to_owned),
                                    completion_request_digest: export
                                        .completion_request_digest()
                                        .to_owned(),
                                    writer_exclusion_evidence_hash: export
                                        .writer_exclusion_blob_hash()
                                        .to_owned(),
                                },
                            },
                            Err(error) => LauncherMessage::Failed {
                                schema: PROTOCOL_SCHEMA,
                                frame_digest: digest,
                                detail: format!("{error:#}"),
                            },
                        };
                        drop(stage);
                        drop(guard);
                        write_message(
                            &mut channel,
                            candidate.control_io_deadline(deadline),
                            &response,
                        )?;
                        continue;
                    }
                    let outcome = match &decoded.frame().payload {
                        ExecutionChannelPayload::Release => {
                            candidate.release(&decoded).map(|_| None)
                        }
                        ExecutionChannelPayload::ProtocolBytes { .. } => {
                            candidate.apply_protocol_chunk(&decoded).map(Some)
                        }
                        ExecutionChannelPayload::Cancel => {
                            output.cancelled();
                            candidate.cancel(&decoded, deadline).map(|_| None)
                        }
                        _ => anyhow::bail!("dedicated launcher received a non-native action"),
                    };
                    let response = match outcome {
                        Ok(written_bytes) => LauncherMessage::Applied {
                            schema: PROTOCOL_SCHEMA,
                            frame_digest: digest,
                            written_bytes,
                        },
                        Err(error) => LauncherMessage::Failed {
                            schema: PROTOCOL_SCHEMA,
                            frame_digest: digest,
                            detail: format!("{error:#}"),
                        },
                    };
                    write_message(
                        &mut channel,
                        candidate.control_io_deadline(deadline),
                        &response,
                    )?;
                }
                LauncherMessage::AcknowledgeFinish {
                    schema,
                    frame_digest,
                } if schema == PROTOCOL_SCHEMA => {
                    candidate.acknowledge_application_finish(&frame_digest)?;
                    write_message(
                        &mut channel,
                        candidate.control_io_deadline(deadline),
                        &LauncherMessage::FinishAcknowledged {
                            schema: PROTOCOL_SCHEMA,
                            frame_digest,
                        },
                    )?;
                }
                LauncherMessage::PollAppliedLaunch { schema } if schema == PROTOCOL_SCHEMA => {
                    let receipt = candidate.try_observe_applied_launch()?;
                    write_message(
                        &mut channel,
                        candidate.control_io_deadline(deadline),
                        &LauncherMessage::AppliedLaunch {
                            schema: PROTOCOL_SCHEMA,
                            receipt,
                        },
                    )?;
                }
                LauncherMessage::PollOutput {
                    schema,
                    maximum_bytes,
                } if schema == PROTOCOL_SCHEMA
                    && (1..=ryeos_state::external_execution::MAX_CHUNK_BYTES)
                        .contains(&maximum_bytes) =>
                {
                    let response = match output.poll(maximum_bytes)? {
                        CandidateExecutionOutput::Bytes(bytes) => LauncherMessage::Output {
                            schema: PROTOCOL_SCHEMA,
                            bytes_base64: STANDARD.encode(bytes),
                            closed: false,
                        },
                        CandidateExecutionOutput::Idle => LauncherMessage::Output {
                            schema: PROTOCOL_SCHEMA,
                            bytes_base64: String::new(),
                            closed: false,
                        },
                        CandidateExecutionOutput::Closed => LauncherMessage::Output {
                            schema: PROTOCOL_SCHEMA,
                            bytes_base64: String::new(),
                            closed: true,
                        },
                        CandidateExecutionOutput::CommandOutput {
                            stream,
                            offset,
                            bytes,
                        } => LauncherMessage::CommandOutput {
                            schema: PROTOCOL_SCHEMA,
                            stream,
                            offset,
                            bytes_base64: STANDARD.encode(bytes),
                        },
                        CandidateExecutionOutput::CommandTerminated { observation } => {
                            LauncherMessage::CommandTerminated {
                                schema: PROTOCOL_SCHEMA,
                                observation,
                            }
                        }
                    };
                    write_message(
                        &mut channel,
                        candidate.control_io_deadline(deadline),
                        &response,
                    )?;
                }
                _ => anyhow::bail!("dedicated launcher received an invalid local message"),
            }
        }
    })();
    if let Err(error) = result {
        return match candidate.stop(deadline) {
            Ok(()) => Err(error),
            Err(cleanup) => {
                Err(error.context(format!("launcher cleanup also failed: {cleanup:#}")))
            }
        };
    }
    result
}

fn write_message(
    channel: &mut lillux::InheritedDuplexChannel,
    deadline: lillux::time::MonotonicDeadline,
    message: &LauncherMessage,
) -> Result<()> {
    let bytes = lillux::canonical_json(&serde_json::to_value(message)?)?.into_bytes();
    ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "launcher message exceeds bound"
    );
    let length = u32::try_from(bytes.len())?.to_be_bytes();
    let mut stream = channel.with_deadline(deadline);
    stream.write_all(&length)?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

fn read_message(
    channel: &mut lillux::InheritedDuplexChannel,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<LauncherMessage> {
    let mut length = [0_u8; 4];
    let mut stream = channel.with_deadline(deadline);
    stream.read_exact(&mut length)?;
    let length = usize::try_from(u32::from_be_bytes(length))?;
    ensure!(
        length > 0 && length <= MAX_MESSAGE_BYTES,
        "invalid launcher message length"
    );
    let mut bytes = vec![0_u8; length];
    stream.read_exact(&mut bytes)?;
    let message: LauncherMessage = serde_json::from_slice(&bytes)?;
    ensure!(
        lillux::canonical_json(&serde_json::to_value(&message)?)?.as_bytes() == bytes,
        "launcher message is not canonical"
    );
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use lillux::crypto::SigningKey;
    use ryeos_state::external_execution::{ChannelDirection, ExecutionFrame, SignedExecutionFrame};

    #[test]
    fn applied_launch_point_read_requires_explicit_pending_or_complete_receipt() {
        let pending = LauncherMessage::AppliedLaunch {
            schema: PROTOCOL_SCHEMA,
            receipt: None,
        };
        let pending_bytes = serde_json::to_vec(&pending).unwrap();
        assert!(matches!(
            serde_json::from_slice::<LauncherMessage>(&pending_bytes).unwrap(),
            LauncherMessage::AppliedLaunch {
                schema: PROTOCOL_SCHEMA,
                receipt: None
            }
        ));
        let mut missing: serde_json::Value = serde_json::from_slice(&pending_bytes).unwrap();
        missing.as_object_mut().unwrap().remove("receipt");
        assert!(serde_json::from_value::<LauncherMessage>(missing).is_err());
        let mut unknown: serde_json::Value = serde_json::from_slice(&pending_bytes).unwrap();
        unknown["ambient_process"] = serde_json::json!(42);
        assert!(serde_json::from_value::<LauncherMessage>(unknown).is_err());
        let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 42,
            namespace_pid: 1,
            effective_uid: 1,
            effective_gid: 1,
            no_new_privs: true,
            seccomp_mode: 2,
            executable_sha256: [1; 32],
            argv_sha256: [2; 32],
            environment_sha256: [3; 32],
            cwd_sha256: [4; 32],
            post_release_mount_view: lillux::LinuxSandboxMountPreparationCommitments {
                schema: 1,
                mount_count: 1,
                destination_access_sha256: [5; 32],
            },
        };
        let complete = LauncherMessage::AppliedLaunch {
            schema: PROTOCOL_SCHEMA,
            receipt: Some(receipt.clone()),
        };
        assert!(matches!(
            serde_json::from_slice::<LauncherMessage>(&serde_json::to_vec(&complete).unwrap())
                .unwrap(),
            LauncherMessage::AppliedLaunch {
                schema: PROTOCOL_SCHEMA,
                receipt: Some(observed)
            } if observed == receipt
        ));
    }

    fn binding() -> (ExecutionChannelBinding, SigningKey) {
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        (
            ExecutionChannelBinding {
                schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
                execution_mode:
                    ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
                placement_thread_id: "T-launcher-protocol".into(),
                allocation_request_digest: "a".repeat(64),
                occurrence_id: "occurrence-launcher-protocol".into(),
                admitted_capsule_hash: "b".repeat(64),
                base_snapshot_hash: "c".repeat(64),
                execution_binding_hash: "d".repeat(64),
                supervisor_runtime_hash: "e".repeat(64),
                candidate_program_digest: "0".repeat(64),
                channel_nonce: "f".repeat(64),
                owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
                supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
                issued_at_ms: now - 1_000,
                execution_deadline_ms: now + 60_000,
                expires_at_ms: now + 120_000,
                candidate_export_max_bytes: 512 * 1024,
                max_frames: 16,
                max_bytes: 1024 * 1024,
            },
            owner,
        )
    }

    fn release(
        binding: &ExecutionChannelBinding,
        owner: &SigningKey,
    ) -> AuthenticatedExecutionFrame {
        let signed = SignedExecutionFrame::sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: binding.digest().unwrap(),
                direction: ChannelDirection::OwnerToSupervisor,
                sequence: 1,
                previous_frame_digest: None,
                acknowledged_peer_sequence: 0,
                payload: ExecutionChannelPayload::Release,
            },
            binding,
            owner,
        )
        .unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(signed).unwrap()).unwrap();
        SignedExecutionFrame::decode_and_verify(
            wire.as_bytes(),
            binding,
            lillux::time::timestamp_millis(),
        )
        .unwrap()
    }

    #[test]
    fn direct_command_output_is_fair_exact_and_requires_actual_status() {
        fn stream(bytes: &[u8]) -> BoundedCandidateExecutionOutput {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            std::io::Seek::rewind(&mut file).unwrap();
            BoundedCandidateExecutionOutput::new(file, 32).unwrap()
        }
        let mut output = DirectCommandObservation {
            stdout: stream(b"abcd"),
            stderr: stream(b"EFGH"),
            stdout_bytes: Vec::new(),
            stderr_bytes: Vec::new(),
            stderr_first: false,
            target_exit: None,
            reason: None,
            stopped: false,
            terminal_sent: false,
        };
        for (stream, offset, bytes) in [
            (ExternalCommandOutputStream::Stdout, 0, b"ab"),
            (ExternalCommandOutputStream::Stderr, 0, b"EF"),
            (ExternalCommandOutputStream::Stdout, 2, b"cd"),
            (ExternalCommandOutputStream::Stderr, 2, b"GH"),
        ] {
            assert_eq!(
                output.poll(2).unwrap(),
                CandidateExecutionOutput::CommandOutput {
                    stream,
                    offset,
                    bytes: bytes.to_vec(),
                }
            );
        }
        // Both EOFs are insufficient; forced cleanup with no target status also
        // remains uncertain. This fixture tests framing, not native observation.
        assert_eq!(output.poll(2).unwrap(), CandidateExecutionOutput::Idle);
        output.stopped = true;
        output.reason = Some(ExternalCommandTerminationReason::Cancelled);
        assert_eq!(output.poll(2).unwrap(), CandidateExecutionOutput::Idle);
        output.target_exit = Some(ExternalTargetExit::Code(0));
        let CandidateExecutionOutput::CommandTerminated { observation } = output.poll(2).unwrap()
        else {
            panic!("complete streams and actual status must produce one terminal observation");
        };
        assert_eq!(
            observation.reason,
            ExternalCommandTerminationReason::Cancelled
        );
        assert_eq!(observation.stdout.bytes, 4);
        assert_eq!(observation.stdout.sha256, lillux::sha256_hex(b"abcd"));
        assert_eq!(observation.stderr.sha256, lillux::sha256_hex(b"EFGH"));
        assert!(!observation.stdout.truncated && !observation.stderr.truncated);
        assert_eq!(output.poll(2).unwrap(), CandidateExecutionOutput::Idle);
    }

    #[test]
    fn direct_cleanup_reason_and_eof_never_invent_target_termination() {
        // Exercise the actual framing owner after a deadline/fault/cleanup
        // indication. This is not a native termination or death-proof fixture:
        // neither the reason nor closed streams can supply the absent status.
        for reason in [
            ExternalCommandTerminationReason::Deadline,
            ExternalCommandTerminationReason::Fault,
            ExternalCommandTerminationReason::Cancelled,
        ] {
            let mut output = DirectCommandObservation {
                stdout: BoundedCandidateExecutionOutput::new(tempfile::tempfile().unwrap(), 1)
                    .unwrap(),
                stderr: BoundedCandidateExecutionOutput::new(tempfile::tempfile().unwrap(), 1)
                    .unwrap(),
                stdout_bytes: Vec::new(),
                stderr_bytes: Vec::new(),
                stderr_first: false,
                target_exit: None,
                reason: Some(reason),
                stopped: true,
                terminal_sent: false,
            };
            for _ in 0..3 {
                assert_eq!(output.poll(1).unwrap(), CandidateExecutionOutput::Idle);
                assert!(output.stdout.closed && output.stderr.closed);
                assert!(output.target_exit.is_none());
                assert!(!output.terminal_sent);
            }
        }
    }

    #[test]
    fn direct_stream_overflow_fails_before_terminal_even_with_zero_target_exit() {
        fn stream(bytes: &[u8]) -> BoundedCandidateExecutionOutput {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            std::io::Seek::rewind(&mut file).unwrap();
            BoundedCandidateExecutionOutput::new(file, 2).unwrap()
        }
        for overflowing in [
            ExternalCommandOutputStream::Stdout,
            ExternalCommandOutputStream::Stderr,
        ] {
            let mut output = DirectCommandObservation {
                stdout: stream(if overflowing == ExternalCommandOutputStream::Stdout {
                    b"bad"
                } else {
                    b"ok"
                }),
                stderr: stream(if overflowing == ExternalCommandOutputStream::Stderr {
                    b"bad"
                } else {
                    b"ok"
                }),
                stdout_bytes: Vec::new(),
                stderr_bytes: Vec::new(),
                stderr_first: false,
                target_exit: Some(ExternalTargetExit::Code(0)),
                reason: Some(ExternalCommandTerminationReason::TargetExited),
                stopped: false,
                terminal_sent: false,
            };
            for expected in [
                ExternalCommandOutputStream::Stdout,
                ExternalCommandOutputStream::Stderr,
            ] {
                assert_eq!(
                    output.poll(2).unwrap(),
                    CandidateExecutionOutput::CommandOutput {
                        stream: expected,
                        offset: 0,
                        bytes: if expected == overflowing {
                            b"ba"
                        } else {
                            b"ok"
                        }
                        .to_vec(),
                    }
                );
            }
            // The next bounded read detects the excess; the production caller
            // propagates this error into native cleanup, not another output poll.
            let error = output.poll(2).unwrap_err();
            assert!(error.to_string().contains("exceeded its aggregate bound"));
            assert_eq!(output.stdout_bytes.len(), 2);
            assert_eq!(output.stderr_bytes.len(), 2);
            assert!(!output.terminal_sent);
        }
    }

    #[test]
    fn launcher_output_wire_refuses_mode_confusion_and_noncanonical_bounds() {
        let direct = ExternalExecutionMode::DirectCommand {
            stdout_max_bytes: 8,
            stderr_max_bytes: 8,
        };
        let protocol = ExternalExecutionMode::StructuredSession {};
        let chunk = |offset| LauncherMessage::CommandOutput {
            schema: PROTOCOL_SCHEMA,
            stream: ExternalCommandOutputStream::Stderr,
            offset,
            bytes_base64: STANDARD.encode(b"err"),
        };
        assert!(decode_execution_output(chunk(0), protocol).is_err());
        assert!(decode_execution_output(chunk(6), direct).is_err());
        assert_eq!(
            decode_execution_output(chunk(5), direct).unwrap(),
            CandidateExecutionOutput::CommandOutput {
                stream: ExternalCommandOutputStream::Stderr,
                offset: 5,
                bytes: b"err".to_vec(),
            }
        );
        for (bytes, closed) in [(b"".as_slice(), true), (b"reply".as_slice(), false)] {
            assert!(
                decode_execution_output(
                    LauncherMessage::Output {
                        schema: PROTOCOL_SCHEMA,
                        bytes_base64: STANDARD.encode(bytes),
                        closed,
                    },
                    direct
                )
                .is_err()
            );
        }
        assert!(
            decode_execution_output(
                LauncherMessage::Output {
                    schema: PROTOCOL_SCHEMA,
                    bytes_base64: STANDARD.encode(b"reply"),
                    closed: true,
                },
                protocol
            )
            .is_err()
        );
        assert_eq!(
            decode_execution_output(
                LauncherMessage::Output {
                    schema: PROTOCOL_SCHEMA,
                    bytes_base64: String::new(),
                    closed: false,
                },
                direct
            )
            .unwrap(),
            CandidateExecutionOutput::Idle
        );
        let commitment = || ExternalCommandOutputCommitment {
            bytes: 0,
            sha256: lillux::sha256_hex(b""),
            truncated: false,
        };
        let terminal = || LauncherMessage::CommandTerminated {
            schema: PROTOCOL_SCHEMA,
            observation: ExternalCommandTermination {
                target_exit: ExternalTargetExit::Code(0),
                reason: ExternalCommandTerminationReason::TargetExited,
                stdout: commitment(),
                stderr: commitment(),
            },
        };
        assert!(decode_execution_output(terminal(), protocol).is_err());
        assert!(matches!(
            decode_execution_output(terminal(), direct).unwrap(),
            CandidateExecutionOutput::CommandTerminated { .. }
        ));
    }

    #[test]
    fn candidate_output_exact_bound_distinguishes_eof_from_overflow() {
        fn file(bytes: &[u8]) -> std::fs::File {
            let mut output = tempfile::tempfile().unwrap();
            output.write_all(bytes).unwrap();
            std::io::Seek::rewind(&mut output).unwrap();
            output
        }

        let mut exact = BoundedCandidateExecutionOutput::new(file(b"exact"), 5).unwrap();
        assert_eq!(
            exact.poll(5).unwrap(),
            CandidateExecutionOutput::Bytes(b"exact".to_vec())
        );
        assert_eq!(exact.poll(5).unwrap(), CandidateExecutionOutput::Closed);
        assert_eq!(exact.poll(5).unwrap(), CandidateExecutionOutput::Closed);

        let mut overflow = BoundedCandidateExecutionOutput::new(file(b"excess"), 5).unwrap();
        assert_eq!(
            overflow.poll(5).unwrap(),
            CandidateExecutionOutput::Bytes(b"exces".to_vec())
        );
        assert!(overflow.poll(5).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn candidate_output_exact_bound_waits_for_real_eof() {
        use std::os::fd::FromRawFd as _;

        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK,) },
            0
        );
        let output = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
        let mut writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
        writer.write_all(b"ok").unwrap();
        let mut bounded = BoundedCandidateExecutionOutput::new(output, 2).unwrap();
        assert_eq!(
            bounded.poll(2).unwrap(),
            CandidateExecutionOutput::Bytes(b"ok".to_vec())
        );
        assert_eq!(bounded.poll(2).unwrap(), CandidateExecutionOutput::Idle);
        drop(writer);
        assert_eq!(bounded.poll(2).unwrap(), CandidateExecutionOutput::Closed);
    }

    #[test]
    fn inherited_launcher_protocol_child_helper() {
        let Ok(binding_json) = std::env::var("RYEOS_TEST_LAUNCHER_BINDING") else {
            return;
        };
        let binding: ExecutionChannelBinding = serde_json::from_str(&binding_json).unwrap();
        let challenge = std::env::var("RYEOS_TEST_LAUNCHER_CHALLENGE").unwrap();
        let occurrence_digest = std::env::var("RYEOS_TEST_LAUNCHER_OCCURRENCE").unwrap();
        let mut channel =
            unsafe { lillux::take_inherited_duplex_channel_from_env("RYEOS_TEST_LAUNCHER_FD") }
                .unwrap();
        let deadline = lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(5));
        let bootstrap = read_message(&mut channel, deadline).unwrap();
        assert!(matches!(
            bootstrap,
            LauncherMessage::Bootstrap {
                schema: PROTOCOL_SCHEMA,
                binding: received,
                challenge: received_challenge,
                occurrence_digest: received_occurrence,
            } if received == binding
                && received_challenge == challenge
                && received_occurrence == occurrence_digest
        ));
        write_message(
            &mut channel,
            deadline,
            &LauncherMessage::Ready {
                schema: PROTOCOL_SCHEMA,
                binding_digest: binding.digest().unwrap(),
                challenge_digest: lillux::sha256_hex(challenge.as_bytes()),
            },
        )
        .unwrap();
        let frame = match read_message(&mut channel, deadline).unwrap() {
            LauncherMessage::Apply {
                schema: PROTOCOL_SCHEMA,
                frame_json,
            } => SignedExecutionFrame::decode_and_verify(
                frame_json.as_bytes(),
                &binding,
                lillux::time::timestamp_millis(),
            )
            .unwrap(),
            _ => panic!("expected exact launcher action"),
        };
        write_message(
            &mut channel,
            deadline,
            &LauncherMessage::Applied {
                schema: PROTOCOL_SCHEMA,
                frame_digest: frame.digest().to_owned(),
                written_bytes: None,
            },
        )
        .unwrap();
        let digest = match read_message(&mut channel, deadline).unwrap() {
            LauncherMessage::AcknowledgeFinish {
                schema: PROTOCOL_SCHEMA,
                frame_digest,
            } => frame_digest,
            _ => panic!("expected exact finish acknowledgement"),
        };
        write_message(
            &mut channel,
            deadline,
            &LauncherMessage::FinishAcknowledged {
                schema: PROTOCOL_SCHEMA,
                frame_digest: digest,
            },
        )
        .unwrap();
        assert!(matches!(
            read_message(&mut channel, deadline).unwrap(),
            LauncherMessage::PollOutput {
                schema: PROTOCOL_SCHEMA,
                maximum_bytes,
            } if maximum_bytes == ryeos_state::external_execution::MAX_CHUNK_BYTES
        ));
        write_message(
            &mut channel,
            deadline,
            &LauncherMessage::Output {
                schema: PROTOCOL_SCHEMA,
                bytes_base64: STANDARD.encode(b"bounded-reply"),
                closed: false,
            },
        )
        .unwrap();
    }

    #[test]
    fn inherited_launcher_handshake_action_and_finish_ack_are_exact() {
        let (parent, child_authority) = lillux::inherited_duplex_channel_pair().unwrap();
        let (binding, owner) = binding();
        let challenge = "1".repeat(64);
        let occurrence_digest = "2".repeat(64);
        let deadline = lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(5));
        let child_descriptor = child_authority.inherited_descriptor().unwrap();
        let mut request = lillux::SubprocessRequest {
            cmd: std::env::current_exe()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
            argv0: None,
            args: vec![
                "--exact".into(),
                "launcher_protocol::tests::inherited_launcher_protocol_child_helper".into(),
                "--nocapture".into(),
            ],
            cwd: None,
            envs: vec![
                (
                    "RYEOS_TEST_LAUNCHER_BINDING".into(),
                    lillux::canonical_json(&serde_json::to_value(&binding).unwrap()).unwrap(),
                ),
                ("RYEOS_TEST_LAUNCHER_CHALLENGE".into(), challenge.clone()),
                (
                    "RYEOS_TEST_LAUNCHER_OCCURRENCE".into(),
                    occurrence_digest.clone(),
                ),
                (
                    "RYEOS_TEST_LAUNCHER_FD".into(),
                    child_descriptor.to_string(),
                ),
            ],
            stdin_data: None,
            timeout: 5.0,
            limits: Some(lillux::SubprocessLimits {
                max_stdout_bytes: Some(64 * 1024),
                max_stderr_bytes: Some(64 * 1024),
                ..lillux::SubprocessLimits::default()
            }),
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        };
        child_authority.retain_for_child(&mut request.inherited_fds);
        let server = lillux::exec::lib_spawn(request)
            .unwrap_or_else(|error| panic!("launch inherited protocol helper: {error:?}"));
        let authenticated = InheritedExternalCandidateLauncherClient::authenticate(
            parent,
            binding.clone(),
            &challenge,
            &occurrence_digest,
            deadline,
        );
        let (mut client, _ready) = match authenticated {
            Ok(authenticated) => authenticated,
            Err(error) => {
                let result = server.wait();
                panic!(
                    "authenticate inherited protocol helper: {error:#}; child success={}; stderr={}",
                    result.success, result.stderr
                );
            }
        };
        let frame = release(&binding, &owner);
        client.release(&frame).unwrap();
        client.acknowledge_finish(frame.digest()).unwrap();
        assert_eq!(
            client.poll_execution_output().unwrap(),
            CandidateExecutionOutput::Bytes(b"bounded-reply".to_vec())
        );
        let result = server.wait();
        assert!(result.success, "helper failed: {}", result.stderr);
    }
}
