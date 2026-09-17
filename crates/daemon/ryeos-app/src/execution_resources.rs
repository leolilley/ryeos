//! Node-owned execution resource observation and deterministic selection.
//!
//! This is deliberately meaning-blind. Node policy supplies resource classes,
//! scalar facts, an observation-contract identity and exact character-device
//! declarations. Selection performs only equality and integer-minimum matching.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context as _, bail};

use crate::node_policy::sections::execution::{
    NodeExecutionResourceDescriptor, NodeExecutionResourcePolicy,
};
use ryeos_engine::contracts::{
    ExecutionResourceAccess, ExecutionResourceEnforcement, ExecutionResourceFactValue,
    ExecutionResourceRequirement, ExecutionResourceSelection, ExecutionTargetRequirement,
};

#[derive(Debug, Clone)]
pub struct PreparedResourceFinancialOperations {
    bindings: Vec<ryeos_accounting::ResourceOperationBinding>,
    occupancy_start: Option<lillux::time::OccupancyCoordinate>,
    occupancy_limit: Option<lillux::time::OccupancyLimit>,
    cleanup_allowance_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct ObservedResource {
    descriptor: NodeExecutionResourceDescriptor,
    devices: Option<Arc<lillux::CharacterDeviceSet>>,
    device_binding_digest: String,
}

/// Immutable resource generation captured from one validated node-policy
/// generation. Live allocation is an index over durable process owners and is
/// intentionally not represented here.
#[derive(Debug, Clone, Default)]
pub struct ExecutionResourcePool {
    resources: Vec<ObservedResource>,
    max_concurrent_exclusive_allocations: Option<u32>,
    cleanup_allowance_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SelectedExecutionResources {
    selections: Vec<ExecutionResourceSelection>,
    devices: Option<Arc<lillux::CharacterDeviceSet>>,
    accounting: Vec<Option<ryeos_accounting::ResourceAccountingAuthority>>,
    max_concurrent_exclusive_allocations: Option<u32>,
    cleanup_allowance_ms: Option<u64>,
}

/// Exact pre-contact owner of one resource-bearing launch.  The process scope
/// is moved into Lillux's held spawn, while the immutable recovery record stays
/// available for failure cleanup until attachment atomically consumes the
/// durable reservation.
pub struct PreparedProcessResourceScope {
    reservation: crate::runtime_db::ProcessResourceReservationRecord,
    scope: Option<lillux::ProcessScope>,
}

impl PreparedProcessResourceScope {
    pub fn take_scope(&mut self) -> anyhow::Result<lillux::ProcessScope> {
        self.scope
            .take()
            .context("prepared process resource scope was already consumed")
    }

    pub fn reservation(&self) -> &crate::runtime_db::ProcessResourceReservationRecord {
        &self.reservation
    }
}

impl ExecutionResourcePool {
    pub fn deny_all() -> Self {
        Self::default()
    }

    pub fn observe(policy: Option<&NodeExecutionResourcePolicy>) -> anyhow::Result<Self> {
        let Some(policy) = policy else {
            return Ok(Self::deny_all());
        };
        policy.validate()?;
        let mut resources = Vec::with_capacity(policy.resources.len());
        let occupancy_clock = if policy
            .resources
            .iter()
            .any(|resource| resource.accounting.is_some())
        {
            let contract = lillux::time::OccupancyClockContract::current()
                .map_err(anyhow::Error::msg)
                .context("resolve Lillux occupancy-clock contract")?;
            contract
                .validate()
                .map_err(anyhow::Error::msg)
                .context("validate Lillux occupancy-clock contract")?;
            Some(contract)
        } else {
            None
        };
        for descriptor in &policy.resources {
            let accounting = descriptor.accounting.as_ref().with_context(|| {
                format!(
                    "node resource `{}` lacks explicit resource-accounting authority",
                    descriptor.stable_id
                )
            })?;
            if accounting.meter.clock_contract_digest.as_str()
                != occupancy_clock
                    .as_ref()
                    .expect("accounted resources resolved a Lillux clock contract")
                    .contract_digest
            {
                bail!(
                    "node resource `{}` accounting meter is not supported by the active Lillux clock contract",
                    descriptor.stable_id
                );
            }
            let devices = if descriptor.character_devices.is_empty() {
                None
            } else {
                Some(Arc::new(
                    lillux::CharacterDeviceSet::observe(&descriptor.character_devices)
                        .map_err(anyhow::Error::msg)
                        .with_context(|| {
                            format!(
                                "observe character devices for node resource `{}`",
                                descriptor.stable_id
                            )
                        })?,
                ))
            };
            if !descriptor.facts.is_empty() {
                bail!(
                    "node resource `{}` declares semantic facts without a qualified admitted host observer",
                    descriptor.stable_id
                );
            }
            if devices.is_none() {
                bail!(
                    "node resource `{}` has no Lillux-observed concrete assignment",
                    descriptor.stable_id
                );
            }
            let device_binding_digest = match &devices {
                Some(devices) => devices.binding_digest().to_owned(),
                None => unreachable!("resource observation requires a concrete assignment"),
            };
            let observed_contract_digest = lillux::resource_observation_contract_digest(
                &descriptor.stable_id,
                &descriptor.class,
                &descriptor.facts,
                devices
                    .as_ref()
                    .map_or(&[][..], |devices| devices.identities()),
            )
            .map_err(anyhow::Error::msg)?;
            if observed_contract_digest != descriptor.observation_contract_digest {
                bail!(
                    "node resource `{}` observation contract does not match its retained host binding",
                    descriptor.stable_id
                );
            }
            resources.push(ObservedResource {
                descriptor: descriptor.clone(),
                devices,
                device_binding_digest,
            });
        }
        Ok(Self {
            resources,
            max_concurrent_exclusive_allocations: Some(
                policy.admission.max_concurrent_exclusive_allocations,
            ),
            cleanup_allowance_ms: Some(policy.cleanup_allowance_ms),
        })
    }

