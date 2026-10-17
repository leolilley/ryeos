//! Privileged original native owner. Signed issuance remains the host's job;
//! no decoded quota or observed allocation count can construct a safe factory.
use crate::{
    BoundSqliteCloseOwner, BoundSqliteConnection, BoundSqliteOpenFailure, ClosedSqliteInspection,
    ReadonlySqliteCloseFailure, ReadonlySqliteCopy, ReadonlySqliteOpenFailure,
    ReleasedSqliteInspection, SqliteCopyLease, SqliteInspectionProductionPermit,
    SqliteInspectionSqlWork, SqliteInspectionProductionKind,
};
use anyhow::{Context, Result, anyhow, ensure};
use crate::original_host_owner::{FixedHostRefusal, OriginalNativeRegionHandle,
    OriginalNativeRegionCBorrow};
use lillux::PinnedDirectory;
use rusqlite::Connection;
use std::{
    ffi::{OsStr, c_int, c_void},
    ptr::NonNull,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
};

fn require_current_native_process() -> Result<()> {
    ensure!(
        unsafe { ryeos_native_process_matches() } == 1,
        "inherited native process authority refused"
    );
    Ok(())
}

/// A native admission check refused only because another thread momentarily
/// held a lock the check needs. It refuses the contended call alone; it is not
/// a fact refusal and never latches service refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeCheckContended;
impl std::fmt::Display for NativeCheckContended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native admission check contended; this call refused without latching")
    }
}
impl std::error::Error for NativeCheckContended {}

/// A native admission check could not observe the fact it checks because of
/// a momentary host condition, such as being out of descriptors while
/// reading its own process identity. It refuses this call alone; it is not a
/// fact refusal and never latches service refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeCheckUnobservable;
impl std::fmt::Display for NativeCheckUnobservable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native admission check could not observe its fact; this call refused without latching")
    }
}
impl std::error::Error for NativeCheckUnobservable {}

/// The planned service-window edge ended service SQL. The authority latches
/// its own distinct edge bit; a boundary does not record it as a fact refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeServiceEdgeReached;
impl std::fmt::Display for NativeServiceEdgeReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native service window edge reached; service SQL ended")
    }
}
impl std::error::Error for NativeServiceEdgeReached {}

/// An earlier refusal that a later refusal supersedes. It rides the later
/// refusal as a context layer, so the later refusal stays the error's own
/// cause chain and remains downcastable, while the earlier refusal is kept
/// only for display: the classifier never reads its class.
pub struct SupersededRefusal {
    later: String,
    earlier: anyhow::Error,
}
impl SupersededRefusal {
    /// The earlier refusal this one supersedes.
    pub fn earlier(&self) -> &anyhow::Error {
        &self.earlier
    }
}
impl std::fmt::Display for SupersededRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; it supersedes an earlier refusal: {:#}", self.later, self.earlier)
    }
}
impl std::fmt::Debug for SupersededRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupersededRefusal")
            .field("later", &self.later)
            .field("earlier", &self.earlier)
            .finish()
    }
}

/// The one way to layer a later refusal over an earlier attempt's refusal.
/// The receiver is the later refusal; the argument is the earlier one.
pub trait RefusalSupersedes {
    /// This (later) refusal, keeping `earlier` for display only. The result
    /// is classified by this refusal alone.
    fn superseding(self, earlier: anyhow::Error) -> anyhow::Error;
}
impl RefusalSupersedes for anyhow::Error {
    fn superseding(self, earlier: anyhow::Error) -> anyhow::Error {
        let later = self.to_string();
        self.context(SupersededRefusal { later, earlier })
    }
}

/// Whether a refusal is a fact refusal: neither momentary contention, nor
/// the planned service edge, nor a momentary shortage or interruption of
/// host resources (descriptors, memory, an interrupted call or a device I/O
/// error) anywhere in its cause chain. A non-fact refusal still refuses its
/// call; it only does not latch a permanent service stop, so a later call
/// re-checks the durable facts.
///
/// Classification reads only types: the class markers and refusal types
/// reachable through the error's anyhow layers and its source chain. Text
/// labels add nothing and hide nothing. A refusal layered over an earlier
/// attempt with [`RefusalSupersedes::superseding`] is classified by the later
/// refusal alone, because the earlier one is not part of the cause chain: a
/// later fact latches over earlier contention, and later contention never
/// latches over an earlier fact. Two refusals combined any other way are
/// read together, and any non-fact class in either keeps the result from
/// latching, so the durable facts are re-checked by the next call.
pub fn native_refusal_is_fact(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NativeCheckContended>().is_none()
        && error.downcast_ref::<NativeCheckUnobservable>().is_none()
        && error.downcast_ref::<NativeServiceEdgeReached>().is_none()
        && !error.downcast_ref::<FixedHostRefusal>().is_some_and(region_refusal_is_contention)
        && !error.downcast_ref::<anyhow::Error>().is_some_and(|layered| !native_refusal_is_fact(layered))
        && !error.chain().any(cause_is_not_fact)
}

fn cause_is_not_fact(cause: &(dyn std::error::Error + 'static)) -> bool {
    cause.is::<NativeCheckContended>()
        || cause.is::<NativeCheckUnobservable>()
        || cause.is::<NativeServiceEdgeReached>()
        || cause.downcast_ref::<FixedHostRefusal>().is_some_and(region_refusal_is_contention)
        || cause_is_transient_resource(cause)
}

/// The one classification of a region refusal for every region-gate caller:
/// a gate held by other threads past its wait bound is momentary contention,
/// refusing that call alone without latching service refusal. Every other
/// region refusal is a fact. A region refusal reaches the classifier however
/// its caller converted it (a `?`, a context, or a source chain), so
/// no caller-side mapping is needed for it to be classified.
fn region_refusal_is_contention(refusal: &FixedHostRefusal) -> bool {
    *refusal == FixedHostRefusal::Contended
}

fn cause_is_transient_resource(cause: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(io) = cause.downcast_ref::<std::io::Error>() {
        return lillux::platform::io_error_is_transient_resource(io);
    }
    matches!(
        cause.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(failure, _))
            if failure.code == rusqlite::ErrorCode::OutOfMemory
    )
}

/// The result of one scoped SQL operation. A step or commit refused inside
/// the scope fails the operation even when the operation swallowed that
/// error and reported success: a refused commit never reads as Ok.
fn sql_scope_outcome<T>(result: Result<T>, post: Result<()>, step_refused: bool) -> Result<T> {
    match (result, post) {
        (Err(original), Err(post)) => Err(post.superseding(original)),
        (Err(original), Ok(())) => Err(original),
        (Ok(_), Err(post)) => Err(post),
        (Ok(_), Ok(())) if step_refused => Err(anyhow::anyhow!(
            "a step or commit of this SQL operation was refused although the operation reported \
             success; its result is withheld"
        )),
        (Ok(value), Ok(())) => Ok(value),
    }
}

#[cfg(test)]
mod refusal_class_tests {
    use anyhow::Context as _;

    #[test]
    fn transient_resource_refusals_are_not_facts() {
        let emfile = Err::<(), _>(std::io::Error::from_raw_os_error(libc::EMFILE))
            .context("read exact retained record")
            .context("original capacity debit check")
            .unwrap_err();
        assert!(!super::native_refusal_is_fact(&emfile));
        let eintr = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EINTR));
        assert!(!super::native_refusal_is_fact(&eintr));
        let nomem = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOMEM),
            None,
        ));
        assert!(!super::native_refusal_is_fact(&nomem));
        let unobservable = anyhow::anyhow!("proc unreadable").context(super::NativeCheckUnobservable);
        assert!(!super::native_refusal_is_fact(&unobservable));
        assert!(!super::native_refusal_is_fact(&anyhow::Error::new(super::NativeCheckContended)));

        // A region gate held past its wait bound is contention however the
        // caller converted it; every other region refusal is a fact.
        use crate::original_host_owner::FixedHostRefusal;
        let contended = anyhow::Error::new(FixedHostRefusal::Contended).context("producer control claim");
        assert!(!super::native_refusal_is_fact(&contended));
        let as_context = anyhow::anyhow!("region gate").context(FixedHostRefusal::Contended);
        assert!(!super::native_refusal_is_fact(&as_context));
        let closed = anyhow::Error::new(FixedHostRefusal::Closed).context("producer control claim");
        assert!(super::native_refusal_is_fact(&closed));
        assert!(super::native_refusal_is_fact(&anyhow::Error::new(FixedHostRefusal::Busy)));

        // A later refusal layered over an earlier attempt is classified by
        // the later refusal only, whatever wraps or labels the result: a fact
        // over a contended or transient attempt latches, and contention over
        // an earlier fact does not.
        use super::RefusalSupersedes as _;
        let fact = || anyhow::anyhow!("original capacity debit no longer matches durable account/redo");
        let contended_attempt = || anyhow::Error::new(FixedHostRefusal::Contended).context("producer control claim");
        assert!(super::native_refusal_is_fact(&fact().superseding(contended_attempt())));
        let checked_attempt = anyhow::Error::new(super::NativeCheckContended);
        assert!(super::native_refusal_is_fact(&fact().superseding(checked_attempt).context("retried close")));
        let transient_attempt = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EMFILE));
        assert!(super::native_refusal_is_fact(&fact().superseding(transient_attempt)));
        let unobservable_attempt = anyhow::anyhow!("proc unreadable").context(super::NativeCheckUnobservable);
        assert!(super::native_refusal_is_fact(&fact().superseding(unobservable_attempt)));
        assert!(!super::native_refusal_is_fact(&contended_attempt().superseding(fact())));
        assert!(!super::native_refusal_is_fact(
            &anyhow::Error::new(super::NativeCheckContended).superseding(fact()).context("retried close")));
        // Repeated retries stack; only the latest refusal classifies.
        let stacked = fact().superseding(contended_attempt()).superseding(fact());
        let stacked_over_contention = contended_attempt().superseding(fact()).superseding(contended_attempt());
        assert!(super::native_refusal_is_fact(&fact().superseding(stacked_over_contention).context("retried close")));
        assert!(super::native_refusal_is_fact(&stacked));
        assert!(!super::native_refusal_is_fact(&contended_attempt().superseding(stacked)));

        // Under a typed wrapper whose source is the layered refusal, only
        // the later refusal is in the cause chain.
        #[derive(Debug)]
        struct Wrapper(anyhow::Error);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.0.as_ref()) }
        }
        let wrapped_fact = anyhow::Error::new(Wrapper(fact().superseding(contended_attempt())));
        assert!(super::native_refusal_is_fact(&wrapped_fact));
        let wrapped_contention = anyhow::Error::new(Wrapper(
            anyhow::Error::new(super::NativeCheckContended).context("lock").superseding(fact())));
        assert!(!super::native_refusal_is_fact(&wrapped_contention));

        // A label never hides a class, and refusals combined without the
        // explicit layer are read together: contention in either part
        // keeps the result from latching, in either order.
        assert!(!super::native_refusal_is_fact(&contended_attempt().context("label").context("outer label")));
        assert!(!super::native_refusal_is_fact(&fact().context(contended_attempt())));
        assert!(!super::native_refusal_is_fact(&contended_attempt().context(fact())));
        assert!(!super::native_refusal_is_fact(&anyhow::Error::new(Wrapper(contended_attempt())).context(fact())));

        // The later refusal stays downcastable and leads the display.
        let layered = anyhow::Error::new(FixedHostRefusal::Closed).superseding(contended_attempt());
        assert_eq!(layered.downcast_ref::<FixedHostRefusal>(), Some(&FixedHostRefusal::Closed));
        assert!(layered.to_string().contains("supersedes an earlier refusal"), "{layered}");
        assert!(layered.downcast_ref::<super::SupersededRefusal>().is_some_and(|superseded|
            superseded.earlier().downcast_ref::<FixedHostRefusal>() == Some(&FixedHostRefusal::Contended)));

        // Facts still latch, including an I/O failure about the file itself.
        assert!(super::native_refusal_is_fact(&anyhow::anyhow!(
            "original capacity debit no longer matches durable account/redo"
        )));
        let missing = Err::<(), _>(std::io::Error::from_raw_os_error(libc::ENOENT))
            .context("open account")
            .unwrap_err();
        assert!(super::native_refusal_is_fact(&missing));
        let corrupt = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        ));
        assert!(super::native_refusal_is_fact(&corrupt));
    }

    #[test]
    fn a_swallowed_step_refusal_never_returns_ok() {
        assert_eq!(super::sql_scope_outcome(Ok(7), Ok(()), false).unwrap(), 7);
        let swallowed = super::sql_scope_outcome(Ok(7), Ok(()), true).unwrap_err().to_string();
        assert!(swallowed.contains("refused"), "{swallowed}");
        assert!(super::sql_scope_outcome::<u8>(Err(anyhow::anyhow!("own")), Ok(()), true).is_err());
        assert!(super::sql_scope_outcome(Ok(7), Err(anyhow::anyhow!("boundary")), false).is_err());
        // The post-operation boundary refusal supersedes the operation's own
        // refusal and alone classifies the result.
        use crate::original_host_owner::FixedHostRefusal;
        let contended = || anyhow::Error::new(FixedHostRefusal::Contended);
        let fact = || anyhow::anyhow!("original capacity debit no longer matches durable account/redo");
        let post_fact = super::sql_scope_outcome::<u8>(Err(contended()), Err(fact()), false).unwrap_err();
        assert!(super::native_refusal_is_fact(&post_fact), "{post_fact:#}");
        let post_contended = super::sql_scope_outcome::<u8>(Err(fact()), Err(contended()), false).unwrap_err();
        assert!(!super::native_refusal_is_fact(&post_contended), "{post_contended:#}");
    }
}

/// One step-hook body for every native and inspection connection. Each VM
/// opcode runs the C process check, the caller's per-operation deadline and
/// one exact step debit, which checks only in-memory facts. A panic while
/// checking refuses the opcode: SQLite's callback trampoline would otherwise
/// treat it as "continue" and run the opcode uncharged.
pub(crate) fn native_step_hook(
    debit: impl Fn() -> Result<()> + Send + 'static,
    deadline: Option<lillux::time::MonotonicDeadline>,
    refused: Option<Arc<AtomicBool>>,
) -> impl FnMut() -> bool + Send + std::panic::RefUnwindSafe + 'static {
    let debit = std::panic::AssertUnwindSafe(debit);
    move || {
        // Borrow the whole wrapper so the closure captures the asserted
        // wrapper itself, not its inner callback field.
        let debit = &debit;
        // A caught panic refuses the opcode, so no state a panic could leave
        // half-updated is observed afterwards: the whole check is asserted
        // unwind-safe rather than only the callback field.
        let refuse = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            require_current_native_process().is_err()
                || deadline.is_some_and(|deadline| deadline.has_elapsed())
                || (debit.0)().is_err()
        }))
        .unwrap_or(true);
        if refuse {
            if let Some(refused) = &refused {
                refused.store(true, Ordering::SeqCst);
            }
        }
        refuse
    }
}

/// Refusal observation of one scoped SQL operation on one connection. The
/// flag is cleared for this scope and an enclosing scope's observation is
/// restored on return or unwind, so a nested scope never hides a refusal
/// the enclosing scope already saw; a refusal inside the nested scope stays
/// visible to the enclosing one.
struct StepRefusalScope {
    flag: Arc<AtomicBool>,
    enclosing: bool,
}
impl StepRefusalScope {
    fn enter(flag: Arc<AtomicBool>) -> Self {
        let enclosing = flag.swap(false, Ordering::SeqCst);
        Self { flag, enclosing }
    }
    fn refused(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}
impl Drop for StepRefusalScope {
    fn drop(&mut self) {
        if self.enclosing {
            self.flag.store(true, Ordering::SeqCst);
        }
    }
}

#[derive(Clone, Copy)]
struct NativeBoundaryFrame {
    authority: *const (),
    cleanup: bool,
    verified: bool,
}
thread_local! {
    static NATIVE_BOUNDARY: std::cell::Cell<Option<NativeBoundaryFrame>> =
        const { std::cell::Cell::new(None) };
}
// Records, for the running boundary on this thread only, that the authority's
// durable reads passed once. Restores the enclosing frame on return or unwind.
struct NativeBoundaryScope {
    previous: Option<NativeBoundaryFrame>,
}
impl NativeBoundaryScope {
    fn enter(authority: *const (), cleanup: bool) -> Self {
        let previous = NATIVE_BOUNDARY.with(|frame| frame.replace(Some(NativeBoundaryFrame {
            authority, cleanup, verified: false,
        })));
        Self { previous }
    }
    fn verified(&self) {
        NATIVE_BOUNDARY.with(|frame| {
            if let Some(mut current) = frame.get() {
                current.verified = true;
                frame.set(Some(current));
            }
        });
    }
}
impl Drop for NativeBoundaryScope {
    fn drop(&mut self) {
        NATIVE_BOUNDARY.with(|frame| frame.set(self.previous));
    }
}

/// Within one running native boundary on this thread, whether `authority`
/// already passed its durable reads in the same mode. A nested check may then
/// skip only those reads; it must still re-read the service latch and the
/// process. Nothing is reused across boundaries or threads.
pub fn native_boundary_reuses_durable_check<T: ?Sized>(authority: &T, cleanup: bool) -> bool {
    let authority = authority as *const T as *const ();
    NATIVE_BOUNDARY.with(|frame| frame.get().is_some_and(|frame| {
        frame.verified && frame.cleanup == cleanup && std::ptr::eq(frame.authority, authority)
    }))
}

/// Fixed connection-ticket capacity of one native arena.
pub const MAX_NATIVE_CONNECTIONS: usize = 64;
/// Storage an arena reserves for its own control block; a funded envelope
/// must exceed it.
pub const CONTROL_STORAGE: u64 = 65_536;
/// Compare-exchange attempts on one budget counter before refusing as busy.
/// Each lost attempt means a concurrent debit succeeded, so the bound limits
/// starvation under contention without changing what any debit charges.
const ORIGINAL_NATIVE_COUNTER_ATTEMPTS: usize = 1024;

/// Exactly one counter object shared by the real signed issuer and C allocator.
/// C has a checked identical layout; callbacks never call host custody methods.
#[repr(C)]
pub struct OriginalNativeBudget {
    storage_bytes: u64,
    allocator_steps_limit: u64,
    cleanup_reserved: u64,
    connections_limit: u32,
    reserved: u32,
    allocator_steps: AtomicU64,
    cleanup_steps: AtomicU64,
    connections_issued: AtomicU32,
    reserved_atomic: AtomicU32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginalNativeBudgetSnapshot {
    pub storage_bytes: u64,
    pub allocator_steps: u64,
    pub cleanup_reserved: u64,
    pub cleanup_steps: u64,
    pub connections_issued: u32,
}
impl OriginalNativeBudget {
    /// # Safety
    /// The caller must have irreversibly reserved this exact storage, work and
    /// ticket envelope from ONE authentic original signed admission. It must
    /// retain that original clock/exclusions and use this object, never copies.
    pub unsafe fn prepaid_by_original_issuer(storage_bytes: u64, allocator_steps: u64,
        connections: u32, cleanup_reserved: u64) -> std::result::Result<Self, FixedHostRefusal> {
        if unsafe { ryeos_native_process_matches() } != 1 { return Err(FixedHostRefusal::Process); }
        if !(storage_bytes > CONTROL_STORAGE && storage_bytes <= i64::MAX as u64
            && allocator_steps > cleanup_reserved && allocator_steps <= i64::MAX as u64
            && cleanup_reserved > 0 && connections > 0 && connections as usize <= MAX_NATIVE_CONNECTIONS) {
            return Err(FixedHostRefusal::Layout);
        }
        Ok(Self { storage_bytes, allocator_steps_limit: allocator_steps, cleanup_reserved,
            connections_limit: connections, reserved: 0, allocator_steps: AtomicU64::new(0),
            cleanup_steps: AtomicU64::new(0), connections_issued: AtomicU32::new(0), reserved_atomic: AtomicU32::new(0) })
    }
    /// Connection tickets this exact envelope admits.
    pub fn connections_limit(&self) -> u32 {
        self.connections_limit
    }
    pub fn snapshot(&self) -> OriginalNativeBudgetSnapshot {
        OriginalNativeBudgetSnapshot {
            storage_bytes: self.storage_bytes,
            allocator_steps: self.allocator_steps.load(Ordering::Acquire),
            cleanup_reserved: self.cleanup_reserved,
            cleanup_steps: self.cleanup_steps.load(Ordering::Acquire),
            connections_issued: self.connections_issued.load(Ordering::Acquire),
        }
    }
    /// Issuer/controller precharge. Never refunds and never invokes native work.
    pub fn debit_steps(&self, steps: u64, cleanup: bool) -> Result<()> {
        require_current_native_process()?;
        ensure!(steps > 0, "empty original native work debit");
        let (counter, limit) = if cleanup {
            (&self.cleanup_steps, self.cleanup_reserved)
        } else {
            (
                &self.allocator_steps,
                self.allocator_steps_limit - self.cleanup_reserved,
            )
        };
        let mut previous = counter.load(Ordering::Acquire);
        // A lost exchange only means another debit landed first; recompute the
        // total from the observed value so exhaustion is judged on what is
        // actually charged. Bounded so contention cannot spin forever.
        for _ in 0..ORIGINAL_NATIVE_COUNTER_ATTEMPTS {
            let total = previous
                .checked_add(steps)
                .context("original native work overflow")?;
            ensure!(total <= limit, "original native work exhausted");
            match counter.compare_exchange(previous, total, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(()),
                Err(observed) => previous = observed,
            }
        }
        Err(anyhow!("original native counter busy; no effect admitted"))
    }
    pub fn debit_connection_ticket(&self) -> Result<u32> {
        require_current_native_process()?;
        let mut previous = self.connections_issued.load(Ordering::Acquire);
        for _ in 0..ORIGINAL_NATIVE_COUNTER_ATTEMPTS {
            let next = previous
                .checked_add(1)
                .context("original native ticket overflow")?;
            ensure!(
                next <= self.connections_limit,
                "original native connection tickets exhausted"
            );
            match self.connections_issued.compare_exchange(previous, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(next),
                Err(observed) => previous = observed,
            }
        }
        Err(anyhow!("original native ticket counter busy"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginalNativeDomain {
    StateTransient,
    StateGeneratedProjection,
    RuntimeMutation,
    RuntimeInspection,
    SchedulerInspection,
    SchedulerRetirement,
    OperationalInspection,
    OperationalReplayReset,
    NodeRuntime,
    NodeRuntimeSchemaReference,
    NodeOperationalSchemaReference,
    NodeProjectionSchemaReference,
    NodeSchedulerSchemaReference,
    NodeAccountingSchemaReference,
    NodeOperational,
    NodeProjection,
    NodeScheduler,
    NodeAccounting,
    NodeProjectionInspection,
    OperationalSyncJobSchemaReference,
    OperationalReplaySchemaReference,
    /// An external candidate supervisor's own occurrence journal.
    ExternalSupervisorJournal,
    /// The guest journal an external candidate supervisor keeps for one
    /// launched occurrence.
    ExternalGuestJournal,
    ExternalSupervisorJournalSchemaReference,
    ExternalGuestJournalSchemaReference,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginalNativeBoundMode {
    Existing,
    CreateExclusive,
    /// Genuine live-store input only; existing native exclusive-create mode 2.
    OpenOrCreate,
    ReadonlyInspection,
    /// Established stable source; no disposable copy lease or extra producer.
    ReadonlySource,
}

/// Trusted host integration, not a user-supplied quota/identity constructor.
pub trait OriginalNativeAdmissionAuthority: Send + Sync {
    /// Durable admission facts. Service mode also reads the service latch on
    /// every call; cleanup mode never reads it. A fact refusal latches.
    fn require_original(&self, cleanup: bool) -> Result<()>;
    fn require_runtime_binding(&self, runtime: &PinnedDirectory) -> Result<()>;
    fn budget(&self) -> Arc<OriginalNativeBudgetOwner>;
    /// Charge exactly `steps` VM steps. Runs once per opcode, so it checks
    /// in-memory facts only (service latch, fixed service edge, budget): no
    /// file, procfs, mutex or digest work. Durable facts run at boundaries.
    fn debit_sql_vm_steps(&self, steps: u64) -> Result<()>;
    /// Latch this incarnation's service refusal. Every later service check
    /// and VM step refuses; cleanup never reads the latch. Never cleared.
    fn refuse_service(&self);
    fn begin_connection(&self) -> Result<OriginalNativeConnectionAdmission>;
    fn begin_projection_lease(&self) -> Result<OriginalNativeProjectionLeaseAdmission>;
}
pub struct OriginalNativeProjectionLeaseAdmission {
    budget: Arc<OriginalNativeBudgetOwner>,
    ticket: u32,
    producer: SqliteInspectionProductionPermit,
}
impl OriginalNativeProjectionLeaseAdmission {
    /// # Safety
    /// SAME authentic prepaid native budget, irreversibly debited cumulative
    /// ticket and genuine distinct lease production interval, all retained
    /// before effect. SQL admission or a decoded grant is not this authority.
    pub unsafe fn issued(
        budget: Arc<OriginalNativeBudgetOwner>,
        ticket: u32,
        producer: SqliteInspectionProductionPermit,
    ) -> std::result::Result<Self, OriginalNativeProjectionLeaseAdmissionFailure> {
        // Retain the exact published producer before any fallible observation.
        // Validation refusal cannot abandon its issuer or manufacture a terminal.
        Self { budget, ticket, producer }.validate_original()
    }
    fn validate_original(self) -> std::result::Result<Self, OriginalNativeProjectionLeaseAdmissionFailure> {
        let result = (|| -> Result<()> {
            self.budget.require_original_process()
                .map_err(|error| anyhow!("original admission process refusal: {error:?}"))?;
            ensure!(self.budget.contains_issued_ticket(self.ticket)?,
                "invalid original projection lease ticket");
            Ok(())
        })();
        match result {
            Ok(()) => Ok(self),
            Err(error) => Err(OriginalNativeProjectionLeaseAdmissionFailure {
                error, inner: lillux::custody::Held::until_taken(PROJECTION_LEASE_ADMISSION_FAILURE_KIND, ProjectionLeaseAdmissionFailureParts { original: self }),
            }),
        }
    }
}

/// Actual never-delivered admission, including its original published permit.
/// Drop retains custody; it does not abandon production or acknowledge C cleanup.
const PROJECTION_LEASE_ADMISSION_FAILURE_KIND: &str = "lillux-sqlite::projection-lease-admission-failure";
/// The admission of a refused delivery. Constructible only in this module.
struct ProjectionLeaseAdmissionFailureParts { original: OriginalNativeProjectionLeaseAdmission }
pub struct OriginalNativeProjectionLeaseAdmissionFailure {
    error: anyhow::Error,
    inner: lillux::custody::Held<ProjectionLeaseAdmissionFailureParts>,
}
impl OriginalNativeProjectionLeaseAdmissionFailure {
    /// Retry only the SAME admission. No ticket, issuer, debit or clock is renewed.
    pub fn retry(self) -> std::result::Result<OriginalNativeProjectionLeaseAdmission, Self> {
        // Copied process must be checked before extracting or dropping any part.
        if self.inner.original.budget.require_original_process().is_err() {
            return Err(self);
        }
        match self.into_fields() {
            Ok((original, _error)) => original.validate_original(),
            Err(failure) => Err(failure),
        }
    }
}
impl OriginalNativeProjectionLeaseAdmissionFailure {
    /// Move both fields out; a copy in another process is refused and comes
    /// back still held. Any other end of the failure retains the admission,
    /// counted: its published producer interval and debited ticket stay held
    /// until the process exits, and no production is abandoned from here.
    fn into_fields(self) -> std::result::Result<(OriginalNativeProjectionLeaseAdmission, anyhow::Error), Self> {
        let Self { error, inner } = self;
        match inner.take_whole() {
            Ok(parts) => Ok((parts.original, error)),
            Err(inner) => Err(Self { error, inner }),
        }
    }
}
impl std::fmt::Debug for OriginalNativeProjectionLeaseAdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("original admission refused; actual published producer retained")
    }
}
impl std::fmt::Display for OriginalNativeProjectionLeaseAdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original admission refused; actual published producer retained: {}", self.error)
    }
}
impl std::error::Error for OriginalNativeProjectionLeaseAdmissionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.error.as_ref()) }
}
#[derive(Clone, Copy)]
pub enum OriginalProjectionLeaseMode { Shared, Exclusive }
pub trait OriginalNativeProjectionLeaseAuthority: Send + Sync {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>, cleanup: bool) -> Result<()>;
    /// Return the actual original endpoint; never restart a duration for this operation.
    fn fork_deadline(&self, factory: &Arc<OriginalNativeFactory>, cleanup: bool)
        -> Result<lillux::time::MonotonicDeadline>;
}
const PROJECTION_LEASE_INPUTS_KIND: &str = "lillux-sqlite::projection-lease-inputs";
/// Only a completed lease close consumes the metadata descriptor, and it ends
/// these inputs through `release_consumed_metadata`. Any other end still holds
/// a live descriptor no close path has verified, so the file stays retained,
/// counted, until the process exits; the root and authority release normally.
pub struct OriginalProjectionLeaseInputs {
    root: Arc<PinnedDirectory>,
    metadata: lillux::custody::Held<std::fs::File>,
    mode: OriginalProjectionLeaseMode,
    authority: Arc<dyn OriginalNativeProjectionLeaseAuthority>,
    identity: (u64, u64),
}
impl OriginalProjectionLeaseInputs {
    /// # Safety
    /// Actual State namespace and SAME factory funding/clock/exclusions must
    /// be retained. This exact metadata inode must have been preflighted against
    /// all original SQLite roles. Host storage quote must be reserved BEFORE
    /// allocating inputs, issuer interval, retained failures or receipts.
    pub unsafe fn issued(
        root: Arc<PinnedDirectory>,
        metadata: std::fs::File,
        mode: OriginalProjectionLeaseMode,
        authority: Arc<dyn OriginalNativeProjectionLeaseAuthority>,
    ) -> Self {
        // Transfer custody before validation: even a contradictory data FD is
        // retained, never automatically closed while refusing unsafe issuance.
        Self { root, metadata: lillux::custody::Held::until_taken(PROJECTION_LEASE_INPUTS_KIND, metadata),
            mode, authority, identity: (0, 0) }
    }
    /// End these inputs after native code consumed the metadata descriptor
    /// number exactly once. The File value is never closed again (its number
    /// is released unclosed); the root and authority release normally. In
    /// another process the copied descriptor is relinquished instead.
    fn release_consumed_metadata(self) {
        use std::os::fd::IntoRawFd;
        let Self { root, metadata, mode: _, authority, identity: _ } = self;
        if let Ok(file) = metadata.take_whole() {
            let _consumed_number = file.into_raw_fd();
        }
        drop(root);
        drop(authority);
    }
}
/// Fixed native debits/layout facts, not funding or total operation cost.
/// Allocator work and repeated refusals additionally debit the original budget.
pub struct ProjectionLeaseNativeQuote {
    pub arena_heap_bytes: u64,
    pub acquisition_debit: u64,
    pub alias_lookup_debit: u64,
    pub cleanup_debit: u64,
    pub cleanup_floor_per_ticket: u64,
}
pub fn projection_lease_native_quote() -> Result<ProjectionLeaseNativeQuote> {
    require_current_native_process()?;
    Ok(ProjectionLeaseNativeQuote {
        arena_heap_bytes: 256,
        acquisition_debit: 4096 + 64 + 16,
        alias_lookup_debit: 64,
        cleanup_debit: 64 + 16,
        cleanup_floor_per_ticket: 64 + 16 + 1,
    })
}

