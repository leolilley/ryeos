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
        anyhow::ensure!(
            schema == 1,
            "thread-history node policy schema is not current"
        );
        let content_hash = ryeos_state::objects::canonical_value_digest(body)?;
        let provenance = ryeos_engine::history_policy::NodeHistoryPolicyProvenance {
            path: PathBuf::from(ryeos_engine::history_policy::NODE_HISTORY_POLICY_CONFIG),
            space: ryeos_engine::contracts::ItemSpace::Node,
            content_hash,
            signer_fingerprint: context.signer_fingerprint.clone(),
        };
        let policy =
            ryeos_engine::history_policy::resolve_node_thread_history_policy(semantic, provenance)
                .map_err(anyhow::Error::from)?;
        Ok(Arc::new(policy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::contracts::ItemSpace;

    fn context() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: "/node/.ai/node/policies/thread_history.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn policy_value() -> Value {
        serde_json::json!({
            "schema": 1,
            "default_retention": "durable",
            "item_authored_retention": "allow",
            "minimum_terminal_for": null
        })
    }

    #[test]
    fn compiles_exact_signed_node_policy_provenance() {
        let parsed = ThreadHistoryPolicySection
            .parse(&context(), &policy_value())
            .unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy>()
            .unwrap();
        assert_eq!(
            policy.provenance.path,
            std::path::Path::new(ryeos_engine::history_policy::NODE_HISTORY_POLICY_CONFIG)
        );
        assert_eq!(policy.provenance.space, ItemSpace::Node);
        assert_eq!(policy.provenance.signer_fingerprint, "ab".repeat(32));
    }

    #[test]
    fn rejects_absent_schema_and_unknown_fields() {
        let mut absent = policy_value();
        absent.as_object_mut().unwrap().remove("schema");
        assert!(
            ThreadHistoryPolicySection
                .parse(&context(), &absent)
                .is_err()
        );

        let mut unknown = policy_value();
        unknown["unexpected"] = Value::Bool(true);
        assert!(
            ThreadHistoryPolicySection
                .parse(&context(), &unknown)
                .is_err()
        );
    }
}
