//! Node-owned client-address policy: which peers may vouch for the address a
//! request came from, and which header they use to say so.
//!
//! The section is required in every profile. An empty `trusted_proxies` list
//! means the socket peer is the address.

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "client_address";

/// The header a trusted proxy uses to carry the client address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAddressHeader {
    Forwarded,
    XForwardedFor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAddressPolicy {
    pub schema: u32,
    pub trusted_proxies: Vec<String>,
    pub header: ClientAddressHeader,
}

/// A CIDR is `<ip>/<prefix>`; the prefix is required and bounded by the
/// address family.
fn validate_cidr(cidr: &str) -> anyhow::Result<()> {
    let Some((address, prefix)) = cidr.split_once('/') else {
        bail!("trusted proxy `{cidr}` must be a CIDR (`<address>/<prefix>`)");
    };
    let address: IpAddr = address
        .parse()
        .with_context(|| format!("trusted proxy `{cidr}` has an invalid address"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("trusted proxy `{cidr}` has an invalid prefix length"))?;
    let max = if address.is_ipv4() { 32 } else { 128 };
    if prefix > max {
        bail!("trusted proxy `{cidr}` prefix length exceeds {max}");
    }
    Ok(())
}

impl ClientAddressPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("client_address policy schema is not current");
        }
        for cidr in &self.trusted_proxies {
            validate_cidr(cidr)?;
        }
        Ok(())
    }
}

impl TypedNodePolicy for ClientAddressPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct ClientAddressPolicySection;

impl NodePolicySection for ClientAddressPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: ClientAddressPolicy = serde_json::from_value(body.clone())
            .context("failed to parse client_address policy record")?;
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

    fn body(proxies: Value, header: &str) -> Value {
        serde_json::json!({"schema": 1, "trusted_proxies": proxies, "header": header})
    }

    #[test]
    fn empty_and_valid_proxies_compile() {
        let section = ClientAddressPolicySection;
        assert_eq!(section.name(), SECTION_NAME);
        assert!(
            section
                .parse(&ctx(), &body(serde_json::json!([]), "forwarded"))
                .is_ok()
        );
        let proxies = serde_json::json!(["10.0.0.0/8", "::1/128", "fd00::/8"]);
        let parsed = section
            .parse(&ctx(), &body(proxies, "x_forwarded_for"))
            .unwrap();
        let record = parsed
            .as_any()
            .downcast_ref::<ClientAddressPolicy>()
            .unwrap();
        assert_eq!(record.header, ClientAddressHeader::XForwardedFor);
    }

    #[test]
    fn bad_cidrs_are_refused() {
        for bad in [
            "10.0.0.1",
            "10.0.0.0/33",
            "::/129",
            "nope/8",
            "10.0.0.0/x",
            "/8",
            "",
        ] {
            let body = body(serde_json::json!([bad]), "forwarded");
            assert!(
                ClientAddressPolicySection.parse(&ctx(), &body).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn unknown_header_and_unknown_field_are_refused() {
        let section = ClientAddressPolicySection;
        assert!(
            section
                .parse(&ctx(), &body(serde_json::json!([]), "x-forwarded-for"))
                .is_err()
        );
        assert!(
            section
                .parse(&ctx(), &body(serde_json::json!([]), "via"))
                .is_err()
        );
        let mut extra = body(serde_json::json!([]), "forwarded");
        extra["trust_all"] = Value::Bool(true);
        assert!(section.parse(&ctx(), &extra).is_err());
    }

    #[test]
    fn missing_fields_and_stale_schema_are_refused() {
        let section = ClientAddressPolicySection;
        assert!(
            section
                .parse(
                    &ctx(),
                    &serde_json::json!({"schema": 1, "header": "forwarded"})
                )
                .is_err()
        );
        assert!(
            section
                .parse(
                    &ctx(),
                    &serde_json::json!({"schema": 1, "trusted_proxies": []})
                )
                .is_err()
        );
        let mut stale = body(serde_json::json!([]), "forwarded");
        stale["schema"] = Value::from(2);
        assert!(section.parse(&ctx(), &stale).is_err());
    }
}
