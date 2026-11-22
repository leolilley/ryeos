//! Operator-authorized, read-only emission of the admitted external candidate
//! program for a product-sourced worker.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub type Request = ryeos_app::operator_external_content::product_program::ProductProgramRequest;

pub const REQUIRED_CAP: &str = "ryeos.execute.service.external-execution/admit-product-program";

/// The consumer is resolved from the installed signed bundle only, through the
/// same preparation and selection admission composition uses. The selections
/// are admitted but nothing is imported, bound or published.
pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let response = ryeos_app::operator_external_content::product_program::admit(
        state,
        ctx,
        req,
        |state, context, request| {
            let mut prepared =
                ryeos_executor::execution::project_source::prepare_installed_bundle_external_consumer(
                    state,
                    &request.consumer_ref,
                    context.fingerprint.clone(),
                    context.scopes.clone(),
                )?;
            prepared.select_products_only(
                state,
                context,
                &request.canonical_selections(),
                None,
            )?;
            ryeos_engine::external_content::resolved_external_product_selections(
                prepared.resolution(),
            )?
            .ok_or_else(|| anyhow::anyhow!("product selection produced no resolved selections"))
        },
    )
    .await?;
    Ok(serde_json::to_value(response)?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-execution/admit-product-program",
    endpoint: "external-execution.admit-product-program",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[REQUIRED_CAP],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_program_has_distinct_authority_from_the_content_program() {
        use super::super::external_execution_candidate_program as content;
        assert_ne!(DESCRIPTOR.service_ref, content::DESCRIPTOR.service_ref);
        assert_ne!(DESCRIPTOR.endpoint, content::DESCRIPTOR.endpoint);
        assert_eq!(DESCRIPTOR.required_caps, &[REQUIRED_CAP]);
        assert!(
            DESCRIPTOR
                .required_caps
                .iter()
                .all(|cap| !content::DESCRIPTOR.required_caps.contains(cap))
        );
        assert!(matches!(
            DESCRIPTOR.availability,
            ServiceAvailability::DaemonOnly
        ));
    }

    #[test]
    fn request_surface_is_only_the_consumer_and_its_selections() {
        let request = serde_json::json!({
            "consumer_ref": "worker:codex/external-hosted-authoring",
            "product_selections": [{
                "declaration_id": "guest-runtime",
                "witness_hash": "a".repeat(64),
                "witness_source": {"kind": "received", "acceptance_hash": "b".repeat(64)},
                "qualification_hash": "c".repeat(64),
                "remote_testimony": {
                    "acceptance_hash": "d".repeat(64),
                    "acceptance_policy_ref": "config:codex/acceptance"
                }
            }]
        });
        let parsed: Request = serde_json::from_value(request.clone()).unwrap();
        parsed.validate().unwrap();
        for field in [
            "policy_ref",
            "verifier_ref",
            "runtime_manifest_hash",
            "project_context",
            "relationship_name",
        ] {
            let mut changed = request.clone();
            changed[field] = serde_json::json!("caller-selected");
            assert!(serde_json::from_value::<Request>(changed).is_err());
        }
    }
}
