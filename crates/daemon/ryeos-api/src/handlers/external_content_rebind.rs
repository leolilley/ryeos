//! Operator binding inventory and explicit generation re-bind.
//!
//! Both prepare the consumer at the requested generation through the same
//! preparation bind uses; the app layer then reads or renews the binding.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use super::external_content_bind::{ConsumerPreparation, prepare_consumer};
use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::operator_external_content::BindConsumerKind;
use ryeos_app::operator_external_content::rebind::{BindingsRequest, RebindRequest};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub async fn bindings(
    req: BindingsRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    req.validate()?;
    let prepared = prepare_consumer(
        &state,
        &ctx,
        ConsumerPreparation {
            consumer_ref: req.consumer_ref.clone(),
            consumer_kind: req.consumer_kind,
            project_snapshot_hash: req.project_snapshot_hash.clone(),
            project_path: req.project_path.clone(),
            product_selections: req.product_selections.clone(),
            product_owner_principal: req.product_owner_principal.clone(),
            checkout_label: "external-content-bindings",
        },
    )
    .await?;
    let inventory_state = Arc::clone(&state);
    let response = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_external_content::rebind::inventory(
            &inventory_state,
            &ctx,
            &req,
            prepared.resolution(),
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("external-content binding inventory panicked: {error}"))??;
    Ok(serde_json::to_value(response)?)
}

pub async fn rebind(
    req: RebindRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    req.validate()?;
    // Refuse a released, superseded or installed-bundle prior binding before
    // any snapshot is materialized or content staged.
    let prior_state = Arc::clone(&state);
    let prior_context = ctx.clone();
    let prior_request = req.clone();
    let prior = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_external_content::rebind::prior_binding(
            &prior_state,
            &prior_context,
            &prior_request,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("rebind prior-binding check panicked: {error}"))??;
    let prepared = prepare_consumer(
        &state,
        &ctx,
        ConsumerPreparation {
            consumer_ref: prior.consumer_ref().to_owned(),
            consumer_kind: BindConsumerKind::PinnedProject,
            project_snapshot_hash: Some(req.project_snapshot_hash.clone()),
            project_path: req.project_path.clone(),
            product_selections: req.product_selections.clone(),
            product_owner_principal: req.product_owner_principal.clone(),
            checkout_label: "external-content-rebind",
        },
    )
    .await?;
    let response = ryeos_app::operator_external_content::rebind::rebind(
        state,
        ctx,
        req,
        prior,
        prepared.resolution(),
    )
    .await?;
    Ok(serde_json::to_value(response)?)
}

pub const BINDINGS_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/bindings",
    endpoint: "external-content.bindings",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/bindings"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req = crate::handler_error::parse_request(params)?;
            bindings(req, ctx, state).await
        })
    },
};

pub const REBIND_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/rebind",
    endpoint: "external-content.rebind",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/rebind"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req = crate::handler_error::parse_request(params)?;
            rebind(req, ctx, state).await
        })
    },
};
