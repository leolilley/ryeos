//! Accept one signed qualification testimony from an authenticated origin
//! operator.
//!
//! The caller is authenticated exactly as a product push is. The submitting
//! key is the only authentication: the testimony may have been carried here
//! offline, so nothing in the body names the caller. The acceptance core runs
//! under the authenticated operator as owner and the origin key pinned for the
//! authenticated site. The call is idempotent.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::operator_external_content::qualification_testimony::accept_testimony_for_owner;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::Attestation;
use serde::Deserialize;
use serde_json::Value;

use super::external_content_product_offer::authenticate_push_origin;
use crate::registry::ServiceDescriptor;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub testimony: Value,
}

impl Request {
    fn attestation(&self) -> Result<Attestation> {
        if !self.testimony.is_object() {
            bail!("qualification testimony must be a signed attestation object");
        }
        Attestation::from_value(&self.testimony)
            .context("qualification testimony is not a signed attestation")
    }
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let authority = authenticate_push_origin(&state, &ctx)?;
    let testimony = req.attestation()?;
    tokio::task::spawn_blocking(move || {
        let accepted = accept_testimony_for_owner(
            &state,
            &authority.owner_principal,
            &authority.origin_key,
            testimony,
            &lillux::time::iso8601_now(),
        )?;
        Ok(serde_json::json!({
            "testimony_hash": accepted.testimony_hash,
            "acceptance_hash": accepted.acceptance_hash,
            "coordinate_id": accepted.coordinate_id,
            "reused_existing": accepted.reused_existing,
        }))
    })
    .await
    .context("qualification testimony accept task stopped")?
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/accept-qualification-testimony",
    endpoint: "external-content.accept-qualification-testimony",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/accept-qualification-testimony"],
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
            "/../../../bundles/core/.ai/services/external-content/accept-qualification-testimony.yaml"
        )))
        .unwrap();
        assert_eq!(descriptor["endpoint"], DESCRIPTOR.endpoint);
        assert_eq!(
            descriptor["required_caps"],
            serde_json::json!(DESCRIPTOR.required_caps)
        );
        assert_eq!(
            descriptor["schema"],
            serde_json::json!({ "testimony": "object" })
        );
        let ok = serde_json::json!({ "testimony": {} });
        assert!(serde_json::from_value::<Request>(ok).is_ok());
        let unknown = serde_json::json!({ "testimony": {}, "not_a_field": 1 });
        assert!(serde_json::from_value::<Request>(unknown).is_err());
        assert!(serde_json::from_value::<Request>(serde_json::json!({})).is_err());
    }

    #[test]
    fn testimony_must_be_a_signed_attestation_object() {
        for body in [
            serde_json::json!("text"),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({ "kind": "attestation" }),
        ] {
            let request = serde_json::from_value::<Request>(serde_json::json!({
                "testimony": body,
            }))
            .unwrap();
            assert!(request.attestation().is_err());
        }
    }

    #[test]
    fn descriptor_names_the_accept_service() {
        assert_eq!(
            DESCRIPTOR.service_ref,
            "service:external-content/accept-qualification-testimony"
        );
        assert_eq!(
            DESCRIPTOR.endpoint,
            "external-content.accept-qualification-testimony"
        );
        assert_eq!(
            DESCRIPTOR.required_caps,
            ["ryeos.execute.service.external-content/accept-qualification-testimony"]
        );
    }
}
