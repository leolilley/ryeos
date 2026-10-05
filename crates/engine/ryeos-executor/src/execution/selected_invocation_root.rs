//! Empty composition groundwork. This module cannot install or launch an owner.
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};

const CAPACITY: usize = 32;

// Concrete upper/lower owners, not a witness DTO. No constructor exists: real
// lower issuance, all upper owners/fences and nonempty close remain unreviewed.
// In particular this type is NOT an element of the public empty bootstrap.
#[cfg(target_os = "linux")]
struct SelectedWholeOwner {
    invocation: super::runner::SelectedQualificationInvocation,
    original_bounds: lillux::time::OriginalLocalDeadlineHandoff,
    provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    lower: lillux::exec::SelectedScopedStorage,
    preparation: SelectedPreparationOwners,
}

#[cfg(target_os = "linux")]
struct SelectedPreparationOwners {
    finalized: Option<super::runner::FinalizedDirectAdmission>,
    compiled: Option<ryeos_engine::authority::isolation::AppliedIsolationLaunchAwaitingAttachment>,
    attached: Option<super::process_attachment::AttachedProcessGuard>,
}

#[derive(Debug, PartialEq, Eq)]
enum RootPhase<B> {
    Pending,
    Occupied,
    BorrowReserved(u64),
    Borrowed(B),
    RestoredAwaitingBorrowAck(B),
    Retiring,
    RestorationConflict,
}

#[derive(Debug, PartialEq, Eq)]
enum RootLoanError { WrongIdentity, WrongState, Closed, Refused, Poisoned, Exhausted }

// Private generic mechanics serve destructor probes only; production is closed
// to the concrete SelectedWholeOwner + actual opaque App ticket types below.
// No production occupied-slot factory, extraction or generic callback exists.
struct SlotCore<O, T, B> {
    owner: Option<Box<O>>,
    ticket: T,
    phase: RootPhase<B>,
    last_returned: Option<B>,
    closed: bool,
    refused: bool,
    next_reservation: u64,
    restoration_conflict: Option<LoanFrame<O, B>>,
}

struct LoanFrame<O, B> { owner: Box<O>, borrower: B }

struct RootLoan<'root, O, T, B> {
    destination: &'root std::sync::Mutex<SlotCore<O, T, B>>,
    frame: Option<LoanFrame<O, B>>,
    invariant: std::marker::PhantomData<fn(&'root ()) -> &'root ()>,
}

impl<O, T: Eq, B: Eq + Clone> SlotCore<O, T, B> {
    fn reserve_loan(
        destination: &std::sync::Mutex<Self>, ticket: &T,
    ) -> Result<u64, RootLoanError> {
        let mut slot = match destination.lock() {
            Ok(slot) => slot,
            Err(poison) => {
                poison.into_inner().refused = true;
                return Err(RootLoanError::Poisoned);
            }
        };
        if &slot.ticket != ticket { return Err(RootLoanError::WrongIdentity); }
        if slot.closed { return Err(RootLoanError::Closed); }
        if slot.refused { return Err(RootLoanError::Refused); }
        if slot.phase != RootPhase::Occupied || slot.owner.is_none() {
            return Err(RootLoanError::WrongState);
        }
        let reservation = slot.next_reservation;
        let next = reservation.checked_add(1).ok_or(RootLoanError::Exhausted)?;
        slot.next_reservation = next;
        slot.phase = RootPhase::BorrowReserved(reservation);
        Ok(reservation)
    }

