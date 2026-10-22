//! Export one local qualification as a signed testimony and submit it to a
//! hosted node.

use std::sync::Arc;

use anyhow::{Result, bail};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use serde::Deserialize;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::qualification_testimony_push::{
    QualificationTestimonyExportRequest, export_qualification_testimony,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub remote: String,
    pub qualification_hash: String,
    pub acceptance_policy_ref: String,
}

impl Request {
    fn validate(&self) -> Result<()> {
        if self.remote.is_empty() || self.remote.len() > 256 {
            bail!("qualification testimony export requires an exact remote");
        }
        if !lillux::valid_hash(&self.qualification_hash)
            || self
                .qualification_hash
                .bytes()
                .any(|byte| byte.is_ascii_uppercase())
        {
            bail!("qualification testimony export requires an exact canonical qualification hash");
        }
        if self.acceptance_policy_ref.len() > 512
            || self
                .acceptance_policy_ref
                .strip_prefix("config:")
                .is_none_or(str::is_empty)
        {
            bail!("qualification testimony export requires an exact config: acceptance policy ref");
        }
        Ok(())
    }
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)?;
    req.validate()?;
    let outcome = export_qualification_testimony(
        &state,
        &ctx,
        &QualificationTestimonyExportRequest {
            remote: req.remote,
            qualification_hash: req.qualification_hash,
            acceptance_policy_ref: req.acceptance_policy_ref,
        },
    )
    .await?;
    Ok(serde_json::to_value(outcome)?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/export-qualification-testimony",
    endpoint: "external-content.export-qualification-testimony",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/export-qualification-testimony"],
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
            "/../../../bundles/core/.ai/services/external-content/export-qualification-testimony.yaml"
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
                "qualification_hash": "string",
                "acceptance_policy_ref": "string",
            })
        );
        let mut coordinates = serde_json::Map::new();
        for (field, kind) in descriptor["schema"].as_object().unwrap() {
            let kind = kind.as_str().unwrap();
            if kind.ends_with('?') {
                continue;
            }
            coordinates.insert(field.clone(), serde_json::json!("x"));
        }
        let mut with_unknown = coordinates.clone();
        with_unknown.insert("not_a_field".to_owned(), serde_json::json!(1));
        assert!(serde_json::from_value::<Request>(serde_json::Value::Object(coordinates)).is_ok());
        assert!(
            serde_json::from_value::<Request>(serde_json::Value::Object(with_unknown)).is_err()
        );
    }

    #[test]
    fn export_requires_exact_coordinates() {
        let valid = serde_json::json!({
            "remote": "hosted",
            "qualification_hash": "a".repeat(64),
            "acceptance_policy_ref": "config:render/snapshot-acceptance-policy",
        });
        serde_json::from_value::<Request>(valid.clone())
            .unwrap()
            .validate()
            .unwrap();
        for field in ["remote", "qualification_hash", "acceptance_policy_ref"] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Request>(missing).is_err());
        }
        for (field, value) in [
            ("remote", serde_json::json!("")),
            ("qualification_hash", serde_json::json!("A".repeat(64))),
            ("qualification_hash", serde_json::json!("b".repeat(63))),
            ("acceptance_policy_ref", serde_json::json!("render/policy")),
            ("acceptance_policy_ref", serde_json::json!("config:")),
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
        substituted["testimony"] = serde_json::json!({});
        assert!(serde_json::from_value::<Request>(substituted).is_err());
    }

    #[test]
    fn descriptor_names_the_export_service() {
        assert_eq!(
            DESCRIPTOR.service_ref,
            "service:external-content/export-qualification-testimony"
        );
        assert_eq!(
            DESCRIPTOR.endpoint,
            "external-content.export-qualification-testimony"
        );
        assert_eq!(
            DESCRIPTOR.required_caps,
            ["ryeos.execute.service.external-content/export-qualification-testimony"]
        );
    }
}