    pub fn select(
        &self,
        target: Option<&ExecutionTargetRequirement>,
    ) -> anyhow::Result<SelectedExecutionResources> {
        let Some(target) = target else {
            return Ok(SelectedExecutionResources::empty());
        };
        target.validate_current_platform()?;
        let mut selected_ids = BTreeSet::new();
        let mut selections = Vec::new();
        let mut device_sets = Vec::new();
        let mut accounting = Vec::new();
        for requirement in &target.resources {
            let mut matches = self
                .resources
                .iter()
                .filter(|resource| {
                    !selected_ids.contains(resource.descriptor.stable_id.as_str())
                        && resource_matches(&resource.descriptor, requirement)
                })
                .take(usize::from(requirement.count).saturating_add(1))
                .collect::<Vec<_>>();
            if matches.len() != usize::from(requirement.count) {
                bail!(
                    "execution resource `{}` requires exactly {} matching resources; observed {}",
                    requirement.class,
                    requirement.count,
                    matches.len()
                );
            }
            matches
                .sort_by(|left, right| left.descriptor.stable_id.cmp(&right.descriptor.stable_id));
            for resource in matches {
                selected_ids.insert(resource.descriptor.stable_id.as_str());
                let (enforcement, identities) = match requirement.access {
                    ExecutionResourceAccess::DeploymentVisible => {
                        (ExecutionResourceEnforcement::DeploymentVisible, Vec::new())
                    }
                    ExecutionResourceAccess::ExecutionRestricted => {
                        let device_set = resource.devices.as_ref().with_context(|| {
                            format!(
                                "resource `{}` has no descriptor grant for execution-restricted access",
                                resource.descriptor.stable_id
                            )
                        })?;
                        device_sets.push(Arc::clone(device_set));
                        (
                            ExecutionResourceEnforcement::CharacterDeviceGrant,
                            device_set.identities().to_vec(),
                        )
                    }
                };
                let selection = ExecutionResourceSelection {
                    stable_id: resource.descriptor.stable_id.clone(),
                    class: resource.descriptor.class.clone(),
                    matched_facts: matched_facts(&resource.descriptor, requirement),
                    observation_contract_digest: resource
                        .descriptor
                        .observation_contract_digest
                        .clone(),
                    device_binding_digest: resource.device_binding_digest.clone(),
                    access: requirement.access,
                    enforcement,
                    character_devices: identities,
                };
                selection.validate()?;
                selections.push(selection);
                accounting.push(resource.descriptor.accounting.clone());
            }
        }
        let devices = if device_sets.is_empty() {
            None
        } else {
            let borrowed = device_sets.iter().map(Arc::as_ref).collect::<Vec<_>>();
            Some(Arc::new(
                lillux::CharacterDeviceSet::combine(&borrowed)
                    .map_err(anyhow::Error::msg)
                    .context("combine selected character-device authorities")?,
            ))
        };
        Ok(SelectedExecutionResources {
            selections,
            devices,
            accounting,
            max_concurrent_exclusive_allocations: self.max_concurrent_exclusive_allocations,
            cleanup_allowance_ms: self.cleanup_allowance_ms,
        })
    }
}

impl SelectedExecutionResources {
    pub fn empty() -> Self {
        Self {
            selections: Vec::new(),
            devices: None,
            accounting: Vec::new(),
            max_concurrent_exclusive_allocations: None,
            cleanup_allowance_ms: None,
        }
    }

    pub fn selections(&self) -> &[ExecutionResourceSelection] {
        &self.selections
    }

    pub fn devices(&self) -> Option<&Arc<lillux::CharacterDeviceSet>> {
        self.devices.as_ref()
    }

    pub fn accounting_authorities(
        &self,
    ) -> &[Option<ryeos_accounting::ResourceAccountingAuthority>] {
        &self.accounting
    }

    pub fn max_concurrent_exclusive_allocations(&self) -> Option<u32> {
        self.max_concurrent_exclusive_allocations
    }

