//! State's finite outside receiver. Actual projection leaves share one real
//! State parent; neither an outer Arc count nor an inode comparison identifies
//! that parent. This receiver does not acknowledge physical disposal.
use super::*;
use lillux::custody::Held;
use lillux_sqlite::original_physical_custody::{
    OriginalPhysicalRegistry, OriginalPhysicalRegistryIdentity, OutsideWork,
    RetiredOriginalPhysicalRegistry,
};
use lillux_sqlite::{
    OriginalNativeFactory, OriginalNativeOutsideLeafEvidence, OriginalNativeTerminal,
};

pub fn quote_state_destination_join(maximum: usize) -> anyhow::Result<OutsideWork> {
    anyhow::ensure!(maximum > 0, "empty original State leaf bound");
    let bytes = maximum
        .checked_mul(std::mem::size_of::<RetainedLeaf>())
        .and_then(|bytes| {
            bytes.checked_add(
                std::mem::size_of::<OriginalStateDestinationJoin>()
                    + 2 * std::mem::size_of::<usize>(),
            )
        })
        .context("original State join storage quote overflow")?;
    Ok(OutsideWork {
        bytes: u64::try_from(bytes)?,
        steps: u64::try_from(maximum)?,
    })
}

pub fn quote_live_state_parent_transfer() -> anyhow::Result<OutsideWork> {
    Ok(OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<live_native::LiveStateNativeOwner>())?,
        steps: 8,
    })
}

/// The destination join retains at most one outside leaf per native resource
/// ticket admitted by this factory. Keep that source-derived cardinality
/// explicit instead of treating the allocator's resulting `Vec` capacity as
/// an admission limit.
pub fn maximum_state_destination_leaves(resource_tickets: u32) -> anyhow::Result<usize> {
    let maximum = usize::try_from(resource_tickets)?;
    anyhow::ensure!(maximum > 0, "empty original State destination ticket bound");
    Ok(maximum)
}

pub fn quote_state_source_coalescing(leaves: usize) -> anyhow::Result<OutsideWork> {
    let leaves = u64::try_from(leaves)?;
    let steps = leaves
        .checked_mul(leaves)
        .and_then(|work| work.checked_add(leaves.checked_add(1)?))
        .context("original State source coalescing work quote overflow")?;
    Ok(OutsideWork { bytes: 0, steps })
}

pub fn quote_state_leaf_admission(
    maximum_tickets: u32,
    attempts: u32,
) -> anyhow::Result<OutsideWork> {
    anyhow::ensure!(
        maximum_tickets > 0 && attempts <= maximum_tickets,
        "State leaf admission quote exceeds ticket bound"
    );
    let per_attempt = u64::from(maximum_tickets)
        .checked_add(1)
        .context("original State leaf admission work quote overflow")?;
    Ok(OutsideWork {
        bytes: 0,
        steps: per_attempt
            .checked_mul(u64::from(attempts))
            .context("original State leaf admission work quote overflow")?,
    })
}

pub fn quote_state_leaf_continuations(leaves: usize) -> anyhow::Result<OutsideWork> {
    let per_leaf = OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<StateLeafContinuation>())?,
        steps: 4,
    };
    per_leaf.checked_mul(u64::try_from(leaves)?)
}

pub fn quote_generated_root_registration(
    maximum_slots: usize,
    leaves: usize,
) -> anyhow::Result<OutsideWork> {
    let queries = u64::try_from(leaves)?
        .checked_mul(2)
        .context("State generated membership count overflow")?;
    OriginalPhysicalRegistry::membership_work_quote(maximum_slots, queries)?.checked_add(
        OutsideWork {
            bytes: 0,
            steps: queries,
        },
    )
}

/// Quote the bounded post-disposal validation of generated-root registrations.
/// The caller supplies the actual leaf and registered-root counts; the native
/// capacity owner uses the ticket bound for its worst-case attempt reserve.
pub fn quote_retired_generated_root_receipts(
    leaves: usize,
    generated_roots: usize,
) -> anyhow::Result<OutsideWork> {
    anyhow::ensure!(
        generated_roots <= leaves,
        "retired State generated roots exceed the retained leaf bound"
    );
    let leaf_steps = u64::try_from(leaves)?
        .checked_mul(4)
        .context("retired State generated leaf census quote overflow")?;
    let receipt_steps = u64::try_from(generated_roots)?
        .checked_mul(12)
        .context("retired State generated-root receipt quote overflow")?;
    Ok(OutsideWork {
        bytes: 0,
        steps: leaf_steps
            .checked_add(receipt_steps)
            .and_then(|steps| steps.checked_add(4))
            .context("retired State generated-root quote overflow")?,
    })
}

fn quote_generated_root_registration_at(
    registry: &OriginalPhysicalRegistry,
    leaves: usize,
    additional_sources: usize,
) -> anyhow::Result<OutsideWork> {
    let queries = u64::try_from(leaves)?
        .checked_mul(2)
        .context("State generated membership count overflow")?;
    registry
        .membership_work_bound(queries, additional_sources)?
        .checked_add(OutsideWork {
            bytes: 0,
            steps: queries,
        })
}

pub(crate) struct OriginalStateDestinationJoin {
    process: u32,
    factory: std::sync::Weak<OriginalNativeFactory>,
    maximum: usize,
    state: Mutex<Held<JoinState>>,
}
const JOIN_STATE_KIND: &str = "ryeos-state::destination-join";

