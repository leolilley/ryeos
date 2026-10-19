//! The native envelope an external candidate supervisor runs its journals
//! on. The node signs it as the `external-supervisor-journal` offline native
//! class of its maintenance policy and seals the checked quota into the
//! occurrence's bootstrap, so the supervisor funds its in-process native host
//! from node-signed data rather than from a value of its own choosing.

use anyhow::{Context, Result, ensure};
use ryeos_node_runtime::node_policy::NodePolicySnapshot;
use ryeos_scheduler_contract::{NodeMaintenancePolicy, OfflineNativeQuota};
use ryeos_state::external_execution::transport::ExternalSupervisorNativeEnvelope;
use std::sync::Arc;

use crate::node_config::sections::external_execution::ExternalPlacementBackendContract;

/// Offline native class funding an external supervisor's journals.
pub(crate) const EXTERNAL_SUPERVISOR_JOURNAL_CLASS: &str = "external-supervisor-journal";

/// The longest life of one supervisor occurrence under `contract`, in
/// milliseconds: allocation contact plus attachment observation, then the
/// execution timeout, then post-execution observation plus cleanup.
pub(crate) fn external_supervisor_occurrence_lifetime_ms(
    contract: &ExternalPlacementBackendContract,
    execution_timeout_seconds: u32,
) -> Result<u64> {
    u64::from(contract.contact_timeout_seconds)
        .checked_add(u64::from(contract.observation_timeout_seconds))
        .and_then(|seconds| seconds.checked_add(u64::from(execution_timeout_seconds)))
        .and_then(|seconds| seconds.checked_add(u64::from(contract.observation_timeout_seconds)))
        .and_then(|seconds| seconds.checked_add(u64::from(contract.cleanup_timeout_seconds)))
        .and_then(|seconds| seconds.checked_mul(1_000))
        .context("external supervisor occurrence lifetime overflow")
}

/// The signed class quota, checked against the native host's own bounds and
/// required to cover the occurrence's whole lifetime in its operation window.
pub(crate) fn external_supervisor_journal_quota(
    policy: &NodePolicySnapshot,
    contract: &ExternalPlacementBackendContract,
    execution_timeout_seconds: u32,
) -> Result<OfflineNativeQuota> {
    let quota = policy
        .require::<NodeMaintenancePolicy>()?
        .offline_native
        .get(EXTERNAL_SUPERVISOR_JOURNAL_CLASS)
        .with_context(|| {
            format!(
                "node maintenance policy grants no offline native quota for `{EXTERNAL_SUPERVISOR_JOURNAL_CLASS}`"
            )
        })?
        .clone();
    ryeos_node_runtime::node_policy::sections::maintenance::validate_offline_native_quota(&quota)
        .with_context(|| format!("offline native `{EXTERNAL_SUPERVISOR_JOURNAL_CLASS}` quota"))?;
    let lifetime = external_supervisor_occurrence_lifetime_ms(contract, execution_timeout_seconds)?;
    ensure!(
        quota.max_operation_millis >= lifetime,
        "offline native `{EXTERNAL_SUPERVISOR_JOURNAL_CLASS}` operation window of {} ms is shorter than the occurrence lifetime of {lifetime} ms",
        quota.max_operation_millis
    );
    Ok(quota)
}

/// Whether a startup still funds supervisor journals from the node's current
/// signed policy. A retained supervisor activation already sealed the envelope
/// it was funded with into its bootstrap; recovering it must not re-read a
/// policy that may have changed since, or a later policy edit would strand
/// that recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SupervisorJournalFunding {
    /// No supervisor activation is retained yet: the current policy must fund
    /// the occurrence's whole lifetime before any provider contact.
    New,
    /// A supervisor activation is retained with its sealed envelope.
    Retained,
}

impl SupervisorJournalFunding {
    /// Refuse a new occurrence the current signed policy cannot fund.
    pub(crate) fn require(
        self,
        policy: &NodePolicySnapshot,
        contract: &ExternalPlacementBackendContract,
        execution_timeout_seconds: u32,
    ) -> Result<()> {
        match self {
            Self::New => {
                external_supervisor_journal_quota(policy, contract, execution_timeout_seconds)?;
                Ok(())
            }
            Self::Retained => Ok(()),
        }
    }
}

/// Where a placement takes its supervisor journal envelope from when it
/// first activates a supervisor: the node's current signed policy, or a fixed
/// envelope in tests.
#[derive(Clone)]
pub(crate) enum SupervisorJournalEnvelopeSource {
    Policy(Arc<NodePolicySnapshot>),
    #[cfg(test)]
    Fixed(ExternalSupervisorNativeEnvelope),
}

impl SupervisorJournalEnvelopeSource {
    /// The envelope sealed into a supervisor bootstrap under `contract`.
    pub(crate) fn envelope(
        &self,
        contract: &ExternalPlacementBackendContract,
        execution_timeout_seconds: u32,
    ) -> Result<ExternalSupervisorNativeEnvelope> {
        match self {
            Self::Policy(policy) => {
                let quota =
                    external_supervisor_journal_quota(policy, contract, execution_timeout_seconds)?;
                Ok(ExternalSupervisorNativeEnvelope {
                    storage_bytes: quota.max_native_storage_bytes,
                    allocator_steps: quota.max_native_allocator_steps,
                    connections: quota.max_native_connections,
                    operation_millis: quota.max_operation_millis,
                    cleanup_millis: quota.max_cleanup_millis,
                    cleanup_steps: quota.max_cleanup_steps,
                })
            }
            #[cfg(test)]
            Self::Fixed(envelope) => Ok(*envelope),
        }
    }
}

/// A fixed envelope for tests: the signed profiles' values.
#[cfg(test)]
pub(crate) fn test_supervisor_journal_envelope() -> ExternalSupervisorNativeEnvelope {
    ExternalSupervisorNativeEnvelope {
        storage_bytes: 67_108_864,
        allocator_steps: 2_000_000_000,
        connections: 16,
        operation_millis: 4_860_000,
        cleanup_millis: 120_000,
        cleanup_steps: 3_000_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetime_covers_every_signed_phase_and_peaks_at_the_contract_bounds() {
        let mut contract =
            crate::node_config::sections::external_execution::InstalledExternalExecutionBinding::test_fixture()
                .backend_contract();
        contract.contact_timeout_seconds = 60;
        contract.observation_timeout_seconds = 300;
        contract.cleanup_timeout_seconds = 600;
        assert_eq!(
            external_supervisor_occurrence_lifetime_ms(&contract, 3_600).unwrap(),
            4_860_000
        );
        contract.contact_timeout_seconds = 1;
        contract.observation_timeout_seconds = 2;
        contract.cleanup_timeout_seconds = 3;
        assert_eq!(
            external_supervisor_occurrence_lifetime_ms(&contract, 4).unwrap(),
            12_000
        );
    }
}
