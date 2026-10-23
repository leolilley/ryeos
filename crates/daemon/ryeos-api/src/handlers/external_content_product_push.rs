//! Push one exact published product from this node to a hosted node.

use std::sync::Arc;

use anyhow::{Result, bail};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::node_policy::sections::external_content::ExternalContentImportPolicyRecord;
use serde::Deserialize;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::product_push::{ProductPushRequest, push_product};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub remote: String,
    pub witness_hash: String,
    pub origin_admission_hash: String,
    pub maximum_bytes: u64,
}

impl Request {
    fn validate(&self) -> Result<()> {
        if self.remote.is_empty() || self.remote.len() > 256 || self.maximum_bytes == 0 {
            bail!("product push requires an exact remote and positive byte bound");
        }
        for hash in [&self.witness_hash, &self.origin_admission_hash] {
            if !lillux::valid_hash(hash) || hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
                bail!("product push requires exact canonical witness and admission hashes");
            }
        }
        Ok(())
    }
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)?;
    req.validate()?;
    let policy = state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?;
    if req.maximum_bytes > policy.limits.max_total_bytes {
        bail!("product push byte bound exceeds current import policy");
    }
    let outcome = push_product(
        &state,
        &ctx,
        &ProductPushRequest {
            remote: req.remote,
            witness_hash: req.witness_hash,
            origin_admission_hash: req.origin_admission_hash,
            maximum_bytes: req.maximum_bytes,
        },
    )
    .await?;
    Ok(serde_json::to_value(outcome)?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/push-product",
    endpoint: "external-content.push-product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/push-product"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { handle(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_service_schema_matches_the_typed_request() {
        let descriptor: serde_json::Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/core/.ai/services/external-content/push-product.yaml"
        )))
        .unwrap();
        assert_eq!(descriptor["endpoint"], DESCRIPTOR.endpoint);
        assert_eq!(
            descriptor["required_caps"],
            serde_json::json!(DESCRIPTOR.required_caps)
        );
        assert_eq!(
            descriptor["schema"],
            serde_json::json!({
                "remote": "string",
                "witness_hash": "string",
                "origin_admission_hash": "string",
                "maximum_bytes": "integer",
            })
        );
        // Every authored field is a field of the typed request: the request
        // refuses unknown fields, so a field missing from it would fail here.
        let mut coordinates = serde_json::Map::new();
        for (field, kind) in descriptor["schema"].as_object().unwrap() {
            let kind = kind.as_str().unwrap();
            if kind.ends_with('?') {
                continue;
            }
            let value = match kind.trim_end_matches('?') {
                "integer" => serde_json::json!(1),
                "object" => serde_json::json!({}),
                "object[]" => serde_json::json!([]),
                _ => serde_json::json!("x"),
            };
            coordinates.insert(field.clone(), value);
        }
        let mut with_unknown = coordinates.clone();
        with_unknown.insert("not_a_field".to_owned(), serde_json::json!(1));
        assert!(serde_json::from_value::<Request>(serde_json::Value::Object(coordinates)).is_ok());
        assert!(
            serde_json::from_value::<Request>(serde_json::Value::Object(with_unknown)).is_err()
        );
    }

    #[test]
    fn push_requires_exact_coordinates_and_no_caller_key() {
        let valid = serde_json::json!({
            "remote": "hosted",
            "witness_hash": "a".repeat(64),
            "origin_admission_hash": "b".repeat(64),
            "maximum_bytes": 1024,
        });
        serde_json::from_value::<Request>(valid.clone())
            .unwrap()
            .validate()
            .unwrap();
        for field in [
            "remote",
            "witness_hash",
            "origin_admission_hash",
            "maximum_bytes",
        ] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Request>(missing).is_err());
        }
        for (field, value) in [
            ("remote", serde_json::json!("")),
            ("witness_hash", serde_json::json!("A".repeat(64))),
            ("origin_admission_hash", serde_json::json!("b".repeat(63))),
            ("maximum_bytes", serde_json::json!(0)),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(
                serde_json::from_value::<Request>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut substituted = valid;
        substituted["staging_id"] = serde_json::json!("caller-chosen");
        assert!(serde_json::from_value::<Request>(substituted).is_err());
    }

    #[test]
    fn descriptor_names_the_push_service() {
        assert_eq!(
            DESCRIPTOR.service_ref,
            "service:external-content/push-product"
        );
        assert_eq!(DESCRIPTOR.endpoint, "external-content.push-product");
        assert_eq!(
            DESCRIPTOR.required_caps,
            ["ryeos.execute.service.external-content/push-product"]
        );
    }
}
