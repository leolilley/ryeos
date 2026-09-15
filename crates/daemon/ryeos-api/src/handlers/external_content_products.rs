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
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::product_qualification::qualify(state, ctx, req)
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
    let preparation_request = req.clone();
    let preparation_state = Arc::clone(&state);
    let preparation_context = ctx.clone();
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
