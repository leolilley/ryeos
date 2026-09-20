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

    /// Perform the same exact exchange beneath the caller's active lifecycle
    /// deadline. Production transports override this to bind their blocking
    /// I/O timeout to the remaining window. The default preserves existing
    /// deterministic fixtures while still refusing an already-expired call.
    fn exchange_until(
        &mut self,
        canonical_supervisor_frame: &[u8],
        deadline: lillux::time::MonotonicDeadline,
    ) -> std::result::Result<ExternalTransportExchange, ExternalTransportStepFailure> {
        if deadline.has_elapsed() {
            return Err(ExternalTransportStepFailure::AmbiguousTransport(
                anyhow::anyhow!("external transport deadline elapsed before exchange"),
            ));
        }
        self.exchange(canonical_supervisor_frame)
            .map_err(ExternalTransportStepFailure::AmbiguousTransport)
    }
}

/// A failed exchange is ambiguous and can be retried with the exact retained
/// signed bytes. Every other failure is local protocol/runtime failure and
/// must not be retried as though the peer had merely missed a response.
#[derive(Debug)]
pub enum ExternalTransportStepFailure {
    AmbiguousTransport(anyhow::Error),
    Fatal(anyhow::Error),
}

impl std::fmt::Display for ExternalTransportStepFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AmbiguousTransport(error) => {
                write!(formatter, "ambiguous external transport outcome: {error:#}")
            }
            Self::Fatal(error) => write!(formatter, "external transport driver failed: {error:#}"),
        }
    }
}

impl std::error::Error for ExternalTransportStepFailure {}

impl From<anyhow::Error> for ExternalTransportStepFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::Fatal(error)
    }
}

/// Runtime surface needed by the transport driver. The production
/// implementation is the live inherited launcher composition; tests can use a
/// protocol fixture without weakening the native launcher boundary.
pub trait ExternalSupervisorRuntime {
    fn binding(&self) -> &ExecutionChannelBinding;
    fn ready_frame(&self) -> &str;
    fn has_durable_capture(&self) -> Result<bool>;
    fn dispatch_owner_frame(&mut self, wire: &[u8]) -> Result<ExternalOwnerFrameDispatch>;
    fn poll_protocol_output(&mut self) -> Result<Option<String>>;
    fn next_pending_transport_frame_after(&self, after_sequence: u64) -> Result<Option<Vec<u8>>>;
}

impl ExternalSupervisorRuntime for LiveInheritedExternalCandidateSupervisor {
    fn binding(&self) -> &ExecutionChannelBinding {
        self.binding()
    }

    fn ready_frame(&self) -> &str {
        self.ready_frame()
    }

    fn has_durable_capture(&self) -> Result<bool> {
        self.has_durable_capture()
    }

    fn dispatch_owner_frame(&mut self, wire: &[u8]) -> Result<ExternalOwnerFrameDispatch> {
        self.dispatch_owner_frame(wire)
    }

    fn poll_protocol_output(&mut self) -> Result<Option<String>> {
        LiveInheritedExternalCandidateSupervisor::poll_protocol_output(self)
    }