    fn finish_reserved_loan<'root>(
        destination: &'root std::sync::Mutex<Self>, ticket: &T, reservation: u64, borrower: B,
    ) -> Result<RootLoan<'root, O, T, B>, RootLoanError> {
        // Production B is an opaque Arc-backed ticket: clone never allocates.
        // Generic instantiations are private mechanical fixtures only.
        let recorded_borrower = borrower.clone();
        let mut slot = match destination.lock() {
            Ok(slot) => slot,
            Err(poison) => {
                let mut slot = poison.into_inner();
                slot.refused = true;
                if &slot.ticket == ticket && slot.phase == RootPhase::BorrowReserved(reservation) {
                    slot.phase = RootPhase::RestoredAwaitingBorrowAck(recorded_borrower);
                }
                return Err(RootLoanError::Poisoned);
            }
        };
        if &slot.ticket != ticket { return Err(RootLoanError::WrongIdentity); }
        if slot.phase != RootPhase::BorrowReserved(reservation) || slot.owner.is_none() {
            return Err(RootLoanError::WrongState);
        }
        if slot.closed || slot.refused {
            // App has issued this borrower but no physical loan started. The
            // Box remains rooted and the exact App return obligation retained.
            slot.phase = RootPhase::RestoredAwaitingBorrowAck(recorded_borrower);
            return Err(if slot.closed { RootLoanError::Closed } else { RootLoanError::Refused });
        }
        slot.phase = RootPhase::Borrowed(recorded_borrower);
        slot.last_returned = None;
        let frame = slot.owner.take().map(|owner| LoanFrame { owner, borrower });
        drop(slot);
        Ok(RootLoan { destination, frame, invariant: std::marker::PhantomData })
    }

    #[cfg(test)]
    fn begin_loan<'root>(
        destination: &'root std::sync::Mutex<Self>, ticket: &T, borrower: B,
    ) -> Result<RootLoan<'root, O, T, B>, RootLoanError> {
        let reservation = Self::reserve_loan(destination, ticket)?;
        Self::finish_reserved_loan(destination, ticket, reservation, borrower)
    }

    fn validate_return(&self, ticket: &T, borrower: &B) -> Result<(), RootLoanError> {
        if &self.ticket != ticket { return Err(RootLoanError::WrongIdentity); }
        if self.owner.is_none() { return Err(RootLoanError::WrongState); }
        if self.phase == RootPhase::RestoredAwaitingBorrowAck(borrower.clone())
            || (self.phase == RootPhase::Occupied && self.last_returned.as_ref() == Some(borrower))
        { return Ok(()); }
        Err(RootLoanError::WrongState)
    }

    // Called only after actual App ACK by the concrete adapter, or mechanical
    // unit controls. It has no public positive-receipt constructor.
    fn complete_return(&mut self, ticket: &T, borrower: &B) -> Result<(), RootLoanError> {
        self.validate_return(ticket, borrower)?;
        let returned = borrower.clone();
        self.phase = RootPhase::Occupied;
        self.last_returned = Some(returned);
        Ok(())
    }
}

impl<O, T, B> Drop for RootLoan<'_, O, T, B> {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            let mut slot = match self.destination.lock() {
                Ok(slot) => slot,
                Err(poison) => {
                    let mut slot = poison.into_inner();
                    slot.refused = true;
                    slot
                }
            };
            // Only begin_loan/this Drop can move owner; while Borrowed no other
            // writer can install or remove it. Thus this reserved destination
            // is None. Restoration performs only moves, before any child Drop,
            // allocation, assertion, formatting, native/token/store call.
            if slot.owner.is_some() {
                // Preallocated quarantine retains both owners on an impossible
                // destination conflict. Unique outstanding loan + no quarantine
                // remover means this cell cannot already hold another frame.
                slot.refused = true;
                slot.restoration_conflict = Some(frame);
                slot.phase = RootPhase::RestorationConflict;
            } else {
                slot.owner = Some(frame.owner);
                slot.phase = RootPhase::RestoredAwaitingBorrowAck(frame.borrower);
            }
        }
        // No ACK in Drop: failure/panic cannot interrupt physical restitution.
    }
}

#[cfg(target_os = "linux")]
struct SelectedRetainedSlot {
    slot: std::sync::Mutex<SlotCore<SelectedWholeOwner, ryeos_app::SelectedTicket, ryeos_app::SelectedBorrowTicket>>,
}

