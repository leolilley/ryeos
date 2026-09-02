//! Node-owned thread-history retention and privacy authority.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "thread_history";

impl TypedNodePolicy for ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct ThreadHistoryPolicySection;

impl NodePolicySection for ThreadHistoryPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let mut semantic = body.clone();
        let object = semantic
            .as_object_mut()
            .context("thread-history node policy must be a mapping")?;
        let schema = object
            .remove("schema")
            .and_then(|value| value.as_u64())
            .context("thread-history node policy requires numeric schema")?;
        anyhow::ensure!(schema == 1, "thread-history node policy schema is not current");
        let content_hash = ryeos_state::objects::canonical_value_digest(body)?;
        let provenance = ryeos_engine::history_policy::NodeHistoryPolicyProvenance::SignedConfig {
            path: PathBuf::from(ryeos_engine::history_policy::NODE_HISTORY_POLICY_CONFIG),
            space: ryeos_engine::contracts::ItemSpace::Node,
            content_hash,
            signer_fingerprint: context.signer_fingerprint.clone(),
        };
        let policy = ryeos_engine::history_policy::resolve_node_thread_history_policy(
            semantic,
            provenance,
        )
        .map_err(anyhow::Error::from)?;
        Ok(Arc::new(policy))
    }
}
