//! The readiness gate in front of a remote lease's channel exchange.
//!
//! Workload: a remote lease whose guest has not yet been admitted. Until its
//! lifecycle adapter judges the guest, the guest's first two frames, Ready
//! then one capability report, are only held in the lease record, never
//! entered in the channel journal, and the guest is told nothing (a held reply
//! carries no acknowledgement and no outbound frame, which the guest reads as
//! idle). When the report arrives the adapter is asked once; on an admitted
//! verdict the held Ready enters the journal and the lease is marked admitted
//! in one transaction, and the report then takes the ordinary exchange. A
//! terminate verdict, a verdict that is no verdict, or a guest that sends
//! anything but exactly those two frames first, refuses the lease durably:
//! the record is marked refused and the allocation cancelled in one
//! transaction, and the cleanup that terminates the lease follows. A failure
//! to obtain any verdict refuses nothing; the guest's retry of the same report
//! asks again, and the activation deadline bounds the whole wait.

use super::*;
use crate::external_lifecycle_adapter::lease_ready::{self, LeaseReadyVerdict};
use crate::runtime_db::external_execution::{
    ExternalLeaseAdmission, ExternalLeaseRecord, ExternalLeaseRecordState,
};
use ryeos_state::external_execution::{ExecutionChannelBinding, ExecutionChannelPayload};

/// The longest one readiness judgement may take, however late the lease's
/// ready deadline is.
const MAX_READINESS_JUDGEMENT: lillux::time::Duration = lillux::time::Duration::from_secs(10);

/// The context carried by an exchange failure that produced no verdict about
/// the lease: the adapter could not be asked or did not answer in time, or
/// durable state failed transiently. The lease is neither admitted nor
/// refused, so the guest's retry of the same frame asks again. A refusal, by
/// contrast, never carries this context.
#[derive(Debug, Clone, Copy)]
pub struct ExternalChannelExchangeUnavailable;

impl std::fmt::Display for ExternalChannelExchangeUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("external channel exchange has no verdict yet; retry the frame")
    }
}

impl ExternalChannelExchangeUnavailable {
    /// Whether `error` is an exchange failure that left the lease undecided.
    pub fn marks(error: &anyhow::Error) -> bool {
        error.downcast_ref::<Self>().is_some()
    }
}

/// Mark a failure to reach a verdict as retryable.
fn no_verdict<T>(outcome: Result<T>) -> Result<T> {
    outcome.map_err(|error| error.context(ExternalChannelExchangeUnavailable))
}

/// What the gate decided for one frame.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LeaseReadyGate {
    /// The frame takes the ordinary exchange.
    Proceed,
    /// The frame is held; the guest gets an idle reply.
    Held,
}

/// What a frame is, as far as the gate cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Ready,
    CapabilityReport,
    Other,
}

impl FrameKind {
    fn of(payload: &ExecutionChannelPayload) -> Self {
        match payload {
            ExecutionChannelPayload::Ready { .. } => Self::Ready,
            ExecutionChannelPayload::CapabilityReport { .. } => Self::CapabilityReport,
            _ => Self::Other,
        }
    }
}

/// The effects the gate has on durable state and the adapter, so the decision
/// flow is exercised apart from them.
trait GatePorts {
    fn hold_ready(&self, wire: &[u8]) -> Result<()>;
    fn hold_report(&self, wire: &[u8]) -> Result<()>;
    fn record(&self) -> Result<ExternalLeaseRecord>;
    /// Ask the adapter; returns its raw verdict bytes.
    fn judge(&self, record: &ExternalLeaseRecord) -> Result<Vec<u8>>;
    fn admit(&self, admission: &ExternalLeaseAdmission) -> Result<()>;
    fn refuse(&self, reason: &str) -> Result<()>;
    fn adapter_id(&self) -> &str;
}

