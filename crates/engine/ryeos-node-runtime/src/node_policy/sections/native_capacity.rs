//! Explicit authority to issue a finite local native-capacity allowance.
//!
//! This is not a currency account, an execution resource tariff or a factory
//! receipt. The admission producer must verify the installed signed generation,
//! record the allowance once, and durably debit the same account before any
//! native allocation. Parsing this policy performs none of those operations.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "native_capacity";
const NATIVE_CONTROL_STORAGE_BYTES: u64 = 65_536;
const MAX_CONNECTIONS_PER_INCARNATION: u64 = 64;

/// Nonfinancial units. In particular, storage or work cannot satisfy USD spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCapacityUnits {
    pub storage_bytes: u64,
    /// Separate allowance for actual host owners/allocator, never native arena credit.
    pub host_control_storage_bytes: u64,
    pub allocator_steps: u64,
    pub connection_tickets: u64,
    pub cleanup_steps: u64,
}

/// Fixed-size refusal usable before a host allocator has been admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCapacityUnitsRefusal {
    Storage,
    HostControlStorage,
    Work,
    Tickets,
    Cleanup,
    CleanupConsumesWork,
}
impl std::fmt::Display for NativeCapacityUnitsRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Storage => "invalid native storage allowance",
            Self::HostControlStorage => "invalid host control storage allowance",
            Self::Work => "invalid native work allowance",
            Self::Tickets => "invalid native ticket allowance",
            Self::Cleanup => "invalid native cleanup allowance",
            Self::CleanupConsumesWork => "native cleanup consumes work allowance",
        })
    }
}
impl std::error::Error for NativeCapacityUnitsRefusal {}

impl NativeCapacityUnits {
    pub fn validate(&self) -> std::result::Result<(), NativeCapacityUnitsRefusal> {
        for (value, refusal) in [
            (self.storage_bytes, NativeCapacityUnitsRefusal::Storage),
            (self.host_control_storage_bytes, NativeCapacityUnitsRefusal::HostControlStorage),
            (self.allocator_steps, NativeCapacityUnitsRefusal::Work),
            (self.connection_tickets, NativeCapacityUnitsRefusal::Tickets),
            (self.cleanup_steps, NativeCapacityUnitsRefusal::Cleanup),
        ] {
            if value == 0 || value > i64::MAX as u64 {
                return Err(refusal);
            }
        }
        if self.cleanup_steps >= self.allocator_steps {
            return Err(NativeCapacityUnitsRefusal::CleanupConsumesWork);
        }
        Ok(())
    }

    pub fn covers(&self, other: &Self) -> bool {
        self.storage_bytes >= other.storage_bytes
            && self.host_control_storage_bytes >= other.host_control_storage_bytes
            && self.allocator_steps >= other.allocator_steps
            && self.connection_tickets >= other.connection_tickets
            && self.cleanup_steps >= other.cleanup_steps
    }
}

/// A bounded request envelope for one actual process incarnation. Its clock is
/// sampled once by the producer before reservation, never reconstructed at Ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeIncarnationEnvelope {
    pub units: NativeCapacityUnits,
    pub maximum_occupancy_ms: u64,
    pub cleanup_occupancy_ms: u64,
    /// How long before the service-window edge the incarnation begins its
    /// planned graceful stop, while service SQL is still admitted. Required:
    /// greater than zero and shorter than the service window.
    pub rollover_drain_margin_ms: u64,
    /// Original cleanup steps transferred to the outside mapping owner.
    /// Bounds syscall attempts, not a promise that cleanup always succeeds.
    pub host_release_steps: u64,
    /// Original cleanup work retained outside the resident/native graph.
    /// Shares units.cleanup_steps with native work and host release.
    pub outside_cleanup_steps: u64,
}

/// Finite control authority for reconciling interrupted capacity records.
/// Separate from native incarnation credit: recovery never maps a new region
/// or refunds the predecessor's consumption. The account durably burns one
/// attempt before repair and retains its original clock/counters on retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRecoveryEnvelope {
    pub maximum_attempts: u64,
    pub work_per_attempt: u64,
    pub control_storage_bytes: u64,
    pub maximum_duration_ms: u64,
}