/// Exact fixed-layout host control floor; includes retained owner, failure and
/// terminal plus Arc header. Native C owner occupies one arena block separately.
/// Dynamic errors, producer/namespace/account journal and caller census storage
/// must be separately prequoted by host; this floor is not a total host allowance.
pub fn projection_lease_host_storage_bytes() -> Result<u64> {
    require_current_native_process()?;
    let bytes = std::mem::size_of::<OriginalProjectionLeaseInputs>()
        .checked_add(std::mem::size_of::<OriginalProjectionLease>())
        .and_then(|n| n.checked_add(std::mem::size_of::<OriginalProjectionLeaseFailure>()))
        .and_then(|n| n.checked_add(std::mem::size_of::<ClosedOriginalProjectionLease>()))
        .and_then(|n| n.checked_add(std::mem::size_of::<NotAcquiredOriginalProjectionLease>()))
        .and_then(|n| n.checked_add(2 * std::mem::size_of::<usize>()))
        .context("projection lease host quote overflow")?;
    u64::try_from(bytes).context("projection lease host quote unsupported")
}
const PROJECTION_LEASE_KIND: &str = "lillux-sqlite::projection-lease";
/// The lease's custody: the metadata inputs, a published producer, the native
/// lock and the fork-close inventory. Constructible only in this module.
struct ProjectionLeaseParts {
    admission_failure: Option<OriginalNativeProjectionLeaseAdmissionFailure>,
    inputs: OriginalProjectionLeaseInputs,
    producer: Option<SqliteInspectionProductionPermit>,
    native: Option<crate::binding::ProjectionLease>,
    fork_reservation: Option<lillux::exec::BorrowedForkChildCloseReservation>,
    fork_registration: Option<lillux::exec::BorrowedForkChildCloseRegistration>,
}
/// No Drop cleanup, unlock, registry removal, ticket acknowledgement or
/// refund. Only a successful close settles a lease. Any other end may hold the
/// metadata descriptor, an acquired native lock, a published producer or fork
/// close inventory, so the whole lease custody stays retained, counted, until
/// the process exits.
pub struct OriginalProjectionLease {
    factory: Arc<OriginalNativeFactory>,
    ticket: Option<u32>,
    pin_consumed: bool,
    native_closed: bool,
    ticket_registered: bool,
    close_refusal: bool,
    fd_consumed: bool,
    fork_invariant_refused: bool,
    inner: lillux::custody::Held<ProjectionLeaseParts>,
}
const PROJECTION_LEASE_FAILURE_KIND: &str = "lillux-sqlite::projection-lease-failure";
/// The retained lease of a refused operation. Constructible only in this
/// module.
struct ProjectionLeaseFailureParts { owner: Mutex<OriginalProjectionLease> }
/// Only `into_owner` settles a lease refusal. Any other end retains the lease,
/// counted, with its partial native and descriptor custody until the process
/// exits.
pub struct OriginalProjectionLeaseFailure {
    error: anyhow::Error,
    inner: lillux::custody::Held<ProjectionLeaseFailureParts>,
}
impl OriginalProjectionLeaseFailure {
    /// Borrow the actual retained admission using exclusive outside custody;
    /// no mutex guard spans recovery or any native/authority effect.
    pub fn retained_admission(&mut self) -> Option<&OriginalNativeProjectionLeaseAdmissionFailure> {
        let owner = self.inner.get_mut().owner.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        if owner.factory.require_process().is_err() { return None; }
        owner.inner.admission_failure.as_ref()
    }
    /// Recover in place so the enclosing failure remains available on every
    /// refusal, including the SAME original callback panic evidence.
    pub fn recover_retained_admission(&mut self) -> Result<()> {
        let owner = self.inner.get_mut().owner.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        owner.factory.require_process()?;
        let result = {
            let actual = std::cell::RefCell::new(owner);
            crate::original_physical_custody::retain_callback_refusal(&|| {
                actual.borrow_mut().recover_retained_admission()
            })
        };
        if let Err(error) = result {
            let previous = std::mem::replace(&mut self.error,
                anyhow!("original lease failure evidence transferred"));
            // The actual new refusal/panic supersedes the previous one: it
            // stays the error's own cause chain, directly downcastable, and
            // alone classifies the result; the previous error object is kept
            // whole for display.
            self.error = error.superseding(previous);
            anyhow::bail!("original lease admission recovery refused; whole owner and evidence retained");
        }
        Ok(())
    }
    pub fn error(&self) -> &anyhow::Error { &self.error }
    pub fn into_owner(mut self) -> std::result::Result<OriginalProjectionLease, Self> {
        if require_current_native_process().is_err() { return Err(self); }
        if self.inner.get_mut().owner.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner)
            .factory.require_process().is_err() { return Err(self); }
        let Self { error, inner } = self;
        match inner.take_whole() {
            Ok(parts) => {
                drop(error);
                Ok(parts.owner.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner))
            }
            Err(inner) => Err(Self { error, inner }),
        }
    }
}
impl std::fmt::Debug for OriginalProjectionLeaseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalProjectionLeaseFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalProjectionLeaseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { std::fmt::Display::fmt(&self.error, f) }
}
impl std::error::Error for OriginalProjectionLeaseFailure {}
/// Actual metadata-pin disposal before any lease ticket/producer/native acquisition.
/// This affine receipt is not a ticket terminal and never enters its census.
/// Caller-held account/namespace custody must survive this separate disposition.
pub struct NotAcquiredOriginalProjectionLease {
    factory: Arc<OriginalNativeFactory>,
    metadata_identity: (u64, u64),
    pin_consumed: bool,
}
impl NotAcquiredOriginalProjectionLease {
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        factory.require_process()?;
        ensure!(Arc::ptr_eq(&self.factory, factory) && self.pin_consumed,
            "foreign/incomplete projection lease non-acquisition");
        Ok(())
    }
    /// Actual disposed metadata inode; diagnostic facts, no namespace/account authority.
    pub fn disposed_metadata_identity(&self) -> (u64, u64) { self.metadata_identity }
}
pub enum OriginalProjectionLeaseDisposition {
    Closed(Arc<ClosedOriginalProjectionLease>),
    NotAcquired(NotAcquiredOriginalProjectionLease),
}
pub struct ClosedOriginalProjectionLease {
    factory: Arc<OriginalNativeFactory>,
    ticket: u32,
}
impl ClosedOriginalProjectionLease {
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        factory.require_process()?;
        ensure!(Arc::ptr_eq(&self.factory, factory) && self.ticket > 0
            && self.ticket as usize <= MAX_NATIVE_CONNECTIONS
            && factory.storage()?.tickets[self.ticket as usize - 1].load(Ordering::Acquire) == 4,
            "projection lease terminal selects foreign or pending owner");
        Ok(())
    }
}
impl OriginalProjectionLease {
    fn new(factory: Arc<OriginalNativeFactory>, inputs: OriginalProjectionLeaseInputs) -> Self {
        Self {
            factory, ticket: None,
            pin_consumed: false, native_closed: false, ticket_registered: false, close_refusal: false,
            fd_consumed: false, fork_invariant_refused: false,
            inner: lillux::custody::Held::until_taken(
                PROJECTION_LEASE_KIND,
                ProjectionLeaseParts {
                    admission_failure: None, inputs, producer: None, native: None,
                    fork_reservation: None, fork_registration: None,
                }),
        }
    }
    pub fn recover_retained_admission(&mut self) -> Result<()> {
        self.factory.require_original(true)?;
        ensure!(self.inner.native.is_none() && self.ticket.is_none() && self.inner.producer.is_none()
            && !self.ticket_registered && !self.pin_consumed && !self.native_closed,
            "lease admission recovery conflicts with actual acquired/disposed owner");
        let failure = self.inner.get_mut().admission_failure.take()
            .context("actual retained lease admission absent")?;
        let original = &failure.inner.original;
        let result = crate::original_physical_custody::retain_callback_refusal(&|| {
            self.factory.recover_undelivered_admission(
                &original.budget, original.ticket, &original.producer,
                SqliteInspectionProductionKind::ProjectionLease)
        });
        if let Err(error) = result {
            self.inner.get_mut().admission_failure = Some(failure);
            return Err(error);
        }
        let (original, _error) = match failure.into_fields() {
            Ok(fields) => fields,
            Err(failure) => {
                self.inner.get_mut().admission_failure = Some(failure);
                anyhow::bail!("lease admission failure belongs to another process");
            }
        };
        self.ticket = Some(original.ticket);
        self.inner.get_mut().producer = Some(original.producer);
        self.ticket_registered = true;
        Ok(())
    }
    pub fn retained_admission(&self) -> Option<&OriginalNativeProjectionLeaseAdmissionFailure> {
        if self.factory.require_process().is_err() { return None; }
        self.inner.admission_failure.as_ref()
    }
    pub fn metadata_pin_consumed(&self) -> bool { self.pin_consumed }
    pub fn native_close_completed(&self) -> bool { self.native_closed }
    pub fn consumed_close_refusal(&self) -> bool { self.close_refusal }
    pub fn data_descriptor_consumed(&self) -> bool { self.fd_consumed }
    pub fn fork_invariant_refused(&self) -> bool { self.fork_invariant_refused }
    pub fn close(mut self) -> std::result::Result<OriginalProjectionLeaseDisposition, OriginalProjectionLeaseFailure> {
        let result = (|| -> Result<OriginalProjectionLeaseDisposition> {
            self.factory.require_original(true)?;
            ensure!(self.inner.admission_failure.is_none(),
                "actual published lease admission unresolved; no close or NotAcquired terminal");
            self.inner.inputs.authority.require_original(&self.factory, true)?;
            ensure!(!self.factory.storage()?.arena_released.load(Ordering::Acquire),
                "lease input/native cleanup must precede actual arena release");
            ensure!(!self.close_refusal, "projection lease consumed-close refusal retained; no replay");
            ensure!(!self.fork_invariant_refused,
                "projection lease fork invariant refused; owner/exclusion retained, no terminal");
            // Establish a truthful disposition route BEFORE consuming any descriptor.
            let not_acquired = !self.ticket_registered && self.ticket.is_none()
                && self.inner.producer.is_none() && self.inner.native.is_none();
            ensure!(self.ticket_registered || not_acquired,
                "unregistered reported lease admission retained; no descriptor disposal");
            let deadline = self.inner.inputs.authority.fork_deadline(&self.factory, true)?;
            let fork_guard = lillux::exec::retain_fork_sensitive_descriptors_until(deadline)
                .context("projection lease original cleanup fork deadline refused")?;
            ensure!(!deadline.has_elapsed(), "projection lease original cleanup deadline elapsed");
            self.factory.require_original(true)?;
            self.inner.inputs.authority.require_original(&self.factory, true)?;
            let _effect = self.factory.enter_effect()?;
            let _scope = if let Some(arena) = self.factory.storage()?.arena.get() {
                Some(arena.scope(true, 0)?)
            } else {
                ensure!(not_acquired, "acquired lease without original arena");
                None
            };
            #[cfg(not(target_os = "linux"))]
            anyhow::bail!("projection lease cleanup requires Linux");
            #[cfg(target_os = "linux")]
            if !self.pin_consumed {
                use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
                let flags = unsafe { libc::fcntl(self.inner.inputs.metadata.as_raw_fd(), libc::F_GETFL) };
                ensure!(flags >= 0 && flags & libc::O_PATH != 0,
                    "lease cleanup retains contradictory non-metadata descriptor");
                let metadata = self.inner.inputs.metadata.metadata()?;
                ensure!(metadata.is_file(), "lease cleanup metadata is not regular");
                if not_acquired { self.inner.get_mut().inputs.identity = (metadata.dev(), metadata.ino()); }
            }
            if !self.native_closed {
                if let Some(native) = &self.inner.native {
                    let (code, consumed, fd_consumed) = native.close();
                    self.pin_consumed |= consumed;
                    self.fd_consumed |= fd_consumed;
                    if code == libsqlite3_sys::SQLITE_OK {
                        self.inner.get_mut().native = None;
                        self.native_closed = true;
                    }
                    // Commit real consumption before interpreting status/late failures.
                    if fd_consumed {
                        if let Some(registration) = self.inner.get_mut().fork_registration.take() {
                            match unsafe { registration.unregister_consumed(&fork_guard) } {
                                Ok(()) => {}
                                Err(registration) => {
                                    self.inner.get_mut().fork_registration = Some(registration);
                                    self.fork_invariant_refused = true;
                                    // Invariant-only fail-closed: prevent every protected fork
                                    // from observing a stale consumed numeric registration.
                                    fork_guard.hold_until_process_exit();
                                    anyhow::bail!("lease consumed-FD fork deregistration invariant refused");
                                }
                            }
                        }
                    }
                    ensure!(code == libsqlite3_sys::SQLITE_OK, "projection lease native close refused ({code})");
                } else {
                    if !self.pin_consumed {
                        use std::os::fd::AsRawFd;
                        self.factory.budget.debit_steps(80, true)?;
                        let outcome = crate::binding::close_original_metadata_pin(
                            self.inner.inputs.metadata.as_raw_fd(), self.inner.inputs.identity.0, self.inner.inputs.identity.1);
                        self.pin_consumed |= outcome.consumed;
                        if outcome.consumed && (outcome.code != libsqlite3_sys::SQLITE_OK || outcome.observed_errno != 0) {
                            self.close_refusal = true;
                        }
                        ensure!(outcome.code == libsqlite3_sys::SQLITE_OK && outcome.consumed
                            && outcome.observed_errno == 0, "projection lease unacquired pin close refused");
                    }
                    self.native_closed = true;
                }
            }
            ensure!(self.inner.fork_registration.is_none() && self.inner.fork_reservation.is_none(),
                "projection lease child-close inventory still retained");
            self.factory.require_original(true)?;
            self.inner.inputs.authority.require_original(&self.factory, true)?;
            if not_acquired {
                ensure!(self.pin_consumed && self.native_closed,
                    "lease non-acquisition metadata disposal incomplete");
                return Ok(OriginalProjectionLeaseDisposition::NotAcquired(
                    NotAcquiredOriginalProjectionLease { factory: self.factory.clone(),
                        metadata_identity: self.inner.inputs.identity, pin_consumed: self.pin_consumed }));
            }
            ensure!(self.ticket_registered, "projection lease has no registered original ticket");
            let ticket = self.ticket.context("projection lease has no issued ticket; no terminal minted")?;
            if self.factory.storage()?.tickets[ticket as usize - 1].load(Ordering::Acquire) != 4 {
                self.factory.require_original(true)?;
                self.inner.inputs.authority.require_original(&self.factory, true)?;
                ensure!(unsafe { ryeos_native_connection_closed(self.factory.arena()?.0.as_ptr(), ticket) }
                    == libsqlite3_sys::SQLITE_OK, "projection lease native ticket close refused");
                // Actual native success is retained before the fallible upper
                // interval ACK; retries do not repeat the consumed C effect.
                self.factory.storage()?.tickets[ticket as usize - 1].store(4, Ordering::Release);
            }
            if let Some(producer) = self.inner.producer.as_ref() { producer.acknowledge_projection_lease_terminal()?; }
            Ok(OriginalProjectionLeaseDisposition::Closed(Arc::new(
                ClosedOriginalProjectionLease { factory: self.factory.clone(), ticket })))
        })();
        match result {
            Ok(receipt) => {
                // Only successful explicit cleanup releases Rust custody; metadata File
                // remains unclosed because C consumed its numeric descriptor once.
                // Every part is ended here in order; `ticket` and the six
                // disposition flags are plain copies.
                match self.inner.take_whole() {
                    Ok(parts) => {
                        let ProjectionLeaseParts { admission_failure, inputs, producer, native,
                            fork_reservation, fork_registration } = parts;
                        drop(producer);
                        inputs.release_consumed_metadata();
                        drop(fork_registration);
                        drop(fork_reservation);
                        drop(admission_failure);
                        drop(self.factory);
                        drop(native);
                        Ok(receipt)
                    }
                    Err(held) => {
                        self.inner = held;
                        Err(OriginalProjectionLeaseFailure {
                            error: anyhow!("projection lease belongs to another process"),
                            inner: lillux::custody::Held::until_taken(
                                PROJECTION_LEASE_FAILURE_KIND,
                                ProjectionLeaseFailureParts { owner: Mutex::new(self) }),
                        })
                    }
                }
            }
            Err(error) => Err(OriginalProjectionLeaseFailure {
                error,
                inner: lillux::custody::Held::until_taken(
                    PROJECTION_LEASE_FAILURE_KIND, ProjectionLeaseFailureParts { owner: Mutex::new(self) }),
            }),
        }
    }
}

pub struct OriginalNativeConnectionAdmission {
    budget: Arc<OriginalNativeBudgetOwner>,
    ticket: u32,
    producer: SqliteInspectionProductionPermit,
}
impl OriginalNativeConnectionAdmission {
    /// # Safety
    /// Exact original Producing phase, SAME signed shared counter ticket and
    /// genuine native connection producer interval, all retained before effect.
    pub unsafe fn issued(
        budget: Arc<OriginalNativeBudgetOwner>,
        ticket: u32,
        producer: SqliteInspectionProductionPermit,
    ) -> std::result::Result<Self, OriginalNativeConnectionAdmissionFailure> {
        // Retain the exact published producer before any fallible observation.
        // Validation refusal cannot abandon its issuer or manufacture a terminal.
        Self { budget, ticket, producer }.validate_original()
    }
    fn validate_original(self) -> std::result::Result<Self, OriginalNativeConnectionAdmissionFailure> {
        let result = (|| -> Result<()> {
            self.budget.require_original_process()
                .map_err(|error| anyhow!("original admission process refusal: {error:?}"))?;
            ensure!(self.budget.contains_issued_ticket(self.ticket)?,
                "invalid original native producer ticket");
            Ok(())
        })();
        match result {
            Ok(()) => Ok(self),
            Err(error) => Err(OriginalNativeConnectionAdmissionFailure {
                error, inner: lillux::custody::Held::until_taken(CONNECTION_ADMISSION_FAILURE_KIND, ConnectionAdmissionFailureParts { original: self }),
            }),
        }
    }
}

/// Actual never-delivered admission, including its original published permit.
/// Drop retains custody; it does not abandon production or acknowledge C cleanup.
const CONNECTION_ADMISSION_FAILURE_KIND: &str = "lillux-sqlite::native-connection-admission-failure";
/// The admission of a refused delivery. Constructible only in this module.
struct ConnectionAdmissionFailureParts { original: OriginalNativeConnectionAdmission }
pub struct OriginalNativeConnectionAdmissionFailure {
    error: anyhow::Error,
    inner: lillux::custody::Held<ConnectionAdmissionFailureParts>,
}
impl OriginalNativeConnectionAdmissionFailure {
    /// Retry only the SAME admission. No ticket, issuer, debit or clock is renewed.
    pub fn retry(self) -> std::result::Result<OriginalNativeConnectionAdmission, Self> {
        // Copied process must be checked before extracting or dropping any part.
        if self.inner.original.budget.require_original_process().is_err() {
            return Err(self);
        }
        match self.into_fields() {
            Ok((original, _error)) => original.validate_original(),
            Err(failure) => Err(failure),
        }
    }
}
impl OriginalNativeConnectionAdmissionFailure {
    /// Move both fields out; a copy in another process is refused and comes
    /// back still held. Any other end of the failure retains the admission,
    /// counted: its published producer interval and debited ticket stay held
    /// until the process exits, and no production is abandoned from here.
    fn into_fields(self) -> std::result::Result<(OriginalNativeConnectionAdmission, anyhow::Error), Self> {
        let Self { error, inner } = self;
        match inner.take_whole() {
            Ok(parts) => Ok((parts.original, error)),
            Err(inner) => Err(Self { error, inner }),
        }
    }
}
impl std::fmt::Debug for OriginalNativeConnectionAdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("original admission refused; actual published producer retained")
    }
}
impl std::fmt::Display for OriginalNativeConnectionAdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original admission refused; actual published producer retained: {}", self.error)
    }
}
impl std::error::Error for OriginalNativeConnectionAdmissionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.error.as_ref()) }
}
pub trait OriginalNativeDestinationAuthority: Send + Sync {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()>;
    /// Consumes this exact original domain authority after genuine aggregate
    /// acknowledgement. A refused transfer returns the SAME authority intact.
    fn into_outside_custody(
        self: Arc<Self>, factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal, leaf: &OriginalNativeOutsideLeafEvidence,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<OriginalDestinationOutsideCustody, OriginalDestinationCustodyFailure>;
}
/// Finite mechanical coalescing of actual outside Arc owners. Domain code
/// owns the closed leaf census, dependency order and typed disposal receiver.
/// This inventory never discovers a graph or grants physical/account proof.
pub struct OriginalExactArcAliasInventory<T: ?Sized + Send + Sync + 'static> {
    process: u32,
    identity: Arc<()>,
    maximum: usize,
    closed: bool,
    inner: lillux::custody::Held<OriginalExactArcAliasSlots<T>>,
}
const ARC_ALIAS_INVENTORY_KIND: &str = "lillux-sqlite::arc-alias-inventory";
/// The inventory's slots. Constructible only in this module.
struct OriginalExactArcAliasSlots<T: ?Sized + Send + Sync + 'static> {
    slots: Vec<OriginalExactArcAliasSlot<T>>,
}
/// An inventory that still holds a canonical owner has not handed it to its
/// typed receiver; that owner stays retained, counted, until the process
/// exits. Empty slots hold only identities and release normally.
fn arc_alias_slots_settled<T: ?Sized + Send + Sync + 'static>(parts: &OriginalExactArcAliasSlots<T>) -> bool {
    parts.slots.iter().all(|slot| slot.original.is_none())
}
struct OriginalExactArcAliasSlot<T: ?Sized + Send + Sync + 'static> {
    identity: std::sync::Weak<T>,
    original: Option<Arc<T>>,
    transferred: bool,
}
pub struct OriginalExactArcAliasBinding<T: ?Sized + Send + Sync + 'static> {
    process: u32,
    inventory: Arc<()>,
    index: usize,
    _owner: std::marker::PhantomData<fn() -> Arc<T>>,
}
impl<T: ?Sized + Send + Sync + 'static> OriginalExactArcAliasInventory<T> {
    /// # Safety
    /// The actual original supervisor prepays this finite outside storage;
    /// T is its real ordinary host owner, never an Arc over mapped payload.
    pub unsafe fn issued(maximum: usize, prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<Self> {
        ensure!(maximum > 0, "empty original Arc alias inventory capacity");
        let bytes = maximum.checked_mul(std::mem::size_of::<OriginalExactArcAliasSlot<T>>())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Self>() + 2 * std::mem::size_of::<usize>()))
            .context("original Arc alias inventory quote overflow")?;
        crate::original_physical_custody::retain_callback_refusal(&|| prepay(u64::try_from(bytes)?, u64::try_from(maximum)?))?;
        Ok(Self { process: std::process::id(), identity: Arc::new(()), maximum, closed: false,
            inner: lillux::custody::Held::new(
                ARC_ALIAS_INVENTORY_KIND,
                OriginalExactArcAliasSlots { slots: Vec::with_capacity(maximum) },
                arc_alias_slots_settled::<T>) })
    }
    fn require_process(&self) -> Result<()> {
        ensure!(!crate::foreign_process(self.process), "foreign original Arc alias inventory");
        Ok(())
    }
    fn require_binding(&self, binding: &OriginalExactArcAliasBinding<T>) -> Result<()> {
        self.require_process()?;
        ensure!(binding.process == self.process && Arc::ptr_eq(&binding.inventory, &self.identity)
            && binding.index < self.inner.slots.len(), "Arc alias binding selects another actual inventory");
        Ok(())
    }
    pub fn retain_original(&mut self, original: Arc<T>, prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<OriginalExactArcAliasBinding<T>, (Arc<T>, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_process()?;
            ensure!(!self.closed, "original Arc alias census already closed");
            let work = u64::try_from(self.inner.slots.len())?.checked_add(4).context("original Arc alias scan overflow")?;
            crate::original_physical_custody::retain_callback_refusal(&|| prepay(u64::try_from(std::mem::size_of::<OriginalExactArcAliasBinding<T>>())?, work))
        })();
        if let Err(error) = checked { return Err((original, error)); }
        for (index, slot) in self.inner.slots.iter().enumerate() {
            if slot.original.as_ref().is_some_and(|canonical| Arc::ptr_eq(canonical, &original)) {
                let binding = OriginalExactArcAliasBinding { process: self.process, inventory: self.identity.clone(),
                    index, _owner: std::marker::PhantomData };
                drop(original); // Canonical actual owner already retained; alias only.
                return Ok(binding);
            }
        }
        if self.inner.slots.len() == self.maximum {
            return Err((original, anyhow!("original Arc alias inventory capacity exhausted")));
        }
        let index = self.inner.slots.len();
        self.inner.get_mut().slots.push(OriginalExactArcAliasSlot { identity: Arc::downgrade(&original), original: Some(original), transferred: false });
        Ok(OriginalExactArcAliasBinding { process: self.process, inventory: self.identity.clone(), index,
            _owner: std::marker::PhantomData })
    }
    pub fn require_retained_alias(&self, binding: &OriginalExactArcAliasBinding<T>, original: &Arc<T>) -> Result<()> {
        self.require_binding(binding)?;
        ensure!(self.inner.slots[binding.index].original.as_ref().is_some_and(|canonical| Arc::ptr_eq(canonical, original)),
            "actual canonical Arc owner is not retained for this alias");
        Ok(())
    }
    /// # Safety
    /// Domain supervisor has closed the COMPLETE actual leaf/parent census.
    /// This excludes new aliases only; it is not graph/physical/native proof.
    pub unsafe fn close_census(&mut self) -> Result<()> {
        self.require_process()?; self.closed = true; Ok(())
    }
    /// # Safety
    /// The typed original domain receiver must cache this entire real owner
    /// before every fallible step and preserve it on unwind/refusal. It may
    /// consume it ONCE in its original dependency order, returning the SAME
    /// whole owner here if transfer fails. Dropping it is not disposal proof.
    pub unsafe fn take_original(&mut self, binding: &OriginalExactArcAliasBinding<T>) -> Result<Arc<T>> {
        self.require_binding(binding)?;
        ensure!(self.closed, "actual original Arc alias census still open");
        self.inner.get_mut().slots[binding.index].original.take().context("actual canonical Arc owner already checked out")
    }
    pub fn restore_original(&mut self, binding: &OriginalExactArcAliasBinding<T>, original: Arc<T>)
        -> std::result::Result<(), (Arc<T>, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_binding(binding)?;
            let slot = &self.inner.slots[binding.index];
            ensure!(!slot.transferred && slot.original.is_none()
                && std::sync::Weak::ptr_eq(&slot.identity, &Arc::downgrade(&original)),
                "Arc refusal restoration selects another original owner or disposition");
            Ok(())
        })();
        if let Err(error) = checked { return Err((original, error)); }
        self.inner.get_mut().slots[binding.index].original = Some(original); Ok(())
    }
    /// # Safety
    /// Actual typed domain receiver cached the entire consumed parent and
    /// genuine leaf evidence. No field/count/weak-null observation proves it.
    pub unsafe fn record_actual_transfer(&mut self, binding: &OriginalExactArcAliasBinding<T>) -> Result<()> {
        self.require_binding(binding)?;
        let closed = self.closed;
        let slot = &mut self.inner.get_mut().slots[binding.index];
        ensure!(closed && slot.original.is_none() && !slot.transferred, "actual canonical Arc owner has not transferred or already recorded");
        slot.transferred = true; Ok(())
    }
}
/// Actual retained sources, never reopened paths or inode-equivalence grants.
pub enum OriginalDestinationPhysicalSource {
    Directory(Arc<PinnedDirectory>),
    Namespace(lillux::PinnedDirectoryLock),
}
/// Concrete domain implementations keep the genuine semantic/account graph.
/// This interface never creates native, physical or settlement authority.
/// # Safety
/// Every method must use the SAME original owner/process/account/clock, retain
/// partial effects before refusal, and preserve every real owner on unwind.
/// Returned booleans are observations; actual zero-loan sealing remains mandatory.
pub unsafe trait OriginalOutsideDestinationContinuation: Send + Sync {
    fn require_original_process(&self) -> Result<()>;
    fn retire_native_references(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal, prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()>;
    unsafe fn register_shared_roots(&mut self,
        registry: &mut crate::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()>;
    fn physical_holders_retired(&self) -> Result<bool>;
    /// Upper bound for the owner's terminal holder census, derived from its
    /// retained source cardinality. The outside finalizer includes this work
    /// before it can consume the continuation.
    fn physical_holders_retirement_quote(
        &self,
    ) -> Result<crate::original_physical_custody::OutsideWork>;
}
const DESTINATION_OUTSIDE_CUSTODY_KIND: &str = "lillux-sqlite::destination-outside-custody";
/// The destination's roots, locks, continuation and the join facts its
/// settlement reads. Constructible only in this module.
struct OriginalDestinationOutsideParts {
    sources: Vec<OriginalDestinationPhysicalSource>,
    continuation: Box<dyn OriginalOutsideDestinationContinuation>,
    registrations: Option<Vec<crate::original_physical_custody::OriginalPhysicalRegistration>>,
    roots_registered: bool,
    native_references_retired: bool,
}
/// A destination whose joins and holder retirement are complete holds no live
/// physical source and releases normally: both joins complete, no unregistered
/// source, every registration retired and the typed continuation reporting its
/// holders retired. Otherwise its roots, locks and continuation stay retained,
/// counted, until the process exits. No continuation callback runs in a copy
/// in another process.
fn destination_outside_settled(parts: &OriginalDestinationOutsideParts) -> bool {
    if !(parts.roots_registered && parts.native_references_retired) || !parts.sources.is_empty() {
        return false;
    }
    let Some(registrations) = &parts.registrations else { return false; };
    registrations.iter().all(|registration| registration.retired())
        && parts.continuation.require_original_process().is_ok()
        && matches!(parts.continuation.physical_holders_retired(), Ok(true))
}
pub struct OriginalDestinationOutsideCustody {
    registry_identity: Option<Arc<()>>,
    inner: lillux::custody::Held<OriginalDestinationOutsideParts>,
}
// The source census for the selected normal and failed-start paths allows at
// most four direct destination roots per packet (inspection is the largest
// producer). Keep this owner bound at the transfer boundary so the
// reservation's P(T)/A(T) quote cannot be exceeded by an oversized packet.
pub const MAX_ORIGINAL_DESTINATION_PHYSICAL_SOURCES: usize = 4;
/// Affine permission to consume one exact destination after resident disposal.
/// The original custody remains inside the permit so it cannot be applied to
/// another destination or dropped on a refusal.
pub struct OriginalDestinationRetirement {
    destination: OriginalDestinationOutsideCustody,
}
impl OriginalDestinationOutsideCustody {
    /// # Safety
    /// The actual consuming domain implementation verified SAME factory and
    /// genuine aggregate, charged concrete Vec/Box storage before allocating,
    /// and moved all original roots/locks/semantics into these real inputs.
    /// This constructor grants no cleanup, zero-loan or account proof.
    pub unsafe fn issued(sources: Vec<OriginalDestinationPhysicalSource>,
        continuation: Box<dyn OriginalOutsideDestinationContinuation>) -> Self {
        Self {
            registry_identity: None,
            inner: lillux::custody::Held::new(
                DESTINATION_OUTSIDE_CUSTODY_KIND,
                OriginalDestinationOutsideParts {
                    sources, continuation, registrations: None,
                    roots_registered: false, native_references_retired: false,
                },
                destination_outside_settled,
            ),
        }
    }
    fn require_process(&self) -> Result<()> {
        ensure!(!self.inner.is_foreign(), "foreign original destination outside custody");
        self.inner.continuation.require_original_process()
    }
    pub fn retire_native_references(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal, prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_process()?;
        factory.require_process()?;
        aggregate.require_original(factory)?;
        if self.inner.native_references_retired { return Ok(()); }
        ensure!(self.inner.roots_registered,
            "original destination roots must join before native-reference retirement");
        let continuation = std::cell::RefCell::new(&mut *self.inner.get_mut().continuation);
        crate::original_physical_custody::retain_callback_refusal(&|| {
            continuation.borrow_mut().retire_native_references(factory, aggregate, prepay)
        })?;
        self.inner.get_mut().native_references_retired = true;
        Ok(())
    }
    /// # Safety
    /// SAME complete original supervisor inventory and irreversible prepay.
    pub unsafe fn register_shared_roots(&mut self,
        registry: &mut crate::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_process()?;
        let registered = self.inner.registrations.as_ref().map_or(0, Vec::len);
        let retained_sources = self.inner.sources.len().checked_add(registered)
            .context("original destination source count overflow")?;
        ensure!(retained_sources <= MAX_ORIGINAL_DESTINATION_PHYSICAL_SOURCES,
            "original destination exceeds its selected four-source owner bound");
        let registry_identity = registry.identity_token()?;
        if let Some(original) = &self.registry_identity {
            ensure!(Arc::ptr_eq(original, &registry_identity),
                "original destination registration changed physical registry");
        } else {
            // Bind before any payer call or source move. A refused partial
            // transfer can only retry against this exact registry.
            self.registry_identity = Some(registry_identity);
        }
        if let Some(registrations) = &self.inner.registrations {
            let quote = registry.membership_work_bound(u64::try_from(registrations.len())?, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.require_process()?;
            for registration in registrations {
                registry.require_membership(registration)?;
            }
        }
        if self.inner.registrations.is_none() {
            let count = self.inner.sources.len();
            let bytes = count.checked_mul(std::mem::size_of::<crate::original_physical_custody::OriginalPhysicalRegistration>())
                .context("destination registration storage overflow")?;
            registry.prepay_transfer(prepay, u64::try_from(bytes)?, u64::try_from(count)?.checked_add(1)
                .context("destination registration work overflow")?)?;
            self.inner.get_mut().registrations = Some(Vec::with_capacity(count));
        }
        while let Some(source) = self.inner.get_mut().sources.pop() {
            let result = match source {
                OriginalDestinationPhysicalSource::Directory(owner) => {
                    // SAFETY: actual source of this authentic consumed owner.
                    unsafe { registry.register_directory(owner, prepay) }
                        .map_err(|(owner, error)| (OriginalDestinationPhysicalSource::Directory(owner), error))
                }
                OriginalDestinationPhysicalSource::Namespace(owner) => {
                    unsafe { registry.register_namespace(owner, prepay) }
                        .map_err(|(owner, error)| (OriginalDestinationPhysicalSource::Namespace(owner), error))
                }
            };
            match result {
                Ok(binding) => self.inner.get_mut().registrations.as_mut().expect("prepaid exact registration storage").push(binding),
                Err((owner, error)) => { self.inner.get_mut().sources.push(owner); return Err(error); }
            }
        }
        let continuation = std::cell::RefCell::new(&mut *self.inner.get_mut().continuation);
        let registry = std::cell::RefCell::new(registry);
        let result = crate::original_physical_custody::retain_callback_refusal(&|| {
            // SAFETY: same retained domain owner and actual supervisor registry.
            unsafe { continuation.borrow_mut().register_shared_roots(&mut **registry.borrow_mut(), prepay) }
        });
        if result.is_ok() { self.inner.get_mut().roots_registered = true; }
        result
    }
    pub fn physical_holders_retired(&self) -> Result<bool> {
        self.require_process()?;
        if !self.inner.sources.is_empty() { return Ok(false); }
        let Some(registrations) = &self.inner.registrations else { return Ok(false); };
        for registration in registrations { if !registration.retired() { return Ok(false); } }
        self.inner.continuation.physical_holders_retired()
    }
    /// Capture the exact live terminal while the same physical registry and
    /// destination remain retained. The returned affine owner is the only
    /// authority to consume this destination after resident disposal.
    pub fn prepare_retirement(
        self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
        registry: &crate::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<OriginalDestinationRetirement, (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_process()?;
            factory.require_process()?;
            aggregate.require_original(factory)?;
            ensure!(self.inner.roots_registered && self.inner.native_references_retired,
                "original destination native-reference and registry joins are incomplete");
            ensure!(self.inner.sources.is_empty(),
                "original destination roots have not transferred to the shared registry");
            let registry_identity = self.registry_identity.as_ref()
                .context("original destination registry identity absent")?;
            registry.require_identity_token(registry_identity)?;
            let registrations = self.inner.registrations.as_ref()
                .context("original destination registration inventory absent")?;
            let quote = registry.membership_work_bound(u64::try_from(registrations.len())?, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.require_process()?;
            factory.require_process()?;
            aggregate.require_original(factory)?;
            ensure!(self.inner.roots_registered && self.inner.native_references_retired
                && self.inner.sources.is_empty(),
                "original destination registration changed during permit preparation");
            registry.require_identity_token(self.registry_identity.as_ref()
                .context("original destination registry identity disappeared")?)?;
            for registration in registrations { registry.require_membership(registration)?; }
            Ok(())
        })();
        if let Err(error) = checked { return Err((self, error)); }
        Ok(OriginalDestinationRetirement { destination: self })
    }
}
impl OriginalDestinationRetirement {
    /// Consume this exact destination only after its own same-registry receipts
    /// and typed continuation report physical retirement. This transition has
    /// no factory, aggregate, or resident-region access by construction.
    pub fn into_retired(
        self,
        registry: &crate::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<(), (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            let destination = &self.destination;
            destination.require_process()?;
            ensure!(destination.inner.roots_registered && destination.inner.native_references_retired
                && destination.inner.sources.is_empty(),
                "original destination permit no longer matches its completed joins");
            let registry_identity = destination.registry_identity.as_ref()
                .context("original destination permit lacks registry identity")?;
            registry.require_same_identity_token(registry_identity)?;
            let registrations = destination.inner.registrations.as_ref()
                .context("original destination registration inventory absent")?;
            let direct_checks = u64::try_from(registrations.len())?
                .checked_mul(4)
                .context("original destination retirement work quote overflow")?;
            let continuation = destination.inner.continuation.physical_holders_retirement_quote()?;
            let quote = crate::original_physical_custody::OutsideWork {
                bytes: 0,
                steps: direct_checks.checked_add(4)
                    .context("original destination retirement work quote overflow")?,
            }.checked_add(continuation)?;
            crate::original_physical_custody::retain_callback_refusal(&|| {
                prepay(quote.bytes, quote.steps)
            })?;
            destination.require_process()?;
            registry.require_same_identity_token(destination.registry_identity.as_ref()
                .context("original destination registry identity disappeared")?)?;
            for registration in registrations { registration.receipt()?; }
            ensure!(destination.inner.sources.is_empty(),
                "original destination retains unregistered physical sources");
            ensure!(destination.inner.continuation.physical_holders_retired()?,
                "original destination physical holders remain; custody retained");
            Ok(())
        })();
        if let Err(error) = checked { return Err((self, error)); }

        // The checks above establish the settlement predicate: both joins, an
        // empty source vector, retired registrations and the continuation's
        // positive holder-retirement report. Settling re-checks it and drops
        // the destination; a refusal returns the permit still held.
        let Self { mut destination } = self;
        match destination.inner.settle() {
            Ok(_settled) => Ok(()),
            Err(held) => {
                destination.inner = held;
                Err((Self { destination },
                    anyhow!("original destination settlement refused; custody retained")))
            }
        }
    }
}
const DESTINATION_CUSTODY_FAILURE_KIND: &str = "lillux-sqlite::destination-custody-failure";
/// The factory and destination authority of a refused transfer. Constructible
/// only in this module.
struct DestinationCustodyFailureParts {
    factory: Arc<OriginalNativeFactory>,
    authority: Arc<dyn OriginalNativeDestinationAuthority>,
}
/// Only `into_original_inputs` settles a refused transfer. The factory and
/// destination authority stay retained, counted, until the process exits
/// rather than end outside their original domain owner.
pub struct OriginalDestinationCustodyFailure {
    error: anyhow::Error,
    inner: lillux::custody::Held<DestinationCustodyFailureParts>,
}
impl OriginalDestinationCustodyFailure {
    pub fn retain(factory: Arc<OriginalNativeFactory>, authority: Arc<dyn OriginalNativeDestinationAuthority>, error: anyhow::Error) -> Self {
        Self {
            error,
            inner: lillux::custody::Held::until_taken(
                DESTINATION_CUSTODY_FAILURE_KIND,
                DestinationCustodyFailureParts { factory, authority }),
        }
    }
    pub fn into_original_inputs(self) -> std::result::Result<(Arc<OriginalNativeFactory>, Arc<dyn OriginalNativeDestinationAuthority>, anyhow::Error), Self> {
        if self.inner.factory.require_process().is_err() { return Err(self); }
        let Self { error, inner } = self;
        match inner.take_whole() {
            Ok(parts) => Ok((parts.factory, parts.authority, error)),
            Err(inner) => Err(Self { error, inner }),
        }
    }
    pub fn error(&self) -> &anyhow::Error { &self.error }
}
impl std::fmt::Debug for OriginalDestinationCustodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("actual destination authority retained") }
}
impl std::fmt::Display for OriginalDestinationCustodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "actual destination authority retained: {}", self.error) }
}
impl std::error::Error for OriginalDestinationCustodyFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.error.as_ref()) }
}
/// Host keeps the actual sealed original producer and authenticated intake
/// authority. This does not supply domain mutation authority by itself.
pub trait OriginalNativeCleanupAuthority: Send + Sync {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()>;
}
#[derive(Clone)]
pub struct OriginalNativeCleanupInputs(Arc<OriginalNativeCleanupInputBody>);
struct OriginalNativeCleanupInputBody {
    terminal: OriginalNativeConnectionTerminal,
    authority: Arc<dyn OriginalNativeCleanupAuthority>,
}
impl OriginalNativeCleanupInputs {
    /// # Safety
    /// Genuine SAME original sealed producer/work/intake and exact actual
    /// Closed/Released must be retained. No enum/path/count substitute.
    pub unsafe fn issued(
        terminal: OriginalNativeConnectionTerminal,
        authority: Arc<dyn OriginalNativeCleanupAuthority>,
    ) -> Result<Self> {
        require_current_native_process()?;
        let factory = terminal.original().0;
        terminal.require_original(factory)?;
        authority.require_original(factory)?;
        Ok(Self(Arc::new(OriginalNativeCleanupInputBody {
            terminal,
            authority,
        })))
    }
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        require_current_native_process()?;
        self.0.terminal.require_original(factory)?;
        self.0.authority.require_original(factory)
    }
    pub fn terminal(&self) -> &OriginalNativeConnectionTerminal {
        &self.0.terminal
    }
}
/// Actual host-sealed and supervised live-node drain, independent of any
/// connection terminal. Only the trusted host producer can issue this input.
#[derive(Clone)]
pub struct OriginalLiveNativeDrainInputs(Arc<OriginalLiveNativeDrainBody>);
struct OriginalLiveNativeDrainBody {
    factory: Arc<OriginalNativeFactory>,
    authority: Arc<dyn OriginalNativeCleanupAuthority>,
}
impl OriginalLiveNativeDrainInputs {
    /// # Safety
    /// The actual live issuer must retain its genuine irreversible producer
    /// seal, supervised drain, original prepaid reservation/clock and SAME
    /// factory. No decoded grant, enum, counter or empty list supplies this
    /// proof. Its authority must revalidate that actual retained host owner.
    pub unsafe fn issued(
        factory: Arc<OriginalNativeFactory>,
        authority: Arc<dyn OriginalNativeCleanupAuthority>,
    ) -> Result<Self> {
        require_current_native_process()?;
        authority.require_original(&factory)?;
        Ok(Self(Arc::new(OriginalLiveNativeDrainBody {
            factory,
            authority,
        })))
    }
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        require_current_native_process()?;
        ensure!(
            Arc::ptr_eq(&self.0.factory, factory),
            "live drain selects foreign factory"
        );
        self.0.authority.require_original(factory)
    }
}
#[derive(Clone, Copy)]
enum OriginalNativeFinishIntake<'a> {
    Offline(&'a OriginalNativeCleanupInputs),
    Live(&'a OriginalLiveNativeDrainInputs),
    InitializationRefusal,
}

/// Borrow exists only within the original factory's lexical cleanup scope.
/// It cannot be constructed, sent to another thread, or retained as authority.
pub struct OriginalNativeCleanupScope<'a> {
    factory: &'a Arc<OriginalNativeFactory>,
    inputs: &'a OriginalNativeCleanupInputs,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl OriginalNativeCleanupScope<'_> {
    pub fn checkpoint(&self) -> Result<()> {
        require_current_native_process()?;
        self.factory.require_original(true)?;
        self.inputs.require_original(self.factory)
    }
}
/// Fixed cleanup steps the native layout adds to every cleanup quote, beyond
/// its per-block, per-ticket and per-map terms.
pub const NATIVE_LAYOUT_CLEANUP_MARGIN_STEPS: u64 = 64;
/// Headroom the layout's fixed margin sets aside for one stopped intake's
/// cleanup steps. It is a sizing figure, not a runtime counter: the stopped
/// intake runs in a cleanup allocator scope, so its allocations are charged to
/// the cleanup reserve, only its stopped create allocates, and retries of the
/// held owner charge nothing more. No cleanup quote adds this charge: it rides
/// the layout's fixed margin, so it must stay within that margin (checked at
/// compile time) and a measured intake must stay within it (checked by a test).
/// The runtime refusal is the cleanup reserve itself, which an intake cannot
/// exceed.
pub const STOPPED_INTAKE_CLEANUP_STEPS: u64 = 32;
const _: () = assert!(STOPPED_INTAKE_CLEANUP_STEPS <= NATIVE_LAYOUT_CLEANUP_MARGIN_STEPS);
/// Borrow present only inside one lexical stopped-intake scope of an
/// original factory: a cleanup allocator scope with no open ticket, entered
/// while the service window is open. It cannot be constructed outside this
/// module, sent to another thread, or retained past its callback.
pub struct OriginalNativeStoppedIntakeScope<'a> {
    _factory: &'a OriginalNativeFactory,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}
/// Borrow present only inside one lexical service allocator scope of an
/// original factory, entered while the service window is open. It cannot be
/// constructed outside this module, sent to another thread, or retained past
/// its callback.
pub struct OriginalNativeServiceScope<'a> {
    _factory: &'a OriginalNativeFactory,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}
/// Refusal of a stopped-intake or service scope. A refusal after the
/// callback ran carries the callback's value, so an owner it produced or
/// handed back stays with the caller; a refusal before it ran carries none.
pub struct OriginalNativeScopeFailure<T> {
    error: anyhow::Error,
    completed: Option<T>,
}
impl<T> OriginalNativeScopeFailure<T> {
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn completed_owner(&mut self) -> Option<T> {
        self.completed.take()
    }
    pub fn into_parts(self) -> (anyhow::Error, Option<T>) {
        (self.error, self.completed)
    }
}
impl<T> std::fmt::Debug for OriginalNativeScopeFailure<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeScopeFailure")
            .field("error", &self.error)
            .field("completed", &self.completed.is_some())
            .finish()
    }
}
impl<T> std::fmt::Display for OriginalNativeScopeFailure<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl<T> std::error::Error for OriginalNativeScopeFailure<T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
/// Callback success may precede a late refusal. Preserve its genuine owner
/// outside any bookkeeping lock, alongside the same factory/terminal/seal.
pub struct OriginalNativeCleanupFailure<T> {
    error: anyhow::Error,
    completed: Mutex<Option<T>>,
    factory: Arc<OriginalNativeFactory>,
    inputs: OriginalNativeCleanupInputs,
}
impl<T> OriginalNativeCleanupFailure<T> {
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn factory(&self) -> &Arc<OriginalNativeFactory> {
        &self.factory
    }
    pub fn inputs(&self) -> OriginalNativeCleanupInputs {
        self.inputs.clone()
    }
    pub fn completed_owner(&mut self) -> Option<T> {
        self.completed
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
    pub fn into_parts(self) -> (anyhow::Error, Option<T>, OriginalNativeCleanupInputs) {
        (
            self.error,
            self.completed
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            self.inputs,
        )
    }
}
impl<T> std::fmt::Debug for OriginalNativeCleanupFailure<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeCleanupFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl<T> std::fmt::Display for OriginalNativeCleanupFailure<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl<T> std::error::Error for OriginalNativeCleanupFailure<T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
pub struct OriginalNativeTransientInputs {
    process: u32,
    domain: OriginalNativeDomain,
    authority: Arc<dyn OriginalNativeDestinationAuthority>,
}
impl OriginalNativeTransientInputs {
    /// Purpose metadata only; this does not issue admission or a ticket.
    pub fn domain(&self) -> OriginalNativeDomain { self.domain }

