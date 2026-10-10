//! The original outside cleanup partition, retained independently of native
//! storage, account debt and exclusion aliases. Only the genuine reservation
//! can construct it after irrevocably transferring that partition once.

use super::LiveNativeCapacityInputs;
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, Mutex, OnceLock, Weak};

struct Remaining {
    work: u64,
    storage_used: u64,
}

enum ClockState {
    Open {
        owner: Arc<lillux::time::RetainedOccupancyClock>,
        input_alias_joined: bool,
    },
    // A before-effect close refusal returns the exact clock by value. Keeping
    // it inline avoids allocating a replacement Arc after an irreversible
    // finalization attempt has begun.
    Ready(lillux::time::RetainedOccupancyClock),
    Closing,
    Retired(lillux::time::RetiredOccupancyClock),
    Refused(lillux::time::RetainedClockCloseFailure),
}

enum ClockOwnerForTerminal {
    Shared(Arc<lillux::time::RetainedOccupancyClock>),
    Ready(lillux::time::RetainedOccupancyClock),
}

enum CachedOutsideRetirementInputState {
    Unissued,
    Issuing,
    Ready(Arc<lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs>),
}

struct CachedOutsideRetirementInput {
    registration: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
    owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    state: CachedOutsideRetirementInputState,
}

struct LateDescriptorRetirementAuthority {
    late: Weak<LateNativeControl>,
    registration: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
    owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    operator_parent: bool,
}

struct LivePacketMetadataPinRetirementAuthority {
    process: u32,
    late: Weak<LateNativeControl>,
    factory: Weak<lillux_sqlite::OriginalNativeFactory>,
    terminal: Weak<lillux_sqlite::OriginalNativeTerminal>,
    leaf: lillux_sqlite::OriginalNativeOutsideLeafEvidence,
    pins: [Option<Weak<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
}

impl lillux_sqlite::OriginalMetadataPinRetirementAuthority
    for LivePacketMetadataPinRetirementAuthority
{
    fn require_original(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        pin: &Arc<lillux_sqlite::RetainedOriginalMetadataPin>,
    ) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign live packet metadata pin");
        let original_factory = self.factory.upgrade().context("live packet factory absent")?;
        let original_terminal = self.terminal.upgrade().context("live packet terminal absent")?;
        ensure!(Arc::ptr_eq(factory, &original_factory)
            && Arc::ptr_eq(terminal, &original_terminal),
            "live packet metadata pin selects another native terminal");
        ensure!(self.pins.iter().flatten().any(|original|
            original.upgrade().is_some_and(|original| Arc::ptr_eq(&original, pin))),
            "live packet metadata pin is outside its exact packet inventory");
        self.leaf.require_original(factory, terminal)?;
        self.late.upgrade().context("live packet cleanup funding absent")?.require_original()
    }

    fn debit_original_steps(&self, steps: u64) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign live packet metadata debit");
        self.late.upgrade().context("live packet cleanup funding absent")?.prepay(0, steps)
    }
}

// SAFETY: constructed only by the retained late owner for the exact original
// registry membership and descriptor owner. The weak back-reference avoids an
// ownership cycle; the reservation/finalizer keeps the late owner alive while
// any issued input can authorize work.
unsafe impl lillux_sqlite::OriginalOutsideFinalizerFileRetirementAuthority
    for LateDescriptorRetirementAuthority
{
    fn require_original(
        &self,
        owner: &Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    ) -> Result<()> {
        ensure!(
            Arc::ptr_eq(owner, &self.owner),
            "outside retirement input selects another descriptor owner"
        );
        ensure!(
            Arc::ptr_eq(&self.registration.owner()?, owner),
            "outside retirement input lost its exact registry membership"
        );
        ensure!(
            self.registration.is_operator_lock()? == self.operator_parent,
            "outside descriptor input selected another source role"
        );
        #[cfg(target_os = "linux")]
        if self.operator_parent {
            ensure!(
                self.registration.operator_target_retired()?,
                "operator parent input precedes its actual target close"
            );
        }
        #[cfg(not(target_os = "linux"))]
        ensure!(
            !self.operator_parent,
            "operator parent retirement requires Linux"
        );
        self.late
            .upgrade()
            .context("original late cleanup owner absent")?
            .require_original()
    }

    fn debit_original_steps(&self, steps: u64) -> Result<()> {
        self.late
            .upgrade()
            .context("original late cleanup owner absent")?
            .prepay(0, steps)
    }
}

pub(super) struct LateNativeControl {
    clock: Mutex<ClockState>,
    registry_identity: OnceLock<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity>,
    registry_retirement: OnceLock<lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry>,
    process: lillux::process_control::ExactProcessIdentity,
    occupancy: lillux::time::OccupancyLimit,
    cleanup: std::time::Duration,
    deadline: lillux::time::MonotonicDeadline,
    storage_limit: u64,
    maximum_sources: usize,
    remaining: Mutex<Remaining>,
    retirement_inputs: Mutex<Vec<Option<CachedOutsideRetirementInput>>>,
}

