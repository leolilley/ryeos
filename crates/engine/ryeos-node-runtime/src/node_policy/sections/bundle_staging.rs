//! Node-owned bounds on the bundle staging area.
//!
//! A staged bundle is inert bytes the node holds until a signed operation
//! applies or an operator clears it. This section names how many bytes and how
//! many stages the node as a whole, and each requesting key, may hold. It is a
//! required member of every complete node-policy generation: there is no
//! daemon fallback, and an absent section means an incomplete generation, not
//! an unbounded area.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "bundle_staging";

/// The only schema this section is read under.
pub const CURRENT_SCHEMA: u32 = 1;

/// Why a `bundle_staging` section was refused. Each variant names the field
/// at fault.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BundleStagingPolicyError {
    #[error("bundle staging policy schema {found} is not the current schema {CURRENT_SCHEMA}")]
    Schema { found: u32 },
    #[error("bundle staging policy field `{field}` must be nonzero")]
    Zero { field: &'static str },
    #[error(
        "bundle staging policy per-key cap `{key_field}` ({key}) exceeds the node cap \
         `{node_field}` ({node})"
    )]
    KeyAboveNode {
        key_field: &'static str,
        key: u64,
        node_field: &'static str,
        node: u64,
    },
}

/// The byte and count bounds on staging, per node and per requesting key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleStagingPolicy {
    pub schema: u32,
    /// Most bytes all stages together may hold.
    pub max_node_bytes: u64,
    /// Most stages the node may hold.
    pub max_node_stages: u64,
    /// Most bytes staged on behalf of one requesting key.
    pub max_key_bytes: u64,
    /// Most stages one requesting key may hold.
    pub max_key_stages: u64,
}

impl TypedNodePolicy for BundleStagingPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

impl BundleStagingPolicy {
    pub fn validate(&self) -> Result<(), BundleStagingPolicyError> {
        if self.schema != CURRENT_SCHEMA {
            return Err(BundleStagingPolicyError::Schema { found: self.schema });
        }
        for (field, value) in [
            ("max_node_bytes", self.max_node_bytes),
            ("max_node_stages", self.max_node_stages),
            ("max_key_bytes", self.max_key_bytes),
            ("max_key_stages", self.max_key_stages),
        ] {
            if value == 0 {
                return Err(BundleStagingPolicyError::Zero { field });
            }
        }
        if self.max_key_bytes > self.max_node_bytes {
            return Err(BundleStagingPolicyError::KeyAboveNode {
                key_field: "max_key_bytes",
                key: self.max_key_bytes,
                node_field: "max_node_bytes",
                node: self.max_node_bytes,
            });
        }
        if self.max_key_stages > self.max_node_stages {
            return Err(BundleStagingPolicyError::KeyAboveNode {
                key_field: "max_key_stages",
                key: self.max_key_stages,
                node_field: "max_node_stages",
                node: self.max_node_stages,
            });
        }
        Ok(())
    }
}

pub struct BundleStagingPolicySection;

impl NodePolicySection for BundleStagingPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let policy: BundleStagingPolicy = serde_json::from_value(body.clone())
            .map_err(|error| anyhow::anyhow!("parse bundle staging node policy: {error}"))?;
        policy.validate()?;
        Ok(Arc::new(policy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: "/node/policies/bundle_staging.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn body() -> Value {
        json!({
            "schema": 1,
            "max_node_bytes": 2147483648_u64,
            "max_node_stages": 64,
            "max_key_bytes": 536870912_u64,
            "max_key_stages": 16
        })
    }

    fn parse(value: Value) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        BundleStagingPolicySection.parse(&context(), &value)
    }

    #[test]
    fn parses_the_shipped_values() {
        let parsed = parse(body()).unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<BundleStagingPolicy>()
            .unwrap();
        assert_eq!(policy.max_node_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(policy.max_node_stages, 64);
        assert_eq!(policy.max_key_bytes, 512 * 1024 * 1024);
        assert_eq!(policy.max_key_stages, 16);
    }

    #[test]
    fn rejects_absent_stale_and_unknown_fields() {
        for field in [
            "schema",
            "max_node_bytes",
            "max_node_stages",
            "max_key_bytes",
            "max_key_stages",
        ] {
            let mut value = body();
            value.as_object_mut().unwrap().remove(field);
            assert!(parse(value).is_err(), "missing `{field}` must be refused");
        }
        for schema in [0, 2] {
            let mut value = body();
            value["schema"] = json!(schema);
            assert!(parse(value).is_err());
        }
        let mut unknown = body();
        unknown["fallback"] = json!(1);
        assert!(parse(unknown).is_err());
    }

    #[test]
    fn every_cap_must_be_nonzero() {
        for field in [
            "max_node_bytes",
            "max_node_stages",
            "max_key_bytes",
            "max_key_stages",
        ] {
            let mut value = body();
            value[field] = json!(0);
            let policy: BundleStagingPolicy = serde_json::from_value(value.clone()).unwrap();
            assert!(matches!(
                policy.validate(),
                Err(BundleStagingPolicyError::Zero { field: named }) if named == field
            ));
            assert!(parse(value).is_err());
        }
    }

    #[test]
    fn a_key_cap_cannot_exceed_the_node_cap() {
        let mut bytes = body();
        bytes["max_key_bytes"] = json!(2147483649_u64);
        let policy: BundleStagingPolicy = serde_json::from_value(bytes.clone()).unwrap();
        assert!(matches!(
            policy.validate(),
            Err(BundleStagingPolicyError::KeyAboveNode {
                key_field: "max_key_bytes",
                ..
            })
        ));
        assert!(parse(bytes).is_err());

        let mut stages = body();
        stages["max_key_stages"] = json!(65);
        let policy: BundleStagingPolicy = serde_json::from_value(stages.clone()).unwrap();
        assert!(matches!(
            policy.validate(),
            Err(BundleStagingPolicyError::KeyAboveNode {
                key_field: "max_key_stages",
                ..
            })
        ));
        assert!(parse(stages).is_err());

        let mut equal = body();
        equal["max_key_bytes"] = equal["max_node_bytes"].clone();
        equal["max_key_stages"] = equal["max_node_stages"].clone();
        assert!(parse(equal).is_ok());
    }
}
