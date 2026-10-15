//! Live admission belongs to the actual opening State and its retained roots.
//! The host and paid factory are real owners; paths/roles do not mint authority.
use super::*;
use lillux::custody::Held;
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeBoundMode, OriginalNativeDestinationAuthority,
    OriginalNativeDomain, OriginalNativeFactory,
};

pub(super) struct LiveStateNativeHost {
    pub(super) factory: Arc<OriginalNativeFactory>,
    pub(super) authority: Arc<dyn OriginalNativeDestinationAuthority>,
    pub(super) lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
    pub(super) deadline: lillux::time::MonotonicDeadline,
}

pub(crate) struct LiveStateNativeOwner {
    pub(super) outside_join: Mutex<std::sync::Weak<super::destination_join::OriginalStateDestinationJoin>>,
    host: LiveStateNativeHost,
    runtime: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
    roots: [lillux::PinnedDirectory; 4],
    _recovery: RecoveryStore,
    _trust: Arc<TrustStore>,
}
pub(super) fn directory_wrapper_storage_bound(count: usize) -> anyhow::Result<u64> {
    let wrapper = std::mem::size_of::<lillux::PinnedDirectory>()
        .checked_add(2 * std::mem::size_of::<usize>())
        .and_then(|n| n.checked_add(2 * std::mem::align_of::<lillux::PinnedDirectory>()))
        .context("live State directory wrapper quote overflow")?;
    u64::try_from(wrapper.checked_mul(count).context("live State wrapper storage quote overflow")?)
        .map_err(Into::into)
}
pub fn quote_live_projection_physical(
    maximum_registry_slots: usize,
    files: usize,
    generations: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let maximum = files.checked_add(generations)
        .context("live projection physical census overflow")?;
    let additional_sources = files.checked_mul(2)
        .and_then(|roots| roots.checked_add(generations))
        .context("live projection projected root count overflow")?;
    anyhow::ensure!(maximum_registry_slots > 0 && additional_sources <= maximum_registry_slots,
        "live projection roots exceed physical registry quote");
    let queries = u64::try_from(maximum)?.checked_mul(4)
        .context("live projection membership count overflow")?;
    let membership = lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::
        membership_work_quote(maximum_registry_slots, queries)?;
    quote_live_projection_physical_with_membership(files, generations, membership)
}

pub fn quote_live_projection_validation(
    maximum_slots: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(maximum_slots > 0, "empty live projection validation bound");
    let slots = u64::try_from(maximum_slots)?;
    let queries = slots.checked_mul(2)
        .context("live projection validation membership count overflow")?;
    let membership = lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::
        membership_work_quote(maximum_slots, queries)?;
    membership.checked_add(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: 0,
        // Bound each slot's retained-owner, source-role, and selected-root
        // checks in addition to the at-most-two opaque membership tokens.
        steps: slots.checked_mul(24)
            .context("live projection validation work overflow")?,
    })
}

/// Quote consuming the exact retained projection-root registrations after the
/// matching physical registry is retired. The caller supplies the bounded
/// owner-frame slot count and selected roots observed in that same frame.
pub fn quote_live_projection_retired_sources(
    slots: usize,
    selected_roots: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(selected_roots <= slots.checked_mul(2)
        .context("retired projection source bound overflow")?,
        "retired projection roots exceed their owner slots");
    retired_root_receipt_work(slots, selected_roots)
}

fn quote_retired_root_receipts(
    slots: usize,
    selected_roots: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(selected_roots <= slots,
        "retired root receipts exceed their selected registration slots");
    retired_root_receipt_work(slots, selected_roots)
}

/// Per-slot validation plus per-root receipt work; each caller bounds roots
/// by its own owner shape (a projection slot holds up to two roots).
fn retired_root_receipt_work(
    slots: usize,
    selected_roots: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let slot_steps = u64::try_from(slots)?
        .checked_mul(4)
        .context("retired root slot validation quote overflow")?;
    let root_steps = u64::try_from(selected_roots)?
        .checked_mul(12)
        .context("retired root receipt quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: 0,
        steps: slot_steps.checked_add(root_steps)
            .and_then(|steps| steps.checked_add(4))
            .context("retired root source quote overflow")?,
    })
}

pub(super) fn quote_state_root_receipt_owner(
    directory_roots: usize,
    namespace_roots: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(namespace_roots <= 1,
        "State root receipt owner exceeds its single namespace slot");
    let registrations = directory_roots.checked_add(namespace_roots)
        .context("State root receipt registration count overflow")?;
    anyhow::ensure!(registrations <= 8,
        "State root receipt owner exceeds the selected fixed source bound");
    quote_retired_root_receipts(registrations, registrations)
}

pub(super) fn bind_root_receipt_registry(
    identity: &mut Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
) -> anyhow::Result<()> {
    let actual = registry.identity()?;
    if let Some(expected) = identity.as_ref() {
        expected.require_same_registry(&actual)?;
    } else {
        *identity = Some(actual);
    }
    Ok(())
}

pub(super) fn require_original_root_receipt_registry(
    identity: Option<&lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
) -> anyhow::Result<()> {
    let actual = registry.identity()?;
    identity.context("original root receipt registry identity absent")?
        .require_same_registry(&actual)
}

fn require_retired_root_receipt_registry(
    identity: Option<&lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
) -> anyhow::Result<()> {
    registry.require_same_registry(identity.context("original root receipt registry identity absent")?)
}

/// Worst-case post-disposal receipt work for both normal State root owners and
/// the shared parent owner, including each owner's separate RecoveryStore set.
pub fn quote_live_state_finalization_root_receipts()
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let normal_roots = quote_state_root_receipt_owner(5, 1)?;
    let recovery_roots = quote_state_root_receipt_owner(2, 0)?;
    normal_roots.checked_add(recovery_roots)?
        .checked_add(normal_roots)?
        .checked_add(recovery_roots)?
        .checked_add(crate::operational::quote_operational_retired_shared_roots()?)?
        .checked_add(crate::operational::quote_operational_marker_receipt())
}

/// Quote the original State parent frame transferred into physical custody.
pub fn quote_live_state_physical_parent() -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: LiveStatePhysicalParent::storage_bound()?,
        steps: 8,
    })
}

/// Quote the fixed owner bookkeeping for one registered State root handoff.
pub fn quote_live_state_root_registration_handoff() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 2 }
}

/// Quote the complete State parent-root census, including each added source.
/// The owner has five directory roots and one optional namespace root.
pub fn quote_live_state_parent_root_membership(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    additional_sources: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(additional_sources <= 6,
        "State parent quote exceeds five directories and its namespace");
    registry.membership_work_bound(12, additional_sources)
}

pub fn quote_live_state_registered_root_validation(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    registered_roots: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(registered_roots <= 6,
        "State registered-root validation exceeds its six owner slots");
    registry.membership_work_bound(u64::try_from(registered_roots)?, 0)?.checked_add(
        lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 8 },
    )
}

/// Quote the fixed retained RecoveryStore directory set and its role checks.
pub fn quote_live_state_recovery_registration(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    additional_sources: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(additional_sources <= 4,
        "State recovery quote exceeds its four original directories");
    registry.membership_work_bound(4, additional_sources)?.checked_add(
        lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 8 },
    )
}

/// Quote all retained State root and recovery membership checks before their
/// host aliases are retired.
pub fn quote_live_state_host_alias_retirement(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    registry.membership_work_bound(8, 0)?.checked_add(
        lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 8 },
    )
}

/// Quote retirement of the State parent's RecoveryStore alias once its two
/// roots are registered: two membership checks plus the owner transition.
pub fn quote_live_state_parent_recovery_owner_retirement(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    registry.membership_work_bound(2, 0)?.checked_add(
        lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 4 },
    )
}

/// Quote preparation of the actual State destination join.
pub fn quote_live_state_destination_join() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 2 }
}

/// Quote the released-startup State physical frame before it is retained.
pub fn quote_released_live_state_physical_custody()
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<ReleasedLiveStatePhysicalCustody>())?,
        steps: 32,
    })
}

pub fn quote_live_projection_retry(work_bound: u64)
    -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: work_bound }
}

pub fn quote_startup_closed_schema_sources(
    pending_references: usize,
    incoming_references: usize,
    retained_definitions: usize,
    retained_terminals: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let count = pending_references.checked_add(incoming_references)
        .context("startup schema source count overflow")?;
    let definitions = retained_definitions.checked_add(count)
        .context("startup schema definition quote overflow")?;
    let terminals = retained_terminals.checked_add(count)
        .context("startup schema terminal quote overflow")?;
    let bytes = count.checked_mul(std::mem::size_of::<Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>>())
        .and_then(|bytes| bytes.checked_add(definitions.checked_mul(std::mem::size_of::<crate::sqlite_schema::SchemaSqlDefinition>())?))
        .and_then(|bytes| bytes.checked_add(terminals.checked_mul(std::mem::size_of::<lillux_sqlite::OriginalNativeConnectionTerminal>())?))
        .and_then(|bytes| bytes.checked_add(3 * std::mem::size_of::<Vec<()>>()))
        .context("startup closed schema storage quote overflow")?;
    let count = u64::try_from(count)?;
    let steps = count.checked_mul(count).and_then(|work| work.checked_add(4))
        .context("startup closed schema work quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(bytes)?, steps,
    })
}

pub fn quote_startup_released_schema_source(
    retained_definitions: usize,
    retained_terminals: usize,
    retained_refusals: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let definitions = retained_definitions.checked_add(1)
        .context("startup released schema definition quote overflow")?;
    let terminals = retained_terminals.checked_add(1)
        .context("startup released schema terminal quote overflow")?;
    let refusals = retained_refusals.checked_add(1)
        .context("startup released schema refusal quote overflow")?;
    let bytes = definitions.checked_mul(std::mem::size_of::<crate::sqlite_schema::SchemaSqlDefinition>())
        .and_then(|bytes| bytes.checked_add(terminals.checked_mul(std::mem::size_of::<lillux_sqlite::OriginalNativeConnectionTerminal>())?))
        .and_then(|bytes| bytes.checked_add(refusals.checked_mul(std::mem::size_of::<anyhow::Error>())?))
        .and_then(|bytes| bytes.checked_add(3 * std::mem::size_of::<Vec<()>>()))
        .context("startup released schema storage quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(bytes)?, steps: 4,
    })
}

/// Maximum physical roots from live projection files and generation leases.
/// Each file contributes at most its runtime and source directories; each
/// generation lease contributes one metadata directory. The two collections
/// are independently bounded by the factory's native-ticket ceiling.
pub fn maximum_live_projection_physical_sources(connection_tickets: u32) -> anyhow::Result<usize> {
    let tickets = usize::try_from(connection_tickets)?;
    anyhow::ensure!(tickets > 0, "empty live projection ticket bound");
    tickets.checked_mul(3)
        .context("live projection physical source bound overflow")
}

fn quote_live_projection_physical_at(
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    files: usize,
    generations: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let maximum = files.checked_add(generations)
        .context("live projection physical census overflow")?;
    let queries = u64::try_from(maximum)?.checked_mul(4)
        .context("live projection membership count overflow")?;
    let additional_sources = files.checked_mul(2)
        .and_then(|roots| roots.checked_add(generations))
        .context("live projection projected root count overflow")?;
    let membership = registry.membership_work_bound(queries, additional_sources)?;
    quote_live_projection_physical_with_membership(files, generations, membership)
}

fn quote_live_projection_physical_with_membership(
    files: usize,
    generations: usize,
    membership: lillux_sqlite::original_physical_custody::OutsideWork,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let maximum = files.checked_add(generations)
        .context("live projection physical census overflow")?;
    let bytes = u64::try_from(maximum.checked_mul(std::mem::size_of::<LiveProjectionPhysicalSlot>())
        .context("live projection physical storage overflow")?)?
        .checked_add(directory_wrapper_storage_bound(maximum.checked_mul(2)
            .context("live projection root wrapper count overflow")?)?)
        .context("live projection storage quote overflow")?;
    let work = u64::try_from(maximum)?.checked_add(1)
        .and_then(|n| n.checked_mul(n)).and_then(|n| n.checked_mul(8))
        .and_then(|n| n.checked_add(membership.steps))
        .context("live projection physical work overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes, steps: work })
}
pub(super) struct LiveProjectionPhysicalCustody {
    process: u32,
    prepared: bool,
    retired_sources_consumed: bool,
    registry_identity: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    maximum: usize,
    work_bound: u64,
    slots: Vec<LiveProjectionPhysicalSlot>,
}
struct LiveProjectionPhysicalSlot {
    roots: [Option<lillux::PinnedDirectory>; 2],
    shared: [Option<Arc<lillux::PinnedDirectory>>; 2],
    lease: Option<Arc<crate::projection::OriginalGenerationLease>>,
    selected: [bool; 2],
    bindings: [Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>; 2],
}
impl LiveProjectionPhysicalSlot {
    fn issued(directory: Option<lillux::PinnedDirectory>, lease: Option<Arc<crate::projection::OriginalGenerationLease>>) -> Self {
        let selected = [directory.is_some(), lease.is_some()];
        Self { roots: [directory, None], shared: [None, None], lease, selected,
            bindings: std::array::from_fn(|_| None) }
    }
}
impl LiveProjectionPhysicalCustody {
    pub(super) fn empty() -> Self {
        Self { process: std::process::id(), prepared: false, retired_sources_consumed: false,
            registry_identity: None, maximum: 0, work_bound: 0, slots: Vec::new() }
    }
    /// An empty frame whose retired sources are already consumed.
    #[cfg(test)]
    pub(super) fn exhausted_for_test() -> Self {
        let mut exhausted = Self::empty();
        exhausted.retired_sources_consumed = true;
        exhausted
    }
    fn require_registered(
        &self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign live projection physical custody");
        anyhow::ensure!(!self.retired_sources_consumed,
            "retired live projection source frame cannot be registered again");
        let registry_identity = registry.identity()?;
        self.registry_identity.as_ref()
            .context("live projection source frame lacks its original registry identity")?
            .require_same_registry(&registry_identity)?;
        for slot in &self.slots {
            anyhow::ensure!(slot.lease.is_none() && slot.roots.iter().all(Option::is_none)
                && slot.shared.iter().all(Option::is_none),
                "live projection original roots remain unregistered");
            for index in 0..2 {
                if !slot.selected[index] { continue; }
                let binding = slot.bindings[index].as_ref()
                    .context("live projection physical membership absent")?;
                registry.require_membership(binding)?;
                anyhow::ensure!(matches!(binding.observed_source_identity()?,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                    "live projection root role changed");
            }
        }
        Ok(())
    }
    /// # Safety
    /// Retain the whole original frame through all partial outcomes. Native
    /// consumers and lease bodies are genuinely retired before these transfers.
    pub(super) unsafe fn register(&mut self,
        files: &mut Vec<crate::projection::OutsideOriginalProjectionCustody>,
        generations: Option<&Mutex<Vec<Arc<crate::projection::OriginalGenerationLease>>>>,
        factory: &Arc<OriginalNativeFactory>,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign live projection physical custody");
        anyhow::ensure!(!self.retired_sources_consumed,
            "retired live projection source frame cannot be registered again");
        let registry_identity = registry.identity()?;
        if let Some(expected) = self.registry_identity.as_ref() {
            expected.require_same_registry(&registry_identity)?;
        } else {
            self.registry_identity = Some(registry_identity);
        }
        factory.require_original_process()?;
        let ticket_bound = usize::try_from(factory.maximum_resource_tickets()?)?;
        anyhow::ensure!(ticket_bound > 0, "live projection has no original native tickets");
        anyhow::ensure!(files.len() <= ticket_bound,
            "live projection file custody exceeds original native-ticket bound");
        let generation_count = match generations {
            Some(generations) => generations.try_lock()
                .map_err(|_| anyhow::anyhow!("live generation physical census busy or poisoned"))?.len(),
            None => 0,
        };
        anyhow::ensure!(generation_count <= ticket_bound,
            "live projection generation custody exceeds original native-ticket bound");
        let source_bound = maximum_live_projection_physical_sources(u32::try_from(ticket_bound)?)?;
        if self.prepared {
            anyhow::ensure!(self.slots.len() <= ticket_bound.checked_mul(2)
                .context("live projection slot bound overflow")?,
                "retained live projection slots exceed original native-ticket bound");
            let validation = quote_live_projection_validation(self.maximum)?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(validation.bytes, validation.steps))?;
            anyhow::ensure!(self.process == std::process::id(),
                "foreign live projection after retry-census debit");
        }
        let no_pending_slots = self.slots.iter().all(|slot| slot.lease.is_none()
            && slot.roots.iter().all(Option::is_none)
            && slot.shared.iter().all(Option::is_none)
            && (0..2).all(|index| !slot.selected[index] || slot.bindings[index].is_some()));
        if self.prepared && files.is_empty() && generation_count == 0 && no_pending_slots {
            return self.require_registered(registry);
        }
        if !self.prepared && files.is_empty() && generation_count == 0 {
            return Ok(());
        }
        if !self.prepared {
            let maximum = files.len().checked_add(generation_count)
                .context("live projection physical census overflow")?;
            anyhow::ensure!(maximum <= ticket_bound.checked_mul(2)
                .context("live projection slot bound overflow")?,
                "live projection slots exceed original native-ticket bound");
            let projected_sources = files.len().checked_mul(2)
                .and_then(|roots| roots.checked_add(generation_count))
                .context("live projection source census overflow")?;
            anyhow::ensure!(projected_sources <= source_bound,
                "live projection roots exceed original native-ticket bound");
            let quote = quote_live_projection_physical_at(registry, files.len(), generation_count)?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
            anyhow::ensure!(self.process == std::process::id(), "foreign projection after original prepay");
            self.slots.try_reserve_exact(maximum)?;
            self.maximum = maximum;
            self.work_bound = quote.steps;
            self.prepared = true;
        } else {
            // Retry reuses original storage but repeats traversal. Charge that
            // work irreversibly against the SAME payer, never a fresh grant.
            let quote = quote_live_projection_retry(self.work_bound);
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
        }
        anyhow::ensure!(self.process == std::process::id(), "foreign projection after original work debit");
        if let Some(generations) = generations {
            let mut generations = generations.try_lock()
                .map_err(|_| anyhow::anyhow!("live generation source transfer busy or poisoned"))?;
            anyhow::ensure!(self.slots.len().checked_add(generations.len())
                .is_some_and(|count| count <= self.maximum), "live generation source exceeds original quote");
            // Only move/compare Rust owners beneath this retention gate.
            while let Some(lease) = generations.pop() {
                if self.slots.iter().any(|slot| slot.lease.as_ref().is_some_and(|original| Arc::ptr_eq(original, &lease))) {
                    drop(lease); // canonical SAME lease remains retained above
                } else {
                    self.slots.push(LiveProjectionPhysicalSlot::issued(None, Some(lease)));
                }
            }
        }
        while !files.is_empty() {
            anyhow::ensure!(self.slots.len() < self.maximum, "live projection source exceeds original quote");
            let actual = files.pop().expect("actual projection source checked");
            match actual.try_into_original_filesystem() {
                Ok(Some((directory, lease))) => {
                    let lease = match lease {
                        Some(lease) if self.slots.iter().any(|slot| slot.lease.as_ref()
                            .is_some_and(|original| Arc::ptr_eq(original, &lease))) => {
                            drop(lease); None
                        }
                        lease => lease,
                    };
                    self.slots.push(LiveProjectionPhysicalSlot::issued(Some(directory), lease));
                }
                Ok(None) => {}
                Err(actual) => { files.push(actual); anyhow::bail!("live projection source is not original native custody"); }
            }
        }
        for slot in &mut self.slots {
            if let Some(lease) = slot.lease.take() {
                match lease.try_into_metadata_root() {
                    Ok(root) => slot.roots[1] = Some(root),
                    Err(lease) => { slot.lease = Some(lease); anyhow::bail!("live generation metadata still shared or active"); }
                }
            }
            for index in 0..2 {
                if !slot.selected[index] { continue; }
                if let Some(binding) = &slot.bindings[index] { registry.require_membership(binding)?; continue; }
                if slot.shared[index].is_none() {
                    let root = slot.roots[index].take().context("actual selected projection root absent")?;
                    slot.shared[index] = Some(Arc::new(root));
                }
                let quote = quote_live_state_root_registration_handoff();
                registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
                anyhow::ensure!(self.process == std::process::id(), "foreign projection registration after prepay");
                let actual = slot.shared[index].as_ref().expect("cached actual projection root").clone();
                match unsafe { registry.register_directory(actual, prepay) } {
                    Ok(binding) => { slot.bindings[index] = Some(binding); drop(slot.shared[index].take()); }
                    Err((alias, error)) => { drop(alias); return Err(error); }
                }
            }
        }
        let actual_sources = self.slots.iter().try_fold(0usize, |total, slot| {
            total.checked_add(slot.selected.iter().filter(|selected| **selected).count())
                .context("live projection retained source count overflow")
        })?;
        anyhow::ensure!(actual_sources <= source_bound,
            "live projection retained roots exceed original native-ticket bound");
        self.require_registered(registry)
    }

    /// Generation leases convert into metadata roots during registration; no
    /// slot may retain a lease (and its native factory) past that join.
    pub(super) fn require_leases_retired(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign live projection lease census");
        anyhow::ensure!(self.slots.iter().all(|slot| slot.lease.is_none()),
            "live projection slot retains a generation lease");
        Ok(())
    }

    /// Consume this owner frame only after every selected root has a receipt
    /// from the exact retired registry. A refusal leaves the same bindings and
    /// slot state in place for the caller's bounded retry.
    pub(super) fn consume_retired_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign retired live projection custody");
        let identity = self.registry_identity.as_ref()
            .context("live projection source frame lacks its original registry identity")?;
        registry.require_same_registry(identity)?;
        if self.retired_sources_consumed {
            anyhow::ensure!(self.slots.is_empty(),
                "consumed live projection frame regained source slots");
            return Ok(());
        }
        anyhow::ensure!(self.slots.len() <= self.maximum
            && (self.prepared || self.slots.is_empty()),
            "live projection source slots exceed the retained owner bound");
        let selected_roots = self.slots.iter().try_fold(0usize, |total, slot| {
            total.checked_add(slot.selected.iter().filter(|selected| **selected).count())
                .context("retired live projection source count overflow")
        })?;
        let quote = quote_live_projection_retired_sources(self.slots.len(), selected_roots)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        anyhow::ensure!(self.process == std::process::id(),
            "foreign live projection custody after retirement debit");

        // Validate the whole source vector before releasing any registration.
        for slot in &self.slots {
            anyhow::ensure!(slot.lease.is_none() && slot.roots.iter().all(Option::is_none)
                && slot.shared.iter().all(Option::is_none),
                "retired live projection frame retains a source or generation lease");
            for index in 0..2 {
                let Some(binding) = slot.bindings[index].as_ref() else {
                    anyhow::ensure!(!slot.selected[index],
                        "retired live projection root lacks its registration");
                    continue;
                };
                anyhow::ensure!(slot.selected[index],
                    "unselected live projection slot has a registration");
                anyhow::ensure!(matches!(binding.observed_source_identity()?,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                    "retired live projection registration changed role");
                drop(binding.receipt_in(registry)?);
            }
        }

        self.slots.clear();
        self.prepared = false;
        self.maximum = 0;
        self.work_bound = 0;
        self.retired_sources_consumed = true;
        Ok(())
    }
}

const STATE_METADATA_PIN_LEAF_STEPS: u64 = 8;
const STATE_METADATA_PIN_RECEIPT_CHECK_STEPS: u64 = 4;
const STATE_NATIVE_SOURCE_RETIREMENT_STEPS: u64 = 16;

/// Candidate closed connections come from three ticket-bounded State vectors:
/// generated terminals, durable proofs and closed projection terminals.
fn maximum_state_metadata_pin_candidates(resource_tickets: u32) -> anyhow::Result<usize> {
    usize::try_from(resource_tickets)?.checked_mul(3)
        .context("State metadata-pin candidate bound overflow")
}

/// Strong native owners ended by the source stage: the three candidate
/// vectors plus one projection refusal per ticket.
fn maximum_state_native_source_owners(resource_tickets: u32) -> anyhow::Result<usize> {
    usize::try_from(resource_tickets)?.checked_mul(4)
        .context("State native source owner bound overflow")
}

/// Candidate vector storage and its pairwise exact-allocation de-duplication.
fn quote_state_metadata_pin_candidates(candidates: usize)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let bytes = candidates
        .checked_mul(std::mem::size_of::<Arc<lillux_sqlite::ClosedOriginalNativeConnection>>())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<()>>()))
        .context("State metadata-pin candidate storage quote overflow")?;
    let count = u64::try_from(candidates)?;
    let steps = count.checked_mul(count).and_then(|steps| steps.checked_add(4))
        .context("State metadata-pin candidate work quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: u64::try_from(bytes)?, steps })
}