/// The join's retained owners and facts. Constructible only in this module,
/// and held so that dropping it unsettled keeps it whole until the process
/// exits: settled exactly when it owns nothing, since every consuming path
/// leaves each slot None or empty first. A retained parent, source alias,
/// generated root or registration ends only through the supervised outside
/// receiver.
struct JoinState {
    // Cache the canonical parent before releasing any redundant parent alias.
    parent: Option<OriginalProjectionStateOwner>,
    leaves: Vec<RetainedLeaf>,
    census_closed: bool,
    // Full actual live parent remains accessible after its canonical Arc body
    // moves. Its Store/lease/factory edges must join the real upper receiver.
    live_parent: Option<live_native::LiveStateNativeOwner>,
    // Moving the body outward is not retirement. Keep native/holder terminal
    // predicates pending until that actual outside body has a terminal join.
    live_parent_outside_pending: bool,
    generated_root_registry: Option<OriginalPhysicalRegistryIdentity>,
    generated_roots_retired: bool,
}
/// No owner remains: the canonical and live parents moved out, and every
/// leaf released its source alias, generated root and registration.
fn join_state_owns_nothing(state: &JoinState) -> bool {
    state.parent.is_none()
        && state.live_parent.is_none()
        && state.leaves.iter().all(|leaf| {
            leaf.source.is_none()
                && leaf
                    .generated
                    .as_ref()
                    .is_none_or(|owner| owner.root.is_none() && owner.registration.is_none())
        })
}
struct RetainedLeaf {
    evidence: OriginalNativeOutsideLeafEvidence,
    source: Option<StateDestinationLeaf>,
    generated: Option<GeneratedLeafCustody>,
}
// Genuine generation/source observations remain with the actual root. Neither
// a weak schema observation nor a path/name is a disposal or reopening grant.
struct GeneratedLeafCustody {
    root: Option<Arc<lillux::PinnedDirectory>>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
    registering: bool,
    schema_reference: std::sync::Weak<crate::sqlite_schema::ClosedNativeSchemaSqlReference>,
    name: std::ffi::OsString,
    expected_identity: Option<(u64, u64)>,
}
pub(super) enum StateDestinationLeaf {
    State(Arc<OriginalProjectionStateOwner>),
    Generated(Arc<OriginalGeneratedProjectionAuthority>),
    Prepared(Arc<OriginalPreparedProjectionAuthority>),
    Live(Arc<live_native::LiveStateNativeOwner>),
}
impl StateDestinationLeaf {
    fn parent(&self) -> OriginalProjectionStateOwner {
        match self {
            Self::State(owner) => owner.as_ref().clone(),
            Self::Generated(owner) => owner.preparation.clone(),
            Self::Prepared(owner) => OriginalProjectionStateOwner::Offline(owner.clone()),
            Self::Live(owner) => OriginalProjectionStateOwner::Live(owner.clone()),
        }
    }
    fn authority(&self) -> Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority> {
        match self {
            Self::State(owner) => owner.clone(),
            Self::Generated(owner) => owner.clone(),
            Self::Prepared(owner) => owner.clone(),
            Self::Live(owner) => owner.clone(),
        }
    }
    fn same_original_source(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::State(a), Self::State(b)) => Arc::ptr_eq(a, b),
            (Self::Generated(a), Self::Generated(b)) => Arc::ptr_eq(a, b),
            (Self::Prepared(a), Self::Prepared(b)) => Arc::ptr_eq(a, b),
            (Self::Live(a), Self::Live(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
    fn into_authority(self) -> Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority> {
        match self {
            Self::State(owner) => owner,
            Self::Generated(owner) => owner,
            Self::Prepared(owner) => owner,
            Self::Live(owner) => owner,
        }
    }
}
impl OriginalProjectionStateOwner {
    fn same_original_parent(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Offline(a), Self::Offline(b)) => Arc::ptr_eq(a, b),
            (Self::Live(a), Self::Live(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
    fn join_link(&self) -> &Mutex<std::sync::Weak<OriginalStateDestinationJoin>> {
        match self {
            Self::Offline(owner) => &owner.outside_join,
            Self::Live(owner) => &owner.outside_join,
        }
    }
    pub(super) fn original_destination_join(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<Arc<OriginalStateDestinationJoin>> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let expected_factory = match self {
            Self::Offline(owner) => owner.factory.clone(),
            Self::Live(owner) => Arc::downgrade(owner.factory()),
        };
        anyhow::ensure!(
            std::sync::Weak::ptr_eq(&expected_factory, &Arc::downgrade(factory)),
            "State parent belongs to another original factory"
        );
        let existing = {
            let link = self
                .join_link()
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original State join link busy or poisoned"))?;
            link.upgrade()
        };
        if let Some(join) = existing {
            join.require_original(factory, aggregate)?;
            return Ok(join);
        }
        let maximum = maximum_state_destination_leaves(factory.maximum_resource_tickets()?)?;
        let quote = quote_state_destination_join(maximum)?;
        // Original callback and allocation precede the retention gate. This
        // is a finite host receiver, not new native admission or a new clock.
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        // Funding callbacks may return in an inherited process. Validate
        // before allocating or retaining aliases of the actual parent graph.
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let candidate = Arc::new(OriginalStateDestinationJoin {
            process: std::process::id(),
            factory: Arc::downgrade(factory),
            maximum,
            state: Mutex::new(Held::new(
                JOIN_STATE_KIND,
                JoinState {
                    parent: Some(self.clone()),
                    leaves: Vec::with_capacity(maximum),
                    census_closed: false,
                    live_parent: None,
                    live_parent_outside_pending: false,
                    generated_root_registry: None,
                    generated_roots_retired: false,
                },
                join_state_owns_nothing,
            )),
        });
        let mut link = match self.join_link().try_lock() {
            Ok(link) => link,
            Err(_) => {
                discard_unpublished(candidate);
                anyhow::bail!("original State join link busy or poisoned");
            }
        };
        if let Some(existing) = link.upgrade() {
            // A concurrent publication won. Its real canonical parent is
            // retained already; release only this unused candidate's aliases.
            drop(link);
            discard_unpublished(candidate);
            existing.require_original(factory, aggregate)?;
            return Ok(existing);
        }
        *link = Arc::downgrade(&candidate);
        Ok(candidate)
    }
}
impl OriginalStateDestinationJoin {
    pub(crate) fn require_original(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign original State join"
        );
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        anyhow::ensure!(
            std::sync::Weak::ptr_eq(&self.factory, &Arc::downgrade(factory)),
            "State join selects another original factory"
        );
        Ok(())
    }
    pub(super) fn holders_retired(&self) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign original State join observation"
        );
        let state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original State join busy or poisoned"))?;
        Ok(state.census_closed
            && state.parent.is_none()
            && state.live_parent.is_none()
            && !state.live_parent_outside_pending
            && state.leaves.iter().all(|leaf| {
                leaf.source.is_none()
                    && leaf.generated.as_ref().is_none_or(|owner| {
                        owner.root.is_none()
                            && !owner.registering
                            && if state.generated_roots_retired {
                                owner.registration.is_none()
                            } else {
                                owner
                                    .registration
                                    .as_ref()
                                    .is_some_and(|binding| binding.retired())
                            }
                    })
            })
            && (!state.generated_roots_retired || state.generated_root_registry.is_some()))
    }
    /// Final post-release census: every holder ended and no generated-root
    /// registration remains unconsumed. Reads only cached receiver state and
    /// the retired registry identity; never upgrades the factory witness.
    pub(crate) fn require_exhausted(
        &self,
        registry: &RetiredOriginalPhysicalRegistry,
    ) -> anyhow::Result<()> {
        self.require_exhausted_holders()?;
        let state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original State join busy or poisoned"))?;
        if let Some(identity) = state.generated_root_registry.as_ref() {
            registry.require_same_registry(identity)?;
        }
        Ok(())
    }
    /// The registry-free part of final exhaustion: every holder retired and
    /// no generated-root registration left unconsumed. A dropped frame that
    /// cached this join settles on exactly these checks.
    pub(crate) fn require_exhausted_holders(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.holders_retired()?,
            "State destination join retains a holder at final exhaustion"
        );
        let state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original State join busy or poisoned"))?;
        anyhow::ensure!(
            state.leaves.iter().all(|leaf| leaf
                .generated
                .as_ref()
                .is_none_or(|owner| owner.registration.is_none())),
            "State destination join retains an unconsumed generated-root registration"
        );
        Ok(())
    }
    /// A join in the shape a completed outside transfer leaves behind: census
    /// closed, no parent, no live parent, nothing registered.
    #[cfg(test)]
    pub(crate) fn exhausted_for_test() -> Arc<Self> {
        Arc::new(Self {
            process: std::process::id(),
            factory: std::sync::Weak::new(),
            maximum: 1,
            state: Mutex::new(Held::new(
                JOIN_STATE_KIND,
                JoinState {
                    parent: None,
                    leaves: Vec::new(),
                    census_closed: true,
                    live_parent: None,
                    live_parent_outside_pending: false,
                    generated_root_registry: None,
                    generated_roots_retired: false,
                },
                join_state_owns_nothing,
            )),
        })
    }
    // Only ordinary wrapper aliases are retired here. The full canonical
    // parent remains cached, and every generated root/name/schema/generation
    // field moves intact into this same receiver before its alias is released.
    fn advance_leaf_sources(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.require_original(factory, aggregate)?;
        let count = {
            let state = self
                .state
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original State leaf quote busy or poisoned"))?;
            if state.leaves.iter().all(|leaf| leaf.source.is_none()) {
                return Ok(());
            }
            state.leaves.len()
        };
        let quote = quote_state_source_coalescing(count)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original State leaf retirement busy or poisoned"))?;
        anyhow::ensure!(
            state.leaves.len() <= count,
            "original State leaf census changed after original quote"
        );
        anyhow::ensure!(
            state.parent.is_some(),
            "original canonical State parent not cached"
        );
        // Coalesce outer allocations, never equal paths or child counts. Each
        // duplicate is removed only while another exact whole source is cached.
        for index in 0..state.leaves.len() {
            let duplicate = state.leaves[index].source.as_ref().is_some_and(|source| {
                state.leaves.iter().skip(index + 1).any(|other| {
                    other
                        .source
                        .as_ref()
                        .is_some_and(|actual| source.same_original_source(actual))
                })
            });
            if duplicate {
                drop(state.get_mut().leaves[index].source.take());
            }
        }
        for index in 0..state.leaves.len() {
            let Some(source) = state.get_mut().leaves[index].source.take() else {
                continue;
            };
            match source {
                StateDestinationLeaf::State(owner) => match Arc::try_unwrap(owner) {
                    Ok(parent_alias) => drop(parent_alias),
                    Err(owner) => {
                        state.get_mut().leaves[index].source =
                            Some(StateDestinationLeaf::State(owner));
                        anyhow::bail!("actual State wrapper still has an external alias");
                    }
                },
                StateDestinationLeaf::Generated(owner) => match Arc::try_unwrap(owner) {
                    Ok(owner) => {
                        // Cache ALL original fields first. This includes the
                        // actual descriptor, not a replacement opened by name.
                        let OriginalGeneratedProjectionAuthority {
                            preparation,
                            schema_reference,
                            root,
                            name,
                            expected_identity,
                        } = owner;
                        state.get_mut().leaves[index].generated = Some(GeneratedLeafCustody {
                            root: Some(root),
                            registration: None,
                            registering: false,
                            schema_reference,
                            name,
                            expected_identity,
                        });
                        // Canonical parent is still retained above; remove only
                        // this generation wrapper's redundant parent alias.
                        drop(preparation);
                    }
                    Err(owner) => {
                        state.get_mut().leaves[index].source =
                            Some(StateDestinationLeaf::Generated(owner));
                        anyhow::bail!("actual generated State wrapper still has an external alias");
                    }
                },
                StateDestinationLeaf::Prepared(owner) => drop(owner),
                StateDestinationLeaf::Live(owner) => drop(owner),
            }
        }
        Ok(())
    }
    /// # Safety
    /// The real closed/failed Store supervisor closed the complete State source
    /// census and removed its own redundant State parent aliases AFTER retaining
    /// the typed graph. All original native packets and authorities transferred.
    /// Moving this body is not native-affinity retirement or physical disposal.
    pub(crate) unsafe fn advance_live_parent(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.require_original(factory, aggregate)?;
        self.advance_leaf_sources(factory, aggregate, prepay)?;
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original live State parent receiver busy or poisoned"))?;
        anyhow::ensure!(
            state.leaves.iter().all(|leaf| leaf.source.is_none()),
            "actual live State leaf aliases remain"
        );
        if state.parent.is_none() {
            anyhow::ensure!(
                state.census_closed && state.live_parent.is_some(),
                "actual live State parent absent without typed body transfer"
            );
            return Ok(());
        }
        anyhow::ensure!(
            matches!(
                state.parent.as_ref(),
                Some(OriginalProjectionStateOwner::Live(_))
            ),
            "live State census cannot consume offline preparation"
        );
        state.get_mut().census_closed = true;
        let OriginalProjectionStateOwner::Live(parent) = state
            .get_mut()
            .parent
            .take()
            .expect("checked actual live State parent")
        else {
            unreachable!()
        };
        match Arc::try_unwrap(parent) {
            Ok(parent) => {
                // Cache every real root/recovery/trust/host field BEFORE any
                // later upper Store transfer or fallible physical operation.
                state.get_mut().live_parent = Some(parent);
                Ok(())
            }
            Err(parent) => {
                state.get_mut().parent = Some(OriginalProjectionStateOwner::Live(parent));
                anyhow::bail!("actual live State parent still has an external alias")
            }
        }
    }
    /// Move the SAME full live parent body into the already retained upper
    /// physical frame. No root or Store/factory edge is dropped by this handoff.
    /// # Safety
    /// The caller retains this receiver and destination slot under the original
    /// outside supervisor, and uses this slot only for this actual receiver.
    pub(crate) unsafe fn transfer_live_parent(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
        destination: &mut Option<live_native::LiveStateNativeOwner>,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.require_original(factory, aggregate)?;
        anyhow::ensure!(
            destination.is_none(),
            "live State parent destination already occupied"
        );
        // Original work/storage debit precedes the bookkeeping gate. Callback
        // refusal leaves the actual parent in its original receiver.
        let quote = quote_live_state_parent_transfer()?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        // Funding is an external callback. Revalidate the original process and
        // aggregate before acquiring the gate or checking out the actual body.
        self.require_original(factory, aggregate)?;
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("live State parent transfer busy or poisoned"))?;
        anyhow::ensure!(
            state.census_closed
                && state.parent.is_none()
                && state.leaves.iter().all(|leaf| leaf.source.is_none()),
            "live State parent transfer lacks complete original source census"
        );
        let original = state
            .get_mut()
            .live_parent
            .take()
            .context("actual live State parent body absent")?;
        state.get_mut().live_parent_outside_pending = true;
        *destination = Some(original);
        Ok(())
    }
    /// End the outside-pending mark of the transferred live parent body once
    /// its parts are registered in the same registry and its host aliases
    /// retired. Idempotent; a refusal changes nothing.
    pub(crate) fn complete_live_parent(
        self: &Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
        parts: &live_native::LiveStatePhysicalParent,
        registry: &OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.require_original(factory, aggregate)?;
        let quote = live_native::quote_live_state_host_alias_retirement(registry)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        self.require_original(factory, aggregate)?;
        parts.require_completed_outside_parent(self, registry)?;
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("live State parent completion busy or poisoned"))?;
        anyhow::ensure!(
            state.census_closed
                && state.parent.is_none()
                && state.live_parent.is_none()
                && state.leaves.iter().all(|leaf| leaf.source.is_none()),
            "live State parent completion lacks its closed source census"
        );
        state.get_mut().live_parent_outside_pending = false;
        Ok(())
    }
    /// Cached completion of the live parent handoff; reads no factory.
    pub(crate) fn live_parent_completed(&self) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign original State join observation"
        );
        let state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original State join busy or poisoned"))?;
        Ok(state.census_closed
            && state.parent.is_none()
            && state.live_parent.is_none()
            && !state.live_parent_outside_pending)
    }
    pub(crate) unsafe fn register_generated_roots(
        &self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign State root registration"
        );
        let registry_identity = registry.identity()?;
        let (count, additional_sources, complete) = {
            let mut state = self
                .state
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original State root inventory busy or poisoned"))?;
            anyhow::ensure!(
                !state.generated_roots_retired,
                "retired State generated-root receipts cannot be registered again"
            );
            if let Some(expected) = state.generated_root_registry.as_ref() {
                expected.require_same_registry(&registry_identity)?;
            } else {
                state.get_mut().generated_root_registry = Some(registry_identity.clone());
            }
            anyhow::ensure!(
                state.census_closed
                    && state.parent.is_none()
                    && state.live_parent.is_none()
                    && state.leaves.iter().all(|leaf| leaf.source.is_none()),
                "actual State parent/source census retirement incomplete"
            );
            anyhow::ensure!(
                state.leaves.len() <= self.maximum,
                "State generated-root census exceeds its owner-derived leaf bound"
            );
            for leaf in state.leaves.iter() {
                if let Some(owner) = &leaf.generated {
                    anyhow::ensure!(
                        !owner.registering,
                        "actual generated root registration already in flight"
                    );
                    anyhow::ensure!(
                        owner.registration.is_none() || owner.root.is_none(),
                        "generated root retains both its source and registry binding"
                    );
                    anyhow::ensure!(
                        owner.registration.is_some() || owner.root.is_some(),
                        "actual generated root absent without original registration"
                    );
                }
            }
            let additional_sources = state
                .leaves
                .iter()
                .filter_map(|leaf| leaf.generated.as_ref())
                .filter(|owner| owner.registration.is_none())
                .count();
            anyhow::ensure!(
                additional_sources <= self.maximum,
                "State generated-root sources exceed their owner-derived bound"
            );
            let complete = state.leaves.iter().all(|leaf| {
                leaf.generated
                    .as_ref()
                    .is_none_or(|owner| owner.root.is_none() && owner.registration.is_some())
            });
            (state.leaves.len(), additional_sources, complete)
        };
        if complete {
            let quote = quote_generated_root_registration_at(registry, count, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            anyhow::ensure!(
                !crate::foreign_process(self.process),
                "foreign State roots after completed-stage validation debit"
            );
            return self.require_generated_roots_registered(registry);
        }
        let quote = quote_generated_root_registration_at(registry, count, additional_sources)?;
        // Counted aliases and membership checks are funded before any
        // source checkout; the original callback runs outside State's gate.
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign State roots after original prepay"
        );
        for index in 0..count {
            let source = {
                let mut state = self
                    .state
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("original State root source busy or poisoned"))?;
                let Some(owner) = &mut state.get_mut().leaves[index].generated else {
                    continue;
                };
                if let Some(binding) = &owner.registration {
                    registry.require_membership(binding)?;
                    continue;
                }
                anyhow::ensure!(
                    !owner.registering,
                    "actual generated root registration already in flight"
                );
                let source = owner
                    .root
                    .as_ref()
                    .context("actual generated root absent without original registration")?
                    .clone();
                owner.registering = true;
                source
            };
            // Keep the actual root cached in State while this exact alias moves
            // into the same supervisor registry. Original callbacks are outside
            // State retention gates and preserve their panic/refusal evidence.
            let result = unsafe { registry.register_directory(source, prepay) };
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let owner = state.get_mut().leaves[index]
                .generated
                .as_mut()
                .expect("cached original generated root");
            owner.registering = false;
            match result {
                Ok(binding) => {
                    owner.registration = Some(binding);
                    // Registry canonical source and exact binding are cached
                    // BEFORE dropping only State's redundant descriptor alias.
                    drop(owner.root.take());
                }
                Err((alias, error)) => {
                    drop(alias); // original root remains accessible above
                    return Err(error);
                }
            }
        }
        self.require_generated_roots_registered(registry)
    }
    pub(super) fn require_generated_roots_registered(
        &self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign State generated registry census"
        );
        let registry_identity = registry.identity()?;
        let state = self
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State generated registry census busy or poisoned"))?;
        anyhow::ensure!(
            !state.generated_roots_retired,
            "retired State generated-root frame reentered the live registry census"
        );
        state
            .generated_root_registry
            .as_ref()
            .context("State generated-root registry identity was not retained")?
            .require_same_registry(&registry_identity)?;
        anyhow::ensure!(
            state.leaves.len() <= self.maximum,
            "State generated registry census exceeds its owner-derived leaf bound"
        );
        anyhow::ensure!(
            state.census_closed && state.parent.is_none() && state.live_parent.is_none(),
            "State original parent or source census remains open"
        );
        for leaf in state.leaves.iter() {
            anyhow::ensure!(
                leaf.source.is_none(),
                "State original leaf source remains untransferred"
            );
            if let Some(generated) = &leaf.generated {
                anyhow::ensure!(
                    !generated.registering && generated.root.is_none(),
                    "State generated original root registration remains pending"
                );
                registry.require_membership(
                    generated
                        .registration
                        .as_ref()
                        .context("State generated original root membership absent")?,
                )?;
            }
        }
        Ok(())
    }

    /// Consume only generated-root registrations after the exact original
    /// physical registry has retired. No factory, native terminal, or source
    /// descriptor is consulted here. A refusal preserves every registration.
    pub(super) fn consume_retired_generated_roots(
        &self,
        registry: &RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign retired State generated-root receipt frame"
        );
        let (leaves, generated_roots, identity) = {
            let state = self.state.try_lock().map_err(|_| {
                anyhow::anyhow!("State generated-root receipt frame busy or poisoned")
            })?;
            let identity = state
                .generated_root_registry
                .as_ref()
                .context("State generated-root frame lacks its original registry identity")?;
            registry.require_same_registry(identity)?;
            if state.generated_roots_retired {
                anyhow::ensure!(
                    state
                        .leaves
                        .iter()
                        .all(|leaf| leaf.generated.as_ref().is_none_or(|owner| owner
                            .root
                            .is_none()
                            && !owner.registering
                            && owner.registration.is_none())),
                    "consumed State generated-root frame regained registration state"
                );
                return Ok(());
            }
            anyhow::ensure!(
                state.census_closed
                    && state.parent.is_none()
                    && state.live_parent.is_none()
                    && !state.live_parent_outside_pending,
                "State generated roots cannot retire before the parent/source census closes"
            );
            anyhow::ensure!(
                state.leaves.len() <= self.maximum,
                "State generated-root receipt census exceeds its retained leaf bound"
            );
            let mut generated_roots = 0usize;
            for leaf in state.leaves.iter() {
                anyhow::ensure!(
                    leaf.source.is_none(),
                    "State generated-root receipt census retains an untransferred leaf"
                );
                if let Some(owner) = leaf.generated.as_ref() {
                    anyhow::ensure!(
                        owner.root.is_none() && !owner.registering,
                        "State generated-root receipt census retains a source or registration attempt"
                    );
                    let binding = owner.registration.as_ref().context(
                        "State generated-root registration absent before retired receipt",
                    )?;
                    anyhow::ensure!(matches!(binding.observed_source_identity()?,
                        lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                        "State generated-root registration changed its Directory role");
                    generated_roots = generated_roots
                        .checked_add(1)
                        .context("State generated-root receipt count overflow")?;
                }
            }
            (state.leaves.len(), generated_roots, identity.clone())
        };

        let quote = quote_retired_generated_root_receipts(leaves, generated_roots)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign State generated-root frame after retirement debit"
        );

        let mut state = self.state.try_lock().map_err(|_| {
            anyhow::anyhow!("State generated-root receipt frame busy after funding")
        })?;
        registry.require_same_registry(&identity)?;
        if state.generated_roots_retired {
            anyhow::ensure!(
                state
                    .leaves
                    .iter()
                    .all(|leaf| leaf
                        .generated
                        .as_ref()
                        .is_none_or(|owner| owner.root.is_none()
                            && !owner.registering
                            && owner.registration.is_none())),
                "consumed State generated-root frame regained registration state"
            );
            return Ok(());
        }
        anyhow::ensure!(
            state.census_closed
                && state.parent.is_none()
                && state.live_parent.is_none()
                && !state.live_parent_outside_pending
                && state.leaves.len() == leaves,
            "State generated-root census changed after retirement funding"
        );
        let current_roots = state
            .leaves
            .iter()
            .filter(|leaf| leaf.generated.is_some())
            .count();
        anyhow::ensure!(
            current_roots == generated_roots,
            "State generated-root owner count changed after retirement funding"
        );
        for leaf in state.leaves.iter() {
            anyhow::ensure!(
                leaf.source.is_none(),
                "State generated-root source appeared after retirement funding"
            );
            if let Some(owner) = leaf.generated.as_ref() {
                anyhow::ensure!(
                    owner.root.is_none() && !owner.registering,
                    "State generated-root source changed after retirement funding"
                );
                let binding = owner
                    .registration
                    .as_ref()
                    .context("State generated-root registration disappeared after funding")?;
                anyhow::ensure!(matches!(binding.observed_source_identity()?,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                    "State generated-root registration changed its Directory role");
                drop(binding.receipt_in(registry)?);
            }
        }
        for leaf in state.get_mut().leaves.iter_mut() {
            if let Some(owner) = leaf.generated.as_mut() {
                owner.registration = None;
            }
        }
        state.get_mut().generated_roots_retired = true;
        Ok(())
    }
    pub(super) fn require_generated_roots_registered_with_prepay(
        &self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !crate::foreign_process(self.process),
            "foreign State generated registry census"
        );
        let queries = {
            let state = self.state.try_lock().map_err(|_| {
                anyhow::anyhow!("original State generated membership census busy or poisoned")
            })?;
            anyhow::ensure!(
                state.leaves.len() <= self.maximum,
                "State generated membership census exceeds its owner-derived leaf bound"
            );
            u64::try_from(
                state
                    .leaves
                    .iter()
                    .filter(|leaf| leaf.generated.is_some())
                    .count(),
            )?
        };
        let quote = registry.membership_work_bound(queries, 0)?;
        let work = quote
            .steps
            .checked_add(8)
            .context("State generated-root census work overflow")?;
        // Do not call the original payer while holding the State retention gate.
        registry.prepay_transfer(prepay, quote.bytes, work)?;
        self.require_generated_roots_registered(registry)
    }
    pub(super) fn retain_leaf(
        &self,
        source: StateDestinationLeaf,
        evidence: OriginalNativeOutsideLeafEvidence,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> Result<
        (),
        (
            StateDestinationLeaf,
            OriginalNativeOutsideLeafEvidence,
            anyhow::Error,
        ),
    > {
        let checked = (|| -> anyhow::Result<()> {
            self.require_original(factory, aggregate)?;
            evidence.require_original_authority(&source.authority(), factory, aggregate)?;
            // Storage is prepaid; the scan still uses the original work debit.
            let maximum = maximum_state_destination_leaves(factory.maximum_resource_tickets()?)?;
            anyhow::ensure!(
                maximum == self.maximum,
                "State destination owner bound changed after construction"
            );
            let quote = quote_state_leaf_admission(u32::try_from(maximum)?, 1)?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                prepay(quote.bytes, quote.steps)
            })
        })();
        if let Err(error) = checked {
            return Err((source, evidence, error));
        }
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(_) => {
                return Err((
                    source,
                    evidence,
                    anyhow::anyhow!("original State join busy or poisoned"),
                ));
            }
        };
        let checked = (|| -> anyhow::Result<()> {
            anyhow::ensure!(!state.census_closed, "original State leaf census is closed");
            anyhow::ensure!(
                state
                    .parent
                    .as_ref()
                    .is_some_and(|parent| parent.same_original_parent(&source.parent())),
                "State leaf selects another actual parent"
            );
            anyhow::ensure!(
                state.leaves.len() < self.maximum,
                "original State leaf capacity exhausted"
            );
            for leaf in state.leaves.iter() {
                anyhow::ensure!(
                    !leaf.evidence.same_original_leaf(&evidence)?,
                    "original State leaf transferred twice"
                );
            }
            Ok(())
        })();
        if let Err(error) = checked {
            return Err((source, evidence, error));
        }
        state.get_mut().leaves.push(RetainedLeaf {
            evidence,
            source: Some(source),
            generated: None,
        });
        Ok(())
    }
}

