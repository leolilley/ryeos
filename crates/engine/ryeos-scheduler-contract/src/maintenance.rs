//! Pure signed maintenance policy vocabulary and validation.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub const SECTION_NAME: &str = "maintenance";
/// Runtime and scheduler each have four SQLite source roles; Operational
/// adds four SQLite roles and its immutable initialization marker.
pub const OFFLINE_RESET_SOURCE_ROLES: u32 = 13;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeMaintenancePolicy {
    pub schema: u32,
    pub schedules: Vec<NodeMaintenanceSchedulePolicy>,
    /// Explicit local operator quota, not a financial reservation or an issued
    /// allocation. Null disables offline reset admission. Missing is invalid.
    #[serde(deserialize_with = "required_nullable_quota")]
    pub offline_reset: Option<OfflineResetQuota>,
}

/// Hard envelope for an aggregate offline maintenance owner. The production
/// issuer must debit these dimensions before acquisition and retain the debit
/// while any partial destination survives. Decoding this data issues no permit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineResetQuota {
    pub max_retained_operations: u32,
    pub max_source_roles: u32,
    pub max_copied_bytes: u64,
    pub max_retained_destination_bytes: u64,
    pub max_read_bytes: u64,
    pub max_sql_vm_steps: u64,
    pub max_memory_bytes: u64,
    /// Ceiling for actual page-rounded native arenas and allocator metadata.
    /// This is a sub-cap of max_memory_bytes, never another memory allowance.
    pub max_native_storage_bytes: u64,
    /// Irreversible allocator work, including bounded metadata scans and
    /// reserved cleanup, across the original factory's complete lifetime.
    pub max_native_allocator_steps: u64,
    /// Total original connection tickets, including failed and abandoned opens.
    pub max_native_connections: u32,
    pub max_operation_millis: u64,
    pub max_cleanup_millis: u64,
    /// Irreversible aggregate cleanup protocol steps across every retry.
    pub max_cleanup_steps: u64,
    pub max_recovery_record_bytes: u64,
}

fn required_nullable_quota<'de, D>(deserializer: D) -> Result<Option<OfflineResetQuota>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<OfflineResetQuota>::deserialize(deserializer)
}

