//! Consuming offline State projection retirement. No SQL survives in the
//! descriptor-only owner used for the final original-operation acknowledgement.
use super::*;
use crate::projection::{
    ClosedOriginalProjection, OriginalProjectionFailure, ReleasedOriginalProjectionAcquisition,
};
use lillux_sqlite::{
    OriginalNativeConnectionTerminal, OriginalNativeFactory, OriginalNativeTerminal,
};

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
    context: Mutex<RetiredStateContext>,
    selected: ClosedOriginalProjection,
    acknowledgement: std::sync::OnceLock<Arc<OriginalStateAcknowledgementBody>>,
}
impl ClosedOriginalState {
    pub fn native_terminals(&self) -> anyhow::Result<Vec<OriginalNativeConnectionTerminal>> {
        self.factory.require_original_process()?;
        self.selected.require_original(&self.factory)?;
        let snapshot = self
            .context
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State context custody busy or poisoned"))?
            .terminal_snapshot()?;
        let mut terminals = snapshot.validate(&self.factory)?;
        let selected = self.selected.native_proof();
        if !terminals.iter().any(|terminal| {
            matches!(terminal,
            OriginalNativeConnectionTerminal::Closed(proof) if Arc::ptr_eq(proof, &selected))
        }) {
            terminals.push(OriginalNativeConnectionTerminal::Closed(selected));
        }
        Ok(terminals)
    }
    /// Every generation lease this State issued, from its retained context.
    fn generation_lease_registry(
        &self,
    ) -> anyhow::Result<Vec<Arc<crate::projection::OriginalGenerationLease>>> {
        let context = self
            .context
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State generation registry custody busy or poisoned"))?;
        let leases = context
            .generated
            .generation_leases
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State generation registry busy or poisoned"))?
            .clone();
        Ok(leases)
    }
    /// Retire every generation lease against the finished native backend.
    /// A closed lease keeps its closed receipt, and with it a factory alias,
    /// until it is retired; dropping the lease never releases them. Run once
    /// the backend terminal exists and before the native host is released.
    /// A retry skips leases already retired; a lease that never reached its
    /// closed disposition refuses and stays in the registry.
    pub fn retire_generation_leases_after_backend(
        &self,
        terminal: &OriginalNativeTerminal,
    ) -> anyhow::Result<()> {
        self.factory.require_original_process()?;
        terminal.require_original(&self.factory)?;
        for lease in self.generation_lease_registry()? {
            lease.retire_after_native_terminal(terminal)?;
        }
        Ok(())
    }
    /// The number of generation leases not yet retired; each still holds a
    /// factory alias.
    pub fn unretired_generation_leases(&self) -> anyhow::Result<usize> {
        let mut unretired = 0;
        for lease in self.generation_lease_registry()? {
            if !lease.is_retired()? {
                unretired += 1;
            }
        }
        Ok(unretired)
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
            let context = self
                .context
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State generation guard custody busy or poisoned"))?;
            context.require_guard(guard)?;
        }
        self.native_terminals()?;
        let leases = {
            let context = self
                .context
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State generation custody busy or poisoned"))?;
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
            let context = self
                .context
                .try_lock()
                .map_err(|_| anyhow::anyhow!("State ACK custody busy or poisoned"))?;
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
        let context = self
            .context
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State context custody busy or poisoned"))?;
        context.require_guard(guard)?;
        anyhow::ensure!(
            context.operation.as_ref() == operation,
            "State context operation differs from the actual captured marker"
        );
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
                    factory: _,
                    context,
                    selected,
                    acknowledgement: _,
                } = closed;
                Ok(ReleasedOriginalStateRetirement {
                    error,
                    context: context.into_inner().unwrap_or_else(|p| p.into_inner()),
                    selected: StateCleanupTerminal::Closed(selected),
                })
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
                    && self.projection.original_factory().same_original(&factory),
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
            // Head verification state, the repair sink and the operational
            // handle serve only live reads; a retired State performs none,
            // so its context does not keep them.
            trust_store: _,
            head_cache: _,
            projection_repair_sink: _,
            operational: _,
            cas_root: _,
            refs_root: _,
            locators_root: _,
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
            context: Mutex::new(context),
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
mod state_ack_quote_tests {
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
