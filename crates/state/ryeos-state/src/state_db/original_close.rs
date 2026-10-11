//! Consuming offline State projection retirement. No SQL survives in the
//! descriptor-only owner used for the final original-operation acknowledgement.
use super::*;
use crate::projection::{
    ClosedOriginalProjection, OriginalProjectionFailure, ReleasedOriginalProjectionAcquisition,
};
use lillux_sqlite::{
    OriginalNativeConnectionTerminal, OriginalNativeFactory, OriginalNativeTerminal,
};
use lillux_sqlite::original_physical_custody::OutsideWork;

pub fn quote_state_finalizer_frame() -> OutsideWork {
    OutsideWork { bytes: 4096, steps: 32 }
}

pub fn quote_state_root_transfer() -> OutsideWork {
    OutsideWork { bytes: 512, steps: 4 }
}

pub fn quote_state_outside_handoff() -> OutsideWork {
    OutsideWork { bytes: 8192, steps: 64 }
}

pub fn quote_state_outside_continuation(
    generation_leases: usize,
    sql_sources: usize,
) -> anyhow::Result<OutsideWork> {
    let lease_count = u64::try_from(generation_leases)?;
    let sql_count = u64::try_from(sql_sources)?;
    let bytes = lease_count.checked_mul(512)
        .and_then(|leases| sql_count.checked_mul(2048).and_then(|sql| leases.checked_add(sql)))
        .context("State outside continuation storage quote overflow")?;
    let steps = lease_count.checked_add(sql_count)
        .and_then(|sources| sources.checked_mul(8))
        .context("State outside continuation work quote overflow")?;
    Ok(OutsideWork { bytes, steps })
}

pub fn quote_state_physical_transfer(sources: usize) -> anyhow::Result<OutsideWork> {
    let count = u64::try_from(sources)
        .context("State physical transfer count exceeds u64")?;
    let bytes = count.checked_mul(512)
        .and_then(|bytes| bytes.checked_add(8192))
        .context("State physical transfer storage quote overflow")?;
    let steps = count.checked_mul(8)
        .and_then(|work| work.checked_add(64))
        .context("State physical transfer work quote overflow")?;
    Ok(OutsideWork { bytes, steps })
}

struct RetiredStateContext {
    factory: Arc<OriginalNativeFactory>,
    runtime: lillux::PinnedDirectory,
    namespace: lillux::PinnedDirectoryLock,
    cas: lillux::PinnedDirectory,
    refs: lillux::PinnedDirectory,
    locators: lillux::PinnedDirectory,
    recovery_directory: lillux::PinnedDirectory,
    recovery: RecoveryStore,
    operation: Option<crate::recovery::ThreadHistoryDiscardOperationRef>,
    previous: Vec<Arc<ClosedOriginalProjection>>,
    generated: OriginalProjectionBuildContext,
    trust: Arc<TrustStore>,
    head_cache: Mutex<HeadCache>,
    repair_sink: Arc<dyn ProjectionRepairSink>,
}
impl RetiredStateContext {
    fn require_guard(&self, guard: &crate::recovery::CasMutationGuard) -> anyhow::Result<()> {
        anyhow::ensure!(
            guard.is_exclusive(),
            "retired State requires original exclusive CAS guard"
        );
        guard.ensure_protects_pinned_runtime(&self.runtime)?;
        self.namespace.ensure_protects(&self.runtime)?;
        for root in [
            &self.runtime,
            &self.cas,
            &self.refs,
            &self.locators,
            &self.recovery_directory,
        ] {
            root.ensure_path_binding()?;
        }
        self.recovery.ensure_guard(guard)
    }
    fn terminal_snapshot(&self) -> anyhow::Result<OriginalStateTerminalSnapshot> {
        // Snapshot only Rust retention here. All native and filesystem
        // provenance observations occur after the enclosing gate is released.
        let references = self
            .generated
            .schema_references
            .try_lock()
            .map_err(|_| anyhow::anyhow!("actual projection schema receipts busy"))?;
        Ok(OriginalStateTerminalSnapshot {
            previous: self
                .previous
                .iter()
                .chain(self.generated.retired_selected.iter())
                .cloned()
                .collect(),
            durable: self.generated.durable.clone(),
            terminals: self.generated.terminals.clone(),
            references: references.clone(),
        })
    }
    fn previous_terminals(&self) -> anyhow::Result<Vec<OriginalNativeConnectionTerminal>> {
        self.terminal_snapshot()?.validate(&self.factory)
    }
}