/// Cached leaf records (receipts inline), per-leaf producer disposition and
/// the closing receipt census. Lillux and the late issuer charge the native
/// close and the retirement inputs separately.
fn quote_state_metadata_pin_leaves(leaves: usize)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let bytes = leaves.checked_mul(std::mem::size_of::<StateMetadataPinLeaf>())
        .context("State metadata-pin record storage quote overflow")?;
    let count = u64::try_from(leaves)?;
    let steps = count.checked_mul(STATE_METADATA_PIN_LEAF_STEPS)
        .and_then(|steps| steps.checked_add(count.checked_mul(2 * STATE_METADATA_PIN_RECEIPT_CHECK_STEPS)?))
        .and_then(|steps| steps.checked_add(4))
        .context("State metadata-pin leaf work quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: u64::try_from(bytes)?, steps })
}

/// Cached-receipt verification only; it reads no factory or terminal.
fn quote_state_metadata_pin_receipt_checks(leaves: usize)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let steps = u64::try_from(leaves)?.checked_mul(2 * STATE_METADATA_PIN_RECEIPT_CHECK_STEPS)
        .and_then(|steps| steps.checked_add(4))
        .context("State metadata-pin receipt check quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps })
}

/// Ending each strong native owner and the fixed frame transition.
fn quote_state_native_source_retirement(owners: usize)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let steps = u64::try_from(owners)?.checked_add(STATE_NATIVE_SOURCE_RETIREMENT_STEPS)
        .context("State native source retirement quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps })
}

/// Worst-case State-local native-source stage work in one finalizer attempt:
/// one leaf per resource ticket, every candidate vector full, every strong
/// owner ended, plus two cached receipt checks: the projection stage's and the
/// owner-frame re-check before the receipt is consumed. A retried source stage
/// charges only one check, which the leaf census term dominates. Native close,
/// issued inputs and the destination-join census are charged and quoted separately.
pub(super) fn quote_maximum_state_metadata_pin_attempt(resource_tickets: u32)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let leaves = usize::try_from(resource_tickets)?;
    let receipt_check = quote_state_metadata_pin_receipt_checks(leaves)?;
    quote_state_metadata_pin_candidates(maximum_state_metadata_pin_candidates(resource_tickets)?)?
        .checked_add(quote_state_metadata_pin_leaves(leaves)?)?
        .checked_add(quote_state_native_source_retirement(
            maximum_state_native_source_owners(resource_tickets)?)?)?
        .checked_add(receipt_check)?
        .checked_add(receipt_check)
}

/// Fund and reserve the candidate vector for one State census pass.
pub(super) fn state_metadata_pin_candidates(
    factory: &Arc<OriginalNativeFactory>,
    candidates: usize,
    prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<Arc<lillux_sqlite::ClosedOriginalNativeConnection>>> {
    anyhow::ensure!(candidates <= maximum_state_metadata_pin_candidates(factory.maximum_resource_tickets()?)?,
        "State metadata-pin candidates exceed the original resource ticket bound");
    let quote = quote_state_metadata_pin_candidates(candidates)?;
    lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
        prepay(quote.bytes, quote.steps))?;
    let mut census = Vec::new();
    census.try_reserve_exact(candidates)?;
    Ok(census)
}

/// Fund ending `owners` strong native owners within the original ticket bound.
pub(super) fn prepay_state_native_source_retirement(
    factory: &Arc<OriginalNativeFactory>,
    owners: usize,
    prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    anyhow::ensure!(owners <= maximum_state_native_source_owners(factory.maximum_resource_tickets()?)?,
        "State native source owners exceed the original resource ticket bound");
    let quote = quote_state_native_source_retirement(owners)?;
    lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
        prepay(quote.bytes, quote.steps))
}

/// Refuse ending retained refusals while any layer of a chain may carry an
/// owner; such owners belong in typed custody, never in a diagnostic that the
/// native-source stage ends. Every error type of the native, State and
/// filesystem owner crates is classified: only the diagnostic ones, which
/// hold copied facts alone (region refusal codes, role-mutation reports,
/// schema/event conflicts, callback panic payloads), are absent here. An enum
/// with diagnostic-only variants is matched by its owner-bearing variants.
pub(super) fn require_refusals_without_native_owners<'a>(
    refusals: impl IntoIterator<Item = &'a anyhow::Error>,
) -> anyhow::Result<()> {
    fn owner_bearing(error: &(dyn std::error::Error + 'static)) -> bool {
        // State.
        error.is::<GeneratedProjectionPublicationFailure>()
            || error.is::<OriginalProjectionLeaseSetupFailure>()
            || error.is::<OriginalProjectionSchemaAdmissionFailure>()
            || error.is::<LiveStateDatabaseOpenFailure>()
            || error.is::<LiveStateStartupRecoveryFailure>()
            || error.is::<super::LiveStateCloseFailure>()
            || error.is::<super::OriginalStateRetirementFailure>()
            || error.is::<crate::projection::OriginalProjectionFailure>()
            || error.is::<crate::sqlite_schema::NativeSchemaSqlReferenceFailure>()
            || error.is::<crate::sqlite_schema::NativeSchemaSqlReferenceOutsideFailure>()
            || error.is::<crate::operational::OriginalOperationalFailure>()
            || error.is::<crate::operational::LiveOperationalOpenFailure>()
            || error.is::<crate::operational::LiveOperationalStartupRecoveryFailure>()
            // Native kernel.
            || error.is::<lillux_sqlite::OriginalNativeProjectionLeaseAdmissionFailure>()
            || error.is::<lillux_sqlite::OriginalProjectionLeaseFailure>()
            || error.is::<lillux_sqlite::OriginalNativeConnectionAdmissionFailure>()
            || error.is::<lillux_sqlite::OriginalDestinationCustodyFailure>()
            || error.is::<lillux_sqlite::OriginalNativeFactoryFailure>()
            || error.is::<lillux_sqlite::LiveOperationalInitializationFailure>()
            || error.is::<lillux_sqlite::OriginalNativeDurabilityFailure>()
            || error.is::<lillux_sqlite::OriginalSchedulerRetirementFailure>()
            || error.is::<lillux_sqlite::OriginalNativeCloseFailure>()
            || error.is::<lillux_sqlite::OriginalNativeOpenFailure>()
            || error.is::<lillux_sqlite::OriginalNativeOutsideCustodyFailure>()
            || error.is::<lillux_sqlite::OriginalMetadataPinRetirementFailure>()
            || error.is::<lillux_sqlite::OriginalOutsideFinalizerFileRetirementFailure>()
            || error.is::<lillux_sqlite::SqliteInspectionProductionFailure>()
            || error.is::<lillux_sqlite::SqliteDestinationRetirementFailure>()
            || error.is::<lillux_sqlite::OriginalWitnessRetirementFailure>()
            || error.is::<lillux_sqlite::ReadonlySqliteOpenFailure>()
            || error.is::<lillux_sqlite::ReadonlySqliteCloseFailure>()
            || error.is::<lillux_sqlite::SqliteProjectionRetirementFailure>()
            || error.is::<lillux_sqlite::BoundSqliteOpenFailure>()
            // Only a held role resolution reports a retained filesystem role;
            // refused and budget-exhausted resolutions are copied diagnostics.
            || matches!(error.downcast_ref::<lillux_sqlite::SqliteRoleResolutionError>(),
                Some(lillux_sqlite::SqliteRoleResolutionError::Held(_)))
            // Filesystem custody.
            || error.is::<lillux::secure_fs::ExclusiveFileCreationFailure>()
            || error.is::<lillux::secure_fs::PlannedQuarantineFailure>()
            || error.is::<lillux::secure_fs::EmptyChildRemovalFailure>()
            || error.is::<lillux::secure_fs::ChildDirectoryCreationFailure>()
            || error.is::<lillux::secure_fs::AtomicRegularCreationFailure>()
            || error.is::<lillux::time::RetainedClockCloseFailure>()
    }
    for error in refusals {
        anyhow::ensure!(!error.chain().any(owner_bearing),
            "State refusal still carries an owner-bearing failure outside typed custody");
    }
    Ok(())
}

/// Refuse while any census connection still owns its original destination
/// inputs (Bound root alias and authority, or a transient authority).
pub(super) fn require_census_without_destination_inputs(
    candidates: &[Arc<lillux_sqlite::ClosedOriginalNativeConnection>],
) -> anyhow::Result<()> {
    for closed in candidates {
        anyhow::ensure!(!closed.retains_destination_inputs()?,
            "State closed connection still retains its original destination inputs");
    }
    Ok(())
}

/// A released State acquisition may be ended only with no destination
/// inputs, no inspection owner (State has no copy-lease receiver) and a
/// refusal that carries no owner.
pub(super) fn require_released_without_owner_inputs(
    released: &lillux_sqlite::ReleasedOriginalNativeAcquisition,
) -> anyhow::Result<()> {
    anyhow::ensure!(!released.retains_destination_inputs()?,
        "State released acquisition still retains its original destination inputs");
    anyhow::ensure!(released.inspection_released().is_none() && released.inspection_closed().is_none(),
        "State released acquisition retains an inspection owner");
    require_refusals_without_native_owners(std::iter::once(released.original_error()))
}

/// Census guard before a State frame enters physical custody: no terminal
/// of the context or of `projection_native` still owns destination inputs.
pub(super) fn require_no_destination_inputs(
    context: Option<&OriginalProjectionBuildContext>,
    projection_native: &[lillux_sqlite::OriginalNativeConnectionTerminal],
) -> anyhow::Result<()> {
    if let Some(context) = context {
        for closed in context.terminals.iter().cloned().chain(context.durable.iter().map(|proof| proof.closed())) {
            anyhow::ensure!(!closed.retains_destination_inputs()?,
                "State terminal still retains its original destination inputs");
        }
    }
    for native in projection_native {
        let retained = match native {
            lillux_sqlite::OriginalNativeConnectionTerminal::Closed(closed) => closed.retains_destination_inputs()?,
            lillux_sqlite::OriginalNativeConnectionTerminal::Released(released) => released.retains_destination_inputs()?,
        };
        anyhow::ensure!(!retained, "State projection terminal still retains its original destination inputs");
    }
    Ok(())
}

/// One destination input taken from a State terminal. It is cached here
/// before any fallible step and converted in place toward its State leaf.
pub(super) struct StateTakenDestination {
    evidence: lillux_sqlite::OriginalNativeOutsideLeafEvidence,
    stage: StateTakenStage,
}
enum StateTakenStage {
    Raw(lillux_sqlite::OriginalNativeDestinationInputs),
    Parts(lillux_sqlite::OriginalNativeOutsideBoundParts),
    Leaf {
        leaf: super::destination_join::StateDestinationLeaf,
        root_alias: Option<Arc<lillux::PinnedDirectory>>,
    },
}

/// Storage of `terminals` taken inputs, plus each take and its identity scan
/// over `issued` records.
pub fn quote_state_destination_input_transfer(terminals: usize, issued: usize)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let bytes = terminals.checked_mul(std::mem::size_of::<StateTakenDestination>())
        .context("State destination input storage quote overflow")?;
    let per_take = lillux_sqlite::ORIGINAL_DESTINATION_INPUT_TAKE_STEPS
        .checked_add(u64::try_from(issued)?)
        .context("State destination input work quote overflow")?;
    let steps = u64::try_from(terminals)?.checked_mul(per_take)
        .and_then(|steps| steps.checked_add(8))
        .context("State destination input work quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: u64::try_from(bytes)?, steps })
}

/// Worst case of one pass: every census candidate (3T) plus the inputs a
/// refused earlier pass kept taken (at most the T issued destinations), each
/// checked against a full issuance record.
pub fn quote_maximum_state_destination_input_transfer(resource_tickets: u32)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let issued = super::destination_join::maximum_state_destination_leaves(resource_tickets)?;
    let taken = maximum_state_metadata_pin_candidates(resource_tickets)?
        .checked_add(issued)
        .context("State destination input census bound overflow")?;
    quote_state_destination_input_transfer(taken, issued)
}

