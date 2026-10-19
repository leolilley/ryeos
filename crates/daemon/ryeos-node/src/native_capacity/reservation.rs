//! The sole conversion from genuine durable local debit to shared native
//! counters. No arena, SQLite open or factory producer is issued by this cut.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, ensure};
use lillux::custody::Held;
use lillux_sqlite::{OriginalNativeBudget, OriginalNativeBudgetOwner, OriginalNativeFactory};

use lillux_sqlite::original_host_owner::backing::HostStorageReleased;
use lillux_sqlite::original_host_owner::{
    OriginalNativeRegionHandle, OriginalNativeRegionOwner, OriginalNativeRegionReleaseExclusion,
    OriginalNativeRegionSealFailure,
};

use super::{LiveNativeCapacityInputs, account::RetainedCapacityAccount, late::LateNativeControl};

/// Upper bound on waiting for a concurrent account check; see
/// `LiveNativeCapacityReservation::account_for_check`.
const ACCOUNT_CHECK_CONTENTION_WAIT: lillux::time::Duration =
    lillux::time::Duration::from_millis(250);

// Fixed source maxima below mirror the selected normal/failed-start finalizer
// owner graph. Dynamic State maxima and Runtime/Operational/Scheduler packet
// maxima come from their owning crates; destination and inspection root counts
// come from the actual lower owners. Keep this breakdown next to the quote so a
// change to the finalizer graph cannot silently preserve an unexplained base.
const NORMAL_STATE_PHYSICAL_ROOTS: usize = 6; // NormalStatePhysicalRoots: five directories + namespace.
const LIVE_STATE_PARENT_PHYSICAL_ROOTS: usize = 6; // LiveStatePhysicalParent: runtime + four roots + namespace.
const LIVE_STATE_RECOVERY_PHYSICAL_ROOTS: usize = 2; // LiveRecoveryPhysicalRegistrations fixed owner array.
const OPERATIONAL_PHYSICAL_ROOTS: usize = 3; // OutsideOriginalOperationalCustody root + namespace + marker control record.
const RUNTIME_HOST_PHYSICAL_ROOTS: usize = 2; // OriginalRuntimeOutsideHostContext fixed registrations.
const RUNTIME_FINALIZER_TAIL_PHYSICAL_ROOTS: usize = 2; // outside-finalizer namespace + refused unused bound root.
const STORE_PARENT_AND_OPERATOR_PHYSICAL_ROOTS: usize = 4; // two directories + namespace + operator lock.
const RETAINED_CAPACITY_CONTROL_SOURCES: usize = 12;
const RETAINED_STORE_METADATA_SOURCES: usize = 7;
const RETAINED_STORE_PARENT_SOURCES: usize = 4;
// Direct unopened Scheduler root plus the failed-admission nested root.
const RETAINED_STORE_SCHEDULER_SOURCES: usize = 2;
const FINALIZER_CONTROL_SOURCE_RECEIPT_STEPS: u64 = 8;
const FINALIZER_CONTROL_SOURCE_PHASE_STEPS: u64 = 16;

#[derive(Clone, Copy)]
struct PhysicalRegistrationOwnerBound {
    physical_slots: usize,
    attempts_per_pass: u64,
}

#[derive(Clone, Copy)]
struct LivePhysicalRegistrationBounds {
    normal_state: PhysicalRegistrationOwnerBound,
    live_state_parent: PhysicalRegistrationOwnerBound,
    live_state_recovery: PhysicalRegistrationOwnerBound,
    live_state_projection: PhysicalRegistrationOwnerBound,
    generated_state_roots: PhysicalRegistrationOwnerBound,
    operational: PhysicalRegistrationOwnerBound,
    runtime_host: PhysicalRegistrationOwnerBound,
    runtime_finalizer_tail: PhysicalRegistrationOwnerBound,
    store_parent_and_operator: PhysicalRegistrationOwnerBound,
    runtime_operational_packets: PhysicalRegistrationOwnerBound,
    scheduler_packets: PhysicalRegistrationOwnerBound,
    inspection_groups: PhysicalRegistrationOwnerBound,
    capacity_control_tail: PhysicalRegistrationOwnerBound,
    store_metadata: PhysicalRegistrationOwnerBound,
}

impl LivePhysicalRegistrationBounds {
    fn source_slots(self) -> Result<usize> {
        [
            self.normal_state.physical_slots,
            self.live_state_parent.physical_slots,
            self.live_state_recovery.physical_slots,
            self.live_state_projection.physical_slots,
            self.generated_state_roots.physical_slots,
            self.operational.physical_slots,
            self.runtime_host.physical_slots,
            self.runtime_finalizer_tail.physical_slots,
            self.store_parent_and_operator.physical_slots,
            self.runtime_operational_packets.physical_slots,
            self.scheduler_packets.physical_slots,
            self.inspection_groups.physical_slots,
            self.capacity_control_tail.physical_slots,
            self.store_metadata.physical_slots,
        ]
        .into_iter()
        .try_fold(0usize, |total, count| {
            total
                .checked_add(count)
                .context("physical source-slot bound overflow")
        })
    }

    fn registration_attempts_per_pass(self) -> Result<u64> {
        [
            self.normal_state.attempts_per_pass,
            self.live_state_parent.attempts_per_pass,
            self.live_state_recovery.attempts_per_pass,
            self.live_state_projection.attempts_per_pass,
            self.generated_state_roots.attempts_per_pass,
            self.operational.attempts_per_pass,
            self.runtime_host.attempts_per_pass,
            self.runtime_finalizer_tail.attempts_per_pass,
            self.store_parent_and_operator.attempts_per_pass,
            self.runtime_operational_packets.attempts_per_pass,
            self.scheduler_packets.attempts_per_pass,
            self.inspection_groups.attempts_per_pass,
            self.capacity_control_tail.attempts_per_pass,
            self.store_metadata.attempts_per_pass,
        ]
        .into_iter()
        .try_fold(0u64, |total, count| {
            total
                .checked_add(count)
                .context("physical registration-attempt bound overflow")
        })
    }
}

fn owner_registration_bound(
    physical_slots: usize,
    attempts_per_pass: usize,
) -> Result<PhysicalRegistrationOwnerBound> {
    Ok(PhysicalRegistrationOwnerBound {
        physical_slots,
        attempts_per_pass: u64::try_from(attempts_per_pass)?,
    })
}

fn live_physical_registration_bounds(tickets: u32) -> Result<LivePhysicalRegistrationBounds> {
    ensure!(tickets > 0, "original physical source quote has no tickets");
    let state_projection =
        ryeos_state::state_db::maximum_live_projection_physical_sources(tickets)?;
    let generated_state_roots = ryeos_state::state_db::maximum_state_destination_leaves(tickets)?;

    let runtime_packets = ryeos_app::runtime_db::MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES;
    let operational_packets = ryeos_state::operational::MAX_LIVE_OPERATIONAL_PACKET_SOURCES;
    // The Store's direct auxiliary collection: Scheduler and accounting ledger.
    let scheduler_packets = ryeos_app::state_store::MAX_LIVE_AUXILIARY_NATIVE_PACKET_SOURCES;
    let packet_roots_per_source = 1usize
        .checked_add(lillux_sqlite::MAX_ORIGINAL_DESTINATION_PHYSICAL_SOURCES)
        .context("per-packet physical root bound overflow")?;
    let runtime_operational_packet_count = runtime_packets
        .checked_add(operational_packets)
        .context("Runtime/Operational packet source bound overflow")?;
    let selected_packet_count = runtime_operational_packet_count
        .checked_add(scheduler_packets)
        .context("selected packet source bound overflow")?;
    ensure!(
        selected_packet_count == ryeos_app::state_store::MAX_LIVE_SELECTED_NATIVE_PACKET_SOURCES,
        "packet owner caps do not reconcile with the Store finalizer bound"
    );
    let runtime_operational_packet_roots = runtime_operational_packet_count
        .checked_mul(packet_roots_per_source)
        .context("Runtime/Operational packet physical root bound overflow")?;
    let scheduler_packet_roots = scheduler_packets
        .checked_mul(packet_roots_per_source)
        .context("Scheduler packet physical root bound overflow")?;
    // Each selected packet has at most one inspection/released-inspection
    // owner. Count all three producer classes, including Scheduler, so this
    // bound does not depend on a narrower producer assumption. One group
    // registers two copy roots, two namespaces, one SQL root and four finite-
    // scratch roots.
    let inspection_groups = selected_packet_count;
    let inspection_roots = inspection_groups
        .checked_mul(ryeos_app::runtime_db::MAX_INSPECTION_COPY_GROUP_PHYSICAL_REGISTRATIONS)
        .context("inspection group physical registration bound overflow")?;

    // Registration attempts are enumerated independently from slot capacity:
    // each fixed owner array entry, ticket-derived State source, packet bound/
    // destination root, inspection group root, settled control source and
    // Store metadata source has one cached register_* call per finalizer pass.
    // Aliases may coalesce slots, but each owner call remains a billed attempt.
    let state_projection_attempts = usize::try_from(tickets)?
        .checked_mul(3)
        .context("State projection registration bound overflow")?;
    let generated_state_root_attempts =
        ryeos_state::state_db::maximum_state_destination_leaves(tickets)?;

    Ok(LivePhysicalRegistrationBounds {
        normal_state: owner_registration_bound(
            NORMAL_STATE_PHYSICAL_ROOTS,
            NORMAL_STATE_PHYSICAL_ROOTS,
        )?,
        live_state_parent: owner_registration_bound(
            LIVE_STATE_PARENT_PHYSICAL_ROOTS,
            LIVE_STATE_PARENT_PHYSICAL_ROOTS,
        )?,
        live_state_recovery: owner_registration_bound(
            LIVE_STATE_RECOVERY_PHYSICAL_ROOTS,
            LIVE_STATE_RECOVERY_PHYSICAL_ROOTS,
        )?,
        live_state_projection: owner_registration_bound(
            state_projection,
            state_projection_attempts,
        )?,
        generated_state_roots: owner_registration_bound(
            generated_state_roots,
            generated_state_root_attempts,
        )?,
        operational: owner_registration_bound(
            OPERATIONAL_PHYSICAL_ROOTS,
            OPERATIONAL_PHYSICAL_ROOTS,
        )?,
        runtime_host: owner_registration_bound(
            RUNTIME_HOST_PHYSICAL_ROOTS,
            RUNTIME_HOST_PHYSICAL_ROOTS,
        )?,
        runtime_finalizer_tail: owner_registration_bound(
            RUNTIME_FINALIZER_TAIL_PHYSICAL_ROOTS,
            RUNTIME_FINALIZER_TAIL_PHYSICAL_ROOTS,
        )?,
        store_parent_and_operator: owner_registration_bound(
            STORE_PARENT_AND_OPERATOR_PHYSICAL_ROOTS,
            STORE_PARENT_AND_OPERATOR_PHYSICAL_ROOTS,
        )?,
        runtime_operational_packets: owner_registration_bound(
            runtime_operational_packet_roots,
            runtime_operational_packet_roots,
        )?,
        scheduler_packets: owner_registration_bound(
            scheduler_packet_roots,
            scheduler_packet_roots,
        )?,
        inspection_groups: owner_registration_bound(inspection_roots, inspection_roots)?,
        capacity_control_tail: owner_registration_bound(
            RETAINED_CAPACITY_CONTROL_SOURCES,
            RETAINED_CAPACITY_CONTROL_SOURCES,
        )?,
        store_metadata: owner_registration_bound(
            RETAINED_STORE_METADATA_SOURCES,
            RETAINED_STORE_METADATA_SOURCES,
        )?,
    })
}

fn maximum_physical_source_slots(tickets: u32) -> Result<usize> {
    live_physical_registration_bounds(tickets)?.source_slots()
}

/// Per-pass registration attempts from the independently enumerated live
/// registration callsites. This is A(T), not a derivation from unique-slot
/// capacity P(T): repeated aliases can use fewer slots while still incurring
/// distinct register_* attempts.
fn maximum_physical_registration_attempts(tickets: u32) -> Result<u64> {
    live_physical_registration_bounds(tickets)?.registration_attempts_per_pass()
}

/// Quote the one-time transfer of the prepared registry out of the reservation
/// before the consuming finalizer can advance its owner stages.
fn quote_prepared_physical_registry_handoff()
-> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 2 }
}

/// Registry part of `required_outside_cleanup`: the independently enumerated
/// P/A registration bounds and the inventory allocation.
fn minimum_registry_cleanup(
    tickets: u32,
) -> Result<(usize, lillux_sqlite::original_physical_custody::OutsideWork)> {
    let maximum = maximum_physical_source_slots(tickets)?;
    let attempts = maximum_physical_registration_attempts(tickets)?
        .checked_mul(u64::from(super::MAX_FINALIZATION_ATTEMPTS))
        .context("original four-pass registration bound overflow")?;
    let allocation =
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::storage_quote(maximum)?;
    let registration =
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::registration_quote(
            maximum, attempts,
        )?;
    Ok((maximum, allocation.checked_add(registration)?))
}

/// Daemon handoff of the primary operator lock into outside custody.
pub fn quote_operator_lock_handoff() -> lillux_sqlite::original_physical_custody::OutsideWork {
    lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 2 }
}