/// Decide one frame of a lease in `record`.
fn drive(
    ports: &dyn GatePorts,
    record: &ExternalLeaseRecord,
    sequence: u64,
    kind: FrameKind,
    wire: &[u8],
) -> Result<LeaseReadyGate> {
    match record.state {
        ExternalLeaseRecordState::Admitted => return Ok(LeaseReadyGate::Proceed),
        ExternalLeaseRecordState::Refused => bail!(
            "external channel refused: the lease was refused: {}",
            record.refusal_reason.as_deref().unwrap_or("no reason recorded")
        ),
        ExternalLeaseRecordState::Reserved => bail!(
            "external channel refused: the lease is not activated, so it accepts no frame"
        ),
        ExternalLeaseRecordState::Activated => {}
    }
    let refuse = |reason: String| -> Result<LeaseReadyGate> {
        let outcome = ports.refuse(&reason);
        match outcome {
            Ok(()) => bail!("external channel refused: the lease was refused: {reason}"),
            Err(error) => Err(error.context(format!(
                "external channel refused: the lease was refused: {reason}; \
                 recording the refusal failed"
            ))),
        }
    };
    match (sequence, kind) {
        (1, FrameKind::Ready) => {
            if record.held_ready_frame.as_deref().is_some_and(|held| held != wire) {
                return refuse("the guest sent a second, different Ready".into());
            }
            no_verdict(ports.hold_ready(wire))?;
            Ok(LeaseReadyGate::Held)
        }
        (2, FrameKind::CapabilityReport) => {
            if record.held_ready_frame.is_none() {
                return refuse("the guest sent its capability report before Ready".into());
            }
            if record.held_report_frame.as_deref().is_some_and(|held| held != wire) {
                return refuse("the guest sent a second, different capability report".into());
            }
            no_verdict(ports.hold_report(wire))?;
            let record = no_verdict(ports.record())?;
            let response = no_verdict(ports.judge(&record))?;
            match lease_ready::read_verdict(&response, &record, ports.adapter_id()) {
                Ok(LeaseReadyVerdict::Admitted(admission)) => {
                    no_verdict(ports.admit(&admission))?;
                    Ok(LeaseReadyGate::Proceed)
                }
                Ok(LeaseReadyVerdict::Terminate { reason }) => {
                    refuse(format!("lease readiness refused by the adapter: {reason}"))
                }
                Err(error) => refuse(format!("lease readiness verdict rejected: {error:#}")),
            }
        }
        (1 | 2, _) => refuse(format!(
            "the guest's frame {sequence} is not the {} the lease waits for",
            if sequence == 1 { "Ready" } else { "capability report" }
        )),
        _ => refuse(format!(
            "the guest sent frame {sequence} before its lease was admitted"
        )),
    }
}

/// The production ports: the state store, the installed backend and cleanup.
struct StatePorts<'a> {
    state: &'a AppState,
    allocation: &'a ExternalAllocationRecord,
    binding: &'a ExecutionChannelBinding,
    backend: Arc<dyn ExternalPlacementBackend>,
}

impl GatePorts for StatePorts<'_> {
    fn hold_ready(&self, wire: &[u8]) -> Result<()> {
        self.state
            .state_store
            .hold_external_lease_ready_frame(&self.allocation.reservation.placement_thread_id, wire)
    }

    fn hold_report(&self, wire: &[u8]) -> Result<()> {
        self.state
            .state_store
            .hold_external_lease_report_frame(&self.allocation.reservation.placement_thread_id, wire)
    }

    fn record(&self) -> Result<ExternalLeaseRecord> {
        self.state
            .state_store
            .external_lease_record(&self.allocation.reservation.placement_thread_id)?
            .context("the lease record disappeared while its readiness was judged")
    }

    fn judge(&self, record: &ExternalLeaseRecord) -> Result<Vec<u8>> {
        let admitted = self
            .allocation
            .reservation
            .admitted_adapter_request()?
            .context("a lease record's allocation holds no admitted lease request")?;
        let request = lease_ready::canonical_request(
            self.backend.backend_id(),
            &admitted,
            record,
            self.binding,
        )?;
        let activation = record
            .activation
            .as_ref()
            .context("an activated lease record carries its activation")?;
        let now_ms = i64::try_from(lillux::time::timestamp_millis())?;
        let remaining_ms = u64::try_from(activation.ready_deadline_ms.saturating_sub(now_ms))?;
        ensure!(
            remaining_ms > 0,
            "the lease's ready deadline elapsed before its readiness was judged"
        );
        let deadline = lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_millis(remaining_ms).min(MAX_READINESS_JUDGEMENT),
        );
        self.backend.await_lease_ready(
            &request,
            record
                .held_ready_frame
                .as_deref()
                .context("the lease record holds no Ready frame")?,
            record
                .held_report_frame
                .as_deref()
                .context("the lease record holds no capability report")?,
            deadline,
        )
    }

    fn admit(&self, admission: &ExternalLeaseAdmission) -> Result<()> {
        self.state.state_store.admit_external_lease_with_ready(
            &self.allocation.reservation.placement_thread_id,
            admission,
        )
    }

    fn refuse(&self, reason: &str) -> Result<()> {
        refuse_lease(
            self.state,
            &self.allocation.reservation.placement_thread_id,
            reason,
        )
    }

    fn adapter_id(&self) -> &str {
        self.backend.backend_id()
    }
}

