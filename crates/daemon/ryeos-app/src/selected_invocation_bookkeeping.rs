//! Inert, process-local metadata. No native installation or settlement authority.
use std::sync::Arc;

const CAPACITY: usize = 32;

#[derive(Debug)]
struct StoreIdentity;

#[derive(Clone)]
pub struct SelectedGeneration {
    store: Arc<StoreIdentity>,
    generation: String,
}

#[derive(Clone)]
/// Opaque exact invocation identity; no production pending issuer exists.
/// ```compile_fail
/// let ticket = ryeos_app::SelectedTicket::default();
/// ```
pub struct SelectedTicket {
    registration: SelectedGeneration,
    slot: usize,
    nonce: u64,
    thread: String,
    claim: String,
    owner: String,
}

#[derive(Clone)]
/// Exact borrower metadata, not native loan or settlement authority.
/// ```compile_fail
/// let borrower = ryeos_app::SelectedBorrowTicket::default();
/// ```
pub struct SelectedBorrowTicket {
    // Cloning the exact return ticket after App mutation is an Arc increment,
    // not a new fallible allocation of strings or a new borrower identity.
    ticket: Arc<SelectedTicket>,
    borrower: u64,
}

impl std::fmt::Debug for SelectedGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SelectedGeneration { .. }")
    }
}
impl std::fmt::Debug for SelectedTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SelectedTicket { .. }")
    }
}
impl std::fmt::Debug for SelectedBorrowTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SelectedBorrowTicket { .. }")
    }
}

impl SelectedBorrowTicket {
    pub fn invocation(&self) -> &SelectedTicket { &self.ticket }
    pub(crate) fn claim_identity(&self) -> (&str, &str, &str) {
        (&self.ticket.thread, &self.ticket.claim, &self.ticket.owner)
    }
}

impl PartialEq for SelectedTicket {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.registration.store, &other.registration.store)
            && self.registration.generation == other.registration.generation
            && self.slot == other.slot && self.nonce == other.nonce
            && self.thread == other.thread && self.claim == other.claim && self.owner == other.owner
    }
}
impl Eq for SelectedTicket {}
impl PartialEq for SelectedBorrowTicket {
    fn eq(&self, other: &Self) -> bool {
        self.ticket == other.ticket && self.borrower == other.borrower
    }
}
impl Eq for SelectedBorrowTicket {}