/// Complete outside-cleanup floor checked by `reserve`: the registry floor,
/// the retained late owner and its retirement inputs, every descriptor stage,
/// and every per-pass finalizer and State pre-seal stage charge, each counted
/// once per finalizer attempt. Returns the physical source maximum with it.
fn required_outside_cleanup(
    tickets: u32,
) -> Result<(usize, lillux_sqlite::original_physical_custody::OutsideWork)> {
    use lillux_sqlite::original_physical_custody::{OriginalPhysicalRegistry, OutsideWork};
    let (physical_maximum, registry_floor) = minimum_registry_cleanup(tickets)?;
    let attempts = u64::from(super::MAX_FINALIZATION_ATTEMPTS);
    let fixed = |steps: u64| OutsideWork { bytes: 0, steps };
    let membership =
        |queries: u64| OriginalPhysicalRegistry::membership_work_quote(physical_maximum, queries);
    let late_owner = LateNativeControl::maximum_storage_quote(physical_maximum)?;
    let retirement_inputs =
        LateNativeControl::maximum_retirement_inputs_quote(physical_maximum, tickets)?;
    // Settled finalizer stages: account/input transfer, registration,
    // exclusion aliases, receipt joins and settlement, once per pass.
    let finalizer = quote_finalization_control_source_receipt_join()?
        .checked_add(quote_finalization_store_metadata_receipts()?)?
        .checked_add(quote_finalization_store_parent_receipts()?)?
        .checked_add(quote_finalization_store_scheduler_receipts()?)?
        .checked_add(quote_finalization_store_exhaustion()?)?
        .checked_add(quote_prepared_physical_registry_handoff())?
        .checked_add(quote_operator_lock_handoff())?
        .checked_add(membership(1)?.checked_add(fixed(8))?)?
        .checked_add(membership(1)?.checked_add(fixed(8))?)?
        .checked_add(membership(2)?.checked_add(fixed(8))?)?
        .checked_add(super::account::quote_capacity_finalization_sources()?)?
        .checked_add(super::account::quote_capacity_finalization_validation()?)?
        .checked_add(super::account::quote_capacity_settlement_attempt()?)?
        .checked_add(super::quote_native_capacity_input_finalization_sources()?)?;
    // Descriptor retirement: one membership check per unretired slot, the
    // operator target and parent, and the operator observations.
    let descriptors = membership(1)?
        .checked_mul(u64::try_from(physical_maximum)?)?
        .checked_add(OriginalPhysicalRegistry::operator_target_retirement_quote(
            1,
        )?)?
        .checked_add(membership(1)?)?;
    #[cfg(target_os = "linux")]
    let descriptors = descriptors.checked_add(
        OriginalPhysicalRegistry::operator_observation_quote(physical_maximum, 2)?,
    )?;
    // Each close owner is prepared once; a refused pass repeats at most one.
    let descriptor_owners = OriginalPhysicalRegistry::descriptor_owner_quote(physical_maximum)?
        .checked_add(
            OriginalPhysicalRegistry::descriptor_owner_quote(1)?
                .checked_mul(attempts.saturating_sub(1))?,
        )?;
    // State pre-seal stages: join, destination-input transfer with one leaf
    // admission per ticket, leaf coalescing, parent transfer and parts, and
    // the host-alias retirement and live-parent completion checks.
    let leaves = ryeos_state::state_db::maximum_state_destination_leaves(tickets)?;
    let state = ryeos_state::state_db::quote_state_destination_join(leaves)?
        .checked_add(
            ryeos_state::state_db::quote_maximum_state_destination_input_transfer(tickets)?,
        )?
        .checked_add(ryeos_state::state_db::quote_state_leaf_admission(
            tickets, tickets,
        )?)?
        .checked_add(ryeos_state::state_db::quote_state_source_coalescing(
            leaves,
        )?)?
        .checked_add(ryeos_state::state_db::quote_live_state_parent_transfer()?)?
        .checked_add(ryeos_state::state_db::quote_live_state_physical_parent()?)?
        .checked_add(membership(8)?.checked_add(fixed(8))?.checked_mul(2)?)?;
    // Remaining State pre-seal charges: one continuation per leaf, the
    // generated-root membership check, the destination join, the parent and
    // registered-root validations, recovery registration, six root handoffs
    // and the Operational physical parent.
    let state_tail = ryeos_state::state_db::quote_state_leaf_continuations(leaves)?
        .checked_add(ryeos_state::state_db::quote_generated_root_registration(
            physical_maximum,
            leaves,
        )?)?
        .checked_add(ryeos_state::state_db::quote_live_state_destination_join())?
        .checked_add(membership(12)?.checked_mul(2)?)?
        .checked_add(membership(8)?)?
        .checked_add(membership(6)?)?
        .checked_add(
            ryeos_state::state_db::quote_live_state_root_registration_handoff().checked_mul(6)?,
        )?
        .checked_add(ryeos_state::state_db::quote_live_state_physical_parent()?)?;
    // Store pre-seal charges: join preparation and receiver, parent transfer
    // and physical parent, one transfer per leaf, parent finalization parts,
    // root and completion memberships, handoffs and alias retirements, the
    // operator lock, packet-vector handoffs and destination retries.
    let selected_packets = ryeos_app::state_store::MAX_LIVE_SELECTED_NATIVE_PACKET_SOURCES;
    let store_tail = ryeos_app::state_store::quote_store_destination_join_preparation()
        .checked_add(ryeos_app::state_store::quote_store_receiver()?)?
        .checked_add(ryeos_app::state_store::quote_store_parent_transfer()?)?
        .checked_add(ryeos_app::state_store::quote_store_physical_parent()?)?
        .checked_add(ryeos_app::state_store::quote_store_leaf_transfers(leaves)?)?
        .checked_add(ryeos_app::state_store::quote_store_parent_finalization_parts()?)?
        .checked_add(membership(5)?.checked_add(fixed(8))?)?
        .checked_add(membership(3)?.checked_add(fixed(8))?)?
        .checked_add(membership(4)?.checked_add(fixed(16))?)?
        .checked_add(membership(4)?.checked_add(fixed(8))?)?
        .checked_add(
            ryeos_app::state_store::quote_store_root_registration_handoff().checked_mul(4)?,
        )?
        .checked_add(ryeos_app::state_store::quote_store_parent_authority_handoff())?
        .checked_add(ryeos_app::state_store::quote_store_parent_alias_retirement())?
        .checked_add(ryeos_app::state_store::quote_store_child_parent_alias_retirement())?
        .checked_add(ryeos_app::state_store::quote_store_parent_facade_retirement())?
        .checked_add(ryeos_app::state_store::quote_store_direct_operational_handoff())?
        .checked_add(ryeos_app::state_store::quote_store_operator_lock_registration())?
        .checked_add(membership(1)?)?
        .checked_add(
            ryeos_app::state_store::quote_store_packet_vector_handoff()
                .checked_mul(u64::try_from(selected_packets)?)?,
        )?
        .checked_add(ryeos_app::state_store::quote_destination_retries(
            selected_packets,
        )?)?;
    let per_pass = finalizer
        .checked_add(descriptors)?
        .checked_add(state)?
        .checked_add(state_tail)?
        .checked_add(store_tail)?;
    let total = registry_floor
        .checked_add(late_owner)?
        .checked_add(retirement_inputs)?
        .checked_add(descriptor_owners)?
        .checked_add(per_pass.checked_mul(attempts)?)?;
    Ok((physical_maximum, total))
}

/// Quote the same-registry receipt checks and clock-alias join that consume the
/// settled account/input source frames after registry retirement. The
/// no-Store path can also retain one direct operator registration, so this
/// covers twelve settled control sources plus that optional operator member.
/// Each incomplete pass validates the whole retained frame before consuming
/// any source and charges this per-pass quote; `required_outside_cleanup`
/// reserves it once per finalizer attempt.
fn quote_finalization_control_source_receipt_join()
-> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let maximum_sources = RETAINED_CAPACITY_CONTROL_SOURCES
        .checked_add(1)
        .context("finalizer control-source receipt bound overflow")?;
    let per_pass = u64::try_from(maximum_sources)?
        .checked_mul(FINALIZER_CONTROL_SOURCE_RECEIPT_STEPS)
        .and_then(|steps| steps.checked_add(FINALIZER_CONTROL_SOURCE_PHASE_STEPS))
        .context("finalizer control-source receipt work quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: 0,
        steps: per_pass,
    })
}

/// Quote the retained State/thread metadata membership receipts after the
/// exact Store inventory has closed, for one finalizer pass.
fn quote_finalization_store_metadata_receipts()
-> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let per_pass = u64::try_from(RETAINED_STORE_METADATA_SOURCES)?
        .checked_mul(FINALIZER_CONTROL_SOURCE_RECEIPT_STEPS)
        .and_then(|steps| steps.checked_add(FINALIZER_CONTROL_SOURCE_PHASE_STEPS))
        .context("finalizer Store metadata receipt work quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: 0,
        steps: per_pass,
    })
}

fn quote_finalization_store_parent_receipts()
-> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let per_pass = u64::try_from(RETAINED_STORE_PARENT_SOURCES)?
        .checked_mul(FINALIZER_CONTROL_SOURCE_RECEIPT_STEPS)
        .and_then(|steps| steps.checked_add(FINALIZER_CONTROL_SOURCE_PHASE_STEPS))
        .context("finalizer Store parent receipt work quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: 0,
        steps: per_pass,
    })
}

fn quote_finalization_store_scheduler_receipts()
-> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let per_pass = u64::try_from(RETAINED_STORE_SCHEDULER_SOURCES)?
        .checked_mul(FINALIZER_CONTROL_SOURCE_RECEIPT_STEPS)
        .and_then(|steps| steps.checked_add(FINALIZER_CONTROL_SOURCE_PHASE_STEPS))
        .context("finalizer Store Scheduler receipt work quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: 0,
        steps: per_pass,
    })
}

/// Fixed census pass over the remaining outside-graph slots; per-frame
/// receipt checks charge their own quotes through the same late owner.
fn quote_finalization_store_exhaustion()
-> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: 0,
        steps: FINALIZER_CONTROL_SOURCE_PHASE_STEPS,
    })
}

fn require_finalizer_directory_receipt(
    source: Option<&super::FinalizerDirectorySource>,
    retired_registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
) -> Result<()> {
    let source = source.context("settled finalizer directory source absent")?;
    ensure!(
        source.owner.is_none(),
        "registered finalizer directory retains its open owner"
    );
    let registration = source
        .registration
        .as_ref()
        .context("settled finalizer directory membership absent")?;
    ensure!(
        matches!(
            registration.observed_source_identity()?,
            lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }
        ),
        "settled finalizer directory source role changed"
    );
    drop(registration.receipt_in(retired_registry)?);
    Ok(())
}

fn require_finalizer_control_receipt(
    source: Option<&super::FinalizerControlSource>,
    retired_registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
) -> Result<()> {
    let source = source.context("settled finalizer control source absent")?;
    ensure!(
        source.pinned_file.is_none() && source.duplicate.is_none() && source.owner.is_none(),
        "registered finalizer control source retains an open owner"
    );
    let registration = source
        .registration
        .as_ref()
        .context("settled finalizer control membership absent")?;
    ensure!(matches!(registration.observed_source_identity()?,
        lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::ControlRecord { .. }),
        "settled finalizer control source role changed");
    drop(registration.receipt_in(retired_registry)?);
    Ok(())
}

/// Sizing floor for the signed rollover drain margin: the sum of the bounded
/// steps of the normal graceful stop that begins that margin before the
/// service edge. It is a sizing heuristic, not a proof of completion. The
/// bounded steps are the two listener settles, the external import drain, the
/// persistent-session pool reap, the dedicated worker retirement, the attached
/// kill and in-process handler wait (one shared deadline) and the accounting
/// publisher stop. Steps with no bound of their own add nothing: the scheduler
/// close and the in-process reservation audit.
pub fn required_rollover_drain_margin() -> std::time::Duration {
    let grace =
        std::time::Duration::from_secs(ryeos_app::process::MAX_GRACEFUL_SHUTDOWN_GRACE_SECS);
    ryeos_app::process::LISTENER_SHUTDOWN_BOUND * 2
        + grace * 4
        + ryeos_app::process::ACCOUNTING_PUBLISHER_STOP_BOUND
}

/// Floor for the cleanup allowance that follows the service edge: the
/// latched remainder of the stop, bounded by the same steps as the normal
/// drain, plus the cleanup chain. The cleanup chain has no bound of its own:
/// an expired cleanup allowance refuses it and custody is retained, so it
/// adds nothing here.
pub fn required_cleanup_occupancy() -> std::time::Duration {
    required_rollover_drain_margin()
}

/// Envelope-derived quantities `reserve` checks before any account effect.
struct ReservationFloors {
    tickets: u32,
    host_layout: lillux_sqlite::original_host_owner::backing::HostStorageLayout,
    outside_control_limit: u64,
    physical_maximum: usize,
    host_work: u64,
}

/// Every pure floor check of one signed incarnation envelope: native cleanup
/// after the signed splits, host payload and resident span, the complete
/// outside-cleanup floor (bytes and steps) and bounded host preparation.
fn require_reservation_floors(
    incarnation: &ryeos_node_runtime::node_policy::sections::native_capacity::NativeIncarnationEnvelope,
) -> Result<ReservationFloors> {
    let units = incarnation.units;
    let tickets = u32::try_from(units.connection_tickets)?;
    let margin = std::time::Duration::from_millis(incarnation.rollover_drain_margin_ms);
    ensure!(
        margin >= required_rollover_drain_margin(),
        "rollover drain margin {margin:?} is below the bounded normal-stop floor {:?}",
        required_rollover_drain_margin()
    );
    let cleanup = std::time::Duration::from_millis(incarnation.cleanup_occupancy_ms);
    ensure!(
        cleanup >= required_cleanup_occupancy(),
        "cleanup occupancy {cleanup:?} is below the latched-stop floor {:?}",
        required_cleanup_occupancy()
    );
    // Pure native layout quote: no arena mapping or SQLite initialization.
    // The actual host layout, rather than assumed source arithmetic, must
    // fit the signed cleanup reserve before the first reservation effect.
    let required = OriginalNativeFactory::required_cleanup_steps(units.storage_bytes, tickets)?;
    let outside_cleanup_steps = incarnation.outside_cleanup_steps;
    ensure!(
        outside_cleanup_steps > 0,
        "cleanup envelope has no original outside work reserve"
    );
    let native_cleanup_steps = units
        .cleanup_steps
        .checked_sub(incarnation.host_release_steps)
        .and_then(|remaining| remaining.checked_sub(outside_cleanup_steps))
        .context("original cleanup splits exceed signed total")?;
    ensure!(
        native_cleanup_steps >= required,
        "native cleanup envelope cannot cover actual layout after signed outside splits"
    );
    let host_minimum = super::factory::LiveNativeFactory::minimum_host_payload_bytes(tickets)?;
    ensure!(
        units.host_control_storage_bytes >= host_minimum,
        "host storage cannot cover actual factory/control payload layouts"
    );
    // Split ONE original durably reserved host dimension. The resident
    // span is its actual rounded physical layout; the remainder funds
    // ordinary outside receiver/refusal storage under this same owner.
    let host_layout =
        lillux_sqlite::original_host_owner::backing::HostStorage::quote_native_region()
            .map_err(anyhow::Error::msg)?;
    let outside_control_limit = units
        .host_control_storage_bytes
        .checked_sub(u64::try_from(host_layout.bytes())?)
        .context("host allowance cannot cover actual rounded resident span")?;
    ensure!(
        outside_control_limit > 0,
        "host allowance has no original outside control reserve"
    );
    let (physical_maximum, required_outside) = required_outside_cleanup(tickets)?;
    ensure!(
        outside_control_limit >= required_outside.bytes,
        "outside control storage is short of the required outside-cleanup floor by {} bytes",
        required_outside.bytes.saturating_sub(outside_control_limit)
    );
    ensure!(
        outside_cleanup_steps >= required_outside.steps,
        "outside cleanup is short of the required outside-cleanup floor by {} steps",
        required_outside.steps.saturating_sub(outside_cleanup_steps)
    );
    // Reject an envelope that cannot fund every late outside charge
    // across all finalizer attempts before account open/debit.
    let host_work = host_layout
        .work
        .checked_add(super::factory::LiveNativeFactory::minimum_host_placement_steps())
        .context("host preparation work overflow")?;
    ensure!(
        units.allocator_steps >= host_work,
        "allocator work cannot cover bounded host preparation"
    );
    Ok(ReservationFloors {
        tickets,
        host_layout,
        outside_control_limit,
        physical_maximum,
        host_work,
    })
}

