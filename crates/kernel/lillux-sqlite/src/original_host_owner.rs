//! One resident SQLite graph. Ordinary Arc wrappers belong to the enclosing
//! bounded node process; they keep counted region handles, never own its unmap.
//! The original outside controller retains actual mapping/account custody.
pub mod backing;
pub mod region;
use std::{alloc::Layout, cell::UnsafeCell, marker::PhantomData, mem::ManuallyDrop,
    ptr::NonNull, sync::atomic::{AtomicU8, AtomicUsize, Ordering}};
use crate::native::{NativeResidentState, OriginalNativeBudget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedHostRefusal { Process, Layout, Provenance, Busy, Closed, Exhausted,
    Overflow, Purpose, Uninitialized, ReferencesRemain, TerminalRequired, Native(i32) }
impl std::fmt::Display for FixedHostRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "native region refusal: {self:?}") }
}
impl std::error::Error for FixedHostRefusal {}
const OPEN: u8 = 0;
const CLOSING: u8 = 1;
const SEALED: u8 = 2;
const UNLOCKED: u8 = 0;
const LOCKED: u8 = 1;
const COUNT_MAX: usize = usize::MAX / 8;
struct Counts { handles: usize, rust: usize, c: usize }
struct RegionHeader {
    gate: AtomicU8, phase: AtomicU8, process: u32, generation: u64,
    counts: UnsafeCell<Counts>,
    pending_handles: AtomicUsize, pending_rust: AtomicUsize, pending_c: AtomicUsize,
}
#[derive(Debug, Clone, Copy)]
pub struct OriginalNativeRegionLayout { pub layout: Layout, pub payload_offset: usize }
pub fn original_native_region_layout() -> Result<OriginalNativeRegionLayout, FixedHostRefusal> {
    let (layout, payload_offset) = Layout::new::<RegionHeader>().extend(Layout::new::<NativeResidentState>())
        .map_err(|_| FixedHostRefusal::Layout)?;
    Ok(OriginalNativeRegionLayout { layout: layout.pad_to_align(), payload_offset })
}
// `armed` is cleared only by `keep_locked`, which leaves the gate word locked
// for the life of the mapping; the gate owns nothing else.
struct Gate { header: NonNull<RegionHeader>, process: u32, armed: bool }
impl Gate {
    /// Leave the gate word locked for good: the region is sealed and its
    /// mapping may be released, so no destructor may touch it afterwards.
    fn keep_locked(mut self) { self.armed = false; }
}
impl Drop for Gate {
    fn drop(&mut self) {
        if !self.armed || self.process != std::process::id() { return; }
        // No foreign callback, allocation, destructor or fallible arithmetic
        // runs in this gate. Only complete primitive metadata transitions.
        unsafe { self.header.as_ref() }.gate.store(UNLOCKED, Ordering::Release);
    }
}
fn gate(header: NonNull<RegionHeader>, process: u32, generation: u64) -> Result<Gate, FixedHostRefusal> {
    if process != std::process::id() { return Err(FixedHostRefusal::Process); }
    let h = unsafe { header.as_ref() };
    if h.process != process || h.generation != generation { return Err(FixedHostRefusal::Provenance); }
    h.gate.compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed).map_err(|_| FixedHostRefusal::Busy)?;
    Ok(Gate { header, process, armed: true })
}
fn increment(value: usize) -> Result<usize, FixedHostRefusal> {
    value.checked_add(1).filter(|n| *n <= COUNT_MAX).ok_or(FixedHostRefusal::Overflow)
}
/// The sole physical-disposal controller token. Must live outside this mapping.
/// Drop performs no unmap, acknowledgement or settlement.
pub struct OriginalNativeRegionOwner {
    header: NonNull<RegionHeader>, payload: NonNull<NativeResidentState>, process: u32, generation: u64, bytes: usize,
}
unsafe impl Send for OriginalNativeRegionOwner {}
unsafe impl Sync for OriginalNativeRegionOwner {}
pub struct OriginalNativeRegionInitializationFailure {
    pub refusal: FixedHostRefusal, budget: ManuallyDrop<OriginalNativeBudget>, process: u32,
}
impl OriginalNativeRegionInitializationFailure {
    pub fn into_budget(mut self) -> Result<OriginalNativeBudget, Self> {
        if self.process != std::process::id() { return Err(self); }
        Ok(unsafe { ManuallyDrop::take(&mut self.budget) })
    }
}
impl OriginalNativeRegionOwner {
    /// # Safety
    /// The real outside controller already durably paid this exact graph and
    /// retains the actual resident mapping, namespace/account and original
    /// clocks through every failure. Storage is aligned, distinct, unpublished
    /// and large enough for the pure concrete quote. It cannot be freed by any
    /// other owner. Budget is the sole genuine by-value issued counter and is
    /// moved here exactly once. Generation is an authentic nonrecycled
    /// incarnation binding, never a decoded grant. No pointer escapes this API.
    pub unsafe fn initialize(storage: NonNull<u8>, bytes: usize, budget: OriginalNativeBudget,
        process: u32, generation: u64) -> Result<Self, OriginalNativeRegionInitializationFailure> {
        let mut retained = ManuallyDrop::new(budget);
        let attempt = (|| {
            if process != std::process::id() { return Err(FixedHostRefusal::Process); }
            if generation == 0 { return Err(FixedHostRefusal::Provenance); }
            let quote = original_native_region_layout()?;
            if bytes < quote.layout.size() || storage.as_ptr() as usize % quote.layout.align() != 0
                || (storage.as_ptr() as usize).checked_add(bytes).is_none() { return Err(FixedHostRefusal::Layout); }
            let header = storage.cast::<RegionHeader>();
            let payload = unsafe { NonNull::new_unchecked(storage.as_ptr().add(quote.payload_offset).cast::<NativeResidentState>()) };
            let state = NativeResidentState::new(unsafe { ManuallyDrop::take(&mut retained) });
            unsafe {
                payload.as_ptr().write(state);
                header.as_ptr().write(RegionHeader { gate: AtomicU8::new(UNLOCKED), phase: AtomicU8::new(OPEN), process, generation,
                    counts: UnsafeCell::new(Counts { handles: 0, rust: 0, c: 0 }),
                    pending_handles: AtomicUsize::new(0), pending_rust: AtomicUsize::new(0), pending_c: AtomicUsize::new(0) });
            }
            Ok(Self { header, payload, process, generation, bytes })
        })();
        attempt.map_err(|refusal| OriginalNativeRegionInitializationFailure { refusal, budget: retained, process })
    }
    pub fn handle(&self) -> Result<OriginalNativeRegionHandle, FixedHostRefusal> {
        let _guard = gate(self.header, self.process, self.generation)?;
        let h = unsafe { self.header.as_ref() };
        if h.phase.load(Ordering::Acquire) != OPEN { return Err(FixedHostRefusal::Closed); }
        let counts = unsafe { &mut *h.counts.get() };
        counts.handles = increment(counts.handles)?;
        Ok(OriginalNativeRegionHandle { header: self.header, payload: self.payload, process: self.process, generation: self.generation })
    }
    pub fn begin_close(&self) -> Result<(), FixedHostRefusal> {
        let _guard = gate(self.header, self.process, self.generation)?;
        unsafe { self.header.as_ref() }.phase.store(CLOSING, Ordering::Release);
        Ok(())
    }
    /// Exactly three paid debt cells; no scanning, allocation or replacement
    /// loan. Busy Drop publishes its actual consumed loan while keeping the
    /// original count live. This applies those exact debts under the same gate.
    pub fn retire_pending(&self) -> Result<(), FixedHostRefusal> {
        let _guard = gate(self.header, self.process, self.generation)?;
        let h = unsafe { self.header.as_ref() }; let counts = unsafe { &mut *h.counts.get() };
        for (pending, count) in [(&h.pending_handles, &mut counts.handles), (&h.pending_rust, &mut counts.rust), (&h.pending_c, &mut counts.c)] {
            let debt = pending.swap(0, Ordering::AcqRel);
            if debt > *count { pending.fetch_add(debt, Ordering::Release); return Err(FixedHostRefusal::Provenance); }
            *count -= debt;
        }
        Ok(())
    }
    pub fn census(&self) -> Result<OriginalNativeRegionCensus, FixedHostRefusal> {
        let _guard = gate(self.header, self.process, self.generation)?;
        let h = unsafe { self.header.as_ref() }; let counts = unsafe { &*h.counts.get() };
        Ok(OriginalNativeRegionCensus { closing: h.phase.load(Ordering::Acquire) != OPEN,
            handles: counts.handles, rust_borrows: counts.rust, c_borrows: counts.c,
            pending_retirements: h.pending_handles.load(Ordering::Acquire) + h.pending_rust.load(Ordering::Acquire) + h.pending_c.load(Ordering::Acquire) })
    }
    /// Final native/producer census and zero loans must be established in the
    /// same sealed region. Account/namespace/physical mapping remain with the
    /// original outside controller after this token is returned.
    pub fn seal(self) -> Result<OriginalNativeRegionReleaseExclusion, OriginalNativeRegionSealFailure> {
        let retained = ManuallyDrop::new(self);
        let attempt = (|| {
            let guard = gate(retained.header, retained.process, retained.generation)?;
            let h = unsafe { retained.header.as_ref() }; let counts = unsafe { &*h.counts.get() };
            if h.phase.load(Ordering::Acquire) != CLOSING || counts.handles != 0 || counts.rust != 0 || counts.c != 0
                || h.pending_handles.load(Ordering::Acquire) != 0 || h.pending_rust.load(Ordering::Acquire) != 0 || h.pending_c.load(Ordering::Acquire) != 0 {
                return Err(FixedHostRefusal::ReferencesRemain);
            }
            unsafe { retained.payload.as_ref() }.require_terminal_exclusion()?;
            h.phase.store(SEALED, Ordering::Release);
            // Permanently disarm before any concrete disposal/unmap. No guard
            // destructor can dereference the mapping once outside release starts.
            guard.keep_locked();
            Ok(OriginalNativeRegionReleaseExclusion { payload: retained.payload, process: retained.process,
                generation: retained.generation, disposition: 0, base: retained.header.as_ptr() as usize, bytes: retained.bytes })
        })();
        attempt.map_err(|refusal| OriginalNativeRegionSealFailure { refusal, owner: retained })
    }
}
#[derive(Debug, Clone, Copy)]
pub struct OriginalNativeRegionCensus { pub closing: bool, pub handles: usize,
    pub rust_borrows: usize, pub c_borrows: usize, pub pending_retirements: usize }
