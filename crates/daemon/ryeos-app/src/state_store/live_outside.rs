//! Actual live Store graph after aggregate native acknowledgement. This owner
//! keeps both normal and failed-start resources accessible to the node supervisor;
//! it is not a physical-disposal receipt or durable account settlement.
use super::{ClosedLiveStateStore, ReleasedLiveStateStoreStartup};
use anyhow::{Context, Result, ensure};
use lillux::custody::Held;
use lillux_sqlite::original_physical_custody::OutsideWork;
use std::sync::Arc;

const MAX_DESTINATION_RETRIES: u8 = 4;
const DESTINATION_RETRY_VALIDATION_STEPS: u64 = 4;
const RETIRED_PACKET_ROOT_RECEIPT_STEPS: u64 = 18;
const RETIRED_PACKET_METADATA_PIN_RECEIPT_STEPS: u64 = 8;
const PACKET_OPTIONAL_SOURCE_RECEIPT_STEPS: u64 = 4;
const PACKET_METADATA_PIN_RETIREMENT_STATE_STEPS: u64 = 24;
const PACKET_OPTIONAL_SOURCE_CENSUS_STEPS: u64 = 8;
const RETIRED_PACKET_SOURCE_PARTS_STEPS: u64 = 24;
const MAX_RUNTIME_PACKET_SOURCES: usize = crate::runtime_db::MAX_LIVE_RUNTIME_NATIVE_PACKET_SOURCES;
const MAX_OPERATIONAL_PACKET_SOURCES: usize =
    ryeos_state::operational::MAX_LIVE_OPERATIONAL_PACKET_SOURCES;
pub const MAX_LIVE_SCHEDULER_NATIVE_PACKET_SOURCES: usize = 2;
/// The accounting ledger's bound connection and its schema reference.
pub const MAX_LIVE_ACCOUNTING_NATIVE_PACKET_SOURCES: usize = 2;
/// The Store's direct auxiliary packet collection carries both the Scheduler
/// and the accounting ledger node databases.
pub const MAX_LIVE_AUXILIARY_NATIVE_PACKET_SOURCES: usize =
    MAX_LIVE_SCHEDULER_NATIVE_PACKET_SOURCES + MAX_LIVE_ACCOUNTING_NATIVE_PACKET_SOURCES;
pub const MAX_LIVE_SELECTED_NATIVE_PACKET_SOURCES: usize = MAX_RUNTIME_PACKET_SOURCES
    + MAX_OPERATIONAL_PACKET_SOURCES
    + MAX_LIVE_AUXILIARY_NATIVE_PACKET_SOURCES;
const MAX_SELECTED_PACKET_SOURCES: usize = MAX_LIVE_SELECTED_NATIVE_PACKET_SOURCES;
const MAX_SELECTED_PACKET_COLLECTIONS: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OriginalPacketSourceKind {
    Runtime,
    Operational,
    Scheduler,
}
impl OriginalPacketSourceKind {
    fn maximum_sources(self) -> usize {
        match self {
            Self::Runtime => MAX_RUNTIME_PACKET_SOURCES,
            Self::Operational => MAX_OPERATIONAL_PACKET_SOURCES,
            Self::Scheduler => MAX_LIVE_AUXILIARY_NATIVE_PACKET_SOURCES,
        }
    }
}

fn destination_retry_attempt_quote() -> OutsideWork {
    OutsideWork {
        bytes: 0,
        steps: DESTINATION_RETRY_VALIDATION_STEPS,
    }
}

pub fn quote_destination_retries(packet_count: usize) -> Result<OutsideWork> {
    ensure!(
        packet_count <= MAX_SELECTED_PACKET_SOURCES,
        "original destination retry quote exceeds the selected Runtime, Operational, and Scheduler source bounds"
    );
    let steps = u64::try_from(packet_count)?
        .checked_mul(u64::from(MAX_DESTINATION_RETRIES))
        .and_then(|passes| passes.checked_mul(DESTINATION_RETRY_VALIDATION_STEPS))
        .context("original destination retry work quote overflow")?;
    Ok(OutsideWork { bytes: 0, steps })
}

pub fn quote_packet_parts(packet_count: usize, collection_count: usize) -> Result<OutsideWork> {
    ensure!(
        packet_count <= MAX_SELECTED_PACKET_SOURCES
            && collection_count <= MAX_SELECTED_PACKET_COLLECTIONS,
        "original packet progress quote exceeds the selected producer collections"
    );
    let bytes = packet_count
        .checked_mul(std::mem::size_of::<OriginalPacketPartsProgress>())
        .and_then(|bytes| {
            collection_count
                .checked_mul(std::mem::size_of::<Vec<OriginalPacketPartsProgress>>())
                .and_then(|headers| bytes.checked_add(headers))
        })
        .context("original packet progress storage quote overflow")?;
    // Includes exact producer-boundary census of the optional inspection,
    // Scheduler-retirement, and acquisition-error fields for each packet.
    let per_packet_steps = 4u64
        .checked_add(PACKET_OPTIONAL_SOURCE_CENSUS_STEPS)
        .context("original packet source census work quote overflow")?;
    let packet_steps = u64::try_from(packet_count)?
        .checked_mul(per_packet_steps)
        .context("original packet progress item work quote overflow")?;
    let collection_steps = u64::try_from(collection_count)?
        .checked_mul(4)
        .context("original packet progress collection work quote overflow")?;
    let steps = packet_steps
        .checked_add(collection_steps)
        .context("original packet progress work quote overflow")?;
    Ok(OutsideWork {
        bytes: u64::try_from(bytes)?,
        steps,
    })
}

/// Quote one post-disposal exact-registry receipt for each packet-owned bound
/// root registration that remains after destination permits are consumed.
pub fn quote_retired_packet_bound_roots(packet_count: usize) -> Result<OutsideWork> {
    ensure!(
        packet_count <= MAX_SELECTED_PACKET_SOURCES,
        "retired packet-root quote exceeds the selected Runtime, Operational, and Scheduler source bounds"
    );
    let steps = u64::try_from(packet_count)?
        .checked_mul(RETIRED_PACKET_ROOT_RECEIPT_STEPS)
        .context("retired packet-root receipt work quote overflow")?;
    Ok(OutsideWork { bytes: 0, steps })
}

fn quote_retired_packet_metadata_pin_receipt() -> Result<OutsideWork> {
    let bytes = std::mem::size_of::<lillux_sqlite::RetiredOriginalMetadataPin>()
        .checked_add(4 * std::mem::size_of::<usize>())
        .context("retired packet metadata-pin receipt storage quote overflow")?;
    Ok(OutsideWork {
        bytes: u64::try_from(bytes)?,
        steps: RETIRED_PACKET_METADATA_PIN_RECEIPT_STEPS,
    })
}

/// Per-pass quote for packet metadata-pin receipt allocation and exact-owner
/// validation, plus the cached optional-source census check. The native close
/// itself is charged by Lillux's pin authority.
pub fn quote_retired_packet_metadata_pin_receipts(packet_count: usize) -> Result<OutsideWork> {
    ensure!(
        packet_count <= MAX_SELECTED_PACKET_SOURCES,
        "retired packet metadata-pin quote exceeds the selected producer bound"
    );
    let pins = packet_count
        .checked_mul(2)
        .context("retired packet metadata-pin count overflow")?;
    let quote = quote_retired_packet_metadata_pin_receipt()?;
    let pin_steps = quote
        .steps
        .checked_mul(u64::try_from(pins)?)
        .context("retired packet metadata-pin work quote overflow")?;
    let census_steps = u64::try_from(packet_count)?
        .checked_mul(PACKET_OPTIONAL_SOURCE_RECEIPT_STEPS)
        .context("retired packet optional-source census quote overflow")?;
    Ok(OutsideWork {
        bytes: quote
            .bytes
            .checked_mul(u64::try_from(pins)?)
            .context("retired packet metadata-pin storage quote overflow")?,
        steps: pin_steps
            .checked_add(census_steps)
            .context("retired packet receipt and census work quote overflow")?,
    })
}

/// Per-attempt work to validate each exact packet's source census and advance
/// its retained metadata-pin retirement phase. Pin close and receipt work have
/// separate quotes because Lillux and the receipt owner charge those effects.
pub fn quote_packet_metadata_pin_retirements(packet_count: usize) -> Result<OutsideWork> {
    ensure!(
        packet_count <= MAX_SELECTED_PACKET_SOURCES,
        "packet metadata-pin retirement quote exceeds the selected producer bound"
    );
    let steps = u64::try_from(packet_count)?
        .checked_mul(PACKET_METADATA_PIN_RETIREMENT_STATE_STEPS)
        .context("packet metadata-pin retirement state work quote overflow")?;
    Ok(OutsideWork { bytes: 0, steps })
}

/// Per-pass work to verify all packet-local receipts and typed absences, retain
/// only the sanitized failure fact, and exhaust the lower packet source after
/// native disposal. This transition does not allocate additional storage.
pub fn quote_retired_packet_source_parts(packet_count: usize) -> Result<OutsideWork> {
    ensure!(
        packet_count <= MAX_SELECTED_PACKET_SOURCES,
        "retired packet-source quote exceeds the selected producer bound"
    );
    let steps = u64::try_from(packet_count)?
        .checked_mul(RETIRED_PACKET_SOURCE_PARTS_STEPS)
        .context("retired packet-source work quote overflow")?;
    Ok(OutsideWork { bytes: 0, steps })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PacketMetadataPinPhase {
    Unclassified,
    Absent,
    Retiring(u8),
    Retired(u8),
    ConsumedAbsent,
    Consumed(u8),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PacketAcquisitionFailureFact {
    source_kind: OriginalPacketSourceKind,
    ticket: Option<u32>,
    domain: Option<lillux_sqlite::OriginalNativeDomain>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PacketOptionalSourceCensus {
    producer: OriginalPacketSourceKind,
    closed_inspection_absent: bool,
    released_inspection_absent: bool,
    scheduler_retirement_absent: bool,
    acquisition_failure: Option<PacketAcquisitionFailureFact>,
    failure_fact_recorded: bool,
}

fn next_destination_retry(current: u8) -> Result<u8> {
    current
        .checked_add(1)
        .filter(|retry| *retry <= MAX_DESTINATION_RETRIES)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "original destination retry allowance exhausted; actual refusal custody retained"
            )
        })
}

fn require_packet_count_within_source_bound(
    packet_count: usize,
    source_kind: OriginalPacketSourceKind,
) -> Result<()> {
    let maximum = source_kind.maximum_sources();
    ensure!(
        maximum > 0 && packet_count <= maximum,
        "original native packet progress exceeds its producer-owned source bound"
    );
    Ok(())
}

fn is_accounting_packet_domain(domain: Option<lillux_sqlite::OriginalNativeDomain>) -> bool {
    matches!(
        domain,
        Some(
            lillux_sqlite::OriginalNativeDomain::NodeAccounting
                | lillux_sqlite::OriginalNativeDomain::NodeAccountingSchemaReference
        )
    )
}

/// The auxiliary collection is shared, but each producer keeps its own bound:
/// at most the Scheduler's packets and at most the accounting ledger's. A
/// total within the shared maximum never admits a third Scheduler packet.
fn require_auxiliary_packet_domains_within_bounds(
    domains: impl Iterator<Item = Option<lillux_sqlite::OriginalNativeDomain>>,
) -> Result<()> {
    let (mut scheduler, mut accounting) = (0usize, 0usize);
    for domain in domains {
        if is_accounting_packet_domain(domain) {
            accounting += 1;
        } else {
            scheduler += 1;
        }
    }
    ensure!(
        scheduler <= MAX_LIVE_SCHEDULER_NATIVE_PACKET_SOURCES,
        "original Scheduler packet progress exceeds its producer-owned source bound"
    );
    ensure!(
        accounting <= MAX_LIVE_ACCOUNTING_NATIVE_PACKET_SOURCES,
        "original accounting packet progress exceeds its producer-owned source bound"
    );
    Ok(())
}

fn producer_metadata_pin_phase(
    parts: &lillux_sqlite::OriginalNativeOutsideConnectionParts,
) -> PacketMetadataPinPhase {
    let mask = parts
        .durable_pins
        .iter()
        .enumerate()
        .fold(0u8, |mask, (index, pin)| {
            mask | (u8::from(pin.is_some()) << index)
        });
    if mask == 0 {
        PacketMetadataPinPhase::Absent
    } else {
        PacketMetadataPinPhase::Retiring(mask)
    }
}

// Exact original packet components, retained before any later destination,
// inspection or descriptor continuation. This is not a retirement receipt.
struct OriginalPacketBoundResources {
    held: Held<OriginalPacketBoundParts>,
}
/// Everything a bound-resources owner retains, in the order it ends.
struct OriginalPacketBoundParts {
    root: Option<std::sync::Arc<lillux::PinnedDirectory>>,
    inspection: Option<(
        lillux_sqlite::SqliteCopyLease,
        std::sync::Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
    )>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
    _name: lillux_sqlite::OriginalNativeOutsideBoundCoordinate,
    _mode: lillux_sqlite::OriginalNativeBoundMode,
    _domain: lillux_sqlite::OriginalNativeDomain,
}
impl OriginalPacketBoundParts {
    /// No root, inspection or registration remains: the retired source
    /// evidence left nothing behind.
    fn holders_retired(&self) -> bool {
        self.root.is_none() && self.inspection.is_none() && self.registration.is_none()
    }
}
/// Settled exactly when no root, inspection or registration remains;
/// otherwise the bound root and inspection lease stay retained together until
/// the process exits.
fn packet_bound_settled(parts: &OriginalPacketBoundParts) -> bool {
    parts.holders_retired()
}
impl OriginalPacketBoundResources {
    fn new(
        root: Option<std::sync::Arc<lillux::PinnedDirectory>>,
        inspection: Option<(
            lillux_sqlite::SqliteCopyLease,
            std::sync::Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
        )>,
        name: lillux_sqlite::OriginalNativeOutsideBoundCoordinate,
        mode: lillux_sqlite::OriginalNativeBoundMode,
        domain: lillux_sqlite::OriginalNativeDomain,
    ) -> Self {
        Self {
            held: Held::new(
                "ryeos-app::packet-bound-resources",
                OriginalPacketBoundParts {
                    root,
                    inspection,
                    registration: None,
                    _name: name,
                    _mode: mode,
                    _domain: domain,
                },
                packet_bound_settled,
            ),
        }
    }
}
pub(super) struct OriginalPacketPartsProgress {
    held: Held<OriginalPacketProgressParts>,
    source_kind: OriginalPacketSourceKind,
    destination_retries: u8,
    destination_started: bool,
    requires_destination: bool,
    destination_roots_registered: bool,
    destination_native_references_retired: bool,
    destination_consumed: bool,
    bound_root_receipt_consumed: bool,
    metadata_pin_phase: PacketMetadataPinPhase,
}
/// Everything a packet progress retains, in the order it ends.
struct OriginalPacketProgressParts {
    bound: Option<OriginalPacketBoundResources>,
    authority: Option<std::sync::Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>>,
    destination: Option<lillux_sqlite::OriginalDestinationOutsideCustody>,
    destination_retirement: Option<lillux_sqlite::OriginalDestinationRetirement>,
    metadata_pin_inputs: Option<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
    metadata_pin_receipts: [Option<Arc<lillux_sqlite::RetiredOriginalMetadataPin>>; 2],
    raw: Option<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
    parts: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    refusal: Option<lillux_sqlite::OriginalDestinationCustodyFailure>,
    source_parts_consumed: bool,
    optional_source_census: Option<PacketOptionalSourceCensus>,
    scheduler_handoff: Option<(
        ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
        ryeos_scheduler::db::SchedulerNativeStoreSourceRole,
    )>,
    // Producer domain copied when an auxiliary packet joins; it selects the
    // per-domain bound inside the shared Scheduler/accounting collection.
    native_domain: Option<lillux_sqlite::OriginalNativeDomain>,
    metadata_pin_registry_identity:
        Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
}
impl OriginalPacketProgressParts {
    /// Source parts consumed and no source, destination, root or pin owner
    /// left.
    fn exhausted(&self) -> bool {
        self.source_parts_consumed
            && self.raw.is_none()
            && self.parts.is_none()
            && self.bound.is_none()
            && self.authority.is_none()
            && self.destination.is_none()
            && self.destination_retirement.is_none()
            && self.refusal.is_none()
            && self.metadata_pin_inputs.is_none()
            && self.metadata_pin_receipts.iter().all(Option::is_none)
    }
}
/// Settled exactly when exhaustion would accept the packet; otherwise every
/// owner stays retained together until the process exits.
fn packet_progress_settled(parts: &OriginalPacketProgressParts) -> bool {
    parts.exhausted()
}

fn register_bound_root_source(
    root: &mut Option<Arc<lillux::PinnedDirectory>>,
    registration: &mut Option<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
    >,
    registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    if root.is_none() {
        let binding = registration
            .as_ref()
            .context("completed actual bound-root source lacks its registration")?;
        registry.require_membership(binding)?;
        ensure!(matches!(binding.observed_source_identity()?,
            lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
            "actual packet bound-root registration changed role");
        return Ok(());
    }
    ensure!(
        registration.is_none(),
        "actual bound-root source competes with a cached registration"
    );
    let quote = registry.membership_work_bound(2, 1)?;
    registry.prepay_transfer(
        prepay,
        0,
        quote
            .steps
            .checked_add(8)
            .context("packet bound-root registration work overflow")?,
    )?;
    let source = root.take().expect("checked actual bound-root source");
    match unsafe { registry.register_directory(source, prepay) } {
        Ok(binding) => *registration = Some(binding),
        Err((source, error)) => {
            *root = Some(source);
            return Err(error);
        }
    }
    let binding = registration
        .as_ref()
        .context("actual packet bound-root registration absent after transfer")?;
    registry.require_membership(binding)?;
    ensure!(
        matches!(
            binding.observed_source_identity()?,
            lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }
        ),
        "actual packet bound-root registration changed role"
    );
    Ok(())
}