    fn next_pending_transport_frame_after(&self, after_sequence: u64) -> Result<Option<Vec<u8>>> {
        LiveInheritedExternalCandidateSupervisor::next_pending_transport_frame_after(
            self,
            after_sequence,
        )
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
    /// The controller signed `applied` evidence for the exact sealed export
    /// authored and durably retained by this supervisor. Transport receipt or
    /// a merely retained/claimed acknowledgement cannot produce this state.
    ExportApplied,
}

/// Bounded, single-occurrence control loop. It owns neither placement nor
/// cloud lifecycle authority and cannot recreate a lost launcher.
pub struct ExternalCandidateTransportDriver<R, T> {
    runtime: R,
    transport: T,
    pending_supervisor: BTreeMap<u64, Vec<u8>>,
    delivered_supervisor: BTreeMap<u64, String>,
    poll_frame: Option<Vec<u8>>,
    partial_owner_frame: Option<Vec<u8>>,
    revocation_progress: Option<ExternalTransportProgress>,
    sealed_exports: BTreeSet<(u64, String)>,
    capture_retained: bool,
    export_applied: bool,
    maximum_response_frames: usize,
    maximum_response_bytes: usize,
}

impl<R: ExternalSupervisorRuntime, T: ExternalExecutionChannelTransport>
    ExternalCandidateTransportDriver<R, T>
{
    pub fn new(runtime: R, transport: T) -> Result<Self> {
        let capture_retained = runtime.has_durable_capture()?;
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
            delivered_supervisor: BTreeMap::new(),
            poll_frame: None,
            partial_owner_frame: None,
            revocation_progress: None,
            sealed_exports: BTreeSet::new(),
            capture_retained,
            export_applied: false,
            maximum_response_frames,
            maximum_response_bytes,
        })
    }

    pub fn is_revoked(&self) -> bool {
        self.revocation_progress.is_some()
    }

    pub fn is_export_applied(&self) -> bool {
        self.export_applied
    }

    pub fn has_sealed_export(&self) -> bool {
        !self.sealed_exports.is_empty()
    }

    /// The candidate has been quiesced, captured, writer-excluded and retained
    /// locally. This is deliberately independent of how far the resulting
    /// export batch has progressed through transport.
    pub fn has_durable_capture(&self) -> bool {
        self.capture_retained
    }

    pub fn into_parts(self) -> (R, T) {
        (self.runtime, self.transport)
    }

    /// Perform one bounded exchange. On transport failure no local queue state
    /// changes, so the caller can retry and the exact same signed bytes are
    /// presented. The method never sleeps and never converts a timeout into
    /// evidence about remote execution.
    pub fn step(&mut self) -> Result<ExternalTransportProgress> {
        self.step_classified().map_err(anyhow::Error::new)
    }

    pub fn step_classified(
        &mut self,
    ) -> std::result::Result<ExternalTransportProgress, ExternalTransportStepFailure> {
        self.step_classified_until(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(24 * 60 * 60),
        ))
    }

    pub fn step_classified_until(
        &mut self,
        deadline: lillux::time::MonotonicDeadline,
    ) -> std::result::Result<ExternalTransportProgress, ExternalTransportStepFailure> {
        self.enqueue_next_durable_supervisor_frame()?;
        if self.pending_supervisor.is_empty()
            && self.revocation_progress.is_none()
            && !self.export_applied
            && self.sealed_exports.is_empty()
            && let Some(wire) = self.runtime.poll_protocol_output()?
        {
            let verified = SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                self.runtime.binding(),
                lillux::time::timestamp_millis(),
            )?;
            if !(verified.frame().direction == ChannelDirection::SupervisorToOwner
                && matches!(
                    verified.frame().payload,
                    ExecutionChannelPayload::ProtocolBytes { .. }
                        | ExecutionChannelPayload::ProtocolEof
                ))
            {
                return Err(anyhow::anyhow!(
                    "external runtime output poll returned a non-protocol supervisor frame"
                )
                .into());
            }
            self.pending_supervisor
                .insert(verified.frame().sequence, wire.into_bytes());
        }
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
        if sent.frame().direction != ChannelDirection::SupervisorToOwner {
            return Err(
                anyhow::anyhow!("external control loop attempted to send an owner frame").into(),
            );
        }

        // Do not mutate any local frontier before this returns. An error is an
        // ambiguous network outcome and must retain exact-byte retry.
        let response = self.transport.exchange_until(&outgoing, deadline)?;
        self.validate_response(&sent, &response)?;

        self.pending_supervisor.remove(&sent.frame().sequence);
        match self.delivered_supervisor.entry(sent.frame().sequence) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(sent.digest().to_owned());
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                if entry.get() != sent.digest() {
                    return Err(anyhow::anyhow!(
                        "external controller retained a forked supervisor sequence"
                    )
                    .into());
                }
            }
        }
        self.poll_frame = Some(outgoing);

        if let Some(urgent) = response.urgent_revocation_frame.as_ref() {
            return Ok(self.dispatch_urgent_revocation(urgent)?);
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
        if self.export_applied {
            Ok(ExternalTransportProgress::ExportApplied)
        } else if let Some(progress) = self.revocation_progress {
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
        let verified = SignedExecutionFrame::decode_and_verify(
            wire,
            self.runtime.binding(),
            lillux::time::timestamp_millis(),
        )?;
        let dispatched = self.runtime.dispatch_owner_frame(wire)?;
        if matches!(&dispatched.outcome, ExternalOwnerFrameOutcome::Capture(_)) {
            self.capture_retained = true;
        }
        if matches!(
            &dispatched.outcome,
            ExternalOwnerFrameOutcome::Acknowledgement
        ) {
            if let ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence,
                peer_frame_digest,
                application: ryeos_state::external_execution::ExecutionFrameApplication::Applied,
            } = &verified.frame().payload
            {
                if self
                    .sealed_exports
                    .contains(&(*peer_frame_sequence, peer_frame_digest.clone()))
                {
                    self.export_applied = true;
                }
            }
        }
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
            self.enqueue_supervisor_wire(wire)?;
        }
        Ok(())
    }

    fn enqueue_supervisor_wire(&mut self, wire: Vec<u8>) -> Result<()> {
        self.enqueue_supervisor_wire_inner(wire, false)
    }

    /// Merge the durable journal backlog with process-local output. Signed
    /// application/data observations remain at the head until peer evidence
    /// advances the journal. Acknowledgements are different: the controller
    /// intentionally suppresses ack-of-ack, so one whose exact bytes received
    /// a successful HTTP response may be crossed by this process-local cursor.
    /// The journal query still validates every skipped predecessor and the
    /// cursor never crosses an unsent or forked sequence.
    fn enqueue_next_durable_supervisor_frame(&mut self) -> Result<()> {
        let mut after_sequence = 0_u64;
        for _ in 0..self.runtime.binding().max_frames {
            let Some(wire) = self
                .runtime
                .next_pending_transport_frame_after(after_sequence)?
            else {
                return Ok(());
            };
            let verified = SignedExecutionFrame::decode_and_verify(
                &wire,
                self.runtime.binding(),
                lillux::time::timestamp_millis(),
            )?;
            if let Some(delivered_digest) =
                self.delivered_supervisor.get(&verified.frame().sequence)
            {
                ensure!(
                    delivered_digest == verified.digest(),
                    "external journal forked a delivered supervisor sequence"
                );
                if matches!(
                    verified.frame().payload,
                    ExecutionChannelPayload::Acknowledge { .. }
                ) {
                    after_sequence = verified.frame().sequence;
                    continue;
                }
            }
            return self.enqueue_durable_supervisor_wire(wire);
        }
        bail!("external durable transport cursor exceeds the admitted frame bound")
    }

    /// A successful exchange is not a signed cumulative receipt. If the
    /// journal still reports an exact frame as pending, retain it for replay
    /// even when this process observed an earlier HTTP delivery. Only peer
    /// evidence advancing the journal frontier permits a later sequence.
    fn enqueue_durable_supervisor_wire(&mut self, wire: Vec<u8>) -> Result<()> {
        self.enqueue_supervisor_wire_inner(wire, true)
    }

    fn enqueue_supervisor_wire_inner(
        &mut self,
        wire: Vec<u8>,
        retry_if_delivered: bool,
    ) -> Result<()> {
        let verified = SignedExecutionFrame::decode_and_verify(
            &wire,
            self.runtime.binding(),
            lillux::time::timestamp_millis(),
        )?;
        ensure!(
            verified.frame().direction == ChannelDirection::SupervisorToOwner,
            "external dispatcher returned a controller-authored frame"
        );
        if matches!(
            verified.frame().payload,
            ExecutionChannelPayload::ExportSealed { .. }
        ) {
            self.sealed_exports
                .insert((verified.frame().sequence, verified.digest().to_owned()));
        }
        if let Some(delivered_digest) = self.delivered_supervisor.get(&verified.frame().sequence) {
            ensure!(
                delivered_digest == verified.digest(),
                "external dispatcher forked a delivered supervisor sequence"
            );
            if !retry_if_delivered {
                return Ok(());
            }
        }
        match self.pending_supervisor.entry(verified.frame().sequence) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(wire);
            }
            std::collections::btree_map::Entry::Occupied(entry) => ensure!(
                entry.get() == &wire,
                "external dispatcher forked a supervisor sequence"
            ),
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
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Arc;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ryeos_state::external_execution::guest_journal::{
        AuthenticatedLauncherReady, GuestApplicationClaim, LauncherOccurrenceEvidence,
        LiveGuestJournal, PreparedGuestJournal,
    };
    use ryeos_state::external_execution::{
        AuthenticatedExecutionFrame, ExecutionFrame, ExecutionFrameApplication, ExportContentKind,
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
        protocol_stays_partial: bool,
        revoked: bool,
        cancel_uncertain: bool,
        fail_next_dispatch: bool,
        protocol_outputs: VecDeque<Vec<u8>>,
        protocol_output_polls: usize,
        pending_transport: RefCell<VecDeque<Vec<u8>>>,
        last_acknowledgement: Option<(u64, String, ExecutionFrameApplication, String)>,
        capture_retained: bool,
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
                protocol_stays_partial: false,
                revoked: false,
                cancel_uncertain: false,
                fail_next_dispatch: false,
                protocol_outputs: VecDeque::new(),
                protocol_output_polls: 0,
                pending_transport: RefCell::new(VecDeque::new()),
                last_acknowledgement: None,
                capture_retained: false,
            }
        }

        fn acknowledgement(
            &mut self,
            peer: &AuthenticatedExecutionFrame,
            application: ExecutionFrameApplication,
        ) -> String {
            if let Some((sequence, digest, retained_application, wire)) = &self.last_acknowledgement
                && *sequence == peer.frame().sequence
                && digest == peer.digest()
                && *retained_application == application
            {
                return wire.clone();
            }
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
            let wire = signed.canonical().to_owned();
            self.last_acknowledgement = Some((
                peer.frame().sequence,
                peer.digest().to_owned(),
                application,
                wire.clone(),
            ));
            wire
        }
    }

    impl ExternalSupervisorRuntime for FixtureRuntime {
        fn binding(&self) -> &ExecutionChannelBinding {
            &self.binding
        }

        fn ready_frame(&self) -> &str {
            &self.ready
        }

        fn has_durable_capture(&self) -> Result<bool> {
            Ok(self.capture_retained)
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
                    if self.protocol_partial && !self.protocol_stays_partial {
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

        fn poll_protocol_output(&mut self) -> Result<Option<String>> {
            self.protocol_output_polls += 1;
            let Some(bytes) = self.protocol_outputs.pop_front() else {
                return Ok(None);
            };
            let frame = sign(
                &self.binding,
                &self.supervisor_key,
                ChannelDirection::SupervisorToOwner,
                self.next_sequence,
                Some(self.previous_digest.clone()),
                0,
                ExecutionChannelPayload::ProtocolBytes {
                    bytes_base64: STANDARD.encode(bytes),
                },
            );
            self.next_sequence += 1;
            self.previous_digest = frame.digest().to_owned();
            Ok(Some(frame.canonical().to_owned()))
        }

        fn next_pending_transport_frame_after(
            &self,
            _after_sequence: u64,
        ) -> Result<Option<Vec<u8>>> {
            Ok(self.pending_transport.borrow_mut().pop_front())
        }
    }

    struct JournalFixtureRuntime {
        binding: ExecutionChannelBinding,
        supervisor_key: lillux::crypto::SigningKey,
        ready: String,
        journal: LiveGuestJournal,
        owner_predecessor_digest: String,
        protocol_outputs: VecDeque<Vec<u8>>,
        capture_retained: bool,
        _state_root: tempfile::TempDir,
        _journal_root: tempfile::TempDir,
        _authority: ryeos_state::PinnedStateAuthority,
    }

    impl JournalFixtureRuntime {
        fn with_export_batch(
            binding: ExecutionChannelBinding,
            owner_key: &lillux::crypto::SigningKey,
            supervisor_key: lillux::crypto::SigningKey,
        ) -> Self {
            let state_root = tempfile::tempdir().unwrap();
            let state = ryeos_state::StateDb::open(
                state_root.path(),
                Arc::new(ryeos_state::TrustStore::new()),
            )
            .unwrap();
            let authority = state.pinned_authority().unwrap();
            drop(state);
            let journal_root = tempfile::tempdir().unwrap();
            let directory = lillux::PinnedDirectory::open(journal_root.path())
                .unwrap()
                .unwrap();
            directory.tighten_owner_private_directory().unwrap();
            let prepared = PreparedGuestJournal::create(
                directory,
                &authority,
                &"9".repeat(64),
                binding.clone(),
            )
            .unwrap();
            let occurrence = LauncherOccurrenceEvidence::from_held_launcher(
                lillux::ExactProcessIdentity {
                    boot_id: "transport-ordering-boot".into(),
                    target_pid: 101,
                    target_start_time_ticks: 202,
                    group_leader_pid: 101,
                    group_leader_start_time_ticks: 202,
                },
                &"8".repeat(64),
                &"7".repeat(64),
            )
            .unwrap();
            let journal = prepared
                .bind_launcher(occurrence)
                .unwrap()
                .mark_launcher_ready(
                    AuthenticatedLauncherReady::from_handshake_transcript(&"6".repeat(64)).unwrap(),
                )
                .unwrap();
            let ready = journal
                .author_supervisor_frame(
                    &supervisor_key,
                    ExecutionChannelPayload::Ready {
                        supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                        base_snapshot_hash: binding.base_snapshot_hash.clone(),
                    },
                )
                .unwrap();
            let release = sign(
                &binding,
                owner_key,
                ChannelDirection::OwnerToSupervisor,
                1,
                None,
                ready.frame().sequence,
                ExecutionChannelPayload::Release,
            );
            assert!(
                journal
                    .record_frame(release.canonical().as_bytes())
                    .unwrap()
            );
            let GuestApplicationClaim::New(release_token) = journal
                .claim(
                    ChannelDirection::OwnerToSupervisor,
                    release.frame().sequence,
                    release.digest(),
                )
                .unwrap()
            else {
                panic!("release must be newly claimed")
            };
            let (_, release_performed) = journal.apply_once(release_token, |_| Ok(())).unwrap();
            journal.finish(release_performed).unwrap();
            let quiesce = sign(
                &binding,
                owner_key,
                ChannelDirection::OwnerToSupervisor,
                2,
                Some(release.digest().to_owned()),
                ready.frame().sequence,
                ExecutionChannelPayload::Quiesce {
                    completion_request_digest: "2".repeat(64),
                },
            );
            assert!(
                journal
                    .record_frame(quiesce.canonical().as_bytes())
                    .unwrap()
            );
            let GuestApplicationClaim::New(quiesce_token) = journal
                .claim(
                    ChannelDirection::OwnerToSupervisor,
                    quiesce.frame().sequence,
                    quiesce.digest(),
                )
                .unwrap()
            else {
                panic!("quiesce must be newly claimed")
            };
            let (_, quiesce_performed) = journal.apply_once(quiesce_token, |_| Ok(())).unwrap();
            journal.finish(quiesce_performed).unwrap();
            let object_hash = lillux::sha256_hex(b"two durable export chunks");
            let export = journal
                .author_supervisor_frames(
                    &supervisor_key,
                    [
                        ExecutionChannelPayload::ExportObjectChunk {
                            content_kind: ExportContentKind::Blob,
                            object_hash: object_hash.clone(),
                            offset: 0,
                            bytes_base64: STANDARD.encode(b"two durable "),
                            final_chunk: false,
                        },
                        ExecutionChannelPayload::ExportObjectChunk {
                            content_kind: ExportContentKind::Blob,
                            object_hash,
                            offset: 12,
                            bytes_base64: STANDARD.encode(b"export chunks"),
                            final_chunk: true,
                        },
                        ExecutionChannelPayload::ExportSealed {
                            candidate_snapshot_hash: "1".repeat(64),
                            completion_request_digest: "2".repeat(64),
                            writer_exclusion_evidence_hash: "3".repeat(64),
                        },
                    ],
                )
                .unwrap();
            assert_eq!(export.len(), 3);
            Self {
                binding,
                supervisor_key,
                ready: ready.canonical().to_owned(),
                journal,
                owner_predecessor_digest: quiesce.digest().to_owned(),
                protocol_outputs: VecDeque::new(),
                capture_retained: true,
                _state_root: state_root,
                _journal_root: journal_root,
                _authority: authority,
            }
        }

        fn running(
            binding: ExecutionChannelBinding,
            owner_key: &lillux::crypto::SigningKey,
            supervisor_key: lillux::crypto::SigningKey,
            output: Vec<u8>,
        ) -> Self {
            let state_root = tempfile::tempdir().unwrap();
            let state = ryeos_state::StateDb::open(
                state_root.path(),
                Arc::new(ryeos_state::TrustStore::new()),
            )
            .unwrap();
            let authority = state.pinned_authority().unwrap();
            drop(state);
            let journal_root = tempfile::tempdir().unwrap();
            let directory = lillux::PinnedDirectory::open(journal_root.path())
                .unwrap()
                .unwrap();
            directory.tighten_owner_private_directory().unwrap();
            let prepared = PreparedGuestJournal::create(
                directory,
                &authority,
                &"5".repeat(64),
                binding.clone(),
            )
            .unwrap();
            let occurrence = LauncherOccurrenceEvidence::from_held_launcher(
                lillux::ExactProcessIdentity {
                    boot_id: "transport-output-boot".into(),
                    target_pid: 303,
                    target_start_time_ticks: 404,
                    group_leader_pid: 303,
                    group_leader_start_time_ticks: 404,
                },
                &"4".repeat(64),
                &"3".repeat(64),
            )
            .unwrap();
            let journal = prepared
                .bind_launcher(occurrence)
                .unwrap()
                .mark_launcher_ready(
                    AuthenticatedLauncherReady::from_handshake_transcript(&"2".repeat(64)).unwrap(),
                )
                .unwrap();
            let ready = journal
                .author_supervisor_frame(
                    &supervisor_key,
                    ExecutionChannelPayload::Ready {
                        supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                        base_snapshot_hash: binding.base_snapshot_hash.clone(),
                    },
                )
                .unwrap();
            let release = sign(
                &binding,
                owner_key,
                ChannelDirection::OwnerToSupervisor,
                1,
                None,
                ready.frame().sequence,
                ExecutionChannelPayload::Release,
            );
            assert!(
                journal
                    .record_frame(release.canonical().as_bytes())
                    .unwrap()
            );
            let GuestApplicationClaim::New(release_token) = journal
                .claim(
                    ChannelDirection::OwnerToSupervisor,
                    release.frame().sequence,
                    release.digest(),
                )
                .unwrap()
            else {
                panic!("release must be newly claimed")
            };
            let (_, release_performed) = journal.apply_once(release_token, |_| Ok(())).unwrap();
            journal.finish(release_performed).unwrap();
            Self {
                binding,
                supervisor_key,
                ready: ready.canonical().to_owned(),
                journal,
                owner_predecessor_digest: release.digest().to_owned(),
                protocol_outputs: VecDeque::from([output]),
                capture_retained: false,
                _state_root: state_root,
                _journal_root: journal_root,
                _authority: authority,
            }
        }
    }

    impl ExternalSupervisorRuntime for JournalFixtureRuntime {
        fn binding(&self) -> &ExecutionChannelBinding {
            &self.binding
        }

        fn ready_frame(&self) -> &str {
            &self.ready
        }

        fn has_durable_capture(&self) -> Result<bool> {
            Ok(self.capture_retained)
        }

        fn dispatch_owner_frame(&mut self, wire: &[u8]) -> Result<ExternalOwnerFrameDispatch> {
            let peer = SignedExecutionFrame::decode_and_verify(
                wire,
                &self.binding,
                lillux::time::timestamp_millis(),
            )?;
            ensure!(
                peer.frame().direction == ChannelDirection::OwnerToSupervisor
                    && matches!(
                        peer.frame().payload,
                        ExecutionChannelPayload::Acknowledge { .. }
                    ),
                "journal fixture admits only controller acknowledgements"
            );
            self.journal.record_owner_acknowledgement(wire)?;
            let acknowledgement = self
                .journal
                .ensure_supervisor_acknowledgement(
                    &self.supervisor_key,
                    peer.frame().sequence,
                    peer.digest(),
                )?
                .context("journal fixture did not author its exact acknowledgement")?;
            Ok(ExternalOwnerFrameDispatch {
                outcome: ExternalOwnerFrameOutcome::Acknowledgement,
                acknowledgement_frame: acknowledgement.canonical().to_owned(),
            })
        }

        fn poll_protocol_output(&mut self) -> Result<Option<String>> {
            let Some(bytes) = self.protocol_outputs.pop_front() else {
                return Ok(None);
            };
            Ok(Some(
                self.journal
                    .author_supervisor_frame(
                        &self.supervisor_key,
                        ExecutionChannelPayload::ProtocolBytes {
                            bytes_base64: STANDARD.encode(bytes),
                        },
                    )?
                    .canonical()
                    .to_owned(),
            ))
        }

        fn next_pending_transport_frame_after(
            &self,
            after_sequence: u64,
        ) -> Result<Option<Vec<u8>>> {
            Ok(self
                .journal
                .pending_supervisor_transport_frames_after(
                    after_sequence,
                    1,
                    ryeos_state::external_execution::MAX_FRAME_BYTES,
                )?
                .into_iter()
                .next()
                .map(|frame| frame.wire().to_vec()))
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

    struct AppliedAckTransport {
        binding: ExecutionChannelBinding,
        owner_key: lillux::crypto::SigningKey,
        next_owner_sequence: u64,
        previous_owner_digest: Option<String>,
        cached: BTreeMap<u64, Vec<u8>>,
        fail_once_at_supervisor_sequence: Option<u64>,
        omit_ack_once_at_supervisor_sequence: Option<u64>,
        suppress_ack_of_ack: bool,
        calls: Vec<Vec<u8>>,
    }

    impl AppliedAckTransport {
        fn new(
            binding: ExecutionChannelBinding,
            owner_key: lillux::crypto::SigningKey,
            next_owner_sequence: u64,
            previous_owner_digest: Option<String>,
            fail_once_at_supervisor_sequence: u64,
        ) -> Self {
            Self {
                binding,
                owner_key,
                next_owner_sequence,
                previous_owner_digest,
                cached: BTreeMap::new(),
                fail_once_at_supervisor_sequence: Some(fail_once_at_supervisor_sequence),
                omit_ack_once_at_supervisor_sequence: None,
                suppress_ack_of_ack: false,
                calls: Vec::new(),
            }
        }
    }

    impl ExternalExecutionChannelTransport for AppliedAckTransport {
        fn exchange(
            &mut self,
            canonical_supervisor_frame: &[u8],
        ) -> Result<ExternalTransportExchange> {
            self.calls.push(canonical_supervisor_frame.to_vec());
            let sent = SignedExecutionFrame::decode_and_verify(
                canonical_supervisor_frame,
                &self.binding,
                lillux::time::timestamp_millis(),
            )?;
            if self.suppress_ack_of_ack
                && matches!(
                    sent.frame().payload,
                    ExecutionChannelPayload::Acknowledge { .. }
                )
            {
                return Ok(response(&sent, vec![], None));
            }
            if self.omit_ack_once_at_supervisor_sequence == Some(sent.frame().sequence) {
                self.omit_ack_once_at_supervisor_sequence = None;
                return Ok(response(&sent, vec![], None));
            }
            let owner_ack = if let Some(cached) = self.cached.get(&sent.frame().sequence) {
                SignedExecutionFrame::decode_and_verify(
                    cached,
                    &self.binding,
                    lillux::time::timestamp_millis(),
                )?
            } else {
                let acknowledgement = sign(
                    &self.binding,
                    &self.owner_key,
                    ChannelDirection::OwnerToSupervisor,
                    self.next_owner_sequence,
                    self.previous_owner_digest.clone(),
                    sent.frame().sequence,
                    ExecutionChannelPayload::Acknowledge {
                        peer_frame_sequence: sent.frame().sequence,
                        peer_frame_digest: sent.digest().to_owned(),
                        application: ExecutionFrameApplication::Applied,
                    },
                );
                self.next_owner_sequence += 1;
                self.previous_owner_digest = Some(acknowledgement.digest().to_owned());
                self.cached.insert(
                    sent.frame().sequence,
                    acknowledgement.canonical().as_bytes().to_vec(),
                );
                acknowledgement
            };
            if self.fail_once_at_supervisor_sequence == Some(sent.frame().sequence) {
                self.fail_once_at_supervisor_sequence = None;
                bail!("ambiguous response after controller applied export chunk")
            }
            Ok(response(&sent, vec![envelope(&owner_ack)], None))
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
            schema: 3,
            placement_thread_id: "T-external-transport".into(),
            allocation_request_digest: "a".repeat(64),
            occurrence_id: "occurrence-one".into(),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            candidate_program_digest: "0".repeat(64),
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
            candidate_export_max_bytes: 512 * 1024,
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
        assert!(matches!(
            driver.step_classified(),
            Err(ExternalTransportStepFailure::AmbiguousTransport(_))
        ));
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
    fn candidate_protocol_output_is_signed_and_retried_without_a_second_read() {
        let (binding, _owner, supervisor) = fixture();
        let runtime = FixtureRuntime::new(binding.clone(), supervisor);
        let ready = SignedExecutionFrame::decode_and_verify(
            runtime.ready.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let transport = FixtureTransport::new(vec![TransportAction::Respond(response(
            &ready,
            vec![],
            None,
        ))]);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        driver
            .runtime
            .protocol_outputs
            .push_back(b"exec-server-response".to_vec());
        driver
            .runtime
            .protocol_outputs
            .push_back(b"later-response".to_vec());
        driver.transport.actions.push_back(TransportAction::Fail);

        assert!(matches!(
            driver.step_classified(),
            Err(ExternalTransportStepFailure::AmbiguousTransport(_))
        ));
        assert_eq!(driver.runtime.protocol_output_polls, 1);
        let output = SignedExecutionFrame::decode_and_verify(
            &driver.transport.calls[1],
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        assert_eq!(
            output.protocol_bytes().unwrap(),
            b"exec-server-response".to_vec()
        );
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(&output, vec![], None)));
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert_eq!(driver.transport.calls[1], driver.transport.calls[2]);
        assert_eq!(driver.runtime.protocol_output_polls, 1);
        assert_eq!(
            driver.runtime.protocol_outputs.front().unwrap(),
            b"later-response"
        );
    }

    #[test]
    fn candidate_output_progresses_while_protocol_input_remains_partial() {
        let (binding, owner, supervisor) = fixture();
        let mut runtime = FixtureRuntime::new(binding.clone(), supervisor);
        runtime.protocol_stays_partial = true;
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
                bytes_base64: STANDARD.encode(b"request larger than pipe capacity"),
            },
        );
        let transport = FixtureTransport::new(vec![TransportAction::Respond(response(
            &ready,
            vec![envelope(&protocol)],
            None,
        ))]);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        let claimed = SignedExecutionFrame::decode_and_verify(
            driver.pending_supervisor.first_key_value().unwrap().1,
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        driver
            .runtime
            .protocol_outputs
            .push_back(b"response needed before more input".to_vec());
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(&claimed, vec![], None)));
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert!(driver.pending_supervisor.is_empty());

        let expected_output = sign(
            &binding,
            &driver.runtime.supervisor_key,
            ChannelDirection::SupervisorToOwner,
            driver.runtime.next_sequence,
            Some(driver.runtime.previous_digest.clone()),
            0,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"response needed before more input"),
            },
        );
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(
                &expected_output,
                vec![],
                None,
            )));
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert_eq!(driver.runtime.protocol_output_polls, 1);
        let sent = SignedExecutionFrame::decode_and_verify(
            driver.transport.calls.last().unwrap(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        assert_eq!(
            sent.protocol_bytes().unwrap(),
            b"response needed before more input".to_vec()
        );
    }

    #[test]
    fn only_applied_acknowledgement_of_exact_sealed_export_is_success_terminal() {
        let (binding, owner, supervisor) = fixture();
        let runtime = FixtureRuntime::new(binding.clone(), supervisor.clone());
        let ready = SignedExecutionFrame::decode_and_verify(
            runtime.ready.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let transport = FixtureTransport::new(vec![TransportAction::Respond(response(
            &ready,
            vec![],
            None,
        ))]);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);

        let export = sign(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready.digest().to_owned()),
            0,
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash: "1".repeat(64),
                completion_request_digest: "2".repeat(64),
                writer_exclusion_evidence_hash: "3".repeat(64),
            },
        );
        driver
            .enqueue_supervisor_wire(export.canonical().as_bytes().to_vec())
            .unwrap();
        driver.runtime.next_sequence = 3;
        driver.runtime.previous_digest = export.digest().to_owned();

        let claimed = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            export.frame().sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: export.frame().sequence,
                peer_frame_digest: export.digest().to_owned(),
                application: ExecutionFrameApplication::Claimed,
            },
        );
        driver
            .dispatch_ordinary(claimed.canonical().as_bytes())
            .unwrap();
        assert!(!driver.is_export_applied());

        let unrelated = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(claimed.digest().to_owned()),
            ready.frame().sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: ready.frame().sequence,
                peer_frame_digest: ready.digest().to_owned(),
                application: ExecutionFrameApplication::Applied,
            },
        );
        driver
            .dispatch_ordinary(unrelated.canonical().as_bytes())
            .unwrap();
        assert!(!driver.is_export_applied());

        let wrong_digest = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            3,
            Some(unrelated.digest().to_owned()),
            export.frame().sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: export.frame().sequence,
                peer_frame_digest: "9".repeat(64),
                application: ExecutionFrameApplication::Applied,
            },
        );
        driver
            .dispatch_ordinary(wrong_digest.canonical().as_bytes())
            .unwrap();
        assert!(!driver.is_export_applied());

        let applied = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            4,
            Some(wrong_digest.digest().to_owned()),
            export.frame().sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: export.frame().sequence,
                peer_frame_digest: export.digest().to_owned(),
                application: ExecutionFrameApplication::Applied,
            },
        );
        driver
            .dispatch_ordinary(applied.canonical().as_bytes())
            .unwrap();
        assert!(driver.is_export_applied());
    }

    #[test]
    fn journal_recovered_sealed_export_retains_exact_terminal_identity() {
        let (binding, owner, supervisor) = fixture();
        let runtime = FixtureRuntime::new(binding.clone(), supervisor.clone());
        let ready = SignedExecutionFrame::decode_and_verify(
            runtime.ready.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let transport = FixtureTransport::new(vec![TransportAction::Respond(response(
            &ready,
            vec![],
            None,
        ))]);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);

        let sealed = sign(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready.digest().to_owned()),
            0,
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash: "1".repeat(64),
                completion_request_digest: "2".repeat(64),
                writer_exclusion_evidence_hash: "3".repeat(64),
            },
        );
        driver
            .runtime
            .pending_transport
            .borrow_mut()
            .push_back(sealed.canonical().as_bytes().to_vec());
        driver.runtime.next_sequence = 3;
        driver.runtime.previous_digest = sealed.digest().to_owned();
        let applied = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            sealed.frame().sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: sealed.frame().sequence,
                peer_frame_digest: sealed.digest().to_owned(),
                application: ExecutionFrameApplication::Applied,
            },
        );
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(
                &sealed,
                vec![envelope(&applied)],
                None,
            )));

        assert_eq!(
            driver.step().unwrap(),
            ExternalTransportProgress::ExportApplied
        );
        assert!(driver.is_export_applied());
    }

    #[test]
    fn real_journal_export_prefix_precedes_later_acks_and_retries_exact_chunk() {
        let (binding, owner, supervisor) = fixture();
        let runtime = JournalFixtureRuntime::with_export_batch(binding.clone(), &owner, supervisor);
        let owner_predecessor_digest = runtime.owner_predecessor_digest.clone();
        let transport =
            AppliedAckTransport::new(binding.clone(), owner, 3, Some(owner_predecessor_digest), 2);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();

        assert!(driver.has_durable_capture());
        assert!(
            !driver.has_sealed_export(),
            "durable capture must not depend on transport reaching the seal"
        );

        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert!(driver.step().is_err(), "first chunk response is ambiguous");
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert_eq!(
            driver.step().unwrap(),
            ExternalTransportProgress::ExportApplied
        );

        let (_runtime, transport) = driver.into_parts();
        let sent = transport
            .calls
            .iter()
            .map(|wire| {
                SignedExecutionFrame::decode_and_verify(wire, &binding, binding.issued_at_ms)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sent.iter()
                .map(|frame| frame.frame().sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 2, 3, 4],
            "later acknowledgement frames must not overtake the durable export prefix"
        );
        assert_eq!(transport.calls[1], transport.calls[2]);
        assert!(matches!(
            sent[1].frame().payload,
            ExecutionChannelPayload::ExportObjectChunk {
                offset: 0,
                final_chunk: false,
                ..
            }
        ));
        assert!(matches!(
            sent[3].frame().payload,
            ExecutionChannelPayload::ExportObjectChunk {
                offset: 12,
                final_chunk: true,
                ..
            }
        ));
        assert!(matches!(
            sent[4].frame().payload,
            ExecutionChannelPayload::ExportSealed { .. }
        ));
    }

    #[test]
    fn journal_pending_chunk_replays_until_delayed_signed_receipt_advances_frontier() {
        let (binding, owner, supervisor) = fixture();
        let runtime = JournalFixtureRuntime::with_export_batch(binding.clone(), &owner, supervisor);
        let owner_predecessor_digest = runtime.owner_predecessor_digest.clone();
        let mut transport =
            AppliedAckTransport::new(binding.clone(), owner, 3, Some(owner_predecessor_digest), 0);
        transport.omit_ack_once_at_supervisor_sequence = Some(2);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();

        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert_eq!(
            driver.step().unwrap(),
            ExternalTransportProgress::Advanced,
            "HTTP delivery without a signed receipt cannot advance durable order"
        );
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert_eq!(
            driver.step().unwrap(),
            ExternalTransportProgress::ExportApplied
        );

        let (_runtime, transport) = driver.into_parts();
        let sequences = transport
            .calls
            .iter()
            .map(|wire| {
                SignedExecutionFrame::decode_and_verify(wire, &binding, binding.issued_at_ms)
                    .unwrap()
                    .frame()
                    .sequence
            })
            .collect::<Vec<_>>();
        assert_eq!(sequences, vec![1, 2, 2, 3, 4]);
        assert_eq!(transport.calls[1], transport.calls[2]);
    }

    #[test]
    fn delivered_ack_without_ack_of_ack_does_not_starve_candidate_output() {
        let (binding, owner, supervisor) = fixture();
        let expected_output = b"candidate response after applied input".to_vec();
        let runtime = JournalFixtureRuntime::running(
            binding.clone(),
            &owner,
            supervisor,
            expected_output.clone(),
        );
        let owner_predecessor_digest = runtime.owner_predecessor_digest.clone();
        let mut transport =
            AppliedAckTransport::new(binding.clone(), owner, 2, Some(owner_predecessor_digest), 0);
        transport.suppress_ack_of_ack = true;
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();

        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);

        let (runtime, transport) = driver.into_parts();
        assert!(runtime.protocol_outputs.is_empty());
        let sent = transport
            .calls
            .iter()
            .map(|wire| {
                SignedExecutionFrame::decode_and_verify(wire, &binding, binding.issued_at_ms)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sent.iter()
                .map(|frame| frame.frame().sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(matches!(
            sent[1].frame().payload,
            ExecutionChannelPayload::Acknowledge { .. }
        ));
        assert_eq!(sent[2].protocol_bytes().unwrap(), expected_output);
    }

    #[test]
    fn sealed_export_wait_does_not_poll_a_reaped_candidate() {
        let (binding, owner, supervisor) = fixture();
        let runtime = FixtureRuntime::new(binding.clone(), supervisor.clone());
        let ready = SignedExecutionFrame::decode_and_verify(
            runtime.ready.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let transport = FixtureTransport::new(vec![TransportAction::Respond(response(
            &ready,
            vec![],
            None,
        ))]);
        let mut driver = ExternalCandidateTransportDriver::new(runtime, transport).unwrap();
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        let export = sign(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready.digest().to_owned()),
            0,
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash: "1".repeat(64),
                completion_request_digest: "2".repeat(64),
                writer_exclusion_evidence_hash: "3".repeat(64),
            },
        );
        driver
            .enqueue_supervisor_wire(export.canonical().as_bytes().to_vec())
            .unwrap();
        driver.runtime.next_sequence = 3;
        driver.runtime.previous_digest = export.digest().to_owned();
        driver
            .runtime
            .protocol_outputs
            .push_back(b"must-not-be-read-after-capture".to_vec());
        let retained = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            export.frame().sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: export.frame().sequence,
                peer_frame_digest: export.digest().to_owned(),
                application: ExecutionFrameApplication::Retained,
            },
        );
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(
                &export,
                vec![envelope(&retained)],
                None,
            )));
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);

        let acknowledgement = SignedExecutionFrame::decode_and_verify(
            driver.pending_supervisor.first_key_value().unwrap().1,
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(
                &acknowledgement,
                vec![],
                None,
            )));
        assert_eq!(driver.step().unwrap(), ExternalTransportProgress::Advanced);
        assert!(driver.pending_supervisor.is_empty());
        assert_eq!(driver.runtime.protocol_output_polls, 0);

        let applied = sign(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(retained.digest().to_owned()),
            export.frame().sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: export.frame().sequence,
                peer_frame_digest: export.digest().to_owned(),
                application: ExecutionFrameApplication::Applied,
            },
        );
        driver
            .transport
            .actions
            .push_back(TransportAction::Respond(response(
                &acknowledgement,
                vec![envelope(&applied)],
                None,
            )));
        assert_eq!(
            driver.step().unwrap(),
            ExternalTransportProgress::ExportApplied
        );
        assert_eq!(driver.runtime.protocol_output_polls, 0);
        assert_eq!(driver.runtime.protocol_outputs.len(), 1);
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
        assert!(matches!(
            driver.step_classified(),
            Err(ExternalTransportStepFailure::Fatal(_))
        ));
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