    /// Consume an unused one-shot schema input only in its issuing process.
    /// This supplies no native terminal and no replacement admission.
    pub fn into_original_parts(
        self,
    ) -> std::result::Result<(OriginalNativeDomain, Arc<dyn OriginalNativeDestinationAuthority>), Self> {
        if crate::foreign_process(self.process) { return Err(self); }
        let Self { process: _, domain, authority } = self;
        Ok((domain, authority))
    }

    /// # Safety
    /// NodeRuntimeSchemaReference must retain the genuine original Runtime
    /// namespace owner, initialized paid factory and original funding, node
    /// exclusion, and actual Runtime signer/trust/process/clock owners. It may
    /// build only declared Runtime schema and fixed bootstrap seed rows; it
    /// grants no State/CAS data or mutation authority. StateTransient,
    /// NodeProjectionSchemaReference, and every Operational schema reference
    /// must retain actual prepared State CAS,
    /// refs, locators, recovery, trust, and namespace custody.
    /// NodeSchedulerSchemaReference must retain the genuine scheduler startup
    /// or service namespace exclusion, same initialized paid factory, original
    /// funding and node/process/clock custody. It may build only the declared
    /// scheduler schema; it grants no scheduler rows or retirement authority.
    /// NodeAccountingSchemaReference must retain the genuine live State
    /// namespace owner, same initialized paid factory, original funding and
    /// node/process/clock custody. It may build only the declared ledger
    /// schema; it grants no ledger rows or admission authority.
    /// ExternalSupervisorJournalSchemaReference and
    /// ExternalGuestJournalSchemaReference must retain the supervisor's
    /// pinned journal directory, its occurrence-funded host factory and the
    /// occurrence process; each may build only its declared journal schema.
    /// No domain enum or path DTO may substitute for these genuine owners.
    pub unsafe fn issued(
        domain: OriginalNativeDomain,
        authority: Arc<dyn OriginalNativeDestinationAuthority>,
    ) -> Result<Self> {
        require_current_native_process()?;
        ensure!(
            matches!(
                domain,
                OriginalNativeDomain::StateTransient
                    | OriginalNativeDomain::OperationalSyncJobSchemaReference
                    | OriginalNativeDomain::OperationalReplaySchemaReference
                    | OriginalNativeDomain::NodeRuntimeSchemaReference
                    | OriginalNativeDomain::NodeOperationalSchemaReference
                    | OriginalNativeDomain::NodeProjectionSchemaReference
                    | OriginalNativeDomain::NodeSchedulerSchemaReference
                    | OriginalNativeDomain::NodeAccountingSchemaReference
                    | OriginalNativeDomain::ExternalSupervisorJournalSchemaReference
                    | OriginalNativeDomain::ExternalGuestJournalSchemaReference
            ),
            "invalid transient native domain"
        );
        Ok(Self { process: std::process::id(), domain, authority })
    }
}
/// The single destination input a native connection was opened with.
pub enum OriginalNativeDestinationInputs {
    Bound(OriginalNativeBoundInputs),
    Transient(Arc<dyn OriginalNativeDestinationAuthority>),
}
/// Fixed work of one `take_destination_inputs` capture.
pub const ORIGINAL_DESTINATION_INPUT_TAKE_STEPS: u64 = 4;
impl OriginalNativeDestinationInputs {
    pub fn authority(&self) -> &Arc<dyn OriginalNativeDestinationAuthority> {
        match self {
            Self::Bound(inputs) => &inputs.authority,
            Self::Transient(authority) => authority,
        }
    }
    /// Purpose metadata of Bound inputs; transient inputs carry none.
    pub fn bound_domain(&self) -> Option<OriginalNativeDomain> {
        match self {
            Self::Bound(inputs) => Some(inputs.domain),
            Self::Transient(_) => None,
        }
    }
    fn into_fields(this: Option<Self>)
        -> (Option<OriginalNativeBoundInputs>, Option<Arc<dyn OriginalNativeDestinationAuthority>>) {
        match this {
            Some(Self::Bound(inputs)) => (Some(inputs), None),
            Some(Self::Transient(authority)) => (None, Some(authority)),
            None => (None, None),
        }
    }
}
/// Weak authority identity cached when a receipt is built, so evidence stays
/// stable after a take-capture moves the inputs out.
fn destination_authority_witness(destination: &Option<OriginalNativeDestinationInputs>)
    -> Option<std::sync::Weak<dyn OriginalNativeDestinationAuthority>> {
    destination.as_ref().map(|inputs| Arc::downgrade(inputs.authority()))
}
/// Poisoning is recovered: the slot holds plain owners with no invariant a
/// panic could break. Only contention refuses.
fn lock_destination(slot: &Mutex<Option<OriginalNativeDestinationInputs>>)
    -> Result<std::sync::MutexGuard<'_, Option<OriginalNativeDestinationInputs>>> {
    match slot.try_lock() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) =>
            anyhow::bail!("original destination inputs are busy"),
    }
}
pub struct OriginalNativeBoundInputs {
    process: u32,
    root: Arc<PinnedDirectory>,
    name: crate::binding::BoundComponent,
    mode: OriginalNativeBoundMode,
    domain: OriginalNativeDomain,
    authority: Arc<dyn OriginalNativeDestinationAuthority>,
    inspection: Option<(SqliteCopyLease, Arc<dyn SqliteInspectionSqlWork>)>,
}
pub struct OriginalNativeOutsideBoundCoordinate(crate::binding::BoundComponent);
impl OriginalNativeOutsideBoundCoordinate {
    pub fn as_os_str(&self) -> &OsStr { self.0.as_os_str() }
}
pub struct OriginalNativeOutsideBoundParts {
    pub root: Arc<PinnedDirectory>,
    pub name: OriginalNativeOutsideBoundCoordinate,
    pub mode: OriginalNativeBoundMode,
    pub domain: OriginalNativeDomain,
    pub authority: Arc<dyn OriginalNativeDestinationAuthority>,
    pub inspection: Option<(SqliteCopyLease, Arc<dyn SqliteInspectionSqlWork>)>,
}
impl OriginalNativeBoundInputs {
    /// Move the exact resource/coordinate owners; this supplies no new grant.
    pub fn into_original_parts(self) -> std::result::Result<OriginalNativeOutsideBoundParts, Self> {
        if crate::foreign_process(self.process) { return Err(self); }
        let Self { process: _, root, name, mode, domain, authority, inspection } = self;
        Ok(OriginalNativeOutsideBoundParts { root, name: OriginalNativeOutsideBoundCoordinate(name), mode, domain, authority, inspection })
    }

    /// Fixed scheduler role check before opening; no descriptor/authority escapes.
    pub fn require_scheduler_coordinate(&self) -> Result<()> {
        ensure!(self.domain == OriginalNativeDomain::NodeScheduler
            && self.name.as_os_str() == std::ffi::OsStr::new("scheduler.sqlite3"),
            "scheduler admission selects another native coordinate");
        Ok(())
    }

    /// Validated mode/domain metadata for a caller check BEFORE native open.
    /// This does not expose a descriptor or issue any authority.
    pub fn domain(&self) -> OriginalNativeDomain {
        self.domain
    }
    /// # Safety
    /// This must be the actual original admitted State/Runtime/scheduler owner,
    /// retaining its real namespace/mutation/role authority, not reconstructed
    /// paths or a decoded identity. The callback revalidates that owner.
    pub unsafe fn issued(
        root: Arc<PinnedDirectory>,
        name: &OsStr,
        mode: OriginalNativeBoundMode,
        domain: OriginalNativeDomain,
        authority: Arc<dyn OriginalNativeDestinationAuthority>,
        inspection: Option<(SqliteCopyLease, Arc<dyn SqliteInspectionSqlWork>)>,
    ) -> Result<Self> {
        require_current_native_process()?;
        ensure!(
            name.to_str()
                .is_some_and(|n| !n.is_empty() && !n.contains('/') && n != "." && n != ".."),
            "invalid original native database coordinate"
        );
        ensure!(
            (mode == OriginalNativeBoundMode::ReadonlyInspection) == inspection.is_some(),
            "readonly original inputs require actual SQL/lease handoff"
        );
        use OriginalNativeBoundMode as Mode;
        use OriginalNativeDomain as Domain;
        let live_store = matches!(
            domain,
            Domain::NodeRuntime
                | Domain::NodeOperational
                | Domain::NodeProjection
                | Domain::NodeScheduler
                | Domain::NodeAccounting
        );
        if live_store {
            ensure!(
                matches!(
                    mode,
                    Mode::Existing | Mode::CreateExclusive | Mode::OpenOrCreate
                ) && inspection.is_none(),
                "live store domain/mode conflict"
            );
        } else if domain == Domain::NodeProjectionInspection {
            ensure!(
                mode == Mode::ReadonlySource && inspection.is_none(),
                "live projection inspection requires readonly source"
            );
        } else {
            ensure!(
                !matches!(
                    domain,
                    Domain::StateTransient
                        | Domain::OperationalSyncJobSchemaReference
                        | Domain::OperationalReplaySchemaReference
                        | Domain::NodeRuntimeSchemaReference
                        | Domain::NodeOperationalSchemaReference
                        | Domain::NodeProjectionSchemaReference
                        | Domain::NodeSchedulerSchemaReference
                        | Domain::NodeAccountingSchemaReference
                        | Domain::ExternalSupervisorJournalSchemaReference
                        | Domain::ExternalGuestJournalSchemaReference
                ),
                "transient domain cannot acquire a bound store"
            );
            ensure!(
                mode != Mode::OpenOrCreate,
                "open-or-create requires genuine live store inputs"
            );
            ensure!(
                matches!(
                    domain,
                    OriginalNativeDomain::RuntimeInspection
                        | OriginalNativeDomain::SchedulerInspection
                ) == (mode == OriginalNativeBoundMode::ReadonlyInspection),
                "native domain/mode conflict"
            );
            ensure!(
                (domain == OriginalNativeDomain::OperationalInspection)
                    == (mode == OriginalNativeBoundMode::ReadonlySource),
                "Operational preview requires readonly original source"
            );
            ensure!(
                domain != OriginalNativeDomain::OperationalReplayReset
                    || mode == OriginalNativeBoundMode::Existing,
                "Operational replay reset requires its established original source"
            );
        }
        Ok(Self {
            process: std::process::id(),
            root,
            name: crate::binding::BoundComponent::new(name).map_err(anyhow::Error::msg)?,
            mode,
            domain,
            authority,
            inspection,
        })
    }
}

unsafe extern "C" {
    fn ryeos_native_process_matches() -> c_int;
    fn ryeos_native_layout(storage: u64, tickets: u32, cleanup: *mut u64) -> c_int;
    fn ryeos_native_arena_create(
        budget: *const OriginalNativeBudget,
        out: *mut *mut c_void,
    ) -> c_int;
    fn ryeos_native_scope_enter(arena: *mut c_void, cleanup: c_int, open_ticket: u32) -> c_int;
    fn ryeos_native_scope_leave(arena: *mut c_void) -> c_int;
    fn ryeos_native_arena_ready(arena: *mut c_void) -> c_int;
    fn ryeos_native_connection_enroll(
        arena: *mut c_void,
        db: *mut libsqlite3_sys::sqlite3,
        ticket: u32,
    ) -> c_int;
    fn ryeos_native_connection_closed(arena: *mut c_void, ticket: u32) -> c_int;
    fn ryeos_native_commit_verifier_install(
        arena: *mut c_void,
        verifier: extern "C" fn(std::ffi::c_uint) -> c_int,
    ) -> c_int;
    fn ryeos_native_arena_reclaim(arena: *mut c_void) -> c_int;
    fn ryeos_native_arena_release(arena: *mut c_void) -> c_int;
}
#[cfg(test)]
unsafe extern "C" {
    fn ryeos_native_shared_census(arena: *mut c_void, bytes: *mut u64, slots: *mut u32) -> c_int;
}
struct NativeArena(NonNull<c_void>, u32);
// Native global ownership and bounded native synchronization protect arena.
unsafe impl Send for NativeArena {}
unsafe impl Sync for NativeArena {}
impl NativeArena {
    /// Enter one allocator scope. Entry waits out other threads' admission
    /// on the native owner mutex; a non-OK code is a fact of this arena or
    /// this thread (poisoned, released, ticket or depth limit, a nested scope
    /// that does not match the open one, entry from inside the commit gate),
    /// never momentary contention.
    fn scope(&self, cleanup: bool, ticket: u32) -> Result<NativeScope<'_>> {
        require_current_native_process()?;
        ensure!(!crate::foreign_process(self.1), "inherited native arena");
        let code = unsafe { ryeos_native_scope_enter(self.0.as_ptr(), cleanup as c_int, ticket) };
        ensure!(
            code == libsqlite3_sys::SQLITE_OK,
            "original native scope entry refused (sqlite code {code})"
        );
        Ok(NativeScope(self))
    }
}
// An unverified mapping is never unmapped by Drop. The original factory/error
// retains it; only supervised terminal calls the actual native release seam.
struct NativeScope<'a>(&'a NativeArena);
impl Drop for NativeScope<'_> {
    fn drop(&mut self) {
        if crate::foreign_process(self.0.1) || unsafe { ryeos_native_process_matches() } != 1 {
            return;
        }
        unsafe {
            ryeos_native_scope_leave(self.0.0.as_ptr());
        }
    }
}