    pub fn cleanup_allowance_ms(&self) -> Option<u64> {
        self.cleanup_allowance_ms
    }
}

/// Reserve exclusive device ownership and a durable Lillux process scope
/// before any kernel process can be created.  An empty selection needs neither
/// contract.  A retained reservation is recovery work, never relaunch input.
pub fn prepare_process_resource_scope(
    state: &crate::state::AppState,
    selected: &SelectedExecutionResources,
    owner_kind: &str,
    owner_coordinate: &str,
) -> anyhow::Result<Option<PreparedProcessResourceScope>> {
    if selected.selections.is_empty() {
        return Ok(None);
    }
    let allocation_limit = selected
        .max_concurrent_exclusive_allocations
        .context("resource-bearing launch lacks an allocation ceiling")?;
    let allocation_name = format!(
        "resource-{}",
        &lillux::sha256_hex(
            lillux::canonical_json(&serde_json::json!({
                "owner_kind": owner_kind,
                "owner_coordinate": owner_coordinate,
                "daemon_generation_id": crate::runtime_db::daemon_generation_id(),
            }))?
            .as_bytes(),
        )[..32]
    );
    let control_timeout = state.isolation.process_scope_control_timeout()?;
    let allocation = state.isolation.plan_process_scope(&allocation_name)?;
    let mut reservation = crate::runtime_db::ProcessResourceReservationRecord {
        owner_kind: owner_kind.to_owned(),
        owner_coordinate: owner_coordinate.to_owned(),
        daemon_generation_id: crate::runtime_db::daemon_generation_id().to_owned(),
        selections: selected.selections.clone(),
        allocation_limit,
        scope_allocation: allocation.clone(),
        scope_recovery: None,
    };
    state
        .state_store
        .reserve_process_resource_launch(&reservation)?;
    let scope = match state.isolation.allocate_process_scope(&allocation) {
        Ok(scope) => scope,
        Err(error) => {
            let error = anyhow::Error::from(error);
            return Err(match allocation.discard_unlaunched() {
                Ok(()) => match state
                    .state_store
                    .clear_process_resource_reservation(&reservation)
                {
                    Ok(()) => error,
                    Err(clear) => error.context(format!(
                        "discarded unlaunched resource scope but retained reservation cleanup failed: {clear:#}"
                    )),
                },
                Err(cleanup) => error.context(format!(
                    "unlaunched process resource scope cleanup remains unproved: {cleanup}"
                )),
            });
        }
    };
    let recovery = scope.recovery().clone();
    if let Err(error) =
        state
            .state_store
            .bind_process_resource_scope(owner_kind, owner_coordinate, &recovery)
    {
        return Err(match scope.retire_unlaunched(control_timeout) {
            Ok(()) => match state
                .state_store
                .clear_process_resource_reservation(&reservation)
            {
                Ok(()) => error,
                Err(clear) => error.context(format!(
                    "retired unlaunched resource scope but reservation cleanup failed: {clear:#}"
                )),
            },
            Err(cleanup) => error.context(format!(
                "bound process resource scope cleanup remains unproved: {cleanup}"
            )),
        });
    }
    reservation.scope_recovery = Some(recovery);
    Ok(Some(PreparedProcessResourceScope {
        reservation,
        scope: Some(scope),
    }))
}

/// Reserve resources against a scope whose creation is already durably
/// journaled by the enclosing dedicated-session owner.  This preserves one
/// generic contention index without duplicating the dedicated launch journal.
pub fn reserve_bound_process_resource_scope(
    state: &crate::state::AppState,
    selected: &SelectedExecutionResources,
    owner_kind: &str,
    owner_coordinate: &str,
    allocation: &lillux::ProcessScopeAllocation,
    recovery: &lillux::ProcessScopeRecovery,
) -> anyhow::Result<Option<crate::runtime_db::ProcessResourceReservationRecord>> {
    if selected.selections.is_empty() {
        return Ok(None);
    }
    if !recovery.matches_allocation(allocation) {
        bail!("dedicated process scope differs from its allocation intent");
    }
    let reservation = crate::runtime_db::ProcessResourceReservationRecord {
        owner_kind: owner_kind.to_owned(),
        owner_coordinate: owner_coordinate.to_owned(),
        daemon_generation_id: crate::runtime_db::daemon_generation_id().to_owned(),
        selections: selected.selections.clone(),
        allocation_limit: selected
            .max_concurrent_exclusive_allocations
            .context("resource-bearing launch lacks an allocation ceiling")?,
        scope_allocation: allocation.clone(),
        scope_recovery: Some(recovery.clone()),
    };
    state
        .state_store
        .reserve_process_resource_launch(&reservation)?;
    Ok(Some(reservation))
}

/// Prove a retained pre-contact scope empty/dead and release the exact
/// reservation. This is safe both before spawn and after a held-spawn failure;
/// it never infers cleanup from a PID or a caller assertion.
pub fn cleanup_process_resource_reservation(
    state: &crate::state::AppState,
    reservation: &crate::runtime_db::ProcessResourceReservationRecord,
) -> anyhow::Result<()> {
    match reservation.scope_recovery.as_ref() {
        Some(recovery) => recovery
            .terminate_and_wait(state.isolation.process_scope_control_timeout()?)
            .map_err(anyhow::Error::msg)?,
        None => reservation
            .scope_allocation
            .discard_unlaunched()
            .map_err(anyhow::Error::msg)?,
    }
    state
        .state_store
        .clear_process_resource_reservation(reservation)
}

pub fn cleanup_process_resource_reservation_for_owner(
    state: &crate::state::AppState,
    owner_kind: &str,
    owner_coordinate: &str,
) -> anyhow::Result<()> {
    if let Some(reservation) = state
        .state_store
        .process_resource_reservation(owner_kind, owner_coordinate)?
    {
        cleanup_process_resource_reservation(state, &reservation)?;
    }
    Ok(())
}

impl PreparedResourceFinancialOperations {
    pub fn bindings(&self) -> &[ryeos_accounting::ResourceOperationBinding] {
        &self.bindings
    }