// Only called before publication, while the actual caller still borrows the
// same parent. No leaf or resource transferred into this unused candidate.
fn discard_unpublished(candidate: Arc<OriginalStateDestinationJoin>) {
    let candidate = Arc::try_unwrap(candidate)
        .ok()
        .expect("unpublished State join is unique");
    let mut state = candidate
        .state
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    drop(state.get_mut().parent.take());
    drop(std::mem::take(&mut state.get_mut().leaves));
    drop(state.get_mut().live_parent.take());
    drop(state);
}

/// Move the actual authority allocation into the shared State receiver. The
/// returned continuation retains that receiver; it is not a disposal receipt.
pub(super) fn transfer_leaf(
    source: StateDestinationLeaf,
    factory: &Arc<OriginalNativeFactory>,
    aggregate: &OriginalNativeTerminal,
    evidence: &OriginalNativeOutsideLeafEvidence,
    prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
) -> Result<
    lillux_sqlite::OriginalDestinationOutsideCustody,
    lillux_sqlite::OriginalDestinationCustodyFailure,
> {
    let prepared = (|| -> anyhow::Result<_> {
        factory.require_original_process()?;
        evidence.require_original_authority(&source.authority(), factory, aggregate)?;
        let parent = source.parent();
        let join = {
            let link = parent
                .join_link()
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original State join link busy or poisoned"))?;
            link.upgrade()
                .context("actual State supervisor has not prepared its original receiver")?
        };
        join.require_original(factory, aggregate)?;
        let quote = quote_state_leaf_continuations(1)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            prepay(quote.bytes, quote.steps)
        })?;
        let continuation = Box::new(StateLeafContinuation {
            join: join.clone(),
            evidence: evidence.clone(),
        });
        Ok((join, continuation))
    })();
    let (join, continuation) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            return Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(
                factory.clone(),
                source.into_authority(),
                error,
            ));
        }
    };
    match join.retain_leaf(source, evidence.clone(), factory, aggregate, prepay) {
        Ok(()) => {
            // All genuine owner fields are now cached by the same typed State
            // receiver. No fallible step follows the consuming source transfer.
            Ok(unsafe {
                lillux_sqlite::OriginalDestinationOutsideCustody::issued(Vec::new(), continuation)
            })
        }
        Err((source, _evidence, error)) => {
            Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(
                factory.clone(),
                source.into_authority(),
                error,
            ))
        }
    }
}
struct StateLeafContinuation {
    join: Arc<OriginalStateDestinationJoin>,
    evidence: OriginalNativeOutsideLeafEvidence,
}
// The receiver owns the actual typed authority allocation and full original
// parent; callbacks do not recurse into another authority or mint a grant.
unsafe impl lillux_sqlite::OriginalOutsideDestinationContinuation for StateLeafContinuation {
    fn require_original_process(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !crate::foreign_process(self.join.process),
            "foreign original State leaf continuation"
        );
        Ok(())
    }
    fn retire_native_references(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.join.require_original(factory, aggregate)?;
        self.evidence.require_original(factory, aggregate)?;
        self.join.advance_leaf_sources(factory, aggregate, prepay)?;
        // Parent consumption and leaf/connection census must be supplied by
        // the real closed State/Store graph. Cached transfer is not retirement.
        let state = self
            .join
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original State receiver busy or poisoned"))?;
        anyhow::ensure!(
            state.census_closed
                && state.parent.is_none()
                && state.live_parent.is_none()
                && !state.live_parent_outside_pending
                && state.leaves.iter().all(|leaf| leaf.source.is_none()),
            "actual State parent/leaf retirement remains incomplete"
        );
        Ok(())
    }
    unsafe fn register_shared_roots(
        &mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.require_original_process()?;
        // SAFETY: the actual outside supervisor supplies its complete original
        // registry; the shared receiver checks source closure before transfer.
        unsafe { self.join.register_generated_roots(registry, prepay) }
    }
    fn physical_holders_retired(&self) -> anyhow::Result<bool> {
        self.require_original_process()?;
        self.join.holders_retired()
    }
    fn physical_holders_retirement_quote(&self) -> anyhow::Result<OutsideWork> {
        self.require_original_process()?;
        let steps = u64::try_from(self.join.maximum)?
            .checked_add(8)
            .context("State destination holder census quote overflow")?;
        Ok(OutsideWork { bytes: 0, steps })
    }
}