impl OfflineResetQuota {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_retained_operations == 0
            || self.max_source_roles == 0
            || self.max_native_connections == 0
        {
            bail!("offline reset operation and source-role quotas must be positive");
        }
        if self.max_source_roles < OFFLINE_RESET_SOURCE_ROLES {
            bail!("offline reset source quota cannot cover Runtime, scheduler and Operational provenance");
        }
        for (name, value) in [
            ("copied bytes", self.max_copied_bytes),
            (
                "retained destination bytes",
                self.max_retained_destination_bytes,
            ),
            ("read bytes", self.max_read_bytes),
            ("SQL VM steps", self.max_sql_vm_steps),
            ("memory bytes", self.max_memory_bytes),
            ("native storage bytes", self.max_native_storage_bytes),
            ("native allocator steps", self.max_native_allocator_steps),
            ("operation milliseconds", self.max_operation_millis),
            ("cleanup milliseconds", self.max_cleanup_millis),
            ("cleanup steps", self.max_cleanup_steps),
            ("recovery record bytes", self.max_recovery_record_bytes),
        ] {
            if value == 0 || value > i64::MAX as u64 {
                bail!("offline reset {name} quota must be positive and fit signed accounting");
            }
        }
        self.max_operation_millis
            .checked_add(self.max_cleanup_millis)
            .and_then(|millis| millis.checked_mul(1_000_000))
            .context("offline reset service/cleanup occupancy exceeds native clock range")?;
        if self.max_retained_destination_bytes < self.max_copied_bytes {
            bail!("offline reset destination quota cannot hold its copied extent");
        }
        // Copy reads the source and receipt verification reads the destination.
        // Subsequent verification/SQL/retries must consume the remaining quota;
        // this minimum is not permission to repeat either pass without debit.
        let minimum_reads = self
            .max_copied_bytes
            .checked_mul(2)
            .context("offline reset minimum read work overflow")?;
        if self.max_read_bytes < minimum_reads {
            bail!("offline reset read quota cannot cover copy and receipt verification");
        }
        let minimum_memory = self
            .max_native_storage_bytes
            .checked_add(128 * 1024)
            .context("offline reset native storage and buffer reservation overflow")?;
        if minimum_memory > self.max_memory_bytes {
            bail!(
                "offline reset memory quota cannot hold native storage and bounded copy/hash buffers"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeMaintenanceSchedulePolicy {
    pub schedule_id: String,
    pub item_ref: String,
    pub ref_bindings: BTreeMap<String, String>,
    pub schedule_type: String,
    pub expression: String,
    pub timezone: String,
    pub misfire_policy: String,
    pub overlap_policy: String,
    pub lateness_grace_secs: i64,
    /// Seed only for first materialization. The ordinary signed scheduler
    /// configuration owns subsequent pause/resume state.
    pub initial_enabled: bool,
    pub params: Value,
    pub capabilities: Vec<String>,
}

impl NodeMaintenancePolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 4 {
            bail!("node maintenance policy schema is not current");
        }
        if let Some(quota) = &self.offline_reset {
            quota
                .validate()
                .context("validate offline reset local quota")?;
        }
        let mut schedule_ids = HashSet::with_capacity(self.schedules.len());
        for schedule in &self.schedules {
            schedule.validate().with_context(|| {
                format!(
                    "validate node maintenance schedule `{}`",
                    schedule.schedule_id
                )
            })?;
            if !schedule_ids.insert(schedule.schedule_id.as_str()) {
                bail!(
                    "node maintenance policy contains duplicate schedule_id `{}`",
                    schedule.schedule_id
                );
            }
        }
        Ok(())
    }
}

impl NodeMaintenanceSchedulePolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        crate::validate_schedule_id(&self.schedule_id)?;
        ryeos_engine::canonical_ref::CanonicalRef::parse(&self.item_ref)
            .with_context(|| format!("invalid item_ref `{}`", self.item_ref))?;
        crate::validate_schedule_ref_bindings(&self.ref_bindings)
            .with_context(|| format!("schedule `{}` has invalid ref_bindings", self.schedule_id))?;
        crate::validate_expression(&self.schedule_type, &self.expression)?;
        crate::validate_timezone(&self.timezone)?;
        crate::parse_overlap_policy(&self.overlap_policy).with_context(|| {
            format!("schedule `{}` has invalid overlap_policy", self.schedule_id)
        })?;
        crate::parse_misfire_policy(&self.misfire_policy).with_context(|| {
            format!("schedule `{}` has invalid misfire_policy", self.schedule_id)
        })?;
        if self.lateness_grace_secs <= 0 {
            bail!(
                "schedule `{}` lateness_grace_secs must be positive",
                self.schedule_id
            );
        }
        if !self.params.is_object() {
            bail!("schedule `{}` params must be a mapping", self.schedule_id);
        }
        if self.capabilities.is_empty()
            || self
                .capabilities
                .iter()
                .any(|capability| capability.trim().is_empty())
        {
            bail!(
                "schedule `{}` must declare non-empty capabilities",
                self.schedule_id
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_schedule_ids() {
        let schedule = NodeMaintenanceSchedulePolicy {
            schedule_id: "maintenance-gc".into(),
            item_ref: "service:maintenance/gc".into(),
            ref_bindings: BTreeMap::new(),
            schedule_type: "cron".into(),
            expression: "0 0 4 * * *".into(),
            timezone: "UTC".into(),
            misfire_policy: "skip".into(),
            overlap_policy: "skip".into(),
            lateness_grace_secs: 60,
            initial_enabled: true,
            params: serde_json::json!({}),
            capabilities: vec!["ryeos.execute.service.maintenance/gc".into()],
        };
        let policy = NodeMaintenancePolicy {
            schema: 4,
            offline_reset: None,
            schedules: vec![schedule.clone(), schedule],
        };
        assert!(policy.validate().is_err());
    }
    #[test]
    fn rejects_source_quota_missing_operational_provenance() {
        let mut candidate = quota();
        candidate.max_source_roles = 8;
        assert!(candidate.validate().is_err());
        candidate.max_source_roles = OFFLINE_RESET_SOURCE_ROLES - 1;
        assert!(candidate.validate().is_err());
        candidate.max_source_roles = OFFLINE_RESET_SOURCE_ROLES;
        candidate.validate().unwrap();
    }

    fn quota() -> OfflineResetQuota {
        OfflineResetQuota {
            max_retained_operations: 4,
            max_source_roles: OFFLINE_RESET_SOURCE_ROLES,
            max_copied_bytes: 268435456,
            max_retained_destination_bytes: 536870912,
            max_read_bytes: 2147483648,
            max_sql_vm_steps: 20000000,
            max_memory_bytes: 67108864,
            max_native_storage_bytes: 50331648,
            max_native_allocator_steps: 1048576,
            max_native_connections: 64,
            max_operation_millis: 300000,
            max_cleanup_millis: 60000,
            max_cleanup_steps: 1024,
            max_recovery_record_bytes: 2097152,
        }
    }

    #[test]
    fn offline_quota_requires_explicit_current_policy_and_every_dimension() {
        let disabled: NodeMaintenancePolicy = serde_json::from_value(
            serde_json::json!({"schema": 4, "schedules": [], "offline_reset": null}),
        )
        .unwrap();
        disabled.validate().unwrap();
        assert!(
            serde_json::from_value::<NodeMaintenancePolicy>(
                serde_json::json!({"schema": 4, "schedules": []})
            )
            .is_err()
        );
        let enabled = NodeMaintenancePolicy {
            schema: 4,
            schedules: vec![],
            offline_reset: Some(quota()),
        };
        enabled.validate().unwrap();
        let mut old = enabled.clone();
        old.schema = 3;
        assert!(old.validate().is_err());
        let record = serde_json::to_value(&enabled).unwrap();
        for name in record["offline_reset"].as_object().unwrap().keys() {
            let mut missing = record.clone();
            missing["offline_reset"]
                .as_object_mut()
                .unwrap()
                .remove(name);
            assert!(
                serde_json::from_value::<NodeMaintenancePolicy>(missing).is_err(),
                "{name}"
            );
        }
        let mut unknown = record;
        unknown["offline_reset"]["financial_reservation"] = serde_json::json!(true);
        assert!(serde_json::from_value::<NodeMaintenancePolicy>(unknown).is_err());
    }

    #[test]
    fn offline_quota_rejects_zero_overflow_and_insufficient_aggregate_bounds() {
        let record = serde_json::to_value(quota()).unwrap();
        for name in record.as_object().unwrap().keys() {
            let mut zero = record.clone();
            zero[name] = serde_json::json!(0);
            let zero: OfflineResetQuota = serde_json::from_value(zero).unwrap();
            assert!(zero.validate().is_err(), "{name}");
        }
        let mut insufficient = quota();
        insufficient.max_retained_destination_bytes = insufficient.max_copied_bytes - 1;
        assert!(insufficient.validate().is_err());
        insufficient = quota();
        insufficient.max_read_bytes = insufficient.max_copied_bytes * 2 - 1;
        assert!(insufficient.validate().is_err());
        insufficient = quota();
        insufficient.max_memory_bytes = 128 * 1024 - 1;
        assert!(insufficient.validate().is_err());
        insufficient = quota();
        insufficient.max_native_storage_bytes = insufficient.max_memory_bytes;
        assert!(insufficient.validate().is_err());
        insufficient = quota();
        insufficient.max_native_storage_bytes = i64::MAX as u64;
        assert!(insufficient.validate().is_err());
        insufficient = quota();
        insufficient.max_native_allocator_steps = u64::MAX;
        assert!(insufficient.validate().is_err());
        insufficient = quota();
        insufficient.max_sql_vm_steps = u64::MAX;
        assert!(insufficient.validate().is_err());
        insufficient = quota();
        insufficient.max_operation_millis = i64::MAX as u64;
        assert!(insufficient.validate().is_err());
        insufficient = quota();
        insufficient.max_operation_millis = u64::MAX / 1_000_000;
        insufficient.max_cleanup_millis = 60000;
        assert!(insufficient.validate().is_err());
    }
}