/// Move every destination input still held by a State terminal into the State
/// destination join as a typed leaf. Root aliases end once the leaf (which
/// holds the same root) is retained. A refusal keeps every taken entry in
/// `slot` for the next pass; an unmatched authority is never dropped.
pub(super) fn advance_destination_input_sources(
    context: Option<&OriginalProjectionBuildContext>,
    projection_native: &[lillux_sqlite::OriginalNativeConnectionTerminal],
    slot: &mut Vec<StateTakenDestination>,
    join: &Arc<super::destination_join::OriginalStateDestinationJoin>,
    factory: &Arc<OriginalNativeFactory>,
    terminal: &lillux_sqlite::OriginalNativeTerminal,
    prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    factory.require_original_process()?;
    terminal.require_original(factory)?;
    join.require_original(factory, terminal)?;
    let (terminals, durable): (&[Arc<lillux_sqlite::ClosedOriginalNativeConnection>],
        &[Arc<lillux_sqlite::DurableOriginalNativeBound>]) = match context {
        Some(context) => (&context.terminals, &context.durable),
        None => (&[], &[]),
    };
    let mut retained = !slot.is_empty();
    for closed in terminals.iter().cloned().chain(durable.iter().map(|proof| proof.closed())) {
        retained = retained || closed.retains_destination_inputs()?;
    }
    for native in projection_native {
        retained = retained || match native {
            lillux_sqlite::OriginalNativeConnectionTerminal::Closed(closed) => closed.retains_destination_inputs()?,
            lillux_sqlite::OriginalNativeConnectionTerminal::Released(released) => released.retains_destination_inputs()?,
        };
    }
    if !retained { return Ok(()); }
    let candidates = terminals.len().checked_add(durable.len())
        .and_then(|count| count.checked_add(projection_native.len()))
        .context("State destination input census overflow")?;
    let issued = match context { Some(context) => context.issued_destination_count()?, None => 0 };
    let quote = quote_state_destination_input_transfer(
        candidates.checked_add(slot.len()).context("State destination input census overflow")?, issued)?;
    lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
        prepay(quote.bytes, quote.steps))?;
    slot.try_reserve_exact(candidates)?;
    factory.require_original_process()?;
    terminal.require_original(factory)?;
    // Each take is pushed at once into reserved capacity, so a later
    // refusal cannot strand an input outside this slot.
    for closed in terminals.iter().cloned().chain(durable.iter().map(|proof| proof.closed())) {
        if let Some((evidence, inputs)) = closed.take_destination_inputs(factory, terminal)? {
            slot.push(StateTakenDestination { evidence, stage: StateTakenStage::Raw(inputs) });
        }
    }
    for native in projection_native {
        let taken = match native {
            lillux_sqlite::OriginalNativeConnectionTerminal::Closed(closed) =>
                closed.take_destination_inputs(factory, terminal)?,
            lillux_sqlite::OriginalNativeConnectionTerminal::Released(released) =>
                released.take_destination_inputs(factory, terminal)?,
        };
        if let Some((evidence, inputs)) = taken {
            slot.push(StateTakenDestination { evidence, stage: StateTakenStage::Raw(inputs) });
        }
    }
    while let Some(StateTakenDestination { evidence, stage }) = slot.pop() {
        let (stage, refusal) = advance_taken_destination(stage, context);
        if let Some(error) = refusal {
            slot.push(StateTakenDestination { evidence, stage });
            return Err(error);
        }
        let StateTakenStage::Leaf { leaf, root_alias } = stage else {
            slot.push(StateTakenDestination { evidence, stage });
            anyhow::bail!("State destination input did not reach its typed leaf");
        };
        match join.retain_leaf(leaf, evidence, factory, terminal, prepay) {
            // The retained leaf holds the same root allocation.
            Ok(()) => drop(root_alias),
            Err((leaf, evidence, error)) => {
                slot.push(StateTakenDestination { evidence, stage: StateTakenStage::Leaf { leaf, root_alias } });
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Convert one taken input toward its typed State leaf. Every refusal
/// returns the furthest stage reached, still owning every input.
fn advance_taken_destination(
    mut stage: StateTakenStage,
    context: Option<&OriginalProjectionBuildContext>,
) -> (StateTakenStage, Option<anyhow::Error>) {
    fn state_projection_domain(domain: Option<lillux_sqlite::OriginalNativeDomain>) -> bool {
        matches!(domain, Some(lillux_sqlite::OriginalNativeDomain::NodeProjection
            | lillux_sqlite::OriginalNativeDomain::StateGeneratedProjection))
    }
    loop {
        stage = match stage {
            StateTakenStage::Raw(lillux_sqlite::OriginalNativeDestinationInputs::Bound(inputs)) => {
                if !state_projection_domain(Some(inputs.domain())) {
                    return (StateTakenStage::Raw(lillux_sqlite::OriginalNativeDestinationInputs::Bound(inputs)),
                        Some(anyhow::anyhow!("State Bound input selects a foreign domain")));
                }
                match inputs.into_original_parts() {
                    Ok(parts) => StateTakenStage::Parts(parts),
                    Err(inputs) => return (
                        StateTakenStage::Raw(lillux_sqlite::OriginalNativeDestinationInputs::Bound(inputs)),
                        Some(anyhow::anyhow!("State Bound input extraction refused"))),
                }
            }
            StateTakenStage::Raw(lillux_sqlite::OriginalNativeDestinationInputs::Transient(authority)) => {
                let concrete = match context.map(|context| context.issued_schema(&authority)) {
                    Some(Ok(Some(concrete))) => concrete,
                    Some(Err(error)) => return (
                        StateTakenStage::Raw(lillux_sqlite::OriginalNativeDestinationInputs::Transient(authority)),
                        Some(error)),
                    Some(Ok(None)) | None => return (
                        StateTakenStage::Raw(lillux_sqlite::OriginalNativeDestinationInputs::Transient(authority)),
                        Some(anyhow::anyhow!("State transient input selects an authority this State did not issue"))),
                };
                // The concrete Arc is the same allocation; only the dyn alias ends.
                drop(authority);
                StateTakenStage::Leaf {
                    leaf: super::destination_join::StateDestinationLeaf::State(concrete),
                    root_alias: None,
                }
            }
            StateTakenStage::Parts(parts) => {
                if parts.inspection.is_some() || !state_projection_domain(Some(parts.domain)) {
                    return (StateTakenStage::Parts(parts),
                        Some(anyhow::anyhow!("State Bound parts carry an inspection or foreign domain")));
                }
                let concrete = match context.map(|context| context.issued_generated(&parts.authority)) {
                    Some(Ok(Some(concrete))) if Arc::ptr_eq(&concrete.root, &parts.root) => concrete,
                    Some(Err(error)) => return (StateTakenStage::Parts(parts), Some(error)),
                    Some(Ok(_)) | None => return (StateTakenStage::Parts(parts),
                        Some(anyhow::anyhow!("State Bound parts select an authority or root this State did not issue"))),
                };
                let lillux_sqlite::OriginalNativeOutsideBoundParts {
                    root, name, mode, domain, authority, inspection } = parts;
                // Coordinate facts and the dyn alias end; the concrete leaf
                // keeps the same authority and root allocations.
                drop(name);
                let _ = (mode, domain);
                drop(inspection);
                drop(authority);
                StateTakenStage::Leaf {
                    leaf: super::destination_join::StateDestinationLeaf::Generated(concrete),
                    root_alias: Some(root),
                }
            }
            leaf @ StateTakenStage::Leaf { leaf: _, root_alias: _ } => return (leaf, None),
        };
    }
}

/// One closed State connection's durable metadata-pin disposition. The Weak
/// names the exact connection allocation across retries without owning it,
/// and is cleared when the record moves into the non-owning receipt.
struct StateMetadataPinLeaf {
    connection: Option<std::sync::Weak<lillux_sqlite::ClosedOriginalNativeConnection>>,
    ticket: Option<u32>,
    pins: [Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
    receipts: [Option<lillux_sqlite::RetiredOriginalMetadataPin>; 2],
}
impl StateMetadataPinLeaf {
    fn selects(&self, connection: &Arc<lillux_sqlite::ClosedOriginalNativeConnection>) -> bool {
        self.connection.as_ref()
            .is_some_and(|original| std::ptr::eq(original.as_ptr(), Arc::as_ptr(connection)))
    }
    fn same_pins(&self, pins: &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2]) -> bool {
        self.pins.iter().zip(pins.iter()).all(|pair| match pair {
            (Some(original), Some(actual)) => Arc::ptr_eq(original, actual),
            (None, None) => true,
            _ => false,
        })
    }
    fn require_receipts(&self) -> anyhow::Result<()> {
        for (pin, receipt) in self.pins.iter().zip(self.receipts.iter()) {
            match (pin, receipt) {
                (Some(pin), Some(receipt)) => receipt.require_original(pin)?,
                (None, None) => {}
                _ => anyhow::bail!("State metadata-pin receipt census changed"),
            }
        }
        Ok(())
    }
}

/// Pre-seal retirement of generated-Bound main/WAL pins reachable from one
/// State frame's closed connections. Records and receipts stay in the frame
/// and move into `StateNativeSourcesRetired`. The borrowed leaf evidence binds
/// issued inputs only and is never retained or compared as a consumed leaf.
pub(super) struct StateMetadataPinRetirement {
    process: u32,
    leaves: Vec<StateMetadataPinLeaf>,
    complete: bool,
}
impl StateMetadataPinRetirement {
    pub(super) fn empty() -> Self {
        Self { process: std::process::id(), leaves: Vec::new(), complete: false }
    }

    /// Requires the issuer-acknowledged terminal, so every `durable_body` is
    /// frozen. A refusal keeps every record, pin and receipt for the retry.
    pub(super) fn retire(
        &mut self,
        mut candidates: Vec<Arc<lillux_sqlite::ClosedOriginalNativeConnection>>,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
        issue_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        ) -> anyhow::Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign State metadata-pin retirement");
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        if self.complete {
            for leaf in &self.leaves { leaf.require_receipts()?; }
            return Ok(());
        }
        for index in (0..candidates.len()).rev() {
            if candidates[..index].iter().any(|prior| Arc::ptr_eq(prior, &candidates[index])) {
                drop(candidates.remove(index));
            }
        }
        anyhow::ensure!(self.leaves.len() <= candidates.len()
            && self.leaves.iter().all(|leaf| candidates.iter().any(|actual| leaf.selects(actual))),
            "State metadata-pin census changed across finalizer attempts");
        let quote = quote_state_metadata_pin_leaves(candidates.len())?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        terminal.require_original(factory)?;
        self.leaves.try_reserve_exact(candidates.len() - self.leaves.len())?;
        for connection in &candidates {
            let (leaf, pins) = connection.retained_durable_metadata_leaf(factory, terminal)?;
            let index = match self.leaves.iter().position(|record| record.selects(connection)) {
                Some(index) => {
                    anyhow::ensure!(self.leaves[index].same_pins(&pins),
                        "State metadata-pin disposition changed after native finish");
                    index
                }
                None => {
                    self.leaves.push(StateMetadataPinLeaf {
                        connection: Some(Arc::downgrade(connection)),
                        ticket: leaf.native_ticket()?,
                        pins, receipts: [None, None] });
                    self.leaves.len() - 1
                }
            };
            let record = &mut self.leaves[index];
            // NotCreated: no body was installed, and the issuer refuses an
            // empty inventory, so this leaf has nothing to retire.
            if record.pins.iter().all(Option::is_none) { continue; }
            if record.pins.iter().zip(record.receipts.iter())
                .all(|(pin, receipt)| pin.is_none() || receipt.is_some()) { continue; }
            // The issuer charges the original late owner before allocating
            // these inputs; they hold strong factory/terminal owners and end here.
            let inputs = issue_inputs(&leaf, &record.pins)?;
            for (pin, receipt) in record.pins.iter().zip(record.receipts.iter_mut()) {
                if receipt.is_some() { continue; }
                let Some(pin) = pin else { continue; };
                *receipt = Some(pin.retire_after_backend(factory, terminal, &inputs)?);
            }
            drop(inputs);
        }
        anyhow::ensure!(self.leaves.len() == candidates.len(),
            "State metadata-pin census lost a leaf record");
        drop(candidates);
        for leaf in &self.leaves { leaf.require_receipts()?; }
        self.complete = true;
        Ok(())
    }

    /// Move the completed census into the non-owning receipt. Connection
    /// Weak identities are cleared; only pins, receipts and tickets remain.
    pub(super) fn into_sources_retired(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        projection_refusals: usize,
        prior_refusal: bool,
    ) -> anyhow::Result<StateNativeSourcesRetired> {
        anyhow::ensure!(self.process == std::process::id() && self.complete,
            "State native sources retire before their metadata-pin census");
        let mut leaves = std::mem::take(&mut self.leaves);
        for leaf in &mut leaves { leaf.connection = None; }
        Ok(StateNativeSourcesRetired { process: self.process, factory: Arc::downgrade(factory),
            leaves, projection_refusals, prior_refusal })
    }
}

/// Non-owning receipt that a State frame's strong native owners ended before
/// the region seal, after its generated Bound metadata pins retired. It holds
/// no factory, terminal, connection or proof: only the pin receipts, copied
/// ticket/process identity, a Weak factory witness and bounded diagnostics.
pub(super) struct StateNativeSourcesRetired {
    process: u32,
    factory: std::sync::Weak<OriginalNativeFactory>,
    leaves: Vec<StateMetadataPinLeaf>,
    projection_refusals: usize,
    prior_refusal: bool,
}
impl StateNativeSourcesRetired {
    /// Same original factory allocation by Weak identity; never upgraded.
    pub(super) fn require_original_factory(&self, factory: &Arc<OriginalNativeFactory>) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id()
            && std::sync::Weak::ptr_eq(&self.factory, &Arc::downgrade(factory)),
            "State native source receipt selects another original factory");
        Ok(())
    }

    /// Verify only cached same-owner close receipts. No factory, terminal or
    /// closed connection is dereferenced, so this is valid after release.
    pub(super) fn require_retired(&self, prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>)
        -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign State native source receipt");
        let quote = quote_state_metadata_pin_receipt_checks(self.leaves.len())?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        self.require_retired_holders()
    }
    /// The funding-free part of `require_retired`: every leaf ended its
    /// connection, kept its ticket and holds its receipts.
    pub(super) fn require_retired_holders(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign State native source receipt");
        for leaf in &self.leaves {
            anyhow::ensure!(leaf.connection.is_none() && leaf.ticket.is_some(),
                "State native source receipt retains a connection or lost its ticket");
            leaf.require_receipts()?;
        }
        Ok(())
    }
    /// The receipt of a frame that had no native source to retire.
    #[cfg(test)]
    pub(super) fn retired_for_test() -> Self {
        Self { process: std::process::id(), factory: std::sync::Weak::new(), leaves: Vec::new(),
            projection_refusals: 0, prior_refusal: false }
    }

    pub(super) fn diagnostics(&self) -> (usize, bool) {
        (self.projection_refusals, self.prior_refusal)
    }
}