struct OriginalStateTerminalSnapshot {
    previous: Vec<Arc<ClosedOriginalProjection>>,
    durable: Vec<Arc<lillux_sqlite::DurableOriginalNativeBound>>,
    terminals: Vec<Arc<lillux_sqlite::ClosedOriginalNativeConnection>>,
    references: Vec<Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>>,
}
impl OriginalStateTerminalSnapshot {
    fn validate(
        self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> anyhow::Result<Vec<OriginalNativeConnectionTerminal>> {
        factory.require_original_process()?;
        let mut terminals = Vec::new();
        for owner in self.previous {
            owner.require_original(factory)?;
            push_outside_terminal(
                &mut terminals,
                OriginalNativeConnectionTerminal::Closed(owner.native_proof()),
            );
        }
        for proof in self.durable {
            proof.require_original(factory)?;
        }
        for proof in self.terminals {
            proof.require_original(factory)?;
            push_outside_terminal(
                &mut terminals,
                OriginalNativeConnectionTerminal::Closed(proof),
            );
        }
        for reference in self.references {
            reference.require_original_closed(
                factory,
                lillux_sqlite::OriginalNativeDomain::NodeProjectionSchemaReference,
            )?;
            push_outside_terminal(&mut terminals, reference.terminal().clone());
        }
        Ok(terminals)
    }
}

/// Memory and steps of one fresh State ACK receipt and its verification.
const STATE_ACK_RECEIPT_MEMORY_BYTES: u64 = 256;
const STATE_ACK_RECEIPT_STEPS: u64 = 1;

/// (read bytes, memory bytes, cleanup steps) one State ACK attempt that
/// rewrites the discard marker charges before any effect: the marker
/// rewrite's bounded work plus the fresh receipt.
pub const STATE_ACK_EFFECT_PREPAY: (u64, u64, u64) = {
    let quote = crate::recovery::thread_history_discard_acknowledgement_quote();
    (
        quote.read_bytes,
        quote.memory_bytes + STATE_ACK_RECEIPT_MEMORY_BYTES,
        quote.steps + STATE_ACK_RECEIPT_STEPS,
    )
};

/// (read bytes, memory bytes, cleanup steps) a replay of a cached State ACK
/// charges: the fresh receipt only, with no marker read or rewrite.
pub const STATE_ACK_REPLAY_PREPAY: (u64, u64, u64) =
    (0, STATE_ACK_RECEIPT_MEMORY_BYTES, STATE_ACK_RECEIPT_STEPS);

/// Keeps every actual State root, factory and generation custody after SQL close.
/// Native close is not filesystem disposal, aggregate settlement or success.
pub struct ClosedOriginalState {
    factory: Arc<OriginalNativeFactory>,
    context: Mutex<StateContextCustody>,
    selected: ClosedOriginalProjection,
    acknowledgement: std::sync::OnceLock<Arc<OriginalStateAcknowledgementBody>>,
}
enum StateContextCustody {
    Retained(RetiredStateContext),
    Finalizer(Arc<OriginalStateFinalizer>),
}
impl ClosedOriginalState {
    pub fn native_terminals(&self) -> anyhow::Result<Vec<OriginalNativeConnectionTerminal>> {
        self.factory.require_original_process()?;
        self.selected.require_original(&self.factory)?;
        let (snapshot, cached) = {
            let custody = self
                .context
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State context custody busy or poisoned"))?;
            match &*custody {
                StateContextCustody::Retained(context) => {
                    (Some(context.terminal_snapshot()?), None)
                }
                StateContextCustody::Finalizer(finalizer) => {
                    (None, Some(finalizer.terminals.clone()))
                }
            }
        };
        let mut terminals = match snapshot {
            Some(snapshot) => snapshot.validate(&self.factory)?,
            None => cached.expect("actual cached finalizer terminal snapshot"),
        };
        let selected = self.selected.native_proof();
        if !terminals.iter().any(|terminal| {
            matches!(terminal,
            OriginalNativeConnectionTerminal::Closed(proof) if Arc::ptr_eq(proof, &selected))
        }) {
            terminals.push(OriginalNativeConnectionTerminal::Closed(selected));
        }
        Ok(terminals)
    }
    /// Close actual generation locks after all SQL owners have supplied their
    /// genuine terminals and the original exclusive CAS frame has quiesced
    /// generation consumers. Refusals stay in the retained registry for retry.
    /// # Safety
    /// The original supervisor must have quiesced every generation consumer,
    /// including escaped projection references; the CAS guard alone does not
    /// prove that. Retain this owner and each refusal until genuine settlement.
    pub unsafe fn close_generation_leases_after_consumers(
        &self,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>> {
        self.factory.require_original_process()?;
        {
            let custody = self
                .context
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State generation guard custody busy or poisoned"))?;
            let StateContextCustody::Retained(context) = &*custody else {
                anyhow::bail!("generation closure requires retained original State context");
            };
            context.require_guard(guard)?;
        }
        self.native_terminals()?;
        let leases = {
            let custody = self
                .context
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State generation custody busy or poisoned"))?;
            let StateContextCustody::Retained(context) = &*custody else {
                anyhow::bail!("generation closure requires retained original State context");
            };
            let leases = context
                .generated
                .generation_leases
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State generation registry busy or poisoned"))?;
            leases.clone()
        };
        let mut receipts = Vec::new();
        for lease in leases {
            // SAFETY: consuming State retirement ended its actual SQL users;
            // the original exclusive CAS frame excludes new generation work.
            if let Some(receipt) = unsafe { lease.close_after_consumers(&self.factory) }? {
                if !receipts.iter().any(|actual| Arc::ptr_eq(actual, &receipt)) {
                    receipts.push(receipt);
                }
            }
        }
        Ok(receipts)
    }
    /// Mint a receipt only after this actual recovery owner completes marker
    /// acknowledgement and directory sync. Native release alone is insufficient.
    pub fn finish_thread_history_discard_admitted(
        self: &Arc<Self>,
        authority: &OriginalStateFinalAckAuthority<'_>,
    ) -> anyhow::Result<Arc<OriginalStateAcknowledgement>> {
        authority.require(self)?;
        if let Some(body) = self.acknowledgement.get() {
            // A cached effect needs no marker read or rewrite. Prepay only this
            // fresh bounded receipt and its verification under original custody.
            let (read_bytes, memory_bytes, steps) = STATE_ACK_REPLAY_PREPAY;
            (authority.prepay)(read_bytes, memory_bytes, steps)?;
            let receipt = Arc::new(OriginalStateAcknowledgement {
                state: self.clone(),
                body: body.clone(),
            });
            receipt.require_original(
                self,
                &self.factory,
                authority.terminal,
                authority.operation,
            )?;
            return Ok(receipt);
        }
        // Charge one complete ACK attempt BEFORE any ACK effect: the marker
        // rewrite's bounded quote (513 read bytes: 512 admitted plus the
        // growth probe; memory for the read buffer, decoded marker, encoded
        // Acknowledged bytes and temporary-name scratch; pinned intake,
        // conditional atomic write and directory sync steps) plus the fresh
        // receipt and its verification, which the cached path above charges
        // as 256 bytes and one step. Each uncertain attempt consumes the
        // same original counters.
        let (read_bytes, memory_bytes, steps) = STATE_ACK_EFFECT_PREPAY;
        (authority.prepay)(read_bytes, memory_bytes, steps)?;
        authority.require(self)?;
        let recovery = {
            let custody = self
                .context
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State ACK custody busy or poisoned"))?;
            let StateContextCustody::Retained(context) = &*custody else {
                anyhow::bail!("State ACK cannot reopen a transferred finalizer context");
            };
            context.recovery.final_ack_custody()
        };
        // No State bookkeeping guard spans marker read, rewrite or sync.
        recovery.finish(authority.guard, authority.operation)?;
        drop(recovery);

        let body = Arc::new(OriginalStateAcknowledgementBody {
            terminal: authority.terminal.clone(),
            operation: authority.operation.clone(),
        });
        // Retain actual completed effects before any fallible late checks. The
        // body has no back-edge to State; the receipt retains its actual owner.
        let _ = self.acknowledgement.set(body);
        let body = self
            .acknowledgement
            .get()
            .expect("actual State acknowledgement retained")
            .clone();
        let receipt = Arc::new(OriginalStateAcknowledgement {
            state: self.clone(),
            body,
        });
        receipt.require_original(self, &self.factory, authority.terminal, authority.operation)?;
        Ok(receipt)
    }
}

impl ClosedOriginalState {
    fn require_retained_guard(
        &self,
        guard: &crate::recovery::CasMutationGuard,
        operation: Option<&crate::recovery::ThreadHistoryDiscardOperationRef>,
    ) -> anyhow::Result<()> {
        self.factory.require_original_process()?;
        let custody = self
            .context
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State context custody busy or poisoned"))?;
        let StateContextCustody::Retained(context) = &*custody else {
            anyhow::bail!("State context has transferred to its actual finalizer");
        };
        context.require_guard(guard)?;
        anyhow::ensure!(
            context.operation.as_ref() == operation,
            "State context operation differs from the actual captured marker"
        );
        Ok(())
    }
    /// Pure provenance check: this actual ACKed State cached THIS producer.
    /// No filesystem, clock or native effect runs under the bookkeeping guard.
    pub fn require_original_finalizer(
        &self,
        finalizer: &Arc<OriginalStateFinalizer>,
    ) -> anyhow::Result<()> {
        self.factory.require_original_process()?;
        let custody = self
            .context
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State finalizer provenance busy or poisoned"))?;
        anyhow::ensure!(
            matches!(&*custody, StateContextCustody::Finalizer(actual) if Arc::ptr_eq(actual, finalizer))
                && self.factory.same_original(&finalizer.factory)
                && self
                    .acknowledgement
                    .get()
                    .is_some_and(|body| Arc::ptr_eq(body, &finalizer.acknowledgement)),
            "State finalizer differs from this actual acknowledged owner"
        );
        Ok(())
    }
    /// # Safety
    /// `prepay` must charge this producer's finite storage and handoff work on
    /// the same genuine post-ACK claim, charged reservation and original clock.
    /// The caller retains actual CAS/namespace custody through this handoff.
    /// This transfers real owners; it does not close or discharge any resource.
    pub unsafe fn begin_finalizer_retirement(
        self: &Arc<Self>,
        acknowledgement: &Arc<OriginalStateAcknowledgement>,
        terminal: &Arc<OriginalNativeTerminal>,
        operation: &crate::recovery::ThreadHistoryDiscardOperationRef,
        guard: &crate::recovery::CasMutationGuard,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<Arc<OriginalStateFinalizer>> {
        self.factory.require_original_process()?;
        acknowledgement.require_original(self, &self.factory, terminal, operation)?;
        {
            let custody = self
                .context
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State finalizer custody busy or poisoned"))?;
            if let StateContextCustody::Finalizer(actual) = &*custody {
                return Ok(actual.clone());
            }
        }
        let quote = quote_state_finalizer_frame();
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        self.require_retained_guard(guard, Some(operation))?;
        let terminals = self.native_terminals()?;
        let body = self
            .acknowledgement
            .get()
            .context("actual State ACK body absent")?
            .clone();
        let finalizer = Arc::new(OriginalStateFinalizer {
            factory: self.factory.clone(),
            acknowledgement: body,
            terminals,
            roots: Mutex::new(StateFinalizerRoots {
                directories: std::array::from_fn(|_| None),
                owners: std::array::from_fn(|_| None),
                receipts: std::array::from_fn(|_| None),
                remaining: None,
            }),
        });
        let mut roots = finalizer
            .roots
            .try_lock()
            .map_err(|_| anyhow::anyhow!("new State finalizer root custody busy or poisoned"))?;
        let mut custody = self
            .context
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State finalizer registration busy or poisoned"))?;
        // A racing same-owner call can only have registered the authentic
        // producer. Keep that owner rather than replacing or duplicating it.
        if let StateContextCustody::Finalizer(actual) = &*custody {
            return Ok(actual.clone());
        }
        let prior = std::mem::replace(
            &mut *custody,
            StateContextCustody::Finalizer(finalizer.clone()),
        );
        let StateContextCustody::Retained(context) = prior else {
            unreachable!()
        };
        let RetiredStateContext {
            runtime,
            cas,
            refs,
            locators,
            recovery_directory,
            namespace,
            recovery,
            previous,
            generated,
            trust,
            head_cache,
            repair_sink,
            ..
        } = context;
        for (slot, directory) in roots.directories.iter_mut().take(5).zip([
            runtime,
            cas,
            refs,
            locators,
            recovery_directory,
        ]) {
            *slot = Some(directory);
        }
        roots.remaining = Some(StateFinalizerRemaining {
            namespace,
            recovery,
            previous,
            generated,
            trust,
            head_cache,
            repair_sink,
            preparation: None,
        });
        // All actual owners are in the cached producer before releasing any
        // bookkeeping guard. No descriptor/native/namespace effect occurred.
        drop(custody);
        drop(roots);
        self.require_original_finalizer(&finalizer)?;
        Ok(finalizer)
    }
}

/// Physical ownership of the actual retired State roots. ACK and native
/// terminals remain evidence; every other holder stays pending until closed.
pub struct OriginalStateFinalizer {
    factory: Arc<OriginalNativeFactory>,
    acknowledgement: Arc<OriginalStateAcknowledgementBody>,
    terminals: Vec<OriginalNativeConnectionTerminal>,
    roots: Mutex<StateFinalizerRoots>,
}
struct StateFinalizerRoots {
    directories: [Option<lillux::PinnedDirectory>; 10],
    owners: [Option<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>>; 10],
    receipts: [Option<Arc<lillux_sqlite::RetiredOriginalFinalizerFile>>; 10],
    remaining: Option<StateFinalizerRemaining>,
}
// These actual owners are deliberately retained; they are NOT disposed by
// handing off the five directories or by keeping their semantic proofs.
struct StateFinalizerRemaining {
    namespace: lillux::PinnedDirectoryLock,
    recovery: RecoveryStore,
    previous: Vec<Arc<ClosedOriginalProjection>>,
    generated: OriginalProjectionBuildContext,
    trust: Arc<TrustStore>,
    head_cache: Mutex<HeadCache>,
    repair_sink: Arc<dyn ProjectionRepairSink>,
    preparation: Option<StatePreparationRemaining>,
}
struct StatePreparationRemaining {
    namespace: Option<lillux::PinnedDirectoryLock>,
    recovery: RecoveryStore,
    trust: Arc<TrustStore>,
    cas_root: PathBuf,
    refs_root: PathBuf,
    locators_root: PathBuf,
}
impl OriginalStateFinalizer {
    /// # Safety
    /// The actual ACKed State must retain this producer. `prepay` must charge
    /// the same original post-ACK clock/envelope before ownership transition.
    /// Old native callbacks refuse once the genuine preparation has moved;
    /// transferring its five actual directories does not retire its recovery,
    /// namespace, trust or other holders and conveys no new admission.
    pub unsafe fn transfer_preparation(
        &self,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.factory.require_original_process()?;
        self.acknowledgement
            .terminal
            .require_original(&self.factory)?;
        {
            let roots = self.roots.try_lock().map_err(|_| {
                anyhow::anyhow!("State preparation finalizer custody busy or poisoned")
            })?;
            if roots
                .remaining
                .as_ref()
                .context("State finalizer remaining owners absent")?
                .preparation
                .is_some()
            {
                return Ok(());
            }
        }
        let quote = quote_state_finalizer_frame();
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        let mut roots = self
            .roots
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State preparation finalizer custody busy or poisoned"))?;
        let remaining = roots
            .remaining
            .as_ref()
            .context("State finalizer remaining owners absent")?;
        if remaining.preparation.is_some() {
            return Ok(());
        }
        let OriginalProjectionStateOwner::Offline(authority) = &remaining.generated.authority
        else {
            anyhow::bail!("offline preparation finalizer cannot consume live State authority");
        };
        let original = authority
            .factory
            .upgrade()
            .context("original State preparation factory absent")?;
        anyhow::ensure!(
            original.same_original(&self.factory),
            "State preparation selected foreign factory"
        );
        let mut preparation = authority
            .preparation
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State preparation handoff busy or poisoned"))?;
        anyhow::ensure!(
            roots.directories[5..].iter().all(Option::is_none)
                && roots.owners[5..].iter().all(Option::is_none),
            "State preparation destination already owns unrelated roots"
        );
        // Both real ownership guards are acquired before the one-use take.
        // The remaining move is infallible and runs no filesystem/native work.
        let actual = preparation
            .take()
            .context("actual State preparation absent without handoff")?;
        // The emptied handoff guard borrows through `roots`; release it before
        // the infallible root moves below.
        drop(preparation);
        let PreparedOfflineProjection {
            runtime_state_directory,
            cas_directory,
            refs_directory,
            locators_directory,
            recovery_directory,
            namespace_lock,
            cas_root,
            refs_root,
            locators_root,
            recovery,
            trust_store,
        } = actual;
        for (slot, directory) in roots.directories.iter_mut().skip(5).zip([
            runtime_state_directory,
            cas_directory,
            refs_directory,
            locators_directory,
            recovery_directory,
        ]) {
            *slot = Some(directory);
        }
        roots
            .remaining
            .as_mut()
            .expect("checked remaining owners")
            .preparation = Some(StatePreparationRemaining {
            namespace: namespace_lock,
            recovery,
            trust: trust_store,
            cas_root,
            refs_root,
            locators_root,
        });
        Ok(())
    }
    pub fn require_original_file(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        owner: &Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.factory.same_original(factory),
            "State finalizer factory changed"
        );
        self.acknowledgement.terminal.require_original(factory)?;
        for terminal in &self.terminals {
            terminal.require_original(factory)?;
        }
        let roots = self
            .roots
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State finalizer membership busy or poisoned"))?;
        anyhow::ensure!(
            roots.remaining.is_some()
                && roots
                    .owners
                    .iter()
                    .flatten()
                    .any(|actual| Arc::ptr_eq(actual, owner)),
            "State finalizer selected a foreign actual descriptor"
        );
        Ok(())
    }
    /// # Safety
    /// The caller must prepay each original bounded owner/receipt allocation
    /// before this handoff and retain this actual cached State producer. No
    /// close, unlink or funded authority is conveyed by the transfer itself.
    pub unsafe fn transfer_root(
        &self,
        index: usize,
    ) -> anyhow::Result<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>> {
        self.factory.require_original_process()?;
        anyhow::ensure!(index < 10, "State finalizer root slot out of range");
        let mut roots = self
            .roots
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State finalizer root transfer busy or poisoned"))?;
        if let Some(owner) = &roots.owners[index] {
            return Ok(owner.clone());
        }
        let directory = roots.directories[index]
            .take()
            .context("State directory transferred without actual owner")?;
        // SAFETY: this private slot contains the same original directory moved
        // from actual retired State context; no duplication or reopening.
        let owner = unsafe {
            lillux_sqlite::RetainedOriginalFinalizerFile::issued(
                directory.into_owned_close_descriptor(),
                lillux_sqlite::OriginalFinalizerFileKind::Directory,
            )
        };
        roots.owners[index] = Some(owner.clone());
        Ok(owner)
    }
    pub fn retain_root_retirement(
        &self,
        index: usize,
        receipt: Arc<lillux_sqlite::RetiredOriginalFinalizerFile>,
    ) -> anyhow::Result<()> {
        self.factory.require_original_process()?;
        anyhow::ensure!(index < 10, "State finalizer receipt slot out of range");
        let owner = {
            let roots = self
                .roots
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State finalizer receipt custody busy or poisoned"))?;
            roots.owners[index]
                .clone()
                .context("State root owner absent")?
        };
        receipt.require_original(&owner)?;
        let mut roots = self.roots.try_lock().map_err(|_| {
            anyhow::anyhow!("State finalizer receipt registration busy or poisoned")
        })?;
        roots.receipts[index] = Some(receipt);
        Ok(())
    }
}

/// The actual post-ACK State continuation outside the native resident graph.
/// It keeps descriptor, namespace and recovery obligations; it is not a
/// physical-disposal or durable-settlement receipt.
pub struct OriginalOutsideStateFinalizer {
    process: u32,
    operation: crate::recovery::ThreadHistoryDiscardOperationRef,
    roots: OutsideStateRoots,
    native: Option<OutsideStateNativeReferences>,
    destination_join: Option<Arc<super::destination_join::OriginalStateDestinationJoin>>,
    projection_files: Vec<crate::projection::OutsideOriginalProjectionCustody>,
    generation_leases: Vec<Arc<crate::projection::OriginalGenerationLease>>,
    durability_pins: Vec<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>,
    native_terminals: Vec<OriginalNativeConnectionTerminal>,
    native_transfer_failure: Option<lillux_sqlite::OriginalNativeOutsideCustodyFailure>,
    sql_custody: Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
}
struct OutsideStateRoots {
    owners: [Option<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>>; 10],
    receipts: [Option<Arc<lillux_sqlite::RetiredOriginalFinalizerFile>>; 10],
    namespace: lillux::PinnedDirectoryLock,
    recovery: RecoveryStore,
    trust: Arc<TrustStore>,
    head_cache: Mutex<HeadCache>,
    repair_sink: Arc<dyn ProjectionRepairSink>,
    preparation: StatePreparationRemaining,
}
/// Actual State physical obligations after the consuming native handoff.
/// There is no public constructor and no implicit disposal on token creation.
pub struct OriginalStatePhysicalCustody {
    process: u32,
    operation: crate::recovery::ThreadHistoryDiscardOperationRef,
    namespaces: Vec<lillux::PinnedDirectoryLock>,
    shared_registrations:
        Vec<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
    recovery_roots: Vec<Arc<lillux::PinnedDirectory>>,
    projection_files: Vec<crate::projection::OutsideOriginalProjectionCustody>,
    generation_leases: Vec<Arc<crate::projection::OriginalGenerationLease>>,
    directories: Vec<lillux::PinnedDirectory>,
    descriptor_owners: Vec<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>>,
    descriptor_receipts: Vec<Option<Arc<lillux_sqlite::RetiredOriginalFinalizerFile>>>,
    durability_pins: Vec<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>,
    durability_receipts: Vec<Option<Arc<lillux_sqlite::RetiredOriginalMetadataPin>>>,
    _semantics: OutsideStateSemantics,
}
struct OutsideStateSemantics {
    _destination_join: Option<Arc<super::destination_join::OriginalStateDestinationJoin>>,
    _trust: Arc<TrustStore>,
    _head_cache: Mutex<HeadCache>,
    _repair_sink: Arc<dyn ProjectionRepairSink>,
    _preparation_trust: Arc<TrustStore>,
    _cas_root: PathBuf,
    _refs_root: PathBuf,
    _locators_root: PathBuf,
}
impl OriginalStatePhysicalCustody {
    /// # Safety
    /// The genuine original supervisor has consumed every actual native SQL
    /// packet and its destination/inspection authorities, closed their complete
    /// source census and excluded further creation. Preserve this same token
    /// and original funding/clock on refusal. Counts alone cannot close census.
    pub unsafe fn advance_destination_join(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign State physical parent retirement");
        let join = self._semantics._destination_join.as_ref()
            .context("actual State physical token has no original destination receiver")?;
        // The private consuming State handoff already moved its authentic
        // original preparation and all roots into this SAME physical token.
        unsafe { join.advance_offline_parent(factory, terminal, prepay) }
    }
    pub fn operation(&self) -> &crate::recovery::ThreadHistoryDiscardOperationRef {
        &self.operation
    }
    pub fn descriptor_owners(&self) -> &[Arc<lillux_sqlite::RetainedOriginalFinalizerFile>] {
        &self.descriptor_owners
    }
    pub fn durability_pins(&self) -> &[Arc<lillux_sqlite::RetainedOriginalMetadataPin>] {
        &self.durability_pins
    }
    /// # Safety
    /// This is the SAME finite original outside inventory used by Runtime,
    /// Operational and lower packets. Register every alias before inventory
    /// seal. Cache these opaque memberships; receipt observation stays bound
    /// to the actual canonical descriptor, not an inode or a numeric grant.
    pub unsafe fn register_shared_roots(
        &mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.process == std::process::id(),
            "foreign State physical inventory"
        );
        while !self.projection_files.is_empty() {
            let quote = quote_state_root_transfer();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.directories.try_reserve(1)?;
            self.generation_leases.try_reserve(1)?;
            let actual = self.projection_files.pop().unwrap();
            match actual.try_into_original_filesystem() {
                Ok(Some((directory, lease))) => {
                    self.directories.push(directory);
                    if let Some(lease) = lease {
                        if !self
                            .generation_leases
                            .iter()
                            .any(|owner| Arc::ptr_eq(owner, &lease))
                        {
                            self.generation_leases.push(lease);
                        }
                    }
                }
                Ok(None) => {}
                Err(actual) => {
                    self.projection_files.push(actual);
                    anyhow::bail!("non-original State projection filesystem");
                }
            }
        }
        while !self.generation_leases.is_empty() {
            let quote = quote_state_root_transfer();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.directories.try_reserve(1)?;
            let actual = self.generation_leases.pop().unwrap();
            match actual.try_into_metadata_root() {
                Ok(directory) => self.directories.push(directory),
                Err(actual) => {
                    self.generation_leases.push(actual);
                    anyhow::bail!("original generation metadata still shared or active");
                }
            }
        }
        while !self.directories.is_empty() {
            let quote = quote_state_root_transfer();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.recovery_roots.try_reserve(1)?;
            // This wraps the actual owned descriptor; no new host FD is created.
            self.recovery_roots
                .push(Arc::new(self.directories.pop().unwrap()));
        }
        while !self.recovery_roots.is_empty() {
            let quote = quote_state_root_transfer();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.shared_registrations.try_reserve(1)?;
            let actual = self.recovery_roots.pop().unwrap();
            match unsafe { registry.register_directory(actual, prepay) } {
                Ok(binding) => self.shared_registrations.push(binding),
                Err((actual, error)) => {
                    self.recovery_roots.push(actual);
                    return Err(error);
                }
            }
        }
        while !self.namespaces.is_empty() {
            let quote = quote_state_root_transfer();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            self.shared_registrations.try_reserve(1)?;
            let actual = self.namespaces.pop().unwrap();
            match unsafe { registry.register_namespace(actual, prepay) } {
                Ok(binding) => self.shared_registrations.push(binding),
                Err((actual, error)) => {
                    self.namespaces.push(actual);
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    /// Borrow the complete actual State token and verify all transferred root
    /// roles in the original registry. No close or settlement is implied.
    pub fn require_shared_roots_registered(
        &self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign State original registration census");
        anyhow::ensure!(self.namespaces.is_empty() && self.recovery_roots.is_empty()
            && self.projection_files.is_empty() && self.generation_leases.is_empty()
            && self.directories.is_empty(), "State actual root sources remain unregistered");
        // Every actual source leaves these private collections only after its
        // genuine registry binding is cached. Check those bindings, not merely
        // the empty collections, including the always-retained State namespace.
        let quote = registry.membership_work_bound(
            u64::try_from(self.shared_registrations.len())?, 0,
        )?;
        let work = quote.steps.checked_add(8)
            .context("State root census work overflow")?;
        registry.prepay_transfer(prepay, quote.bytes, work)?;
        let mut namespace_seen = false;
        for binding in &self.shared_registrations {
            registry.require_membership(binding)?;
            namespace_seen |= matches!(binding.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. });
        }
        anyhow::ensure!(namespace_seen, "State original namespace membership absent");
        if let Some(join) = &self._semantics._destination_join {
            join.require_generated_roots_registered_with_prepay(registry, prepay)?;
        }
        Ok(())
    }
    pub fn retain_descriptor_retirement(
        &mut self,
        index: usize,
        receipt: Arc<lillux_sqlite::RetiredOriginalFinalizerFile>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.process == std::process::id(),
            "foreign State physical custody"
        );
        let owner = self
            .descriptor_owners
            .get(index)
            .context("State physical owner index absent")?;
        receipt.require_original(owner)?;
        self.descriptor_receipts[index] = Some(receipt);
        Ok(())
    }
    pub fn retain_metadata_retirement(
        &mut self,
        index: usize,
        receipt: Arc<lillux_sqlite::RetiredOriginalMetadataPin>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.process == std::process::id(),
            "foreign State metadata custody"
        );
        let owner = self
            .durability_pins
            .get(index)
            .context("State original metadata slot absent")?;
        receipt.require_original(owner)?;
        self.durability_receipts[index] = Some(receipt);
        Ok(())
    }
    /// Actual descriptor and metadata receipts only. Original backing release,
    /// semantic recovery and durable account settlement are separate obligations.
    pub fn physical_holders_retired(&self) -> bool {
        self.descriptors_retired() && self.durability_receipts.iter().all(Option::is_some)
            && self._semantics._destination_join.as_ref().is_some_and(|join| join.holders_retired().unwrap_or(false))
    }
    /// Descriptor-only observation; metadata-pin and durable settlement
    /// obligations remain distinct and must be joined by the outside controller.
    pub fn descriptors_retired(&self) -> bool {
        self.process == std::process::id()
            && self.namespaces.is_empty()
            && self.recovery_roots.is_empty()
            && self.projection_files.is_empty()
            && self.generation_leases.is_empty()
            && self.directories.is_empty()
            && self
                .shared_registrations
                .iter()
                .all(|binding| binding.retired())
            && self.descriptor_receipts.iter().all(Option::is_some)
    }
}
struct OutsideStateNativeReferences {
    selected: Option<ClosedOriginalProjection>,
    previous: Vec<Arc<ClosedOriginalProjection>>,
    generated: OriginalProjectionBuildContext,
}
/// An owned refusal preserves all authentic handoff inputs for the original
/// controller. No new ACK, factory, reservation or clock is issued on retry.
pub struct OriginalStateOutsideRetirementFailure {
    error: anyhow::Error,
    state: Arc<ClosedOriginalState>,
    acknowledgement: Arc<OriginalStateAcknowledgement>,
    finalizer: Arc<OriginalStateFinalizer>,
}
impl OriginalStateOutsideRetirementFailure {
    pub fn original_error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn into_original_inputs(
        self,
    ) -> (
        Arc<ClosedOriginalState>,
        Arc<OriginalStateAcknowledgement>,
        Arc<OriginalStateFinalizer>,
    ) {
        (self.state, self.acknowledgement, self.finalizer)
    }
}
impl std::fmt::Debug for OriginalStateOutsideRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalStateOutsideRetirementFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalStateOutsideRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for OriginalStateOutsideRetirementFailure {}

impl ClosedOriginalState {
    /// # Safety
    /// The original controller must have quiesced all consumers, retained the
    /// same genuine post-ACK funding/clock and actual CAS/namespace authority,
    /// and prepaid this finite continuation before handoff. Remove its other
    /// State/ACK/finalizer aliases first, and cache the returned outside owner
    /// before any physical release. Native retirement and unresolved physical
    /// obligations remain explicit; this method grants no new authority.
    pub unsafe fn into_outside_finalizer(
        self: Arc<Self>,
        acknowledgement: Arc<OriginalStateAcknowledgement>,
        terminal: &Arc<OriginalNativeTerminal>,
        operation: &crate::recovery::ThreadHistoryDiscardOperationRef,
        guard: &crate::recovery::CasMutationGuard,
        finalizer: Arc<OriginalStateFinalizer>,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> std::result::Result<OriginalOutsideStateFinalizer, OriginalStateOutsideRetirementFailure>
    {
        let checked = (|| -> anyhow::Result<(Vec<_>, Vec<_>)> {
            self.factory.require_original_process()?;
            acknowledgement.require_original(&self, &self.factory, terminal, operation)?;
            self.require_original_finalizer(&finalizer)?;
            anyhow::ensure!(
                guard.is_exclusive(),
                "State outside handoff needs original exclusive CAS frame"
            );
            // Counts establish only exclusive Rust transfer AFTER authentic
            // terminal/ACK checks. They are never native settlement evidence.
            anyhow::ensure!(
                Arc::strong_count(&self) == 2
                    && Arc::weak_count(&self) == 0
                    && Arc::strong_count(&acknowledgement) == 1
                    && Arc::weak_count(&acknowledgement) == 0
                    && Arc::strong_count(&finalizer) == 2
                    && Arc::weak_count(&finalizer) == 0,
                "State outside handoff still has other owner aliases"
            );
            // Pay fixed transfer/snapshot/refusal storage before creating any
            // continuation clones. This callback runs outside retention gates.
            let quote = quote_state_outside_handoff();
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                prepay(quote.bytes, quote.steps)
            })?;
            let roots = finalizer
                .roots
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State outside handoff roots busy or poisoned"))?;
            anyhow::ensure!(
                roots.directories.iter().all(Option::is_none),
                "State root descriptor handoff incomplete"
            );
            for index in 0..10 {
                let owner = roots.owners[index]
                    .as_ref()
                    .context("State root owner missing")?;
                roots.receipts[index]
                    .as_ref()
                    .context("State root retirement missing")?
                    .require_original(owner)?;
            }
            let remaining = roots
                .remaining
                .as_ref()
                .context("State outside remaining owners absent")?;
            let recovery = remaining.recovery.clone();
            anyhow::ensure!(
                matches!(
                    &remaining.generated.authority,
                    OriginalProjectionStateOwner::Offline(_)
                ),
                "offline State outside handoff cannot adopt live authority"
            );
            anyhow::ensure!(
                remaining.preparation.is_some(),
                "State original preparation has not transferred"
            );
            let leases = remaining
                .generated
                .generation_leases
                .try_lock()
                .map_err(|_| {
                    anyhow::anyhow!("State outside generation registry busy or poisoned")
                })?;
            let count = leases.len();
            let schema_count = remaining
                .generated
                .schema_references
                .try_lock()
                .map_err(|_| {
                    anyhow::anyhow!("State outside schema quote custody busy or poisoned")
                })?
                .len();
            let sql_count = remaining
                .previous
                .len()
                .checked_add(remaining.generated.retired_selected.len())
                .and_then(|n| n.checked_add(remaining.generated.terminals.len()))
                .and_then(|n| n.checked_add(remaining.generated.durable.len()))
                .and_then(|n| n.checked_add(schema_count))
                .and_then(|n| n.checked_add(1))
                .context("State outside SQL quote overflow")?;
            let quote = quote_state_outside_continuation(count, sql_count)?;
            // No host observation, issuer/clock/debit or native effect occurs
            // while these retention gates are held.
            drop(leases);
            drop(roots);
            recovery.ensure_guard(guard)?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                prepay(quote.bytes, quote.steps)
            })?;
            let leases = {
                let roots = finalizer.roots.try_lock().map_err(|_| {
                    anyhow::anyhow!("State outside snapshot roots busy or poisoned")
                })?;
                let remaining = roots
                    .remaining
                    .as_ref()
                    .context("State outside remaining owners absent")?;
                let snapshot = remaining
                    .generated
                    .generation_leases
                    .try_lock()
                    .map_err(|_| {
                        anyhow::anyhow!("State outside snapshot registry busy or poisoned")
                    })?
                    .clone();
                snapshot
            };
            let durable = {
                let roots = finalizer.roots.try_lock().map_err(|_| {
                    anyhow::anyhow!("State outside durability roots busy or poisoned")
                })?;
                roots
                    .remaining
                    .as_ref()
                    .context("State outside remaining owners absent")?
                    .generated
                    .durable
                    .clone()
            };
            let mut pins = Vec::new();
            for proof in durable {
                for pin in proof.metadata_pins(&self.factory)? {
                    if !pins.iter().any(|actual| Arc::ptr_eq(actual, &pin)) {
                        pins.push(pin);
                    }
                }
            }
            Ok((leases, pins))
        })();
        let (generation_leases, durability_pins) = match checked {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(OriginalStateOutsideRetirementFailure {
                    error,
                    state: self,
                    acknowledgement,
                    finalizer,
                });
            }
        };
        let acknowledgement = match Arc::try_unwrap(acknowledgement) {
            Ok(actual) => actual,
            Err(acknowledgement) => {
                return Err(OriginalStateOutsideRetirementFailure {
                    error: anyhow::anyhow!("State ACK exclusive transfer refused"),
                    state: self,
                    acknowledgement,
                    finalizer,
                });
            }
        };
        let OriginalStateAcknowledgement {
            state: acknowledged_state,
            body,
        } = acknowledgement;
        drop(acknowledged_state);
        let state = match Arc::try_unwrap(self) {
            Ok(actual) => actual,
            Err(state) => {
                let acknowledgement = Arc::new(OriginalStateAcknowledgement {
                    state: state.clone(),
                    body,
                });
                return Err(OriginalStateOutsideRetirementFailure {
                    error: anyhow::anyhow!("State exclusive transfer refused"),
                    state,
                    acknowledgement,
                    finalizer,
                });
            }
        };
        let ClosedOriginalState {
            factory,
            context,
            selected,
            acknowledgement: cached_ack,
        } = state;
        let context = context
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Preflight established this exact finalizer. Preserve a contrary
        // observation as the original closed State rather than dropping it.
        let StateContextCustody::Finalizer(cached_finalizer) = context else {
            let state = Arc::new(ClosedOriginalState {
                factory,
                context: Mutex::new(context),
                selected,
                acknowledgement: cached_ack,
            });
            let acknowledgement = Arc::new(OriginalStateAcknowledgement {
                state: state.clone(),
                body,
            });
            return Err(OriginalStateOutsideRetirementFailure {
                error: anyhow::anyhow!("State finalizer transfer changed"),
                state,
                acknowledgement,
                finalizer,
            });
        };
        drop(cached_finalizer);
        let finalizer = match Arc::try_unwrap(finalizer) {
            Ok(actual) => actual,
            Err(finalizer) => {
                let state = Arc::new(ClosedOriginalState {
                    factory,
                    context: Mutex::new(StateContextCustody::Finalizer(finalizer.clone())),
                    selected,
                    acknowledgement: cached_ack,
                });
                let acknowledgement = Arc::new(OriginalStateAcknowledgement {
                    state: state.clone(),
                    body,
                });
                return Err(OriginalStateOutsideRetirementFailure {
                    error: anyhow::anyhow!("State finalizer exclusive transfer refused"),
                    state,
                    acknowledgement,
                    finalizer,
                });
            }
        };
        let OriginalStateFinalizer {
            factory: finalizer_factory,
            acknowledgement: finalizer_ack,
            terminals,
            roots,
        } = finalizer;
        let StateFinalizerRoots {
            owners,
            receipts,
            remaining,
            ..
        } = roots
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Under exclusive ownership, preflighted slots cannot disappear.
        let StateFinalizerRemaining {
            namespace,
            recovery,
            previous,
            generated,
            trust,
            head_cache,
            repair_sink,
            preparation,
        } = remaining.expect("checked actual State remaining owners");
        let outside = OriginalOutsideStateFinalizer {
            process: std::process::id(),
            operation: operation.clone(),
            roots: OutsideStateRoots {
                owners,
                receipts,
                namespace,
                recovery,
                trust,
                head_cache,
                repair_sink,
                preparation: preparation.expect("checked actual State preparation"),
            },
            destination_join: None,
            native: Some(OutsideStateNativeReferences {
                selected: Some(selected),
                previous,
                generated,
            }),
            projection_files: Vec::new(),
            generation_leases,
            durability_pins,
            native_terminals: Vec::new(),
            native_transfer_failure: None,
            sql_custody: Vec::new(),
        };
        // Only native evidence aliases are released. Real remaining physical
        // and recovery owners are already inside the returned continuation.
        drop(terminals);
        drop(finalizer_ack);
        drop(finalizer_factory);
        drop(cached_ack);
        drop(body);
        drop(factory);
        Ok(outside)
    }
}
impl OriginalOutsideStateFinalizer {
    /// # Safety
    /// Caller has cached the actual SQL payloads in its original outside owner.
    /// Prepay this finite consuming storage transfer against that same envelope.
    /// No physical retirement or replacement grant is implied by this handoff.
    pub unsafe fn into_physical_custody(
        self,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> std::result::Result<OriginalStatePhysicalCustody, (Self, anyhow::Error)> {
        let checked = (|| -> anyhow::Result<()> {
            anyhow::ensure!(
                self.process == std::process::id(),
                "foreign State physical transfer"
            );
            anyhow::ensure!(
                self.native_receipts_transferred() && self.sql_custody.is_empty(),
                "State SQL payload transfer incomplete"
            );
            let count = self
                .projection_files
                .len()
                .checked_add(self.generation_leases.len())
                .and_then(|count| count.checked_add(self.durability_pins.len()))
                .and_then(|count| count.checked_add(24))
                .context("State physical transfer count overflow")?;
            let quote = quote_state_physical_transfer(count)?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                prepay(quote.bytes, quote.steps)
            })
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }
        let Self {
            process,
            operation,
            roots,
            destination_join,
            projection_files,
            generation_leases,
            durability_pins,
            ..
        } = self;
        let OutsideStateRoots {
            owners,
            receipts,
            namespace,
            recovery,
            trust,
            head_cache,
            repair_sink,
            preparation,
        } = roots;
        let StatePreparationRemaining {
            namespace: prepared_namespace,
            recovery: prepared_recovery,
            trust: preparation_trust,
            cas_root,
            refs_root,
            locators_root,
        } = preparation;
        let mut namespaces = vec![namespace];
        if let Some(actual) = prepared_namespace {
            if !namespaces
                .iter()
                .any(|owner| owner.same_retained_owner(&actual))
            {
                namespaces.push(actual);
            }
        }
        let mut recovery_roots = Vec::new();
        for actual in recovery
            .into_original_directory_custody()
            .into_iter()
            .chain(prepared_recovery.into_original_directory_custody())
        {
            if !recovery_roots
                .iter()
                .any(|owner| Arc::ptr_eq(owner, &actual))
            {
                recovery_roots.push(actual);
            }
        }
        let mut descriptor_owners = Vec::new();
        let mut descriptor_receipts = Vec::new();
        for (owner, receipt) in owners.into_iter().zip(receipts) {
            if let Some(owner) = owner {
                descriptor_owners.push(owner);
                descriptor_receipts.push(receipt);
            }
        }
        Ok(OriginalStatePhysicalCustody {
            process,
            operation,
            namespaces,
            shared_registrations: Vec::new(),
            recovery_roots,
            projection_files,
            generation_leases,
            directories: Vec::new(),
            descriptor_owners,
            descriptor_receipts,
            durability_receipts: vec![None; durability_pins.len()],
            durability_pins,
            _semantics: OutsideStateSemantics {
                _destination_join: destination_join,
                _trust: trust,
                _head_cache: head_cache,
                _repair_sink: repair_sink,
                _preparation_trust: preparation_trust,
                _cas_root: cas_root,
                _refs_root: refs_root,
                _locators_root: locators_root,
            },
        })
    }

