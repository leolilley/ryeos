//! Exact operator-owned product capture and durable lookup.

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use anyhow::Result;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use serde_json::Value;
use std::sync::Arc;

pub type Request = ryeos_app::operator_external_content::products::ProductRequest;

pub async fn capture(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::products::capture(state, ctx, req).await?,
    )?)
}

pub async fn get(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::products::get(state, ctx, req).await?,
    )?)
}

pub async fn qualify(
    req: ryeos_app::operator_external_content::product_qualification::ProductQualificationRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let project_context_resolver = super::qualification_project_context::resolver(state.as_ref());
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::product_qualification::qualify_with_project_context_resolver(
            state,
            ctx,
            req,
            Some(project_context_resolver),
        )
        .await?,
    )?)
}

pub async fn compose(
    req: ryeos_app::operator_external_content::product_composition::ComposeRetainedProductsRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    req.validate()?;
    ryeos_app::operator_authority::require_admitted_operator(&state, &ctx)?;
    let qualification_project_context_resolver =
        super::qualification_project_context::resolver(state.as_ref());
    let preparation_request = req.clone();
    let preparation_state = Arc::clone(&state);
    let preparation_context = ctx.clone();
    let preparation_qualification_resolver = Arc::clone(&qualification_project_context_resolver);
    let (prepared, imported) = tokio::task::spawn_blocking(move || {
        let mut prepared =
            ryeos_executor::execution::project_source::prepare_external_product_consumer(
                &preparation_state,
                &preparation_request,
                &preparation_context,
                &format!("product-composition-{}", uuid::Uuid::new_v4()),
            )?;
        let imported = prepared.select_and_import_products(
            preparation_state,
            preparation_context,
            &preparation_request,
            Some(preparation_qualification_resolver),
        )?;
        anyhow::Ok((prepared, imported))
    })
    .await
    .map_err(|error| anyhow::anyhow!("product consumer preparation failed: {error}"))??;
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::product_composition::compose_selected_products(
            state,
            ctx,
            req,
            prepared.resolution(),
            imported,
        )
        .await?,
    )?)
}

pub const CAPTURE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/capture-product",
    endpoint: "external-content.capture-product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/capture-product"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { capture(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};

pub const GET_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/product",
    endpoint: "external-content.product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/product"],
    handler: |params, ctx, state| {
        Box::pin(async move { get(crate::handler_error::parse_request(params)?, ctx, state).await })
    },
};

pub const COMPOSE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/compose-product",
    endpoint: "external-content.compose-product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/compose-product"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { compose(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};

#[cfg(test)]
mod tests {
    use ryeos_app::operator_external_content::product_composition::ComposeRetainedProductsRequest;
    use ryeos_engine::contracts::SubjectResolutionAuthority;
    use serde_json::json;

    #[test]
    fn signed_compose_contract_preserves_required_nullable_project_authority() {
        let root = ryeos_engine::test_support::workspace_root();
        let service: serde_yaml::Value = serde_yaml::from_slice(
            &std::fs::read(
                root.join("bundles/core/.ai/services/external-content/compose-product.yaml"),
            )
            .unwrap(),
        )
        .unwrap();
        let command: serde_yaml::Value = serde_yaml::from_slice(
            &std::fs::read(
                root.join("bundles/core/.ai/node/commands/external-content-compose-product.yaml"),
            )
            .unwrap(),
        )
        .unwrap();
        let schema = serde_json::to_value(&service["schema"]).unwrap();
        let contract =
            ryeos_runtime::InvocationInputContract::from_lightweight_schema_value(&schema)
                .unwrap()
                .unwrap();
        let project_field = &contract.fields["project_context"];
        assert!(project_field.required);
        assert!(project_field.nullable);
        assert!(command["help"]["usage"].as_str().unwrap().contains(
            "\"project_context\":{\"snapshot_hash\":\"<exact-lowercase-project-snapshot-hash>\"}"
        ));

        let projectless_wire = json!({
            "consumer_ref": "tool:example/verify",
            "project_context": null,
            "selections": [{
                "declaration_id": "runtime",
                "witness_hash": "a".repeat(64),
                "witness_source": {"kind": "local_capture"},
                "qualification_hash": null
            }],
            "maximum_bytes": 1024
        });
        let normalized = ryeos_runtime::arg_binder::normalize_params_with_contract(
            projectless_wire.clone(),
            Some(&contract),
        )
        .unwrap();
        let projectless: ComposeRetainedProductsRequest =
            serde_json::from_value(normalized).unwrap();
        projectless.validate().unwrap();
        assert_eq!(
            projectless.subject_resolution_authority(),
            SubjectResolutionAuthority::Projectless
        );

        let mut pinned_wire = projectless_wire.clone();
        let snapshot_hash = "b".repeat(64);
        pinned_wire["project_context"] = json!({"snapshot_hash": snapshot_hash});
        let normalized =
            ryeos_runtime::arg_binder::normalize_params_with_contract(pinned_wire, Some(&contract))
                .unwrap();
        let pinned: ComposeRetainedProductsRequest = serde_json::from_value(normalized).unwrap();
        pinned.validate().unwrap();
        assert_eq!(
            pinned.subject_resolution_authority(),
            SubjectResolutionAuthority::PinnedGeneration { snapshot_hash }
        );

        let missing = json!({
            "consumer_ref": "tool:example/verify",
            "selections": [{
                "declaration_id": "runtime",
                "witness_hash": "a".repeat(64),
                "witness_source": {"kind": "local_capture"},
                "qualification_hash": null
            }],
            "maximum_bytes": 1024
        });
        assert!(
            ryeos_runtime::arg_binder::normalize_params_with_contract(missing, Some(&contract))
                .unwrap_err()
                .contains("--project-context is required")
        );
    }
}

pub const QUALIFY_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/qualify-product",
    endpoint: "external-content.qualify-product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/qualify-product"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { qualify(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};