pub(super) struct LiveRecoveryPhysicalRegistrations {
    process: u32,
    owners: [Option<std::sync::Weak<lillux::PinnedDirectory>>; 2],
    bindings: [Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>; 2],
    registry_identity: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    retired_sources_consumed: bool,
}
impl LiveRecoveryPhysicalRegistrations {
    pub(super) fn empty() -> Self {
        Self { process: std::process::id(), owners: [None, None], bindings: [None, None],
            registry_identity: None, retired_sources_consumed: false }
    }
    /// Empty registrations whose retired sources are already consumed.
    #[cfg(test)]
    pub(super) fn exhausted_for_test() -> Self {
        let mut exhausted = Self::empty();
        exhausted.retired_sources_consumed = true;
        exhausted
    }
    pub(super) fn require_registered(
        &self,
        roots: &[Arc<lillux::PinnedDirectory>; 2],
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign live recovery registration");
        require_original_root_receipt_registry(self.registry_identity.as_ref(), registry)?;
        for (slot, actual) in roots.iter().enumerate() {
            let owner = self.owners[slot].as_ref()
                .context("actual recovery root owner identity absent")?;
            anyhow::ensure!(std::sync::Weak::ptr_eq(owner, &Arc::downgrade(actual)),
                "recovery registration changed actual owner allocation");
            let binding = self.bindings[slot].as_ref()
                .context("actual recovery root membership absent")?;
            registry.require_membership(binding)?;
            anyhow::ensure!(matches!(binding.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                "actual recovery registered root role changed");
        }
        Ok(())
    }
    pub(super) fn require_registered_sources(
        &self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign live recovery registration");
        require_original_root_receipt_registry(self.registry_identity.as_ref(), registry)?;
        for slot in 0..self.bindings.len() {
            let owner = self.owners[slot].as_ref()
                .context("actual recovery root owner identity absent")?
                .upgrade().context("registered recovery root owner disappeared")?;
            let binding = self.bindings[slot].as_ref()
                .context("actual recovery root membership absent")?;
            registry.require_directory_source(binding, &owner)?;
        }
        Ok(())
    }
    /// # Safety
    /// The SAME original typed recovery owner remains retained by its outside
    /// supervisor; registration does not certify semantic recovery completion.
    pub(super) unsafe fn register(&mut self, recovery: &RecoveryStore,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign live recovery registration");
        anyhow::ensure!(!self.retired_sources_consumed,
            "retired live recovery registrations cannot be reopened");
        bind_root_receipt_registry(&mut self.registry_identity, registry)?;
        let additional_sources = self.bindings.iter().filter(|binding| binding.is_none()).count();
        let quote = quote_live_state_recovery_registration(registry, additional_sources)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        anyhow::ensure!(self.process == std::process::id(), "foreign recovery after original validation debit");
        if self.bindings.iter().all(Option::is_some) {
            let roots = recovery.borrow_original_directory_custody();
            return self.require_registered(&roots, registry);
        }
        let roots = recovery.borrow_original_directory_custody();
        for (slot, actual) in roots.into_iter().enumerate() {
            if let Some(binding) = &self.bindings[slot] {
                anyhow::ensure!(self.owners[slot].as_ref().is_some_and(|owner|
                    std::sync::Weak::ptr_eq(owner, &Arc::downgrade(&actual))),
                    "recovery registration changed actual owner allocation");
                registry.require_membership(binding)?;
                drop(actual); // actual typed recovery owner still holds this root
                continue;
            }
            let identity = Arc::downgrade(&actual);
            match unsafe { registry.register_directory(actual, prepay) } {
                Ok(binding) => { self.owners[slot] = Some(identity); self.bindings[slot] = Some(binding); }
                Err((alias, error)) => { drop(alias); return Err(error); }
            }
        }
        self.require_registered(&recovery.borrow_original_directory_custody(), registry)
    }

    pub(super) fn consume_retired_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign retired live recovery registration");
        require_retired_root_receipt_registry(self.registry_identity.as_ref(), registry)?;
        let quote = quote_state_root_receipt_owner(2, 0)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        require_retired_root_receipt_registry(self.registry_identity.as_ref(), registry)?;
        if self.retired_sources_consumed {
            anyhow::ensure!(self.bindings.iter().all(Option::is_none)
                && self.owners.iter().all(Option::is_none),
                "retired live recovery owner retained a consumed source");
            return Ok(());
        }
        for slot in 0..self.bindings.len() {
            anyhow::ensure!(self.owners[slot].is_some(),
                "selected live recovery root lost its original owner identity");
            let binding = self.bindings[slot].as_ref()
                .context("selected live recovery root lost its registration")?;
            anyhow::ensure!(matches!(binding.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                "retired live recovery root registration changed role");
        }
        for binding in self.bindings.iter().flatten() {
            drop(binding.receipt_in(registry)?);
        }
        self.bindings = std::array::from_fn(|_| None);
        self.owners = std::array::from_fn(|_| None);
        self.retired_sources_consumed = true;
        Ok(())
    }
}
/// End a State parent's RecoveryStore alias while the registry is still
/// live. Its two roots must already be registered as the SAME allocations, so
/// the registry becomes their sole owner before descriptor preparation
/// requires unique custody. Refusal keeps the alias for retry.
fn retire_parent_recovery_owner(
    recovery: &mut Option<RecoveryStore>,
    consumed: &mut bool,
    registrations: &LiveRecoveryPhysicalRegistrations,
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if *consumed {
        return registrations.require_registered_sources(registry);
    }
    let quote = quote_live_state_parent_recovery_owner_retirement(registry)?;
    registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
    let retained = recovery.as_ref().context("State parent recovery owner absent before retirement")?;
    registrations.require_registered(&retained.borrow_original_directory_custody(), registry)?;
    let owner = recovery.take().context("State parent recovery owner absent before retirement")?;
    drop(owner.into_original_directory_custody());
    *consumed = true;
    Ok(())
}

const LIVE_STATE_PHYSICAL_PARENT_KIND: &str = "ryeos-state::live-state-physical-parent";

/// Everything a physical parent owns, in the order its release drops it, plus
/// plain copied facts. Constructible only in this module, so no caller can
/// swap the whole out from under its custody. `process` repeats the issuing
/// process for the mid-operation re-checks after funding callbacks.
struct LiveStatePhysicalParentParts {
    process: u32,
    runtime: Option<Arc<lillux::PinnedDirectory>>,
    namespace: Option<lillux::PinnedDirectoryLock>,
    roots: [Option<Arc<lillux::PinnedDirectory>>; 4],
    registrations: [Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>; 6],
    registry_identity: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    // Authority and lease edges remain actual original obligations. Root
    // registration cannot discard these or stand in for their terminal join.
    _host: Option<LiveStateNativeHost>,
    retired_host: Option<RetiredLiveStateHostAliases>,
    // Taken by name when the parent's recovery alias retires.
    _recovery: Option<RecoveryStore>,
    recovery_registrations: LiveRecoveryPhysicalRegistrations,
    _trust: Arc<TrustStore>,
    _join_link: Mutex<std::sync::Weak<super::destination_join::OriginalStateDestinationJoin>>,
    retired_root_sources_consumed: bool,
    recovery_owner_consumed: bool,
}
/// Settled exactly when final exhaustion would accept these parts; they then
/// release normally. Otherwise the host, trust map, join link and every root,
/// registration and recovery owner stay retained together until the process
/// exits.
fn live_state_physical_parent_exhausted(parts: &LiveStatePhysicalParentParts) -> bool {
    parts.require_exhausted_holders().is_ok()
}
pub(crate) struct LiveStatePhysicalParent {
    inner: Held<LiveStatePhysicalParentParts>,
}
struct RetiredLiveStateHostAliases {
    factory: std::sync::Weak<OriginalNativeFactory>,
    store: std::sync::Weak<dyn OriginalNativeDestinationAuthority>,
    lease: std::sync::Weak<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
    _deadline: lillux::time::MonotonicDeadline,
}
impl LiveStatePhysicalParentParts {
    pub(crate) fn retire_host_aliases(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        store: &Arc<dyn OriginalNativeDestinationAuthority>,
        lease: &Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign live State host alias transfer");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let quote = quote_live_state_host_alias_retirement(registry)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        anyhow::ensure!(self.process == std::process::id(), "foreign State host after original prepay");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.require_registered(registry)?;
        for binding in &self.recovery_registrations.bindings {
            registry.require_membership(binding.as_ref().context("actual State recovery membership absent")?)?;
        }
        if let Some(retired) = &self.retired_host {
            anyhow::ensure!(self._host.is_none()
                && std::sync::Weak::ptr_eq(&retired.factory, &Arc::downgrade(factory))
                && std::sync::Weak::ptr_eq(&retired.store, &Arc::downgrade(store))
                && std::sync::Weak::ptr_eq(&retired.lease, &Arc::downgrade(lease)),
                "retired State host aliases changed canonical owner");
            return self.retire_recovery_owner(registry, prepay);
        }
        let host = self._host.as_ref().context("actual State native host absent without transfer")?;
        anyhow::ensure!(Arc::ptr_eq(&host.factory, factory)
            && Arc::ptr_eq(&host.authority, store) && Arc::ptr_eq(&host.lease_authority, lease),
            "State host transfer changes actual Store/factory/lease allocations");
        let host = self._host.take().expect("checked actual original host");
        let LiveStateNativeHost { factory: original_factory, authority, lease_authority, deadline } = host;
        self.retired_host = Some(RetiredLiveStateHostAliases {
            factory: Arc::downgrade(&original_factory), store: Arc::downgrade(&authority),
            lease: Arc::downgrade(&lease_authority), _deadline: deadline,
        });
        // Canonical SAME allocations are held by the actual upper Store and
        // outside factory before these redundant edges end. No owner disposal,
        // expiry renewal or terminal acknowledgement follows from this fact.
        drop(lease_authority); drop(authority); drop(original_factory);
        self.retire_recovery_owner(registry, prepay)
    }

    fn retire_recovery_owner(&mut self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign State parent recovery owner");
        retire_parent_recovery_owner(&mut self._recovery, &mut self.recovery_owner_consumed,
            &self.recovery_registrations, registry, prepay)
    }

    /// This body is the live parent `join` transferred out, is registered in
    /// `registry` with its recovery roots, and has retired its host aliases.
    pub(crate) fn require_completed_outside_parent(&self,
        join: &Arc<super::destination_join::OriginalStateDestinationJoin>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign live State parent completion");
        {
            let link = self._join_link.try_lock()
                .map_err(|_| anyhow::anyhow!("live State parent join link busy or poisoned"))?;
            anyhow::ensure!(std::sync::Weak::ptr_eq(&link, &Arc::downgrade(join)),
                "live State parent body belongs to another destination join");
        }
        anyhow::ensure!(self._host.is_none() && self.retired_host.is_some()
            && self.recovery_owner_consumed,
            "live State parent host or recovery aliases remain before completion");
        self.require_registered(registry)?;
        for binding in &self.recovery_registrations.bindings {
            registry.require_membership(binding.as_ref().context("actual State recovery membership absent")?)?;
        }
        Ok(())
    }

    fn from_original(original: LiveStateNativeOwner) -> Self {
        let LiveStateNativeOwner { outside_join, host, runtime, namespace, roots, _recovery, _trust } = original;
        Self { process: std::process::id(), runtime: Some(runtime), namespace: Some(namespace),
            roots: roots.map(|root| Some(Arc::new(root))),
            registrations: std::array::from_fn(|_| None),
            registry_identity: None, retired_root_sources_consumed: false,
            _host: Some(host), retired_host: None, _recovery: Some(_recovery),
            recovery_owner_consumed: false, recovery_registrations: LiveRecoveryPhysicalRegistrations::empty(),
            _trust, _join_link: outside_join }
    }
    /// Parts in the shape final exhaustion accepts: every root, registration,
    /// host alias and recovery owner already ended.
    #[cfg(test)]
    fn exhausted_for_test() -> Self {
        Self { process: std::process::id(), runtime: None, namespace: None,
            roots: std::array::from_fn(|_| None), registrations: std::array::from_fn(|_| None),
            registry_identity: None, retired_root_sources_consumed: true,
            _host: None,
            retired_host: Some(RetiredLiveStateHostAliases {
                factory: std::sync::Weak::new(),
                store: std::sync::Weak::<LiveStateNativeOwner>::new(),
                lease: std::sync::Weak::<LiveStateNativeOwner>::new(),
                _deadline: lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(1)),
            }),
            _recovery: None, recovery_owner_consumed: true,
            recovery_registrations: LiveRecoveryPhysicalRegistrations::exhausted_for_test(),
            _trust: Arc::new(TrustStore::new()),
            _join_link: Mutex::new(std::sync::Weak::new()) }
    }
    /// # Safety
    /// This whole parent remains cached in the same prepaid original outside
    /// frame. Registration never closes sources or ends host/Store authority.
    pub(crate) unsafe fn register_roots(&mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign live State physical parent");
        anyhow::ensure!(!self.retired_root_sources_consumed,
            "retired live State parent roots cannot be reopened");
        bind_root_receipt_registry(&mut self.registry_identity, registry)?;
        if self.recovery_owner_consumed {
            // The RecoveryStore alias ends only after every root joined this
            // registry, so a retry can only revalidate complete membership.
            let quote = quote_live_state_parent_root_membership(registry, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            anyhow::ensure!(self.process == std::process::id(),
                "foreign State parent after consumed-recovery validation debit");
            self.require_registered(registry)?;
            return self.recovery_registrations.require_registered_sources(registry);
        }
        if self.runtime.is_none() && self.namespace.is_none()
            && self.roots.iter().all(Option::is_none)
            && self.registrations.iter().all(Option::is_some) {
            let quote = quote_live_state_parent_root_membership(registry, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            anyhow::ensure!(self.process == std::process::id(),
                "foreign State parent after completed-stage validation debit");
            self.require_registered(registry)?;
            let recovery = self._recovery.as_ref().context("State parent recovery owner already retired")?;
            return unsafe { self.recovery_registrations.register(recovery, registry, prepay) };
        }
        let additional_sources = usize::from(self.registrations[0].is_none() && self.runtime.is_some())
            + (0..4).filter(|slot| {
                self.registrations[*slot + 1].is_none() && self.roots[*slot].is_some()
            }).count()
            + usize::from(self.registrations[5].is_none() && self.namespace.is_some());
        let quote = quote_live_state_parent_root_membership(registry, additional_sources)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        anyhow::ensure!(self.process == std::process::id(), "foreign State parent after membership debit");
        for slot in 0..5 {
            let source = if slot == 0 { &mut self.runtime } else { &mut self.roots[slot - 1] };
            if let Some(binding) = &self.registrations[slot] {
                registry.require_membership(binding)?;
                anyhow::ensure!(source.is_none(), "registered State root retains duplicate source");
                continue;
            }
            let quote = quote_live_state_root_registration_handoff();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            anyhow::ensure!(self.process == std::process::id(), "foreign State parent after original prepay");
            let actual = source.as_ref().context("actual live State root absent without registration")?.clone();
            match unsafe { registry.register_directory(actual, prepay) } {
                Ok(binding) => {
                    self.registrations[slot] = Some(binding);
                    drop(source.take());
                }
                Err((alias, error)) => { drop(alias); return Err(error); }
            }
        }
        if self.registrations[5].is_none() {
            let quote = quote_live_state_root_registration_handoff();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            anyhow::ensure!(self.process == std::process::id(), "foreign State parent after original prepay");
            let actual = self.namespace.as_ref().context("actual State namespace absent without registration")?.clone();
            match unsafe { registry.register_namespace(actual, prepay) } {
                Ok(binding) => { self.registrations[5] = Some(binding); drop(self.namespace.take()); }
                Err((alias, error)) => { drop(alias); return Err(error); }
            }
        }
        self.require_registered(registry)?;
        let recovery = self._recovery.as_ref().context("State parent recovery owner already retired")?;
        unsafe { self.recovery_registrations.register(recovery, registry, prepay) }
    }
    pub(crate) fn require_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign State parent registration census");
        let identity = registry.identity()?;
        self.registry_identity.as_ref()
            .context("State parent original registry identity absent")?
            .require_same_registry(&identity)?;
        anyhow::ensure!(self.runtime.is_none() && self.namespace.is_none()
            && self.roots.iter().all(Option::is_none), "actual State parent roots remain unregistered");
        for (slot, binding) in self.registrations.iter().enumerate() {
            let binding = binding.as_ref().context("actual State parent membership absent")?;
            registry.require_membership(binding)?;
            let source = binding.observed_source_identity()?;
            anyhow::ensure!(if slot == 5 {
                matches!(source, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. })
            } else {
                matches!(source, lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. })
            }, "registered State parent source role changed");
        }
        Ok(())
    }

    pub(crate) fn consume_retired_root_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign retired State parent roots");
        require_retired_root_receipt_registry(self.registry_identity.as_ref(), registry)?;
        anyhow::ensure!(self._host.is_none() && self.retired_host.is_some(),
            "State parent host aliases remain before root receipt consumption");
        anyhow::ensure!(self.recovery_owner_consumed,
            "State parent RecoveryStore alias survived descriptor retirement");
        let quote = quote_state_root_receipt_owner(5, 1)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        require_retired_root_receipt_registry(self.registry_identity.as_ref(), registry)?;
        if !self.retired_root_sources_consumed {
            anyhow::ensure!(self.runtime.is_none() && self.namespace.is_none()
                && self.roots.iter().all(Option::is_none),
                "retired State parent retains a physical root alias");
            for (slot, binding) in self.registrations.iter().enumerate() {
                let binding = binding.as_ref().context("retired State parent root registration absent")?;
                anyhow::ensure!(if slot == 5 {
                    matches!(binding.observed_source_identity()?,
                        lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. })
                } else {
                    matches!(binding.observed_source_identity()?,
                        lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. })
                }, "retired State parent root registration changed role");
            }
            for binding in self.registrations.iter().flatten() {
                drop(binding.receipt_in(registry)?);
            }
            self.registrations = std::array::from_fn(|_| None);
            self.retired_root_sources_consumed = true;
        } else {
            anyhow::ensure!(self.registrations.iter().all(Option::is_none)
                && self.runtime.is_none() && self.namespace.is_none()
                && self.roots.iter().all(Option::is_none),
                "consumed State parent roots reappeared");
        }
        self.recovery_registrations.consume_retired_sources(registry, prepay)
    }

    pub(crate) fn require_registered_with_prepay(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let quote = quote_live_state_registered_root_validation(registry, self.registrations.len())?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        self.require_registered(registry)
    }
}
impl LiveStateNativeOwner {
    pub(super) fn capture(
        host: LiveStateNativeHost,
        runtime: &lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        cas: &lillux::PinnedDirectory,
        refs: &lillux::PinnedDirectory,
        locators: &lillux::PinnedDirectory,
        recovery_directory: &lillux::PinnedDirectory,
        recovery: &RecoveryStore,
        trust: Arc<TrustStore>,
    ) -> anyhow::Result<Arc<Self>> {
        host.factory.require_ready()?;
        host.factory.require_runtime_binding(runtime)?;
        host.authority.require_original(&host.factory)?;
        namespace.ensure_protects(runtime)?;
        let original = Arc::new(Self {
            outside_join: Mutex::new(std::sync::Weak::new()),
            host,
            runtime: Arc::new(runtime.try_clone()?),
            namespace: namespace.clone(),
            roots: [
                cas.try_clone()?,
                refs.try_clone()?,
                locators.try_clone()?,
                recovery_directory.try_clone()?,
            ],
            _recovery: recovery.clone(),
            _trust: trust,
        });
        original.require_original(&original.host.factory)?;
        Ok(original)
    }
    /// Prepare the same receiver for a real State parent whose Operational
    /// startup failed before a StateDb could be published. No synthetic State
    /// or offline authority is constructed for that failure path.
    pub(crate) fn prepare_original_destination_join(self: &Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<Arc<super::OriginalStateDestinationJoin>> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        OriginalNativeDestinationAuthority::require_original(self.as_ref(), factory)?;
        let quote = quote_live_state_destination_join();
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        OriginalNativeDestinationAuthority::require_original(self.as_ref(), factory)?;
        let actual = super::OriginalProjectionStateOwner::Live(Arc::clone(self));
        actual.original_destination_join(factory, aggregate, prepay)
    }

    pub(crate) fn runtime(&self) -> &Arc<lillux::PinnedDirectory> {
        &self.runtime
    }
    pub(crate) fn namespace(&self) -> lillux::PinnedDirectoryLock {
        self.namespace.clone()
    }
    pub(crate) fn require_original_store_parent(&self,
        store: &Arc<dyn OriginalNativeDestinationAuthority>, factory: &Arc<OriginalNativeFactory>) -> anyhow::Result<()> {
        factory.require_original_process()?;
        anyhow::ensure!(Arc::ptr_eq(factory, &self.host.factory), "live State parent selected foreign factory");
        anyhow::ensure!(Arc::ptr_eq(store, &self.host.authority), "live State parent belongs to another actual Store owner");
        Ok(())
    }
    pub(crate) fn factory(&self) -> &Arc<OriginalNativeFactory> {
        &self.host.factory
    }

    pub(crate) fn schema_reference_inputs(
        self: &Arc<Self>,
    ) -> anyhow::Result<lillux_sqlite::OriginalNativeTransientInputs> {
        self.require_original(&self.host.factory)?;
        self.host.factory.require_ready()?;
        anyhow::ensure!(
            !self.host.deadline.has_elapsed(),
            "original live State service deadline elapsed"
        );
        let authority: Arc<dyn OriginalNativeDestinationAuthority> = self.clone();
        // SAME actual State roots/trust/host and paid nonrecycled native ticket.
        unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::NodeOperationalSchemaReference,
                authority,
            )
        }
    }

    pub(crate) fn operational_inputs(
        self: &Arc<Self>,
        established: bool,
    ) -> anyhow::Result<OriginalNativeBoundInputs> {
        self.require_original(&self.host.factory)?;
        self.host.factory.require_ready()?;
        self.host.factory.require_runtime_binding(&self.runtime)?;
        anyhow::ensure!(
            !self.host.deadline.has_elapsed(),
            "original live State service deadline elapsed"
        );
        let authority: Arc<dyn OriginalNativeDestinationAuthority> = self.clone();
        // SAFETY: StateDb created this owner from its actual pinned authority,
        // recovery and trust roots under the retained host/namespace/funding.
        // The fixed Operational role is issued here, not chosen by a DTO.
        unsafe {
            OriginalNativeBoundInputs::issued(
                Arc::clone(&self.runtime),
                std::ffi::OsStr::new(crate::operational::OPERATIONAL_DB_FILENAME),
                if established {
                    OriginalNativeBoundMode::Existing
                } else {
                    OriginalNativeBoundMode::OpenOrCreate
                },
                OriginalNativeDomain::NodeOperational,
                authority,
                None,
            )
        }
    }
}
impl OriginalNativeDestinationAuthority for LiveStateNativeOwner {
    fn into_outside_custody(self: Arc<Self>, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal, leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>)
        -> Result<lillux_sqlite::OriginalDestinationOutsideCustody, lillux_sqlite::OriginalDestinationCustodyFailure> {
        super::destination_join::transfer_leaf(super::destination_join::StateDestinationLeaf::Live(self), factory, aggregate, leaf, prepay)
    }

    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(factory, &self.host.factory),
            "foreign live State native factory"
        );
        self.host.authority.require_original(factory)?;
        self.runtime.ensure_path_binding()?;
        self.namespace.ensure_protects(&self.runtime)?;
        for root in &self.roots {
            root.ensure_path_binding()?;
        }
        Ok(())
    }
}