impl LateNativeControl {
    /// Maximum fixed control backing for any admitted identity/clock input.
    /// This allows the pure pre-debit cleanup quote to cover the retained late
    /// owner without depending on a live input value.
    pub(super) fn maximum_storage_quote(
        maximum_sources: usize,
    ) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        let inline = std::mem::size_of::<Self>()
            .checked_add(2 * std::mem::size_of::<usize>())
            .context("late cleanup owner layout overflow")?;
        let coordinate_bytes = 64usize
            .checked_add(64)
            .and_then(|bytes| bytes.checked_add(64))
            .context("late cleanup maximum coordinate storage overflow")?;
        let retirement_bytes = maximum_sources
            .checked_mul(std::mem::size_of::<Option<CachedOutsideRetirementInput>>())
            .context("late cleanup descriptor progress storage overflow")?;
        Ok(lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: u64::try_from(
                inline
                    .checked_add(coordinate_bytes)
                    .and_then(|bytes| bytes.checked_add(retirement_bytes))
                    .context("late cleanup maximum storage quote overflow")?,
            )?,
            steps: Self::owner_construction_steps(maximum_sources)?,
        })
    }

    fn owner_construction_steps(maximum_sources: usize) -> Result<u64> {
        u64::try_from(maximum_sources)?
            .checked_add(1)
            .context("late cleanup owner construction work overflow")
    }

    /// Quote the retained owner and every copied String payload. The String
    /// headers are already included in `size_of::<Self>()`; these source
    /// lengths bound the three owned backing buffers created below.
    pub(super) fn storage_quote(
        inputs: &LiveNativeCapacityInputs,
        maximum_sources: usize,
    ) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        ensure!(
            inputs.process.boot_id.len() <= 64,
            "late cleanup boot identity exceeds its fixed coordinate bound"
        );
        ensure!(
            inputs.occupancy.start.contract_digest.len() == 64
                && inputs.occupancy.start.incarnation_digest.len() == 64,
            "late cleanup clock digests exceed or miss their fixed coordinate bounds"
        );
        let inline = std::mem::size_of::<Self>()
            .checked_add(2 * std::mem::size_of::<usize>())
            .context("late cleanup owner layout overflow")?;
        let coordinate_bytes = inputs
            .process
            .boot_id
            .len()
            .checked_add(inputs.occupancy.start.contract_digest.len())
            .and_then(|bytes| bytes.checked_add(inputs.occupancy.start.incarnation_digest.len()))
            .context("late cleanup coordinate storage overflow")?;
        let retirement_bytes = maximum_sources
            .checked_mul(std::mem::size_of::<Option<CachedOutsideRetirementInput>>())
            .context("late cleanup descriptor progress storage overflow")?;
        let quote = lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: u64::try_from(
                inline
                    .checked_add(coordinate_bytes)
                    .and_then(|bytes| bytes.checked_add(retirement_bytes))
                    .context("late cleanup storage quote overflow")?,
            )
            .context("late cleanup storage quote exceeds u64")?,
            steps: Self::owner_construction_steps(maximum_sources)?,
        };
        ensure!(
            quote.bytes <= Self::maximum_storage_quote(maximum_sources)?.bytes
                && quote.steps <= Self::maximum_storage_quote(maximum_sources)?.steps,
            "late cleanup inputs exceed their fixed source bound"
        );
        Ok(quote)
    }

    fn retirement_input_quote() -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        let inline = std::mem::size_of::<LateDescriptorRetirementAuthority>()
            .checked_add(std::mem::size_of::<
                lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs,
            >())
            .context("outside descriptor input layout overflow")?;
        // Two Arc allocations retain the concrete authority and the input.
        let allocation_overhead = 4usize
            .checked_mul(std::mem::size_of::<usize>())
            .context("outside descriptor input allocator quote overflow")?;
        Ok(lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: u64::try_from(
                inline
                    .checked_add(allocation_overhead)
                    .context("outside descriptor input storage quote overflow")?,
            )?,
            steps: 4,
        })
    }

    fn packet_metadata_pin_input_quote(
        pin_count: usize,
    ) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        ensure!(pin_count > 0 && pin_count <= 2,
            "live packet metadata-pin inventory exceeds its two producer roles");
        let bytes = std::mem::size_of::<LivePacketMetadataPinRetirementAuthority>()
            .checked_add(std::mem::size_of::<lillux_sqlite::OriginalMetadataPinRetirementInputs>())
            .and_then(|bytes| pin_count.checked_mul(std::mem::size_of::<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>())
                .and_then(|pins| bytes.checked_add(pins)))
            // One Arc control block for the authority, one for the issued
            // inputs, and one bounded Vec allocation for the exact pins.
            .and_then(|bytes| bytes.checked_add(4 * std::mem::size_of::<usize>()))
            .context("live packet metadata-pin input storage quote overflow")?;
        let steps = u64::try_from(pin_count)?.checked_mul(4)
            .and_then(|steps| steps.checked_add(8))
            .context("live packet metadata-pin input work quote overflow")?;
        Ok(lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: u64::try_from(bytes)?,
            steps,
        })
    }

    /// Issue the kernel's exact-pin retirement input from the same late owner
    /// that retains the original clock and cleanup partition. The caller must
    /// pass pins borrowed from one actual closed packet; the leaf and each pin
    /// are then bound into this one-shot input before any pin is retired.
    pub(super) fn packet_metadata_pin_retirement_inputs(
        self: &Arc<Self>,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: Arc<lillux_sqlite::OriginalNativeTerminal>,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        source_pins: &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
    ) -> Result<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>> {
        self.require_original()?;
        terminal.require_original(&factory)?;
        leaf.require_original(&factory, &terminal)?;
        let pin_count = source_pins.iter().flatten().count();
        let quote = Self::packet_metadata_pin_input_quote(pin_count)?;
        self.prepay(quote.bytes, quote.steps)?;

        let mut pins = Vec::new();
        pins.try_reserve_exact(pin_count)
            .context("reserve exact live packet metadata pins")?;
        for pin in source_pins.iter().flatten() {
            pins.push(Arc::clone(pin));
        }
        let authority = Arc::new(LivePacketMetadataPinRetirementAuthority {
            process: std::process::id(),
            late: Arc::downgrade(self),
            factory: Arc::downgrade(&factory),
            terminal: Arc::downgrade(&terminal),
            leaf: leaf.clone(),
            pins: std::array::from_fn(|index| source_pins[index].as_ref().map(Arc::downgrade)),
        });
        let authority: Arc<dyn lillux_sqlite::OriginalMetadataPinRetirementAuthority> = authority;
        // SAFETY: this private issuer receives the exact pins from one actual
        // live packet, binds its leaf and same factory/terminal, and charges
        // the original retained late-cleanup owner before allocating inputs.
        let inputs = unsafe {
            lillux_sqlite::OriginalMetadataPinRetirementInputs::issued(
                factory,
                terminal,
                pins,
                authority,
            )?
        };
        Ok(Arc::new(inputs))
    }

    /// One bounded registration-slot lookup plus at most two opaque membership
    /// validations. The cache is indexed by the exact registry's source slot,
    /// so lookup work is constant-time rather than a scan of prior retirements.
    fn retirement_lookup_quote(
        maximum_sources: usize,
    ) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        let membership = lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::membership_work_quote(
            maximum_sources,
            3,
        )?;
        membership.checked_add(lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: 0,
            // Exact owner/role checks and the fixed cache-state transition are
            // constant-time work outside the opaque membership comparisons.
            steps: 8,
        })
    }

    /// Worst-case descriptor and packet-owner authority issuance, close
    /// receipts, and source consumption across the selected owner bounds and
    /// every finalizer attempt. Failed-attempt backing is not refunded.
    pub(super) fn maximum_packet_metadata_pin_retirement_quote(
    ) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        let packet_count = ryeos_app::state_store::MAX_LIVE_SELECTED_NATIVE_PACKET_SOURCES;
        let attempts = u64::from(super::MAX_FINALIZATION_ATTEMPTS);
        let packet_calls = u64::try_from(packet_count)?
            .checked_mul(attempts)
            .context("packet metadata-pin input retry bound overflow")?;
        let input = Self::packet_metadata_pin_input_quote(2)?.checked_mul(packet_calls)?;
        let census = ryeos_app::state_store::quote_packet_metadata_pin_retirements(packet_count)?
            .checked_mul(attempts)?;
        let receipts_per_pass =
            ryeos_app::state_store::quote_retired_packet_metadata_pin_receipts(packet_count)?;
        // One pass can allocate one receipt per pin before close; another
        // validates and consumes those same receipts after native disposal.
        let receipt_steps = receipts_per_pass.steps
            .checked_mul(attempts)
            .and_then(|steps| steps.checked_mul(2))
            .context("packet receipt retry work quote overflow")?;
        let receipts = lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: receipts_per_pass.bytes
                .checked_mul(attempts)
                .context("packet receipt retry storage quote overflow")?,
            steps: receipt_steps,
        };
        let close_attempts = packet_calls
            .checked_mul(2)
            .context("packet metadata-pin close attempt bound overflow")?;
        let close = lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: 0,
            steps: close_attempts
                .checked_mul(lillux_sqlite::ORIGINAL_METADATA_PIN_RETIREMENT_STEPS)
                .context("packet metadata-pin native close work quote overflow")?,
        };
        let source_parts = ryeos_app::state_store::quote_retired_packet_source_parts(packet_count)?
            .checked_mul(attempts)?;
        input.checked_add(census)?.checked_add(receipts)?.checked_add(close)?
            .checked_add(source_parts)
    }

    /// Worst-case State-generated Bound metadata-pin retirement across every
    /// finalizer attempt: per closed State leaf (one per resource ticket) the
    /// issued two-pin input and two native close charges, plus State's own
    /// census, cached receipt storage, strong-owner retirement and the
    /// post-release receipt checks of the projection and owner-frame stages.
    /// Failed-attempt backing is not refunded.
    pub(super) fn maximum_state_metadata_pin_retirement_quote(
        maximum_resource_tickets: u32,
    ) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        let attempts = u64::from(super::MAX_FINALIZATION_ATTEMPTS);
        let leaves = u64::from(maximum_resource_tickets);
        let close = lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: 0,
            steps: 2 * lillux_sqlite::ORIGINAL_METADATA_PIN_RETIREMENT_STEPS,
        };
        let per_leaf = Self::packet_metadata_pin_input_quote(2)?.checked_add(close)?;
        let state = ryeos_state::state_db::ClosedLiveStatePhysicalCustody::quote_maximum_metadata_pin_attempt(
            maximum_resource_tickets,
        )?;
        per_leaf.checked_mul(leaves)?.checked_add(state)?.checked_mul(attempts)
    }

    /// Worst-case lookup and input issuance across every descriptor and every
    /// bounded finalizer pass, plus selected Store packet-pin, State projection,
    /// generated-root, State/Operational receipt transitions, and the one-time
    /// retained Operational marker owner allocation. Issuance bytes are charged
    /// on every failed attempt because the outside account does not refund
    /// transient backing.
    pub(super) fn maximum_retirement_inputs_quote(
        maximum_sources: usize,
        maximum_resource_tickets: u32,
    ) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        let calls = u64::try_from(maximum_sources)?
            .checked_mul(u64::from(super::MAX_FINALIZATION_ATTEMPTS))
            .context("outside descriptor retirement call bound overflow")?;
        let generated_root_receipts = ryeos_state::state_db::quote_retired_generated_root_receipts(
            usize::try_from(maximum_resource_tickets)?,
            usize::try_from(maximum_resource_tickets)?,
        )?.checked_mul(u64::from(super::MAX_FINALIZATION_ATTEMPTS))?;
        let ticket_count = usize::try_from(maximum_resource_tickets)?;
        let projection_slots = ticket_count.checked_mul(2)
            .context("live State projection slot bound overflow")?;
        let projection_roots = ryeos_state::state_db::maximum_live_projection_physical_sources(
            maximum_resource_tickets,
        )?;
        let projection_receipts = ryeos_state::state_db::quote_live_projection_retired_sources(
            projection_slots,
            projection_roots,
        )?.checked_mul(u64::from(super::MAX_FINALIZATION_ATTEMPTS))?;
        let state_root_receipts = ryeos_state::state_db::quote_live_state_finalization_root_receipts()?
            .checked_mul(u64::from(super::MAX_FINALIZATION_ATTEMPTS))?;
        let operational_metadata = ryeos_state::operational::quote_maximum_operational_metadata_retirement()?
            .checked_mul(u64::from(super::MAX_FINALIZATION_ATTEMPTS))?;
        Self::retirement_lookup_quote(maximum_sources)?
            .checked_add(Self::retirement_input_quote()?)?
            .checked_mul(calls)?
            .checked_add(Self::maximum_packet_metadata_pin_retirement_quote()?)?
            .checked_add(Self::maximum_state_metadata_pin_retirement_quote(maximum_resource_tickets)?)?
            .checked_add(generated_root_receipts)?
            .checked_add(projection_receipts)?
            .checked_add(state_root_receipts)?
            .checked_add(operational_metadata)?
            .checked_add(ryeos_state::operational::quote_operational_marker_owner()?)
    }

    /// This is not callable from a policy DTO or decoded receipt. The private
    /// reservation invokes it immediately after consuming its actual raw native
    /// counter's outside split, before first mapping, and caches the result.
    pub(super) fn from_native_transfer(
        inputs: &LiveNativeCapacityInputs,
        storage_limit: u64,
        work: u64,
        maximum_sources: usize,
    ) -> Result<Arc<Self>> {
        inputs.require_original(true)?;
        ensure!(
            work == inputs.policy.incarnation.outside_cleanup_steps && work > 0,
            "late cleanup differs from its original transferred partition"
        );
        let owner_quote = Self::storage_quote(inputs, maximum_sources)?;
        ensure!(
            storage_limit >= owner_quote.bytes,
            "late cleanup cannot cover its retained owner"
        );
        ensure!(
            work > owner_quote.steps,
            "late cleanup cannot cover its retained owner construction"
        );
        let mut retirement_inputs = Vec::new();
        retirement_inputs
            .try_reserve_exact(maximum_sources)
            .context("reserve exact outside descriptor retirement progress")?;
        retirement_inputs.resize_with(maximum_sources, || None);
        // Ordinary wrapper/Arc storage is bounded by the selected enclosing
        // node scope. These fields do not borrow any mapped native metadata.
        Ok(Arc::new(Self {
            clock: Mutex::new(ClockState::Open {
                owner: Arc::clone(&inputs.clock),
                input_alias_joined: false,
            }),
            registry_identity: OnceLock::new(),
            registry_retirement: OnceLock::new(),
            process: inputs.process.clone(),
            occupancy: inputs.occupancy.clone(),
            cleanup: std::time::Duration::from_millis(
                inputs.policy.incarnation.cleanup_occupancy_ms,
            ),
            deadline: inputs.cleanup_deadline,
            storage_limit,
            maximum_sources,
            remaining: Mutex::new(Remaining {
                work: work - owner_quote.steps,
                storage_used: owner_quote.bytes,
            }),
            retirement_inputs: Mutex::new(retirement_inputs),
        }))
    }

    /// Bind the late owner to the exact physical registry before the first
    /// native mapping. The registry identity is evidence only; it grants no
    /// access to registry contents or late cleanup work.
    pub(super) fn bind_original_registry_identity(
        &self,
        identity: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistryIdentity,
    ) -> Result<()> {
        self.require_original_process()?;
        self.registry_identity
            .set(identity)
            .map_err(|_| anyhow::anyhow!("original physical registry identity already bound"))
    }

    fn require_retired_registry(
        &self,
        receipt: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        self.require_original_process()?;
        receipt.require_same_registry(
            self.registry_identity
                .get()
                .context("original physical registry identity was not bound before mapping")?,
        )
    }

    /// Retain the consumed-registry receipt inside the already quoted late
    /// owner. This is idempotent for the exact original identity and preserves
    /// the proof across bounded clock-close retries.
    pub(super) fn retain_registry_retirement(
        &self,
        receipt: lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        // Cache the one-shot receipt before any fallible check so a refusal
        // cannot lose it; only a duplicate is ever left with the caller.
        let duplicate = self.registry_retirement.set(receipt).err();
        self.require_original_process()?;
        let identity = self
            .registry_identity
            .get()
            .context("original physical registry identity was not bound before mapping")?;
        if let Some(receipt) = duplicate {
            receipt.require_same_registry(identity)?;
        }
        self.retired_registry()?;
        Ok(())
    }

    pub(super) fn retired_registry(
        &self,
    ) -> Result<&lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry> {
        let receipt = self
            .registry_retirement
            .get()
            .context("original physical registry has not been consumed")?;
        self.require_retired_registry(receipt)?;
        Ok(receipt)
    }

    pub(super) fn registry_retired(&self) -> bool {
        self.registry_retirement.get().is_some()
    }

    /// Return the cached, owner-bound outside retirement input, or issue it
    /// once from this same late authority after debiting its fixed backing.
    /// The vector backing is reserved before first mapping; no bookkeeping
    /// mutex spans the debit or authority callback.
    pub(super) fn outside_retirement_inputs(
        self: &Arc<Self>,
        registration: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
        owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    ) -> Result<Arc<lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs>> {
        let lookup_quote = Self::retirement_lookup_quote(self.maximum_sources)?;
        self.prepay(lookup_quote.bytes, lookup_quote.steps)?;
        let registry_identity = self
            .registry_identity
            .get()
            .context("original physical registry identity was not bound before mapping")?;
        let slot_index = registration.source_slot_in(registry_identity)?;
        ensure!(
            slot_index < self.maximum_sources,
            "outside descriptor source exceeds its exact registry bound"
        );
        ensure!(
            Arc::ptr_eq(&registration.owner()?, &owner),
            "outside retirement input differs from its actual registry owner"
        );
        let operator_parent = registration.is_operator_lock()?;
        #[cfg(target_os = "linux")]
        if operator_parent {
            ensure!(
                registration.operator_target_retired()?,
                "operator parent input precedes its actual target close"
            );
        }
        #[cfg(not(target_os = "linux"))]
        ensure!(
            !operator_parent,
            "operator parent retirement requires Linux"
        );
        {
            let mut cached = self
                .retirement_inputs
                .try_lock()
                .map_err(|_| anyhow::anyhow!("outside descriptor input cache busy or poisoned"))?;
            let slot = cached
                .get_mut(slot_index)
                .context("outside descriptor slot exceeds its preallocated cache")?;
            if let Some(entry) = slot {
                ensure!(entry.registration.same_member(registration)?,
                    "outside descriptor cache slot names another registration");
                ensure!(
                    Arc::ptr_eq(&entry.registration.owner()?, &owner),
                    "cached outside descriptor membership changed"
                );
                match std::mem::replace(
                    &mut entry.state,
                    CachedOutsideRetirementInputState::Issuing,
                ) {
                    CachedOutsideRetirementInputState::Unissued => {}
                    CachedOutsideRetirementInputState::Ready(inputs) => {
                        entry.state = CachedOutsideRetirementInputState::Ready(Arc::clone(&inputs));
                        return Ok(inputs);
                    }
                    CachedOutsideRetirementInputState::Issuing => {
                        entry.state = CachedOutsideRetirementInputState::Issuing;
                        anyhow::bail!("outside descriptor input issuance is already active");
                    }
                }
            } else {
                *slot = Some(CachedOutsideRetirementInput {
                    registration: registration.clone(),
                    owner: owner.clone(),
                    state: CachedOutsideRetirementInputState::Issuing,
                });
            }
        }

        let result =
            (|| -> Result<Arc<lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs>> {
                let quote = Self::retirement_input_quote()?;
                self.prepay(quote.bytes, quote.steps)?;
                let authority = Arc::new(LateDescriptorRetirementAuthority {
                    late: Arc::downgrade(self),
                    registration: registration.clone(),
                    owner: owner.clone(),
                    operator_parent,
                });
                // SAFETY: this exact member/owner is in the preallocated original
                // registry; the private authority keeps only weak access to the
                // late owner and debits its original finite counters.
                let inputs = unsafe {
                    lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs::issued(
                        owner.clone(),
                        authority,
                    )?
                };
                inputs.require_original_owner(&owner)?;
                Ok(Arc::new(inputs))
            })();

        let mut cached = self
            .retirement_inputs
            .try_lock()
            .map_err(|_| anyhow::anyhow!("outside descriptor input cache busy or poisoned"))?;
        let entry = cached
            .get_mut(slot_index)
            .and_then(Option::as_mut)
            .context("outside descriptor input reservation disappeared")?;
        ensure!(entry.registration.same_member(registration)?,
            "outside descriptor cache slot changed during issuance");
        ensure!(
            matches!(&entry.state, CachedOutsideRetirementInputState::Issuing),
            "outside descriptor input issuance state changed"
        );
        match result {
            Ok(inputs) => {
                entry.state = CachedOutsideRetirementInputState::Ready(Arc::clone(&inputs));
                Ok(inputs)
            }
            Err(error) => {
                // Keep the bounded slot and actual source binding for a later
                // charged retry; no input or close owner is discarded.
                entry.state = CachedOutsideRetirementInputState::Unissued;
                Err(error)
            }
        }
    }

    /// Retire the canonical exclusion target only after the exact registry
    /// reports every non-operator descriptor receipt. The registry preserves
    /// its target/parent owner and charges this same late counter before each
    /// irreversible close.
    #[cfg(target_os = "linux")]
    pub(super) fn retire_operator_targets(
        &self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    ) -> Result<()> {
        self.require_original()?;
        // SAFETY: the caller holds the same exclusive original registry after
        // durable settlement; its own predicate enforces non-operator receipts.
        unsafe {
            registry
                .retire_operator_targets(self.deadline, &|bytes, steps| self.prepay(bytes, steps))
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn original_deadline(&self) -> Result<lillux::time::MonotonicDeadline> {
        self.require_original()?;
        Ok(self.deadline)
    }

    pub(super) fn require_original(&self) -> Result<()> {
        self.require_original_process()?;
        ensure!(
            !self.deadline.has_elapsed(),
            "original late cleanup deadline exhausted"
        );
        let clock = self
            .clock
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original late cleanup clock busy or poisoned"))?;
        let owner = match &*clock {
            ClockState::Open { owner, .. } => owner.as_ref(),
            ClockState::Ready(owner) => owner,
            _ => anyhow::bail!("original late cleanup clock is already terminal or closing"),
        };
        let phase = owner.window_fixed(&self.occupancy, self.cleanup).map_err(anyhow::Error::msg)?;
        ensure!(
            matches!(
                phase,
                lillux::time::OccupancyWindowState::Service { .. }
                    | lillux::time::OccupancyWindowState::Cleanup { .. }
            ),
            "original late cleanup interval exhausted"
        );
        Ok(())
    }

    pub(super) fn require_original_process(&self) -> Result<()> {
        lillux::process_control::require_current_process_identity_fixed(&self.process)
            .map_err(anyhow::Error::msg)
    }

    /// Consume the exact input-source clock alias after verifying that it
    /// shares this late owner's original allocation. No clock descriptors are
    /// closed here; the owner retains the sole alias for its last stage.
    pub(super) fn join_clock_input_alias(
        &self,
        alias: Arc<lillux::time::RetainedOccupancyClock>,
        retired_registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> std::result::Result<(), (Arc<lillux::time::RetainedOccupancyClock>, anyhow::Error)> {
        if let Err(error) = self.require_retired_registry(retired_registry) {
            return Err((alias, error));
        }
        if let Err(error) = self.require_original_process() {
            return Err((alias, error));
        }
        let mut state = match self.clock.try_lock() {
            Ok(state) => state,
            Err(_) => {
                return Err((
                    alias,
                    anyhow::anyhow!("original late cleanup clock busy or poisoned"),
                ));
            }
        };
        let ClockState::Open {
            owner,
            input_alias_joined,
        } = &mut *state
        else {
            return Err((
                alias,
                anyhow::anyhow!("original late cleanup clock is not open"),
            ));
        };
        if !Arc::ptr_eq(owner, &alias) {
            return Err((
                alias,
                anyhow::anyhow!("native input clock alias is foreign"),
            ));
        }
        if *input_alias_joined {
            return Err((
                alias,
                anyhow::anyhow!("native input clock alias was already joined"),
            ));
        }
        *input_alias_joined = true;
        drop(state);
        drop(alias);
        Ok(())
    }

    /// Last clock operation of the outside owner. The finalizer first joins
    /// the input alias, then this method prepays the Lillux-owned close quote
    /// while clock observation is still valid. It never retries after either
    /// descriptor close was entered; a before-effect refusal restores the
    /// same clock into this state for the shared bounded finalizer retry.
    pub(super) fn retire_clock(
        &self,
        retired_registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        self.require_retired_registry(retired_registry)?;
        if self.clock_retired()? {
            return Ok(());
        }
        self.require_original()?;
        self.prepay(
            0,
            lillux::time::RetainedOccupancyClock::terminal_close_work_per_attempt(),
        )?;
        let (clock_owner, limit, cleanup, deadline) = {
            let mut state = self
                .clock
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original late cleanup clock busy or poisoned"))?;
            match std::mem::replace(&mut *state, ClockState::Closing) {
                ClockState::Open {
                    owner,
                    input_alias_joined: true,
                } => (
                    ClockOwnerForTerminal::Shared(owner),
                    self.occupancy.clone(),
                    self.cleanup,
                    self.deadline,
                ),
                ClockState::Open {
                    owner,
                    input_alias_joined: false,
                } => {
                    *state = ClockState::Open {
                        owner,
                        input_alias_joined: false,
                    };
                    anyhow::bail!("original late cleanup clock input alias is not joined");
                }
                ClockState::Ready(owner) => (
                    ClockOwnerForTerminal::Ready(owner),
                    self.occupancy.clone(),
                    self.cleanup,
                    self.deadline,
                ),
                ClockState::Retired(receipt) => {
                    *state = ClockState::Retired(receipt);
                    return Ok(());
                }
                ClockState::Refused(failure) => {
                    *state = ClockState::Refused(failure);
                    anyhow::bail!("original late cleanup clock close is terminally unresolved");
                }
                ClockState::Closing => {
                    *state = ClockState::Closing;
                    anyhow::bail!("original late cleanup clock close is already active");
                }
            }
        };
        let clock = match clock_owner {
            ClockOwnerForTerminal::Ready(clock) => clock,
            ClockOwnerForTerminal::Shared(owner) => match Arc::try_unwrap(owner) {
                Ok(clock) => clock,
                Err(owner) => {
                    let mut state = self.clock.try_lock().map_err(|_| {
                        anyhow::anyhow!("original late cleanup clock busy or poisoned")
                    })?;
                    ensure!(matches!(*state, ClockState::Closing), "original clock close state changed");
                    *state = ClockState::Open { owner, input_alias_joined: true };
                    anyhow::bail!("original clock descriptor owner remains aliased");
                }
            },
        };
        match clock.into_terminal(&limit, cleanup, deadline) {
            Ok(receipt) => {
                let mut state = self
                    .clock
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("original late cleanup clock busy or poisoned"))?;
                ensure!(
                    matches!(*state, ClockState::Closing),
                    "original clock close state changed"
                );
                *state = ClockState::Retired(receipt);
                Ok(())
            }
            Err(failure) if !failure.effects_entered() => match failure.into_original() {
                Ok(clock) => {
                    let mut state = self.clock.try_lock().map_err(|_| {
                        anyhow::anyhow!("original late cleanup clock busy or poisoned")
                    })?;
                    ensure!(
                        matches!(*state, ClockState::Closing),
                        "original clock close state changed"
                    );
                    *state = ClockState::Ready(clock);
                    anyhow::bail!("original clock close refused before descriptor effects");
                }
                Err(failure) => {
                    let mut state = self.clock.try_lock().map_err(|_| {
                        anyhow::anyhow!("original late cleanup clock busy or poisoned")
                    })?;
                    ensure!(
                        matches!(*state, ClockState::Closing),
                        "original clock close state changed"
                    );
                    *state = ClockState::Refused(failure);
                    anyhow::bail!(
                        "original clock close refusal could not restore original authority"
                    );
                }
            },
            Err(failure) => {
                let mut state = self
                    .clock
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("original late cleanup clock busy or poisoned"))?;
                ensure!(
                    matches!(*state, ClockState::Closing),
                    "original clock close state changed"
                );
                *state = ClockState::Refused(failure);
                anyhow::bail!("original clock close entered and remains unresolved");
            }
        }
    }

    pub(super) fn clock_retired(&self) -> Result<bool> {
        let state = self
            .clock
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original late cleanup clock busy or poisoned"))?;
        Ok(matches!(*state, ClockState::Retired(_)))
    }

    pub(super) fn clock_input_alias_joined(&self) -> Result<bool> {
        let state = self
            .clock
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original late cleanup clock busy or poisoned"))?;
        Ok(match &*state {
            ClockState::Open { input_alias_joined, .. } => *input_alias_joined,
            ClockState::Ready(_) | ClockState::Closing | ClockState::Retired(_) | ClockState::Refused(_) => true,
        })
    }

    /// Irreversible use of the SAME paid counters. Successful transfer/close
    /// callers cache their receipts; only unfinished attempts spend again.
    pub(super) fn prepay(&self, bytes: u64, work: u64) -> Result<()> {
        self.require_original()?;
        ensure!(work > 0, "late cleanup requires positive original work");
        {
            let mut remaining = self
                .remaining
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original late cleanup counters busy or poisoned"))?;
            remaining.work = remaining
                .work
                .checked_sub(work)
                .context("original late cleanup work exhausted")?;
            let used = remaining
                .storage_used
                .checked_add(bytes)
                .context("original late cleanup storage overflow")?;
            ensure!(
                used <= self.storage_limit,
                "original late cleanup storage exhausted"
            );
            remaining.storage_used = used;
        }
        // No counter lock spans host observation, descriptor operations or
        // callbacks. Late refusal preserves debits; it does not refund them.
        self.require_original()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retirement_cache_quotes_constant_lookup_and_every_bounded_issue_attempt() {
        let small_lookup = LateNativeControl::retirement_lookup_quote(2).unwrap();
        let large_lookup = LateNativeControl::retirement_lookup_quote(425).unwrap();
        assert_eq!(small_lookup, large_lookup);
        assert_eq!(small_lookup.steps, 24);

        let input = LateNativeControl::retirement_input_quote().unwrap();
        let all_attempts = LateNativeControl::maximum_retirement_inputs_quote(2, 2).unwrap();
        let maximum_calls = 2 * u64::from(super::super::MAX_FINALIZATION_ATTEMPTS);
        let descriptor_attempts = small_lookup.checked_add(input).unwrap()
            .checked_mul(maximum_calls).unwrap();
        let packet_pins = LateNativeControl::maximum_packet_metadata_pin_retirement_quote().unwrap();
        let state_pins = LateNativeControl::maximum_state_metadata_pin_retirement_quote(2).unwrap();
        let generated_roots = ryeos_state::state_db::quote_retired_generated_root_receipts(2, 2).unwrap()
            .checked_mul(u64::from(super::super::MAX_FINALIZATION_ATTEMPTS)).unwrap();
        let projection_roots = ryeos_state::state_db::quote_live_projection_retired_sources(4, 6).unwrap()
            .checked_mul(u64::from(super::super::MAX_FINALIZATION_ATTEMPTS)).unwrap();
        let state_roots = ryeos_state::state_db::quote_live_state_finalization_root_receipts().unwrap()
            .checked_mul(u64::from(super::super::MAX_FINALIZATION_ATTEMPTS)).unwrap();
        let operational_metadata = ryeos_state::operational::quote_maximum_operational_metadata_retirement().unwrap()
            .checked_mul(u64::from(super::super::MAX_FINALIZATION_ATTEMPTS)).unwrap();
        let marker_owner = ryeos_state::operational::quote_operational_marker_owner().unwrap();
        assert_eq!(all_attempts,
            descriptor_attempts.checked_add(packet_pins).unwrap()
                .checked_add(state_pins).unwrap()
                .checked_add(generated_roots).unwrap().checked_add(projection_roots).unwrap()
                .checked_add(state_roots).unwrap().checked_add(operational_metadata).unwrap()
                .checked_add(marker_owner).unwrap());
    }

    #[test]
    fn indexed_retirement_cache_backing_is_preallocated_and_quoted_per_source() {
        let small = LateNativeControl::maximum_storage_quote(2).unwrap();
        let large = LateNativeControl::maximum_storage_quote(5).unwrap();
        let entry = u64::try_from(std::mem::size_of::<Option<CachedOutsideRetirementInput>>())
            .unwrap();
        assert_eq!(large.bytes - small.bytes, entry * 3);
        assert_eq!(large.steps - small.steps, 3);
    }

    #[test]
    fn packet_metadata_pin_inputs_quote_exact_bounded_inventory() {
        let one = LateNativeControl::packet_metadata_pin_input_quote(1).unwrap();
        let two = LateNativeControl::packet_metadata_pin_input_quote(2).unwrap();
        assert_eq!(two.bytes - one.bytes,
            u64::try_from(std::mem::size_of::<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>()).unwrap());
        assert_eq!(one.steps, 12);
        assert_eq!(two.steps, 16);
        assert!(LateNativeControl::packet_metadata_pin_input_quote(0).is_err());
        assert!(LateNativeControl::packet_metadata_pin_input_quote(3).is_err());

        let attempts_state = u64::from(super::super::MAX_FINALIZATION_ATTEMPTS);
        let state_leaf = two.checked_add(lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: 0, steps: 2 * lillux_sqlite::ORIGINAL_METADATA_PIN_RETIREMENT_STEPS,
        }).unwrap();
        let state_local = ryeos_state::state_db::ClosedLiveStatePhysicalCustody::quote_maximum_metadata_pin_attempt(64)
            .unwrap();
        assert_eq!(LateNativeControl::maximum_state_metadata_pin_retirement_quote(64).unwrap(),
            state_leaf.checked_mul(64).unwrap().checked_add(state_local).unwrap()
                .checked_mul(attempts_state).unwrap());

        let packet_count = ryeos_app::state_store::MAX_LIVE_SELECTED_NATIVE_PACKET_SOURCES;
        let attempts = u64::from(super::super::MAX_FINALIZATION_ATTEMPTS);
        let calls = u64::try_from(packet_count).unwrap() * attempts;
        let packet_inputs = two.checked_mul(calls).unwrap();
        let census = ryeos_app::state_store::quote_packet_metadata_pin_retirements(packet_count)
            .unwrap().checked_mul(attempts).unwrap();
        let receipt = ryeos_app::state_store::quote_retired_packet_metadata_pin_receipts(packet_count)
            .unwrap();
        let receipt_total = lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: receipt.bytes * attempts,
            steps: receipt.steps * attempts * 2,
        };
        let close_total = lillux_sqlite::original_physical_custody::OutsideWork {
            bytes: 0,
            steps: calls * 2 * lillux_sqlite::ORIGINAL_METADATA_PIN_RETIREMENT_STEPS,
        };
        let source_parts = ryeos_app::state_store::quote_retired_packet_source_parts(packet_count)
            .unwrap().checked_mul(attempts).unwrap();
        assert_eq!(LateNativeControl::maximum_packet_metadata_pin_retirement_quote().unwrap(),
            packet_inputs.checked_add(census).unwrap()
                .checked_add(receipt_total).unwrap()
                .checked_add(close_total).unwrap()
                .checked_add(source_parts).unwrap());
    }
}
