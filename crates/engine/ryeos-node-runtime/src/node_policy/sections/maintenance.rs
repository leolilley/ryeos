//! Node-owned recurring maintenance and retention authority.

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};
use anyhow::Context as _;
use ryeos_scheduler_contract::{NodeMaintenancePolicy, SECTION_NAME};
use serde_json::Value;
use std::sync::Arc;

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
        if let Some(quota) = &policy.offline_reset {
            validate_offline_reset_quota(quota)
                .context("validate offline reset quota against the native host")?;
        }
        for (class, quota) in &policy.offline_native {
            validate_offline_native_quota(quota).with_context(|| {
                format!("validate offline native quota `{class}` against the native host")
            })?;
        }
        Ok(Arc::new(policy))
    }
}

/// Check an offline native quota against the bounds the native host owns, so
/// a policy is refused when it is applied rather than when it is first used.
pub fn validate_offline_native_quota(
    quota: &ryeos_scheduler_contract::OfflineNativeQuota,
) -> anyhow::Result<()> {
    quota.validate_native(
        lillux_sqlite::CONTROL_STORAGE,
        u32::try_from(lillux_sqlite::MAX_NATIVE_CONNECTIONS)
            .context("native ticket capacity exceeds u32")?,
        lillux_sqlite::offline_host::offline_native_required_cleanup_steps,
    )
}

/// Check the signed offline reset quota's native step budget against the
/// native host's minimum allocation funding margin and exact cleanup quote.
pub fn validate_offline_reset_quota(
    quota: &ryeos_scheduler_contract::OfflineResetQuota,
) -> anyhow::Result<()> {
    quota.validate_native(
        lillux_sqlite::offline_host::offline_native_allocation_funding_margin_steps,
        lillux_sqlite::offline_host::offline_native_required_cleanup_steps,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_scheduler_contract::NodeMaintenancePolicy;

    fn shipped_maintenance_policies() -> Vec<(String, NodeMaintenancePolicy)> {
        let profiles = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/.ai/node/init/profiles");
        let mut policies = Vec::new();
        for entry in std::fs::read_dir(&profiles).expect("init profiles directory") {
            let path = entry.expect("profile entry").path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("profile text");
            let profile: serde_json::Value = serde_yaml::from_str(&text).expect("profile YAML");
            let Some(maintenance) = profile.pointer("/policies/maintenance") else {
                continue;
            };
            let policy: NodeMaintenancePolicy =
                serde_json::from_value(maintenance.clone()).expect("maintenance policy");
            policies.push((path.display().to_string(), policy));
        }
        policies
    }

    #[test]
    fn shipped_profiles_fund_native_sql_in_both_budgets() {
        let policies = shipped_maintenance_policies();
        let mut checked = 0;
        for (path, policy) in &policies {
            policy
                .validate()
                .unwrap_or_else(|error| panic!("{path}: {error:#}"));
            if let Some(quota) = &policy.offline_reset {
                validate_offline_reset_quota(quota)
                    .unwrap_or_else(|error| panic!("{path}: {error:#}"));
                checked += 1;
            }
        }
        assert_eq!(checked, 16, "every shipped offline reset quota is checked");
    }

    #[test]
    fn undersized_native_step_budget_is_refused() {
        let policies = shipped_maintenance_policies();
        let mut quota = policies
            .iter()
            .find_map(|(_, policy)| policy.offline_reset.clone())
            .expect("a shipped offline reset quota");
        // The previous shipped value: enough for the allocation margin and cleanup, far
        // below the SQL ceiling that native SQL also charges.
        quota.max_native_allocator_steps = 1_048_576;
        let error = validate_offline_reset_quota(&quota).unwrap_err();
        assert!(
            format!("{error:#}").contains("SQL VM step ceiling"),
            "{error:#}"
        );
        // Above the ceiling but short of the allocation funding margin and
        // the cleanup quote.
        quota.max_native_allocator_steps = quota.max_sql_vm_steps + 1;
        let error = validate_offline_reset_quota(&quota).unwrap_err();
        assert!(
            format!("{error:#}").contains("allocation funding margin"),
            "{error:#}"
        );
    }
}