pub(super) struct LiveStateOpenCustody {
    pub(super) original: Arc<LiveStateNativeOwner>,
    pub(super) operational: Option<OperationalDb>,
    pub(super) projection_context: Option<OriginalProjectionBuildContext>,
}
impl LiveStateOpenCustody {
    pub(super) fn fail(self, error: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(LiveStateDatabaseOpenFailure {
            error,
            factory: Arc::clone(self.original.factory()),
            _custody: Mutex::new(self),
        })
    }
}
impl Drop for LiveStateOpenCustody {
    fn drop(&mut self) {
        if self.operational.is_some() || self.projection_context.is_some() {
            // Projection context can retain schema terminals, generation
            // leases and publication custody even without an Operational
            // wrapper. None may disappear through a partial-start unwind or a
            // discarded open failure: native handles may still be live, so
            // the namespace exclusion and roots must outlive them. The owners
            // move whole into the process-lifetime abandoned-opening registry.
            // This is retention, not a completed recovery route.
            retain_abandoned_live_opening(AbandonedLiveOpening::State(AbandonedLiveStateOpening {
                _projection_context: self.projection_context.take(),
                _operational: self.operational.take(),
                _original: Arc::clone(&self.original),
            }));
        }
    }
}

/// The owners of a live State opening discarded without settlement.
pub(crate) struct AbandonedLiveStateOpening {
    _projection_context: Option<OriginalProjectionBuildContext>,
    _operational: Option<OperationalDb>,
    _original: Arc<LiveStateNativeOwner>,
}

/// One live opening discarded without settlement through `close_retained`.
pub(crate) enum AbandonedLiveOpening {
    State(AbandonedLiveStateOpening),
    Operational(Box<crate::operational::live_native::AbandonedLiveOperationalOpening>),
}

/// Process-lifetime retention of live openings discarded without settlement.
/// Their native handles may still be live, so every owner, including the
/// namespace exclusion and roots, stays reachable here until the process
/// exits. Nothing is ever removed from this list.
static ABANDONED_LIVE_OPENINGS: Mutex<Vec<AbandonedLiveOpening>> = Mutex::new(Vec::new());

pub(crate) fn retain_abandoned_live_opening(opening: AbandonedLiveOpening) {
    ABANDONED_LIVE_OPENINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(opening);
}

/// Count of live openings this process retains because they were discarded
/// without settlement. Any nonzero count means native custody and its
/// namespace exclusion stay held until the process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AbandonedLiveOpenings {
    pub state: usize,
    pub operational: usize,
}

/// Report the abandoned live openings this process retains.
pub fn abandoned_live_openings() -> AbandonedLiveOpenings {
    let retained = ABANDONED_LIVE_OPENINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut report = AbandonedLiveOpenings::default();
    for opening in retained.iter() {
        match opening {
            AbandonedLiveOpening::State(_) => report.state += 1,
            AbandonedLiveOpening::Operational(_) => report.operational += 1,
        }
    }
    report
}

/// A refused live State opening. It retains every partial owner; settle it
/// through [`Self::close_retained`]. Discarding it moves the owners into the
/// process-lifetime abandoned-opening registry.
#[must_use = "settle a refused live State opening through close_retained"]
pub struct LiveStateDatabaseOpenFailure {
    error: anyhow::Error,
    factory: Arc<OriginalNativeFactory>,
    _custody: Mutex<LiveStateOpenCustody>,
}
impl std::fmt::Debug for LiveStateDatabaseOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveStateDatabaseOpenFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveStateDatabaseOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "live State partial opening retained: {}", self.error)
    }
}
impl std::error::Error for LiveStateDatabaseOpenFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

struct StartupCloseProgress {
    original: Arc<LiveStateNativeOwner>,
    context: Option<OriginalProjectionBuildContext>,
    operational: Option<OperationalDb>,
    operational_failure: Option<crate::operational::OriginalOperationalFailure>,
    operational_closed: Option<crate::operational::ClosedOriginalOperational>,
    operational_outside: Option<crate::operational::OutsideOriginalOperationalCustody>,
    projection_failure: Option<crate::projection::OriginalProjectionFailure>,
    projection_released: Option<crate::projection::ReleasedOriginalProjectionAcquisition>,
    schema_failure: Option<crate::sqlite_schema::NativeSchemaSqlReferenceFailure>,
    schema_released: Option<crate::sqlite_schema::ReleasedNativeSchemaSqlReference>,
    schema_admission_custody: Vec<(Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>, OriginalProjectionStateOwner)>,
    // Staged durable generations whose publication refused; each filesystem
    // (and its generation-lease alias) joins projection file custody.
    publication_custody: Vec<(Arc<crate::projection::DurableOriginalProjection>, RecoveryStore, String, String)>,
    // Generation sidecar setups that refused before any native lease owner.
    lease_setup_custody: Vec<(std::fs::File, lillux::PinnedDirectory, std::ffi::OsString, Arc<OriginalNativeFactory>)>,
    prior_refusal: Option<anyhow::Error>,
}
/// Actual cleanup holders for a State which never became a service. This
/// neither repairs domain recovery nor certifies successful startup.
pub struct ReleasedLiveStateStartup {
    inner: Held<ReleasedLiveStateStartupParts>,
    // A plain copied fact; it carries no custody.
    projection_leases_retired: bool,
}
const RELEASED_LIVE_STATE_STARTUP_KIND: &str = "ryeos-state::released-live-state-startup";

/// Everything a released startup owns. No drop settles a released startup:
/// its progress always holds the State parent, whose namespace exclusion,
/// roots and host authority end only through the physical-custody or census
/// transfer that consumes this frame whole, so it is held until taken.
/// Constructible only in this module.
struct ReleasedLiveStateStartupParts {
    schema_pending: Vec<Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>>,
    schema_definitions: Vec<crate::sqlite_schema::SchemaSqlDefinition>,
    projection_files: Vec<crate::projection::OutsideOriginalProjectionCustody>,
    projection_native: Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    projection_refusals: Vec<anyhow::Error>,
    // Destination inputs taken from State terminals, pending their State leaf.
    destination_inputs: Vec<StateTakenDestination>,
    destination_join: std::sync::OnceLock<Arc<super::destination_join::OriginalStateDestinationJoin>>,
    progress: StartupCloseProgress,
}
impl ReleasedLiveStateStartup {
    fn new(progress: StartupCloseProgress) -> Self {
        Self {
            inner: Held::until_taken(
                RELEASED_LIVE_STATE_STARTUP_KIND,
                ReleasedLiveStateStartupParts {
                    schema_pending: Vec::new(),
                    schema_definitions: Vec::new(),
                    projection_files: Vec::new(),
                    projection_native: Vec::new(),
                    projection_refusals: Vec::new(),
                    destination_inputs: Vec::new(),
                    destination_join: std::sync::OnceLock::new(),
                    progress,
                },
            ),
            projection_leases_retired: false,
        }
    }
    /// Move every field out; a copy in another process is refused and comes
    /// back still held. Whether the leases retired is a plain copied fact.
    fn into_parts(self) -> std::result::Result<ReleasedLiveStateStartupParts, Self> {
        let Self { inner, projection_leases_retired } = self;
        inner.take_whole().map_err(|inner| Self { inner, projection_leases_retired })
    }
    /// Move every field out, for a caller that already established the
    /// original process; a foreign copy is relinquished and the process
    /// aborts.
    fn into_parts_or_abort(self) -> ReleasedLiveStateStartupParts {
        self.inner.take_or_abort()
    }
}
// Original failed-start frame after its redundant parent edges end. Retaining
// this graph is a resumable source transfer, not a physical terminal receipt.
pub struct ReleasedLiveStatePhysicalCustody {
    inner: Held<ReleasedLiveStatePhysicalParts>,
}
const RELEASED_LIVE_STATE_PHYSICAL_KIND: &str = "ryeos-state::released-live-state-physical-custody";

