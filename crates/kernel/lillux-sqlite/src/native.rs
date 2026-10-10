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

const MAX_NATIVE_CONNECTIONS: usize = 64;
const CONTROL_STORAGE: u64 = 65_536;

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
        let previous = counter.load(Ordering::Acquire);
        let total = previous
            .checked_add(steps)
            .context("original native work overflow")?;
        ensure!(total <= limit, "original native work exhausted");
        counter
            .compare_exchange(previous, total, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("original native counter busy; no effect admitted"))?;
        Ok(())
    }
    pub fn debit_connection_ticket(&self) -> Result<u32> {
        require_current_native_process()?;
        let previous = self.connections_issued.load(Ordering::Acquire);
        let next = previous
            .checked_add(1)
            .context("original native ticket overflow")?;
        ensure!(
            next <= self.connections_limit,
            "original native connection tickets exhausted"
        );
        self.connections_issued
            .compare_exchange(previous, next, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("original native ticket counter busy"))?;
        Ok(next)
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
    NodeOperational,
    NodeProjection,
    NodeScheduler,
    NodeAccounting,
    NodeProjectionInspection,
    OperationalSyncJobSchemaReference,
    OperationalReplaySchemaReference,
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
    fn require_original(&self, cleanup: bool) -> Result<()>;
    fn require_runtime_binding(&self, runtime: &PinnedDirectory) -> Result<()>;
    fn budget(&self) -> Arc<OriginalNativeBudgetOwner>;
    fn debit_sql_vm_steps(&self, steps: u64) -> Result<()>;
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
                error, original: std::mem::ManuallyDrop::new(self),
            }),
        }
    }
}

/// Actual never-delivered admission, including its original published permit.
/// Drop retains custody; it does not abandon production or acknowledge C cleanup.
pub struct OriginalNativeProjectionLeaseAdmissionFailure {
    error: anyhow::Error,
    original: std::mem::ManuallyDrop<OriginalNativeProjectionLeaseAdmission>,
}
impl OriginalNativeProjectionLeaseAdmissionFailure {
    /// Retry only the SAME admission. No ticket, issuer, debit or clock is renewed.
    pub fn retry(self) -> std::result::Result<OriginalNativeProjectionLeaseAdmission, Self> {
        // Copied process must be checked before extracting or dropping any part.
        if self.original.budget.require_original_process().is_err() {
            return Err(self);
        }
        let Self { error: _, original } = self;
        std::mem::ManuallyDrop::into_inner(original).validate_original()
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
pub struct OriginalProjectionLeaseInputs {
    root: Arc<PinnedDirectory>,
    metadata: std::mem::ManuallyDrop<std::fs::File>,
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
        Self { root, metadata: std::mem::ManuallyDrop::new(metadata), mode,
            authority, identity: (0, 0) }
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
pub struct OriginalProjectionLease {
    admission_failure: Option<OriginalNativeProjectionLeaseAdmissionFailure>,
    factory: Arc<OriginalNativeFactory>,
    inputs: std::mem::ManuallyDrop<OriginalProjectionLeaseInputs>,
    producer: std::mem::ManuallyDrop<Option<SqliteInspectionProductionPermit>>,
    native: Option<crate::binding::ProjectionLease>,
    ticket: Option<u32>,
    pin_consumed: bool,
    native_closed: bool,
    ticket_registered: bool,
    close_refusal: bool,
    fd_consumed: bool,
    fork_invariant_refused: bool,
    fork_reservation: std::mem::ManuallyDrop<Option<lillux::exec::BorrowedForkChildCloseReservation>>,
    fork_registration: std::mem::ManuallyDrop<Option<lillux::exec::BorrowedForkChildCloseRegistration>>,
}
pub struct OriginalProjectionLeaseFailure {
    error: anyhow::Error,
    owner: std::mem::ManuallyDrop<Mutex<OriginalProjectionLease>>,
}
impl OriginalProjectionLeaseFailure {
    /// Borrow the actual retained admission using exclusive outside custody;
    /// no mutex guard spans recovery or any native/authority effect.
    pub fn retained_admission(&mut self) -> Option<&OriginalNativeProjectionLeaseAdmissionFailure> {
        let owner = self.owner.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        if owner.factory.require_process().is_err() { return None; }
        owner.admission_failure.as_ref()
    }
    /// Recover in place so the enclosing failure remains available on every
    /// refusal, including the SAME original callback panic evidence.
    pub fn recover_retained_admission(&mut self) -> Result<()> {
        let owner = self.owner.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            // Keep the actual new refusal/panic as the error root so its typed
            // payload remains directly downcastable; the previous error object
            // is retained as context, never reduced to a display string.
            self.error = error.context(previous);
            anyhow::bail!("original lease admission recovery refused; whole owner and evidence retained");
        }
        Ok(())
    }
    pub fn error(&self) -> &anyhow::Error { &self.error }
    pub fn into_owner(mut self) -> std::result::Result<OriginalProjectionLease, Self> {
        if require_current_native_process().is_err() { return Err(self); }
        if self.owner.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner)
            .factory.require_process().is_err() { return Err(self); }
        let owner = unsafe { std::mem::ManuallyDrop::take(&mut self.owner) };
        Ok(owner.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner))
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
    pub fn recover_retained_admission(&mut self) -> Result<()> {
        self.factory.require_original(true)?;
        ensure!(self.native.is_none() && self.ticket.is_none() && self.producer.is_none()
            && !self.ticket_registered && !self.pin_consumed && !self.native_closed,
            "lease admission recovery conflicts with actual acquired/disposed owner");
        let failure = self.admission_failure.take()
            .context("actual retained lease admission absent")?;
        let original = &failure.original;
        let result = crate::original_physical_custody::retain_callback_refusal(&|| {
            self.factory.recover_undelivered_admission(
                &original.budget, original.ticket, &original.producer,
                SqliteInspectionProductionKind::ProjectionLease)
        });
        if let Err(error) = result {
            self.admission_failure = Some(failure);
            return Err(error);
        }
        let OriginalNativeProjectionLeaseAdmissionFailure { original, error: _ } = failure;
        let original = std::mem::ManuallyDrop::into_inner(original);
        self.ticket = Some(original.ticket);
        *self.producer = Some(original.producer);
        self.ticket_registered = true;
        Ok(())
    }
    pub fn retained_admission(&self) -> Option<&OriginalNativeProjectionLeaseAdmissionFailure> {
        if self.factory.require_process().is_err() { return None; }
        self.admission_failure.as_ref()
    }
    pub fn metadata_pin_consumed(&self) -> bool { self.pin_consumed }
    pub fn native_close_completed(&self) -> bool { self.native_closed }
    pub fn consumed_close_refusal(&self) -> bool { self.close_refusal }
    pub fn data_descriptor_consumed(&self) -> bool { self.fd_consumed }
    pub fn fork_invariant_refused(&self) -> bool { self.fork_invariant_refused }
    pub fn close(mut self) -> std::result::Result<OriginalProjectionLeaseDisposition, OriginalProjectionLeaseFailure> {
        let result = (|| -> Result<OriginalProjectionLeaseDisposition> {
            self.factory.require_original(true)?;
            ensure!(self.admission_failure.is_none(),
                "actual published lease admission unresolved; no close or NotAcquired terminal");
            self.inputs.authority.require_original(&self.factory, true)?;
            ensure!(!self.factory.storage()?.arena_released.load(Ordering::Acquire),
                "lease input/native cleanup must precede actual arena release");
            ensure!(!self.close_refusal, "projection lease consumed-close refusal retained; no replay");
            ensure!(!self.fork_invariant_refused,
                "projection lease fork invariant refused; owner/exclusion retained, no terminal");
            // Establish a truthful disposition route BEFORE consuming any descriptor.
            let not_acquired = !self.ticket_registered && self.ticket.is_none()
                && self.producer.is_none() && self.native.is_none();
            ensure!(self.ticket_registered || not_acquired,
                "unregistered reported lease admission retained; no descriptor disposal");
            let deadline = self.inputs.authority.fork_deadline(&self.factory, true)?;
            let fork_guard = lillux::exec::retain_fork_sensitive_descriptors_until(deadline)
                .context("projection lease original cleanup fork deadline refused")?;
            ensure!(!deadline.has_elapsed(), "projection lease original cleanup deadline elapsed");
            self.factory.require_original(true)?;
            self.inputs.authority.require_original(&self.factory, true)?;
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
                let flags = unsafe { libc::fcntl(self.inputs.metadata.as_raw_fd(), libc::F_GETFL) };
                ensure!(flags >= 0 && flags & libc::O_PATH != 0,
                    "lease cleanup retains contradictory non-metadata descriptor");
                let metadata = self.inputs.metadata.metadata()?;
                ensure!(metadata.is_file(), "lease cleanup metadata is not regular");
                if not_acquired { self.inputs.identity = (metadata.dev(), metadata.ino()); }
            }
            if !self.native_closed {
                if let Some(native) = &self.native {
                    let (code, consumed, fd_consumed) = native.close();
                    self.pin_consumed |= consumed;
                    self.fd_consumed |= fd_consumed;
                    if code == libsqlite3_sys::SQLITE_OK {
                        self.native = None;
                        self.native_closed = true;
                    }
                    // Commit real consumption before interpreting status/late failures.
                    if fd_consumed {
                        if let Some(registration) = self.fork_registration.take() {
                            match unsafe { registration.unregister_consumed(&fork_guard) } {
                                Ok(()) => {}
                                Err(registration) => {
                                    *self.fork_registration = Some(registration);
                                    self.fork_invariant_refused = true;
                                    // Invariant-only fail-closed: prevent every protected fork
                                    // from observing a stale consumed numeric registration.
                                    std::mem::forget(fork_guard);
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
                            self.inputs.metadata.as_raw_fd(), self.inputs.identity.0, self.inputs.identity.1);
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
            ensure!(self.fork_registration.is_none() && self.fork_reservation.is_none(),
                "projection lease child-close inventory still retained");
            self.factory.require_original(true)?;
            self.inputs.authority.require_original(&self.factory, true)?;
            if not_acquired {
                ensure!(self.pin_consumed && self.native_closed,
                    "lease non-acquisition metadata disposal incomplete");
                return Ok(OriginalProjectionLeaseDisposition::NotAcquired(
                    NotAcquiredOriginalProjectionLease { factory: self.factory.clone(),
                        metadata_identity: self.inputs.identity, pin_consumed: self.pin_consumed }));
            }
            ensure!(self.ticket_registered, "projection lease has no registered original ticket");
            let ticket = self.ticket.context("projection lease has no issued ticket; no terminal minted")?;
            if self.factory.storage()?.tickets[ticket as usize - 1].load(Ordering::Acquire) != 4 {
                self.factory.require_original(true)?;
                self.inputs.authority.require_original(&self.factory, true)?;
                ensure!(unsafe { ryeos_native_connection_closed(self.factory.arena()?.0.as_ptr(), ticket) }
                    == libsqlite3_sys::SQLITE_OK, "projection lease native ticket close refused");
                // Actual native success is retained before the fallible upper
                // interval ACK; retries do not repeat the consumed C effect.
                self.factory.storage()?.tickets[ticket as usize - 1].store(4, Ordering::Release);
            }
            if let Some(producer) = self.producer.as_ref() { producer.acknowledge_projection_lease_terminal()?; }
            Ok(OriginalProjectionLeaseDisposition::Closed(Arc::new(
                ClosedOriginalProjectionLease { factory: self.factory.clone(), ticket })))
        })();
        match result {
            Ok(receipt) => {
                // Only successful explicit cleanup releases Rust custody; metadata File
                // remains ManuallyDrop because C consumed its numeric descriptor once.
                unsafe {
                    std::mem::ManuallyDrop::drop(&mut self.producer);
                    std::mem::ManuallyDrop::drop(&mut self.inputs);
                    std::mem::ManuallyDrop::drop(&mut self.fork_registration);
                    std::mem::ManuallyDrop::drop(&mut self.fork_reservation);
                }
                Ok(receipt)
            }
            Err(error) => Err(OriginalProjectionLeaseFailure {
                error, owner: std::mem::ManuallyDrop::new(Mutex::new(self)) }),
        }
    }
}
// No Drop cleanup, unlock, registry removal, ticket acknowledgement or refund.

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
                error, original: std::mem::ManuallyDrop::new(self),
            }),
        }
    }
}