// One nonblocking effect gate serializes bootstrap/open admission/finalization.
// Native scopes retain their separate exact C activity census for SQL/close.
struct NativeEffectGuard<'a>(&'a AtomicBool, u32);
impl Drop for NativeEffectGuard<'_> {
    fn drop(&mut self) {
        if !crate::foreign_process(self.1) && require_current_native_process().is_ok() {
            self.0.store(false, Ordering::Release);
        }
    }
}
pub struct OriginalNativeFactory {
    original_process: u32,
    region: OriginalNativeRegionHandle,
    authority: Arc<dyn OriginalNativeAdmissionAuthority>,
    budget: Arc<OriginalNativeBudgetOwner>,
    producer: Mutex<Option<SqliteInspectionProductionPermit>>,
    budget_c_lease: OnceLock<OriginalNativeRegionCBorrow>,
    // Heap-owned by this facade, never resident in the mapped region.
    commit_table: Arc<NativeCommitTable>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundaryMode {
    Service,
    CommitGate,
    Cleanup,
}

/// Boundary inputs of one enrolled connection, read by the compiled commit
/// gate. Only weak or copied references: no strong factory, destination or
/// producer owner and no counted region handle, so closing every connection
/// returns the factory to its sole owner.
struct NativeCommitSlot {
    factory: Weak<OriginalNativeFactory>,
    destination: Weak<dyn OriginalNativeDestinationAuthority>,
    control: Weak<dyn crate::SqliteInspectionProductionControl>,
    cell: Option<OriginalNativeProducerCellIndex>,
    refused: Arc<AtomicBool>,
}
struct NativeCommitTable {
    // Tickets are never reused, so each slot is written once.
    slots: [OnceLock<NativeCommitSlot>; MAX_NATIVE_CONNECTIONS],
}
// The arena is a process singleton, so one table is current at a time. Set and
// cleared together with the C verifier pointer; holds only a weak reference.
static NATIVE_COMMIT_TABLE: std::sync::RwLock<Option<Weak<NativeCommitTable>>> =
    std::sync::RwLock::new(None);

extern "C" fn native_commit_verifier(ticket: std::ffi::c_uint) -> c_int {
    std::panic::catch_unwind(|| verify_native_commit(ticket).is_ok())
        .map_or(0, |verified| c_int::from(verified))
}
fn verify_native_commit(ticket: u32) -> Result<()> {
    let table = NATIVE_COMMIT_TABLE
        .read()
        .map_err(|_| anyhow!("native commit table poisoned"))?
        .as_ref()
        .and_then(Weak::upgrade)
        .context("native commit table absent")?;
    let index = usize::try_from(ticket)?.checked_sub(1).context("native commit ticket zero")?;
    let slot = table.slots.get(index).and_then(OnceLock::get)
        .context("native commit on an unenrolled ticket")?;
    let verified = match slot.factory.upgrade() {
        Some(factory) => factory.boundary(BoundaryMode::CommitGate, || {
            if let Some(cell) = slot.cell {
                producer_cell_checkpoint(&factory.region, cell)?;
            }
            slot.control.upgrade().context("native commit producer owner vanished")?.checkpoint()?;
            slot.destination
                .upgrade()
                .context("native commit destination owner vanished")?
                .require_original(&factory)
        }),
        None => Err(anyhow!("native commit factory owner vanished")),
    };
    if verified.is_err() {
        slot.refused.store(true, Ordering::SeqCst);
    }
    verified
}
/// No authority wrappers or region-owning backedges live inside the mapping.
/// Fixed kernel control state and actual terminal observation are resident.
pub(crate) struct NativeFactoryStorage {
    installed: AtomicBool, arena: OnceLock<NativeArena>, arena_entered: AtomicBool, arena_attempt: AtomicU8,
    initialized: AtomicBool, ready_acknowledged: AtomicBool, shutdown_completed: AtomicBool,
    reclaim_completed: AtomicBool, arena_released: AtomicBool, finishing: AtomicBool,
    terminal: Mutex<Option<Weak<OriginalNativeTerminal>>>,
    terminal_body: OnceLock<OriginalNativeTerminalBody>, finalizer: AtomicBool,
    tickets: [AtomicU8; MAX_NATIVE_CONNECTIONS],
}
impl NativeFactoryStorage {
    fn new() -> Self { Self { installed: AtomicBool::new(false), arena: OnceLock::new(), arena_entered: AtomicBool::new(false),
        arena_attempt: AtomicU8::new(0), initialized: AtomicBool::new(false), ready_acknowledged: AtomicBool::new(false),
        shutdown_completed: AtomicBool::new(false), reclaim_completed: AtomicBool::new(false), arena_released: AtomicBool::new(false), finishing: AtomicBool::new(false),
        terminal: Mutex::new(None), terminal_body: OnceLock::new(), finalizer: AtomicBool::new(false),
        tickets: std::array::from_fn(|_| AtomicU8::new(0)) } }
}
impl std::fmt::Debug for OriginalNativeFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeFactory")
            .finish_non_exhaustive()
    }
}
impl OriginalNativeFactory {
    fn storage(&self) -> Result<&NativeFactoryStorage> {
        self.region.require_original_process()?;
        // All returned private borrows are tied to this facade's live counted
        // region handle. An ordinary Arc cannot drop its value while borrowed.
        Ok(&self.region.state_ref()?.factory)
    }
    /// # Safety
    /// Only the genuine signed original host issuer may call this. It must
    /// supply its exact prepaid shared budget and actual native producer permit;
    /// installing this object is not signature verification or mutation power.
    pub unsafe fn install_from_original_issuer(
        region: OriginalNativeRegionHandle,
        authority: Arc<dyn OriginalNativeAdmissionAuthority>, producer: SqliteInspectionProductionPermit,
    ) -> std::result::Result<Arc<Self>, OriginalNativeFactoryFailure> {
        let budget = authority.budget();
        let factory = Arc::new(Self { original_process: std::process::id(), region, authority, budget,
            producer: Mutex::new(Some(producer)), budget_c_lease: OnceLock::new(),
            commit_table: Arc::new(NativeCommitTable { slots: std::array::from_fn(|_| OnceLock::new()) }) });
        let result = (|| -> Result<()> {
            factory.require_original(false)?;
            ensure!(factory.budget.same_region(&factory.region)?, "foreign resident budget region");
            factory.region.claim_factory_install()?;
            producer_kind_requires_region(&factory)?;
            factory.budget_c_lease.set(factory.region.borrow_c()?)
                .map_err(|_| anyhow::anyhow!("native budget C loan already published"))?;
            Ok(())
        })();
        result.map_err(|error| factory.retain_error(error))?;
        Ok(factory)
    }
    /// Original resident budget limit, for bounded pre-effect inventories.
    /// This copied observation is not resource admission or a new ticket.
    pub fn maximum_resource_tickets(&self) -> Result<u32> {
        self.require_process()?;
        let borrow = self.region.borrow(true)?;
        Ok(borrow.state()?.budget.connections_limit)
    }
    /// No allocation/SQLite initialization: authentic caller uses this exact
    /// bounded layout to reserve cleanup BEFORE the original arena effect.
    pub fn required_cleanup_steps(storage: u64, tickets: u32) -> Result<u64> {
        require_current_native_process()?;
        let mut cleanup = 0;
        ensure!(
            unsafe { ryeos_native_layout(storage, tickets, &mut cleanup) }
                == libsqlite3_sys::SQLITE_OK,
            "unsupported original native arena layout"
        );
        Ok(cleanup)
    }
    /// Dispose the actual never-delivered prepared metadata input without
    /// admission, acquisition or a ticket. The same concrete close path checks
    /// original cleanup authority, physically consumes the exact pin and
    /// retains an accessible owner on every refusal.
    pub fn release_unacquired_projection_lease_inputs(self: &Arc<Self>, inputs: OriginalProjectionLeaseInputs)
        -> std::result::Result<OriginalProjectionLeaseDisposition, OriginalProjectionLeaseFailure> {
        let owner = OriginalProjectionLease::new(self.clone(), inputs);
        owner.close()
    }
    pub fn acquire_projection_lease(
        self: &Arc<Self>, inputs: OriginalProjectionLeaseInputs,
    ) -> std::result::Result<OriginalProjectionLease, OriginalProjectionLeaseFailure> {
        let mut owner = OriginalProjectionLease::new(self.clone(), inputs);
        let result = (|| -> Result<()> {
            self.require_ready()?;
            owner.inner.inputs.authority.require_original(self, false)?;
            let _effect = self.enter_effect()?;
            #[cfg(not(target_os = "linux"))]
            anyhow::bail!("projection leases require Linux");
            #[cfg(target_os = "linux")]
            {
                use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
                let flags = unsafe { libc::fcntl(owner.inner.inputs.metadata.as_raw_fd(), libc::F_GETFL) };
                ensure!(flags >= 0 && flags & libc::O_PATH != 0,
                    "projection lease requires actual O_PATH metadata");
                let observed = owner.inner.inputs.metadata.metadata()?;
                ensure!(observed.is_file(), "projection lease metadata is not regular");
                owner.inner.get_mut().inputs.identity = (observed.dev(), observed.ino());
                owner.inner.inputs.root.ensure_path_binding()?;
                owner.inner.inputs.root.ensure_open_regular_same_mount(&owner.inner.inputs.metadata)?;
            }
            let admission = match self.authority.begin_projection_lease() {
                Ok(admission) => admission,
                Err(error) => match error.downcast::<OriginalNativeProjectionLeaseAdmissionFailure>() {
                    Ok(failure) => {
                        owner.inner.get_mut().admission_failure = Some(failure);
                        anyhow::bail!("actual published lease admission retained; validation recovery required");
                    }
                    Err(error) => return Err(error),
                },
            };
            owner.ticket = Some(admission.ticket);
            owner.inner.get_mut().producer = Some(admission.producer);
            owner.inner.producer.as_ref().context("lease producer missing")?.require_original_region(&self.region)?;
            ensure!(admission.budget.same_original(&self.budget), "projection lease selects foreign budget");
            let ticket = owner.ticket.expect("actual issued lease ticket");
            ensure!(ticket > 0 && ticket as usize <= MAX_NATIVE_CONNECTIONS
                && self.storage()?.tickets[ticket as usize - 1].compare_exchange(0, 1,
                    Ordering::AcqRel, Ordering::Acquire).is_ok(), "projection lease ticket is foreign/reused");
            owner.ticket_registered = true;
            self.require_ready()?;
            owner.inner.inputs.authority.require_original(self, false)?;
            owner.inner.inputs.root.ensure_path_binding()?;
            owner.inner.inputs.root.ensure_open_regular_same_mount(&owner.inner.inputs.metadata)?;
            let _scope = self.arena()?.scope(false, ticket)?;
            owner.inner.producer.as_ref().context("lease producer missing")?.checkpoint()?;
            use std::os::fd::AsRawFd;
            let mode = match owner.inner.inputs.mode { OriginalProjectionLeaseMode::Shared => 1,
                OriginalProjectionLeaseMode::Exclusive => 2 };
            let deadline = owner.inner.inputs.authority.fork_deadline(self, false)?;
            let fork_guard = lillux::exec::retain_fork_sensitive_descriptors_until(deadline)
                .context("projection lease original service fork deadline refused")?;
            ensure!(!deadline.has_elapsed(), "projection lease original service deadline elapsed");
            self.require_ready()?;
            owner.inner.inputs.authority.require_original(self, false)?;
            let reservation = lillux::exec::reserve_borrowed_fork_child_close_descriptor(&fork_guard)
                .map_err(|error| anyhow::anyhow!(error))?;
            owner.inner.get_mut().fork_reservation = Some(reservation);
            let (code, actual, actual_fd) = crate::binding::ProjectionLease::open(owner.inner.inputs.metadata.as_raw_fd(), mode);
            owner.inner.get_mut().native = actual;
            let reservation = owner.inner.get_mut().fork_reservation.take().expect("actual pre-effect fork reservation");
            if let Some(actual_fd) = actual_fd {
                match unsafe { reservation.bind(&fork_guard, actual_fd) } {
                    Ok(registration) => owner.inner.get_mut().fork_registration = Some(registration),
                    Err(reservation) => {
                        owner.inner.get_mut().fork_reservation = Some(reservation);
                        owner.fork_invariant_refused = true;
                        // Never release fork exclusion around an unregistered real FD.
                        // This invariant-only refusal remains held until restart/recovery.
                        fork_guard.hold_until_process_exit();
                        anyhow::bail!("lease acquired-FD fork enrollment invariant refused");
                    }
                }
            } else {
                if let Err(reservation) = reservation.cancel(&fork_guard) {
                    owner.inner.get_mut().fork_reservation = Some(reservation);
                    owner.fork_invariant_refused = true;
                    fork_guard.hold_until_process_exit();
                    anyhow::bail!("lease unused fork reservation invariant refused");
                }
            }
            // Every actual partial FD is enrolled before status or late host checks.
            ensure!(code == libsqlite3_sys::SQLITE_OK && owner.inner.native.is_some(),
                "projection lease acquisition/flock refused ({code})");
            self.require_ready()?;
            owner.inner.inputs.authority.require_original(self, false)
        })();
        match result {
            Ok(()) => Ok(owner),
            Err(error) => Err(OriginalProjectionLeaseFailure {
                error,
                inner: lillux::custody::Held::until_taken(
                    PROJECTION_LEASE_FAILURE_KIND, ProjectionLeaseFailureParts { owner: Mutex::new(owner) }),
            }),
        }
    }
    pub fn retained_terminal(self: &Arc<Self>) -> Option<Arc<OriginalNativeTerminal>> {
        if self.require_process().is_err() {
            return None;
        }
        self.storage().ok()?.terminal_body.get().map(|body| {
            Arc::new(OriginalNativeTerminal {
                factory: self.clone(),
                body: NonNull::from(body),
            })
        })
    }
    pub fn same_original(self: &Arc<Self>, other: &Arc<Self>) -> bool {
        Arc::ptr_eq(self, other)
    }
    /// Check actual bootstrap and issuer handoff while original service remains admitted.
    /// Whether the issuer acknowledged Ready. Observation only: it selects
    /// the matching finish (drain after Ready, initialization refusal before
    /// it) and grants no service.
    pub fn ready_acknowledged(&self) -> Result<bool> {
        require_current_native_process()?;
        Ok(self.storage()?.ready_acknowledged.load(Ordering::Acquire))
    }
    pub fn require_ready(&self) -> Result<()> {
        self.require_original(false)?;
        ensure!(
            self.storage()?.initialized.load(Ordering::Acquire)
                && self.storage()?.ready_acknowledged.load(Ordering::Acquire)
                && !self.storage()?.shutdown_completed.load(Ordering::Acquire)
                && !self.storage()?.reclaim_completed.load(Ordering::Acquire),
            "original native factory is not service-ready"
        );
        self.require_original(false)
    }
    pub fn require_runtime_binding(&self, runtime: &PinnedDirectory) -> Result<()> {
        require_current_native_process()?;
        self.authority.require_original(false)?;
        self.authority.require_runtime_binding(runtime)
    }
    pub fn budget_snapshot(&self) -> Result<OriginalNativeBudgetSnapshot> {
        self.budget.snapshot()
    }
    /// Debit bounded consumer bookkeeping done for an admitted issuance (for
    /// example pruning a ticket-bounded issuance record) against the ordinary,
    /// non-cleanup work allowance of this original budget.
    pub fn debit_issuance_steps(&self, steps: u64) -> Result<()> {
        self.require_process()?;
        self.budget.debit_steps(steps, false)
    }
    /// Original process check only, usable before cleanup mutex traversal.
    /// It does not authorize native effects or renew a clock.
    pub fn require_original_process(&self) -> Result<()> { self.require_process() }
    fn require_process(&self) -> Result<()> {
        require_current_native_process()?;
        ensure!(
            !crate::foreign_process(self.original_process),
            "foreign native process"
        );
        require_current_native_process()
    }
    fn require_original(&self, cleanup: bool) -> Result<()> {
        require_current_native_process()?;
        self.require_process()?;
        self.authority.require_original(cleanup)?;
        ensure!(
            !self.storage()?.finishing.load(Ordering::Acquire) || cleanup,
            "original factory draining; new SQL refused"
        );
        Ok(())
    }
    /// One native operation boundary. Every mode checks the process and the
    /// authority's durable facts; service and commit-gate boundaries also
    /// check the producer and destination through `service`. Within the
    /// boundary the authority's durable reads run once; nested checks still
    /// re-read the latch and the process. A fact refusal latches service
    /// refusal; contention, the planned edge and a foreign process do not.
    fn boundary(&self, mode: BoundaryMode, service: impl FnOnce() -> Result<()>) -> Result<()> {
        let cleanup = mode == BoundaryMode::Cleanup;
        let checked = {
            let scope = NativeBoundaryScope::enter(Arc::as_ptr(&self.authority) as *const (), cleanup);
            (|| -> Result<()> {
                self.require_process()?;
                self.require_original(cleanup)?;
                scope.verified();
                if !cleanup {
                    service()?;
                }
                Ok(())
            })()
        };
        checked.map_err(|error| {
            // A forked child never writes the parent's latch.
            if self.require_process().is_ok() && native_refusal_is_fact(&error) {
                self.authority.refuse_service();
            }
            error
        })
    }
    fn retain_error(self: &Arc<Self>, error: anyhow::Error) -> OriginalNativeFactoryFailure {
        OriginalNativeFactoryFailure {
            error,
            factory: self.clone(),
        }
    }
    fn enter_effect(&self) -> Result<NativeEffectGuard<'_>> {
        require_current_native_process()?;
        self.require_process()?;
        ensure!(
            !self.storage()?.finalizer.swap(true, Ordering::AcqRel),
            "original native effect in progress"
        );
        Ok(NativeEffectGuard(&self.storage()?.finalizer, self.original_process))
    }
    fn arena(&self) -> Result<&NativeArena> {
        require_current_native_process()?;
        self.require_process()?;
        self.storage()?.arena.get().context("original native arena absent")
    }
    /// Guarded evidence read run inside this factory's native scope. The
    /// alias gate's projection-lease census then answers for this genuine
    /// owner, so a lease this factory holds on another inode no longer
    /// refuses the read; a lease on the read inode still does. The unscoped
    /// free function keeps refusing whenever any lease is listed.
    pub fn read_regular_without_sqlite_alias(
        &self,
        runtime: &PinnedDirectory,
        name: &std::ffi::OsStr,
        maximum_bytes: usize,
    ) -> Result<Option<Vec<u8>>> {
        self.require_original(false)?;
        let result = {
            let _scope = self.arena()?.scope(false, 0)?;
            crate::read_regular_without_sqlite_alias(runtime, name, maximum_bytes)
        };
        let post = self.require_original(false);
        match (result, post) {
            (Err(original), Err(post)) => Err(post.superseding(original)),
            (Err(original), _) => Err(original),
            (Ok(_), Err(post)) => Err(post),
            (Ok(value), Ok(())) => Ok(value),
        }
    }
    /// Guarded source verification run inside this factory's native scope,
    /// with the same census answer as the scoped evidence read above.
    pub fn verify_regular_without_sqlite_alias(
        &self,
        source: &PinnedDirectory,
        name: &std::ffi::OsStr,
        expected: Option<&crate::SqliteRoleIdentity>,
    ) -> Result<()> {
        self.require_original(false)?;
        let result = {
            let _scope = self.arena()?.scope(false, 0)?;
            crate::verify_regular_without_sqlite_alias(source, name, expected)
        };
        let post = self.require_original(false);
        match (result, post) {
            (Err(original), Err(post)) => Err(post.superseding(original)),
            (Err(original), _) => Err(original),
            (Ok(()), Err(post)) => Err(post),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
    /// Read a retained original witness inside this factory's native scope,
    /// with the same census answer as the scoped evidence read above: a lease
    /// this factory holds on another inode no longer refuses, a lease on the
    /// witness inode still does. It does not revalidate the admission
    /// authority, because the host authority calls it from inside its own
    /// revalidation, which that check would re-enter. When no scope can be
    /// entered (the arena is absent or released, or this thread already runs
    /// in a scope of the other window), the read runs unchanged: inside an
    /// enclosing scope of this arena it already answers for this owner, and
    /// outside every scope it refuses while any lease is listed. The lease
    /// census of this read is not charged to the arena budget: the owner
    /// rereads its witness from its own checkpoints, so a cleanup reservation
    /// sized by the arena layout could not bound the reread count.
    pub fn read_original_witness(
        &self,
        witness: &crate::RetainedOriginalWitnessFile,
        maximum: usize,
        cleanup: bool,
    ) -> Result<Vec<u8>> {
        self.require_process()?;
        let arena = match self.storage() {
            Ok(storage) if !storage.arena_released.load(Ordering::Acquire) => storage.arena.get(),
            Ok(_) | Err(_) => None,
        };
        let _scope = match arena.map(|arena| arena.scope(cleanup, 0)) {
            Some(Ok(scope)) => Some(scope),
            Some(Err(_)) | None => None,
        };
        witness.read_original(maximum)
    }
    pub fn initialize_original_backend(
        self: &Arc<Self>,
    ) -> std::result::Result<(), OriginalNativeFactoryFailure> {
        let result = (|| -> Result<()> {
            self.require_original(false)?;
            let _effect = self.enter_effect()?;
            self.require_original(false)?;
            if self.storage()?.initialized.load(Ordering::Acquire) {
                self.acknowledge_ready_issuer()?;
                return self.require_original(false);
            }
            if self.storage()?.arena.get().is_none() {
                ensure!(
                    !self.storage()?.arena_entered.swap(true, Ordering::AcqRel),
                    "original arena acquisition pending; no reissue"
                );
                self.budget.debit_steps(1, false)?;
                let mut raw = std::ptr::null_mut();
                self.storage()?.arena_attempt.store(1, Ordering::Release);
                let code =
                    unsafe { ryeos_native_arena_create(self.budget_c_lease.get().context("budget C loan absent")?.budget_pointer()?, &mut raw) };
                // Retain actual effect before status/window checks.
                if let Some(raw) = NonNull::new(raw) {
                    self.storage()?.arena_attempt.store(3, Ordering::Release);
                    self.storage()?.arena
                        .set(NativeArena(raw, std::process::id()))
                        .map_err(|_| anyhow::anyhow!("original arena registration conflict"))?;
                } else if code != libsqlite3_sys::SQLITE_OK {
                    // Actual C return establishes this attempt acquired no map.
                    // A contradictory OK/null outcome stays unknown/HOLD.
                    self.storage()?.arena_attempt.store(2, Ordering::Release);
                }
                ensure!(
                    code == libsqlite3_sys::SQLITE_OK,
                    "original native arena creation refused ({code})"
                );
            }
            let arena = self.arena()?;
            ensure!(
                unsafe { ryeos_native_commit_verifier_install(arena.0.as_ptr(), native_commit_verifier) }
                    == libsqlite3_sys::SQLITE_OK,
                "original native commit verifier installation refused"
            );
            *NATIVE_COMMIT_TABLE.write().map_err(|_| anyhow!("native commit table poisoned"))? =
                Some(Arc::downgrade(&self.commit_table));
            let _scope = arena.scope(false, 0)?;
            let code = unsafe { libsqlite3_sys::sqlite3_initialize() };
            ensure!(
                code == libsqlite3_sys::SQLITE_OK,
                "original native initialization refused ({code})"
            );
            ensure!(
                unsafe { ryeos_native_arena_ready(arena.0.as_ptr()) } == libsqlite3_sys::SQLITE_OK,
                "original native allocator/bootstrap binding refused"
            );
            self.storage()?.initialized.store(true, Ordering::Release);
            self.acknowledge_ready_issuer()?;
            self.require_original(false)?;
            Ok(())
        })();
        result.map_err(|error| self.retain_error(error))
    }
    fn acknowledge_ready_issuer(&self) -> Result<()> {
        require_current_native_process()?;
        self.producer
            .try_lock()
            .map_err(|_| anyhow::anyhow!("native issuer handoff busy"))?
            .as_ref()
            .context("original native issuer interval missing")?
            .acknowledge_factory_handoff()?;
        // Publish Ready only after the actual issuer interval acknowledged it.
        self.storage()?.ready_acknowledged.store(true, Ordering::Release);
        Ok(())
    }
    fn acknowledge_terminal_issuer(&self, body: &OriginalNativeTerminalBody) -> Result<()> {
        require_current_native_process()?;
        if !body.issuer_acknowledged.load(Ordering::Acquire) {
            self.producer
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original factory refusal acknowledgement busy"))?
                .as_ref()
                .context("original factory issuer interval missing")?
                .acknowledge_factory_refusal()?;
            body.issuer_acknowledged.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn ticket(&self) -> Result<OriginalNativeConnectionAdmission> {
        require_current_native_process()?;
        self.require_original(false)?;
        ensure!(
            self.storage()?.initialized.load(Ordering::Acquire)
                && self.storage()?.ready_acknowledged.load(Ordering::Acquire),
            "original native bootstrap/Ready handoff not acknowledged"
        );
        // The caller retains the returned permit before every fallible join.
        self.authority.begin_connection()
    }
    pub fn open_transient(
        self: &Arc<Self>,
        inputs: OriginalNativeTransientInputs,
    ) -> std::result::Result<OriginalNativeConnection, OriginalNativeOpenFailure> {
        self.open_owned(inputs.domain, OriginalNativeDestinationInputs::Transient(inputs.authority))
    }
    pub fn open_bound(
        self: &Arc<Self>,
        inputs: OriginalNativeBoundInputs,
    ) -> std::result::Result<OriginalNativeConnection, OriginalNativeOpenFailure> {
        self.open_owned(inputs.domain, OriginalNativeDestinationInputs::Bound(inputs))
    }
    fn open_owned(
        self: &Arc<Self>,
        domain: OriginalNativeDomain,
        destination: OriginalNativeDestinationInputs,
    ) -> std::result::Result<OriginalNativeConnection, OriginalNativeOpenFailure> {
        let mut ticket = None;
        let mut producer = None;
        let mut admission_failure = None;
        let mut acquired = None;
        let step_refused = Arc::new(AtomicBool::new(false));
        let opened = (|| -> Result<()> {
            self.require_original(false)?;
            let _effect = self.enter_effect()?;
            self.require_original(false)?;
            ensure!(
                matches!(destination, OriginalNativeDestinationInputs::Bound(_))
                    || matches!(
                        domain,
                        OriginalNativeDomain::StateTransient
                            | OriginalNativeDomain::OperationalSyncJobSchemaReference
                            | OriginalNativeDomain::OperationalReplaySchemaReference
                            | OriginalNativeDomain::NodeRuntimeSchemaReference
                            | OriginalNativeDomain::NodeOperationalSchemaReference
                            | OriginalNativeDomain::NodeProjectionSchemaReference
                    | OriginalNativeDomain::NodeSchedulerSchemaReference
                    | OriginalNativeDomain::NodeAccountingSchemaReference
                    | OriginalNativeDomain::ExternalSupervisorJournalSchemaReference
                    | OriginalNativeDomain::ExternalGuestJournalSchemaReference
                    ),
                "only admitted State domain may open transient SQLite"
            );
            destination.authority().require_original(self)?;
            let admission = match self.ticket() {
                Ok(admission) => admission,
                Err(error) => match error.downcast::<OriginalNativeConnectionAdmissionFailure>() {
                    Ok(failure) => {
                        admission_failure = Some(failure);
                        anyhow::bail!("actual published SQL admission retained; validation recovery required");
                    }
                    Err(error) => return Err(error),
                },
            };
            let original_ticket = admission.ticket;
            ticket = Some(original_ticket);
            producer = Some(admission.producer);
            producer.as_ref().context("native producer missing")?.require_original_region(&self.region)?;
            // Preserve the genuine issuer interval even if custody/window
            // validation refuses before the first native acquisition.
            ensure!(
                admission.budget.same_original(&self.budget),
                "foreign original native producer counter"
            );
            self.storage()?.tickets[original_ticket as usize - 1].store(1, Ordering::Release);
            producer.as_ref().unwrap().checkpoint()?;
            let arena = self.arena()?;
            let _scope = arena.scope(false, original_ticket)?;
            let body = if let OriginalNativeDestinationInputs::Bound(inputs) = &destination {
                match inputs.mode {
                    OriginalNativeBoundMode::ReadonlyInspection => {
                        let (lease, sql) = inputs.inspection.as_ref().unwrap();
                        match ReadonlySqliteCopy::open(
                            inputs.root.clone(),
                            inputs.name.as_os_str(),
                            lease.clone(),
                            sql.clone(),
                        ) {
                            Ok(owner) => NativeConnectionBody::Inspection(owner),
                            Err(owner) => {
                                acquired = Some(NativeOpenOwner::Inspection(owner));
                                anyhow::bail!("original readonly native acquisition refused")
                            }
                        }
                    }
                    _ => {
                        let mode = match inputs.mode {
                            OriginalNativeBoundMode::Existing => 0,
                            OriginalNativeBoundMode::CreateExclusive => 1,
                            OriginalNativeBoundMode::OpenOrCreate => 2,
                            OriginalNativeBoundMode::ReadonlySource => 4,
                            OriginalNativeBoundMode::ReadonlyInspection => unreachable!(),
                        };
                        match BoundSqliteConnection::acquire(
                            inputs.root.clone(),
                            inputs.name.as_os_str(),
                            mode,
                            None,
                            None,
                            None,
                        ) {
                            Ok(owner) => NativeConnectionBody::Bound(owner),
                            Err(owner) => {
                                acquired = Some(NativeOpenOwner::Bound(owner));
                                anyhow::bail!("original bound native acquisition refused")
                            }
                        }
                    }
                }
            } else {
                NativeConnectionBody::Transient(Connection::open_in_memory()?)
            };
            acquired = Some(NativeOpenOwner::Live(body));
            let NativeOpenOwner::Live(body) = acquired.as_ref().unwrap() else {
                unreachable!()
            };
            let db = unsafe { body.connection().handle() };
            // The commit slot is filled before enrollment, so open-time
            // staging commits pass the same gate.
            let permit = producer.as_ref().context("native producer missing")?;
            self.commit_table.slots[original_ticket as usize - 1]
                .set(NativeCommitSlot {
                    factory: Arc::downgrade(self),
                    destination: Arc::downgrade(destination.authority()),
                    control: Arc::downgrade(permit.control_owner()),
                    cell: permit.region_cell(),
                    refused: Arc::clone(&step_refused),
                })
                .map_err(|_| anyhow!("original native commit slot already filled"))?;
            ensure!(
                unsafe { ryeos_native_connection_enroll(arena.0.as_ptr(), db, original_ticket) }
                    == libsqlite3_sys::SQLITE_OK,
                "original native connection enrollment refused"
            );
            let authority = self.authority.clone();
            body.connection().progress_handler(
                1,
                Some(native_step_hook(move || authority.debit_sql_vm_steps(1), None, Some(Arc::clone(&step_refused)))),
            );
            body.connection().busy_timeout(std::time::Duration::ZERO)?;
            body.connection().pragma_update(None, "mmap_size", 0)?;
            body.connection().pragma_update(None, "threads", 0)?;
            // The bound VFS admits main and WAL roles only, never a rollback
            // journal file, so a database this owner just created cannot take
            // its first write through one. It enters WAL through an in-memory
            // rollback journal; there is no prior content for that to protect.
            // An existing main that is provably empty (a create interrupted
            // before its first WAL commit) is staged the same way; any other
            // non-WAL existing main keeps refusing its first rollback write.
            if let NativeConnectionBody::Bound(owner) = body {
                let writable_existing = matches!(
                    &destination,
                    OriginalNativeDestinationInputs::Bound(inputs) if matches!(
                        inputs.mode,
                        OriginalNativeBoundMode::Existing | OriginalNativeBoundMode::OpenOrCreate
                    )
                );
                let stage = match owner.origin() {
                    crate::BoundSqliteOrigin::Created => true,
                    crate::BoundSqliteOrigin::Existing => {
                        writable_existing
                            && owner.binding.as_ref().context("bound binding missing")?.main_empty()?
                            && owner.connection()
                                .query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))? == 0
                    }
                };
                if stage {
                    let staged: String = owner.connection()
                        .query_row("PRAGMA journal_mode=MEMORY", [], |row| row.get(0))?;
                    let mode: String = owner.connection()
                        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
                    ensure!(staged == "memory" && mode == "wal", "empty bound database did not enter WAL");
                }
            }
            self.boundary(BoundaryMode::Service, || {
                producer.as_ref().context("native producer missing")?.checkpoint()?;
                destination.authority().require_original(self)
            })?;
            Ok(())
        })();
        match opened {
            Ok(()) => {
                let Some(NativeOpenOwner::Live(body)) = acquired.take() else {
                    unreachable!()
                };
                Ok(OriginalNativeConnection {
                    ticket: ticket.unwrap(),
                    domain,
                    deadline_active: std::cell::Cell::new(false),
                    deadline_anchor: std::cell::Cell::new(None),
                    hook_refused: std::cell::Cell::new(false),
                    transaction_retained: std::cell::Cell::new(false),
                    inner: lillux::custody::Held::new(
                        NATIVE_CONNECTION_WRAPPER_KIND,
                        NativeConnectionParts {
                            body: Some(native_connection_body(body)),
                            producer,
                            destination: Some(destination),
                            step_refused,
                            factory: self.clone(),
                        },
                        native_connection_parts_released,
                    ),
                })
            }
            Err(error) => Err(OriginalNativeOpenFailure {
                parts: lillux::custody::Held::new(
                    NATIVE_OPEN_FAILURE_WRAPPER_KIND,
                    OriginalNativeOpenFailureParts {
                        domain,
                        admission_failure,
                        error,
                        factory: self.clone(),
                        ticket,
                        producer,
                        owner: open_owner_slot(acquired),
                        destination: Some(destination),
                    },
                    native_open_failure_parts_released),
            }),
        }
    }
    fn recover_undelivered_admission(
        &self, budget: &OriginalNativeBudgetOwner, ticket: u32,
        producer: &SqliteInspectionProductionPermit,
        kind: SqliteInspectionProductionKind,
    ) -> Result<()> {
        self.require_original(true)?;
        budget.require_original_process()
            .map_err(|error| anyhow!("undelivered admission process refusal: {error:?}"))?;
        let _effect = self.enter_effect()?;
        ensure!(budget.same_original(&self.budget) && budget.contains_issued_ticket(ticket)?,
            "undelivered admission selects foreign original budget/ticket");
        ensure!(producer.kind == kind, "undelivered admission has foreign purpose");
        let crate::ProductionControlOwner::Region { core, .. } = &producer.control else {
            anyhow::bail!("undelivered admission lacks actual resident producer");
        };
        ensure!(core.same_region(&self.region), "undelivered admission selects foreign region");
        core.require_kind(kind)?;
        let cell = core.cell()?;
        ensure!(cell.ticket.load(Ordering::Acquire) == ticket,
            "undelivered admission selects another actual producer ticket");
        let slot = &self.storage()?.tickets[ticket as usize - 1];
        match cell.phase.load(Ordering::Acquire) {
            1 => {
                ensure!(slot.load(Ordering::Acquire) == 0,
                    "undelivered admission already has conflicting native activity");
                // Only these private typed failure owners prove non-delivery.
                // Phase/slot observations alone do not authorize this transition.
                cell.phase.compare_exchange(1, 5, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| anyhow!("undelivered producer recovery busy"))?;
            }
            5 => {}
            _ => anyhow::bail!("undelivered producer has conflicting original phase"),
        }
        match slot.load(Ordering::Acquire) {
            0 => { slot.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| anyhow!("undelivered ticket recovery busy"))?; }
            1 => {}
            _ => anyhow::bail!("undelivered ticket has conflicting native disposition"),
        }
        // Cache registration before this late original-clock check. Failure
        // keeps the SAME typed admission and phase-5 registration for retry.
        self.require_original(true)
    }
    /// Retires a genuine ticket whose resident permit was never published.
    /// The caller retains `core` and its outside preparation custody on every
    /// refusal. This produces a real C closed-ticket receipt; it neither opens
    /// a connection/lease nor substitutes an enum for native acknowledgement.
    pub fn retire_unpublished_ticket(self: &Arc<Self>, core: &OriginalNativeProducerControl)
        -> Result<Arc<ReleasedOriginalNativeAcquisition>> {
        self.require_original(true)?;
        let _effect = self.enter_effect()?;
        ensure!(core.same_region(&self.region), "unpublished ticket selects foreign region");
        let cell = core.cell()?;
        ensure!(matches!(cell.kind.load(Ordering::Acquire), 4 | 5),
            "ticket recovery requires native or lease preparation");
        let ticket = cell.ticket.load(Ordering::Acquire);
        ensure!(self.budget.contains_issued_ticket(ticket)?, "preparation lacks actual issued ticket");
        match cell.phase.load(Ordering::Acquire) {
            0 => { cell.phase.compare_exchange(0, 5, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| anyhow::anyhow!("preparation publication/recovery busy"))?; }
            5 => {}
            2 if self.storage()?.tickets[ticket as usize - 1].load(Ordering::Acquire) == 3 => {}
            _ => anyhow::bail!("published producer cannot use preparation recovery"),
        }
        let slot = &self.storage()?.tickets[ticket as usize - 1];
        match slot.load(Ordering::Acquire) {
            0 => {
                // Phase 5 permanently excludes publication; the only permitted
                // effect is acknowledging absence under the original cleanup
                // scope. A refused C call leaves this same preparation retriable.
                self.require_original(true)?;
                let _scope = self.arena()?.scope(true, 0)?;
                self.budget.debit_steps(1, true)?;
                // Gate/phase acquisition and precharge do not extend the
                // original cleanup clock. Recheck the same authority at the
                // irreversible C boundary; refusal retains phase 5/slot 0.
                self.require_original(true)?;
                self.mark_closed(ticket, true)?;
            }
            3 => {}
            _ => anyhow::bail!("unpublished ticket has conflicting native activity"),
        }
        core.completed()?;
        core.reference_retired();
        Ok(Arc::new(ReleasedOriginalNativeAcquisition {
            domain: None,
            factory: self.clone(), ticket: Some(ticket),
            original_error: anyhow::anyhow!("original producer preparation ended before publication"),
            inspection: None, closed_inspection: None,
            destination: Mutex::new(None), destination_authority: None,
        }))
    }
    fn mark_closed(&self, ticket: u32, acquisition: bool) -> Result<()> {
        require_current_native_process()?;
        let arena = self.arena()?;
        ensure!(
            unsafe { ryeos_native_connection_closed(arena.0.as_ptr(), ticket) }
                == libsqlite3_sys::SQLITE_OK,
            "original native closed ticket acknowledgement refused"
        );
        self.storage()?.tickets[ticket as usize - 1].store(if acquisition { 3 } else { 2 }, Ordering::Release);
        Ok(())
    }
    /// Original allocator cleanup scope only. Caller keeps its genuine
    /// signed-intent/CAS/namespace frames and retains returned/partial owners.
    /// A late error owns the callback's completed T; no owner disappears on ?.
    pub fn with_original_cleanup<T>(
        self: &Arc<Self>,
        inputs: &OriginalNativeCleanupInputs,
        operation: impl for<'c> FnOnce(&'c OriginalNativeCleanupScope<'c>) -> Result<T>,
    ) -> std::result::Result<T, OriginalNativeCleanupFailure<T>> {
        // No callback owner exists yet; the refusal still retains this
        // factory and the caller's inputs rather than discarding them.
        if let Err(error) = require_current_native_process() {
            return Err(OriginalNativeCleanupFailure {
                error,
                completed: Mutex::new(None),
                factory: self.clone(),
                inputs: inputs.clone(),
            });
        }
        let mut completed = None;
        let result = (|| -> Result<()> {
            self.require_original(true)?;
            inputs.require_original(self)?;
            let _effect = self.enter_effect()?;
            self.require_original(true)?;
            inputs.require_original(self)?;
            ensure!(
                !self.storage()?.shutdown_completed.load(Ordering::Acquire)
                    && self.storage()?.terminal_body.get().is_none(),
                "original backend shutdown; new cleanup intake refused"
            );
            let _scope = self.arena()?.scope(true, 0)?;
            let context = OriginalNativeCleanupScope {
                factory: self,
                inputs,
                _not_send: std::marker::PhantomData,
            };
            completed = Some(operation(&context)?);
            context.checkpoint()?;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(completed.take().expect("actual completed cleanup callback")),
            Err(error) => Err(OriginalNativeCleanupFailure {
                error,
                completed: Mutex::new(completed),
                factory: self.clone(),
                inputs: inputs.clone(),
            }),
        }
    }
    /// Stopped (no-SQL) intake inside a cleanup allocator scope with no open
    /// ticket, the only scope the native stopped-intake entry admits. It is
    /// admitted while the service window is open, holds this factory's effect
    /// gate so no open, finish or other effect runs beside it, and refuses
    /// once the backend has shut down or recorded its terminal. Nothing in
    /// `operation` may enter a service scope of this arena (that nested entry
    /// refuses BUSY), so permit and journal reads run outside it. Owners the
    /// callback consumes should be borrowed, not moved in: a refusal before
    /// the callback runs drops the callback. A refusal after it ran carries
    /// its value in the failure.
    pub fn with_original_stopped_intake<T>(
        &self,
        operation: impl for<'s> FnOnce(&'s OriginalNativeStoppedIntakeScope<'s>) -> T,
    ) -> std::result::Result<T, OriginalNativeScopeFailure<T>> {
        let mut completed = None;
        let entered = (|| -> Result<()> {
            self.require_original(false)?;
            let _effect = self.enter_effect()?;
            self.require_original(false)?;
            ensure!(
                !self.storage()?.shutdown_completed.load(Ordering::Acquire)
                    && self.storage()?.terminal_body.get().is_none(),
                "original backend shutdown; stopped intake refused"
            );
            let _scope = self.arena()?.scope(true, 0)?;
            let context = OriginalNativeStoppedIntakeScope {
                _factory: self,
                _not_send: std::marker::PhantomData,
            };
            completed = Some(operation(&context));
            Ok(())
        })();
        Self::scope_outcome(entered.and_then(|()| self.require_original(false)), completed)
    }
    /// Native work that holds no ticket of its own, such as a stopped
    /// owner's observations, role mutations, release and drop, inside a
    /// service allocator scope admitted while the service window is open.
    /// Guarded reads made through this factory nest inside it. Owners the
    /// callback consumes should be borrowed, not moved in: a refusal before
    /// the callback runs drops the callback. A refusal after it ran carries
    /// its value in the failure.
    pub fn with_original_service_scope<T>(
        &self,
        operation: impl for<'s> FnOnce(&'s OriginalNativeServiceScope<'s>) -> T,
    ) -> std::result::Result<T, OriginalNativeScopeFailure<T>> {
        let mut completed = None;
        let entered = (|| -> Result<()> {
            self.require_original(false)?;
            let _scope = self.arena()?.scope(false, 0)?;
            let context = OriginalNativeServiceScope {
                _factory: self,
                _not_send: std::marker::PhantomData,
            };
            completed = Some(operation(&context));
            Ok(())
        })();
        Self::scope_outcome(entered.and_then(|()| self.require_original(false)), completed)
    }
    fn scope_outcome<T>(
        checked: Result<()>,
        completed: Option<T>,
    ) -> std::result::Result<T, OriginalNativeScopeFailure<T>> {
        match (checked, completed) {
            (Ok(()), Some(value)) => Ok(value),
            (Ok(()), None) => Err(OriginalNativeScopeFailure {
                error: anyhow!("original native scope admitted without running its callback"),
                completed: None,
            }),
            (Err(error), completed) => Err(OriginalNativeScopeFailure { error, completed }),
        }
    }
    /// Call after the lexical cleanup callback returns. Genuine sealed intake
    /// and exhaustive actual terminals authorize native component retirement;
    /// this does not discharge source pins, reservations or final State ACK.
    pub fn finish_original_backend(
        self: &Arc<Self>,
        terminals: &[OriginalNativeConnectionTerminal],
        leases: &[Arc<ClosedOriginalProjectionLease>],
        inputs: &OriginalNativeCleanupInputs,
    ) -> std::result::Result<Arc<OriginalNativeTerminal>, OriginalNativeFactoryFailure> {
        self.finish_original_backend_owned(terminals, leases, OriginalNativeFinishIntake::Offline(inputs))
    }
    /// Drain an actual pre-Ready initialization refusal without inventing a
    /// connection terminal or requiring a seal blocked by the issuer interval.
    pub fn finish_original_initialization_refusal(
        self: &Arc<Self>,
    ) -> std::result::Result<Arc<OriginalNativeTerminal>, OriginalNativeFactoryFailure> {
        self.finish_original_backend_owned(&[], &[], OriginalNativeFinishIntake::InitializationRefusal)
    }
    /// Genuine live factory drain permits a Ready zero-ticket census without
    /// inventing a connection terminal. SQL and lease receipts remain distinct;
    /// one exhaustive original resource-ticket census checks both below.
    pub fn finish_live_backend(
        self: &Arc<Self>,
        terminals: &[OriginalNativeConnectionTerminal],
        leases: &[Arc<ClosedOriginalProjectionLease>],
        inputs: &OriginalLiveNativeDrainInputs,
    ) -> std::result::Result<Arc<OriginalNativeTerminal>, OriginalNativeFactoryFailure> {
        self.finish_original_backend_owned(terminals, leases, OriginalNativeFinishIntake::Live(inputs))
    }
    fn require_finish_intake(
        self: &Arc<Self>,
        inputs: OriginalNativeFinishIntake<'_>,
    ) -> Result<()> {
        require_current_native_process()?;
        match inputs {
            OriginalNativeFinishIntake::Offline(inputs) => {
                ensure!(
                    self.storage()?.ready_acknowledged.load(Ordering::Acquire),
                    "original native Ready handoff absent"
                );
                inputs.require_original(self)
            }
            OriginalNativeFinishIntake::Live(inputs) => {
                ensure!(
                    self.storage()?.ready_acknowledged.load(Ordering::Acquire),
                    "live native Ready handoff absent"
                );
                inputs.require_original(self)
            }
            OriginalNativeFinishIntake::InitializationRefusal => {
                ensure!(
                    !self.storage()?.ready_acknowledged.load(Ordering::Acquire)
                        && self.budget.connections_issued()? == 0,
                    "initialization refusal requires pre-Ready factory without tickets"
                );
                Ok(())
            }
        }
    }
    fn finish_original_backend_owned(
        self: &Arc<Self>,
        terminals: &[OriginalNativeConnectionTerminal],
        leases: &[Arc<ClosedOriginalProjectionLease>],
        inputs: OriginalNativeFinishIntake<'_>,
    ) -> std::result::Result<Arc<OriginalNativeTerminal>, OriginalNativeFactoryFailure> {
        let result = (|| -> Result<Arc<OriginalNativeTerminal>> {
            self.require_original(true)?;
            self.require_finish_intake(inputs)?;
            let _effect = self.enter_effect()?;
            self.require_original(true)?;
            self.require_finish_intake(inputs)?;
            self.storage()?.finishing.store(true, Ordering::Release);
            let count = self.budget.connections_issued()? as usize;
            ensure!(
                terminals.len().checked_add(leases.len()) == Some(count),
                "original native terminal ticket cardinality differs"
            );
            let mut seen = [false; MAX_NATIVE_CONNECTIONS];
            for terminal in terminals {
                let (factory, ticket) = terminal.original();
                ensure!(
                    Arc::ptr_eq(factory, self) && ticket > 0 && (ticket as usize) <= count,
                    "foreign native terminal ticket"
                );
                ensure!(
                    !seen[ticket as usize - 1],
                    "duplicate original native terminal ticket"
                );
                seen[ticket as usize - 1] = true;
                ensure!(
                    matches!(
                        self.storage()?.tickets[ticket as usize - 1].load(Ordering::Acquire),
                        2 | 3
                    ),
                    "original native connection still pending"
                );
            }
            for lease in leases {
                lease.require_original(self)?;
                let index = lease.ticket as usize - 1;
                ensure!(index < count && !seen[index], "foreign/duplicate projection lease census ticket");
                seen[index] = true;
            }
            if let Some(proof) = self.retained_terminal() {
                self.acknowledge_terminal_issuer(proof.body()?)?;
                self.require_original(true)?;
                return Ok(proof);
            }
            if self.storage()?.arena.get().is_none() {
                ensure!(
                    count == 0
                        && !self.storage()?.initialized.load(Ordering::Acquire)
                        && matches!(self.storage()?.arena_attempt.load(Ordering::Acquire), 0 | 2),
                    "original factory acquisition unresolved; no absence terminal"
                );
                let body = OriginalNativeTerminalBody {
                    budget: self.budget.snapshot()?, arena_disposition: OriginalNativeArenaDisposition::NotAcquired,
                    issuer_acknowledged: AtomicBool::new(false),
                };
                self.storage()?.terminal_body
                    .set(body)
                    .map_err(|_| anyhow::anyhow!("original refusal body conflict"))?;
                let body = self.storage()?.terminal_body.get().context("retained absence body missing")?;
                self.acknowledge_terminal_issuer(body)?;
                self.require_original(true)?;
                return Ok(Arc::new(OriginalNativeTerminal { factory: self.clone(), body: NonNull::from(body) }));
            }
            if self.storage()?.arena_released.load(Ordering::Acquire) {
                let body = OriginalNativeTerminalBody { budget: self.budget.snapshot()?,
                    arena_disposition: OriginalNativeArenaDisposition::Released, issuer_acknowledged: AtomicBool::new(false) };
                let _ = self.storage()?.terminal_body.set(body);
                let body = self.storage()?.terminal_body.get().context("actual release body absent")?;
                self.acknowledge_terminal_issuer(body)?;
                self.require_original(true)?;
                return Ok(Arc::new(OriginalNativeTerminal { factory: self.clone(), body: NonNull::from(body) }));
            }
            let arena = self.arena()?;
            let _scope = arena.scope(true, 0)?;
            if !self.storage()?.shutdown_completed.load(Ordering::Acquire) {
                self.budget.debit_steps(1, true)?;
                let code = unsafe { libsqlite3_sys::sqlite3_shutdown() };
                ensure!(
                    code == libsqlite3_sys::SQLITE_OK,
                    "original native shutdown pending ({code})"
                );
                self.storage()?.shutdown_completed.store(true, Ordering::Release);
            }
            if !self.storage()?.reclaim_completed.load(Ordering::Acquire) {
                ensure!(
                    unsafe { ryeos_native_arena_reclaim(arena.0.as_ptr()) }
                        == libsqlite3_sys::SQLITE_OK,
                    "original native pending allocations remain"
                );
                self.storage()?.reclaim_completed.store(true, Ordering::Release);
            }
            drop(_scope);
            self.budget.debit_steps(1, true)?;
            ensure!(
                unsafe { ryeos_native_arena_release(arena.0.as_ptr()) }
                    == libsqlite3_sys::SQLITE_OK,
                "original native arena release pending"
            );
            self.storage()?.arena_released.store(true, Ordering::Release);
            {
                let mut current = NATIVE_COMMIT_TABLE.write()
                    .map_err(|_| anyhow!("native commit table poisoned"))?;
                if current.as_ref().is_some_and(|table| std::ptr::eq(table.as_ptr(), Arc::as_ptr(&self.commit_table))) {
                    *current = None;
                }
            }
            // Store irreversible actual release before any later clock/memo
            // check; retries return SAME body and never touch unmapped arena.
            let body = OriginalNativeTerminalBody {
                budget: self.budget.snapshot()?, arena_disposition: OriginalNativeArenaDisposition::Released,
                issuer_acknowledged: AtomicBool::new(false),
            };
            self.storage()?.terminal_body
                .set(body)
                .map_err(|_| anyhow::anyhow!("original native terminal body conflict"))?;
            let body = self.storage()?.terminal_body.get().context("retained release body missing")?;
            self.acknowledge_terminal_issuer(body)?;
            let proof = Arc::new(OriginalNativeTerminal { factory: self.clone(), body: NonNull::from(body) });
            *self.storage()?.terminal
                .try_lock()
                .map_err(|_| anyhow::anyhow!("native terminal memo busy"))? =
                Some(Arc::downgrade(&proof));
            self.require_original(true)?;
            Ok(proof)
        })();
        result.map_err(|error| self.retain_error(error))
    }
}

pub struct OriginalNativeFactoryFailure {
    error: anyhow::Error,
    factory: Arc<OriginalNativeFactory>,
}
impl OriginalNativeFactoryFailure {
    pub fn factory(&self) -> &Arc<OriginalNativeFactory> {
        &self.factory
    }
}
impl std::fmt::Debug for OriginalNativeFactoryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeFactoryFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalNativeFactoryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for OriginalNativeFactoryFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

enum NativeConnectionBody {
    Transient(Connection),
    Bound(BoundSqliteConnection),
    Inspection(ReadonlySqliteCopy),
}
impl NativeConnectionBody {
    fn connection(&self) -> &Connection {
        match self {
            Self::Transient(c) => c,
            Self::Bound(c) => c.connection(),
            Self::Inspection(c) => c.connection(),
        }
    }
    fn connection_mut(&mut self) -> &mut Connection {
        match self {
            Self::Transient(c) => c,
            Self::Bound(c) => c.connection.as_mut().unwrap(),
            Self::Inspection(c) => c.owner.connection.as_mut().unwrap(),
        }
    }
    fn close(self) -> std::result::Result<ClosedNativeBody, NativeCloseOwner> {
        match self {
            Self::Transient(connection) => match connection.close() {
                Ok(()) => Ok(ClosedNativeBody::Plain),
                Err((c, e)) => Err(NativeCloseOwner::Transient(c, e)),
            },
            Self::Bound(connection) => match connection.close() {
                Ok(()) => Ok(ClosedNativeBody::Plain),
                Err(f) => Err(NativeCloseOwner::Bound(f)),
            },
            Self::Inspection(connection) => match connection.close() {
                Ok(p) => Ok(ClosedNativeBody::Inspection(Arc::new(p))),
                Err(f) => Err(NativeCloseOwner::Inspection(f)),
            },
        }
    }
}
// No SQL capability survives close. Inspection contains the actual proof.
enum ClosedNativeBody {
    Plain,
    Inspection(Arc<ClosedSqliteInspection>),
}
enum NativeOpenOwner {
    Closing(NativeCloseOwner),
    Closed(ClosedNativeBody),
    Released(Arc<ReleasedSqliteInspection>),
    Live(NativeConnectionBody),
    Bound(BoundSqliteOpenFailure),
    Inspection(ReadonlySqliteOpenFailure),
}
enum NativeCloseOwner {
    Live(NativeConnectionBody),
    Transient(Connection, rusqlite::Error),
    Bound(crate::BoundSqliteCloseFailure),
    Inspection(ReadonlySqliteCloseFailure),
    Closed(ClosedNativeBody),
}
// Restoration owns the actual same native hook/busy configuration. Scope
// Drop restores before the surrounding C native scope is removed on unwind.
struct NativeOriginalDeadlineScope<'a> {
    owner: &'a OriginalNativeConnection,
    connection: &'a Connection,
    prior_busy: std::time::Duration,
    installed: bool,
}
impl NativeOriginalDeadlineScope<'_> {
    fn restore(&mut self) -> Result<()> {
        require_current_native_process()?;
        if !self.installed {
            return Ok(());
        }
        self.installed = false;
        let authority = self.owner.inner.factory.authority.clone();
        self.connection.progress_handler(
            1,
            Some(native_step_hook(move || authority.debit_sql_vm_steps(1), None,
                Some(Arc::clone(&self.owner.inner.step_refused)))),
        );
        let restored = self.connection.busy_timeout(self.prior_busy);
        self.owner.deadline_active.set(false);
        if let Err(error) = restored {
            self.owner.hook_refused.set(true);
            return Err(error).context("restore SAME original native busy owner; service refused");
        }
        Ok(())
    }
}
impl Drop for NativeOriginalDeadlineScope<'_> {
    fn drop(&mut self) {
        if crate::foreign_process(self.owner.inner.factory.original_process)
            || self.owner.inner.factory.require_process().is_err()
        {
            return;
        }
        let _ = self.restore();
    }
}
/// Actual completed stages, diagnostic facts rather than admission authority.
#[derive(Clone, Copy, Debug, Default)]
pub struct LiveOperationalInitializationOutcome {
    pub checkpoint_completed: bool,
    pub wal_synced: bool,
    pub main_synced: bool,
    pub directory_synced: bool,
}
/// Snapshot of actual initialization durability; later SQL can change the files.
/// This receipt supplies no marker publication or mutation authority.
pub struct DurableLiveOperationalInitialization {
    factory: Arc<OriginalNativeFactory>,
    ticket: u32,
    bound_identity: (u64, u64),
}
/// Bounded, non-owning evidence that an exact original Operational ticket was
/// observed closed before resident disposal. It cannot reopen or inspect the
/// factory; later validation compares only its allocation witness.
pub struct ClosedOriginalNativeTicketReceipt {
    process: u32,
    domain: OriginalNativeDomain,
    ticket: u32,
    bound_identity: Option<(u64, u64)>,
    factory: Weak<OriginalNativeFactory>,
}
impl ClosedOriginalNativeTicketReceipt {
    /// Compare this cached witness with another already-retained weak identity.
    /// This remains valid after the factory value and resident mapping are gone.
    pub fn require_factory_witness(&self, factory: &Weak<OriginalNativeFactory>) -> Result<()> {
        require_current_native_process()?;
        ensure!(!crate::foreign_process(self.process),
            "closed native ticket receipt belongs to another process");
        ensure!(self.domain == OriginalNativeDomain::NodeOperational
            && self.ticket > 0 && self.ticket as usize <= MAX_NATIVE_CONNECTIONS
            && self.bound_identity.is_some(),
            "closed native ticket receipt has invalid Operational identity");
        ensure!(Weak::ptr_eq(&self.factory, factory),
            "closed native ticket receipt selects a foreign factory allocation");
        Ok(())
    }
}
impl DurableLiveOperationalInitialization {
    pub fn require_original(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        owner: &OriginalNativeConnection,
    ) -> Result<()> {
        owner.require_original_process()?;
        owner.inner.factory.require_original(false)?;
        ensure!(
            Arc::ptr_eq(&self.factory, factory)
                && Arc::ptr_eq(&self.factory, &owner.inner.factory)
                && self.ticket == owner.ticket
                && owner.domain == OriginalNativeDomain::NodeOperational
                && owner.inner.body.is_some(),
            "live initialization receipt selects foreign or closed owner"
        );
        ensure!(
            owner.bound_main_identity()? == self.bound_identity,
            "live initialization receipt selects foreign Bound identity"
        );
        Ok(())
    }

