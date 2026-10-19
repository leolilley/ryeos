//! Finite original outside directory/namespace inventory. This groups only
//! aliases of actual retained owners, never paths or matching inode numbers.
//! Admission, original funding and settlement remain with the real supervisor.
use crate::{
    OriginalFinalizerFileKind, OriginalOutsideFinalizerFileRetirementInputs,
    RetainedOriginalFinalizerFile, RetiredOriginalFinalizerFile,
};
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, OnceLock};

/// Preserve the original panic payload as evidence while returning the actual
/// source to its supervisor. This opaque payload supplies no authority.
pub struct OriginalPhysicalCallbackPanic {
    process: u32,
    payload: std::sync::Mutex<Option<Box<dyn std::any::Any + Send>>>,
}
impl OriginalPhysicalCallbackPanic {
    /// Recover the SAME standard Rust panic payload once. It supplies no
    /// capability or grant; source recovery and all original debits remain
    /// with the actual supervisor. Busy/foreign-process refusal keeps it here.
    pub fn take_original_payload(&self) -> Result<Option<Box<dyn std::any::Any + Send>>> {
        ensure!(
            !crate::foreign_process(self.process),
            "foreign original physical panic custody"
        );
        let mut payload = self
            .payload
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original physical panic custody busy or poisoned"))?;
        Ok(payload.take())
    }
}
impl std::fmt::Debug for OriginalPhysicalCallbackPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("original physical custody callback panicked")
    }
}
impl std::fmt::Display for OriginalPhysicalCallbackPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("original physical custody callback panicked; original source retained")
    }
}
impl std::error::Error for OriginalPhysicalCallbackPanic {}
fn original_prepay(prepay: &dyn Fn(u64, u64) -> Result<()>, memory: u64, work: u64) -> Result<()> {
    // The callback may already have irreversibly charged the original account.
    // Catching unwind grants no refund, renewed deadline or replacement grant.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepay(memory, work))) {
        Ok(result) => result,
        Err(payload) => Err(anyhow::Error::new(OriginalPhysicalCallbackPanic {
            process: std::process::id(),
            payload: std::sync::Mutex::new(Some(payload)),
        })),
    }
}

/// Run a trusted original controller callback while its actual source remains
/// cached by the caller. A callback refusal or unwind never supplies a refund,
/// terminal receipt, alias retirement or replacement authority.
pub fn retain_callback_refusal(callback: &dyn Fn() -> Result<()>) -> Result<()> {
    original_prepay(&|_, _| callback(), 0, 0)
}

enum Source {
    Directory(Arc<lillux::PinnedDirectory>),
    Namespace(lillux::PinnedDirectoryLock),
    ControlRecord(Arc<RetainedOriginalFinalizerFile>),
    #[cfg(target_os = "linux")]
    Operator(Arc<lillux::ExactExclusiveFileLock>),
}
impl Source {
    fn same_owner(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Directory(a), Self::Directory(b)) => Arc::ptr_eq(a, b),
            (Self::Namespace(a), Self::Namespace(b)) => a.same_retained_owner(b),
            (Self::ControlRecord(a), Self::ControlRecord(b)) => Arc::ptr_eq(a, b),
            #[cfg(target_os = "linux")]
            (Self::Operator(a), Self::Operator(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}
/// Descriptor identities observed from an actual source at registration.
/// These copied facts survive consuming handoff; they cannot reconstruct an
/// owner, establish alias equality, or authorize physical retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginalPhysicalSourceIdentity {
    Directory {
        device: u64,
        inode: u64,
    },
    Namespace {
        device: u64,
        inode: u64,
    },
    ControlRecord {
        device: u64,
        inode: u64,
    },
    Operator {
        target_device: u64,
        target_inode: u64,
        parent_device: u64,
        parent_inode: u64,
    },
}
impl Source {
    fn observe_identity(&self) -> Result<OriginalPhysicalSourceIdentity> {
        Ok(match self {
            Self::Directory(actual) => {
                let (device, inode) = actual.device_inode()?;
                OriginalPhysicalSourceIdentity::Directory { device, inode }
            }
            Self::Namespace(actual) => {
                let identity = actual.observe_retained_identity()?;
                OriginalPhysicalSourceIdentity::Namespace {
                    device: identity.device(),
                    inode: identity.inode(),
                }
            }
            Self::ControlRecord(actual) => {
                let identity = actual.observed_identity()?;
                OriginalPhysicalSourceIdentity::ControlRecord {
                    device: identity.device(),
                    inode: identity.inode(),
                }
            }
            #[cfg(target_os = "linux")]
            Self::Operator(actual) => {
                let identity = actual.observe_retained_descriptors()?;
                OriginalPhysicalSourceIdentity::Operator {
                    target_device: identity.target().device(),
                    target_inode: identity.target().inode(),
                    parent_device: identity.parent().device(),
                    parent_inode: identity.parent().inode(),
                }
            }
        })
    }
}
struct Observation {
    process: u32,
    registry: Arc<()>,
    slot_index: usize,
    owner: OnceLock<Arc<RetainedOriginalFinalizerFile>>,
    receipt: OnceLock<Arc<RetiredOriginalFinalizerFile>>,
    operator: bool,
    source_identity: OriginalPhysicalSourceIdentity,
    operator_target_retired: OnceLock<()>,
}
/// Only operator-target retirement consumes the exclusive lock body. A slot
/// dropped while still holding it keeps the lock and its registered
/// descriptors retained, counted, until the process exits.
const OPERATOR_LOCK_KIND: &str = "lillux-sqlite::physical-operator-lock";
struct Slot {
    source: Option<Source>,
    #[cfg(target_os = "linux")]
    operator_body: Option<lillux::custody::Held<lillux::ExactExclusiveFileLock>>,
    observation: Arc<Observation>,
}
/// Opaque membership in one actual original inventory. Cloning membership
/// shares observation, not a descriptor, a lock lease, or a new admission.
#[derive(Clone)]
pub struct OriginalPhysicalRegistration {
    observation: Arc<Observation>,
}
impl OriginalPhysicalRegistration {
    /// Compare two opaque registrations by the identity of their actual slot.
    /// Matching source roles or descriptor identities do not make memberships
    /// equivalent across registries.
    pub fn same_member(&self, other: &Self) -> Result<bool> {
        ensure!(
            !crate::foreign_process(self.observation.process)
                && !crate::foreign_process(other.observation.process),
            "foreign original physical membership"
        );
        Ok(Arc::ptr_eq(&self.observation, &other.observation))
    }

    /// Return this actual registration's bounded slot in one exact registry.
    /// The index is only a cache position; it neither constructs nor authorizes
    /// access to the registry. Retaining this registration keeps the slot
    /// observation alive, and the registry identity check rejects equal indices
    /// from another inventory.
    pub fn source_slot_in(&self, expected: &OriginalPhysicalRegistryIdentity) -> Result<usize> {
        ensure!(
            !crate::foreign_process(self.observation.process)
                && !crate::foreign_process(expected.process)
                && Arc::ptr_eq(&self.observation.registry, &expected.identity),
            "original physical registration belongs to another registry"
        );
        Ok(self.observation.slot_index)
    }

