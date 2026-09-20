//! Exact inherited-channel protocol between the protected supervisor and one
//! dedicated native candidate launcher.

use std::io::{Read as _, Write as _};

use anyhow::{Context as _, Result, ensure};
use ryeos_state::external_execution::guest_journal::AuthenticatedLauncherReady;
use ryeos_state::external_execution::guest_journal::{
    LauncherOccurrenceEvidence, PreparedGuestJournal,
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
    ExternalCandidateLauncherClient, SerializedExternalCandidateSupervisor,
    SupervisorApplicationOutcome,
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
    AcknowledgeFinish {
        schema: u32,
        frame_digest: String,
    },
    FinishAcknowledged {
        schema: u32,
        frame_digest: String,
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
}

impl LiveInheritedExternalCandidateSupervisor {
    pub fn process_identity(&self) -> &lillux::ExactProcessIdentity {
        &self.process_identity
    }

    pub fn occurrence_digest(&self) -> &str {
        &self.occurrence_digest
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
    launcher_artifact_digest: &str,
    channel_env_name: &str,
    channel_target_fd: u32,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<LiveInheritedExternalCandidateSupervisor> {
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
    Ok(LiveInheritedExternalCandidateSupervisor {
        supervisor: SerializedExternalCandidateSupervisor::new(live_journal, client),
        launcher_process: running,
        process_identity,
        occurrence_digest,
    })
}

pub fn launch_prepared_external_candidate_supervisor(
    prepared_journal: PreparedGuestJournal,
    prepared: PreparedExternalCandidateLauncherRequest,
    launcher_artifact_digest: &str,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<LiveInheritedExternalCandidateSupervisor> {
    launch_external_candidate_supervisor(
        prepared_journal,
        prepared.request,
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
        let bootstrap = LauncherMessage::Bootstrap {
            schema: PROTOCOL_SCHEMA,
            binding: binding.clone(),
            challenge: challenge.to_owned(),
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
}

/// Run the bounded local protocol after a dedicated launcher has prepared its
/// native candidate. This loop owns no signing key or remote transport.
pub fn serve_native_candidate_launcher(
    mut channel: lillux::InheritedDuplexChannel,
    mut candidate: NativeExternalCandidate,
    expected_binding: ExecutionChannelBinding,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    let bootstrap = read_message(&mut channel, deadline)?;
    let challenge = match &bootstrap {
        LauncherMessage::Bootstrap {
            schema,
            binding,
            challenge,
        } if *schema == PROTOCOL_SCHEMA
            && binding == &expected_binding
            && challenge.len() == 64
            && challenge
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) =>
        {
            challenge
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
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use lillux::crypto::SigningKey;
    use ryeos_state::external_execution::{ChannelDirection, ExecutionFrame, SignedExecutionFrame};

    fn binding() -> (ExecutionChannelBinding, SigningKey) {
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        (
            ExecutionChannelBinding {
                schema: 1,
                placement_thread_id: "T-launcher-protocol".into(),
                allocation_request_digest: "a".repeat(64),
                occurrence_id: "occurrence-launcher-protocol".into(),
                admitted_capsule_hash: "b".repeat(64),
                base_snapshot_hash: "c".repeat(64),
                execution_binding_hash: "d".repeat(64),
                supervisor_runtime_hash: "e".repeat(64),
                channel_nonce: "f".repeat(64),
                owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
                supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
                issued_at_ms: now - 1_000,
                execution_deadline_ms: now + 60_000,
                expires_at_ms: now + 120_000,
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
    fn inherited_launcher_protocol_child_helper() {
        let Ok(binding_json) = std::env::var("RYEOS_TEST_LAUNCHER_BINDING") else {
            return;
        };
        let binding: ExecutionChannelBinding = serde_json::from_str(&binding_json).unwrap();
        let challenge = std::env::var("RYEOS_TEST_LAUNCHER_CHALLENGE").unwrap();
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
            } if received == binding && received_challenge == challenge
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
    }

    #[test]
    fn inherited_launcher_handshake_action_and_finish_ack_are_exact() {
        let (parent, child_authority) = lillux::inherited_duplex_channel_pair().unwrap();
        let (binding, owner) = binding();
        let challenge = "1".repeat(64);
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
            .env("RYEOS_TEST_LAUNCHER_CHALLENGE", &challenge);
        child_authority
            .bind_to_command(&mut command, "RYEOS_TEST_LAUNCHER_FD")
            .unwrap();
        let mut server = command.spawn().unwrap();
        let (mut client, _ready) = InheritedExternalCandidateLauncherClient::authenticate(
            parent,
            binding.clone(),
            &challenge,
            deadline,
        )
        .unwrap();
        let frame = release(&binding, &owner);
        client.release(&frame).unwrap();
        client.acknowledge_finish(frame.digest()).unwrap();
        assert!(server.wait().unwrap().success());
    }
}