    pub fn occupancy_start(&self) -> Option<lillux::time::OccupancyCoordinate> {
        self.occupancy_start.clone()
    }

    pub fn occupancy_limit(&self) -> Option<lillux::time::OccupancyLimit> {
        self.occupancy_limit.clone()
    }

    pub fn cleanup_allowance_ms(&self) -> Option<u64> {
        self.cleanup_allowance_ms
    }
}

/// Reserve every selected resource under the launch's existing financial
/// scope, then sample the platform-neutral occupancy start through Lillux.
/// The held process has not been released at this boundary.
pub fn reserve_process_resource_operations(
    state: &crate::state::AppState,
    selected: &SelectedExecutionResources,
    scope: Option<&ryeos_state::objects::AdmittedAccountingScope>,
    root_chain_id: &str,
    thread_id: &str,
    launch_generation: &str,
    owner_identity: &crate::process::ExecutionProcessIdentity,
) -> anyhow::Result<PreparedResourceFinancialOperations> {
    if selected.selections.is_empty() {
        return Ok(PreparedResourceFinancialOperations {
            bindings: Vec::new(),
            occupancy_start: None,
            occupancy_limit: None,
            cleanup_allowance_ms: None,
        });
    }
    if selected.selections.len() != selected.accounting.len() {
        bail!("selected resource/accounting cardinality is inconsistent");
    }
    let maximum_occupancy_milliseconds = selected
        .accounting
        .iter()
        .map(|authority| {
            authority
                .as_ref()
                .context("selected resource has no admitted accounting authority")
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|authority| match authority.spend {
            ryeos_accounting::ResourceSpendAuthority::Bounded {
                maximum_occupancy_milliseconds,
                ..
            } => Some(maximum_occupancy_milliseconds),
            ryeos_accounting::ResourceSpendAuthority::Advisory => None,
        })
        .min();
    let cleanup_allowance_ms = maximum_occupancy_milliseconds
        .map(|maximum| {
            let allowance = selected
                .cleanup_allowance_ms
                .context("bounded resource execution lacks its node cleanup allowance")?;
            if allowance >= maximum {
                bail!("resource cleanup allowance consumes the finite occupancy maximum");
            }
            maximum
                .checked_mul(1_000_000)
                .context("resource occupancy limit overflows nanoseconds")?;
            Ok(allowance)
        })
        .transpose()?;
    let accounting = state
        .accounting
        .as_ref()
        .context("resource-bearing execution requires the shared accounting ledger")?;
    let scope = scope.context("resource-bearing execution lacks admitted accounting scope")?;
    scope.validate()?;
    let owner_incarnation = owner_identity.owner_incarnation_digest()?;
    let owner_gate_id = accounting.open_resource_owner_accounting_gate(
        owner_identity,
        &scope.execution_budget_id,
        scope.directive_budget_id.as_deref(),
        root_chain_id,
        root_chain_id,
        lillux::time::timestamp_millis(),
    )?;
    let fence_owner_gate = || {
        accounting.release_unattached_resource_owner_gate(
            owner_gate_id.as_str(),
            owner_incarnation.as_str(),
            lillux::time::timestamp_millis(),
        )
    };
    let mut bindings = Vec::with_capacity(selected.selections.len());
    for (selection, authority) in selected.selections.iter().zip(&selected.accounting) {
        let authority = authority.as_ref().with_context(|| {
            format!(
                "selected resource `{}` has no admitted accounting authority",
                selection.stable_id
            )
        })?;
        let request = serde_json::json!({
            "version": 2,
            "owner_gate_id": owner_gate_id,
            "thread_id": thread_id,
            "launch_generation": launch_generation,
            "root_chain_id": root_chain_id,
            "execution_budget_id": scope.execution_budget_id,
            "directive_budget_id": scope.directive_budget_id,
            "owner_incarnation": owner_incarnation,
            "stable_resource_id": selection.stable_id,
            "authority_digest": authority.authority_digest,
        });
        let request_digest =
            ryeos_accounting::HexDigest::of_canonical_json(&request).map_err(anyhow::Error::msg)?;
        let operation_id = ryeos_accounting::HexDigest::of_canonical_json(&serde_json::json!({
            "kind": "resource_occupancy_operation",
            "request_digest": request_digest,
        }))
        .map_err(anyhow::Error::msg)?;
        let outcome = match accounting.reserve_resource_operation(
            crate::accounting_db::ReserveResourceOperationArgs {
                owner_gate_id: owner_gate_id.as_str(),
                operation_id: operation_id.as_str(),
                request_digest: request_digest.as_str(),
                execution_budget_id: &scope.execution_budget_id,
                directive_budget_id: scope.directive_budget_id.as_deref(),
                root_chain_id,
                audit_chain_root_id: root_chain_id,
                thread_id,
                launch_generation,
                owner_incarnation: owner_incarnation.as_str(),
                authority,
                now_ms: lillux::time::timestamp_millis(),
            },
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Err(match fence_owner_gate() {
                    Ok(()) => error,
                    Err(cleanup) => error.context(format!(
                        "resource reservation cleanup remained incomplete: {cleanup:#}"
                    )),
                });
            }
        };
        match outcome {
            crate::accounting_db::ResourceOperationReserveOutcome::Reserved { .. }
            | crate::accounting_db::ResourceOperationReserveOutcome::Advisory { .. } => {}
            crate::accounting_db::ResourceOperationReserveOutcome::Denied { .. } => {
                fence_owner_gate()?;
                bail!("resource financial reservation was denied");
            }
            crate::accounting_db::ResourceOperationReserveOutcome::ReleasedUnissued { .. } => {
                fence_owner_gate()?;
                bail!("resource financial operation was already released unissued");
            }
        }
        bindings.push(ryeos_accounting::ResourceOperationBinding {
            version: ryeos_accounting::RESOURCE_OPERATION_BINDING_VERSION,
            owner_gate_id: owner_gate_id.clone(),
            operation_id,
            request_digest,
            owner_incarnation: ryeos_accounting::HexDigest::new(owner_incarnation.clone())
                .map_err(anyhow::Error::msg)?,
            stable_resource_id: selection.stable_id.clone(),
            authority_digest: authority.authority_digest.clone(),
            meter_contract_digest: authority.meter.contract_digest.clone(),
            clock_contract_digest: authority.meter.clock_contract_digest.clone(),
            maximum_occupancy_milliseconds: match authority.spend {
                ryeos_accounting::ResourceSpendAuthority::Bounded {
                    maximum_occupancy_milliseconds,
                    ..
                } => Some(maximum_occupancy_milliseconds),
                ryeos_accounting::ResourceSpendAuthority::Advisory => None,
            },
        });
    }
    let occupancy_start = match lillux::time::occupancy_now()
        .map_err(anyhow::Error::msg)
        .context("sample resource occupancy start after financial reservation")
    {
        Ok(start) => start,
        Err(error) => {
            return Err(match fence_owner_gate() {
                Ok(()) => error,
                Err(cleanup) => error.context(format!(
                    "occupancy sampling failed and reservation cleanup failed: {cleanup:#}"
                )),
            });
        }
    };
    let occupancy_limit = maximum_occupancy_milliseconds
        .map(|maximum| {
            maximum
                .checked_mul(1_000_000)
                .ok_or_else(|| anyhow::anyhow!("resource occupancy limit overflows nanoseconds"))
                .and_then(|maximum_ns| {
                    lillux::time::OccupancyLimit::new(occupancy_start.clone(), maximum_ns)
                        .map_err(anyhow::Error::msg)
                })
        })
        .transpose();
    let occupancy_limit = match occupancy_limit {
        Ok(limit) => limit,
        Err(error) => {
            return match fence_owner_gate() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "occupancy-limit construction failed and reservation cleanup failed: {cleanup:#}"
                ))),
            };
        }
    };
    Ok(PreparedResourceFinancialOperations {
        bindings,
        occupancy_start: Some(occupancy_start),
        occupancy_limit,
        cleanup_allowance_ms,
    })
}