    /// Confirm that this exact initialization ticket has reached the native
    /// closed phase before its startup-only evidence is released from outside
    /// custody. This is not a native terminal, descriptor receipt, or account
    /// settlement proof; the caller must already hold the corresponding
    /// retired physical inventory.
    pub fn require_closed_ticket_for_retirement(
        &self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<()> {
        self.factory.require_original_process()?;
        factory.require_original_process()?;
        ensure!(Arc::ptr_eq(&self.factory, factory),
            "live initialization retirement selects a foreign factory");
        ensure!(self.ticket > 0 && self.ticket as usize <= MAX_NATIVE_CONNECTIONS,
            "live initialization retirement ticket is outside the native bound");
        ensure!(factory.storage()?.tickets[self.ticket as usize - 1].load(Ordering::Acquire) == 2,
            "live initialization ticket remains open before metadata retirement");
        Ok(())
    }

    /// Capture immutable ticket identity while the exact factory is still
    /// resident. The caller must drop this strong initialization owner after
    /// caching the returned receipt and before the final zero-handle seal.
    pub fn closed_ticket_receipt(
        &self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<ClosedOriginalNativeTicketReceipt> {
        self.require_closed_ticket_for_retirement(factory)?;
        Ok(ClosedOriginalNativeTicketReceipt {
            process: std::process::id(),
            domain: OriginalNativeDomain::NodeOperational,
            ticket: self.ticket,
            bound_identity: Some(self.bound_identity),
            factory: Arc::downgrade(factory),
        })
    }
}
const LIVE_INITIALIZATION_FAILURE_KIND: &str = "lillux-sqlite::live-operational-initialization-failure";
/// The live connection of a refused initialization. Constructible only in this
/// module.
struct LiveInitializationFailureParts { owner: OriginalNativeConnection }
/// Holds the SAME live owner and actual partial outcomes; no cleanup runs from
/// here. Only `into_parts` settles an initialization refusal. Any other end
/// retains the whole owner, counted, until the process exits.
pub struct LiveOperationalInitializationFailure {
    error: anyhow::Error,
    outcome: LiveOperationalInitializationOutcome,
    inner: lillux::custody::Held<LiveInitializationFailureParts>,
}
impl LiveOperationalInitializationFailure {
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn outcome(&self) -> LiveOperationalInitializationOutcome {
        self.outcome
    }
    pub fn into_parts(
        self,
    ) -> std::result::Result<
        (anyhow::Error, OriginalNativeConnection, LiveOperationalInitializationOutcome),
        Self,
    > {
        if self.inner.owner.require_original_process().is_err() {
            return Err(self);
        }
        // Exactly this retained owner transfers once, together with its partial facts.
        let Self { error, outcome, inner } = self;
        match inner.take_whole() {
            Ok(parts) => Ok((error, parts.owner, outcome)),
            Err(inner) => Err(Self { error, outcome, inner }),
        }
    }
}
impl std::fmt::Debug for LiveOperationalInitializationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveOperationalInitializationFailure")
            .field("error", &self.error)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveOperationalInitializationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for LiveOperationalInitializationFailure {}

pub struct OriginalNativeConnection {
    deadline_active: std::cell::Cell<bool>,
    deadline_anchor: std::cell::Cell<Option<lillux::time::MonotonicDeadline>>,
    hook_refused: std::cell::Cell<bool>,
    // A scope that ended in refusal with a transaction still open. The owner
    // must close this connection; no later service scope runs on it.
    transaction_retained: std::cell::Cell<bool>,
    ticket: u32,
    domain: OriginalNativeDomain,
    inner: lillux::custody::Held<NativeConnectionParts>,
}
/// Every owning field of a connection, so a copy in another process can give
/// up the whole connection at once. Constructible only in this module.
struct NativeConnectionParts {
    // The unsupervised body is retained, counted, when a connection ends
    // without a supervised close: no terminal runs from a destructor.
    body: Option<lillux::custody::Held<NativeConnectionBody>>,
    producer: Option<SqliteInspectionProductionPermit>,
    destination: Option<OriginalNativeDestinationInputs>,
    // Set by this connection's step hook or commit gate when they refuse.
    step_refused: Arc<AtomicBool>,
    factory: Arc<OriginalNativeFactory>,
}
/// Whether the running process is the one that issued this factory and its
/// native process check still passes. A copy in another process, or one the
/// native side refuses, is never settled: dropping its fields would act on
/// custody that process does not hold.
fn factory_held_here(factory: &OriginalNativeFactory) -> bool {
    process_held_here(factory.original_process, || factory.require_process().is_ok())
}
fn process_held_here(original_process: u32, native_check: impl FnOnce() -> bool) -> bool {
    !crate::foreign_process(original_process) && native_check()
}
/// Settled exactly when this process holds the factory: then the connection's
/// own fields drop normally while a still-present body retains itself through
/// its own custody. Otherwise the whole connection, factory included, is
/// retained and counted (or relinquished outright in a forked copy), and no
/// field's destructor runs.
fn native_connection_parts_released(parts: &NativeConnectionParts) -> bool {
    factory_held_here(&parts.factory)
}
const NATIVE_CONNECTION_WRAPPER_KIND: &str = "lillux-sqlite::native-connection-wrapper";
fn native_connection_body(body: NativeConnectionBody) -> lillux::custody::Held<NativeConnectionBody> {
    lillux::custody::Held::until_taken(
        crate::AbandonedSqliteOwnerKind::NativeConnection.custody_kind(), body)
}
impl OriginalNativeConnection {
    /// Check the actual factory process identity before changing wrapper state.
    pub fn require_original_process(&self) -> Result<()> {
        self.inner.factory.require_process()
    }
    /// Consume this actual live Operational Bound for checked initialization durability.
    /// Success returns the SAME connection. Failure retains it and all completed stages.
    pub fn sync_live_operational_initialization(
        self,
    ) -> std::result::Result<
        (Self, DurableLiveOperationalInitialization),
        LiveOperationalInitializationFailure,
    > {
        let mut outcome = LiveOperationalInitializationOutcome::default();
        let mut bound_identity = None;
        let result = (|| -> Result<(u64, u64)> {
            self.require_original_process()?;
            ensure!(
                self.domain == OriginalNativeDomain::NodeOperational,
                "live initialization durability requires NodeOperational"
            );
            let Some(OriginalNativeDestinationInputs::Bound(inputs)) = self.inner.destination.as_ref() else {
                anyhow::bail!("live Bound inputs absent");
            };
            ensure!(
                matches!(inputs.mode, OriginalNativeBoundMode::Existing
                    | OriginalNativeBoundMode::CreateExclusive
                    | OriginalNativeBoundMode::OpenOrCreate),
                "live initialization durability requires writable Bound"
            );
            self.inner.factory.require_ready()?;
            let _effect = self.inner.factory.enter_effect()?;
            self.with_sql(|connection| {
                let (busy, log, checkpointed): (i64, i64, i64) = connection.query_row(
                    "PRAGMA wal_checkpoint(TRUNCATE)", [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                ensure!(busy == 0 && log == 0 && checkpointed == 0,
                    "live initialization checkpoint incomplete ({busy}, {log}, {checkpointed})");
                outcome.checkpoint_completed = true;
                self.service_boundary()?;
                let NativeConnectionBody::Bound(owner) = self.inner.body.as_deref().context("live body absent")? else {
                    anyhow::bail!("live initialization owner is not Bound")
                };
                let binding = owner.binding.as_ref().context("live Bound keeper absent")?;
                binding.validate(connection, false)?;
                bound_identity = Some(binding.main_identity()?);
                let synced = binding.sync_live(connection);
                // Retain actual irreversible effects before interpreting code or late authority.
                outcome.wal_synced = synced.wal_synced;
                outcome.main_synced = synced.main_synced;
                ensure!(synced.code == libsqlite3_sys::SQLITE_OK
                    && synced.wal_synced && synced.main_synced,
                    "live Bound initialization sync refused ({})", synced.code);
                self.service_boundary()?;
                self.inner.factory.budget.debit_steps(1, false)?;
                inputs.root.sync()?;
                outcome.directory_synced = true;
                self.service_boundary()
            })?;
            self.inner.factory.require_ready()?;
            bound_identity.context("live initialization Bound identity absent")
        })();
        match result {
            Ok(bound_identity) => {
                let receipt = DurableLiveOperationalInitialization {
                    factory: Arc::clone(&self.inner.factory),
                    ticket: self.ticket,
                    bound_identity,
                };
                Ok((self, receipt))
            }
            Err(error) => Err(LiveOperationalInitializationFailure {
                error,
                outcome,
                inner: lillux::custody::Held::until_taken(
                    LIVE_INITIALIZATION_FAILURE_KIND, LiveInitializationFailureParts { owner: self }),
            }),
        }
    }
    /// Metadata of this actual retained owner; grants no SQL or mutation.
    pub fn domain(&self) -> OriginalNativeDomain {
        self.domain
    }
    /// The actual owner remains retained and can close through original cleanup.
    /// ROOT uses this to close its existing attachment-admission signal.
    pub fn sql_owner_refused(&self) -> bool {
        self.hook_refused.get()
    }
    /// A scope on this connection ended in refusal with its transaction
    /// still open. No later service scope runs; the owner closes it, which
    /// rolls the open transaction back.
    pub fn transaction_retained(&self) -> bool {
        self.transaction_retained.get()
    }
    fn require_sql_owner(&self) -> Result<()> {
        require_current_native_process()?;
        ensure!(
            !self.hook_refused.get(),
            "original native hook/busy restoration refused; SQL owner retained"
        );
        ensure!(
            !self.transaction_retained.get(),
            "original native transaction retained after refusal; connection must close"
        );
        if let Some(deadline) = self.deadline_anchor.get() {
            ensure!(
                !deadline.has_elapsed(),
                "original Runtime native deadline expired; no renewal"
            );
        }
        Ok(())
    }
    /// Only this same-original offline RuntimeMutation owner. The deadline
    /// becomes a connection-wide anchor: once it elapses the offline
    /// operation is over and the owner serves no further SQL. No default
    /// clock, inspection hook replacement or deadline renewal.
    pub fn with_sql_deadline<T>(
        &self,
        deadline: lillux::time::MonotonicDeadline,
        operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> Result<T> {
        require_current_native_process()?;
        ensure!(
            self.domain == OriginalNativeDomain::RuntimeMutation,
            "original anchored Runtime deadline requires the offline RuntimeMutation owner"
        );
        ensure!(
            !self.deadline_active.get(),
            "original native deadline cannot nest or renew"
        );
        self.require_sql_owner()?;
        let effective = self
            .deadline_anchor
            .get()
            .map_or(deadline, |prior| prior.min(deadline));
        self.deadline_anchor.set(Some(effective));
        ensure!(
            !effective.has_elapsed(),
            "original native deadline expired before scope"
        );
        self.with_sql(|connection| {
            let prior: i64 = connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            let prior_busy = std::time::Duration::from_millis(
                u64::try_from(prior).context("original native busy timeout negative")?,
            );
            ensure!(
                !effective.has_elapsed(),
                "original native deadline expired before hook installation"
            );
            connection.busy_timeout(std::time::Duration::ZERO)?;
            self.deadline_active.set(true);
            let mut scope = NativeOriginalDeadlineScope {
                owner: self,
                connection,
                prior_busy,
                installed: true,
            };
            let authority = self.inner.factory.authority.clone();
            connection.progress_handler(
                1,
                Some(native_step_hook(move || authority.debit_sql_vm_steps(1), Some(effective),
                    Some(Arc::clone(&self.inner.step_refused)))),
            );
            let result = if effective.has_elapsed() {
                Err(anyhow::anyhow!("original native deadline expired before operation"))
            } else {
                operation(connection)
            };
            let result = match result {
                Ok(value) if effective.has_elapsed() => {
                    drop(value);
                    Err(anyhow::anyhow!(
                        "original native operation completed after expiry; commit outcome may be uncertain"
                    ))
                }
                other => other,
            };
            let restored = scope.restore();
            match (result, restored) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(original), Ok(())) => Err(original),
                (Ok(_), Err(restore)) => Err(restore),
                (Err(original), Err(restore)) => Err(restore.superseding(original)),
            }
        })
    }
    /// Per-operation absolute deadline for a live daemon owner: the
    /// accounting ledger or the Runtime store. The deadline check is
    /// installed together with this connection's VM-step debit, and the
    /// debit-only hook is restored afterwards. Unlike `with_sql_deadline`,
    /// the deadline is not retained as a connection-wide anchor: an elapsed
    /// deadline ends only its own operation, and the live connection keeps
    /// serving later operations under their own deadlines.
    pub fn with_sql_until<T>(
        &self,
        deadline: lillux::time::MonotonicDeadline,
        operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> Result<T> {
        require_current_native_process()?;
        ensure!(
            matches!(
                self.domain,
                OriginalNativeDomain::NodeAccounting | OriginalNativeDomain::NodeRuntime
            ),
            "original per-operation deadline requires a live NodeAccounting or NodeRuntime owner"
        );
        ensure!(
            !self.deadline_active.get(),
            "original native deadline cannot nest"
        );
        self.require_sql_owner()?;
        ensure!(
            !deadline.has_elapsed(),
            "original native deadline expired before scope"
        );
        self.with_sql(|connection| {
            let prior: i64 = connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            let prior_busy = std::time::Duration::from_millis(
                u64::try_from(prior).context("original native busy timeout negative")?,
            );
            ensure!(
                !deadline.has_elapsed(),
                "original native deadline expired before hook installation"
            );
            connection.busy_timeout(std::time::Duration::ZERO)?;
            self.deadline_active.set(true);
            let mut scope = NativeOriginalDeadlineScope {
                owner: self,
                connection,
                prior_busy,
                installed: true,
            };
            let authority = self.inner.factory.authority.clone();
            connection.progress_handler(
                1,
                Some(native_step_hook(move || authority.debit_sql_vm_steps(1), Some(deadline),
                    Some(Arc::clone(&self.inner.step_refused)))),
            );
            let result = if deadline.has_elapsed() {
                Err(anyhow::anyhow!("original native deadline expired before operation"))
            } else {
                operation(connection)
            };
            let result = match result {
                Ok(value) if deadline.has_elapsed() => {
                    drop(value);
                    Err(anyhow::anyhow!(
                        "original native operation completed after expiry; commit outcome may be uncertain"
                    ))
                }
                other => other,
            };
            let restored = scope.restore();
            match (result, restored) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(original), Ok(())) => Err(original),
                (Ok(_), Err(restore)) => Err(restore),
                (Err(original), Err(restore)) => Err(restore.superseding(original)),
            }
        })
    }
    /// Observe the actual scheduler keeper's creation disposition. This is
    /// metadata, not namespace or retirement authority; no connection escapes.
    pub fn scheduler_bound_origin(&self) -> Result<crate::BoundSqliteOrigin> {
        require_current_native_process()?;
        ensure!(self.domain == OriginalNativeDomain::NodeScheduler,
            "scheduler origin requires actual NodeScheduler owner");
        self.inner.factory.require_ready()?;
        self.with_sql(|connection| {
            let NativeConnectionBody::Bound(owner) = self.inner.body.as_deref()
                .context("original scheduler connection closed")? else {
                anyhow::bail!("scheduler origin requires actual Bound keeper")
            };
            self.inner.factory.budget.debit_steps(1, false)?;
            owner.binding.as_ref().context("original scheduler keeper missing")?
                .validate(connection, false)?;
            Ok(owner.origin())
        })
    }
    /// Check the SAME scheduler runtime and keeper under actual native scope.
    /// Matching a directory alone supplies no scheduler service authority.
    pub fn require_scheduler_runtime(&self, runtime: &PinnedDirectory) -> Result<()> {
        require_current_native_process()?;
        ensure!(self.domain == OriginalNativeDomain::NodeScheduler,
            "scheduler runtime check requires actual NodeScheduler owner");
        self.inner.factory.require_ready()?;
        self.with_sql(|connection| {
            let NativeConnectionBody::Bound(owner) = self.inner.body.as_deref()
                .context("original scheduler connection closed")? else {
                anyhow::bail!("scheduler runtime check requires actual Bound keeper")
            };
            ensure!(owner.runtime().is_same_directory(runtime)?,
                "scheduler owner selects foreign runtime");
            runtime.ensure_path_binding()?;
            self.inner.factory.budget.debit_steps(1, false)?;
            owner.binding.as_ref().context("original scheduler keeper missing")?
                .validate(connection, false)
        })
    }
    /// After admitted scheduler DDL has established WAL, adopt and validate
    /// the SAME keeper's actual sidecars. Refusal leaves this owner retained;
    /// success is not schema, durability, retirement or terminal evidence.
    pub fn establish_scheduler_wal_binding(&self) -> Result<()> {
        require_current_native_process()?;
        ensure!(self.domain == OriginalNativeDomain::NodeScheduler,
            "scheduler WAL requires actual NodeScheduler owner");
        self.inner.factory.require_ready()?;
        self.with_sql(|_| {
            let NativeConnectionBody::Bound(owner) = self.inner.body.as_deref()
                .context("original scheduler connection closed")? else {
                anyhow::bail!("scheduler WAL requires actual Bound keeper")
            };
            self.inner.factory.budget.debit_steps(1, false)?;
            owner.establish_wal_binding()
        })
    }
    /// Facts from the SAME admitted Bound keeper, within its lexical native
    /// scope. The caller retains this live owner when a comparison refuses.
    pub fn bound_main_identity(&self) -> Result<(u64, u64)> {
        require_current_native_process()?;
        self.with_sql(|connection| {
            let NativeConnectionBody::Bound(owner) = self
                .inner
                .body
                .as_deref()
                .context("original live native body missing")?
            else {
                anyhow::bail!("original connection is not a Bound owner")
            };
            let binding = owner
                .binding
                .as_ref()
                .context("original Bound keeper missing")?;
            self.inner.factory.budget.debit_steps(1, false)?;
            // The keeper's own inode, observed before validation: a refused
            // validation poisons the keeper, which then reports no identity.
            let keeper = binding.main_identity()?;
            if let Err(refusal) = binding.validate(connection, false) {
                // Name what the bound name now resolves to when that explains
                // the refusal; the keeper stays poisoned either way.
                let named = binding
                    .path()
                    .ok()
                    .and_then(|path| std::path::Path::new(path).file_name().map(OsStr::to_os_string))
                    .map(|name| owner.runtime().entry_no_follow(&name));
                return Err(match named {
                    Some(Ok(None)) => refusal
                        .context("bound database was removed from its name while its keeper is live"),
                    Some(Ok(Some(entry))) if (entry.containing_device, entry.inode) != keeper => refusal
                        .context("bound database was replaced: its name resolves to a different file than the live keeper"),
                    _ => refusal,
                });
            }
            binding.main_identity()
        })
    }
    /// Service boundary of one scoped SQL operation: process, durable
    /// authority facts, producer and destination.
    fn service_boundary(&self) -> Result<()> {
        self.inner.factory.boundary(BoundaryMode::Service, || {
            self.inner.producer
                .as_ref()
                .context("original native producer missing")?
                .checkpoint()?;
            if let Some(destination) = &self.inner.destination {
                destination.authority().require_original(&self.inner.factory)?;
            }
            Ok(())
        })
    }
    /// Exit of one scoped SQL operation. A scope that ended in a boundary,
    /// step-hook or commit-gate refusal with a transaction still open retains
    /// that transaction for close; the owner's own SQL errors do not.
    fn finish_sql_scope<T>(&self, result: Result<T>, autocommit: bool,
        refusal: &StepRefusalScope) -> Result<T> {
        let post = self.service_boundary();
        let step_refused = refusal.refused();
        if (post.is_err() || step_refused) && !autocommit {
            self.transaction_retained.set(true);
        }
        let post = post.and_then(|()| self.require_sql_owner());
        sql_scope_outcome(result, post, step_refused)
    }
    pub fn with_sql<T>(
        &self,
        operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> Result<T> {
        require_current_native_process()?;
        self.require_sql_owner()?;
        self.service_boundary()?;
        let connection = self
            .inner
            .body
            .as_deref()
            .context("original SQL connection closed")?
            .connection();
        let refusal = StepRefusalScope::enter(Arc::clone(&self.inner.step_refused));
        let result = {
            let _scope = self.inner.factory.arena()?.scope(false, 0)?;
            operation(connection)
        };
        self.finish_sql_scope(result, connection.is_autocommit(), &refusal)
    }
    pub fn with_sql_mut<T>(
        &mut self,
        operation: impl for<'c> FnOnce(&'c mut Connection) -> Result<T>,
    ) -> Result<T> {
        require_current_native_process()?;
        self.require_sql_owner()?;
        self.service_boundary()?;
        let refusal = StepRefusalScope::enter(Arc::clone(&self.inner.step_refused));
        let factory = Arc::clone(&self.inner.factory);
        let (result, autocommit) = {
            let _scope = factory.arena()?.scope(false, 0)?;
            let connection = self
                .inner
                .get_mut()
                .body
                .as_mut()
                .map(|held| held.get_mut())
                .context("original SQL connection closed")?
                .connection_mut();
            let result = operation(connection);
            (result, connection.is_autocommit())
        };
        self.finish_sql_scope(result, autocommit, &refusal)
    }
    pub fn close(
        mut self,
    ) -> std::result::Result<Arc<ClosedOriginalNativeConnection>, OriginalNativeCloseFailure> {
        let mut process_error = self.inner.factory.require_process().err();
        let held = self.inner.get_mut().body.take().expect("original live native body");
        // A copy in another process may not take the body: it stays with the
        // copy, which relinquishes it, and the failure carries no owner.
        let owner = match held.take_whole() {
            Ok(body) => Some(NativeCloseOwner::Live(body)),
            Err(held) => {
                self.inner.get_mut().body = Some(held);
                process_error.get_or_insert_with(|| anyhow!("native connection belongs to another process"));
                None
            }
        };
        let producer = self.inner.get_mut().producer.take();
        let destination = self.inner.get_mut().destination.take();
        OriginalNativeCloseFailure {
            parts: lillux::custody::Held::new(
                NATIVE_CLOSE_FAILURE_WRAPPER_KIND,
                OriginalNativeCloseFailureParts {
                    error: process_error,
                    bound_identity: None,
                    factory: Arc::clone(&self.inner.factory),
                    ticket: self.ticket,
                    domain: self.domain,
                    producer,
                    owner: close_owner_slot(owner),
                    destination,
                },
                native_close_failure_parts_released),
        }
        .retry()
    }
}

pub struct ClosedOriginalNativeConnection {
    durable_body: OnceLock<Arc<OriginalNativeDurableBody>>,
    durable_finalizer: AtomicBool,
    bound_identity: Option<(u64, u64)>,
    factory: Arc<OriginalNativeFactory>,
    ticket: u32,
    domain: OriginalNativeDomain,
    body: ClosedNativeBody,
    // Moves out once, by a take-capture or the consuming outside handoff.
    destination: Mutex<Option<OriginalNativeDestinationInputs>>,
    destination_authority: Option<std::sync::Weak<dyn OriginalNativeDestinationAuthority>>,
    _scheduler_retirement: Option<OriginalSchedulerRetirementInputs>,
}
impl ClosedOriginalNativeConnection {
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        require_current_native_process()?;
        ensure!(
            Arc::ptr_eq(&self.factory, factory)
                && self.factory.storage()?.tickets[self.ticket as usize - 1].load(Ordering::Acquire) == 2,
            "closed native proof selects foreign/pending owner"
        );
        Ok(())
    }
    pub fn domain(&self) -> OriginalNativeDomain {
        self.domain
    }
    /// Capture this actual closed ticket as non-owning evidence while its
    /// factory and resident ticket table are still valid.
    pub fn closed_ticket_receipt(
        &self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<ClosedOriginalNativeTicketReceipt> {
        self.require_original(factory)?;
        ensure!(self.domain == OriginalNativeDomain::NodeOperational,
            "closed ticket receipt requires NodeOperational");
        Ok(ClosedOriginalNativeTicketReceipt {
            process: std::process::id(),
            domain: self.domain,
            ticket: self.ticket,
            bound_identity: self.bound_identity,
            factory: Arc::downgrade(factory),
        })
    }
    pub fn inspection_closed(&self) -> Option<Arc<ClosedSqliteInspection>> {
        match &self.body {
            ClosedNativeBody::Inspection(p) => Some(p.clone()),
            _ => None,
        }
    }
    /// Producer disposition of this closed connection's durable metadata pins.
    /// Requires the issuer-acknowledged terminal: finish has frozen the
    /// write-once body, so `[None, None]` is a complete NotCreated answer. The
    /// leaf and pins come from this same allocation; the leaf is a borrowed
    /// capture, not a consuming outside-custody receipt.
    pub fn retained_durable_metadata_leaf(
        self: &Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &OriginalNativeTerminal,
    ) -> Result<(
        OriginalNativeOutsideLeafEvidence,
        [Option<Arc<RetainedOriginalMetadataPin>>; 2],
    )> {
        require_current_native_process()?;
        terminal.require_original(factory)?;
        self.require_original(factory)?;
        let leaf = OriginalNativeOutsideLeafEvidence {
            process: std::process::id(),
            identity: OriginalNativeOutsideLeafIdentity::Closed(Arc::downgrade(self)),
            factory: Arc::downgrade(&self.factory),
            authority: self.destination_authority.clone(),
            ticket: Some(self.ticket),
            domain: Some(self.domain),
            closed: true,
        };
        let pins = match self.durable_body.get() {
            Some(body) => [Some(body.main.clone()), body.wal.clone()],
            None => [None, None],
        };
        Ok((leaf, pins))
    }
    /// Close this connection's durability pins at the end of its host, after
    /// the backend finished. `recheck` runs before each close, and `tally`
    /// counts each pin as it ends; pins already retired are skipped, so a
    /// retry after a refusal closes only the rest. A refused close keeps its
    /// descriptor in the pin and stops here.
    pub(crate) fn retire_durability_pins(
        &self,
        recheck: &dyn Fn() -> Result<()>,
        tally: &mut crate::offline_host::DurabilityPinRelease,
    ) -> Result<()> {
        require_current_native_process()?;
        let Some(body) = self.durable_body.get() else {
            return Ok(());
        };
        for pin in std::iter::once(&body.main).chain(body.wal.iter()) {
            recheck()?;
            // Counted per pin as it ends, so a refusal of a later pin loses
            // no count of an earlier one.
            tally.count(pin.close_at_host_release()?)?;
        }
        Ok(())
    }
}
// Durable facts are recorded immediately after actual file fsync. There is
// no strong proof->owner->proof cycle and no reconstruction from inode facts.
struct OriginalNativeDurableBody {
    main: Arc<RetainedOriginalMetadataPin>,
    wal: Option<Arc<RetainedOriginalMetadataPin>>,
    file_synced: AtomicBool,
    close_refusal: OnceLock<(i32, i32)>,
    directory_synced: AtomicBool,
}
pub struct DurableOriginalNativeBound {
    closed: Arc<ClosedOriginalNativeConnection>,
    body: Arc<OriginalNativeDurableBody>,
}
impl DurableOriginalNativeBound {
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        require_current_native_process()?;
        self.closed.require_original(factory)?;
        ensure!(
            self.body.file_synced.load(Ordering::Acquire)
                && self.body.directory_synced.load(Ordering::Acquire)
                && self.body.close_refusal.get().is_none(),
            "original bound directory fsync pending"
        );
        ensure!(
            self.closed
                .durable_body
                .get()
                .is_some_and(|body| Arc::ptr_eq(body, &self.body)),
            "foreign original durable body"
        );
        Ok(())
    }
    /// Actual durability O_PATH holders, distinct from the original source
    /// pin inventory. Arc clones preserve these same owners, never dup FDs.
    pub fn metadata_pins(
        &self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<Vec<Arc<RetainedOriginalMetadataPin>>> {
        require_current_native_process()?;
        self.require_original(factory)?;
        Ok(std::iter::once(self.body.main.clone())
            .chain(self.body.wal.iter().cloned())
            .collect())
    }
    pub fn main_identity(&self) -> (u64, u64) {
        self.closed
            .bound_identity
            .expect("genuine durable Bound identity")
    }
    pub fn closed(&self) -> Arc<ClosedOriginalNativeConnection> {
        self.closed.clone()
    }
}
pub struct OriginalNativeDurabilityFailure {
    error: anyhow::Error,
    closed: Arc<ClosedOriginalNativeConnection>,
}
impl std::fmt::Debug for OriginalNativeDurabilityFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeDurabilityFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalNativeDurabilityFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for OriginalNativeDurabilityFailure {}
impl OriginalNativeDurabilityFailure {
    pub fn file_fsync_completed(&self) -> bool {
        self.closed
            .durable_body
            .get()
            .is_some_and(|body| body.file_synced.load(Ordering::Acquire))
    }
    pub fn temporary_close_refusal(&self) -> Option<(i32, i32)> {
        self.closed
            .durable_body
            .get()
            .and_then(|body| body.close_refusal.get().copied())
    }
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn closed(&self) -> Arc<ClosedOriginalNativeConnection> {
        self.closed.clone()
    }
    pub fn retry(self) -> std::result::Result<Arc<DurableOriginalNativeBound>, Self> {
        match self.closed.sync_generated_bound() {
            Ok(proof) => Ok(proof),
            Err(mut next) => {
                next.error = self.error.context(next.error);
                Err(next)
            }
        }
    }
}
impl ClosedOriginalNativeConnection {
    pub fn bound_main_identity(&self) -> Option<(u64, u64)> {
        self.bound_identity
    }
    /// Whether this closed receipt still owns its original destination
    /// inputs (Bound root alias and authority, or a transient authority).
    pub fn retains_destination_inputs(&self) -> Result<bool> {
        Ok(lock_destination(&self.destination)?.is_some())
    }
    /// Move the destination inputs out of this shared receipt, once. The
    /// capture is consuming for those inputs and may enter join leaf sets; a
    /// later outside handoff of this allocation carries no destination. Any
    /// refusal moves nothing; `None` means the inputs were already taken.
    pub fn take_destination_inputs(
        self: &Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &OriginalNativeTerminal,
    ) -> Result<Option<(OriginalNativeOutsideLeafEvidence, OriginalNativeDestinationInputs)>> {
        require_current_native_process()?;
        terminal.require_original(factory)?;
        self.require_original(factory)?;
        ensure!(!self.durable_finalizer.load(Ordering::Acquire),
            "original generated durability effect is in flight");
        let mut slot = lock_destination(&self.destination)?;
        let Some(inputs) = slot.take() else { return Ok(None) };
        Ok(Some((OriginalNativeOutsideLeafEvidence {
            process: std::process::id(),
            identity: OriginalNativeOutsideLeafIdentity::Closed(Arc::downgrade(self)),
            factory: Arc::downgrade(&self.factory),
            authority: self.destination_authority.clone(),
            ticket: Some(self.ticket),
            domain: Some(self.domain),
            closed: true,
        }, inputs)))
    }
    /// Exact original generated Bound only, after real SQL/native close.
    /// No SQL reopen, raw connection, data descriptor or generic fsync grant.
    pub fn sync_generated_bound(
        self: &Arc<Self>,
    ) -> std::result::Result<Arc<DurableOriginalNativeBound>, OriginalNativeDurabilityFailure> {
        let result = (|| -> Result<Arc<DurableOriginalNativeBound>> {
            self.require_original(&self.factory)?;
            self.factory.require_original(false)?;
            // Serialize every charged post-close filesystem effect and its
            // proof against this SAME factory's final terminal snapshot.
            let _effect = self.factory.enter_effect()?;
            self.factory.require_original(false)?;
            // Held for the whole effect so no take-capture can interleave.
            let destination = lock_destination(&self.destination)?;
            let Some(OriginalNativeDestinationInputs::Bound(inputs)) = destination.as_ref() else {
                anyhow::bail!("original Bound inputs missing");
            };
            ensure!(
                self.domain == OriginalNativeDomain::StateGeneratedProjection
                    && inputs.mode == OriginalNativeBoundMode::CreateExclusive,
                "durable generated proof requires exact original created State Bound"
            );
            inputs.authority.require_original(&self.factory)?;
            let identity = self
                .bound_identity
                .context("actual Bound close identity missing")?;
            ensure!(
                !self.durable_finalizer.swap(true, Ordering::AcqRel),
                "original generated durability pending"
            );
            struct Finalizer<'a>(&'a AtomicBool, u32);
            impl Drop for Finalizer<'_> {
                fn drop(&mut self) {
                    if !crate::foreign_process(self.1) && require_current_native_process().is_ok() {
                        self.0.store(false, Ordering::Release);
                    }
                }
            }
            let _finalizer = Finalizer(&self.durable_finalizer, self.factory.original_process);
            #[cfg(not(target_os = "linux"))]
            anyhow::bail!("original generated durability unavailable on this platform");
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::ffi::OsStrExt;
                let main_bytes = inputs.name.as_os_str().as_bytes();
                let mut wal_bytes = [0u8; crate::binding::BOUND_COMPONENT_MAX_BYTES + 4];
                wal_bytes[..main_bytes.len()].copy_from_slice(main_bytes);
                wal_bytes[main_bytes.len()..main_bytes.len() + 4].copy_from_slice(b"-wal");
                let wal_name = OsStr::from_bytes(&wal_bytes[..main_bytes.len() + 4]);
                let body = if let Some(body) = self.durable_body.get() {
                    body.clone()
                } else {
                    let main = inputs
                        .root
                        .open_mount_entry(inputs.name.as_os_str())?
                        .context("original closed main coordinate absent")?;
                    let wal = inputs.root.open_mount_entry(wal_name)?;
                    // Retain filesystem pins BEFORE the fallible fsync.
                    let body = Arc::new(OriginalNativeDurableBody {
                        // Safety: actual original post-close O_PATH owners
                        // transfer once, before fsync or fallible late checks.
                        main: unsafe { RetainedOriginalMetadataPin::issued(main) },
                        wal: wal.map(|pin| unsafe { RetainedOriginalMetadataPin::issued(pin) }),
                        file_synced: AtomicBool::new(false),
                        close_refusal: OnceLock::new(),
                        directory_synced: AtomicBool::new(false),
                    });
                    self.durable_body.set(body.clone()).map_err(|_| {
                        anyhow::anyhow!("original durable fsync registration conflict")
                    })?;
                    body
                };
                body.main.with_original_file(|main| {
                    crate::ensure_guarded_read_name(&inputs.root, inputs.name.as_os_str(), main)
                })?;
                if let Some(wal) = &body.wal {
                    wal.with_original_file(|pin| {
                        crate::ensure_guarded_read_name(&inputs.root, wal_name, pin)
                    })?;
                    ensure!(
                        wal.observed_identity()?.size() == 0,
                        "original generated WAL became nonempty"
                    );
                } else {
                    ensure!(
                        inputs.root.entry_no_follow(&wal_name)?.is_none(),
                        "original generated WAL appeared after close"
                    );
                }
                ensure!(
                    body.close_refusal.get().is_none(),
                    "original temporary data descriptor close refused (SQLite code/errno {:?}); no FD/effect retry",
                    body.close_refusal.get()
                );
                if !body.file_synced.load(Ordering::Acquire) {
                    self.factory.require_original(false)?;
                    inputs.authority.require_original(&self.factory)?;
                    self.factory.budget.debit_steps(1, false)?;
                    // The closed-main gate consults the native projection
                    // lease census, which only answers inside a service scope;
                    // outside one every listed lease reads as blocking.
                    let _scope = self.factory.arena()?.scope(false, 0)?;
                    let outcome = body.main.with_original_file(|main| {
                        if let Some(wal) = &body.wal {
                            wal.with_original_file(|wal| {
                                Ok(crate::binding::sync_closed_main(main, Some(wal), identity))
                            })
                        } else {
                            Ok(crate::binding::sync_closed_main(main, None, identity))
                        }
                    })?;
                    // Commit each actual effect BEFORE interpreting final code.
                    if outcome.synced {
                        body.file_synced.store(true, Ordering::Release);
                    }
                    if outcome.close_attempted
                        && (outcome.close_error != 0
                            || outcome.code == libsqlite3_sys::SQLITE_IOERR_CLOSE)
                    {
                        body.close_refusal
                            .set((outcome.code, outcome.close_error))
                            .map_err(|_| {
                                anyhow::anyhow!("original temporary close refusal conflict")
                            })?;
                    }
                    outcome.require_success()?;
                }
                if !body.directory_synced.load(Ordering::Acquire) {
                    self.factory.require_original(false)?;
                    inputs.authority.require_original(&self.factory)?;
                    inputs.root.sync()?;
                    body.directory_synced.store(true, Ordering::Release);
                }
                self.factory.require_original(false)?;
                inputs.authority.require_original(&self.factory)?;
                Ok(Arc::new(DurableOriginalNativeBound {
                    closed: self.clone(),
                    body,
                }))
            }
        })();
        result.map_err(|error| OriginalNativeDurabilityFailure {
            error,
            closed: self.clone(),
        })
    }
}
/// Exact requested effect, not a grant or a reusable work allowance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginalSchedulerRetirementEffect {
    Capture,
    Drain,
    Quarantine(crate::SqliteRole),
    Purge(crate::SqliteRole),
    Resolve(crate::SqliteRole),
    ObserveResolution(crate::SqliteRole),
    Release,
    Abort,
    Terminal,
}
/// # Safety
/// The implementation must retain the actual immutable authenticated intent,
/// original generation/State CAS/namespace exclusion and original service and
/// cleanup clocks for this SAME factory. Each prepayment must debit the SAME
/// original native budget for a complete checked bounded effect (including
/// scans, barriers, physical IO, hashing and partial retries), and retain real
/// separately funded host backing for all controls, errors and receipts.
/// Existing blocking/unbounded C scans cannot satisfy this contract by a
/// guessed counter. If the exact lower effect has no bounded implementation,
/// prepay_effect MUST refuse before that effect; no fallback is permitted.
pub unsafe trait OriginalSchedulerRetirementAuthority: Send + Sync {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>,
        generation: [u8; 16], cleanup: bool) -> Result<()>;
    fn prepay_effect(&self, factory: &Arc<OriginalNativeFactory>,
        generation: [u8; 16], effect: OriginalSchedulerRetirementEffect,
        cleanup: bool) -> Result<()>;
}
pub struct OriginalSchedulerRetirementInputs {
    generation: [u8; 16],
    authority: Arc<dyn OriginalSchedulerRetirementAuthority>,
}
impl OriginalSchedulerRetirementInputs {
    /// Immutable request identity, not authenticated retirement authority.
    pub fn generation(&self) -> [u8; 16] { self.generation }