    /// Direct receipt references have transferred. Opaque authority payloads
    /// may retain other native affinities; the actual region seal remains the
    /// mandatory final exclusion proof, and physical obligations remain here.
    pub fn native_receipts_transferred(&self) -> bool {
        self.process == std::process::id()
            && self.native.is_none()
            && self.native_terminals.is_empty()
            && self.native_transfer_failure.is_none()
    }
    pub fn operation(&self) -> &crate::recovery::ThreadHistoryDiscardOperationRef {
        &self.operation
    }
    /// # Safety
    /// Cache these same original payloads in the outside supervisor before
    /// any fallible retirement/physical release. They retain actual descriptors
    /// and authority affinities; this transfer proves neither their disposal
    /// nor an empty region census. The State continuation must remain retained.
    pub unsafe fn take_original_sql_custody(
        &mut self,
    ) -> anyhow::Result<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>> {
        anyhow::ensure!(
            self.process == std::process::id(),
            "outside State custody belongs to original process"
        );
        anyhow::ensure!(
            self.native_receipts_transferred(),
            "outside State SQL receipt handoff incomplete"
        );
        Ok(std::mem::take(&mut self.sql_custody))
    }
    /// Install the actual State-owned parent receiver before releasing source
    /// facade aliases. Cache it here so receipt transfers and later physical
    /// custody cannot lose the shared parent behind a weak destination link.
    pub fn prepare_destination_join(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.process == std::process::id(), "foreign outside State join preparation");
        if let Some(join) = &self.destination_join {
            return join.require_original(factory, terminal);
        }
        let native = self.native.as_ref().context("original State source already transferred before join preparation")?;
        anyhow::ensure!(native.generated.factory.same_original(factory), "outside State join selected foreign factory");
        let join = native.generated.authority.original_destination_join(factory, terminal, prepay)?;
        self.destination_join = Some(join);
        Ok(())
    }
    /// Retire actual native evidence holders only after original aggregate ACK.
    /// Busy projection aliases remain accessible here and can be retried; they
    /// prevent physical release rather than being dropped or called complete.
    pub fn retire_native_references(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &OriginalNativeTerminal,
        retire_aliases: &dyn Fn(&OriginalNativeConnectionTerminal) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.process == std::process::id(),
            "outside State custody belongs to original process"
        );
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        anyhow::ensure!(self.destination_join.is_some(), "original State destination receiver not prepared");
        if let Some(failure) = self.native_transfer_failure.take() {
            match failure.into_original() {
                Ok(original) => self.native_terminals.push(original),
                Err(failure) => {
                    self.native_transfer_failure = Some(failure);
                    anyhow::bail!("outside State original receipt recovery refused");
                }
            }
        }
        if let Some(native) = self.native.as_mut() {
            anyhow::ensure!(
                native.generated.factory.same_original(factory),
                "outside State selected foreign original factory"
            );
            for lease in &self.generation_leases {
                lease.retire_after_native_terminal(terminal)?;
            }
            if let Some(selected) = native.selected.take() {
                let (files, proof) = selected.into_outside_custody();
                self.projection_files.push(files);
                push_outside_terminal(
                    &mut self.native_terminals,
                    OriginalNativeConnectionTerminal::Closed(proof),
                );
            }
            native
                .previous
                .append(&mut native.generated.retired_selected);
            let mut unique = Vec::new();
            for owner in native.previous.drain(..) {
                if !unique.iter().any(|actual| Arc::ptr_eq(actual, &owner)) {
                    unique.push(owner);
                }
            }
            native.previous = unique;
            while let Some(owner) = native.previous.pop() {
                match Arc::try_unwrap(owner) {
                    Ok(owner) => {
                        let (files, proof) = owner.into_outside_custody();
                        self.projection_files.push(files);
                        push_outside_terminal(
                            &mut self.native_terminals,
                            OriginalNativeConnectionTerminal::Closed(proof),
                        );
                    }
                    Err(owner) => {
                        native.previous.push(owner);
                        anyhow::bail!(
                            "outside State projection still has an actual shared consumer"
                        );
                    }
                }
            }
            for proof in &native.generated.terminals {
                push_outside_terminal(
                    &mut self.native_terminals,
                    OriginalNativeConnectionTerminal::Closed(Arc::clone(proof)),
                );
            }
            for proof in &native.generated.durable {
                push_outside_terminal(
                    &mut self.native_terminals,
                    OriginalNativeConnectionTerminal::Closed(proof.closed()),
                );
            }
            let references =
                native.generated.schema_references.try_lock().map_err(|_| {
                    anyhow::anyhow!("outside State schema custody busy or poisoned")
                })?;
            for reference in references.iter() {
                push_outside_terminal(&mut self.native_terminals, reference.terminal().clone());
            }
            drop(references);
            // Every original terminal is registered above, each actual
            // filesystem holder remains outside, and durability pins plus
            // generation roots were retained during the genuine handoff.
            // Remove only redundant receipt/authority facade aliases here.
            drop(self.native.take());
        }
        while let Some(original) = self.native_terminals.last() {
            original.require_original(factory)?;
            // Keep the canonical genuine receipt cached before the controller
            // removes only its matching census aliases, outside retention gates.
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                retire_aliases(original)
            })?;
            let original = self.native_terminals.pop().unwrap();
            match original.into_outside_custody(terminal) {
                Ok(custody) => self.sql_custody.push(custody),
                Err(failure) => {
                    self.native_transfer_failure = Some(failure);
                    anyhow::bail!(
                        "outside State SQL receipt still has original aliases or obligations"
                    );
                }
            }
        }
        Ok(())
    }
}