impl OriginalPacketPartsProgress {
    fn new_progress(
        raw: Option<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
        parts: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
        scheduler_handoff: Option<(
            ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
            ryeos_scheduler::db::SchedulerNativeStoreSourceRole,
        )>,
        source_kind: OriginalPacketSourceKind,
        native_domain: Option<lillux_sqlite::OriginalNativeDomain>,
        metadata_pin_phase: PacketMetadataPinPhase,
    ) -> Self {
        Self {
            held: Held::new(
                "ryeos-app::packet-parts-progress",
                OriginalPacketProgressParts {
                    bound: None,
                    authority: None,
                    destination: None,
                    destination_retirement: None,
                    metadata_pin_inputs: None,
                    metadata_pin_receipts: [None, None],
                    raw,
                    parts,
                    refusal: None,
                    source_parts_consumed: false,
                    optional_source_census: None,
                    scheduler_handoff,
                    native_domain,
                    metadata_pin_registry_identity: None,
                },
                packet_progress_settled,
            ),
            source_kind,
            destination_retries: 0,
            destination_started: false,
            requires_destination: false,
            destination_roots_registered: false,
            destination_native_references_retired: false,
            destination_consumed: false,
            bound_root_receipt_consumed: false,
            metadata_pin_phase,
        }
    }
    fn from_original(
        raw: lillux_sqlite::OriginalNativeOutsideConnectionCustody,
        source_kind: OriginalPacketSourceKind,
    ) -> Self {
        Self::new_progress(
            Some(raw),
            None,
            None,
            source_kind,
            None,
            PacketMetadataPinPhase::Unclassified,
        )
    }
    pub(super) fn from_parts(
        parts: lillux_sqlite::OriginalNativeOutsideConnectionParts,
        scheduler_handoff: Option<(
            ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
            ryeos_scheduler::db::SchedulerNativeStoreSourceRole,
        )>,
        source_kind: OriginalPacketSourceKind,
    ) -> Self {
        let metadata_pin_phase = producer_metadata_pin_phase(&parts);
        let native_domain = parts.domain;
        Self::new_progress(
            None,
            Some(parts),
            scheduler_handoff,
            source_kind,
            native_domain,
            metadata_pin_phase,
        )
    }

    pub(super) fn source_parts_consumed(&self) -> bool {
        self.held.source_parts_consumed
    }
    fn record_optional_source_census(&mut self) -> Result<()> {
        let parts = self
            .held
            .parts
            .as_ref()
            .context("actual packet parts absent before optional-source census")?;
        ensure!(
            parts.inspection.is_none(),
            "selected live Store packet unexpectedly contains a Closed inspection owner"
        );
        ensure!(
            parts.released_inspection.is_none(),
            "selected live Store packet unexpectedly contains a Released inspection owner"
        );
        ensure!(
            parts.scheduler_retirement.is_none(),
            "selected live Store packet unexpectedly contains Scheduler retirement inputs"
        );
        let census = PacketOptionalSourceCensus {
            producer: self.source_kind,
            closed_inspection_absent: true,
            released_inspection_absent: true,
            scheduler_retirement_absent: true,
            acquisition_failure: parts.original_error.as_ref().map(|_| {
                PacketAcquisitionFailureFact {
                    source_kind: self.source_kind,
                    ticket: parts.ticket,
                    domain: parts.domain,
                }
            }),
            failure_fact_recorded: false,
        };
        if let Some(existing) = self.held.optional_source_census {
            ensure!(
                existing == census,
                "selected live Store packet optional-source census changed"
            );
        } else {
            self.held.get_mut().optional_source_census = Some(census);
        }
        Ok(())
    }

    /// Factory-free census of this packet's pre-seal joins: source handoff,
    /// destination permit and metadata-pin retirement with no retained inputs.
    fn require_pre_seal_complete(&self) -> Result<()> {
        ensure!(!self.held.is_foreign(), "foreign packet pre-seal census");
        ensure!(
            self.held.raw.is_none()
                && self.held.authority.is_none()
                && self.held.refusal.is_none()
                && self.destination_started,
            "packet source handoff is incomplete before the region seal"
        );
        if self.requires_destination {
            ensure!(
                self.destination_roots_registered
                    && self.destination_native_references_retired
                    && self.held.destination.is_none()
                    && (self.held.destination_retirement.is_some() || self.destination_consumed),
                "packet destination lacks its retirement permit before the region seal"
            );
        } else {
            ensure!(
                self.held.destination.is_none()
                    && self.held.destination_retirement.is_none()
                    && !self.destination_consumed,
                "packet without destination retains destination state"
            );
        }
        ensure!(
            matches!(
                self.metadata_pin_phase,
                PacketMetadataPinPhase::Absent | PacketMetadataPinPhase::Retired(_)
            ) && self.held.metadata_pin_inputs.is_none(),
            "packet metadata pins are not retired before the region seal"
        );
        Ok(())
    }

    fn require_optional_source_census(&self, expected: OriginalPacketSourceKind) -> Result<()> {
        ensure!(
            self.source_kind == expected,
            "original packet moved to another selected live producer collection"
        );
        let census = self.held.optional_source_census.context(
            "selected live Store packet lacks its producer-qualified optional-source census",
        )?;
        ensure!(
            census.producer == expected
                && census.closed_inspection_absent
                && census.released_inspection_absent
                && census.scheduler_retirement_absent,
            "selected live Store packet optional-source absence proof changed"
        );
        if self.held.source_parts_consumed {
            ensure!(
                census.failure_fact_recorded
                    && self.held.parts.is_none()
                    && self.held.raw.is_none()
                    && self.held.authority.is_none()
                    && self.held.destination.is_none()
                    && self.held.destination_retirement.is_none()
                    && self.held.refusal.is_none()
                    && self.held.bound.is_none()
                    && self.held.metadata_pin_inputs.is_none()
                    && self.held.metadata_pin_receipts.iter().all(Option::is_none)
                    && self.bound_root_receipt_consumed
                    && (!self.requires_destination && !self.destination_consumed
                        || self.requires_destination && self.destination_consumed)
                    && matches!(
                        self.metadata_pin_phase,
                        PacketMetadataPinPhase::ConsumedAbsent
                            | PacketMetadataPinPhase::Consumed(_)
                    )
                    && self.held.metadata_pin_registry_identity.is_some(),
                "consumed packet source regained native custody or lost its terminal fact"
            );
            return Ok(());
        }
        ensure!(
            !census.failure_fact_recorded,
            "packet failure fact was recorded before source evidence reached its terminal stage"
        );
        let parts = self
            .held
            .parts
            .as_ref()
            .context("actual packet parts absent during optional-source census check")?;
        ensure!(
            parts.inspection.is_none()
                && parts.released_inspection.is_none()
                && parts.scheduler_retirement.is_none(),
            "selected live Store packet acquired an unowned optional source"
        );
        match (parts.original_error.as_ref(), census.acquisition_failure) {
            (None, None) => {}
            (Some(_), Some(fact)) => ensure!(
                fact.source_kind == expected
                    && fact.ticket == parts.ticket
                    && fact.domain == parts.domain,
                "retained acquisition failure fact no longer names its original packet"
            ),
            _ => anyhow::bail!("selected live Store packet lost its acquisition-failure evidence"),
        }
        Ok(())
    }