/// Everything a failed-start State frame owns, in the order its release
/// drops it. Constructible only in this module, so no caller can swap the
/// whole out from under its custody.
struct ReleasedLiveStatePhysicalParts {
    projection_physical: super::live_native::LiveProjectionPhysicalCustody,
    parent: Option<super::live_native::LiveStateNativeOwner>,
    parent_parts: Option<super::live_native::LiveStatePhysicalParent>,
    join: Arc<super::destination_join::OriginalStateDestinationJoin>,
    _context: Option<RetiredStartupProjectionContext>,
    _schema: (Vec<Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>>,
        Vec<crate::sqlite_schema::SchemaSqlDefinition>,
        Vec<(Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>, OriginalProjectionStateOwner)>),
    _projection: (Vec<crate::projection::OutsideOriginalProjectionCustody>,
        Vec<lillux_sqlite::OriginalNativeConnectionTerminal>, Vec<anyhow::Error>),
    operational: Option<crate::operational::OutsideOriginalOperationalCustody>,
    _refusal: Option<anyhow::Error>,
    metadata_pins: StateMetadataPinRetirement,
    native_retired: Option<StateNativeSourcesRetired>,
}
impl ReleasedLiveStatePhysicalParts {
    /// The registry- and funding-free part of final exhaustion. The dropped
    /// frame settles on exactly these checks.
    fn require_exhausted_holders(&self) -> anyhow::Result<()> {
        self.native_retired.as_ref()
            .context("failed State native-source receipt absent at exhaustion")?
            .require_retired_holders()?;
        self.metadata_pins.require_exhausted()?;
        self.projection_physical.require_exhausted()?;
        anyhow::ensure!(self.parent.is_none(), "failed State parent body remains at exhaustion");
        self.parent_parts.as_ref()
            .context("failed State parent parts absent at exhaustion")?
            .require_exhausted_holders()?;
        self.join.require_exhausted_holders()?;
        anyhow::ensure!(self._context.is_none()
            && self._schema.0.is_empty() && self._schema.1.is_empty() && self._schema.2.is_empty()
            && self._projection.0.is_empty() && self._projection.1.is_empty()
            && self._projection.2.is_empty() && self._refusal.is_none(),
            "failed State frame retains a native, schema or projection owner");
        if let Some(operational) = self.operational.as_ref() {
            operational.require_exhausted_holders()?;
        }
        Ok(())
    }
}
/// Settled exactly when final exhaustion would accept the frame; it then
/// releases normally. Otherwise every cached source, parent part, context,
/// schema, projection, Operational owner and receipt stays retained together
/// until the process exits.
fn released_state_physical_settled(parts: &ReleasedLiveStatePhysicalParts) -> bool {
    parts.require_exhausted_holders().is_ok()
}
struct RetiredStartupProjectionContext {
    _factory: Arc<OriginalNativeFactory>,
    _terminals: Vec<Arc<lillux_sqlite::ClosedOriginalNativeConnection>>,
    _durable: Vec<Arc<lillux_sqlite::DurableOriginalNativeBound>>,
    _selected: Vec<Arc<crate::projection::ClosedOriginalProjection>>,
    _schema: Mutex<Vec<Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>>>,
    _leases: Mutex<Vec<Arc<crate::projection::OriginalGenerationLease>>>,
    _attempts: std::sync::atomic::AtomicU32,
}
impl ReleasedLiveStatePhysicalCustody {
    /// Consume only projection-root registration receipts from the exact
    /// retired inventory. Startup context and remaining State custody stay
    /// retained for their own typed finalization stages.
    pub fn consume_retired_projection_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign failed State projection custody");
        self.inner.native_retired.as_ref()
            .context("failed State native sources were not retired before the region seal")?
            .require_retired(prepay)?;
        self.inner.get_mut().projection_physical.consume_retired_sources(registry, prepay)?;
        self.inner.join.consume_retired_generated_roots(registry, prepay)
    }

    /// Consume the shared State-parent and parent-RecoveryStore roots from
    /// the exact retired inventory. The caller first consumes matching Store
    /// packet sources; this then releases only completed live Operational
    /// startup metadata from the same retired inventory. Other startup
    /// context and State evidence remain for later typed finalization stages.
    pub fn consume_retired_root_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign failed State root custody");
        anyhow::ensure!(self.inner.native_retired.is_some(),
            "failed State roots cannot retire before its native-source stage");
        self.inner.get_mut().parent_parts.as_mut().context("actual failed State parent parts absent")?
            .consume_retired_root_sources(registry, prepay)?;
        if let Some(operational) = self.inner.get_mut().operational.as_mut() {
            operational.consume_retired_shared_roots(registry, prepay)?;
            operational.consume_retired_live_metadata(registry, prepay)?;
        }
        Ok(())
    }

    /// Pre-seal native-source stage for this failed-start frame. First retire
    /// the generated-Bound metadata pins of its closed connections (startup
    /// terminals, durable proofs and closed projection terminals, including a
    /// projection-failure retry's closed terminal). Then end every remaining
    /// strong factory holder into a non-owning `StateNativeSourcesRetired`.
    /// Any refusal retains the whole frame and every pin.
    pub fn retire_native_sources(&mut self, factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
        issue_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        ) -> anyhow::Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
    ) -> anyhow::Result<()> {
        self.require_original(factory, terminal)?;
        let ReleasedLiveStatePhysicalParts { projection_physical, parent, parent_parts, join, _context, _schema,
            _projection, operational: _, _refusal, metadata_pins, native_retired } = self.inner.get_mut();
        if let Some(retired) = native_retired.as_ref() {
            retired.require_original_factory(factory)?;
            return retired.require_retired(prepay);
        }
        {
            let projection = &_projection.1;
            let (terminals, durable) = match _context.as_ref() {
                Some(context) => {
                    anyhow::ensure!(context._selected.is_empty(),
                        "failed State retains a projection owner outside its metadata-pin census");
                    (&context._terminals[..], &context._durable[..])
                }
                None => (&[][..], &[][..]),
            };
            let count = terminals.len().checked_add(durable.len())
                .and_then(|count| count.checked_add(projection.len()))
                .context("failed State metadata-pin candidate count overflow")?;
            let mut candidates = state_metadata_pin_candidates(factory, count, prepay)?;
            candidates.extend(terminals.iter().cloned());
            candidates.extend(durable.iter().map(|proof| proof.closed()));
            candidates.extend(projection.iter().filter_map(|terminal| match terminal {
                lillux_sqlite::OriginalNativeConnectionTerminal::Closed(closed) => Some(Arc::clone(closed)),
                lillux_sqlite::OriginalNativeConnectionTerminal::Released(_) => None,
            }));
            require_census_without_destination_inputs(&candidates)?;
            metadata_pins.retire(candidates, factory, terminal, prepay, issue_inputs)?;
        }
        // Holders already ended by earlier joins: the parent body moved into
        // its parts, generation leases became metadata roots, and the receiver
        // retains no leaf source (including a strong `Live` State owner).
        anyhow::ensure!(join.live_parent_completed()?,
            "State live parent handoff is not complete before native-source retirement");
        anyhow::ensure!(parent.is_none() && parent_parts.is_some(),
            "failed State parent body was not transferred into its parts");
        projection_physical.require_leases_retired()?;
        join.require_generated_roots_registered_with_prepay(registry, prepay)?;
        let mut owners = _projection.1.len().checked_add(_projection.2.len())
            .context("failed State native owner count overflow")?;
        if let Some(context) = _context.as_ref() {
            anyhow::ensure!(Arc::ptr_eq(&context._factory, factory) && context._selected.is_empty(),
                "failed State context selects another factory or retains a projection owner");
            anyhow::ensure!(context._schema.try_lock()
                .map_err(|_| anyhow::anyhow!("failed State schema census busy or poisoned"))?.is_empty()
                && context._leases.try_lock()
                .map_err(|_| anyhow::anyhow!("failed State lease census busy or poisoned"))?.is_empty(),
                "failed State retains a schema or generation-lease source");
            owners = owners.checked_add(context._terminals.len())
                .and_then(|owners| owners.checked_add(context._durable.len()))
                .context("failed State native owner count overflow")?;
        }
        anyhow::ensure!(_projection.0.is_empty() && _schema.0.is_empty() && _schema.2.is_empty(),
            "failed State retains an unregistered projection file or schema source");
        require_refusals_without_native_owners(_projection.2.iter().chain(_refusal.iter()))?;
        for native in _projection.1.iter() {
            if let lillux_sqlite::OriginalNativeConnectionTerminal::Released(released) = native {
                require_released_without_owner_inputs(released)?;
            }
        }
        prepay_state_native_source_retirement(factory, owners, prepay)?;
        terminal.require_original(factory)?;
        let receipt = metadata_pins.into_sources_retired(factory, _projection.2.len(), _refusal.is_some())?;
        // Infallible from here: each owner is moved out and ended explicitly;
        // the retained fields keep only empty values.
        if let Some(context) = std::mem::take(&mut *_context) {
            let RetiredStartupProjectionContext { _factory, _terminals, _durable, _selected,
                _schema: schema, _leases: leases, _attempts } = context;
            drop(_durable);
            drop(_terminals);
            drop(_selected);
            drop(schema);
            drop(leases);
            drop(_factory);
        }
        let (files, native, refusals) = std::mem::take(&mut *_projection);
        drop(native);
        drop(refusals);
        drop(files);
        let (pending, definitions, admission) = std::mem::take(&mut *_schema);
        drop(pending);
        drop(admission);
        drop(definitions);
        drop(std::mem::take(&mut *_refusal));
        *native_retired = Some(receipt);
        Ok(())
    }

    pub fn native_sources_retired(&self) -> bool {
        self.inner.native_retired.is_some()
    }

    /// Cached: the live parent handoff completed and its pending mark ended.
    pub fn live_parent_completed(&self) -> anyhow::Result<bool> {
        self.inner.join.live_parent_completed()
    }

    /// Bounded failure diagnostics kept after the native-source stage ended
    /// the projection refusals and prior refusal: (refusal count, prior?).
    pub fn retired_native_source_diagnostics(&self) -> Option<(usize, bool)> {
        self.inner.native_retired.as_ref().map(StateNativeSourcesRetired::diagnostics)
    }

    pub fn retire_parent_host_aliases(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        store: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        lease: &Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.require_original(factory, aggregate)?;
        let parts = self.inner.get_mut();
        let parent = parts.parent_parts.as_mut().context("actual State parent parts absent")?;
        parent.retire_host_aliases(factory, aggregate, store, lease, registry, prepay)?;
        parts.join.complete_live_parent(factory, aggregate, parent, registry, prepay)
    }

    /// # Safety
    /// Retain this whole original frame and the same original registry across
    /// every partial refusal; original admission funds all wrapper/scan work.
    pub unsafe fn register_parent_roots(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.require_original(factory, aggregate)?;
        if self.inner.parent_parts.is_none() {
            anyhow::ensure!(self.inner.parent.is_some(), "actual live State parent body not transferred");
            let quote = quote_live_state_physical_parent()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
            self.require_original(factory, aggregate)?;
            let original = self.inner.get_mut().parent.take().expect("checked actual live State body");
            self.inner.get_mut().parent_parts = Some(super::live_native::LiveStatePhysicalParent::from_original(original));
        }
        let parts = self.inner.get_mut();
        unsafe { parts.parent_parts.as_mut().expect("cached actual live State parts")
            .register_roots(registry, prepay) }?;
        let generations = parts._context.as_ref().map(|context| &context._leases);
        unsafe { parts.projection_physical.register(&mut parts._projection.0,
            generations, factory, registry, prepay) }?;
        unsafe { parts.join.register_generated_roots(registry, prepay) }
    }

    /// # Safety
    /// The actual outside Store must retain this complete frame, have handed
    /// over every admitted native/destination source, and exclude new producers.
    /// A refusal retains the same receiver and all cached partial transfers.
    pub unsafe fn advance_parent(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.require_original(factory, aggregate)?;
        // Upper redundant parent edges ended only after this whole frame and
        // canonical receiver were retained by the consuming constructor.
        if self.inner.parent.is_some() || self.inner.parent_parts.is_some() { return Ok(()); }
        let parts = self.inner.get_mut();
        unsafe { parts.join.advance_live_parent(factory, aggregate, prepay) }?;
        // Preserve every actual parent field in this whole cached upper frame
        // before generated roots or the Store backedge can advance separately.
        unsafe { parts.join.transfer_live_parent(factory, aggregate, &mut parts.parent, prepay) }
    }

    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign failed State physical frame");
        self.inner.join.require_original(factory, aggregate)
    }
    pub fn require_operational_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign failed State registration");
        if let Some(outside) = self.inner.operational.as_ref() {
            outside.require_shared_roots_registered(registry, prepay)?;
        }
        Ok(())
    }
}
impl ReleasedLiveStateStartup {
    /// Transfer the complete failed-start frame, then end its two possible
    /// redundant parent edges. Every refusal returns the original source.
    pub fn into_physical_custody(self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>)
        -> std::result::Result<ReleasedLiveStatePhysicalCustody, (Self, anyhow::Error)> {
        let checked = lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            self.inner.progress.original.require_original(factory)?;
            self.inner.destination_join.get().context("actual failed State receiver absent")?
                .require_original(factory, aggregate)?;
            let p = &self.inner.progress;
            anyhow::ensure!(p.operational.is_none() && p.operational_failure.is_none()
                && p.operational_closed.is_none() && p.projection_failure.is_none()
                && p.projection_released.is_none() && p.schema_failure.is_none()
                && p.schema_released.is_none() && p.schema_admission_custody.is_empty()
                && p.publication_custody.is_empty() && p.lease_setup_custody.is_empty()
                && self.inner.schema_pending.is_empty() && self.projection_leases_retired,
                "failed State source handoff incomplete");
            if let Some(context) = &p.context {
                anyhow::ensure!(context.retired_selected.is_empty()
                    && matches!(context.authority, OriginalProjectionStateOwner::Live(_)),
                    "failed State retains projection source or foreign parent");
                anyhow::ensure!(context.schema_references.try_lock()
                    .map_err(|_| anyhow::anyhow!("failed State schema census busy or poisoned"))?.is_empty(),
                    "failed State schema sources remain");
            }
            if let Some(outside) = &p.operational_outside {
                outside.require_shared_roots_registered(registry, prepay)?;
            }
            anyhow::ensure!(self.inner.destination_inputs.is_empty(),
                "failed State destination inputs remain outside the State join");
            require_no_destination_inputs(p.context.as_ref(), &self.inner.projection_native)?;
            require_refusals_without_native_owners(self.inner.projection_refusals.iter())?;
            let quote = quote_released_live_state_physical_custody()?;
            prepay(quote.bytes, quote.steps)?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            self.inner.destination_join.get().context("actual failed State receiver absent after funding")?
                .require_original(factory, aggregate)
        });
        if let Err(error) = checked { return Err((self, error)); }
        let ReleasedLiveStateStartupParts { schema_pending, schema_definitions,
            projection_files, projection_native, projection_refusals, destination_inputs,
            destination_join, progress } = match self.into_parts() {
            Ok(parts) => parts,
            Err(this) => return Err((this, anyhow::anyhow!("foreign failed State startup"))),
        };
        // Checked empty above.
        drop(destination_inputs);
        let StartupCloseProgress { original, context, operational, operational_failure, operational_closed,
            operational_outside, projection_failure, projection_released, schema_failure,
            schema_released, schema_admission_custody, publication_custody, lease_setup_custody,
            prior_refusal } = progress;
        debug_assert!(operational.is_none() && operational_failure.is_none() && operational_closed.is_none()
            && projection_failure.is_none() && projection_released.is_none()
            && schema_failure.is_none() && schema_released.is_none());
        // Checked empty above; only the vector allocations end here.
        drop(publication_custody);
        drop(lease_setup_custody);
        let (context, authority) = match context {
            Some(context) => {
                let OriginalProjectionBuildContext { factory, authority, terminals, durable,
                    retired_selected, schema_references, generation_leases, generation_lease_attempts,
                    issued_destinations } = context;
                // Non-owning identity records; every issued input already joined.
                drop(issued_destinations);
                (Some(RetiredStartupProjectionContext { _factory: factory, _terminals: terminals,
                    _durable: durable, _selected: retired_selected, _schema: schema_references,
                    _leases: generation_leases, _attempts: generation_lease_attempts }), Some(authority))
            }
            None => (None, None),
        };
        let frame = ReleasedLiveStatePhysicalCustody {
            inner: Held::new(
                RELEASED_LIVE_STATE_PHYSICAL_KIND,
                ReleasedLiveStatePhysicalParts {
                    projection_physical: super::live_native::LiveProjectionPhysicalCustody::empty(),
                    parent: None,
                    parent_parts: None,
                    join: destination_join.into_inner().expect("checked failed State receiver"),
                    _context: context,
                    _schema: (schema_pending, schema_definitions, schema_admission_custody),
                    _projection: (projection_files, projection_native, projection_refusals),
                    operational: operational_outside,
                    _refusal: prior_refusal,
                    metadata_pins: StateMetadataPinRetirement::empty(),
                    native_retired: None,
                },
                released_state_physical_settled,
            ),
        };
        // Canonical SAME parent is retained in the receiver before either
        // redundant upper edge ends. Its roots and host controls remain there.
        drop(authority);
        drop(original);
        Ok(frame)
    }

    /// Transfer the actual Operational native packet vector to the Store
    /// supervisor. Context, schema bodies and marker custody remain here.
    /// # Safety
    /// The same original outside controller must immediately cache the returned
    /// vector before another effect; extraction does not retire packet affinity.
    pub unsafe fn take_operational_sql_custody(&mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> anyhow::Result<Option<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.inner.destination_join.get().context("actual State destination receiver absent")?
            .require_original(factory, aggregate)?;
        anyhow::ensure!(self.inner.progress.operational.is_none()
            && self.inner.progress.operational_failure.is_none()
            && self.inner.progress.operational_closed.is_none(),
            "State retains an untransferred Operational native frame");
        match self.inner.get_mut().progress.operational_outside.as_mut() {
            Some(outside) => unsafe { outside.take_original_sql_custody() }.map(Some),
            None => Ok(None), // Actual pre-acquisition path; no retirement claim.
        }
    }

    /// # Safety
    /// The original outside supervisor retains this complete State frame and
    /// prepays transfers into the SAME unsealed Store registry. Native receipt
    /// handoff must already have completed; no new admission is issued here.
    pub unsafe fn register_operational_shared_roots(&mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.inner.destination_join.get().context("actual State destination receiver absent")?
            .require_original(factory, aggregate)?;
        anyhow::ensure!(self.inner.progress.operational.is_none()
            && self.inner.progress.operational_failure.is_none()
            && self.inner.progress.operational_closed.is_none(),
            "State retains an untransferred Operational native frame");
        // Before Operational acquisition there is no such physical owner.
        // If one was acquired, close/recovery cached its actual outside frame.
        let StartupCloseProgress { operational_outside, original: parent, .. } = &mut self.inner.get_mut().progress;
        if let Some(outside) = operational_outside.as_mut() {
            outside.normalize_live_context(factory, aggregate, parent, prepay)?;
            unsafe { outside.register_shared_roots(registry, prepay) }?;
            outside.require_shared_roots_registered(registry, prepay)?;
        }
        Ok(())
    }

    /// Transfer the actual Operational context into accessible post-ACK custody.
    /// Cache the complete outcome before further native or physical effects.
    pub fn advance_operational_outside(
        &mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.inner.destination_join.get().context("actual State destination receiver absent")?
            .require_original(factory, aggregate)?;
        let StartupCloseProgress { operational_closed, operational_outside, original: parent, .. } = &mut self.inner.get_mut().progress;

        if let Some(outside) = operational_outside {
            return outside.advance_live_native_receipt(factory, aggregate, prepay);
        }
        let Some(source) = operational_closed.take() else { return Ok(()); };
        match source.into_live_outside_custody(factory, aggregate, parent, prepay) {
            Ok(outside) => *operational_outside = Some(outside),
            Err((source, error)) => {
                *operational_closed = Some(source);
                return Err(error);
            }
        }
        operational_outside.as_mut().context("actual Operational outside custody absent")?
            .advance_live_native_receipt(factory, aggregate, prepay)
    }

    fn advance_closed_schema_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let parts = self.inner.get_mut();
        let StartupCloseProgress { original, context, schema_admission_custody, .. } = &mut parts.progress;
        let registered = match context.as_ref() {
            Some(context) => context.schema_references.try_lock().map_err(|_| anyhow::anyhow!("startup schema quote registry busy or poisoned"))?.len(),
            None => 0,
        };
        let incoming = registered.checked_add(schema_admission_custody.len()).context("startup schema incoming count overflow")?;
        let count = incoming.checked_add(parts.schema_pending.len()).context("startup schema source count overflow")?;
        if count == 0 { return Ok(()); }
        let quote = quote_startup_closed_schema_sources(
            parts.schema_pending.len(), incoming,
            parts.schema_definitions.len(), parts.projection_native.len(),
        )?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        parts.schema_pending.try_reserve_exact(incoming)?;
        parts.schema_definitions.try_reserve_exact(count)?;
        parts.projection_native.try_reserve_exact(count)?;
        if let Some(context) = context.as_mut() {
            let mut references = context.schema_references.try_lock().map_err(|_| anyhow::anyhow!("startup schema registry busy or poisoned"))?;
            anyhow::ensure!(references.len() <= registered, "startup schema registry changed after original quote");
            parts.schema_pending.append(&mut references);
        }
        while let Some((reference, parent)) = schema_admission_custody.last() {
            anyhow::ensure!(matches!(parent, OriginalProjectionStateOwner::Live(parent) if Arc::ptr_eq(parent, original)), "startup schema admission belongs to another original State");
            // The actual canonical reference is cached before its redundant
            // parent alias retires; the retained original State remains here.
            let _ = reference;
            let (reference, parent) = schema_admission_custody.pop().expect("actual cached schema admission");
            parts.schema_pending.push(reference);
            drop(parent);
        }
        for index in (0..parts.schema_pending.len()).rev() {
            if parts.schema_pending.iter().take(index).any(|reference| Arc::ptr_eq(reference, &parts.schema_pending[index])) {
                drop(parts.schema_pending.remove(index));
            }
        }
        while let Some(reference) = parts.schema_pending.pop() {
            match Arc::try_unwrap(reference) {
                Ok(reference) => {
                    let (definition, proof) = reference.into_original_parts();
                    parts.schema_definitions.push(definition);
                    parts.projection_native.push(proof);
                }
                Err(reference) => {
                    parts.schema_pending.push(reference);
                    anyhow::bail!("actual startup closed schema still has an external alias");
                }
            }
        }
        Ok(())
    }
    /// Advance the actual released schema-construction owner, preserving the
    /// original definition, optional native receipt and refusal in this State.
    /// Other closed schema/admission references remain a separate source stage.
    pub fn advance_schema_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        self.inner.destination_join.get().context("actual startup State receiver not prepared")?.require_original(factory, terminal)?;
        if let Some(operational) = &mut self.inner.get_mut().progress.operational_closed {
            operational.advance_live_schema_sources(factory, terminal, prepay)?;
        }
        self.advance_closed_schema_sources(factory, prepay)?;
        let Some(released) = self.inner.progress.schema_released.as_ref() else { return Ok(()); };
        released.terminal(factory)?;
        let quote = quote_startup_released_schema_source(
            self.inner.schema_definitions.len(), self.inner.projection_native.len(), self.inner.projection_refusals.len(),
        )?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        self.inner.get_mut().schema_definitions.try_reserve_exact(1)?;
        self.inner.get_mut().projection_native.try_reserve_exact(1)?;
        self.inner.get_mut().projection_refusals.try_reserve_exact(1)?;
        let released = self.inner.get_mut().progress.schema_released.take().expect("cached actual released schema source");
        match released.into_original_parts(factory, terminal) {
            Ok((definition, proof, error)) => {
                if let Some(definition) = definition { self.inner.get_mut().schema_definitions.push(definition); }
                if let Some(proof) = proof { self.inner.get_mut().projection_native.push(proof); }
                self.inner.get_mut().projection_refusals.push(error);
                Ok(())
            }
            Err(released) => {
                self.inner.get_mut().progress.schema_released = Some(released);
                anyhow::bail!("original released schema source transfer refused")
            }
        }
    }
    /// Move every destination input still held by this failed-start State's
    /// terminals into its destination join as a typed leaf. Runs after the
    /// projection and schema stages; a refusal keeps every taken input here.
    pub fn advance_destination_input_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let parts = self.inner.get_mut();
        let join = parts.destination_join.get().context("actual failed-start State receiver not prepared")?;
        advance_destination_input_sources(parts.progress.context.as_ref(),
            &parts.projection_native, &mut parts.destination_inputs, join, factory, terminal, prepay)
    }
    /// Consume actual recovered projection sources while the whole unpublished
    /// State stays under its original Store supervisor. Native ACK and receiver
    /// preparation precede these moves; no source refusal becomes success.
    pub fn advance_projection_sources(&mut self,
        factory: &Arc<OriginalNativeFactory>, terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let parts = self.inner.get_mut();
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        parts.destination_join.get().context("actual failed-start State receiver not prepared")?.require_original(factory, terminal)?;
        let StartupCloseProgress { context, projection_released, publication_custody,
            lease_setup_custody, .. } = &mut parts.progress;
        let retired_count = context.as_ref().map_or(0, |context| context.retired_selected.len());
        let lease_count = match context.as_ref() {
            Some(context) => context.generation_leases.try_lock().map_err(|_| anyhow::anyhow!("startup generation source busy or poisoned"))?.len(),
            None => 0,
        };
        let count = retired_count.checked_add(usize::from(projection_released.is_some()))
            .and_then(|count| count.checked_add(publication_custody.len()))
            .and_then(|count| count.checked_add(lease_setup_custody.len()))
            .context("startup projection count overflow")?;
        if count == 0 && (self.projection_leases_retired || lease_count == 0) { return Ok(()); }
        let memory = super::live_close::quote_startup_projection_source_transfer(count, lease_count, parts.projection_files.len(), parts.projection_native.len(), parts.projection_refusals.len())?.bytes;
        let work = u64::try_from(count)?.checked_mul(u64::try_from(count)?)
            .and_then(|work| work.checked_add(u64::try_from(lease_count).ok()?))
            .and_then(|work| work.checked_add(8)).context("startup projection source work overflow")?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| prepay(memory, work))?;
        parts.projection_files.try_reserve_exact(count)?;
        parts.projection_native.try_reserve_exact(count)?;
        parts.projection_refusals.try_reserve_exact(count)?;
        if let Some(context) = context.as_mut() {
            let leases = {
                let retained = context.generation_leases.try_lock().map_err(|_| anyhow::anyhow!("startup generation source busy or poisoned"))?;
                anyhow::ensure!(retained.len() <= lease_count, "startup generation source changed after original quote");
                retained.clone()
            };
            for lease in leases { lease.retire_after_native_terminal(terminal)?; }
        }
        self.projection_leases_retired = true;
        if let Some(released) = projection_released.take() {
            let (files, proof, error) = released.into_outside_custody();
            parts.projection_files.push(files);
            parts.projection_native.push(proof);
            parts.projection_refusals.push(error);
        }
        while let Some((staged, _, _, _)) = publication_custody.last() {
            // The durable proof stays canonical in the startup context; only
            // the staged filesystem (directory and lease alias) moves here.
            let proof = staged.native_proof();
            anyhow::ensure!(context.as_ref().is_some_and(|context|
                context.durable.iter().any(|durable| Arc::ptr_eq(durable, &proof))),
                "staged publication durable proof is absent from the startup context");
            drop(proof);
            let (staged, recovery, instance_id, projection_file) =
                publication_custody.pop().expect("cached staged publication custody");
            match Arc::try_unwrap(staged) {
                Ok(staged) => {
                    let (files, proof) = staged.into_outside_custody();
                    parts.projection_files.push(files);
                    drop(proof);
                    drop(recovery);
                    drop(instance_id);
                    drop(projection_file);
                }
                Err(staged) => {
                    publication_custody.push((staged, recovery, instance_id, projection_file));
                    anyhow::bail!("actual staged publication generation still has an external alias");
                }
            }
        }
        while let Some((_, _, _, owner)) = lease_setup_custody.last() {
            anyhow::ensure!(Arc::ptr_eq(owner, factory),
                "generation lease setup belongs to another original factory");
            let (sidecar, directory, name, owner) =
                lease_setup_custody.pop().expect("cached generation lease setup custody");
            parts.projection_files.push(crate::projection::OutsideOriginalProjectionCustody::lease_setup_directory(directory));
            // The aggregate native terminal was verified above, so no native
            // owner can still depend on this creation descriptor's inode.
            drop(sidecar);
            drop(name);
            drop(owner);
        }
        if let Some(context) = context.as_mut() {
            let previous = &mut context.retired_selected;
            for index in (0..previous.len()).rev() {
                if previous.iter().take(index).any(|owner| Arc::ptr_eq(owner, &previous[index])) { drop(previous.remove(index)); }
            }
            while let Some(owner) = previous.pop() {
                match Arc::try_unwrap(owner) {
                    Ok(owner) => {
                        let (files, proof) = owner.into_outside_custody();
                        parts.projection_files.push(files);
                        parts.projection_native.push(lillux_sqlite::OriginalNativeConnectionTerminal::Closed(proof));
                    }
                    Err(owner) => {
                        previous.push(owner);
                        anyhow::bail!("actual startup retired projection still has an external alias");
                    }
                }
            }
        }
        Ok(())
    }
    pub fn require_original_store_parent(&self,
        store: &Arc<dyn OriginalNativeDestinationAuthority>, factory: &Arc<OriginalNativeFactory>) -> anyhow::Result<()> {
        self.inner.progress.original.require_original_store_parent(store, factory)
    }
    /// The original failed-start State remains the canonical parent. Retain
    /// its same receiver before transferring actual destination leaf packets.
    pub fn prepare_destination_join(&self,
        factory: &Arc<OriginalNativeFactory>, terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        if let Some(join) = self.inner.destination_join.get() { return join.require_original(factory, terminal); }
        let parent = OriginalProjectionStateOwner::Live(self.inner.progress.original.clone());
        let join = parent.original_destination_join(factory, terminal, prepay)?;
        let _ = self.inner.destination_join.set(join);
        self.inner.destination_join.get().context("failed-start State receiver publication absent")?.require_original(factory, terminal)
    }
    fn generation_registry_snapshot(&self) -> anyhow::Result<Vec<Arc<crate::projection::OriginalGenerationLease>>> {
        self.inner.progress.original.factory().require_original_process()?;
        let Some(context) = &self.inner.progress.context else { return Ok(Vec::new()); };
        let leases = context.generation_leases.try_lock()
            .map_err(|_| anyhow::anyhow!("startup generation registry busy or poisoned"))?;
        Ok(leases.clone())
    }
    /// # Safety
    /// The original startup supervisor must prove no service/generation
    /// consumer was published and that all actual SQL cleanup completed.
    /// Preserve this owner on refusal; no fresh acquisition is permitted.
    pub unsafe fn generation_lease_terminals(&self, factory: &Arc<OriginalNativeFactory>)
        -> anyhow::Result<Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>> {
        self.native_terminals(factory)?;
        let mut receipts = Vec::new();
        for lease in self.generation_registry_snapshot()? {
            // SAFETY: the caller supplies actual unpublished startup quiescence;
            // genuine SQL terminal checks precede native effects outside gates.
            if let Some(receipt) = unsafe { lease.close_after_consumers(factory) }? {
                if !receipts.iter().any(|actual| Arc::ptr_eq(actual, &receipt)) { receipts.push(receipt); }
            }
        }
        Ok(receipts)
    }
    pub fn native_terminals(&self, factory: &Arc<OriginalNativeFactory>)
        -> anyhow::Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>> {
        factory.require_original_process()?;
        anyhow::ensure!(self.inner.progress.operational_outside.is_none(), "Operational source already transferred after aggregate acknowledgement");
        self.inner.progress.original.require_original(factory)?;
        let mut terminals = Vec::new();
        if let Some(closed) = &self.inner.progress.operational_closed { terminals.extend(closed.native_terminals()?); }
        if let Some(released) = &self.inner.progress.projection_released { terminals.push(released.terminal().clone()); }
        for proof in self.inner.projection_native.iter() { proof.require_original(factory)?; terminals.push(proof.clone()); }
        for reference in self.inner.schema_pending.iter() { reference.terminal().require_original(factory)?; terminals.push(reference.terminal().clone()); }
        if let Some(released) = &self.inner.progress.schema_released {
            if let Some(terminal) = released.terminal(factory)? { terminals.push(terminal.clone()); }
        }
        if let Some(context) = &self.inner.progress.context {
            anyhow::ensure!(Arc::ptr_eq(&context.factory, factory), "foreign startup projection context");
            for terminal in &context.terminals { terminals.push(lillux_sqlite::OriginalNativeConnectionTerminal::Closed(Arc::clone(terminal))); }
            for closed in &context.retired_selected {
                closed.require_original(factory)?;
                let terminal = closed.native_proof();
                if !context.terminals.iter().any(|existing| Arc::ptr_eq(existing, &terminal)) {
                    terminals.push(lillux_sqlite::OriginalNativeConnectionTerminal::Closed(terminal));
                }
            }
            let references = context.schema_references.try_lock()
                .map_err(|_| anyhow::anyhow!("startup schema receipt custody busy"))?;
            for reference in references.iter() { terminals.push(reference.terminal().clone()); }
        }
        for (reference, _) in &self.inner.progress.schema_admission_custody {
            let terminal = reference.terminal();
            let exists = terminals.iter().any(|actual| match (actual, terminal) {
                (lillux_sqlite::OriginalNativeConnectionTerminal::Closed(a), lillux_sqlite::OriginalNativeConnectionTerminal::Closed(b)) => Arc::ptr_eq(a, b),
                (lillux_sqlite::OriginalNativeConnectionTerminal::Released(a), lillux_sqlite::OriginalNativeConnectionTerminal::Released(b)) => Arc::ptr_eq(a, b),
                _ => false,
            });
            if !exists { terminals.push(terminal.clone()); }
        }
        for terminal in &terminals { terminal.require_original(factory)?; }
        Ok(terminals)
    }
    /// End a failed-start State whose sources never left it, yielding its
    /// native census for a host that settles in process: every connection
    /// terminal and every closed generation lease. The roots, recovery and
    /// trust end with this call; the terminals keep their own destination
    /// owners until the host's census ends. A State whose sources began an
    /// outside handoff, or whose census refuses, is returned whole.
    pub fn into_census(self, factory: &Arc<OriginalNativeFactory>)
        -> std::result::Result<super::LiveStateCensus, (Self, anyhow::Error)> {
        let collected = (|| -> anyhow::Result<super::LiveStateCensus> {
            factory.require_original_process()?;
            anyhow::ensure!(Arc::ptr_eq(self.inner.progress.original.factory(), factory),
                "failed-start State census selected a foreign factory");
            let progress = &self.inner.progress;
            anyhow::ensure!(self.inner.destination_join.get().is_none() && self.inner.destination_inputs.is_empty()
                && self.inner.projection_files.is_empty() && self.inner.projection_refusals.is_empty()
                && self.inner.schema_definitions.is_empty() && !self.projection_leases_retired
                && progress.operational_outside.is_none(),
                "failed-start State began an outside source handoff");
            anyhow::ensure!(progress.operational.is_none() && progress.operational_failure.is_none()
                && progress.projection_failure.is_none() && progress.schema_failure.is_none(),
                "failed-start State close is incomplete");
            let terminals = self.native_terminals(factory)?;
            // SAFETY: a failed start published no service or generation
            // consumer, and the census above proved every SQL owner closed.
            let leases = unsafe { self.generation_lease_terminals(factory) }?;
            let owners = self.generation_registry_snapshot()?;
            Ok(super::LiveStateCensus { terminals, leases,
                lease_retirement: super::GenerationLeaseRetirement::new(owners), prior_refusal: None })
        })();
        let mut census = match collected {
            Ok(census) => census,
            Err(error) => return Err((self, error)),
        };
        let ReleasedLiveStateStartupParts { schema_pending, schema_definitions, projection_files,
            projection_native, projection_refusals, destination_inputs, destination_join, progress } =
            match self.into_parts() {
                Ok(parts) => parts,
                Err(this) => return Err((this, anyhow::anyhow!("foreign failed State startup"))),
            };
        // Each vector is either checked empty above or carries terminals the
        // census now also holds.
        drop((schema_pending, schema_definitions, projection_files, projection_native,
            projection_refusals, destination_inputs, destination_join));
        let StartupCloseProgress { original, context, operational, operational_failure, operational_closed,
            operational_outside, projection_failure, projection_released, schema_failure, schema_released,
            schema_admission_custody, publication_custody, lease_setup_custody, prior_refusal } = progress;
        // Owners first, then the State parent that holds the roots and the
        // namespace exclusion.
        drop((operational, operational_failure, operational_closed, operational_outside,
            projection_failure, projection_released, schema_failure, schema_released));
        drop((schema_admission_custody, publication_custody, lease_setup_custody, context));
        drop(original);
        census.prior_refusal = prior_refusal;
        Ok(census)
    }
    pub fn prior_refusal(&self) -> Option<&anyhow::Error> { self.inner.progress.prior_refusal.as_ref() }
}
enum StartupRecoveryCustody {
    Opening(LiveStateDatabaseOpenFailure),
    Closing { factory: Arc<OriginalNativeFactory>, progress: Mutex<StartupCloseProgress> },
}
const STATE_STARTUP_RECOVERY_KIND: &str = "ryeos-state::state-startup-recovery-failure";