fn push_outside_terminal(
    terminals: &mut Vec<OriginalNativeConnectionTerminal>,
    terminal: OriginalNativeConnectionTerminal,
) {
    let exists = terminals.iter().any(|actual| match (actual, &terminal) {
        (
            OriginalNativeConnectionTerminal::Closed(a),
            OriginalNativeConnectionTerminal::Closed(b),
        ) => Arc::ptr_eq(a, b),
        (
            OriginalNativeConnectionTerminal::Released(a),
            OriginalNativeConnectionTerminal::Released(b),
        ) => Arc::ptr_eq(a, b),
        _ => false,
    });
    if !exists {
        terminals.push(terminal);
    }
}

/// Borrowed finalizer capability; the real CAS guard remains thread-affine.
pub struct OriginalStateFinalAckAuthority<'frame> {
    state: &'frame Arc<ClosedOriginalState>,
    guard: &'frame crate::recovery::CasMutationGuard,
    terminal: &'frame Arc<OriginalNativeTerminal>,
    operation: &'frame crate::recovery::ThreadHistoryDiscardOperationRef,
    verify: &'frame dyn Fn() -> anyhow::Result<()>,
    prepay: &'frame dyn Fn(u64, u64, u64) -> anyhow::Result<()>,
}
impl<'frame> OriginalStateFinalAckAuthority<'frame> {
    /// # Safety
    /// The verifier must retain and revalidate the genuine ReadyForFinalACK
    /// owner: exact original claim/work/native terminal, durable prepared
    /// retirement and source-close receipts, charged reservation, original
    /// cleanup clock/budget and declared finalizer lifelines. It must join the
    /// authenticated staged operation to this actual exclusive State/CAS
    /// frame. `prepay` must irreversibly debit the supplied read, memory and
    /// cleanup-step bounds against that SAME original funded cleanup envelope
    /// before effects; partial debit/refusal retains all charges. It must not
    /// renew a deadline or use service re-admission after backend shutdown.
    /// Neither a phase enum nor decoded evidence grants this authority.
    pub unsafe fn issued(
        state: &'frame Arc<ClosedOriginalState>,
        guard: &'frame crate::recovery::CasMutationGuard,
        terminal: &'frame Arc<OriginalNativeTerminal>,
        operation: &'frame crate::recovery::ThreadHistoryDiscardOperationRef,
        verify: &'frame dyn Fn() -> anyhow::Result<()>,
        prepay: &'frame dyn Fn(u64, u64, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let authority = Self {
            state,
            guard,
            terminal,
            operation,
            verify,
            prepay,
        };
        authority.require(state)?;
        Ok(authority)
    }
    fn require(&self, state: &Arc<ClosedOriginalState>) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(self.state, state),
            "final ACK selected a different State owner"
        );
        (self.verify)()?;
        state.require_retained_guard(self.guard, Some(self.operation))?;
        self.terminal.require_original(&state.factory)?;
        state.native_terminals()?;
        Ok(())
    }
}
struct OriginalStateAcknowledgementBody {
    terminal: Arc<OriginalNativeTerminal>,
    operation: crate::recovery::ThreadHistoryDiscardOperationRef,
}
/// Actual State acknowledgement evidence, not full resource settlement.
/// Construction is private and requires the completed recovery effect above.
pub struct OriginalStateAcknowledgement {
    state: Arc<ClosedOriginalState>,
    body: Arc<OriginalStateAcknowledgementBody>,
}
impl OriginalStateAcknowledgement {
    pub fn require_original(
        &self,
        state: &Arc<ClosedOriginalState>,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<OriginalNativeTerminal>,
        operation: &crate::recovery::ThreadHistoryDiscardOperationRef,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&self.state, state)
                && state.factory.same_original(factory)
                && Arc::ptr_eq(&self.body.terminal, terminal)
                && &self.body.operation == operation
                && state
                    .acknowledgement
                    .get()
                    .is_some_and(|body| Arc::ptr_eq(body, &self.body)),
            "State acknowledgement selects a foreign effect or owner"
        );
        self.body.terminal.require_original(factory)
    }
}

