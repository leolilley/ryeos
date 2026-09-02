//! Node-owned accounting/provider-contact timing authority.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "accounting";
pub const MAX_ISSUE_ACCEPTANCE_WINDOW_MS: u64 = 60 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeAccountingPolicy {
    pub schema: u32,
    pub issue_acceptance_window_ms: u64,
}

impl NodeAccountingPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node accounting policy schema is not current");
        }
        if self.issue_acceptance_window_ms == 0
            || self.issue_acceptance_window_ms > MAX_ISSUE_ACCEPTANCE_WINDOW_MS
        {
            bail!(
                "node accounting issue_acceptance_window_ms must be in 1..={MAX_ISSUE_ACCEPTANCE_WINDOW_MS}"
            );
        }
        Ok(())
    }
}

impl TypedNodePolicy for NodeAccountingPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct NodeAccountingPolicySection;

impl NodePolicySection for NodeAccountingPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: NodeAccountingPolicy = serde_json::from_value(body.clone())
            .context("parse node accounting policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}
use std::sync::Arc;