    fn advance_destination(
        &mut self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.normalize(factory, aggregate)?;
        if !self.destination_started {
            let parts = self
                .held
                .get_mut()
                .parts
                .as_mut()
                .expect("cached original packet parts");
            ensure!(
                parts.bound_inputs.is_none() || parts.transient.is_none(),
                "actual packet retains conflicting original destination owners"
            );
            if let Some(bound) = parts.bound_inputs.take() {
                let bound = match bound.into_original_parts() {
                    Ok(bound) => bound,
                    Err(bound) => {
                        parts.bound_inputs = Some(bound);
                        anyhow::bail!("actual original bound-input extraction refused");
                    }
                };
                let lillux_sqlite::OriginalNativeOutsideBoundParts {
                    root,
                    name,
                    mode,
                    domain,
                    authority,
                    inspection,
                } = bound;
                self.held.get_mut().bound = Some(OriginalPacketBoundResources::new(
                    Some(root),
                    inspection,
                    name,
                    mode,
                    domain,
                ));
                self.held.get_mut().authority = Some(authority);
            } else {
                self.held.get_mut().authority = parts.transient.take();
            }
            self.requires_destination = self.held.authority.is_some();
            self.destination_started = true;
        }
        if self.held.destination_retirement.is_some() {
            ensure!(
                self.held.authority.is_none()
                    && self.held.refusal.is_none()
                    && self.destination_roots_registered
                    && self.destination_native_references_retired,
                "retirement permit precedes complete packet destination join"
            );
            return Ok(());
        }
        if self.held.destination.is_some() {
            ensure!(
                self.held.authority.is_none() && self.held.refusal.is_none(),
                "original packet retains conflicting destination phases"
            );
            return Ok(());
        }
        if self.held.refusal.is_some() {
            let next_retry = next_destination_retry(self.destination_retries)?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                prepay(0, destination_retry_attempt_quote().steps)
            })?;
            ensure!(
                !self.held.is_foreign(),
                "foreign Store packet after retry funding"
            );
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            self.destination_retries = next_retry;
            let refusal = self
                .held
                .get_mut()
                .refusal
                .take()
                .expect("retained original destination refusal");
            match refusal.into_original_inputs() {
                Ok((original_factory, authority, error)) => {
                    if !std::sync::Arc::ptr_eq(&original_factory, factory) {
                        self.held.get_mut().refusal =
                            Some(lillux_sqlite::OriginalDestinationCustodyFailure::retain(
                                original_factory,
                                authority,
                                error,
                            ));
                        anyhow::bail!("retained destination refusal changed original factory");
                    }
                    self.held.get_mut().authority = Some(authority);
                    // The lower failure keeps its factory and actual authority
                    // in separate fields. Once those exact owners return here,
                    // an earlier diagnostic error is not custody and retaining
                    // a history of them would make cleanup storage grow per retry.
                    drop(error);
                    // Canonical caller factory remains retained; alias only.
                    drop(original_factory);
                }
                Err(refusal) => {
                    self.held.get_mut().refusal = Some(refusal);
                    anyhow::bail!("actual destination refusal extraction refused");
                }
            }
        }
        let Some(authority) = self.held.get_mut().authority.take() else {
            ensure!(
                !self.requires_destination,
                "actual original destination authority absent"
            );
            return Ok(()); // Genuine packet has no destination; no retirement inferred.
        };
        let leaf = &self
            .held
            .parts
            .as_ref()
            .expect("retained actual original parts")
            .leaf_evidence;
        match authority.into_outside_custody(factory, aggregate, leaf, prepay) {
            Ok(destination) => self.held.get_mut().destination = Some(destination),
            Err(refusal) => {
                self.held.get_mut().refusal = Some(refusal);
                anyhow::bail!("actual packet destination handoff refused; original retained");
            }
        }
        Ok(())
    }

    fn register_bound_root(
        &mut self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(!self.held.is_foreign(), "foreign Store packet root custody");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        ensure!(
            self.destination_started
                && self.held.authority.is_none()
                && self.held.refusal.is_none()
                && (!self.requires_destination || self.held.destination.is_some()),
            "packet destination must transfer before bound-root registration"
        );
        self.held
            .parts
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("actual Store packet parts absent"))?
            .leaf_evidence
            .require_original(factory, aggregate)?;
        let Some(bound) = self.held.bound.as_ref() else {
            return Ok(());
        };
        // The actual inspection/SQL lease is still an independent obligation.
        // Never dispose it or infer retirement from a directory registration.
        ensure!(
            bound.held.inspection.is_none(),
            "original bound inspection context still needs consuming custody join"
        );
        // Completed transfer revalidates its exact membership without charging
        // again; only the first owner transfer consumes the registration quote.
        let bound = self
            .held
            .get_mut()
            .bound
            .as_mut()
            .expect("retained actual bound-root source");
        {
            let bound = bound.held.get_mut();
            register_bound_root_source(&mut bound.root, &mut bound.registration, registry, prepay)?;
        }
        ensure!(
            !self.held.is_foreign(),
            "foreign Store packet after root funding"
        );
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        Ok(())
    }

    fn register_destination_roots(
        &mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        if !self.requires_destination {
            ensure!(
                self.held.destination.is_none()
                    && self.held.destination_retirement.is_none()
                    && !self.destination_consumed,
                "packet without destination retains a destination token"
            );
            return Ok(());
        }
        if self.destination_consumed {
            ensure!(
                self.held.destination.is_none() && self.held.destination_retirement.is_none(),
                "consumed packet destination token reappeared"
            );
            return Ok(());
        }
        if self.held.destination_retirement.is_some() {
            ensure!(
                self.destination_roots_registered && self.destination_native_references_retired,
                "destination retirement permit lost its completed joins"
            );
            return Ok(());
        }
        let destination = self
            .held
            .get_mut()
            .destination
            .as_mut()
            .context("actual packet destination token absent before root registration")?;
        // SAFETY: this is the exact pre-mapping registry retained by the
        // Store supervisor, and `prepay` debits the same original reservation.
        unsafe {
            destination.register_shared_roots(registry, prepay)?;
        }
        self.destination_roots_registered = true;
        Ok(())
    }

    fn retire_destination_native_references(
        &mut self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        if !self.requires_destination {
            ensure!(
                self.held.destination.is_none()
                    && self.held.destination_retirement.is_none()
                    && !self.destination_consumed,
                "packet without destination retains a destination token"
            );
            return Ok(());
        }
        if self.destination_consumed {
            ensure!(
                self.held.destination.is_none() && self.held.destination_retirement.is_none(),
                "consumed packet destination token reappeared"
            );
            return Ok(());
        }
        if self.held.destination_retirement.is_some() {
            ensure!(
                self.destination_roots_registered,
                "destination permit lost its completed root registration"
            );
            return Ok(());
        }
        ensure!(
            self.destination_roots_registered,
            "packet destination roots must join before native-reference retirement"
        );
        if self.destination_native_references_retired {
            return Ok(());
        }
        let destination =
            self.held.get_mut().destination.as_mut().context(
                "actual packet destination token absent before native-reference retirement",
            )?;
        destination.retire_native_references(factory, aggregate, prepay)?;
        self.destination_native_references_retired = true;
        Ok(())
    }

    fn retire_metadata_pins(
        &mut self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        registry_identity: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity,
        issue_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        )
            -> Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
    ) -> Result<()> {
        ensure!(
            !self.held.is_foreign() && self.held.raw.is_none(),
            "foreign or unnormalized original packet metadata pins"
        );
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.require_optional_source_census(self.source_kind)?;
        if self.requires_destination {
            ensure!(
                self.destination_roots_registered && self.destination_native_references_retired,
                "packet metadata pins precede destination and native-reference joins"
            );
        }

        let metadata_pin_phase = &mut self.metadata_pin_phase;
        let held = self.held.get_mut();
        let metadata_pin_inputs = &mut held.metadata_pin_inputs;
        let metadata_pin_receipts = &mut held.metadata_pin_receipts;
        let metadata_pin_registry_identity = &mut held.metadata_pin_registry_identity;
        let parts = held
            .parts
            .as_mut()
            .context("actual packet parts absent before metadata-pin retirement")?;
        parts.leaf_evidence.require_original(factory, aggregate)?;
        ensure!(
            parts.ticket == parts.leaf_evidence.native_ticket()?
                && parts.domain == parts.leaf_evidence.domain()?,
            "actual packet ticket/domain differ from their live leaf evidence"
        );
        let registry_identity_slot = metadata_pin_registry_identity;
        if let Some(original) = registry_identity_slot.as_ref() {
            original.require_same_registry(registry_identity)?;
        } else {
            *registry_identity_slot = Some(registry_identity.clone());
        }
        let mask = parts
            .durable_pins
            .iter()
            .enumerate()
            .fold(0u8, |mask, (index, pin)| {
                mask | (u8::from(pin.is_some()) << index)
            });
        match *metadata_pin_phase {
            PacketMetadataPinPhase::Unclassified => anyhow::bail!(
                "original packet metadata-pin census was not recorded at normalization"
            ),
            PacketMetadataPinPhase::Absent => {
                ensure!(
                    mask == 0,
                    "producer-qualified packet metadata-pin absence changed"
                );
                ensure!(
                    metadata_pin_inputs.is_none()
                        && metadata_pin_receipts.iter().all(Option::is_none),
                    "absent packet metadata pins retain retirement inputs or receipts"
                );
                return Ok(());
            }
            PacketMetadataPinPhase::Retiring(expected)
            | PacketMetadataPinPhase::Retired(expected) => {
                ensure!(
                    mask == expected,
                    "actual packet metadata-pin source census changed"
                );
            }
            PacketMetadataPinPhase::ConsumedAbsent | PacketMetadataPinPhase::Consumed(_) => {
                anyhow::bail!(
                    "consumed packet metadata pins cannot re-enter pre-disposal retirement"
                );
            }
        }
        if let PacketMetadataPinPhase::Retired(expected) = *metadata_pin_phase {
            ensure!(
                expected == mask,
                "retired packet metadata-pin census changed"
            );
            let receipts = &*metadata_pin_receipts;
            for (index, pin) in parts.durable_pins.iter().enumerate() {
                match (pin, receipts[index].as_ref()) {
                    (Some(pin), Some(receipt)) => receipt.require_original(pin)?,
                    (None, None) => {}
                    _ => anyhow::bail!("retired packet metadata-pin receipt census changed"),
                }
            }
            ensure!(
                metadata_pin_inputs.is_none(),
                "completed packet metadata retirement retains native factory inputs"
            );
            return Ok(());
        }
        let PacketMetadataPinPhase::Retiring(expected_mask) = *metadata_pin_phase else {
            anyhow::bail!("packet metadata-pin phase changed before native disposal")
        };
        ensure!(
            expected_mask == mask && mask != 0,
            "packet metadata-pin retirement lost its producer census"
        );

        let inputs_slot = &mut *metadata_pin_inputs;
        if inputs_slot.is_none() {
            // The issuer charges exact authority and backing from the original
            // late owner before allocating this one-shot input.
            *inputs_slot = Some(issue_inputs(&parts.leaf_evidence, &parts.durable_pins)?);
        }
        let inputs = inputs_slot
            .as_ref()
            .expect("packet metadata retirement input cached");
        let receipts = &mut *metadata_pin_receipts;
        for (index, pin) in parts.durable_pins.iter().enumerate() {
            let Some(pin) = pin else {
                continue;
            };
            if let Some(receipt) = receipts[index].as_ref() {
                receipt.require_original(pin)?;
                continue;
            }
            let quote = quote_retired_packet_metadata_pin_receipt()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                prepay(quote.bytes, quote.steps)
            })?;
            let receipt = pin.retire_after_backend(factory, aggregate, inputs)?;
            receipts[index] = Some(Arc::new(receipt));
        }
        for (index, pin) in parts.durable_pins.iter().enumerate() {
            if let Some(pin) = pin {
                receipts[index]
                    .as_ref()
                    .context("packet metadata pin retired without retaining its receipt")?
                    .require_original(pin)?;
            }
        }
        drop(inputs_slot.take());
        *metadata_pin_phase = PacketMetadataPinPhase::Retired(mask);
        Ok(())
    }

    fn prepare_destination_retirement(
        &mut self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        if !self.requires_destination {
            ensure!(
                self.held.destination.is_none()
                    && self.held.destination_retirement.is_none()
                    && !self.destination_consumed,
                "packet without destination retains retirement state"
            );
            return Ok(());
        }
        if self.destination_consumed {
            ensure!(
                self.held.destination.is_none() && self.held.destination_retirement.is_none(),
                "consumed packet destination retirement state reappeared"
            );
            return Ok(());
        }
        if self.held.destination_retirement.is_some() {
            ensure!(
                self.held.destination.is_none()
                    && self.destination_roots_registered
                    && self.destination_native_references_retired,
                "cached destination permit has conflicting packet state"
            );
            return Ok(());
        }
        ensure!(
            self.destination_roots_registered && self.destination_native_references_retired,
            "destination permit requires successful root and native-reference joins"
        );
        let destination = self
            .held
            .get_mut()
            .destination
            .take()
            .context("actual packet destination absent before retirement permit")?;
        match destination.prepare_retirement(factory, aggregate, registry, prepay) {
            Ok(permit) => {
                self.held.get_mut().destination_retirement = Some(permit);
                Ok(())
            }
            Err((destination, error)) => {
                self.held.get_mut().destination = Some(destination);
                Err(error)
            }
        }
    }

    fn consume_retired_destination(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        if !self.requires_destination {
            ensure!(
                self.held.destination.is_none()
                    && self.held.destination_retirement.is_none()
                    && !self.destination_consumed,
                "packet without destination retains a destination token"
            );
            return Ok(());
        }
        if self.destination_consumed {
            ensure!(
                self.held.destination.is_none() && self.held.destination_retirement.is_none(),
                "consumed packet destination token reappeared"
            );
            return Ok(());
        }
        ensure!(
            self.destination_roots_registered
                && self.destination_native_references_retired
                && self.held.destination.is_none(),
            "packet destination permit cannot be consumed before its native and registry joins"
        );
        let permit = self
            .held
            .get_mut()
            .destination_retirement
            .take()
            .context("actual packet destination retirement permit absent")?;
        match permit.into_retired(registry, prepay) {
            Ok(()) => {
                self.destination_consumed = true;
                Ok(())
            }
            Err((permit, error)) => {
                self.held.get_mut().destination_retirement = Some(permit);
                Err(error)
            }
        }
    }

    fn consume_retired_bound_root(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<bool> {
        ensure!(
            !self.held.is_foreign(),
            "foreign retired packet bound-root receipt"
        );
        if self.bound_root_receipt_consumed {
            ensure!(
                self.held
                    .bound
                    .as_ref()
                    .is_none_or(|bound| bound.held.registration.is_none()),
                "consumed packet bound-root receipt regained a registration"
            );
            return Ok(false);
        }
        ensure!(
            self.held.raw.is_none()
                && self.held.parts.is_some()
                && self.held.destination.is_none()
                && self.held.destination_retirement.is_none()
                && (!self.requires_destination || self.destination_consumed),
            "packet bound-root receipt precedes destination and source normalization"
        );
        let Some(bound) = self.held.get_mut().bound.as_mut() else {
            self.bound_root_receipt_consumed = true;
            return Ok(false);
        };
        ensure!(
            bound.held.root.is_none() && bound.held.inspection.is_none(),
            "retired packet bound-root receipt precedes actual root/inspection transfer"
        );
        let registration = bound
            .held
            .registration
            .as_ref()
            .context("actual packet bound root lacks its retired-registry registration")?;
        ensure!(matches!(registration.observed_source_identity()?,
            lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
            "retired packet bound-root registration changed its Directory role");
        drop(registration.receipt_in(registry)?);
        bound.held.get_mut().registration = None;
        self.bound_root_receipt_consumed = true;
        Ok(true)
    }

    fn consume_retired_source_parts(
        &mut self,
        expected: OriginalPacketSourceKind,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<bool> {
        ensure!(
            !self.held.is_foreign(),
            "foreign retired packet source evidence"
        );
        self.require_optional_source_census(expected)?;
        let registry_identity =
            self.held.metadata_pin_registry_identity.as_ref().context(
                "retired packet source evidence lacks its pre-disposal registry identity",
            )?;
        registry.require_same_registry(registry_identity)?;
        if self.held.source_parts_consumed {
            let census = self
                .held
                .optional_source_census
                .as_ref()
                .expect("checked consumed packet census");
            return Ok(census.acquisition_failure.is_some());
        }
        ensure!(
            self.held.raw.is_none() && self.held.parts.is_some(),
            "retired packet source evidence is not normalized"
        );
        ensure!(
            self.bound_root_receipt_consumed
                && self.held.destination.is_none()
                && self.held.destination_retirement.is_none()
                && (!self.requires_destination && !self.destination_consumed
                    || self.requires_destination && self.destination_consumed),
            "retired packet source evidence precedes exact destination and bound-root receipts"
        );
        ensure!(
            self.held.refusal.is_none()
                && self.held.authority.is_none()
                && self
                    .held
                    .bound
                    .as_ref()
                    .is_none_or(|bound| bound.held.registration.is_none()
                        && bound.held.root.is_none()
                        && bound.held.inspection.is_none()),
            "retired packet still retains a destination, refusal, root, or inspection owner"
        );
        ensure!(
            matches!(
                self.metadata_pin_phase,
                PacketMetadataPinPhase::ConsumedAbsent | PacketMetadataPinPhase::Consumed(_)
            ),
            "retired packet source evidence precedes metadata-pin receipts"
        );
        ensure!(
            self.held.metadata_pin_inputs.is_none()
                && self.held.metadata_pin_receipts.iter().all(Option::is_none),
            "retired packet source evidence retains metadata-pin retirement custody"
        );

        let parts = self
            .held
            .parts
            .as_ref()
            .context("retired packet source evidence lost its original parts")?;
        ensure!(
            parts.bound_inputs.is_none()
                && parts.transient.is_none()
                && parts.scheduler_retirement.is_none()
                && parts.inspection.is_none()
                && parts.released_inspection.is_none()
                && parts.durable_pins.iter().all(Option::is_none),
            "retired packet still retains an unconsumed native source owner"
        );

        // The census stores only source kind, ticket, and domain; the raw
        // acquisition error is diagnostic data, not a cleanup owner. The
        // producer keeps native owners in separate typed fields, all checked
        // above, so this fact is the retained terminal disposition.
        let mut parts = self
            .held
            .get_mut()
            .parts
            .take()
            .expect("validated retired packet parts remain present");
        let failure = self
            .held
            .optional_source_census
            .as_ref()
            .expect("validated retired packet census remains present")
            .acquisition_failure;
        drop(parts.original_error.take());
        drop(parts);

        let mut bound = self.held.get_mut().bound.take();
        if let Some(bound) = bound.as_mut() {
            drop(bound.held.get_mut().root.take());
            drop(bound.held.get_mut().inspection.take());
        }
        drop(bound);
        drop(self.held.get_mut().authority.take());
        drop(self.held.get_mut().destination.take());
        drop(self.held.get_mut().destination_retirement.take());
        drop(self.held.get_mut().metadata_pin_inputs.take());
        drop(std::mem::take(
            &mut self.held.get_mut().metadata_pin_receipts,
        ));
        drop(self.held.get_mut().raw.take());
        self.held.get_mut().scheduler_handoff = None;
        self.held
            .get_mut()
            .optional_source_census
            .as_mut()
            .expect("validated retired packet census remains present")
            .failure_fact_recorded = true;
        self.held.get_mut().source_parts_consumed = true;
        Ok(failure.is_some())
    }

    fn normalize(
        &mut self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<()> {
        ensure!(
            !self.held.is_foreign(),
            "foreign original Store packet progress"
        );
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if self.held.parts.is_none() {
            let raw = self
                .held
                .get_mut()
                .raw
                .take()
                .ok_or_else(|| anyhow::anyhow!("actual original packet source absent"))?;
            match raw.into_original_parts() {
                Ok(parts) => {
                    self.metadata_pin_phase = producer_metadata_pin_phase(&parts);
                    self.held.get_mut().parts = Some(parts);
                }
                Err(raw) => {
                    self.held.get_mut().raw = Some(raw);
                    anyhow::bail!("actual original packet extraction refused; source retained");
                }
            }
        }
        ensure!(
            self.held.raw.is_none(),
            "original packet retains conflicting source phases"
        );
        // The complete payload is cached before any identity refusal. Bound
        // inputs, transient authority, inspections, pins and errors stay intact.
        self.held
            .parts
            .as_ref()
            .expect("cached original packet parts")
            .leaf_evidence
            .require_original(factory, aggregate)?;
        self.record_optional_source_census()
    }

    fn require_scheduler_store_acceptance(
        &self,
        handoff_id: &ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
        role: ryeos_scheduler::db::SchedulerNativeStoreSourceRole,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    ) -> Result<()> {
        ensure!(
            !self.held.is_foreign()
                && self.held.raw.is_none()
                && self.destination_started
                && self.held.authority.is_none()
                && self.held.refusal.is_none(),
            "Scheduler Store handoff packet is not retained in a normalized source phase"
        );
        let (recorded_id, recorded_role) = self
            .held
            .scheduler_handoff
            .as_ref()
            .context("Scheduler Store handoff packet lacks its source marker")?;
        ensure!(
            recorded_id.same_handoff(handoff_id) && *recorded_role == role,
            "Scheduler Store handoff packet belongs to another source role"
        );
        if self.requires_destination {
            ensure!(
                self.held.destination.is_some()
                    || self.held.destination_retirement.is_some()
                    || self.destination_consumed,
                "Scheduler Store handoff packet lost its retained destination phase"
            );
        } else {
            ensure!(
                self.held.destination.is_none()
                    && self.held.destination_retirement.is_none()
                    && !self.destination_consumed,
                "Scheduler packet without a destination retains a conflicting phase"
            );
        }
        let parts = self
            .held
            .parts
            .as_ref()
            .context("Scheduler Store handoff packet lost its actual connection parts")?;
        self.require_optional_source_census(OriginalPacketSourceKind::Scheduler)?;
        ensure!(
            parts.bound_inputs.is_none() && parts.transient.is_none(),
            "Scheduler Store handoff packet retains an unprocessed input authority"
        );
        parts.leaf_evidence.require_original(factory, aggregate)?;
        if let Some(bound) = self.held.bound.as_ref() {
            ensure!(
                bound.held.root.is_none() && bound.held.inspection.is_none(),
                "Scheduler Store handoff packet retains an unregistered bound owner"
            );
            let registration = bound
                .held
                .registration
                .as_ref()
                .context("Scheduler Store handoff packet lacks its bound-root registration")?;
            registry.require_membership(registration)?;
            ensure!(matches!(registration.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                "Scheduler Store handoff packet bound-root registration changed role");
        }
        Ok(())
    }
}

pub(super) fn packet_source_parts_consumed(
    packets: Option<&Vec<OriginalPacketPartsProgress>>,
) -> bool {
    packets.is_none_or(|packets| {
        packets
            .iter()
            .all(OriginalPacketPartsProgress::source_parts_consumed)
    })
}

/// Retained live scheduler inputs when shutdown wins before SQLite opens.
/// There is no fabricated native leaf: the actual bound root and the exact
/// Store-issued authority alias join the Store's existing physical inventory.
pub(super) struct UnopenedSchedulerDatabaseProgress {
    held: Held<UnopenedSchedulerDatabaseParts>,
    root_identity_checked: bool,
}
/// Everything an unopened Scheduler database progress retains, in the order
/// it ends.
struct UnopenedSchedulerDatabaseParts {
    root: Option<std::sync::Arc<lillux::PinnedDirectory>>,
    authority: Option<std::sync::Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>>,
    inspection: Option<(
        lillux_sqlite::SqliteCopyLease,
        std::sync::Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
    )>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
    handoff_id: Option<ryeos_scheduler::db::SchedulerNativeStoreHandoffId>,
    name: Option<lillux_sqlite::OriginalNativeOutsideBoundCoordinate>,
    mode: Option<lillux_sqlite::OriginalNativeBoundMode>,
    domain: Option<lillux_sqlite::OriginalNativeDomain>,
    authority_joined: bool,
    registration_consumed: bool,
}
/// Settled exactly when exhaustion would accept the source: authority
/// joined, registration consumed and no root, authority, inspection or
/// coordinate owner left. Otherwise the bound root, authority alias and
/// inspection lease stay retained together until the process exits.
fn unopened_scheduler_database_settled(parts: &UnopenedSchedulerDatabaseParts) -> bool {
    parts.authority_joined
        && parts.registration_consumed
        && parts.registration.is_none()
        && parts.root.is_none()
        && parts.authority.is_none()
        && parts.inspection.is_none()
        && parts.name.is_none()
        && parts.mode.is_none()
        && parts.domain.is_none()
}

impl UnopenedSchedulerDatabaseProgress {
    fn from_parts(
        parts: lillux_sqlite::OriginalNativeOutsideBoundParts,
        handoff_id: Option<ryeos_scheduler::db::SchedulerNativeStoreHandoffId>,
    ) -> Self {
        let lillux_sqlite::OriginalNativeOutsideBoundParts {
            root,
            name,
            mode,
            domain,
            authority,
            inspection,
        } = parts;
        Self {
            held: Held::new(
                "ryeos-app::unopened-scheduler-database",
                UnopenedSchedulerDatabaseParts {
                    root: Some(root),
                    authority: Some(authority),
                    inspection,
                    registration: None,
                    handoff_id,
                    name: Some(name),
                    mode: Some(mode),
                    domain: Some(domain),
                    authority_joined: false,
                    registration_consumed: false,
                },
                unopened_scheduler_database_settled,
            ),
            root_identity_checked: false,
        }
    }

    fn advance(
        &mut self,
        expected_store: &std::sync::Arc<super::live_native::LiveStateOpen>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(!self.held.is_foreign(), "foreign unopened scheduler source");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if self.held.authority_joined {
            anyhow::ensure!(
                self.held.root.is_none()
                    && self.held.authority.is_none()
                    && self.held.inspection.is_none()
                    && self.held.registration.is_some(),
                "completed unopened scheduler source retains conflicting custody"
            );
            return Ok(());
        }

        // A short-lived alias of the retained authority, so the slot itself
        // can change while the checks below read it.
        let authority_alias = self
            .held
            .authority
            .clone()
            .context("unopened scheduler Store authority absent")?;
        let authority = &authority_alias;
        anyhow::ensure!(
            self.held
                .name
                .as_ref()
                .is_some_and(|name| name.as_os_str() == std::ffi::OsStr::new("scheduler.sqlite3"))
                && self.held.mode == Some(lillux_sqlite::OriginalNativeBoundMode::OpenOrCreate)
                && self.held.domain == Some(lillux_sqlite::OriginalNativeDomain::NodeScheduler)
                && self.held.inspection.is_none(),
            "unopened scheduler source changed its fixed live coordinate"
        );
        expected_store.require_unopened_native_authority(authority, factory)?;
        if let Some(root) = self.held.root.as_ref() {
            expected_store.require_unopened_scheduler_bound_input(root, authority, factory)?;
            self.root_identity_checked = true;
        } else {
            anyhow::ensure!(
                self.held.registration.is_some() && self.root_identity_checked,
                "unopened scheduler source lost its root before Store membership"
            );
        }

        if let Some(binding) = self.held.registration.as_ref() {
            anyhow::ensure!(
                self.held.root.is_none() && self.root_identity_checked,
                "registered unopened scheduler root retains its source alias"
            );
            let quote = registry.membership_work_bound(1, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            expected_store.require_unopened_native_authority(authority, factory)?;
            registry.require_membership(binding)?;
            anyhow::ensure!(matches!(binding.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                "unopened scheduler source registration changed role");
        } else {
            let bytes = std::mem::size_of::<lillux_sqlite::OriginalNativeOutsideBoundCoordinate>()
                .checked_add(std::mem::size_of::<usize>())
                .context("unopened scheduler source storage quote overflow")?;
            registry.prepay_transfer(prepay, u64::try_from(bytes)?, 8)?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            expected_store.require_unopened_scheduler_bound_input(
                self.held
                    .root
                    .as_ref()
                    .context("unopened scheduler root disappeared after funding")?,
                authority,
                factory,
            )?;
            let root = self
                .held
                .get_mut()
                .root
                .take()
                .context("unopened scheduler root absent before registration")?;
            match unsafe { registry.register_directory(root, prepay) } {
                Ok(binding) => self.held.get_mut().registration = Some(binding),
                Err((root, error)) => {
                    self.held.get_mut().root = Some(root);
                    return Err(error);
                }
            }
            let binding = self
                .held
                .registration
                .as_ref()
                .expect("cached scheduler root membership");
            registry.require_membership(binding)?;
            anyhow::ensure!(matches!(binding.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                "unopened scheduler source registration changed role");
        }

        // The Store graph retains the canonical LiveStateOpen authority. This
        // exact duplicate is released only after its root joined that same
        // registry and identity was checked above.
        registry.prepay_transfer(prepay, 0, 4)?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        expected_store.require_unopened_native_authority(authority, factory)?;
        let authority = self
            .held
            .get_mut()
            .authority
            .take()
            .context("unopened scheduler Store authority disappeared")?;
        drop(authority);
        self.held.get_mut().name.take();
        self.held.get_mut().mode.take();
        self.held.get_mut().domain.take();
        self.held.get_mut().authority_joined = true;
        Ok(())
    }

    pub(super) fn is_joined(&self) -> bool {
        !self.held.is_foreign()
            && self.held.authority_joined
            && self.held.root.is_none()
            && self.held.name.is_none()
            && self.held.mode.is_none()
            && self.held.domain.is_none()
            && self.held.authority.is_none()
            && self.held.inspection.is_none()
            && self.held.registration.is_some()
    }
}

impl UnopenedSchedulerDatabaseProgress {
    /// Consume the joined root registration only with its exact close
    /// receipt from the retired registry. Refusal keeps the registration.
    pub(super) fn consume_retired(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        ensure!(
            !self.held.is_foreign(),
            "foreign retired unopened Scheduler source"
        );
        if self.held.registration_consumed {
            return self.require_exhausted();
        }
        ensure!(
            self.is_joined(),
            "unopened Scheduler root did not join the Store inventory"
        );
        let binding = self
            .held
            .registration
            .as_ref()
            .expect("checked joined Scheduler registration");
        ensure!(matches!(binding.observed_source_identity()?,
            lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
            "retired unopened Scheduler registration changed role");
        drop(binding.receipt_in(registry)?);
        self.held.get_mut().registration = None;
        self.held.get_mut().registration_consumed = true;
        Ok(())
    }

    pub(super) fn require_exhausted(&self) -> Result<()> {
        ensure!(
            !self.held.is_foreign() && unopened_scheduler_database_settled(&self.held),
            "unopened Scheduler source retains an owner or unconsumed registration"
        );
        Ok(())
    }

    /// End an exhausted source. Its held custody settles on the exhaustion check the
    /// caller already passed, so every slot is released normally.
    pub(super) fn exhaust(self) {
        drop(self);
    }
}
/// Retained unused native schema input after its one-use admission failed
/// before native issuance. It has no ticket or physical source; only its exact
/// Store-authority alias remains to join and retire.
pub(super) struct UnopenedNativeTransientProgress {
    held: Held<UnopenedNativeTransientParts>,
    expected_domain: lillux_sqlite::OriginalNativeDomain,
}
/// Everything an unused native schema input retains, in the order it ends.
struct UnopenedNativeTransientParts {
    authority: Option<std::sync::Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>>,
    domain: Option<lillux_sqlite::OriginalNativeDomain>,
    joined: bool,
}
/// Settled exactly when the authority alias joined the inventory and left
/// its slot; otherwise the alias stays retained until the process exits.
fn unopened_native_transient_settled(parts: &UnopenedNativeTransientParts) -> bool {
    parts.joined && parts.domain.is_none() && parts.authority.is_none()
}

impl UnopenedNativeTransientProgress {
    fn from_parts(
        domain: lillux_sqlite::OriginalNativeDomain,
        expected_domain: lillux_sqlite::OriginalNativeDomain,
        authority: std::sync::Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
    ) -> Self {
        Self {
            held: Held::new(
                "ryeos-app::unopened-native-transient",
                UnopenedNativeTransientParts {
                    authority: Some(authority),
                    domain: Some(domain),
                    joined: false,
                },
                unopened_native_transient_settled,
            ),
            expected_domain,
        }
    }

    fn advance(
        &mut self,
        expected_store: &std::sync::Arc<super::live_native::LiveStateOpen>,
        expected_domain: lillux_sqlite::OriginalNativeDomain,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.held.is_foreign(),
            "foreign unused native schema input"
        );
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        anyhow::ensure!(
            self.expected_domain == expected_domain,
            "unused native schema progress changed its fixed caller purpose"
        );
        if self.held.joined {
            anyhow::ensure!(
                self.held.domain.is_none() && self.held.authority.is_none(),
                "joined unused native schema input retains source custody"
            );
            return Ok(());
        }
        let authority = self
            .held
            .authority
            .as_ref()
            .context("unused native schema authority absent")?;
        anyhow::ensure!(
            self.held.domain == Some(expected_domain),
            "unused native schema input changed its fixed native role"
        );
        expected_store.require_unopened_native_authority(authority, factory)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| prepay(0, 4))?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        expected_store.require_unopened_native_authority(authority, factory)?;
        let authority = self
            .held
            .get_mut()
            .authority
            .take()
            .context("unused native schema authority disappeared")?;
        drop(authority);
        self.held.get_mut().domain.take();
        self.held.get_mut().joined = true;
        Ok(())
    }

    pub(super) fn is_joined(&self) -> bool {
        !self.held.is_foreign()
            && self.held.joined
            && self.held.domain.is_none()
            && self.held.authority.is_none()
    }
}

impl UnopenedNativeTransientProgress {
    pub(super) fn require_exhausted(&self) -> Result<()> {
        ensure!(
            self.is_joined(),
            "unused native schema input retains its authority alias"
        );
        Ok(())
    }

    /// End a joined input. Its held custody settles on the joined check the caller
    /// already passed, so the authority slot is released normally.
    pub(super) fn exhaust(self) {
        drop(self);
    }
}
impl OriginalPacketPartsProgress {
    fn require_exhausted(&self) -> Result<()> {
        ensure!(
            !self.held.is_foreign() && self.held.exhausted(),
            "Store packet retains a source, destination, root or pin owner at exhaustion"
        );
        Ok(())
    }

    /// End an exhausted packet. Its held custody settles on the exhaustion check the
    /// caller already passed, so every slot is released normally.
    fn exhaust(self) {
        drop(self);
    }
}
/// Every packet in the slot has consumed its source parts and holds no
/// owner. Absence is accepted; it carries no owner.
pub(super) fn require_original_packets_exhausted(
    packets: &Option<Vec<OriginalPacketPartsProgress>>,
) -> Result<()> {
    if let Some(packets) = packets {
        for packet in packets.iter() {
            packet.require_exhausted()?;
        }
    }
    Ok(())
}

pub(super) fn exhaust_original_packets(packets: Option<Vec<OriginalPacketPartsProgress>>) {
    if let Some(packets) = packets {
        for packet in packets {
            packet.exhaust();
        }
    }
}

/// A drained producer packet vector holds no owner.
pub(super) fn require_sql_custody_exhausted(
    packets: &Option<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
) -> Result<()> {
    ensure!(
        packets.as_ref().is_none_or(|packets| packets.is_empty()),
        "Store retains an undrained native packet vector at exhaustion"
    );
    Ok(())
}

pub(super) fn exhaust_sql_custody(
    packets: Option<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
) {
    drop(packets);
}

/// Non-owning record of one exhausted Store outside graph: the identity of
/// the exact retired registry its receipts were checked against.
pub struct StoreExhausted {
    process: u32,
    _registry: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity,
}
impl StoreExhausted {
    pub(super) fn new(
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<Self> {
        Ok(Self {
            process: std::process::id(),
            _registry: registry.identity()?,
        })
    }
    pub fn require_original_process(&self) -> Result<()> {
        ensure!(
            self.process == std::process::id(),
            "foreign exhausted Store record"
        );
        Ok(())
    }
}

pub(super) fn issue_scheduler_store_handoff_receipt<'store, Store: ?Sized>(
    store: &'store Store,
    packets: Option<&Vec<OriginalPacketPartsProgress>>,
    unopened_database: Option<&UnopenedSchedulerDatabaseProgress>,
    unopened_schema: Option<&UnopenedNativeTransientProgress>,
    admission_sources: Option<&super::live_native::LiveSchedulerNativeAdmissionOutsideSources>,
    registry: Option<&lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry>,
    handoff_id: &ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
) -> Result<ryeos_scheduler::db::SchedulerNativeStoreHandoffReceipt<'store>> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    let registry =
        registry.context("Scheduler Store handoff lacks its retained physical inventory")?;
    let mut database_sources = 0usize;
    let mut schema_sources = 0usize;

    if let Some(packets) = packets {
        require_packet_count_within_source_bound(
            packets.len(),
            OriginalPacketSourceKind::Scheduler,
        )?;
        require_auxiliary_packet_domains_within_bounds(
            packets.iter().map(|packet| packet.held.native_domain),
        )?;
        for packet in packets.iter() {
            let Some((packet_id, role)) = packet.held.scheduler_handoff.as_ref() else {
                continue;
            };
            ensure!(
                packet_id.same_handoff(handoff_id),
                "Store retains a Scheduler packet from another handoff"
            );
            packet.require_scheduler_store_acceptance(
                handoff_id, *role, factory, aggregate, registry,
            )?;
            match role {
                ryeos_scheduler::db::SchedulerNativeStoreSourceRole::Database => {
                    database_sources = database_sources
                        .checked_add(1)
                        .context("Scheduler database source count overflow")?
                }
                ryeos_scheduler::db::SchedulerNativeStoreSourceRole::Schema => {
                    schema_sources = schema_sources
                        .checked_add(1)
                        .context("Scheduler schema source count overflow")?
                }
            }
        }
    }

    if let Some(unopened) = unopened_database {
        if let Some(recorded_id) = unopened.held.handoff_id.as_ref() {
            ensure!(
                recorded_id.same_handoff(handoff_id),
                "Store retains an unopened Scheduler database from another handoff"
            );
            ensure!(
                unopened.is_joined(),
                "unopened Scheduler database is not joined to the exact Store inventory"
            );
            let registration = unopened
                .held
                .registration
                .as_ref()
                .context("joined unopened Scheduler database lacks its root registration")?;
            registry.require_membership(registration)?;
            ensure!(matches!(registration.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                "unopened Scheduler database registration changed role");
            database_sources = database_sources
                .checked_add(1)
                .context("Scheduler database source count overflow")?;
        } else {
            ensure!(
                unopened.is_joined(),
                "unmarked unopened Scheduler failure input is not joined to Store custody"
            );
        }
    }
    ensure!(
        unopened_schema.is_none_or(UnopenedNativeTransientProgress::is_joined),
        "unused Scheduler schema input is not joined to the exact Store authority"
    );
    ensure!(
        admission_sources
            .is_none_or(super::live_native::LiveSchedulerNativeAdmissionOutsideSources::is_joined),
        "failed Scheduler admission inputs are not joined to the exact Store inventory"
    );
    ensure!(
        database_sources == 1 && schema_sources == 1,
        "Store did not accept exactly one database and one schema source for this Scheduler handoff"
    );

    // SAFETY: this Store-owned function has just verified the exact opaque
    // handoff identity, one retained database source, one retained schema
    // source, and all associated unopened/failure inputs in this Store frame.
    unsafe {
        ryeos_scheduler::db::SchedulerNativeStoreHandoffReceipt::from_store_source_join(
            store,
            handoff_id.clone(),
        )
    }
}

pub(super) fn advance_unopened_scheduler_transient(
    source: &mut Option<lillux_sqlite::OriginalNativeTransientInputs>,
    destination: &mut Option<UnopenedNativeTransientProgress>,
    expected_store: Option<&std::sync::Arc<super::live_native::LiveStateOpen>>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    advance_unopened_native_transient(
        source,
        destination,
        lillux_sqlite::OriginalNativeDomain::NodeSchedulerSchemaReference,
        expected_store,
        factory,
        aggregate,
        prepay,
    )
}

pub(super) fn advance_unopened_native_transient(
    source: &mut Option<lillux_sqlite::OriginalNativeTransientInputs>,
    destination: &mut Option<UnopenedNativeTransientProgress>,
    expected_domain: lillux_sqlite::OriginalNativeDomain,
    expected_store: Option<&std::sync::Arc<super::live_native::LiveStateOpen>>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    if source.is_some() && destination.is_none() {
        let bytes = std::mem::size_of::<UnopenedNativeTransientProgress>();
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(u64::try_from(bytes)?, 4)
        })?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        // SAFETY: this exact mutable source slot is taken once and cleared
        // before conversion; a foreign-process refusal restores the same owner.
        let raw = source.take().expect("checked Scheduler transient input");
        let (domain, authority) = match raw.into_original_parts() {
            Ok(parts) => parts,
            Err(raw) => {
                *source = Some(raw);
                anyhow::bail!("unused native transient input belongs to another process");
            }
        };
        *destination = Some(UnopenedNativeTransientProgress::from_parts(
            domain,
            expected_domain,
            authority,
        ));
    }
    let Some(progress) = destination.as_mut() else {
        anyhow::ensure!(
            source.is_none(),
            "unused Scheduler transient transfer was not retained"
        );
        return Ok(());
    };
    if !progress.is_joined() {
        let expected_store =
            expected_store.context("unused native transient input lacks its exact Store owner")?;
        progress.advance(expected_store, expected_domain, factory, aggregate, prepay)?;
    }
    Ok(())
}

/// Normalize one authentic pre-acknowledged connection source directly from
/// its retained parts. The bounded scheduler vector is allocated before the
/// caller's owner moves, then every retry resumes the same packet progress.
pub(super) fn prepare_scheduler_packet_parts(
    source: &mut Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    destination: &mut Option<Vec<OriginalPacketPartsProgress>>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
    handoff: Option<(
        ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
        ryeos_scheduler::db::SchedulerNativeStoreSourceRole,
    )>,
) -> Result<()> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    if source.is_some() && destination.is_none() {
        let quote = quote_packet_parts(MAX_LIVE_AUXILIARY_NATIVE_PACKET_SOURCES, 1)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let mut packets = Vec::new();
        packets.try_reserve_exact(MAX_LIVE_AUXILIARY_NATIVE_PACKET_SOURCES)?;
        *destination = Some(packets);
    }
    if source.is_some() {
        let packets = destination
            .as_mut()
            .context("scheduler packet progress absent after reservation")?;
        let next_count = packets
            .len()
            .checked_add(1)
            .context("scheduler packet count overflow")?;
        require_packet_count_within_source_bound(next_count, OriginalPacketSourceKind::Scheduler)?;
        let incoming = source
            .as_ref()
            .expect("checked scheduler packet source")
            .domain;
        require_auxiliary_packet_domains_within_bounds(
            packets
                .iter()
                .map(|packet| packet.held.native_domain)
                .chain(std::iter::once(incoming)),
        )?;
        let source = source.take().expect("checked scheduler packet source");
        let parts = source;
        packets.push(OriginalPacketPartsProgress::from_parts(
            parts,
            handoff,
            OriginalPacketSourceKind::Scheduler,
        ));
    }
    if let Some(packets) = destination.as_mut() {
        require_packet_count_within_source_bound(
            packets.len(),
            OriginalPacketSourceKind::Scheduler,
        )?;
        require_auxiliary_packet_domains_within_bounds(
            packets.iter().map(|packet| packet.held.native_domain),
        )?;
        for packet in packets.iter_mut() {
            packet.advance_destination(factory, aggregate, prepay)?;
        }
    }
    Ok(())
}

pub(super) fn advance_unopened_scheduler_database(
    source: &mut Option<lillux_sqlite::OriginalNativeBoundInputs>,
    destination: &mut Option<UnopenedSchedulerDatabaseProgress>,
    expected_store: Option<&std::sync::Arc<super::live_native::LiveStateOpen>>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
    handoff_id: Option<ryeos_scheduler::db::SchedulerNativeStoreHandoffId>,
) -> Result<()> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    if source.is_some() && destination.is_none() {
        let raw = source.as_ref().expect("checked unopened scheduler source");
        raw.require_scheduler_coordinate()?;
        let bytes = std::mem::size_of::<UnopenedSchedulerDatabaseProgress>()
            .checked_add(std::mem::size_of::<
                lillux_sqlite::OriginalNativeOutsideBoundCoordinate,
            >())
            .context("unopened scheduler source transfer quote overflow")?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(u64::try_from(bytes)?, 8)
        })?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        // SAFETY: this branch has exactly one mutable source slot. Take it
        // once and clear the slot before any fallible conversion; the exact
        // owner is restored below if process validation refuses the move.
        let raw = source.take().expect("checked source");
        let parts = match raw.into_original_parts() {
            Ok(parts) => parts,
            Err(raw) => {
                *source = Some(raw);
                anyhow::bail!("unopened scheduler bound input belongs to another process");
            }
        };
        *destination = Some(UnopenedSchedulerDatabaseProgress::from_parts(
            parts, handoff_id,
        ));
    }
    let Some(progress) = destination.as_mut() else {
        anyhow::ensure!(
            source.is_none(),
            "unopened scheduler source transfer was not retained"
        );
        return Ok(());
    };
    if !progress.is_joined() {
        let expected_store =
            expected_store.context("unopened scheduler source lacks its exact Store owner")?;
        progress.advance(expected_store, factory, aggregate, registry, prepay)?;
    }
    Ok(())
}

