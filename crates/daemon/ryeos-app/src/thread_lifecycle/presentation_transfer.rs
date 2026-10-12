//! Deterministic transfer-settlement planning in the existing lifecycle owner.
//!
//! Pure plans are not admission. The bounded reader authenticates fixed CAS
//! histories, v3 control causality/frontier and adjacent v2 append receipts,
//! but does not prove expiry or the runtime-owner fence.
//! Its results are read-only candidates. The known-transfer lifecycle writer
//! separately holds scheduler and two-seat publication fences; the legacy v1
//! writer remains sealed. A claimed-seat Abort can be selected only through
//! that writer. Exhaustive completed clearance also feeds the existing
//! claimed-seat finalization owner; no path recreates a head or restores live
//! browser authority.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use ryeos_ui_presentation_contract::{
    identity::UiSeatAppendIdentity,
    initialization::UiPresentationInitializationBound,
    seat_control::{
        SEAT_APPEND_RECEIPT_SCHEMA, SEAT_PRODUCER_EVENT, SeatAppendReceipt,
        SeatAppendSemanticEvent, UiSeatControlPredecessor, UiSeatProducer, UiSeatProducerState,
        authenticate_control_transition,
    },
    transfer::{
        TRANSFER_EVENT, TransferEvidence, TransferIdentity, TransferPhase, TransferRecord,
        TransferSeat,
    },
    transfer_lifecycle::{
        TRANSFER_LIFECYCLE_EVENT, TransferLifecycleEventPayload, verify_closed_producer_fence,
    },
    transfer_lifecycle_v2::{
        TRANSFER_LIFECYCLE_V2_EVENT, TransferLifecycleEventPayloadV2, verify_latest_control,
    },
    transfer_proof::TransferTestimony,
    transfer_state::TransferLifecycleState,
};

use crate::state_store::StateStore;

/// Existing scheduler exclusion acquired by the lifecycle owner. The retained
/// Arc prevents a guard for another node from authorizing this recovery pass.
pub struct UiSeatStartupExclusion {
    gate: Arc<tokio::sync::RwLock<()>>,
    guard: tokio::sync::OwnedRwLockWriteGuard<()>,
}

impl UiSeatStartupExclusion {
    pub fn guard(&self) -> &tokio::sync::OwnedRwLockWriteGuard<()> {
        &self.guard
    }

    fn validate(&self, gate: &Arc<tokio::sync::RwLock<()>>) -> anyhow::Result<()> {
        if !Arc::ptr_eq(&self.gate, gate) {
            bail!("startup seat recovery uses another scheduler exclusion");
        }
        Ok(())
    }
}

// Constructed only while an existing scheduler read/write guard is borrowed.
struct SettlementExclusion<'a>(&'a ());

fn discovery_slot_must_be_released(queued: &anyhow::Result<bool>) -> bool {
    !queued.as_ref().is_ok_and(|queued| *queued)
}

#[derive(Debug, Default)]
pub struct UiSeatStartupRecoveryReport {
    pub claimed_roots: usize,
    pub finalized: usize,
    pub deferred: bool,
}