/// What a refused recovery owns. Constructible only in this module.
struct LiveStateStartupRecoveryParts {
    custody: StartupRecoveryCustody,
}
/// No drop settles a refused closing recovery: its progress stays retained
/// whole, with its factory alias, until the process exits. An opening failure
/// retains its own checkout through that checkout's Drop, so it releases
/// normally here.
fn state_recovery_failure_settled(parts: &LiveStateStartupRecoveryParts) -> bool {
    matches!(parts.custody, StartupRecoveryCustody::Opening(_))
}
pub struct LiveStateStartupRecoveryFailure {
    inner: Held<LiveStateStartupRecoveryParts>,
}
impl LiveStateStartupRecoveryFailure {
    fn new(custody: StartupRecoveryCustody) -> Self {
        Self {
            inner: Held::new(
                STATE_STARTUP_RECOVERY_KIND,
                LiveStateStartupRecoveryParts { custody },
                state_recovery_failure_settled,
            ),
        }
    }
    /// Move the whole custody out; a copy in another process is refused and
    /// comes back still held.
    fn into_custody(self) -> std::result::Result<StartupRecoveryCustody, Self> {
        self.inner.take_whole().map(|parts| parts.custody).map_err(|inner| Self { inner })
    }
}
impl std::fmt::Debug for LiveStateStartupRecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveStateStartupRecoveryFailure").finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveStateStartupRecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("actual State startup recovery remains pending") }
}
impl std::error::Error for LiveStateStartupRecoveryFailure {}
impl LiveStateDatabaseOpenFailure {
    pub fn close_retained(self) -> std::result::Result<ReleasedLiveStateStartup, LiveStateStartupRecoveryFailure> {
        if self.factory.require_original_process().is_err() {
            return Err(LiveStateStartupRecoveryFailure::new(StartupRecoveryCustody::Opening(self)));
        }
        let Self { error, factory: _, _custody } = self;
        let mut custody = _custody.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
        // The custody ends below with both service owners moved out, so its
        // own retention runs no abandonment; this owner continues the close.
        let original = Arc::clone(&custody.original);
        let mut progress = StartupCloseProgress { original, context: custody.projection_context.take(),
            operational: custody.operational.take(), operational_failure: None, operational_closed: None, operational_outside: None,
            projection_failure: None, projection_released: None, schema_failure: None,
            schema_released: None, schema_admission_custody: Vec::new(), publication_custody: Vec::new(),
            lease_setup_custody: Vec::new(), prior_refusal: None };
        // Peel every owner-bearing wrapper into its typed custody slot; only
        // the innermost refusal may remain as a plain error.
        let mut error = error;
        let error = loop {
            error = match error.downcast::<OriginalProjectionSchemaAdmissionFailure>() {
                Ok(OriginalProjectionSchemaAdmissionFailure { error: inner, _reference, _owner }) => {
                    progress.schema_admission_custody.push((_reference, _owner));
                    inner
                }
                Err(error) => match error.downcast::<GeneratedProjectionPublicationFailure>() {
                    Ok(GeneratedProjectionPublicationFailure { error: inner, _staged, _recovery,
                        _instance_id, _projection_file }) => {
                        progress.publication_custody.push((_staged, _recovery, _instance_id, _projection_file));
                        inner
                    }
                    Err(error) => match error.downcast::<OriginalProjectionLeaseSetupFailure>() {
                        Ok(OriginalProjectionLeaseSetupFailure { error: inner, _lease, _directory,
                            _name, _factory }) => {
                            progress.lease_setup_custody.push((_lease, _directory, _name, _factory));
                            inner
                        }
                        Err(error) => break error,
                    },
                },
            };
        };
        match error.downcast::<crate::projection::OriginalProjectionFailure>() {
            Ok(failure) => progress.projection_failure = Some(failure),
            Err(error) => match error.downcast::<crate::sqlite_schema::NativeSchemaSqlReferenceFailure>() {
                Ok(failure) => progress.schema_failure = Some(failure),
                Err(error) => progress.prior_refusal = Some(error),
            },
        }
        advance_startup_close(progress)
    }
}
impl LiveStateStartupRecoveryFailure {
    pub fn retry(self) -> std::result::Result<ReleasedLiveStateStartup, Self> {
        let custody = match self.into_custody() {
            Ok(custody) => custody,
            Err(this) => return Err(this),
        };
        match custody {
            StartupRecoveryCustody::Opening(failure) => failure.close_retained(),
            StartupRecoveryCustody::Closing { factory, progress } => {
                if factory.require_original_process().is_err() {
                    return Err(Self::new(StartupRecoveryCustody::Closing { factory, progress }));
                }
                advance_startup_close(progress.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner))
            }
        }
    }
}
/// Kind under which a startup close progress unwound mid-step is retained.
const STARTUP_CLOSE_UNWIND: &str = "ryeos-state::state-startup-close-unwind";
fn advance_startup_close(progress: StartupCloseProgress)
    -> std::result::Result<ReleasedLiveStateStartup, LiveStateStartupRecoveryFailure> {
    // Unwind cannot implicitly drop active SQL or generation custody: the
    // progress is held from here on, so a panicking step retains this one
    // actual checkout whole. Normal success/refusal explicitly transfers it.
    let factory = Arc::clone(progress.original.factory());
    if factory.require_original_process().is_err() {
        // Refused before any step: the progress goes back unchanged, still
        // owned by the recovery failure, in this process or a copied one.
        return Err(LiveStateStartupRecoveryFailure::new(StartupRecoveryCustody::Closing {
            factory: Arc::clone(&factory), progress: Mutex::new(progress) }));
    }
    let mut progress = lillux::custody::Held::until_taken(STARTUP_CLOSE_UNWIND, progress);
    let retain = |progress: lillux::custody::Held<StartupCloseProgress>| {
        // The progress was held only after the original process was confirmed,
        // so a foreign copy here means a forked child resumed this frame; it
        // is relinquished and the process aborts.
        let progress = progress.take_or_abort();
        LiveStateStartupRecoveryFailure::new(StartupRecoveryCustody::Closing {
            factory: Arc::clone(progress.original.factory()), progress: Mutex::new(progress) })
    };
    let validated = (|| -> anyhow::Result<_> {
        factory.require_original_process()?;
        progress.original.require_original(&factory)?;
        let guard = crate::recovery::CasMutationGuard::acquire_existing_exclusive_in_pinned_runtime(progress.original.runtime())?;
        progress.original._recovery.ensure_guard(&guard)?;
        Ok(guard)
    })();
    let guard = match validated { Ok(guard) => guard, Err(_) => return Err(retain(progress)) };
    if let Some(operational) = progress.get_mut().operational.take() {
        match operational.close_live(Arc::clone(&factory)) {
            Ok(closed) => progress.get_mut().operational_closed = Some(closed),
            Err(failure) => progress.get_mut().operational_failure = Some(failure),
        }
    } else if let Some(failure) = progress.get_mut().operational_failure.take() {
        match failure.retry_close() {
            Ok(closed) => progress.get_mut().operational_closed = Some(closed),
            Err(failure) => progress.get_mut().operational_failure = Some(failure),
        }
    }
    if progress.operational_failure.is_some() { return Err(retain(progress)); }
    if let Some(failure) = progress.get_mut().projection_failure.take() {
        match failure.retry_close(&factory) {
            Ok(released) => progress.get_mut().projection_released = Some(released),
            Err(failure) => { progress.get_mut().projection_failure = Some(failure); return Err(retain(progress)); }
        }
    }
    if let Some(failure) = progress.get_mut().schema_failure.take() {
        match failure.close_retained() {
            Ok(released) => progress.get_mut().schema_released = Some(released),
            Err(failure) => { progress.get_mut().schema_failure = Some(failure); return Err(retain(progress)); }
        }
    }
    drop(guard);
    let progress = match progress.take_whole() {
        Ok(progress) => progress,
        Err(foreign_copy) => return Err(retain(foreign_copy)),
    };
    let released = ReleasedLiveStateStartup::new(progress);
    if released.native_terminals(&factory).is_err() {
        // Only the progress carries owners; the other slots are still empty.
        // The startup was just issued in the original process, which this
        // frame confirmed at its start.
        let progress = lillux::custody::Held::until_taken(
            STARTUP_CLOSE_UNWIND, released.into_parts_or_abort().progress);
        return Err(retain(progress));
    }
    Ok(released)
}

impl lillux_sqlite::OriginalNativeProjectionLeaseAuthority for LiveStateNativeOwner {
    fn require_original(&self, factory: &Arc<OriginalNativeFactory>, cleanup: bool) -> anyhow::Result<()> {
        factory.require_original_process()?;
        OriginalNativeDestinationAuthority::require_original(self, factory)?;
        self.host.lease_authority.require_original(factory, cleanup)
    }
    fn fork_deadline(&self, factory: &Arc<OriginalNativeFactory>, cleanup: bool)
        -> anyhow::Result<lillux::time::MonotonicDeadline> {
        lillux_sqlite::OriginalNativeProjectionLeaseAuthority::require_original(self, factory, cleanup)?;
        self.host.lease_authority.fork_deadline(factory, cleanup)
    }
}

#[cfg(test)]
mod quote_tests {
    use super::{
        LiveProjectionPhysicalSlot, directory_wrapper_storage_bound,
        maximum_live_projection_physical_sources, quote_live_projection_physical,
        quote_live_projection_retired_sources, quote_live_projection_validation,
        LiveStatePhysicalParent, ReleasedLiveStatePhysicalCustody, quote_live_projection_retry,
        quote_live_state_destination_join, quote_live_state_physical_parent,
        quote_live_state_root_registration_handoff, quote_released_live_state_physical_custody,
        quote_startup_closed_schema_sources, quote_startup_released_schema_source,
    };
    use std::mem::size_of;
    use std::sync::Arc;

    #[test]
    fn live_projection_quote_covers_slots_wrappers_and_registration_work() {
        let quote = quote_live_projection_physical(16, 2, 1).unwrap();
        assert_eq!(quote.bytes, (3 * size_of::<LiveProjectionPhysicalSlot>()) as u64
            + directory_wrapper_storage_bound(6).unwrap());
        assert_eq!(quote.steps, 4 * 4 * 8 + 4 * (3 * 4) + 4);
        assert_eq!(quote.steps, 180);
        assert_eq!(quote_live_projection_validation(3).unwrap().steps, 100);
        assert_eq!(quote_live_projection_retired_sources(3, 6).unwrap().steps, 88);
        assert!(quote_live_projection_retired_sources(3, 7).is_err());
        assert!(quote_live_projection_validation(0).is_err());
        assert!(quote_live_projection_physical(5, 2, 2).is_err());
        assert!(quote_live_projection_physical(usize::MAX, usize::MAX, 1).is_err());
    }

    #[test]
    fn live_projection_physical_source_bound_uses_ticket_owned_maxima() {
        assert_eq!(maximum_live_projection_physical_sources(64).unwrap(), 192);
        assert!(maximum_live_projection_physical_sources(0).is_err());
    }