    /// # Safety
    /// These are genuine retained original intent and effect-funding owners,
    /// not coordinates or a permissive callback. Control storage must already
    /// be funded before this input or any failure owner is constructed.
    pub unsafe fn issued(generation: [u8; 16],
        authority: Arc<dyn OriginalSchedulerRetirementAuthority>) -> Self {
        Self { generation, authority }
    }
}
enum OriginalSchedulerRetirementPhase {
    Initial(OriginalNativeConnection),
    Bound(BoundSqliteConnection),
    Captured(crate::CapturedSqliteProjection),
    Drained(crate::DrainedSqliteProjection),
    Failed(crate::SqliteProjectionRetirementFailure),
    Released,
}
struct OriginalSchedulerRetirementBody {
    factory: Option<Arc<OriginalNativeFactory>>,
    ticket: u32,
    inputs: OriginalSchedulerRetirementInputs,
    destination: Option<OriginalNativeDestinationInputs>,
    producer: Option<SqliteInspectionProductionPermit>,
    phase: Option<OriginalSchedulerRetirementPhase>,
    bound_identity: Option<(u64, u64)>,
    sql_consumed: bool,
    c_sql_cleared: bool,
    physical_release: bool,
    terminal: Option<Arc<ClosedOriginalNativeConnection>>,
}
impl OriginalSchedulerRetirementBody {
    fn factory(&self) -> &Arc<OriginalNativeFactory> {
        if let Some(factory) = &self.factory { return factory; }
        let Some(OriginalSchedulerRetirementPhase::Initial(owner)) = self.phase.as_ref() else {
            unreachable!("original retirement factory remains in initial owner until transfer")
        };
        &owner.inner.factory
    }
}
const SCHEDULER_RETIREMENT_KIND: &str = "lillux-sqlite::scheduler-retirement";
const SCHEDULER_RETIREMENT_FAILURE_KIND: &str = "lillux-sqlite::scheduler-retirement-failure";
/// Affine SAME original owner. Only a completed release or abort settles a
/// retirement, taking the body whole. Any other end keeps the connection,
/// partial role owner, destination and producer retained, counted, until the
/// process exits; it never settles by itself.
pub struct OriginalSchedulerRetirement {
    body: lillux::custody::Held<OriginalSchedulerRetirementBody>,
}
/// The retained retirement of a refused step. Constructible only in this
/// module.
struct SchedulerRetirementFailureParts { owner: Mutex<OriginalSchedulerRetirement> }
/// Only `into_owner` settles a retirement refusal. Any other end retains the
/// retirement with every phase owner, counted, until the process exits.
pub struct OriginalSchedulerRetirementFailure {
    error: anyhow::Error,
    inner: lillux::custody::Held<SchedulerRetirementFailureParts>,
}
impl std::fmt::Debug for OriginalSchedulerRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalSchedulerRetirementFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalSchedulerRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { std::fmt::Display::fmt(&self.error, f) }
}
impl std::error::Error for OriginalSchedulerRetirementFailure {}
impl OriginalSchedulerRetirementFailure {
    pub fn error(&self) -> &anyhow::Error { &self.error }
    pub fn into_owner(mut self) -> std::result::Result<OriginalSchedulerRetirement, Self> {
        if require_current_native_process().is_err() { return Err(self); }
        let owner = self.inner.get_mut().owner.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        if owner.body.factory().require_process().is_err() { return Err(self); }
        let Self { error, inner } = self;
        match inner.take_whole() {
            Ok(parts) => {
                drop(error);
                Ok(parts.owner.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner))
            }
            Err(inner) => Err(Self { error, inner }),
        }
    }
}
impl OriginalNativeConnection {
    /// Stop ordinary SQL by transferring this SAME scheduler connection into
    /// authenticated retirement. Failure retains the original connection or
    /// exact partial role owner; no ordinary Bound can escape.
    pub fn begin_scheduler_retirement(self, inputs: OriginalSchedulerRetirementInputs)
        -> std::result::Result<(OriginalSchedulerRetirement, crate::CapturedSqliteRoleFacts), OriginalSchedulerRetirementFailure> {
        let owner = OriginalSchedulerRetirement {
            body: lillux::custody::Held::until_taken(
                SCHEDULER_RETIREMENT_KIND,
                OriginalSchedulerRetirementBody {
                    factory: None, ticket: self.ticket, inputs,
                    destination: None, producer: None,
                    phase: Some(OriginalSchedulerRetirementPhase::Initial(self)),
                    bound_identity: None, sql_consumed: false, c_sql_cleared: false,
                    physical_release: false, terminal: None,
                }),
        };
        owner.capture()
    }
}
impl OriginalSchedulerRetirement {
    fn failure(self, error: anyhow::Error) -> OriginalSchedulerRetirementFailure {
        OriginalSchedulerRetirementFailure {
            error,
            inner: lillux::custody::Held::until_taken(
                SCHEDULER_RETIREMENT_FAILURE_KIND, SchedulerRetirementFailureParts { owner: Mutex::new(self) }),
        }
    }
    fn require_authority(&self, cleanup: bool) -> Result<()> {
        self.body.factory().require_original(cleanup)?;
        self.body.inputs.authority.require_original(self.body.factory(),
            self.body.inputs.generation, cleanup)
    }
    // Commit known physical consumption BEFORE interpreting the overall lower
    // result. C phase3 alone is never a Rust terminal or producer ACK.
    fn observe_sql_consumption(&mut self) -> Result<()> {
        let consumed = match self.body.phase.as_ref().context("retirement phase absent")? {
            OriginalSchedulerRetirementPhase::Initial(owner) => {
                ensure!(owner.inner.body.is_some(), "contradictory initial SQL custody"); false
            }
            OriginalSchedulerRetirementPhase::Bound(owner) => {
                ensure!(owner.connection.is_some(), "contradictory Bound SQL custody"); false
            }
            OriginalSchedulerRetirementPhase::Captured(_) => false,
            OriginalSchedulerRetirementPhase::Drained(owner) => {
                ensure!(owner.stopped_plan.is_none(), "stopped source cannot clear live ticket"); true
            }
            OriginalSchedulerRetirementPhase::Failed(failure) => failure.original_live_sql_consumed()?,
            OriginalSchedulerRetirementPhase::Released => true,
        };
        ensure!(!self.body.sql_consumed || consumed, "retirement cannot resurrect consumed SQL");
        if consumed { self.body.get_mut().sql_consumed = true; }
        if consumed && !self.body.c_sql_cleared {
            let arena = self.body.factory().arena()?;
            let code = unsafe { ryeos_native_connection_closed(arena.0.as_ptr(), self.body.ticket) };
            if code == libsqlite3_sys::SQLITE_OK { self.body.get_mut().c_sql_cleared = true; }
            ensure!(code == libsqlite3_sys::SQLITE_OK, "retirement SQL census clear refused");
        }
        Ok(())
    }
    fn effect<T>(&mut self, effect: OriginalSchedulerRetirementEffect, cleanup: bool,
        operation: impl FnOnce(&mut OriginalSchedulerRetirementBody) -> Result<T>) -> Result<T> {
        require_current_native_process()?;
        self.require_authority(cleanup)?;
        let factory = self.body.factory().clone();
        let _effect = factory.enter_effect()?;
        self.require_authority(cleanup)?;
        if !cleanup {
            factory.require_ready()?;
            let producer = match self.body.phase.as_ref() {
                Some(OriginalSchedulerRetirementPhase::Initial(owner)) => owner.inner.producer.as_ref(),
                _ => self.body.producer.as_ref(),
            }.context("original retirement producer absent")?;
            producer.checkpoint()?;
        }
        self.body.inputs.authority.prepay_effect(&factory,
            self.body.inputs.generation, effect, cleanup)?;
        self.require_authority(cleanup)?;
        let _scope = factory.arena()?.scope(cleanup, 0)?;
        let result = operation(self.body.get_mut());
        // Even a lower failure can have successfully consumed SQL or released
        // the entire keeper; record that fact before late refusals.
        let facts = self.observe_sql_consumption();
        let post = self.require_authority(cleanup);
        match (result, facts, post) {
            (Err(error), _, _) => Err(error),
            (Ok(_), Err(error), _) | (Ok(_), Ok(()), Err(error)) => Err(error),
            (Ok(value), Ok(()), Ok(())) => Ok(value),
        }
    }
    fn prepare_initial(body: &mut OriginalSchedulerRetirementBody, cleanup: bool) -> Result<()> {
            if let Some(OriginalSchedulerRetirementPhase::Initial(owner)) = body.phase.as_ref() {
                ensure!(owner.domain == OriginalNativeDomain::NodeScheduler,
                    "retirement requires actual NodeScheduler connection");
                let Some(OriginalNativeDestinationInputs::Bound(destination)) = owner.inner.destination.as_ref() else {
                    anyhow::bail!("scheduler destination absent");
                };
                ensure!(destination.name.as_os_str() == OsStr::new("scheduler.sqlite3"),
                    "retirement requires exact fixed scheduler role");
                if !cleanup { destination.authority.require_original(body.factory())?; }
                let NativeConnectionBody::Bound(bound) = owner.inner.body.as_deref().context("scheduler SQL owner absent")? else {
                    anyhow::bail!("retirement requires actual scheduler Bound keeper")
                };
                bound.binding.as_ref().context("scheduler keeper absent")?.validate(bound.connection(), false)?;
                body.bound_identity = Some(bound.binding.as_ref().unwrap().main_identity()?);
            }
            if matches!(body.phase.as_ref(), Some(OriginalSchedulerRetirementPhase::Initial(_))) {
                let Some(OriginalSchedulerRetirementPhase::Initial(mut owner)) = body.phase.take() else { unreachable!() };
                let held = owner.inner.get_mut().body.take();
                let bound = match held.map(lillux::custody::Held::take_whole) {
                    Some(Ok(NativeConnectionBody::Bound(bound))) => bound,
                    Some(Ok(_)) | None => unreachable!(),
                    Some(Err(held)) => {
                        // A copy in another process keeps its body.
                        owner.inner.get_mut().body = Some(held);
                        body.phase = Some(OriginalSchedulerRetirementPhase::Initial(owner));
                        anyhow::bail!("scheduler connection belongs to another process");
                    }
                };
                body.factory = Some(Arc::clone(&owner.inner.factory));
                body.destination = owner.inner.get_mut().destination.take();
                body.producer = owner.inner.get_mut().producer.take();
                body.phase = Some(OriginalSchedulerRetirementPhase::Bound(bound));
            }
        Ok(())
    }
    pub fn capture(mut self) -> std::result::Result<(Self, crate::CapturedSqliteRoleFacts), OriginalSchedulerRetirementFailure> {
        let result = self.effect(OriginalSchedulerRetirementEffect::Capture, false, |body| {
            Self::prepare_initial(body, false)?;
            let phase = body.phase.take().context("retirement phase absent")?;
            let result = match phase {
                OriginalSchedulerRetirementPhase::Bound(owner) => owner.capture_projection_retirement(body.inputs.generation),
                OriginalSchedulerRetirementPhase::Failed(failure) => failure.retry_capture(body.inputs.generation),
                OriginalSchedulerRetirementPhase::Captured(owner) => {
                    let facts = owner.captured_roles().clone();
                    body.phase = Some(OriginalSchedulerRetirementPhase::Captured(owner)); return Ok(facts);
                }
                other => { body.phase = Some(other); anyhow::bail!("invalid retirement capture phase") }
            };
            match result {
                Ok(owner) => {
                    let facts = owner.captured_roles().clone();
                    body.phase = Some(OriginalSchedulerRetirementPhase::Captured(owner)); Ok(facts)
                }
                Err(failure) => {
                    body.phase = Some(OriginalSchedulerRetirementPhase::Failed(failure));
                    anyhow::bail!("scheduler retirement capture retained")
                }
            }
        });
        match result { Ok(facts) => Ok((self, facts)), Err(error) => Err(self.failure(error)) }
    }
    pub fn drain(mut self) -> std::result::Result<(Self, crate::DrainedSqliteRoleFacts), OriginalSchedulerRetirementFailure> {
        let result = self.effect(OriginalSchedulerRetirementEffect::Drain, true, |body| {
            let phase = body.phase.take().context("retirement phase absent")?;
            let result = match phase {
                OriginalSchedulerRetirementPhase::Captured(owner) => owner.drain_sql(),
                OriginalSchedulerRetirementPhase::Failed(failure) => failure.retry_drain(),
                OriginalSchedulerRetirementPhase::Drained(owner) => {
                    let facts = owner.role_facts().clone();
                    body.phase = Some(OriginalSchedulerRetirementPhase::Drained(owner)); return Ok(facts);
                }
                other => { body.phase = Some(other); anyhow::bail!("invalid retirement drain phase") }
            };
            match result {
                Ok(owner) => {
                    let facts = owner.role_facts().clone();
                    body.phase = Some(OriginalSchedulerRetirementPhase::Drained(owner)); Ok(facts)
                }
                Err(failure) => {
                    body.phase = Some(OriginalSchedulerRetirementPhase::Failed(failure));
                    anyhow::bail!("scheduler retirement drain retained")
                }
            }
        });
        match result { Ok(facts) => Ok((self, facts)), Err(error) => Err(self.failure(error)) }
    }
    fn mutate(mut self, role: crate::SqliteRole, purge: bool)
        -> std::result::Result<(Self, crate::SqliteRoleMutationOutcome), OriginalSchedulerRetirementFailure> {
        let effect = if purge { OriginalSchedulerRetirementEffect::Purge(role) } else { OriginalSchedulerRetirementEffect::Quarantine(role) };
        let result = self.effect(effect, true, |body| {
            let Some(OriginalSchedulerRetirementPhase::Drained(owner)) = body.phase.as_mut() else {
                anyhow::bail!("role effect requires actual drained retirement owner")
            };
            let result = if purge { owner.purge_role(role) } else { owner.quarantine_role(role) };
            result.map_err(anyhow::Error::new)
        });
        match result { Ok(outcome) => Ok((self, outcome)), Err(error) => Err(self.failure(error)) }
    }
    pub fn quarantine(self, role: crate::SqliteRole)
        -> std::result::Result<(Self, crate::SqliteRoleMutationOutcome), OriginalSchedulerRetirementFailure> { self.mutate(role, false) }
    pub fn purge(self, role: crate::SqliteRole)
        -> std::result::Result<(Self, crate::SqliteRoleMutationOutcome), OriginalSchedulerRetirementFailure> { self.mutate(role, true) }
    /// Observe actual retained role/failure coordinates under the same paid
    /// cleanup authority. These facts do not authorize namespace mutation.
    pub fn observe_role_resolution(mut self, role: crate::SqliteRole)
        -> std::result::Result<(Self, crate::SqliteRoleResolutionObservation), OriginalSchedulerRetirementFailure> {
        let result = self.effect(OriginalSchedulerRetirementEffect::ObserveResolution(role), true, |body| {
            let Some(OriginalSchedulerRetirementPhase::Drained(owner)) = body.phase.as_mut() else {
                anyhow::bail!("resolution observation requires actual drained retirement owner")
            };
            owner.observe_role_resolution(role)
        });
        match result { Ok(observation) => Ok((self, observation)), Err(error) => Err(self.failure(error)) }
    }
    pub fn resolve(mut self, request: crate::SqliteRoleResolutionRequest)
        -> std::result::Result<(Self, crate::SqliteRoleResolutionOutcome), OriginalSchedulerRetirementFailure> {
        let result = self.effect(OriginalSchedulerRetirementEffect::Resolve(request.expected.role), true, |body| {
            let Some(OriginalSchedulerRetirementPhase::Drained(owner)) = body.phase.as_mut() else {
                anyhow::bail!("resolution requires actual drained retirement owner")
            };
            owner.resolve_role(request).map_err(anyhow::Error::new)
        });
        match result { Ok(outcome) => Ok((self, outcome)), Err(error) => Err(self.failure(error)) }
    }
    pub fn release(self) -> std::result::Result<Arc<ClosedOriginalNativeConnection>, OriginalSchedulerRetirementFailure> { self.finish(false) }
    pub fn abort(self) -> std::result::Result<Arc<ClosedOriginalNativeConnection>, OriginalSchedulerRetirementFailure> { self.finish(true) }
    fn finish(mut self, abort: bool) -> std::result::Result<Arc<ClosedOriginalNativeConnection>, OriginalSchedulerRetirementFailure> {
        let result = (|| -> Result<()> {
            if !self.body.physical_release {
                self.effect(if abort { OriginalSchedulerRetirementEffect::Abort } else { OriginalSchedulerRetirementEffect::Release }, true, |body| {
                    if abort { Self::prepare_initial(body, true)?; }
                    let phase = body.phase.take().context("retirement phase absent")?;
                    let result = match phase {
                        OriginalSchedulerRetirementPhase::Drained(owner) => if abort { owner.abort() } else { owner.release() },
                        OriginalSchedulerRetirementPhase::Captured(owner) if abort => owner.abort(),
                        OriginalSchedulerRetirementPhase::Bound(owner) if abort => owner.close().map_err(|failure| {
                            let crate::BoundSqliteCloseFailure { owner, error } = failure;
                            crate::SqliteProjectionRetirementFailure {
                                owner: crate::SqliteProjectionRetirementOwner::Releasing(owner),
                                error: crate::SqliteProjectionRetirementError::NativeRelease(error),
                            }
                        }),
                        OriginalSchedulerRetirementPhase::Failed(failure) => if abort { failure.abort() } else { failure.retry_release() },
                        other => { body.phase = Some(other); anyhow::bail!("invalid retirement release phase") }
                    };
                    match result {
                        Ok(()) => {
                            body.physical_release = true;
                            body.phase = Some(OriginalSchedulerRetirementPhase::Released); Ok(())
                        }
                        Err(failure) => {
                            body.phase = Some(OriginalSchedulerRetirementPhase::Failed(failure));
                            anyhow::bail!("scheduler retirement physical release retained")
                        }
                    }
                })?;
            }
            self.effect(OriginalSchedulerRetirementEffect::Terminal, true, |body| {
                ensure!(body.physical_release && body.sql_consumed && body.c_sql_cleared,
                    "scheduler retirement terminal lacks actual complete release");
                if body.terminal.is_none() {
                    let receipt = Arc::new(ClosedOriginalNativeConnection {
                        factory: body.factory().clone(), ticket: body.ticket,
                        domain: OriginalNativeDomain::NodeScheduler, body: ClosedNativeBody::Plain,
                        _scheduler_retirement: Some(OriginalSchedulerRetirementInputs {
                            generation: body.inputs.generation,
                            authority: body.inputs.authority.clone(),
                        }),
                        bound_identity: body.bound_identity,
                        destination_authority: destination_authority_witness(&body.destination),
                        destination: Mutex::new(body.destination.take()), durable_body: OnceLock::new(),
                        durable_finalizer: AtomicBool::new(false),
                    });
                    // Actual receipt and destination custody survive every
                    // subsequent ACK/census/authority refusal. It stays private
                    // until the SAME original Rust ticket is really terminal.
                    body.terminal = Some(receipt);
                }
                body.producer.as_ref().context("retirement producer absent")?.acknowledge_native_terminal()?;
                body.factory().mark_closed(body.ticket, false)
            })
        })();
        if let Err(error) = result { return Err(self.failure(error)); }
        let receipt = self.body.get_mut().terminal.take().expect("actual retained retirement terminal");
        // Explicit successful physical release and terminal completed; only
        // Rust custody is disposed here, with no automatic native close.
        match self.body.take_whole() {
            Ok(body) => {
                drop(body);
                Ok(receipt)
            }
            Err(body) => {
                self.body = body;
                self.body.get_mut().terminal = Some(receipt);
                Err(self.failure(anyhow!("scheduler retirement belongs to another process")))
            }
        }
    }
}

const NATIVE_CLOSE_FAILURE_WRAPPER_KIND: &str = "lillux-sqlite::native-close-failure-wrapper";
const NATIVE_CLOSE_OWNER_KIND: &str = crate::AbandonedSqliteOwnerKind::NativeCloseFailure.custody_kind();
/// A failed close keeps every field here so a copy in another process can give
/// up the whole failure at once. Settled only while this process holds its
/// factory: then its fields drop normally while a still-present close owner retains itself,
/// counted, through its own custody; otherwise the whole failure is retained
/// and counted, never dropped.
fn native_close_failure_parts_released(parts: &OriginalNativeCloseFailureParts) -> bool {
    factory_held_here(&parts.factory)
}
pub struct OriginalNativeCloseFailure {
    parts: lillux::custody::Held<OriginalNativeCloseFailureParts>,
}
struct OriginalNativeCloseFailureParts {
    bound_identity: Option<(u64, u64)>,
    error: Option<anyhow::Error>,
    factory: Arc<OriginalNativeFactory>,
    ticket: u32,
    domain: OriginalNativeDomain,
    producer: Option<SqliteInspectionProductionPermit>,
    owner: Mutex<Option<lillux::custody::Held<NativeCloseOwner>>>,
    destination: Option<OriginalNativeDestinationInputs>,
}
fn close_owner_slot(owner: Option<NativeCloseOwner>) -> Mutex<Option<lillux::custody::Held<NativeCloseOwner>>> {
    Mutex::new(owner.map(|owner| lillux::custody::Held::until_taken(NATIVE_CLOSE_OWNER_KIND, owner)))
}
impl OriginalNativeCloseFailure {
    pub fn error(&self) -> Option<&anyhow::Error> {
        self.parts.error.as_ref()
    }
    fn retain_close_error(&mut self, error: anyhow::Error) {
        self.parts.get_mut().error = Some(match self.parts.get_mut().error.take() {
            Some(earlier) => error.superseding(earlier),
            None => error,
        });
    }
    pub fn retry(mut self) -> std::result::Result<Arc<ClosedOriginalNativeConnection>, Self> {
        let result = (|| -> Result<ClosedNativeBody> {
            let parts = self.parts.get_mut();
            parts.factory.boundary(BoundaryMode::Cleanup, || Ok(()))?;
            let _scope = parts.factory.arena()?.scope(true, 0)?;
            // Snapshot from the genuine live Bound keeper before taking it.
            // Failed metadata validation retains the actual Live owner.
            if parts.bound_identity.is_none() {
                if let Some(NativeCloseOwner::Live(NativeConnectionBody::Bound(owner))) = slot_peek(&mut parts.owner)
                {
                    owner
                        .binding
                        .as_ref()
                        .context("original bound keeper missing")?
                        .validate(owner.connection(), false)?;
                    parts.bound_identity = Some(owner.binding.as_ref().unwrap().main_identity()?);
                }
            }
            let owner = slot_take(&mut parts.owner)
                .context("original native close owner missing")?;
            match owner {
                NativeCloseOwner::Closed(body) => Ok(body),
                NativeCloseOwner::Live(body) => match body.close() {
                    Ok(body) => Ok(body),
                    Err(owner) => {
                        slot_put(&mut parts.owner, NATIVE_CLOSE_OWNER_KIND, owner);
                        anyhow::bail!("original native close pending")
                    }
                },
                NativeCloseOwner::Transient(c, _) => match c.close() {
                    Ok(()) => Ok(ClosedNativeBody::Plain),
                    Err((c, e)) => {
                        slot_put(&mut parts.owner, NATIVE_CLOSE_OWNER_KIND, NativeCloseOwner::Transient(c, e));
                        anyhow::bail!("original transient close pending")
                    }
                },
                NativeCloseOwner::Bound(f) => match f.retry() {
                    Ok(()) => Ok(ClosedNativeBody::Plain),
                    Err(f) => {
                        slot_put(&mut parts.owner, NATIVE_CLOSE_OWNER_KIND, NativeCloseOwner::Bound(f));
                        anyhow::bail!("original bound close pending")
                    }
                },
                NativeCloseOwner::Inspection(f) => match f.retry_close() {
                    Ok(p) => Ok(ClosedNativeBody::Inspection(Arc::new(p))),
                    Err(f) => {
                        slot_put(&mut parts.owner, NATIVE_CLOSE_OWNER_KIND, NativeCloseOwner::Inspection(f));
                        anyhow::bail!("original inspection close pending")
                    }
                },
            }
        })();
        match result {
            Ok(body) => {
                slot_put(&mut self.parts.get_mut().owner, NATIVE_CLOSE_OWNER_KIND, NativeCloseOwner::Closed(body));
                let acknowledged = (|| -> Result<()> {
                    if let Some(NativeCloseOwner::Closed(ClosedNativeBody::Inspection(proof))) =
                        slot_peek(&mut self.parts.get_mut().owner)
                    {
                        proof.acknowledge_producer_terminal()?;
                    }
                    self.parts.producer
                        .as_ref()
                        .context("native connection producer missing")?
                        .acknowledge_native_terminal()
                })();
                if let Err(error) = acknowledged {
                    self.retain_close_error(error);
                    return Err(self);
                }
                if let Err(error) = self.parts.factory.mark_closed(self.parts.ticket, false) {
                    self.retain_close_error(error);
                    return Err(self);
                }
                let Some(NativeCloseOwner::Closed(body)) = slot_take(&mut self.parts.get_mut().owner)
                else {
                    unreachable!()
                };
                Ok(Arc::new(ClosedOriginalNativeConnection {
                    durable_body: OnceLock::new(),
                    durable_finalizer: AtomicBool::new(false),
                    bound_identity: self.parts.bound_identity,
                    factory: self.parts.factory.clone(),
                    ticket: self.parts.ticket,
                    domain: self.parts.domain,
                    body,
                    destination_authority: destination_authority_witness(&self.parts.destination),
                    destination: Mutex::new(self.parts.get_mut().destination.take()),
                    _scheduler_retirement: None,
                }))
            }
            Err(error) => {
                self.retain_close_error(error);
                Err(self)
            }
        }
    }
}
impl std::fmt::Debug for OriginalNativeCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeCloseFailure")
            .field("error", &self.parts.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalNativeCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original native close remains pending")
    }
}
impl std::error::Error for OriginalNativeCloseFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.parts.error.as_ref().map(|error| error.as_ref())
    }
}

const NATIVE_OPEN_FAILURE_WRAPPER_KIND: &str = "lillux-sqlite::native-open-failure-wrapper";
const NATIVE_OPEN_OWNER_KIND: &str = crate::AbandonedSqliteOwnerKind::NativeOpenFailure.custody_kind();
/// A failed open keeps every field here so a copy in another process can give
/// up the whole failure at once. Settled only while this process holds its
/// factory: then its fields drop normally while a still-present open owner retains itself,
/// counted, through its own custody; otherwise the whole failure is retained
/// and counted, never dropped.
fn native_open_failure_parts_released(parts: &OriginalNativeOpenFailureParts) -> bool {
    factory_held_here(&parts.factory)
}
pub struct OriginalNativeOpenFailure {
    parts: lillux::custody::Held<OriginalNativeOpenFailureParts>,
}
fn open_owner_slot(owner: Option<NativeOpenOwner>) -> Mutex<Option<lillux::custody::Held<NativeOpenOwner>>> {
    Mutex::new(owner.map(|owner| lillux::custody::Held::until_taken(NATIVE_OPEN_OWNER_KIND, owner)))
}
/// Look at an owner slot's content through exclusive access.
fn slot_peek<O: Send + 'static>(slot: &mut Mutex<Option<lillux::custody::Held<O>>>) -> Option<&O> {
    slot.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner).as_deref()
}
/// Take an owner out of its slot. A copy in another process is refused: the
/// owner stays in its slot and nothing is returned.
fn slot_take<O: Send + 'static>(slot: &mut Mutex<Option<lillux::custody::Held<O>>>) -> Option<O> {
    let cell = slot.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
    match cell.take()?.take_whole() {
        Ok(owner) => Some(owner),
        Err(held) => { *cell = Some(held); None }
    }
}
/// Put an owner into its slot, under the kind a retained end counts it as.
fn slot_put<O: Send + 'static>(
    slot: &mut Mutex<Option<lillux::custody::Held<O>>>, kind: &'static str, owner: O,
) {
    *slot.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(lillux::custody::Held::until_taken(kind, owner));
}
struct OriginalNativeOpenFailureParts {
    domain: OriginalNativeDomain,
    admission_failure: Option<OriginalNativeConnectionAdmissionFailure>,
    error: anyhow::Error,
    factory: Arc<OriginalNativeFactory>,
    ticket: Option<u32>,
    producer: Option<SqliteInspectionProductionPermit>,
    owner: Mutex<Option<lillux::custody::Held<NativeOpenOwner>>>,
    destination: Option<OriginalNativeDestinationInputs>,
}
pub struct ReleasedOriginalNativeAcquisition {
    domain: Option<OriginalNativeDomain>,
    factory: Arc<OriginalNativeFactory>,
    ticket: Option<u32>,
    original_error: anyhow::Error,
    inspection: Option<Arc<ReleasedSqliteInspection>>,
    closed_inspection: Option<Arc<ClosedSqliteInspection>>,
    // Moves out once, by a take-capture or the consuming outside handoff.
    destination: Mutex<Option<OriginalNativeDestinationInputs>>,
    destination_authority: Option<std::sync::Weak<dyn OriginalNativeDestinationAuthority>>,
}
impl ReleasedOriginalNativeAcquisition {
    /// Copied original purpose observation only; never ticket/native/physical
    /// cleanup authority. Original process validation precedes access.
    pub fn original_domain(&self) -> Result<Option<OriginalNativeDomain>> {
        self.factory.require_original_process()?;
        Ok(self.domain)
    }

    /// Issued ticket fact only; before-admission refusals are not members of
    /// the factory's exhaustive native ticket census.
    pub fn native_ticket(&self) -> Option<u32> {
        self.ticket
    }
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        require_current_native_process()?;
        ensure!(
            Arc::ptr_eq(&self.factory, factory),
            "foreign native acquisition release"
        );
        if let Some(ticket) = self.ticket {
            ensure!(
                self.factory.storage()?.tickets[ticket as usize - 1].load(Ordering::Acquire) == 3,
                "native acquisition remains pending"
            );
        }
        Ok(())
    }
    pub fn original_error(&self) -> &anyhow::Error {
        &self.original_error
    }
    pub fn inspection_closed(&self) -> Option<Arc<ClosedSqliteInspection>> {
        self.closed_inspection.clone()
    }
    pub fn inspection_released(&self) -> Option<Arc<ReleasedSqliteInspection>> {
        self.inspection.clone()
    }
    /// Whether this release still owns its original destination inputs.
    pub fn retains_destination_inputs(&self) -> Result<bool> {
        Ok(lock_destination(&self.destination)?.is_some())
    }
    /// Move the destination inputs out of this shared release, once, with
    /// the same consuming semantics as the closed-receipt capture. Ticketless
    /// releases are checked by factory identity only.
    pub fn take_destination_inputs(
        self: &Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &OriginalNativeTerminal,
    ) -> Result<Option<(OriginalNativeOutsideLeafEvidence, OriginalNativeDestinationInputs)>> {
        require_current_native_process()?;
        terminal.require_original(factory)?;
        self.require_original(factory)?;
        let mut slot = lock_destination(&self.destination)?;
        let Some(inputs) = slot.take() else { return Ok(None) };
        Ok(Some((OriginalNativeOutsideLeafEvidence {
            process: std::process::id(),
            identity: OriginalNativeOutsideLeafIdentity::Released(Arc::downgrade(self)),
            factory: Arc::downgrade(&self.factory),
            authority: self.destination_authority.clone(),
            ticket: self.ticket,
            domain: self.domain,
            closed: false,
        }, inputs)))
    }
}
impl OriginalNativeOpenFailure {
    pub fn recover_retained_admission(&mut self) -> Result<()> {
        self.parts.factory.require_original(true)?;
        ensure!(self.parts.ticket.is_none() && self.parts.producer.is_none()
            && slot_peek(&mut self.parts.get_mut().owner).is_none(),
            "SQL admission recovery conflicts with actual native owner");
        let failure = self.parts.get_mut().admission_failure.take()
            .context("actual retained SQL admission absent")?;
        let original = &failure.inner.original;
        let result = crate::original_physical_custody::retain_callback_refusal(&|| {
            self.parts.factory.recover_undelivered_admission(
                &original.budget, original.ticket, &original.producer,
                SqliteInspectionProductionKind::Native)
        });
        if let Err(error) = result {
            self.parts.get_mut().admission_failure = Some(failure);
            return Err(error);
        }
        let (original, error) = match failure.into_fields() {
            Ok(fields) => fields,
            Err(failure) => {
                self.parts.get_mut().admission_failure = Some(failure);
                anyhow::bail!("SQL admission failure belongs to another process");
            }
        };
        // The recovered admission's own refusal is the cause the open
        // refusal deferred; it supersedes that placeholder refusal.
        let parts = self.parts.get_mut();
        parts.error = error.superseding(std::mem::replace(&mut parts.error,
            anyhow!("original admission error transferred")));
        parts.ticket = Some(original.ticket);
        parts.producer = Some(original.producer);
        Ok(())
    }
    pub fn retained_admission(&self) -> Option<&OriginalNativeConnectionAdmissionFailure> {
        if self.parts.factory.require_process().is_err() { return None; }
        self.parts.admission_failure.as_ref()
    }
    pub fn error(&self) -> &anyhow::Error {
        &self.parts.error
    }
    pub fn retry_close(
        mut self,
    ) -> std::result::Result<Arc<ReleasedOriginalNativeAcquisition>, Self> {
        let result = (|| -> Result<()> {
            let parts = self.parts.get_mut();
            parts.factory.require_original(true)?;
            ensure!(parts.admission_failure.is_none(),
                "actual published SQL admission unresolved; no close or no-ticket terminal");
            let _scope = parts.factory.arena()?.scope(true, 0)?;
            if let Some(owner) = slot_take(&mut parts.owner)
            {
                let closed = match owner {
                    NativeOpenOwner::Released(p) => {
                        slot_put(&mut parts.owner, NATIVE_OPEN_OWNER_KIND, NativeOpenOwner::Released(p));
                        None
                    }
                    NativeOpenOwner::Closed(body) => Some(body),
                    NativeOpenOwner::Bound(f) => match f.retry_close() {
                        Ok(()) => Some(ClosedNativeBody::Plain),
                        Err(f) => {
                            slot_put(&mut parts.owner, NATIVE_OPEN_OWNER_KIND, NativeOpenOwner::Bound(f));
                            anyhow::bail!("original partial bound retained")
                        }
                    },
                    NativeOpenOwner::Inspection(f) => match f.retry_close() {
                        Ok(p) => {
                            slot_put(&mut parts.owner, NATIVE_OPEN_OWNER_KIND, NativeOpenOwner::Released(Arc::new(p)));
                            None
                        }
                        Err(f) => {
                            slot_put(&mut parts.owner, NATIVE_OPEN_OWNER_KIND, NativeOpenOwner::Inspection(f));
                            anyhow::bail!("original partial inspection retained")
                        }
                    },
                    NativeOpenOwner::Live(body) => match body.close() {
                        Ok(body) => Some(body),
                        Err(owner) => {
                            slot_put(&mut parts.owner, NATIVE_OPEN_OWNER_KIND, NativeOpenOwner::Closing(owner));
                            anyhow::bail!("factory acquisition close retained")
                        }
                    },
                    NativeOpenOwner::Closing(owner) => {
                        let result = match owner {
                            NativeCloseOwner::Live(body) => body.close(),
                            NativeCloseOwner::Closed(body) => Ok(body),
                            NativeCloseOwner::Transient(c, _) => match c.close() {
                                Ok(()) => Ok(ClosedNativeBody::Plain),
                                Err((c, e)) => Err(NativeCloseOwner::Transient(c, e)),
                            },
                            NativeCloseOwner::Bound(f) => match f.retry() {
                                Ok(()) => Ok(ClosedNativeBody::Plain),
                                Err(f) => Err(NativeCloseOwner::Bound(f)),
                            },
                            NativeCloseOwner::Inspection(f) => match f.retry_close() {
                                Ok(p) => Ok(ClosedNativeBody::Inspection(Arc::new(p))),
                                Err(f) => Err(NativeCloseOwner::Inspection(f)),
                            },
                        };
                        match result {
                            Ok(body) => Some(body),
                            Err(owner) => {
                                slot_put(&mut parts.owner, NATIVE_OPEN_OWNER_KIND, NativeOpenOwner::Closing(owner));
                                anyhow::bail!("original acquisition close retry retained")
                            }
                        }
                    }
                };
                if let Some(body) = closed {
                    slot_put(&mut parts.owner, NATIVE_OPEN_OWNER_KIND, NativeOpenOwner::Closed(body));
                }
            }
            if let Some(NativeOpenOwner::Released(proof)) = slot_peek(&mut parts.owner)
            {
                proof.acknowledge_producer_terminal()?;
            }
            if let Some(NativeOpenOwner::Closed(ClosedNativeBody::Inspection(proof))) = slot_peek(&mut parts.owner)
            {
                proof.acknowledge_producer_terminal()?;
            }
            if let Some(ticket) = parts.ticket {
                if parts.factory.storage()?.tickets[ticket as usize - 1].load(Ordering::Acquire) != 3 {
                    parts.factory.budget.debit_steps(1, true)?;
                    parts.factory.require_original(true)?;
                    parts.factory.mark_closed(ticket, true)?;
                }
            }
            if let Some(producer) = &parts.producer {
                producer.acknowledge_native_terminal()?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                let (inspection, closed_inspection) = match slot_take(&mut self.parts.get_mut().owner)
                {
                    Some(NativeOpenOwner::Released(p)) => (Some(p), None),
                    Some(NativeOpenOwner::Closed(ClosedNativeBody::Inspection(p))) => {
                        (None, Some(p))
                    }
                    Some(NativeOpenOwner::Closed(ClosedNativeBody::Plain)) | None => (None, None),
                    _ => unreachable!(),
                };
                Ok(Arc::new(ReleasedOriginalNativeAcquisition {
                    domain: Some(self.parts.domain),
                    factory: self.parts.factory.clone(),
                    ticket: self.parts.ticket,
                    original_error: std::mem::replace(
                        &mut self.parts.get_mut().error,
                        anyhow::anyhow!("original error transferred"),
                    ),
                    inspection,
                    closed_inspection,
                    destination_authority: destination_authority_witness(&self.parts.destination),
                    destination: Mutex::new(self.parts.get_mut().destination.take()),
                }))
            }
            Err(error) => {
                let parts = self.parts.get_mut();
                parts.error = error.superseding(
                    std::mem::replace(&mut parts.error, anyhow::anyhow!("original error retained")));
                Err(self)
            }
        }
    }
}
impl std::fmt::Debug for OriginalNativeOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeOpenFailure")
            .field("error", &self.parts.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalNativeOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.parts.error, f)
    }
}
impl std::error::Error for OriginalNativeOpenFailure {}