pub fn issue_process_resource_operations(
    state: &crate::state::AppState,
    bindings: &[ryeos_accounting::ResourceOperationBinding],
) -> anyhow::Result<()> {
    if bindings.is_empty() {
        return Ok(());
    }
    let accounting = state
        .accounting
        .as_ref()
        .context("resource operation issue requires the shared accounting ledger")?;
    for binding in bindings {
        accounting.issue_resource_operation(
            binding.operation_id.as_str(),
            binding.request_digest.as_str(),
            lillux::time::timestamp_millis(),
        )?;
    }
    Ok(())
}

/// Revalidate the exact financial owner immediately before live process
/// release. This is deliberately separate from issuing the operation: an
/// idempotent historical issue result is not a current release capability.
pub fn authorize_process_resource_release(
    state: &crate::state::AppState,
    bindings: &[ryeos_accounting::ResourceOperationBinding],
) -> anyhow::Result<()> {
    if bindings.is_empty() {
        return Ok(());
    }
    state
        .accounting
        .as_ref()
        .context("resource-owner release requires the shared accounting ledger")?
        .authorize_resource_owner_release(bindings, lillux::time::timestamp_millis())
}

pub fn release_unissued_process_resource_operations(
    state: &crate::state::AppState,
    bindings: &[ryeos_accounting::ResourceOperationBinding],
) -> anyhow::Result<()> {
    if bindings.is_empty() {
        return Ok(());
    }
    let accounting = state
        .accounting
        .as_ref()
        .context("resource operation release requires the shared accounting ledger")?;
    for binding in bindings {
        accounting.release_unissued_resource_operation(
            binding.operation_id.as_str(),
            binding.request_digest.as_str(),
            lillux::time::timestamp_millis(),
        )?;
    }
    Ok(())
}