#[cfg(target_os = "linux")]
struct SelectedRetainedRoot {
    // The node stays in its fixed slot even while its WHOLE Box is borrowed.
    // No constructor/removal/close for this nonempty storage is exposed.
    slots: [Option<SelectedRetainedSlot>; CAPACITY],
}

#[cfg(target_os = "linux")]
impl SelectedRetainedSlot {
    // No caller/constructor installs this type. App borrow tickets name only
    // metadata; obtaining one does not mint a lower native loan.
    fn begin_loan(
        &self, store: &ryeos_app::state_store::StateStore, ticket: &ryeos_app::SelectedTicket,
    ) -> anyhow::Result<RootLoan<'_, SelectedWholeOwner, ryeos_app::SelectedTicket, ryeos_app::SelectedBorrowTicket>> {
        {
            let slot = match self.slot.lock() {
                Ok(slot) => slot,
                Err(poison) => {
                    poison.into_inner().refused = true;
                    anyhow::bail!("selected root identity check poisoned; whole owner retained");
                }
            };
            let owner = slot.owner.as_ref().ok_or_else(|| anyhow::anyhow!("selected whole owner is borrowed"))?;
            let (thread, claim, guarded_owner) = owner.invocation.claim_identity();
            anyhow::ensure!(ticket.matches_claim(thread, claim, guarded_owner), "selected actual claim differs from root ticket");
        }
        let reservation = SlotCore::reserve_loan(&self.slot, ticket)
            .map_err(|error| anyhow::anyhow!("selected root reservation refused: {error:?}"))?;
        // Root transition is already close-counted; no root lock overlaps App.
        let borrower = match store.begin_selected_metadata_borrow(ticket) {
            Ok(borrower) => borrower,
            Err(error) => {
                let mut slot = self.slot.lock().unwrap_or_else(|poison| poison.into_inner());
                slot.refused = true;
                // No rollback based on an assumed failed App mutation. Keep
                // BorrowReserved and the original whole Box for reconciliation.
                return Err(error);
            }
        };
        SlotCore::finish_reserved_loan(&self.slot, ticket, reservation, borrower)
            .map_err(|error| anyhow::anyhow!("selected root/App loan join retained: {error:?}"))
    }

    fn acknowledge_pending_return(
        &self, store: &ryeos_app::state_store::StateStore,
    ) -> anyhow::Result<ryeos_app::SelectedBorrowReturn> {
        acknowledge_pending_return(&self.slot, store)
    }

}

// Private shared adapter: production uses the concrete whole owner; unit
// controls use destructor probes with genuine App metadata/claim fixtures.
// No native factory, generic cleanup callback or external borrower is needed.
fn acknowledge_pending_return<O>(
    destination: &std::sync::Mutex<SlotCore<O, ryeos_app::SelectedTicket, ryeos_app::SelectedBorrowTicket>>,
    store: &ryeos_app::state_store::StateStore,
) -> anyhow::Result<ryeos_app::SelectedBorrowReturn> {
    let (borrower, original_owner) = {
        let slot = match destination.lock() {
            Ok(slot) => slot,
            Err(poison) => {
                let mut slot = poison.into_inner();
                slot.refused = true;
                slot
            }
        };
        anyhow::ensure!(slot.restoration_conflict.is_none(), "selected restoration conflict retains both owners");
        let borrower = match &slot.phase {
            RootPhase::RestoredAwaitingBorrowAck(borrower) => borrower.clone(),
            RootPhase::Occupied => slot.last_returned.clone()
                .ok_or_else(|| anyhow::anyhow!("selected root has no pending/replayed return"))?,
            _ => anyhow::bail!("selected root is not physically restored"),
        };
        slot.validate_return(borrower.invocation(), &borrower)
            .map_err(|error| anyhow::anyhow!("selected retained return identity refused: {error:?}"))?;
        let owner = slot.owner.as_deref()
            .ok_or_else(|| anyhow::anyhow!("selected root whole owner is absent"))?;
        (borrower, std::ptr::from_ref(owner))
    };
    // Exact ticket comes from the retained phase, including begin_loan Err
    // after App borrower issuance. No root lock overlaps either App call.
    store.mark_selected_metadata_restored(&borrower)?;
    let returned = store.ack_selected_borrow_return(&borrower)?;
    let mut slot = match destination.lock() {
        Ok(slot) => slot,
        Err(poison) => {
            let mut slot = poison.into_inner();
            slot.refused = true;
            slot
        }
    };
    anyhow::ensure!(slot.restoration_conflict.is_none(), "selected conflict remains retained after App ACK");
    anyhow::ensure!(
        slot.owner.as_deref().map(std::ptr::from_ref) == Some(original_owner),
        "selected whole owner changed during return acknowledgment"
    );
    slot.complete_return(borrower.invocation(), &borrower)
        .map_err(|error| anyhow::anyhow!("selected root return acknowledgment pending: {error:?}"))?;
    // Poison is deliberately NOT cleared and sticky refusal is unchanged.
    // Only exact App/root return metadata reconciles, never native/empty proof.
    Ok(returned)
}