pub struct OriginalNativeRegionSealFailure { pub refusal: FixedHostRefusal, owner: ManuallyDrop<OriginalNativeRegionOwner> }
impl OriginalNativeRegionSealFailure {
    pub fn into_owner(mut self) -> Result<OriginalNativeRegionOwner, Self> {
        if self.owner.process != std::process::id() { return Err(self); }
        Ok(unsafe { ManuallyDrop::take(&mut self.owner) })
    }
}
pub struct OriginalNativeRegionReleaseExclusion { payload: NonNull<NativeResidentState>, process: u32, generation: u64, disposition: u8, base: usize, bytes: usize }
// SAFETY: seal consumed the sole outside owner after observing zero handles,
// Rust/C loans and pending retirements, and permanently locked the region.
// Moving this token therefore transfers exclusive concrete-disposal custody.
// Every method checks the copied original PID before a possible mapped access;
// dispose requires &mut self, while shared methods read only outside metadata.
// Neither transfer nor Drop releases the backing; its original outside owner
// must retain the mapping through disposal and physical-release acknowledgement.
unsafe impl Send for OriginalNativeRegionReleaseExclusion {}
unsafe impl Sync for OriginalNativeRegionReleaseExclusion {}
impl OriginalNativeRegionReleaseExclusion {
    fn require_physical_mapping(&self, base: usize, bytes: usize) -> Result<(), FixedHostRefusal> {
        if self.process != std::process::id() { return Err(FixedHostRefusal::Process); }
        if self.base != base || self.bytes != bytes { return Err(FixedHostRefusal::Provenance); }
        Ok(())
    }
    /// Concrete resident data has no native cleanup destructor. This ends its
    /// Rust lifetime once; all authority/account callbacks remain outside it.
    pub fn dispose(&mut self) -> Result<(), FixedHostRefusal> {
        if self.process != std::process::id() { return Err(FixedHostRefusal::Process); }
        if self.disposition == 2 { return Ok(()); }
        if self.disposition == 1 { return Err(FixedHostRefusal::TerminalRequired); }
        self.disposition = 1;
        unsafe { self.payload.as_ptr().drop_in_place(); }
        self.disposition = 2;
        Ok(())
    }
    pub fn original_generation(&self) -> Result<u64, FixedHostRefusal> {
        if self.process != std::process::id() { return Err(FixedHostRefusal::Process); }
        Ok(self.generation)
    }
    pub fn ready_for_physical_release(&self) -> Result<(), FixedHostRefusal> {
        if self.process != std::process::id() { return Err(FixedHostRefusal::Process); }
        if self.disposition != 2 { return Err(FixedHostRefusal::TerminalRequired); }
        Ok(())
    }
}
/// Opaque whole-region handle. Ordinary Arc/Weak wrappers cannot resurrect this
/// after their last strong value drops; their heap blocks are outside the map.
pub struct OriginalNativeRegionHandle { header: NonNull<RegionHeader>, payload: NonNull<NativeResidentState>, process: u32, generation: u64 }
unsafe impl Send for OriginalNativeRegionHandle {}
unsafe impl Sync for OriginalNativeRegionHandle {}
impl OriginalNativeRegionHandle {
    pub fn try_clone(&self) -> Result<Self, FixedHostRefusal> {
        let _guard = gate(self.header, self.process, self.generation)?;
        let h = unsafe { self.header.as_ref() };
        if h.phase.load(Ordering::Acquire) != OPEN { return Err(FixedHostRefusal::Closed); }
        let c = unsafe { &mut *h.counts.get() }; c.handles = increment(c.handles)?;
        Ok(Self { header: self.header, payload: self.payload, process: self.process, generation: self.generation })
    }
    pub(crate) fn original_process(&self) -> u32 { self.process }
    pub fn require_original_process(&self) -> Result<(), FixedHostRefusal> {
        if self.process != std::process::id() { return Err(FixedHostRefusal::Process); }
        Ok(())
    }
    pub fn same_region(&self, other: &Self) -> bool {
        self.process == other.process && self.generation == other.generation && self.header == other.header
    }
    pub(crate) fn claim_factory_install(&self) -> Result<(), FixedHostRefusal> {
        let _guard = gate(self.header, self.process, self.generation)?;
        if unsafe { self.header.as_ref() }.phase.load(Ordering::Acquire) != OPEN {
            return Err(FixedHostRefusal::Closed);
        }
        unsafe { self.payload.as_ref() }.claim_factory_install()
    }
    /// Observation-based preinstallation recovery through an actual counted
    /// region handle. The copied index grants no admission/native authority:
    /// the concrete CLOSING gate, never-installed backend census and exact
    /// allocated FactoryIssuer/ticket-zero cell establish absence here.
    /// The outside original supervisor must retain its actual issuer join,
    /// close producer admission, retire wrappers and settle the same account.
    pub fn retire_uninstalled_factory_issuer(&self,
        index: crate::native::OriginalNativeProducerCellIndex) -> Result<(), FixedHostRefusal> {
        let _guard = gate(self.header, self.process, self.generation)?;
        if unsafe { self.header.as_ref() }.phase.load(Ordering::Acquire) != CLOSING {
            return Err(FixedHostRefusal::Closed);
        }
        // Concrete atomic metadata only: no callback/destructor under gate.
        unsafe { self.payload.as_ref() }.retire_uninstalled_factory_issuer(index)
    }
    pub(crate) fn state_ref(&self) -> Result<&NativeResidentState, FixedHostRefusal> {
        self.require_original_process()?;
        // Private &self borrow is covered by this already-counted handle.
        Ok(unsafe { self.payload.as_ref() })
    }
    pub(crate) fn borrow(&self, cleanup: bool) -> Result<OriginalNativeRegionRustBorrow<'_>, FixedHostRefusal> {
        let _guard = gate(self.header, self.process, self.generation)?;
        let h = unsafe { self.header.as_ref() };
        if !cleanup && h.phase.load(Ordering::Acquire) != OPEN { return Err(FixedHostRefusal::Closed); }
        let c = unsafe { &mut *h.counts.get() }; c.rust = increment(c.rust)?;
        Ok(OriginalNativeRegionRustBorrow { header: self.header, payload: self.payload, process: self.process, generation: self.generation, _handle: PhantomData })
    }
    pub(crate) fn borrow_c(&self) -> Result<OriginalNativeRegionCBorrow, FixedHostRefusal> {
        let _guard = gate(self.header, self.process, self.generation)?;
        let h = unsafe { self.header.as_ref() };
        if h.phase.load(Ordering::Acquire) != OPEN { return Err(FixedHostRefusal::Closed); }
        let c = unsafe { &mut *h.counts.get() }; c.c = increment(c.c)?;
        Ok(OriginalNativeRegionCBorrow { header: self.header, payload: self.payload, process: self.process, generation: self.generation })
    }
}
pub(crate) struct OriginalNativeRegionRustBorrow<'a> { header: NonNull<RegionHeader>, payload: NonNull<NativeResidentState>, process: u32, generation: u64, _handle: PhantomData<&'a OriginalNativeRegionHandle> }
impl OriginalNativeRegionRustBorrow<'_> {
    pub(crate) fn state(&self) -> Result<&NativeResidentState, FixedHostRefusal> {
        if self.process != std::process::id() { return Err(FixedHostRefusal::Process); }
        Ok(unsafe { self.payload.as_ref() })
    }
}
pub(crate) struct OriginalNativeRegionCBorrow { header: NonNull<RegionHeader>, payload: NonNull<NativeResidentState>, process: u32, generation: u64 }
unsafe impl Send for OriginalNativeRegionCBorrow {}
unsafe impl Sync for OriginalNativeRegionCBorrow {}
impl OriginalNativeRegionCBorrow {
    pub(crate) fn budget_pointer(&self) -> Result<*const OriginalNativeBudget, FixedHostRefusal> {
        if self.process != std::process::id() { return Err(FixedHostRefusal::Process); }
        Ok(&unsafe { self.payload.as_ref() }.budget)
    }
}
enum LoanKind { Handle, Rust, C }
fn retire(header: NonNull<RegionHeader>, process: u32, generation: u64, kind: LoanKind) {
    if process != std::process::id() { return; }
    let h = unsafe { header.as_ref() };
    let pending = match kind { LoanKind::Handle => &h.pending_handles, LoanKind::Rust => &h.pending_rust, LoanKind::C => &h.pending_c };
    let Ok(_guard) = gate(header, process, generation) else { pending.fetch_add(1, Ordering::Release); return; };
    let c = unsafe { &mut *h.counts.get() };
    let count = match kind { LoanKind::Handle => &mut c.handles, LoanKind::Rust => &mut c.rust, LoanKind::C => &mut c.c };
    if *count != 0 { *count -= 1; }
}
impl Drop for OriginalNativeRegionHandle { fn drop(&mut self) { retire(self.header, self.process, self.generation, LoanKind::Handle); } }
impl Drop for OriginalNativeRegionRustBorrow<'_> { fn drop(&mut self) { retire(self.header, self.process, self.generation, LoanKind::Rust); } }
impl Drop for OriginalNativeRegionCBorrow { fn drop(&mut self) { retire(self.header, self.process, self.generation, LoanKind::C); } }