/// Refuse a lease durably and start the cleanup that terminates it. The
/// refusal and the allocation's cancellation are one transaction; the owner
/// revocation, which needs the protected channel signer, follows. A failure of
/// that cleanup leaves the refusal recorded and is reported to the caller.
pub(super) fn refuse_lease(state: &AppState, placement: &str, reason: &str) -> Result<()> {
    state
        .state_store
        .refuse_external_lease_and_cancel(placement, &bounded_refusal(reason))?;
    request_external_candidate_cleanup(state, placement)
}

/// Refuse a lease like `refuse_lease`, but only if it is still activated when
/// the refusal commits. A lease admitted since the caller listed it is left
/// alone and no cleanup starts.
pub(super) fn refuse_activated_lease(state: &AppState, placement: &str, reason: &str) -> Result<()> {
    if state
        .state_store
        .refuse_activated_external_lease_and_cancel(placement, &bounded_refusal(reason))?
    {
        request_external_candidate_cleanup(state, placement)?;
    }
    Ok(())
}

fn bounded_refusal(reason: &str) -> String {
    let mut stored = String::from(reason);
    if stored.len() > MAX_STORED_REFUSAL_BYTES {
        let mut end = MAX_STORED_REFUSAL_BYTES;
        while !stored.is_char_boundary(end) {
            end -= 1;
        }
        stored.truncate(end);
    }
    stored
}

/// The longest refusal reason stored, in bytes; the record's own bound is
/// 1024.
const MAX_STORED_REFUSAL_BYTES: usize = 1000;

/// Gate one authenticated supervisor frame of `allocation`'s channel. A
/// placement without a lease record has no lease to admit and its frames
/// proceed.
pub(super) fn admit_frame(
    state: &AppState,
    allocation: &ExternalAllocationRecord,
    binding: &ExecutionChannelBinding,
    frame: &ryeos_state::external_execution::ExecutionFrame,
    wire: &[u8],
) -> Result<LeaseReadyGate> {
    let placement = &allocation.reservation.placement_thread_id;
    let current = || -> Result<Option<ExternalLeaseRecord>> {
        state.state_store.external_lease_record(placement)
    };
    let Some(record) = no_verdict(current())? else {
        return Ok(LeaseReadyGate::Proceed);
    };
    if record.state == ExternalLeaseRecordState::Admitted {
        return Ok(LeaseReadyGate::Proceed);
    }
    // Hold, judge and admit of one placement run one at a time, so two
    // concurrent exchanges of the same report ask the adapter once: the one
    // that waits re-reads the record and finds the winner's outcome.
    serialised_per_placement(placement, || {
        let Some(record) = no_verdict(current())? else {
            return Ok(LeaseReadyGate::Proceed);
        };
        let retained = no_verdict(
            state
                .state_store
                .retained_external_binding(&allocation.reservation.binding_hash)
                .and_then(|retained| {
                    retained.context("lease readiness lost its retained signed binding")
                }),
        )?;
        let backend = no_verdict(
            state
                .external_placement_backends
                .installed(&retained.backend_contract()),
        )?;
        let ports = StatePorts {
            state,
            allocation,
            binding,
            backend,
        };
        drive(
            &ports,
            &record,
            frame.sequence,
            FrameKind::of(&frame.payload),
            wire,
        )
    })
}