#[derive(Clone)]
pub enum OriginalNativeConnectionTerminal {
    Closed(Arc<ClosedOriginalNativeConnection>),
    Released(Arc<ReleasedOriginalNativeAcquisition>),
}
/// Concrete resource owners moved out of an actual native terminal. Retaining
/// this packet is not descriptor disposal or proof of zero region references:
/// its destination/inspection authorities can still own native-affinity edges.
/// No Drop disposes these original resources or acknowledges an account.
pub struct OriginalNativeOutsideConnectionCustody {
    parts: lillux::custody::Held<OriginalNativeOutsideConnectionParts>,
}
const NATIVE_OUTSIDE_CONNECTION_KIND: &str = "lillux-sqlite::native-outside-connection-custody";
/// Private evidence captured ONLY from an actual consuming Closed/Released
/// handoff, or borrowed from a live closed-connection Arc for its durable
/// metadata-pin disposition. Weak identities refer to real outside
/// allocations, never mapped payload; copied ticket/domain/outcome are facts,
/// not an admission/grant.
#[derive(Clone)]
enum OriginalNativeOutsideLeafIdentity {
    Closed(std::sync::Weak<ClosedOriginalNativeConnection>),
    Released(std::sync::Weak<ReleasedOriginalNativeAcquisition>),
}
/// A consuming capture enters outside-custody leaf sets and `same_original_leaf`
/// comparisons. A `take_destination_inputs` capture is consuming for the
/// inputs it moves; that take is one-shot, so a later outside handoff of the
/// same allocation carries no destination and cannot add a second leaf. A
/// borrowed capture from `retained_durable_metadata_leaf` only binds
/// metadata-pin retirement inputs: the connection stays with its owner, so it
/// is never a consumed-leaf receipt and must not join those sets.
#[derive(Clone)]
pub struct OriginalNativeOutsideLeafEvidence {
    process: u32,
    identity: OriginalNativeOutsideLeafIdentity,
    factory: std::sync::Weak<OriginalNativeFactory>,
    authority: Option<std::sync::Weak<dyn OriginalNativeDestinationAuthority>>,
    ticket: Option<u32>,
    domain: Option<OriginalNativeDomain>,
    closed: bool,
}
impl OriginalNativeOutsideLeafEvidence {
    /// Compare exact original consumed receipt allocations, including
    /// ticketless refusals. The retained Weak identities prevent allocation
    /// reuse from equating unrelated leaves; nullness is never used as proof.
    pub fn same_original_leaf(&self, other: &Self) -> Result<bool> {
        ensure!(!crate::foreign_process(self.process) && other.process == self.process,
            "foreign original leaf identity comparison");
        Ok(match (&self.identity, &other.identity) {
            (OriginalNativeOutsideLeafIdentity::Closed(a), OriginalNativeOutsideLeafIdentity::Closed(b)) => std::sync::Weak::ptr_eq(a, b),
            (OriginalNativeOutsideLeafIdentity::Released(a), OriginalNativeOutsideLeafIdentity::Released(b)) => std::sync::Weak::ptr_eq(a, b),
            _ => false,
        })
    }
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>, aggregate: &OriginalNativeTerminal) -> Result<()> {
        ensure!(!crate::foreign_process(self.process), "foreign original outside leaf evidence");
        factory.require_process()?;
        aggregate.require_original(factory)?;
        ensure!(std::sync::Weak::ptr_eq(&self.factory, &Arc::downgrade(factory)),
            "outside leaf evidence selects another actual factory allocation");
        if let Some(ticket) = self.ticket {
            ensure!(ticket > 0 && ticket as usize <= MAX_NATIVE_CONNECTIONS,
                "outside leaf has invalid original ticket");
            let phase = factory.storage()?.tickets[ticket as usize - 1].load(Ordering::Acquire);
            ensure!(phase == if self.closed { 2 } else { 3 }, "outside leaf genuine native disposition changed");
        } else {
            ensure!(!self.closed, "actual closed leaf cannot lack its original ticket");
        }
        Ok(())
    }
    pub fn require_original_authority(&self, authority: &Arc<dyn OriginalNativeDestinationAuthority>,
        factory: &Arc<OriginalNativeFactory>, aggregate: &OriginalNativeTerminal) -> Result<()> {
        self.require_original(factory, aggregate)?;
        ensure!(self.authority.as_ref().is_some_and(|original|
            std::sync::Weak::ptr_eq(original, &Arc::downgrade(authority))),
            "outside leaf selects another actual destination authority");
        Ok(())
    }
    pub fn native_ticket(&self) -> Result<Option<u32>> {
        ensure!(!crate::foreign_process(self.process), "foreign original leaf observation"); Ok(self.ticket)
    }
    pub fn domain(&self) -> Result<Option<OriginalNativeDomain>> {
        ensure!(!crate::foreign_process(self.process), "foreign original leaf observation"); Ok(self.domain)
    }
}
pub struct OriginalNativeOutsideConnectionParts {
    pub leaf_evidence: OriginalNativeOutsideLeafEvidence,
    pub ticket: Option<u32>,
    pub domain: Option<OriginalNativeDomain>,
    pub bound_inputs: Option<OriginalNativeBoundInputs>,
    pub transient: Option<Arc<dyn OriginalNativeDestinationAuthority>>,
    pub scheduler_retirement: Option<OriginalSchedulerRetirementInputs>,
    pub inspection: Option<Arc<ClosedSqliteInspection>>,
    pub released_inspection: Option<Arc<ReleasedSqliteInspection>>,
    pub durable_pins: [Option<Arc<RetainedOriginalMetadataPin>>; 2],
    pub original_error: Option<anyhow::Error>,
}
impl OriginalNativeOutsideConnectionCustody {
    /// Consumes the actual packet into accessible original resource owners.
    /// The outside supervisor must preserve them and retire their real affinity
    /// and physical obligations explicitly; none of these facts issues a grant.
    ///
    /// Only this hands the packet to its outside supervisor. Any other end
    /// retains its descriptor, inspection and destination owners, counted,
    /// until the process exits; a copy in another process is refused here and
    /// relinquished on its own end.
    pub fn into_original_parts(self) -> std::result::Result<OriginalNativeOutsideConnectionParts, Self> {
        self.parts.take_whole().map_err(|parts| Self { parts })
    }
}
const NATIVE_OUTSIDE_CUSTODY_FAILURE_KIND: &str = "lillux-sqlite::native-outside-custody-failure";
/// The terminal of a refused outside handoff and the refusal. Constructible
/// only in this module.
struct OutsideCustodyFailureParts {
    error: anyhow::Error,
    original: OriginalNativeConnectionTerminal,
}
/// Only `into_original` settles a refused outside handoff. Any other end
/// retains the terminal, counted, with its input and descriptor owners until
/// the process exits.
pub struct OriginalNativeOutsideCustodyFailure {
    inner: lillux::custody::Held<OutsideCustodyFailureParts>,
}
impl OriginalNativeOutsideCustodyFailure {
    fn new(error: anyhow::Error, original: OriginalNativeConnectionTerminal) -> Self {
        Self {
            inner: lillux::custody::Held::until_taken(
                NATIVE_OUTSIDE_CUSTODY_FAILURE_KIND, OutsideCustodyFailureParts { error, original }),
        }
    }
    pub fn into_original(self) -> std::result::Result<OriginalNativeConnectionTerminal, Self> {
        let factory = match &self.inner.original {
            OriginalNativeConnectionTerminal::Closed(owner) => &owner.factory,
            OriginalNativeConnectionTerminal::Released(owner) => &owner.factory,
        };
        if factory.require_process().is_err() { return Err(self); }
        match self.inner.take_whole() {
            Ok(parts) => {
                let OutsideCustodyFailureParts { error, original } = parts;
                drop(error);
                Ok(original)
            }
            Err(inner) => Err(Self { inner }),
        }
    }
}
impl std::fmt::Debug for OriginalNativeOutsideCustodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeOutsideCustodyFailure").field("error", &self.inner.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalNativeOutsideCustodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { std::fmt::Display::fmt(&self.inner.error, f) }
}
impl std::error::Error for OriginalNativeOutsideCustodyFailure {}
impl OriginalNativeConnectionTerminal {
    /// The connection ticket this terminal ends, or `None` for an acquisition
    /// refused before any ticket was debited. Only ticketed terminals belong
    /// to the backend finish census.
    pub fn ticket(&self) -> Option<u32> {
        match self {
            Self::Closed(p) => Some(p.ticket),
            Self::Released(p) => p.ticket,
        }
    }
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        require_current_native_process()?;
        match self {
            Self::Closed(proof) => proof.require_original(factory),
            Self::Released(proof) => proof.require_original(factory),
        }
    }
    /// Move actual input/descriptor owners only after the genuine aggregate
    /// native terminal and unique consuming receipt. Arc uniqueness is not the
    /// native proof: exact issuer ACK and ticket disposition are checked first.
    pub fn into_outside_custody(self, terminal: &OriginalNativeTerminal)
        -> std::result::Result<OriginalNativeOutsideConnectionCustody, OriginalNativeOutsideCustodyFailure> {
        let (factory, _) = self.original();
        if let Err(error) = terminal.require_original(factory).and_then(|()| self.require_original(factory)) {
            return Err(OriginalNativeOutsideCustodyFailure::new(error, self));
        }
        // After a take-capture the inputs already joined a receiver; this
        // allocation must not report a destination-free handoff.
        let taken = match &self {
            Self::Closed(owner) => owner.destination_authority.is_some()
                && lock_destination(&owner.destination).map_or(true, |slot| slot.is_none()),
            Self::Released(owner) => owner.destination_authority.is_some()
                && lock_destination(&owner.destination).map_or(true, |slot| slot.is_none()),
        };
        if taken {
            return Err(OriginalNativeOutsideCustodyFailure::new(
                anyhow::anyhow!("original destination inputs were taken or are busy"), self));
        }
        let process = std::process::id();
        let parts = match self {
            Self::Closed(owner) => {
                let identity = OriginalNativeOutsideLeafIdentity::Closed(Arc::downgrade(&owner));
                let owner = match Arc::try_unwrap(owner) {
                    Ok(owner) => owner,
                    Err(owner) => return Err(OriginalNativeOutsideCustodyFailure::new(
                        anyhow::anyhow!("actual closed receipt still has aliases"), Self::Closed(owner))),
                };
                let ClosedOriginalNativeConnection { durable_body, durable_finalizer: _, bound_identity: _,
                    factory, ticket, domain, body, destination, destination_authority,
                    _scheduler_retirement } = owner;
                let (_inputs, transient) = OriginalNativeDestinationInputs::into_fields(
                    destination.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner));
                let durable_pins = match durable_body.into_inner() {
                    Some(body) => [Some(body.main.clone()), body.wal.clone()],
                    None => [None, None],
                };
                let inspection = match body { ClosedNativeBody::Plain => None, ClosedNativeBody::Inspection(owner) => Some(owner) };
                // Only the direct native facade edge ends here. Every original
                // descriptor/authority owner was moved intact into this packet.
                let leaf_evidence = OriginalNativeOutsideLeafEvidence { process, identity,
                    factory: Arc::downgrade(&factory), authority: destination_authority,
                    ticket: Some(ticket), domain: Some(domain), closed: true };
                drop(factory);
                OriginalNativeOutsideConnectionParts { leaf_evidence, ticket: Some(ticket), domain: Some(domain),
                    bound_inputs: _inputs, transient, scheduler_retirement: _scheduler_retirement,
                    inspection, released_inspection: None, durable_pins, original_error: None }
            }
            Self::Released(owner) => {
                let identity = OriginalNativeOutsideLeafIdentity::Released(Arc::downgrade(&owner));
                let owner = match Arc::try_unwrap(owner) {
                    Ok(owner) => owner,
                    Err(owner) => return Err(OriginalNativeOutsideCustodyFailure::new(
                        anyhow::anyhow!("actual released receipt still has aliases"), Self::Released(owner))),
                };
                let ReleasedOriginalNativeAcquisition { domain, factory, ticket, original_error,
                    inspection, closed_inspection, destination, destination_authority } = owner;
                let (_inputs, transient) = OriginalNativeDestinationInputs::into_fields(
                    destination.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner));
                let leaf_evidence = OriginalNativeOutsideLeafEvidence { process, identity,
                    factory: Arc::downgrade(&factory), authority: destination_authority,
                    ticket, domain, closed: false };
                drop(factory);
                OriginalNativeOutsideConnectionParts { leaf_evidence, ticket, domain, bound_inputs: _inputs,
                    transient, scheduler_retirement: None, inspection: closed_inspection,
                    released_inspection: inspection, durable_pins: [None, None], original_error: Some(original_error) }
            }
        };
        Ok(OriginalNativeOutsideConnectionCustody {
            parts: lillux::custody::Held::until_taken(NATIVE_OUTSIDE_CONNECTION_KIND, parts),
        })
    }
    fn original(&self) -> (&Arc<OriginalNativeFactory>, u32) {
        match self {
            Self::Closed(p) => (&p.factory, p.ticket),
            Self::Released(p) => (&p.factory, p.ticket.unwrap_or(0)),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginalNativeArenaDisposition {
    NotAcquired,
    Released,
}
struct OriginalNativeTerminalBody {
    budget: OriginalNativeBudgetSnapshot,
    arena_disposition: OriginalNativeArenaDisposition,
    issuer_acknowledged: AtomicBool,
}
pub struct OriginalNativeTerminal {
    factory: Arc<OriginalNativeFactory>,
    body: NonNull<OriginalNativeTerminalBody>,
}
// The facade's counted region handle protects this exact immutable body;
// only the original terminal path mutates its atomic acknowledgement.
unsafe impl Send for OriginalNativeTerminal {}
unsafe impl Sync for OriginalNativeTerminal {}
impl OriginalNativeTerminal {
    fn body(&self) -> Result<&OriginalNativeTerminalBody> {
        self.factory.require_process()?;
        let actual = self.factory.storage()?.terminal_body.get().context("native terminal body absent")?;
        ensure!(NonNull::from(actual) == self.body, "native terminal body differs");
        Ok(actual)
    }
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        require_current_native_process()?;
        ensure!(Arc::ptr_eq(&self.factory, factory) && self.body()?.issuer_acknowledged.load(Ordering::Acquire),
            "native terminal selects foreign original scope");
        Ok(())
    }
    pub fn arena_disposition(&self) -> Result<OriginalNativeArenaDisposition> { Ok(self.body()?.arena_disposition) }
    pub fn budget_snapshot(&self) -> Result<OriginalNativeBudgetSnapshot> { Ok(self.body()?.budget.clone()) }
}

/// Authentic original cleanup funding and exact source-pin ownership. No
/// default deadline/debit or reconstructed path/count authority is supplied.
pub trait OriginalMetadataPinRetirementAuthority: Send + Sync {
    fn require_original(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<OriginalNativeTerminal>,
        pin: &Arc<RetainedOriginalMetadataPin>,
    ) -> Result<()>;
    fn debit_original_steps(&self, steps: u64) -> Result<()>;
}
/// Fixed native work charged for one original metadata-pin retirement attempt.
pub const ORIGINAL_METADATA_PIN_RETIREMENT_STEPS: u64 = 6;
/// Opaque trusted-host join; safe possession of a pin or native terminal alone
/// cannot replace the genuine prepared phase and original funding authority.
pub struct OriginalMetadataPinRetirementInputs {
    factory: Arc<OriginalNativeFactory>,
    terminal: Arc<OriginalNativeTerminal>,
    pins: Vec<Arc<RetainedOriginalMetadataPin>>,
    authority: Arc<dyn OriginalMetadataPinRetirementAuthority>,
}
impl OriginalMetadataPinRetirementInputs {
    /// # Safety
    /// Host must retain actual signed prepared evidence and original charged
    /// cleanup/clock custody, bind the genuine same factory/terminal and exact
    /// actual original permitted pin inventory, and prepay this owner storage.
    /// A permissive trait, decoded metadata, duplicate or new allowance is not
    /// an original authority. Every supplied pin must belong to that evidence.
    pub unsafe fn issued(
        factory: Arc<OriginalNativeFactory>,
        terminal: Arc<OriginalNativeTerminal>,
        pins: Vec<Arc<RetainedOriginalMetadataPin>>,
        authority: Arc<dyn OriginalMetadataPinRetirementAuthority>,
    ) -> Result<Self> {
        require_current_native_process()?;
        terminal.require_original(&factory)?;
        ensure!(
            !pins.is_empty() && pins.len() <= 13,
            "original metadata retirement inventory exceeds fixed source roles"
        );
        for (index, pin) in pins.iter().enumerate() {
            ensure!(
                !pins[..index].iter().any(|prior| Arc::ptr_eq(prior, pin)),
                "duplicate original metadata retirement owner"
            );
            authority.require_original(&factory, &terminal, pin)?;
        }
        Ok(Self {
            factory,
            terminal,
            pins,
            authority,
        })
    }
    fn require_original(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<OriginalNativeTerminal>,
        pin: &Arc<RetainedOriginalMetadataPin>,
    ) -> Result<()> {
        require_current_native_process()?;
        ensure!(
            Arc::ptr_eq(factory, &self.factory)
                && Arc::ptr_eq(terminal, &self.terminal)
                && self.pins.iter().any(|original| Arc::ptr_eq(original, pin)),
            "foreign original metadata retirement inputs"
        );
        terminal.require_original(factory)?;
        self.authority.require_original(factory, terminal, pin)
    }
}
const ORIGINAL_METADATA_PIN_KIND: &str = "lillux-sqlite::original-metadata-pin";
const ORIGINAL_FINALIZER_FILE_KIND: &str = "lillux-sqlite::original-finalizer-file";
/// An unretired descriptor is never closed from a destructor, whether or not
/// the native process check still passes: it stays retained, counted, until
/// the process exits. A pending numeric coordinate is already retained by its
/// consumed close path and owns no File, and a retired or consumed state holds
/// nothing, so those settle. A busy lock is unsettled; a poisoned one is read
/// through.
fn pin_state_settled(state: &Mutex<OriginalMetadataPinState>) -> bool {
    use std::sync::TryLockError;
    match state.try_lock() {
        Ok(state) => !matches!(*state, OriginalMetadataPinState::Open(_)),
        Err(TryLockError::Poisoned(poisoned)) => !matches!(*poisoned.into_inner(), OriginalMetadataPinState::Open(_)),
        Err(TryLockError::WouldBlock) => false,
    }
}
enum OriginalMetadataPinState {
    Open(std::fs::File),
    PendingRaw(std::os::fd::RawFd),
    Retired,
    ConsumedError { code: i32, errno: i32 },
}
/// Owns the actual original O_PATH descriptor, with no File/FD/dup escape.
/// Unretired Drop retains the descriptor; it never fabricates a close receipt.
pub struct RetainedOriginalMetadataPin {
    process: u32,
    state: lillux::custody::Held<Mutex<OriginalMetadataPinState>>,
    identity: OnceLock<lillux::OpenFileIdentity>,
    /// Whether a host release already reported this pin's consumed refusal.
    consumed_error_reported: AtomicBool,
}
/// How one durability pin ended at its host's release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurabilityPinClose {
    /// This call closed the descriptor.
    Closed,
    /// The close consumed the descriptor but reported an error. No
    /// descriptor is left to retain or retry, so it is a fact, not a refusal.
    ConsumedWithError,
    /// Another path closed it, or an earlier release attempt already
    /// reported it.
    NotClosedHere,
}
impl RetainedOriginalMetadataPin {
    /// A successful physical close memo can be observed before a late account
    /// callback finishes. Open/pending/error owners never supply a receipt.
    pub fn original_retirement_receipt(self: &Arc<Self>) -> Result<Option<RetiredOriginalMetadataPin>> {
        require_current_native_process()?;
        let state = self.state.try_lock()
            .map_err(|_| anyhow::anyhow!("actual metadata close observation busy or poisoned"))?;
        Ok(if matches!(*state, OriginalMetadataPinState::Retired) {
            Some(RetiredOriginalMetadataPin { owner: self.clone() })
        } else { None })
    }
    /// Kernel-private original durability operation. No public File/FD/dup
    /// escape; callers retain the actual factory effect/service authorization.
    fn with_original_file<T>(
        &self,
        operation: impl FnOnce(&std::fs::File) -> Result<T>,
    ) -> Result<T> {
        require_current_native_process()?;
        let state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original durability pin owner busy or poisoned"))?;
        let OriginalMetadataPinState::Open(file) = &*state else {
            anyhow::bail!("actual original durability pin retired/pending")
        };
        operation(file)
    }
    /// # Safety
    /// Transfer the actual original regular O_PATH metadata pin, never a
    /// duplicate, data descriptor or newly opened replacement.
    pub unsafe fn issued(file: std::fs::File) -> Arc<Self> {
        Arc::new(Self {
            process: std::process::id(),
            state: lillux::custody::Held::new(
                ORIGINAL_METADATA_PIN_KIND, Mutex::new(OriginalMetadataPinState::Open(file)), pin_state_settled),
            identity: OnceLock::new(),
            consumed_error_reported: AtomicBool::new(false),
        })
    }
    pub fn observed_identity(&self) -> Result<lillux::OpenFileIdentity> {
        require_current_native_process()?;
        let state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original metadata pin owner busy or poisoned"))?;
        let OriginalMetadataPinState::Open(file) = &*state else {
            anyhow::bail!("original metadata pin no longer open")
        };
        ensure!(
            file.metadata()?.is_file(),
            "original metadata pin is not regular"
        );
        let current = lillux::observe_open_file_identity(file)?;
        let original = self.identity.get_or_init(|| current);
        ensure!(
            current.device() == original.device() && current.inode() == original.inode(),
            "original metadata pin identity changed"
        );
        Ok(current)
    }
    pub fn retire_after_backend(
        self: &Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<OriginalNativeTerminal>,
        inputs: &OriginalMetadataPinRetirementInputs,
    ) -> Result<RetiredOriginalMetadataPin> {
        require_current_native_process()?;
        let result = self.retire_after_backend_owned(factory, terminal, inputs);
        result.map_err(|error| {
            anyhow::Error::new(OriginalMetadataPinRetirementFailure {
                error,
                owner: self.clone(),
            })
        })
    }
    fn retire_after_backend_owned(
        self: &Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<OriginalNativeTerminal>,
        inputs: &OriginalMetadataPinRetirementInputs,
    ) -> Result<RetiredOriginalMetadataPin> {
        require_current_native_process()?;
        terminal.require_original(factory)?;
        inputs.require_original(factory, terminal, self)?;
        // Flags, identity and one actual close have a fixed finite charge.
        // Host booking guards are released before descriptor-owner/native work.
        inputs.authority.debit_original_steps(ORIGINAL_METADATA_PIN_RETIREMENT_STEPS)?;
        terminal.require_original(factory)?;
        inputs.require_original(factory, terminal, self)?;
        self.close_original_descriptor()?;
        // Retain success before any late clock/funding check.
        inputs.require_original(factory, terminal, self)?;
        Ok(RetiredOriginalMetadataPin {
            owner: self.clone(),
        })
    }
    /// Close this pin at the end of its host. A close that consumed the
    /// descriptor with an error is reported once as such and the release
    /// goes on: the descriptor is gone, so refusing would only keep the
    /// host's region forever. Any refusal that keeps the descriptor stays a
    /// refusal.
    pub(crate) fn close_at_host_release(&self) -> Result<DurabilityPinClose> {
        let refusal = match self.close_original_descriptor() {
            Ok(true) => return Ok(DurabilityPinClose::Closed),
            Ok(false) => return Ok(DurabilityPinClose::NotClosedHere),
            Err(refusal) => refusal,
        };
        let consumed = match self.state.try_lock() {
            Ok(state) => matches!(&*state, OriginalMetadataPinState::ConsumedError { code: _, errno: _ }),
            Err(_) => return Err(refusal.context("original metadata close outcome busy or poisoned")),
        };
        if !consumed {
            return Err(refusal);
        }
        Ok(if self.consumed_error_reported.swap(true, Ordering::AcqRel) {
            DurabilityPinClose::NotClosedHere
        } else {
            DurabilityPinClose::ConsumedWithError
        })
    }
    /// The identity-checked close of this pin's descriptor, shared by every
    /// retirement path. Returns whether this call closed it; an already
    /// retired pin returns `false`. A consumed refusal is kept and refuses
    /// every later call; a refusal before the close keeps the descriptor.
    pub(crate) fn close_original_descriptor(&self) -> Result<bool> {
        require_current_native_process()?;
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original metadata retirement owner busy or poisoned"))?;
        if matches!(*state, OriginalMetadataPinState::Retired) {
            return Ok(false);
        }
        if let OriginalMetadataPinState::ConsumedError { code, errno } = &*state {
            anyhow::bail!("original metadata close consumed with retained refusal ({code}/{errno})")
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::IntoRawFd;
            if let OriginalMetadataPinState::Open(file) = &*state {
                ensure!(
                    file.metadata()?.is_file(),
                    "original metadata pin is not regular"
                );
                let current = lillux::observe_open_file_identity(file)?;
                let original = self.identity.get_or_init(|| current);
                ensure!(
                    current.device() == original.device() && current.inode() == original.inode(),
                    "original metadata retirement identity changed"
                );
                let OriginalMetadataPinState::Open(file) =
                    std::mem::replace(&mut *state, OriginalMetadataPinState::Retired)
                else {
                    unreachable!()
                };
                *state = OriginalMetadataPinState::PendingRaw(file.into_raw_fd());
            }
            let OriginalMetadataPinState::PendingRaw(fd) = &*state else {
                unreachable!()
            };
            let identity = self
                .identity
                .get()
                .context("original metadata identity absent")?;
            let result = crate::binding::close_original_metadata_pin(
                *fd,
                identity.device(),
                identity.inode(),
            );
            if result.consumed {
                *state = if result.code == libsqlite3_sys::SQLITE_OK && result.observed_errno == 0 {
                    OriginalMetadataPinState::Retired
                } else {
                    OriginalMetadataPinState::ConsumedError {
                        code: result.code,
                        errno: result.observed_errno,
                    }
                };
            }
            ensure!(
                result.consumed
                    && result.code == libsqlite3_sys::SQLITE_OK
                    && result.observed_errno == 0,
                "original metadata retirement pending/refused ({}/{}, consumed={})",
                result.code,
                result.observed_errno,
                result.consumed
            );
            drop(state);
            Ok(true)
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("original metadata retirement unavailable")
        }
    }
}
pub struct RetiredOriginalMetadataPin {
    owner: Arc<RetainedOriginalMetadataPin>,
}
impl RetiredOriginalMetadataPin {
    /// Observe only the SAME successful close's cached original identity.
    /// This is data evidence, not permission to close or reuse a descriptor.
    pub fn original_identity(&self) -> Result<lillux::OpenFileIdentity> {
        self.require_original(&self.owner)?;
        self.owner.identity.get().copied().context("actual retired metadata identity absent")
    }
    pub fn require_original(&self, owner: &Arc<RetainedOriginalMetadataPin>) -> Result<()> {
        require_current_native_process()?;
        ensure!(
            Arc::ptr_eq(owner, &self.owner)
                && matches!(
                    *owner.state.try_lock().map_err(|_| anyhow::anyhow!(
                        "original metadata receipt owner busy or poisoned"
                    ))?,
                    OriginalMetadataPinState::Retired
                ),
            "original metadata pin lacks actual same-owner close receipt"
        );
        Ok(())
    }
}

pub struct OriginalMetadataPinRetirementFailure {
    error: anyhow::Error,
    owner: Arc<RetainedOriginalMetadataPin>,
}
impl OriginalMetadataPinRetirementFailure {
    pub fn owner(&self) -> &Arc<RetainedOriginalMetadataPin> {
        &self.owner
    }
}
impl std::fmt::Debug for OriginalMetadataPinRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalMetadataPinRetirementFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalMetadataPinRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for OriginalMetadataPinRetirementFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Role describes the actual transferred original finalizer descriptor. A
/// close receipt proves that descriptor only, not aggregate lock/alias release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum OriginalFinalizerFileKind {
    /// Actual authenticated input/account descriptor, including a genuine
    /// duplicate whose own close must be observed. No SQLite/data-lock alias.
    ControlRecord = 0,
    Directory = 1,
    DirectoryLock = 2,
    StateLock = 3,
    /// Actual metadata-only (`O_PATH`) regular control pin. It can carry no
    /// data lock, so it closes through the metadata-pin path, which refuses
    /// any data descriptor; the data-descriptor path refuses this one.
    MetadataPin = 4,
}
/// Original outside descriptor-retirement authority, independent of mapped
/// SQLite state. The actual host supervisor retains its original account,
/// clock, operation and descriptor inventory through physical disposal.
/// # Safety
/// Implementations must revalidate the same authentic consuming host handoff
/// and exact private descriptor membership, irreversibly debit that original
/// outside cleanup owner, and preserve required lock/publication ordering.
/// A snapshot, matching inode, role or caller-provided vector is not authority.
/// Callbacks must never access a disposed resident/native graph.
pub unsafe trait OriginalOutsideFinalizerFileRetirementAuthority: Send + Sync {
    fn require_original(&self, owner: &Arc<RetainedOriginalFinalizerFile>) -> Result<()>;
    fn debit_original_steps(&self, steps: u64) -> Result<()>;
}
/// Retained original outside capability for ONE actual transferred descriptor.
/// Reuse charges the same original counter; it cannot renew a cleanup clock.
pub struct OriginalOutsideFinalizerFileRetirementInputs {
    process: u32,
    owner: Arc<RetainedOriginalFinalizerFile>,
    authority: Arc<dyn OriginalOutsideFinalizerFileRetirementAuthority>,
}
impl OriginalOutsideFinalizerFileRetirementInputs {
    /// # Safety
    /// The original consuming supervisor has retained authentic ACK/terminal
    /// effects and actual physical/account custody outside native storage.
    /// This is its exact transferred File and genuine original cleanup owner;
    /// prepay and cache this input before effects. All actual aliases, locks,
    /// fork ownership and publication ordering must be supervised. The grant
    /// proves neither native exclusion nor physical/durable settlement.
    pub unsafe fn issued(
        owner: Arc<RetainedOriginalFinalizerFile>,
        authority: Arc<dyn OriginalOutsideFinalizerFileRetirementAuthority>,
    ) -> Result<Self> {
        owner.require_original_process()?;
        authority.require_original(&owner)?;
        Ok(Self { process: owner.process, owner, authority })
    }
    fn require_original(&self, owner: &Arc<RetainedOriginalFinalizerFile>) -> Result<()> {
        ensure!(!crate::foreign_process(self.process), "outside descriptor grant belongs to original process");
        owner.require_original_process()?;
        ensure!(Arc::ptr_eq(owner, &self.owner), "outside descriptor grant selects another actual owner");
        self.authority.require_original(owner)
    }

    /// Verify that this outside capability is bound to the exact retained
    /// descriptor owner before a registry caches it. This validates custody;
    /// it performs no descriptor operation and grants no new authority.
    pub fn require_original_owner(
        &self,
        owner: &Arc<RetainedOriginalFinalizerFile>,
    ) -> Result<()> {
        self.require_original(owner)
    }
}

/// Actual transferred finalizer File; metadata/role only, no FD/dup escape.
pub struct RetainedOriginalFinalizerFile {
    process: u32,
    kind: OriginalFinalizerFileKind,
    state: lillux::custody::Held<Mutex<OriginalMetadataPinState>>,
    identity: OnceLock<lillux::OpenFileIdentity>,
}
impl RetainedOriginalFinalizerFile {
    /// # Safety
    /// Transfer the exact descriptor from its actual original producer only
    /// after that producer's last use and into the same retained outside owner
    /// that funds and orders its retirement. The role must describe the actual
    /// source; lock/registry custody and retirement ordering must be exact. No
    /// duplicate or reopened stand-in may be supplied.
    pub unsafe fn issued(file: std::fs::File, kind: OriginalFinalizerFileKind) -> Arc<Self> {
        Arc::new(Self {
            process: std::process::id(),
            kind,
            state: lillux::custody::Held::new(
                ORIGINAL_FINALIZER_FILE_KIND, Mutex::new(OriginalMetadataPinState::Open(file)), pin_state_settled),
            identity: OnceLock::new(),
        })
    }
    fn require_original_process(&self) -> Result<()> {
        ensure!(!crate::foreign_process(self.process), "actual finalizer descriptor belongs to original process");
        require_current_native_process()
    }
    /// Actual descriptor-only retirement after a consuming outside handoff.
    /// This method never borrows native storage or reconstructs a terminal.
    pub fn retire_after_outside_handoff(
        self: &Arc<Self>,
        inputs: &Arc<OriginalOutsideFinalizerFileRetirementInputs>,
    ) -> Result<RetiredOriginalFinalizerFile> {
        self.require_original_process()?;
        let result = (|| -> Result<_> {
            inputs.require_original(self)?;
            inputs.authority.debit_original_steps(6)?;
            inputs.require_original(self)?;
            self.retire_original_descriptor(&|| inputs.require_original(self))
        })();
        result.map_err(|error| anyhow::Error::new(OriginalOutsideFinalizerFileRetirementFailure {
            error, owner: self.clone(), inputs: inputs.clone(),
        }))
    }
    pub fn kind(&self) -> OriginalFinalizerFileKind {
        self.kind
    }

    /// Return the physical close receipt already established by this exact
    /// owner, including when a later authority recheck refused after the
    /// kernel consumed the descriptor. This does not retry the close or
    /// authorize any subsequent cleanup stage.
    pub fn completed_close_receipt(
        self: &Arc<Self>,
    ) -> Result<Option<RetiredOriginalFinalizerFile>> {
        self.require_original_process()?;
        let state = self.state.try_lock()
            .map_err(|_| anyhow::anyhow!("original finalizer pin owner busy or poisoned"))?;
        Ok(matches!(*state, OriginalMetadataPinState::Retired).then(||
            RetiredOriginalFinalizerFile { owner: Arc::clone(self) }))
    }
    fn require_kind(&self, metadata: std::fs::Metadata) -> Result<bool> {
        require_current_native_process()?;
        Ok(match self.kind {
            OriginalFinalizerFileKind::ControlRecord | OriginalFinalizerFileKind::StateLock
            | OriginalFinalizerFileKind::MetadataPin => {
                metadata.is_file()
            }
            OriginalFinalizerFileKind::Directory | OriginalFinalizerFileKind::DirectoryLock => {
                metadata.is_dir()
            }
        })
    }
    pub fn observed_identity(&self) -> Result<lillux::OpenFileIdentity> {
        self.require_original_process()?;
        let state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original finalizer pin owner busy or poisoned"))?;
        let OriginalMetadataPinState::Open(file) = &*state else {
            anyhow::bail!("original finalizer pin no longer open")
        };
        ensure!(
            self.require_kind(file.metadata()?)?,
            "original finalizer pin is not regular"
        );
        let current = lillux::observe_open_file_identity(file)?;
        let original = self.identity.get_or_init(|| current);
        ensure!(
            current.device() == original.device() && current.inode() == original.inode(),
            "original finalizer pin identity changed"
        );
        Ok(current)
    }
    fn retire_original_descriptor(
        self: &Arc<Self>,
        recheck: &dyn Fn() -> Result<()>,
    ) -> Result<RetiredOriginalFinalizerFile> {
        self.require_original_process()?;
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original finalizer retirement owner busy or poisoned"))?;
        if matches!(*state, OriginalMetadataPinState::Retired) {
            return Ok(RetiredOriginalFinalizerFile {
                owner: self.clone(),
            });
        }
        if let OriginalMetadataPinState::ConsumedError { code, errno } = &*state {
            anyhow::bail!(
                "original finalizer close consumed with retained refusal ({code}/{errno})"
            )
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::IntoRawFd;
            if let OriginalMetadataPinState::Open(file) = &*state {
                ensure!(
                    self.require_kind(file.metadata()?)?,
                    "original finalizer pin is not regular"
                );
                let current = lillux::observe_open_file_identity(file)?;
                let original = self.identity.get_or_init(|| current);
                ensure!(
                    current.device() == original.device() && current.inode() == original.inode(),
                    "original finalizer retirement identity changed"
                );
                let OriginalMetadataPinState::Open(file) =
                    std::mem::replace(&mut *state, OriginalMetadataPinState::Retired)
                else {
                    unreachable!()
                };
                *state = OriginalMetadataPinState::PendingRaw(file.into_raw_fd());
            }
            let OriginalMetadataPinState::PendingRaw(fd) = &*state else {
                unreachable!()
            };
            let identity = self
                .identity
                .get()
                .context("original finalizer identity absent")?;
            let result = match self.kind {
                OriginalFinalizerFileKind::MetadataPin => crate::binding::close_original_metadata_pin(
                    *fd,
                    identity.device(),
                    identity.inode(),
                ),
                kind => crate::binding::close_original_finalizer_file(
                    *fd,
                    identity.device(),
                    identity.inode(),
                    kind as i32,
                ),
            };
            if result.consumed {
                *state = if result.code == libsqlite3_sys::SQLITE_OK && result.observed_errno == 0 {
                    OriginalMetadataPinState::Retired
                } else {
                    OriginalMetadataPinState::ConsumedError {
                        code: result.code,
                        errno: result.observed_errno,
                    }
                };
            }
            ensure!(
                result.consumed
                    && result.code == libsqlite3_sys::SQLITE_OK
                    && result.observed_errno == 0,
                "original finalizer retirement pending/refused ({}/{}, consumed={})",
                result.code,
                result.observed_errno,
                result.consumed
            );
            // Retain success before any late clock/funding check.
            drop(state);
            recheck()?;
            Ok(RetiredOriginalFinalizerFile {
                owner: self.clone(),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("original finalizer retirement unavailable")
        }
    }
}
pub struct RetiredOriginalFinalizerFile {
    owner: Arc<RetainedOriginalFinalizerFile>,
}
impl RetiredOriginalFinalizerFile {
    pub fn require_original(&self, owner: &Arc<RetainedOriginalFinalizerFile>) -> Result<()> {
        owner.require_original_process()?;
        ensure!(
            Arc::ptr_eq(owner, &self.owner)
                && matches!(
                    *owner.state.try_lock().map_err(|_| anyhow::anyhow!(
                        "original metadata receipt owner busy or poisoned"
                    ))?,
                    OriginalMetadataPinState::Retired
                ),
            "original metadata pin lacks actual same-owner close receipt"
        );
        Ok(())
    }
}

pub struct OriginalOutsideFinalizerFileRetirementFailure {
    error: anyhow::Error,
    owner: Arc<RetainedOriginalFinalizerFile>,
    inputs: Arc<OriginalOutsideFinalizerFileRetirementInputs>,
}
impl OriginalOutsideFinalizerFileRetirementFailure {
    pub fn retry(&self) -> Result<RetiredOriginalFinalizerFile> {
        self.owner.retire_after_outside_handoff(&self.inputs)
    }
    pub fn owner(&self) -> &Arc<RetainedOriginalFinalizerFile> { &self.owner }
}
impl std::fmt::Debug for OriginalOutsideFinalizerFileRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalOutsideFinalizerFileRetirementFailure")
            .field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalOutsideFinalizerFileRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.error.fmt(f) }
}
impl std::error::Error for OriginalOutsideFinalizerFileRetirementFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.error.as_ref()) }
}