impl NativeRecoveryEnvelope {
    pub fn validate(&self) -> std::result::Result<(), NativeCapacityPolicyRefusal> {
        // Structural bounds only. The real account producer checks its actual
        // record/namespace/publication/descriptor quote before any mutation.
        for value in [self.maximum_attempts, self.work_per_attempt, self.control_storage_bytes] {
            if value == 0 || value > i64::MAX as u64 {
                return Err(NativeCapacityPolicyRefusal::Recovery);
            }
        }
        if self.maximum_duration_ms == 0
            || self.maximum_duration_ms.checked_mul(1_000_000).is_none() {
            return Err(NativeCapacityPolicyRefusal::Recovery);
        }
        if self.maximum_attempts.checked_mul(self.work_per_attempt).is_none()
            || self.maximum_attempts.checked_mul(self.control_storage_bytes).is_none() {
            return Err(NativeCapacityPolicyRefusal::Recovery);
        }
        Ok(())
    }
}

/// Fixed refusals retained by the pre-allocation admission owner. These
/// validate shape only; a parsed policy is not authentication or funding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCapacityPolicyRefusal {
    Schema,
    IssuanceNonce,
    Units(NativeCapacityUnitsRefusal),
    ControlStorage,
    Tickets,
    OccupancyInterval,
    OccupancyOverflow,
    Allowance,
    Recovery,
    RolloverMargin,
}
impl std::fmt::Display for NativeCapacityPolicyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Units(error) => std::fmt::Display::fmt(error, f),
            _ => f.write_str(match self {
                Self::Schema => "native capacity policy schema is not current",
                Self::IssuanceNonce => "native capacity issuance nonce is not canonical lowercase hex",
                Self::ControlStorage => "native storage omits control storage",
                Self::Tickets => "native ticket envelope exceeds the supported factory",
                Self::OccupancyInterval => "native service/cleanup interval is invalid",
                Self::OccupancyOverflow => "native occupancy overflows nanoseconds",
                Self::Allowance => "native account allowance cannot fund the incarnation",
                Self::Recovery => "capacity recovery allowance is invalid or overflows",
                Self::RolloverMargin => "native rollover drain margin must be positive and shorter than the service window",
                Self::Units(_) => unreachable!(),
            }),
        }
    }
}
impl std::error::Error for NativeCapacityPolicyRefusal {}
impl From<NativeCapacityUnitsRefusal> for NativeCapacityPolicyRefusal {
    fn from(error: NativeCapacityUnitsRefusal) -> Self { Self::Units(error) }
}
impl NativeIncarnationEnvelope {
    pub fn validate(&self) -> std::result::Result<(), NativeCapacityPolicyRefusal> {
        self.units.validate()?;
        if self.units.storage_bytes <= NATIVE_CONTROL_STORAGE_BYTES {
            return Err(NativeCapacityPolicyRefusal::ControlStorage);
        }
        if self.units.connection_tickets > MAX_CONNECTIONS_PER_INCARNATION {
            return Err(NativeCapacityPolicyRefusal::Tickets);
        }
        if self.host_release_steps < 2 || self.outside_cleanup_steps == 0
            || self.host_release_steps.checked_add(self.outside_cleanup_steps)
                .is_none_or(|outside| outside >= self.units.cleanup_steps) {
            return Err(NativeCapacityPolicyRefusal::Units(NativeCapacityUnitsRefusal::Cleanup));
        }
        if self.cleanup_occupancy_ms == 0 || self.cleanup_occupancy_ms >= self.maximum_occupancy_ms {
            return Err(NativeCapacityPolicyRefusal::OccupancyInterval);
        }
        if self.maximum_occupancy_ms.checked_mul(1_000_000).is_none() {
            return Err(NativeCapacityPolicyRefusal::OccupancyOverflow);
        }
        if self.rollover_drain_margin_ms == 0
            || self.rollover_drain_margin_ms >= self.maximum_occupancy_ms - self.cleanup_occupancy_ms {
            return Err(NativeCapacityPolicyRefusal::RolloverMargin);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeNativeCapacityPolicy {
    pub schema: u32,
    /// Explicit issuance nonce, scoped to the actual node signer and root.
    /// Policy refresh cannot mint another copy of this allowance. Changing its
    /// dimensions under the same nonce must refuse, even after completion.
    pub issuance_nonce: String,
    pub allowance: NativeCapacityUnits,
    pub incarnation: NativeIncarnationEnvelope,
    pub recovery: NativeRecoveryEnvelope,
}

impl NodeNativeCapacityPolicy {
    pub fn validate(&self) -> std::result::Result<(), NativeCapacityPolicyRefusal> {
        if self.schema != 5 { return Err(NativeCapacityPolicyRefusal::Schema); }
        if self.issuance_nonce.len() != 32 || !self.issuance_nonce.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(NativeCapacityPolicyRefusal::IssuanceNonce);
        }
        self.allowance.validate()?;
        self.incarnation.validate()?;
        self.recovery.validate()?;
        if !self.allowance.covers(&self.incarnation.units) {
            return Err(NativeCapacityPolicyRefusal::Allowance);
        }
        Ok(())
    }
}

impl TypedNodePolicy for NodeNativeCapacityPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct NodeNativeCapacityPolicySection;

impl NodePolicySection for NodeNativeCapacityPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> Result<Arc<dyn ErasedNodePolicy>> {
        let policy: NodeNativeCapacityPolicy = serde_json::from_value(body.clone())
            .context("parse explicit node native-capacity authority")?;
        policy.validate()?;
        Ok(Arc::new(policy))
    }
}

#[cfg(test)]
mod outside_cleanup_tests {
    use super::*;