/// Every slot of one reservation. The slots are listed in the order a settled
/// reservation releases them. Settled exactly when final consumption would
/// accept the reservation without a registry: its admission inputs moved into
/// settled finalizer custody, the release receipt is present and no account,
/// native, region or registry owner remains. Otherwise the whole content
/// (admission inputs, account, budgets, region, exclusion, host storage, late
/// control and registry) stays retained until the process exits, so no part
/// of an unsettled reservation is released. The atomic flag and the
/// generation number own nothing.
pub struct ReservationParts {
    // The verified admission inputs, taken exactly once when settlement moves
    // them into finalizer custody; every other reader goes through `inputs`.
    inputs: Option<Arc<LiveNativeCapacityInputs>>,
    // Boundaries share the read lock; only settlement, recovery and the
    // initial debit take it exclusively, and none of those overlaps service SQL.
    account: std::sync::RwLock<Option<RetainedCapacityAccount>>,
    budget: OnceLock<Arc<OriginalNativeBudgetOwner>>,
    raw_budget: Mutex<Option<OriginalNativeBudget>>,
    region: Mutex<Option<OriginalNativeRegionOwner>>,
    release_exclusion: Option<OriginalNativeRegionReleaseExclusion>,
    seal_failure: Option<OriginalNativeRegionSealFailure>,
    resident_generation: Option<u64>,
    factory_entered: AtomicBool,
    host_storage: OnceLock<lillux_sqlite::original_host_owner::backing::HostStorage>,
    host_release_credit: Option<lillux_sqlite::original_host_owner::backing::HostReleaseCredit>,
    // Actual physical evidence survives an account-publication refusal. Once
    // present, retries may settle only; they must never revisit resident data.
    released: Option<ReleasedReservation>,
    // Affine cleanup work debited from the genuine raw counter once before
    // mapping, alongside actual pre-reserved registry capacity. The same owner
    // survives account settlement without an account or lock-lease backedge.
    late: Option<Arc<LateNativeControl>>,
    physical_registry:
        Mutex<Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry>>,
    // The stop owner's rollover claim, fixed by the first release call. A
    // retry after a refused settlement keeps that first claim and never
    // takes a fresher one, so the settlement it retries cannot re-decide.
    rollover_claim: OnceLock<Option<RolloverClaim>>,
}

/// One original reservation under custody: its end releases a settled
/// reservation, retains an unsettled one and relinquishes a copy reached in
/// another process.
pub struct LiveNativeCapacityReservation {
    held: Held<ReservationParts>,
}

impl LiveNativeCapacityReservation {
    fn held(parts: ReservationParts) -> Self {
        const KIND: &str = "ryeos-node::native-capacity-reservation";
        Self {
            held: Held::new(KIND, parts, |parts| parts.require_settled_holders().is_ok()),
        }
    }
}
impl std::ops::Deref for LiveNativeCapacityReservation {
    type Target = ReservationParts;
    fn deref(&self) -> &ReservationParts {
        &self.held
    }
}
impl std::ops::DerefMut for LiveNativeCapacityReservation {
    fn deref_mut(&mut self) -> &mut ReservationParts {
        self.held.get_mut()
    }
}

/// What the stop owner observed when it chose an orderly rollover. It is a
/// claim, not evidence: the account owner re-proves every part before it
/// opens an epoch, and publishes a plain release settlement if any part fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloverClaim {
    /// Service-refusal latch bits at the moment of the claim; any FACT bit forbids it.
    pub latch_bits: u8,
}

enum ReleasedReservation {
    BeforeRegion(HostStorageReleased),
    Region(HostStorageReleased),
}

enum RetainedReservation {
    Bootstrap(LiveNativeCapacityReservation),
    Published(Arc<LiveNativeCapacityReservation>),
}

/// No end settles a retained reservation: outside its refusal it always holds
/// the reservation that supervised recovery takes back, so only
/// `into_taken` releases it from custody.
struct RetainedReservationOwner {
    held: Held<RetainedReservation>,
}
impl RetainedReservationOwner {
    const KIND: &'static str = "ryeos-node::native-capacity-reservation-failure";

    fn bootstrap(original: LiveNativeCapacityReservation) -> Self {
        Self {
            held: Held::until_taken(Self::KIND, RetainedReservation::Bootstrap(original)),
        }
    }

    fn published(original: Arc<LiveNativeCapacityReservation>) -> Self {
        Self {
            held: Held::until_taken(Self::KIND, RetainedReservation::Published(original)),
        }
    }

    fn original(&self) -> &LiveNativeCapacityReservation {
        match &*self.held {
            RetainedReservation::Bootstrap(owner) => owner,
            RetainedReservation::Published(owner) => owner,
        }
    }

    /// Move the retained reservation out. A copy in another process is
    /// refused and comes back still retained.
    fn into_taken(self) -> std::result::Result<Arc<LiveNativeCapacityReservation>, Self> {
        match self.held.take_whole() {
            Ok(RetainedReservation::Bootstrap(original)) => Ok(Arc::new(original)),
            Ok(RetainedReservation::Published(original)) => Ok(original),
            Err(held) => Err(Self { held }),
        }
    }
}

pub struct LiveNativeCapacityReservationFailure {
    original: RetainedReservationOwner,
    error: anyhow::Error,
}

/// Consuming account-settled phase. The actual account/input descriptors,
/// release receipt, original clock and prepared inventory survive here until
/// the finalizer transfers them to explicit close custody. This is not a
/// graceful-shutdown receipt and exposes no renewed reservation authority.
///
/// The slots are listed in the order a settled phase releases them. Settled
/// exactly when final consumption would accept it: every finalizer frame
/// consumed, the late registry and clock retired and the reservation holding
/// only its release receipt. Otherwise the frames and the reservation stay
/// retained together until the process exits.
pub struct SettledParts {
    account_sources: Option<super::account::RetainedCapacityFinalizationSources>,
    input_sources: Option<super::NativeCapacityInputFinalizationSources>,
    // Present on the no-Store acquisition-failure route, where this original
    // reservation owns the pre-mapping registry and must also own the exact
    // operator-lock membership used by its exclusion alias.
    operator_registration:
        Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
    original: LiveNativeCapacityReservation,
    destination_phase: FinalizationDestinationPhase,
    control_sources_consumed: bool,
    store_metadata_sources_consumed: bool,
    store_parent_sources_consumed: bool,
    store_state_projection_sources_consumed: bool,
    store_state_root_sources_consumed: bool,
    store_runtime_shared_roots_consumed: bool,
    store_packet_bound_roots_consumed: bool,
    store_packet_metadata_pins_consumed: bool,
    store_packet_source_parts_consumed: bool,
    store_packet_acquisition_failure_count: Option<u8>,
    store_scheduler_sources_consumed: bool,
    store_owner_frames_consumed: bool,
    store_outside_exhausted: bool,
}

pub struct SettledLiveNative {
    held: Held<SettledParts>,
}

impl SettledLiveNative {
    fn held(parts: SettledParts) -> Self {
        const KIND: &str = "ryeos-node::native-capacity-settled";
        Self {
            held: Held::new(KIND, parts, |parts| {
                parts.require_finalized_holders().is_ok()
            }),
        }
    }