enum RetirementCustody {
    State(StateDb),
    Projection(RetiredStateContext, OriginalProjectionFailure),
    Closed(ClosedOriginalState),
}
pub struct OriginalStateRetirementFailure {
    error: anyhow::Error,
    factory: Arc<OriginalNativeFactory>,
    custody: Mutex<RetirementCustody>,
}
impl std::fmt::Debug for OriginalStateRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalStateRetirementFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalStateRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for OriginalStateRetirementFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Cleanup outcome of an already refused State retirement. Retains its original
/// error and real roots/leases; it cannot issue successful marker acknowledgement.
pub struct ReleasedOriginalStateRetirement {
    error: anyhow::Error,
    context: RetiredStateContext,
    selected: StateCleanupTerminal,
}
enum StateCleanupTerminal {
    Acquisition(ReleasedOriginalProjectionAcquisition),
    Closed(ClosedOriginalProjection),
}
impl ReleasedOriginalStateRetirement {
    pub fn original_error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn native_terminals(&self) -> anyhow::Result<Vec<OriginalNativeConnectionTerminal>> {
        let mut terminals = self.context.previous_terminals()?;
        let selected = match &self.selected {
            StateCleanupTerminal::Acquisition(owner) => owner.terminal().clone(),
            StateCleanupTerminal::Closed(owner) => {
                owner.require_original(&self.context.factory)?;
                OriginalNativeConnectionTerminal::Closed(owner.native_proof())
            }
        };
        selected.require_original(&self.context.factory)?;
        terminals.push(selected);
        Ok(terminals)
    }
}

