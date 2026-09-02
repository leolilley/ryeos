//! Node-owned recurring maintenance and retention authority.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "maintenance";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeMaintenancePolicy {
    pub schema: u32,
    pub schedules: Vec<NodeMaintenanceSchedulePolicy>,
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
    fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node maintenance policy schema is not current");
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
    fn validate(&self) -> anyhow::Result<()> {
        ryeos_scheduler::crontab::validate_schedule_id(&self.schedule_id)?;
        ryeos_engine::canonical_ref::CanonicalRef::parse(&self.item_ref)
            .with_context(|| format!("invalid item_ref `{}`", self.item_ref))?;
        ryeos_scheduler::types::validate_schedule_ref_bindings(&self.ref_bindings)
            .with_context(|| format!("schedule `{}` has invalid ref_bindings", self.schedule_id))?;
        ryeos_scheduler::crontab::validate_expression(&self.schedule_type, &self.expression)?;
        ryeos_scheduler::crontab::validate_timezone(&self.timezone)?;
        ryeos_scheduler::overlap::parse_overlap_policy(&self.overlap_policy).with_context(
            || format!("schedule `{}` has invalid overlap_policy", self.schedule_id),
        )?;
        ryeos_scheduler::misfire::parse_misfire_policy(&self.misfire_policy).with_context(
            || format!("schedule `{}` has invalid misfire_policy", self.schedule_id),
        )?;
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

impl TypedNodePolicy for NodeMaintenancePolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct NodeMaintenancePolicySection;

impl NodePolicySection for NodeMaintenancePolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let policy: NodeMaintenancePolicy =
            serde_json::from_value(body.clone()).context("parse node maintenance policy")?;
        policy.validate()?;
        Ok(Arc::new(policy))
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
            schema: 1,
            schedules: vec![schedule.clone(), schedule],
        };
        assert!(policy.validate().is_err());
    }
}
