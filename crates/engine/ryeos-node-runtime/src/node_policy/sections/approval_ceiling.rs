//! Node-owned ceiling on which classes of worker approval request an owner may
//! accept.
//!
//! The admitted, frozen profile decides what a class of request can be
//! (`deny_only` or not). This policy lets the node operator cap it further, per
//! `operation_class`, and only ever narrow it: `refuse` is final and `allow`
//! still needs the profile to allow. A placement records no measured enclosure
//! fact today, so every placement counts as unmeasured: a class the policy does
//! not list is refused, and only a class listed `allow` can be accepted.
//! Shipped profiles list none, so the shipped default accepts nothing.
//!
//! The section has no daemon fallback: registration makes it a required member
//! of every complete node-policy generation, read at accept time.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "approval_ceiling";

/// Most classes one policy may list.
pub const MAX_APPROVAL_CEILING_CLASSES: usize = 128;
/// Longest operation class name, the same bound a worker's request carries.
pub const MAX_OPERATION_CLASS_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalClassRule {
    /// The class may be accepted, if the admitted profile also allows it.
    Allow,
    /// The class is never accepted on this node.
    Refuse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalCeilingPolicy {
    pub schema: u32,
    /// `operation_class` (the profile's own strings) to what this node allows.
    pub classes: BTreeMap<String, ApprovalClassRule>,
}

impl ApprovalCeilingPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node approval_ceiling policy schema is not current");
        }
        if self.classes.len() > MAX_APPROVAL_CEILING_CLASSES {
            bail!("node approval_ceiling lists more than {MAX_APPROVAL_CEILING_CLASSES} classes");
        }
        for class in self.classes.keys() {
            if class.is_empty()
                || class.len() > MAX_OPERATION_CLASS_BYTES
                || class.chars().any(char::is_control)
            {
                bail!("node approval_ceiling names an invalid operation class `{class}`");
            }
        }
        Ok(())
    }

    /// Whether this node lets a request of `operation_class` be accepted.
    /// Every placement is unmeasured today, so only a listed `allow` passes;
    /// the admitted profile's own rule is a separate condition.
    pub fn allows(&self, operation_class: &str) -> bool {
        self.classes.get(operation_class) == Some(&ApprovalClassRule::Allow)
    }

    /// The policy every shipped init profile carries: no class listed, so no
    /// accept is allowed. Builds exact test snapshots only; a node's policy
    /// always comes from its signed generation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn as_shipped() -> Self {
        Self {
            schema: 1,
            classes: BTreeMap::new(),
        }
    }
}

impl TypedNodePolicy for ApprovalCeilingPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct ApprovalCeilingPolicySection;

impl NodePolicySection for ApprovalCeilingPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: ApprovalCeilingPolicy =
            serde_json::from_value(body.clone()).context("parse node approval_ceiling policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: "/node/policies/approval_ceiling.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn parse(body: Value) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        ApprovalCeilingPolicySection.parse(&context(), &body)
    }

    #[test]
    fn the_shipped_body_lists_no_class_and_so_allows_none() {
        let parsed = parse(json!({"schema": 1, "classes": {}})).unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<ApprovalCeilingPolicy>()
            .unwrap();
        assert_eq!(policy, &ApprovalCeilingPolicy::as_shipped());
        assert!(!policy.allows("file_change"));
        assert!(!policy.allows("command_execution"));
    }

    #[test]
    fn only_a_class_listed_allow_passes() {
        let parsed = parse(json!({"schema": 1, "classes": {
            "file_change": "allow",
            "command_execution": "refuse",
        }}))
        .unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<ApprovalCeilingPolicy>()
            .unwrap();
        assert!(policy.allows("file_change"));
        assert!(!policy.allows("command_execution"), "refuse is final");
        assert!(!policy.allows("unlisted_class"), "unlisted is refused");
    }

    #[test]
    fn absent_stale_unknown_and_malformed_bodies_are_refused() {
        for body in [
            json!({"classes": {}}),
            json!({"schema": 1}),
            json!({"schema": 2, "classes": {}}),
            json!({"schema": 1, "classes": {}, "default_widening": "auto"}),
            json!({"schema": 1, "classes": {"file_change": "sometimes"}}),
            json!({"schema": 1, "classes": {"": "allow"}}),
            json!({"schema": 1, "classes": {"x".repeat(257): "allow"}}),
        ] {
            assert!(parse(body.clone()).is_err(), "{body}");
        }
        let too_many: serde_json::Map<String, Value> = (0..=MAX_APPROVAL_CEILING_CLASSES)
            .map(|index| (format!("class_{index}"), json!("refuse")))
            .collect();
        assert!(parse(json!({"schema": 1, "classes": too_many})).is_err());
    }
}
