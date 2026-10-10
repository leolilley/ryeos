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
        Ok(Arc::new(policy))
    }
}
