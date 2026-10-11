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
        for (class, quota) in &policy.offline_native {
            validate_offline_native_quota(quota)
                .with_context(|| format!("validate offline native quota `{class}` against the native host"))?;
        }
        Ok(Arc::new(policy))
    }
}

/// Check an offline native quota against the bounds the native host owns, so
/// a policy is refused when it is applied rather than when it is first used.
pub fn validate_offline_native_quota(quota: &ryeos_scheduler_contract::OfflineNativeQuota) -> anyhow::Result<()> {
    quota.validate_native(
        lillux_sqlite::CONTROL_STORAGE,
        u32::try_from(lillux_sqlite::MAX_NATIVE_CONNECTIONS).context("native ticket capacity exceeds u32")?,
        lillux_sqlite::offline_host::offline_native_required_cleanup_steps,
    )
}