/// Mechanical packet normalization shared by normal and failed Store paths.
/// It moves complete genuine components; it supplies no native/physical grant.
pub(super) fn prepare_original_packet_parts(
    source: &mut Option<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
    destination: &mut Option<Vec<OriginalPacketPartsProgress>>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    source_kind: OriginalPacketSourceKind,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    let Some(raw) = source.as_mut() else {
        ensure!(
            destination.is_none(),
            "original packet destination lacks its source phase"
        );
        return Ok(()); // No source selected; no completion inference.
    };
    if destination.is_none() {
        let count = raw.len();
        // Each producer applies this structural limit before it grows or
        // transfers its source vector. This receiver repeats the exact check
        // before allocating progress or moving any packet payload.
        require_packet_count_within_source_bound(count, source_kind)?;
        let quote = quote_packet_parts(count, 1)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let mut slots = Vec::new();
        slots.try_reserve_exact(count)?;
        *destination = Some(slots);
        let slots = destination
            .as_mut()
            .expect("cached original packet progress vector");
        // Capacity is reserved and the destination is retained before source
        // transfer. This loop invokes no host operation or external callback.
        for packet in raw.drain(..) {
            slots.push(OriginalPacketPartsProgress::from_original(
                packet,
                source_kind,
            ));
        }
    }
    if let Some(destination) = destination.as_ref() {
        require_packet_count_within_source_bound(destination.len(), source_kind)?;
    }
    ensure!(
        raw.is_empty(),
        "new native packets appeared after original normalization"
    );
    let slots = destination
        .as_mut()
        .expect("retained original packet progress vector");
    for slot in slots.iter_mut() {
        slot.advance_destination(factory, aggregate, prepay)?;
    }
    Ok(())
}