impl OriginalStateRetirementFailure {
    pub fn retry_cleanup(self) -> std::result::Result<ReleasedOriginalStateRetirement, Self> {
        let Self {
            error,
            factory,
            custody,
        } = self;
        let custody = custody.into_inner().unwrap_or_else(|p| p.into_inner());
        match custody {
            RetirementCustody::Projection(context, owner) => match owner.retry_close(&factory) {
                Ok(selected) => Ok(ReleasedOriginalStateRetirement {
                    error,
                    context,
                    selected: StateCleanupTerminal::Acquisition(selected),
                }),
                Err(owner) => Err(Self {
                    error,
                    factory,
                    custody: Mutex::new(RetirementCustody::Projection(context, owner)),
                }),
            },
            RetirementCustody::Closed(closed) => {
                let ClosedOriginalState {
                    factory: original_factory,
                    context,
                    selected,
                    acknowledgement,
                } = closed;
                match context.into_inner().unwrap_or_else(|p| p.into_inner()) {
                    StateContextCustody::Retained(context) => Ok(ReleasedOriginalStateRetirement {
                        error,
                        context,
                        selected: StateCleanupTerminal::Closed(selected),
                    }),
                    StateContextCustody::Finalizer(finalizer) => Err(Self {
                        error,
                        factory,
                        custody: Mutex::new(RetirementCustody::Closed(ClosedOriginalState {
                            factory: original_factory,
                            context: Mutex::new(StateContextCustody::Finalizer(finalizer)),
                            selected,
                            acknowledgement,
                        })),
                    }),
                }
            }
            // A refused preflight retains the complete original live State.
            // It must be reconciled under its original guard, never default-opened.
            RetirementCustody::State(state) => Err(Self {
                error,
                factory,
                custody: Mutex::new(RetirementCustody::State(state)),
            }),
        }
    }
}

