//! Node-owned hosted admission policy.
//!
//! The policy is required. Protocol and authorization invariants remain
//! code-owned; the operator chooses whether one-time-token admission is
//! enabled, its token lifetime ceiling, and whether descriptor URLs may use
//! plain HTTP on loopback.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "hosted";
pub const MAX_ADMISSION_TOKEN_TTL_SECS: u64 = 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedNodePolicy {
    pub schema: u32,
    pub admission_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_token_ttl_secs: Option<u64>,
    pub allow_loopback_http: bool,
}

impl HostedNodePolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("hosted-node policy schema is not current");
        }
        match (self.admission_enabled, self.admission_token_ttl_secs) {
            (true, Some(0)) => {
                bail!("hosted-node admission_token_ttl_secs must be greater than zero")
            }
            (true, Some(ttl)) if ttl > MAX_ADMISSION_TOKEN_TTL_SECS => bail!(
                "hosted-node admission_token_ttl_secs must not exceed {MAX_ADMISSION_TOKEN_TTL_SECS}"
            ),
            (true, Some(_)) | (false, None) => {}
            (true, None) => bail!("enabled hosted-node admission requires a bounded token TTL"),
            (false, Some(_)) => {
                bail!("disabled hosted-node admission must not retain a latent token TTL")
            }
        }
        Ok(())
    }
}

pub struct HostedNodePolicySection;

impl TypedNodePolicy for HostedNodePolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

impl NodePolicySection for HostedNodePolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: HostedNodePolicy = serde_json::from_value(body.clone())
            .context("failed to parse hosted-node policy record")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.into(),
            source_file: format!("/node/policies/{SECTION_NAME}.yaml").into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn valid_body() -> Value {
        serde_json::json!({
            "schema": 1,
            "admission_enabled": true,
            "admission_token_ttl_secs": 600,
            "allow_loopback_http": true
        })
    }

    #[test]
    fn section_compiles_current_node_policy() {
        let section = HostedNodePolicySection;
        assert_eq!(section.name(), SECTION_NAME);
        assert!(section.parse(&ctx(), &valid_body()).is_ok());
    }

    #[test]
    fn policy_rejects_unknown_schema_and_unbounded_ttl() {
        let section = HostedNodePolicySection;

        let mut body = valid_body();
        body["schema"] = Value::from(2);
        assert!(section.parse(&ctx(), &body).is_err());

        let mut body = valid_body();
        body["admission_token_ttl_secs"] = Value::from(0);
        assert!(section.parse(&ctx(), &body).is_err());

        let mut body = valid_body();
        body["admission_token_ttl_secs"] = Value::from(MAX_ADMISSION_TOKEN_TTL_SECS + 1);
        assert!(section.parse(&ctx(), &body).is_err());
    }

    #[test]
    fn admission_enablement_is_an_explicit_operator_choice() {
        let section = HostedNodePolicySection;
        let mut body = valid_body();
        body["admission_enabled"] = Value::Bool(false);
        body.as_object_mut()
            .unwrap()
            .remove("admission_token_ttl_secs");
        let parsed = section.parse(&ctx(), &body).unwrap();
        let record = parsed.as_any().downcast_ref::<HostedNodePolicy>().unwrap();
        assert!(!record.admission_enabled);
        assert_eq!(record.admission_token_ttl_secs, None);
    }

    #[test]
    fn admission_state_and_ttl_must_be_coherent() {
        let section = HostedNodePolicySection;

        let mut enabled_without_ttl = valid_body();
        enabled_without_ttl
            .as_object_mut()
            .unwrap()
            .remove("admission_token_ttl_secs");
        assert!(section.parse(&ctx(), &enabled_without_ttl).is_err());

        let mut disabled_with_ttl = valid_body();
        disabled_with_ttl["admission_enabled"] = Value::Bool(false);
        assert!(section.parse(&ctx(), &disabled_with_ttl).is_err());
    }

    #[test]
    fn legacy_configurable_invariants_are_rejected() {
        let section = HostedNodePolicySection;
        let mut body = valid_body();
        body["admission"] = serde_json::json!({
            "mode": "one_time_token",
            "reject_wildcard_scopes": true
        });
        let err = section.parse(&ctx(), &body).err().unwrap();
        assert!(format!("{err:#}").contains("unknown field"));
    }
}
use std::sync::Arc;