/// Actual never-delivered admission, including its original published permit.
/// Drop retains custody; it does not abandon production or acknowledge C cleanup.
pub struct OriginalNativeConnectionAdmissionFailure {
    error: anyhow::Error,
    original: std::mem::ManuallyDrop<OriginalNativeConnectionAdmission>,
}
impl OriginalNativeConnectionAdmissionFailure {
    /// Retry only the SAME admission. No ticket, issuer, debit or clock is renewed.
    pub fn retry(self) -> std::result::Result<OriginalNativeConnectionAdmission, Self> {
        // Copied process must be checked before extracting or dropping any part.
        if self.original.budget.require_original_process().is_err() {
            return Err(self);
        }
        let Self { error: _, original } = self;
        std::mem::ManuallyDrop::into_inner(original).validate_original()
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
    slots: std::mem::ManuallyDrop<Vec<OriginalExactArcAliasSlot<T>>>,
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
            slots: std::mem::ManuallyDrop::new(Vec::with_capacity(maximum)) })
    }
    fn require_process(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original Arc alias inventory");
        Ok(())
    }
    fn require_binding(&self, binding: &OriginalExactArcAliasBinding<T>) -> Result<()> {
        self.require_process()?;
        ensure!(binding.process == self.process && Arc::ptr_eq(&binding.inventory, &self.identity)
            && binding.index < self.slots.len(), "Arc alias binding selects another actual inventory");
        Ok(())
    }
    pub fn retain_original(&mut self, original: Arc<T>, prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<OriginalExactArcAliasBinding<T>, (Arc<T>, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_process()?;
            ensure!(!self.closed, "original Arc alias census already closed");
            let work = u64::try_from(self.slots.len())?.checked_add(4).context("original Arc alias scan overflow")?;
            crate::original_physical_custody::retain_callback_refusal(&|| prepay(u64::try_from(std::mem::size_of::<OriginalExactArcAliasBinding<T>>())?, work))
        })();
        if let Err(error) = checked { return Err((original, error)); }
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.original.as_ref().is_some_and(|canonical| Arc::ptr_eq(canonical, &original)) {
                let binding = OriginalExactArcAliasBinding { process: self.process, inventory: self.identity.clone(),
                    index, _owner: std::marker::PhantomData };
                drop(original); // Canonical actual owner already retained; alias only.
                return Ok(binding);
            }
        }
        if self.slots.len() == self.maximum {
            return Err((original, anyhow!("original Arc alias inventory capacity exhausted")));
        }
        let index = self.slots.len();
        self.slots.push(OriginalExactArcAliasSlot { identity: Arc::downgrade(&original), original: Some(original), transferred: false });
        Ok(OriginalExactArcAliasBinding { process: self.process, inventory: self.identity.clone(), index,
            _owner: std::marker::PhantomData })
    }
    pub fn require_retained_alias(&self, binding: &OriginalExactArcAliasBinding<T>, original: &Arc<T>) -> Result<()> {
        self.require_binding(binding)?;
        ensure!(self.slots[binding.index].original.as_ref().is_some_and(|canonical| Arc::ptr_eq(canonical, original)),
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
        self.slots[binding.index].original.take().context("actual canonical Arc owner already checked out")
    }
    pub fn restore_original(&mut self, binding: &OriginalExactArcAliasBinding<T>, original: Arc<T>)
        -> std::result::Result<(), (Arc<T>, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_binding(binding)?;
            let slot = &self.slots[binding.index];
            ensure!(!slot.transferred && slot.original.is_none()
                && std::sync::Weak::ptr_eq(&slot.identity, &Arc::downgrade(&original)),
                "Arc refusal restoration selects another original owner or disposition");
            Ok(())
        })();
        if let Err(error) = checked { return Err((original, error)); }
        self.slots[binding.index].original = Some(original); Ok(())
    }
    /// # Safety
    /// Actual typed domain receiver cached the entire consumed parent and
    /// genuine leaf evidence. No field/count/weak-null observation proves it.
    pub unsafe fn record_actual_transfer(&mut self, binding: &OriginalExactArcAliasBinding<T>) -> Result<()> {
        self.require_binding(binding)?;
        let slot = &mut self.slots[binding.index];
        ensure!(self.closed && slot.original.is_none() && !slot.transferred, "actual canonical Arc owner has not transferred or already recorded");
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
/// Trusted domain receiver of one complete original inspection context. This
/// declaration performs no graph discovery or disposal; the App owns census.
/// # Safety
/// On success retain the SAME actual lease/SQL/copy-work owners in canonical
/// custody, bind the genuine factory/aggregate/leaf and copied original PID.
/// Success permits only surrender of exact redundant aliases, never physical
/// release, native zero-loan sealing or durable account completion.
pub unsafe trait OriginalOutsideInspectionAliasReceiver: Send + Sync {
    fn require_original_context(&self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &OriginalNativeTerminal,
        leaf: &OriginalNativeOutsideLeafEvidence,
        lease: &SqliteCopyLease, sql: &Arc<dyn SqliteInspectionSqlWork>,
        work: &Arc<dyn crate::SqliteInspectionCopyWork>) -> Result<()>;
}
pub unsafe trait OriginalOutsideDestinationContinuation: Send + Sync {
    /// False means this continuation did not join an inspection context. It
    /// grants no completion and lets the actual supervisor retain/refuse it.
    fn join_original_inspection_aliases(&mut self,
        _factory: &Arc<OriginalNativeFactory>, _aggregate: &OriginalNativeTerminal,
        _receiver: &dyn OriginalOutsideInspectionAliasReceiver) -> Result<bool> { Ok(false) }
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
pub struct OriginalDestinationOutsideCustody {
    process: u32,
    sources: std::mem::ManuallyDrop<Vec<OriginalDestinationPhysicalSource>>,
    continuation: std::mem::ManuallyDrop<Box<dyn OriginalOutsideDestinationContinuation>>,
    registrations: Option<Vec<crate::original_physical_custody::OriginalPhysicalRegistration>>,
    registry_identity: Option<Arc<()>>,
    roots_registered: bool,
    native_references_retired: bool,
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
    destination: std::mem::ManuallyDrop<OriginalDestinationOutsideCustody>,
}
impl OriginalDestinationOutsideCustody {
    /// # Safety
    /// The actual consuming domain implementation verified SAME factory and
    /// genuine aggregate, charged concrete Vec/Box storage before allocating,
    /// and moved all original roots/locks/semantics into these real inputs.
    /// This constructor grants no cleanup, zero-loan or account proof.
    pub unsafe fn issued(sources: Vec<OriginalDestinationPhysicalSource>,
        continuation: Box<dyn OriginalOutsideDestinationContinuation>) -> Self {
        Self { process: std::process::id(), sources: std::mem::ManuallyDrop::new(sources),
            continuation: std::mem::ManuallyDrop::new(continuation), registrations: None,
            registry_identity: None, roots_registered: false, native_references_retired: false }
    }
    fn require_process(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original destination outside custody");
        self.continuation.require_original_process()
    }
    pub fn join_original_inspection_aliases(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &OriginalNativeTerminal,
        receiver: &dyn OriginalOutsideInspectionAliasReceiver) -> Result<bool> {
        self.require_process()?;
        aggregate.require_original(factory)?;
        let joined = std::cell::Cell::new(false);
        let continuation = std::cell::RefCell::new(&mut **self.continuation);
        crate::original_physical_custody::retain_callback_refusal(&|| {
            joined.set(continuation.borrow_mut().join_original_inspection_aliases(factory, aggregate, receiver)?);
            Ok(())
        })?;
        Ok(joined.get())
    }
    pub fn retire_native_references(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal, prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_process()?;
        factory.require_process()?;
        aggregate.require_original(factory)?;
        if self.native_references_retired { return Ok(()); }
        ensure!(self.roots_registered,
            "original destination roots must join before native-reference retirement");
        let continuation = std::cell::RefCell::new(&mut **self.continuation);
        crate::original_physical_custody::retain_callback_refusal(&|| {
            continuation.borrow_mut().retire_native_references(factory, aggregate, prepay)
        })?;
        self.native_references_retired = true;
        Ok(())
    }
    /// # Safety
    /// SAME complete original supervisor inventory and irreversible prepay.
    pub unsafe fn register_shared_roots(&mut self,
        registry: &mut crate::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_process()?;
        let registered = self.registrations.as_ref().map_or(0, Vec::len);
        let retained_sources = self.sources.len().checked_add(registered)
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
        if let Some(registrations) = &self.registrations {
            let quote = registry.membership_work_bound(u64::try_from(registrations.len())?, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.require_process()?;
            for registration in registrations {
                registry.require_membership(registration)?;
            }
        }
        if self.registrations.is_none() {
            let count = self.sources.len();
            let bytes = count.checked_mul(std::mem::size_of::<crate::original_physical_custody::OriginalPhysicalRegistration>())
                .context("destination registration storage overflow")?;
            registry.prepay_transfer(prepay, u64::try_from(bytes)?, u64::try_from(count)?.checked_add(1)
                .context("destination registration work overflow")?)?;
            self.registrations = Some(Vec::with_capacity(count));
        }
        while let Some(source) = self.sources.pop() {
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
                Ok(binding) => self.registrations.as_mut().expect("prepaid exact registration storage").push(binding),
                Err((owner, error)) => { self.sources.push(owner); return Err(error); }
            }
        }
        let continuation = std::cell::RefCell::new(&mut **self.continuation);
        let registry = std::cell::RefCell::new(registry);
        let result = crate::original_physical_custody::retain_callback_refusal(&|| {
            // SAFETY: same retained domain owner and actual supervisor registry.
            unsafe { continuation.borrow_mut().register_shared_roots(&mut **registry.borrow_mut(), prepay) }
        });
        if result.is_ok() { self.roots_registered = true; }
        result
    }
    pub fn physical_holders_retired(&self) -> Result<bool> {
        self.require_process()?;
        if !self.sources.is_empty() { return Ok(false); }
        let Some(registrations) = &self.registrations else { return Ok(false); };
        for registration in registrations { if !registration.retired() { return Ok(false); } }
        self.continuation.physical_holders_retired()
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
            ensure!(self.roots_registered && self.native_references_retired,
                "original destination native-reference and registry joins are incomplete");
            ensure!(self.sources.is_empty(),
                "original destination roots have not transferred to the shared registry");
            let registry_identity = self.registry_identity.as_ref()
                .context("original destination registry identity absent")?;
            registry.require_identity_token(registry_identity)?;
            let registrations = self.registrations.as_ref()
                .context("original destination registration inventory absent")?;
            let quote = registry.membership_work_bound(u64::try_from(registrations.len())?, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.require_process()?;
            factory.require_process()?;
            aggregate.require_original(factory)?;
            ensure!(self.roots_registered && self.native_references_retired
                && self.sources.is_empty(),
                "original destination registration changed during permit preparation");
            registry.require_identity_token(self.registry_identity.as_ref()
                .context("original destination registry identity disappeared")?)?;
            for registration in registrations { registry.require_membership(registration)?; }
            Ok(())
        })();
        if let Err(error) = checked { return Err((self, error)); }
        Ok(OriginalDestinationRetirement { destination: std::mem::ManuallyDrop::new(self) })
    }
}
impl OriginalDestinationRetirement {
    /// Consume this exact destination only after its own same-registry receipts
    /// and typed continuation report physical retirement. This transition has
    /// no factory, aggregate, or resident-region access by construction.
    pub fn into_retired(
        mut self,
        registry: &crate::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<(), (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            let destination = unsafe { &*self.destination };
            destination.require_process()?;
            ensure!(destination.roots_registered && destination.native_references_retired
                && destination.sources.is_empty(),
                "original destination permit no longer matches its completed joins");
            let registry_identity = destination.registry_identity.as_ref()
                .context("original destination permit lacks registry identity")?;
            registry.require_same_identity_token(registry_identity)?;
            let registrations = destination.registrations.as_ref()
                .context("original destination registration inventory absent")?;
            let direct_checks = u64::try_from(registrations.len())?
                .checked_mul(4)
                .context("original destination retirement work quote overflow")?;
            let continuation = destination.continuation.physical_holders_retirement_quote()?;
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
            ensure!(destination.sources.is_empty(),
                "original destination retains unregistered physical sources");
            ensure!(destination.continuation.physical_holders_retired()?,
                "original destination physical holders remain; custody retained");
            Ok(())
        })();
        if let Err(error) = checked { return Err((self, error)); }

        let destination = unsafe { std::mem::ManuallyDrop::take(&mut self.destination) };
        let OriginalDestinationOutsideCustody {
            process: _, mut sources, mut continuation, registrations, ..
        } = destination;
        // SAFETY: the checks above require an empty source vector, retired
        // registrations, and the concrete owner's positive holder-retirement
        // predicate. These ManuallyDrop fields have no live physical source.
        unsafe {
            std::mem::ManuallyDrop::drop(&mut sources);
            std::mem::ManuallyDrop::drop(&mut continuation);
        }
        drop(registrations);
        Ok(())
    }
}
pub struct OriginalDestinationCustodyFailure {
    factory: std::mem::ManuallyDrop<Arc<OriginalNativeFactory>>,
    authority: std::mem::ManuallyDrop<Arc<dyn OriginalNativeDestinationAuthority>>,
    error: anyhow::Error,
}
impl OriginalDestinationCustodyFailure {
    pub fn retain(factory: Arc<OriginalNativeFactory>, authority: Arc<dyn OriginalNativeDestinationAuthority>, error: anyhow::Error) -> Self {
        Self { factory: std::mem::ManuallyDrop::new(factory), authority: std::mem::ManuallyDrop::new(authority), error }
    }
    pub fn into_original_inputs(self) -> std::result::Result<(Arc<OriginalNativeFactory>, Arc<dyn OriginalNativeDestinationAuthority>, anyhow::Error), Self> {
        if self.factory.require_process().is_err() { return Err(self); }
        let Self { factory, authority, error } = self;
        Ok((std::mem::ManuallyDrop::into_inner(factory), std::mem::ManuallyDrop::into_inner(authority), error))
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
        if self.process != std::process::id() { return Err(self); }
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
        if self.process != std::process::id() { return Err(self); }
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
    fn scope(&self, cleanup: bool, ticket: u32) -> Result<NativeScope<'_>> {
        require_current_native_process()?;
        ensure!(self.1 == std::process::id(), "inherited native arena");
        ensure!(
            unsafe { ryeos_native_scope_enter(self.0.as_ptr(), cleanup as c_int, ticket) }
                == libsqlite3_sys::SQLITE_OK,
            "original native scope entry refused"
        );
        Ok(NativeScope(self))
    }
}
// An unverified mapping is never unmapped by Drop. The original factory/error
// retains it; only supervised terminal calls the actual native release seam.
struct NativeScope<'a>(&'a NativeArena);
impl Drop for NativeScope<'_> {
    fn drop(&mut self) {
        if self.0.1 != std::process::id() || unsafe { ryeos_native_process_matches() } != 1 {
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
        if self.1 == std::process::id() && require_current_native_process().is_ok() {
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
            producer: Mutex::new(Some(producer)), budget_c_lease: OnceLock::new() });
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
        let owner = OriginalProjectionLease {
            factory: self.clone(), inputs: std::mem::ManuallyDrop::new(inputs),
            admission_failure: None,
            producer: std::mem::ManuallyDrop::new(None), native: None, ticket: None,
            pin_consumed: false, native_closed: false, ticket_registered: false, close_refusal: false,
            fd_consumed: false, fork_invariant_refused: false,
            fork_reservation: std::mem::ManuallyDrop::new(None),
            fork_registration: std::mem::ManuallyDrop::new(None),
        };
        owner.close()
    }
    pub fn acquire_projection_lease(
        self: &Arc<Self>, inputs: OriginalProjectionLeaseInputs,
    ) -> std::result::Result<OriginalProjectionLease, OriginalProjectionLeaseFailure> {
        let mut owner = OriginalProjectionLease {
            factory: self.clone(), inputs: std::mem::ManuallyDrop::new(inputs),
            admission_failure: None,
            producer: std::mem::ManuallyDrop::new(None), native: None, ticket: None,
            pin_consumed: false, native_closed: false, ticket_registered: false, close_refusal: false,
            fd_consumed: false, fork_invariant_refused: false,
            fork_reservation: std::mem::ManuallyDrop::new(None),
            fork_registration: std::mem::ManuallyDrop::new(None),
        };
        let result = (|| -> Result<()> {
            self.require_ready()?;
            owner.inputs.authority.require_original(self, false)?;
            let _effect = self.enter_effect()?;
            #[cfg(not(target_os = "linux"))]
            anyhow::bail!("projection leases require Linux");
            #[cfg(target_os = "linux")]
            {
                use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
                let flags = unsafe { libc::fcntl(owner.inputs.metadata.as_raw_fd(), libc::F_GETFL) };
                ensure!(flags >= 0 && flags & libc::O_PATH != 0,
                    "projection lease requires actual O_PATH metadata");
                let observed = owner.inputs.metadata.metadata()?;
                ensure!(observed.is_file(), "projection lease metadata is not regular");
                owner.inputs.identity = (observed.dev(), observed.ino());
                owner.inputs.root.ensure_path_binding()?;
                owner.inputs.root.ensure_open_regular_same_mount(&owner.inputs.metadata)?;
            }
            let admission = match self.authority.begin_projection_lease() {
                Ok(admission) => admission,
                Err(error) => match error.downcast::<OriginalNativeProjectionLeaseAdmissionFailure>() {
                    Ok(failure) => {
                        owner.admission_failure = Some(failure);
                        anyhow::bail!("actual published lease admission retained; validation recovery required");
                    }
                    Err(error) => return Err(error),
                },
            };
            owner.ticket = Some(admission.ticket);
            *owner.producer = Some(admission.producer);
            owner.producer.as_ref().context("lease producer missing")?.require_original_region(&self.region)?;
            ensure!(admission.budget.same_original(&self.budget), "projection lease selects foreign budget");
            let ticket = owner.ticket.expect("actual issued lease ticket");
            ensure!(ticket > 0 && ticket as usize <= MAX_NATIVE_CONNECTIONS
                && self.storage()?.tickets[ticket as usize - 1].compare_exchange(0, 1,
                    Ordering::AcqRel, Ordering::Acquire).is_ok(), "projection lease ticket is foreign/reused");
            owner.ticket_registered = true;
            self.require_ready()?;
            owner.inputs.authority.require_original(self, false)?;
            owner.inputs.root.ensure_path_binding()?;
            owner.inputs.root.ensure_open_regular_same_mount(&owner.inputs.metadata)?;
            let _scope = self.arena()?.scope(false, ticket)?;
            owner.producer.as_ref().context("lease producer missing")?.checkpoint()?;
            use std::os::fd::AsRawFd;
            let mode = match owner.inputs.mode { OriginalProjectionLeaseMode::Shared => 1,
                OriginalProjectionLeaseMode::Exclusive => 2 };
            let deadline = owner.inputs.authority.fork_deadline(self, false)?;
            let fork_guard = lillux::exec::retain_fork_sensitive_descriptors_until(deadline)
                .context("projection lease original service fork deadline refused")?;
            ensure!(!deadline.has_elapsed(), "projection lease original service deadline elapsed");
            self.require_ready()?;
            owner.inputs.authority.require_original(self, false)?;
            let reservation = lillux::exec::reserve_borrowed_fork_child_close_descriptor(&fork_guard)
                .map_err(|error| anyhow::anyhow!(error))?;
            *owner.fork_reservation = Some(reservation);
            let (code, actual, actual_fd) = crate::binding::ProjectionLease::open(owner.inputs.metadata.as_raw_fd(), mode);
            owner.native = actual;
            let reservation = owner.fork_reservation.take().expect("actual pre-effect fork reservation");
            if let Some(actual_fd) = actual_fd {
                match unsafe { reservation.bind(&fork_guard, actual_fd) } {
                    Ok(registration) => *owner.fork_registration = Some(registration),
                    Err(reservation) => {
                        *owner.fork_reservation = Some(reservation);
                        owner.fork_invariant_refused = true;
                        // Never release fork exclusion around an unregistered real FD.
                        // This invariant-only refusal remains held until restart/recovery.
                        std::mem::forget(fork_guard);
                        anyhow::bail!("lease acquired-FD fork enrollment invariant refused");
                    }
                }
            } else {
                if let Err(reservation) = reservation.cancel(&fork_guard) {
                    *owner.fork_reservation = Some(reservation);
                    owner.fork_invariant_refused = true;
                    std::mem::forget(fork_guard);
                    anyhow::bail!("lease unused fork reservation invariant refused");
                }
            }
            // Every actual partial FD is enrolled before status or late host checks.
            ensure!(code == libsqlite3_sys::SQLITE_OK && owner.native.is_some(),
                "projection lease acquisition/flock refused ({code})");
            self.require_ready()?;
            owner.inputs.authority.require_original(self, false)
        })();
        match result {
            Ok(()) => Ok(owner),
            Err(error) => Err(OriginalProjectionLeaseFailure {
                error, owner: std::mem::ManuallyDrop::new(Mutex::new(owner)) }),
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
            self.original_process == std::process::id(),
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
            ensure!(
                unsafe { ryeos_native_connection_enroll(arena.0.as_ptr(), db, original_ticket) }
                    == libsqlite3_sys::SQLITE_OK,
                "original native connection enrollment refused"
            );
            // The SQLite progress hook crosses rusqlite's unwind boundary; the
            // admission authority holds no state a caught unwind could expose.
            let authority = std::panic::AssertUnwindSafe(self.authority.clone());
            body.connection().progress_handler(
                1,
                Some(move || {
                    require_current_native_process().is_err()
                        || authority.debit_sql_vm_steps(1).is_err()
                }),
            );
            body.connection().busy_timeout(std::time::Duration::ZERO)?;
            body.connection().pragma_update(None, "mmap_size", 0)?;
            body.connection().pragma_update(None, "threads", 0)?;
            self.require_original(false)?;
            Ok(())
        })();
        match opened {
            Ok(()) => {
                let Some(NativeOpenOwner::Live(body)) = acquired.take() else {
                    unreachable!()
                };
                Ok(OriginalNativeConnection {
                    factory: self.clone(),
                    ticket: ticket.unwrap(),
                    producer,
                    domain,
                    deadline_active: std::cell::Cell::new(false),
                    deadline_anchor: std::cell::Cell::new(None),
                    hook_refused: std::cell::Cell::new(false),
                    body: Some(body),
                    destination: Some(destination),
                })
            }
            Err(error) => Err(OriginalNativeOpenFailure {
                domain,
                admission_failure,
                error,
                factory: self.clone(),
                ticket,
                producer,
                owner: std::mem::ManuallyDrop::new(Mutex::new(acquired)),
                destination: Some(destination),
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
        // Same unwind-boundary hook as acquisition; see the enrollment path.
        let authority = std::panic::AssertUnwindSafe(self.owner.factory.authority.clone());
        self.connection.progress_handler(
            1,
            Some(move || {
                require_current_native_process().is_err()
                    || authority.debit_sql_vm_steps(1).is_err()
            }),
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
        if self.owner.factory.require_process().is_err() {
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
        ensure!(self.process == std::process::id(),
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
        owner.factory.require_original(false)?;
        ensure!(
            Arc::ptr_eq(&self.factory, factory)
                && Arc::ptr_eq(&self.factory, &owner.factory)
                && self.ticket == owner.ticket
                && owner.domain == OriginalNativeDomain::NodeOperational
                && owner.body.is_some(),
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
/// Holds the SAME live owner and actual partial outcomes; Drop performs no cleanup.
pub struct LiveOperationalInitializationFailure {
    error: anyhow::Error,
    owner: std::mem::ManuallyDrop<OriginalNativeConnection>,
    outcome: LiveOperationalInitializationOutcome,
}
impl LiveOperationalInitializationFailure {
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn outcome(&self) -> LiveOperationalInitializationOutcome {
        self.outcome
    }
    pub fn into_parts(
        mut self,
    ) -> std::result::Result<
        (anyhow::Error, OriginalNativeConnection, LiveOperationalInitializationOutcome),
        Self,
    > {
        if self.owner.require_original_process().is_err() {
            return Err(self);
        }
        // Exactly this retained owner transfers once, together with its partial facts.
        let owner = unsafe { std::mem::ManuallyDrop::take(&mut self.owner) };
        Ok((self.error, owner, self.outcome))
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
    factory: Arc<OriginalNativeFactory>,
    ticket: u32,
    domain: OriginalNativeDomain,
    producer: Option<SqliteInspectionProductionPermit>,
    body: Option<NativeConnectionBody>,
    destination: Option<OriginalNativeDestinationInputs>,
}
impl OriginalNativeConnection {
    /// Check the actual factory process identity before changing wrapper state.
    pub fn require_original_process(&self) -> Result<()> {
        self.factory.require_process()
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
            let Some(OriginalNativeDestinationInputs::Bound(inputs)) = self.destination.as_ref() else {
                anyhow::bail!("live Bound inputs absent");
            };
            ensure!(
                matches!(inputs.mode, OriginalNativeBoundMode::Existing
                    | OriginalNativeBoundMode::CreateExclusive
                    | OriginalNativeBoundMode::OpenOrCreate),
                "live initialization durability requires writable Bound"
            );
            self.factory.require_ready()?;
            let _effect = self.factory.enter_effect()?;
            self.with_sql(|connection| {
                let (busy, log, checkpointed): (i64, i64, i64) = connection.query_row(
                    "PRAGMA wal_checkpoint(TRUNCATE)", [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                ensure!(busy == 0 && log == 0 && checkpointed == 0,
                    "live initialization checkpoint incomplete ({busy}, {log}, {checkpointed})");
                outcome.checkpoint_completed = true;
                self.factory.require_original(false)?;
                inputs.authority.require_original(&self.factory)?;
                let NativeConnectionBody::Bound(owner) = self.body.as_ref().context("live body absent")? else {
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
                self.factory.require_original(false)?;
                inputs.authority.require_original(&self.factory)?;
                self.factory.budget.debit_steps(1, false)?;
                inputs.root.sync()?;
                outcome.directory_synced = true;
                self.factory.require_original(false)?;
                inputs.authority.require_original(&self.factory)
            })?;
            self.factory.require_ready()?;
            bound_identity.context("live initialization Bound identity absent")
        })();
        match result {
            Ok(bound_identity) => {
                let receipt = DurableLiveOperationalInitialization {
                    factory: self.factory.clone(),
                    ticket: self.ticket,
                    bound_identity,
                };
                Ok((self, receipt))
            }
            Err(error) => Err(LiveOperationalInitializationFailure {
                error,
                owner: std::mem::ManuallyDrop::new(self),
                outcome,
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
    fn require_sql_owner(&self) -> Result<()> {
        require_current_native_process()?;
        ensure!(
            !self.hook_refused.get(),
            "original native hook/busy restoration refused; SQL owner retained"
        );
        if let Some(deadline) = self.deadline_anchor.get() {
            ensure!(
                !deadline.has_elapsed(),
                "original Runtime native deadline expired; no renewal"
            );
        }
        Ok(())
    }
    /// Only this same-original offline RuntimeMutation or live NodeRuntime
    /// claim/connection. No default clock, inspection hook replacement or deadline renewal.
    pub fn with_sql_deadline<T>(
        &self,
        deadline: lillux::time::MonotonicDeadline,
        operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> Result<T> {
        require_current_native_process()?;
        ensure!(
            matches!(
                self.domain,
                OriginalNativeDomain::RuntimeMutation | OriginalNativeDomain::NodeRuntime
            ),
            "original Runtime deadline requires offline RuntimeMutation or live NodeRuntime owner"
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
            // Same unwind-boundary hook as acquisition; see the enrollment path.
            let authority = std::panic::AssertUnwindSafe(self.factory.authority.clone());
            connection.progress_handler(
                1,
                Some(move || {
                    require_current_native_process().is_err() || effective.has_elapsed() || authority.debit_sql_vm_steps(1).is_err()
                }),
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
                (Err(original), Err(restore)) => Err(original.context(restore)),
            }
        })
    }
    /// Observe the actual scheduler keeper's creation disposition. This is
    /// metadata, not namespace or retirement authority; no connection escapes.
    pub fn scheduler_bound_origin(&self) -> Result<crate::BoundSqliteOrigin> {
        require_current_native_process()?;
        ensure!(self.domain == OriginalNativeDomain::NodeScheduler,
            "scheduler origin requires actual NodeScheduler owner");
        self.factory.require_ready()?;
        self.with_sql(|connection| {
            let NativeConnectionBody::Bound(owner) = self.body.as_ref()
                .context("original scheduler connection closed")? else {
                anyhow::bail!("scheduler origin requires actual Bound keeper")
            };
            self.factory.budget.debit_steps(1, false)?;
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
        self.factory.require_ready()?;
        self.with_sql(|connection| {
            let NativeConnectionBody::Bound(owner) = self.body.as_ref()
                .context("original scheduler connection closed")? else {
                anyhow::bail!("scheduler runtime check requires actual Bound keeper")
            };
            ensure!(owner.runtime().is_same_directory(runtime)?,
                "scheduler owner selects foreign runtime");
            runtime.ensure_path_binding()?;
            self.factory.budget.debit_steps(1, false)?;
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
        self.factory.require_ready()?;
        self.with_sql(|_| {
            let NativeConnectionBody::Bound(owner) = self.body.as_ref()
                .context("original scheduler connection closed")? else {
                anyhow::bail!("scheduler WAL requires actual Bound keeper")
            };
            self.factory.budget.debit_steps(1, false)?;
            owner.establish_wal_binding()
        })
    }
    /// Facts from the SAME admitted Bound keeper, within its lexical native
    /// scope. The caller retains this live owner when a comparison refuses.
    pub fn bound_main_identity(&self) -> Result<(u64, u64)> {
        require_current_native_process()?;
        self.with_sql(|connection| {
            let NativeConnectionBody::Bound(owner) = self
                .body
                .as_ref()
                .context("original live native body missing")?
            else {
                anyhow::bail!("original connection is not a Bound owner")
            };
            let binding = owner
                .binding
                .as_ref()
                .context("original Bound keeper missing")?;
            self.factory.budget.debit_steps(1, false)?;
            binding.validate(connection, false)?;
            binding.main_identity()
        })
    }
    pub fn with_sql<T>(
        &self,
        operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> Result<T> {
        require_current_native_process()?;
        self.require_sql_owner()?;
        self.factory.require_original(false)?;
        self.producer
            .as_ref()
            .context("original native producer missing")?
            .checkpoint()?;
        if let Some(destination) = &self.destination {
            destination.authority().require_original(&self.factory)?;
        }
        let _scope = self.factory.arena()?.scope(false, 0)?;
        let result = operation(
            self.body
                .as_ref()
                .context("original SQL connection closed")?
                .connection(),
        );
        let post = self
            .factory
            .require_original(false)
            .and_then(|()| self.require_sql_owner());
        match (result, post) {
            (Err(original), Err(post)) => Err(original.context(post)),
            (Err(original), _) => Err(original),
            (Ok(_), Err(post)) => Err(post),
            (Ok(value), Ok(())) => Ok(value),
        }
    }
    pub fn with_sql_mut<T>(
        &mut self,
        operation: impl for<'c> FnOnce(&'c mut Connection) -> Result<T>,
    ) -> Result<T> {
        require_current_native_process()?;
        self.require_sql_owner()?;
        self.factory.require_original(false)?;
        self.producer
            .as_ref()
            .context("original native producer missing")?
            .checkpoint()?;
        if let Some(destination) = &self.destination {
            destination.authority().require_original(&self.factory)?;
        }
        let _scope = self.factory.arena()?.scope(false, 0)?;
        let result = operation(
            self.body
                .as_mut()
                .context("original SQL connection closed")?
                .connection_mut(),
        );
        let post = self
            .factory
            .require_original(false)
            .and_then(|()| self.require_sql_owner());
        match (result, post) {
            (Err(original), Err(post)) => Err(original.context(post)),
            (Err(original), _) => Err(original),
            (Ok(_), Err(post)) => Err(post),
            (Ok(value), Ok(())) => Ok(value),
        }
    }
    pub fn close(
        mut self,
    ) -> std::result::Result<Arc<ClosedOriginalNativeConnection>, OriginalNativeCloseFailure> {
        let process_error = self.factory.require_process().err();
        let body = self.body.take().expect("original live native body");
        OriginalNativeCloseFailure {
            error: process_error,
            bound_identity: None,
            factory: self.factory.clone(),
            ticket: self.ticket,
            domain: self.domain,
            producer: self.producer.take(),
            owner: std::mem::ManuallyDrop::new(Mutex::new(Some(NativeCloseOwner::Live(body)))),
            destination: self.destination.take(),
        }
        .retry()
    }
}
impl Drop for OriginalNativeConnection {
    fn drop(&mut self) {
        if self.factory.require_process().is_err() {
            if let Some(p) = self.producer.take() {
                std::mem::forget(p);
            }
            if let Some(destination) = self.destination.take() {
                std::mem::forget(destination);
            }
        }
        if let Some(body) = self.body.take() {
            std::mem::forget(body);
        } /* no terminal from Drop */
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
                    if self.1 == std::process::id() && require_current_native_process().is_ok() {
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
        &owner.factory
    }
}
/// Affine SAME original owner; Drop retains every phase and never settles it.
pub struct OriginalSchedulerRetirement {
    body: std::mem::ManuallyDrop<OriginalSchedulerRetirementBody>,
}
pub struct OriginalSchedulerRetirementFailure {
    error: anyhow::Error,
    owner: std::mem::ManuallyDrop<Mutex<OriginalSchedulerRetirement>>,
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
        let owner = self.owner.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        if owner.body.factory().require_process().is_err() { return Err(self); }
        // SAFETY: the process-checked exclusive owner is taken exactly once.
        let mutex = unsafe { std::mem::ManuallyDrop::take(&mut self.owner) };
        Ok(mutex.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}
impl OriginalNativeConnection {
    /// Stop ordinary SQL by transferring this SAME scheduler connection into
    /// authenticated retirement. Failure retains the original connection or
    /// exact partial role owner; no ordinary Bound can escape.
    pub fn begin_scheduler_retirement(self, inputs: OriginalSchedulerRetirementInputs)
        -> std::result::Result<(OriginalSchedulerRetirement, crate::CapturedSqliteRoleFacts), OriginalSchedulerRetirementFailure> {
        let owner = OriginalSchedulerRetirement {
            body: std::mem::ManuallyDrop::new(OriginalSchedulerRetirementBody {
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
        OriginalSchedulerRetirementFailure { error, owner: std::mem::ManuallyDrop::new(Mutex::new(self)) }
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
                ensure!(owner.body.is_some(), "contradictory initial SQL custody"); false
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
        if consumed { self.body.sql_consumed = true; }
        if consumed && !self.body.c_sql_cleared {
            let arena = self.body.factory().arena()?;
            let code = unsafe { ryeos_native_connection_closed(arena.0.as_ptr(), self.body.ticket) };
            if code == libsqlite3_sys::SQLITE_OK { self.body.c_sql_cleared = true; }
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
                Some(OriginalSchedulerRetirementPhase::Initial(owner)) => owner.producer.as_ref(),
                _ => self.body.producer.as_ref(),
            }.context("original retirement producer absent")?;
            producer.checkpoint()?;
        }
        self.body.inputs.authority.prepay_effect(&factory,
            self.body.inputs.generation, effect, cleanup)?;
        self.require_authority(cleanup)?;
        let _scope = factory.arena()?.scope(cleanup, 0)?;
        let result = operation(&mut self.body);
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
                let Some(OriginalNativeDestinationInputs::Bound(destination)) = owner.destination.as_ref() else {
                    anyhow::bail!("scheduler destination absent");
                };
                ensure!(destination.name.as_os_str() == OsStr::new("scheduler.sqlite3"),
                    "retirement requires exact fixed scheduler role");
                if !cleanup { destination.authority.require_original(body.factory())?; }
                let NativeConnectionBody::Bound(bound) = owner.body.as_ref().context("scheduler SQL owner absent")? else {
                    anyhow::bail!("retirement requires actual scheduler Bound keeper")
                };
                bound.binding.as_ref().context("scheduler keeper absent")?.validate(bound.connection(), false)?;
                body.bound_identity = Some(bound.binding.as_ref().unwrap().main_identity()?);
            }
            if matches!(body.phase.as_ref(), Some(OriginalSchedulerRetirementPhase::Initial(_))) {
                let Some(OriginalSchedulerRetirementPhase::Initial(mut owner)) = body.phase.take() else { unreachable!() };
                let Some(NativeConnectionBody::Bound(bound)) = owner.body.take() else { unreachable!() };
                body.factory = Some(owner.factory.clone());
                body.destination = owner.destination.take();
                body.producer = owner.producer.take();
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
        let receipt = self.body.terminal.take().expect("actual retained retirement terminal");
        // SAFETY: explicit successful physical release and terminal completed;
        // only Rust custody is disposed here, with no automatic native close.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.body); }
        Ok(receipt)
    }
}

pub struct OriginalNativeCloseFailure {
    bound_identity: Option<(u64, u64)>,
    error: Option<anyhow::Error>,
    factory: Arc<OriginalNativeFactory>,
    ticket: u32,
    domain: OriginalNativeDomain,
    producer: Option<SqliteInspectionProductionPermit>,
    owner: std::mem::ManuallyDrop<Mutex<Option<NativeCloseOwner>>>,
    destination: Option<OriginalNativeDestinationInputs>,
}
impl OriginalNativeCloseFailure {
    pub fn error(&self) -> Option<&anyhow::Error> {
        self.error.as_ref()
    }
    fn retain_close_error(&mut self, error: anyhow::Error) {
        self.error = Some(match self.error.take() {
            Some(original) => original.context(error),
            None => error,
        });
    }
    pub fn retry(mut self) -> std::result::Result<Arc<ClosedOriginalNativeConnection>, Self> {
        let result = (|| -> Result<ClosedNativeBody> {
            self.factory.require_original(true)?;
            let _scope = self.factory.arena()?.scope(true, 0)?;
            // Snapshot from the genuine live Bound keeper before taking it.
            // Failed metadata validation retains the actual Live owner.
            if self.bound_identity.is_none() {
                if let Some(NativeCloseOwner::Live(NativeConnectionBody::Bound(owner))) = self
                    .owner
                    .get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    owner
                        .binding
                        .as_ref()
                        .context("original bound keeper missing")?
                        .validate(owner.connection(), false)?;
                    self.bound_identity = Some(owner.binding.as_ref().unwrap().main_identity()?);
                }
            }
            let owner = self
                .owner
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .context("original native close owner missing")?;
            match owner {
                NativeCloseOwner::Closed(body) => Ok(body),
                NativeCloseOwner::Live(body) => match body.close() {
                    Ok(body) => Ok(body),
                    Err(owner) => {
                        *self
                            .owner
                            .get_mut()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(owner);
                        anyhow::bail!("original native close pending")
                    }
                },
                NativeCloseOwner::Transient(c, _) => match c.close() {
                    Ok(()) => Ok(ClosedNativeBody::Plain),
                    Err((c, e)) => {
                        *self
                            .owner
                            .get_mut()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(NativeCloseOwner::Transient(c, e));
                        anyhow::bail!("original transient close pending")
                    }
                },
                NativeCloseOwner::Bound(f) => match f.retry() {
                    Ok(()) => Ok(ClosedNativeBody::Plain),
                    Err(f) => {
                        *self
                            .owner
                            .get_mut()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(NativeCloseOwner::Bound(f));
                        anyhow::bail!("original bound close pending")
                    }
                },
                NativeCloseOwner::Inspection(f) => match f.retry_close() {
                    Ok(p) => Ok(ClosedNativeBody::Inspection(Arc::new(p))),
                    Err(f) => {
                        *self
                            .owner
                            .get_mut()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(NativeCloseOwner::Inspection(f));
                        anyhow::bail!("original inspection close pending")
                    }
                },
            }
        })();
        match result {
            Ok(body) => {
                *self
                    .owner
                    .get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(NativeCloseOwner::Closed(body));
                let acknowledged = (|| -> Result<()> {
                    if let Some(NativeCloseOwner::Closed(ClosedNativeBody::Inspection(proof))) =
                        &*self
                            .owner
                            .get_mut()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                    {
                        proof.acknowledge_producer_terminal()?;
                    }
                    self.producer
                        .as_ref()
                        .context("native connection producer missing")?
                        .acknowledge_native_terminal()
                })();
                if let Err(error) = acknowledged {
                    self.retain_close_error(error);
                    return Err(self);
                }
                if let Err(error) = self.factory.mark_closed(self.ticket, false) {
                    self.retain_close_error(error);
                    return Err(self);
                }
                let Some(NativeCloseOwner::Closed(body)) = self
                    .owner
                    .get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                else {
                    unreachable!()
                };
                Ok(Arc::new(ClosedOriginalNativeConnection {
                    durable_body: OnceLock::new(),
                    durable_finalizer: AtomicBool::new(false),
                    bound_identity: self.bound_identity,
                    factory: self.factory.clone(),
                    ticket: self.ticket,
                    domain: self.domain,
                    body,
                    destination_authority: destination_authority_witness(&self.destination),
                    destination: Mutex::new(self.destination.take()),
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
impl Drop for OriginalNativeCloseFailure {
    fn drop(&mut self) {
        if self.factory.require_process().is_err() {
            if let Some(p) = self.producer.take() {
                std::mem::forget(p);
            }
            if let Some(destination) = self.destination.take() {
                std::mem::forget(destination);
            }
            return;
        }
        if let Some(owner) = self
            .owner
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            std::mem::forget(owner);
        }
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.owner);
        }
    }
}
impl std::fmt::Debug for OriginalNativeCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeCloseFailure")
            .field("error", &self.error)
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
        self.error.as_ref().map(|error| error.as_ref())
    }
}

pub struct OriginalNativeOpenFailure {
    domain: OriginalNativeDomain,
    admission_failure: Option<OriginalNativeConnectionAdmissionFailure>,
    error: anyhow::Error,
    factory: Arc<OriginalNativeFactory>,
    ticket: Option<u32>,
    producer: Option<SqliteInspectionProductionPermit>,
    owner: std::mem::ManuallyDrop<Mutex<Option<NativeOpenOwner>>>,
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
        self.factory.require_original(true)?;
        ensure!(self.ticket.is_none() && self.producer.is_none()
            && self.owner.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner).is_none(),
            "SQL admission recovery conflicts with actual native owner");
        let failure = self.admission_failure.take()
            .context("actual retained SQL admission absent")?;
        let original = &failure.original;
        let result = crate::original_physical_custody::retain_callback_refusal(&|| {
            self.factory.recover_undelivered_admission(
                &original.budget, original.ticket, &original.producer,
                SqliteInspectionProductionKind::Native)
        });
        if let Err(error) = result {
            self.admission_failure = Some(failure);
            return Err(error);
        }
        let OriginalNativeConnectionAdmissionFailure { original, error } = failure;
        self.error = std::mem::replace(&mut self.error,
            anyhow!("original admission error transferred")).context(error);
        let original = std::mem::ManuallyDrop::into_inner(original);
        self.ticket = Some(original.ticket);
        self.producer = Some(original.producer);
        Ok(())
    }
    pub fn retained_admission(&self) -> Option<&OriginalNativeConnectionAdmissionFailure> {
        if self.factory.require_process().is_err() { return None; }
        self.admission_failure.as_ref()
    }
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn retry_close(
        mut self,
    ) -> std::result::Result<Arc<ReleasedOriginalNativeAcquisition>, Self> {
        let result = (|| -> Result<()> {
            self.factory.require_original(true)?;
            ensure!(self.admission_failure.is_none(),
                "actual published SQL admission unresolved; no close or no-ticket terminal");
            let _scope = self.factory.arena()?.scope(true, 0)?;
            if let Some(owner) = self
                .owner
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let closed = match owner {
                    NativeOpenOwner::Released(p) => {
                        *self
                            .owner
                            .get_mut()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(NativeOpenOwner::Released(p));
                        None
                    }
                    NativeOpenOwner::Closed(body) => Some(body),
                    NativeOpenOwner::Bound(f) => match f.retry_close() {
                        Ok(()) => Some(ClosedNativeBody::Plain),
                        Err(f) => {
                            *self
                                .owner
                                .get_mut()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(NativeOpenOwner::Bound(f));
                            anyhow::bail!("original partial bound retained")
                        }
                    },
                    NativeOpenOwner::Inspection(f) => match f.retry_close() {
                        Ok(p) => {
                            *self
                                .owner
                                .get_mut()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(NativeOpenOwner::Released(Arc::new(p)));
                            None
                        }
                        Err(f) => {
                            *self
                                .owner
                                .get_mut()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(NativeOpenOwner::Inspection(f));
                            anyhow::bail!("original partial inspection retained")
                        }
                    },
                    NativeOpenOwner::Live(body) => match body.close() {
                        Ok(body) => Some(body),
                        Err(owner) => {
                            *self
                                .owner
                                .get_mut()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(NativeOpenOwner::Closing(owner));
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
                                *self
                                    .owner
                                    .get_mut()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(NativeOpenOwner::Closing(owner));
                                anyhow::bail!("original acquisition close retry retained")
                            }
                        }
                    }
                };
                if let Some(body) = closed {
                    *self
                        .owner
                        .get_mut()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(NativeOpenOwner::Closed(body));
                }
            }
            if let Some(NativeOpenOwner::Released(proof)) = &*self
                .owner
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                proof.acknowledge_producer_terminal()?;
            }
            if let Some(NativeOpenOwner::Closed(ClosedNativeBody::Inspection(proof))) = &*self
                .owner
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                proof.acknowledge_producer_terminal()?;
            }
            if let Some(ticket) = self.ticket {
                if self.factory.storage()?.tickets[ticket as usize - 1].load(Ordering::Acquire) != 3 {
                    self.factory.budget.debit_steps(1, true)?;
                    self.factory.require_original(true)?;
                    self.factory.mark_closed(ticket, true)?;
                }
            }
            if let Some(producer) = &self.producer {
                producer.acknowledge_native_terminal()?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                let (inspection, closed_inspection) = match self
                    .owner
                    .get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    Some(NativeOpenOwner::Released(p)) => (Some(p), None),
                    Some(NativeOpenOwner::Closed(ClosedNativeBody::Inspection(p))) => {
                        (None, Some(p))
                    }
                    Some(NativeOpenOwner::Closed(ClosedNativeBody::Plain)) | None => (None, None),
                    _ => unreachable!(),
                };
                Ok(Arc::new(ReleasedOriginalNativeAcquisition {
                    domain: Some(self.domain),
                    factory: self.factory.clone(),
                    ticket: self.ticket,
                    original_error: std::mem::replace(
                        &mut self.error,
                        anyhow::anyhow!("original error transferred"),
                    ),
                    inspection,
                    closed_inspection,
                    destination_authority: destination_authority_witness(&self.destination),
                    destination: Mutex::new(self.destination.take()),
                }))
            }
            Err(error) => {
                self.error =
                    std::mem::replace(&mut self.error, anyhow::anyhow!("original error retained"))
                        .context(error);
                Err(self)
            }
        }
    }
}
impl Drop for OriginalNativeOpenFailure {
    fn drop(&mut self) {
        if self.factory.require_process().is_err() {
            if let Some(p) = self.producer.take() {
                std::mem::forget(p);
            }
            if let Some(destination) = self.destination.take() {
                std::mem::forget(destination);
            }
            return;
        }
        if let Some(owner) = self
            .owner
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            std::mem::forget(owner);
        }
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.owner);
        }
    }
}
impl std::fmt::Debug for OriginalNativeOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeOpenFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalNativeOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
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
    process: u32,
    parts: std::mem::ManuallyDrop<OriginalNativeOutsideConnectionParts>,
}
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
        ensure!(self.process == std::process::id() && other.process == self.process,
            "foreign original leaf identity comparison");
        Ok(match (&self.identity, &other.identity) {
            (OriginalNativeOutsideLeafIdentity::Closed(a), OriginalNativeOutsideLeafIdentity::Closed(b)) => std::sync::Weak::ptr_eq(a, b),
            (OriginalNativeOutsideLeafIdentity::Released(a), OriginalNativeOutsideLeafIdentity::Released(b)) => std::sync::Weak::ptr_eq(a, b),
            _ => false,
        })
    }
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>, aggregate: &OriginalNativeTerminal) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original outside leaf evidence");
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
        ensure!(self.process == std::process::id(), "foreign original leaf observation"); Ok(self.ticket)
    }
    pub fn domain(&self) -> Result<Option<OriginalNativeDomain>> {
        ensure!(self.process == std::process::id(), "foreign original leaf observation"); Ok(self.domain)
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
    pub fn into_original_parts(mut self) -> std::result::Result<OriginalNativeOutsideConnectionParts, Self> {
        if self.process != std::process::id() { return Err(self); }
        Ok(unsafe { std::mem::ManuallyDrop::take(&mut self.parts) })
    }
}
pub struct OriginalNativeOutsideCustodyFailure {
    error: anyhow::Error,
    original: std::mem::ManuallyDrop<OriginalNativeConnectionTerminal>,
}
impl OriginalNativeOutsideCustodyFailure {
    pub fn into_original(mut self) -> std::result::Result<OriginalNativeConnectionTerminal, Self> {
        let factory = match &*self.original {
            OriginalNativeConnectionTerminal::Closed(owner) => &owner.factory,
            OriginalNativeConnectionTerminal::Released(owner) => &owner.factory,
        };
        if factory.require_process().is_err() { return Err(self); }
        Ok(unsafe { std::mem::ManuallyDrop::take(&mut self.original) })
    }
}
impl std::fmt::Debug for OriginalNativeOutsideCustodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeOutsideCustodyFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalNativeOutsideCustodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { std::fmt::Display::fmt(&self.error, f) }
}
impl std::error::Error for OriginalNativeOutsideCustodyFailure {}
impl OriginalNativeConnectionTerminal {
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
            return Err(OriginalNativeOutsideCustodyFailure { error, original: std::mem::ManuallyDrop::new(self) });
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
            return Err(OriginalNativeOutsideCustodyFailure {
                error: anyhow::anyhow!("original destination inputs were taken or are busy"),
                original: std::mem::ManuallyDrop::new(self),
            });
        }
        let process = std::process::id();
        let parts = match self {
            Self::Closed(owner) => {
                let identity = OriginalNativeOutsideLeafIdentity::Closed(Arc::downgrade(&owner));
                let owner = match Arc::try_unwrap(owner) {
                    Ok(owner) => owner,
                    Err(owner) => return Err(OriginalNativeOutsideCustodyFailure {
                        error: anyhow::anyhow!("actual closed receipt still has aliases"),
                        original: std::mem::ManuallyDrop::new(Self::Closed(owner)),
                    }),
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
                    Err(owner) => return Err(OriginalNativeOutsideCustodyFailure {
                        error: anyhow::anyhow!("actual released receipt still has aliases"),
                        original: std::mem::ManuallyDrop::new(Self::Released(owner)),
                    }),
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
        Ok(OriginalNativeOutsideConnectionCustody { process, parts: std::mem::ManuallyDrop::new(parts) })
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
enum OriginalMetadataPinState {
    Open(std::fs::File),
    PendingRaw(std::os::fd::RawFd),
    Retired,
    ConsumedError { code: i32, errno: i32 },
}
/// Owns the actual original O_PATH descriptor, with no File/FD/dup escape.
/// Unretired Drop retains the descriptor; it never fabricates a close receipt.
pub struct RetainedOriginalMetadataPin {
    state: std::mem::ManuallyDrop<Mutex<OriginalMetadataPinState>>,
    identity: OnceLock<lillux::OpenFileIdentity>,
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
            state: std::mem::ManuallyDrop::new(Mutex::new(OriginalMetadataPinState::Open(file))),
            identity: OnceLock::new(),
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
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original metadata retirement owner busy or poisoned"))?;
        if matches!(*state, OriginalMetadataPinState::Retired) {
            return Ok(RetiredOriginalMetadataPin {
                owner: self.clone(),
            });
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
            // Retain success before any late clock/funding check.
            drop(state);
            inputs.require_original(factory, terminal, self)?;
            Ok(RetiredOriginalMetadataPin {
                owner: self.clone(),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("original metadata retirement unavailable")
        }
    }
}
impl Drop for RetainedOriginalMetadataPin {
    fn drop(&mut self) {
        if require_current_native_process().is_err() {
            return;
        }
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let OriginalMetadataPinState::Open(file) =
            std::mem::replace(state, OriginalMetadataPinState::Retired)
        {
            use std::os::fd::IntoRawFd;
            let _retained_original_fd = file.into_raw_fd();
        }
        // PendingRaw is already retained; consumed coordinates are never closed again.
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.state);
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
}
pub trait OriginalFinalizerFileRetirementAuthority: Send + Sync {
    fn require_original(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<OriginalNativeTerminal>,
        owner: &Arc<RetainedOriginalFinalizerFile>,
    ) -> Result<()>;
    fn debit_original_steps(&self, steps: u64) -> Result<()>;
}
/// Trusted original post-ACK join for ONE actual transferred finalizer owner.
pub struct OriginalFinalizerFileRetirementInputs {
    factory: Arc<OriginalNativeFactory>,
    terminal: Arc<OriginalNativeTerminal>,
    owner: Arc<RetainedOriginalFinalizerFile>,
    authority: Arc<dyn OriginalFinalizerFileRetirementAuthority>,
}
impl OriginalFinalizerFileRetirementInputs {
    /// # Safety
    /// Retain genuine original State ACK, exact transferred finalizer custody,
    /// original charged clock/budget and actual native terminal. Prepay this
    /// input storage and verify retirement ordering: required namespace/ledger
    /// locks and evidence publishers must survive through their actual final
    /// effects. This owner must be the original File, never a duplicate used to
    /// stand in for retirement. Control records must have authenticated exact
    /// original input/account identity and no SQLite/data-lock aliases. Directory/State lock authority
    /// includes ALL actual fork/lock ownership; this input grants no registry
    /// unregister, explicit unlock, alias disposal or aggregate-zero proof.
    pub unsafe fn issued(
        factory: Arc<OriginalNativeFactory>,
        terminal: Arc<OriginalNativeTerminal>,
        owner: Arc<RetainedOriginalFinalizerFile>,
        authority: Arc<dyn OriginalFinalizerFileRetirementAuthority>,
    ) -> Result<Self> {
        require_current_native_process()?;
        terminal.require_original(&factory)?;
        authority.require_original(&factory, &terminal, &owner)?;
        Ok(Self {
            factory,
            terminal,
            owner,
            authority,
        })
    }
    fn require_original(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<OriginalNativeTerminal>,
        owner: &Arc<RetainedOriginalFinalizerFile>,
    ) -> Result<()> {
        require_current_native_process()?;
        ensure!(
            Arc::ptr_eq(factory, &self.factory)
                && Arc::ptr_eq(terminal, &self.terminal)
                && Arc::ptr_eq(owner, &self.owner),
            "foreign original finalizer retirement inputs"
        );
        terminal.require_original(factory)?;
        self.authority.require_original(factory, terminal, owner)
    }
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
        ensure!(self.process == std::process::id(), "outside descriptor grant belongs to original process");
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
    state: std::mem::ManuallyDrop<Mutex<OriginalMetadataPinState>>,
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
            state: std::mem::ManuallyDrop::new(Mutex::new(OriginalMetadataPinState::Open(file))),
            identity: OnceLock::new(),
        })
    }
    fn require_original_process(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "actual finalizer descriptor belongs to original process");
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
            OriginalFinalizerFileKind::ControlRecord | OriginalFinalizerFileKind::StateLock => {
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
    pub fn retire_after_backend(
        self: &Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<OriginalNativeTerminal>,
        inputs: &OriginalFinalizerFileRetirementInputs,
    ) -> Result<RetiredOriginalFinalizerFile> {
        require_current_native_process()?;
        let result = self.retire_after_backend_owned(factory, terminal, inputs);
        result.map_err(|error| {
            anyhow::Error::new(OriginalFinalizerFileRetirementFailure {
                error,
                owner: self.clone(),
            })
        })
    }
    fn retire_after_backend_owned(
        self: &Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<OriginalNativeTerminal>,
        inputs: &OriginalFinalizerFileRetirementInputs,
    ) -> Result<RetiredOriginalFinalizerFile> {
        require_current_native_process()?;
        terminal.require_original(factory)?;
        inputs.require_original(factory, terminal, self)?;
        // Flags, identity and one actual close have a fixed finite charge.
        // Host booking guards are released before descriptor-owner/native work.
        inputs.authority.debit_original_steps(6)?;
        terminal.require_original(factory)?;
        inputs.require_original(factory, terminal, self)?;
        self.retire_original_descriptor(&|| inputs.require_original(factory, terminal, self))
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
            let result = crate::binding::close_original_finalizer_file(
                *fd,
                identity.device(),
                identity.inode(),
                self.kind as i32,
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
impl Drop for RetainedOriginalFinalizerFile {
    fn drop(&mut self) {
        if self.require_original_process().is_err() {
            return;
        }
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let OriginalMetadataPinState::Open(file) =
            std::mem::replace(state, OriginalMetadataPinState::Retired)
        {
            use std::os::fd::IntoRawFd;
            let _retained_original_fd = file.into_raw_fd();
        }
        // PendingRaw is already retained; consumed coordinates are never closed again.
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.state);
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

pub struct OriginalFinalizerFileRetirementFailure {
    error: anyhow::Error,
    owner: Arc<RetainedOriginalFinalizerFile>,
}
impl OriginalFinalizerFileRetirementFailure {
    pub fn owner(&self) -> &Arc<RetainedOriginalFinalizerFile> {
        &self.owner
    }
}
impl std::fmt::Debug for OriginalFinalizerFileRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalFinalizerFileRetirementFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalFinalizerFileRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for OriginalFinalizerFileRetirementFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
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
        ensure!(self.process == std::process::id(), "foreign original budget facade");
        Ok(())
    }
    fn with_raw_budget<T>(&self, cleanup: bool,
        apply: impl FnOnce(&OriginalNativeBudget) -> Result<T>) -> Result<T> {
        self.require_original_process()?;
        let retained = self.region.try_lock()
            .map_err(|_| anyhow!("original budget facade busy or poisoned"))?;
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
        let retained = self.region.try_lock()
            .map_err(|_| anyhow!("original budget provenance gate busy or poisoned"))?;
        Ok(retained.as_ref().is_some_and(|original| original.same_region(region)))
    }
    pub fn same_original(&self, other: &Self) -> bool {
        if self.require_original_process().is_err() || other.require_original_process().is_err() { return false; }
        let Ok(original) = self.region.try_lock() else { return false; };
        let Some(original) = original.as_ref() else { return false; };
        if std::ptr::eq(self, other) { return true; }
        let Ok(other) = other.region.try_lock() else { return false; };
        other.as_ref().is_some_and(|other| original.same_region(other))
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
        let mut retained = self.region.try_lock()
            .map_err(|_| anyhow!("original budget handle retirement busy or poisoned"))?;
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
        let previous = state.controls_next.load(Ordering::Acquire);
        let next = previous.checked_add(1).ok_or(FixedHostRefusal::Overflow)?;
        if next as usize > MAX_NATIVE_PRODUCER_CELLS { return Err(FixedHostRefusal::Exhausted); }
        state.controls_next.compare_exchange(previous, next, Ordering::AcqRel, Ordering::Acquire).map_err(|_| FixedHostRefusal::Busy)?;
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
}
#[cfg(test)]
impl OriginalNativeFactory {
    /// Arm one fresh ticket around `open` on this thread. The open gate
    /// consumes the ticket at the first SQLite open inside `open`.
    pub(crate) fn arm_raw_for_test<T>(self: &Arc<Self>, open: impl FnOnce() -> T) -> Result<(T, RawTestTicket)> {
        let admission = self.ticket()?;
        let ticket = admission.ticket;
        self.storage()?.tickets[ticket as usize - 1].store(1, Ordering::Release);
        let raw = RawTestTicket { ticket, producer: admission.producer };
        let value = {
            let _scope = self.arena()?.scope(false, ticket)?;
            open()
        };
        Ok((value, raw))
    }
    /// Run raw SQLite work inside an unticketed allocator scope.
    pub(crate) fn raw_scope_for_test<T>(&self, work: impl FnOnce() -> T) -> Result<T> {
        let _scope = self.arena()?.scope(false, 0)?;
        Ok(work())
    }
    /// Recorded shared-mapping bytes and nonempty ledger slots.
    pub(crate) fn shared_census_for_test(&self) -> Result<(u64, u32)> {
        let arena = self.arena()?;
        let (mut bytes, mut slots) = (0u64, 0u32);
        let rc = unsafe { ryeos_native_shared_census(arena.0.as_ptr(), &mut bytes, &mut slots) };
        ensure!(rc == libsqlite3_sys::SQLITE_OK, "native shared census refused: {rc}");
        Ok((bytes, slots))
    }
    /// Run raw native cleanup work inside a cleanup allocator scope.
    pub(crate) fn raw_cleanup_scope_for_test<T>(&self, work: impl FnOnce() -> T) -> Result<T> {
        let _scope = self.arena()?.scope(true, 0)?;
        Ok(work())
    }
    /// Close an armed ticket into a released terminal for the host census.
    pub(crate) fn retire_raw_for_test(self: &Arc<Self>, raw: RawTestTicket) -> Result<OriginalNativeConnectionTerminal> {
        let RawTestTicket { ticket, producer } = raw;
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
