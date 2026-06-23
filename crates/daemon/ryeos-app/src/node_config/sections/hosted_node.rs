//! `hosted` section handler for node-config.
//!
//! Hosted node policy is node-level operator configuration for daemons exposed
//! as public remotes. It is loaded from bundles/system state but is not an
//! execution-authority mechanism.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, NodeItemContext, SectionRecord, SectionSourcePolicy};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedNodePolicyRecord {
    pub version: String,
    pub schema_version: String,
    pub description: String,
    pub transport: HostedNodeTransportPolicy,
    pub admission: HostedNodeAdmissionPolicy,
    pub descriptor: HostedNodeDescriptorPolicy,
    pub authorization: HostedNodeAuthorizationPolicy,
    pub operations: HostedNodeOperationsPolicy,
    #[serde(skip)]
    pub source_file: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedNodeTransportPolicy {
    pub public_https_required: bool,
    pub loopback_http_allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedNodeAdmissionPolicy {
    pub mode: String,
    pub token_ttl_secs: u64,
    pub reject_wildcard_scopes: bool,
    pub token_delivery: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedNodeDescriptorPolicy {
    pub require_live_identity_match: bool,
    #[serde(default)]
    pub advertised_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedNodeAuthorizationPolicy {
    pub authority: String,
    pub central_bearer_tokens_allowed: bool,
    pub implicit_cross_node_authority_allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedNodeOperationsPolicy {
    pub audit_admission_events: bool,
    pub audit_grant_changes: bool,
    pub prefer_isolated_node_per_principal: bool,
    pub shared_daemon_multitenancy_enabled: bool,
}

pub struct HostedNodePolicySection;

impl NodeConfigSection for HostedNodePolicySection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::EffectiveBundleRootsAndState
    }

    fn parse(&self, ctx: &NodeItemContext, body: &Value) -> Result<Box<dyn SectionRecord>> {
        let record: HostedNodePolicyRecord = serde_json::from_value(body.clone())
            .context("failed to parse hosted-node policy record")?;

        if ctx.id != "policy" {
            bail!(
                "hosted-node policy filename must be 'policy', got '{}'",
                ctx.id
            );
        }
        if record.schema_version != "1.0.0" {
            bail!(
                "hosted-node policy schema_version '{}' is unsupported",
                record.schema_version
            );
        }
        if !record.transport.public_https_required {
            bail!("hosted-node policy must require public HTTPS for non-loopback exposure");
        }
        if record.admission.mode != "one_time_token" {
            bail!(
                "hosted-node admission.mode '{}' is unsupported",
                record.admission.mode
            );
        }
        if record.admission.token_ttl_secs == 0 {
            bail!("hosted-node admission.token_ttl_secs must be greater than zero");
        }
        if !record.admission.reject_wildcard_scopes {
            bail!("hosted-node policy must reject wildcard admission scopes");
        }
        if record.admission.token_delivery != "out_of_band" {
            bail!(
                "hosted-node admission.token_delivery '{}' is unsupported",
                record.admission.token_delivery
            );
        }
        if !record.descriptor.require_live_identity_match {
            bail!("hosted-node policy must require live descriptor identity matching");
        }
        if record.authorization.authority != "target_node_authorized_keys" {
            bail!(
                "hosted-node authorization.authority '{}' is unsupported",
                record.authorization.authority
            );
        }
        if record.authorization.central_bearer_tokens_allowed {
            bail!("hosted-node policy must not allow central bearer tokens as execution authority");
        }
        if record.authorization.implicit_cross_node_authority_allowed {
            bail!("hosted-node policy must not allow implicit cross-node authority");
        }
        if record.operations.shared_daemon_multitenancy_enabled {
            bail!("hosted-node policy must not enable shared-daemon multitenancy in this bundle");
        }

        Ok(Box::new(record))
    }
}

impl SectionRecord for HostedNodePolicyRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_config::NodeItemContext;

    fn ctx(id: &str) -> NodeItemContext {
        NodeItemContext {
            section: "hosted".into(),
            id: id.into(),
            stem: id.into(),
            rel_path: format!("{id}.yaml").into(),
            source_file: format!("/tmp/{id}.yaml").into(),
            signer_fingerprint: "test".into(),
        }
    }

    fn valid_body() -> Value {
        serde_json::json!({
            "version": "0.1.0",
            "schema_version": "1.0.0",
            "description": "Default hosted-node operator policy for decentralized remote admission.",
            "transport": {
                "public_https_required": true,
                "loopback_http_allowed": true
            },
            "admission": {
                "mode": "one_time_token",
                "token_ttl_secs": 600,
                "reject_wildcard_scopes": true,
                "token_delivery": "out_of_band"
            },
            "descriptor": {
                "require_live_identity_match": true,
                "advertised_capabilities": ["remote-execute", "bundle-install"]
            },
            "authorization": {
                "authority": "target_node_authorized_keys",
                "central_bearer_tokens_allowed": false,
                "implicit_cross_node_authority_allowed": false
            },
            "operations": {
                "audit_admission_events": true,
                "audit_grant_changes": true,
                "prefer_isolated_node_per_principal": true,
                "shared_daemon_multitenancy_enabled": false
            }
        })
    }

    #[test]
    fn valid_policy_parses() {
        let section = HostedNodePolicySection;
        assert!(section.parse(&ctx("policy"), &valid_body()).is_ok());
    }

    #[test]
    fn central_bearer_authority_is_rejected() {
        let section = HostedNodePolicySection;
        let mut body = valid_body();
        body["authorization"]["central_bearer_tokens_allowed"] = serde_json::json!(true);
        let err = section
            .parse(&ctx("policy"), &body)
            .unwrap_err()
            .to_string();
        assert!(err.contains("central bearer tokens"), "got: {err}");
    }

    #[test]
    fn unsafe_transport_and_admission_defaults_are_rejected() {
        let section = HostedNodePolicySection;

        let mut body = valid_body();
        body["transport"]["public_https_required"] = serde_json::json!(false);
        let err = section
            .parse(&ctx("policy"), &body)
            .unwrap_err()
            .to_string();
        assert!(err.contains("public HTTPS"), "got: {err}");

        let mut body = valid_body();
        body["admission"]["reject_wildcard_scopes"] = serde_json::json!(false);
        let err = section
            .parse(&ctx("policy"), &body)
            .unwrap_err()
            .to_string();
        assert!(err.contains("wildcard"), "got: {err}");

        let mut body = valid_body();
        body["admission"]["token_delivery"] = serde_json::json!("provider_session");
        let err = section
            .parse(&ctx("policy"), &body)
            .unwrap_err()
            .to_string();
        assert!(err.contains("token_delivery"), "got: {err}");

        let mut body = valid_body();
        body["descriptor"]["require_live_identity_match"] = serde_json::json!(false);
        let err = section
            .parse(&ctx("policy"), &body)
            .unwrap_err()
            .to_string();
        assert!(err.contains("descriptor identity"), "got: {err}");
    }
}
