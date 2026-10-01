//! Explicit launch intent in the existing resource reservation transaction.
//! A reservation is never a relaunch permit. Only the exact winning transition
//! from reserved to spawn-intent mints a one-use trusted held-spawn permit.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::{ProcessResourceReservationRecord, RuntimeDb, daemon_generation_id};
use crate::process::{ExecutionProcessIdentity, ProcessResourceSettlementAuthority};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustedResourceLaunchPhase {
    Reserved,
    SpawnIntent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessResourceLaunchAuthority {
    LocalProcessScope {
        allocation: lillux::ProcessScopeAllocation,
        #[serde(deserialize_with = "serde::Deserialize::deserialize")]
        recovery: Option<lillux::ProcessScopeRecovery>,
    },
    TrustedProcessGroup {
        cleanup_contract_digest: String,
        host_lifetime: lillux::ProcessHostLifetime,
        phase: TrustedResourceLaunchPhase,
    },
}

impl ProcessResourceLaunchAuthority {
    pub fn validate(&self) -> Result<()> {
        self.settlement_authority().validate()?;
        match self {
            Self::LocalProcessScope {
                allocation,
                recovery,
            } => {
                allocation.validate().map_err(anyhow::Error::msg)?;
                if recovery
                    .as_ref()
                    .is_some_and(|recovery| !recovery.matches_allocation(allocation))
                {
                    bail!("resource scope recovery contradicts its exact allocation");
                }
            }
            Self::TrustedProcessGroup { host_lifetime, .. } => {
                host_lifetime.validate().map_err(anyhow::Error::msg)?;
            }
        }
        Ok(())
    }

    pub fn settlement_authority(&self) -> ProcessResourceSettlementAuthority {
        match self {
            Self::LocalProcessScope { .. } => {
                ProcessResourceSettlementAuthority::LocalProcessScope {}
            }
            Self::TrustedProcessGroup {
                cleanup_contract_digest,
                ..
            } => ProcessResourceSettlementAuthority::TrustedProcessGroup {
                cleanup_contract_digest: cleanup_contract_digest.clone(),
            },
        }
    }

    pub fn host_lifetime(&self) -> Result<lillux::ProcessHostLifetime> {
        match self {
            Self::LocalProcessScope { allocation, .. } => {
                allocation.host_lifetime().map_err(anyhow::Error::msg)
            }
            Self::TrustedProcessGroup { host_lifetime, .. } => Ok(host_lifetime.clone()),
        }
    }

    /// Normalize only the two permitted one-shot transitions for the existing
    /// reservation primary key. The selected cleanup contract remains identity.
    pub(super) fn allocation_identity(&self) -> Self {
        match self {
            Self::LocalProcessScope { allocation, .. } => Self::LocalProcessScope {
                allocation: allocation.clone(),
                recovery: None,
            },
            Self::TrustedProcessGroup {
                cleanup_contract_digest,
                host_lifetime,
                ..
            } => Self::TrustedProcessGroup {
                cleanup_contract_digest: cleanup_contract_digest.clone(),
                host_lifetime: host_lifetime.clone(),
                phase: TrustedResourceLaunchPhase::Reserved,
            },
        }
    }

    pub(super) fn matches_held_process(&self, identity: &ExecutionProcessIdentity) -> Result<bool> {
        if identity.resource_settlement_authority.as_ref() != Some(&self.settlement_authority()) {
            return Ok(false);
        }
        match self {
            Self::LocalProcessScope { recovery, .. } => {
                Ok(recovery.is_some() && recovery.as_ref() == identity.process_scope.as_ref())
            }
            Self::TrustedProcessGroup {
                host_lifetime,
                phase,
                ..
            } => Ok(*phase == TrustedResourceLaunchPhase::SpawnIntent
                && identity.process_scope.is_none()
                && identity.belongs_to_host_lifetime(host_lifetime)?
                && *host_lifetime
                    == lillux::ProcessHostLifetime::capture_current()
                        .map_err(anyhow::Error::msg)?),
        }
    }

    pub fn local_scope_recovery(&self) -> Option<&lillux::ProcessScopeRecovery> {
        match self {
            Self::LocalProcessScope { recovery, .. } => recovery.as_ref(),
            Self::TrustedProcessGroup { .. } => None,
        }
    }

    pub fn bind_local_scope(&mut self, bound: &lillux::ProcessScopeRecovery) -> Result<()> {
        let Self::LocalProcessScope {
            allocation,
            recovery,
        } = self
        else {
            bail!("trusted resource reservation cannot bind a local process scope");
        };
        if recovery.is_some() || !bound.matches_allocation(allocation) {
            bail!("resource scope binding lost its exact unbound allocation");
        }
        *recovery = Some(bound.clone());
        Ok(())
    }
}

/// Cannot be cloned, serialized, reconstructed from an intent row or minted
/// from an acknowledgement. Dropping it leaves the intent quarantined.
pub struct TrustedResourceSpawnPermit {
    reservation: ProcessResourceReservationRecord,
}

impl TrustedResourceSpawnPermit {
    pub fn reservation(&self) -> &ProcessResourceReservationRecord {
        &self.reservation
    }

    pub fn into_reservation(self) -> ProcessResourceReservationRecord {
        self.reservation
    }
}

impl RuntimeDb {
    /// Journal contact intent before a trusted held spawn. A lost commit ACK
    /// cannot mint another permit: repeat/stale generations refuse. The caller
    /// must first admit node AND exact signed product cleanup authority; this
    /// method does not admit a product or authorize process release.
    pub fn begin_trusted_process_resource_spawn(
        &self,
        expected: &ProcessResourceReservationRecord,
    ) -> Result<TrustedResourceSpawnPermit> {
        expected.validate()?;
        if expected.daemon_generation_id != daemon_generation_id() {
            bail!("trusted held-spawn intent belongs to another daemon generation");
        }
        let mut intent = expected.clone();
        let ProcessResourceLaunchAuthority::TrustedProcessGroup {
            host_lifetime,
            phase,
            ..
        } = &mut intent.launch_authority
        else {
            bail!("trusted held spawn requires an explicitly trusted reservation");
        };
        if *phase != TrustedResourceLaunchPhase::Reserved
            || *host_lifetime
                != lillux::ProcessHostLifetime::capture_current().map_err(anyhow::Error::msg)?
        {
            bail!("trusted held-spawn intent is not reserved in this host occurrence");
        }
        *phase = TrustedResourceLaunchPhase::SpawnIntent;
        let before = lillux::canonical_json(&serde_json::to_value(expected)?)?;
        let after = lillux::canonical_json(&serde_json::to_value(&intent)?)?;
        let changed = self.conn.execute(
            "UPDATE process_resource_reservation SET reservation=?4, updated_at_ms=?5
              WHERE owner_kind=?1 AND owner_coordinate=?2 AND reservation=?3",
            rusqlite::params![
                expected.owner_kind,
                expected.owner_coordinate,
                before,
                after,
                i64::try_from(lillux::time::timestamp_millis())?
            ],
        )?;
        if changed != 1 {
            bail!("trusted held-spawn intent was not committed exactly once");
        }
        Ok(TrustedResourceSpawnPermit {
            reservation: intent,
        })
    }
}