impl StateDb {
    pub fn close_original_projection(
        self,
        factory: Arc<OriginalNativeFactory>,
        guard: &crate::recovery::CasMutationGuard,
        operation: Option<&crate::recovery::ThreadHistoryDiscardOperationRef>,
    ) -> std::result::Result<ClosedOriginalState, OriginalStateRetirementFailure> {
        let checked = (|| -> anyhow::Result<()> {
            self.ensure_cas_mutation_guard(guard)?;
            anyhow::ensure!(
                guard.is_exclusive(),
                "original State close requires exclusive CAS guard"
            );
            anyhow::ensure!(
                matches!(&self.operational, OperationalAccess::ProjectionMaintenance),
                "original State retirement cannot drop an operational database owner"
            );
            let original = self
                .original_projection_context
                .as_ref()
                .context("original State factory context absent")?;
            anyhow::ensure!(
                original.factory.same_original(&factory)
                    && self
                        .projection
                        .original_factory()
                        .is_some_and(|owner| owner.same_original(&factory)),
                "original State close selects another factory"
            );
            self._offline_namespace_lock
                .as_ref()
                .context("original State namespace absent")?
                .ensure_protects(&self._runtime_state_directory)?;
            anyhow::ensure!(
                self.recovery.thread_history_discard_operation_scoped(&factory)?.as_ref() == operation,
                "original State retirement operation differs from active marker"
            );
            Ok(())
        })();
        if let Err(error) = checked {
            return Err(OriginalStateRetirementFailure {
                error,
                factory,
                custody: Mutex::new(RetirementCustody::State(self)),
            });
        }
        let StateDb {
            _runtime_state_directory: runtime,
            _offline_namespace_lock: namespace,
            _cas_directory: cas,
            _refs_directory: refs,
            _locators_directory: locators,
            _recovery_directory: recovery_directory,
            projection,
            original_projection_terminals: previous,
            original_projection_context: generated,
            recovery,
            trust_store: trust,
            head_cache,
            projection_repair_sink: repair_sink,
            ..
        } = self;
        let context = RetiredStateContext {
            factory: Arc::clone(&factory),
            runtime,
            namespace: namespace.expect("checked namespace"),
            cas,
            refs,
            locators,
            recovery_directory,
            recovery,
            operation: operation.cloned(),
            previous,
            generated: generated.expect("checked original context"),
            trust,
            head_cache,
            repair_sink,
        };
        let selected = match projection.close_original(&factory) {
            Ok(selected) => selected,
            Err(owner) => {
                return Err(OriginalStateRetirementFailure {
                    error: anyhow::anyhow!(
                        "original State projection close remains pending: {owner}"
                    ),
                    factory,
                    custody: Mutex::new(RetirementCustody::Projection(context, owner)),
                });
            }
        };
        let closed = ClosedOriginalState {
            factory: factory.clone(),
            context: Mutex::new(StateContextCustody::Retained(context)),
            selected,
            acknowledgement: std::sync::OnceLock::new(),
        };
        let checked = closed
            .require_retained_guard(guard, operation)
            .and_then(|()| closed.native_terminals().map(|_| ()));
        if let Err(error) = checked {
            return Err(OriginalStateRetirementFailure {
                error,
                factory,
                custody: Mutex::new(RetirementCustody::Closed(closed)),
            });
        }
        Ok(closed)
    }
}