#[cfg(test)]
mod drop_tests {
    use super::*;

    const KIND: &str = "ryeos-state::destination-join";

    struct NoLease;
    impl lillux_sqlite::OriginalNativeProjectionLeaseAuthority for NoLease {
        fn require_original(
            &self,
            _factory: &Arc<OriginalNativeFactory>,
            _cleanup: bool,
        ) -> anyhow::Result<()> {
            anyhow::bail!("drop test admits no projection lease")
        }
        fn fork_deadline(
            &self,
            _factory: &Arc<OriginalNativeFactory>,
            _cleanup: bool,
        ) -> anyhow::Result<lillux::time::MonotonicDeadline> {
            anyhow::bail!("drop test admits no projection lease")
        }
    }

    fn join(
        parent: Option<OriginalProjectionStateOwner>,
        census_closed: bool,
        live_parent_outside_pending: bool,
    ) -> OriginalStateDestinationJoin {
        OriginalStateDestinationJoin {
            process: std::process::id(),
            factory: std::sync::Weak::new(),
            maximum: 1,
            state: Mutex::new(Held::new(
                JOIN_STATE_KIND,
                JoinState {
                    parent,
                    leaves: Vec::with_capacity(1),
                    census_closed,
                    live_parent: None,
                    live_parent_outside_pending,
                    generated_root_registry: None,
                    generated_roots_retired: false,
                },
                join_state_owns_nothing,
            )),
        }
    }