pub fn abandon_prepared_process_resource_operations(
    state: &crate::state::AppState,
    bindings: &[ryeos_accounting::ResourceOperationBinding],
) -> anyhow::Result<()> {
    if bindings.is_empty() {
        return Ok(());
    }
    let owner_gate_id = &bindings[0].owner_gate_id;
    let owner_incarnation = &bindings[0].owner_incarnation;
    if bindings.iter().any(|binding| {
        binding.owner_gate_id != *owner_gate_id || binding.owner_incarnation != *owner_incarnation
    }) {
        bail!("prepared resource operations use multiple owner identities");
    }
    state
        .accounting
        .as_ref()
        .context("prepared resource cleanup requires the shared accounting ledger")?
        .release_unattached_resource_owner_gate(
            owner_gate_id.as_str(),
            owner_incarnation.as_str(),
            lillux::time::timestamp_millis(),
        )
}

/// Reconcile every resource operation after the exact process occurrence has
/// been proved cleaned. Reserved operations return their hold; issued ones
/// retain complete or explicitly partial Lillux occupancy evidence.
pub fn settle_process_resource_operations_after_cleanup(
    state: &crate::state::AppState,
    identity: &crate::process::ExecutionProcessIdentity,
    evidence: &crate::runtime_db::ProcessResourceCleanupEvidence,
) -> anyhow::Result<()> {
    settle_process_resource_operations_with_accounting(
        state.accounting.as_deref(),
        identity,
        evidence,
    )
}