/// The placements whose lease gate is running, each with the lock that
/// serialises it.
static PLACEMENT_GATES: Mutex<BTreeMap<String, Arc<Mutex<()>>>> = Mutex::new(BTreeMap::new());

/// Run `body` while holding the placement's gate lock. The caller is a
/// blocking path and `body` never awaits. The lock guards no data, so a
/// poisoned lock is still a lock.
fn serialised_per_placement<T>(placement: &str, body: impl FnOnce() -> T) -> T {
    let gate = {
        let mut gates = PLACEMENT_GATES.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(gates.entry(placement.to_owned()).or_default())
    };
    let outcome = {
        let _held = gate.lock().unwrap_or_else(|e| e.into_inner());
        body()
    };
    let mut gates = PLACEMENT_GATES.lock().unwrap_or_else(|e| e.into_inner());
    // The map and this call hold the only two references once no other
    // exchange waits on the placement.
    if Arc::strong_count(&gate) == 2 {
        gates.remove(placement);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_lifecycle_adapter::lease_ready::LEASE_READY_SCHEMA;
    use crate::runtime_db::external_execution::ExternalLeaseActivation;
    use std::cell::RefCell;

    struct FakePorts {
        record: RefCell<ExternalLeaseRecord>,
        verdict: RefCell<Result<Vec<u8>, String>>,
        judged: RefCell<u32>,
        refused: RefCell<Vec<String>>,
        admitted: RefCell<Vec<ExternalLeaseAdmission>>,
    }

    impl FakePorts {
        fn new() -> Self {
            Self {
                record: RefCell::new(ExternalLeaseRecord {
                    placement_thread_id: "T-lease".into(),
                    admission_request_sha256: "a".repeat(64),
                    activation_span_ms: 1_000,
                    state: ExternalLeaseRecordState::Activated,
                    activation: Some(ExternalLeaseActivation {
                        lease_id: "lease-1".into(),
                        activation_request_digest: "2".repeat(64),
                        signed_import_sha256: "3".repeat(64),
                        guest_package_sha256: "4".repeat(64),
                        ready_deadline_ms: 5_000,
                    }),
                    held_ready_frame: None,
                    held_report_frame: None,
                    admission: None,
                    refusal_reason: None,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                }),
                verdict: RefCell::new(Err("no verdict scripted".into())),
                judged: RefCell::new(0),
                refused: RefCell::new(Vec::new()),
                admitted: RefCell::new(Vec::new()),
            }
        }

        fn script_admitted(&self) {
            let record = self.record.borrow();
            *self.verdict.borrow_mut() = Ok(serde_json::to_vec(&serde_json::json!({
                "verdict": "admitted",
                "schema": LEASE_READY_SCHEMA,
                "lease_id": "lease-1",
                "admission_request_sha256": record.admission_request_sha256,
                "boot_id": "boot",
                "ready_frame_digest": lillux::sha256_hex(b"ready"),
                "report_frame_digest": lillux::sha256_hex(b"report"),
                "met_by": {},
                "channel": {"chain": 1},
            }))
            .unwrap());
        }

        fn script_terminate(&self) {
            *self.verdict.borrow_mut() = Ok(serde_json::to_vec(&serde_json::json!({
                "verdict": "terminate",
                "schema": LEASE_READY_SCHEMA,
                "lease_id": "lease-1",
                "reason": "the guest reports a route that is open",
            }))
            .unwrap());
        }

        fn frame(&self, sequence: u64, kind: FrameKind, wire: &[u8]) -> Result<LeaseReadyGate> {
            let record = self.record.borrow().clone();
            drive(self, &record, sequence, kind, wire)
        }
    }

    impl GatePorts for FakePorts {
        fn hold_ready(&self, wire: &[u8]) -> Result<()> {
            self.record.borrow_mut().held_ready_frame = Some(wire.to_vec());
            Ok(())
        }
        fn hold_report(&self, wire: &[u8]) -> Result<()> {
            self.record.borrow_mut().held_report_frame = Some(wire.to_vec());
            Ok(())
        }
        fn record(&self) -> Result<ExternalLeaseRecord> {
            Ok(self.record.borrow().clone())
        }
        fn judge(&self, _record: &ExternalLeaseRecord) -> Result<Vec<u8>> {
            *self.judged.borrow_mut() += 1;
            self.verdict.borrow().clone().map_err(anyhow::Error::msg)
        }
        fn admit(&self, admission: &ExternalLeaseAdmission) -> Result<()> {
            self.admitted.borrow_mut().push(admission.clone());
            self.record.borrow_mut().state = ExternalLeaseRecordState::Admitted;
            Ok(())
        }
        fn refuse(&self, reason: &str) -> Result<()> {
            self.refused.borrow_mut().push(reason.to_owned());
            self.record.borrow_mut().state = ExternalLeaseRecordState::Refused;
            Ok(())
        }
        fn adapter_id(&self) -> &str {
            "remote-lease-test"
        }
    }

    #[test]
    fn ready_is_held_and_the_report_asks_the_adapter_once() {
        let ports = FakePorts::new();
        assert_eq!(
            ports.frame(1, FrameKind::Ready, b"ready").unwrap(),
            LeaseReadyGate::Held
        );
        assert_eq!(*ports.judged.borrow(), 0, "Ready alone asks nothing");
        assert!(ports.admitted.borrow().is_empty());

        ports.script_admitted();
        assert_eq!(
            ports
                .frame(2, FrameKind::CapabilityReport, b"report")
                .unwrap(),
            LeaseReadyGate::Proceed
        );
        assert_eq!(*ports.judged.borrow(), 1);
        let admitted = ports.admitted.borrow();
        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].ready_frame_digest, lillux::sha256_hex(b"ready"));
        assert_eq!(admitted[0].resume_state_json, "{\"chain\":1}");
        assert!(ports.refused.borrow().is_empty());
        // Once admitted, every later frame takes the ordinary exchange.
        assert_eq!(
            ports.frame(3, FrameKind::Other, b"later").unwrap(),
            LeaseReadyGate::Proceed
        );
    }

    #[test]
    fn a_terminate_verdict_refuses_the_lease_with_the_adapters_reason() {
        let ports = FakePorts::new();
        ports.frame(1, FrameKind::Ready, b"ready").unwrap();
        ports.script_terminate();
        let error = ports
            .frame(2, FrameKind::CapabilityReport, b"report")
            .unwrap_err()
            .to_string();
        assert!(error.contains("the guest reports a route that is open"), "{error}");
        assert_eq!(ports.refused.borrow().len(), 1);
        assert!(ports.admitted.borrow().is_empty());
        // A refused lease stays refused for every later frame.
        assert!(ports.frame(2, FrameKind::CapabilityReport, b"report").is_err());
        assert_eq!(*ports.judged.borrow(), 1);
    }

    #[test]
    fn frames_out_of_order_refuse_the_lease_durably() {
        for (sequence, kind, ready_first) in [
            (1, FrameKind::Other, false),
            (1, FrameKind::CapabilityReport, false),
            (2, FrameKind::CapabilityReport, false),
            (2, FrameKind::Ready, true),
            (2, FrameKind::Other, true),
            (3, FrameKind::Other, true),
            (3, FrameKind::CapabilityReport, true),
        ] {
            let ports = FakePorts::new();
            if ready_first {
                ports.frame(1, FrameKind::Ready, b"ready").unwrap();
            }
            assert!(
                ports.frame(sequence, kind, b"frame").is_err(),
                "{sequence} {kind:?}"
            );
            assert_eq!(ports.refused.borrow().len(), 1, "{sequence} {kind:?}");
            assert_eq!(*ports.judged.borrow(), 0, "{sequence} {kind:?}");
            assert!(ports.admitted.borrow().is_empty());
        }
    }

    #[test]
    fn a_second_different_report_is_refused_and_the_same_report_retries() {
        let ports = FakePorts::new();
        ports.frame(1, FrameKind::Ready, b"ready").unwrap();
        // No verdict (the adapter failed): nothing is refused, and the same
        // report asks again.
        let failure = format!(
            "{:#}",
            ports
                .frame(2, FrameKind::CapabilityReport, b"report")
                .unwrap_err()
        );
        assert!(failure.contains("no verdict scripted"), "{failure}");
        assert!(ports.refused.borrow().is_empty());
        ports.script_admitted();
        assert_eq!(
            ports
                .frame(2, FrameKind::CapabilityReport, b"report")
                .unwrap(),
            LeaseReadyGate::Proceed
        );
        assert_eq!(*ports.judged.borrow(), 2);

        let ports = FakePorts::new();
        ports.frame(1, FrameKind::Ready, b"ready").unwrap();
        assert!(ports.frame(2, FrameKind::CapabilityReport, b"report").is_err());
        let different = ports
            .frame(2, FrameKind::CapabilityReport, b"another report")
            .unwrap_err()
            .to_string();
        assert!(different.contains("second, different capability report"), "{different}");
        assert_eq!(ports.refused.borrow().len(), 1);
        assert_eq!(*ports.judged.borrow(), 1);
    }

    #[test]
    fn a_verdict_about_other_frames_refuses_the_lease() {
        let ports = FakePorts::new();
        ports.frame(1, FrameKind::Ready, b"ready").unwrap();
        ports.script_admitted();
        let error = ports
            .frame(2, FrameKind::CapabilityReport, b"another report")
            .unwrap_err()
            .to_string();
        assert!(error.contains("judged different frames"), "{error}");
        assert_eq!(ports.refused.borrow().len(), 1);
        assert!(ports.admitted.borrow().is_empty());
    }

    #[test]
    fn a_retried_ready_is_held_again_and_a_different_one_refuses() {
        let ports = FakePorts::new();
        ports.frame(1, FrameKind::Ready, b"ready").unwrap();
        assert_eq!(
            ports.frame(1, FrameKind::Ready, b"ready").unwrap(),
            LeaseReadyGate::Held
        );
        assert!(ports.frame(1, FrameKind::Ready, b"other ready").is_err());
        assert_eq!(ports.refused.borrow().len(), 1);
    }

    #[test]
    fn an_unactivated_lease_accepts_no_frame() {
        let ports = FakePorts::new();
        ports.record.borrow_mut().state = ExternalLeaseRecordState::Reserved;
        assert!(ports.frame(1, FrameKind::Ready, b"ready").is_err());
        assert!(ports.refused.borrow().is_empty());
    }

    #[test]
    fn a_failure_to_reach_a_verdict_is_marked_and_a_refusal_is_not() {
        let ports = FakePorts::new();
        ports.frame(1, FrameKind::Ready, b"ready").unwrap();
        let unavailable = ports
            .frame(2, FrameKind::CapabilityReport, b"report")
            .unwrap_err();
        assert!(ExternalChannelExchangeUnavailable::marks(&unavailable), "{unavailable:#}");

        ports.script_terminate();
        let refused = ports
            .frame(2, FrameKind::CapabilityReport, b"report")
            .unwrap_err();
        assert!(!ExternalChannelExchangeUnavailable::marks(&refused), "{refused:#}");
        // A lease that is already refused, or out of order, is a refusal too.
        let again = ports
            .frame(2, FrameKind::CapabilityReport, b"report")
            .unwrap_err();
        assert!(!ExternalChannelExchangeUnavailable::marks(&again), "{again:#}");
    }

    #[test]
    fn one_placement_gate_runs_at_a_time_and_leaves_no_lock_behind() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let inside = Arc::new(AtomicUsize::new(0));
        let overlaps = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let (inside, overlaps) = (Arc::clone(&inside), Arc::clone(&overlaps));
                std::thread::spawn(move || {
                    serialised_per_placement("T-serialised-test", || {
                        if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                            overlaps.fetch_add(1, Ordering::SeqCst);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        inside.fetch_sub(1, Ordering::SeqCst);
                    });
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(overlaps.load(Ordering::SeqCst), 0);
        assert!(
            !PLACEMENT_GATES
                .lock()
                .unwrap()
                .contains_key("T-serialised-test")
        );
    }
}