/// Transfer one already-retained outside-parts owner into its Runtime packet
/// slot. The exact source stays in `source` until destination storage has
/// been prepaid and reserved; after transfer every retry resumes the packet
/// progress in the same Store frame.
pub(super) fn prepare_original_packet_parts_from_owner(
    source: &mut Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    raw: &mut Option<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
    destination: &mut Option<Vec<OriginalPacketPartsProgress>>,
    expected_domain: lillux_sqlite::OriginalNativeDomain,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    source_kind: OriginalPacketSourceKind,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    let Some(actual) = source.as_ref() else {
        ensure!(
            raw.is_none() == destination.is_none(),
            "retained original packet marker lost its matching source phase"
        );
        return Ok(());
    };
    ensure!(
        actual.domain == Some(expected_domain),
        "retained original packet changed its fixed native purpose"
    );
    ensure!(
        actual.leaf_evidence.domain()? == Some(expected_domain),
        "retained original packet leaf changed its fixed native purpose"
    );
    ensure!(
        actual.bound_inputs.is_none() || actual.transient.is_none(),
        "retained original packet has conflicting destination owners"
    );
    require_packet_count_within_source_bound(1, source_kind)?;
    let quote = if destination.is_none()
        || raw.is_none()
        || destination
            .as_ref()
            .is_some_and(|slots| slots.capacity() == 0)
    {
        quote_packet_parts(1, 2)?
    } else {
        OutsideWork {
            bytes: 0,
            steps: 4 + PACKET_OPTIONAL_SOURCE_CENSUS_STEPS,
        }
    };
    lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
        prepay(quote.bytes, quote.steps)
    })?;
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    ensure!(
        destination.as_ref().is_none_or(|slots| slots.is_empty()),
        "Runtime schema packet source slot already occupied"
    );
    ensure!(
        raw.as_ref().is_none_or(|raw| raw.is_empty()),
        "retained original packet source marker is not empty"
    );

    if destination.is_none() {
        let mut slots = Vec::new();
        slots.try_reserve_exact(1)?;
        *destination = Some(slots);
    }
    if raw.is_none() {
        *raw = Some(Vec::new());
    }
    let slots = destination
        .as_mut()
        .expect("reserved original packet progress");
    if slots.capacity() < 1 {
        slots.try_reserve_exact(1)?;
    }
    // SAFETY: the same original Store progress retains this exact parts owner;
    // destination capacity is reserved before its one-time extraction.
    let actual = source.take().expect("checked actual outside parts");
    slots.push(OriginalPacketPartsProgress::from_parts(
        actual,
        None,
        source_kind,
    ));
    for packet in slots.iter_mut() {
        packet.advance_destination(factory, aggregate, prepay)?;
    }
    Ok(())
}

/// Join selected packet roots to the SAME open physical inventory. This
/// transfers actual owners only; the complete inventory is not sealed here.
pub(super) fn register_original_packet_bound_roots(
    packets: &mut Option<Vec<OriginalPacketPartsProgress>>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    source_kind: OriginalPacketSourceKind,
    registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    if let Some(packets) = packets {
        require_packet_count_within_source_bound(packets.len(), source_kind)?;
        for packet in packets.iter_mut() {
            packet.register_bound_root(factory, aggregate, registry, prepay)?;
        }
    }
    Ok(()) // Absence selects no source; it supplies no retirement evidence.
}

pub(super) fn register_original_packet_destination_roots(
    packets: &mut Option<Vec<OriginalPacketPartsProgress>>,
    source_kind: OriginalPacketSourceKind,
    registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    let Some(packets) = packets else {
        return Ok(());
    };
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    for packet in packets.iter_mut() {
        packet.register_destination_roots(registry, prepay)?;
    }
    Ok(())
}

/// Check actual selected packet handoff phases. This observes retained
/// components only and does not imply native-reference or physical retirement.
pub(super) fn require_original_packet_root_handoffs(
    packets: &Option<Vec<OriginalPacketPartsProgress>>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    source_kind: OriginalPacketSourceKind,
) -> Result<()> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    let packets = packets
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("actual packet handoff inventory absent"))?;
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    for packet in packets.iter() {
        ensure!(
            !packet.held.is_foreign()
                && packet.held.raw.is_none()
                && packet.destination_started
                && packet.held.authority.is_none()
                && packet.held.refusal.is_none()
                && (packet.held.destination.is_none()
                    || packet.held.destination_retirement.is_none())
                && (!packet.requires_destination
                    || (packet.destination_roots_registered
                        && (((packet.held.destination.is_some()
                            || packet.held.destination_retirement.is_some())
                            && !packet.destination_consumed)
                            || (packet.held.destination.is_none()
                                && packet.held.destination_retirement.is_none()
                                && packet.destination_consumed
                                && packet.destination_native_references_retired)))),
            "actual original packet root handoff incomplete"
        );
        packet
            .held
            .parts
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("genuine original packet parts absent"))?
            .leaf_evidence
            .require_original(factory, aggregate)?;
        packet.require_optional_source_census(source_kind)?;
        if let Some(bound) = packet.held.bound.as_ref() {
            ensure!(
                bound.held.root.is_none()
                    && bound.held.registration.is_some()
                    && bound.held.inspection.is_none(),
                "actual packet bound/inspection custody handoff incomplete"
            );
        }
    }
    Ok(())
}

pub(super) fn require_original_packet_sources_transferred(
    packets: &Option<Vec<OriginalPacketPartsProgress>>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    source_kind: OriginalPacketSourceKind,
) -> Result<()> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    let packets = packets
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("actual packet source inventory absent"))?;
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    for packet in packets.iter() {
        ensure!(
            !packet.held.is_foreign()
                && packet.held.raw.is_none()
                && packet.destination_started
                && packet.held.authority.is_none()
                && packet.held.refusal.is_none()
                && (packet.held.destination.is_none()
                    || packet.held.destination_retirement.is_none())
                && (!packet.requires_destination
                    || packet.held.destination.is_some()
                    || packet.held.destination_retirement.is_some()
                    || (packet.destination_consumed
                        && packet.destination_native_references_retired)),
            "actual original packet source transfer incomplete"
        );
        let parts = packet
            .held
            .parts
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("genuine original packet parts absent"))?;
        parts.leaf_evidence.require_original(factory, aggregate)?;
        packet.require_optional_source_census(source_kind)?;
        let mask = parts
            .durable_pins
            .iter()
            .enumerate()
            .fold(0u8, |mask, (index, pin)| {
                mask | (u8::from(pin.is_some()) << index)
            });
        match packet.metadata_pin_phase {
            PacketMetadataPinPhase::Absent => ensure!(
                mask == 0,
                "producer-qualified packet metadata-pin absence changed"
            ),
            PacketMetadataPinPhase::Retiring(expected)
            | PacketMetadataPinPhase::Retired(expected) => ensure!(
                mask == expected,
                "producer-qualified packet metadata-pin census changed"
            ),
            PacketMetadataPinPhase::Unclassified
            | PacketMetadataPinPhase::ConsumedAbsent
            | PacketMetadataPinPhase::Consumed(_) => {
                anyhow::bail!("packet metadata-pin source is outside its pre-disposal phase")
            }
        }
    }
    Ok(())
}

/// Pre-seal census of one retained packet vector. An absent vector was
/// already qualified by the parent join that would have produced it.
pub(super) fn require_original_packet_pre_seal_complete(
    packets: &Option<Vec<OriginalPacketPartsProgress>>,
    source_kind: OriginalPacketSourceKind,
) -> Result<()> {
    let Some(packets) = packets else {
        return Ok(());
    };
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    for packet in packets.iter() {
        ensure!(
            packet.source_kind == source_kind,
            "packet pre-seal census changed source kind"
        );
        packet.require_pre_seal_complete()?;
    }
    Ok(())
}

pub(super) fn retire_original_packet_native_references(
    packets: &mut Option<Vec<OriginalPacketPartsProgress>>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    source_kind: OriginalPacketSourceKind,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    let Some(packets) = packets else {
        return Ok(());
    };
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    for packet in packets.iter_mut() {
        packet.retire_destination_native_references(factory, aggregate, prepay)?;
    }
    Ok(())
}

pub(super) fn retire_original_packet_metadata_pins(
    packets: &mut Option<Vec<OriginalPacketPartsProgress>>,
    factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &Arc<lillux_sqlite::OriginalNativeTerminal>,
    source_kind: OriginalPacketSourceKind,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
    registry_identity: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity,
    issue_inputs: &dyn Fn(
        &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
    ) -> Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
) -> Result<()> {
    let Some(packets) = packets else {
        return Ok(());
    };
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    let quote = quote_packet_metadata_pin_retirements(packets.len())?;
    if quote.steps > 0 {
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
    }
    for packet in packets.iter_mut() {
        packet.retire_metadata_pins(factory, aggregate, prepay, registry_identity, issue_inputs)?;
    }
    Ok(())
}

pub(super) fn prepare_original_packet_destination_retirements(
    packets: &mut Option<Vec<OriginalPacketPartsProgress>>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    source_kind: OriginalPacketSourceKind,
    registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    let Some(packets) = packets else {
        return Ok(());
    };
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    for packet in packets.iter_mut() {
        packet.prepare_destination_retirement(factory, aggregate, registry, prepay)?;
    }
    Ok(())
}

pub(super) fn consume_retired_original_packet_destinations(
    packets: &mut Option<Vec<OriginalPacketPartsProgress>>,
    registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    source_kind: OriginalPacketSourceKind,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    let Some(packets) = packets else {
        return Ok(());
    };
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    for packet in packets.iter_mut() {
        packet.consume_retired_destination(registry, prepay)?;
    }
    Ok(())
}

pub(super) fn consume_retired_original_packet_bound_roots(
    packets: &mut Option<Vec<OriginalPacketPartsProgress>>,
    registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    source_kind: OriginalPacketSourceKind,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    let Some(packets) = packets else {
        return Ok(());
    };
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    let pending = packets
        .iter()
        .filter(|packet| !packet.bound_root_receipt_consumed && packet.held.bound.is_some())
        .count();
    let quote = quote_retired_packet_bound_roots(pending)?;
    if quote.steps > 0 {
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
    }
    for packet in packets.iter_mut() {
        packet.consume_retired_bound_root(registry)?;
    }
    Ok(())
}

pub(super) fn consume_retired_original_packet_metadata_pins(
    packets: &mut Option<Vec<OriginalPacketPartsProgress>>,
    source_kind: OriginalPacketSourceKind,
    registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<()> {
    let Some(packets) = packets else {
        return Ok(());
    };
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    let quote = quote_retired_packet_metadata_pin_receipts(packets.len())?;
    if quote.steps > 0 {
        // Receipt storage was prepaid before each close. This pass only
        // validates and consumes the retained receipts and source Arcs.
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(0, quote.steps)
        })?;
    }
    for packet in packets.iter_mut() {
        ensure!(
            !packet.held.is_foreign() && packet.held.raw.is_none(),
            "foreign or unnormalized retired packet metadata pins"
        );
        ensure!(
            packet.held.destination.is_none()
                && packet.held.destination_retirement.is_none()
                && (!packet.requires_destination || packet.destination_consumed)
                && packet.bound_root_receipt_consumed,
            "packet metadata-pin receipts precede retired destination and root receipts"
        );
        packet.require_optional_source_census(source_kind)?;
        let original_registry = packet
            .held
            .metadata_pin_registry_identity
            .as_ref()
            .context("packet metadata pins lack their pre-disposal registry identity")?;
        registry.require_same_registry(original_registry)?;
        let held = packet.held.get_mut();
        let parts = held
            .parts
            .as_mut()
            .context("packet parts absent at metadata-pin receipt consumption")?;
        match packet.metadata_pin_phase {
            PacketMetadataPinPhase::Absent => {
                ensure!(
                    parts.durable_pins.iter().all(Option::is_none)
                        && held.metadata_pin_inputs.is_none()
                        && held.metadata_pin_receipts.iter().all(Option::is_none),
                    "producer-qualified absent packet pins acquired a retirement owner"
                );
                packet.metadata_pin_phase = PacketMetadataPinPhase::ConsumedAbsent;
            }
            PacketMetadataPinPhase::Retired(mask) => {
                for (index, pin) in parts.durable_pins.iter().enumerate() {
                    let expected = mask & (1 << index) != 0;
                    ensure!(
                        pin.is_some() == expected,
                        "packet metadata-pin source disagrees with its pre-disposal census"
                    );
                    match (pin, held.metadata_pin_receipts[index].as_ref()) {
                        (Some(pin), Some(receipt)) => {
                            receipt.require_original(pin)?;
                        }
                        (None, None) => {}
                        _ => anyhow::bail!("packet metadata-pin close receipt mismatch"),
                    }
                }
                ensure!(
                    held.metadata_pin_inputs.is_none(),
                    "post-disposal packet pin consumer retains native-bound inputs"
                );
                parts.durable_pins = [None, None];
                held.metadata_pin_receipts = [None, None];
                packet.metadata_pin_phase = PacketMetadataPinPhase::Consumed(mask);
            }
            PacketMetadataPinPhase::ConsumedAbsent => {
                ensure!(
                    parts.durable_pins.iter().all(Option::is_none),
                    "consumed packet metadata-pin absence regained a source"
                );
            }
            PacketMetadataPinPhase::Consumed(mask) => {
                ensure!(
                    mask != 0
                        && parts.durable_pins.iter().all(Option::is_none)
                        && held.metadata_pin_receipts.iter().all(Option::is_none)
                        && held.metadata_pin_inputs.is_none(),
                    "consumed packet metadata-pin receipts regained custody"
                );
            }
            PacketMetadataPinPhase::Unclassified | PacketMetadataPinPhase::Retiring(_) => {
                anyhow::bail!(
                    "packet metadata pins lack a completed pre-disposal owner transition"
                );
            }
        }
    }
    Ok(())
}

