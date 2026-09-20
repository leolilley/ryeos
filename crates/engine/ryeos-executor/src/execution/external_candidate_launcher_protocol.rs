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
    SignedExecutionFrame,
};
use serde::{Deserialize, Serialize};

use super::external_candidate::NativeExternalCandidate;
use super::external_candidate_launcher::{
    LAUNCHER_CONTROL_FD, PreparedExternalCandidateLauncherRequest,
};
use super::external_candidate_supervisor::{
    CandidateProtocolOutput, ExternalCandidateLauncherClient,
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
    PollOutput {
        schema: u32,
        maximum_bytes: usize,
    },
    Output {
        schema: u32,
        bytes_base64: String,
        closed: bool,
    },
    Failed {
        schema: u32,
        frame_digest: String,
        detail: String,
    },
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
}

pub enum ExternalOwnerFrameOutcome {
    Application(SupervisorApplicationOutcome),
    Capture(super::external_candidate_supervisor::SupervisorCaptureOutcome),
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
    ) -> Result<super::external_candidate_supervisor::SupervisorCaptureOutcome> {
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

    pub fn poll_protocol_output(&mut self) -> Result<Option<String>> {
        Ok(self
            .supervisor
            .poll_protocol_output(&self.supervisor_signing_key)?
            .map(|frame| frame.canonical().to_owned()))
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
        self.launcher_process
            .abort_and_reap_checked()
            .map_err(anyhow::Error::msg)
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
            let cleanup = running.abort_and_reap_checked();
            return Err(error.context(format!(
                "authenticate dedicated launcher; cleanup={cleanup:?}"
            )));
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
    })
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
        let ready = read_message(&mut channel, deadline)?;
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
        match read_message(&mut self.channel, self.deadline)? {
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
        match read_message(&mut self.channel, self.deadline)? {
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
                read_message(&mut self.channel, self.deadline)?,
                LauncherMessage::FinishAcknowledged {
                    schema: PROTOCOL_SCHEMA,
                    frame_digest: response,
                } if response == frame_digest
            ),
            "dedicated launcher changed finish acknowledgement"
        );
        Ok(())
    }

    fn poll_protocol_output(&mut self) -> Result<CandidateProtocolOutput> {
        write_message(
            &mut self.channel,
            self.deadline,
            &LauncherMessage::PollOutput {
                schema: PROTOCOL_SCHEMA,
                maximum_bytes: ryeos_state::external_execution::MAX_CHUNK_BYTES,
            },
        )?;
        match read_message(&mut self.channel, self.deadline)? {
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
                    (false, _) => Ok(CandidateProtocolOutput::Bytes(bytes)),
                    (true, false) => Ok(CandidateProtocolOutput::Idle),
                    (true, true) => Ok(CandidateProtocolOutput::Closed),
                }
            }
            _ => anyhow::bail!("dedicated launcher returned a mismatched output poll response"),
        }
    }
}

struct BoundedCandidateProtocolOutput {
    output: std::fs::File,
    maximum_bytes: u64,
    observed_bytes: u64,
    closed: bool,
}

impl BoundedCandidateProtocolOutput {
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

    fn poll(&mut self, maximum_bytes: usize) -> Result<CandidateProtocolOutput> {
        ensure!(
            (1..=ryeos_state::external_execution::MAX_CHUNK_BYTES).contains(&maximum_bytes),
            "candidate protocol output poll bound is invalid"
        );
        if self.closed {
            return Ok(CandidateProtocolOutput::Closed);
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
                return Ok(CandidateProtocolOutput::Closed);
            }
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(CandidateProtocolOutput::Idle);
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
        Ok(CandidateProtocolOutput::Bytes(bytes))
    }
}