    #[test]
    fn transferred_join_drops_without_abandoning() {
        // The shape a closed census leaves after its live parent body moved
        // to the outside frame: no parent, no live parent, no leaf alias.
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(join(None, true, true));
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn join_with_untransferred_parent_is_abandoned_whole() {
        let lease: std::sync::Weak<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority> =
            std::sync::Weak::<NoLease>::new();
        let owner = Arc::new(OriginalPreparedProjectionAuthority {
            outside_join: Mutex::new(std::sync::Weak::new()),
            preparation: Mutex::new(None),
            factory: std::sync::Weak::new(),
            lease_authority: lease,
        });
        let observer = Arc::downgrade(&owner);
        let published = join(
            Some(OriginalProjectionStateOwner::Offline(owner)),
            false,
            false,
        );
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(published);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
        assert!(observer.upgrade().is_some());
    }
}

#[cfg(test)]
mod quote_tests {
    use super::{
        OriginalStateDestinationJoin, RetainedLeaf, StateLeafContinuation,
        maximum_state_destination_leaves, quote_generated_root_registration,
        quote_live_state_parent_transfer, quote_retired_generated_root_receipts,
        quote_state_destination_join, quote_state_leaf_admission, quote_state_leaf_continuations,
        quote_state_source_coalescing,
    };
    use lillux_sqlite::original_physical_custody::OutsideWork;
    use std::mem::size_of;