pub(super) fn consume_retired_original_packet_source_parts(
    packets: &mut Option<Vec<OriginalPacketPartsProgress>>,
    source_kind: OriginalPacketSourceKind,
    registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    prepay: &dyn Fn(u64, u64) -> Result<()>,
) -> Result<u8> {
    let Some(packets) = packets else {
        return Ok(0);
    };
    require_packet_count_within_source_bound(packets.len(), source_kind)?;
    // Every packet is visited on each pass so already-consumed entries still
    // prove same-registry idempotence and contribute their retained failure
    // fact to the aggregate result. Charge the complete visited vector.
    let quote = quote_retired_packet_source_parts(packets.len())?;
    if quote.steps > 0 {
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
    }
    let mut failures = 0u8;
    for packet in packets.iter_mut() {
        if packet.consume_retired_source_parts(source_kind, registry)? {
            failures = failures
                .checked_add(1)
                .context("retired packet acquisition-failure count overflow")?;
        }
    }
    Ok(failures)
}

pub struct LiveStateStoreOutsideCustody {
    // The whole concrete original graph remains here until typed, owner-specific
    // physical transfers finish. Dropping this wrapper supplies no cleanup.
    held: Held<LiveStateStoreOutsideParts>,
}
/// The whole closed or failed-start Store graph the wrapper retains.
struct LiveStateStoreOutsideParts {
    source: LiveStateStoreOutsideSource,
}
pub enum LiveStateStoreOutsideSource {
    Closed(ClosedLiveStateStore),
    FailedStartup(ReleasedLiveStateStoreStartup),
}
impl LiveStateStoreOutsideCustody {
    /// Hold a whole source. No drop settles this wrapper: it always holds the
    /// whole closed or failed-start Store graph, which only the typed
    /// physical transfers or the final exhaustion consume, so a dropped
    /// wrapper keeps the graph retained until the process exits and a copy in
    /// another process is relinquished.
    fn from_source(source: LiveStateStoreOutsideSource) -> Self {
        Self {
            held: Held::until_taken(
                "ryeos-app::store-outside-custody",
                LiveStateStoreOutsideParts { source },
            ),
        }
    }

    /// Move the whole source out. A copy in another process is refused and
    /// comes back still held.
    fn into_source(self) -> std::result::Result<LiveStateStoreOutsideSource, Self> {
        match self.held.take_whole() {
            Ok(LiveStateStoreOutsideParts { source }) => Ok(source),
            Err(held) => Err(Self { held }),
        }
    }
}
impl LiveStateStoreOutsideSource {
    fn physical_registry_mut(
        &mut self,
    ) -> Result<&mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry> {
        let slot = match self {
            Self::Closed(source) => source.physical_registry_slot(),
            Self::FailedStartup(source) => source.physical_registry_slot(),
        };
        slot.as_mut()
            .context("actual Store physical registry absent")
    }

    /// # Safety
    /// `owner` is the exact original pinned directory transferred by the
    /// settled account/input finalizer, and this source retains the same
    /// pre-mapping inventory. The caller retains the returned membership and
    /// restores the owner on refusal; this does not seal or retire it.
    pub unsafe fn register_finalizer_directory_source(
        &mut self,
        owner: Arc<lillux::PinnedDirectory>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        (Arc<lillux::PinnedDirectory>, anyhow::Error),
    > {
        let registry = match self.physical_registry_mut() {
            Ok(registry) => registry,
            Err(error) => return Err((owner, error)),
        };
        unsafe { registry.register_directory(owner, prepay) }
    }

    /// # Safety
    /// `owner` is the exact settled account/input descriptor (including only
    /// its genuine duplicate role where applicable), already transferred into
    /// explicit close custody. It is no longer used for durable publication,
    /// and this source retains the same pre-mapping inventory. The caller
    /// retains the returned membership and restores the owner on refusal.
    pub unsafe fn register_finalizer_control_record_source(
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
        let registry = match self.physical_registry_mut() {
            Ok(registry) => registry,
            Err(error) => return Err((owner, error)),
        };
        unsafe { registry.register_control_record(owner, prepay) }
    }

    pub fn require_finalizer_source_membership(
        &mut self,
        registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity> {
        let registry = self.physical_registry_mut()?;
        let quote = registry.membership_work_bound(1, 0)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        registry.require_membership(registration)?;
        registration.observed_source_identity()
    }

    /// # Safety
    /// The finalizer has joined every actual Store, account and input source
    /// into this inventory and excluded all producers. Sealing closes
    /// registration only; it proves neither close nor account settlement.
    pub unsafe fn seal_finalizer_physical_inventory(&mut self) -> Result<()> {
        match self {
            Self::Closed(source) => source.require_state_native_sources_retired()?,
            Self::FailedStartup(source) => source.require_state_native_sources_retired()?,
        }
        unsafe { self.physical_registry_mut()?.seal_inventory() }
    }

    /// # Safety
    /// The exact inventory was sealed after its complete source census. The
    /// callback is the same original outside payer, and every refusal leaves
    /// the registry and its remaining actual sources retained here.
    pub unsafe fn prepare_finalizer_descriptor_owners(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        unsafe {
            self.physical_registry_mut()?
                .prepare_descriptor_owners(prepay)
        }
    }

    pub fn finalizer_non_account_descriptors_retired(&mut self) -> Result<bool> {
        Ok(self
            .physical_registry_mut()?
            .non_account_descriptors_retired())
    }

    /// # Safety
    /// This is the exact sealed registry already owned by this Store source.
    /// Every producer is excluded, the native account is durably settled, and
    /// both callbacks use its retained outside authority. The registry is
    /// borrowed exclusively; no bookkeeping mutex spans either callback.
    pub unsafe fn retire_finalizer_non_account_descriptors(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_inputs: &dyn Fn(
            &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
            Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
        ) -> Result<
            Arc<lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs>,
        >,
    ) -> Result<()> {
        let registry = self.physical_registry_mut()?;
        unsafe { registry.retire_non_operator_descriptors(prepay, issue_inputs) }
    }

    /// # Safety
    /// The original settled-capacity finalizer has completed account
    /// settlement and non-operator descriptor retirement. This consumes only
    /// the actual Store-held exclusion alias; the canonical registered lock
    /// stays in this same inventory.
    pub unsafe fn retire_settled_store_exclusion_alias(
        &mut self,
        admission_alias: Option<&Arc<ryeos_node_runtime::state_lock::StateLockLease>>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        match self {
            Self::Closed(source) => unsafe {
                source.retire_settled_exclusion_alias(admission_alias, prepay)
            },
            Self::FailedStartup(source) => unsafe {
                source.retire_settled_exclusion_alias(admission_alias, prepay)
            },
        }
    }

    /// # Safety
    /// Durable account work has completed and the exact same sealed registry
    /// reports every non-operator descriptor receipt. `prepay` is the original
    /// outside authority; no resident/native owner is consulted here.
    #[cfg(target_os = "linux")]
    pub unsafe fn retire_finalizer_operator_targets(
        &mut self,
        deadline: lillux::time::MonotonicDeadline,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let registry = self.physical_registry_mut()?;
        unsafe { registry.retire_operator_targets(deadline, prepay) }
    }

    /// # Safety
    /// The target was retired in this same registry. `issue_inputs` binds the
    /// original outside grant to its retained operator membership and parent.
    #[cfg(target_os = "linux")]
    pub unsafe fn retire_finalizer_operator_parent_descriptor(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_inputs: &dyn Fn(
            &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
            Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
        ) -> Result<
            Arc<lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs>,
        >,
    ) -> Result<()> {
        let registry = self.physical_registry_mut()?;
        unsafe { registry.retire_operator_parent_descriptor(prepay, issue_inputs) }
    }
}

impl LiveStateStoreOutsideCustody {
    /// Transfer the typed source pair from one Scheduler owner into this exact
    /// Store progress frame. Only this method assigns the database/schema roles
    /// used by the Store-issued acceptance receipt.
    pub fn advance_scheduler_native_handoff(
        &mut self,
        sources: &mut Option<ryeos_scheduler::db::SchedulerNativeOutsideSources>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        let Some(sources) = sources.as_mut() else {
            return Ok(());
        };
        let handoff_id = sources.handoff_id()?.clone();

        // SAFETY: each taken value is held locally, moved into this exact
        // Store progress frame, or restored to the wrapper on refusal.
        if let Some(database) = unsafe { sources.take_database()? } {
            let taken = match database.into_taken() {
                Ok(taken) => taken,
                Err(database) => {
                    // SAFETY: the refused source is the exact one just taken
                    // from this wrapper; the Store progress never saw it.
                    unsafe {
                        sources.restore_database(database);
                    }
                    anyhow::bail!("foreign Scheduler database source");
                }
            };
            match taken {
                ryeos_scheduler::db::TakenSchedulerNativeDatabaseSource::Connection(parts) => {
                    let mut source = Some(parts);
                    let result = self.advance_scheduler_packet_parts_with_handoff(
                        &mut source,
                        factory,
                        aggregate,
                        prepay,
                        Some((
                            handoff_id.clone(),
                            ryeos_scheduler::db::SchedulerNativeStoreSourceRole::Database,
                        )),
                    );
                    let unaccepted =
                        if let Some(parts) = source.take() {
                            // SAFETY: these are the exact source parts just taken
                            // from this wrapper; the Store progress did not accept them.
                            unsafe {
                                sources.restore_database(
                            ryeos_scheduler::db::SchedulerNativeDatabaseSource::from_connection(
                                parts));
                            }
                            true
                        } else {
                            false
                        };
                    result?;
                    ensure!(
                        !unaccepted,
                        "Scheduler database packet was not accepted by Store progress"
                    );
                }
                ryeos_scheduler::db::TakenSchedulerNativeDatabaseSource::NeverOpened(inputs) => {
                    let mut source = Some(inputs);
                    let result = self.advance_unopened_scheduler_database_with_handoff(
                        &mut source,
                        factory,
                        aggregate,
                        prepay,
                        Some(handoff_id.clone()),
                    );
                    let unaccepted = if let Some(inputs) = source.take() {
                        // SAFETY: these are the exact unopened inputs just taken
                        // from this wrapper; Store progress did not accept them.
                        unsafe {
                            sources.restore_database(
                            ryeos_scheduler::db::SchedulerNativeDatabaseSource::from_never_opened(
                                inputs));
                        }
                        true
                    } else {
                        false
                    };
                    result?;
                    ensure!(
                        !unaccepted,
                        "unopened Scheduler database was not accepted by Store progress"
                    );
                }
            }
        }

        // SAFETY: the schema value remains local until accepted by the tagged
        // Store packet progress or restored to this wrapper on refusal.
        if let Some(schema) = unsafe { sources.take_schema()? } {
            let mut source = Some(schema);
            let result = self.advance_scheduler_packet_parts_with_handoff(
                &mut source,
                factory,
                aggregate,
                prepay,
                Some((
                    handoff_id,
                    ryeos_scheduler::db::SchedulerNativeStoreSourceRole::Schema,
                )),
            );
            let unaccepted = if let Some(schema) = source.take() {
                // SAFETY: these are the exact schema parts just taken from
                // this wrapper; Store progress did not accept them.
                unsafe {
                    sources.restore_schema(schema);
                }
                true
            } else {
                false
            };
            result?;
            ensure!(
                !unaccepted,
                "Scheduler schema packet was not accepted by Store progress"
            );
        }
        Ok(())
    }

    /// Join one selected Scheduler DB/schema packet to the actual Store source
    /// graph. Source movement is cached before any fallible lower transition.
    pub fn advance_scheduler_packet_parts(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.advance_scheduler_packet_parts_with_handoff(source, factory, aggregate, prepay, None)
    }

    fn advance_scheduler_packet_parts_with_handoff(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        handoff: Option<(
            ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
            ryeos_scheduler::db::SchedulerNativeStoreSourceRole,
        )>,
    ) -> Result<()> {
        self.require_original_process()?;
        let store_source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        let source = std::cell::RefCell::new(source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            match &mut **store_source.borrow_mut() {
                LiveStateStoreOutsideSource::Closed(store) => store.advance_scheduler_packet_parts(
                    &mut source.borrow_mut(),
                    factory,
                    aggregate,
                    prepay,
                    handoff.clone(),
                ),
                LiveStateStoreOutsideSource::FailedStartup(store) => store
                    .advance_scheduler_packet_parts(
                        &mut source.borrow_mut(),
                        factory,
                        aggregate,
                        prepay,
                        handoff.clone(),
                    ),
            }
        })
    }

    /// Join genuinely unopened live Scheduler admission to the same Store
    /// inventory without manufacturing a native terminal receipt.
    pub fn advance_unopened_scheduler_database(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeBoundInputs>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.advance_unopened_scheduler_database_with_handoff(
            source, factory, aggregate, prepay, None,
        )
    }

    fn advance_unopened_scheduler_database_with_handoff(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeBoundInputs>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        handoff_id: Option<ryeos_scheduler::db::SchedulerNativeStoreHandoffId>,
    ) -> Result<()> {
        self.require_original_process()?;
        let store_source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        let source = std::cell::RefCell::new(source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            match &mut **store_source.borrow_mut() {
                LiveStateStoreOutsideSource::Closed(store) => store
                    .advance_unopened_scheduler_database(
                        &mut source.borrow_mut(),
                        factory,
                        aggregate,
                        prepay,
                        handoff_id.clone(),
                    ),
                LiveStateStoreOutsideSource::FailedStartup(store) => store
                    .advance_unopened_scheduler_database(
                        &mut source.borrow_mut(),
                        factory,
                        aggregate,
                        prepay,
                        handoff_id.clone(),
                    ),
            }
        })
    }

    /// Join the accounting ledger's acknowledged connection or schema
    /// reference packet to the Store's direct auxiliary collection. Source
    /// movement is cached before any fallible lower transition.
    pub fn advance_accounting_packet_parts(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        let domain = source.as_ref().map(|parts| parts.domain);
        ensure!(
            domain.is_none_or(|domain| matches!(
                domain,
                Some(
                    lillux_sqlite::OriginalNativeDomain::NodeAccounting
                        | lillux_sqlite::OriginalNativeDomain::NodeAccountingSchemaReference
                )
            )),
            "accounting packet selects another native role"
        );
        self.advance_scheduler_packet_parts_with_handoff(source, factory, aggregate, prepay, None)
    }