// Uninhabited until a separately reviewed owner implementation replaces this
// empty storage cut. This is not a fake native owner or settlement witness.
enum UnavailableOwner {}

struct EmptyRoot {
    slots: [Option<UnavailableOwner>; CAPACITY],
    closed: AtomicBool,
}

pub struct SelectedInvocationCompositionBarrier {
    root: Arc<EmptyRoot>,
    registered_generation: Option<(Arc<ryeos_app::state_store::StateStore>, ryeos_app::SelectedGeneration)>,
}

#[derive(Clone)]
/// Refusal observation only; no owned root/native extraction or loan issuer.
/// ```compile_fail
/// let (barrier, attachment) = ryeos_executor::SelectedInvocationCompositionBarrier::bootstrap();
/// let owned = attachment.into_inner();
/// ```
/// ```compile_fail
/// let (barrier, _) = ryeos_executor::SelectedInvocationCompositionBarrier::bootstrap();
/// let loan = barrier.borrow_native();
/// ```
/// Lower lexical invariance and borrowed-owner escape, no native fixture mint:
/// ```compile_fail
/// fn shorten<'long: 'short, 'short>(
///     loan: lillux::exec::SelectedScopedBorrow<'long>,
/// ) -> lillux::exec::SelectedScopedBorrow<'short> { loan }
/// ```
/// ```compile_fail
/// fn escape(loan: &lillux::exec::SelectedScopedBorrow<'_>)
///     -> Option<lillux::exec::SelectedScopedHeldView<'static>> { loan.held() }
/// ```
pub struct SelectedInvocationAttachment {
    root: Weak<EmptyRoot>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SelectedInstallationRefusal {
    Unarmed,
    Closed,
    Detached,
}

impl SelectedInvocationCompositionBarrier {
    pub fn bootstrap() -> (Self, SelectedInvocationAttachment) {
        let root = Arc::new(EmptyRoot {
            slots: std::array::from_fn(|_| None),
            closed: AtomicBool::new(false),
        });
        let attachment = SelectedInvocationAttachment { root: Arc::downgrade(&root) };
        (Self { root, registered_generation: None }, attachment)
    }

    /// Bind only the actual shared AppState generation; this never arms contact.
    pub fn arm_from_app_generation(&mut self, app: &ryeos_app::state::AppState) -> anyhow::Result<()> {
        self.register_from_store(&app.state_store)
    }

    fn register_from_store(&mut self, store: &Arc<ryeos_app::state_store::StateStore>) -> anyhow::Result<()> {
        if self.root.closed.load(Ordering::Acquire) {
            anyhow::bail!("empty selected composition is closed");
        }
        if self.registered_generation.is_some() {
            anyhow::bail!("selected composition generation is already registered");
        }
        let generation = store.register_selected_bookkeeping_generation()?;
        self.registered_generation = Some((Arc::clone(store), generation));
        Ok(())
    }