    /// A settled phase that has finalized nothing yet.
    fn begin(original: LiveNativeCapacityReservation) -> Self {
        Self::held(SettledParts {
            account_sources: None,
            input_sources: None,
            operator_registration: None,
            original,
            destination_phase: FinalizationDestinationPhase::Pending,
            control_sources_consumed: false,
            store_metadata_sources_consumed: false,
            store_parent_sources_consumed: false,
            store_state_projection_sources_consumed: false,
            store_state_root_sources_consumed: false,
            store_runtime_shared_roots_consumed: false,
            store_packet_bound_roots_consumed: false,
            store_packet_metadata_pins_consumed: false,
            store_packet_source_parts_consumed: false,
            store_packet_acquisition_failure_count: None,
            store_scheduler_sources_consumed: false,
            store_owner_frames_consumed: false,
            store_outside_exhausted: false,
        })
    }
}
impl std::ops::Deref for SettledLiveNative {
    type Target = SettledParts;
    fn deref(&self) -> &SettledParts {
        &self.held
    }
}
impl std::ops::DerefMut for SettledLiveNative {
    fn deref_mut(&mut self) -> &mut SettledParts {
        self.held.get_mut()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FinalizationDestinationPhase {
    Pending,
    StoreRegistryRetired,
    StoreDestinationsRetired,
    NoStoreRegistryRetired,
}

/// The admission inputs a reservation still retains. Absent only after
/// settlement moved them into finalizer custody. Borrows the slot alone, so
/// a caller holding another field of the same reservation mutably can use it.
fn retained_inputs(
    inputs: &Option<Arc<LiveNativeCapacityInputs>>,
) -> Result<&Arc<LiveNativeCapacityInputs>> {
    inputs
        .as_ref()
        .context("original native admission inputs already moved into settled finalizer custody")
}

/// The consumed process operator lock awaiting registration with the settled
/// finalizer. No end settles it: outside a refusal it always passes to the
/// registry that retains it, so only `into_taken` releases it from custody
/// and any other end retains it until the process exits.
#[cfg(target_os = "linux")]
pub struct OperatorLockCustody {
    held: Held<Arc<lillux::ExactExclusiveFileLock>>,
}

#[cfg(target_os = "linux")]
impl OperatorLockCustody {
    const KIND: &'static str = "ryeos-node::native-operator-lock";

    pub fn new(owner: Arc<lillux::ExactExclusiveFileLock>) -> Self {
        Self {
            held: Held::until_taken(Self::KIND, owner),
        }
    }

    /// Move the lock out. A copy in another process is refused and comes back
    /// still retained.
    pub fn into_taken(self) -> std::result::Result<Arc<lillux::ExactExclusiveFileLock>, Self> {
        self.held.take_whole().map_err(|held| Self { held })
    }
}

#[cfg(target_os = "linux")]
impl std::ops::Deref for OperatorLockCustody {
    type Target = Arc<lillux::ExactExclusiveFileLock>;
    fn deref(&self) -> &Self::Target {
        &self.held
    }
}

pub(super) trait SettledSourceRegistry {
    fn register_directory(
        &mut self,
        owner: Arc<lillux::PinnedDirectory>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        (Arc<lillux::PinnedDirectory>, anyhow::Error),
    >;
    fn register_control_record(
        &mut self,
        owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        (
            Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
            anyhow::Error,
        ),
    >;
    fn require_membership(
        &mut self,
        registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity>;
}

impl SettledSourceRegistry for lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry {
    fn register_directory(
        &mut self,
        owner: Arc<lillux::PinnedDirectory>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        (Arc<lillux::PinnedDirectory>, anyhow::Error),
    > {
        unsafe {
            lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::register_directory(
                self, owner, prepay,
            )
        }
    }
    fn register_control_record(
        &mut self,
        owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        (
            Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
            anyhow::Error,
        ),
    > {
        unsafe {
            lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::register_control_record(self, owner, prepay)
        }
    }
    fn require_membership(
        &mut self,
        registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity> {
        let quote = self.membership_work_bound(1, 0)?;
        self.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::require_membership(
            self,
            registration,
        )?;
        registration.observed_source_identity()
    }
}

impl SettledSourceRegistry for ryeos_app::state_store::LiveStateStoreOutsideSource {
    fn register_directory(
        &mut self,
        owner: Arc<lillux::PinnedDirectory>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        (Arc<lillux::PinnedDirectory>, anyhow::Error),
    > {
        unsafe { self.register_finalizer_directory_source(owner, prepay) }
    }
    fn register_control_record(
        &mut self,
        owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        (
            Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
            anyhow::Error,
        ),
    > {
        unsafe { self.register_finalizer_control_record_source(owner, prepay) }
    }
    fn require_membership(
        &mut self,
        registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity> {
        self.require_finalizer_source_membership(registration, prepay)
    }
}

impl SettledSourceRegistry for ryeos_app::state_store::LiveStateStoreOutsideCustody {
    fn register_directory(
        &mut self,
        owner: Arc<lillux::PinnedDirectory>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        (Arc<lillux::PinnedDirectory>, anyhow::Error),
    > {
        unsafe { self.register_finalizer_directory_source(owner, prepay) }
    }
    fn register_control_record(
        &mut self,
        owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        (
            Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
            anyhow::Error,
        ),
    > {
        unsafe { self.register_finalizer_control_record_source(owner, prepay) }
    }
    fn require_membership(
        &mut self,
        registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity> {
        self.require_finalizer_source_membership(registration, prepay)
    }
}

fn register_finalizer_directory_source(
    source: &mut super::FinalizerDirectorySource,
    registry: &mut dyn SettledSourceRegistry,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    if let Some(registration) = source.registration.as_ref() {
        ensure!(matches!(registry.require_membership(registration, prepay)?,
            lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
            "settled directory membership changed source role");
        ensure!(
            source.owner_transferred(),
            "registered settled directory retains its source owner"
        );
        return Ok(());
    }
    let owner = source
        .take_owner()
        .context("settled directory source absent")?;
    match registry.register_directory(owner, prepay) {
        Ok(registration) => source.set_registration(registration).map_err(|_| {
            anyhow::anyhow!("settled directory source gained a membership during registration")
        }),
        Err((owner, error)) => {
            source.restore_owner(owner).map_err(|_| {
                anyhow::anyhow!(
                    "settled directory source gained an owner during a refused registration"
                )
            })?;
            Err(error)
        }
    }
}

fn register_finalizer_control_source(
    source: &mut super::FinalizerControlSource,
    registry: &mut dyn SettledSourceRegistry,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    if let Some(registration) = source.registration.as_ref() {
        ensure!(matches!(registry.require_membership(registration, prepay)?,
            lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::ControlRecord { .. }),
            "settled control membership changed source role");
        ensure!(
            source.owner.is_none() && source.source_transferred(),
            "registered settled control source retains an open owner"
        );
        return Ok(());
    }
    ensure!(
        !(source.pinned_file.is_some() && source.duplicate.is_some()),
        "settled control source has competing original and duplicate descriptors"
    );
    if source.owner.is_none() {
        let quote = lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::descriptor_owner_quote(1)?;
        prepay(quote.bytes, quote.steps)?;
        let file = if let Some(pinned) = source.take_pinned_file() {
            pinned.into_owned_close_descriptor()
        } else if let Some(duplicate) = source.take_duplicate() {
            duplicate
        } else {
            anyhow::bail!("settled control descriptor source absent");
        };
        // SAFETY: this private frame is built only from the exact settled
        // account/input source pins and genuine captured duplicates. Their
        // durable use is complete, and no inode/path replacement is involved.
        source
            .set_owner(unsafe {
                lillux_sqlite::RetainedOriginalFinalizerFile::issued(
                    file,
                    lillux_sqlite::OriginalFinalizerFileKind::ControlRecord,
                )
            })
            .map_err(|_| {
                anyhow::anyhow!("settled control source gained a close owner during issuance")
            })?;
    }
    let owner = source
        .take_owner()
        .context("settled control close owner absent")?;
    match registry.register_control_record(owner, prepay) {
        Ok(registration) => source.set_registration(registration).map_err(|_| {
            anyhow::anyhow!("settled control source gained a membership during registration")
        }),
        Err((owner, error)) => {
            source.set_owner(owner).map_err(|_| {
                anyhow::anyhow!(
                    "settled control source gained an owner during a refused registration"
                )
            })?;
            Err(error)
        }
    }
}

fn register_finalization_source_frames(
    account: &mut Option<super::account::RetainedCapacityFinalizationSources>,
    inputs: &mut Option<super::NativeCapacityInputFinalizationSources>,
    registry: &mut dyn SettledSourceRegistry,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    let account = account
        .as_mut()
        .context("settled account source frame absent")?;
    for directory in [&mut account.account_parent, &mut account.anchor_parent] {
        register_finalizer_directory_source(
            directory
                .as_mut()
                .context("settled account directory source absent")?,
            registry,
            prepay,
        )?;
    }
    for file in [&mut account.account_file, &mut account.anchor_file] {
        register_finalizer_control_source(
            file.as_mut()
                .context("settled account control source absent")?,
            registry,
            prepay,
        )?;
    }
    let inputs = inputs
        .as_mut()
        .context("settled input source frame absent")?;
    for directory in inputs.directory_slots() {
        register_finalizer_directory_source(
            directory
                .as_mut()
                .context("settled input directory source absent")?,
            registry,
            prepay,
        )?;
    }
    for file in inputs.control_slots() {
        register_finalizer_control_source(
            file.as_mut()
                .context("settled input control source absent")?,
            registry,
            prepay,
        )?;
    }
    Ok(())
}

impl SettledParts {
    pub fn require_original_process(&self) -> Result<()> {
        if self.input_sources.is_some() {
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?
                .require_original_process()
        } else {
            self.original.require_original_process()
        }
    }

    /// Revalidate the same original cleanup window after admission inputs have
    /// moved into settled finalizer custody. This checks the retained late
    /// owner or original captured inputs; it never creates or renews authority.
    pub fn require_original_cleanup(&self) -> Result<()> {
        self.require_finalization_cleanup()
    }

    fn require_finalization_cleanup(&self) -> Result<()> {
        if self.input_sources.is_some() {
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?
                .require_original()
        } else {
            self.original.inputs()?.require_original_cleanup()
        }
    }

    /// Move the durably settled account and redo pins into explicit finalizer
    /// custody. A refusal restores the complete account owner; a completed
    /// transfer is cached here and never repeats its validation or charge.
    pub fn prepare_account_finalization_sources(&mut self) -> Result<()> {
        self.require_finalization_cleanup()?;
        let original = &mut *self.original;
        ensure!(
            original.released.is_some(),
            "settled account lacks its physical release receipt"
        );
        if self.account_sources.is_some() {
            ensure!(
                original
                    .account
                    .get_mut()
                    .map_err(|_| anyhow::anyhow!("capacity account poisoned"))?
                    .is_none(),
                "settled account source frame competes with its live account owner"
            );
            return Ok(());
        }
        let account = original
            .account
            .get_mut()
            .map_err(|_| anyhow::anyhow!("capacity account poisoned"))?
            .take()
            .context("settled capacity account owner absent")?;
        let inputs = match original.inputs() {
            Ok(inputs) => inputs,
            Err(error) => {
                *original
                    .account
                    .get_mut()
                    .map_err(|_| anyhow::anyhow!("capacity account poisoned"))? = Some(account);
                return Err(error);
            }
        };
        let late = original
            .late
            .as_ref()
            .context("settled outside cleanup owner absent")?;
        match account.into_finalization_sources(
            inputs,
            &|steps| late.prepay(0, steps),
            &|bytes, steps| late.prepay(bytes, steps),
        ) {
            Ok(sources) => {
                self.account_sources = Some(sources);
                Ok(())
            }
            Err((account, error)) => {
                *original
                    .account
                    .get_mut()
                    .map_err(|_| anyhow::anyhow!("capacity account poisoned"))? = Some(account);
                Err(error)
            }
        }
    }

    /// Transfer the genuine admission-input pins and their two shared aliases
    /// into the same settled owner. The composition alias is discarded only
    /// after pointer identity is checked; Arc refusal restores the reservation
    /// owner and leaves all source pins unchanged.
    pub fn prepare_input_finalization_sources(
        &mut self,
        composition_inputs: &mut Option<Arc<LiveNativeCapacityInputs>>,
    ) -> Result<()> {
        self.require_finalization_cleanup()?;
        if self.input_sources.is_some() {
            return Ok(());
        }
        ensure!(
            self.account_sources.is_some(),
            "settled admission inputs cannot transfer before account sources"
        );
        let original = &mut *self.original;
        let late = original
            .late
            .as_ref()
            .context("settled outside cleanup owner absent")?;
        if let Some(alias) = composition_inputs.as_ref() {
            ensure!(
                Arc::ptr_eq(alias, original.inputs()?),
                "composition retains a foreign native input owner"
            );
            drop(composition_inputs.take());
        }
        // The input slot is taken exactly once. The exact Arc is restored on
        // uniqueness or prepay refusal; success moves every actual source
        // into the retained finalizer frame and leaves the slot empty.
        let input_owner = original.inputs.take().context(
            "original native admission inputs already moved into settled finalizer custody",
        )?;
        let inputs = match Arc::try_unwrap(input_owner) {
            Ok(inputs) => inputs,
            Err(input_owner) => {
                let aliases = Arc::strong_count(&input_owner);
                original.inputs = Some(input_owner);
                anyhow::bail!("settled input source still has {aliases} actual Arc owners");
            }
        };
        match inputs.into_finalization_sources(&|bytes, steps| late.prepay(bytes, steps)) {
            Ok(sources) => {
                self.input_sources = Some(sources);
                Ok(())
            }
            Err((inputs, error)) => {
                original.inputs = Some(Arc::new(inputs));
                Err(error)
            }
        }
    }

    /// Join every actual settled account/input directory and control file to
    /// the same original registry used by the Store graph. Partial success is
    /// cached per source; retries charge only unfinished owner transfers.
    pub(super) fn register_finalization_sources(
        &mut self,
        registry: &mut dyn SettledSourceRegistry,
    ) -> Result<()> {
        self.require_finalization_cleanup()?;
        ensure!(
            self.account_sources.is_some() && self.input_sources.is_some(),
            "settled source registration precedes account/input extraction"
        );
        let late = self
            .original
            .late
            .as_ref()
            .context("settled outside cleanup owner absent")?;
        let prepay = |bytes: u64, steps: u64| late.prepay(bytes, steps);
        register_finalization_source_frames(
            &mut self.account_sources,
            &mut self.input_sources,
            registry,
            &prepay,
        )
    }

    pub fn register_finalization_sources_in_store(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_finalization_cleanup()?;
        self.register_finalization_sources(outside)
    }

    /// Register settled account/input owners when startup failed before a
    /// Store custody took the reservation's original pre-mapping registry.
    /// The registry remains inside this exact settled reservation on refusal.
    pub fn register_finalization_sources_in_reservation(&mut self) -> Result<()> {
        self.require_finalization_cleanup()?;
        ensure!(
            self.account_sources.is_some() && self.input_sources.is_some(),
            "settled source registration precedes account/input extraction"
        );
        let Self {
            original,
            account_sources,
            input_sources,
            operator_registration: _,
            destination_phase: _,
            control_sources_consumed: _,
            store_metadata_sources_consumed: _,
            store_parent_sources_consumed: _,
            store_state_projection_sources_consumed: _,
            store_state_root_sources_consumed: _,
            store_runtime_shared_roots_consumed: _,
            store_packet_bound_roots_consumed: _,
            store_packet_metadata_pins_consumed: _,
            store_packet_source_parts_consumed: _,
            store_packet_acquisition_failure_count: _,
            store_scheduler_sources_consumed: _,
            store_owner_frames_consumed: _,
            store_outside_exhausted: _,
        } = self;
        let original = &mut **original;
        let late = original
            .late
            .as_ref()
            .context("settled outside cleanup owner absent")?;
        let prepay = |bytes: u64, steps: u64| late.prepay(bytes, steps);
        let registry = original
            .physical_registry
            .get_mut()
            .map_err(|_| anyhow::anyhow!("original settled physical registry poisoned"))?
            .as_mut()
            .context("original settled physical registry absent")?;
        register_finalization_source_frames(account_sources, input_sources, registry, &prepay)
    }

    /// Seal the completed Store-owned inventory, prepare its exact descriptor
    /// owners, and retire every non-operator member through the same late
    /// authority. Each member's input and close receipt remain cached in the
    /// original outside owners on refusal.
    pub fn retire_non_account_finalization_descriptors_in_store(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_finalization_cleanup()?;
        ensure!(
            self.account_sources.is_some() && self.input_sources.is_some(),
            "physical retirement precedes settled account/input source transfer"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let prepay = |bytes, steps| late.prepay(bytes, steps);
        let issue_inputs =
            |registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
                owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>| late.outside_retirement_inputs(registration, owner);
        let admission_alias = self
            .input_sources
            .as_ref()
            .and_then(|sources| sources.exclusion_alias.as_ref());
        if admission_alias.is_none() {
            // A prior pass validated and retired the admission alias as its
            // final step. Its completed receipts make this stage a no-op
            // apart from the Store's own alias, which may still remain.
            anyhow::ensure!(
                outside.finalizer_non_account_descriptors_retired()?,
                "settled admission exclusion alias absent before Store target retirement"
            );
            // SAFETY: the same sealed registry reports every non-operator
            // receipt; the original outside owner pays.
            return unsafe { outside.retire_settled_store_exclusion_alias(None, &prepay) };
        }
        // SAFETY: the same Store-held registry already contains every source;
        // durable settlement is complete and the original outside owner pays.
        unsafe {
            outside.seal_finalizer_physical_inventory()?;
            outside.prepare_finalizer_descriptor_owners(&prepay)?;
            outside.retire_finalizer_non_account_descriptors(&prepay, &issue_inputs)?;
            // Both the Store lease and the original admission lease are real
            // Arc aliases to the same canonical operator lock. Validate both
            // against its live registry membership before ending either alias.
            outside.retire_settled_store_exclusion_alias(admission_alias, &prepay)?;
        }
        let sources = self
            .input_sources
            .as_mut()
            .context("settled admission exclusion source frame absent")?;
        let alias = sources
            .take_exclusion_alias()
            .context("settled admission exclusion alias disappeared after validation")?;
        drop(alias);
        Ok(())
    }

    /// Register the actual process operator lock in the original pre-mapping
    /// inventory for startup failures that never transferred a Store owner.
    /// On every refusal the same lock wrapper remains with the daemon caller.
    #[cfg(target_os = "linux")]
    pub fn register_operator_lock_in_reservation(
        &mut self,
        lock_owner: &mut Option<OperatorLockCustody>,
    ) -> Result<()> {
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::Pending
            ),
            "no-Store operator registration follows registry retirement"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let prepay = |bytes, steps| late.prepay(bytes, steps);

        if let Some(registration) = self.operator_registration.as_ref() {
            ensure!(
                lock_owner.is_none(),
                "registered no-Store operator lock still has a caller owner"
            );
            let original = &mut *self.original;
            let registry = original
                .physical_registry
                .get_mut()
                .map_err(|_| anyhow::anyhow!("original settled physical registry poisoned"))?
                .as_mut()
                .context("original settled physical registry absent")?;
            let quote = registry.membership_work_bound(1, 0)?;
            registry.prepay_transfer(&prepay, quote.bytes, quote.steps)?;
            lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::require_membership(
                registry,
                registration,
            )?;
            ensure!(
                registration.is_operator_lock()?,
                "no-Store operator registration has a foreign source role"
            );
            return Ok(());
        }

        let retained = lock_owner
            .take()
            .context("actual no-Store operator lock owner absent")?;
        let owner = match retained.into_taken() {
            Ok(owner) => owner,
            Err(retained) => {
                *lock_owner = Some(retained);
                anyhow::bail!("no-Store operator lock custody is not held by this process")
            }
        };
        if let Err(error) = late.prepay(0, 8) {
            *lock_owner = Some(OperatorLockCustody::new(owner));
            return Err(error);
        }
        let original = &mut *self.original;
        let registry = match original.physical_registry.get_mut() {
            Ok(slot) => match slot.as_mut() {
                Some(registry) => registry,
                None => {
                    *lock_owner = Some(OperatorLockCustody::new(owner));
                    anyhow::bail!("original settled physical registry absent")
                }
            },
            Err(_) => {
                *lock_owner = Some(OperatorLockCustody::new(owner));
                anyhow::bail!("original settled physical registry poisoned")
            }
        };
        match unsafe { registry.register_operator_lock(owner, &prepay) } {
            Ok(registration) => {
                self.operator_registration = Some(registration);
                Ok(())
            }
            Err((owner, error)) => {
                *lock_owner = Some(OperatorLockCustody::new(owner));
                Err(error)
            }
        }
    }

    /// No-Store startup failures keep the same pre-mapping inventory in the
    /// reservation. This path performs the same stages on that original owner,
    /// without constructing a replacement registry or holding its Mutex.
    pub fn retire_non_account_finalization_descriptors_in_reservation(&mut self) -> Result<()> {
        self.require_finalization_cleanup()?;
        ensure!(
            self.account_sources.is_some() && self.input_sources.is_some(),
            "physical retirement precedes settled account/input source transfer"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let prepay = |bytes, steps| late.prepay(bytes, steps);
        let issue_inputs =
            |registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
                owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>| late.outside_retirement_inputs(registration, owner);
        let original = &mut *self.original;
        let registry = original
            .physical_registry
            .get_mut()
            .map_err(|_| anyhow::anyhow!("original settled physical registry poisoned"))?
            .as_mut()
            .context("original settled physical registry absent")?;
        // SAFETY: this is the same pre-mapping registry funded by the original
        // reservation. Durable settlement is complete; callbacks run without a
        // retention mutex and use only the late outside owner.
        unsafe {
            registry.seal_inventory()?;
            registry.prepare_descriptor_owners(&prepay)?;
            registry.retire_non_operator_descriptors(&prepay, &issue_inputs)?;
        }
        #[cfg(target_os = "linux")]
        {
            self.retire_input_exclusion_alias_in_reservation()
        }
        #[cfg(not(target_os = "linux"))]
        {
            ensure!(
                self.input_sources
                    .as_ref()
                    .is_some_and(|sources| sources.exclusion_alias.is_none()),
                "settled admission exclusion retirement is unsupported on this host"
            );
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    fn retire_input_exclusion_alias_in_reservation(&mut self) -> Result<()> {
        self.require_finalization_cleanup()?;
        let sources = self
            .input_sources
            .as_mut()
            .context("settled input source frame absent before exclusion retirement")?;
        let Some(alias) = sources.exclusion_alias.as_ref() else {
            return Ok(());
        };
        let registration = self
            .operator_registration
            .as_ref()
            .context("no-Store operator membership absent before exclusion retirement")?;
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let original = &mut *self.original;
        let registry = original
            .physical_registry
            .get_mut()
            .map_err(|_| anyhow::anyhow!("original settled physical registry poisoned"))?
            .as_ref()
            .context("original settled physical registry absent")?;
        ensure!(
            registry.non_account_descriptors_retired(),
            "input exclusion retirement precedes non-operator descriptor receipts"
        );
        let quote = registry.membership_work_bound(1, 0)?.checked_add(
            lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 8 },
        )?;
        registry.prepay_transfer(
            &|bytes, steps| late.prepay(bytes, steps),
            quote.bytes,
            quote.steps,
        )?;
        let lock = (**alias).original_owner_alias()?;
        registry.require_operator_lock_alias(registration, &lock)?;
        drop(lock);
        let alias = sources
            .take_exclusion_alias()
            .expect("validated settled input exclusion alias");
        drop(alias);
        Ok(())
    }

    /// Retire the exact operator target and then its registry-retained parent
    /// after all ordinary descriptor receipts and durable account settlement.
    /// The operator member remains in the same inventory throughout.
    #[cfg(target_os = "linux")]
    pub fn retire_operator_finalization_descriptors_in_store(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_finalization_cleanup()?;
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let prepay = |bytes, steps| late.prepay(bytes, steps);
        let issue_inputs =
            |registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
                owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>| late.outside_retirement_inputs(registration, owner);
        let deadline = late.original_deadline()?;
        // SAFETY: this exact Store registry already holds the canonical target;
        // account settlement is complete and its remaining outside authority is
        // retained in the original settled reservation.
        unsafe {
            outside.retire_finalizer_operator_targets(deadline, &prepay)?;
            outside.retire_finalizer_operator_parent_descriptor(&prepay, &issue_inputs)
        }
    }

    #[cfg(target_os = "linux")]
    pub fn retire_operator_finalization_descriptors_in_reservation(&mut self) -> Result<()> {
        self.require_finalization_cleanup()?;
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let prepay = |bytes, steps| late.prepay(bytes, steps);
        let issue_inputs =
            |registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
                owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>| late.outside_retirement_inputs(registration, owner);
        let original = &mut *self.original;
        let registry = original
            .physical_registry
            .get_mut()
            .map_err(|_| anyhow::anyhow!("original settled physical registry poisoned"))?
            .as_mut()
            .context("original settled physical registry absent")?;
        // SAFETY: the original no-Store reservation owns this same registry;
        // durable settlement and all non-operator receipts are prerequisites
        // enforced by the registry's operator retirement transition.
        late.retire_operator_targets(registry)?;
        unsafe { registry.retire_operator_parent_descriptor(&prepay, &issue_inputs) }
    }

    /// Consume the exact Store-held registry only after every source has an
    /// actual physical close receipt. On refusal the Store keeps the same
    /// registry; on success the opaque proof remains here across clock refusal.
    pub fn retire_finalization_registry_in_store(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        ensure!(
            !matches!(
                self.destination_phase,
                FinalizationDestinationPhase::NoStoreRegistryRetired
            ),
            "Store destination retirement conflicts with the completed no-Store path"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        if matches!(
            self.destination_phase,
            FinalizationDestinationPhase::Pending
        ) {
            ensure!(
                !late.registry_retired(),
                "original registry retired before the Store phase was recorded"
            );
            self.require_finalization_cleanup()?;
            let receipt = outside.retire_finalizer_physical_registry()?;
            late.retain_registry_retirement(receipt)?;
            late.retired_registry()?;
            self.destination_phase = FinalizationDestinationPhase::StoreRegistryRetired;
        }
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreRegistryRetired
                    | FinalizationDestinationPhase::StoreDestinationsRetired
            ),
            "Store destination retirement phase is incomplete"
        );
        Ok(())
    }

    pub fn consume_finalization_packet_destinations_in_store(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if matches!(
            self.destination_phase,
            FinalizationDestinationPhase::StoreDestinationsRetired
        ) {
            return Ok(());
        }
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreRegistryRetired
            ),
            "Store packet destinations cannot retire before the exact Store registry"
        );
        self.require_finalization_cleanup()?;
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let receipt = late.retired_registry()?;
        outside.consume_retired_packet_destinations(receipt, &|bytes, steps| {
            late.prepay(bytes, steps)
        })?;
        self.destination_phase = FinalizationDestinationPhase::StoreDestinationsRetired;
        Ok(())
    }

    /// Consume packet-local bound-root registrations against the exact retired
    /// Store registry after destination permits and before final source-frame
    /// retirement. Refusal preserves each packet registration for retry.
    pub fn consume_finalization_store_packet_bound_roots(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_packet_bound_roots_consumed {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreDestinationsRetired
            ),
            "packet bound roots cannot be consumed before destination retirement"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        outside.consume_retired_packet_bound_roots(late.retired_registry()?, &|bytes, steps| {
            late.prepay(bytes, steps)
        })?;
        self.store_packet_bound_roots_consumed = true;
        Ok(())
    }

    /// Consume exact packet metadata-pin close receipts only after native
    /// disposal. This removes those pin owners from the retained Store packet;
    /// other packet evidence remains with its own source frame.
    pub fn consume_finalization_store_packet_metadata_pins(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_packet_metadata_pins_consumed {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreDestinationsRetired
            ),
            "packet metadata pins cannot be consumed before Store destination retirement"
        );
        ensure!(
            self.store_packet_bound_roots_consumed,
            "packet metadata pins cannot be consumed before bound-root receipts"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        outside
            .consume_retired_packet_metadata_pins(late.retired_registry()?, &|bytes, steps| {
                late.prepay(bytes, steps)
            })?;
        self.store_packet_metadata_pins_consumed = true;
        Ok(())
    }

    /// Exhaust the Store packet sources only after the exact retired-registry
    /// receipts for destinations, roots and metadata pins have been consumed.
    /// Raw Released errors become only the already-cached secret-free source,
    /// ticket and domain fact. Refusal leaves the packet and its fact retained.
    pub fn consume_finalization_store_packet_source_parts(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_packet_source_parts_consumed {
            ensure!(
                self.store_packet_acquisition_failure_count.is_some(),
                "consumed Store packet sources lost their failure-fact disposition"
            );
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreDestinationsRetired
            ) && self.store_packet_bound_roots_consumed
                && self.store_packet_metadata_pins_consumed,
            "Store packet sources cannot be consumed before all exact retired-registry receipts"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let failures = outside
            .consume_retired_packet_source_parts(late.retired_registry()?, &|bytes, steps| {
                late.prepay(bytes, steps)
            })?;
        self.store_packet_acquisition_failure_count = Some(failures);
        self.store_packet_source_parts_consumed = true;
        Ok(())
    }

    /// Consume only the closed Store's State/thread metadata registrations
    /// after their actual source receipts bind to the same retired inventory.
    /// Other Store, packet and final-clock custody remains in its typed owner.
    pub fn consume_finalization_store_metadata_sources(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_metadata_sources_consumed {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreDestinationsRetired
            ),
            "Store metadata sources cannot be consumed before destination retirement"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let quote = quote_finalization_store_metadata_receipts()?;
        late.prepay(quote.bytes, quote.steps)?;
        outside.consume_retired_auxiliary_physical_sources(late.retired_registry()?)?;
        self.store_metadata_sources_consumed = true;
        Ok(())
    }

    /// Consume the normal Store's completed parent frame only after each root
    /// and operator close receipt belongs to the exact retired inventory.
    pub fn consume_finalization_store_parent_sources(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_parent_sources_consumed {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreDestinationsRetired
            ),
            "Store parent sources cannot be consumed before destination retirement"
        );
        ensure!(
            self.store_metadata_sources_consumed,
            "Store parent sources cannot be consumed before Store metadata receipts"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let quote = quote_finalization_store_parent_receipts()?;
        late.prepay(quote.bytes, quote.steps)?;
        outside.consume_retired_parent_finalization_parts(late.retired_registry()?)?;
        self.store_parent_sources_consumed = true;
        Ok(())
    }

    /// Consume State projection-root registration frames only after the same
    /// Store registry, packet destinations, metadata and parent joins retired.
    pub fn consume_finalization_store_state_projection_sources(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_state_projection_sources_consumed {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreDestinationsRetired
            ),
            "State projection sources cannot be consumed before Store destination retirement"
        );
        ensure!(
            self.store_metadata_sources_consumed && self.store_parent_sources_consumed,
            "State projection sources cannot be consumed before Store metadata and parent receipts"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        outside.consume_retired_state_projection_sources(
            late.retired_registry()?,
            &|bytes, steps| late.prepay(bytes, steps),
        )?;
        self.store_state_projection_sources_consumed = true;
        Ok(())
    }

    /// Consume the State and Operational root registrations only after their
    /// projection join and the other Store metadata/parent receipts.
    pub fn consume_finalization_store_state_root_sources(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_state_root_sources_consumed {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreDestinationsRetired
            ),
            "State root sources cannot be consumed before Store destination retirement"
        );
        ensure!(
            self.store_metadata_sources_consumed
                && self.store_parent_sources_consumed
                && self.store_state_projection_sources_consumed,
            "State root sources cannot be consumed before Store metadata, parent, and projection receipts"
        );
        ensure!(
            self.store_packet_bound_roots_consumed
                && self.store_packet_metadata_pins_consumed
                && self.store_packet_source_parts_consumed
                && self.store_packet_acquisition_failure_count.is_some(),
            "State and Operational metadata cannot retire before all Store packet source receipts"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        outside.consume_retired_state_root_sources(late.retired_registry()?, &|bytes, steps| {
            late.prepay(bytes, steps)
        })?;
        self.store_state_root_sources_consumed = true;
        Ok(())
    }

    /// Consume Runtime's duplicate directory/namespace registrations only
    /// after the same Store's packet, metadata, parent, State and root joins.
    pub fn consume_finalization_store_runtime_shared_roots(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_runtime_shared_roots_consumed {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreDestinationsRetired
            ),
            "Runtime shared roots cannot be consumed before Store destination retirement"
        );
        ensure!(
            self.store_metadata_sources_consumed
                && self.store_parent_sources_consumed
                && self.store_state_projection_sources_consumed
                && self.store_state_root_sources_consumed,
            "Runtime shared roots cannot be consumed before Store metadata, parent, State and root receipts"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        outside
            .consume_retired_runtime_shared_roots(late.retired_registry()?, &|bytes, steps| {
                late.prepay(bytes, steps)
            })?;
        self.store_runtime_shared_roots_consumed = true;
        Ok(())
    }

    /// Consume the unopened Scheduler root registrations only after the same
    /// Store's packet, metadata, parent, State, root and Runtime joins.
    pub fn consume_finalization_store_scheduler_sources(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_scheduler_sources_consumed {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreDestinationsRetired
            ),
            "Scheduler sources cannot be consumed before Store destination retirement"
        );
        ensure!(
            self.store_runtime_shared_roots_consumed,
            "Scheduler sources cannot be consumed before the Runtime shared-root receipts"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let quote = quote_finalization_store_scheduler_receipts()?;
        late.prepay(quote.bytes, quote.steps)?;
        outside.consume_retired_scheduler_sources(late.retired_registry()?)?;
        self.store_scheduler_sources_consumed = true;
        Ok(())
    }

    /// Consume the State, Operational and Runtime outside frames once every
    /// receipt stage over the retired registry has completed.
    pub fn consume_finalization_store_owner_frames(
        &mut self,
        outside: &mut ryeos_app::state_store::LiveStateStoreOutsideCustody,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_owner_frames_consumed {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            self.store_scheduler_sources_consumed,
            "Store owner frames cannot be consumed before the Scheduler receipts"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        outside.consume_retired_owner_frames(late.retired_registry()?, &|bytes, steps| {
            late.prepay(bytes, steps)
        })?;
        self.store_owner_frames_consumed = true;
        Ok(())
    }

    /// Final exhaustion of the Store outside graph against the exact retired
    /// registry. The slot is cleared only on success; refusal restores it.
    pub fn exhaust_finalization_store(
        &mut self,
        outside: &mut Option<ryeos_app::state_store::LiveStateStoreOutsideCustody>,
    ) -> Result<()> {
        self.require_original_process()?;
        if self.store_outside_exhausted {
            ensure!(
                outside.is_none(),
                "exhausted Store outside graph reappeared"
            );
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            self.store_owner_frames_consumed,
            "Store outside graph cannot be exhausted before its owner frames"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let quote = quote_finalization_store_exhaustion()?;
        late.prepay(quote.bytes, quote.steps)?;
        let registry = late.retired_registry()?;
        let graph = outside
            .take()
            .context("Store outside graph absent before exhaustion")?;
        match graph.into_exhausted(registry) {
            Ok(exhausted) => {
                // Non-owning record of the checked registry identity only.
                drop(exhausted);
                self.store_outside_exhausted = true;
                Ok(())
            }
            Err((graph, error)) => {
                *outside = Some(graph);
                Err(error)
            }
        }
    }

    /// Record the destination phase of a registry receipt that a refused pass
    /// cached after retirement. The path is the one that retired it: a Store
    /// graph stays retained until after its phase is recorded.
    pub fn resume_finalization_registry_phase(&mut self, store_graph: bool) -> Result<()> {
        self.require_original_process()?;
        if !matches!(
            self.destination_phase,
            FinalizationDestinationPhase::Pending
        ) {
            return Ok(());
        }
        let Some(late) = self.original.late.as_ref() else {
            return Ok(());
        };
        if !late.registry_retired() {
            return Ok(());
        }
        late.retired_registry()?;
        self.destination_phase = if store_graph {
            FinalizationDestinationPhase::StoreRegistryRetired
        } else {
            FinalizationDestinationPhase::NoStoreRegistryRetired
        };
        Ok(())
    }

    pub fn finalization_registry_retired(&self) -> bool {
        self.original
            .late
            .as_ref()
            .is_some_and(|late| late.registry_retired())
    }

    /// The no-Store path consumes only its original pre-mapping registry. If
    /// any descriptor remains, restore that exact registry into the reservation
    /// instead of losing its owners or issuing a replacement.
    pub fn retire_finalization_registry_in_reservation(&mut self) -> Result<()> {
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::Pending
                    | FinalizationDestinationPhase::NoStoreRegistryRetired
            ),
            "no-Store registry retirement conflicts with the Store destination path"
        );
        if matches!(
            self.destination_phase,
            FinalizationDestinationPhase::NoStoreRegistryRetired
        ) {
            return Ok(());
        }
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        ensure!(
            !late.registry_retired(),
            "original registry retired before the no-Store phase was recorded"
        );
        self.require_finalization_cleanup()?;
        let original = &mut *self.original;
        let slot = original
            .physical_registry
            .get_mut()
            .map_err(|_| anyhow::anyhow!("original settled physical registry poisoned"))?;
        let registry = slot
            .take()
            .context("original settled physical registry absent before retirement")?;
        match registry.retire() {
            Ok(receipt) => {
                late.retain_registry_retirement(receipt)?;
                late.retired_registry()?;
                self.destination_phase = FinalizationDestinationPhase::NoStoreRegistryRetired;
                Ok(())
            }
            Err(registry) => {
                *slot = Some(registry);
                anyhow::bail!("original physical registry still has unretired descriptor owners")
            }
        }
    }

    /// Consume settled account/input membership only after every close receipt
    /// is bound to the exact retired registry. The clock stays open in the
    /// same late owner for later source-graph stages; only its input alias is
    /// joined here. A refused alias join restores that exact alias and leaves
    /// every registration frame intact.
    pub fn consume_finalization_control_sources(&mut self) -> Result<()> {
        self.require_original_process()?;
        if self.control_sources_consumed {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        ensure!(
            matches!(
                self.destination_phase,
                FinalizationDestinationPhase::StoreDestinationsRetired
                    | FinalizationDestinationPhase::NoStoreRegistryRetired
            ),
            "settled control sources cannot be consumed before destination retirement"
        );

        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        let quote = quote_finalization_control_source_receipt_join()?;
        late.prepay(quote.bytes, quote.steps)?;
        let retired_registry = late.retired_registry()?;

        let account_sources = self
            .account_sources
            .as_ref()
            .context("settled account source frame absent after registry retirement")?;
        require_finalizer_directory_receipt(
            account_sources.account_parent.as_ref(),
            retired_registry,
        )?;
        require_finalizer_control_receipt(account_sources.account_file.as_ref(), retired_registry)?;
        require_finalizer_directory_receipt(
            account_sources.anchor_parent.as_ref(),
            retired_registry,
        )?;
        require_finalizer_control_receipt(account_sources.anchor_file.as_ref(), retired_registry)?;

        let input_sources = self
            .input_sources
            .as_ref()
            .context("settled admission-input source frame absent after registry retirement")?;
        require_finalizer_directory_receipt(input_sources.root.as_ref(), retired_registry)?;
        require_finalizer_directory_receipt(input_sources.runtime.as_ref(), retired_registry)?;
        require_finalizer_directory_receipt(
            input_sources.completion_parent.as_ref(),
            retired_registry,
        )?;
        require_finalizer_control_receipt(
            input_sources.completion_file.as_ref(),
            retired_registry,
        )?;
        require_finalizer_control_receipt(
            input_sources.completion_duplicate.as_ref(),
            retired_registry,
        )?;
        require_finalizer_directory_receipt(
            input_sources.policy_parent.as_ref(),
            retired_registry,
        )?;
        require_finalizer_control_receipt(input_sources.policy_file.as_ref(), retired_registry)?;
        require_finalizer_control_receipt(
            input_sources.policy_duplicate.as_ref(),
            retired_registry,
        )?;
        ensure!(
            input_sources.exclusion_alias.is_none(),
            "settled admission exclusion alias remains after operator retirement"
        );
        ensure!(
            input_sources.clock_alias.is_some(),
            "settled admission clock alias is absent before its late-owner join"
        );

        match self.destination_phase {
            FinalizationDestinationPhase::NoStoreRegistryRetired => {
                #[cfg(target_os = "linux")]
                {
                    let registration = self
                        .operator_registration
                        .as_ref()
                        .context("no-Store operator membership absent after registry retirement")?;
                    ensure!(
                        registration.is_operator_lock()?
                            && registration.operator_target_retired()?,
                        "no-Store operator target lacks its exact retirement evidence"
                    );
                    drop(registration.receipt_in(retired_registry)?);
                }
                #[cfg(not(target_os = "linux"))]
                ensure!(
                    self.operator_registration.is_none(),
                    "unsupported no-Store operator registration remains"
                );
            }
            FinalizationDestinationPhase::StoreDestinationsRetired => {
                ensure!(
                    self.operator_registration.is_none(),
                    "Store finalization retained a competing reservation operator member"
                );
                ensure!(
                    self.store_metadata_sources_consumed,
                    "Store metadata registrations remain after physical registry retirement"
                );
                ensure!(
                    self.store_parent_sources_consumed,
                    "Store parent registrations remain after physical registry retirement"
                );
                ensure!(
                    self.store_state_projection_sources_consumed,
                    "State projection registrations remain after physical registry retirement"
                );
                ensure!(
                    self.store_state_root_sources_consumed,
                    "State and Operational root registrations remain after physical registry retirement"
                );
                ensure!(
                    self.store_runtime_shared_roots_consumed,
                    "Runtime shared-root registrations remain after physical registry retirement"
                );
                ensure!(
                    self.store_packet_bound_roots_consumed,
                    "packet bound-root registrations remain after physical registry retirement"
                );
                ensure!(
                    self.store_packet_metadata_pins_consumed,
                    "packet metadata-pin receipts remain after physical registry retirement"
                );
                ensure!(
                    self.store_packet_source_parts_consumed
                        && self.store_packet_acquisition_failure_count.is_some(),
                    "Store packet source evidence remains after physical registry retirement"
                );
                ensure!(
                    self.store_scheduler_sources_consumed
                        && self.store_owner_frames_consumed
                        && self.store_outside_exhausted,
                    "Store outside graph is not exhausted after physical registry retirement"
                );
            }
            _ => anyhow::bail!("settled control-source retirement phase changed"),
        }

        let alias = {
            let input_sources = self
                .input_sources
                .as_mut()
                .context("settled admission-input source frame disappeared")?;
            input_sources
                .take_clock_alias()
                .context("settled admission clock alias disappeared after validation")?
        };
        if let Err((alias, error)) = late.join_clock_input_alias(alias, retired_registry) {
            self.input_sources
                .as_mut()
                .expect("validated settled admission-input frame remains present")
                .restore_clock_alias(alias)
                .map_err(|_| {
                    anyhow::anyhow!(
                        "settled admission clock alias slot refilled during a refused join"
                    )
                })?;
            return Err(error);
        }

        self.account_sources = None;
        let input_sources = self
            .input_sources
            .as_mut()
            .expect("validated settled admission-input frame remains present");
        input_sources.end_validated_sources();
        self.operator_registration = None;
        self.control_sources_consumed = true;
        Ok(())
    }

    /// Retire the original late cleanup clock. This is the last clock-bound
    /// effect: the settled control sources must already have joined its
    /// input alias, and no later stage may prepay from the late owner.
    pub fn retire_finalization_clock(&mut self) -> Result<()> {
        self.require_original_process()?;
        ensure!(
            self.control_sources_consumed,
            "late cleanup clock cannot retire before the settled control sources"
        );
        let late = Arc::clone(
            self.original
                .late
                .as_ref()
                .context("settled outside cleanup owner absent")?,
        );
        if late.clock_retired()? {
            return Ok(());
        }
        self.require_finalization_cleanup()?;
        late.retire_clock(late.retired_registry()?)
    }

    /// True once the late clock retired. Only the non-charging final
    /// consumption may follow, so a retry needs no further cleanup window.
    pub fn finalization_clock_retired(&self) -> bool {
        self.require_original_process().is_ok()
            && self
                .original
                .late
                .as_ref()
                .is_some_and(|late| late.clock_retired().unwrap_or(false))
    }

    /// The registry-free part of final consumption: this process, every
    /// finalizer source frame consumed, the destination phase complete, the
    /// late registry and clock retired, and the reservation holding nothing
    /// but its release receipt. A dropped settled owner settles on exactly
    /// these checks.
    fn require_finalized_holders(&self) -> Result<()> {
        self.require_original_process()?;
        ensure!(
            self.control_sources_consumed
                && self.account_sources.is_none()
                && self.operator_registration.is_none(),
            "settled control sources remain before final consumption"
        );
        let inputs = self
            .input_sources
            .as_ref()
            .context("settled admission-input frame absent before final consumption")?;
        ensure!(
            inputs.consumed(),
            "settled admission-input sources remain before final consumption"
        );
        match self.destination_phase {
            FinalizationDestinationPhase::NoStoreRegistryRetired => {}
            FinalizationDestinationPhase::StoreDestinationsRetired => ensure!(
                self.store_metadata_sources_consumed
                    && self.store_parent_sources_consumed
                    && self.store_state_projection_sources_consumed
                    && self.store_state_root_sources_consumed
                    && self.store_runtime_shared_roots_consumed
                    && self.store_packet_bound_roots_consumed
                    && self.store_packet_metadata_pins_consumed
                    && self.store_packet_source_parts_consumed
                    && self.store_packet_acquisition_failure_count.is_some()
                    && self.store_scheduler_sources_consumed
                    && self.store_owner_frames_consumed
                    && self.store_outside_exhausted,
                "settled Store receipts remain before final consumption"
            ),
            _ => anyhow::bail!("settled destination phase is incomplete before final consumption"),
        }
        let late = self
            .original
            .late
            .as_ref()
            .context("settled outside cleanup owner absent before final consumption")?;
        ensure!(
            late.registry_retired() && late.clock_retired()?,
            "late cleanup registry or clock is not retired before final consumption"
        );
        self.original.require_settled_holders()
    }
}

impl SettledLiveNative {
    /// Consume this settled phase into its final non-owning record once the
    /// clock retired and every finalizer source frame was consumed. A refusal
    /// returns the same settled owner unchanged.
    pub fn into_finalized(self) -> std::result::Result<FinalizedLiveNative, (Self, anyhow::Error)> {
        if let Err(error) = self.require_finalized_holders() {
            return Err((self, error));
        }
        let mut settled = match self.held.take_whole() {
            Ok(settled) => settled,
            Err(held) => {
                return Err((
                    Self { held },
                    anyhow::anyhow!("foreign settled live native owner"),
                ));
            }
        };
        let mut original = match settled.original.held.take_whole() {
            Ok(original) => original,
            Err(held) => {
                settled.original = LiveNativeCapacityReservation { held };
                return Err((
                    Self::held(settled),
                    anyhow::anyhow!("foreign settled live native reservation"),
                ));
            }
        };
        // Every holder is released exactly once, in the order final
        // consumption releases them; what is taken here is no longer held.
        drop(settled.account_sources.take());
        drop(settled.operator_registration.take());
        drop(settled.input_sources.take());
        // The input Arc moved into the finalizer frame consumed above; the
        // slot is already empty.
        drop(original.inputs.take());
        drop(
            original
                .account
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        drop(original.budget.take());
        original
            .raw_budget
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        original
            .region
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(original.seal_failure.take());
        drop(
            original
                .physical_registry
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        // Disposed exclusion, released mapping and spent credit are plain
        // post-release records; none closes or unmaps anything on drop.
        original.release_exclusion.take();
        drop(original.host_storage.take());
        original.host_release_credit.take();
        drop(original.late.take());
        // The settled claim is a plain copied observation; nothing to settle.
        original.rollover_claim.take();
        let released = original
            .released
            .take()
            .expect("checked settled release receipt");
        Ok(FinalizedLiveNative {
            process: std::process::id(),
            _released: released,
        })
    }
}

/// Final non-owning record of one settled live native reservation: its
/// physical release receipt only. No account, clock or descriptor remains.
pub struct FinalizedLiveNative {
    process: u32,
    _released: ReleasedReservation,
}
impl FinalizedLiveNative {
    pub fn require_original_process(&self) -> Result<()> {
        ensure!(
            !lillux::exec::in_forked_child() && self.process == std::process::id(),
            "foreign finalized live native record"
        );
        Ok(())
    }
}

impl LiveNativeCapacityReservationFailure {
    /// Recover the SAME retained reservation for supervised recovery. This
    /// does not renew authority, debit again, or certify a resource terminal.
    /// A forked copy must keep its refusal and cannot extract original custody.
    pub fn into_original(self) -> std::result::Result<Arc<LiveNativeCapacityReservation>, Self> {
        if self.original().require_original_process().is_err() {
            return Err(self);
        }
        let Self { original, error } = self;
        original
            .into_taken()
            .map_err(|original| Self { original, error })
    }

    pub fn original(&self) -> &LiveNativeCapacityReservation {
        self.original.original()
    }
}
impl std::fmt::Debug for LiveNativeCapacityReservationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveNativeCapacityReservationFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveNativeCapacityReservationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "original native capacity reservation retained: {}",
            self.error
        )
    }
}
impl std::error::Error for LiveNativeCapacityReservationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl ReservationParts {
    /// The admission inputs this reservation still retains. Absent only after
    /// settlement moved them into finalizer custody.
    pub(super) fn inputs(&self) -> Result<&Arc<LiveNativeCapacityInputs>> {
        retained_inputs(&self.inputs)
    }

    pub(super) fn require_original_process(&self) -> Result<()> {
        self.inputs()?.require_original_process()
    }

    pub(super) fn require_service_edge(&self) -> Result<()> {
        self.inputs()?.require_service_edge()
    }

    /// The registry-free part of final consumption for this reservation: its
    /// admission inputs moved into settled finalizer custody, the physical
    /// release receipt is present, and no native, account, budget, region
    /// or registry owner remains. A dropped reservation settles on exactly
    /// these checks.
    pub(super) fn require_settled_holders(&self) -> Result<()> {
        ensure!(
            self.inputs.is_none(),
            "settled reservation retains its original admission inputs"
        );
        ensure!(
            self.released.is_some() && self.seal_failure.is_none() && self.budget.get().is_none(),
            "settled reservation lacks its release receipt or retains a native owner"
        );
        ensure!(
            self.account
                .try_read()
                .map_err(|_| anyhow::anyhow!("settled account owner busy or poisoned"))?
                .is_none()
                && self
                    .raw_budget
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("settled raw budget busy or poisoned"))?
                    .is_none()
                && self
                    .region
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("settled region owner busy or poisoned"))?
                    .is_none()
                && self
                    .physical_registry
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("settled physical registry busy or poisoned"))?
                    .is_none(),
            "settled reservation retains an account, budget, region or registry owner"
        );
        Ok(())
    }

    pub(super) fn take_prepared_physical_registry(
        &self,
    ) -> Result<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry> {
        self.require_original(true)?;
        let quote = quote_prepared_physical_registry_handoff();
        self.late
            .as_ref()
            .context("original late cleanup absent")?
            .prepay(quote.bytes, quote.steps)?;
        self.physical_registry
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original physical registry busy or poisoned"))?
            .take()
            .context("original prepared physical registry already transferred")
    }

    /// Charge actual outside control storage before allocation. The original
    /// account paid the whole host dimension once; this cursor spends only its
    /// unmapped split. It is not arena storage, new admission or a fresh clock.
    pub(crate) fn prepay_outside_control(&self, memory_bytes: u64, steps: u64) -> Result<()> {
        self.require_original(true)?;
        self.late
            .as_ref()
            .context("original late cleanup partition absent")?
            .prepay(memory_bytes, steps)?;
        // Late refusal retains irreversible work/byte debits and same clock.
        self.require_original(true)
    }

    pub(crate) fn packet_metadata_pin_retirement_inputs(
        &self,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: Arc<lillux_sqlite::OriginalNativeTerminal>,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        pins: &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
    ) -> Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>> {
        self.require_original(true)?;
        let late = self
            .late
            .as_ref()
            .context("original late cleanup partition absent")?;
        late.packet_metadata_pin_retirement_inputs(factory, terminal, leaf, pins)
    }

    pub(crate) fn require_original(&self, cleanup: bool) -> Result<()> {
        self.inputs()?.require_original(cleanup)?;
        self.account_for_check()?
            .as_ref()
            .context("original capacity account owner absent")?
            .require_original_debit(self.inputs()?, cleanup)
    }

    /// Re-check, before new native work is admitted against the resident
    /// region, that its physical residency proof still holds.
    pub(crate) fn confirm_residency(&self) -> Result<()> {
        self.host_storage
            .get()
            .context("original host backing absent")?
            .confirm_residency()
            .map_err(anyhow::Error::msg)
    }

    /// The proof that keeps the resident region in memory, once residency
    /// completed.
    pub fn residency_proof(&self) -> Option<lillux::process_control::ResidencyProof> {
        self.host_storage
            .get()
            .and_then(|storage| storage.residency_proof().cloned())
    }

    /// This incarnation's own process digest and the digest of the settled
    /// incarnation its durable debit followed. Read from the retained account
    /// after its debit; refuses before it or once settlement took the owner.
    pub fn incarnation_lineage(&self) -> Result<([u8; 32], Option<[u8; 32]>)> {
        self.inputs()?.require_original(true)?;
        self.account_for_check()?
            .as_ref()
            .context("original capacity account owner absent")?
            .incarnation_lineage()
    }

    /// Read-only view of the debited account for the boot log and status:
    /// epoch, remaining allowance and recovery attempts. Shares the boundary
    /// read lock and writes nothing.
    pub fn capacity_summary(&self) -> Result<super::account::CapacityAccountSummary> {
        self.inputs()?.require_original(true)?;
        self.account_for_check()?
            .as_ref()
            .context("original capacity account owner absent")?
            .summary(&self.inputs()?.policy)
    }

    /// Durable account verification runs at native operation boundaries,
    /// never per VM step. Boundaries share the account read lock; only
    /// settlement and recovery take it exclusively, and neither overlaps
    /// service SQL. A bounded wait absorbs a momentary writer; past the bound
    /// the check reports contention, which refuses this call without latching
    /// service refusal. A poisoned lock is a fact refusal.
    fn account_for_check(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, Option<RetainedCapacityAccount>>> {
        let mut deadline = None;
        loop {
            match self.account.try_read() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::WouldBlock) => {
                    let deadline = *deadline.get_or_insert_with(|| {
                        lillux::time::MonotonicDeadline::after(ACCOUNT_CHECK_CONTENTION_WAIT)
                    });
                    if deadline.has_elapsed() {
                        return Err(anyhow::Error::new(lillux_sqlite::NativeCheckContended)
                            .context("original capacity account busy"));
                    }
                    std::thread::yield_now();
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    anyhow::bail!("original capacity account poisoned")
                }
            }
        }
    }

    /// A reservation refused before its first mapping never issued the
    /// outside cleanup owner: its account debit was refused (exhausted,
    /// outstanding, overflowed), or the raw counter or its partition was
    /// refused right after the debit. It still retains the same account,
    /// anchor and admission descriptors a mapped reservation retires through
    /// that owner, so settlement constructs the owner and its registry here,
    /// bounded in memory by the signed outside-cleanup floor every start is
    /// admitted against. Nothing durable is charged: no incarnation is
    /// debited and no native counter is issued or transferred. A refusal
    /// here leaves the reservation exactly as it was for the next attempt.
    fn ensure_outside_cleanup_owner(&mut self) -> Result<()> {
        if self.late.is_some() {
            return Ok(());
        }
        ensure!(
            self.host_storage.get().is_none()
                && self.budget.get().is_none()
                && self.released.is_none()
                && self.release_exclusion.is_none()
                && self.seal_failure.is_none()
                && !self.factory_entered.load(Ordering::Acquire),
            "unmapped reservation retains a mapping or native owner without its cleanup owner"
        );
        ensure!(
            self.physical_registry
                .get_mut()
                .map_err(|_| anyhow::anyhow!("original physical registry poisoned"))?
                .is_none(),
            "unmapped reservation retains a physical registry without its cleanup owner"
        );
        let inputs = self.inputs()?;
        inputs.require_original(true)?;
        let ReservationFloors {
            tickets: _,
            host_layout: _,
            outside_control_limit,
            physical_maximum,
            host_work: _,
        } = require_reservation_floors(&inputs.policy.incarnation)?;
        let late = LateNativeControl::from_refused_reservation(
            inputs,
            outside_control_limit,
            inputs.policy.incarnation.outside_cleanup_steps,
            physical_maximum,
        )?;
        // SAFETY: the inventory is funded from this late owner's finite
        // counters, bounded by the floor checked above and debited through
        // the original clock; the owner and registry are cached in this same
        // unmapped reservation, which never maps or issues native work.
        let registry = unsafe {
            lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::issued(
                physical_maximum,
                &|bytes, steps| late.prepay(bytes, steps),
            )
        }?;
        late.bind_original_registry_identity(registry.identity()?)?;
        *self
            .physical_registry
            .get_mut()
            .map_err(|_| anyhow::anyhow!("original physical registry poisoned"))? = Some(registry);
        self.late = Some(late);
        Ok(())
    }

    /// Account-only continuation, under the same original cleanup authority.
    /// A refusal leaves the affine release receipt and the account's exact
    /// predecessor/target in this owner for `failure.into_original()`.
    fn settle_retained_release(&mut self) -> Result<()> {
        self.inputs()?.require_original(true)?;
        let account = self
            .account
            .get_mut()
            .map_err(|_| anyhow::anyhow!("capacity account poisoned"))?
            .as_mut()
            .context("original capacity account owner absent")?;
        match self
            .released
            .as_ref()
            .context("original physical release receipt absent")?
        {
            ReleasedReservation::BeforeRegion(released) => {
                account.settle_before_region(retained_inputs(&self.inputs)?, released)?;
                self.raw_budget
                    .get_mut()
                    .map_err(|_| anyhow::anyhow!("original raw budget poisoned"))?
                    .take();
            }
            ReleasedReservation::Region(released) => {
                // Spend the same outside partition irreversibly prepaid before
                // mapping. Account settlement cannot validate through resident
                // metadata or its now-cleared outstanding debit. Keep the
                // remaining counter in this owner on every late write failure.
                let late = self
                    .late
                    .as_ref()
                    .context("original released region lost its late cleanup owner")?;
                let inputs = retained_inputs(&self.inputs)?;
                // Only a served region release carries the rollover claim; a
                // before-region release above is never a served window.
                let claim = self.rollover_claim.get().copied().flatten();
                account.settle_released(inputs, released, claim, &|steps| {
                    inputs.require_original(true)?;
                    late.prepay(0, steps)
                })?;
            }
        }
        Ok(())
    }

    /// Seal construction before retiring a genuinely uninstalled issuer.
    /// This does not acknowledge native shutdown or release physical backing.
    pub(super) fn close_uninstalled_construction(&self) -> Result<()> {
        self.inputs()?.require_original(true)?;
        let owner = self
            .region
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original region owner busy"))?;
        owner
            .as_ref()
            .context("original construction region absent")?
            .begin_close()?;
        Ok(())
    }

    pub(super) fn region_handle(&self) -> Result<OriginalNativeRegionHandle> {
        self.require_original(false)?;
        self.region
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original region owner busy"))?
            .as_ref()
            .context("original resident region absent")?
            .handle()
            .map_err(anyhow::Error::new)
    }

    pub(super) fn claim_factory_install(&self) -> Result<()> {
        self.require_original(false)?;
        ensure!(
            !self.factory_entered.swap(true, Ordering::AcqRel),
            "original native factory already entered; retain existing owner"
        );
        Ok(())
    }
}

impl LiveNativeCapacityReservation {
    /// Capture has already verified actual operator completion, policy,
    /// exclusions and the original clock. Retain this owner BEFORE its first
    /// irreversible publication. No public report or decoded grant is accepted.
    ///
    /// A refusal before the account owner exists (the admission interval, a
    /// signed envelope floor, or opening the capacity account) is a plain
    /// error: nothing was debited, mapped or issued, and this reference to
    /// the inputs is released with it. Every later refusal carries the
    /// typed owner that retains the account, inputs and every partial
    /// publication for settlement.
    pub fn reserve(inputs: Arc<LiveNativeCapacityInputs>) -> Result<Arc<Self>> {
        inputs.require_original(false)?;
        let units = inputs.policy.incarnation.units;
        let ReservationFloors {
            tickets,
            host_layout,
            outside_control_limit,
            physical_maximum,
            host_work,
        } = require_reservation_floors(&inputs.policy.incarnation)?;
        let outside_cleanup_steps = inputs.policy.incarnation.outside_cleanup_steps;
        let account = RetainedCapacityAccount::open(&inputs)?;
        // Retain the real account and every partial publication by value.
        // Creating the shared reservation must not precede durable debit.
        // This does not fund the remaining input/error/shared allocations.
        let mut owner = Self::held(ReservationParts {
            inputs: Some(inputs),
            account: std::sync::RwLock::new(Some(account)),
            budget: OnceLock::new(),
            raw_budget: Mutex::new(None),
            region: Mutex::new(None),
            release_exclusion: None,
            seal_failure: None,
            resident_generation: None,
            factory_entered: AtomicBool::new(false),
            host_storage: OnceLock::new(),
            host_release_credit: None,
            released: None,
            late: None,
            physical_registry: Mutex::new(None),
            rollover_claim: OnceLock::new(),
        });
        let result = (|| -> Result<()> {
            let original: &mut ReservationParts = &mut owner;
            let mut account_guard = original
                .account
                .try_write()
                .map_err(|_| anyhow::anyhow!("original capacity account busy"))?;
            let account = account_guard
                .as_mut()
                .context("original capacity account owner absent")?;
            account.debit(original.inputs()?)?;
            account.require_original_debit(original.inputs()?, false)?;
            // Release the lock itself, not only the borrowed owner, before
            // later original-authority checks reacquire the same account.
            drop(account_guard);
            // SAFETY: the same original verified allowance durably and
            // irreversibly paid precisely these dimensions before conversion;
            // this retained owner holds original inputs/exclusion/clock and
            // actual account/anchor descriptors through every late error.
            let budget = unsafe {
                OriginalNativeBudget::prepaid_by_original_issuer(
                    units.storage_bytes,
                    units.allocator_steps,
                    tickets,
                    units.cleanup_steps,
                )
            }?;
            // Retain the sole issued counter before any mapping/residency
            // refusal. It moves by value into the region only after residency.
            let mut raw = original
                .raw_budget
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original raw budget busy"))?;
            *raw = Some(budget);
            raw.as_ref()
                .context("original raw budget absent")?
                .debit_steps(host_work, false)?;
            // Transfer the explicit signed split once while the original raw
            // counter is accessible. It cannot be spent again by native cleanup.
            let host_release_steps = original.inputs()?.policy.incarnation.host_release_steps;
            raw.as_ref()
                .context("original raw budget absent")?
                .debit_steps(host_release_steps, true)?;
            // SAFETY: this same raw counter irreversibly transferred the signed
            // cleanup split immediately above, exactly once for this owner.
            original.host_release_credit = Some(unsafe {
                lillux_sqlite::original_host_owner::backing::HostReleaseCredit::prepaid(
                    host_release_steps,
                )
            });
            // Partition the SAME cleanup envelope, leaving its concrete native
            // requirement and signed native headroom available. Publish credit
            // only after the irreversible original raw debit succeeds.
            raw.as_ref()
                .context("original raw budget absent")?
                .debit_steps(outside_cleanup_steps, true)?;
            drop(raw);
            original.late = Some(LateNativeControl::from_native_transfer(
                original.inputs()?,
                outside_control_limit,
                outside_cleanup_steps,
                physical_maximum,
            )?);
            let late = original
                .late
                .as_ref()
                .context("original late cleanup owner absent")?;
            // SAFETY: this exact retained partition was irreversibly prepaid
            // above. Allocate and cache the real finite slot capacity BEFORE
            // mapping so finalization cannot depend on a late allocation of it.
            let registry = unsafe {
                lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::issued(
                    physical_maximum,
                    &|bytes, steps| late.prepay(bytes, steps),
                )
            }?;
            late.bind_original_registry_identity(registry.identity()?)?;
            *original
                .physical_registry
                .get_mut()
                .map_err(|_| anyhow::anyhow!("original physical registry poisoned"))? =
                Some(registry);
            original.require_original(false)?;
            // SAFETY: the original durable account debit and finite signed layout
            // are retained outside before this first mapping effect.
            let storage = unsafe {
                lillux_sqlite::original_host_owner::backing::HostStorage::map(host_layout)
            }
            .map_err(anyhow::Error::msg)?;
            original
                .host_storage
                .set(storage)
                .map_err(|_| anyhow::anyhow!("original host backing already installed"))?;
            let storage = original
                .host_storage
                .get()
                .context("original host backing absent")?;
            // SAFETY: same retained durably debited mapping/layout; each
            // page effect rechecks its actual original service authority.
            unsafe {
                storage.make_resident(&host_layout, || {
                    original
                        .require_original(false)
                        .map_err(|_| "original host residency authority refused")
                })
            }
            .map_err(anyhow::Error::msg)?;
            let generation = original
                .account
                .try_write()
                .map_err(|_| anyhow::anyhow!("original capacity account busy"))?
                .as_mut()
                .context("original capacity account owner absent")?
                .resident_generation(original.inputs()?)?;
            original.resident_generation = Some(generation);
            let (pointer, length) = storage.resident_region().map_err(anyhow::Error::msg)?;
            let mut region_slot = original
                .region
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original resident owner busy"))?;
            let mut raw = original
                .raw_budget
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original raw budget busy"))?;
            let budget = raw
                .take()
                .context("original raw counter already transferred")?;
            // SAFETY: the actual durable debit precedes this retained resident
            // mapping; its account sequence is nonrecycled. This sole outside
            // owner retains account, backing and exact original inputs.
            let region = match unsafe {
                OriginalNativeRegionOwner::initialize(
                    pointer,
                    length,
                    budget,
                    std::process::id(),
                    generation,
                )
            } {
                Ok(region) => region,
                Err(failure) => {
                    let refusal = failure.refusal;
                    *raw = Some(failure.into_budget().map_err(|failure| {
                        anyhow::anyhow!(
                            "retained region counter recovery refused: {}",
                            failure.refusal
                        )
                    })?);
                    return Err(anyhow::Error::new(refusal));
                }
            };
            *region_slot = Some(region);
            drop(raw);
            let handle = region_slot
                .as_ref()
                .context("original region absent")?
                .handle()?;
            original
                .budget
                .set(OriginalNativeBudgetOwner::from_region(handle))
                .map_err(|_| anyhow::anyhow!("original region budget already installed"))?;
            drop(region_slot);
            original.require_original(false)?;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(Arc::new(owner)),
            Err(error) => Err(anyhow::Error::new(LiveNativeCapacityReservationFailure {
                original: RetainedReservationOwner::bootstrap(owner),
                error,
            })),
        }
    }

    pub(crate) fn budget(self: &Arc<Self>) -> Result<Arc<OriginalNativeBudgetOwner>> {
        let result = (|| {
            self.require_original(false)?;
            self.budget
                .get()
                .cloned()
                .context("original native budget not installed")
        })();
        result.map_err(|error| {
            anyhow::Error::new(LiveNativeCapacityReservationFailure {
                original: RetainedReservationOwner::published(Arc::clone(self)),
                error,
            })
        })
    }

    /// The outside supervisor transfers its sole reservation after closing
    /// every real native/service owner. Counts and native terminal evidence,
    /// rather than Arc count alone, authorize resident disposal. `claim` is
    /// what the stop owner observed when it chose an orderly rollover; the
    /// account owner re-proves it before opening an epoch and settles plainly
    /// when any part fails, so passing it never weakens a settlement.
    pub fn release_and_settle(
        self: Arc<Self>,
        claim: Option<RolloverClaim>,
    ) -> std::result::Result<SettledLiveNative, LiveNativeCapacityReservationFailure> {
        let mut owner = match Arc::try_unwrap(self) {
            Ok(owner) => owner,
            Err(original) => {
                return Err(LiveNativeCapacityReservationFailure {
                    original: RetainedReservationOwner::published(original),
                    error: anyhow::anyhow!("original reservation still has outside owners"),
                });
            }
        };
        let result = (|| -> Result<()> {
            let original: &mut ReservationParts = &mut owner;
            // Fix the claim before any check can refuse: the first call's
            // observation is the one every retry settles under.
            original.rollover_claim.get_or_init(|| claim);
            original.inputs()?.require_original(true)?;
            // Match the outside receipt before consulting any region state.
            // The original mapping may already be gone and account writes may
            // be partially complete. Reuse the same owner/target, never release
            // again or mint another proof from copied generation facts.
            if original.released.is_some() {
                return original.settle_retained_release();
            }
            if original
                .region
                .get_mut()
                .map_err(|_| anyhow::anyhow!("resident owner poisoned"))?
                .is_none()
                && original.seal_failure.is_none()
                && original.release_exclusion.is_none()
            {
                ensure!(
                    !original.factory_entered.load(Ordering::Acquire)
                        && original.budget.get().is_none(),
                    "original region publication/factory custody is unresolved"
                );
                original.ensure_outside_cleanup_owner()?;
                // SAFETY: Arc::try_unwrap transferred the actual sole outside
                // owner; original PID/cleanup checked above. This attempt has
                // no initialized region/seal/publication, no budget facade or
                // factory, so no counted/raw/native loan escaped. A refused
                // initializer restored its same raw budget before this branch.
                let released = match original.host_storage.get_mut() {
                    Some(storage) => unsafe {
                        storage.release_before_region(
                            original
                                .host_release_credit
                                .as_mut()
                                .context("original host release credit absent")?,
                            || {
                                retained_inputs(&original.inputs)
                                    .and_then(|inputs| inputs.require_original(true))
                                    .map_err(|_| "original pre-region cleanup authority refused")?;
                                Ok(())
                            },
                        )
                    }
                    .map_err(anyhow::Error::msg)?,
                    // Same actual outside state additionally has no mapping.
                    None => unsafe {
                        lillux_sqlite::original_host_owner::backing::HostStorageReleased::never_mapped()
                    },
                };
                original.released = Some(ReleasedReservation::BeforeRegion(released));
                return original.settle_retained_release();
            }
            // Acquire the destination before consuming failure custody. A
            // poisoned destination cannot strand the extracted original token.
            let owner_slot = original
                .region
                .get_mut()
                .map_err(|_| anyhow::anyhow!("resident owner poisoned"))?;
            if let Some(failure) = original.seal_failure.take() {
                match failure.into_owner() {
                    Ok(owner) => *owner_slot = Some(owner),
                    Err(failure) => {
                        original.seal_failure = Some(failure);
                        anyhow::bail!("original seal custody transfer refused");
                    }
                }
            }
            if original.release_exclusion.is_none() {
                let owner = original
                    .region
                    .get_mut()
                    .map_err(|_| anyhow::anyhow!("resident owner poisoned"))?
                    .as_ref()
                    .context("original resident owner absent; partial-start recovery required")?;
                retained_inputs(&original.inputs)?.require_original(true)?;
                owner.begin_close()?;
                // The sole local facade is retired only after admission is
                // closed. All other real handles still block the lower seal.
                drop(original.budget.take());
                owner.retire_pending()?;
                retained_inputs(&original.inputs)?.require_original(true)?;
                let owner = original
                    .region
                    .get_mut()
                    .map_err(|_| anyhow::anyhow!("resident owner poisoned"))?
                    .take()
                    .context("original resident owner transfer absent")?;
                match owner.seal() {
                    Ok(exclusion) => original.release_exclusion = Some(exclusion),
                    Err(failure) => {
                        let refusal = failure.refusal;
                        original.seal_failure = Some(failure);
                        return Err(anyhow::Error::new(refusal));
                    }
                }
            }
            let exclusion = original
                .release_exclusion
                .as_mut()
                .context("resident release exclusion absent")?;
            ensure!(
                Some(exclusion.original_generation()?) == original.resident_generation,
                "resident release selects foreign durable generation"
            );
            retained_inputs(&original.inputs)?.require_original(true)?;
            exclusion.dispose()?;
            // SAFETY: this exclusion was retained from sealing this same
            // reservation's initialized region, whose span came only from
            // its actual backing. Sole outside custody, original generation
            // and zero native/Rust/C loans were checked before disposal.
            // No other owner can access this mapping during physical release.
            let released = unsafe {
                original
                    .host_storage
                    .get_mut()
                    .context("original physical backing absent")?
                    .release(
                        exclusion,
                        original
                            .host_release_credit
                            .as_mut()
                            .context("original host release credit absent")?,
                        || {
                            retained_inputs(&original.inputs)
                                .and_then(|inputs| inputs.require_original(true))
                                .map_err(|_| "original host cleanup authority refused")?;
                            Ok(())
                        },
                    )
            }
            .map_err(anyhow::Error::msg)?;
            original.released = Some(ReleasedReservation::Region(released));
            original.settle_retained_release()
        })();
        match result {
            Ok(()) => Ok(SettledLiveNative::begin(owner)),
            Err(error) => Err(LiveNativeCapacityReservationFailure {
                original: RetainedReservationOwner::bootstrap(owner),
                error,
            }),
        }
    }
}

#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const RESERVATION: &str = "ryeos-node::native-capacity-reservation";
    const SETTLED: &str = "ryeos-node::native-capacity-settled";
    const FAILURE: &str = "ryeos-node::native-capacity-reservation-failure";

    /// A reservation that holds nothing: inputs already transferred, no
    /// account, native, region or registry owner. With `released` it is
    /// exactly what final consumption accepts.
    fn empty_reservation(released: bool) -> LiveNativeCapacityReservation {
        LiveNativeCapacityReservation::held(ReservationParts {
            inputs: None,
            account: std::sync::RwLock::new(None),
            budget: OnceLock::new(),
            raw_budget: Mutex::new(None),
            region: Mutex::new(None),
            release_exclusion: None,
            seal_failure: None,
            resident_generation: None,
            factory_entered: AtomicBool::new(false),
            host_storage: OnceLock::new(),
            host_release_credit: None,
            // SAFETY: a test-only release fact for a reservation that never
            // mapped, published or retained anything.
            released: released.then(|| {
                ReleasedReservation::BeforeRegion(unsafe { HostStorageReleased::never_mapped() })
            }),
            late: None,
            physical_registry: Mutex::new(None),
            rollover_claim: OnceLock::new(),
        })
    }

    fn settled_phase(original: LiveNativeCapacityReservation) -> SettledLiveNative {
        SettledLiveNative::begin(original)
    }

    #[test]
    fn dropped_settled_reservation_releases_normally() {
        let _serial = super::super::abandoned_custody_serial();
        let reservation = empty_reservation(true);
        reservation.require_settled_holders().unwrap();
        assert!(reservation.inputs().is_err());
        let before = lillux::custody::abandoned_custody().count(RESERVATION);
        drop(reservation);
        assert_eq!(
            lillux::custody::abandoned_custody().count(RESERVATION),
            before
        );
    }

    #[test]
    fn dropped_reservation_without_release_receipt_is_retained() {
        let _serial = super::super::abandoned_custody_serial();
        let reservation = empty_reservation(false);
        assert!(reservation.require_settled_holders().is_err());
        let before = lillux::custody::abandoned_custody().count(RESERVATION);
        drop(reservation);
        assert_eq!(
            lillux::custody::abandoned_custody().count(RESERVATION),
            before + 1
        );
    }

    #[test]
    fn dropped_settled_phase_before_final_consumption_is_retained_whole() {
        let _serial = super::super::abandoned_custody_serial();
        let settled = settled_phase(empty_reservation(true));
        assert!(settled.require_finalized_holders().is_err());
        // A refused final consumption hands the same owner back unchanged.
        let (settled, _error) = settled
            .into_finalized()
            .err()
            .expect("unconsumed frames refuse");
        let before = lillux::custody::abandoned_custody();
        drop(settled);
        let after = lillux::custody::abandoned_custody();
        assert_eq!(after.count(SETTLED), before.count(SETTLED) + 1);
        // The reservation went with its settled owner; its own Drop never ran.
        assert_eq!(after.count(RESERVATION), before.count(RESERVATION));
    }

    #[test]
    fn dropped_retained_reservation_owner_is_abandoned_and_taken_one_is_not() {
        let _serial = super::super::abandoned_custody_serial();
        let before = lillux::custody::abandoned_custody();
        drop(RetainedReservationOwner::bootstrap(empty_reservation(true)));
        let after = lillux::custody::abandoned_custody();
        assert_eq!(after.count(FAILURE), before.count(FAILURE) + 1);
        assert_eq!(after.count(RESERVATION), before.count(RESERVATION));

        let published = Arc::new(empty_reservation(true));
        let alias = Arc::clone(&published);
        let taken = RetainedReservationOwner::published(published)
            .into_taken()
            .map_err(|_| ())
            .expect("the issuing process takes its own reservation");
        assert!(Arc::ptr_eq(&taken, &alias));
        drop(alias);
        drop(taken);
        let settled = lillux::custody::abandoned_custody();
        assert_eq!(settled.count(FAILURE), after.count(FAILURE));
        assert_eq!(settled.count(RESERVATION), after.count(RESERVATION));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisional_registry_floor_includes_capacity_and_metadata_sources() {
        let (maximum, floor) = minimum_registry_cleanup(64).unwrap();
        assert_eq!(maximum, 412);
        assert_eq!(maximum_physical_registration_attempts(64).unwrap(), 412);
        let registration =
            lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::registration_quote(
                412, 1_648,
            )
            .unwrap();
        let allocation =
            lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::storage_quote(412)
                .unwrap();
        let late_owner = LateNativeControl::maximum_storage_quote(412).unwrap();
        let retirement_inputs =
            LateNativeControl::maximum_retirement_inputs_quote(412, 64).unwrap();
        let packet_pins =
            LateNativeControl::maximum_packet_metadata_pin_retirement_quote().unwrap();
        let state_pins =
            LateNativeControl::maximum_state_metadata_pin_retirement_quote(64).unwrap();
        assert!(
            state_pins.steps > 0
                && state_pins.bytes > 0
                && retirement_inputs.steps >= state_pins.steps + packet_pins.steps
        );
        assert_eq!(registration.bytes, 843_776);
        assert_eq!(registration.steps, 692_160);
        assert_eq!(floor, allocation.checked_add(registration).unwrap());
        assert_eq!(floor.steps, 695_456);
        assert_eq!(late_owner.steps, 413);
        assert!(packet_pins.steps > 0 && packet_pins.bytes > 0);
        assert!(retirement_inputs.steps > 47_600);
        assert_eq!(
            floor
                .checked_add(late_owner)
                .unwrap()
                .checked_add(retirement_inputs)
                .unwrap()
                .steps,
            695_456 + 413 + retirement_inputs.steps
        );
        assert_eq!(
            quote_prepared_physical_registry_handoff(),
            lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 2 }
        );
        assert_eq!(
            quote_finalization_control_source_receipt_join().unwrap(),
            lillux_sqlite::original_physical_custody::OutsideWork {
                bytes: 0,
                steps: 120
            }
        );
        assert_eq!(
            quote_finalization_store_metadata_receipts().unwrap(),
            lillux_sqlite::original_physical_custody::OutsideWork {
                bytes: 0,
                steps: 72
            }
        );
        assert_eq!(
            quote_finalization_store_parent_receipts().unwrap(),
            lillux_sqlite::original_physical_custody::OutsideWork {
                bytes: 0,
                steps: 48
            }
        );
        assert_eq!(
            quote_finalization_store_scheduler_receipts().unwrap(),
            lillux_sqlite::original_physical_custody::OutsideWork {
                bytes: 0,
                steps: 32
            }
        );
        assert_eq!(
            quote_finalization_store_exhaustion().unwrap(),
            lillux_sqlite::original_physical_custody::OutsideWork {
                bytes: 0,
                steps: 16
            }
        );
        let (maximum, required) = required_outside_cleanup(64).unwrap();
        assert_eq!(maximum, 412);
        assert!(required.steps > floor.steps + late_owner.steps + retirement_inputs.steps);
    }

    /// Every shipped init profile that declares native capacity must pass the
    /// same floor checks `reserve` runs, so a fresh node can start.
    #[test]
    fn shipped_profiles_satisfy_reservation_floors() {
        use ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy;
        let profiles = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/.ai/node/init/profiles");
        let mut entries = std::fs::read_dir(&profiles)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "yaml")
            })
            .collect::<Vec<_>>();
        entries.sort();
        assert!(entries.iter().any(|path| path.ends_with("core.yaml")));
        let mut checked = 0usize;
        for path in entries {
            let document: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let Some(section) = document
                .get("policies")
                .and_then(|p| p.get("native_capacity"))
            else {
                continue;
            };
            let policy: NodeNativeCapacityPolicy = serde_yaml::from_value(section.clone())
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            policy
                .validate()
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let tickets = u32::try_from(policy.incarnation.units.connection_tickets).unwrap();
            let (_, required) = required_outside_cleanup(tickets).unwrap();
            assert!(
                policy.incarnation.outside_cleanup_steps >= required.steps,
                "{}: outside_cleanup_steps {} below floor {}",
                path.display(),
                policy.incarnation.outside_cleanup_steps,
                required.steps
            );
            require_reservation_floors(&policy.incarnation)
                .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
            eprintln!(
                "{}: rollover_drain_margin_ms {} (floor {:?}), cleanup_occupancy_ms {} (floor {:?})",
                path.display(),
                policy.incarnation.rollover_drain_margin_ms,
                required_rollover_drain_margin(),
                policy.incarnation.cleanup_occupancy_ms,
                required_cleanup_occupancy()
            );
            checked += 1;
        }
        assert!(checked > 0, "no shipped profile declares native capacity");
    }
}