pub fn settle_process_resource_operations_with_accounting(
    accounting: Option<&crate::accounting_db::AccountingDb>,
    identity: &crate::process::ExecutionProcessIdentity,
    evidence: &crate::runtime_db::ProcessResourceCleanupEvidence,
) -> anyhow::Result<()> {
    if identity.resource_operations.is_empty() {
        return Ok(());
    }
    let accounting =
        accounting.context("resource cleanup settlement requires the shared accounting ledger")?;
    let owner_gate_id = &identity.resource_operations[0].owner_gate_id;
    if identity
        .resource_operations
        .iter()
        .any(|binding| binding.owner_gate_id != *owner_gate_id)
    {
        bail!("resource-bearing process has multiple owner accounting gates");
    }
    let owner_incarnation = &identity.resource_operations[0].owner_incarnation;
    let start = identity
        .resource_occupancy_start
        .as_ref()
        .context("resource-bearing process lacks retained occupancy start")?;
    let end = evidence.occupancy_end.as_ref();
    let states = identity
        .resource_operations
        .iter()
        .map(|binding| accounting.resource_operation_state(binding.operation_id.as_str()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let retains_unknown_advisory_liability = end.is_none()
        && states
            .iter()
            .any(|state| *state == ryeos_accounting::ResourceBudgetState::AdvisoryIssued);
    accounting.mark_resource_owner_cleanup_proved(
        owner_gate_id.as_str(),
        owner_incarnation.as_str(),
        !retains_unknown_advisory_liability,
        lillux::time::timestamp_millis(),
    )?;
    for (binding, state) in identity.resource_operations.iter().zip(states) {
        match state {
            ryeos_accounting::ResourceBudgetState::Reserved
            | ryeos_accounting::ResourceBudgetState::AdvisoryPending => {
                accounting.release_unissued_resource_operation(
                    binding.operation_id.as_str(),
                    binding.request_digest.as_str(),
                    lillux::time::timestamp_millis(),
                )?;
            }
            ryeos_accounting::ResourceBudgetState::Issued
            | ryeos_accounting::ResourceBudgetState::AdvisoryIssued
            | ryeos_accounting::ResourceBudgetState::ChargedReservedMaximum => {
                let (coverage, intervals, incarnation) = match end {
                    Some(end) if end.elapsed_nanoseconds_since(start).is_ok() => (
                        ryeos_accounting::ResourceUsageCoverage::Complete,
                        vec![ryeos_accounting::ResourceUsageInterval {
                            start_tick_ns: start.tick_ns,
                            end_tick_ns: end.tick_ns,
                        }],
                        end.incarnation_digest.clone(),
                    ),
                    _ => (
                        ryeos_accounting::ResourceUsageCoverage::Partial,
                        Vec::new(),
                        start.incarnation_digest.clone(),
                    ),
                };
                let usage = ryeos_accounting::ResourceUsageObservation {
                    version: ryeos_accounting::RESOURCE_USAGE_OBSERVATION_VERSION,
                    operation_id: binding.operation_id.as_str().to_owned(),
                    owner_incarnation: binding.owner_incarnation.as_str().to_owned(),
                    stable_resource_id: binding.stable_resource_id.clone(),
                    clock_incarnation_digest: ryeos_accounting::HexDigest::new(incarnation)
                        .map_err(anyhow::Error::msg)?,
                    meter_contract_digest: binding.meter_contract_digest.clone(),
                    clock_contract_digest: binding.clock_contract_digest.clone(),
                    coverage,
                    intervals,
                };
                accounting.settle_resource_operation(
                    binding.operation_id.as_str(),
                    &usage,
                    lillux::time::timestamp_millis(),
                )?;
            }
            ryeos_accounting::ResourceBudgetState::ReservationDenied
            | ryeos_accounting::ResourceBudgetState::ReleasedUnissued
            | ryeos_accounting::ResourceBudgetState::AdvisoryReleasedUnissued
            | ryeos_accounting::ResourceBudgetState::Reconciled
            | ryeos_accounting::ResourceBudgetState::ReservationBoundViolated
            | ryeos_accounting::ResourceBudgetState::AdvisoryReconciled => {}
        }
    }
    if retains_unknown_advisory_liability {
        Ok(())
    } else {
        accounting.fence_resource_owner_accounting_gate(
            owner_gate_id.as_str(),
            "owner_cleaned",
            lillux::time::timestamp_millis(),
        )
    }
}

fn resource_matches(
    descriptor: &NodeExecutionResourceDescriptor,
    requirement: &ExecutionResourceRequirement,
) -> bool {
    if descriptor.class != requirement.class {
        return false;
    }
    let exact = requirement.exact_facts.iter().all(|(key, expected)| {
        descriptor.facts.get(key) == Some(&ExecutionResourceFactValue::Text(expected.clone()))
    });
    let minimum = requirement.minimum_facts.iter().all(|(key, expected)| {
        matches!(
            descriptor.facts.get(key),
            Some(ExecutionResourceFactValue::Integer(actual)) if actual >= expected
        )
    });
    exact && minimum
}

fn matched_facts(
    descriptor: &NodeExecutionResourceDescriptor,
    requirement: &ExecutionResourceRequirement,
) -> BTreeMap<String, ExecutionResourceFactValue> {
    requirement
        .exact_facts
        .keys()
        .chain(requirement.minimum_facts.keys())
        .filter_map(|key| {
            descriptor
                .facts
                .get(key)
                .cloned()
                .map(|value| (key.clone(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::contracts::{ExecutionResourceAllocation, ExecutionTargetLimits};

    fn descriptor() -> NodeExecutionResourceDescriptor {
        let facts = BTreeMap::new();
        let character_devices = vec![lillux::CharacterDeviceSpec {
            role: "assignment".to_owned(),
            source: "/dev/null".into(),
            destination: "/dev/assigned-resource".into(),
            access: lillux::CharacterDeviceAccess::ReadWrite,
        }];
        let observed = lillux::CharacterDeviceSet::observe(&character_devices).unwrap();
        let observation_contract_digest = lillux::resource_observation_contract_digest(
            "device-a",
            "accelerator",
            &facts,
            observed.identities(),
        )
        .unwrap();
        let clock = lillux::time::OccupancyClockContract::current().unwrap();
        let meter = ryeos_accounting::ResourceMeterContract {
            version: ryeos_accounting::RESOURCE_METER_CONTRACT_VERSION,
            kind: ryeos_accounting::ResourceMeterKind::OccupancyDuration,
            clock_contract_digest: ryeos_accounting::HexDigest::new(clock.contract_digest).unwrap(),
            contract_digest: ryeos_accounting::HexDigest::new("0".repeat(64)).unwrap(),
        }
        .sealed()
        .unwrap();
        let tariff = ryeos_accounting::ResourceTariffDocument {
            version: ryeos_accounting::RESOURCE_TARIFF_VERSION,
            currency: ryeos_accounting::Currency::Usd,
            pricing_generation: "test".to_string(),
            charge_class: ryeos_accounting::ResourceChargeClass::InternalAllocation,
            rate_per_million_milliseconds: ryeos_accounting::UsdNanos::from_nanos(1).unwrap(),
            billing_quantum_milliseconds: 1,
            minimum_billable_milliseconds: 0,
            expires_at_ms: None,
        };
        let authority = ryeos_accounting::ResourceAccountingAuthority {
            version: ryeos_accounting::RESOURCE_ACCOUNTING_AUTHORITY_VERSION,
            authority_digest: ryeos_accounting::HexDigest::new("0".repeat(64)).unwrap(),
            stable_resource_id: "device-a".to_string(),
            resource_class: "accelerator".to_string(),
            observation_contract_digest: ryeos_accounting::HexDigest::new(
                observation_contract_digest.clone(),
            )
            .unwrap(),
            meter,
            tariff,
            spend: ryeos_accounting::ResourceSpendAuthority::Advisory,
        }
        .sealed()
        .unwrap();
        NodeExecutionResourceDescriptor {
            stable_id: "device-a".to_string(),
            class: "accelerator".to_string(),
            observation_contract_digest,
            facts,
            character_devices,
            accounting: Some(authority),
        }
    }

    fn policy() -> NodeExecutionResourcePolicy {
        NodeExecutionResourcePolicy {
            admission: ryeos_engine::contracts::ExecutionResourceAdmissionPolicy {
                limits: ExecutionTargetLimits {
                    max_requirements: 1,
                    max_resource_count: 1,
                    max_facts_per_requirement: 4,
                },
                max_total_resource_count: 1,
                max_concurrent_exclusive_allocations: 1,
                allowed_classes: vec!["accelerator".to_string()],
                allowed_allocations: vec![ExecutionResourceAllocation::Exclusive],
                allowed_access: vec![ExecutionResourceAccess::DeploymentVisible],
            },
            cleanup_allowance_ms: 1_000,
            resources: vec![descriptor()],
        }
    }

    #[test]
    fn selection_retains_lillux_observed_assignment() {
        let pool = ExecutionResourcePool::observe(Some(&policy())).unwrap();
        let target = ExecutionTargetRequirement {
            os: lillux::platform::current_target().os.to_string(),
            arch: lillux::platform::current_target().arch.to_string(),
            resources: vec![ExecutionResourceRequirement {
                class: "accelerator".to_string(),
                count: 1,
                allocation: ExecutionResourceAllocation::Exclusive,
                access: ExecutionResourceAccess::DeploymentVisible,
                exact_facts: BTreeMap::new(),
                minimum_facts: BTreeMap::new(),
            }],
        };
        let selected = pool.select(Some(&target)).unwrap();
        assert_eq!(selected.selections().len(), 1);
        assert!(selected.selections()[0].matched_facts.is_empty());
        assert_eq!(selected.selections()[0].stable_id, "device-a");
        assert_ne!(
            selected.selections()[0].device_binding_digest,
            "0".repeat(64)
        );
    }

    #[test]
    fn restricted_selection_uses_observed_descriptor_authority() {
        let pool = ExecutionResourcePool::observe(Some(&policy())).unwrap();
        let target = ExecutionTargetRequirement {
            os: lillux::platform::current_target().os.to_string(),
            arch: lillux::platform::current_target().arch.to_string(),
            resources: vec![ExecutionResourceRequirement {
                class: "accelerator".to_string(),
                count: 1,
                allocation: ExecutionResourceAllocation::Exclusive,
                access: ExecutionResourceAccess::ExecutionRestricted,
                exact_facts: BTreeMap::new(),
                minimum_facts: BTreeMap::new(),
            }],
        };
        let selected = pool.select(Some(&target)).unwrap();
        assert_eq!(selected.selections()[0].character_devices.len(), 1);
        assert_eq!(
            selected.selections()[0].enforcement,
            ExecutionResourceEnforcement::CharacterDeviceGrant
        );
    }

    #[test]
    fn authored_semantic_facts_are_not_treated_as_observation() {
        let mut policy = policy();
        policy.resources[0].facts.insert(
            "memory_bytes".to_owned(),
            ExecutionResourceFactValue::Integer(96),
        );
        assert!(ExecutionResourcePool::observe(Some(&policy)).is_err());
    }

    #[test]
    fn finite_occupancy_reserves_a_strict_cleanup_interval() {
        let mut policy = policy();
        let authority = policy.resources[0].accounting.as_mut().unwrap();
        let maximum_occupancy_milliseconds = policy.cleanup_allowance_ms;
        authority.spend = ryeos_accounting::ResourceSpendAuthority::Bounded {
            maximum_occupancy_milliseconds,
            maximum: authority
                .tariff
                .charge_for_nanoseconds(maximum_occupancy_milliseconds * 1_000_000)
                .unwrap(),
        };
        authority.authority_digest = authority.compute_digest().unwrap();
        assert!(policy.validate().is_err());

        let authority = policy.resources[0].accounting.as_mut().unwrap();
        let maximum_occupancy_milliseconds = policy.cleanup_allowance_ms + 1;
        authority.spend = ryeos_accounting::ResourceSpendAuthority::Bounded {
            maximum_occupancy_milliseconds,
            maximum: authority
                .tariff
                .charge_for_nanoseconds(maximum_occupancy_milliseconds * 1_000_000)
                .unwrap(),
        };
        authority.authority_digest = authority.compute_digest().unwrap();
        policy.validate().unwrap();
    }
}