fn claimed_discovery_start(counter: &std::sync::atomic::AtomicUsize, roots: usize) -> usize {
    if roots == 0 {
        return 0;
    }
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % roots
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferSettlementSide {
    Source,
    Destination,
}

/// One bounded, newest-first page of transfer identities found in an exact
/// signed seat history. Identities may recur on later pages: this is a locator,
/// not a proof that any transfer is complete or ready for settlement.
pub struct SignedSeatTransferIdentityPage {
    /// Positive ordinary freeze-preparation locators, not authenticated
    /// lifecycle clearance. Retirement must discharge these independently.
    pub freeze_preparations: Vec<TransferIdentity>,
    pub identities: Vec<TransferIdentity>,
    pub progress: Option<ryeos_state::AuthoritativeThreadEventReadProgress>,
}

/// Opaque fixed-head progress for lifecycle discovery. It is deliberately not
/// a settlement, absence or finalization token.
#[derive(Clone)]
pub struct ClaimedSeatTransferDiscovery {
    store: Arc<StateStore>,
    seat_root_id: String,
    claim: crate::runtime_db::ClaimedSeatLeaseSnapshot,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    progress: ryeos_state::AuthoritativeThreadEventReadProgress,
}

/// Each call has a bounded work budget. Only `Complete` means the signed seat
/// history reached genesis; even then, a counterpart Prepared elsewhere
/// remains undiscovered. The caller must retain identities from all pages.
pub enum ClaimedSeatTransferDiscoveryOutcome {
    Complete(Vec<TransferIdentity>),
    Incomplete {
        identities: Vec<TransferIdentity>,
        continuation: ClaimedSeatTransferDiscovery,
    },
    Conflict,
}

/// An admission-independent throughput bound, not a seat-history limit.
/// When all slots are occupied, another claimed seat waits rather than
/// evicting a cursor that must reach older positive candidates.
const MAX_CLAIMED_TRANSFER_SCAN_CURSORS: usize = 1024;
// A receipt may legally cover 128 native events, each at the native 512 KiB
// ceiling. Keep both the per-cursor protocol maximum and the number of
// simultaneously retained decoders explicit; source and destination are read
// serially, so only one receipt can be pending in each cursor.
const MAX_SETTLEMENT_CURSORS: usize = 2;
// A permanently unreadable history must not monopolize both decoder slots.
// Dropping this process-local cursor is not absence or settlement: the signed
// positive locator rediscovers the operation on its next complete cycle.
const MAX_SETTLEMENT_READER_FAILURES: u8 = 3;
// Maintenance must consume exactly one signed event per locator call. The
// extractor yields at most one transfer identity from that event, so one free
// decoder slot is enough before advancing its opaque positive cursor.
const MAINTENANCE_DISCOVERY_PAGES: u64 = 1;
const MAINTENANCE_DISCOVERY_EVENTS_PER_PAGE: u64 = 1;
const MAX_PENDING_RECEIPT_BYTES: u64 =
    128 * ryeos_state::objects::thread_event::MAX_THREAD_EVENT_SERIALIZED_BYTES as u64;

fn charge_pending_receipt_bytes(current: u64, event_bytes: u64) -> anyhow::Result<u64> {
    let total = current
        .checked_add(event_bytes)
        .ok_or_else(|| anyhow!("seat receipt interval byte count overflow"))?;
    if total > MAX_PENDING_RECEIPT_BYTES {
        bail!("seat receipt interval exceeds its protocol byte maximum");
    }
    Ok(total)
}

fn next_settlement_turn(turns: &mut VecDeque<(String, String)>) -> Option<(String, String)> {
    turns.pop_front()
}

fn decoder_has_room(readers: usize, reserved: usize) -> bool {
    readers
        .checked_add(reserved)
        .is_some_and(|occupied| occupied < MAX_SETTLEMENT_CURSORS)
}

fn retained_settlement_decoders(cursors: &ClaimedTransferSettlementCursors) -> usize {
    cursors.readers.len() + cursors.ready.len() + cursors.publishing_slots
}

fn retain_failed_settlement_reader(failures: &mut u8) -> bool {
    *failures = failures.saturating_add(1);
    *failures < MAX_SETTLEMENT_READER_FAILURES
}

#[derive(Default)]
pub(crate) struct ClaimedTransferSettlementCursors {
    readers: BTreeMap<(String, String), ClaimedTransferSettlementReader>,
    ready: BTreeMap<(String, String), ReadyClaimedTransferSettlement>,
    pending: BTreeMap<String, PendingClaimedTransfer>,
    turn: VecDeque<(String, String)>,
    publishing_slots: usize,
    reserved_slots: usize,
}

struct PendingClaimedTransfer {
    claim: crate::runtime_db::ClaimedSeatLeaseSnapshot,
    transfer: TransferIdentity,
    side: TransferSettlementSide,
    failures: u8,
}

struct ClaimedTransferSettlementReader {
    claim: crate::runtime_db::ClaimedSeatLeaseSnapshot,
    read: UiTransferSettlementRead,
    failures: u8,
}

struct ReadyClaimedTransferSettlement {
    claim: crate::runtime_db::ClaimedSeatLeaseSnapshot,
    transfer: TransferIdentity,
    side: TransferSettlementSide,
    candidate: SettlementPreparation,
    failures: u8,
}

fn note_pending_reader_start_failure(
    cursors: &mut ClaimedTransferSettlementCursors,
    seat_root_id: &str,
) {
    let evict = cursors
        .pending
        .get_mut(seat_root_id)
        .is_some_and(|pending| !retain_failed_settlement_reader(&mut pending.failures));
    if evict {
        // Only the process-local positive hint is released. The signed seat
        // and durable claim remain fenced; a later full cycle may rediscover
        // this exact identity after older locator pages get a turn.
        cursors.pending.remove(seat_root_id);
    }
}

fn retain_failed_ready_settlement(
    cursors: &mut ClaimedTransferSettlementCursors,
    key: (String, String),
    mut ready: ReadyClaimedTransferSettlement,
) {
    if retain_failed_settlement_reader(&mut ready.failures) {
        cursors.ready.insert(key.clone(), ready);
        cursors.turn.push_back(key);
    }
}

pub(crate) struct ClaimedSeatFinalizationCursor {
    claim: crate::runtime_db::ClaimedSeatLeaseSnapshot,
    stage: ClaimedSeatFinalizationStage,
}

enum ClaimedSeatFinalizationStage {
    Discovery(crate::state_store::TerminalSeatTransferDiscoveryRead),
    Clearance(
        crate::state_store::TerminalSeatTransferDiscoveryScope,
        TerminalTransferClearanceRead,
    ),
}

#[derive(Default)]
pub(crate) struct TerminalSeatRetirementCursors {
    after: Option<String>,
    pending: BTreeMap<String, ClaimedSeatFinalizationStage>,
    turn: VecDeque<String>,
}

impl super::ThreadLifecycleService {
    fn attached_scheduler_gate(&self) -> anyhow::Result<Arc<tokio::sync::RwLock<()>>> {
        self.scheduler_runtime_gate
            .read()
            .map_err(|_| anyhow!("scheduler runtime gate lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow!("scheduler runtime gate is not attached"))
    }

    pub async fn acquire_ui_seat_startup_exclusion(
        &self,
    ) -> anyhow::Result<UiSeatStartupExclusion> {
        let gate = self.attached_scheduler_gate()?;
        let guard = gate.clone().write_owned().await;
        Ok(UiSeatStartupExclusion { gate, guard })
    }

    /// Retry only already-fenced seats under startup's existing scheduler lock.
    /// This is independent of maintenance schedules and never claims expiry,
    /// runs GC, unlinks history, attaches a browser, or restores execution.
    /// A finite fair pass leaves unfinished signed obligations/cursors intact.
    pub fn recover_claimed_ui_seats_at_startup(
        &self,
        exclusion: &UiSeatStartupExclusion,
    ) -> anyhow::Result<UiSeatStartupRecoveryReport> {
        exclusion.validate(&self.attached_scheduler_gate()?)?;
        self.recover_claimed_ui_seats_under_exclusion(&SettlementExclusion(&exclusion.guard))
    }

    /// Existing periodic runtime recovery continues the bounded startup pass,
    /// including when every maintenance schedule is paused or absent.
    pub async fn recover_claimed_ui_seats(&self) -> anyhow::Result<UiSeatStartupRecoveryReport> {
        let gate = self.attached_scheduler_gate()?;
        let guard = gate.read_owned().await;
        self.recover_claimed_ui_seats_under_exclusion(&SettlementExclusion(&guard))
    }

    fn recover_claimed_ui_seats_under_exclusion(
        &self,
        held: &SettlementExclusion<'_>,
    ) -> anyhow::Result<UiSeatStartupRecoveryReport> {
        let scheduler = self
            .scheduler_db
            .read()
            .map_err(|_| anyhow!("scheduler database lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow!("scheduler database is not attached"))?;
        if !scheduler.fire_projection_is_current()? {
            bail!("startup seat recovery requires current scheduler fire projection");
        }
        let mut report = UiSeatStartupRecoveryReport::default();
        let mut scan = self
            .claimed_seat_recovery_scan
            .lock()
            .map_err(|_| anyhow!("claimed seat recovery scan lock poisoned"))?;
        if scan.is_none() {
            let Some(max_id) = self.state_store.max_seat_lease_id()? else {
                return Ok(report);
            };
            *scan = Some((max_id, None));
        }
        let (max_id, after_id) = scan.as_ref().expect("recovery scan initialized");
        let roots = self
            .state_store
            .claimed_seat_leases_slice(after_id.as_deref(), max_id, 64)?;
        if roots.len() < 64 {
            *scan = None;
        } else {
            scan.as_mut().expect("recovery scan initialized").1 = roots.last().cloned();
        }
        drop(scan);
        report.claimed_roots = roots.len();
        if roots.is_empty() {
            return Ok(report);
        }
        // More roots or longer histories are deferred, never treated as absent.
        report.deferred = roots.len() == 64;
        let start = self.next_claimed_ui_seat_discovery_start(roots.len());
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        let mut finalized = BTreeSet::new();
        for turn in 0..256 {
            if std::time::Instant::now() >= deadline {
                break;
            }
            let root = &roots[(start + turn) % roots.len()];
            if finalized.contains(root) {
                continue;
            }
            if let Err(error) = self.queue_next_claimed_ui_seat_transfer(root) {
                tracing::debug!(%root, %error, "startup seat discovery retained for retry");
            }
            if let Err(error) = self.advance_claimed_ui_transfer_under_exclusion(held) {
                tracing::debug!(%error, "startup seat settlement retained for retry");
            }
            match self.finalize_claimed_ui_seat_under_exclusion(root, held) {
                Ok(true) => {
                    finalized.insert(root.clone());
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::debug!(%root, %error, "startup seat finalization retained for retry")
                }
            }
        }
        report.finalized = finalized.len();
        report.deferred |= report.finalized < report.claimed_roots;
        Ok(report)
    }

    /// One keyset discovery slice and at most sixteen fair bounded proof
    /// turns. Caller holds the existing scheduler retention exclusion.
    pub fn advance_terminal_ui_seat_retirement(
        &self,
        now: &str,
        dry_run: bool,
    ) -> anyhow::Result<crate::state_store::TerminalChainRetirement> {
        let scheduler = self
            .scheduler_db
            .read()
            .map_err(|_| anyhow!("scheduler database lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow!("scheduler database is not attached"))?;
        if !scheduler.fire_projection_is_current()? {
            bail!("scheduler fire projection is not current");
        }
        let mut cursors = self
            .terminal_seat_retirement_cursors
            .lock()
            .map_err(|_| anyhow!("terminal retirement cursor lock poisoned"))?;
        let roots =
            self.state_store
                .due_signed_seat_retirement_slice(now, cursors.after.as_deref(), 16)?;
        cursors.after = roots.last().cloned();
        for root in roots {
            if cursors.pending.contains_key(&root)
                || cursors.pending.len() >= MAX_CLAIMED_TRANSFER_SCAN_CURSORS
            {
                continue;
            }
            let read = match self
                .state_store
                .begin_terminal_seat_transfer_discovery(&root)
            {
                Ok(read) => read,
                Err(error) => {
                    tracing::debug!(%root, %error, "terminal signed seat discovery deferred");
                    continue;
                }
            };
            if let Some(read) = read {
                cursors
                    .pending
                    .insert(root.clone(), ClaimedSeatFinalizationStage::Discovery(read));
                cursors.turn.push_back(root);
            }
        }
        let mut result = crate::state_store::TerminalChainRetirement::default();
        for _ in 0..16 {
            let Some(root) = cursors.turn.pop_front() else {
                break;
            };
            let Some(stage) = cursors.pending.remove(&root) else {
                continue;
            };
            let step = (|| -> anyhow::Result<Option<ClaimedSeatFinalizationStage>> {
                match stage {
                    ClaimedSeatFinalizationStage::Discovery(mut read) => {
                        if self
                            .state_store
                            .capture_thread_event_read_anchor(&root, &root)?
                            .as_ref()
                            != Some(read.anchor())
                        {
                            return Ok(None);
                        }
                        if !self
                            .state_store
                            .read_terminal_seat_transfer_discovery_page(&mut read, 1, 1024 * 1024)?
                        {
                            return Ok(Some(ClaimedSeatFinalizationStage::Discovery(read)));
                        }
                        let scope = self
                            .state_store
                            .finish_terminal_seat_transfer_discovery(read)?;
                        let requests = scope.identities().cloned().collect::<Vec<_>>();
                        let read = begin_terminal_transfer_clearance(
                            self.state_store.clone(),
                            scope.anchor(),
                            &requests,
                        )?;
                        Ok(Some(ClaimedSeatFinalizationStage::Clearance(scope, read)))
                    }
                    ClaimedSeatFinalizationStage::Clearance(scope, mut read) => {
                        if self
                            .state_store
                            .capture_thread_event_read_anchor(&root, &root)?
                            .as_ref()
                            != Some(scope.anchor())
                        {
                            return Ok(None);
                        }
                        if !read_terminal_transfer_clearance_page(&mut read)? {
                            return Ok(Some(ClaimedSeatFinalizationStage::Clearance(scope, read)));
                        }
                        let clearance = finish_terminal_transfer_clearance(read)?;
                        let candidate = self
                            .state_store
                            .qualify_terminal_transfer_clearance(scope, clearance)?;
                        if let Some(retired) = self.state_store.retire_qualified_terminal_seat(
                            &candidate,
                            now,
                            dry_run,
                            |threads| {
                                let mut pins = 0u64;
                                for thread in threads {
                                    if scheduler.find_fire_by_thread(thread)?.is_some() {
                                        pins = pins.checked_add(1).ok_or_else(|| {
                                            anyhow!("scheduler pin count overflow")
                                        })?;
                                    }
                                }
                                Ok(pins)
                            },
                        )? {
                            result.candidate_chains += retired.candidate_chains;
                            result.retired_chains += retired.retired_chains;
                            result.deleted_projection_rows += retired.deleted_projection_rows;
                            result.deleted_runtime_rows += retired.deleted_runtime_rows;
                            result.deleted_runtime_files += retired.deleted_runtime_files;
                            result.pending_retirements_recovered +=
                                retired.pending_retirements_recovered;
                        }
                        Ok(None)
                    }
                }
            })();
            match step {
                Ok(Some(stage)) => {
                    cursors.pending.insert(root.clone(), stage);
                    cursors.turn.push_back(root);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%root, %error, "terminal signed seat retirement deferred")
                }
            }
        }
        Ok(result)
    }
    /// Complete local discovery, not the known-operation scheduling queue,
    /// supplies finalization coverage. Publication repeats the durable claim,
    /// all dependency cuts and pins while the scheduler is excluded.
    pub async fn finalize_claimed_ui_seat_if_cleared(
        &self,
        seat_root_id: &str,
    ) -> anyhow::Result<bool> {
        let gate = self.attached_scheduler_gate()?;
        let guard = gate.read_owned().await;
        self.finalize_claimed_ui_seat_under_exclusion(seat_root_id, &SettlementExclusion(&guard))
    }

    fn finalize_claimed_ui_seat_under_exclusion(
        &self,
        seat_root_id: &str,
        exclusion: &SettlementExclusion<'_>,
    ) -> anyhow::Result<bool> {
        let _ = exclusion.0;
        let Some(claim) = self
            .state_store
            .claimed_running_seat_root_snapshot(seat_root_id)?
        else {
            self.claimed_seat_finalization_cursors
                .lock()
                .map_err(|_| anyhow!("claimed finalization cursor lock poisoned"))?
                .remove(seat_root_id);
            return Ok(false);
        };
        let mut cursors = self
            .claimed_seat_finalization_cursors
            .lock()
            .map_err(|_| anyhow!("claimed finalization cursor lock poisoned"))?;
        // One rotating stale-slot check also covers roots no longer returned
        // by the lease keyset. Removing scheduling state never releases an
        // obligation or proves absence; future discovery starts from genesis.
        if !cursors.is_empty() {
            let index = self
                .claimed_finalization_prune_turn
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                % cursors.len();
            let key = cursors
                .keys()
                .nth(index)
                .cloned()
                .expect("bounded finalization slot");
            if self
                .state_store
                .claimed_running_seat_root_snapshot(&key)?
                .as_ref()
                != cursors.get(&key).map(|cursor| &cursor.claim)
            {
                cursors.remove(&key);
            }
        }
        let cursor = match cursors.remove(seat_root_id) {
            Some(cursor) if cursor.claim == claim => cursor,
            _ => {
                if cursors.len() >= MAX_CLAIMED_TRANSFER_SCAN_CURSORS {
                    return Ok(false);
                }
                let Some(anchor) = self
                    .state_store
                    .capture_thread_event_read_anchor(seat_root_id, seat_root_id)?
                else {
                    return Ok(false);
                };
                let Some(read) = self
                    .state_store
                    .begin_claimed_seat_transfer_discovery(seat_root_id, anchor)?
                else {
                    return Ok(false);
                };
                ClaimedSeatFinalizationCursor {
                    claim: claim.clone(),
                    stage: ClaimedSeatFinalizationStage::Discovery(read),
                }
            }
        };
        let local_anchor = match &cursor.stage {
            ClaimedSeatFinalizationStage::Discovery(read) => read.anchor(),
            ClaimedSeatFinalizationStage::Clearance(scope, _) => scope.anchor(),
        };
        if self
            .state_store
            .capture_thread_event_read_anchor(seat_root_id, seat_root_id)?
            .as_ref()
            != Some(local_anchor)
        {
            // Discard stale partial testimony; a later turn starts at a new
            // cut. Never splice a new suffix into an absence scope.
            return Ok(false);
        }
        let (scope, clearance) = match cursor.stage {
            ClaimedSeatFinalizationStage::Discovery(mut read) => {
                if self
                    .state_store
                    .read_terminal_seat_transfer_discovery_page(&mut read, 1, 1024 * 1024)?
                {
                    let scope = self
                        .state_store
                        .finish_terminal_seat_transfer_discovery(read)?;
                    let identities = scope.identities().cloned().collect::<Vec<_>>();
                    let read = begin_terminal_transfer_clearance(
                        self.state_store.clone(),
                        scope.anchor(),
                        &identities,
                    )?;
                    cursors.insert(
                        seat_root_id.to_owned(),
                        ClaimedSeatFinalizationCursor {
                            claim,
                            stage: ClaimedSeatFinalizationStage::Clearance(scope, read),
                        },
                    );
                } else {
                    cursors.insert(
                        seat_root_id.to_owned(),
                        ClaimedSeatFinalizationCursor {
                            claim,
                            stage: ClaimedSeatFinalizationStage::Discovery(read),
                        },
                    );
                }
                return Ok(false);
            }
            ClaimedSeatFinalizationStage::Clearance(scope, mut read) => {
                if !read_terminal_transfer_clearance_page(&mut read)? {
                    cursors.insert(
                        seat_root_id.to_owned(),
                        ClaimedSeatFinalizationCursor {
                            claim,
                            stage: ClaimedSeatFinalizationStage::Clearance(scope, read),
                        },
                    );
                    return Ok(false);
                }
                (scope, finish_terminal_transfer_clearance(read)?)
            }
        };
        drop(cursors);
        let scheduler = self
            .scheduler_db
            .read()
            .map_err(|_| anyhow!("scheduler database lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow!("scheduler database is not attached"))?;
        if !scheduler.fire_projection_is_current()? {
            bail!("scheduler fire projection is not current");
        }
        let records = self.state_store.finalize_claimed_seat_with_clearance(
            &scope,
            &clearance,
            &claim,
            |threads| {
                let mut pins = 0u64;
                for thread in threads {
                    if scheduler.find_fire_by_thread(thread)?.is_some() {
                        pins = pins
                            .checked_add(1)
                            .ok_or_else(|| anyhow!("scheduler pin count overflow"))?;
                    }
                }
                Ok(pins)
            },
        )?;
        if let Some(records) = records {
            self.publish_records(&records);
            self.discard_claimed_ui_seat_transfer_scan(seat_root_id)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Forget a scheduling cursor only after the exact lease was proved
    /// terminal and removed. No retained testimony or authority is deleted.
    pub fn discard_claimed_ui_seat_transfer_scan(&self, seat_root_id: &str) -> anyhow::Result<()> {
        self.claimed_seat_finalization_cursors
            .lock()
            .map_err(|_| anyhow!("claimed finalization cursor lock poisoned"))?
            .remove(seat_root_id);
        self.claimed_transfer_scan_cursors
            .lock()
            .map_err(|_| anyhow!("claimed transfer scan cursor lock poisoned"))?
            .remove(seat_root_id);
        let mut settlement = self
            .claimed_transfer_settlement_cursors
            .lock()
            .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?;
        settlement
            .readers
            .retain(|(seat, _), _| seat != seat_root_id);
        settlement.ready.retain(|(seat, _), _| seat != seat_root_id);
        settlement.pending.remove(seat_root_id);
        settlement.turn.retain(|(seat, _)| seat != seat_root_id);
        Ok(())
    }

    /// Leave space before moving a locator cursor: a discovered identity must
    /// never be silently dropped merely because the decoder cache is full.
    pub fn reserve_claimed_ui_seat_transfer_discovery(
        &self,
        seat_root_id: &str,
    ) -> anyhow::Result<bool> {
        self.prune_unclaimed_pending_transfers_if_full(seat_root_id)?;
        let mut cursors = self
            .claimed_transfer_settlement_cursors
            .lock()
            .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?;
        if !decoder_has_room(
            retained_settlement_decoders(&cursors),
            cursors.reserved_slots,
        ) {
            return Ok(false);
        }
        // Every advanced positive locator must have space to retain its one
        // identity if starting the exact two-seat reader fails.
        if !cursors.pending.contains_key(seat_root_id)
            && cursors.pending.len() >= MAX_CLAIMED_TRANSFER_SCAN_CURSORS
        {
            return Ok(false);
        }
        cursors.reserved_slots += 1;
        Ok(true)
    }

    /// Shared startup/maintenance orchestration: reserve before discovery and
    /// retain every discovered positive identity until its decoder is queued.
    pub fn queue_next_claimed_ui_seat_transfer(&self, root: &str) -> anyhow::Result<()> {
        if !self.reserve_claimed_ui_seat_transfer_discovery(root)? {
            return Ok(());
        }
        let queued = (|| -> anyhow::Result<bool> {
            if let Some(queued) = self.retry_pending_claimed_ui_seat_transfer(root)? {
                if !queued {
                    bail!("reserved transfer decoder slot disappeared");
                }
                return Ok(true);
            }
            let identities = match self.discover_claimed_ui_seat_transfers_for_maintenance(root)? {
                ClaimedSeatTransferDiscoveryOutcome::Complete(identities)
                | ClaimedSeatTransferDiscoveryOutcome::Incomplete { identities, .. } => identities,
                ClaimedSeatTransferDiscoveryOutcome::Conflict => Vec::new(),
            };
            if identities.len() > 1 {
                bail!("single-event transfer locator returned multiple identities");
            }
            let Some(transfer) = identities.first() else {
                return Ok(false);
            };
            let side = if transfer.source.chain_root_id == root && transfer.source.thread_id == root
            {
                TransferSettlementSide::Source
            } else if transfer.destination.chain_root_id == root
                && transfer.destination.thread_id == root
            {
                TransferSettlementSide::Destination
            } else {
                bail!("claimed seat transfer locator returned another exact seat");
            };
            if !self.queue_claimed_ui_seat_transfer(root, transfer, side)? {
                bail!("reserved transfer decoder slot disappeared");
            }
            Ok(true)
        })();
        if discovery_slot_must_be_released(&queued) {
            self.release_claimed_ui_seat_transfer_discovery()?;
        }
        queued?;
        Ok(())
    }

    fn prune_unclaimed_pending_transfers_if_full(&self, seat_root_id: &str) -> anyhow::Result<()> {
        let keys = {
            let cursors = self
                .claimed_transfer_settlement_cursors
                .lock()
                .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?;
            if cursors.pending.len() < MAX_CLAIMED_TRANSFER_SCAN_CURSORS
                || cursors.pending.contains_key(seat_root_id)
            {
                return Ok(());
            }
            cursors.pending.keys().cloned().collect::<Vec<_>>()
        };
        for key in keys {
            if !self.state_store.has_claimed_seat_lease(&key)? {
                self.claimed_transfer_settlement_cursors
                    .lock()
                    .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?
                    .pending
                    .remove(&key);
            }
        }
        Ok(())
    }

    pub fn release_claimed_ui_seat_transfer_discovery(&self) -> anyhow::Result<()> {
        let mut cursors = self
            .claimed_transfer_settlement_cursors
            .lock()
            .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?;
        if cursors.reserved_slots == 0 {
            bail!("claimed transfer discovery has no reserved decoder slot");
        }
        cursors.reserved_slots -= 1;
        Ok(())
    }

    pub fn queue_claimed_ui_seat_transfer(
        &self,
        seat_root_id: &str,
        transfer: &TransferIdentity,
        side: TransferSettlementSide,
    ) -> anyhow::Result<bool> {
        let key = (seat_root_id.to_owned(), transfer.operation_id.clone());
        let claim = self
            .state_store
            .claimed_running_seat_root_snapshot(seat_root_id)?
            .ok_or_else(|| anyhow!("claimed settlement seat lost its exact running claim"))?;
        let mut cursors = self
            .claimed_transfer_settlement_cursors
            .lock()
            .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?;
        if cursors.reserved_slots == 0 {
            bail!("claimed transfer queue has no reserved decoder slot");
        }
        if let Some(pending) = cursors.pending.get(seat_root_id) {
            if pending.claim != claim || pending.transfer != *transfer || pending.side != side {
                bail!("claimed transfer pending positive identity changed before queue");
            }
        } else {
            if cursors.pending.len() >= MAX_CLAIMED_TRANSFER_SCAN_CURSORS {
                bail!("claimed transfer pending positive cache is full");
            }
            cursors.pending.insert(
                seat_root_id.to_owned(),
                PendingClaimedTransfer {
                    claim: claim.clone(),
                    transfer: transfer.clone(),
                    side,
                    failures: 0,
                },
            );
        }
        if let Some((identity_matches, claim_matches)) = cursors.ready.get(&key).map(|ready| {
            (
                ready.transfer == *transfer && ready.side == side,
                ready.claim == claim,
            )
        }) {
            if !identity_matches {
                note_pending_reader_start_failure(&mut cursors, seat_root_id);
                bail!("claimed ready settlement operation changed identity or side");
            }
            if claim_matches {
                cursors.pending.remove(seat_root_id);
                cursors.reserved_slots -= 1;
                return Ok(true);
            }
            cursors.ready.remove(&key);
            cursors.turn.retain(|queued| queued != &key);
        }
        if let Some((identity_matches, claim_matches)) = cursors.readers.get(&key).map(|existing| {
            (
                existing.read.transfer == *transfer && existing.read.side == side,
                existing.claim == claim,
            )
        }) {
            if !identity_matches {
                note_pending_reader_start_failure(&mut cursors, seat_root_id);
                bail!("claimed settlement operation changed identity or side");
            }
            if claim_matches {
                cursors.pending.remove(seat_root_id);
                cursors.reserved_slots -= 1;
                return Ok(true);
            }
            // An old fixed anchor is only a scheduling hint. A replacement
            // durable claim must begin from fresh exact signed heads.
            cursors.readers.remove(&key);
            cursors.turn.retain(|queued| queued != &key);
        }
        if retained_settlement_decoders(&cursors) >= MAX_SETTLEMENT_CURSORS {
            return Ok(false);
        }
        let read = match begin_read(self.state_store.clone(), transfer, side) {
            Ok(read) => read,
            Err(error) => {
                // A permanently unreadable positive hint must not starve
                // older identities behind this process-local cursor. The
                // exact claim and signed-seat finalization veto remain; a
                // later full cycle may rediscover this identity.
                note_pending_reader_start_failure(&mut cursors, seat_root_id);
                return Err(error);
            }
        };
        cursors.readers.insert(
            key.clone(),
            ClaimedTransferSettlementReader {
                claim,
                read,
                failures: 0,
            },
        );
        cursors.turn.push_back(key);
        cursors.pending.remove(seat_root_id);
        cursors.reserved_slots -= 1;
        Ok(true)
    }

    /// Retry one positively located identity before moving its signed-history
    /// cursor. A changed claim discards this scheduling hint, never evidence.
    pub fn retry_pending_claimed_ui_seat_transfer(
        &self,
        seat_root_id: &str,
    ) -> anyhow::Result<Option<bool>> {
        let claim = self
            .state_store
            .claimed_running_seat_root_snapshot(seat_root_id)?;
        let pending = {
            let mut cursors = self
                .claimed_transfer_settlement_cursors
                .lock()
                .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?;
            if cursors
                .pending
                .get(seat_root_id)
                .is_some_and(|p| claim.as_ref() != Some(&p.claim))
            {
                cursors.pending.remove(seat_root_id);
            }
            cursors
                .pending
                .get(seat_root_id)
                .map(|p| (p.transfer.clone(), p.side))
        };
        pending
            .map(|(transfer, side)| {
                self.queue_claimed_ui_seat_transfer(seat_root_id, &transfer, side)
            })
            .transpose()
    }

    /// At most one signed page is authenticated on each maintenance turn.
    /// A complete candidate retains its bounded fair-turn slot until the
    /// synchronous publication attempt finishes; all publication admission
    /// is rechecked by the existing known-transfer writer. A
    /// deferred or conflicting reader never proves absence: the positive
    /// locator continues to genesis and rediscovers identities from a fresh
    /// signed head. A failed page briefly keeps its fair-turn slot and
    /// restarts from fresh signed anchors rather than reusing a poisoned
    /// partial fold; repeated failures release the process-local slot for
    /// later positive identities without proving the failed one absent.
    pub async fn advance_claimed_ui_transfer_settlement_once(
        &self,
    ) -> anyhow::Result<Option<UiTransferSettlementWriteOutcome>> {
        // This is the only suspension point. Waiting here cannot consume a
        // reader or ready candidate; cancellation cannot lose its positive
        // identity after the locator has advanced.
        let runtime_gate = self.attached_scheduler_gate()?;
        let scheduler_guard = runtime_gate.read_owned().await;
        self.advance_claimed_ui_transfer_under_exclusion(&SettlementExclusion(&scheduler_guard))
    }

    fn advance_claimed_ui_transfer_under_exclusion(
        &self,
        exclusion: &SettlementExclusion<'_>,
    ) -> anyhow::Result<Option<UiTransferSettlementWriteOutcome>> {
        let (key, ready) = {
            let mut cursors = self
                .claimed_transfer_settlement_cursors
                .lock()
                .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?;
            let Some(key) = next_settlement_turn(&mut cursors.turn) else {
                return Ok(None);
            };
            if let Some(ready) = cursors.ready.remove(&key) {
                cursors.publishing_slots += 1;
                (key, ready)
            } else {
                let mut cursor = cursors
                    .readers
                    .remove(&key)
                    .ok_or_else(|| anyhow!("claimed settlement turn has no reader"))?;
                let current_claim =
                    match self.state_store.claimed_running_seat_root_snapshot(&key.0) {
                        Ok(claim) => claim,
                        Err(error) => {
                            if retain_failed_settlement_reader(&mut cursor.failures) {
                                cursors.readers.insert(key.clone(), cursor);
                                cursors.turn.push_back(key);
                            }
                            return Err(error);
                        }
                    };
                if current_claim != Some(cursor.claim.clone()) {
                    return Ok(Some(UiTransferSettlementWriteOutcome::Conflict));
                }
                // A failed page poisons its partial fold. Keep the positive
                // identity in the fair-turn queue, but restart from fresh signed
                // anchors before retrying; never reuse a partially folded page.
                if cursor.read.poisoned {
                    match begin_read(
                        self.state_store.clone(),
                        &cursor.read.transfer,
                        cursor.read.side,
                    ) {
                        Ok(read) => cursor.read = read,
                        Err(error) => {
                            if retain_failed_settlement_reader(&mut cursor.failures) {
                                cursors.readers.insert(key.clone(), cursor);
                                cursors.turn.push_back(key);
                            }
                            return Err(error);
                        }
                    }
                }
                // Hold only this process-local cursor mutex for one bounded CAS
                // object. It reserves the decoder slot until the reader is either
                // requeued or complete; concurrent discovery cannot overfill it.
                let complete = match read_page(&mut cursor.read, 1, 1024 * 1024) {
                    Ok(complete) => complete,
                    Err(error) => {
                        if retain_failed_settlement_reader(&mut cursor.failures) {
                            cursors.readers.insert(key.clone(), cursor);
                            cursors.turn.push_back(key);
                        }
                        return Err(error);
                    }
                };
                if !complete {
                    cursors.readers.insert(key.clone(), cursor);
                    cursors.turn.push_back(key);
                    return Ok(None);
                }
                let transfer = cursor.read.transfer.clone();
                let side = cursor.read.side;
                let candidate = prepare_write(cursor.read)?;
                cursors.publishing_slots += 1;
                (
                    key,
                    ReadyClaimedTransferSettlement {
                        claim: cursor.claim,
                        transfer,
                        side,
                        candidate,
                        failures: 0,
                    },
                )
            }
        };
        // The writer remains the sole admission owner. Its scheduler, exact
        // head and durable-claim checks run again on every retry.
        let outcome = match self.state_store.claimed_running_seat_root_snapshot(&key.0) {
            Ok(Some(claim)) if claim == ready.claim => {
                self.publish_known_ui_transfer_candidate(&ready.candidate, exclusion)
            }
            Ok(_) => Ok(UiTransferSettlementWriteOutcome::Conflict),
            Err(error) => Err(error),
        };
        {
            let mut cursors = self
                .claimed_transfer_settlement_cursors
                .lock()
                .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?;
            cursors.publishing_slots -= 1;
            if outcome.is_err() {
                retain_failed_ready_settlement(&mut cursors, key, ready);
            }
        }
        outcome.map(Some)
    }

    /// Process-local scheduling only; an empty queue never proves absence
    /// from a retained signed seat history.
    pub fn has_pending_claimed_ui_transfer_settlement(&self) -> anyhow::Result<bool> {
        let cursors = self
            .claimed_transfer_settlement_cursors
            .lock()
            .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?;
        Ok(!cursors.turn.is_empty())
    }

    /// Return at most one bounded keyset slice of expired or already-claimed
    /// seat leases. The fixed lexical maximum stops new high IDs from keeping
    /// an old cycle open forever. Every returned row is rechecked by the
    /// existing exact claim path; this cursor grants no reaping authority.
    pub fn next_expired_ui_seat_lease_slice(
        &self,
        cutoff_ms: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        if limit == 0 || limit > 64 {
            bail!("seat lease maintenance slice limit must be 1..=64");
        }
        let mut scan = self
            .claimed_seat_maintenance_scan
            .lock()
            .map_err(|_| anyhow!("claimed seat maintenance scan lock poisoned"))?;
        if scan.is_none() {
            let Some(max_id) = self.state_store.max_seat_lease_id()? else {
                return Ok(Vec::new());
            };
            *scan = Some((max_id, None));
        }
        let (max_id, after_id) = scan.as_ref().expect("scan initialized");
        let page = self.state_store.expired_seat_leases_slice(
            cutoff_ms,
            after_id.as_deref(),
            max_id,
            limit,
        )?;
        if page.exhausted {
            *scan = None;
        } else if let Some(last) = &page.last_examined_id {
            scan.as_mut().expect("scan initialized").1 = Some(last.clone());
        }
        Ok(page.eligible_ids)
    }

    /// Process-local fair starting point for the current claimed-seat slice.
    /// A short GC budget must not restart at the same lexical first seat on
    /// every run. The claimed lease and signed history remain the authority;
    /// this counter merely schedules which root receives the first turn.
    pub fn next_claimed_ui_seat_discovery_start(&self, roots: usize) -> usize {
        claimed_discovery_start(&self.claimed_seat_discovery_start, roots)
    }

    /// Advance one process-local positive-only discovery cursor. The cache is
    /// never a source of negative proof; a restart or stale CAS ancestry
    /// resets its scheduling position and leaves the reaping veto intact.
    pub fn discover_claimed_ui_seat_transfers_for_maintenance(
        &self,
        seat_root_id: &str,
    ) -> anyhow::Result<ClaimedSeatTransferDiscoveryOutcome> {
        self.prune_unclaimed_transfer_scan_cursors_if_full(seat_root_id)?;
        let discovery_claim = self
            .state_store
            .claimed_running_seat_root_snapshot(seat_root_id)?;
        let previous = {
            let mut cursors = self
                .claimed_transfer_scan_cursors
                .lock()
                .map_err(|_| anyhow!("claimed transfer scan cursor lock poisoned"))?;
            let previous = cursors.remove(seat_root_id);
            if previous.is_none() && cursors.len() >= MAX_CLAIMED_TRANSFER_SCAN_CURSORS {
                return Ok(ClaimedSeatTransferDiscoveryOutcome::Conflict);
            }
            previous
        };
        let result = self.discover_claimed_ui_seat_transfers(
            seat_root_id,
            previous,
            MAINTENANCE_DISCOVERY_PAGES,
        );
        if let Ok(outcome) = &result {
            let identities = match outcome {
                ClaimedSeatTransferDiscoveryOutcome::Complete(identities)
                | ClaimedSeatTransferDiscoveryOutcome::Incomplete { identities, .. } => {
                    Some(identities)
                }
                ClaimedSeatTransferDiscoveryOutcome::Conflict => None,
            };
            if let Some(identities) = identities {
                if identities.len() > 1 {
                    bail!("single-event transfer locator returned multiple identities");
                }
            }
            if let Some(transfer) = identities.and_then(|identities| identities.first()) {
                let side = if transfer.source.chain_root_id == seat_root_id
                    && transfer.source.thread_id == seat_root_id
                {
                    TransferSettlementSide::Source
                } else if transfer.destination.chain_root_id == seat_root_id
                    && transfer.destination.thread_id == seat_root_id
                {
                    TransferSettlementSide::Destination
                } else {
                    bail!("claimed seat transfer locator returned another exact seat");
                };
                let current_claim = self
                    .state_store
                    .claimed_running_seat_root_snapshot(seat_root_id)?
                    .ok_or_else(|| anyhow!("claimed transfer discovery lost its exact claim"))?;
                if discovery_claim.as_ref() != Some(&current_claim) {
                    return Ok(ClaimedSeatTransferDiscoveryOutcome::Conflict);
                }
                let mut cursors = self
                    .claimed_transfer_settlement_cursors
                    .lock()
                    .map_err(|_| anyhow!("claimed transfer settlement cursor lock poisoned"))?;
                if cursors.pending.contains_key(seat_root_id)
                    || cursors.pending.len() >= MAX_CLAIMED_TRANSFER_SCAN_CURSORS
                {
                    bail!("claimed transfer pending positive slot is unavailable");
                }
                cursors.pending.insert(
                    seat_root_id.to_owned(),
                    PendingClaimedTransfer {
                        claim: current_claim,
                        transfer: transfer.clone(),
                        side,
                        failures: 0,
                    },
                );
            }
        }
        match result {
            Ok(ClaimedSeatTransferDiscoveryOutcome::Incomplete {
                identities,
                continuation,
            }) => {
                let mut cursors = self
                    .claimed_transfer_scan_cursors
                    .lock()
                    .map_err(|_| anyhow!("claimed transfer scan cursor lock poisoned"))?;
                if cursors.len() >= MAX_CLAIMED_TRANSFER_SCAN_CURSORS {
                    return Ok(ClaimedSeatTransferDiscoveryOutcome::Conflict);
                }
                cursors.insert(seat_root_id.to_owned(), continuation.clone());
                Ok(ClaimedSeatTransferDiscoveryOutcome::Incomplete {
                    identities,
                    continuation,
                })
            }
            Ok(other) => Ok(other),
            // The cursor has already been discarded. Surface the error: a
            // missing old CAS object can be retried from a fresh head, but
            // malformed signed content or I/O must not be hidden as conflict.
            Err(error) => Err(error),
        }
    }

    /// A seat can close through the ordinary API, not only through GC. Its
    /// process-local discovery hint must not consume one of the bounded slots
    /// forever. Inspect only when full; the DB claim is a pruning hint, never
    /// positive settlement evidence. Current-path maintenance holds the GC
    /// flock, so two maintenance scans cannot race this sweep.
    fn prune_unclaimed_transfer_scan_cursors_if_full(
        &self,
        requested_seat_root_id: &str,
    ) -> anyhow::Result<()> {
        let keys = {
            let cursors = self
                .claimed_transfer_scan_cursors
                .lock()
                .map_err(|_| anyhow!("claimed transfer scan cursor lock poisoned"))?;
            if cursors.len() < MAX_CLAIMED_TRANSFER_SCAN_CURSORS
                || cursors.contains_key(requested_seat_root_id)
            {
                return Ok(());
            }
            cursors.keys().cloned().collect::<Vec<_>>()
        };
        for key in keys {
            if !self.state_store.has_claimed_seat_lease(&key)? {
                self.discard_claimed_ui_seat_transfer_scan(&key)?;
            }
        }
        Ok(())
    }

    /// Locate transfer identities on one already-claimed exact running seat.
    /// Recheck the durable claim after every bounded call. A fresh cursor also
    /// rechecks its captured signed head. A resumed cursor may walk that older
    /// immutable head after settlement appends, but its events are only
    /// positive locator hints: the known writer reauthenticates both current
    /// histories, and no result authorizes absence, finalization or close.
    pub fn discover_claimed_ui_seat_transfers(
        &self,
        seat_root_id: &str,
        continuation: Option<ClaimedSeatTransferDiscovery>,
        max_pages: u64,
    ) -> anyhow::Result<ClaimedSeatTransferDiscoveryOutcome> {
        if max_pages == 0 || max_pages > 16 {
            bail!("claimed seat transfer discovery page budget must be 1..=16");
        }
        let current_claim = self
            .state_store
            .claimed_running_seat_root_snapshot(seat_root_id)?;
        let Some(claim) = current_claim else {
            return Ok(ClaimedSeatTransferDiscoveryOutcome::Conflict);
        };
        let resuming = continuation.is_some();
        let (anchor, mut progress) = if let Some(previous) = continuation {
            if !Arc::ptr_eq(&self.state_store, &previous.store)
                || previous.seat_root_id != seat_root_id
                || previous.claim != claim
            {
                return Ok(ClaimedSeatTransferDiscoveryOutcome::Conflict);
            }
            (previous.anchor, Some(previous.progress))
        } else {
            let Some(anchor) = self
                .state_store
                .capture_thread_event_read_anchor(seat_root_id, seat_root_id)?
            else {
                return Ok(ClaimedSeatTransferDiscoveryOutcome::Conflict);
            };
            (anchor, None)
        };
        if anchor.chain_root_id() != seat_root_id || anchor.thread_id() != seat_root_id {
            return Ok(ClaimedSeatTransferDiscoveryOutcome::Conflict);
        }
        let mut identities = Vec::new();
        for _ in 0..max_pages {
            let page = read_signed_seat_transfer_identity_page(
                &self.state_store,
                seat_root_id,
                &anchor,
                progress.as_ref(),
                MAINTENANCE_DISCOVERY_EVENTS_PER_PAGE,
                1024 * 1024,
            )?;
            for identity in page.identities {
                if !identities.contains(&identity) {
                    identities.push(identity);
                }
            }
            progress = page.progress;
            if progress.is_none() {
                break;
            }
        }
        if self
            .state_store
            .claimed_running_seat_root_snapshot(seat_root_id)?
            .as_ref()
            != Some(&claim)
            || (!resuming
                && self
                    .state_store
                    .capture_thread_event_read_anchor(seat_root_id, seat_root_id)?
                    .as_ref()
                    != Some(&anchor))
        {
            return Ok(ClaimedSeatTransferDiscoveryOutcome::Conflict);
        }
        Ok(match progress {
            Some(progress) => ClaimedSeatTransferDiscoveryOutcome::Incomplete {
                identities,
                continuation: ClaimedSeatTransferDiscovery {
                    store: self.state_store.clone(),
                    seat_root_id: seat_root_id.to_string(),
                    claim,
                    anchor,
                    progress,
                },
            },
            None => ClaimedSeatTransferDiscoveryOutcome::Complete(identities),
        })
    }
}

/// Enumerate only the supplied fixed signed head. The caller must retain the
/// anchor and resume with the returned opaque progress until it is `None`;
/// absence in one page is never absence from the seat's history. Decoding is
/// deliberately exact-owner and fail-closed, even for lifecycle testimony.
pub fn read_signed_seat_transfer_identity_page(
    store: &StateStore,
    seat_root_id: &str,
    anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    progress: Option<&ryeos_state::AuthoritativeThreadEventReadProgress>,
    max_events: u64,
    max_bytes: u64,
) -> anyhow::Result<SignedSeatTransferIdentityPage> {
    if anchor.chain_root_id() != seat_root_id || anchor.thread_id() != seat_root_id {
        bail!("transfer enumeration requires an exact seat-root anchor");
    }
    let page = store.read_thread_event_page(anchor, progress, max_events, max_bytes)?;
    if page.events.is_empty() && page.progress.is_some() {
        bail!("signed transfer enumeration page made no progress");
    }
    let mut identities = Vec::new();
    let mut freeze_preparations = Vec::new();
    for (_, event) in page.events {
        if event.chain_root_id != seat_root_id || event.thread_id != seat_root_id {
            bail!("signed transfer enumeration changed exact seat");
        }
        if let Some(identity) = signed_transfer_identity_for_seat(seat_root_id, &event)? {
            if event.event_type == "seat.presentation_operation.prepared" {
                freeze_preparations.push(identity.clone());
            }
            identities.push(identity);
        }
    }
    Ok(SignedSeatTransferIdentityPage {
        freeze_preparations,
        identities,
        progress: page.progress,
    })
}

/// Discover immutable obligations from one marker-bound retired prefix page.
/// This is a locator, not completed lifecycle or browser read authorization.
/// Callers must finish discovery before freezing the reconstruction scope.
pub fn read_retired_seat_transfer_identity_page(
    store: &StateStore,
    prefix: &ryeos_state::RetiredSeatPositivePrefix,
    progress: Option<&ryeos_state::AuthoritativeThreadEventReadProgress>,
    max_events: u64,
    max_bytes: u64,
) -> anyhow::Result<SignedSeatTransferIdentityPage> {
    let authority = store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let page = authority
        .read_retired_seat_positive_prefix_page(prefix, progress, max_events, max_bytes, &guard)?;
    if page.events.is_empty() && page.progress.is_some() {
        bail!("retired transfer enumeration page made no progress");
    }
    let root = prefix.chain_root_id();
    let mut identities = Vec::new();
    let mut freeze_preparations = Vec::new();
    for (_, event) in page.events {
        if event.chain_root_id != root || event.thread_id != root {
            bail!("retired transfer enumeration changed exact seat");
        }
        if let Some(identity) = signed_transfer_identity_for_seat(root, &event)? {
            if event.event_type == "seat.presentation_operation.prepared" {
                freeze_preparations.push(identity.clone());
            }
            identities.push(identity);
        }
    }
    Ok(SignedSeatTransferIdentityPage {
        identities,
        freeze_preparations,
        progress: page.progress,
    })
}

fn signed_transfer_identity_for_seat(
    seat_root_id: &str,
    event: &ryeos_state::objects::thread_event::ThreadEvent,
) -> anyhow::Result<Option<TransferIdentity>> {
    if event.chain_root_id != seat_root_id || event.thread_id != seat_root_id {
        bail!("signed transfer enumeration changed exact seat");
    }
    if event
        .event_type
        .starts_with("contract.ryeos_ui.move_freeze_cancelled")
        && event.event_type
            != ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT
    {
        bail!("signed freeze cancellation enumeration has unsupported testimony");
    }
    // Locator testimony only. Ordinary preparation is authenticated by its
    // receipt and initialization only when the shared fixed scans complete.
    // This positive hint never establishes absence or permits publication.
    let freeze_identity = if event.event_type == "seat.presentation_operation.prepared" {
        let semantic = event
            .payload
            .get("payload")
            .ok_or_else(|| anyhow!("signed preparation semantic payload absent"))?;
        ryeos_ui_presentation_contract::move_freeze_lifecycle::preparation_commitment(semantic)
            .map_err(anyhow::Error::msg)?
            .map(|commitment| commitment.transfer)
    } else if event.event_type
        == ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT
    {
        Some(
            ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(
                &event.to_value(),
            )
            .map_err(anyhow::Error::msg)?
            .transfer,
        )
    } else {
        None
    };
    if let Some(identity) = freeze_identity {
        if identity.source.chain_root_id != seat_root_id
            || identity.source.thread_id != seat_root_id
        {
            bail!("signed freeze locator has another exact source owner");
        }
        return Ok(Some(identity));
    }
    let Some((record, _)) = decode_transfer_phase(event)? else {
        return Ok(None);
    };
    if record.event_owner().chain_root_id != seat_root_id
        || record.event_owner().thread_id != seat_root_id
    {
        bail!("signed transfer phase has another exact seat owner");
    }
    Ok(Some(record.transfer))
}

/// Every candidate additionally requires authenticated complete current-phase
/// proof, the exact owning seat's existing durable producer fence, system writer
/// attribution and an atomic conditional append. These are required checks, not
/// caller-supplied assertions that the checks already happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferSettlementRequirement {
    /// Cancel a destination-only inbound commitment only while exact source
    /// and destination heads are excluded and the complete authenticated
    /// source history proves no Prepared for this operation.
    SerializeOrphanCancellationAndSourcePreparation,
    /// A source abort must not race a commit or late destination acceptance.
    /// An incomplete scan with no Accepted is never a no-reservation proof.
    SerializeSourceDecisionAndDestinationAcceptance,
    /// Retain, Release or Observe only the exact already-established testimony.
    ExactCurrentPhaseAndOwningSeatFence,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TransferSettlementPlan {
    Candidate {
        record: TransferRecord,
        requirement: TransferSettlementRequirement,
    },
    /// Preserve the exact prior outcome; in particular do not relabel Activated
    /// as Retained after session loss. This is not a retirement clearance.
    AlreadyRecorded(TransferEvidence),
    /// A source seat with no Prepared has no transfer obligation to settle.
    AwaitingSourcePreparation,
    AwaitingSourceDecision,
    AwaitingDestinationSettlement,
    /// No destination acceptance appears in this caller-supplied fold. Source
    /// release still requires a complete authenticated no-reservation proof,
    /// serialized against late acceptance. Never manufacture Released/Observed.
    NoAcceptedReservation {
        source_abort: Option<TransferEvidence>,
    },
}

impl super::ThreadLifecycleService {
    /// Publish only a completely authenticated fixed-history candidate. The
    /// bounded maintenance reader constructs it before the scheduler gate;
    /// publication rechecks both heads and the exact durable reaping claim.
    fn publish_known_ui_transfer_candidate(
        &self,
        candidate: &SettlementPreparation,
        exclusion: &SettlementExclusion<'_>,
    ) -> anyhow::Result<UiTransferSettlementWriteOutcome> {
        let _ = exclusion.0;
        let scheduler = self
            .scheduler_db
            .read()
            .map_err(|_| anyhow!("scheduler database lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow!("scheduler database is not attached"))?;
        match &candidate {
            SettlementPreparation::Deferred(TransferSettlementPlan::AlreadyRecorded(evidence)) => {
                return Ok(UiTransferSettlementWriteOutcome::AlreadyRecorded(
                    evidence.clone(),
                ));
            }
            SettlementPreparation::Deferred(plan) => {
                return Ok(UiTransferSettlementWriteOutcome::Deferred(plan.clone()));
            }
            _ => {}
        }
        if !scheduler.fire_projection_is_current()? {
            bail!("scheduler fire projection is not current");
        }
        let append_bound = (*self
            .ui_seat_append_bound
            .read()
            .map_err(|_| anyhow!("UI seat append bound lock poisoned"))?)
        .ok_or_else(|| anyhow!("admitted UI seat append route bound is absent"))?;
        if let SettlementPreparation::FreezeCancellation(proof) = candidate {
            let outcome = self
                .state_store
                .append_reaping_ui_move_freeze_cancellation(proof, append_bound, |thread_ids| {
                    let mut pins = 0u64;
                    for thread_id in thread_ids {
                        if scheduler.find_fire_by_thread(thread_id)?.is_some() {
                            pins = pins
                                .checked_add(1)
                                .ok_or_else(|| anyhow!("scheduler pin count overflow"))?;
                        }
                    }
                    Ok(pins)
                })?;
            return Ok(match outcome {
                crate::state_store::UiMoveFreezeCancellationAppendResult::Committed(records) => {
                    self.publish_records(&records);
                    UiTransferSettlementWriteOutcome::Committed(records)
                }
                crate::state_store::UiMoveFreezeCancellationAppendResult::AlreadyRecorded(
                    evidence,
                ) => UiTransferSettlementWriteOutcome::AlreadyRecorded(evidence),
                crate::state_store::UiMoveFreezeCancellationAppendResult::Conflict => {
                    UiTransferSettlementWriteOutcome::Conflict
                }
            });
        }
        let committed = self.state_store.append_reaping_ui_transfer_settlement_v2(
            candidate,
            append_bound,
            |thread_ids| {
                let mut pins = 0u64;
                for thread_id in thread_ids {
                    if scheduler.find_fire_by_thread(thread_id)?.is_some() {
                        pins = pins
                            .checked_add(1)
                            .ok_or_else(|| anyhow!("scheduler pin count overflow"))?;
                    }
                }
                Ok(pins)
            },
        )?;
        Ok(match committed {
            crate::state_store::RunningConditionalEventAppendResult::Committed(records) => {
                self.publish_records(&records);
                UiTransferSettlementWriteOutcome::Committed(records)
            }
            crate::state_store::RunningConditionalEventAppendResult::Conflict => {
                UiTransferSettlementWriteOutcome::Conflict
            }
            crate::state_store::RunningConditionalEventAppendResult::NotRunning => {
                UiTransferSettlementWriteOutcome::Conflict
            }
        })
    }

    /// Select data-only testimony from a consistency fold. This does not assert
    /// the supplied fold is current/authenticated, and does not write anything.
    /// There is intentionally no boolean or public token that claims the seat
    /// is already fenced. The known-transfer writer performs its own shared
    /// fence admission; source-absence release remains unavailable.
    pub fn plan_ui_transfer_settlement(
        &self,
        state: &TransferLifecycleState,
        side: TransferSettlementSide,
    ) -> Result<TransferSettlementPlan, String> {
        select_settlement(state, side)
    }

    /// Start a read-only fixed-history check. No session, producer, lease or
    /// head is created. Both exact heads must exist; absence is never settlement
    /// evidence. Full CAS ancestry is still not domain fence authentication.
    pub fn begin_closed_ui_transfer_settlement(
        &self,
        transfer: &TransferIdentity,
        side: TransferSettlementSide,
    ) -> anyhow::Result<UiTransferSettlementRead> {
        begin_read(self.state_store.clone(), transfer, side)
    }

    /// Authenticate one owner-budgeted page, outside the StateStore mutation
    /// lock. True means both fixed histories reached genesis. A partial read
    /// cannot authorize an append, and a failed read is permanently poisoned.
    pub fn read_closed_ui_transfer_settlement_page(
        &self,
        read: &mut UiTransferSettlementRead,
        max_events: u64,
        max_bytes: u64,
    ) -> anyhow::Result<bool> {
        if !Arc::ptr_eq(&self.state_store, &read.store) {
            bail!("transfer settlement read belongs to another state owner");
        }
        read_page(read, max_events, max_bytes)
    }

    /// Finish the read-only consistency check. An authenticated Closed control
    /// still lacks expiry provenance, and runtime admission must serialize with
    /// publication. Candidate results remain explicitly blocked and cannot grant
    /// permission to append, finalize or retire either head.
    pub fn finish_closed_ui_transfer_settlement(
        &self,
        read: UiTransferSettlementRead,
    ) -> anyhow::Result<UiTransferSettlementOutcome> {
        if !Arc::ptr_eq(&self.state_store, &read.store) {
            bail!("transfer settlement read belongs to another state owner");
        }
        match prepare_write(read)? {
            SettlementPreparation::FreezeCancellation(_) => {
                Ok(UiTransferSettlementOutcome::BlockedFreezeCancellation)
            }
            SettlementPreparation::Deferred(plan) => {
                Ok(UiTransferSettlementOutcome::Deferred(plan))
            }
            SettlementPreparation::Candidate(candidate) => {
                Ok(UiTransferSettlementOutcome::Blocked(candidate.payload))
            }
            SettlementPreparation::ReapingCandidate(candidate) => {
                Ok(UiTransferSettlementOutcome::BlockedReaping {
                    record: candidate.record,
                    latest_control: candidate.latest_control,
                })
            }
        }
    }
}

#[derive(Debug)]
pub enum UiTransferSettlementWriteOutcome {
    Committed(Vec<crate::state_store::PersistedEventRecord>),
    AlreadyRecorded(TransferEvidence),
    Deferred(TransferSettlementPlan),
    Conflict,
}

/// Opaque, in-memory progress owned by this lifecycle service. Its evidence is
/// reconstructed after a crash; this is not another durable transfer registry.
pub struct UiTransferSettlementRead {
    store: Arc<StateStore>,
    transfer: TransferIdentity,
    side: TransferSettlementSide,
    source: UiTransferFixedSeatRead,
    destination: UiTransferFixedSeatRead,
    poisoned: bool,
}

#[derive(Debug)]
pub enum UiTransferSettlementOutcome {
    /// Authenticated negative commitment only; publication still requires
    /// the owning source's durable reaping claim and ordered pin exclusions.
    BlockedFreezeCancellation,
    /// Consistent fixed-history candidate, not permission to append. Gate R is
    /// still missing the approved lifecycle expiry fence and runtime-owner
    /// exclusion through publication.
    Blocked(TransferLifecycleEventPayload),
    /// A pure reaping candidate; only the lifecycle writer may read the stable
    /// first claim timestamp and construct v2 testimony under publication locks.
    BlockedReaping {
        record: TransferRecord,
        latest_control: TransferEvidence,
    },
    /// Includes already-recorded outcomes and precommit/absence cases that this
    /// writer deliberately cannot settle. It grants no retirement permission.
    Deferred(TransferSettlementPlan),
}

struct SeatHistory<A = ryeos_state::AuthoritativeThreadEventReadAnchor> {
    seat: TransferSeat,
    anchor: A,
    progress: Option<ryeos_state::AuthoritativeThreadEventReadProgress>,
    complete: bool,
    previous_chain_seq: Option<u64>,
    previous_thread_seq: Option<u64>,
    latest_control: Option<(TransferEvidence, UiSeatProducer)>,
    newer_control: Option<UiSeatProducer>,
    frontier: Option<u64>,
    interval_producer: Option<String>,
    pending_receipt: Option<(
        SeatAppendReceipt,
        Vec<ryeos_state::objects::thread_event::ThreadEvent>,
    )>,
    pending_receipt_bytes: u64,
    pending_receipt_evidence: Option<TransferEvidence>,
    validated_receipt: Option<(TransferEvidence, SeatAppendReceipt)>,
    validated_receipt_events: Vec<ryeos_state::objects::thread_event::ThreadEvent>,
    pending_cancellation_control:
        Option<ryeos_ui_presentation_contract::transfer_lifecycle_v2::TransferLifecycleFenceV2>,
    initialization_seen: bool,
    original_append_bound: Option<u64>,
    initialized_root_binding: Option<serde_json::Value>,
    initialized_payload: Option<serde_json::Value>,
    #[cfg(test)]
    phases: Vec<RetainedPhase>,
}

#[derive(Clone)]
struct RetainedPhase {
    evidence: TransferEvidence,
    record: TransferRecord,
    lifecycle: Option<RetainedLifecycle>,
}

#[derive(Clone)]
enum RetainedLifecycle {
    V1(TransferLifecycleEventPayload),
    V2(TransferLifecycleEventPayloadV2),
}

/// Shared closed-vocabulary decoding for positive discovery and authenticated
/// reconstruction. Foreign activity is not a phase; unknown owned versions
/// are never an absence/no-op result.
fn decode_transfer_phase(
    event: &ryeos_state::objects::thread_event::ThreadEvent,
) -> anyhow::Result<Option<(TransferRecord, Option<RetainedLifecycle>)>> {
    if !ryeos_ui_presentation_contract::transfer::is_transfer_testimony(&event.event_type) {
        return Ok(None);
    }
    let value = event.to_value();
    let phase = match event.event_type.as_str() {
        TRANSFER_EVENT => (
            ryeos_ui_presentation_contract::transfer::from_event_value(&value)
                .map_err(anyhow::Error::msg)?
                .payload,
            None,
        ),
        TRANSFER_LIFECYCLE_EVENT => {
            let payload =
                ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(&value)
                    .map_err(anyhow::Error::msg)?;
            (
                payload.payload.clone(),
                Some(RetainedLifecycle::V1(payload)),
            )
        }
        TRANSFER_LIFECYCLE_V2_EVENT => {
            let payload =
                ryeos_ui_presentation_contract::transfer_lifecycle_v2::from_event_value(&value)
                    .map_err(anyhow::Error::msg)?;
            (
                payload.payload.clone(),
                Some(RetainedLifecycle::V2(payload)),
            )
        }
        _ => bail!("transfer history encountered unsupported testimony"),
    };
    Ok(Some(phase))
}

impl RetainedPhase {
    fn testimony(&self) -> TransferTestimony<'_> {
        TransferTestimony {
            evidence: &self.evidence,
            record: &self.record,
        }
    }
}

/// Only the complete CAS reader can construct this candidate. Its original
/// anchors support storage consistency checks, not domain fence admission.
/// In particular authenticated Closed control does not establish session expiry
/// or prove its original writer admission.
pub(crate) struct ClosedTransferSettlementCandidate {
    store: Arc<StateStore>,
    payload: TransferLifecycleEventPayload,
    owner_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    counterpart_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    original_append_bound: u64,
}

impl ClosedTransferSettlementCandidate {
    pub(crate) fn belongs_to(&self, store: &StateStore) -> bool {
        std::ptr::eq(self.store.as_ref(), store)
    }
    pub(crate) fn payload(&self) -> &TransferLifecycleEventPayload {
        &self.payload
    }
    pub(crate) fn owner_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.owner_anchor
    }
    pub(crate) fn counterpart_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.counterpart_anchor
    }
    pub(crate) fn original_append_bound(&self) -> u64 {
        self.original_append_bound
    }
}

pub(crate) enum SettlementPreparation {
    FreezeCancellation(UiMoveFreezeCancellationProof),
    Candidate(ClosedTransferSettlementCandidate),
    ReapingCandidate(ReapingTransferSettlementCandidate),
    Deferred(TransferSettlementPlan),
}

pub(crate) struct ReapingTransferSettlementCandidate {
    store: Arc<StateStore>,
    record: TransferRecord,
    latest_control: TransferEvidence,
    closed_control: bool,
    owner_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    counterpart_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    original_append_bound: u64,
}

impl ReapingTransferSettlementCandidate {
    pub(crate) fn belongs_to(&self, store: &StateStore) -> bool {
        std::ptr::eq(self.store.as_ref(), store)
    }

    pub(crate) fn record(&self) -> &TransferRecord {
        &self.record
    }

    pub(crate) fn latest_control(&self) -> &TransferEvidence {
        &self.latest_control
    }

    pub(crate) fn closed_control(&self) -> bool {
        self.closed_control
    }

    pub(crate) fn owner_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.owner_anchor
    }

    pub(crate) fn counterpart_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.counterpart_anchor
    }
    pub(crate) fn original_append_bound(&self) -> u64 {
        self.original_append_bound
    }
}

pub(crate) fn begin_read(
    store: Arc<StateStore>,
    transfer: &TransferIdentity,
    side: TransferSettlementSide,
) -> anyhow::Result<UiTransferSettlementRead> {
    if transfer.source.chain_root_id == transfer.destination.chain_root_id
        || transfer.source.thread_id == transfer.destination.thread_id
        || transfer.source.presentation_id == transfer.destination.presentation_id
    {
        bail!("transfer settlement requires two distinct retained seats");
    }
    let capture = |seat: &TransferSeat| {
        store
            .capture_thread_event_read_anchor(&seat.chain_root_id, &seat.thread_id)?
            .ok_or_else(|| anyhow!("transfer settlement requires both retained signed heads"))
    };
    // Capture BOTH cuts before reading either history. The shared scanner
    // authenticates ordinary freeze receipts in the same bounded page owner.
    let source_anchor = capture(&transfer.source)?;
    let destination_anchor = capture(&transfer.destination)?;
    let source = begin_fixed_seat_read(
        store.clone(),
        source_anchor,
        &transfer.source,
        std::slice::from_ref(transfer),
    )?;
    let destination = begin_fixed_seat_read(
        store.clone(),
        destination_anchor,
        &transfer.destination,
        std::slice::from_ref(transfer),
    )?;
    Ok(UiTransferSettlementRead {
        store,
        transfer: transfer.clone(),
        side,
        source,
        destination,
        poisoned: false,
    })
}

pub(crate) fn read_page(
    read: &mut UiTransferSettlementRead,
    max_events: u64,
    max_bytes: u64,
) -> anyhow::Result<bool> {
    if read.poisoned {
        bail!("transfer settlement read previously failed");
    }
    let result = (|| {
        let history = if !read.source.history.complete {
            &mut read.source
        } else if !read.destination.history.complete {
            &mut read.destination
        } else {
            return Ok(true);
        };
        read_fixed_seat_page(history, max_events, max_bytes)?;
        Ok(read.source.history.complete && read.destination.history.complete)
    })();
    if result.is_err() {
        read.poisoned = true;
    }
    result
}

impl<A> SeatHistory<A> {
    fn fresh(seat: TransferSeat, anchor: A) -> Self {
        Self {
            seat,
            anchor,
            progress: None,
            complete: false,
            previous_chain_seq: None,
            previous_thread_seq: None,
            latest_control: None,
            newer_control: None,
            frontier: None,
            interval_producer: None,
            pending_receipt: None,
            pending_receipt_bytes: 0,
            pending_receipt_evidence: None,
            validated_receipt: None,
            validated_receipt_events: Vec::new(),
            pending_cancellation_control: None,
            initialization_seen: false,
            original_append_bound: None,
            initialized_root_binding: None,
            initialized_payload: None,
            #[cfg(test)]
            phases: Vec::new(),
        }
    }

    #[cfg(test)]
    fn push(
        &mut self,
        transfer: &TransferIdentity,
        hash: String,
        event: ryeos_state::objects::thread_event::ThreadEvent,
    ) -> anyhow::Result<()> {
        if let Some(phase) = self.validate_seat_event(hash, event)? {
            self.route_operation_phase(transfer, phase)?;
        }
        Ok(())
    }

    /// Advance the exact-seat control/receipt/initialization reconstruction
    /// independently of operation selection. A decoded phase is provisional:
    /// the fixed CAS scan must still finish and verify its lifecycle fences.
    /// No phase slots or operation-specific identity checks happen here.
    fn validate_seat_event(
        &mut self,
        hash: String,
        event: ryeos_state::objects::thread_event::ThreadEvent,
    ) -> anyhow::Result<Option<RetainedPhase>> {
        if event.chain_root_id != self.seat.chain_root_id
            || event.thread_id != self.seat.thread_id
            || self
                .previous_chain_seq
                .is_some_and(|previous| event.chain_seq >= previous)
            || self
                .previous_thread_seq
                .is_some_and(|previous| event.thread_seq.checked_add(1) != Some(previous))
        {
            bail!("transfer history changed exact seat or adjacent thread order");
        }
        if self.initialization_seen
            && (event.event_type.starts_with("seat.")
                || event.event_type.starts_with("ui_seat.")
                || event.event_type == TRANSFER_EVENT
                || event.event_type == TRANSFER_LIFECYCLE_EVENT
                || event.event_type == TRANSFER_LIFECYCLE_V2_EVENT
                || event
                    .event_type
                    .starts_with("contract.ryeos_ui.move_freeze_cancelled"))
        {
            bail!("signed seat activity precedes its initialization");
        }
        let newer_chain_seq = self.previous_chain_seq;
        self.validated_receipt_events.clear();
        self.previous_chain_seq = Some(event.chain_seq);
        self.previous_thread_seq = Some(event.thread_seq);
        let evidence = TransferEvidence {
            event_hash: hash,
            chain_seq: event.chain_seq,
        };
        if let Some((receipt, events)) = self.pending_receipt.as_mut() {
            if !event.event_type.starts_with("seat.")
                || events.len() >= 128
                || newer_chain_seq != event.chain_seq.checked_add(1)
            {
                bail!("seat receipt interval is interrupted or oversized");
            }
            let bytes = lillux::canonical_json(&event.to_value())?.len() as u64;
            self.pending_receipt_bytes =
                charge_pending_receipt_bytes(self.pending_receipt_bytes, bytes)?;
            events.push(event);
            if events.len() == receipt.event_count {
                self.finish_receipt()?;
            }
            return Ok(None);
        }
        if event.event_type == "ui_seat.append_receipt.v2" {
            let receipt: SeatAppendReceipt = serde_json::from_value(event.payload)
                .context("decode CAS-authenticated seat append receipt")?;
            if receipt.schema_version != SEAT_APPEND_RECEIPT_SCHEMA
                || receipt.event_count == 0
                || receipt.event_count > 128
            {
                bail!("invalid seat append receipt version or bound");
            }
            let identity = UiSeatAppendIdentity {
                presentation_id: self.seat.presentation_id.clone(),
                chain_root_id: self.seat.chain_root_id.clone(),
                thread_id: self.seat.thread_id.clone(),
                producer_incarnation: receipt.producer_incarnation.clone(),
                first_engine_seq: receipt.first_engine_seq,
                last_engine_seq: receipt.last_engine_seq,
                event_count: receipt.event_count as u64,
                payload_digest: receipt.payload_digest.clone(),
            };
            if identity.derive_identity().map_err(anyhow::Error::msg)? != receipt.operation_id {
                bail!("seat append receipt identity differs from its exact seat interval");
            }
            self.pending_receipt_evidence = Some(evidence);
            self.pending_receipt = Some((receipt, Vec::with_capacity(128)));
            self.pending_receipt_bytes = 0;
            return Ok(None);
        }
        if event.event_type.starts_with("seat.")
            || event.event_type.starts_with("ui_seat.append_receipt.")
        {
            bail!("unreceipted or unsupported predecessor seat activity");
        }
        if event.event_type == SEAT_PRODUCER_EVENT {
            let control: UiSeatProducer = serde_json::from_value(event.payload)
                .context("decode CAS-authenticated seat producer control")?;
            control.validate().map_err(anyhow::Error::msg)?;
            control
                .validate_subject(&self.seat.chain_root_id, &self.seat.thread_id)
                .map_err(anyhow::Error::msg)?;
            if let Some(fence) = self.pending_cancellation_control.take() {
                ryeos_ui_presentation_contract::transfer_lifecycle_v2::verify_control_fence(
                    &self.seat, &fence, &evidence, &control,
                )
                .map_err(anyhow::Error::msg)?;
            }
            if self.latest_control.is_none() {
                if self.frontier.is_some() && self.frontier != Some(control.next_engine_seq) {
                    bail!("newest producer control disagrees with retained receipt frontier");
                }
                self.latest_control = Some((evidence.clone(), control.clone()));
            } else if let Some(newer) = self.newer_control.as_ref() {
                if self.frontier != Some(control.next_engine_seq) {
                    bail!("control transition breaks reconstructed engine frontier");
                }
                let predecessor = UiSeatControlPredecessor::Control {
                    event_hash: evidence.event_hash.clone(),
                };
                authenticate_control_transition(
                    newer,
                    Some(&control),
                    &predecessor,
                    &self.seat.presentation_id,
                    &self.seat.chain_root_id,
                    &self.seat.thread_id,
                    newer.next_engine_seq,
                )
                .map_err(anyhow::Error::msg)?;
            }
            if let Some(producer) = self.interval_producer.take() {
                if control.state != UiSeatProducerState::Active
                    || control.producer_incarnation != producer
                {
                    bail!("receipt interval has no owning active producer");
                }
            }
            self.frontier = Some(control.next_engine_seq);
            self.newer_control = Some(control);
            return Ok(None);
        }
        if event.event_type.starts_with("ui_seat.producer.") {
            bail!("unsupported predecessor producer-control history");
        }
        if event.event_type == "ui_seat.presentation_initialized.v1" {
            if self.initialization_seen {
                bail!("duplicate signed seat initialization");
            }
            // This lifecycle reader proves the exact signed predecessor,
            // presentation subject and original root descriptor append bound.
            // Full composition validation remains with the UI seat owner.
            let initial = UiPresentationInitializationBound::from_payload(
                &event.payload,
                &self.seat.presentation_id,
            )
            .map_err(anyhow::Error::msg)?;
            self.original_append_bound = Some(initial.max_seat_append_bytes);
            self.initialized_payload = Some(
                event
                    .payload
                    .get("initialization")
                    .cloned()
                    .ok_or_else(|| anyhow!("original signed initialization absent"))?,
            );
            self.initialized_root_binding = Some(
                event
                    .payload
                    .pointer("/initialization/root_binding")
                    .cloned()
                    .ok_or_else(|| anyhow!("signed initialization root binding absent"))?,
            );
            let newer = self
                .newer_control
                .as_ref()
                .ok_or_else(|| anyhow!("seat history has no producer control"))?;
            let predecessor = UiSeatControlPredecessor::Initialization {
                event_hash: evidence.event_hash,
            };
            authenticate_control_transition(
                newer,
                None,
                &predecessor,
                &self.seat.presentation_id,
                &self.seat.chain_root_id,
                &self.seat.thread_id,
                newer.next_engine_seq,
            )
            .map_err(anyhow::Error::msg)?;
            if self.frontier != Some(0) {
                bail!("initial producer frontier is not zero");
            }
            self.initialization_seen = true;
            return Ok(None);
        }
        if event
            .event_type
            .starts_with("contract.ryeos_ui.move_freeze_cancelled")
        {
            let cancellation =
                ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(
                    &event.to_value(),
                )
                .map_err(anyhow::Error::msg)?;
            if !same_stable_seat(&cancellation.transfer.source, &self.seat) {
                bail!("freeze cancellation belongs to another stable source seat");
            }
            self.authenticate_cancellation_witness(&cancellation)?;
            return Ok(None);
        }
        let Some((record, lifecycle)) = decode_transfer_phase(&event)? else {
            return Ok(None);
        };
        if !same_stable_seat(record.event_owner(), &self.seat) {
            bail!("transfer testimony belongs to another stable seat");
        }
        Ok(Some(RetainedPhase {
            evidence,
            record,
            lifecycle,
        }))
    }

    fn authenticate_cancellation_witness(
        &mut self,
        cancellation: &ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
    ) -> anyhow::Result<()> {
        use ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellationWriter;
        match &cancellation.writer {
            MoveFreezeCancellationWriter::CurrentProducer {
                producer_incarnation,
                next_engine_seq,
            } => {
                if self
                    .frontier
                    .is_some_and(|frontier| frontier != *next_engine_seq)
                    || self
                        .interval_producer
                        .as_ref()
                        .is_some_and(|incarnation| incarnation != producer_incarnation)
                {
                    bail!("cancellation producer witness contradicts its event-time frontier");
                }
                // Nonadvancing pending constraints. Older validated receipts
                // and an ACTIVE historical control must independently close
                // these claims before any completed proof is issued.
                self.frontier = Some(*next_engine_seq);
                self.interval_producer = Some(producer_incarnation.clone());
            }
            MoveFreezeCancellationWriter::NodeLifecycle { fence } => {
                if self
                    .pending_cancellation_control
                    .as_ref()
                    .is_some_and(|prior| prior != fence)
                {
                    bail!("cancellations contradict the same historical control interval");
                }
                self.pending_cancellation_control = Some(fence.clone());
            }
        }
        Ok(())
    }

    /// Retain only the selected operation under its existing bounded phase
    /// policy. Shared decoding checks the outer phase-owner thread/root;
    /// only this routing step binds the full selected two-seat identity,
    /// including presentation, producer incarnation and origin revision.
    #[cfg(test)]
    fn route_operation_phase(
        &mut self,
        transfer: &TransferIdentity,
        phase: RetainedPhase,
    ) -> anyhow::Result<()> {
        route_phase_into_slots(transfer, &self.seat, &mut self.phases, phase)
    }

    fn finish_receipt(&mut self) -> anyhow::Result<()> {
        let (receipt, mut events) = self
            .pending_receipt
            .take()
            .ok_or_else(|| anyhow!("missing seat receipt"))?;
        self.pending_receipt_bytes = 0;
        events.reverse();
        let semantic = events
            .iter()
            .map(|event| {
                let engine_seq = event
                    .payload
                    .get("seq")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| anyhow!("seat event lacks engine sequence"))?;
                let payload = event
                    .payload
                    .get("payload")
                    .ok_or_else(|| anyhow!("seat event lacks semantic payload"))?;
                Ok(SeatAppendSemanticEvent {
                    engine_seq,
                    event_type: &event.event_type,
                    payload,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        receipt
            .validate_semantic_interval(&semantic, 128)
            .map_err(anyhow::Error::msg)?;
        if self.frontier.is_some() && self.frontier != receipt.last_engine_seq.checked_add(1) {
            bail!("receipt interval breaks authenticated producer frontier");
        }
        if self
            .interval_producer
            .as_ref()
            .is_some_and(|prior| prior != &receipt.producer_incarnation)
        {
            bail!("receipt intervals switch producer without control");
        }
        self.validated_receipt = Some((
            self.pending_receipt_evidence
                .take()
                .ok_or_else(|| anyhow!("validated receipt has no exact signed evidence"))?,
            receipt.clone(),
        ));
        self.interval_producer = Some(receipt.producer_incarnation);
        self.frontier = Some(receipt.first_engine_seq);
        self.validated_receipt_events = events;
        Ok(())
    }

    fn finish(&self) -> anyhow::Result<()> {
        if self.pending_receipt.is_some()
            || self.pending_cancellation_control.is_some()
            || !self.initialization_seen
            || self.original_append_bound.is_none()
            || self.interval_producer.is_some()
        {
            bail!("seat history ends with incomplete receipt or control ancestry");
        }
        Ok(())
    }
}

fn route_phase_into_slots(
    transfer: &TransferIdentity,
    owner: &TransferSeat,
    phases: &mut Vec<RetainedPhase>,
    phase: RetainedPhase,
) -> anyhow::Result<()> {
    let record = &phase.record;
    if record.transfer.operation_id != transfer.operation_id {
        return Ok(());
    }
    if record.transfer != *transfer || record.event_owner() != owner {
        bail!("transfer operation changed identity or exact phase owner");
    }
    if phases.iter().any(|prior| {
        std::mem::discriminant(&prior.record.phase) == std::mem::discriminant(&record.phase)
    }) {
        bail!("transfer history repeats a settled phase");
    }
    // Eight ordinary phases plus the inbound; OrphanCancelled is
    // exclusive with Prepared and all its successors.
    if phases.len() >= 9 {
        bail!("transfer history exceeds bounded phase slots");
    }
    phases.push(phase);
    Ok(())
}

fn verify_phase_slice_fences(
    latest_control: Option<&(TransferEvidence, UiSeatProducer)>,
    phases: &[RetainedPhase],
) -> anyhow::Result<()> {
    for phase in phases {
        if let Some(lifecycle) = &phase.lifecycle {
            let (fence, control) = latest_control
                .ok_or_else(|| anyhow!("lifecycle transfer has no retained producer fence"))?;
            match lifecycle {
                RetainedLifecycle::V1(payload) => {
                    verify_closed_producer_fence(payload, fence, control)
                        .map_err(anyhow::Error::msg)?
                }
                RetainedLifecycle::V2(payload) => {
                    verify_latest_control(payload, fence, control).map_err(anyhow::Error::msg)?
                }
            }
            if fence.chain_seq >= phase.evidence.chain_seq {
                bail!("lifecycle transfer precedes its producer fence");
            }
        } else if latest_control.is_some_and(|(fence, control)| {
            control.state
                == ryeos_ui_presentation_contract::seat_control::UiSeatProducerState::Closed
                && phase.evidence.chain_seq > fence.chain_seq
        }) {
            bail!("browser transfer testimony follows its Closed producer fence");
        }
    }
    Ok(())
}

fn phase_order(phase: &TransferPhase) -> u8 {
    match phase {
        TransferPhase::InboundCommitment => 0,
        TransferPhase::OrphanCancelled { .. } | TransferPhase::Prepared { .. } => 1,
        TransferPhase::Accepted { .. } => 2,
        TransferPhase::Committed { .. } | TransferPhase::Aborted { .. } => 3,
        TransferPhase::Activated { .. }
        | TransferPhase::Retained { .. }
        | TransferPhase::Released { .. } => 4,
        TransferPhase::SettlementObserved { .. } => 5,
    }
}

fn require_complete_settlement_read(
    poisoned: bool,
    source: bool,
    destination: bool,
) -> anyhow::Result<()> {
    if poisoned || !source || !destination {
        bail!("transfer settlement requires both complete authenticated histories");
    }
    Ok(())
}

fn fold_transfer_phase_slices(
    transfer: &TransferIdentity,
    source: &[RetainedPhase],
    destination: &[RetainedPhase],
) -> anyhow::Result<TransferLifecycleState> {
    let mut phases: Vec<_> = source.iter().chain(destination).collect();
    // Topological consistency order only, never global publication order.
    // Prepared links the exact destination inbound commitment. Typed Commit
    // evidence authenticates the exact Accepted object, but Abort links only
    // Prepared: sorting cannot prove an Accepted did not arrive
    // after Abort. Source Abort therefore needs both fixed heads unchanged at
    // publication under the ordered two-chain locks.
    phases.sort_by_key(|phase| phase_order(&phase.record.phase));
    let first = phases
        .first()
        .ok_or_else(|| anyhow!("transfer inbound commitment is absent"))?;
    let mut state = TransferLifecycleState::begin(first.testimony()).map_err(anyhow::Error::msg)?;
    let mut last_source = None;
    let mut last_destination = None;
    for phase in phases {
        let previous = if phase.record.event_owner() == &transfer.source {
            &mut last_source
        } else {
            &mut last_destination
        };
        if previous.is_some_and(|previous| phase.evidence.chain_seq <= previous) {
            bail!("transfer phase order contradicts its owning chain");
        }
        *previous = Some(phase.evidence.chain_seq);
        state.fold(phase.testimony()).map_err(anyhow::Error::msg)?;
    }
    Ok(state)
}

/// Complete, immutable, two-seat CAS proof at the exact two captured signed
/// heads. This is read-only and carries no live session, producer, reservation
/// or publication authority. A writer must condition on BOTH returned anchors
/// and independently recheck its mutable domain admission before publication.
pub struct UiTransferFixedHistoryProof {
    store: Arc<StateStore>,
    source_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    destination_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    lifecycle: TransferLifecycleState,
}

/// Completed paired absence plus exact receipted freeze witnesses. This is
/// read testimony only: publication must recheck both anchors and independently
/// admit the current producer or durable lifecycle fence under existing locks.
pub struct UiMoveFreezeCancellationProof {
    store: Arc<StateStore>,
    source_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    destination_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    transfer: TransferIdentity,
    freeze: ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellationEvidence,
    retained: Option<(
        TransferEvidence,
        ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
    )>,
    destination_inbound: Option<(TransferEvidence, TransferRecord)>,
    destination_orphan_cancelled: Option<(TransferEvidence, TransferRecord)>,
    original_append_bound: u64,
    latest_control: (TransferEvidence, UiSeatProducer),
    next_engine_seq: u64,
}

impl UiMoveFreezeCancellationProof {
    /// Positive orphan decision from the completed exact destination prefix.
    /// It never substitutes for the paired source-no-Prepared coverage.
    pub fn destination_orphan_cancelled(&self) -> Option<(&TransferEvidence, &TransferRecord)> {
        self.destination_orphan_cancelled
            .as_ref()
            .map(|(evidence, record)| (evidence, record))
    }
    pub fn original_append_bound(&self) -> u64 {
        self.original_append_bound
    }
    pub fn latest_control(&self) -> (&TransferEvidence, &UiSeatProducer) {
        (&self.latest_control.0, &self.latest_control.1)
    }
    pub fn next_engine_seq(&self) -> u64 {
        self.next_engine_seq
    }
    pub fn belongs_to(&self, store: &StateStore) -> bool {
        std::ptr::eq(self.store.as_ref(), store)
    }
    pub fn source_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.source_anchor
    }
    pub fn destination_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.destination_anchor
    }
    pub fn transfer(&self) -> &TransferIdentity {
        &self.transfer
    }
    pub fn freeze(
        &self,
    ) -> &ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellationEvidence
    {
        &self.freeze
    }
    pub fn retained_cancellation(
        &self,
    ) -> Option<(
        &TransferEvidence,
        &ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
    )> {
        self.retained
            .as_ref()
            .map(|(evidence, record)| (evidence, record))
    }
    pub fn destination_inbound(&self) -> Option<(&TransferEvidence, &TransferRecord)> {
        self.destination_inbound
            .as_ref()
            .map(|(evidence, record)| (evidence, record))
    }
}

struct RequestedSeatOperation {
    freeze_witnesses: BTreeMap<String, UiHistoricalReceiptedFreezeWitness>,
    transfer: TransferIdentity,
    owner: TransferSeat,
    phases: Vec<RetainedPhase>,
    freeze_preparation: Option<(
        TransferEvidence,
        ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezePreparationCommitment,
    )>,
    freeze_completion: Option<(TransferEvidence, serde_json::Value)>,
    cancellation: Option<(
        TransferEvidence,
        ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
    )>,
}

fn same_stable_seat(left: &TransferSeat, right: &TransferSeat) -> bool {
    left.presentation_id == right.presentation_id
        && left.chain_root_id == right.chain_root_id
        && left.thread_id == right.thread_id
}

fn freeze_requested_seat_operations(
    seat: &TransferSeat,
    requested: &[TransferIdentity],
) -> anyhow::Result<BTreeMap<String, RequestedSeatOperation>> {
    let mut operations: BTreeMap<String, RequestedSeatOperation> = BTreeMap::new();
    for transfer in requested {
        TransferRecord {
            schema: ryeos_ui_presentation_contract::transfer::TRANSFER_SCHEMA,
            transfer: transfer.clone(),
            phase: TransferPhase::InboundCommitment,
        }
        .validate_shape()
        .map_err(anyhow::Error::msg)?;
        let owner = if same_stable_seat(seat, &transfer.source) {
            &transfer.source
        } else if same_stable_seat(seat, &transfer.destination) {
            &transfer.destination
        } else {
            bail!("requested transfer does not belong to the exact fixed seat");
        };
        if let Some(prior) = operations.get(&transfer.operation_id) {
            if prior.transfer != *transfer {
                bail!("requested transfer operation has conflicting identities");
            }
            continue;
        }
        operations.insert(
            transfer.operation_id.clone(),
            RequestedSeatOperation {
                freeze_witnesses: BTreeMap::new(),
                transfer: transfer.clone(),
                owner: owner.clone(),
                phases: Vec::new(),
                freeze_preparation: None,
                freeze_completion: None,
                cancellation: None,
            },
        );
    }
    Ok(operations)
}

fn route_requested_seat_phase(
    requested: &mut BTreeMap<String, RequestedSeatOperation>,
    phase: RetainedPhase,
) -> anyhow::Result<()> {
    if let Some(operation) = requested.get_mut(&phase.record.transfer.operation_id) {
        route_phase_into_slots(
            &operation.transfer,
            &operation.owner,
            &mut operation.phases,
            phase,
        )?;
    }
    Ok(())
}

fn route_receipted_freeze_event(
    requested: &mut BTreeMap<String, RequestedSeatOperation>,
    event: &ryeos_state::objects::thread_event::ThreadEvent,
    receipt_producer: &str,
    receipt: &(TransferEvidence, SeatAppendReceipt),
) -> anyhow::Result<()> {
    use ryeos_ui_presentation_contract::move_freeze_lifecycle::{
        preparation_commitment, validate_completion_commitment,
    };
    let semantic = event
        .payload
        .get("payload")
        .ok_or_else(|| anyhow!("receipted producer semantic payload absent"))?;
    let evidence = || -> anyhow::Result<TransferEvidence> {
        Ok(TransferEvidence {
            event_hash: lillux::sha256_hex(lillux::canonical_json(&event.to_value())?.as_bytes()),
            chain_seq: event.chain_seq,
        })
    };
    if event.event_type == "seat.presentation_operation.prepared" {
        let Some(commitment) = preparation_commitment(semantic).map_err(anyhow::Error::msg)? else {
            return Ok(());
        };
        let Some(operation) = requested.get_mut(&commitment.transfer.operation_id) else {
            return Ok(());
        };
        if commitment.transfer != operation.transfer
            || operation.owner != operation.transfer.source
            || commitment.transfer.source.producer_incarnation != receipt_producer
            || event.payload.get("seq").and_then(serde_json::Value::as_u64)
                != Some(commitment.prepared_engine_seq)
            || operation.freeze_preparation.is_some()
        {
            bail!("receipted freeze preparation conflicts with exact selected source");
        }
        if let Some((_, completion)) = &operation.freeze_completion {
            validate_completion_commitment(completion, &commitment).map_err(anyhow::Error::msg)?;
        }
        let proof_evidence = evidence()?;
        operation.freeze_witnesses.insert(
            proof_evidence.event_hash.clone(),
            UiHistoricalReceiptedFreezeWitness {
                evidence: proof_evidence.clone(),
                event: event.clone(),
                receipt_evidence: receipt.0.clone(),
                receipt: receipt.1.clone(),
            },
        );
        operation.freeze_preparation = Some((proof_evidence, commitment));
    } else if event.event_type == "seat.presentation_operation.completed" {
        let Some(id) = semantic
            .pointer("/result/transfer_operation_id")
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(());
        };
        let Some(operation) = requested.get_mut(id) else {
            return Ok(());
        };
        if operation.owner != operation.transfer.source || operation.freeze_completion.is_some() {
            bail!("receipted freeze completion conflicts with selected source");
        }
        if let Some((_, preparation)) = &operation.freeze_preparation {
            validate_completion_commitment(semantic, preparation).map_err(anyhow::Error::msg)?;
        }
        let proof_evidence = evidence()?;
        operation.freeze_witnesses.insert(
            proof_evidence.event_hash.clone(),
            UiHistoricalReceiptedFreezeWitness {
                evidence: proof_evidence.clone(),
                event: event.clone(),
                receipt_evidence: receipt.0.clone(),
                receipt: receipt.1.clone(),
            },
        );
        operation.freeze_completion = Some((proof_evidence, semantic.clone()));
    }
    Ok(())
}

fn consume_selected_seat_event<A>(
    history: &mut SeatHistory<A>,
    requested: &mut BTreeMap<String, RequestedSeatOperation>,
    hash: String,
    event: ryeos_state::objects::thread_event::ThreadEvent,
) -> anyhow::Result<()> {
    if event.event_type
        == ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT
    {
        let cancellation = ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(
            &event.to_value(),
        )
        .map_err(anyhow::Error::msg)?;
        if let Some(operation) = requested.get_mut(&cancellation.transfer.operation_id) {
            if operation.transfer != cancellation.transfer
                || operation.owner != operation.transfer.source
                || operation.cancellation.is_some()
            {
                bail!("cancellation conflicts with exact requested source identity");
            }
            operation.cancellation = Some((
                TransferEvidence {
                    event_hash: hash.clone(),
                    chain_seq: event.chain_seq,
                },
                cancellation,
            ));
        }
    }
    if let Some(phase) = history.validate_seat_event(hash, event)? {
        route_requested_seat_phase(requested, phase)?;
    }
    for receipted in std::mem::take(&mut history.validated_receipt_events) {
        route_receipted_freeze_event(
            requested,
            &receipted,
            history
                .interval_producer
                .as_deref()
                .ok_or_else(|| anyhow!("validated receipt producer absent"))?,
            history
                .validated_receipt
                .as_ref()
                .ok_or_else(|| anyhow!("validated receipt attribution absent"))?,
        )?;
    }
    Ok(())
}

fn verify_selected_freeze(operation: &RequestedSeatOperation) -> anyhow::Result<()> {
    use ryeos_ui_presentation_contract::move_freeze_lifecycle::validate_completion_commitment;
    match (&operation.freeze_preparation, &operation.freeze_completion) {
        (Some((preparation_evidence, preparation)), Some((completion_evidence, completion))) => {
            validate_completion_commitment(completion, preparation).map_err(anyhow::Error::msg)?;
            if preparation_evidence.chain_seq >= completion_evidence.chain_seq {
                bail!("freeze completion precedes exact preparation");
            }
            if let Some((evidence, cancellation)) = &operation.cancellation {
                if cancellation.freeze.preparation != *preparation_evidence
                    || cancellation.freeze.completion.as_ref() != Some(completion_evidence)
                    || evidence.chain_seq <= completion_evidence.chain_seq
                    || operation
                        .phases
                        .iter()
                        .any(|phase| matches!(phase.record.phase, TransferPhase::Prepared { .. }))
                {
                    bail!(
                        "freeze cancellation conflicts with authenticated obligation or Prepared"
                    );
                }
            }
        }
        (None, None) if operation.cancellation.is_none() => {}
        (Some((preparation_evidence, _)), None) => {
            if let Some((evidence, cancellation)) = &operation.cancellation {
                if cancellation.freeze.preparation != *preparation_evidence
                    || cancellation.freeze.completion.is_some()
                    || evidence.chain_seq <= preparation_evidence.chain_seq
                    || operation
                        .phases
                        .iter()
                        .any(|phase| matches!(phase.record.phase, TransferPhase::Prepared { .. }))
                {
                    bail!(
                        "pending freeze cancellation conflicts with authenticated obligation or Prepared"
                    );
                }
            }
        }
        _ => bail!("freeze completion or cancellation has no exact receipted preparation"),
    }
    for phase in &operation.phases {
        if let TransferPhase::Prepared {
            freeze: Some(links),
            ..
        } = &phase.record.phase
        {
            if operation
                .freeze_preparation
                .as_ref()
                .map(|(evidence, _)| evidence)
                != Some(&links.preparation)
                || operation
                    .freeze_completion
                    .as_ref()
                    .map(|(evidence, _)| evidence)
                    != Some(&links.completion)
                || links.completion.chain_seq >= phase.evidence.chain_seq
            {
                bail!("MOVE Prepared has no exact earlier receipted freeze witnesses");
            }
        }
    }
    Ok(())
}

/// Read one supplied fixed signed seat cut once for a frozen set of requested
/// transfer identities. Private state is process-local, never a registry or
/// admission token. Memory is O(requested operations), with at most nine phase
/// slots per operation plus one protocol-bounded receipt; resident lifetime is
/// not bounded by this API. No arbitrary operation cap or eviction is applied.
pub struct UiTransferFixedSeatRead {
    store: Arc<StateStore>,
    history: SeatHistory,
    requested: BTreeMap<String, RequestedSeatOperation>,
    poisoned: bool,
    endpoint: Option<PendingFixedSeatEndpoint>,
    predecessor: Option<Arc<UiTransferFixedSeatProof>>,
    joined_ancestry: bool,
}

impl UiTransferFixedSeatRead {
    /// Scheduling metadata only: this distinguishes an attempted suffix join
    /// from a cold scan, not completed coverage or admission authority.
    pub fn is_suffix_read(&self) -> bool {
        self.predecessor.is_some()
    }
}

struct PendingFixedSeatEndpoint {
    tip: TransferEvidence,
    thread_seq: u64,
    next_engine_seq: Option<u64>,
}

struct FixedSeatEndpoint {
    tip: TransferEvidence,
    thread_seq: u64,
    next_engine_seq: u64,
}

/// Opaque completed coverage of exactly the identities requested at begin.
/// Missing coverage is not absence evidence. Control and initialization prove
/// ancestry/final-control fences, not composition revision, event-time witness,
/// session expiry or live producer/publication authority.
pub struct UiTransferFixedSeatProof {
    store: Arc<StateStore>,
    history: SeatHistory,
    requested: BTreeMap<String, RequestedSeatOperation>,
    endpoint: FixedSeatEndpoint,
    joined_ancestry: bool,
}

impl UiTransferFixedSeatProof {
    /// Classify exact selected source testimony from this completed same-store
    /// cut before a caller attempts positive lifecycle assembly. An inbound
    /// destination phase cannot hide a retained source cancellation.
    pub fn selected_retained_cancellation(
        &self,
        store: &StateStore,
        transfer: &TransferIdentity,
    ) -> anyhow::Result<bool> {
        if !std::ptr::eq(self.store.as_ref(), store)
            || !same_stable_seat(&self.history.seat, &transfer.source)
        {
            bail!("cancellation classification requires exact completed source cut");
        }
        Ok(
            covered_seat_operation(&self.history.seat, &self.requested, transfer)?
                .cancellation
                .is_some(),
        )
    }

    /// Completed exact selected source cut, checked by the pair writer before
    /// it rechecks the identical anchor under publication locks. A cancellation
    /// remains permanent even after producer replacement or unrelated edits.
    pub fn validate_preparation_source(
        &self,
        store: &StateStore,
        anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        transfer: &TransferIdentity,
    ) -> anyhow::Result<()> {
        if !std::ptr::eq(self.store.as_ref(), store)
            || &self.history.anchor != anchor
            || !same_stable_seat(&self.history.seat, &transfer.source)
        {
            bail!("preparation requires completed same-store exact source cut");
        }
        let operation = covered_seat_operation(&self.history.seat, &self.requested, transfer)?;
        if operation.cancellation.is_some() {
            bail!("transfer preparation permanently refused by retained freeze cancellation");
        }
        if !operation.phases.is_empty() {
            bail!("transfer preparation repeats an existing source phase");
        }
        Ok(())
    }

    /// Derive mandatory MOVE links only from this completed receipted source
    /// cut. The UI owner still verifies the full typed Core export at its
    /// original event position; this method does not admit renderer contents.
    pub fn preparation_freeze_links(
        &self,
        store: &StateStore,
        anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        transfer: &TransferIdentity,
    ) -> anyhow::Result<ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence> {
        self.validate_preparation_source(store, anchor, transfer)?;
        let operation = covered_seat_operation(&self.history.seat, &self.requested, transfer)?;
        verify_selected_freeze(operation)?;
        let (preparation, _) = operation
            .freeze_preparation
            .as_ref()
            .ok_or_else(|| anyhow!("MOVE preparation has no receipted freeze preparation"))?;
        let (completion, _) = operation
            .freeze_completion
            .as_ref()
            .ok_or_else(|| anyhow!("MOVE preparation has no receipted freeze completion"))?;
        Ok(
            ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence {
                preparation: preparation.clone(),
                completion: completion.clone(),
            },
        )
    }

    pub fn validate_preparation_destination(
        &self,
        store: &StateStore,
        anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        transfer: &TransferIdentity,
    ) -> anyhow::Result<()> {
        if !std::ptr::eq(self.store.as_ref(), store)
            || &self.history.anchor != anchor
            || !same_stable_seat(&self.history.seat, &transfer.destination)
        {
            bail!("preparation requires completed same-store exact destination cut");
        }
        let operation = covered_seat_operation(&self.history.seat, &self.requested, transfer)?;
        if !operation.phases.is_empty() || operation.cancellation.is_some() {
            bail!("transfer preparation repeats an existing destination phase");
        }
        Ok(())
    }
    fn has_completed_ancestry(&self) -> bool {
        self.history.complete
            && (self.joined_ancestry || self.history.initialization_seen)
            && self.history.original_append_bound.is_some()
    }
}

pub fn begin_fixed_seat_read(
    store: Arc<StateStore>,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    seat: &TransferSeat,
    requested: &[TransferIdentity],
) -> anyhow::Result<UiTransferFixedSeatRead> {
    // TransferSeat's revision/incarnation are deliberately NOT context here:
    // each operation retains its own immutable origin in requested coverage.
    if uuid::Uuid::parse_str(&seat.presentation_id)
        .ok()
        .is_none_or(|id| id.to_string() != seat.presentation_id)
        || [&seat.chain_root_id, &seat.thread_id]
            .into_iter()
            .any(|id| id.is_empty() || id.trim() != id || id.chars().any(char::is_control))
        || anchor.chain_root_id() != seat.chain_root_id
        || anchor.thread_id() != seat.thread_id
    {
        bail!("fixed seat context differs from its exact signed anchor");
    }
    let requested = freeze_requested_seat_operations(seat, requested)?;
    Ok(UiTransferFixedSeatRead {
        store,
        history: SeatHistory::fresh(seat.clone(), anchor),
        requested,
        poisoned: false,
        endpoint: None,
        predecessor: None,
        joined_ancestry: false,
    })
}

/// Continue the supplied newer cut only when one completed predecessor covers
/// every request. Missing scope or an ineligible predecessor starts a cold
/// read of this SAME supplied anchor; divergence during an attempted join is
/// an error, never a silent new-head recapture or a suffix-absence result.
pub fn begin_fixed_seat_read_from_proof(
    store: Arc<StateStore>,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    seat: &TransferSeat,
    requested: &[TransferIdentity],
    proof: Arc<UiTransferFixedSeatProof>,
) -> anyhow::Result<UiTransferFixedSeatRead> {
    let mut read = begin_fixed_seat_read(store, anchor, seat, requested)?;
    let advancing = read
        .history
        .anchor
        .tip()
        .is_some_and(|(_, seq)| seq > proof.endpoint.thread_seq);
    if !Arc::ptr_eq(&read.store, &proof.store)
        || !proof.has_completed_ancestry()
        || !same_stable_seat(seat, &proof.history.seat)
        || !advancing
        || read.requested.values().any(|operation| {
            covered_seat_operation(seat, &proof.requested, &operation.transfer).is_err()
        })
    {
        return Ok(read);
    }
    // Preserve the single predecessor's broad coverage, even if this caller
    // requested a subset. Do not union independently read proofs.
    let broad = proof
        .requested
        .values()
        .map(|operation| operation.transfer.clone())
        .collect::<Vec<_>>();
    read.requested = freeze_requested_seat_operations(seat, &broad)?;
    read.predecessor = Some(proof);
    Ok(read)
}

fn finish_fixed_seat_join<A, B>(
    history: &mut SeatHistory<A>,
    cached: &SeatHistory<B>,
    endpoint: &FixedSeatEndpoint,
    hash: &str,
    event: &ryeos_state::objects::thread_event::ThreadEvent,
) -> anyhow::Result<()> {
    if event.chain_root_id != history.seat.chain_root_id
        || event.thread_id != history.seat.thread_id
        || !same_stable_seat(&history.seat, &cached.seat)
        || hash != endpoint.tip.event_hash
        || event.thread_seq != endpoint.thread_seq
        || event.chain_seq != endpoint.tip.chain_seq
        || history.previous_thread_seq != event.thread_seq.checked_add(1)
        || history
            .previous_chain_seq
            .is_none_or(|newer| newer <= event.chain_seq)
    {
        bail!("fixed seat suffix does not join its exact completed endpoint");
    }
    if history.pending_receipt.is_some() || history.pending_receipt_bytes != 0 {
        bail!("fixed seat suffix joins inside an incomplete receipt interval");
    }
    if history.initialization_seen {
        bail!("fixed seat suffix contains a new initialization");
    }
    if history
        .frontier
        .is_some_and(|frontier| frontier != endpoint.next_engine_seq)
    {
        bail!("fixed seat suffix frontier differs from completed predecessor");
    }
    let (fence, control) = cached
        .latest_control
        .as_ref()
        .ok_or_else(|| anyhow!("completed fixed seat predecessor has no producer control"))?;
    if let Some(pending) = history.pending_cancellation_control.take() {
        ryeos_ui_presentation_contract::transfer_lifecycle_v2::verify_control_fence(
            &history.seat,
            &pending,
            fence,
            control,
        )
        .map_err(anyhow::Error::msg)?;
    }
    if history
        .interval_producer
        .as_ref()
        .is_some_and(|incarnation| {
            control.state != UiSeatProducerState::Active
                || incarnation != &control.producer_incarnation
        })
    {
        bail!("fixed seat suffix receipt has no matching predecessor producer");
    }
    if let Some(newer) = &history.newer_control {
        authenticate_control_transition(
            newer,
            Some(control),
            &UiSeatControlPredecessor::Control {
                event_hash: fence.event_hash.clone(),
            },
            &history.seat.presentation_id,
            &history.seat.chain_root_id,
            &history.seat.thread_id,
            newer.next_engine_seq,
        )
        .map_err(anyhow::Error::msg)?;
    }
    let append_bound = cached
        .original_append_bound
        .ok_or_else(|| anyhow!("completed fixed seat predecessor has no initialization bound"))?;
    // Inherit authenticated ancestry only AFTER the exact join and frontier,
    // producer and control-predecessor checks. Do not fake genesis folding.
    if history.latest_control.is_none() {
        history.latest_control = cached.latest_control.clone();
    }
    history.original_append_bound = Some(append_bound);
    history.initialized_root_binding = cached.initialized_root_binding.clone();
    Ok(())
}

pub fn read_fixed_seat_page(
    read: &mut UiTransferFixedSeatRead,
    max_events: u64,
    max_bytes: u64,
) -> anyhow::Result<bool> {
    if read.poisoned {
        bail!("transfer settlement read previously failed");
    }
    let result = (|| {
        if read.history.complete {
            return Ok(true);
        }
        let page = read.store.read_thread_event_page(
            &read.history.anchor,
            read.history.progress.as_ref(),
            max_events,
            max_bytes,
        )?;
        if page.events.is_empty() && page.progress.is_some() {
            bail!("transfer settlement history made no progress");
        }
        for (hash, event) in page.events {
            if read.endpoint.is_none() {
                if read.history.anchor.tip() != Some((hash.as_str(), event.thread_seq)) {
                    bail!("fixed seat read does not begin at its supplied signed endpoint");
                }
                read.endpoint = Some(PendingFixedSeatEndpoint {
                    tip: TransferEvidence {
                        event_hash: hash.clone(),
                        chain_seq: event.chain_seq,
                    },
                    thread_seq: event.thread_seq,
                    next_engine_seq: None,
                });
            }
            if let Some(cached) = &read.predecessor {
                if event.thread_seq <= cached.endpoint.thread_seq
                    || hash == cached.endpoint.tip.event_hash
                {
                    // The cached tip is a joining witness, not a new event.
                    // Never fold it again or continue into its older history.
                    finish_fixed_seat_join(
                        &mut read.history,
                        &cached.history,
                        &cached.endpoint,
                        &hash,
                        &event,
                    )?;
                    for operation in cached.requested.values() {
                        for phase in &operation.phases {
                            route_requested_seat_phase(&mut read.requested, phase.clone())?;
                        }
                        let target = read
                            .requested
                            .get_mut(&operation.transfer.operation_id)
                            .expect("cached broad coverage");
                        if target.freeze_preparation.is_some()
                            && operation.freeze_preparation.is_some()
                            || target.freeze_completion.is_some()
                                && operation.freeze_completion.is_some()
                            || target.cancellation.is_some() && operation.cancellation.is_some()
                        {
                            bail!("suffix duplicates retained freeze testimony");
                        }
                        if target.freeze_preparation.is_none() {
                            target.freeze_preparation = operation.freeze_preparation.clone();
                        }
                        if target.freeze_completion.is_none() {
                            target.freeze_completion = operation.freeze_completion.clone();
                        }
                        if target.cancellation.is_none() {
                            target.cancellation = operation.cancellation.clone();
                        }
                    }
                    let endpoint = read.endpoint.as_mut().expect("captured supplied endpoint");
                    if endpoint.next_engine_seq.is_none() {
                        endpoint.next_engine_seq = Some(cached.endpoint.next_engine_seq);
                    }
                    read.joined_ancestry = true;
                    read.history.complete = true;
                    read.history.progress = None;
                    return Ok(true);
                }
            }
            // The first RECEIPT frontier is trusted only when its complete
            // interval passes semantic validation. It can precede the newest
            // control in reverse history, whose original frontier is older.
            let pending_head_frontier = read
                .history
                .pending_receipt
                .as_ref()
                .map(|(receipt, _)| receipt.last_engine_seq.checked_add(1));
            consume_selected_seat_event(&mut read.history, &mut read.requested, hash, event)?;
            let endpoint = read.endpoint.as_mut().expect("captured supplied endpoint");
            if endpoint.next_engine_seq.is_none() && read.history.pending_receipt.is_none() {
                if let Some(frontier) = pending_head_frontier {
                    endpoint.next_engine_seq = Some(frontier.ok_or_else(|| {
                        anyhow!("fixed seat endpoint engine frontier is exhausted")
                    })?);
                } else if let Some((_, control)) = &read.history.latest_control {
                    endpoint.next_engine_seq = Some(control.next_engine_seq);
                }
            }
        }
        read.history.complete = page.progress.is_none();
        if read.history.complete {
            if read.predecessor.is_some() {
                bail!("fixed seat suffix reached genesis without its completed endpoint");
            }
            read.history.finish()?;
        }
        read.history.progress = page.progress;
        Ok(read.history.complete)
    })();
    if result.is_err() {
        read.poisoned = true;
    }
    result
}

pub fn finish_fixed_seat_read(
    read: UiTransferFixedSeatRead,
) -> anyhow::Result<UiTransferFixedSeatProof> {
    require_complete_settlement_read(read.poisoned, read.history.complete, true)?;
    if read.predecessor.is_some() {
        if !read.joined_ancestry || read.history.pending_receipt.is_some() {
            bail!("fixed seat suffix has no complete authenticated join");
        }
    } else {
        read.history.finish()?;
    }
    for operation in read.requested.values() {
        verify_phase_slice_fences(read.history.latest_control.as_ref(), &operation.phases)?;
        verify_selected_freeze(operation)?;
        if let Some((_, preparation)) = &operation.freeze_preparation {
            if read.history.initialized_root_binding.as_ref() != Some(&preparation.root_binding) {
                bail!("receipted freeze root differs from authenticated initialization");
            }
        }
    }
    let endpoint = read
        .endpoint
        .ok_or_else(|| anyhow!("completed fixed seat has no signed endpoint"))?;
    let endpoint = FixedSeatEndpoint {
        tip: endpoint.tip,
        thread_seq: endpoint.thread_seq,
        next_engine_seq: endpoint
            .next_engine_seq
            .ok_or_else(|| anyhow!("completed fixed seat has no authenticated head frontier"))?,
    };
    Ok(UiTransferFixedSeatProof {
        store: read.store,
        history: read.history,
        requested: read.requested,
        endpoint,
        joined_ancestry: read.joined_ancestry,
    })
}

/// Reuse completed coverage only at the identical signed cut and state owner.
/// A miss is not absence, suffix coverage or live authority. A hit retains the
/// whole prior proof rather than narrowing or unioning its frozen requests.
pub fn reuse_fixed_seat_proof(
    store: &StateStore,
    anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    seat: &TransferSeat,
    requested: &[TransferIdentity],
    proof: &Arc<UiTransferFixedSeatProof>,
) -> anyhow::Result<Option<Arc<UiTransferFixedSeatProof>>> {
    // Validate even on a cache miss: malformed or conflicting requests must
    // not be reinterpreted as a harmless lack of cached coverage.
    let requested = freeze_requested_seat_operations(seat, requested)?;
    if !std::ptr::eq(proof.store.as_ref(), store)
        || !proof.has_completed_ancestry()
        || proof.history.anchor != *anchor
        || !same_stable_seat(&proof.history.seat, seat)
    {
        return Ok(None);
    }
    for operation in requested.values() {
        if covered_seat_operation(seat, &proof.requested, &operation.transfer).is_err() {
            return Ok(None);
        }
    }
    Ok(Some(Arc::clone(proof)))
}

fn covered_seat_operation<'a>(
    seat: &TransferSeat,
    requested: &'a BTreeMap<String, RequestedSeatOperation>,
    transfer: &TransferIdentity,
) -> anyhow::Result<&'a RequestedSeatOperation> {
    let operation = requested
        .get(&transfer.operation_id)
        .ok_or_else(|| anyhow!("fixed seat proof did not request this transfer identity"))?;
    if operation.transfer != *transfer || !same_stable_seat(seat, &operation.owner) {
        bail!("fixed seat proof coverage differs from the exact transfer identity");
    }
    Ok(operation)
}

/// Assemble one requested operation from two independently completed cuts.
/// Reuses their frozen coverage without rescanning either seat. This topology
/// proof alone grants no composition/event-time or live mutation authority.
pub fn assemble_fixed_history_proof(
    source: &UiTransferFixedSeatProof,
    destination: &UiTransferFixedSeatProof,
    transfer: &TransferIdentity,
) -> anyhow::Result<UiTransferFixedHistoryProof> {
    if !Arc::ptr_eq(&source.store, &destination.store) {
        bail!("fixed seat proofs belong to different state owners");
    }
    if !same_stable_seat(&source.history.seat, &transfer.source)
        || !same_stable_seat(&destination.history.seat, &transfer.destination)
    {
        bail!("fixed seat proofs differ from exact source and destination seats");
    }
    let source_operation =
        covered_seat_operation(&source.history.seat, &source.requested, transfer)?;
    let destination_operation =
        covered_seat_operation(&destination.history.seat, &destination.requested, transfer)?;
    let lifecycle = fold_transfer_phase_slices(
        transfer,
        &source_operation.phases,
        &destination_operation.phases,
    )?;
    Ok(UiTransferFixedHistoryProof {
        store: source.store.clone(),
        source_anchor: source.history.anchor.clone(),
        destination_anchor: destination.history.anchor.clone(),
        lifecycle,
    })
}

pub(crate) fn completed_negative_terminal_decision(
    lifecycle: &TransferLifecycleState,
) -> Option<(TransferEvidence, TransferRecord)> {
    if lifecycle.accepted().is_some()
        || lifecycle.committed().is_some()
        || lifecycle.activated().is_some()
        || lifecycle.retained().is_some()
        || lifecycle.released().is_some()
    {
        return None;
    }
    if let Some(aborted) = lifecycle.aborted() {
        return Some((aborted.evidence.clone(), aborted.record.clone()));
    }
    if lifecycle.prepared().is_none() {
        if let Some(orphan) = lifecycle.orphan_cancelled() {
            return Some((orphan.evidence.clone(), orphan.record.clone()));
        }
    }
    None
}

fn validate_negative_destination_phases(
    transfer: &TransferIdentity,
    phases: &[RetainedPhase],
) -> anyhow::Result<()> {
    if phases.iter().any(|phase| {
        !matches!(
            phase.record.phase,
            TransferPhase::InboundCommitment | TransferPhase::OrphanCancelled { .. }
        )
    }) {
        bail!("negative destination prefix has a preparation or accepted obligation");
    }
    if !phases.is_empty() {
        // Exact links/order/full identity remain checked by the existing fold;
        // merely naming an orphan phase is not negative admission.
        let lifecycle = fold_transfer_phase_slices(transfer, &[], phases)?;
        if lifecycle.prepared().is_some() || lifecycle.accepted().is_some() {
            bail!("negative destination prefix has conflicting lifecycle evidence");
        }
    }
    Ok(())
}

pub fn assemble_move_freeze_cancellation_proof(
    source: &UiTransferFixedSeatProof,
    destination: &UiTransferFixedSeatProof,
    transfer: &TransferIdentity,
) -> anyhow::Result<UiMoveFreezeCancellationProof> {
    if !Arc::ptr_eq(&source.store, &destination.store)
        || !same_stable_seat(&source.history.seat, &transfer.source)
        || !same_stable_seat(&destination.history.seat, &transfer.destination)
    {
        bail!("freeze cancellation requires same-store completed exact paired cuts");
    }
    let source_operation =
        covered_seat_operation(&source.history.seat, &source.requested, transfer)?;
    let destination_operation =
        covered_seat_operation(&destination.history.seat, &destination.requested, transfer)?;
    if !source_operation.phases.is_empty() {
        bail!("freeze cancellation has an existing transfer preparation or destination obligation");
    }
    validate_negative_destination_phases(transfer, &destination_operation.phases)?;
    let preparation = source_operation
        .freeze_preparation
        .as_ref()
        .ok_or_else(|| anyhow!("freeze cancellation has no receipted source preparation"))?;
    let completion = source_operation.freeze_completion.as_ref();
    verify_selected_freeze(source_operation)?;
    Ok(UiMoveFreezeCancellationProof {
        store: source.store.clone(),
        source_anchor: source.history.anchor.clone(),
        destination_anchor: destination.history.anchor.clone(),
        transfer: transfer.clone(),
        freeze:
            ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellationEvidence {
                preparation: preparation.0.clone(),
                completion: completion.map(|value| value.0.clone()),
            },
        retained: source_operation.cancellation.clone(),
        destination_inbound: destination_operation
            .phases
            .iter()
            .find(|phase| matches!(phase.record.phase, TransferPhase::InboundCommitment))
            .map(|phase| (phase.evidence.clone(), phase.record.clone())),
        destination_orphan_cancelled: destination_operation
            .phases
            .iter()
            .find(|phase| matches!(phase.record.phase, TransferPhase::OrphanCancelled { .. }))
            .map(|phase| (phase.evidence.clone(), phase.record.clone())),
        original_append_bound: source
            .history
            .original_append_bound
            .ok_or_else(|| anyhow!("completed source has no authenticated append bound"))?,
        latest_control: source
            .history
            .latest_control
            .clone()
            .ok_or_else(|| anyhow!("completed source has no authenticated producer control"))?,
        next_engine_seq: source.endpoint.next_engine_seq,
    })
}

/// Opaque, owner-budgeted two-seat read for browser transfer admission. A
/// partial page is never a proof, including when it contains no matching
/// phase. The reader is poisoned by any failed page and can only be finished
/// after both exact fixed histories reach genesis.
pub struct UiTransferFixedHistoryRead {
    transfer: TransferIdentity,
    source: UiTransferFixedSeatRead,
    destination: UiTransferFixedSeatRead,
    poisoned: bool,
}

pub fn begin_fixed_history_read(
    store: Arc<StateStore>,
    transfer: &TransferIdentity,
) -> anyhow::Result<UiTransferFixedHistoryRead> {
    if transfer.source.chain_root_id == transfer.destination.chain_root_id
        || transfer.source.thread_id == transfer.destination.thread_id
        || transfer.source.presentation_id == transfer.destination.presentation_id
    {
        bail!("transfer settlement requires two distinct retained seats");
    }
    // Capture BOTH cuts before either scanner consumes a page.
    let source_anchor = store
        .capture_thread_event_read_anchor(
            &transfer.source.chain_root_id,
            &transfer.source.thread_id,
        )?
        .ok_or_else(|| anyhow!("transfer settlement requires both retained signed heads"))?;
    let destination_anchor = store
        .capture_thread_event_read_anchor(
            &transfer.destination.chain_root_id,
            &transfer.destination.thread_id,
        )?
        .ok_or_else(|| anyhow!("transfer settlement requires both retained signed heads"))?;
    let source = begin_fixed_seat_read(
        store.clone(),
        source_anchor,
        &transfer.source,
        std::slice::from_ref(transfer),
    )?;
    let destination = begin_fixed_seat_read(
        store,
        destination_anchor,
        &transfer.destination,
        std::slice::from_ref(transfer),
    )?;
    Ok(UiTransferFixedHistoryRead {
        transfer: transfer.clone(),
        source,
        destination,
        poisoned: false,
    })
}

pub fn read_fixed_history_page(
    read: &mut UiTransferFixedHistoryRead,
    max_events: u64,
    max_bytes: u64,
) -> anyhow::Result<bool> {
    if read.poisoned {
        bail!("transfer settlement read previously failed");
    }
    // One call reads one seat only, preserving the caller's page work budget.
    let result = if !read.source.history.complete {
        read_fixed_seat_page(&mut read.source, max_events, max_bytes)
    } else {
        read_fixed_seat_page(&mut read.destination, max_events, max_bytes)
    };
    match result {
        Ok(_) => Ok(read.source.history.complete && read.destination.history.complete),
        Err(error) => {
            read.poisoned = true;
            Err(error)
        }
    }
}

pub fn finish_fixed_history_read(
    read: UiTransferFixedHistoryRead,
) -> anyhow::Result<UiTransferFixedHistoryProof> {
    require_complete_settlement_read(
        read.poisoned,
        read.source.history.complete,
        read.destination.history.complete,
    )?;
    let source = finish_fixed_seat_read(read.source)?;
    let destination = finish_fixed_seat_read(read.destination)?;
    assemble_fixed_history_proof(&source, &destination, &read.transfer)
}

impl UiTransferFixedHistoryProof {
    /// Read-only owner identity, not admission or publication authority.
    pub fn is_for_store(&self, store: &StateStore) -> bool {
        std::ptr::eq(self.store.as_ref(), store)
    }

    pub(crate) fn belongs_to(&self, store: &StateStore) -> bool {
        self.is_for_store(store)
    }

    pub fn source_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.source_anchor
    }

    pub fn destination_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.destination_anchor
    }

    pub fn lifecycle(&self) -> &TransferLifecycleState {
        &self.lifecycle
    }
}

pub fn read_complete_fixed_history(
    store: Arc<StateStore>,
    transfer: &TransferIdentity,
) -> anyhow::Result<UiTransferFixedHistoryProof> {
    let mut read = begin_fixed_history_read(store, transfer)?;
    while !read_fixed_history_page(&mut read, 64, 2 * 1024 * 1024)? {}
    finish_fixed_history_read(read)
}

enum TerminalHistoryEndpoint {
    Current(ryeos_state::AuthoritativeThreadEventReadAnchor),
    Retired(ryeos_state::RetiredSeatPositivePrefix),
}

#[derive(Clone)]
pub enum UiTransferHistoryEndpoint {
    Current(ryeos_state::AuthoritativeThreadEventReadAnchor),
    Retired(ryeos_state::RetiredSeatPositivePrefix),
}

impl UiTransferHistoryEndpoint {
    pub fn chain_root_id(&self) -> &str {
        match self {
            Self::Current(anchor) => anchor.chain_root_id(),
            Self::Retired(prefix) => prefix.chain_root_id(),
        }
    }
    pub fn current_anchor(&self) -> Option<&ryeos_state::AuthoritativeThreadEventReadAnchor> {
        match self {
            Self::Current(anchor) => Some(anchor),
            Self::Retired(_) => None,
        }
    }
}

struct CompletedTransferHistory {
    store: Arc<StateStore>,
    local_endpoint: UiTransferHistoryEndpoint,
    identities: BTreeMap<String, TransferIdentity>,
    anchors: Vec<ryeos_state::AuthoritativeThreadEventReadAnchor>,
    retired: Vec<ryeos_state::RetiredSeatPositivePrefix>,
    negative_closures:
        Vec<ryeos_state::objects::terminal_seat_retirement::TerminalTransferNegativeClosure>,
}

/// Bounded continuation under the existing lifecycle reader. Its frozen scope
/// is the full exact dependency closure discovered by its authorized caller.
pub struct UiHistoricalTransferCutRead {
    read: TerminalTransferClearanceRead,
}

#[derive(Clone)]
pub struct UiHistoricalReceiptedFreezeWitness {
    pub evidence: TransferEvidence,
    pub event: ryeos_state::objects::thread_event::ThreadEvent,
    pub receipt_evidence: TransferEvidence,
    pub receipt: SeatAppendReceipt,
}

/// Completed terminal historical testimony. Mutable publication still needs
/// current membership authorization and mixed current/retired exclusions.
pub struct UiHistoricalTransferCutProof {
    initializations: BTreeMap<String, serde_json::Value>,
    freeze_witnesses: BTreeMap<String, UiHistoricalReceiptedFreezeWitness>,
    clearance: CompletedTransferHistory,
    lifecycles: BTreeMap<String, TransferLifecycleState>,
    cancellations: BTreeMap<
        String,
        (
            TransferEvidence,
            ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
        ),
    >,
}

impl UiHistoricalTransferCutProof {
    pub fn initialization_matches(&self, root: &str, initialization: &serde_json::Value) -> bool {
        self.initializations.get(root) == Some(initialization)
    }
    pub fn freeze_witness(
        &self,
        evidence: &TransferEvidence,
    ) -> Option<&UiHistoricalReceiptedFreezeWitness> {
        self.freeze_witnesses
            .get(&evidence.event_hash)
            .filter(|witness| witness.evidence == *evidence)
    }
    pub fn is_for_store(&self, store: &StateStore) -> bool {
        std::ptr::eq(self.clearance.store.as_ref(), store)
    }
    pub fn local_endpoint(&self) -> &UiTransferHistoryEndpoint {
        &self.clearance.local_endpoint
    }
    pub fn identities(&self) -> &BTreeMap<String, TransferIdentity> {
        &self.clearance.identities
    }
    pub fn current_anchors(&self) -> &[ryeos_state::AuthoritativeThreadEventReadAnchor] {
        &self.clearance.anchors
    }
    pub fn retired_prefixes(&self) -> &[ryeos_state::RetiredSeatPositivePrefix] {
        &self.clearance.retired
    }
    pub fn lifecycle(&self, operation_id: &str) -> Option<&TransferLifecycleState> {
        self.lifecycles.get(operation_id)
    }
    pub fn cancellation(
        &self,
        operation_id: &str,
    ) -> Option<&(
        TransferEvidence,
        ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
    )> {
        self.cancellations.get(operation_id)
    }
}

pub fn begin_historical_transfer_cut_read(
    store: Arc<StateStore>,
    local_anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    requested: &[TransferIdentity],
) -> anyhow::Result<UiHistoricalTransferCutRead> {
    Ok(UiHistoricalTransferCutRead {
        read: begin_transfer_clearance_read(
            store,
            &UiTransferHistoryEndpoint::Current(local_anchor.clone()),
            requested,
            false,
        )?,
    })
}

/// Preserve discovery's original endpoints; missing scope never recaptures.
pub fn begin_historical_transfer_cut_read_at_endpoints(
    store: Arc<StateStore>,
    local_anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    requested: &[TransferIdentity],
    endpoints: BTreeMap<String, UiTransferHistoryEndpoint>,
) -> anyhow::Result<UiHistoricalTransferCutRead> {
    begin_historical_transfer_cut_read_at_endpoint(
        store,
        &UiTransferHistoryEndpoint::Current(local_anchor.clone()),
        requested,
        endpoints,
    )
}

pub fn begin_historical_transfer_cut_read_at_endpoint(
    store: Arc<StateStore>,
    local_endpoint: &UiTransferHistoryEndpoint,
    requested: &[TransferIdentity],
    endpoints: BTreeMap<String, UiTransferHistoryEndpoint>,
) -> anyhow::Result<UiHistoricalTransferCutRead> {
    let mut read = begin_transfer_clearance_read(store, local_endpoint, requested, false)?;
    if read.pending_groups.len() != endpoints.len() {
        bail!("historical endpoint inventory differs from frozen dependency scope");
    }
    for root in read.pending_groups.keys() {
        let endpoint = endpoints
            .get(root)
            .ok_or_else(|| anyhow!("historical frozen endpoint is absent"))?;
        let exact_root = match endpoint {
            UiTransferHistoryEndpoint::Current(anchor) => {
                anchor.chain_root_id() == root
                    && anchor.thread_id() == root
                    && (root != local_endpoint.chain_root_id()
                        || local_endpoint.current_anchor() == Some(anchor))
            }
            UiTransferHistoryEndpoint::Retired(prefix) => {
                prefix.chain_root_id() == root
                    && (root != local_endpoint.chain_root_id()
                        || matches!(local_endpoint, UiTransferHistoryEndpoint::Retired(local) if local.same_cut(prefix)))
            }
        };
        if !exact_root {
            bail!("historical endpoint changes exact root or local cut");
        }
    }
    read.supplied_endpoints = Some(endpoints);
    Ok(UiHistoricalTransferCutRead { read })
}

pub fn read_historical_transfer_cut_page(
    read: &mut UiHistoricalTransferCutRead,
) -> anyhow::Result<bool> {
    read_terminal_transfer_clearance_page(&mut read.read)
}

pub fn finish_historical_transfer_cut_read(
    read: UiHistoricalTransferCutRead,
) -> anyhow::Result<UiHistoricalTransferCutProof> {
    let mut lifecycles = BTreeMap::new();
    let mut cancellations = BTreeMap::new();
    let initializations =
        read.read
            .histories
            .iter()
            .map(|(root, (history, _))| {
                Ok((
                    root.clone(),
                    history.initialized_payload.clone().ok_or_else(|| {
                        anyhow!("historical endpoint lacks original initialization")
                    })?,
                ))
            })
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let mut freeze_witnesses = BTreeMap::new();
    for transfer in read.read.identities.values() {
        let (source_history, source_ops) = read
            .read
            .histories
            .get(&transfer.source.chain_root_id)
            .ok_or_else(|| anyhow!("historical source endpoint is incomplete"))?;
        let (destination_history, destination_ops) = read
            .read
            .histories
            .get(&transfer.destination.chain_root_id)
            .ok_or_else(|| anyhow!("historical destination endpoint is incomplete"))?;
        let source = covered_seat_operation(&source_history.seat, source_ops, transfer)?;
        let destination =
            covered_seat_operation(&destination_history.seat, destination_ops, transfer)?;
        if !source.phases.is_empty() || !destination.phases.is_empty() {
            lifecycles.insert(
                transfer.operation_id.clone(),
                fold_transfer_phase_slices(transfer, &source.phases, &destination.phases)?,
            );
        }
        if let Some(cancellation) = &source.cancellation {
            for (hash, witness) in &source.freeze_witnesses {
                freeze_witnesses.insert(hash.clone(), witness.clone());
            }
            cancellations.insert(transfer.operation_id.clone(), cancellation.clone());
        }
    }
    // Only this existing completed proof owner qualifies absence, original tip
    // ancestry and every inherited negative closure. No partial scope escapes.
    let clearance = finish_transfer_clearance_read(read.read, false)?;
    Ok(UiHistoricalTransferCutProof {
        initializations,
        freeze_witnesses,
        clearance,
        lifecycles,
        cancellations,
    })
}

/// Completed positive settlement/cancellation testimony for exactly a frozen
/// requested set. Exhaustive local discovery is a separate required input to
/// qualification. Retired endpoints never establish a missing terminal phase.
pub(crate) struct TerminalTransferClearance {
    store: Arc<StateStore>,
    local_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    identities: BTreeMap<String, TransferIdentity>,
    anchors: Vec<ryeos_state::AuthoritativeThreadEventReadAnchor>,
    retired: Vec<ryeos_state::RetiredSeatPositivePrefix>,
    negative_closures:
        Vec<ryeos_state::objects::terminal_seat_retirement::TerminalTransferNegativeClosure>,
}

impl TerminalTransferClearance {
    pub(crate) fn negative_closures(
        &self,
    ) -> &[ryeos_state::objects::terminal_seat_retirement::TerminalTransferNegativeClosure] {
        &self.negative_closures
    }
    pub(crate) fn belongs_to(&self, store: &StateStore) -> bool {
        std::ptr::eq(self.store.as_ref(), store)
    }
    pub(crate) fn local_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.local_anchor
    }
    pub(crate) fn identities(&self) -> &BTreeMap<String, TransferIdentity> {
        &self.identities
    }
    pub(crate) fn anchors(&self) -> &[ryeos_state::AuthoritativeThreadEventReadAnchor] {
        &self.anchors
    }
    pub(crate) fn retired(&self) -> &[ryeos_state::RetiredSeatPositivePrefix] {
        &self.retired
    }
}

/// Existing sessionless lifecycle reader. Capture every exact counterpart
/// before scanning any one; each stable seat is scanned once for all selected
/// operations, with one byte/event-bounded page call at a time. Resident scope
/// is O(requested operations), not a lifetime-bounded cache.
pub(crate) struct TerminalTransferClearanceRead {
    supplied_endpoints: Option<BTreeMap<String, UiTransferHistoryEndpoint>>,
    store: Arc<StateStore>,
    local_endpoint: UiTransferHistoryEndpoint,
    identities: BTreeMap<String, TransferIdentity>,
    anchors: Vec<ryeos_state::AuthoritativeThreadEventReadAnchor>,
    retired: Vec<ryeos_state::RetiredSeatPositivePrefix>,
    histories: BTreeMap<
        String,
        (
            SeatHistory<TerminalHistoryEndpoint>,
            BTreeMap<String, RequestedSeatOperation>,
        ),
    >,
    pending_groups: BTreeMap<String, (TransferSeat, Vec<TransferIdentity>)>,
    scan_turn: VecDeque<String>,
    closure_turn: VecDeque<(
        String,
        Option<ryeos_state::state_db::TerminalSeatRetirementClosureProgress>,
    )>,
    inherited_negative: BTreeMap<
        String,
        ryeos_state::objects::terminal_seat_retirement::TerminalTransferNegativeClosure,
    >,
    marked_negative: BTreeSet<(String, String)>,
    needed_negative_tips: BTreeMap<
        (String, String),
        ryeos_state::objects::terminal_seat_retirement::TerminalTransferPrefixTip,
    >,
    observed_negative_tips: BTreeSet<(String, String)>,
    endpoint_tips:
        BTreeMap<String, ryeos_state::objects::terminal_seat_retirement::TerminalTransferPrefixTip>,
    poisoned: bool,
}

pub(crate) fn begin_terminal_transfer_clearance(
    store: Arc<StateStore>,
    local_anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    requested: &[TransferIdentity],
) -> anyhow::Result<TerminalTransferClearanceRead> {
    begin_transfer_clearance_read(
        store,
        &UiTransferHistoryEndpoint::Current(local_anchor.clone()),
        requested,
        true,
    )
}

fn begin_transfer_clearance_read(
    store: Arc<StateStore>,
    local_endpoint: &UiTransferHistoryEndpoint,
    requested: &[TransferIdentity],
    require_local_scope: bool,
) -> anyhow::Result<TerminalTransferClearanceRead> {
    let mut groups = BTreeMap::<String, (TransferSeat, Vec<TransferIdentity>)>::new();
    let mut presentations = BTreeMap::new();
    let mut identities = BTreeMap::new();
    for transfer in requested {
        TransferRecord {
            schema: 1,
            transfer: transfer.clone(),
            phase: TransferPhase::InboundCommitment,
        }
        .validate_shape()
        .map_err(anyhow::Error::msg)?;
        if let Some(prior) = identities.insert(transfer.operation_id.clone(), transfer.clone()) {
            if prior != *transfer {
                bail!("terminal clearance has conflicting operation identities");
            }
            continue;
        }
        if require_local_scope
            && transfer.source.chain_root_id != local_endpoint.chain_root_id()
            && transfer.destination.chain_root_id != local_endpoint.chain_root_id()
        {
            bail!("terminal clearance operation does not touch local cut");
        }
        for seat in [&transfer.source, &transfer.destination] {
            if seat.chain_root_id != seat.thread_id {
                bail!("terminal clearance requires root seats");
            }
            if let Some(prior) =
                presentations.insert(seat.presentation_id.clone(), seat.chain_root_id.clone())
            {
                if prior != seat.chain_root_id {
                    bail!("terminal clearance presentation changes root");
                }
            }
            let group = groups
                .entry(seat.chain_root_id.clone())
                .or_insert_with(|| (seat.clone(), Vec::new()));
            if !same_stable_seat(&group.0, seat) {
                bail!("terminal clearance root changes stable seat ownership");
            }
            group.1.push(transfer.clone());
        }
    }
    Ok(TerminalTransferClearanceRead {
        supplied_endpoints: None,
        store,
        local_endpoint: local_endpoint.clone(),
        identities,
        anchors: local_endpoint
            .current_anchor()
            .cloned()
            .into_iter()
            .collect(),
        retired: Vec::new(),
        histories: BTreeMap::new(),
        pending_groups: groups,
        scan_turn: VecDeque::new(),
        closure_turn: VecDeque::new(),
        inherited_negative: BTreeMap::new(),
        marked_negative: BTreeSet::new(),
        needed_negative_tips: BTreeMap::new(),
        observed_negative_tips: BTreeSet::new(),
        endpoint_tips: BTreeMap::new(),
        poisoned: false,
    })
}

pub(crate) fn read_terminal_transfer_clearance_page(
    read: &mut TerminalTransferClearanceRead,
) -> anyhow::Result<bool> {
    if read.poisoned {
        bail!("terminal clearance read is poisoned");
    }
    let result = read_terminal_transfer_clearance_turn(read);
    if result.is_err() {
        read.poisoned = true;
    }
    result
}

fn read_terminal_transfer_clearance_turn(
    read: &mut TerminalTransferClearanceRead,
) -> anyhow::Result<bool> {
    let store = &read.store;
    let authority = store.pinned_state_authority()?;
    // Capture one endpoint per turn, and capture every endpoint before any
    // history scan. No arbitrary lifetime-operation cap substitutes for scope.
    if let Some((root, (seat, requests))) = read.pending_groups.pop_first() {
        let endpoint = if let Some(endpoints) = &mut read.supplied_endpoints {
            match endpoints
                .remove(&root)
                .ok_or_else(|| anyhow!("historical fixed endpoint omitted"))?
            {
                UiTransferHistoryEndpoint::Current(anchor) => {
                    if root != read.local_endpoint.chain_root_id() {
                        read.anchors.push(anchor.clone());
                    }
                    TerminalHistoryEndpoint::Current(anchor)
                }
                UiTransferHistoryEndpoint::Retired(prefix) => {
                    read.retired.push(prefix.clone());
                    read.closure_turn.push_back((root.clone(), None));
                    TerminalHistoryEndpoint::Retired(prefix)
                }
            }
        } else if root == read.local_endpoint.chain_root_id() {
            match &read.local_endpoint {
                UiTransferHistoryEndpoint::Current(anchor) => {
                    TerminalHistoryEndpoint::Current(anchor.clone())
                }
                UiTransferHistoryEndpoint::Retired(_) => {
                    bail!("retired local read requires complete supplied endpoints")
                }
            }
        } else if let Some(anchor) = store.capture_thread_event_read_anchor(&root, &root)? {
            read.anchors.push(anchor.clone());
            TerminalHistoryEndpoint::Current(anchor)
        } else {
            let guard = authority.acquire_shared_guard()?;
            let prefix = authority.capture_retired_seat_positive_prefix(&root, &guard)?
                .ok_or_else(|| anyhow!("terminal clearance counterpart has no authenticated retained positive prefix"))?;
            read.retired.push(prefix.clone());
            read.closure_turn.push_back((root.clone(), None));
            TerminalHistoryEndpoint::Retired(prefix)
        };
        read.histories.insert(
            root.clone(),
            (
                SeatHistory::fresh(seat.clone(), endpoint),
                freeze_requested_seat_operations(&seat, &requests)?,
            ),
        );
        read.scan_turn.push_back(root);
        return Ok(false);
    }
    if let Some((root, progress)) = read.closure_turn.pop_front() {
        let TerminalHistoryEndpoint::Retired(prefix) = &read.histories[&root].0.anchor else {
            bail!("retirement closure metadata is not a retired endpoint");
        };
        let guard = authority.acquire_shared_guard()?;
        let page = authority.read_retired_seat_retirement_closure_page(
            prefix,
            progress.as_ref(),
            ryeos_state::objects::MAX_THREAD_EVENT_SERIALIZED_BYTES as u64,
            &guard,
        )?;
        for closure in page.negative_transfer_closures {
            let transfer = &closure.decision.transfer;
            let Some(requested) = read.identities.get(&transfer.operation_id) else {
                continue;
            };
            if transfer != requested {
                bail!("retired negative closure changes full requested identity");
            }
            for (seat, tip) in [
                (&transfer.source, &closure.source_tip),
                (&transfer.destination, &closure.destination_tip),
            ] {
                let key = (seat.chain_root_id.clone(), tip.event_hash.clone());
                if read
                    .needed_negative_tips
                    .insert(key, tip.clone())
                    .is_some_and(|prior| prior != *tip)
                {
                    bail!("retired negative closure changes native prefix coordinates");
                }
            }
            read.marked_negative
                .insert((root.clone(), transfer.operation_id.clone()));
            if read
                .inherited_negative
                .insert(transfer.operation_id.clone(), closure.clone())
                .is_some_and(|prior| prior != closure)
            {
                bail!("retired negative closures disagree across exact markers");
            }
        }
        if !page.complete {
            if page.next_progress.is_none() {
                bail!("retirement closure page made no progress");
            }
            read.closure_turn.push_back((root, page.next_progress));
        }
        return Ok(false);
    }
    if let Some(root) = read.scan_turn.pop_front() {
        let (history, operations) = read
            .histories
            .get_mut(&root)
            .expect("frozen clearance scan group");
        let page = match &history.anchor {
            TerminalHistoryEndpoint::Current(anchor) => {
                if store
                    .capture_thread_event_read_anchor(anchor.chain_root_id(), anchor.thread_id())?
                    .as_ref()
                    != Some(anchor)
                {
                    bail!("terminal clearance current cut advanced");
                }
                store.read_thread_event_page(
                    anchor,
                    history.progress.as_ref(),
                    64,
                    2 * 1024 * 1024,
                )?
            }
            TerminalHistoryEndpoint::Retired(prefix) => {
                let guard = authority.acquire_shared_guard()?;
                authority.read_retired_seat_positive_prefix_page(
                    prefix,
                    history.progress.as_ref(),
                    64,
                    2 * 1024 * 1024,
                    &guard,
                )?
            }
        };
        if page.events.is_empty() && page.progress.is_some() {
            bail!("terminal clearance page made no progress");
        }
        for (hash, event) in page.events {
            let key = (root.clone(), hash.clone());
            if let Some(tip) = read.needed_negative_tips.get(&key) {
                if event.thread_seq != tip.thread_seq || event.chain_seq != tip.chain_seq {
                    bail!("retired negative closure prefix differs from authenticated ancestry");
                }
                read.observed_negative_tips.insert(key);
            }
            if history.previous_thread_seq.is_none() {
                match &history.anchor {
                    TerminalHistoryEndpoint::Current(anchor)
                        if anchor.tip() == Some((hash.as_str(), event.thread_seq)) => {}
                    TerminalHistoryEndpoint::Retired(prefix)
                        if prefix.tip()
                            == Some((hash.as_str(), event.thread_seq, event.chain_seq)) => {}
                    _ => bail!("terminal clearance scan differs from authenticated endpoint"),
                }
                read.endpoint_tips.insert(
                    root.clone(),
                    ryeos_state::objects::terminal_seat_retirement::TerminalTransferPrefixTip {
                        event_hash: hash.clone(),
                        thread_seq: event.thread_seq,
                        chain_seq: event.chain_seq,
                    },
                );
            }
            consume_selected_seat_event(history, operations, hash, event)?;
        }
        history.complete = page.progress.is_none();
        history.progress = page.progress;
        if !history.complete {
            read.scan_turn.push_back(root);
        }
    }
    Ok(read.scan_turn.is_empty())
}

pub(crate) fn finish_terminal_transfer_clearance(
    read: TerminalTransferClearanceRead,
) -> anyhow::Result<TerminalTransferClearance> {
    let completed = finish_transfer_clearance_read(read, true)?;
    let UiTransferHistoryEndpoint::Current(local_anchor) = completed.local_endpoint else {
        bail!("terminal writer clearance requires a current local endpoint");
    };
    Ok(TerminalTransferClearance {
        store: completed.store,
        local_anchor,
        identities: completed.identities,
        anchors: completed.anchors,
        retired: completed.retired,
        negative_closures: completed.negative_closures,
    })
}

fn finish_transfer_clearance_read(
    read: TerminalTransferClearanceRead,
    require_all_terminal: bool,
) -> anyhow::Result<CompletedTransferHistory> {
    let TerminalTransferClearanceRead {
        supplied_endpoints,
        store,
        local_endpoint,
        identities,
        anchors,
        retired,
        histories,
        pending_groups,
        scan_turn,
        closure_turn,
        inherited_negative,
        marked_negative,
        needed_negative_tips,
        observed_negative_tips,
        endpoint_tips,
        poisoned,
    } = read;
    if poisoned
        || supplied_endpoints
            .as_ref()
            .is_some_and(|endpoints| !endpoints.is_empty())
        || !pending_groups.is_empty()
        || !scan_turn.is_empty()
        || !closure_turn.is_empty()
        || needed_negative_tips.len() != observed_negative_tips.len()
        || histories.values().any(|(history, _)| !history.complete)
    {
        bail!("terminal clearance read is incomplete");
    }
    for (history, operations) in histories.values() {
        history.finish()?;
        for operation in operations.values() {
            verify_phase_slice_fences(history.latest_control.as_ref(), &operation.phases)?;
            verify_selected_freeze(operation)?;
            if let Some((_, prep)) = &operation.freeze_preparation {
                if history.initialized_root_binding.as_ref() != Some(&prep.root_binding) {
                    bail!("terminal clearance freeze root differs from original initialization");
                }
            }
        }
    }
    let mut negative_closures = Vec::new();
    for transfer in identities.values() {
        let (source_history, source_ops) = histories
            .get(&transfer.source.chain_root_id)
            .expect("source group frozen");
        let (destination_history, destination_ops) = histories
            .get(&transfer.destination.chain_root_id)
            .expect("destination group frozen");
        let source = covered_seat_operation(&source_history.seat, source_ops, transfer)?;
        let destination =
            covered_seat_operation(&destination_history.seat, destination_ops, transfer)?;
        // Historical UI folds may include still-live dependencies, but a
        // retired endpoint ALWAYS requires the existing terminal closure proof.
        // The actual retirement/finalization caller never selects this mode.
        let require_terminal = require_all_terminal
            || matches!(source_history.anchor, TerminalHistoryEndpoint::Retired(_))
            || matches!(
                destination_history.anchor,
                TerminalHistoryEndpoint::Retired(_)
            );
        let cancelled = if let Some((_, cancellation)) = &source.cancellation {
            if cancellation.transfer != *transfer
                || source.freeze_preparation.is_none()
                || !source.phases.is_empty()
            {
                bail!("terminal clearance retained cancellation conflicts with transfer phases");
            }
            validate_negative_destination_phases(transfer, &destination.phases)?;
            true
        } else {
            false
        };
        if cancelled && destination.phases.is_empty() {
            continue;
        }
        let lifecycle = fold_transfer_phase_slices(transfer, &source.phases, &destination.phases)?;
        let negative = completed_negative_terminal_decision(&lifecycle);
        if require_terminal && cancelled && negative.is_none() {
            bail!("terminal cancellation still has an undisposed destination inbound obligation");
        }
        if require_terminal
            && !cancelled
            && lifecycle.settlement_observed().is_none()
            && !(lifecycle.aborted().is_some() && lifecycle.released().is_some())
            && negative.is_none()
        {
            bail!("terminal clearance requires positive completed settlement on both seats");
        }
        if require_terminal
            && source.freeze_preparation.is_some()
            && !cancelled
            && !lifecycle.prepared().is_some_and(|prepared| {
                matches!(
                    &prepared.record.phase,
                    TransferPhase::Prepared {
                        freeze: Some(_),
                        ..
                    }
                )
            })
        {
            bail!("terminal clearance ordinary freeze has no matching MOVE preparation");
        }
        if let Some((evidence, decision)) = negative {
            for history in [source_history, destination_history] {
                if matches!(history.anchor, TerminalHistoryEndpoint::Retired(_))
                    && !marked_negative.contains(&(
                        history.seat.chain_root_id.clone(),
                        transfer.operation_id.clone(),
                    ))
                {
                    bail!("retired prefix lacks exact durable negative operation closure");
                }
            }
            let closure = if let Some(inherited) = inherited_negative.get(&transfer.operation_id) {
                if inherited.decision != decision || inherited.evidence != evidence {
                    bail!("retired negative closure differs from completed exact lifecycle");
                }
                inherited.clone()
            } else {
                ryeos_state::objects::terminal_seat_retirement::TerminalTransferNegativeClosure {
                    decision,
                    evidence,
                    source_tip: endpoint_tips
                        .get(&transfer.source.chain_root_id)
                        .ok_or_else(|| anyhow!("negative closure source has no authenticated tip"))?
                        .clone(),
                    destination_tip: endpoint_tips
                        .get(&transfer.destination.chain_root_id)
                        .ok_or_else(|| {
                            anyhow!("negative closure destination has no authenticated tip")
                        })?
                        .clone(),
                }
            };
            closure.validate()?;
            negative_closures.push(closure);
        } else if inherited_negative.contains_key(&transfer.operation_id) {
            bail!("retired negative closure does not match a terminal negative lifecycle");
        }
    }
    Ok(CompletedTransferHistory {
        store,
        local_endpoint,
        identities,
        anchors,
        retired,
        negative_closures,
    })
}

pub(crate) fn prepare_write(
    read: UiTransferSettlementRead,
) -> anyhow::Result<SettlementPreparation> {
    require_complete_settlement_read(
        read.poisoned,
        read.source.history.complete,
        read.destination.history.complete,
    )?;
    let source = finish_fixed_seat_read(read.source)?;
    let destination = finish_fixed_seat_read(read.destination)?;
    let source_operation = source
        .requested
        .get(&read.transfer.operation_id)
        .ok_or_else(|| anyhow!("selected source coverage absent"))?;
    if read.side == TransferSettlementSide::Source
        && source_operation.phases.is_empty()
        && source_operation.freeze_preparation.is_some()
    {
        return Ok(SettlementPreparation::FreezeCancellation(
            assemble_move_freeze_cancellation_proof(&source, &destination, &read.transfer)?,
        ));
    }
    let destination_operation = destination
        .requested
        .get(&read.transfer.operation_id)
        .ok_or_else(|| anyhow!("selected destination coverage absent"))?;
    let state = fold_transfer_phase_slices(
        &read.transfer,
        &source_operation.phases,
        &destination_operation.phases,
    )?;
    let plan = select_settlement(&state, read.side).map_err(anyhow::Error::msg)?;
    let TransferSettlementPlan::Candidate {
        record,
        requirement,
    } = &plan
    else {
        return Ok(SettlementPreparation::Deferred(plan));
    };
    // Complete signed histories and two exact publication anchors are only a
    // candidate for source Abort. Its durable reaping claim, pin fence and
    // both-head exclusion are checked by the lifecycle writer; a concurrent
    // destination Accepted changes its head and conflicts the publication.
    let (owner, counterpart) = match read.side {
        TransferSettlementSide::Source => (&source.history, &destination.history),
        TransferSettlementSide::Destination => (&destination.history, &source.history),
    };
    let (fence, control) = owner
        .latest_control
        .as_ref()
        .ok_or_else(|| anyhow!("transfer settlement seat has no retained producer control"))?;
    if control.state != UiSeatProducerState::Closed
        || *requirement
            == TransferSettlementRequirement::SerializeSourceDecisionAndDestinationAcceptance
        || *requirement
            == TransferSettlementRequirement::SerializeOrphanCancellationAndSourcePreparation
    {
        return Ok(SettlementPreparation::ReapingCandidate(
            ReapingTransferSettlementCandidate {
                store: read.store,
                record: record.clone(),
                latest_control: fence.clone(),
                closed_control: control.state == UiSeatProducerState::Closed,
                owner_anchor: owner.anchor.clone(),
                counterpart_anchor: counterpart.anchor.clone(),
                original_append_bound: owner
                    .original_append_bound
                    .ok_or_else(|| anyhow!("seat initialization bound is absent"))?,
            },
        ));
    }
    let payload = TransferLifecycleEventPayload::new(record.clone(), fence.clone())
        .map_err(anyhow::Error::msg)?;
    verify_closed_producer_fence(&payload, fence, control).map_err(anyhow::Error::msg)?;
    let owner_anchor = owner.anchor.clone();
    let counterpart_anchor = counterpart.anchor.clone();
    Ok(SettlementPreparation::Candidate(
        ClosedTransferSettlementCandidate {
            store: read.store,
            payload,
            owner_anchor,
            counterpart_anchor,
            original_append_bound: owner
                .original_append_bound
                .ok_or_else(|| anyhow!("seat initialization bound is absent"))?,
        },
    ))
}

fn select_settlement(
    state: &TransferLifecycleState,
    side: TransferSettlementSide,
) -> Result<TransferSettlementPlan, String> {
    use TransferSettlementPlan as Plan;
    use TransferSettlementRequirement as Requirement;

    let Some(prepared) = state.prepared() else {
        return match side {
            TransferSettlementSide::Source => Ok(Plan::AwaitingSourcePreparation),
            TransferSettlementSide::Destination => {
                if let Some(cancelled) = state.orphan_cancelled() {
                    Ok(Plan::AlreadyRecorded(cancelled.evidence.clone()))
                } else {
                    let record = TransferRecord {
                        schema: state.inbound().record.schema,
                        transfer: state.transfer().clone(),
                        phase: TransferPhase::OrphanCancelled {
                            inbound: state.inbound().evidence.clone(),
                        },
                    };
                    record.validate_shape()?;
                    Ok(Plan::Candidate {
                        record,
                        requirement: Requirement::SerializeOrphanCancellationAndSourcePreparation,
                    })
                }
            }
        };
    };

    let candidate = |phase, requirement| {
        let record = TransferRecord {
            schema: prepared.record.schema,
            transfer: state.transfer().clone(),
            phase,
        };
        record.validate_shape()?;
        Ok(Plan::Candidate {
            record,
            requirement,
        })
    };

    match side {
        TransferSettlementSide::Source => {
            if let Some(observed) = state.settlement_observed() {
                return Ok(Plan::AlreadyRecorded(observed.evidence.clone()));
            }
            if let Some(terminal) = state.activated().or(state.retained()).or(state.released()) {
                return candidate(
                    TransferPhase::SettlementObserved {
                        settlement: terminal.evidence.clone(),
                    },
                    Requirement::ExactCurrentPhaseAndOwningSeatFence,
                );
            }
            if state.committed().is_some() {
                return Ok(Plan::AwaitingDestinationSettlement);
            }
            if let Some(aborted) = state.aborted() {
                return if state.accepted().is_none() {
                    Ok(Plan::NoAcceptedReservation {
                        source_abort: Some(aborted.evidence.clone()),
                    })
                } else {
                    Ok(Plan::AwaitingDestinationSettlement)
                };
            }
            candidate(
                TransferPhase::Aborted {
                    preparation: prepared.evidence.clone(),
                },
                Requirement::SerializeSourceDecisionAndDestinationAcceptance,
            )
        }
        TransferSettlementSide::Destination => {
            if let Some(terminal) = state.activated().or(state.retained()).or(state.released()) {
                return Ok(Plan::AlreadyRecorded(terminal.evidence.clone()));
            }
            let Some(accepted) = state.accepted() else {
                return Ok(Plan::NoAcceptedReservation {
                    source_abort: state.aborted().map(|aborted| aborted.evidence.clone()),
                });
            };
            let TransferPhase::Accepted { import_digest, .. } = &accepted.record.phase else {
                return Err("transfer fold's acceptance has the wrong phase".into());
            };
            if let Some(committed) = state.committed() {
                return candidate(
                    TransferPhase::Retained {
                        commit: committed.evidence.clone(),
                        import_digest: import_digest.clone(),
                    },
                    Requirement::ExactCurrentPhaseAndOwningSeatFence,
                );
            }
            if let Some(aborted) = state.aborted() {
                return candidate(
                    TransferPhase::Released {
                        abort: aborted.evidence.clone(),
                        acceptance: accepted.evidence.clone(),
                        import_digest: import_digest.clone(),
                    },
                    Requirement::ExactCurrentPhaseAndOwningSeatFence,
                );
            }
            Ok(Plan::AwaitingSourceDecision)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_ui_presentation_contract::{
        payload_digest::seat_payload_digest,
        seat_control::{SEAT_PRODUCER_SCHEMA, UiSeatControlRequest, UiSeatProducerClaim},
        transfer::{TransferIdentity, TransferSeat},
        transfer_proof::TransferTestimony,
    };
    use serde_json::json;

    #[test]
    fn failed_or_empty_locator_releases_its_decoder_reservation() {
        assert!(discovery_slot_must_be_released(&Err(anyhow!(
            "locator failed"
        ))));
        assert!(discovery_slot_must_be_released(&Ok(false)));
        assert!(!discovery_slot_must_be_released(&Ok(true)));
    }

    #[tokio::test]
    async fn startup_exclusion_is_exact_and_borrowed_without_relocking() {
        let gate = Arc::new(tokio::sync::RwLock::new(()));
        let other = Arc::new(tokio::sync::RwLock::new(()));
        let exclusion = UiSeatStartupExclusion {
            gate: gate.clone(),
            guard: gate.clone().write_owned().await,
        };
        exclusion.validate(&gate).unwrap();
        assert!(exclusion.validate(&other).is_err());
        assert!(gate.try_read().is_err());
        assert!(gate.try_write().is_err());
        let held = SettlementExclusion(exclusion.guard());
        let _ = held.0;
        // Lifecycle's synchronous guarded path borrows this existing lock;
        // it must not attempt read_owned while startup holds its write side.
        drop(exclusion);
        assert!(gate.try_write().is_ok());
    }

    fn evidence(marker: &str, chain_seq: u64) -> TransferEvidence {
        TransferEvidence {
            event_hash: marker.repeat(32),
            chain_seq,
        }
    }

    fn prepared() -> TransferLifecycleState {
        let seat = |presentation_id: &str, side: &str| TransferSeat {
            presentation_id: presentation_id.into(),
            thread_id: format!("thread-{side}"),
            chain_root_id: format!("chain-{side}"),
            producer_incarnation: "ab".repeat(32),
            revision: 1,
        };
        let export = json!({"mode": "copy", "view": "retained"});
        let transfer = TransferIdentity {
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            source: seat("11111111-1111-4111-8111-111111111111", "source"),
            destination: seat("22222222-2222-4222-8222-222222222222", "destination"),
            export_digest: seat_payload_digest(&export).unwrap(),
        };
        let inbound = TransferRecord {
            schema: 1,
            transfer: transfer.clone(),
            phase: TransferPhase::InboundCommitment,
        };
        let mut state = TransferLifecycleState::begin(TransferTestimony {
            evidence: &evidence("ff", 1),
            record: &inbound,
        })
        .unwrap();
        let record = TransferRecord {
            schema: 1,
            transfer,
            phase: TransferPhase::Prepared {
                inbound: evidence("ff", 1),
                freeze: None,
                export,
            },
        };
        state
            .fold(TransferTestimony {
                evidence: &evidence("aa", 1),
                record: &record,
            })
            .unwrap();
        state
    }

    fn fold(state: &mut TransferLifecycleState, phase: TransferPhase, marker: &str, seq: u64) {
        let record = TransferRecord {
            schema: 1,
            transfer: state.transfer().clone(),
            phase,
        };
        state
            .fold(TransferTestimony {
                evidence: &evidence(marker, seq),
                record: &record,
            })
            .unwrap();
    }

    fn accepted() -> TransferLifecycleState {
        let mut state = prepared();
        let reservation = json!({"mount": 1});
        fold(
            &mut state,
            TransferPhase::Accepted {
                preparation: evidence("aa", 1),
                import_digest: seat_payload_digest(&reservation).unwrap(),
                reservation,
            },
            "bb",
            2,
        );
        state
    }

    #[test]
    fn destination_only_inbound_has_only_a_fenced_cancellation_candidate() {
        let prepared = prepared();
        let mut orphan = TransferLifecycleState::begin(prepared.inbound()).unwrap();
        assert_eq!(
            select_settlement(&orphan, TransferSettlementSide::Source).unwrap(),
            TransferSettlementPlan::AwaitingSourcePreparation
        );
        let TransferSettlementPlan::Candidate {
            record,
            requirement,
        } = select_settlement(&orphan, TransferSettlementSide::Destination).unwrap()
        else {
            panic!("destination orphan needs a fenced cancellation candidate")
        };
        assert_eq!(
            record.phase,
            TransferPhase::OrphanCancelled {
                inbound: evidence("ff", 1)
            }
        );
        assert_eq!(
            requirement,
            TransferSettlementRequirement::SerializeOrphanCancellationAndSourcePreparation
        );
        let mut wrong = prepared.prepared().unwrap().record.clone();
        let TransferPhase::Prepared { inbound, .. } = &mut wrong.phase else {
            unreachable!()
        };
        *inbound = evidence("ee", 1);
        assert!(
            orphan
                .fold(TransferTestimony {
                    evidence: &evidence("aa", 1),
                    record: &wrong,
                })
                .is_err()
        );
        assert!(matches!(
            select_settlement(&orphan, TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::Candidate { .. }
        ));

        orphan
            .fold(TransferTestimony {
                evidence: &evidence("dd", 2),
                record: &record,
            })
            .unwrap();
        assert_eq!(
            select_settlement(&orphan, TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::AlreadyRecorded(evidence("dd", 2))
        );
        assert!(orphan.fold(prepared.prepared().unwrap()).is_err());
        let (decision, retained) = completed_negative_terminal_decision(&orphan).unwrap();
        assert_eq!(decision, evidence("dd", 2));
        assert_eq!(retained, record);
    }

    #[test]
    fn linked_preparation_never_selects_orphan_cancellation() {
        let state = prepared();
        assert!(!matches!(
            select_settlement(&state, TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::Candidate {
                record: TransferRecord {
                    phase: TransferPhase::OrphanCancelled { .. },
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn orphan_cancellation_reader_requires_both_complete_signed_histories() {
        assert!(require_complete_settlement_read(false, true, true).is_ok());
        for (source, destination) in [(false, true), (true, false), (false, false)] {
            assert!(require_complete_settlement_read(false, source, destination).is_err());
        }
        assert!(require_complete_settlement_read(true, true, true).is_err());
    }

    #[test]
    fn prepared_without_its_signed_destination_inbound_cannot_begin_replay() {
        let state = prepared();
        assert!(TransferLifecycleState::begin(state.prepared().unwrap()).is_err());
        assert!(
            phase_order(&state.inbound().record.phase)
                < phase_order(&state.prepared().unwrap().record.phase)
        );
    }

    fn committed() -> TransferLifecycleState {
        let mut state = accepted();
        fold(
            &mut state,
            TransferPhase::Committed {
                preparation: evidence("aa", 1),
                acceptance: evidence("bb", 2),
                import_digest: seat_payload_digest(&json!({"mount": 1})).unwrap(),
            },
            "cc",
            3,
        );
        state
    }

    fn candidate(state: &TransferLifecycleState, side: TransferSettlementSide) -> TransferRecord {
        let TransferSettlementPlan::Candidate { record, .. } =
            select_settlement(state, side).unwrap()
        else {
            panic!("expected a provisional settlement candidate")
        };
        record
    }

    fn synthetic_history() -> (
        SeatHistory<()>,
        Vec<(String, ryeos_state::objects::thread_event::ThreadEvent)>,
    ) {
        use ryeos_state::objects::thread_event::NewEvent;
        let seat = prepared().transfer().source.clone();
        let hash = |marker: &str| marker.repeat(64);
        let make = |marker: &str, thread_seq, chain_seq, event_type: &str, payload| {
            (
                hash(marker),
                NewEvent::new(&seat.chain_root_id, &seat.thread_id, event_type)
                    .thread_seq(thread_seq)
                    .chain_seq(chain_seq)
                    .payload(payload)
                    .build(),
            )
        };
        let predecessor = UiSeatControlPredecessor::Initialization {
            event_hash: hash("a"),
        };
        let mut claim = UiSeatControlRequest::Claim {
            schema: 2,
            operation_id: String::new(),
            presentation_id: seat.presentation_id.clone(),
            thread_id: seat.thread_id.clone(),
            chain_root_id: seat.chain_root_id.clone(),
            predecessor,
            intent_nonce: "44444444-4444-4444-8444-444444444444".into(),
            claim: UiSeatProducerClaim::Fresh,
        };
        claim.assign_identity().unwrap();
        let incarnation = "b".repeat(64);
        let active = UiSeatProducer {
            schema_version: SEAT_PRODUCER_SCHEMA.into(),
            operation_id: claim.operation_id().into(),
            request: claim,
            claim: Some(UiSeatProducerClaim::Fresh),
            predecessor: None,
            producer_incarnation: incarnation.clone(),
            state: UiSeatProducerState::Active,
            next_engine_seq: 0,
        };
        let mut close = UiSeatControlRequest::Close {
            schema: 2,
            operation_id: String::new(),
            presentation_id: seat.presentation_id.clone(),
            thread_id: seat.thread_id.clone(),
            chain_root_id: seat.chain_root_id.clone(),
            predecessor: UiSeatControlPredecessor::Control {
                event_hash: hash("b"),
            },
            intent_nonce: "55555555-5555-4555-8555-555555555555".into(),
            producer_incarnation: incarnation.clone(),
        };
        close.assign_identity().unwrap();
        let closed = UiSeatProducer {
            schema_version: SEAT_PRODUCER_SCHEMA.into(),
            operation_id: close.operation_id().into(),
            request: close,
            claim: None,
            predecessor: Some(incarnation.clone()),
            producer_incarnation: incarnation.clone(),
            state: UiSeatProducerState::Closed,
            next_engine_seq: 1,
        };
        let semantic_payload = json!({"value": true});
        let semantic = [SeatAppendSemanticEvent {
            engine_seq: 0,
            event_type: "seat.facet",
            payload: &semantic_payload,
        }];
        let digest = seat_payload_digest(&serde_json::to_value(semantic).unwrap()).unwrap();
        let mut receipt = SeatAppendReceipt {
            schema_version: SEAT_APPEND_RECEIPT_SCHEMA.into(),
            producer_incarnation: incarnation.clone(),
            operation_id: String::new(),
            first_engine_seq: 0,
            last_engine_seq: 0,
            event_count: 1,
            payload_digest: digest.clone(),
        };
        receipt.operation_id = UiSeatAppendIdentity {
            presentation_id: seat.presentation_id.clone(),
            chain_root_id: seat.chain_root_id.clone(),
            thread_id: seat.thread_id.clone(),
            producer_incarnation: incarnation,
            first_engine_seq: 0,
            last_engine_seq: 0,
            event_count: 1,
            payload_digest: digest,
        }
        .derive_identity()
        .unwrap();
        let events = vec![
            make(
                "d",
                5,
                9,
                SEAT_PRODUCER_EVENT,
                serde_json::to_value(closed).unwrap(),
            ),
            make(
                "c",
                4,
                7,
                "ui_seat.append_receipt.v2",
                serde_json::to_value(receipt).unwrap(),
            ),
            make(
                "e",
                3,
                6,
                "seat.facet",
                json!({"seq":0,"payload":semantic_payload}),
            ),
            make(
                "b",
                2,
                5,
                SEAT_PRODUCER_EVENT,
                serde_json::to_value(active).unwrap(),
            ),
            make(
                "a",
                1,
                3,
                "ui_seat.presentation_initialized.v1",
                json!({
                    "operation_id":"66666666-6666-4666-8666-666666666666",
                    "initialization":{"schema":2,"selection":{"kind":"configured_defaults"},
                        "creation_request_id":"66666666-6666-4666-8666-666666666666",
                        "presentation_id":seat.presentation_id,
                        "root_binding":{"binding_attachment_id":"root","binding_generation":1,
                            "binding_digest":"a".repeat(64)},
                        "binding_attachments":[{"binding_attachment_id":"root",
                            "binding_generation":1,"binding_digest":"a".repeat(64),
                            "binding_request_bounds":{"max_request_bytes":4096,
                                "max_input_bytes":1024,"max_seat_append_bytes":65536}}]}
                }),
            ),
            make("f", 0, 1, "thread_created", json!({})),
        ];
        let history = SeatHistory {
            seat,
            anchor: (),
            progress: None,
            complete: false,
            previous_chain_seq: None,
            previous_thread_seq: None,
            latest_control: None,
            newer_control: None,
            frontier: None,
            interval_producer: None,
            pending_receipt: None,
            pending_receipt_bytes: 0,
            pending_receipt_evidence: None,
            validated_receipt: None,
            validated_receipt_events: Vec::new(),
            pending_cancellation_control: None,
            initialization_seen: false,
            original_append_bound: None,
            initialized_root_binding: None,
            initialized_payload: None,
            phases: Vec::new(),
        };
        (history, events)
    }

    #[test]
    fn signed_identity_locator_keeps_only_exact_phase_owner() {
        use ryeos_state::objects::thread_event::NewEvent;
        let state = prepared();
        let mut record = state.prepared().unwrap().record.clone();
        record.transfer.source.chain_root_id = record.transfer.source.thread_id.clone();
        let source = &record.transfer.source;
        let payload = ryeos_ui_presentation_contract::transfer::TransferEventPayload {
            seq: 0,
            operation_id: record.transfer.operation_id.clone(),
            payload: record.clone(),
        };
        let event = NewEvent::new(&source.chain_root_id, &source.thread_id, TRANSFER_EVENT)
            .thread_seq(1)
            .chain_seq(1)
            .payload(serde_json::to_value(payload).unwrap())
            .build();
        assert_eq!(
            signed_transfer_identity_for_seat(&source.thread_id, &event)
                .unwrap()
                .unwrap(),
            record.transfer
        );
        assert!(signed_transfer_identity_for_seat("another-seat", &event).is_err());
        let ordinary = NewEvent::new(&source.chain_root_id, &source.thread_id, "thread_created")
            .thread_seq(1)
            .chain_seq(1)
            .payload(json!({}))
            .build();
        assert!(
            signed_transfer_identity_for_seat(&source.thread_id, &ordinary)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn signed_identity_extractor_does_not_infer_absence_from_newer_event() {
        use ryeos_state::objects::thread_event::NewEvent;

        let mut record = prepared().prepared().unwrap().record.clone();
        record.transfer.source.chain_root_id = record.transfer.source.thread_id.clone();
        let seat = &record.transfer.source.thread_id;
        let ordinary = NewEvent::new(seat, seat, "seat.facet")
            .thread_seq(3)
            .chain_seq(3)
            .payload(json!({"value": true}))
            .build();
        let malformed = NewEvent::new(seat, seat, TRANSFER_EVENT)
            .thread_seq(2)
            .chain_seq(2)
            .payload(json!({"invalid": true}))
            .build();
        let transfer = NewEvent::new(seat, seat, TRANSFER_EVENT)
            .thread_seq(1)
            .chain_seq(1)
            .payload(
                serde_json::to_value(
                    ryeos_ui_presentation_contract::transfer::TransferEventPayload {
                        seq: 0,
                        operation_id: record.transfer.operation_id.clone(),
                        payload: record.clone(),
                    },
                )
                .unwrap(),
            )
            .build();

        // A newer event gives no negative proof. Malformed signed transfer
        // content is an error, not an empty locator result.
        assert!(
            signed_transfer_identity_for_seat(seat, &ordinary)
                .unwrap()
                .is_none()
        );
        assert!(signed_transfer_identity_for_seat(seat, &malformed).is_err());

        // An older positive is still discoverable; this locator never grants
        // settlement authority.
        let found = signed_transfer_identity_for_seat(seat, &transfer).unwrap();
        assert_eq!(found, Some(record.transfer));
    }

    #[test]
    fn signed_identity_locator_refuses_unknown_transfer_family_members() {
        use ryeos_state::objects::thread_event::NewEvent;
        let seat = "T-transfer-fixture";
        for kind in [
            format!("{TRANSFER_EVENT}.future"),
            format!("{TRANSFER_LIFECYCLE_V2_EVENT}.future"),
            "contract.ryeos_ui.move_freeze_cancelled.v2".into(),
        ] {
            let event = NewEvent::new(seat, seat, &kind)
                .thread_seq(1)
                .chain_seq(1)
                .payload(json!({}))
                .build();
            let error = signed_transfer_identity_for_seat(seat, &event).unwrap_err();
            assert!(error.to_string().contains("unsupported testimony"));
        }
    }

    #[test]
    fn reverse_reader_cannot_skip_unknown_transfer_testimony() {
        use ryeos_state::objects::thread_event::NewEvent;
        let transfer = prepared().transfer().clone();
        for kind in [
            format!("{TRANSFER_EVENT}.future"),
            format!("{TRANSFER_LIFECYCLE_V2_EVENT}.future"),
        ] {
            let (mut history, _) = synthetic_history();
            let event = NewEvent::new(&history.seat.chain_root_id, &history.seat.thread_id, &kind)
                .thread_seq(10)
                .chain_seq(10)
                .payload(json!({}))
                .build();
            let error = history.push(&transfer, "a".repeat(64), event).unwrap_err();
            assert!(error.to_string().contains("unsupported testimony"));
            assert!(history.phases.is_empty());
        }
    }

    #[test]
    fn cancellation_current_witness_is_closed_by_historical_receipts_and_control() {
        use ryeos_ui_presentation_contract::move_freeze_lifecycle::{
            MoveFreezeCancellation, MoveFreezeCancellationWriter,
        };
        let make = |incarnation: &str, next_engine_seq| {
            MoveFreezeCancellation::new(
                prepared().transfer().clone(),
                ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence {
                    preparation: evidence("01", 1),
                    completion: evidence("02", 2),
                }
                .into(),
                MoveFreezeCancellationWriter::CurrentProducer {
                    producer_incarnation: incarnation.repeat(64),
                    next_engine_seq,
                },
            )
            .unwrap()
        };
        let (mut history, events) = synthetic_history();
        history
            .push(
                prepared().transfer(),
                events[0].0.clone(),
                events[0].1.clone(),
            )
            .unwrap();
        assert!(
            history
                .authenticate_cancellation_witness(&make("b", 2))
                .is_err()
        );
        history
            .authenticate_cancellation_witness(&make("b", 1))
            .unwrap();
        for (hash, event) in events[1..].iter().cloned() {
            history.push(prepared().transfer(), hash, event).unwrap();
        }
        history.finish().unwrap();
        let (mut wrong, events) = synthetic_history();
        wrong
            .push(
                prepared().transfer(),
                events[0].0.clone(),
                events[0].1.clone(),
            )
            .unwrap();
        wrong
            .authenticate_cancellation_witness(&make("c", 1))
            .unwrap();
        assert!(
            events[1..]
                .iter()
                .cloned()
                .try_for_each(|(hash, event)| wrong.push(prepared().transfer(), hash, event))
                .is_err()
        );
    }

    #[test]
    fn selected_move_requires_receipted_freeze_not_a_hash_pair() {
        let state = prepared();
        let transfer = state.transfer().clone();
        let mut requested =
            freeze_requested_seat_operations(&transfer.source, std::slice::from_ref(&transfer))
                .unwrap();
        let operation = requested.get_mut(&transfer.operation_id).unwrap();
        let mut record = state.prepared().unwrap().record.clone();
        let TransferPhase::Prepared { freeze, .. } = &mut record.phase else {
            unreachable!()
        };
        *freeze = Some(
            ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence {
                preparation: evidence("01", 1),
                completion: evidence("02", 2),
            },
        );
        operation.phases.push(RetainedPhase {
            evidence: evidence("aa", 3),
            record,
            lifecycle: None,
        });
        assert!(
            verify_selected_freeze(operation)
                .unwrap_err()
                .to_string()
                .contains("receipted freeze witnesses")
        );
    }

    #[test]
    fn reverse_control_receipt_and_initialization_survive_page_split() {
        let (mut history, events) = synthetic_history();
        let transfer = prepared().transfer().clone();
        for (hash, event) in events[..2].iter().cloned() {
            history.push(&transfer, hash, event).unwrap();
        }
        assert!(history.pending_receipt.is_some());
        for (hash, event) in events[2..].iter().cloned() {
            history.push(&transfer, hash, event).unwrap();
        }
        history.finish().unwrap();
        assert_eq!(
            history.latest_control.as_ref().unwrap().1.state,
            UiSeatProducerState::Closed
        );
    }

    #[test]
    fn frozen_seat_requests_bind_full_identities_but_only_stable_context() {
        let transfer = prepared().transfer().clone();
        let mut second = transfer.clone();
        second.operation_id = "77777777-7777-4777-8777-777777777777".into();
        second.source.revision += 1;
        second.source.producer_incarnation = "c".repeat(64);
        let mut context = transfer.source.clone();
        context.revision += 100;
        context.producer_incarnation = "d".repeat(64);
        let mut requests = vec![transfer.clone(), second.clone(), transfer.clone()];
        let frozen = freeze_requested_seat_operations(&context, &requests).unwrap();
        assert_eq!(frozen.len(), 2);
        assert_eq!(frozen[&second.operation_id].owner, second.source);
        requests[0].export_digest = "e".repeat(64);
        assert!(covered_seat_operation(&context, &frozen, &transfer).is_ok());
        assert!(covered_seat_operation(&context, &frozen, &requests[0]).is_err());
        assert!(freeze_requested_seat_operations(&context, &requests).is_err());
        let mut wrong_context = context.clone();
        wrong_context.presentation_id = "88888888-8888-4888-8888-888888888888".into();
        assert!(freeze_requested_seat_operations(&wrong_context, &[transfer.clone()]).is_err());
        let empty = freeze_requested_seat_operations(&context, &[]).unwrap();
        assert!(covered_seat_operation(&context, &empty, &transfer).is_err());
        let mut malformed = transfer;
        malformed.destination.thread_id = malformed.source.thread_id.clone();
        assert!(freeze_requested_seat_operations(&context, &[malformed]).is_err());
    }

    fn synthetic_receipt_tip_join() -> (
        SeatHistory<()>,
        SeatHistory<()>,
        FixedSeatEndpoint,
        String,
        ryeos_state::objects::thread_event::ThreadEvent,
    ) {
        let (mut cached, events) = synthetic_history();
        // This complete cut ends at the receipt, AFTER its Active control's
        // original frontier. A newer Close must use the head frontier (1),
        // not the original control frontier (0), when authenticating the join.
        for (hash, event) in events[1..].iter().cloned() {
            assert!(cached.validate_seat_event(hash, event).unwrap().is_none());
        }
        cached.finish().unwrap();
        let (mut suffix, _) = synthetic_history();
        suffix
            .validate_seat_event(events[0].0.clone(), events[0].1.clone())
            .unwrap();
        let endpoint = FixedSeatEndpoint {
            tip: TransferEvidence {
                event_hash: events[1].0.clone(),
                chain_seq: events[1].1.chain_seq,
            },
            thread_seq: events[1].1.thread_seq,
            next_engine_seq: 1,
        };
        (
            cached,
            suffix,
            endpoint,
            events[1].0.clone(),
            events[1].1.clone(),
        )
    }

    #[test]
    fn suffix_join_authenticates_control_after_receipt_advanced_cached_frontier() {
        let (cached, mut suffix, endpoint, hash, event) = synthetic_receipt_tip_join();
        assert_eq!(cached.latest_control.as_ref().unwrap().1.next_engine_seq, 0);
        assert_eq!(endpoint.next_engine_seq, 1);
        assert!(!suffix.initialization_seen);
        finish_fixed_seat_join(&mut suffix, &cached, &endpoint, &hash, &event).unwrap();
        assert!(
            !suffix.initialization_seen,
            "a suffix must not impersonate genesis replay"
        );
        assert_eq!(
            suffix.latest_control.as_ref().unwrap().1.state,
            UiSeatProducerState::Closed
        );
        assert_eq!(suffix.original_append_bound, Some(65536));
        // The join receipt was not semantically folded a second time.
        assert!(suffix.pending_receipt.is_none());
        assert_eq!(suffix.frontier, Some(1));
        assert_eq!(suffix.previous_thread_seq, Some(5));
    }

    #[test]
    fn suffix_join_refuses_changed_endpoint_pending_receipt_frontier_and_producer() {
        for fault in [
            "hash",
            "thread",
            "chain",
            "adjacency",
            "pending",
            "frontier",
            "producer",
            "control",
        ] {
            let (mut cached, mut suffix, endpoint, mut hash, mut event) =
                synthetic_receipt_tip_join();
            match fault {
                "hash" => hash = "f".repeat(64),
                "thread" => event.thread_seq += 1,
                "chain" => event.chain_seq += 1,
                "adjacency" => suffix.previous_thread_seq = Some(6),
                "pending" => {
                    suffix.pending_receipt = Some((
                        serde_json::from_value(event.payload.clone()).unwrap(),
                        Vec::new(),
                    ))
                }
                "frontier" => suffix.frontier = Some(0),
                "producer" => suffix.interval_producer = Some("c".repeat(64)),
                _ => cached.latest_control.as_mut().unwrap().0.event_hash = "f".repeat(64),
            }
            assert!(
                finish_fixed_seat_join(&mut suffix, &cached, &endpoint, &hash, &event).is_err(),
                "{fault}"
            );
            assert!(
                suffix.original_append_bound.is_none(),
                "{fault} inherited ancestry before authentication"
            );
            assert!(!suffix.initialization_seen);
        }
    }

    #[test]
    fn shared_seat_validation_routes_each_requested_origin_without_rescanning() {
        use ryeos_state::objects::thread_event::NewEvent;
        let state = prepared();
        let first = state.transfer().clone();
        let mut second = first.clone();
        second.operation_id = "77777777-7777-4777-8777-777777777777".into();
        second.source.revision += 1;
        second.source.producer_incarnation = "c".repeat(64);
        let (mut history, _) = synthetic_history();
        let mut requested =
            freeze_requested_seat_operations(&history.seat, &[first.clone(), second.clone()])
                .unwrap();
        let first_record = state.prepared().unwrap().record.clone();
        let mut second_record = first_record.clone();
        second_record.transfer = second.clone();
        let mut unrequested = first_record.clone();
        unrequested.transfer.operation_id = "88888888-8888-4888-8888-888888888888".into();
        for (index, record) in [first_record.clone(), second_record, unrequested]
            .into_iter()
            .enumerate()
        {
            let seq = 13 - index as u64;
            let event = NewEvent::new(
                &history.seat.chain_root_id,
                &history.seat.thread_id,
                TRANSFER_EVENT,
            )
            .thread_seq(seq)
            .chain_seq(seq)
            .payload(json!({"seq":0,"operation_id":record.transfer.operation_id,"payload":record}))
            .build();
            let phase = history
                .validate_seat_event(format!("{seq:064x}"), event)
                .unwrap()
                .unwrap();
            route_requested_seat_phase(&mut requested, phase).unwrap();
        }
        assert_eq!(history.previous_thread_seq, Some(11));
        assert!(history.phases.is_empty());
        assert_eq!(requested.len(), 2);
        assert_eq!(requested[&first.operation_id].phases.len(), 1);
        assert_eq!(requested[&second.operation_id].phases.len(), 1);
        assert_eq!(
            requested[&second.operation_id].phases[0].record.transfer,
            second
        );
        let mut changed_origin = first_record;
        changed_origin.transfer.source.revision += 1;
        let phase = RetainedPhase {
            evidence: evidence("ee", 10),
            record: changed_origin,
            lifecycle: None,
        };
        assert!(route_requested_seat_phase(&mut requested, phase).is_err());
        assert_eq!(
            history.previous_thread_seq,
            Some(11),
            "routing never consumes ancestry"
        );
        // Partial synthetic ancestry is not a completed fixed-seat proof.
        assert!(history.finish().is_err());
    }

    #[test]
    fn common_seat_validation_refuses_foreign_presentation_before_operation_selection() {
        use ryeos_state::objects::thread_event::NewEvent;
        let (mut history, _) = synthetic_history();
        let mut record = prepared().prepared().unwrap().record.clone();
        record.transfer.operation_id = "88888888-8888-4888-8888-888888888888".into();
        record.transfer.source.presentation_id = "77777777-7777-4777-8777-777777777777".into();
        let event = NewEvent::new(
            &history.seat.chain_root_id,
            &history.seat.thread_id,
            TRANSFER_EVENT,
        )
        .thread_seq(10)
        .chain_seq(10)
        .payload(json!({"seq":0,"operation_id":record.transfer.operation_id,"payload":record}))
        .build();
        let error = history
            .validate_seat_event("c".repeat(64), event)
            .err()
            .expect("foreign presentation must refuse common validation");
        assert!(error.to_string().contains("another stable seat"));
        assert!(history.phases.is_empty());
    }

    #[test]
    fn common_validation_cannot_skip_unrequested_foreign_thread_or_root() {
        use ryeos_state::objects::thread_event::NewEvent;
        for change_root in [false, true] {
            let (mut history, _) = synthetic_history();
            let mut record = prepared().prepared().unwrap().record.clone();
            record.transfer.operation_id = "88888888-8888-4888-8888-888888888888".into();
            if change_root {
                record.transfer.source.chain_root_id = "T-other-root".into();
            } else {
                record.transfer.source.thread_id = "T-other-thread".into();
            }
            let event = NewEvent::new(
                &history.seat.chain_root_id,
                &history.seat.thread_id,
                TRANSFER_EVENT,
            )
            .thread_seq(10)
            .chain_seq(10)
            .payload(json!({"seq":0,"operation_id":record.transfer.operation_id,"payload":record}))
            .build();
            assert!(history.validate_seat_event("c".repeat(64), event).is_err());
            assert!(history.phases.is_empty());
        }
    }

    #[test]
    fn independent_seat_validation_reconstructs_ancestry_without_phase_routing() {
        let (mut history, events) = synthetic_history();
        for (index, (hash, event)) in events.into_iter().enumerate() {
            assert!(history.validate_seat_event(hash, event).unwrap().is_none());
            assert!(history.phases.is_empty());
            if index == 1 {
                assert!(history.pending_receipt.is_some());
            }
            if index == 2 {
                assert!(history.pending_receipt.is_none());
                assert_eq!(history.pending_receipt_bytes, 0);
            }
        }
        history.finish().unwrap();
        assert_eq!(history.frontier, Some(0));
        assert_eq!(history.original_append_bound, Some(65536));
        assert_eq!(
            history.latest_control.as_ref().unwrap().1.state,
            UiSeatProducerState::Closed
        );
    }

    #[test]
    fn decoded_phases_are_independent_of_selected_operation_and_bounded_routing() {
        use ryeos_state::objects::thread_event::NewEvent;
        let state = prepared();
        let transfer = state.transfer().clone();
        let (mut history, _) = synthetic_history();
        let selected = state.prepared().unwrap().record.clone();
        let mut unrelated = selected.clone();
        unrelated.transfer.operation_id = "77777777-7777-4777-8777-777777777777".into();
        // Same stable seat, different operation origin. The common validator
        // must not compare this origin to the selected transfer's TransferSeat.
        unrelated.transfer.source.revision += 1;
        unrelated.transfer.source.producer_incarnation = "c".repeat(64);
        let event = |record: &TransferRecord, seq| {
            NewEvent::new(
                &history.seat.chain_root_id,
                &history.seat.thread_id,
                TRANSFER_EVENT,
            )
            .thread_seq(seq)
            .chain_seq(seq)
            .payload(json!({"seq":0,"operation_id":record.transfer.operation_id,"payload":record}))
            .build()
        };
        let unrelated_event = event(&unrelated, 11);
        let selected_event = event(&selected, 10);
        let unrelated_phase = history
            .validate_seat_event("c".repeat(64), unrelated_event)
            .unwrap()
            .unwrap();
        let selected_phase = history
            .validate_seat_event("d".repeat(64), selected_event)
            .unwrap()
            .unwrap();
        assert_eq!(unrelated_phase.record, unrelated);
        assert_eq!(selected_phase.record, selected);
        assert!(history.phases.is_empty());
        let reconstructed_order = (history.previous_chain_seq, history.previous_thread_seq);
        history
            .route_operation_phase(&transfer, unrelated_phase)
            .unwrap();
        assert!(history.phases.is_empty());
        history
            .route_operation_phase(&transfer, selected_phase)
            .unwrap();
        assert_eq!(history.phases.len(), 1);
        assert_eq!(history.phases[0].evidence.event_hash, "d".repeat(64));
        assert_eq!(
            (history.previous_chain_seq, history.previous_thread_seq),
            reconstructed_order
        );
        // Routing alone keeps the existing duplicate-phase refusal. Neither
        // these provisional phases nor partial ancestry imply settlement.
        let duplicate = RetainedPhase {
            evidence: evidence("ee", 9),
            record: selected,
            lifecycle: None,
        };
        let error = history
            .route_operation_phase(&transfer, duplicate)
            .unwrap_err();
        assert!(error.to_string().contains("repeats a settled phase"));
    }

    #[test]
    fn reverse_reader_rejects_broken_receipt_adjacency_and_frontier() {
        let transfer = prepared().transfer().clone();
        let (mut history, mut events) = synthetic_history();
        events[2].1.chain_seq = 5;
        for (hash, event) in events[..2].iter().cloned() {
            history.push(&transfer, hash, event).unwrap();
        }
        assert!(
            history
                .push(&transfer, events[2].0.clone(), events[2].1.clone())
                .is_err()
        );
        let (mut history, mut events) = synthetic_history();
        events[0].1.payload["next_engine_seq"] = json!(2);
        assert!(
            history
                .push(&transfer, events[0].0.clone(), events[0].1.clone())
                .is_ok()
        );
        history
            .push(&transfer, events[1].0.clone(), events[1].1.clone())
            .unwrap();
        assert!(
            history
                .push(&transfer, events[2].0.clone(), events[2].1.clone())
                .is_err()
        );
    }

    #[test]
    fn reverse_reader_rejects_wrong_initialization_subject() {
        let (mut history, mut events) = synthetic_history();
        let transfer = prepared().transfer().clone();
        events[4].1.payload["initialization"]["presentation_id"] =
            json!("77777777-7777-4777-8777-777777777777");
        for (hash, event) in events[..4].iter().cloned() {
            history.push(&transfer, hash, event).unwrap();
        }
        assert!(
            history
                .push(&transfer, events[4].0.clone(), events[4].1.clone())
                .is_err()
        );
    }

    #[test]
    fn precommit_abort_explicitly_requires_late_acceptance_exclusion() {
        for state in [prepared(), accepted()] {
            let TransferSettlementPlan::Candidate {
                record,
                requirement,
            } = select_settlement(&state, TransferSettlementSide::Source).unwrap()
            else {
                panic!("expected provisional abort")
            };
            assert!(matches!(record.phase, TransferPhase::Aborted { .. }));
            assert_eq!(
                requirement,
                TransferSettlementRequirement::SerializeSourceDecisionAndDestinationAcceptance
            );
        }
        assert_eq!(
            select_settlement(&accepted(), TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::AwaitingSourceDecision
        );
    }

    #[test]
    fn abort_without_acceptance_never_manufactures_release_or_observation() {
        let mut state = prepared();
        fold(
            &mut state,
            TransferPhase::Aborted {
                preparation: evidence("aa", 1),
            },
            "cc",
            3,
        );
        let (decision, record) = completed_negative_terminal_decision(&state).unwrap();
        assert_eq!(decision, evidence("cc", 3));
        assert!(matches!(record.phase, TransferPhase::Aborted { .. }));
        assert!(completed_negative_terminal_decision(&accepted()).is_none());
        for side in [
            TransferSettlementSide::Source,
            TransferSettlementSide::Destination,
        ] {
            assert_eq!(
                select_settlement(&state, side).unwrap(),
                TransferSettlementPlan::NoAcceptedReservation {
                    source_abort: Some(evidence("cc", 3)),
                }
            );
        }
    }

    #[test]
    fn committed_data_is_retained_then_observed_without_live_activation() {
        let mut state = committed();
        assert_eq!(
            select_settlement(&state, TransferSettlementSide::Source).unwrap(),
            TransferSettlementPlan::AwaitingDestinationSettlement
        );
        let retained = candidate(&state, TransferSettlementSide::Destination);
        assert!(matches!(retained.phase, TransferPhase::Retained { .. }));
        fold(&mut state, retained.phase, "dd", 4);
        let observed = candidate(&state, TransferSettlementSide::Source);
        assert_eq!(
            observed.phase,
            TransferPhase::SettlementObserved {
                settlement: evidence("dd", 4),
            }
        );
        fold(&mut state, observed.phase, "ee", 5);
        assert_eq!(
            select_settlement(&state, TransferSettlementSide::Source).unwrap(),
            TransferSettlementPlan::AlreadyRecorded(evidence("ee", 5))
        );
        assert_eq!(
            select_settlement(&state, TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::AlreadyRecorded(evidence("dd", 4))
        );
    }

    #[test]
    fn accepted_abort_releases_exact_reservation_before_source_observation() {
        let mut state = accepted();
        fold(
            &mut state,
            TransferPhase::Aborted {
                preparation: evidence("aa", 1),
            },
            "cc",
            3,
        );
        assert_eq!(
            select_settlement(&state, TransferSettlementSide::Source).unwrap(),
            TransferSettlementPlan::AwaitingDestinationSettlement
        );
        let released = candidate(&state, TransferSettlementSide::Destination);
        assert_eq!(
            released.phase,
            TransferPhase::Released {
                abort: evidence("cc", 3),
                acceptance: evidence("bb", 2),
                import_digest: seat_payload_digest(&json!({"mount": 1})).unwrap(),
            }
        );
        fold(&mut state, released.phase, "dd", 4);
        assert_eq!(
            candidate(&state, TransferSettlementSide::Source).phase,
            TransferPhase::SettlementObserved {
                settlement: evidence("dd", 4),
            }
        );
    }

    #[test]
    fn historical_activation_is_preserved_after_session_loss() {
        let mut state = committed();
        fold(
            &mut state,
            TransferPhase::Activated {
                commit: evidence("cc", 3),
                import_digest: seat_payload_digest(&json!({"mount": 1})).unwrap(),
            },
            "dd",
            4,
        );
        assert_eq!(
            select_settlement(&state, TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::AlreadyRecorded(evidence("dd", 4))
        );
        assert_eq!(
            candidate(&state, TransferSettlementSide::Source).phase,
            TransferPhase::SettlementObserved {
                settlement: evidence("dd", 4),
            }
        );
    }

    #[test]
    fn pending_receipt_budget_accepts_the_full_native_protocol_interval() {
        let event = ryeos_state::objects::thread_event::MAX_THREAD_EVENT_SERIALIZED_BYTES as u64;
        let mut bytes = 0;
        for _ in 0..128 {
            bytes = charge_pending_receipt_bytes(bytes, event).unwrap();
        }
        assert_eq!(bytes, MAX_PENDING_RECEIPT_BYTES);
        assert!(charge_pending_receipt_bytes(bytes, 1).is_err());
        assert!(charge_pending_receipt_bytes(u64::MAX, 1).is_err());
    }

    #[test]
    fn settlement_turns_revisit_long_readers_without_eviction() {
        let first = ("seat-a".to_owned(), "transfer-a".to_owned());
        let second = ("seat-b".to_owned(), "transfer-b".to_owned());
        let mut turns = VecDeque::from([first.clone(), second.clone()]);
        for expected in [first.clone(), second.clone(), first.clone(), second.clone()] {
            let active = next_settlement_turn(&mut turns).unwrap();
            assert_eq!(active, expected);
            turns.push_back(active);
        }
        assert_eq!(turns.len(), MAX_SETTLEMENT_CURSORS);
    }

    #[test]
    fn repeated_reader_failures_release_only_the_process_local_decoder_slot() {
        let mut failures = 0;
        assert!(retain_failed_settlement_reader(&mut failures));
        assert!(retain_failed_settlement_reader(&mut failures));
        assert!(!retain_failed_settlement_reader(&mut failures));
        assert_eq!(failures, MAX_SETTLEMENT_READER_FAILURES);
        // A newly discovered exact operation starts a new bounded attempt;
        // dropping the old cursor does not assert signed-history absence.
        let mut rediscovered = 0;
        assert!(retain_failed_settlement_reader(&mut rediscovered));
    }

    #[test]
    fn unreadable_pending_positive_hint_yields_to_older_locator_pages() {
        let mut cursors = ClaimedTransferSettlementCursors::default();
        cursors.pending.insert(
            "seat".into(),
            PendingClaimedTransfer {
                claim: crate::runtime_db::ClaimedSeatLeaseSnapshot {
                    seat_thread_id: "seat".into(),
                    owner: "owner".into(),
                    surface: "surface".into(),
                    client_ref: "client".into(),
                    reaping_at_ms: 1,
                },
                transfer: prepared().transfer().clone(),
                side: TransferSettlementSide::Source,
                failures: 0,
            },
        );
        cursors.reserved_slots = 1;
        for _ in 1..MAX_SETTLEMENT_READER_FAILURES {
            note_pending_reader_start_failure(&mut cursors, "seat");
            assert!(cursors.pending.contains_key("seat"));
        }
        note_pending_reader_start_failure(&mut cursors, "seat");
        assert!(!cursors.pending.contains_key("seat"));
        // The maintenance caller still owns this reservation and releases it
        // on the returned error; no reader or signed evidence was retired.
        assert_eq!(cursors.reserved_slots, 1);
        assert!(cursors.readers.is_empty());
    }

    #[test]
    fn failed_ready_publication_keeps_a_bounded_exact_retry_turn() {
        let key = ("seat".to_owned(), "operation".to_owned());
        let mut cursors = ClaimedTransferSettlementCursors::default();
        for failure in 1..=MAX_SETTLEMENT_READER_FAILURES {
            retain_failed_ready_settlement(
                &mut cursors,
                key.clone(),
                ReadyClaimedTransferSettlement {
                    claim: crate::runtime_db::ClaimedSeatLeaseSnapshot {
                        seat_thread_id: "seat".into(),
                        owner: "owner".into(),
                        surface: "surface".into(),
                        client_ref: "client".into(),
                        reaping_at_ms: 1,
                    },
                    transfer: prepared().transfer().clone(),
                    side: TransferSettlementSide::Source,
                    candidate: SettlementPreparation::Deferred(
                        TransferSettlementPlan::AwaitingSourceDecision,
                    ),
                    failures: failure - 1,
                },
            );
            if failure < MAX_SETTLEMENT_READER_FAILURES {
                assert_eq!(cursors.turn.pop_front(), Some(key.clone()));
                let retained = cursors
                    .ready
                    .remove(&key)
                    .expect("ready candidate retained");
                assert_eq!(retained.failures, failure);
            } else {
                assert!(cursors.turn.is_empty());
                assert!(cursors.ready.is_empty());
            }
        }
        // The retry cache is process-local. Its exhaustion never retires a
        // durable claim or supplies negative signed-history evidence.
        assert_eq!(cursors.reserved_slots, 0);
    }

    #[test]
    fn in_flight_publication_keeps_its_decoder_slot_reserved() {
        let mut cursors = ClaimedTransferSettlementCursors::default();
        cursors.publishing_slots = 1;
        assert!(decoder_has_room(retained_settlement_decoders(&cursors), 0));
        cursors.reserved_slots = 1;
        assert!(!decoder_has_room(
            retained_settlement_decoders(&cursors),
            cursors.reserved_slots,
        ));
    }

    #[test]
    fn maintenance_discovery_cannot_outproduce_one_reserved_decoder_slot() {
        assert_eq!(MAINTENANCE_DISCOVERY_PAGES, 1);
        assert_eq!(MAINTENANCE_DISCOVERY_EVENTS_PER_PAGE, 1);
        // `signed_transfer_identity_for_seat` returns one Option per event.
        assert_eq!(
            MAINTENANCE_DISCOVERY_PAGES * MAINTENANCE_DISCOVERY_EVENTS_PER_PAGE,
            1
        );
        assert!(decoder_has_room(1, 0));
        assert!(!decoder_has_room(1, 1));
        assert!(!decoder_has_room(2, 0));
        assert!(!decoder_has_room(usize::MAX, 1));
    }

    #[test]
    fn incomplete_or_poisoned_histories_cannot_prepare_publication() {
        assert!(require_complete_settlement_read(false, true, true).is_ok());
        for (poisoned, source, destination) in [
            (false, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            assert!(require_complete_settlement_read(poisoned, source, destination).is_err());
        }
    }

    #[test]
    fn claimed_discovery_start_rotates_across_short_gc_runs() {
        let counter = std::sync::atomic::AtomicUsize::new(0);
        assert_eq!(claimed_discovery_start(&counter, 0), 0);
        assert_eq!(claimed_discovery_start(&counter, 2), 0);
        assert_eq!(claimed_discovery_start(&counter, 2), 1);
        assert_eq!(claimed_discovery_start(&counter, 2), 0);
    }
}
