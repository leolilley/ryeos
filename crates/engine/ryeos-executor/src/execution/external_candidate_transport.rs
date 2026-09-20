//! Reconnectable transport driver for one protected external candidate.
//!
//! HTTP success is not application evidence. This driver retries the same
//! signed supervisor frame after transport ambiguity, validates every returned
//! owner frame against the attached binding, and lets the durable guest journal
//! decide whether an exact frame is new, applied, uncertain, or revoked.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_state::external_execution::{
    ChannelDirection, ExecutionChannelBinding, ExecutionChannelPayload, SignedExecutionFrame,
};

use super::external_candidate_launcher_protocol::{
    ExternalOwnerFrameDispatch, ExternalOwnerFrameOutcome, LiveInheritedExternalCandidateSupervisor,
};
use super::external_candidate_supervisor::SupervisorApplicationOutcome;

/// Controller frame returned by the occurrence-authenticated exchange route.
/// The transport adapter must decode canonical base64 before constructing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalTransportFrame {
    pub sequence: u64,
    pub frame_digest: String,
    pub canonical_wire: Vec<u8>,
}

/// One exact response from the signed external-channel exchange route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalTransportExchange {
    pub schema: u32,
    pub incoming_new: bool,
    pub incoming_sequence: u64,
    pub incoming_frame_digest: String,
    pub acknowledgement_frame_digest: Option<String>,
    pub outbound_frames: Vec<ExternalTransportFrame>,
    pub urgent_revocation_frame: Option<ExternalTransportFrame>,
}

/// Narrow adapter boundary. A failed call is ambiguous: the implementation
/// must not synthesize a successor frame and the driver will retry these exact
/// canonical bytes.
pub trait ExternalExecutionChannelTransport {
    fn exchange(&mut self, canonical_supervisor_frame: &[u8]) -> Result<ExternalTransportExchange>;
}

/// Runtime surface needed by the transport driver. The production
/// implementation is the live inherited launcher composition; tests can use a
/// protocol fixture without weakening the native launcher boundary.
pub trait ExternalSupervisorRuntime {
    fn binding(&self) -> &ExecutionChannelBinding;
    fn ready_frame(&self) -> &str;
    fn dispatch_owner_frame(&mut self, wire: &[u8]) -> Result<ExternalOwnerFrameDispatch>;
}

impl ExternalSupervisorRuntime for LiveInheritedExternalCandidateSupervisor {
    fn binding(&self) -> &ExecutionChannelBinding {
        self.binding()
    }

    fn ready_frame(&self) -> &str {
        self.ready_frame()
    }