#[cfg(test)]
mod outside_quote_tests {
    use super::{
        quote_state_finalizer_frame, quote_state_outside_continuation,
        quote_state_outside_handoff, quote_state_physical_transfer,
        quote_state_root_transfer,
    };
    use lillux_sqlite::original_physical_custody::OutsideWork;

    #[test]
    fn state_outside_quotes_cover_fixed_and_counted_owners() {
        assert_eq!(quote_state_finalizer_frame(), OutsideWork { bytes: 4096, steps: 32 });
        assert_eq!(quote_state_root_transfer(), OutsideWork { bytes: 512, steps: 4 });
        assert_eq!(quote_state_outside_handoff(), OutsideWork { bytes: 8192, steps: 64 });
        assert_eq!(quote_state_outside_continuation(3, 4).unwrap(),
            OutsideWork { bytes: 9728, steps: 56 });
        assert_eq!(quote_state_physical_transfer(2).unwrap(),
            OutsideWork { bytes: 9216, steps: 80 });
        assert!(quote_state_physical_transfer(usize::MAX).is_err());
    }

    #[test]
    fn state_ack_prepay_is_the_marker_rewrite_quote_plus_one_receipt() {
        let rewrite = crate::recovery::thread_history_discard_acknowledgement_quote();
        let (read, memory, steps) = super::STATE_ACK_EFFECT_PREPAY;
        let (replay_read, replay_memory, replay_steps) = super::STATE_ACK_REPLAY_PREPAY;
        assert_eq!(read, 513);
        assert_eq!((replay_read, replay_memory, replay_steps), (0, 256, 1));
        assert_eq!(memory, rewrite.memory_bytes + replay_memory);
        assert_eq!(steps, rewrite.steps + replay_steps);
        assert_eq!(steps, 15);
    }
}