    /// Consume the ledger's unused one-shot schema input only after its exact
    /// authority has joined this Store source graph. The ledger opens only
    /// after the live Store opened, so a failed-startup Store has none.
    pub fn advance_unopened_accounting_transient(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeTransientInputs>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        let store_source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        let source = std::cell::RefCell::new(source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            match &mut **store_source.borrow_mut() {
                LiveStateStoreOutsideSource::Closed(store) => store
                    .advance_unopened_accounting_transient(
                        &mut source.borrow_mut(),
                        factory,
                        aggregate,
                        prepay,
                    ),
                LiveStateStoreOutsideSource::FailedStartup(_) => {
                    ensure!(
                        source.borrow().is_none(),
                        "accounting ledger input cannot precede the live Store it was issued from"
                    );
                    Ok(())
                }
            }
        })
    }

    /// Consume an unused one-shot Scheduler schema input only after its exact
    /// authority has joined this Store source graph.
    pub fn advance_unopened_scheduler_transient(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeTransientInputs>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        let store_source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        let source = std::cell::RefCell::new(source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            match &mut **store_source.borrow_mut() {
                LiveStateStoreOutsideSource::Closed(store) => store
                    .advance_unopened_scheduler_transient(
                        &mut source.borrow_mut(),
                        factory,
                        aggregate,
                        prepay,
                    ),
                LiveStateStoreOutsideSource::FailedStartup(store) => store
                    .advance_unopened_scheduler_transient(
                        &mut source.borrow_mut(),
                        factory,
                        aggregate,
                        prepay,
                    ),
            }
        })
    }

    /// Join a failed pre-open Scheduler admission and all actually prepared
    /// unused inputs to the same Store source graph. No ticket or terminal is
    /// inferred from the failed attempt.
    pub fn advance_scheduler_admission_sources(
        &mut self,
        source: &mut Option<super::live_native::LiveSchedulerNativeAdmissionOutsideSources>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        let store_source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        let source = std::cell::RefCell::new(source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            match &mut **store_source.borrow_mut() {
                LiveStateStoreOutsideSource::Closed(store) => store
                    .advance_scheduler_admission_sources(
                        &mut source.borrow_mut(),
                        factory,
                        aggregate,
                        prepay,
                    ),
                LiveStateStoreOutsideSource::FailedStartup(store) => store
                    .advance_scheduler_admission_sources(
                        &mut source.borrow_mut(),
                        factory,
                        aggregate,
                        prepay,
                    ),
            }
        })
    }

    /// Issue proof only when this exact retained Store frame holds one marked
    /// Scheduler database source and one marked schema source, with every
    /// associated unopened or failed-admission input joined.
    pub fn scheduler_native_store_handoff_receipt<'store>(
        &'store self,
        handoff_id: &ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<ryeos_scheduler::db::SchedulerNativeStoreHandoffReceipt<'store>> {
        self.require_original_process()?;
        match &self.held.source {
            LiveStateStoreOutsideSource::Closed(store) => {
                store.scheduler_native_store_handoff_receipt(handoff_id, factory, aggregate)
            }
            LiveStateStoreOutsideSource::FailedStartup(store) => {
                store.scheduler_native_store_handoff_receipt(handoff_id, factory, aggregate)
            }
        }
    }

    /// Observation of retained preparation only; this is not native/physical
    /// evidence or permission to construct another inventory.
    pub fn has_prepared_physical_registry(&mut self) -> bool {
        match &mut self.held.get_mut().source {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.physical_registry_slot().is_some()
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.physical_registry_slot().is_some()
            }
        }
    }

    /// Consume only the already-retired registry retained by this exact Store
    /// source. An incomplete inventory is restored without losing any owner;
    /// the receipt identifies the consumed pre-mapping registry for the late
    /// clock owner's final close gate.
    pub fn retire_finalizer_physical_registry(
        &mut self,
    ) -> Result<lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry> {
        self.require_original_process()?;
        let slot = match &mut self.held.get_mut().source {
            LiveStateStoreOutsideSource::Closed(source) => source.physical_registry_slot(),
            LiveStateStoreOutsideSource::FailedStartup(source) => source.physical_registry_slot(),
        };
        let registry = slot
            .take()
            .context("actual Store physical registry absent before retirement")?;
        match registry.retire() {
            Ok(receipt) => Ok(receipt),
            Err(registry) => {
                *slot = Some(registry);
                anyhow::bail!("actual Store physical registry still has unretired descriptors")
            }
        }
    }

    /// # Safety
    /// `prepared` is the actual registry allocated before first mapping by the
    /// SAME genuine native reservation, transferred by its factory under this
    /// aggregate. The caller caches it on refusal. No policy/data/alias vector
    /// can substitute for that original funded preparation.
    pub unsafe fn accept_prepared_physical_registry(
        &mut self,
        prepared: &mut Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<()> {
        self.require_original_process()?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let slot = match &mut self.held.get_mut().source {
            LiveStateStoreOutsideSource::Closed(source) => source.physical_registry_slot(),
            LiveStateStoreOutsideSource::FailedStartup(source) => source.physical_registry_slot(),
        };
        if slot.is_some() {
            ensure!(
                prepared.is_none(),
                "original Store and supervisor retain competing registries"
            );
            return Ok(());
        }
        ensure!(prepared.is_some(), "original prepared registry absent");
        *slot = prepared.take(); // no fallible operation after the consuming move
        Ok(())
    }

    /// Register an alias of the SAME original operator lock. Its canonical
    /// supervisor still holds exclusion; this neither closes nor seals it.
    #[cfg(target_os = "linux")]
    pub fn register_operator_lock_alias(
        &mut self,
        owner: &std::sync::Arc<lillux::ExactExclusiveFileLock>,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        owner.require_original_process()?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| match &mut **source
            .borrow_mut()
        {
            LiveStateStoreOutsideSource::Closed(source) => unsafe {
                source.register_operator_lock_alias(owner, factory, aggregate, prepay)
            },
            LiveStateStoreOutsideSource::FailedStartup(source) => unsafe {
                source.register_operator_lock_alias(owner, factory, aggregate, prepay)
            },
        })
    }

    /// Consume the completed Store physical parent into retained settlement
    /// parts, then permit its nested destination continuations to join the
    /// same actual registry. This transition follows root and operator
    /// registration; it is never inferred from native acknowledgement alone.
    #[cfg(target_os = "linux")]
    pub fn complete_store_parent_join(
        &mut self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_metadata_pin_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        ) -> Result<
            Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>,
        >,
    ) -> Result<()> {
        self.require_original_process()?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| match &mut **source
            .borrow_mut()
        {
            LiveStateStoreOutsideSource::Closed(source) => source.complete_store_parent_join(
                factory,
                aggregate,
                prepay,
                issue_metadata_pin_inputs,
            ),
            LiveStateStoreOutsideSource::FailedStartup(source) => source
                .complete_store_parent_join(factory, aggregate, prepay, issue_metadata_pin_inputs),
        })
    }

    /// Retire State-generated Bound main/WAL metadata pins on the SAME normal
    /// or failed-start graph, then end State's remaining strong native owners
    /// into a non-owning receipt, before the region seal. Inputs come from the
    /// original late-cleanup issuer per closed connection; receipts stay in
    /// the State frame and refusal retains every pin and owner.
    pub fn retire_state_native_sources(
        &mut self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_metadata_pin_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        ) -> Result<
            Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>,
        >,
    ) -> Result<()> {
        self.require_original_process()?;
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        let source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| match &mut **source
            .borrow_mut()
        {
            LiveStateStoreOutsideSource::Closed(source) => source.retire_state_native_sources(
                factory,
                terminal,
                prepay,
                issue_metadata_pin_inputs,
            ),
            LiveStateStoreOutsideSource::FailedStartup(source) => source
                .retire_state_native_sources(factory, terminal, prepay, issue_metadata_pin_inputs),
        })
    }

    /// Every pre-seal stage of this graph has completed: the Store parent
    /// join with its native authorities retired, every packet's destination
    /// permit and metadata pins, and State's native-source stage. The
    /// caller's release of the factory and reservation follows only this.
    pub fn require_pre_seal_complete(&self) -> Result<()> {
        self.require_original_process()?;
        match &self.held.source {
            LiveStateStoreOutsideSource::Closed(source) => source.require_pre_seal_complete(),
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.require_pre_seal_complete()
            }
        }
    }

    /// Advance the real Runtime roots/unused input aliases under the original
    /// outside supervisor. The registry remains open until every producer has
    /// handed over its actual owners; no seal or close receipt is issued here.
    pub fn advance_runtime_physical_sources(
        &mut self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            match &mut **source.borrow_mut() {
                LiveStateStoreOutsideSource::Closed(source) => {
                    // SAFETY: whole actual graph and registry stay cached in
                    // this supervisor across refusal and callback unwind.
                    unsafe { source.register_runtime_sources(factory, aggregate, prepay) }
                }
                LiveStateStoreOutsideSource::FailedStartup(source) => {
                    // SAFETY: same unpublished original graph, no new grant.
                    unsafe { source.register_runtime_sources(factory, aggregate, prepay) }
                }
            }
        })
    }
    /// Advance actual closed or recovered-startup projection/schema sources
    /// and Operational/Runtime native handoffs. Parent and physical custody
    /// remain distinct pending stages.
    pub fn advance_state_projection_sources(
        &mut self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        let source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| match &mut **source
            .borrow_mut()
        {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.advance_state_projection_sources(factory, terminal, prepay)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.advance_state_projection_sources(factory, terminal, prepay)
            }
        })
    }

    /// Consume packet-local destination tokens only after their own source
    /// registrations have receipts and their typed native-reference joins have
    /// succeeded. Each refusal retains the remaining token in this Store.
    pub fn consume_retired_packet_destinations(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        let source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| match &mut **source
            .borrow_mut()
        {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.consume_retired_packet_destinations(registry, prepay)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.consume_retired_packet_destinations(registry, prepay)
            }
        })
    }

    /// Consume packet-owned bound-root registrations only after their exact
    /// destination transitions and same-registry close receipts are complete.
    pub fn consume_retired_packet_bound_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        let source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| match &mut **source
            .borrow_mut()
        {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.consume_retired_packet_bound_roots(registry, prepay)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.consume_retired_packet_bound_roots(registry, prepay)
            }
        })
    }

    /// Consume only exact packet metadata-pin receipts after the native close
    /// has retired the same pin owners. Destination and bound-root receipts
    /// must already be consumed; packet inspection and error evidence remains.
    pub fn consume_retired_packet_metadata_pins(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        let source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| match &mut **source
            .borrow_mut()
        {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.consume_retired_packet_metadata_pins(registry, prepay)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.consume_retired_packet_metadata_pins(registry, prepay)
            }
        })
    }

    /// Exhaust packet source frames only after each packet's destination,
    /// bound-root and metadata-pin receipts match this retired registry. A
    /// sanitized acquisition-failure fact remains in the packet census.
    pub fn consume_retired_packet_source_parts(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<u8> {
        self.require_original_process()?;
        let source = std::cell::RefCell::new(&mut self.held.get_mut().source);
        let consumed = std::cell::Cell::new(0u8);
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            consumed.set(match &mut **source.borrow_mut() {
                LiveStateStoreOutsideSource::Closed(source) => {
                    source.consume_retired_packet_source_parts(registry, prepay)
                }
                LiveStateStoreOutsideSource::FailedStartup(source) => {
                    source.consume_retired_packet_source_parts(registry, prepay)
                }
            }?);
            Ok(())
        })?;
        Ok(consumed.get())
    }

    /// Consume only the closed State/thread metadata source registrations from
    /// the same retired inventory. Failed-start Store custody uses the
    /// producer-issued receipt created at `close_retained`; its source absence
    /// is not inferred from this enum variant.
    pub fn consume_retired_auxiliary_physical_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        self.require_original_process()?;
        match &mut self.held.get_mut().source {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.consume_retired_auxiliary_sources(registry)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.consume_retired_auxiliary_sources(registry)
            }
        }
    }

    /// Consume the retained Store parent authority frame only after its roots,
    /// operator close and receiver completion bind to this exact retired
    /// inventory. Normal and failed-start Store owners retain distinct source
    /// frames and each validates its own producer path.
    pub fn consume_retired_parent_finalization_parts(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        self.require_original_process()?;
        match &mut self.held.get_mut().source {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.consume_retired_parent_finalization_parts(registry)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.consume_retired_parent_finalization_parts(registry)
            }
        }
    }

    /// Consume only the State projection-root registrations from this exact
    /// retired inventory. The remaining State, Runtime, packet and Scheduler
    /// source graph stays retained for its own typed transitions.
    pub fn consume_retired_state_projection_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        match &mut self.held.get_mut().source {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.consume_retired_state_projection_sources(registry, prepay)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.consume_retired_state_projection_sources(registry, prepay)
            }
        }
    }

    /// Consume selected State/Operational root registrations after the
    /// State-projection stage (or its typed absence) and exact registry retirement.
    pub fn consume_retired_state_root_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        match &mut self.held.get_mut().source {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.consume_retired_state_root_sources(registry, prepay)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.consume_retired_state_root_sources(registry, prepay)
            }
        }
    }

    /// Consume the actual Runtime directory and namespace registrations from
    /// the same retired registry. Runtime semantic and packet evidence remains
    /// retained for its own later owner transitions.
    pub fn consume_retired_runtime_shared_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        match &mut self.held.get_mut().source {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.consume_retired_runtime_shared_roots(registry, prepay)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.consume_retired_runtime_shared_roots(registry, prepay)
            }
        }
    }

    /// Consume the unopened Scheduler root registrations with their exact
    /// close receipts from the retired registry. Refusal keeps them.
    pub fn consume_retired_scheduler_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        self.require_original_process()?;
        match &mut self.held.get_mut().source {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.consume_retired_scheduler_sources(registry)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.consume_retired_scheduler_sources(registry)
            }
        }
    }

    /// Consume the State, Operational and Runtime outside frames after their
    /// receipt stages. Each frame refuses intact and stays in its slot.
    pub fn consume_retired_owner_frames(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        match &mut self.held.get_mut().source {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.consume_retired_owner_frames(registry, prepay)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.consume_retired_owner_frames(registry, prepay)
            }
        }
    }

    /// Final consumption of the whole outside graph against the exact retired
    /// registry. Refuses, returning this owner intact, while any descriptor,
    /// lock, pin or registry membership remains; the output owns nothing.
    pub fn into_exhausted(
        self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> std::result::Result<StoreExhausted, (Self, anyhow::Error)> {
        if let Err(error) = self.require_original_process() {
            return Err((self, error));
        }
        let source = match self.into_source() {
            Ok(source) => source,
            Err(owner) => {
                return Err((owner, anyhow::anyhow!("foreign live Store outside custody")));
            }
        };
        match source {
            LiveStateStoreOutsideSource::Closed(source) => source
                .into_exhausted(registry)
                .map_err(|(source, error)| (Self::closed(source), error)),
            LiveStateStoreOutsideSource::FailedStartup(source) => source
                .into_exhausted(registry)
                .map_err(|(source, error)| (Self::failed_startup(source), error)),
        }
    }

    /// Prepare the receiver on the SAME concrete normal/failed-start graph.
    /// The graph remains retained here on callback refusal or caught unwind.
    pub fn prepare_destination_join(
        &self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| match &self
            .held
            .source
        {
            LiveStateStoreOutsideSource::Closed(source) => {
                source.prepare_destination_join(factory, terminal, prepay)
            }
            LiveStateStoreOutsideSource::FailedStartup(source) => {
                source.prepare_destination_join(factory, terminal, prepay)
            }
        })
    }
    pub(super) fn closed(original: ClosedLiveStateStore) -> Self {
        Self::from_source(LiveStateStoreOutsideSource::Closed(original))
    }
    pub(super) fn failed_startup(original: ReleasedLiveStateStoreStartup) -> Self {
        Self::from_source(LiveStateStoreOutsideSource::FailedStartup(original))
    }
    /// Recover the SAME complete concrete graph for a consuming physical
    /// continuation. This moves existing owners without reopening databases,
    /// renewing admission, closing descriptors, or acknowledging an account.
    /// A foreign process keeps the whole outside owner on refusal.
    pub fn into_original_source(self) -> std::result::Result<LiveStateStoreOutsideSource, Self> {
        self.into_source()
    }

    /// # Safety
    /// The consuming native finalizer supplies the exact settled account/input
    /// owner and this wrapper retains the one original pre-mapping inventory.
    /// The returned registration is cached by that owner and every refusal
    /// returns the actual descriptor source unchanged.
    pub unsafe fn register_finalizer_directory_source(
        &mut self,
        owner: Arc<lillux::PinnedDirectory>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        (Arc<lillux::PinnedDirectory>, anyhow::Error),
    > {
        if let Err(error) = self.require_original_process() {
            return Err((owner, error));
        }
        unsafe {
            self.held
                .get_mut()
                .source
                .register_finalizer_directory_source(owner, prepay)
        }
    }

    /// # Safety
    /// `owner` is one exact settled account/input descriptor and its genuine
    /// close owner. The source graph retains the same inventory through the
    /// transfer; a refusal returns the same owner without closing it.
    pub unsafe fn register_finalizer_control_record_source(
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
        if let Err(error) = self.require_original_process() {
            return Err((owner, error));
        }
        unsafe {
            self.held
                .get_mut()
                .source
                .register_finalizer_control_record_source(owner, prepay)
        }
    }

    pub fn require_finalizer_source_membership(
        &mut self,
        registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity> {
        self.require_original_process()?;
        self.held
            .get_mut()
            .source
            .require_finalizer_source_membership(registration, prepay)
    }

    /// # Safety
    /// All Store, account and input sources have joined this exact inventory,
    /// all producers are excluded, and the original retained owner pays the
    /// bounded registry transition.
    pub unsafe fn seal_finalizer_physical_inventory(&mut self) -> Result<()> {
        self.require_original_process()?;
        unsafe {
            self.held
                .get_mut()
                .source
                .seal_finalizer_physical_inventory()
        }
    }

    /// # Safety
    /// The same original inventory has been sealed after its complete census;
    /// `prepay` is its retained outside-cleanup authority.
    pub unsafe fn prepare_finalizer_descriptor_owners(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        unsafe {
            self.held
                .get_mut()
                .source
                .prepare_finalizer_descriptor_owners(prepay)
        }
    }

    pub fn finalizer_non_account_descriptors_retired(&mut self) -> Result<bool> {
        self.require_original_process()?;
        self.held
            .get_mut()
            .source
            .finalizer_non_account_descriptors_retired()
    }

    /// # Safety
    /// The caller has completed durable settlement and retained the same
    /// outside authority which funded the actual inventory. This forwards to
    /// the exact registry inside the source; it does not create another one.
    pub unsafe fn retire_finalizer_non_account_descriptors(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_inputs: &dyn Fn(
            &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
            Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
        ) -> Result<
            Arc<lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs>,
        >,
    ) -> Result<()> {
        self.require_original_process()?;
        unsafe {
            self.held
                .get_mut()
                .source
                .retire_finalizer_non_account_descriptors(prepay, issue_inputs)
        }
    }

    /// # Safety
    /// This forwards the original settled-capacity finalizer's alias-retirement
    /// transition to the same retained normal or failed-start Store frame.
    pub unsafe fn retire_settled_store_exclusion_alias(
        &mut self,
        admission_alias: Option<&Arc<ryeos_node_runtime::state_lock::StateLockLease>>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        unsafe {
            self.held
                .get_mut()
                .source
                .retire_settled_store_exclusion_alias(admission_alias, prepay)
        }
    }

    #[cfg(target_os = "linux")]
    pub unsafe fn retire_finalizer_operator_targets(
        &mut self,
        deadline: lillux::time::MonotonicDeadline,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_original_process()?;
        unsafe {
            self.held
                .get_mut()
                .source
                .retire_finalizer_operator_targets(deadline, prepay)
        }
    }

    #[cfg(target_os = "linux")]
    pub unsafe fn retire_finalizer_operator_parent_descriptor(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_inputs: &dyn Fn(
            &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
            Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
        ) -> Result<
            Arc<lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs>,
        >,
    ) -> Result<()> {
        self.require_original_process()?;
        unsafe {
            self.held
                .get_mut()
                .source
                .retire_finalizer_operator_parent_descriptor(prepay, issue_inputs)
        }
    }

    /// Checks only original-process custody. It does not authorize physical
    /// effects, prove zero native affinity, or release any namespace/account.
    pub fn require_original_process(&self) -> Result<()> {
        ensure!(
            !self.held.is_foreign(),
            "foreign live Store outside custody"
        );
        Ok(())
    }
}

/// Require the actual pre-mapping registry already transferred into this graph.
/// Shutdown cannot allocate a replacement inventory from remaining numeric credit.
pub(super) fn require_physical_registry(
    slot: &Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry>,
    factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
) -> Result<()> {
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    ensure!(
        slot.is_some(),
        "actual pre-mapping physical inventory has not transferred"
    );
    Ok(())
}

#[cfg(test)]
mod destination_retry_tests {
    use super::{
        MAX_LIVE_ACCOUNTING_NATIVE_PACKET_SOURCES, MAX_LIVE_SCHEDULER_NATIVE_PACKET_SOURCES,
        MAX_LIVE_SELECTED_NATIVE_PACKET_SOURCES, MAX_OPERATIONAL_PACKET_SOURCES,
        MAX_RUNTIME_PACKET_SOURCES, MAX_SELECTED_PACKET_SOURCES, OriginalPacketSourceKind,
        OutsideWork, PACKET_OPTIONAL_SOURCE_CENSUS_STEPS, next_destination_retry,
        quote_destination_retries, quote_packet_metadata_pin_retirements, quote_packet_parts,
        quote_retired_packet_bound_roots, quote_retired_packet_metadata_pin_receipts,
        quote_retired_packet_source_parts, register_bound_root_source,
        require_auxiliary_packet_domains_within_bounds, require_packet_count_within_source_bound,
    };

    #[test]
    fn auxiliary_packet_bound_is_per_producer_domain() {
        use lillux_sqlite::OriginalNativeDomain as Domain;
        let full = [
            Some(Domain::NodeScheduler),
            Some(Domain::NodeSchedulerSchemaReference),
            Some(Domain::NodeAccounting),
            Some(Domain::NodeAccountingSchemaReference),
        ];
        require_auxiliary_packet_domains_within_bounds(full.into_iter()).unwrap();
        // Four packets fit the shared total, but never three from one producer.
        let scheduler = [
            Some(Domain::NodeScheduler),
            Some(Domain::NodeSchedulerSchemaReference),
            Some(Domain::NodeScheduler),
            Some(Domain::NodeAccounting),
        ];
        assert!(require_auxiliary_packet_domains_within_bounds(scheduler.into_iter()).is_err());
        let accounting = [
            Some(Domain::NodeAccounting),
            Some(Domain::NodeAccountingSchemaReference),
            Some(Domain::NodeAccounting),
        ];
        assert!(require_auxiliary_packet_domains_within_bounds(accounting.into_iter()).is_err());
    }

    #[test]
    fn destination_retry_allowance_and_quote_are_finite() {
        let mut retries = 0;
        for expected in 1..=4 {
            retries = next_destination_retry(retries).expect("retry remains within the allowance");
            assert_eq!(retries, expected);
        }
        assert!(next_destination_retry(retries).is_err());
        assert_eq!(retries, 4);
        assert_eq!(quote_destination_retries(0).unwrap().steps, 0);
        assert_eq!(quote_destination_retries(1).unwrap().steps, 16);
        assert_eq!(quote_destination_retries(3).unwrap().steps, 48);
        assert_eq!(quote_destination_retries(1).unwrap().bytes, 0);
        assert!(quote_destination_retries(MAX_SELECTED_PACKET_SOURCES + 1).is_err());
        let one = quote_packet_parts(1, 1).unwrap();
        assert_eq!(
            one.bytes,
            (std::mem::size_of::<super::OriginalPacketPartsProgress>()
                + std::mem::size_of::<Vec<super::OriginalPacketPartsProgress>>())
                as u64
        );
        let complete = quote_packet_parts(MAX_SELECTED_PACKET_SOURCES, 2).unwrap();
        assert_eq!(
            complete.bytes,
            (MAX_SELECTED_PACKET_SOURCES
                * std::mem::size_of::<super::OriginalPacketPartsProgress>()
                + 2 * std::mem::size_of::<Vec<super::OriginalPacketPartsProgress>>())
                as u64
        );
        assert_eq!(
            complete.steps,
            (MAX_SELECTED_PACKET_SOURCES
                * (4 + usize::try_from(PACKET_OPTIONAL_SOURCE_CENSUS_STEPS).unwrap())
                + 2 * 4) as u64
        );
        assert!(quote_packet_parts(MAX_SELECTED_PACKET_SOURCES + 1, 2).is_err());
        assert!(
            quote_packet_parts(
                MAX_SELECTED_PACKET_SOURCES,
                super::MAX_SELECTED_PACKET_COLLECTIONS
            )
            .is_ok()
        );
        assert!(
            quote_packet_parts(
                MAX_SELECTED_PACKET_SOURCES,
                super::MAX_SELECTED_PACKET_COLLECTIONS + 1
            )
            .is_err()
        );
    }

    #[test]
    fn packet_progress_uses_exact_producer_owned_limits() {
        assert_eq!(MAX_RUNTIME_PACKET_SOURCES, 2);
        assert_eq!(MAX_OPERATIONAL_PACKET_SOURCES, 2);
        assert_eq!(MAX_LIVE_SCHEDULER_NATIVE_PACKET_SOURCES, 2);
        assert_eq!(MAX_LIVE_ACCOUNTING_NATIVE_PACKET_SOURCES, 2);
        assert_eq!(
            MAX_LIVE_SELECTED_NATIVE_PACKET_SOURCES,
            MAX_RUNTIME_PACKET_SOURCES
                + MAX_OPERATIONAL_PACKET_SOURCES
                + MAX_LIVE_SCHEDULER_NATIVE_PACKET_SOURCES
                + MAX_LIVE_ACCOUNTING_NATIVE_PACKET_SOURCES
        );
        assert!(
            require_packet_count_within_source_bound(4, OriginalPacketSourceKind::Scheduler)
                .is_ok()
        );
        assert!(
            require_packet_count_within_source_bound(5, OriginalPacketSourceKind::Scheduler)
                .is_err()
        );
        assert!(
            require_packet_count_within_source_bound(2, OriginalPacketSourceKind::Runtime).is_ok()
        );
        assert!(
            require_packet_count_within_source_bound(3, OriginalPacketSourceKind::Runtime).is_err()
        );
        assert!(
            require_packet_count_within_source_bound(2, OriginalPacketSourceKind::Operational)
                .is_ok()
        );
        assert!(
            require_packet_count_within_source_bound(3, OriginalPacketSourceKind::Operational)
                .is_err()
        );
        assert_eq!(quote_retired_packet_bound_roots(0).unwrap().steps, 0);
        assert_eq!(
            quote_retired_packet_bound_roots(MAX_SELECTED_PACKET_SOURCES)
                .unwrap()
                .steps,
            18 * u64::try_from(MAX_SELECTED_PACKET_SOURCES).unwrap()
        );
        assert!(quote_retired_packet_bound_roots(MAX_SELECTED_PACKET_SOURCES + 1).is_err());
    }

    #[test]
    fn packet_metadata_pin_receipt_quote_covers_pin_roles_and_optional_source_census() {
        let one = quote_retired_packet_metadata_pin_receipts(1).unwrap();
        let maximum =
            quote_retired_packet_metadata_pin_receipts(MAX_SELECTED_PACKET_SOURCES).unwrap();
        assert_eq!(
            one.steps,
            2 * super::RETIRED_PACKET_METADATA_PIN_RECEIPT_STEPS
                + super::PACKET_OPTIONAL_SOURCE_RECEIPT_STEPS
        );
        assert_eq!(
            maximum.steps,
            u64::try_from(MAX_SELECTED_PACKET_SOURCES).unwrap()
                * (2 * super::RETIRED_PACKET_METADATA_PIN_RECEIPT_STEPS
                    + super::PACKET_OPTIONAL_SOURCE_RECEIPT_STEPS)
        );
        assert_eq!(
            maximum.bytes,
            u64::try_from(MAX_SELECTED_PACKET_SOURCES).unwrap()
                * 2
                * super::quote_retired_packet_metadata_pin_receipt()
                    .unwrap()
                    .bytes
        );
        assert!(
            quote_retired_packet_metadata_pin_receipts(MAX_SELECTED_PACKET_SOURCES + 1).is_err()
        );
        assert_eq!(
            quote_packet_metadata_pin_retirements(MAX_SELECTED_PACKET_SOURCES)
                .unwrap()
                .steps,
            u64::try_from(MAX_SELECTED_PACKET_SOURCES).unwrap()
                * super::PACKET_METADATA_PIN_RETIREMENT_STATE_STEPS
        );
        assert!(quote_packet_metadata_pin_retirements(MAX_SELECTED_PACKET_SOURCES + 1).is_err());
    }

    #[test]
    fn retired_packet_source_quote_is_bounded_and_has_no_extra_backing() {
        assert_eq!(
            quote_retired_packet_source_parts(0).unwrap(),
            OutsideWork { bytes: 0, steps: 0 }
        );
        assert_eq!(
            quote_retired_packet_source_parts(MAX_SELECTED_PACKET_SOURCES).unwrap(),
            OutsideWork {
                bytes: 0,
                steps: u64::try_from(MAX_SELECTED_PACKET_SOURCES).unwrap()
                    * super::RETIRED_PACKET_SOURCE_PARTS_STEPS,
            }
        );
        assert!(quote_retired_packet_source_parts(MAX_SELECTED_PACKET_SOURCES + 1).is_err());
    }

    #[test]
    fn completed_bound_root_validation_does_not_charge_again() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = std::sync::Arc::new(
            lillux::PinnedDirectory::open(temporary.path())
                .unwrap()
                .unwrap(),
        );
        let mut registry = unsafe {
            lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::issued(
                1,
                &|_, _| Ok(()),
            )
        }
        .unwrap();
        let registration = unsafe { registry.register_directory(directory, &|_, _| Ok(())) }
            .unwrap_or_else(|(_, error)| panic!("bound-root fixture registration failed: {error}"));
        let mut root = None;
        let mut saved_registration = Some(registration);
        let calls = std::cell::Cell::new(0usize);
        register_bound_root_source(
            &mut root,
            &mut saved_registration,
            &mut registry,
            &|_, _| {
                calls.set(calls.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 0);
        assert!(root.is_none());
        assert!(saved_registration.is_some());
    }
}

#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const BOUND: &str = "ryeos-app::packet-bound-resources";
    const PACKET: &str = "ryeos-app::packet-parts-progress";
    const DATABASE: &str = "ryeos-app::unopened-scheduler-database";
    const TRANSIENT: &str = "ryeos-app::unopened-native-transient";
    const OUTSIDE: &str = "ryeos-app::store-outside-custody";
    const CLOSED: &str = "ryeos-app::closed-store";

    fn abandoned(kind: &str) -> usize {
        lillux::custody::abandoned_custody().count(kind)
    }

    fn pinned_tempdir() -> (tempfile::TempDir, Arc<lillux::PinnedDirectory>) {
        let dir = tempfile::tempdir().unwrap();
        let pinned = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        (dir, pinned)
    }

    /// Bound resources of never-opened inputs on `root`, with the root
    /// retained inside when `keep_root` holds. No ticket is issued, so the
    /// shared host releases its arena whatever becomes of the resources.
    fn bound_resources(
        host: &lillux_sqlite::test_host::TestNativeHost,
        root: Arc<lillux::PinnedDirectory>,
        keep_root: bool,
    ) -> OriginalPacketBoundResources {
        // SAFETY: test-only issuance under the shared host's own funded factory.
        let inputs = unsafe {
            lillux_sqlite::OriginalNativeBoundInputs::issued(
                root,
                std::ffi::OsStr::new("packet.sqlite3"),
                lillux_sqlite::OriginalNativeBoundMode::OpenOrCreate,
                lillux_sqlite::OriginalNativeDomain::NodeScheduler,
                host.destination(),
                None,
            )
        }
        .unwrap();
        let lillux_sqlite::OriginalNativeOutsideBoundParts {
            root,
            name,
            mode,
            domain,
            authority,
            inspection,
        } = inputs
            .into_original_parts()
            .ok()
            .expect("fresh inputs split in their own process");
        drop((authority, inspection));
        OriginalPacketBoundResources::new(keep_root.then_some(root), None, name, mode, domain)
    }

    #[test]
    fn dropped_retired_bound_resources_release_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = ryeos_state::test_support::shared_test_host();
        let (_dir, root) = pinned_tempdir();
        let bound = bound_resources(&host, Arc::clone(&root), false);
        assert!(bound.held.holders_retired());
        let before = abandoned(BOUND);
        drop(bound);
        assert_eq!(abandoned(BOUND), before);
        assert_eq!(Arc::strong_count(&root), 1, "the root alias was released");
        ryeos_state::test_support::release_shared_test_host(host);
    }

    #[test]
    fn dropped_bound_resources_with_root_are_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = ryeos_state::test_support::shared_test_host();
        let (_dir, root) = pinned_tempdir();
        let bound = bound_resources(&host, Arc::clone(&root), true);
        assert!(!bound.held.holders_retired());
        let before = abandoned(BOUND);
        drop(bound);
        assert_eq!(abandoned(BOUND), before + 1);
        assert_eq!(Arc::strong_count(&root), 2, "the root alias stays retained");
        ryeos_state::test_support::release_shared_test_host(host);
    }

    fn packet(
        authority: Option<Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>>,
    ) -> OriginalPacketPartsProgress {
        let mut packet = OriginalPacketPartsProgress::new_progress(
            None,
            None,
            None,
            OriginalPacketSourceKind::Runtime,
            None,
            PacketMetadataPinPhase::ConsumedAbsent,
        );
        packet.held.get_mut().source_parts_consumed = true;
        packet.held.get_mut().authority = authority;
        packet.bound_root_receipt_consumed = true;
        packet
    }

    #[test]
    fn dropped_exhausted_packet_releases_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let packet = packet(None);
        packet.require_exhausted().unwrap();
        let before = abandoned(PACKET);
        packet.exhaust();
        assert_eq!(abandoned(PACKET), before);
    }

    #[test]
    fn dropped_packet_with_authority_is_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = ryeos_state::test_support::shared_test_host();
        // The authority alias holds no ticket, so the host still releases
        // its arena while the abandoned packet keeps the alias.
        let packet = packet(Some(host.destination()));
        assert!(packet.require_exhausted().is_err());
        let before = abandoned(PACKET);
        drop(packet);
        assert_eq!(abandoned(PACKET), before + 1);
        ryeos_state::test_support::release_shared_test_host(host);
    }

    fn unopened_database(
        root: Option<Arc<lillux::PinnedDirectory>>,
    ) -> UnopenedSchedulerDatabaseProgress {
        UnopenedSchedulerDatabaseProgress {
            held: Held::new(
                "ryeos-app::unopened-scheduler-database",
                UnopenedSchedulerDatabaseParts {
                    root,
                    authority: None,
                    inspection: None,
                    registration: None,
                    handoff_id: None,
                    name: None,
                    mode: None,
                    domain: None,
                    authority_joined: true,
                    registration_consumed: true,
                },
                unopened_scheduler_database_settled,
            ),
            root_identity_checked: true,
        }
    }

    #[test]
    fn dropped_exhausted_unopened_database_releases_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let database = unopened_database(None);
        database.require_exhausted().unwrap();
        let before = abandoned(DATABASE);
        database.exhaust();
        assert_eq!(abandoned(DATABASE), before);
    }

    #[test]
    fn dropped_unopened_database_with_root_is_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let (_dir, root) = pinned_tempdir();
        let database = unopened_database(Some(Arc::clone(&root)));
        assert!(database.require_exhausted().is_err());
        let before = abandoned(DATABASE);
        drop(database);
        assert_eq!(abandoned(DATABASE), before + 1);
        assert_eq!(Arc::strong_count(&root), 2, "the root alias stays retained");
    }

    fn unopened_transient(
        authority: Option<Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>>,
    ) -> UnopenedNativeTransientProgress {
        UnopenedNativeTransientProgress {
            held: Held::new(
                "ryeos-app::unopened-native-transient",
                UnopenedNativeTransientParts {
                    joined: authority.is_none(),
                    domain: None,
                    authority,
                },
                unopened_native_transient_settled,
            ),
            expected_domain: lillux_sqlite::OriginalNativeDomain::NodeSchedulerSchemaReference,
        }
    }

    #[test]
    fn dropped_joined_transient_releases_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let transient = unopened_transient(None);
        assert!(transient.is_joined());
        let before = abandoned(TRANSIENT);
        transient.exhaust();
        assert_eq!(abandoned(TRANSIENT), before);
    }

    #[test]
    fn dropped_transient_with_authority_is_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = ryeos_state::test_support::shared_test_host();
        let transient = unopened_transient(Some(host.destination()));
        assert!(!transient.is_joined());
        let before = abandoned(TRANSIENT);
        drop(transient);
        assert_eq!(abandoned(TRANSIENT), before + 1);
        ryeos_state::test_support::release_shared_test_host(host);
    }

    #[test]
    fn dropped_outside_custody_is_retained_whole() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(
            module_path!(),
            "::dropped_outside_custody_is_retained_whole"
        );
        // The retained graph keeps the Store's namespace exclusion and native
        // context, which only the process end releases, so the scenario runs
        // in its own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "outside +1; closed +0", "{outcome}");
            return;
        }
        let app_root = tempfile::tempdir().unwrap();
        let (closed, retained) = super::super::test_native::closed_store_for_test(app_root.path());
        let custody = LiveStateStoreOutsideCustody::closed(closed);
        let before = (abandoned(OUTSIDE), abandoned(CLOSED));
        drop(custody);
        let after = (abandoned(OUTSIDE), abandoned(CLOSED));
        lillux_sqlite::test_host::end_isolated_process(
            (app_root, retained),
            &format!(
                "outside +{}; closed +{}",
                after.0 - before.0,
                after.1 - before.1
            ),
        )
    }
}