    /// Closes only the structurally empty bootstrap. No native settlement claim.
    pub fn close_empty_bootstrap(&self) {
        // The sole constructor fills every slot with None; no owner issuer,
        // storage mutation, extraction or loan exists in this implementation.
        for slot in &self.root.slots {
            if let Some(owner) = slot { match *owner {} }
        }
        self.root.closed.store(true, Ordering::Release);
    }
}

impl Drop for SelectedInvocationCompositionBarrier {
    fn drop(&mut self) { self.close_empty_bootstrap(); }
}

impl SelectedInvocationAttachment {
    pub fn installation_refusal(&self) -> SelectedInstallationRefusal {
        match self.root.upgrade() {
            None => SelectedInstallationRefusal::Detached,
            Some(root) if root.closed.load(Ordering::Acquire) => SelectedInstallationRefusal::Closed,
            Some(_) => SelectedInstallationRefusal::Unarmed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DropProbe(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for DropProbe {
        fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    fn mechanical_slot() -> (std::sync::Mutex<SlotCore<DropProbe, u64, u64>>, Arc<std::sync::atomic::AtomicUsize>) {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (std::sync::Mutex::new(SlotCore {
            owner: Some(Box::new(DropProbe(Arc::clone(&drops)))), ticket: 7,
            phase: RootPhase::Occupied, last_returned: None, closed: false, refused: false,
            next_reservation: 1, restoration_conflict: None,
        }), drops)
    }

    #[test]
    fn same_whole_box_restores_on_error_and_unwind_before_child_drop() {
        let (slot, drops) = mechanical_slot();
        let original = slot.lock().unwrap().owner.as_deref().unwrap() as *const DropProbe;
        let error = (|| -> Result<(), ()> {
            let loan = SlotCore::begin_loan(&slot, &7, 11).unwrap();
            assert_eq!(loan.frame.as_ref().unwrap().owner.as_ref() as *const DropProbe, original);
            assert!(slot.lock().unwrap().owner.is_none());
            Err(())
        })();
        assert!(error.is_err());
        {
            let mut state = slot.lock().unwrap();
            assert_eq!(state.owner.as_deref().unwrap() as *const DropProbe, original);
            assert_eq!(state.phase, RootPhase::RestoredAwaitingBorrowAck(11));
            state.complete_return(&7, &11).unwrap();
        }
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _loan = SlotCore::begin_loan(&slot, &7, 12).unwrap();
            panic!("mechanical unwind");
        }));
        assert!(panic.is_err());
        assert_eq!(slot.lock().unwrap().owner.as_deref().unwrap() as *const DropProbe, original);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(slot); // Fixture only: no native/upper owner or settlement receipt.
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_ack_and_stale_replay_cannot_start_another_loan() {
        let (slot, drops) = mechanical_slot();
        drop(SlotCore::begin_loan(&slot, &7, 11).unwrap());
        // Model ACK failure by leaving the physically restored phase untouched.
        assert!(SlotCore::begin_loan(&slot, &7, 12).is_err());
        {
            let mut state = slot.lock().unwrap();
            assert!(state.complete_return(&8, &11).is_err());
            assert!(state.complete_return(&7, &12).is_err());
            assert_eq!(state.phase, RootPhase::RestoredAwaitingBorrowAck(11));
            state.complete_return(&7, &11).unwrap();
            state.complete_return(&7, &11).unwrap();
        }
        let loan = SlotCore::begin_loan(&slot, &7, 12).unwrap();
        assert!(slot.lock().unwrap().complete_return(&7, &11).is_err());
        drop(loan);
        assert!(slot.lock().unwrap().complete_return(&7, &11).is_err());
        slot.lock().unwrap().complete_return(&7, &12).unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn outstanding_frame_remains_occupied_on_close_and_poison_restores_refused() {
        let (slot, drops) = mechanical_slot();
        let original = slot.lock().unwrap().owner.as_deref().unwrap() as *const DropProbe;
        let loan = SlotCore::begin_loan(&slot, &7, 11).unwrap();
        slot.lock().unwrap().closed = true;
        assert_eq!(slot.lock().unwrap().phase, RootPhase::Borrowed(11));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = slot.lock().unwrap();
            panic!("mechanical poison");
        }));
        assert!(panic.is_err());
        drop(loan);
        {
            let state = match slot.lock() {
                Err(poison) => poison.into_inner(),
                Ok(_) => panic!("fixture must retain poison"),
            };
            assert!(state.refused && state.closed);
            assert_eq!(state.phase, RootPhase::RestoredAwaitingBorrowAck(11));
            assert_eq!(state.owner.as_deref().unwrap() as *const DropProbe, original);
        }
        assert!(SlotCore::begin_loan(&slot, &7, 12).is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn wrong_ticket_pending_retiring_and_closed_leave_box_intact() {
        let (slot, drops) = mechanical_slot();
        let original = slot.lock().unwrap().owner.as_deref().unwrap() as *const DropProbe;
        assert!(SlotCore::begin_loan(&slot, &8, 11).is_err());
        slot.lock().unwrap().phase = RootPhase::Pending;
        assert!(SlotCore::begin_loan(&slot, &7, 11).is_err());
        slot.lock().unwrap().phase = RootPhase::Retiring;
        assert!(SlotCore::begin_loan(&slot, &7, 11).is_err());
        slot.lock().unwrap().phase = RootPhase::Occupied;
        slot.lock().unwrap().closed = true;
        assert!(SlotCore::begin_loan(&slot, &7, 11).is_err());
        assert_eq!(slot.lock().unwrap().owner.as_deref().unwrap() as *const DropProbe, original);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn two_phase_close_and_wrong_reservation_keep_original_owner_rooted() {
        let (slot, drops) = mechanical_slot();
        let reservation = SlotCore::reserve_loan(&slot, &7).unwrap();
        assert_eq!(slot.lock().unwrap().phase, RootPhase::BorrowReserved(reservation));
        // This gap models an App transition with no root lock held.
        assert!(SlotCore::finish_reserved_loan(&slot, &7, reservation + 1, 11).is_err());
        assert!(slot.lock().unwrap().owner.is_some());
        assert_eq!(slot.lock().unwrap().phase, RootPhase::BorrowReserved(reservation));
        slot.lock().unwrap().closed = true;
        assert!(SlotCore::finish_reserved_loan(&slot, &7, reservation, 11).is_err());
        assert_eq!(slot.lock().unwrap().phase, RootPhase::RestoredAwaitingBorrowAck(11));
        assert!(slot.lock().unwrap().owner.is_some());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn conflicting_destination_quarantines_whole_frame_without_replacing_owner() {
        let (slot, drops) = mechanical_slot();
        let original = slot.lock().unwrap().owner.as_deref().unwrap() as *const DropProbe;
        let loan = SlotCore::begin_loan(&slot, &7, 11).unwrap();
        // Private mechanical fault injection; no production owner setter exists.
        slot.lock().unwrap().owner = Some(Box::new(DropProbe(Arc::clone(&drops))));
        drop(loan);
        let state = slot.lock().unwrap();
        assert!(state.refused);
        assert_eq!(state.phase, RootPhase::RestorationConflict);
        assert_eq!(state.restoration_conflict.as_ref().unwrap().owner.as_ref() as *const DropProbe, original);
        assert!(state.owner.is_some());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    fn actual_store(root: &std::path::Path) -> Arc<ryeos_app::state_store::StateStore> {
        let identity = ryeos_app::identity::NodeIdentity::create(&root.join("node.pem")).unwrap();
        let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(&identity));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let state = root.join(".ai/state");
        Arc::new(ryeos_app::state_store::StateStore::new_with_head_trust(
            root.to_owned(), state.clone(), state.join("runtime.sqlite3"), signer,
            ryeos_app::write_barrier::WriteBarrier::new(), Arc::new(trust),
        ).unwrap())
    }

    type MetadataProbeSlot = std::sync::Mutex<SlotCore<DropProbe, ryeos_app::SelectedTicket, ryeos_app::SelectedBorrowTicket>>;

    fn actual_metadata_probe() -> (tempfile::TempDir, Arc<ryeos_app::state_store::StateStore>, MetadataProbeSlot, Arc<std::sync::atomic::AtomicUsize>) {
        let root = tempfile::tempdir().unwrap();
        let store = actual_store(root.path());
        store.reserve_fresh_thread_launch_active(
            "T-adapter", "fixture-claim", ryeos_app::runtime_db::daemon_generation_id(),
        ).unwrap().unwrap();
        // App metadata only; no born capsule, lower owner or native issuer.
        let borrower = store.fixture_selected_restored_metadata("T-adapter").unwrap();
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slot = std::sync::Mutex::new(SlotCore {
            owner: Some(Box::new(DropProbe(Arc::clone(&drops)))),
            ticket: borrower.invocation().clone(),
            phase: RootPhase::RestoredAwaitingBorrowAck(borrower), last_returned: None,
            closed: false, refused: false, next_reservation: 1, restoration_conflict: None,
        });
        (root, store, slot, drops)
    }

    #[test]
    fn actual_adapter_retries_retained_ticket_after_wrong_store_and_exact_ack() {
        let (_root, store, slot, drops) = actual_metadata_probe();
        let pointer = slot.lock().unwrap().owner.as_deref().unwrap() as *const DropProbe;
        let other_root = tempfile::tempdir().unwrap();
        let other_store = actual_store(other_root.path());
        assert!(acknowledge_pending_return(&slot, &other_store).is_err());
        assert!(matches!(slot.lock().unwrap().phase, RootPhase::RestoredAwaitingBorrowAck(_)));
        assert_eq!(acknowledge_pending_return(&slot, &store).unwrap(), ryeos_app::SelectedBorrowReturn::Returned);
        assert_eq!(acknowledge_pending_return(&slot, &store).unwrap(), ryeos_app::SelectedBorrowReturn::AlreadyReturned);
        assert_eq!(slot.lock().unwrap().owner.as_deref().unwrap() as *const DropProbe, pointer);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn actual_adapter_missing_durable_claim_retains_box_and_pending_borrower() {
        let (_root, store, slot, drops) = actual_metadata_probe();
        let before = match &slot.lock().unwrap().phase {
            RootPhase::RestoredAwaitingBorrowAck(borrower) => borrower.clone(),
            _ => panic!("fixture pending return"),
        };
        store.release_thread_launch_claim("T-adapter", "fixture-claim").unwrap();
        assert!(acknowledge_pending_return(&slot, &store).is_err());
        assert_eq!(slot.lock().unwrap().phase, RootPhase::RestoredAwaitingBorrowAck(before));
        assert!(slot.lock().unwrap().owner.is_some());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn actual_adapter_reconciles_post_ack_poison_without_clearing_refusal() {
        let (_root, store, slot, drops) = actual_metadata_probe();
        let borrower = match &slot.lock().unwrap().phase {
            RootPhase::RestoredAwaitingBorrowAck(borrower) => borrower.clone(),
            _ => panic!("fixture pending return"),
        };
        store.mark_selected_metadata_restored(&borrower).unwrap();
        store.ack_selected_borrow_return(&borrower).unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = slot.lock().unwrap();
            panic!("post-App-ACK poison");
        }));
        assert!(panic.is_err());
        assert_eq!(acknowledge_pending_return(&slot, &store).unwrap(), ryeos_app::SelectedBorrowReturn::AlreadyReturned);
        assert_eq!(acknowledge_pending_return(&slot, &store).unwrap(), ryeos_app::SelectedBorrowReturn::AlreadyReturned);
        assert!(slot.is_poisoned());
        let state = slot.lock().unwrap_or_else(|poison| poison.into_inner());
        assert!(state.refused);
        assert_eq!(state.phase, RootPhase::Occupied);
        assert_eq!(state.last_returned.as_ref(), Some(&borrower));
        assert!(state.owner.is_some());
        drop(state);
        assert!(SlotCore::reserve_loan(&slot, borrower.invocation()).is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn actual_post_app_borrow_error_retries_from_slot_and_stale_return_refuses() {
        let (_root, store, slot, drops) = actual_metadata_probe();
        acknowledge_pending_return(&slot, &store).unwrap();
        let (ticket, first) = {
            let state = slot.lock().unwrap();
            (state.ticket.clone(), state.last_returned.clone().unwrap())
        };
        let reservation = SlotCore::reserve_loan(&slot, &ticket).unwrap();
        let second = store.begin_selected_metadata_borrow(&ticket).unwrap();
        assert!(slot.lock().unwrap().complete_return(&ticket, &first).is_err());
        slot.lock().unwrap().closed = true;
        // Consumes the only external second borrower; the adapter must recover
        // its exact identity from the retained phase after this error.
        assert!(SlotCore::finish_reserved_loan(&slot, &ticket, reservation, second).is_err());
        assert_eq!(acknowledge_pending_return(&slot, &store).unwrap(), ryeos_app::SelectedBorrowReturn::Returned);
        assert_eq!(acknowledge_pending_return(&slot, &store).unwrap(), ryeos_app::SelectedBorrowReturn::AlreadyReturned);
        assert!(store.ack_selected_borrow_return(&first).is_err());
        assert!(slot.lock().unwrap().complete_return(&ticket, &first).is_err());
        assert!(slot.lock().unwrap().closed);
        assert!(slot.lock().unwrap().owner.is_some());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn actual_shared_generation_registration_is_one_shot_and_never_contact() {
        let root = tempfile::tempdir().unwrap();
        let store = actual_store(root.path());
        let (mut barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        barrier.register_from_store(&store).unwrap();
        assert!(Arc::ptr_eq(&barrier.registered_generation.as_ref().unwrap().0, &store));
        assert!(barrier.register_from_store(&Arc::clone(&store)).is_err());
        let other_root = tempfile::tempdir().unwrap();
        let other_store = actual_store(other_root.path());
        assert!(barrier.register_from_store(&other_store).is_err());
        assert!(Arc::ptr_eq(&barrier.registered_generation.as_ref().unwrap().0, &store));
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Unarmed);
        barrier.close_empty_bootstrap();
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Closed);
    }

    #[test]
    fn closed_root_or_store_cannot_publish_generation_association() {
        let root = tempfile::tempdir().unwrap();
        let store = actual_store(root.path());
        let (mut barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        store.close_process_attachment_admission().unwrap();
        assert!(barrier.register_from_store(&store).is_err());
        assert!(barrier.registered_generation.is_none());
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Unarmed);
        barrier.close_empty_bootstrap();
        assert!(barrier.register_from_store(&store).is_err());
        assert!(barrier.registered_generation.is_none());
    }

    #[test]
    fn bootstrap_refuses_and_empty_close_is_idempotent() {
        let (barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        assert_eq!(barrier.root.slots.len(), CAPACITY);
        assert!(barrier.root.slots.iter().all(Option::is_none));
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Unarmed);
        barrier.close_empty_bootstrap();
        barrier.close_empty_bootstrap();
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Closed);
        drop(barrier);
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Detached);
    }

    #[test]
    fn weak_attachment_does_not_retain_barrier_after_unwind() {
        let (barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        let observer = attachment.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _barrier = barrier;
            panic!("fixture unwind");
        }));
        assert!(result.is_err());
        assert_eq!(observer.installation_refusal(), SelectedInstallationRefusal::Detached);
    }
}