impl SelectedTicket {
    pub fn matches_claim(&self, thread: &str, claim: &str, owner: &str) -> bool {
        self.thread == thread && self.claim == claim && self.owner == owner
    }
    pub(crate) fn claim_identity(&self) -> (&str, &str, &str) {
        (&self.thread, &self.claim, &self.owner)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Pending,
    Occupied,
    Borrowed(u64),
    RestoredAwaitingBorrowAck(u64),
    Retiring,
}

#[derive(Debug)]
struct Record {
    ticket: SelectedTicket,
    phase: Phase,
    refused: bool,
    last_returned: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectedBookkeepingError {
    Closed,
    Capacity,
    WrongIdentity,
    WrongState,
    Refused,
    Exhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectedBorrowReturn {
    Returned,
    AlreadyReturned,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum SelectedFixturePhase { Pending, Occupied, Borrowed, RestoredAwaitingBorrowAck, Retiring }

pub(crate) struct SelectedInvocationBookkeeping {
    identity: Arc<StoreIdentity>,
    generation: Option<String>,
    slots: [Option<Record>; CAPACITY],
    next_nonce: u64,
    next_borrower: u64,
    closed: bool,
}

impl SelectedInvocationBookkeeping {
    pub(crate) fn new() -> Self {
        Self {
            identity: Arc::new(StoreIdentity),
            generation: None,
            slots: std::array::from_fn(|_| None),
            next_nonce: 1,
            next_borrower: 1,
            closed: false,
        }
    }

    /// Every retained phase, including refused records, remains occupied.
    pub(crate) fn has_thread(&self, thread_id: &str) -> bool {
        self.slots.iter().flatten().any(|record| record.ticket.thread == thread_id)
    }

    pub(crate) fn has_any(&self) -> bool {
        self.slots.iter().any(Option::is_some)
    }

    // Registration names only this metadata domain, never an installed owner.
    pub(crate) fn register_generation(
        &mut self,
        generation: &str,
    ) -> Result<SelectedGeneration, SelectedBookkeepingError> {
        if self.closed { return Err(SelectedBookkeepingError::Closed); }
        if generation.is_empty()
            || self.generation.as_deref().is_some_and(|g| g != generation)
        {
            return Err(SelectedBookkeepingError::WrongIdentity);
        }
        let registration = SelectedGeneration {
            store: Arc::clone(&self.identity),
            generation: generation.to_owned(),
        };
        self.generation = Some(generation.to_owned());
        Ok(registration)
    }

    fn validate_generation(&self, generation: &SelectedGeneration) -> Result<(), SelectedBookkeepingError> {
        if !Arc::ptr_eq(&self.identity, &generation.store)
            || self.generation.as_deref() != Some(generation.generation.as_str())
        { return Err(SelectedBookkeepingError::WrongIdentity); }
        Ok(())
    }

    // Deliberately private: only transition controls can reserve/install today.
    fn pending(&mut self, generation: &SelectedGeneration, thread: &str, claim: &str, owner: &str)
        -> Result<SelectedTicket, SelectedBookkeepingError>
    {
        self.validate_generation(generation)?;
        if self.closed { return Err(SelectedBookkeepingError::Closed); }
        if thread.is_empty() || claim.is_empty() || owner.is_empty() {
            return Err(SelectedBookkeepingError::WrongIdentity);
        }
        if self.slots.iter().flatten().any(|r| r.ticket.thread == thread || r.ticket.owner == owner) {
            return Err(SelectedBookkeepingError::WrongIdentity);
        }
        let slot = self.slots.iter().position(Option::is_none).ok_or(SelectedBookkeepingError::Capacity)?;
        let next = self.next_nonce.checked_add(1).ok_or(SelectedBookkeepingError::Exhausted)?;
        let ticket = SelectedTicket {
            registration: generation.clone(), slot, nonce: self.next_nonce,
            thread: thread.to_owned(), claim: claim.to_owned(), owner: owner.to_owned(),
        };
        self.slots[slot] = Some(Record {
            ticket: ticket.clone(), phase: Phase::Pending, refused: false, last_returned: None,
        });
        self.next_nonce = next;
        Ok(ticket)
    }

    fn record(&mut self, ticket: &SelectedTicket) -> Result<&mut Record, SelectedBookkeepingError> {
        self.validate_generation(&ticket.registration)?;
        let record = self.slots.get_mut(ticket.slot).and_then(Option::as_mut)
            .ok_or(SelectedBookkeepingError::WrongIdentity)?;
        if record.ticket.nonce != ticket.nonce || record.ticket.thread != ticket.thread
            || record.ticket.claim != ticket.claim || record.ticket.owner != ticket.owner
        { return Err(SelectedBookkeepingError::WrongIdentity); }
        Ok(record)
    }

    pub(crate) fn install(&mut self, ticket: &SelectedTicket) -> Result<(), SelectedBookkeepingError> {
        if self.closed { return Err(SelectedBookkeepingError::Closed); }
        let r = self.record(ticket)?;
        if r.refused { return Err(SelectedBookkeepingError::Refused); }
        if r.phase != Phase::Pending { return Err(SelectedBookkeepingError::WrongState); }
        r.phase = Phase::Occupied;
        Ok(())
    }

    pub(crate) fn borrow(&mut self, ticket: &SelectedTicket) -> Result<SelectedBorrowTicket, SelectedBookkeepingError> {
        if self.closed { return Err(SelectedBookkeepingError::Closed); }
        let borrower = self.next_borrower;
        let next = borrower.checked_add(1).ok_or(SelectedBookkeepingError::Exhausted)?;
        let r = self.record(ticket)?;
        if r.refused { return Err(SelectedBookkeepingError::Refused); }
        if r.phase != Phase::Occupied { return Err(SelectedBookkeepingError::WrongState); }
        let loan = SelectedBorrowTicket { ticket: Arc::new(ticket.clone()), borrower };
        r.phase = Phase::Borrowed(borrower);
        r.last_returned = None;
        self.next_borrower = next;
        Ok(loan)
    }

    pub(crate) fn restored(&mut self, loan: &SelectedBorrowTicket) -> Result<(), SelectedBookkeepingError> {
        let r = self.record(&loan.ticket)?;
        if r.phase == Phase::RestoredAwaitingBorrowAck(loan.borrower)
            || (r.phase == Phase::Occupied && r.last_returned == Some(loan.borrower))
        { return Ok(()); }
        if r.phase != Phase::Borrowed(loan.borrower) { return Err(SelectedBookkeepingError::WrongState); }
        r.phase = Phase::RestoredAwaitingBorrowAck(loan.borrower);
        Ok(())
    }

    pub(crate) fn ack_selected_borrow_return(&mut self, loan: &SelectedBorrowTicket)
        -> Result<SelectedBorrowReturn, SelectedBookkeepingError>
    {
        let r = self.record(&loan.ticket)?;
        if r.phase == Phase::Occupied && r.last_returned == Some(loan.borrower) {
            return Ok(SelectedBorrowReturn::AlreadyReturned);
        }
        if r.phase != Phase::RestoredAwaitingBorrowAck(loan.borrower) {
            return Err(SelectedBookkeepingError::WrongState);
        }
        r.phase = Phase::Occupied;
        r.last_returned = Some(loan.borrower);
        Ok(SelectedBorrowReturn::Returned)
    }

    fn refuse(&mut self, ticket: &SelectedTicket) -> Result<(), SelectedBookkeepingError> {
        self.record(ticket)?.refused = true;
        Ok(())
    }

    fn retire(&mut self, ticket: &SelectedTicket) -> Result<(), SelectedBookkeepingError> {
        let r = self.record(ticket)?;
        if r.phase != Phase::Occupied { return Err(SelectedBookkeepingError::WrongState); }
        r.phase = Phase::Retiring;
        // No final removal: real native/finance/durable release adapters are absent.
        Ok(())
    }

    fn close_admission(&mut self) { self.closed = true; }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn fixture_restored_borrow(
        &mut self, generation: &SelectedGeneration, thread: &str, claim: &str, owner: &str,
    ) -> SelectedBorrowTicket {
        let ticket = self.pending(generation, thread, claim, owner).unwrap();
        self.install(&ticket).unwrap();
        let borrower = self.borrow(&ticket).unwrap();
        self.restored(&borrower).unwrap();
        borrower
    }

    #[cfg(test)]
    pub(crate) fn fixture_retained_phase(&mut self, ticket: &SelectedTicket, phase: SelectedFixturePhase, refused: bool) {
        let record = self.record(ticket).unwrap();
        record.phase = match phase {
            SelectedFixturePhase::Pending => Phase::Pending,
            SelectedFixturePhase::Occupied => Phase::Occupied,
            SelectedFixturePhase::Borrowed => Phase::Borrowed(1),
            SelectedFixturePhase::RestoredAwaitingBorrowAck => Phase::RestoredAwaitingBorrowAck(1),
            SelectedFixturePhase::Retiring => Phase::Retiring,
        };
        record.refused = refused;
    }

    #[cfg(test)]
    pub(crate) fn fixture_awaiting_ack(&mut self, borrower: &SelectedBorrowTicket) -> bool {
        self.record(&borrower.ticket).unwrap().phase
            == Phase::RestoredAwaitingBorrowAck(borrower.borrower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn occupied() -> (SelectedInvocationBookkeeping, SelectedTicket) {
        let mut b = SelectedInvocationBookkeeping::new();
        let g = b.register_generation("fixture-generation").unwrap();
        let t = b.pending(&g, "thread", "claim", "owner").unwrap();
        b.install(&t).unwrap();
        (b, t)
    }

    #[test]
    fn public_identity_debug_is_redacted() {
        let (mut b, ticket) = occupied();
        let borrower = b.borrow(&ticket).unwrap();
        assert_eq!(format!("{:?}", ticket.registration), "SelectedGeneration { .. }");
        assert_eq!(format!("{ticket:?}"), "SelectedTicket { .. }");
        assert_eq!(format!("{borrower:?}"), "SelectedBorrowTicket { .. }");
    }

    #[test]
    fn exact_return_ack_blocks_second_loan_and_old_replay() {
        let (mut b, t) = occupied();
        let first = b.borrow(&t).unwrap();
        assert_eq!(b.ack_selected_borrow_return(&first), Err(SelectedBookkeepingError::WrongState));
        b.restored(&first).unwrap();
        assert!(b.borrow(&t).is_err());
        let mut wrong = first.clone();
        wrong.borrower += 1;
        assert!(b.ack_selected_borrow_return(&wrong).is_err());
        assert_eq!(b.record(&t).unwrap().phase, Phase::RestoredAwaitingBorrowAck(first.borrower));
        assert_eq!(b.ack_selected_borrow_return(&first), Ok(SelectedBorrowReturn::Returned));
        assert_eq!(b.ack_selected_borrow_return(&first), Ok(SelectedBorrowReturn::AlreadyReturned));
        let second = b.borrow(&t).unwrap();
        assert!(b.ack_selected_borrow_return(&first).is_err());
        b.restored(&second).unwrap();
        assert!(b.ack_selected_borrow_return(&first).is_err());
        b.ack_selected_borrow_return(&second).unwrap();
    }

    #[test]
    fn identities_and_refusal_preserve_occupancy() {
        let (mut b, t) = occupied();
        let mut wrong = t.clone();
        wrong.nonce += 1;
        assert!(b.borrow(&wrong).is_err());
        wrong = t.clone();
        wrong.claim.push('x');
        assert!(b.retire(&wrong).is_err());
        wrong = t.clone();
        wrong.slot = CAPACITY;
        assert!(b.borrow(&wrong).is_err());
        wrong = t.clone();
        wrong.owner.push('x');
        assert!(b.install(&wrong).is_err());
        wrong = t.clone();
        wrong.registration.generation.push('x');
        assert!(b.refuse(&wrong).is_err());
        let mut other = SelectedInvocationBookkeeping::new();
        other.register_generation("fixture-generation").unwrap();
        assert!(other.borrow(&t).is_err());
        assert_eq!(b.record(&t).unwrap().phase, Phase::Occupied);
        let loan = b.borrow(&t).unwrap();
        b.refuse(&t).unwrap();
        b.restored(&loan).unwrap();
        b.ack_selected_borrow_return(&loan).unwrap();
        assert_eq!(b.borrow(&t).err(), Some(SelectedBookkeepingError::Refused));
        b.retire(&t).unwrap();
        assert_eq!(b.record(&t).unwrap().phase, Phase::Retiring);
        assert!(b.record(&t).unwrap().refused);
    }

    #[test]
    fn bounded_capacity_and_closed_registration_leave_records_intact() {
        let mut b = SelectedInvocationBookkeeping::new();
        let g = b.register_generation("g").unwrap();
        for n in 0..CAPACITY {
            b.pending(&g, &format!("t{n}"), "c", &format!("o{n}")).unwrap();
        }
        assert_eq!(b.pending(&g, "extra", "c", "extra").err(), Some(SelectedBookkeepingError::Capacity));
        assert_eq!(b.slots.iter().flatten().count(), CAPACITY);
        b.close_admission();
        assert_eq!(b.register_generation("g").err(), Some(SelectedBookkeepingError::Closed));
        assert_eq!(b.slots.iter().flatten().count(), CAPACITY);
    }

    #[test]
    fn closure_and_refusal_allow_only_exact_restoration_and_ack() {
        let (mut b, t) = occupied();
        let loan = b.borrow(&t).unwrap();
        assert!(b.retire(&t).is_err());
        b.close_admission();
        b.refuse(&t).unwrap();
        assert_eq!(b.borrow(&t).err(), Some(SelectedBookkeepingError::Closed));
        b.restored(&loan).unwrap();
        assert_eq!(b.ack_selected_borrow_return(&loan), Ok(SelectedBorrowReturn::Returned));
        assert!(b.record(&t).unwrap().refused);
        assert_eq!(b.borrow(&t).err(), Some(SelectedBookkeepingError::Closed));
    }
}