/// Actual sole counter reference. This ordinary facade and its Arc control
/// belong to the enclosing bounded process; the raw repr(C) value is resident.
pub struct OriginalNativeBudgetOwner {
    process: u32,
    region: Mutex<Option<OriginalNativeRegionHandle>>,
    retired_snapshot: OnceLock<OriginalNativeBudgetSnapshot>,
}
impl OriginalNativeBudgetOwner {
    pub fn from_region(region: OriginalNativeRegionHandle) -> Arc<Self> {
        Arc::new(Self { process: region.original_process(), region: Mutex::new(Some(region)),
            retired_snapshot: OnceLock::new() })
    }
    fn require_original_process(&self) -> Result<()> {
        ensure!(!crate::foreign_process(self.process), "foreign original budget facade");
        Ok(())
    }
    /// Waits for, rather than refuses on, another thread's charge: the
    /// facade lock is held only for the bounded region-gate wait and a few
    /// raw-counter operations. The closures run under it are the fixed atomic
    /// counter operations of this impl plus the C process-identity check, which
    /// reads only the process id and atomics. No SQL, SQLite callback, C
    /// allocator effect or other call into this facade can run on the holding
    /// thread, so the blocking lock cannot self-deadlock.
    fn with_raw_budget<T>(&self, cleanup: bool,
        apply: impl FnOnce(&OriginalNativeBudget) -> Result<T>) -> Result<T> {
        self.require_original_process()?;
        let retained = self.region.lock()
            .map_err(|_| anyhow!("original budget facade poisoned"))?;
        let region = retained.as_ref().context("actual original budget handle consumed")?;
        let borrow = region.borrow(cleanup)?;
        // Only local fixed raw-counter operations execute here. The genuine
        // handle and counted Rust loan remain alive throughout the use; no
        // outside authority callback or native C effect runs under this mutex.
        apply(&borrow.state()?.budget)
    }
    pub fn snapshot(&self) -> Result<OriginalNativeBudgetSnapshot> {
        self.with_raw_budget(true, |budget| Ok(budget.snapshot()))
    }
    pub fn debit_steps(&self, steps: u64, cleanup: bool) -> Result<()> {
        self.with_raw_budget(cleanup, |budget| budget.debit_steps(steps, cleanup))
    }
    pub fn debit_connection_ticket(&self) -> Result<u32> {
        self.with_raw_budget(false, OriginalNativeBudget::debit_connection_ticket)
    }
    fn contains_issued_ticket(&self, ticket: u32) -> Result<bool> {
        self.with_raw_budget(true, |budget| Ok(ticket > 0 && ticket <= budget.connections_limit
            && ticket <= budget.connections_issued.load(Ordering::Acquire)))
    }
    fn connections_issued(&self) -> Result<u32> {
        self.with_raw_budget(true, |budget| Ok(budget.connections_issued.load(Ordering::Acquire)))
    }
    fn same_region(&self, region: &OriginalNativeRegionHandle) -> Result<bool> {
        self.require_original_process()?;
        region.require_original_process()?;
        let retained = self.region.lock()
            .map_err(|_| anyhow!("original budget provenance gate poisoned"))?;
        Ok(retained.as_ref().is_some_and(|original| original.same_region(region)))
    }
    pub fn same_original(&self, other: &Self) -> bool {
        if self.require_original_process().is_err() || other.require_original_process().is_err() { return false; }
        if std::ptr::eq(self, other) {
            let Ok(original) = self.region.lock() else { return false; };
            return original.is_some();
        }
        // Both facade locks are taken in address order, so two threads
        // comparing the same pair in opposite directions cannot deadlock.
        let (first, second) = if (self as *const Self) < (other as *const Self) {
            (self, other)
        } else {
            (other, self)
        };
        let Ok(first) = first.region.lock() else { return false; };
        let Ok(second) = second.region.lock() else { return false; };
        match (first.as_ref(), second.as_ref()) {
            (Some(first), Some(second)) => first.same_region(second),
            _ => false,
        }
    }
    /// Consume only this facade's counted handle after the SAME actual
    /// aggregate ACK. Native factory/C loans and host release credit remain
    /// independently owned. This does not dispose, unmap or settle anything.
    pub fn retire_original_handle(self: &Arc<Self>, factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal) -> Result<OriginalNativeBudgetSnapshot> {
        self.require_original_process()?;
        factory.require_process()?;
        ensure!(Arc::ptr_eq(self, &factory.budget), "budget retirement selects a different actual facade");
        aggregate.require_original(factory)?;
        let mut retained = self.region.lock()
            .map_err(|_| anyhow!("original budget handle retirement poisoned"))?;
        if retained.is_none() {
            return self.retired_snapshot.get().cloned()
                .context("actual retired budget evidence absent");
        }
        let region = retained.as_ref().expect("same retained original budget handle");
        ensure!(region.same_region(&factory.region), "budget retirement selects a foreign actual region");
        let snapshot = {
            let borrow = region.borrow(true)?;
            borrow.state()?.budget.snapshot()
        };
        // Cache the real snapshot before making the handle unavailable. All
        // future mapped counter methods refuse; evidence is not a new grant.
        self.retired_snapshot.set(snapshot.clone())
            .map_err(|_| anyhow!("actual original budget retirement evidence already cached"))?;
        let original = retained.take();
        drop(retained);
        drop(original); // Actual reference retirement, outside the facade gate.
        Ok(snapshot)
    }
    pub fn retained_retirement_snapshot(&self) -> Result<OriginalNativeBudgetSnapshot> {
        self.require_original_process()?;
        self.retired_snapshot.get().cloned().context("actual budget handle not yet retired")
    }
}

const MAX_NATIVE_PRODUCER_CELLS: usize = 256;
struct NativeProducerCell { phase: AtomicU8, kind: AtomicU8, ticket: AtomicU32, reference_retired: AtomicBool }
impl NativeProducerCell { fn new() -> Self { Self { phase: AtomicU8::new(0), kind: AtomicU8::new(0), ticket: AtomicU32::new(0), reference_retired: AtomicBool::new(false) } } }
pub(crate) struct NativeResidentState {
    pub(crate) budget: OriginalNativeBudget,
    factory: NativeFactoryStorage,
    controls_next: AtomicU32,
    controls: [NativeProducerCell; MAX_NATIVE_PRODUCER_CELLS],
}
impl NativeResidentState {
    pub(crate) fn new(budget: OriginalNativeBudget) -> Self { Self { budget, factory: NativeFactoryStorage::new(),
        controls_next: AtomicU32::new(0), controls: std::array::from_fn(|_| NativeProducerCell::new()) } }
    // Both methods are invoked under the concrete region gate. Installation
    // requires OPEN; retirement requires CLOSING, so absence cannot race an
    // actual lower factory publication.
    pub(crate) fn claim_factory_install(&self) -> std::result::Result<(), FixedHostRefusal> {
        self.factory.installed.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ()).map_err(|_| FixedHostRefusal::Provenance)
    }
    pub(crate) fn retire_uninstalled_factory_issuer(&self, index: OriginalNativeProducerCellIndex)
        -> std::result::Result<(), FixedHostRefusal> {
        if self.factory.installed.load(Ordering::Acquire) || self.factory.arena.get().is_some()
            || self.factory.arena_entered.load(Ordering::Acquire)
            || self.factory.arena_attempt.load(Ordering::Acquire) != 0
            || self.factory.initialized.load(Ordering::Acquire)
            || self.budget.connections_issued.load(Ordering::Acquire) != 0 {
            return Err(FixedHostRefusal::TerminalRequired);
        }
        if index.0 as u32 >= self.controls_next.load(Ordering::Acquire) {
            return Err(FixedHostRefusal::Provenance);
        }
        let cell = self.controls.get(index.0 as usize).ok_or(FixedHostRefusal::Provenance)?;
        if cell.kind.load(Ordering::Acquire) != producer_kind(SqliteInspectionProductionKind::FactoryIssuer)
            || cell.ticket.load(Ordering::Acquire) != 0 {
            return Err(FixedHostRefusal::Purpose);
        }
        let phase = cell.phase.load(Ordering::Acquire);
        if !matches!(phase, 0 | 1 | 2 | 3) { return Err(FixedHostRefusal::Busy); }
        cell.phase.compare_exchange(phase, 2, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| FixedHostRefusal::Busy)?;
        // This ends only the never-installed issuer interval. Actual wrappers
        // retain their counted handles until outside retirement completes.
        cell.reference_retired.store(true, Ordering::Release);
        Ok(())
    }
    pub(crate) fn require_terminal_exclusion(&self) -> std::result::Result<(), FixedHostRefusal> {
        // Called only by the sole owner under the permanent-close gate with
        // zero handles and Rust/C loans. A never-installed region has no native
        // producer/backend to acknowledge; concrete absence permits disposal,
        // while the outside account still requires its own durable settlement.
        if !self.factory.installed.load(Ordering::Acquire) {
            if self.factory.arena.get().is_some() || self.factory.arena_entered.load(Ordering::Acquire)
                || self.factory.arena_attempt.load(Ordering::Acquire) != 0
                || self.factory.initialized.load(Ordering::Acquire)
                || self.budget.connections_issued.load(Ordering::Acquire) != 0 {
                return Err(FixedHostRefusal::TerminalRequired);
            }
        } else {
            let body = self.factory.terminal_body.get().ok_or(FixedHostRefusal::TerminalRequired)?;
            if !body.issuer_acknowledged.load(Ordering::Acquire) { return Err(FixedHostRefusal::TerminalRequired); }
        }
        if self.budget.connections_issued.load(Ordering::Acquire) as usize > MAX_NATIVE_CONNECTIONS { return Err(FixedHostRefusal::Provenance); }
        for ticket in &self.factory.tickets[..self.budget.connections_issued.load(Ordering::Acquire) as usize] {
            if !matches!(ticket.load(Ordering::Acquire), 2 | 3 | 4) { return Err(FixedHostRefusal::TerminalRequired); }
        }
        let count = self.controls_next.load(Ordering::Acquire) as usize;
        if count > MAX_NATIVE_PRODUCER_CELLS { return Err(FixedHostRefusal::Provenance); }
        for control in &self.controls[..count] {
            if control.phase.load(Ordering::Acquire) != 2 || !control.reference_retired.load(Ordering::Acquire) {
                return Err(FixedHostRefusal::TerminalRequired);
            }
        }
        Ok(())
    }
}

/// A copied index is provenance/observation only, never producer admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginalNativeProducerCellIndex(u16);
/// Ordinary reference wrapper over one nonrecycled concrete resident cell.
pub struct OriginalNativeProducerControl { region: OriginalNativeRegionHandle, index: OriginalNativeProducerCellIndex }
/// Producer-cell check rebuilt from the factory's own counted region and a
/// copied cell index; it takes no new counted region handle.
fn producer_cell_checkpoint(region: &OriginalNativeRegionHandle, index: OriginalNativeProducerCellIndex)
    -> std::result::Result<(), FixedHostRefusal> {
    let cell = region.state_ref()?.controls.get(index.0 as usize).ok_or(FixedHostRefusal::Provenance)?;
    if cell.phase.load(Ordering::Acquire) != 1 { return Err(FixedHostRefusal::Closed); }
    Ok(())
}
impl OriginalNativeProducerControl {
    pub fn try_clone(&self) -> std::result::Result<Self, FixedHostRefusal> {
        Ok(Self { region: self.region.try_clone()?, index: self.index })
    }
    pub fn index(&self) -> OriginalNativeProducerCellIndex { self.index }
    pub(crate) fn same_region(&self, region: &OriginalNativeRegionHandle) -> bool { self.region.same_region(region) }
    fn cell(&self) -> std::result::Result<&NativeProducerCell, FixedHostRefusal> {
        let state = self.region.state_ref()?;
        state.controls.get(self.index.0 as usize).ok_or(FixedHostRefusal::Provenance)
    }
    pub fn bind_ticket(&self, ticket: u32) -> std::result::Result<(), FixedHostRefusal> {
        if ticket == 0 || ticket as usize > MAX_NATIVE_CONNECTIONS { return Err(FixedHostRefusal::Layout); }
        let cell = self.cell()?;
        cell.phase.compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| FixedHostRefusal::Busy)?;
        let result = cell.ticket.compare_exchange(0, ticket, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ()).map_err(|_| FixedHostRefusal::Provenance);
        cell.phase.store(0, Ordering::Release);
        result
    }
    /// Ends only a never-published, unticketed preparation. The cell is not
    /// reused and every surviving wrapper still holds its counted region handle.
    pub fn retire_unbound_preparation(&self) -> std::result::Result<(), FixedHostRefusal> {
        let cell = self.cell()?;
        cell.phase.compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| FixedHostRefusal::Closed)?;
        if cell.ticket.load(Ordering::Acquire) != 0 {
            cell.phase.store(0, Ordering::Release);
            return Err(FixedHostRefusal::TerminalRequired);
        }
        cell.reference_retired.store(true, Ordering::Release);
        cell.phase.store(2, Ordering::Release);
        Ok(())
    }
    /// Requires actual region closure and absence of any installed factory.
    /// No native/backend acknowledgement is manufactured.
    pub fn retire_uninstalled_factory_issuer(&self) -> std::result::Result<(), FixedHostRefusal> {
        self.region.retire_uninstalled_factory_issuer(self.index)
    }
    pub fn checkpoint(&self) -> std::result::Result<(), FixedHostRefusal> {
        if self.cell()?.phase.load(Ordering::Acquire) != 1 { return Err(FixedHostRefusal::Closed); }
        Ok(())
    }
    pub(crate) fn require_kind(&self, kind: SqliteInspectionProductionKind) -> std::result::Result<(), FixedHostRefusal> {
        if self.cell()?.kind.load(Ordering::Acquire) != producer_kind(kind) { return Err(FixedHostRefusal::Purpose); }
        Ok(())
    }
    pub(crate) fn published(&self) -> std::result::Result<(), FixedHostRefusal> {
        self.cell()?.phase.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).map_err(|_| FixedHostRefusal::Provenance)?;
        Ok(())
    }
    pub(crate) fn completed(&self) -> std::result::Result<(), FixedHostRefusal> {
        self.cell()?.phase.store(2, Ordering::Release); Ok(())
    }
    pub(crate) fn abandoned(&self) {
        if let Ok(cell) = self.cell() { let _ = cell.phase.compare_exchange(1, 3, Ordering::AcqRel, Ordering::Acquire); }
    }
    pub(crate) fn reference_retired(&self) {
        if let Ok(cell) = self.cell() { cell.reference_retired.store(true, Ordering::Release); }
    }
    pub fn observation(&self) -> std::result::Result<OriginalNativeProducerObservation, FixedHostRefusal> {
        let cell = self.cell()?;
        Ok(OriginalNativeProducerObservation { index: self.index, phase: cell.phase.load(Ordering::Acquire),
            ticket: cell.ticket.load(Ordering::Acquire), reference_retired: cell.reference_retired.load(Ordering::Acquire) })
    }
}
#[derive(Debug, Clone, Copy)]
pub struct OriginalNativeProducerObservation { pub index: OriginalNativeProducerCellIndex, pub phase: u8,
    pub ticket: u32, pub reference_retired: bool }
fn producer_kind(kind: SqliteInspectionProductionKind) -> u8 {
    match kind { SqliteInspectionProductionKind::FactoryIssuer => 1, SqliteInspectionProductionKind::Namespace => 2,
        SqliteInspectionProductionKind::Copy => 3, SqliteInspectionProductionKind::Native => 4, SqliteInspectionProductionKind::ProjectionLease => 5 }
}
impl OriginalNativeRegionHandle {
    pub fn claim_producer_control(&self, kind: SqliteInspectionProductionKind)
        -> std::result::Result<OriginalNativeProducerControl, FixedHostRefusal> {
        if matches!(kind, SqliteInspectionProductionKind::Namespace | SqliteInspectionProductionKind::Copy) {
            return Err(FixedHostRefusal::Purpose);
        }
        // Preserve a real whole-region reference before reserving any cell.
        let region = self.try_clone()?;
        let state = region.state_ref()?;
        let mut previous = state.controls_next.load(Ordering::Acquire);
        // A lost exchange means another claim took a cell first; retry from
        // the observed count so only overflow or exhaustion refuses. Every
        // loss is another claim's success and the cell count caps those.
        loop {
            let next = previous.checked_add(1).ok_or(FixedHostRefusal::Overflow)?;
            if next as usize > MAX_NATIVE_PRODUCER_CELLS { return Err(FixedHostRefusal::Exhausted); }
            match state.controls_next.compare_exchange(previous, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(observed) => previous = observed,
            }
        }
        let cell = &state.controls[previous as usize];
        cell.kind.store(producer_kind(kind), Ordering::Release);
        Ok(OriginalNativeProducerControl { region, index: OriginalNativeProducerCellIndex(previous as u16) })
    }
    /// Observation only. The outside original owner must permanently close
    /// admission and retain the same factory/issuer join before using this
    /// result; a copied index cannot authorize cleanup or settle an account.
    pub fn require_quiescent_nonissuer_producers(&self, issuer: OriginalNativeProducerCellIndex)
        -> std::result::Result<(), FixedHostRefusal> {
        let state = self.state_ref()?;
        let count = state.controls_next.load(Ordering::Acquire) as usize;
        if count > MAX_NATIVE_PRODUCER_CELLS || issuer.0 as usize >= count {
            return Err(FixedHostRefusal::Provenance);
        }
        let actual = &state.controls[issuer.0 as usize];
        if actual.kind.load(Ordering::Acquire) != producer_kind(SqliteInspectionProductionKind::FactoryIssuer)
            || actual.ticket.load(Ordering::Acquire) != 0 {
            return Err(FixedHostRefusal::Purpose);
        }
        if !matches!(actual.phase.load(Ordering::Acquire), 1 | 2) {
            return Err(FixedHostRefusal::TerminalRequired);
        }
        for (index, cell) in state.controls[..count].iter().enumerate() {
            if index != issuer.0 as usize && cell.phase.load(Ordering::Acquire) != 2 {
                return Err(FixedHostRefusal::TerminalRequired);
            }
        }
        Ok(())
    }
    pub fn producer_control_census(&self) -> std::result::Result<OriginalNativeProducerCensus, FixedHostRefusal> {
        let state = self.state_ref()?;
        let count = state.controls_next.load(Ordering::Acquire) as usize;
        if count > MAX_NATIVE_PRODUCER_CELLS { return Err(FixedHostRefusal::Provenance); }
        let mut census = OriginalNativeProducerCensus { allocated: count, active: 0, completed: 0, abandoned: 0,
            unpublished: 0, live_references: 0, unbound_nonissuer: 0, issuer_cells: 0 };
        for cell in &state.controls[..count] {
            if cell.kind.load(Ordering::Acquire) == producer_kind(SqliteInspectionProductionKind::FactoryIssuer) {
                census.issuer_cells += 1;
            } else if cell.ticket.load(Ordering::Acquire) == 0 && cell.phase.load(Ordering::Acquire) != 2 {
                census.unbound_nonissuer += 1;
            }
            match cell.phase.load(Ordering::Acquire) { 1 => census.active += 1, 2 => census.completed += 1,
                3 => census.abandoned += 1, _ => census.unpublished += 1 }
            if !cell.reference_retired.load(Ordering::Acquire) { census.live_references += 1; }
        }
        Ok(census)
    }
}
#[derive(Debug, Clone, Copy)]
pub struct OriginalNativeProducerCensus { pub allocated: usize, pub active: usize, pub completed: usize,
    pub abandoned: usize, pub unpublished: usize, pub live_references: usize,
    pub unbound_nonissuer: usize, pub issuer_cells: usize }
fn producer_kind_requires_region(factory: &OriginalNativeFactory) -> Result<()> {
    factory.producer.try_lock().map_err(|_| anyhow::anyhow!("original issuer interval busy"))?
        .as_ref().context("original issuer interval absent")?.require_original_region(&factory.region)
}

/// One admitted ticket armed around a raw open by this crate's own tests.
/// Retire it once every connection it opened is closed.
#[cfg(test)]
pub(crate) struct RawTestTicket {
    ticket: u32,
    producer: SqliteInspectionProductionPermit,
    // Keeps the commit slot's weak destination alive while the ticket lives.
    _destination: Arc<dyn OriginalNativeDestinationAuthority>,
}

/// A raw test open whose SQLite handle can be enrolled, so its write
/// commits pass the compiled commit gate like any native connection.
#[cfg(test)]
pub(crate) trait RawTestConnection {
    fn raw_test_handle(&self) -> Option<*mut libsqlite3_sys::sqlite3>;
}
#[cfg(test)]
impl RawTestConnection for Connection {
    fn raw_test_handle(&self) -> Option<*mut libsqlite3_sys::sqlite3> {
        // SAFETY: the handle is only enrolled, never used after close.
        Some(unsafe { self.handle() })
    }
}
#[cfg(test)]
impl<E> RawTestConnection for std::result::Result<Connection, E> {
    fn raw_test_handle(&self) -> Option<*mut libsqlite3_sys::sqlite3> {
        self.as_ref().ok().and_then(RawTestConnection::raw_test_handle)
    }
}
#[cfg(test)]
impl RawTestConnection for crate::BoundSqliteConnection {
    fn raw_test_handle(&self) -> Option<*mut libsqlite3_sys::sqlite3> {
        self.connection.as_ref().and_then(RawTestConnection::raw_test_handle)
    }
}
#[cfg(test)]
impl<E> RawTestConnection for std::result::Result<crate::BoundSqliteConnection, E> {
    fn raw_test_handle(&self) -> Option<*mut libsqlite3_sys::sqlite3> {
        self.as_ref().ok().and_then(RawTestConnection::raw_test_handle)
    }
}
#[cfg(test)]
impl RawTestConnection for crate::ReadonlySqliteCopy {
    fn raw_test_handle(&self) -> Option<*mut libsqlite3_sys::sqlite3> {
        self.owner.raw_test_handle()
    }
}
#[cfg(test)]
impl<E> RawTestConnection for std::result::Result<crate::ReadonlySqliteCopy, E> {
    fn raw_test_handle(&self) -> Option<*mut libsqlite3_sys::sqlite3> {
        self.as_ref().ok().and_then(RawTestConnection::raw_test_handle)
    }
}
#[cfg(any(test, feature = "test-support"))]
impl OriginalNativeFactory {
    /// Run raw SQLite work inside an unticketed allocator scope.
    pub(crate) fn raw_scope_for_test<T>(&self, work: impl FnOnce() -> T) -> Result<T> {
        let _scope = self.arena()?.scope(false, 0)?;
        Ok(work())
    }
    /// Run raw native cleanup work inside a cleanup allocator scope.
    pub(crate) fn raw_cleanup_scope_for_test<T>(&self, work: impl FnOnce() -> T) -> Result<T> {
        let _scope = self.arena()?.scope(true, 0)?;
        Ok(work())
    }
}
#[cfg(test)]
impl OriginalNativeFactory {
    /// Present one backend lifecycle stage (finishing, shut down) to an
    /// admission check without running the stages around it. A test sets
    /// both back to false before the host finishes its own backend.
    pub(crate) fn mark_lifecycle_for_test(&self, finishing: bool, shutdown: bool) -> Result<()> {
        self.storage()?.finishing.store(finishing, Ordering::Release);
        self.storage()?.shutdown_completed.store(shutdown, Ordering::Release);
        Ok(())
    }
}
#[cfg(test)]
impl OriginalNativeFactory {
    /// Arm one fresh ticket around `open` on this thread. The open gate
    /// consumes the ticket at the first SQLite open inside `open`.
    /// The opened connection is enrolled with a commit slot over the
    /// ticket's producer and `destination`, so its commits pass the gate.
    pub(crate) fn arm_raw_for_test<T: RawTestConnection>(
        self: &Arc<Self>,
        destination: Arc<dyn OriginalNativeDestinationAuthority>,
        open: impl FnOnce() -> T,
    ) -> Result<(T, RawTestTicket)> {
        let admission = self.ticket()?;
        let ticket = admission.ticket;
        self.storage()?.tickets[ticket as usize - 1].store(1, Ordering::Release);
        let value = {
            let arena = self.arena()?;
            let _scope = arena.scope(false, ticket)?;
            let value = open();
            if let Some(db) = value.raw_test_handle() {
                self.commit_table.slots[ticket as usize - 1]
                    .set(NativeCommitSlot {
                        factory: Arc::downgrade(self),
                        destination: Arc::downgrade(&destination),
                        control: Arc::downgrade(admission.producer.control_owner()),
                        cell: admission.producer.region_cell(),
                        refused: Arc::new(AtomicBool::new(false)),
                    })
                    .map_err(|_| anyhow!("raw test commit slot already filled"))?;
                ensure!(
                    unsafe { ryeos_native_connection_enroll(arena.0.as_ptr(), db, ticket) }
                        == libsqlite3_sys::SQLITE_OK,
                    "raw test connection enrollment refused"
                );
            }
            value
        };
        let raw = RawTestTicket { ticket, producer: admission.producer, _destination: destination };
        Ok((value, raw))
    }
    /// Recorded shared-mapping bytes and nonempty ledger slots.
    pub(crate) fn shared_census_for_test(&self) -> Result<(u64, u32)> {
        let arena = self.arena()?;
        let (mut bytes, mut slots) = (0u64, 0u32);
        let rc = unsafe { ryeos_native_shared_census(arena.0.as_ptr(), &mut bytes, &mut slots) };
        ensure!(rc == libsqlite3_sys::SQLITE_OK, "native shared census refused: {rc}");
        Ok((bytes, slots))
    }
    /// Close an armed ticket into a released terminal for the host census.
    pub(crate) fn retire_raw_for_test(self: &Arc<Self>, raw: RawTestTicket) -> Result<OriginalNativeConnectionTerminal> {
        let RawTestTicket { ticket, producer, _destination } = raw;
        self.budget.debit_steps(1, true)?;
        self.mark_closed(ticket, true)?;
        producer.acknowledge_native_terminal()?;
        Ok(OriginalNativeConnectionTerminal::Released(Arc::new(ReleasedOriginalNativeAcquisition {
            domain: None,
            factory: self.clone(),
            ticket: Some(ticket),
            original_error: anyhow!("raw test ticket retired"),
            inspection: None,
            closed_inspection: None,
            destination: Mutex::new(None),
            destination_authority: None,
        })))
    }
}

#[cfg(test)]
mod original_native_counter_controls {
    use super::*;

    #[test]
    fn concurrent_step_debits_all_land_and_exhaustion_still_refuses() {
        let _serial = crate::test_host::serial_guard();
        const THREADS: u64 = 2;
        const DEBITS: u64 = 20_000;
        const CLEANUP: u64 = 64;
        let limit = THREADS * DEBITS + CLEANUP;
        // SAFETY: test-only counter that funds no installed region.
        let budget = Arc::new(unsafe {
            OriginalNativeBudget::prepaid_by_original_issuer(16 * 1024 * 1024, limit, 1, CLEANUP)
        }.unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(THREADS as usize));
        let workers: Vec<_> = (0..THREADS).map(|_| {
            let (budget, barrier) = (budget.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..DEBITS { budget.debit_steps(1, false).unwrap(); }
            })
        }).collect();
        for worker in workers { worker.join().unwrap(); }
        assert_eq!(budget.snapshot().allocator_steps, THREADS * DEBITS);
        let error = budget.debit_steps(1, false).unwrap_err();
        assert!(error.to_string().contains("exhausted"), "{error:#}");
        assert_eq!(budget.snapshot().allocator_steps, THREADS * DEBITS);
    }

    /// One budget facade over a fresh resident region. The storage stays
    /// with the returned owner for the rest of the test process.
    fn resident_facade(limit: u64, cleanup: u64)
        -> (crate::original_host_owner::OriginalNativeRegionOwner, Arc<OriginalNativeBudgetOwner>) {
        use crate::original_host_owner::{OriginalNativeRegionOwner, original_native_region_layout};
        // SAFETY: test-only counter funding a region that never installs a factory.
        let raw = unsafe { OriginalNativeBudget::prepaid_by_original_issuer(16 * 1024 * 1024, limit, 1, cleanup) }
            .unwrap();
        let quote = original_native_region_layout().unwrap();
        // SAFETY: a fresh aligned allocation of the quoted layout, used only by
        // this region and never freed while the test process runs.
        let storage = NonNull::new(unsafe { std::alloc::alloc_zeroed(quote.layout) }).unwrap();
        let owner = match unsafe {
            OriginalNativeRegionOwner::initialize(storage, quote.layout.size(), raw, std::process::id(), 1)
        } {
            Ok(owner) => owner,
            Err(failure) => panic!("initialization refused: {}", failure.refusal),
        };
        let facade = OriginalNativeBudgetOwner::from_region(owner.handle().unwrap());
        (owner, facade)
    }

    #[test]
    fn concurrent_facade_charges_wait_instead_of_refusing() {
        let _serial = crate::test_host::serial_guard();
        const THREADS: u64 = 2;
        const DEBITS: u64 = 20_000;
        const CLEANUP: u64 = 64;
        let (owner, facade) = resident_facade(THREADS * DEBITS + CLEANUP, CLEANUP);
        let barrier = Arc::new(std::sync::Barrier::new(THREADS as usize));
        let workers: Vec<_> = (0..THREADS).map(|_| {
            let (facade, barrier) = (facade.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..DEBITS {
                    // Contention on the facade lock or region gate waits;
                    // below the limit no charge may refuse.
                    facade.debit_steps(1, false).unwrap();
                }
            })
        }).collect();
        for worker in workers { worker.join().unwrap(); }
        assert_eq!(facade.snapshot().unwrap().allocator_steps, THREADS * DEBITS);
        // Every counted Rust loan was retired, directly or as published debt.
        owner.retire_pending().unwrap();
        assert_eq!(owner.census().unwrap().rust_borrows, 0);
    }

    #[test]
    fn facade_charges_still_refuse_at_the_limit() {
        let _serial = crate::test_host::serial_guard();
        const SERVICE: u64 = 8;
        const CLEANUP: u64 = 4;
        let (_owner, facade) = resident_facade(SERVICE + CLEANUP, CLEANUP);
        for _ in 0..SERVICE { facade.debit_steps(1, false).unwrap(); }
        let error = facade.debit_steps(1, false).unwrap_err();
        assert!(error.to_string().contains("exhausted"), "{error:#}");
        assert!(native_refusal_is_fact(&error), "{error:#}");
        assert_eq!(facade.snapshot().unwrap().allocator_steps, SERVICE);
        for _ in 0..CLEANUP { facade.debit_steps(1, true).unwrap(); }
        let error = facade.debit_steps(1, true).unwrap_err();
        assert!(error.to_string().contains("exhausted"), "{error:#}");
        assert_eq!(facade.snapshot().unwrap().cleanup_steps, CLEANUP);
    }
}

#[cfg(test)]
mod custody_drop_controls {
    use super::*;

    fn abandoned(kind: &str) -> usize {
        lillux::custody::abandoned_custody().count(kind)
    }

    #[test]
    fn dropped_alias_inventory_retains_only_a_held_original() {
        const KIND: &str = "lillux-sqlite::arc-alias-inventory";
        let prepay = |_: u64, _: u64| -> Result<()> { Ok(()) };
        // SAFETY: test-only inventory over an ordinary heap owner.
        let empty = unsafe { OriginalExactArcAliasInventory::<u8>::issued(1, &prepay) }.unwrap();
        let before = abandoned(KIND);
        drop(empty);
        assert_eq!(abandoned(KIND), before, "an inventory holding no original is settled");
        // SAFETY: as above.
        let mut held = unsafe { OriginalExactArcAliasInventory::<u8>::issued(1, &prepay) }.unwrap();
        let original = Arc::new(7u8);
        let _binding = held
            .retain_original(Arc::clone(&original), &prepay)
            .map_err(|(_, error)| error)
            .unwrap();
        drop(held);
        assert_eq!(abandoned(KIND), before + 1);
        assert_eq!(Arc::strong_count(&original), 2, "the canonical owner stays retained");
    }

    struct AdmittedLease;
    impl OriginalNativeProjectionLeaseAuthority for AdmittedLease {
        fn require_original(&self, _factory: &Arc<OriginalNativeFactory>, _cleanup: bool) -> Result<()> {
            Ok(())
        }
        fn fork_deadline(&self, _factory: &Arc<OriginalNativeFactory>, _cleanup: bool)
            -> Result<lillux::time::MonotonicDeadline> {
            Ok(lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(60)))
        }
    }

    #[test]
    fn dropped_unused_lease_inputs_retain_the_metadata_descriptor() {
        const KIND: &str = "lillux-sqlite::projection-lease-inputs";
        let tmp = tempfile::tempdir().unwrap();
        let root = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        // SAFETY: test-only inputs that never reach a factory; no native
        // code consumes this descriptor.
        let inputs = unsafe {
            OriginalProjectionLeaseInputs::issued(
                Arc::clone(&root), tempfile::tempfile().unwrap(),
                OriginalProjectionLeaseMode::Shared, Arc::new(AdmittedLease),
            )
        };
        let before = abandoned(KIND);
        drop(inputs);
        assert_eq!(abandoned(KIND), before + 1);
        assert_eq!(Arc::strong_count(&root), 1, "only the descriptor is retained");
    }

    /// A continuation whose holders never retire.
    struct HeldContinuation;
    // SAFETY: test-only continuation; it owns no original resource.
    unsafe impl OriginalOutsideDestinationContinuation for HeldContinuation {
        fn require_original_process(&self) -> Result<()> { Ok(()) }
        fn retire_native_references(&mut self, _factory: &Arc<OriginalNativeFactory>,
            _aggregate: &OriginalNativeTerminal, _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> { Ok(()) }
        unsafe fn register_shared_roots(&mut self,
            _registry: &mut crate::original_physical_custody::OriginalPhysicalRegistry,
            _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> { Ok(()) }
        fn physical_holders_retired(&self) -> Result<bool> { Ok(false) }
        fn physical_holders_retirement_quote(
            &self,
        ) -> Result<crate::original_physical_custody::OutsideWork> {
            Ok(crate::original_physical_custody::OutsideWork::ZERO)
        }
    }

    #[test]
    fn dropped_unretired_destination_custody_retains_its_continuation() {
        const KIND: &str = "lillux-sqlite::destination-outside-custody";
        // SAFETY: test-only custody with no physical source.
        let custody = unsafe { OriginalDestinationOutsideCustody::issued(Vec::new(), Box::new(HeldContinuation)) };
        let before = abandoned(KIND);
        drop(custody);
        assert_eq!(abandoned(KIND), before + 1);
    }
}

#[cfg(test)]
mod process_held_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Observed(Arc<AtomicBool>);
    impl Drop for Observed {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    struct Parts {
        process: u32,
        native_ok: bool,
        observed: Observed,
    }
    fn settled(parts: &Parts) -> bool {
        process_held_here(parts.process, || parts.native_ok)
    }
    const KIND: &str = "lillux-sqlite::test-process-held";

    #[test]
    fn held_here_drops_and_refused_copies_are_retained_counted() {
        let _serial = crate::test_host::serial_guard();
        let count = || lillux::custody::abandoned_custody().count(KIND);
        let dropped = Arc::new(AtomicBool::new(false));
        let before = count();
        drop(lillux::custody::Held::new(
            KIND,
            Parts { process: std::process::id(), native_ok: true, observed: Observed(dropped.clone()) },
            settled));
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(count(), before);
        // A different issuing process, and a refusing native check, are both
        // retained and counted, never dropped.
        for (process, native_ok) in [(std::process::id().wrapping_add(1), true), (std::process::id(), false)] {
            let dropped = Arc::new(AtomicBool::new(false));
            let before = count();
            drop(lillux::custody::Held::new(
                KIND, Parts { process, native_ok, observed: Observed(dropped.clone()) }, settled));
            assert!(!dropped.load(Ordering::Acquire));
            assert_eq!(count(), before + 1);
        }
    }
}