    #[test]
    fn live_state_owner_quotes_cover_transfers_and_failed_start_frames() {
        use lillux_sqlite::original_physical_custody::OutsideWork;

        assert_eq!(quote_live_state_root_registration_handoff(), OutsideWork { bytes: 0, steps: 2 });
        assert_eq!(quote_live_state_destination_join(), OutsideWork { bytes: 0, steps: 2 });
        assert_eq!(quote_live_projection_retry(19), OutsideWork { bytes: 0, steps: 19 });
        assert_eq!(quote_live_state_physical_parent().unwrap(), OutsideWork {
            bytes: LiveStatePhysicalParent::storage_bound().unwrap(), steps: 8,
        });
        assert_eq!(quote_released_live_state_physical_custody().unwrap(), OutsideWork {
            bytes: size_of::<ReleasedLiveStatePhysicalCustody>() as u64, steps: 32,
        });

        let closed = quote_startup_closed_schema_sources(2, 3, 4, 6).unwrap();
        let count = 5usize;
        let bytes = count * size_of::<Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>>()
            + (4 + count) * size_of::<crate::sqlite_schema::SchemaSqlDefinition>()
            + (6 + count) * size_of::<lillux_sqlite::OriginalNativeConnectionTerminal>()
            + 3 * size_of::<Vec<()>>();
        assert_eq!(closed, OutsideWork { bytes: bytes as u64, steps: 29 });
        assert!(quote_startup_closed_schema_sources(usize::MAX, 1, 0, 0).is_err());

        let released = quote_startup_released_schema_source(2, 3, 4).unwrap();
        let bytes = 3 * size_of::<crate::sqlite_schema::SchemaSqlDefinition>()
            + 4 * size_of::<lillux_sqlite::OriginalNativeConnectionTerminal>()
            + 5 * size_of::<anyhow::Error>()
            + 3 * size_of::<Vec<()>>();
        assert_eq!(released, OutsideWork { bytes: bytes as u64, steps: 4 });
    }
}

// Final post-release exhaustion. Each check reads only cached receipts, copied
// identity and Weak witnesses; no factory, terminal or native state is read.
impl LiveProjectionPhysicalCustody {
    pub(super) fn require_exhausted(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id()
            && self.retired_sources_consumed && self.slots.is_empty(),
            "live projection frame retains a source slot at final exhaustion");
        Ok(())
    }
}

impl LiveRecoveryPhysicalRegistrations {
    pub(super) fn require_exhausted(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> anyhow::Result<()> {
        self.require_exhausted_holders()?;
        if let Some(identity) = self.registry_identity.as_ref() {
            registry.require_same_registry(identity)?;
        }
        Ok(())
    }
    /// The registry-free part of final exhaustion.
    pub(super) fn require_exhausted_holders(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id()
            && self.retired_sources_consumed
            && self.bindings.iter().all(Option::is_none)
            && self.owners.iter().all(Option::is_none),
            "live recovery registrations remain at final exhaustion");
        Ok(())
    }
}

impl StateMetadataPinRetirement {
    pub(super) fn require_exhausted(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id() && self.leaves.is_empty(),
            "State metadata-pin census retains a leaf outside its receipt");
        Ok(())
    }
}

impl LiveStatePhysicalParentParts {
    pub(crate) fn require_exhausted(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> anyhow::Result<()> {
        self.require_exhausted_holders()?;
        require_retired_root_receipt_registry(self.registry_identity.as_ref(), registry)?;
        self.recovery_registrations.require_exhausted(registry)
    }
    /// The registry-free part of final exhaustion. The dropped parts settle
    /// on exactly these checks.
    pub(crate) fn require_exhausted_holders(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign State parent parts at exhaustion");
        anyhow::ensure!(self.retired_root_sources_consumed && self.recovery_owner_consumed
            && self._recovery.is_none()
            && self.registrations.iter().all(Option::is_none)
            && self.runtime.is_none() && self.namespace.is_none()
            && self.roots.iter().all(Option::is_none)
            && self._host.is_none() && self.retired_host.is_some(),
            "State parent parts retain a root, registration, host or recovery owner");
        self.recovery_registrations.require_exhausted_holders()
    }

}
impl LiveStatePhysicalParent {
    fn from_parts(parts: LiveStatePhysicalParentParts) -> Self {
        Self { inner: Held::new(LIVE_STATE_PHYSICAL_PARENT_KIND, parts, live_state_physical_parent_exhausted) }
    }
    pub(crate) fn from_original(original: LiveStateNativeOwner) -> Self {
        Self::from_parts(LiveStatePhysicalParentParts::from_original(original))
    }
    #[cfg(test)]
    pub(crate) fn exhausted_for_test() -> Self {
        Self::from_parts(LiveStatePhysicalParentParts::exhausted_for_test())
    }
    pub(crate) fn storage_bound() -> anyhow::Result<u64> {
        u64::try_from(std::mem::size_of::<Self>())?
            .checked_add(directory_wrapper_storage_bound(4)?)
            .context("live State parent storage quote overflow")
    }
    pub(crate) fn retire_host_aliases(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        store: &Arc<dyn OriginalNativeDestinationAuthority>,
        lease: &Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign live State host alias transfer");
        self.inner.get_mut().retire_host_aliases(factory, aggregate, store, lease, registry, prepay)
    }
    pub(crate) fn require_completed_outside_parent(&self,
        join: &Arc<super::destination_join::OriginalStateDestinationJoin>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign live State parent completion");
        self.inner.require_completed_outside_parent(join, registry)
    }
    /// # Safety
    /// This whole parent remains cached in the same prepaid original outside
    /// frame. Registration never closes sources or ends host/Store authority.
    pub(crate) unsafe fn register_roots(&mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign live State physical parent");
        unsafe { self.inner.get_mut().register_roots(registry, prepay) }
    }
    pub(crate) fn consume_retired_root_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign retired State parent roots");
        self.inner.get_mut().consume_retired_root_sources(registry, prepay)
    }
    pub(crate) fn require_registered_with_prepay(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign State parent registration census");
        self.inner.require_registered_with_prepay(registry, prepay)
    }
    pub(crate) fn require_exhausted(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign State parent parts at exhaustion");
        self.inner.require_exhausted(registry)
    }
    pub(crate) fn require_exhausted_holders(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign State parent parts at exhaustion");
        self.inner.require_exhausted_holders()
    }
}

/// Refuse while any retained refusal chain still carries an owner-bearing
/// failure outside typed custody.
pub fn require_exhausted_refusals<'a>(
    refusals: impl IntoIterator<Item = &'a anyhow::Error>,
) -> anyhow::Result<()> {
    require_refusals_without_native_owners(refusals)
}

impl ReleasedLiveStatePhysicalCustody {
    /// Consume the whole failed-start State frame after its native-source,
    /// projection-root and root receipt stages. Refusal returns the frame
    /// intact; success leaves no owner.
    pub fn into_exhausted(
        self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> std::result::Result<(), (Self, anyhow::Error)> {
        let checked = (|| -> anyhow::Result<()> {
            self.require_exhausted_holders()?;
            self.inner.native_retired.as_ref()
                .context("failed State native-source receipt absent at exhaustion")?
                .require_retired(prepay)?;
            self.inner.parent_parts.as_ref()
                .context("failed State parent parts absent at exhaustion")?
                .require_exhausted(registry)?;
            self.inner.join.require_exhausted(registry)?;
            if let Some(operational) = self.inner.operational.as_ref() {
                operational.require_exhausted(registry)?;
            }
            Ok(())
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }
        // The checks above include this frame's settled predicate, so its
        // Drop releases every slot normally.
        drop(self);
        Ok(())
    }

    /// The registry- and funding-free part of final exhaustion. The dropped
    /// frame settles on exactly these checks.
    fn require_exhausted_holders(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.inner.is_foreign(), "foreign failed State frame at exhaustion");
        self.inner.require_exhausted_holders()
    }
}
#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const PARENT: &str = "ryeos-state::live-state-physical-parent";
    const PHYSICAL: &str = "ryeos-state::released-live-state-physical-custody";
    const STARTUP: &str = "ryeos-state::released-live-state-startup";
    const RECOVERY: &str = "ryeos-state::state-startup-recovery-failure";

    fn abandoned(kind: &str) -> usize {
        lillux::custody::abandoned_custody().count(kind)
    }

    /// Stands in for a startup close progress: records whether its
    /// destructor ran.
    struct ObservedProgress(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for ObservedProgress {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    #[test]
    fn held_close_progress_handed_on_is_not_retained() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let before = abandoned(super::STARTUP_CLOSE_UNWIND);
        let held = lillux::custody::Held::until_taken(super::STARTUP_CLOSE_UNWIND, ObservedProgress(Arc::clone(&dropped)));
        let progress = match held.take_whole() {
            Ok(progress) => progress,
            Err(_) => panic!("the issuing process takes its own close progress"),
        };
        drop(progress);
        assert!(dropped.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(abandoned(super::STARTUP_CLOSE_UNWIND), before);
    }

    #[test]
    fn held_close_progress_unwound_mid_step_is_retained_and_counted() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let before = abandoned(super::STARTUP_CLOSE_UNWIND);
        let observed = Arc::clone(&dropped);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = lillux::custody::Held::until_taken(super::STARTUP_CLOSE_UNWIND, ObservedProgress(observed));
            panic!("unwind through the held close step");
        }));
        assert!(unwound.is_err());
        assert!(!dropped.load(std::sync::atomic::Ordering::Acquire), "the unwound progress was never dropped");
        assert_eq!(abandoned(super::STARTUP_CLOSE_UNWIND), before + 1);
    }

    #[test]
    fn dropped_exhausted_parent_parts_release_normally() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let parts = LiveStatePhysicalParent::exhausted_for_test();
        parts.require_exhausted_holders().unwrap();
        let trust = Arc::clone(&parts.inner._trust);
        let before = abandoned(PARENT);
        drop(parts);
        assert_eq!(abandoned(PARENT), before);
        assert_eq!(Arc::strong_count(&trust), 1, "the trust alias was released");
    }

    #[test]
    fn dropped_parent_parts_with_unconsumed_roots_are_retained() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let mut parts = LiveStatePhysicalParent::exhausted_for_test();
        parts.inner.get_mut().retired_root_sources_consumed = false;
        assert!(parts.require_exhausted_holders().is_err());
        let trust = Arc::clone(&parts.inner._trust);
        let before = abandoned(PARENT);
        drop(parts);
        assert_eq!(abandoned(PARENT), before + 1);
        assert_eq!(Arc::strong_count(&trust), 2, "the trust alias stays retained");
    }

    fn physical_custody(native_retired: Option<StateNativeSourcesRetired>) -> ReleasedLiveStatePhysicalCustody {
        ReleasedLiveStatePhysicalCustody {
            inner: Held::new(
                RELEASED_LIVE_STATE_PHYSICAL_KIND,
                ReleasedLiveStatePhysicalParts {
                    projection_physical: LiveProjectionPhysicalCustody::exhausted_for_test(),
                    parent: None,
                    parent_parts: Some(LiveStatePhysicalParent::exhausted_for_test()),
                    join: super::super::destination_join::OriginalStateDestinationJoin::exhausted_for_test(),
                    _context: None,
                    _schema: (Vec::new(), Vec::new(), Vec::new()),
                    _projection: (Vec::new(), Vec::new(), Vec::new()),
                    operational: None,
                    _refusal: None,
                    metadata_pins: StateMetadataPinRetirement::empty(),
                    native_retired,
                },
                released_state_physical_settled,
            ),
        }
    }

    #[test]
    fn dropped_exhausted_physical_custody_releases_normally() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let frame = physical_custody(Some(StateNativeSourcesRetired::retired_for_test()));
        frame.require_exhausted_holders().unwrap();
        let before = (abandoned(PHYSICAL), abandoned(PARENT));
        drop(frame);
        assert_eq!((abandoned(PHYSICAL), abandoned(PARENT)), before);
    }

    #[test]
    fn dropped_physical_custody_before_native_retirement_is_retained() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let frame = physical_custody(None);
        assert!(frame.require_exhausted_holders().is_err());
        let before = (abandoned(PHYSICAL), abandoned(PARENT));
        drop(frame);
        // The frame is held whole; its exhausted parent parts inside it are
        // never dropped, so they record nothing of their own.
        assert_eq!((abandoned(PHYSICAL), abandoned(PARENT)), (before.0 + 1, before.1));
    }

    #[test]
    fn dropped_recovery_failure_while_closing_is_retained_whole() {
        let _serial = crate::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_recovery_failure_while_closing_is_retained_whole");
        // The retained progress keeps the live State parent and its namespace
        // exclusion, which only the process end releases, so the scenario
        // runs in its own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "recovery +1; parent retained", "{outcome}");
            return;
        }
        let host = crate::test_support::shared_test_host();
        let runtime = tempfile::tempdir().unwrap();
        let state = crate::test_support::open_live_state(&host, runtime.path(), Arc::new(TrustStore::new())).unwrap();
        let original = state.live_native_owner_for_test();
        let parent = Arc::downgrade(&original);
        let failure = LiveStateStartupRecoveryFailure::new(StartupRecoveryCustody::Closing {
            factory: Arc::clone(original.factory()),
            progress: Mutex::new(StartupCloseProgress {
                original, context: None, operational: None, operational_failure: None,
                operational_closed: None, operational_outside: None, projection_failure: None,
                projection_released: None, schema_failure: None, schema_released: None,
                schema_admission_custody: Vec::new(), publication_custody: Vec::new(),
                lease_setup_custody: Vec::new(), prior_refusal: None,
            }),
        });
        let before = abandoned(RECOVERY);
        drop(failure);
        let delta = abandoned(RECOVERY) - before;
        let retained = if parent.upgrade().is_some() { "parent retained" } else { "parent released" };
        lillux_sqlite::test_host::end_isolated_process(
            (state, runtime, host),
            &format!("recovery +{delta}; {retained}"),
        )
    }

    #[test]
    fn dropped_released_startup_is_retained_with_its_state_parent() {
        let _serial = crate::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_released_startup_is_retained_with_its_state_parent");
        // The retained frame keeps the live State parent and its namespace
        // exclusion, which only the process end releases, so the scenario
        // runs in its own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "startup +1; parent retained", "{outcome}");
            return;
        }
        let host = crate::test_support::shared_test_host();
        let runtime = tempfile::tempdir().unwrap();
        let state = crate::test_support::open_live_state(&host, runtime.path(), Arc::new(TrustStore::new())).unwrap();
        let original = match &state.original_projection_context.as_ref().expect("live context").authority {
            OriginalProjectionStateOwner::Live(owner) => Arc::clone(owner),
            OriginalProjectionStateOwner::Offline(_) => panic!("a live State has a live parent"),
        };
        let parent = Arc::downgrade(&original);
        let aliases = Arc::strong_count(&original);
        let released = ReleasedLiveStateStartup::new(StartupCloseProgress {
            original, context: None, operational: None, operational_failure: None,
            operational_closed: None, operational_outside: None, projection_failure: None,
            projection_released: None, schema_failure: None, schema_released: None,
            schema_admission_custody: Vec::new(), publication_custody: Vec::new(),
            lease_setup_custody: Vec::new(), prior_refusal: None,
        });
        let before = abandoned(STARTUP);
        drop(released);
        let delta = abandoned(STARTUP) - before;
        // The upgrade adds one alias of its own.
        let retained = parent.upgrade().is_some_and(|owner| Arc::strong_count(&owner) == aliases + 1);
        lillux_sqlite::test_host::end_isolated_process(
            (state, runtime, host),
            &format!("startup +{delta}; parent {}", if retained { "retained" } else { "released" }),
        )
    }
}

#[cfg(test)]
mod recovery_owner_tests {
    use super::{LiveRecoveryPhysicalRegistrations, retire_parent_recovery_owner};
    use crate::recovery::RecoveryStore;
    use lillux_sqlite::{
        OriginalOutsideFinalizerFileRetirementAuthority,
        OriginalOutsideFinalizerFileRetirementInputs, RetainedOriginalFinalizerFile,
    };
    use lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry;
    use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

    struct FixtureRetirementAuthority {
        owner: Arc<RetainedOriginalFinalizerFile>,
        remaining: AtomicU64,
    }

    unsafe impl OriginalOutsideFinalizerFileRetirementAuthority for FixtureRetirementAuthority {
        fn require_original(&self, owner: &Arc<RetainedOriginalFinalizerFile>) -> anyhow::Result<()> {
            anyhow::ensure!(Arc::ptr_eq(owner, &self.owner), "fixture selected a foreign owner");
            Ok(())
        }

        fn debit_original_steps(&self, steps: u64) -> anyhow::Result<()> {
            self.remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining|
                remaining.checked_sub(steps))
                .map(|_| ())
                .map_err(|_| anyhow::anyhow!("fixture retirement authority exhausted"))
        }
    }

    // A State parent's RecoveryStore alias held past root registration keeps
    // the runtime-state root shared, so descriptor preparation refuses. The
    // parent must end that alias in its live stage, before preparation.
    #[test]
    fn parent_recovery_alias_must_end_before_descriptor_preparation() {
        let temporary = tempfile::tempdir().unwrap();
        let recovery = RecoveryStore::from_runtime_state_dir(temporary.path()).unwrap();
        let mut recovery = Some(recovery);
        let mut consumed = false;
        let mut registrations = LiveRecoveryPhysicalRegistrations::empty();
        let mut registry = unsafe { OriginalPhysicalRegistry::issued(2, &|_, _| Ok(())) }.unwrap();
        unsafe { registrations.register(recovery.as_ref().unwrap(), &mut registry, &|_, _| Ok(())) }.unwrap();
        unsafe { registry.seal_inventory() }.unwrap();

        let refused = unsafe { registry.prepare_descriptor_owners(&|_, _| Ok(())) }
            .expect_err("a retained parent RecoveryStore alias must keep the root shared");
        assert!(refused.to_string().contains("still shared outside complete inventory"), "{refused}");

        assert!(retire_parent_recovery_owner(&mut recovery, &mut consumed, &registrations,
            &registry, &|_, _| anyhow::bail!("funding unavailable")).is_err());
        assert!(!consumed, "a funding refusal must keep the alias for retry");

        retire_parent_recovery_owner(&mut recovery, &mut consumed, &registrations,
            &registry, &|_, _| Ok(())).unwrap();
        assert!(consumed);
        retire_parent_recovery_owner(&mut recovery, &mut consumed, &registrations,
            &registry, &|_, _| Ok(())).unwrap();

        unsafe { registry.prepare_descriptor_owners(&|_, _| Ok(())) }.unwrap();
        unsafe {
            registry.retire_non_operator_descriptors(
                &|_, _| Ok(()),
                &|_, owner| {
                    let authority = Arc::new(FixtureRetirementAuthority {
                        owner: Arc::clone(&owner),
                        remaining: AtomicU64::new(64),
                    });
                    let inputs = unsafe {
                        OriginalOutsideFinalizerFileRetirementInputs::issued(owner, authority)?
                    };
                    Ok(Arc::new(inputs))
                },
            ).unwrap();
        }
        let retired = match registry.retire() {
            Ok(retired) => retired,
            Err(_) => panic!("fully retired fixture registry must produce a receipt"),
        };
        registrations.consume_retired_sources(&retired, &|_, _| Ok(())).unwrap();
        registrations.require_exhausted(&retired).unwrap();
    }
}
