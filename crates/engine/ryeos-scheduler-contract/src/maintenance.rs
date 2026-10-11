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
    /// Native envelopes for stopped-node maintenance, keyed by operation
    /// class. A class absent from this map is refused; an empty map
    /// disables every stopped-node native consumer.
    pub offline_native: BTreeMap<String, OfflineNativeQuota>,
}

/// Process-scoped native envelope for one stopped-node operation class.
///
/// The holder issues it only while it owns the exclusive node StateLock and
/// the runtime namespace lock, so no live daemon or other maintainer runs
/// concurrently. The arena is private process memory reclaimed by the kernel
/// when the process ends, so nothing charged here outlives the process and no
/// durable debit is recorded. A consumer that creates an obligation outliving
/// its process must use a durable claim instead of this envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineNativeQuota {
    /// Page-rounded native arena plus allocator metadata.
    pub max_native_storage_bytes: u64,
    /// Irreversible allocator work across the host's lifetime, including the
    /// reserved cleanup steps below.
    pub max_native_allocator_steps: u64,
    /// Connection tickets, including failed and abandoned opens.
    pub max_native_connections: u32,
    pub max_operation_millis: u64,
    pub max_cleanup_millis: u64,
    /// Reserved teardown steps. Issuance refuses when this is below the
    /// native quote for the storage and connections above.
    pub max_cleanup_steps: u64,
}

impl OfflineNativeQuota {
    /// Shape checks that need no native layout knowledge.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_native_connections == 0 {
            bail!("offline native connections must be positive");
        }
        for (name, value) in [
            ("native storage bytes", self.max_native_storage_bytes),
            ("native allocator steps", self.max_native_allocator_steps),
            ("operation milliseconds", self.max_operation_millis),
            ("cleanup milliseconds", self.max_cleanup_millis),
            ("cleanup steps", self.max_cleanup_steps),
        ] {
            if value == 0 || value > i64::MAX as u64 {
                bail!("offline native {name} quota must be positive and fit signed accounting");
            }
        }
        if self.max_cleanup_steps >= self.max_native_allocator_steps {
            bail!("offline native cleanup steps must be a strict sub-reservation of allocator steps");
        }
        self.max_operation_millis
            .checked_add(self.max_cleanup_millis)
            .and_then(|millis| millis.checked_mul(1_000_000))
            .context("offline native service/cleanup occupancy exceeds native clock range")?;
        Ok(())
    }

    /// Bounds owned by the native host, supplied by its owner: the arena's
    /// control storage, its fixed ticket capacity and the cleanup quote for
    /// this quota's storage and connections.
    pub fn validate_native(
        &self,
        control_storage: u64,
        max_connections: u32,
        required_cleanup_steps: impl FnOnce(u64, u32) -> anyhow::Result<u64>,
    ) -> anyhow::Result<()> {
        self.validate()?;
        if self.max_native_storage_bytes <= control_storage {
            bail!("offline native storage must exceed the arena's {control_storage}-byte control storage");
        }
        if self.max_native_connections > max_connections {
            bail!("offline native connections exceed the arena's {max_connections}-ticket capacity");
        }
        let required = required_cleanup_steps(self.max_native_storage_bytes, self.max_native_connections)?;
        if self.max_cleanup_steps < required {
            bail!(
                "offline native cleanup reserves {} steps; the arena quote requires {required}",
                self.max_cleanup_steps
            );
        }
        Ok(())
    }
}