    #[test]
    fn state_owner_quotes_match_capacity_and_per_leaf_charges() {
        assert_eq!(maximum_state_destination_leaves(64).unwrap(), 64);
        assert!(maximum_state_destination_leaves(0).is_err());
        let join = quote_state_destination_join(2).unwrap();
        assert_eq!(
            join.bytes,
            (2 * size_of::<RetainedLeaf>()
                + size_of::<OriginalStateDestinationJoin>()
                + 2 * size_of::<usize>()) as u64
        );
        assert_eq!(join.steps, 2);
        assert!(quote_state_destination_join(usize::MAX).is_err());
        assert_eq!(
            quote_live_state_parent_transfer().unwrap(),
            OutsideWork {
                bytes: size_of::<super::live_native::LiveStateNativeOwner>() as u64,
                steps: 8,
            }
        );

        assert_eq!(
            quote_state_source_coalescing(3).unwrap(),
            OutsideWork {
                bytes: 0,
                steps: 13
            }
        );
        assert!(quote_state_source_coalescing(usize::MAX).is_err());
        assert_eq!(quote_state_leaf_admission(4, 3).unwrap().steps, 15);
        assert!(quote_state_leaf_admission(4, 5).is_err());
        assert_eq!(
            quote_state_leaf_continuations(2).unwrap(),
            OutsideWork {
                bytes: (2 * size_of::<StateLeafContinuation>()) as u64,
                steps: 8,
            }
        );
        assert_eq!(
            quote_generated_root_registration(6, 2).unwrap(),
            OutsideWork {
                bytes: 0,
                steps: 24
            }
        );
        assert!(quote_generated_root_registration(6, usize::MAX).is_err());
        assert_eq!(
            quote_retired_generated_root_receipts(2, 2).unwrap(),
            OutsideWork {
                bytes: 0,
                steps: 36
            }
        );
        assert!(quote_retired_generated_root_receipts(1, 2).is_err());
        assert!(quote_retired_generated_root_receipts(usize::MAX, usize::MAX).is_err());
    }
}