    fn policy() -> NodeNativeCapacityPolicy {
        let units = NativeCapacityUnits {
            storage_bytes: 67_108_864, host_control_storage_bytes: 1_048_576,
            allocator_steps: 20_000_000, connection_tickets: 64, cleanup_steps: 300_000,
        };
        NodeNativeCapacityPolicy {
            schema: 5, issuance_nonce: "a".repeat(32), allowance: units,
            incarnation: NativeIncarnationEnvelope {
                units, maximum_occupancy_ms: 7_200_000, cleanup_occupancy_ms: 120_000,
                rollover_drain_margin_ms: 90_000,
                host_release_steps: 64, outside_cleanup_steps: 32_768,
            },
            recovery: NativeRecoveryEnvelope {
                maximum_attempts: 32, work_per_attempt: 65_536,
                control_storage_bytes: 262_144, maximum_duration_ms: 120_000,
            },
        }
    }

    #[test]
    fn explicit_outside_split_preserves_native_work_and_refuses_exhausted_total() {
        let mut policy = policy();
        policy.validate().unwrap();
        policy.incarnation.outside_cleanup_steps = 0;
        assert!(policy.validate().is_err());
        policy.incarnation.outside_cleanup_steps = policy.incarnation.units.cleanup_steps - 64;
        assert!(policy.validate().is_err());
        policy.incarnation.outside_cleanup_steps = u64::MAX;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn outside_split_is_required_and_previous_schema_is_refused() {
        let mut policy = policy();
        let mut encoded = serde_json::to_value(&policy).unwrap();
        encoded["incarnation"].as_object_mut().unwrap().remove("outside_cleanup_steps");
        assert!(serde_json::from_value::<NodeNativeCapacityPolicy>(encoded).is_err());
        policy.schema = 4;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn recovery_is_explicit_finite_and_independent_of_exhausted_native_credit() {
        let mut policy = policy();
        policy.validate().unwrap();
        assert_eq!(policy.allowance, policy.incarnation.units);
        let mut encoded = serde_json::to_value(&policy).unwrap();
        encoded.as_object_mut().unwrap().remove("recovery");
        assert!(serde_json::from_value::<NodeNativeCapacityPolicy>(encoded).is_err());
        for field in ["maximum_attempts", "work_per_attempt", "control_storage_bytes", "maximum_duration_ms"] {
            let mut invalid = serde_json::to_value(&policy).unwrap();
            invalid["recovery"][field] = 0.into();
            assert!(serde_json::from_value::<NodeNativeCapacityPolicy>(invalid).unwrap().validate().is_err());
        }
        policy.recovery.maximum_attempts = i64::MAX as u64;
        assert!(policy.validate().is_err());
    }
}