/// Operation class keys are lowercase ASCII words joined by `.` or `-`.
pub fn validate_offline_native_class(class: &str) -> anyhow::Result<()> {
    if class.is_empty()
        || class.len() > 64
        || !class.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'-')
    {
        bail!("offline native class `{class}` must match [a-z0-9.-]{{1,64}}");
    }
    Ok(())
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
        // Every native SQL opcode charges the host's native step budget as
        // well as the SQL ceiling, so the native budget must exceed it.
        if self.max_native_allocator_steps <= self.max_sql_vm_steps {
            bail!(
                "offline reset native allocator steps ({}) must exceed the SQL VM step ceiling ({}) that native SQL also charges",
                self.max_native_allocator_steps,
                self.max_sql_vm_steps
            );
        }
        Ok(())
    }

    /// Check the native step budget against what the native host owns.
    /// Native SQL charges both the host's step budget and the SQL ceiling, so
    /// the budget must cover the whole ceiling, plus the host's minimum
    /// allocation funding margin, plus its exact cleanup quote, for this
    /// quota's storage and connections. The allocation margin is a heuristic
    /// floor, not a bound on allocator work; native exhaustion still fails
    /// closed at run time.
    pub fn validate_native(
        &self,
        allocation_funding_margin_steps: impl FnOnce(u64, u32) -> anyhow::Result<u64>,
        required_cleanup_steps: impl FnOnce(u64, u32) -> anyhow::Result<u64>,
    ) -> anyhow::Result<()> {
        self.validate()?;
        let allocation = allocation_funding_margin_steps(self.max_native_storage_bytes, self.max_native_connections)?;
        let cleanup = required_cleanup_steps(self.max_native_storage_bytes, self.max_native_connections)?;
        let required = self
            .max_sql_vm_steps
            .checked_add(allocation)
            .and_then(|steps| steps.checked_add(cleanup))
            .context("offline reset native step requirement overflow")?;
        if self.max_native_allocator_steps < required {
            bail!(
                "offline reset native allocator steps ({}) cannot cover the SQL VM step ceiling ({}) plus the minimum native allocation funding margin ({allocation}) and the cleanup quote ({cleanup}); at least {required} are required",
                self.max_native_allocator_steps,
                self.max_sql_vm_steps
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
        if self.schema != 5 {
            bail!("node maintenance policy schema is not current; reset the node policy generation");
        }
        if let Some(quota) = &self.offline_reset {
            quota
                .validate()
                .context("validate offline reset local quota")?;
        }
        for (class, quota) in &self.offline_native {
            validate_offline_native_class(class)?;
            quota
                .validate()
                .with_context(|| format!("validate offline native quota `{class}`"))?;
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
            schema: 5,
            offline_reset: None,
            offline_native: BTreeMap::new(),
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
            max_native_allocator_steps: 24000000,
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
            serde_json::json!({"schema": 5, "schedules": [], "offline_reset": null, "offline_native": {}}),
        )
        .unwrap();
        disabled.validate().unwrap();
        assert!(
            serde_json::from_value::<NodeMaintenancePolicy>(
                serde_json::json!({"schema": 5, "schedules": [], "offline_native": {}})
            )
            .is_err()
        );
        let enabled = NodeMaintenancePolicy {
            schema: 5,
            schedules: vec![],
            offline_reset: Some(quota()),
            offline_native: BTreeMap::new(),
        };
        enabled.validate().unwrap();
        assert!(
            serde_json::from_value::<NodeMaintenancePolicy>(
                serde_json::json!({"schema": 5, "schedules": [], "offline_reset": null})
            )
            .is_err(),
            "offline_native is required"
        );
        let mut old = enabled.clone();
        old.schema = 4;
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

    #[test]
    fn offline_reset_native_steps_cover_sql_allocation_margin_and_cleanup() {
        let mut undersized = quota();
        undersized.max_native_allocator_steps = 1048576;
        let error = undersized.validate().unwrap_err();
        assert!(format!("{error:#}").contains("SQL VM step ceiling"), "{error:#}");
        let allocation = |_: u64, _: u32| Ok(1_000_000);
        let cleanup = |_: u64, _: u32| Ok(200_000);
        quota().validate_native(allocation, cleanup).unwrap();
        let mut short = quota();
        short.max_native_allocator_steps = short.max_sql_vm_steps + 1_200_000 - 1;
        let error = short.validate_native(allocation, cleanup).unwrap_err();
        assert!(format!("{error:#}").contains("at least 21200000"), "{error:#}");
        assert!(format!("{error:#}").contains("minimum native allocation funding margin"), "{error:#}");
        short.max_native_allocator_steps += 1;
        short.validate_native(allocation, cleanup).unwrap();
    }

    fn native_quota() -> OfflineNativeQuota {
        OfflineNativeQuota {
            max_native_storage_bytes: 33554432,
            max_native_allocator_steps: 20000000,
            max_native_connections: 8,
            max_operation_millis: 300000,
            max_cleanup_millis: 60000,
            max_cleanup_steps: 400000,
        }
    }

    #[test]
    fn offline_native_classes_are_data_keyed_and_every_dimension_is_bounded() {
        let mut policy = NodeMaintenancePolicy {
            schema: 5,
            schedules: vec![],
            offline_reset: None,
            offline_native: BTreeMap::from([("replay-indexes".to_owned(), native_quota())]),
        };
        policy.validate().unwrap();
        policy.offline_native.insert("Replay_Indexes".to_owned(), native_quota());
        assert!(policy.validate().is_err());
        policy.offline_native.remove("Replay_Indexes");
        let record = serde_json::to_value(native_quota()).unwrap();
        for name in record.as_object().unwrap().keys() {
            let mut zero = record.clone();
            zero[name] = serde_json::json!(0);
            let zero: OfflineNativeQuota = serde_json::from_value(zero).unwrap();
            assert!(zero.validate().is_err(), "{name}");
            let mut missing = record.clone();
            missing.as_object_mut().unwrap().remove(name);
            assert!(serde_json::from_value::<OfflineNativeQuota>(missing).is_err(), "{name}");
        }
        let quote = |_: u64, _: u32| Ok(1000);
        native_quota().validate_native(65_536, 64, quote).unwrap();
        let mut over = native_quota();
        over.max_native_connections = 65;
        assert!(over.validate_native(65_536, 64, quote).is_err());
        let mut small = native_quota();
        small.max_native_storage_bytes = 65_536;
        assert!(small.validate_native(65_536, 64, quote).is_err());
        let mut short = native_quota();
        short.max_cleanup_steps = 999;
        assert!(short.validate_native(65_536, 64, quote).is_err());
        let mut cleanup = native_quota();
        cleanup.max_cleanup_steps = cleanup.max_native_allocator_steps;
        assert!(cleanup.validate().is_err());
    }
}