    /// This role selects the late account-exclusion tail. It is not a close
    /// receipt or authority to omit the target/parent from the full inventory.
    /// Ordinary descriptor retirement must leave this role pending until the
    /// original account finishes durable work under the retained lock.
    pub fn is_operator_lock(&self) -> Result<bool> {
        ensure!(
            !crate::foreign_process(self.observation.process),
            "foreign original physical membership"
        );
        Ok(self.observation.operator)
    }
    #[cfg(target_os = "linux")]
    pub fn operator_target_retired(&self) -> Result<bool> {
        ensure!(
            !crate::foreign_process(self.observation.process),
            "foreign original physical membership"
        );
        Ok(self.observation.operator && self.observation.operator_target_retired.get().is_some())
    }
    /// Actual registration-time source facts, not current-open or close
    /// evidence. The complete original registry must still establish membership
    /// and census; a decoded matching identity never substitutes for its owner.
    pub fn observed_source_identity(&self) -> Result<OriginalPhysicalSourceIdentity> {
        ensure!(
            !crate::foreign_process(self.observation.process),
            "foreign original physical source observation"
        );
        Ok(self.observation.source_identity)
    }
    pub fn owner(&self) -> Result<Arc<RetainedOriginalFinalizerFile>> {
        ensure!(
            !crate::foreign_process(self.observation.process),
            "foreign original physical membership"
        );
        self.observation.owner.get().cloned().with_context(|| {
            format!(
                "original physical owner not transferred (slot {}, operator {}, source {:?})",
                self.observation.slot_index,
                self.observation.operator,
                self.observation.source_identity
            )
        })
    }
    pub fn receipt(&self) -> Result<Arc<RetiredOriginalFinalizerFile>> {
        ensure!(
            !crate::foreign_process(self.observation.process),
            "foreign original physical membership"
        );
        let owner = self.owner()?;
        let receipt = self
            .observation
            .receipt
            .get()
            .cloned()
            .context("original physical descriptor not retired")?;
        receipt.require_original(&owner)?;
        Ok(receipt)
    }
    /// Return this source's actual close receipt only when it belongs to the
    /// exact consumed registry. A close receipt by itself does not bind a
    /// registration to the inventory whose retirement the caller observed.
    pub fn receipt_in(
        &self,
        retired_registry: &RetiredOriginalPhysicalRegistry,
    ) -> Result<Arc<RetiredOriginalFinalizerFile>> {
        ensure!(
            !crate::foreign_process(self.observation.process),
            "foreign original physical membership"
        );
        retired_registry.require_same_identity_token(&self.observation.registry)?;
        self.receipt()
    }
    pub fn retired(&self) -> bool {
        self.receipt().is_ok()
    }
}
/// Must remain cached under the original outside supervisor on every refusal.
/// The inventory is finite and can be sealed only once all actual producers
/// have handed off their owners. Sealing is not native exclusion or settlement.
pub struct OriginalPhysicalRegistry {
    process: u32,
    identity: Arc<()>,
    maximum: usize,
    inner: lillux::custody::Held<RegistrySlots>,
}
const REGISTRY_KIND: &str = "lillux-sqlite::physical-registry";
/// The inventory's slots and its sealed flag. Constructible only in this
/// module.
struct RegistrySlots {
    sealed: bool,
    slots: Vec<Slot>,
}
/// Finite ordinary-descriptor phase of one inventory.
fn non_account_descriptors_retired_in(parts: &RegistrySlots) -> bool {
    parts.sealed
        && !parts.slots.is_empty()
        && parts
            .slots
            .iter()
            .filter(|slot| !slot.observation.operator)
            .all(|slot| {
                slot.source.is_none()
                    && slot.observation.owner.get().is_some()
                    && slot.observation.receipt.get().is_some()
            })
}
/// Every slot's descriptor is retired.
fn descriptors_retired_in(parts: &RegistrySlots) -> bool {
    parts.sealed
        && !parts.slots.is_empty()
        && parts.slots.iter().all(|slot| {
            slot.source.is_none()
                && (!slot.observation.operator
                    || slot.observation.operator_target_retired.get().is_some())
                && slot.observation.owner.get().is_some()
                && slot.observation.receipt.get().is_some()
        })
}
/// Whether no slot holds a source, an operator lock body or a prepared close
/// owner: an unused or emptied inventory owns nothing.
fn holds_no_custody_in(parts: &RegistrySlots) -> bool {
    parts.slots.iter().all(|slot| {
        #[cfg(target_os = "linux")]
        if slot.operator_body.is_some() {
            return false;
        }
        slot.source.is_none() && slot.observation.owner.get().is_none()
    })
}
/// A registry whose every descriptor is retired, the predicate `retire`
/// requires, or one that holds no source, lock body or close owner at all
/// releases its slots normally. Otherwise its sources, close owners and
/// operator locks stay retained, counted, until the process exits.
fn registry_settled(parts: &RegistrySlots) -> bool {
    descriptors_retired_in(parts) || holds_no_custody_in(parts)
}

/// Opaque identity captured from the actual pre-mapping registry. Cloning it
/// preserves identity only; it cannot mutate, retire, or replace that registry.
#[derive(Clone)]
pub struct OriginalPhysicalRegistryIdentity {
    process: u32,
    identity: Arc<()>,
}

impl OriginalPhysicalRegistryIdentity {
    /// Compare two retained identities without reopening the registry or
    /// consulting any native resident state. This is used when a completed
    /// pre-disposal join must later be matched to the exact retired inventory.
    pub fn require_same_registry(&self, other: &Self) -> Result<()> {
        ensure!(
            !crate::foreign_process(self.process)
                && !crate::foreign_process(other.process)
                && Arc::ptr_eq(&self.identity, &other.identity),
            "original physical registry identities differ"
        );
        Ok(())
    }
}

/// Evidence that one exact original physical registry was consumed after all
/// registered descriptor owners produced matching physical-close receipts.
/// This proves registry retirement only, not native disposal or account
/// settlement.
pub struct RetiredOriginalPhysicalRegistry {
    process: u32,
    identity: Arc<()>,
}

impl RetiredOriginalPhysicalRegistry {
    /// Return the opaque identity of this retired registry without exposing
    /// registrations or reopening its consumed bookkeeping. Retained
    /// post-disposal owners use this only to make receipt consumption
    /// idempotent for the same physical inventory.
    pub fn identity(&self) -> Result<OriginalPhysicalRegistryIdentity> {
        ensure!(
            !crate::foreign_process(self.process),
            "foreign retired original physical registry"
        );
        Ok(OriginalPhysicalRegistryIdentity {
            process: self.process,
            identity: Arc::clone(&self.identity),
        })
    }

    pub fn require_same_registry(&self, expected: &OriginalPhysicalRegistryIdentity) -> Result<()> {
        ensure!(
            !crate::foreign_process(self.process)
                && !crate::foreign_process(expected.process)
                && Arc::ptr_eq(&self.identity, &expected.identity),
            "retired physical registry differs from the exact original registry"
        );
        Ok(())
    }

    pub(crate) fn require_same_identity_token(&self, expected: &Arc<()>) -> Result<()> {
        ensure!(
            !crate::foreign_process(self.process) && Arc::ptr_eq(&self.identity, expected),
            "retired physical registry differs from the exact original identity"
        );
        Ok(())
    }
}

/// A checked amount from the genuine outside cleanup partition. This is a
/// quote only; it supplies neither backing nor admission authority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutsideWork {
    pub bytes: u64,
    pub steps: u64,
}
impl OutsideWork {
    pub const ZERO: Self = Self { bytes: 0, steps: 0 };

    pub fn checked_add(self, other: Self) -> Result<Self> {
        Ok(Self {
            bytes: self
                .bytes
                .checked_add(other.bytes)
                .context("outside cleanup storage quote overflow")?,
            steps: self
                .steps
                .checked_add(other.steps)
                .context("outside cleanup work quote overflow")?,
        })
    }

