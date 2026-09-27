//! Node-owned resource limits for authenticated UI browser sessions.
//!
//! The UI session store enforces this compiled policy when it admits retained
//! binding attachments. The policy has no daemon fallback: registration makes
//! the section a required member of every complete node-policy generation.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "ui_browser_sessions";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBrowserSessionPolicy {
    pub schema: u32,
    pub max_live_binding_attachments_per_session: u32,
}

impl UiBrowserSessionPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("UI browser-session node policy schema is not current");
        }
        if self.max_live_binding_attachments_per_session == 0 {
            bail!("UI browser-session max_live_binding_attachments_per_session must be non-zero");
        }
        Ok(())
    }
}

impl TypedNodePolicy for UiBrowserSessionPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct UiBrowserSessionPolicySection;

impl NodePolicySection for UiBrowserSessionPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: UiBrowserSessionPolicy =
            serde_json::from_value(body.clone()).context("parse UI browser-session node policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: "/node/policies/ui_browser_sessions.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    #[test]
    fn parses_exact_current_limits() {
        let parsed = UiBrowserSessionPolicySection
            .parse(
                &context(),
                &serde_json::json!({
                    "schema": 1,
                    "max_live_binding_attachments_per_session": 16
                }),
            )
            .unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<UiBrowserSessionPolicy>()
            .unwrap();
        assert_eq!(policy.max_live_binding_attachments_per_session, 16);
    }

    #[test]
    fn rejects_absent_zero_stale_and_unknown_limits() {
        for body in [
            serde_json::json!({"schema": 1}),
            serde_json::json!({
                "schema": 1,
                "max_live_binding_attachments_per_session": 0
            }),
            serde_json::json!({
                "schema": 2,
                "max_live_binding_attachments_per_session": 16
            }),
            serde_json::json!({
                "schema": 1,
                "max_live_binding_attachments_per_session": 16,
                "fallback": 1
            }),
        ] {
            assert!(
                UiBrowserSessionPolicySection
                    .parse(&context(), &body)
                    .is_err()
            );
        }
    }
}