    fn dispatch_owner_frame(&mut self, wire: &[u8]) -> Result<ExternalOwnerFrameDispatch> {
        self.dispatch_owner_frame(wire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalTransportProgress {
    /// At least one signed frame was exchanged or locally applied.
    Advanced,
    /// The exact poll frame was accepted but no new controller work arrived.
    Idle,
    /// Sticky cancellation was durably applied by the guest. This is not
    /// external-provider cleanup or occurrence termination evidence.
    Revoked,
    /// Cancellation is durably claimed or applied but independent native and
    /// provider cleanup evidence is still missing. This must not be reported
    /// as successful revocation or used to release placement capacity.
    RevokedAwaitingCleanup,
}

/// Bounded, single-occurrence control loop. It owns neither placement nor
/// cloud lifecycle authority and cannot recreate a lost launcher.
pub struct ExternalCandidateTransportDriver<R, T> {
    runtime: R,
    transport: T,
    pending_supervisor: BTreeMap<u64, Vec<u8>>,
    poll_frame: Option<Vec<u8>>,
    partial_owner_frame: Option<Vec<u8>>,
    revocation_progress: Option<ExternalTransportProgress>,
    maximum_response_frames: usize,
    maximum_response_bytes: usize,
}

impl<R: ExternalSupervisorRuntime, T: ExternalExecutionChannelTransport>
    ExternalCandidateTransportDriver<R, T>
{
    pub fn new(runtime: R, transport: T) -> Result<Self> {
        let ready = runtime.ready_frame().as_bytes().to_vec();
        let verified = SignedExecutionFrame::decode_and_verify(
            &ready,
            runtime.binding(),
            lillux::time::timestamp_millis(),
        )?;
        ensure!(
            verified.frame().direction == ChannelDirection::SupervisorToOwner
                && matches!(
                    verified.frame().payload,
                    ExecutionChannelPayload::Ready { .. }
                ),
            "external control loop must start from its exact signed Ready frame"
        );
        let maximum_response_bytes = usize::try_from(runtime.binding().max_bytes)
            .unwrap_or(usize::MAX)
            .min(1024 * 1024);
        let maximum_response_frames = usize::try_from(runtime.binding().max_frames)
            .unwrap_or(usize::MAX)
            .min(16);
        let mut pending_supervisor = BTreeMap::new();
        pending_supervisor.insert(verified.frame().sequence, ready);
        Ok(Self {
            runtime,
            transport,
            pending_supervisor,
            poll_frame: None,
            partial_owner_frame: None,
            revocation_progress: None,
            maximum_response_frames,
            maximum_response_bytes,
        })
    }

    pub fn is_revoked(&self) -> bool {
        self.revocation_progress.is_some()
    }

    pub fn into_parts(self) -> (R, T) {
        (self.runtime, self.transport)
    }

    /// Perform one bounded exchange. On transport failure no local queue state
    /// changes, so the caller can retry and the exact same signed bytes are
    /// presented. The method never sleeps and never converts a timeout into
    /// evidence about remote execution.
    pub fn step(&mut self) -> Result<ExternalTransportProgress> {
        let outgoing = self
            .pending_supervisor
            .first_key_value()
            .map(|(_, wire)| wire.clone())
            .or_else(|| self.poll_frame.clone())
            .context("external control loop has no exact frame available for polling")?;
        let sent = SignedExecutionFrame::decode_and_verify(
            &outgoing,
            self.runtime.binding(),
            lillux::time::timestamp_millis(),
        )?;
        ensure!(
            sent.frame().direction == ChannelDirection::SupervisorToOwner,
            "external control loop attempted to send an owner frame"
        );

        // Do not mutate any local frontier before this returns. An error is an
        // ambiguous network outcome and must retain exact-byte retry.
        let response = self.transport.exchange(&outgoing)?;
        self.validate_response(&sent, &response)?;

        self.pending_supervisor.remove(&sent.frame().sequence);
        self.poll_frame = Some(outgoing);

        if let Some(urgent) = response.urgent_revocation_frame.as_ref() {
            return self.dispatch_urgent_revocation(urgent);
        }

        let mut advanced = false;
        if let Some(partial) = self.partial_owner_frame.clone() {
            advanced = true;
            self.dispatch_ordinary(&partial)?;
        } else {
            for frame in &response.outbound_frames {
                if self.revocation_progress.is_some()
                    && !self.owner_frame_is_acknowledgement(frame)?
                {
                    // After sticky cancellation, ordinary input is not executed.
                    // Feed at most one through the journal so it receives exact
                    // revoked evidence; later backlog remains controller-owned.
                    self.dispatch_ordinary(&frame.canonical_wire)?;
                    advanced = true;
                    break;
                }
                self.dispatch_ordinary(&frame.canonical_wire)?;
                advanced = true;
                if self.partial_owner_frame.is_some() {
                    break;
                }
            }
        }
        if let Some(progress) = self.revocation_progress {
            Ok(progress)
        } else if advanced || response.incoming_new {
            Ok(ExternalTransportProgress::Advanced)
        } else {
            Ok(ExternalTransportProgress::Idle)
        }
    }

    fn validate_response(
        &self,
        sent: &ryeos_state::external_execution::AuthenticatedExecutionFrame,
        response: &ExternalTransportExchange,
    ) -> Result<()> {
        ensure!(
            response.schema == 1,
            "unsupported external exchange response schema"
        );
        ensure!(
            response.incoming_sequence == sent.frame().sequence
                && response.incoming_frame_digest == sent.digest(),
            "external exchange response changed the submitted frame"
        );
        if let Some(digest) = response.acknowledgement_frame_digest.as_deref() {
            validate_digest(digest, "external acknowledgement frame digest")?;
        }
        ensure!(
            response.outbound_frames.len() <= self.maximum_response_frames,
            "external exchange response exceeds its frame bound"
        );
        let mut bytes = 0_usize;
        let mut ordinary_sequences = BTreeSet::new();
        let mut previous = None;
        for frame in &response.outbound_frames {
            bytes = bytes
                .checked_add(frame.canonical_wire.len())
                .context("external response byte count overflow")?;
            ensure!(
                ordinary_sequences.insert(frame.sequence),
                "external exchange response repeats an owner sequence"
            );
            if let Some(prior) = previous {
                ensure!(
                    frame.sequence > prior,
                    "external owner backlog is not ordered"
                );
            }
            previous = Some(frame.sequence);
            self.validate_owner_envelope(frame)?;
        }
        ensure!(
            bytes <= self.maximum_response_bytes,
            "external exchange response exceeds its byte bound"
        );
        if let Some(urgent) = response.urgent_revocation_frame.as_ref() {
            self.validate_owner_envelope(urgent)?;
            let verified = SignedExecutionFrame::decode_and_verify(
                &urgent.canonical_wire,
                self.runtime.binding(),
                lillux::time::timestamp_millis(),
            )?;
            ensure!(
                matches!(verified.frame().payload, ExecutionChannelPayload::Cancel),
                "external urgent lane carried non-cancellation input"
            );
            if ordinary_sequences.contains(&urgent.sequence) {
                let ordinary = response
                    .outbound_frames
                    .iter()
                    .find(|frame| frame.sequence == urgent.sequence)
                    .expect("set membership came from the response");
                ensure!(
                    ordinary.frame_digest == urgent.frame_digest
                        && ordinary.canonical_wire == urgent.canonical_wire,
                    "urgent cancellation contradicts ordinary backlog"
                );
            }
        }
        Ok(())
    }

    fn validate_owner_envelope(&self, envelope: &ExternalTransportFrame) -> Result<()> {
        validate_digest(&envelope.frame_digest, "external owner frame digest")?;
        ensure!(
            envelope.canonical_wire.len() <= ryeos_state::external_execution::MAX_FRAME_BYTES,
            "external owner frame exceeds its wire bound"
        );
        let verified = SignedExecutionFrame::decode_and_verify(
            &envelope.canonical_wire,
            self.runtime.binding(),
            lillux::time::timestamp_millis(),
        )?;
        ensure!(
            verified.frame().direction == ChannelDirection::OwnerToSupervisor
                && verified.frame().sequence == envelope.sequence
                && verified.digest() == envelope.frame_digest,
            "external owner envelope contradicts its signed frame"
        );
        Ok(())
    }

    fn owner_frame_is_acknowledgement(&self, envelope: &ExternalTransportFrame) -> Result<bool> {
        let verified = SignedExecutionFrame::decode_and_verify(
            &envelope.canonical_wire,
            self.runtime.binding(),
            lillux::time::timestamp_millis(),
        )?;
        Ok(matches!(
            verified.frame().payload,
            ExecutionChannelPayload::Acknowledge { .. }
        ))
    }

    fn dispatch_urgent_revocation(
        &mut self,
        frame: &ExternalTransportFrame,
    ) -> Result<ExternalTransportProgress> {
        self.partial_owner_frame = None;
        let dispatched = self.runtime.dispatch_owner_frame(&frame.canonical_wire)?;
        let progress = match dispatched.outcome {
            ExternalOwnerFrameOutcome::Application(
                SupervisorApplicationOutcome::Applied
                | SupervisorApplicationOutcome::FinishReconciled,
            ) => ExternalTransportProgress::Revoked,
            ExternalOwnerFrameOutcome::Application(
                SupervisorApplicationOutcome::RevokedAwaitingCleanup
                | SupervisorApplicationOutcome::ClaimedUnknown,
            ) => ExternalTransportProgress::RevokedAwaitingCleanup,
            _ => bail!(
                "urgent external cancellation did not reach a durable terminal application state"
            ),
        };
        self.revocation_progress = Some(progress);
        self.enqueue_dispatch(dispatched)?;
        Ok(progress)
    }

    fn dispatch_ordinary(&mut self, wire: &[u8]) -> Result<()> {
        let dispatched = self.runtime.dispatch_owner_frame(wire)?;
        let is_partial = matches!(
            dispatched.outcome,
            ExternalOwnerFrameOutcome::Application(SupervisorApplicationOutcome::Partial)
        );
        let partial_wire = is_partial.then(|| wire.to_vec());
        self.enqueue_dispatch(dispatched)?;
        self.partial_owner_frame = partial_wire;
        Ok(())
    }

    fn enqueue_dispatch(&mut self, dispatched: ExternalOwnerFrameDispatch) -> Result<()> {
        let mut frames = vec![dispatched.acknowledgement_frame.into_bytes()];
        if let ExternalOwnerFrameOutcome::Capture(capture) = dispatched.outcome {
            frames.push(capture.sealed_frame.into_bytes());
        }
        frames.sort_by_key(|wire| {
            SignedExecutionFrame::decode_and_verify(
                wire,
                self.runtime.binding(),
                self.runtime.binding().issued_at_ms,
            )
            .map(|frame| frame.frame().sequence)
            .unwrap_or(u64::MAX)
        });
        for wire in frames {
            let verified = SignedExecutionFrame::decode_and_verify(
                &wire,
                self.runtime.binding(),
                lillux::time::timestamp_millis(),
            )?;
            ensure!(
                verified.frame().direction == ChannelDirection::SupervisorToOwner,
                "external dispatcher returned a controller-authored frame"
            );
            match self.pending_supervisor.entry(verified.frame().sequence) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(wire);
                }
                std::collections::btree_map::Entry::Occupied(entry) => ensure!(
                    entry.get() == &wire,
                    "external dispatcher forked a supervisor sequence"
                ),
            }
        }
        Ok(())
    }
}

fn validate_digest(value: &str, label: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} is not a canonical SHA-256 digest");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ryeos_state::external_execution::{
        AuthenticatedExecutionFrame, ExecutionFrame, ExecutionFrameApplication,
    };

    use super::*;

    struct FixtureRuntime {
        binding: ExecutionChannelBinding,
        supervisor_key: lillux::crypto::SigningKey,
        ready: String,
        next_sequence: u64,
        previous_digest: String,
        dispatched: Vec<&'static str>,
        protocol_partial: bool,
        revoked: bool,
        cancel_uncertain: bool,
        fail_next_dispatch: bool,
    }

    impl FixtureRuntime {
        fn new(
            binding: ExecutionChannelBinding,
            supervisor_key: lillux::crypto::SigningKey,
        ) -> Self {
            let ready = sign(
                &binding,
                &supervisor_key,
                ChannelDirection::SupervisorToOwner,
                1,
                None,
                0,
                ExecutionChannelPayload::Ready {
                    supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                    base_snapshot_hash: binding.base_snapshot_hash.clone(),
                },
            );
            Self {
                binding,
                supervisor_key,
                ready: ready.canonical().to_owned(),
                next_sequence: 2,
                previous_digest: ready.digest().to_owned(),
                dispatched: Vec::new(),
                protocol_partial: false,
                revoked: false,
                cancel_uncertain: false,
                fail_next_dispatch: false,
            }
        }

        fn acknowledgement(
            &mut self,
            peer: &AuthenticatedExecutionFrame,
            application: ExecutionFrameApplication,
        ) -> String {
            let signed = sign(
                &self.binding,
                &self.supervisor_key,
                ChannelDirection::SupervisorToOwner,
                self.next_sequence,
                Some(self.previous_digest.clone()),
                peer.frame().sequence,
                ExecutionChannelPayload::Acknowledge {
                    peer_frame_sequence: peer.frame().sequence,
                    peer_frame_digest: peer.digest().to_owned(),
                    application,
                },
            );
            self.next_sequence += 1;
            self.previous_digest = signed.digest().to_owned();
            signed.canonical().to_owned()
        }
    }

    impl ExternalSupervisorRuntime for FixtureRuntime {
        fn binding(&self) -> &ExecutionChannelBinding {
            &self.binding
        }

        fn ready_frame(&self) -> &str {
            &self.ready
        }

        fn dispatch_owner_frame(&mut self, wire: &[u8]) -> Result<ExternalOwnerFrameDispatch> {
            if self.fail_next_dispatch {
                self.fail_next_dispatch = false;
                bail!("fixture dispatch failed after an uncertain local boundary");
            }
            let peer = SignedExecutionFrame::decode_and_verify(
                wire,
                &self.binding,
                lillux::time::timestamp_millis(),
            )?;
            let (outcome, application) = match &peer.frame().payload {
                ExecutionChannelPayload::Cancel => {
                    self.dispatched.push("cancel");
                    self.revoked = true;
                    if self.cancel_uncertain {
                        (
                            ExternalOwnerFrameOutcome::Application(
                                SupervisorApplicationOutcome::RevokedAwaitingCleanup,
                            ),
                            ExecutionFrameApplication::Claimed,
                        )
                    } else {
                        (
                            ExternalOwnerFrameOutcome::Application(
                                SupervisorApplicationOutcome::Applied,
                            ),
                            ExecutionFrameApplication::Applied,
                        )
                    }
                }
                ExecutionChannelPayload::ProtocolBytes { .. } if !self.revoked => {
                    self.dispatched.push("protocol");
                    if self.protocol_partial {
                        (
                            ExternalOwnerFrameOutcome::Application(
                                SupervisorApplicationOutcome::Applied,
                            ),
                            ExecutionFrameApplication::Applied,
                        )
                    } else {
                        self.protocol_partial = true;
                        (
                            ExternalOwnerFrameOutcome::Application(
                                SupervisorApplicationOutcome::Partial,
                            ),
                            ExecutionFrameApplication::Claimed,
                        )
                    }
                }
                ExecutionChannelPayload::Release if !self.revoked => {
                    self.dispatched.push("release");
                    (
                        ExternalOwnerFrameOutcome::Application(
                            SupervisorApplicationOutcome::Applied,
                        ),
                        ExecutionFrameApplication::Applied,
                    )
                }
                ExecutionChannelPayload::Release
                | ExecutionChannelPayload::ProtocolBytes { .. } => {
                    self.dispatched.push("ordinary");
                    (
                        ExternalOwnerFrameOutcome::Application(
                            SupervisorApplicationOutcome::Revoked,
                        ),
                        ExecutionFrameApplication::Revoked,
                    )
                }
                ExecutionChannelPayload::Acknowledge { .. } => {
                    self.dispatched.push("acknowledge");
                    (
                        ExternalOwnerFrameOutcome::Acknowledgement,
                        ExecutionFrameApplication::Applied,
                    )
                }
                _ => bail!("unsupported fixture controller payload"),
            };
            let acknowledgement_frame = self.acknowledgement(&peer, application);
            Ok(ExternalOwnerFrameDispatch {
                outcome,
                acknowledgement_frame,
            })
        }
    }

    enum TransportAction {
        Fail,
        Respond(ExternalTransportExchange),
    }

    struct FixtureTransport {
        actions: VecDeque<TransportAction>,
        calls: Vec<Vec<u8>>,
    }

    impl FixtureTransport {
        fn new(actions: Vec<TransportAction>) -> Self {
            Self {
                actions: actions.into(),
                calls: Vec::new(),
            }
        }
    }

    impl ExternalExecutionChannelTransport for FixtureTransport {
        fn exchange(
            &mut self,
            canonical_supervisor_frame: &[u8],
        ) -> Result<ExternalTransportExchange> {
            self.calls.push(canonical_supervisor_frame.to_vec());
            match self
                .actions
                .pop_front()
                .context("fixture transport exhausted")?
            {
                TransportAction::Fail => bail!("ambiguous fixture transport failure"),
                TransportAction::Respond(response) => Ok(response),
            }
        }
    }

    fn fixture() -> (
        ExecutionChannelBinding,
        lillux::crypto::SigningKey,
        lillux::crypto::SigningKey,
    ) {
        let owner = lillux::crypto::SigningKey::from_bytes(&[11; 32]);
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[12; 32]);
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        let binding = ExecutionChannelBinding {
            schema: 1,
            placement_thread_id: "T-external-transport".into(),
            allocation_request_digest: "a".repeat(64),
            occurrence_id: "occurrence-one".into(),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            channel_nonce: "f".repeat(64),
            owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
                &owner.verifying_key(),
            )
            .unwrap(),
            supervisor_public_key: ryeos_state::external_execution::encode_channel_public_key(
                &supervisor.verifying_key(),
            )
            .unwrap(),
            issued_at_ms: now - 1_000,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            max_frames: 64,
            max_bytes: 1024 * 1024,
        };
        binding.validate().unwrap();
        (binding, owner, supervisor)
    }

    fn sign(
        binding: &ExecutionChannelBinding,
        key: &lillux::crypto::SigningKey,
        direction: ChannelDirection,
        sequence: u64,
        previous_frame_digest: Option<String>,
        acknowledged_peer_sequence: u64,
        payload: ExecutionChannelPayload,
    ) -> AuthenticatedExecutionFrame {
        let signed = SignedExecutionFrame::sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: binding.digest().unwrap(),
                direction,
                sequence,
                previous_frame_digest,
                acknowledged_peer_sequence,
                payload,
            },
            binding,
            key,
        )
        .unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(signed).unwrap()).unwrap();
        SignedExecutionFrame::decode_and_verify(wire.as_bytes(), binding, binding.issued_at_ms)
            .unwrap()
    }

    fn envelope(frame: &AuthenticatedExecutionFrame) -> ExternalTransportFrame {
        ExternalTransportFrame {
            sequence: frame.frame().sequence,
            frame_digest: frame.digest().to_owned(),
            canonical_wire: frame.canonical().as_bytes().to_vec(),
        }
    }

    fn response(
        sent: &AuthenticatedExecutionFrame,
        outgoing: Vec<ExternalTransportFrame>,
        urgent: Option<ExternalTransportFrame>,
    ) -> ExternalTransportExchange {
        ExternalTransportExchange {
            schema: 1,
            incoming_new: true,
            incoming_sequence: sent.frame().sequence,
            incoming_frame_digest: sent.digest().to_owned(),
            acknowledgement_frame_digest: None,
            outbound_frames: outgoing,
            urgent_revocation_frame: urgent,
        }
    }

    #[test]
    fn ambiguous_exchange_retries_byte_identical_frame_without_ack_ping_pong() {
        let (binding, owner, supervisor) = fixture();
        let runtime = FixtureRuntime::new(binding.clone(), supervisor);
        let ready = SignedExecutionFrame::decode_and_verify(
            runtime.ready.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let owner_ack = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            ready.frame().sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: ready.frame().sequence,
                peer_frame_digest: ready.digest().to_owned(),
                application: ExecutionFrameApplication::Applied,
            },
        );
        let transport = FixtureTransport::new(vec![
            TransportAction::Fail,
            TransportAction::Respond(response(&ready, vec![envelope(&owner_ack)], None)),
        ]);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();
        assert!(driver.step().is_err());
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        let outgoing = driver
            .pending_supervisor
            .first_key_value()
            .unwrap()
            .1
            .clone();
        let sent =
            SignedExecutionFrame::decode_and_verify(&outgoing, &binding, binding.issued_at_ms)
                .unwrap();
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(&sent, vec![], None)));
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(&sent, vec![], None)));
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        let (runtime, transport) = driver.into_parts();
        assert_eq!(transport.calls.len(), 4);
        assert_eq!(transport.calls[0], transport.calls[1]);
        assert_eq!(transport.calls[2], transport.calls[3]);
        assert_eq!(runtime.dispatched, vec!["acknowledge"]);
    }

    #[test]
    fn urgent_cancel_overtakes_ordinary_backlog_and_revokes_later_replay() {
        let (binding, owner, supervisor) = fixture();
        let runtime = FixtureRuntime::new(binding.clone(), supervisor);
        let ready = SignedExecutionFrame::decode_and_verify(
            runtime.ready.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let release = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            ready.frame().sequence,
            ExecutionChannelPayload::Release,
        );
        let cancel = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release.digest().to_owned()),
            ready.frame().sequence,
            ExecutionChannelPayload::Cancel,
        );
        let first = response(&ready, vec![envelope(&release)], Some(envelope(&cancel)));
        let transport = FixtureTransport::new(vec![TransportAction::Respond(first)]);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Revoked);
        let outgoing = driver
            .pending_supervisor
            .first_key_value()
            .unwrap()
            .1
            .clone();
        let sent =
            SignedExecutionFrame::decode_and_verify(&outgoing, &binding, binding.issued_at_ms)
                .unwrap();
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(
                &sent,
                vec![envelope(&release)],
                None,
            )));
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Revoked);
        let (runtime, _) = driver.into_parts();
        assert_eq!(runtime.dispatched, vec!["cancel", "ordinary"]);
    }

    #[test]
    fn uncertain_cancel_remains_uncertain_across_later_empty_poll() {
        let (binding, owner, supervisor) = fixture();
        let mut runtime = FixtureRuntime::new(binding.clone(), supervisor);
        runtime.cancel_uncertain = true;
        let ready = SignedExecutionFrame::decode_and_verify(
            runtime.ready.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let cancel = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            ready.frame().sequence,
            ExecutionChannelPayload::Cancel,
        );
        let transport = FixtureTransport::new(vec![TransportAction::Respond(response(
            &ready,
            vec![],
            Some(envelope(&cancel)),
        ))]);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();

        assert_eq!(
            driver.step().unwrap(),
            ExternalTransportProgress::RevokedAwaitingCleanup
        );
        let outgoing = driver
            .pending_supervisor
            .first_key_value()
            .unwrap()
            .1
            .clone();
        let sent =
            SignedExecutionFrame::decode_and_verify(&outgoing, &binding, binding.issued_at_ms)
                .unwrap();
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(&sent, vec![], None)));

        assert_eq!(
            driver.step().unwrap(),
            ExternalTransportProgress::RevokedAwaitingCleanup
        );
        assert!(driver.is_revoked());
        let (runtime, _) = driver.into_parts();
        assert_eq!(runtime.dispatched, vec!["cancel"]);
    }

    #[test]
    fn partial_protocol_input_finishes_before_later_backlog_is_dispatched() {
        let (binding, owner, supervisor) = fixture();
        let runtime = FixtureRuntime::new(binding.clone(), supervisor);
        let ready = SignedExecutionFrame::decode_and_verify(
            runtime.ready.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let protocol = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            ready.frame().sequence,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"bounded"),
            },
        );
        let release = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(protocol.digest().to_owned()),
            ready.frame().sequence,
            ExecutionChannelPayload::Release,
        );
        let first = response(&ready, vec![envelope(&protocol)], None);
        let transport = FixtureTransport::new(vec![TransportAction::Respond(first)]);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);

        let outgoing = driver
            .pending_supervisor
            .first_key_value()
            .unwrap()
            .1
            .clone();
        let sent =
            SignedExecutionFrame::decode_and_verify(&outgoing, &binding, binding.issued_at_ms)
                .unwrap();
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(
                &sent,
                vec![envelope(&protocol), envelope(&release)],
                None,
            )));
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        let outgoing = driver
            .pending_supervisor
            .first_key_value()
            .unwrap()
            .1
            .clone();
        let sent =
            SignedExecutionFrame::decode_and_verify(&outgoing, &binding, binding.issued_at_ms)
                .unwrap();
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(
                &sent,
                vec![envelope(&release)],
                None,
            )));
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        let (runtime, _) = driver.into_parts();
        assert_eq!(runtime.dispatched, vec!["protocol", "protocol", "release"]);
    }

    #[test]
    fn partial_protocol_continuation_survives_local_dispatch_error() {
        let (binding, owner, supervisor) = fixture();
        let runtime = FixtureRuntime::new(binding.clone(), supervisor);
        let ready = SignedExecutionFrame::decode_and_verify(
            runtime.ready.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let protocol = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            ready.frame().sequence,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"bounded"),
            },
        );
        let transport = FixtureTransport::new(vec![TransportAction::Respond(response(
            &ready,
            vec![envelope(&protocol)],
            None,
        ))]);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        driver.runtime.fail_next_dispatch = true;

        let outgoing = driver
            .pending_supervisor
            .first_key_value()
            .unwrap()
            .1
            .clone();
        let sent =
            SignedExecutionFrame::decode_and_verify(&outgoing, &binding, binding.issued_at_ms)
                .unwrap();
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(&sent, vec![], None)));
        assert!(driver.step().is_err());
        assert_eq!(
            driver.partial_owner_frame.as_deref(),
            Some(protocol.canonical().as_bytes())
        );

        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(&sent, vec![], None)));
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        let (runtime, transport) = driver.into_parts();
        assert_eq!(runtime.dispatched, vec!["protocol", "protocol"]);
        assert_eq!(transport.calls[1], transport.calls[2]);
    }
}