/// Run the bounded local protocol after a dedicated launcher has prepared its
/// native candidate. This loop owns no signing key or remote transport.
pub fn serve_native_candidate_launcher(
    mut channel: lillux::InheritedDuplexChannel,
    mut candidate: NativeExternalCandidate,
    protocol_output: std::fs::File,
    maximum_output_bytes: u64,
    authority: ryeos_state::PinnedStateAuthority,
    bootstrap_digest: String,
    expected_binding: ExecutionChannelBinding,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    let mut protocol_output =
        BoundedCandidateProtocolOutput::new(protocol_output, maximum_output_bytes)?;
    let bootstrap = read_message(&mut channel, deadline)?;
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
        deadline,
        &LauncherMessage::Ready {
            schema: PROTOCOL_SCHEMA,
            binding_digest: expected_binding.digest()?,
            challenge_digest: lillux::sha256_hex(challenge.as_bytes()),
        },
    )?;
    loop {
        match read_message(&mut channel, deadline)? {
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
                        deadline.remaining(),
                    );
                    let response = match export {
                        Ok(export) => LauncherMessage::Captured {
                            schema: PROTOCOL_SCHEMA,
                            capture: DurableNativeCandidateCapture {
                                quiesce_frame_digest: digest,
                                occurrence_digest: export.occurrence_digest().to_owned(),
                                durable_stage_id: export.durable_stage_id().to_owned(),
                                snapshot_hash: export.snapshot_hash().to_owned(),
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
                    write_message(&mut channel, deadline, &response)?;
                    continue;
                }
                let outcome = match &decoded.frame().payload {
                    ExecutionChannelPayload::Release => candidate.release(&decoded).map(|_| None),
                    ExecutionChannelPayload::ProtocolBytes { .. } => {
                        candidate.apply_protocol_chunk(&decoded).map(Some)
                    }
                    ExecutionChannelPayload::Cancel => candidate
                        .cancel(&decoded, deadline.remaining())
                        .map(|_| None),
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
                write_message(&mut channel, deadline, &response)?;
            }
            LauncherMessage::AcknowledgeFinish {
                schema,
                frame_digest,
            } if schema == PROTOCOL_SCHEMA => {
                candidate.acknowledge_application_finish(&frame_digest)?;
                write_message(
                    &mut channel,
                    deadline,
                    &LauncherMessage::FinishAcknowledged {
                        schema: PROTOCOL_SCHEMA,
                        frame_digest,
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
                let (bytes, closed) = match protocol_output.poll(maximum_bytes)? {
                    CandidateProtocolOutput::Bytes(bytes) => (bytes, false),
                    CandidateProtocolOutput::Idle => (Vec::new(), false),
                    CandidateProtocolOutput::Closed => (Vec::new(), true),
                };
                write_message(
                    &mut channel,
                    deadline,
                    &LauncherMessage::Output {
                        schema: PROTOCOL_SCHEMA,
                        bytes_base64: STANDARD.encode(bytes),
                        closed,
                    },
                )?;
            }
            _ => anyhow::bail!("dedicated launcher received an invalid local message"),
        }
    }
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

    fn binding() -> (ExecutionChannelBinding, SigningKey) {
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        (
            ExecutionChannelBinding {
                schema: 3,
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
    fn candidate_output_exact_bound_distinguishes_eof_from_overflow() {
        fn file(bytes: &[u8]) -> std::fs::File {
            let mut output = tempfile::tempfile().unwrap();
            output.write_all(bytes).unwrap();
            std::io::Seek::rewind(&mut output).unwrap();
            output
        }

        let mut exact = BoundedCandidateProtocolOutput::new(file(b"exact"), 5).unwrap();
        assert_eq!(
            exact.poll(5).unwrap(),
            CandidateProtocolOutput::Bytes(b"exact".to_vec())
        );
        assert_eq!(exact.poll(5).unwrap(), CandidateProtocolOutput::Closed);
        assert_eq!(exact.poll(5).unwrap(), CandidateProtocolOutput::Closed);

        let mut overflow = BoundedCandidateProtocolOutput::new(file(b"excess"), 5).unwrap();
        assert_eq!(
            overflow.poll(5).unwrap(),
            CandidateProtocolOutput::Bytes(b"exces".to_vec())
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
        let mut bounded = BoundedCandidateProtocolOutput::new(output, 2).unwrap();
        assert_eq!(
            bounded.poll(2).unwrap(),
            CandidateProtocolOutput::Bytes(b"ok".to_vec())
        );
        assert_eq!(bounded.poll(2).unwrap(), CandidateProtocolOutput::Idle);
        drop(writer);
        assert_eq!(bounded.poll(2).unwrap(), CandidateProtocolOutput::Closed);
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
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("execution::external_candidate_launcher_protocol::tests::inherited_launcher_protocol_child_helper")
            .arg("--nocapture")
            .env_clear()
            .env(
                "RYEOS_TEST_LAUNCHER_BINDING",
                lillux::canonical_json(&serde_json::to_value(&binding).unwrap()).unwrap(),
            )
            .env("RYEOS_TEST_LAUNCHER_CHALLENGE", &challenge)
            .env("RYEOS_TEST_LAUNCHER_OCCURRENCE", &occurrence_digest);
        child_authority
            .bind_to_command(&mut command, "RYEOS_TEST_LAUNCHER_FD")
            .unwrap();
        let mut server = command.spawn().unwrap();
        let (mut client, _ready) = InheritedExternalCandidateLauncherClient::authenticate(
            parent,
            binding.clone(),
            &challenge,
            &occurrence_digest,
            deadline,
        )
        .unwrap();
        let frame = release(&binding, &owner);
        client.release(&frame).unwrap();
        client.acknowledge_finish(frame.digest()).unwrap();
        assert_eq!(
            client.poll_protocol_output().unwrap(),
            CandidateProtocolOutput::Bytes(b"bounded-reply".to_vec())
        );
        assert!(server.wait().unwrap().success());
    }
}