    pub fn checked_mul(self, count: u64) -> Result<Self> {
        Ok(Self {
            bytes: self
                .bytes
                .checked_mul(count)
                .context("outside cleanup storage quote overflow")?,
            steps: self
                .steps
                .checked_mul(count)
                .context("outside cleanup work quote overflow")?,
        })
    }
}
impl OriginalPhysicalRegistry {
    /// Bounded control storage/work for this actual slot layout and identity.
    /// This quote grants no backing or membership. Descriptor observations and
    /// close owners are charged separately when actual sources transfer.
    pub fn storage_quote(maximum: usize) -> Result<OutsideWork> {
        ensure!(maximum > 0, "empty original physical inventory bound");
        let bytes = maximum
            .checked_mul(std::mem::size_of::<Slot>())
            .and_then(|bytes| {
                bytes.checked_add(std::mem::size_of::<Self>() + 2 * std::mem::size_of::<usize>())
            })
            .context("physical inventory storage overflow")?;
        let work = u64::try_from(maximum)?
            .checked_mul(8)
            .context("physical inventory work overflow")?;
        Ok(OutsideWork {
            bytes: u64::try_from(bytes)?,
            steps: work,
        })
    }
    /// Worst-case charge for bounded actual-source registration attempts at
    /// full inventory occupancy. The caller supplies a proven finite attempt
    /// count, including aliases and retries; slot capacity alone is not that
    /// proof.
    pub fn registration_quote(maximum: usize, attempts: u64) -> Result<OutsideWork> {
        ensure!(maximum > 0, "empty original physical registration bound");
        let per_attempt = u64::try_from(maximum)?
            .checked_add(8)
            .context("physical registration work quote overflow")?;
        Ok(OutsideWork {
            bytes: attempts
                .checked_mul(512)
                .context("physical registration storage quote overflow")?,
            steps: attempts
                .checked_mul(per_attempt)
                .context("physical registration work quote overflow")?,
        })
    }
    /// Worst-case preparation of one actual retained close owner per slot.
    /// Existing control-record owners may consume less; this remains an upper
    /// quote for the complete inventory.
    pub fn descriptor_owner_quote(maximum: usize) -> Result<OutsideWork> {
        ensure!(maximum > 0, "empty original physical descriptor bound");
        let slots = u64::try_from(maximum)?;
        Ok(OutsideWork {
            bytes: slots
                .checked_mul(512)
                .context("physical descriptor storage quote overflow")?,
            steps: slots
                .checked_mul(4)
                .context("physical descriptor work quote overflow")?,
        })
    }
    /// Final operator target closes charge their actual owner transfer before
    /// touching the target. `targets` must be derived from the source census.
    pub fn operator_target_retirement_quote(targets: u64) -> Result<OutsideWork> {
        Ok(OutsideWork {
            bytes: targets
                .checked_mul(512)
                .context("operator retirement storage quote overflow")?,
            steps: targets
                .checked_mul(8)
                .context("operator retirement work quote overflow")?,
        })
    }
    /// The observation method scans at most the currently registered slots
    /// and allocates one fixed observation per operator source.
    #[cfg(target_os = "linux")]
    pub fn operator_observation_quote(maximum: usize, operators: u64) -> Result<OutsideWork> {
        ensure!(maximum > 0, "empty operator observation inventory bound");
        let per_observation =
            u64::try_from(std::mem::size_of::<lillux::ExactExclusiveFileLockObservation>())?;
        let per_scan = u64::try_from(maximum)?
            .checked_add(8)
            .context("operator observation work quote overflow")?;
        Ok(OutsideWork {
            bytes: operators
                .checked_mul(per_observation)
                .context("operator observation storage quote overflow")?,
            steps: operators
                .checked_mul(per_scan)
                .context("operator observation work quote overflow")?,
        })
    }
    /// Quote bounded membership-token checks for reservation-time planning.
    /// A token carries its registry identity, so validating it is constant-time
    /// and does not scan the inventory. Dynamic callers should also use
    /// `membership_work_bound` to validate the projected slot count.
    pub fn membership_work_quote(maximum_slots: usize, queries: u64) -> Result<OutsideWork> {
        ensure!(
            maximum_slots > 0,
            "empty original physical membership bound"
        );
        const MEMBERSHIP_CHECK_STEPS: u64 = 4;
        let steps = queries
            .checked_mul(MEMBERSHIP_CHECK_STEPS)
            .and_then(|work| work.checked_add(4))
            .context("original physical membership work quote overflow")?;
        Ok(OutsideWork { bytes: 0, steps })
    }
    /// # Safety
    /// The genuine original controller funds this exact finite inventory from
    /// its admitted envelope before allocation. `prepay` must irreversibly
    /// debit that envelope and original clock; a numeric maximum is no grant.
    pub unsafe fn issued(maximum: usize, prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<Self> {
        let quote = Self::storage_quote(maximum)?;
        original_prepay(prepay, quote.bytes, quote.steps)?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(maximum)
            .context("reserve original physical inventory")?;
        Ok(Self {
            process: std::process::id(),
            identity: Arc::new(()),
            maximum,
            inner: lillux::custody::Held::new(
                REGISTRY_KIND,
                RegistrySlots {
                    sealed: false,
                    slots,
                },
                registry_settled,
            ),
        })
    }
    fn require_process(&self) -> Result<()> {
        ensure!(
            !crate::foreign_process(self.process),
            "foreign original physical registry"
        );
        Ok(())
    }
    /// Charge component transfer storage through the same unwind-preserving
    /// boundary before moving its actual source. This creates no authority;
    /// the genuine outside supervisor supplies its original account and clock.
    /// Prove the exact projected slot count for the caller's serialized stage.
    /// `additional_sources` is the maximum number of actual registrations the
    /// same serialized stage completes before those queries. The registry
    /// cannot be mutated through this borrow; callers must calculate and debit
    /// this quote immediately before that stage. The query work itself is
    /// constant-time per opaque membership token; actual source-registration
    /// scans are charged by `register` separately.
    pub fn membership_work_bound(
        &self,
        queries: u64,
        additional_sources: usize,
    ) -> Result<OutsideWork> {
        self.require_process()?;
        let future_slots = self
            .inner
            .slots
            .len()
            .checked_add(additional_sources)
            .context("original physical projected membership count overflow")?;
        ensure!(
            future_slots <= self.maximum,
            "projected original physical membership exceeds inventory capacity"
        );
        Self::membership_work_quote(future_slots, queries)
    }
    pub fn prepay_transfer(
        &self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        memory: u64,
        work: u64,
    ) -> Result<()> {
        self.require_process()?;
        original_prepay(prepay, memory, work)
    }
    /// # Safety
    /// Hand over the actual original root, under the same admitted supervisor.
    /// A refusal returns that whole owner. No reopen or inode substitution.
    pub unsafe fn register_directory(
        &mut self,
        owner: Arc<lillux::PinnedDirectory>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        OriginalPhysicalRegistration,
        (Arc<lillux::PinnedDirectory>, anyhow::Error),
    > {
        match self.register(Source::Directory(owner), prepay) {
            Ok(binding) => Ok(binding),
            Err((Source::Directory(owner), error)) => Err((owner, error)),
            Err(_) => unreachable!("directory source type preserved"),
        }
    }
    /// # Safety
    /// Hand over the actual original lock lease. Exact shared owner identity
    /// may join a prior slot while the canonical lease stays retained there.
    pub unsafe fn register_namespace(
        &mut self,
        owner: lillux::PinnedDirectoryLock,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        OriginalPhysicalRegistration,
        (lillux::PinnedDirectoryLock, anyhow::Error),
    > {
        match self.register(Source::Namespace(owner), prepay) {
            Ok(binding) => Ok(binding),
            Err((Source::Namespace(owner), error)) => Err((owner, error)),
            Err(_) => unreachable!("namespace source type preserved"),
        }
    }
    /// # Safety
    /// A source-specific original owner supplied this exact control-record
    /// descriptor through its consuming handoff, with authentic original
    /// funding. The source no longer needs it for publication or service use;
    /// it contains no SQLite/data-lock alias, and any actual duplicates also
    /// join this same registry before sealing. No inode observation, reopened
    /// file or manufactured close owner suffices.
    pub unsafe fn register_control_record(
        &mut self,
        owner: Arc<RetainedOriginalFinalizerFile>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        OriginalPhysicalRegistration,
        (Arc<RetainedOriginalFinalizerFile>, anyhow::Error),
    > {
        if !matches!(
            owner.kind(),
            OriginalFinalizerFileKind::ControlRecord | OriginalFinalizerFileKind::MetadataPin
        ) {
            return Err((
                owner,
                anyhow::anyhow!("physical control registration has a foreign role"),
            ));
        }
        match self.register(Source::ControlRecord(owner), prepay) {
            Ok(binding) => Ok(binding),
            Err((Source::ControlRecord(owner), error)) => Err((owner, error)),
            Err(_) => unreachable!("control record source type preserved"),
        }
    }
    /// # Safety
    /// Transfer the actual account exclusion owner under its original outside
    /// supervisor. All aliases must join this SAME allocation before preparing
    /// descriptor owners; neither inode equality nor a reopened lock qualifies.
    #[cfg(target_os = "linux")]
    pub unsafe fn register_operator_lock(
        &mut self,
        owner: Arc<lillux::ExactExclusiveFileLock>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        OriginalPhysicalRegistration,
        (Arc<lillux::ExactExclusiveFileLock>, anyhow::Error),
    > {
        if let Err(error) = owner.require_original_process() {
            return Err((owner, error));
        }
        match self.register(Source::Operator(owner), prepay) {
            Ok(binding) => Ok(binding),
            Err((Source::Operator(owner), error)) => Err((owner, error)),
            Err(_) => unreachable!("operator source type preserved"),
        }
    }
    fn register(
        &mut self,
        source: Source,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<OriginalPhysicalRegistration, (Source, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_process()?;
            ensure!(!self.inner.sealed, "original physical registration sealed");
            // Charge the actual finite worst-case scan before searching. No
            // uncharged repeated alias census or replacement budget on refusal.
            let quote = Self::registration_quote(self.maximum, 1)?;
            original_prepay(prepay, quote.bytes, quote.steps)
        })();
        if let Err(error) = checked {
            return Err((source, error));
        }
        if let Some(slot) = self.inner.slots.iter().find(|slot| {
            slot.source
                .as_ref()
                .is_some_and(|actual| actual.same_owner(&source))
        }) {
            // Only this redundant alias drops. The exact actual root/lock
            // remains retained in the canonical slot, with no close effect.
            return Ok(OriginalPhysicalRegistration {
                observation: Arc::clone(&slot.observation),
            });
        }
        if self.inner.slots.len() >= self.maximum {
            return Err((
                source,
                anyhow::anyhow!("original physical inventory exhausted"),
            ));
        }
        let operator = match &source {
            #[cfg(target_os = "linux")]
            Source::Operator(_) => true,
            _ => false,
        };
        // The same original registration quote covers the actual descriptor
        // observations before allocation or ownership transfer. Refusal returns
        // the whole source, with no replacement descriptor or receipt.
        let source_identity = match source.observe_identity() {
            Ok(identity) => identity,
            Err(error) => return Err((source, error)),
        };
        let observation = Arc::new(Observation {
            process: self.process,
            registry: Arc::clone(&self.identity),
            slot_index: self.inner.slots.len(),
            owner: OnceLock::new(),
            receipt: OnceLock::new(),
            operator,
            source_identity,
            operator_target_retired: OnceLock::new(),
        });
        self.inner.get_mut().slots.push(Slot {
            source: Some(source),
            #[cfg(target_os = "linux")]
            operator_body: None,
            observation: Arc::clone(&observation),
        });
        Ok(OriginalPhysicalRegistration { observation })
    }
    /// # Safety
    /// Every real source has registered and new producers are excluded by the
    /// original controller. This closes registration only, not native custody.
    pub unsafe fn seal_inventory(&mut self) -> Result<()> {
        self.require_process()?;
        ensure!(
            !self.inner.slots.is_empty(),
            "original physical inventory has no actual sources"
        );
        self.inner.get_mut().sealed = true;
        Ok(())
    }
    pub fn registrations(&self) -> Result<impl Iterator<Item = OriginalPhysicalRegistration> + '_> {
        self.require_process()?;
        Ok(self
            .inner
            .slots
            .iter()
            .map(|slot| OriginalPhysicalRegistration {
                observation: Arc::clone(&slot.observation),
            }))
    }
    /// Opaque identity for binding a retained destination to this exact
    /// registry across its pre-disposal permit and post-disposal receipt check.
    /// This keeps no native factory or resident region owner alive.
    pub(crate) fn identity_token(&self) -> Result<Arc<()>> {
        self.require_process()?;
        Ok(Arc::clone(&self.identity))
    }
    pub(crate) fn require_identity_token(&self, identity: &Arc<()>) -> Result<()> {
        self.require_process()?;
        ensure!(
            Arc::ptr_eq(&self.identity, identity),
            "original physical registry identity changed"
        );
        Ok(())
    }
    /// Bind another retained owner to this exact registry before native
    /// resource creation. The returned value cannot construct a registry or
    /// prove that it has been retired.
    pub fn identity(&self) -> Result<OriginalPhysicalRegistryIdentity> {
        self.require_process()?;
        Ok(OriginalPhysicalRegistryIdentity {
            process: self.process,
            identity: Arc::clone(&self.identity),
        })
    }
    /// Consume bookkeeping only after actual same-owner receipts for every
    /// descriptor. The returned receipt binds to this exact pre-mapping
    /// registry. This grants no durable account settlement or native seal.
    pub fn retire(self) -> std::result::Result<RetiredOriginalPhysicalRegistry, Self> {
        if !self.descriptors_retired() {
            return Err(self);
        }
        let receipt = RetiredOriginalPhysicalRegistry {
            process: self.process,
            identity: Arc::clone(&self.identity),
        };
        // Every descriptor is retired, so this registry's Drop releases its
        // bookkeeping slots normally.
        drop(self);
        Ok(receipt)
    }
    /// # Safety
    /// The original supervisor prepays and retains this registry throughout
    /// transfer and later descriptor retirement. Effects cannot run beneath a
    /// caller retention mutex. Every partial refusal keeps its actual source.
    pub unsafe fn prepare_descriptor_owners(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        ensure!(self.inner.sealed, "original physical inventory still open");
        let mut shared = None;
        for slot in self.inner.get_mut().slots.iter_mut() {
            if slot.observation.owner.get().is_some() {
                continue;
            }
            #[cfg(target_os = "linux")]
            if slot.operator_body.is_some() {
                continue;
            }
            let already_owned = matches!(slot.source.as_ref(), Some(Source::ControlRecord(_)));
            let quote = Self::descriptor_owner_quote(1)?;
            let bytes = if already_owned { 0 } else { quote.bytes };
            original_prepay(prepay, bytes, quote.steps)?;
            let source = slot
                .source
                .take()
                .context("physical slot missing original source")?;
            let source = match source {
                Source::ControlRecord(actual) => {
                    // The same already prepared close owner moves into the
                    // sealed observation; no duplicate or new owner is created.
                    assert!(slot.observation.owner.set(actual).is_ok());
                    continue;
                }
                source => source,
            };
            #[cfg(target_os = "linux")]
            let source = match source {
                Source::Operator(actual) => {
                    // Account holders still validate through Weak backedges to
                    // this SAME allocation. Keep it alive through their final
                    // durable work; ordinary descriptor preparation cannot
                    // consume their canonical authority even if uniquely owned.
                    slot.source = Some(Source::Operator(actual));
                    continue;
                }
                source => source,
            };
            let transferred = match source {
                Source::Directory(actual) => match Arc::try_unwrap(actual) {
                    Ok(directory) => Ok((
                        directory.into_owned_close_descriptor(),
                        OriginalFinalizerFileKind::Directory,
                    )),
                    Err(actual) => Err(Source::Directory(actual)),
                },
                #[cfg(target_os = "linux")]
                Source::Operator(_) => unreachable!("operator body retained separately"),
                Source::Namespace(actual) => match actual.try_into_owned_close_descriptor() {
                    Ok(file) => Ok((file, OriginalFinalizerFileKind::DirectoryLock)),
                    Err(actual) => Err(Source::Namespace(actual)),
                },
                Source::ControlRecord(_) => unreachable!("control owner transferred above"),
            };
            match transferred {
                Ok((file, kind)) => {
                    let owner = unsafe { RetainedOriginalFinalizerFile::issued(file, kind) };
                    // The only setter runs here under exclusive registry access;
                    // observation aliases expose getters and cannot publish.
                    assert!(slot.observation.owner.set(owner).is_ok());
                }
                Err(source) => {
                    let holders = match &source {
                        Source::Directory(actual) => Arc::strong_count(actual),
                        _ => 0,
                    };
                    slot.source = Some(source);
                    shared = Some((
                        slot.observation.slot_index,
                        slot.observation.source_identity,
                        holders,
                    ));
                    break;
                }
            }
        }
        if let Some((index, identity, holders)) = shared {
            anyhow::bail!(
                "original physical source still shared outside complete inventory (slot {index}, source {identity:?}, strong holders {holders})"
            );
        }
        Ok(())
    }
    /// # Safety
    /// The genuine original supervisor has completed all durable account work
    /// needing these locks, excluded producers and joined every actual alias.
    /// It retains the same funded cleanup deadline and debits via `prepay`.
    /// No caller retention mutex may span this native operation. Refusal keeps
    /// the original body; an ambiguous failed close is cached without FD retry.
    #[cfg(target_os = "linux")]
    pub unsafe fn retire_operator_targets(
        &mut self,
        original: lillux::time::MonotonicDeadline,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_process()?;
        ensure!(self.inner.sealed, "original physical inventory still open");
        ensure!(
            self.non_account_descriptors_retired(),
            "original non-operator descriptors remain open"
        );
        for slot in self
            .inner
            .get_mut()
            .slots
            .iter_mut()
            .filter(|slot| slot.observation.operator)
        {
            if slot.observation.operator_target_retired.get().is_some()
                && slot.observation.owner.get().is_some()
            {
                continue;
            }
            // Fund custody transfer, syscall and parent-owner publication
            // BEFORE moving any actual owner. This late phase is permitted only
            // once no durable account callback needs the canonical Arc again.
            let quote = Self::operator_target_retirement_quote(1)?;
            original_prepay(prepay, quote.bytes, quote.steps)?;
            if slot.operator_body.is_none() {
                let source = slot
                    .source
                    .take()
                    .context("actual operator source absent")?;
                match source {
                    Source::Operator(actual) => match Arc::try_unwrap(actual) {
                        Ok(body) => {
                            slot.operator_body =
                                Some(lillux::custody::Held::until_taken(OPERATOR_LOCK_KIND, body))
                        }
                        Err(actual) => {
                            slot.source = Some(Source::Operator(actual));
                            anyhow::bail!(
                                "actual operator lock still shared outside complete inventory"
                            );
                        }
                    },
                    source => {
                        slot.source = Some(source);
                        anyhow::bail!("operator membership changed source type");
                    }
                }
            }
            let body = slot
                .operator_body
                .as_mut()
                .context("actual operator body absent")?
                .get_mut();
            ensure!(
                body.try_retire_registered_target(original)?,
                "original registered lock close deferred"
            );
            // Only the genuine kernel success path reaches this observation.
            if slot.observation.operator_target_retired.get().is_none() {
                assert!(slot.observation.operator_target_retired.set(()).is_ok());
            }
            let held = slot
                .operator_body
                .take()
                .expect("actual operator body retained");
            let body = match held.take_whole() {
                Ok(body) => body,
                Err(held) => {
                    slot.operator_body = Some(held);
                    anyhow::bail!("actual operator lock belongs to another process");
                }
            };
            let parent = match body.into_retired_target_parent() {
                Ok(parent) => parent,
                Err((body, error)) => {
                    slot.operator_body =
                        Some(lillux::custody::Held::until_taken(OPERATOR_LOCK_KIND, body));
                    return Err(error);
                }
            };
            let owner = unsafe {
                RetainedOriginalFinalizerFile::issued(parent, OriginalFinalizerFileKind::Directory)
            };
            assert!(slot.observation.owner.set(owner).is_ok());
        }
        Ok(())
    }
    /// Opaque membership was made only by actual registration in this exact
    /// inventory. Checking it grants no close, native exclusion or settlement.
    pub fn require_membership(&self, registration: &OriginalPhysicalRegistration) -> Result<()> {
        self.require_process()?;
        ensure!(
            registration.observation.process == self.process
                && Arc::ptr_eq(&self.identity, &registration.observation.registry),
            "foreign original physical membership"
        );
        Ok(())
    }
    /// Borrowed proof that this exact directory allocation is STILL the
    /// canonical source retained in this registry. An inode match, copied
    /// observation, or post-transfer descriptor is not an ownership join.
    /// No extraction, descriptor operation or retirement is performed here.
    pub fn require_directory_source(
        &self,
        registration: &OriginalPhysicalRegistration,
        directory: &Arc<lillux::PinnedDirectory>,
    ) -> Result<()> {
        self.require_membership(registration)?;
        let slot = self
            .inner
            .slots
            .iter()
            .find(|slot| Arc::ptr_eq(&slot.observation, &registration.observation))
            .context("actual original directory membership slot absent")?;
        ensure!(
            matches!(slot.source.as_ref(), Some(Source::Directory(actual))
            if Arc::ptr_eq(actual, directory)),
            "original directory source is foreign or already transferred"
        );
        Ok(())
    }
    /// Borrowed proof that `owner` is the exact control-record owner of this
    /// membership: still retained as the registry source before descriptor
    /// preparation, or moved into the sealed observation afterwards. A copied
    /// identity or another owner allocation never qualifies.
    pub fn require_control_record_source(
        &self,
        registration: &OriginalPhysicalRegistration,
        owner: &Arc<RetainedOriginalFinalizerFile>,
    ) -> Result<()> {
        self.require_membership(registration)?;
        let slot = self
            .inner
            .slots
            .iter()
            .find(|slot| Arc::ptr_eq(&slot.observation, &registration.observation))
            .context("actual original control-record membership slot absent")?;
        let retained = match slot.source.as_ref() {
            Some(Source::ControlRecord(actual)) => Arc::ptr_eq(actual, owner),
            Some(_) => false,
            None => {
                matches!(
                    slot.observation.source_identity,
                    OriginalPhysicalSourceIdentity::ControlRecord {
                        device: _,
                        inode: _
                    }
                ) && slot
                    .observation
                    .owner
                    .get()
                    .is_some_and(|actual| Arc::ptr_eq(actual, owner))
            }
        };
        ensure!(
            retained,
            "original control-record source is foreign or changed owner"
        );
        Ok(())
    }
    /// Prove that `lock` is the exact live allocation retained by this
    /// operator membership. A matching lock path or inode is insufficient:
    /// this retires redundant `StateLockLease` aliases before the registry
    /// consumes the canonical target and parent descriptors.
    #[cfg(target_os = "linux")]
    pub fn require_operator_lock_alias(
        &self,
        registration: &OriginalPhysicalRegistration,
        lock: &Arc<lillux::ExactExclusiveFileLock>,
    ) -> Result<()> {
        self.require_membership(registration)?;
        ensure!(
            registration.is_operator_lock()?,
            "membership is not the original operator lock"
        );
        lock.require_original_process()?;
        let slot = self
            .inner
            .slots
            .iter()
            .find(|slot| Arc::ptr_eq(&slot.observation, &registration.observation))
            .context("actual operator lock membership slot absent")?;
        ensure!(
            matches!(slot.source.as_ref(), Some(Source::Operator(actual))
            if Arc::ptr_eq(actual, lock)),
            "operator alias is not the exact live lock retained by this registry"
        );
        Ok(())
    }
    /// Locate and validate the canonical operator membership when the Store
    /// never produced a parent frame. The same bounded scan proves every
    /// non-operator descriptor already has a physical receipt and matches the
    /// supplied lease to the exact live operator allocation in this registry.
    #[cfg(target_os = "linux")]
    pub fn require_operator_lock_alias_owner(
        &self,
        lock: &Arc<lillux::ExactExclusiveFileLock>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<OriginalPhysicalRegistration> {
        self.require_process()?;
        lock.require_original_process()?;
        let quote = Self::operator_observation_quote(self.maximum, 1)?;
        original_prepay(prepay, quote.bytes, quote.steps)?;
        let mut operator = None;
        for slot in self.inner.slots.iter() {
            if slot.observation.operator {
                ensure!(
                    operator.is_none(),
                    "original inventory contains multiple operator locks"
                );
                ensure!(
                    matches!(slot.source.as_ref(), Some(Source::Operator(actual))
                    if Arc::ptr_eq(actual, lock)),
                    "admission exclusion differs from the canonical live operator source"
                );
                operator = Some(Arc::clone(&slot.observation));
            } else {
                ensure!(
                    slot.observation.receipt.get().is_some(),
                    "non-operator original descriptor remains unretired before exclusion alias"
                );
            }
        }
        Ok(OriginalPhysicalRegistration {
            observation: operator
                .context("original operator membership absent from physical inventory")?,
        })
    }
    /// Observe the SAME operator target/parent while they are still retained
    /// in this exact registry. Call outside all caller bookkeeping gates with
    /// the original payer. No pathname/FD is exported or reopened, and these
    /// identity facts do not attest that either descriptor has been closed.
    #[cfg(target_os = "linux")]
    pub fn observe_operator_lock(
        &self,
        registration: &OriginalPhysicalRegistration,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<lillux::ExactExclusiveFileLockObservation> {
        self.require_membership(registration)?;
        ensure!(
            registration.is_operator_lock()?,
            "membership is not the original operator lock"
        );
        let quote = Self::operator_observation_quote(self.maximum, 1)?;
        original_prepay(prepay, quote.bytes, quote.steps)?;
        let slot = self
            .inner
            .slots
            .iter()
            .find(|slot| Arc::ptr_eq(&slot.observation, &registration.observation))
            .context("actual operator inventory slot absent")?;
        match slot.source.as_ref() {
            Some(Source::Operator(actual)) => actual.observe_retained_descriptors(),
            _ => slot
                .operator_body
                .as_ref()
                .context("actual operator descriptor body absent")?
                .observe_retained_descriptors(),
        }
    }
    pub fn retain_retirement(
        &mut self,
        registration: &OriginalPhysicalRegistration,
        receipt: Arc<RetiredOriginalFinalizerFile>,
    ) -> Result<()> {
        self.require_membership(registration)?;
        let owner = registration.owner()?;
        receipt.require_original(&owner)?;
        if registration.observation.receipt.get().is_none() {
            assert!(registration.observation.receipt.set(receipt).is_ok());
        }
        Ok(())
    }
    /// Retire every already prepared non-operator descriptor through the same
    /// sealed inventory. The issuer must bind each outside input to the exact
    /// membership and owner and irreversibly charge its retained authority
    /// before allocation. Inputs and receipts remain attached to each actual
    /// membership across refusal; a close that succeeded before a late
    /// authority refusal is recovered from the owner's physical state without
    /// repeating the syscall or debit. No bookkeeping mutex is held across
    /// either callback.
    ///
    /// # Safety
    /// All producers are excluded, the exact registry is sealed, non-operator
    /// source owners have been prepared, and durable account work that requires
    /// the canonical operator exclusion has finished. Both callbacks refer to
    /// the same original outside authority; `issue_inputs` cannot use native
    /// storage or substitute a matching descriptor.
    pub unsafe fn retire_non_operator_descriptors(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_inputs: &dyn Fn(
            &OriginalPhysicalRegistration,
            Arc<RetainedOriginalFinalizerFile>,
        ) -> Result<Arc<OriginalOutsideFinalizerFileRetirementInputs>>,
    ) -> Result<()> {
        self.require_process()?;
        ensure!(self.inner.sealed, "original physical inventory still open");
        if self.non_account_descriptors_retired() {
            return Ok(());
        }
        let count = self.inner.slots.len();
        for index in 0..count {
            let observation = &self.inner.slots[index].observation;
            if observation.operator || observation.receipt.get().is_some() {
                continue;
            }
            let registration = OriginalPhysicalRegistration {
                observation: Arc::clone(observation),
            };
            let owner = registration.owner()?;

            // A close may have succeeded even when its post-close authority
            // recheck refused. Cache that physical fact before asking the
            // payer for another attempt.
            if let Some(receipt) = owner.completed_close_receipt()? {
                self.retain_retirement(&registration, Arc::new(receipt))?;
                continue;
            }

            let membership_quote = self.membership_work_bound(1, 0)?;
            self.prepay_transfer(prepay, membership_quote.bytes, membership_quote.steps)?;
            self.require_membership(&registration)?;
            ensure!(
                !registration.is_operator_lock()?,
                "operator exclusion entered the non-operator descriptor phase"
            );

            let inputs = issue_inputs(&registration, owner.clone())?;
            inputs.require_original_owner(&owner)?;
            let receipt = match owner.retire_after_outside_handoff(&inputs) {
                Ok(receipt) => receipt,
                Err(error) => {
                    if let Some(receipt) = owner.completed_close_receipt()? {
                        self.retain_retirement(&registration, Arc::new(receipt))?;
                    }
                    return Err(error.context(format!(
                        "retire original descriptor (slot {}, role {:?}, source {:?})",
                        registration.observation.slot_index,
                        owner.kind(),
                        registration.observation.source_identity,
                    )));
                }
            };
            self.retain_retirement(&registration, Arc::new(receipt))?;
        }
        ensure!(
            self.non_account_descriptors_retired(),
            "actual non-operator original descriptors remain pending"
        );
        Ok(())
    }
    /// Close the actual operator parent descriptor only after the canonical
    /// target was retired by `retire_operator_targets`. Its matching outside
    /// input and physical receipt are retained on refusal just like every
    /// other registered descriptor.
    ///
    /// # Safety
    /// The exact sealed inventory is retained, all non-operator descriptors
    /// have actual receipts, durable account work is complete, and both
    /// callbacks use the same original late authority. No target alias may be
    /// reconstructed or retried from an inode observation.
    #[cfg(target_os = "linux")]
    pub unsafe fn retire_operator_parent_descriptor(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_inputs: &dyn Fn(
            &OriginalPhysicalRegistration,
            Arc<RetainedOriginalFinalizerFile>,
        ) -> Result<Arc<OriginalOutsideFinalizerFileRetirementInputs>>,
    ) -> Result<()> {
        self.require_process()?;
        ensure!(self.inner.sealed, "original physical inventory still open");
        if self.descriptors_retired() {
            return Ok(());
        }
        let count = self.inner.slots.len();
        let mut found_operator = false;
        for index in 0..count {
            let observation = &self.inner.slots[index].observation;
            if !observation.operator {
                continue;
            }
            found_operator = true;
            if observation.receipt.get().is_some() {
                continue;
            }
            let registration = OriginalPhysicalRegistration {
                observation: Arc::clone(observation),
            };
            ensure!(
                registration.operator_target_retired()?,
                "operator parent cannot close before its actual target"
            );
            let owner = registration.owner()?;
            if let Some(receipt) = owner.completed_close_receipt()? {
                self.retain_retirement(&registration, Arc::new(receipt))?;
                continue;
            }

            let membership_quote = self.membership_work_bound(1, 0)?;
            self.prepay_transfer(prepay, membership_quote.bytes, membership_quote.steps)?;
            self.require_membership(&registration)?;
            ensure!(
                registration.is_operator_lock()? && registration.operator_target_retired()?,
                "operator parent retirement lost its exact target receipt"
            );

            let inputs = issue_inputs(&registration, owner.clone())?;
            inputs.require_original_owner(&owner)?;
            let receipt = match owner.retire_after_outside_handoff(&inputs) {
                Ok(receipt) => receipt,
                Err(error) => {
                    if let Some(receipt) = owner.completed_close_receipt()? {
                        self.retain_retirement(&registration, Arc::new(receipt))?;
                    }
                    return Err(error.context(format!(
                        "retire original descriptor (slot {}, role {:?}, source {:?})",
                        registration.observation.slot_index,
                        owner.kind(),
                        registration.observation.source_identity,
                    )));
                }
            };
            self.retain_retirement(&registration, Arc::new(receipt))?;
        }
        ensure!(
            found_operator,
            "original physical inventory lacks its canonical operator target"
        );
        ensure!(
            self.descriptors_retired(),
            "original operator parent descriptor remains pending"
        );
        Ok(())
    }
    /// Finite ordinary-descriptor phase only. The operator exclusion source
    /// remains in the SAME inventory and prevents full descriptor completion.
    /// This is not a native-disposal, account-settlement or lock-close receipt.
    pub fn non_account_descriptors_retired(&self) -> bool {
        !self.inner.is_foreign() && non_account_descriptors_retired_in(&self.inner)
    }
    pub fn descriptors_retired(&self) -> bool {
        !self.inner.is_foreign() && descriptors_retired_in(&self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct FixtureClose {
        owner: Arc<RetainedOriginalFinalizerFile>,
        work: std::sync::Mutex<u64>,
    }
    // This is a disposable host-descriptor control, not authenticated node
    // admission, account settlement or a native lifecycle qualification.
    unsafe impl crate::OriginalOutsideFinalizerFileRetirementAuthority for FixtureClose {
        fn require_original(&self, owner: &Arc<RetainedOriginalFinalizerFile>) -> Result<()> {
            ensure!(
                Arc::ptr_eq(owner, &self.owner),
                "fixture selected a foreign descriptor owner"
            );
            Ok(())
        }
        fn debit_original_steps(&self, work: u64) -> Result<()> {
            let mut remaining = self.work.lock().unwrap();
            *remaining = remaining
                .checked_sub(work)
                .context("fixture close work exhausted")?;
            Ok(())
        }
    }

    /// Admits every recheck until the owner's descriptor is physically
    /// closed, then refuses: the refusal lands only on the post-close
    /// recheck, however many pre-close rechecks the retirement path makes.
    struct FixtureCloseLateRefusal {
        owner: Arc<RetainedOriginalFinalizerFile>,
    }
    unsafe impl crate::OriginalOutsideFinalizerFileRetirementAuthority for FixtureCloseLateRefusal {
        fn require_original(&self, owner: &Arc<RetainedOriginalFinalizerFile>) -> Result<()> {
            ensure!(
                Arc::ptr_eq(owner, &self.owner),
                "fixture selected a foreign descriptor owner"
            );
            ensure!(
                owner.completed_close_receipt()?.is_none(),
                "fixture late authority refused after close"
            );
            Ok(())
        }
        fn debit_original_steps(&self, _work: u64) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn dropped_empty_registry_releases_without_abandoning() {
        let _serial = crate::test_host::serial_guard();
        const KIND: &str = "lillux-sqlite::physical-registry";
        let registry = unsafe { OriginalPhysicalRegistry::issued(2, &|_, _| Ok(())) }.unwrap();
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(registry);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn dropped_unretired_registry_retains_its_slots() {
        let _serial = crate::test_host::serial_guard();
        const KIND: &str = "lillux-sqlite::physical-registry";
        let owner = unsafe {
            RetainedOriginalFinalizerFile::issued(
                tempfile::tempfile().unwrap(),
                crate::OriginalFinalizerFileKind::ControlRecord,
            )
        };
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        let _membership =
            unsafe { registry.register_control_record(Arc::clone(&owner), &|_, _| Ok(())) }
                .map_err(|(_, error)| error)
                .unwrap();
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(registry);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
        assert!(
            Arc::strong_count(&owner) >= 2,
            "the registered owner stays retained"
        );
    }

    #[test]
    fn control_record_owner_joins_before_and_after_descriptor_preparation() {
        let _serial = crate::test_host::serial_guard();
        let issued = || unsafe {
            RetainedOriginalFinalizerFile::issued(
                tempfile::tempfile().unwrap(),
                crate::OriginalFinalizerFileKind::ControlRecord,
            )
        };
        let owner = issued();
        let foreign = issued();
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        let membership =
            unsafe { registry.register_control_record(Arc::clone(&owner), &|_, _| Ok(())) }
                .map_err(|(_, error)| error)
                .unwrap();
        // Before sealing, the owner is still the registry source and has not
        // been published to the observation.
        assert!(membership.owner().is_err());
        registry
            .require_control_record_source(&membership, &owner)
            .unwrap();
        assert!(
            registry
                .require_control_record_source(&membership, &foreign)
                .is_err()
        );
        unsafe { registry.seal_inventory() }.unwrap();
        unsafe { registry.prepare_descriptor_owners(&|_, _| Ok(())) }.unwrap();
        assert!(Arc::ptr_eq(&membership.owner().unwrap(), &owner));
        registry
            .require_control_record_source(&membership, &owner)
            .unwrap();
        assert!(
            registry
                .require_control_record_source(&membership, &foreign)
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn metadata_pin_retires_only_through_the_metadata_close_path() {
        let _serial = crate::test_host::serial_guard();
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(temporary.path().join("marker"), b"m").unwrap();
        let directory = lillux::PinnedDirectory::open(temporary.path())
            .unwrap()
            .unwrap();
        let metadata_pin = || {
            directory
                .open_mount_entry(std::ffi::OsStr::new("marker"))
                .unwrap()
                .unwrap()
        };
        let inputs = |owner: Arc<RetainedOriginalFinalizerFile>| {
            let authority = Arc::new(FixtureClose {
                owner: owner.clone(),
                work: std::sync::Mutex::new(32),
            });
            Arc::new(
                unsafe {
                    crate::OriginalOutsideFinalizerFileRetirementInputs::issued(owner, authority)
                }
                .unwrap(),
            )
        };

        // A metadata-only pin cannot pass as a data control record, and a data
        // descriptor cannot pass as a metadata pin. Neither refusal consumes.
        let mislabeled_pin = unsafe {
            RetainedOriginalFinalizerFile::issued(
                metadata_pin(),
                crate::OriginalFinalizerFileKind::ControlRecord,
            )
        };
        assert!(
            mislabeled_pin
                .retire_after_outside_handoff(&inputs(mislabeled_pin.clone()))
                .is_err()
        );
        assert!(mislabeled_pin.completed_close_receipt().unwrap().is_none());
        let mislabeled_data = unsafe {
            RetainedOriginalFinalizerFile::issued(
                std::fs::File::open(temporary.path().join("marker")).unwrap(),
                crate::OriginalFinalizerFileKind::MetadataPin,
            )
        };
        assert!(
            mislabeled_data
                .retire_after_outside_handoff(&inputs(mislabeled_data.clone()))
                .is_err()
        );
        assert!(mislabeled_data.completed_close_receipt().unwrap().is_none());

        let owner = unsafe {
            RetainedOriginalFinalizerFile::issued(
                metadata_pin(),
                crate::OriginalFinalizerFileKind::MetadataPin,
            )
        };
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        unsafe { registry.register_control_record(Arc::clone(&owner), &|_, _| Ok(())) }
            .map_err(|(_, error)| error)
            .unwrap();
        unsafe { registry.seal_inventory() }.unwrap();
        unsafe { registry.prepare_descriptor_owners(&|_, _| Ok(())) }.unwrap();
        unsafe {
            registry.retire_non_operator_descriptors(&|_, _| Ok(()), &|_, owner| Ok(inputs(owner)))
        }
        .unwrap();
        assert!(registry.descriptors_retired());
        assert!(owner.completed_close_receipt().unwrap().is_some());
        assert!(registry.retire().is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn operator_alias_requires_the_exact_registered_lock_allocation() {
        let _serial = crate::test_host::serial_guard();
        let temporary = tempfile::tempdir().unwrap();
        let registered = Arc::new(
            lillux::ExactExclusiveFileLock::acquire(&temporary.path().join("operator.lock"))
                .unwrap(),
        );
        let foreign = Arc::new(
            lillux::ExactExclusiveFileLock::acquire(&temporary.path().join("foreign.lock"))
                .unwrap(),
        );
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        let membership =
            unsafe { registry.register_operator_lock(Arc::clone(&registered), &|_, _| Ok(())) }
                .map_err(|(_, error)| error)
                .unwrap();
        registry
            .require_operator_lock_alias(&membership, &registered)
            .unwrap();
        assert!(
            registry
                .require_operator_lock_alias(&membership, &foreign)
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn original_control_record_closes_before_operator_target_and_parent() {
        let _serial = crate::test_host::serial_guard();
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("operator.lock");
        let lock = Arc::new(lillux::ExactExclusiveFileLock::acquire(&target).unwrap());
        let record = unsafe {
            RetainedOriginalFinalizerFile::issued(
                tempfile::tempfile().unwrap(),
                OriginalFinalizerFileKind::ControlRecord,
            )
        };
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(2, &|_, _| Ok(())) }.unwrap();
        let registry_identity = registry.identity().unwrap();
        let control =
            match unsafe { registry.register_control_record(record.clone(), &|_, _| Ok(())) } {
                Ok(member) => member,
                Err(_) => panic!("original fixture record must register"),
            };
        let alias = match unsafe { registry.register_control_record(record, &|_, _| Ok(())) } {
            Ok(member) => member,
            Err(_) => panic!("same original fixture allocation must join"),
        };
        assert!(Arc::ptr_eq(&control.observation, &alias.observation));
        assert_eq!(control.source_slot_in(&registry_identity).unwrap(), 0);
        assert_eq!(alias.source_slot_in(&registry_identity).unwrap(), 0);
        assert_eq!(registry.inner.slots.len(), 1);
        let operator = match unsafe { registry.register_operator_lock(lock, &|_, _| Ok(())) } {
            Ok(member) => member,
            Err(_) => panic!("original fixture lock must register"),
        };
        assert_eq!(operator.source_slot_in(&registry_identity).unwrap(), 1);
        registry = match registry.retire() {
            Ok(_) => panic!("unsealed inventory cannot be retired"),
            Err(registry) => registry,
        };
        unsafe { registry.seal_inventory() }.unwrap();
        unsafe { registry.prepare_descriptor_owners(&|_, _| Ok(())) }.unwrap();
        let deadline = lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(5));
        assert!(unsafe { registry.retire_operator_targets(deadline, &|_, _| Ok(())) }.is_err());
        assert!(lillux::ExactExclusiveFileLock::acquire(&target).is_err());
        let issued_inputs = Arc::new(AtomicUsize::new(0));
        let issued_counter = Arc::clone(&issued_inputs);
        unsafe {
            registry
                .retire_non_operator_descriptors(&|_, _| Ok(()), &|_, owner| {
                    issued_counter.fetch_add(1, Ordering::SeqCst);
                    let authority = Arc::new(FixtureClose {
                        owner: owner.clone(),
                        work: std::sync::Mutex::new(32),
                    });
                    let inputs = crate::OriginalOutsideFinalizerFileRetirementInputs::issued(
                        owner, authority,
                    )?;
                    Ok(Arc::new(inputs))
                })
                .unwrap();
        }
        assert_eq!(issued_inputs.load(Ordering::SeqCst), 1);
        assert!(registry.non_account_descriptors_retired());
        assert!(!registry.descriptors_retired());
        unsafe {
            registry
                .retire_non_operator_descriptors(
                    &|_, _| panic!("a completed descriptor phase must not recharge"),
                    &|_, _| panic!("a completed descriptor phase must not reissue inputs"),
                )
                .unwrap();
        }
        assert_eq!(issued_inputs.load(Ordering::SeqCst), 1);
        unsafe { registry.retire_operator_targets(deadline, &|_, _| Ok(())) }.unwrap();
        assert!(
            !registry.descriptors_retired(),
            "operator parent still requires its own actual close"
        );
        unsafe {
            registry
                .retire_operator_parent_descriptor(&|_, _| Ok(()), &|_, owner| {
                    let authority = Arc::new(FixtureClose {
                        owner: owner.clone(),
                        work: std::sync::Mutex::new(32),
                    });
                    let inputs = crate::OriginalOutsideFinalizerFileRetirementInputs::issued(
                        owner, authority,
                    )?;
                    Ok(Arc::new(inputs))
                })
                .unwrap();
        }
        assert!(registry.descriptors_retired());
        registry_identity
            .require_same_registry(&registry.identity().unwrap())
            .unwrap();
        let retired = match registry.retire() {
            Ok(receipt) => receipt,
            Err(_) => panic!("fully retired inventory must produce a receipt"),
        };
        retired.require_same_registry(&registry_identity).unwrap();
        control.receipt_in(&retired).unwrap();
        alias.receipt_in(&retired).unwrap();
        operator.receipt_in(&retired).unwrap();
        let foreign_record = unsafe {
            RetainedOriginalFinalizerFile::issued(
                tempfile::tempfile().unwrap(),
                OriginalFinalizerFileKind::ControlRecord,
            )
        };
        let mut foreign = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        unsafe { foreign.register_control_record(foreign_record, &|_, _| Ok(())) }
            .map_err(|(_, error)| error)
            .unwrap();
        unsafe { foreign.seal_inventory() }.unwrap();
        unsafe { foreign.prepare_descriptor_owners(&|_, _| Ok(())) }.unwrap();
        unsafe {
            foreign
                .retire_non_operator_descriptors(&|_, _| Ok(()), &|_, owner| {
                    let authority = Arc::new(FixtureClose {
                        owner: owner.clone(),
                        work: std::sync::Mutex::new(32),
                    });
                    let inputs = crate::OriginalOutsideFinalizerFileRetirementInputs::issued(
                        owner, authority,
                    )?;
                    Ok(Arc::new(inputs))
                })
                .unwrap();
        }
        let foreign_identity = foreign.identity().unwrap();
        let foreign = match foreign.retire() {
            Ok(receipt) => receipt,
            Err(_) => panic!("fully retired foreign inventory must produce a receipt"),
        };
        assert!(
            registry_identity
                .require_same_registry(&foreign_identity)
                .is_err()
        );
        assert!(retired.require_same_registry(&foreign_identity).is_err());
        assert!(control.receipt_in(&foreign).is_err());
        assert!(control.source_slot_in(&foreign_identity).is_err());
        let _next = lillux::ExactExclusiveFileLock::acquire(&target).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_operator_close_caches_physical_receipt_before_late_authority_refusal() {
        let _serial = crate::test_host::serial_guard();
        let record = unsafe {
            RetainedOriginalFinalizerFile::issued(
                tempfile::tempfile().unwrap(),
                OriginalFinalizerFileKind::ControlRecord,
            )
        };
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        let member = unsafe { registry.register_control_record(record, &|_, _| Ok(())) }
            .map_err(|(_, error)| error)
            .unwrap();
        unsafe { registry.seal_inventory() }.unwrap();
        unsafe { registry.prepare_descriptor_owners(&|_, _| Ok(())) }.unwrap();
        let issued = Arc::new(AtomicUsize::new(0));
        let issue_count = Arc::clone(&issued);
        let result = unsafe {
            registry.retire_non_operator_descriptors(&|_, _| Ok(()), &|_, owner| {
                issue_count.fetch_add(1, Ordering::SeqCst);
                let authority = Arc::new(FixtureCloseLateRefusal {
                    owner: owner.clone(),
                });
                let inputs =
                    crate::OriginalOutsideFinalizerFileRetirementInputs::issued(owner, authority)?;
                Ok(Arc::new(inputs))
            })
        };
        assert!(result.is_err(), "late recheck must preserve its refusal");
        assert!(
            registry.non_account_descriptors_retired(),
            "the physical close receipt is retained despite late refusal"
        );
        assert!(member.receipt().is_ok());
        assert_eq!(issued.load(Ordering::SeqCst), 1);
        unsafe {
            registry
                .retire_non_operator_descriptors(
                    &|_, _| panic!("a closed descriptor must not be charged again"),
                    &|_, _| panic!("a closed descriptor must not receive new inputs"),
                )
                .unwrap();
        }
        assert_eq!(issued.load(Ordering::SeqCst), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ordinary_preparation_preserves_account_weak_authority_and_exclusion() {
        let _serial = crate::test_host::serial_guard();
        use std::os::unix::fs::MetadataExt as _;
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("operator.lock");
        let original = Arc::new(lillux::ExactExclusiveFileLock::acquire(&target).unwrap());
        let account_backedge = Arc::downgrade(&original);
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        let binding = match unsafe { registry.register_operator_lock(original, &|_, _| Ok(())) } {
            Ok(binding) => binding,
            Err(_) => panic!("actual fixture lock must register"),
        };
        assert!(binding.is_operator_lock().unwrap());
        let observed = registry
            .observe_operator_lock(&binding, &|_, _| Ok(()))
            .unwrap();
        assert_eq!(
            observed.target().inode(),
            std::fs::metadata(&target).unwrap().ino()
        );
        let original_identity = binding.observed_source_identity().unwrap();
        assert_eq!(
            original_identity,
            OriginalPhysicalSourceIdentity::Operator {
                target_device: observed.target().device(),
                target_inode: observed.target().inode(),
                parent_device: observed.parent().device(),
                parent_inode: observed.parent().inode(),
            }
        );
        unsafe { registry.seal_inventory() }.unwrap();
        unsafe { registry.prepare_descriptor_owners(&|_, _| Ok(())) }.unwrap();
        // This SAME canonical allocation is now the only strong lock owner.
        // Preparing other descriptors must not invalidate account backedges.
        let account_owner = account_backedge
            .upgrade()
            .expect("original account authority retained");
        account_owner.ensure_path_binding(&target).unwrap();
        assert!(lillux::ExactExclusiveFileLock::acquire(&target).is_err());
        assert!(binding.owner().is_err());
        assert_eq!(
            binding.observed_source_identity().unwrap(),
            original_identity
        );
        assert!(registry.non_account_descriptors_retired());
        assert!(!registry.descriptors_retired());
        drop(account_owner);
        // Fixture RAII cleanup only. It is not a registered close receipt,
        // durable account settlement or production disposal qualification.
        drop(std::mem::take(&mut registry.inner.get_mut().slots));
        assert!(account_backedge.upgrade().is_none());
        let _replacement = lillux::ExactExclusiveFileLock::acquire(&target).unwrap();
    }

    struct PanicEvidence(Arc<AtomicBool>);
    impl Drop for PanicEvidence {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    // These are source-authored host-custody controls, not native admission,
    // backing, producer-census or durable-settlement qualification.
    #[test]
    fn directory_source_join_requires_same_allocation_not_matching_inode() {
        let _serial = crate::test_host::serial_guard();
        let temporary = tempfile::tempdir().unwrap();
        let original = Arc::new(
            lillux::PinnedDirectory::open(temporary.path())
                .unwrap()
                .unwrap(),
        );
        let independently_opened = Arc::new(
            lillux::PinnedDirectory::open(temporary.path())
                .unwrap()
                .unwrap(),
        );
        assert_eq!(
            original.device_inode().unwrap(),
            independently_opened.device_inode().unwrap()
        );
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(1, &|_, _| Ok(())) }.unwrap();
        let binding = match unsafe { registry.register_directory(original.clone(), &|_, _| Ok(())) }
        {
            Ok(binding) => binding,
            Err(_) => panic!("actual directory fixture must register"),
        };
        registry
            .require_directory_source(&binding, &original)
            .unwrap();
        assert!(
            registry
                .require_directory_source(&binding, &independently_opened)
                .is_err()
        );
        drop(original);
        unsafe { registry.seal_inventory() }.unwrap();
        unsafe { registry.prepare_descriptor_owners(&|_, _| Ok(())) }.unwrap();
        assert!(
            registry
                .require_directory_source(&binding, &independently_opened)
                .is_err()
        );
        assert!(!registry.descriptors_retired());
        // Fixture cleanup is not an original production close receipt.
        drop(std::mem::take(&mut registry.inner.get_mut().slots));
    }

    #[test]
    fn empty_inventory_cannot_supply_a_retirement_proof() {
        let _serial = crate::test_host::serial_guard();
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(2, &|_, _| Ok(())) }.unwrap();
        assert!(unsafe { registry.seal_inventory() }.is_err());
        assert!(!registry.descriptors_retired());
        // No host owner was ever registered. Dispose only fixture bookkeeping.
        drop(std::mem::take(&mut registry.inner.get_mut().slots));
    }
    #[test]
    fn prepayment_unwind_returns_the_exact_original_root_and_keeps_evidence() {
        let _serial = crate::test_host::serial_guard();
        let temporary = tempfile::tempdir().unwrap();
        let original = Arc::new(
            lillux::PinnedDirectory::open(temporary.path())
                .unwrap()
                .unwrap(),
        );
        let witness = Arc::clone(&original);
        let evidence_dropped = Arc::new(AtomicBool::new(false));
        let callback_evidence = Arc::clone(&evidence_dropped);
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(2, &|_, _| Ok(())) }.unwrap();
        let result = unsafe {
            registry.register_directory(original, &|_, _| {
                std::panic::panic_any(PanicEvidence(Arc::clone(&callback_evidence)))
            })
        };
        let (returned, error) = match result {
            Err(refusal) => refusal,
            Ok(_) => panic!("panic must not register or dispose an original root"),
        };
        assert!(Arc::ptr_eq(&returned, &witness));
        assert_eq!(Arc::strong_count(&returned), 2);
        assert!(registry.inner.slots.is_empty());
        assert!(!registry.descriptors_retired());
        assert!(!evidence_dropped.load(Ordering::SeqCst));
        let refusal = error
            .downcast_ref::<OriginalPhysicalCallbackPanic>()
            .unwrap();
        let original_payload = refusal.take_original_payload().unwrap().unwrap();
        assert!(refusal.take_original_payload().unwrap().is_none());
        assert!(original_payload.is::<PanicEvidence>());
        drop(error);
        assert!(!evidence_dropped.load(Ordering::SeqCst));
        drop(original_payload);
        assert!(evidence_dropped.load(Ordering::SeqCst));
        drop(std::mem::take(&mut registry.inner.get_mut().slots));
    }
    #[test]
    fn alias_scan_budget_refusal_returns_original_before_registration() {
        let _serial = crate::test_host::serial_guard();
        let temporary = tempfile::tempdir().unwrap();
        let original = Arc::new(
            lillux::PinnedDirectory::open(temporary.path())
                .unwrap()
                .unwrap(),
        );
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(2, &|_, _| Ok(())) }.unwrap();
        let first = unsafe {
            registry.register_directory(Arc::clone(&original), &|_, work| {
                assert_eq!(work, 10);
                Ok(())
            })
        };
        assert!(first.is_ok());
        let refused = unsafe {
            registry.register_directory(Arc::clone(&original), &|_, work| {
                assert_eq!(work, 10);
                anyhow::bail!("original fixture work budget exhausted")
            })
        };
        let returned = match refused {
            Err((owner, _)) => owner,
            Ok(_) => panic!("alias scan must not bypass original budget refusal"),
        };
        assert!(Arc::ptr_eq(&returned, &original));
        assert_eq!(registry.inner.slots.len(), 1);
        assert!(!registry.descriptors_retired());
        drop(std::mem::take(&mut registry.inner.get_mut().slots));
    }

    #[test]
    fn projected_membership_quote_checks_capacity_without_pricing_a_false_scan() {
        let _serial = crate::test_host::serial_guard();
        let temporary = tempfile::tempdir().unwrap();
        let original = Arc::new(
            lillux::PinnedDirectory::open(temporary.path())
                .unwrap()
                .unwrap(),
        );
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(2, &|_, _| Ok(())) }.unwrap();
        // Three constant-time token checks at four steps each, plus the fixed
        // four-step quote; the projected slot count is checked, never priced.
        assert_eq!(
            registry.membership_work_bound(3, 1).unwrap(),
            OutsideWork {
                bytes: 0,
                steps: 16
            }
        );
        assert!(registry.membership_work_bound(0, 3).is_err());

        let binding = unsafe { registry.register_directory(original, &|_, _| Ok(())) }
            .unwrap_or_else(|_| panic!("fixture root must register"));
        assert!(registry.require_membership(&binding).is_ok());
        assert_eq!(
            registry.membership_work_bound(3, 1).unwrap(),
            OutsideWork {
                bytes: 0,
                steps: 16
            }
        );
        assert!(registry.membership_work_bound(0, 2).is_err());
        assert!(registry.membership_work_bound(u64::MAX, 0).is_err());
        assert_eq!(
            OriginalPhysicalRegistry::membership_work_quote(2, 3)
                .unwrap()
                .steps,
            OriginalPhysicalRegistry::membership_work_quote(200, 3)
                .unwrap()
                .steps
        );
        drop(std::mem::take(&mut registry.inner.get_mut().slots));
    }

    #[test]
    fn physical_owner_quotes_cover_their_bounded_charges() {
        let _serial = crate::test_host::serial_guard();
        let inventory = OriginalPhysicalRegistry::storage_quote(2).unwrap();
        assert_eq!(
            inventory.bytes,
            (2 * std::mem::size_of::<Slot>()
                + std::mem::size_of::<OriginalPhysicalRegistry>()
                + 2 * std::mem::size_of::<usize>()) as u64
        );
        assert_eq!(inventory.steps, 16);
        assert_eq!(
            OriginalPhysicalRegistry::registration_quote(4, 3).unwrap(),
            OutsideWork {
                bytes: 1536,
                steps: 36
            }
        );
        assert_eq!(
            OriginalPhysicalRegistry::descriptor_owner_quote(4).unwrap(),
            OutsideWork {
                bytes: 2048,
                steps: 16
            }
        );
        assert_eq!(
            OriginalPhysicalRegistry::operator_target_retirement_quote(2).unwrap(),
            OutsideWork {
                bytes: 1024,
                steps: 16
            }
        );
        assert_eq!(
            OutsideWork {
                bytes: 10,
                steps: 12
            }
            .checked_add(OutsideWork { bytes: 5, steps: 9 })
            .unwrap(),
            OutsideWork {
                bytes: 15,
                steps: 21
            }
        );
        assert_eq!(
            OutsideWork {
                bytes: 10,
                steps: 12
            }
            .checked_mul(3)
            .unwrap(),
            OutsideWork {
                bytes: 30,
                steps: 36
            }
        );
        assert!(
            OutsideWork {
                bytes: u64::MAX,
                steps: 0
            }
            .checked_add(OutsideWork { bytes: 1, steps: 0 })
            .is_err()
        );
        assert!(
            OutsideWork {
                bytes: 0,
                steps: u64::MAX
            }
            .checked_mul(2)
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn operator_observation_quote_covers_scans_and_fixed_results() {
        let _serial = crate::test_host::serial_guard();
        let quote = OriginalPhysicalRegistry::operator_observation_quote(4, 2).unwrap();
        assert_eq!(
            quote.bytes,
            2 * std::mem::size_of::<lillux::ExactExclusiveFileLockObservation>() as u64
        );
        assert_eq!(quote.steps, 24);
    }
}
