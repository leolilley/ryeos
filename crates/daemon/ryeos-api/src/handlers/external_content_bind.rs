//! Operator-authorized promotion of an import stage to a consumer binding.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use ryeos_executor::execution::project_source::PreparedExternalProductConsumer;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub type Request = ryeos_app::operator_external_content::BindRequest;

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    req.validate_consumer_request()?;
    if req.product_selections.is_none()
        && req.consumer_kind
            == ryeos_app::operator_external_content::BindConsumerKind::InstalledBundle
    {
        return Ok(serde_json::to_value(
            ryeos_app::operator_external_content::bind(state, ctx, req).await?,
        )?);
    }
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
            checkout_label: "external-content-bind",
        },
    )
    .await?;
    let response = if req.product_selections.is_some() {
        ryeos_app::operator_external_content::bind_selected_literal_resolution(
            state,
            ctx,
            req,
            prepared.resolution(),
        )
        .await?
    } else {
        ryeos_app::operator_external_content::bind_pinned_project(
            state,
            ctx,
            req,
            prepared.resolution(),
        )
        .await?
    };
    Ok(serde_json::to_value(response)?)
}

/// The exact consumer coordinate bind, the binding inventory and rebind
/// prepare. Callers validate the coordinate first.
pub(crate) struct ConsumerPreparation {
    pub consumer_ref: String,
    pub consumer_kind: ryeos_app::operator_external_content::BindConsumerKind,
    pub project_snapshot_hash: Option<String>,
    pub project_path: Option<PathBuf>,
    pub product_selections:
        Option<Vec<ryeos_state::external_content::products::composition::ProductSelection>>,
    pub product_owner_principal: Option<String>,
    pub checkout_label: &'static str,
}

/// Prepare one consumer through the same snapshot materializer, source
/// admission and (when selected) product-selection verification that launch
/// uses. Only the configured local operator may prepare; a selected consumer
/// additionally names the exact admitted owner of every selected product.
pub(crate) async fn prepare_consumer(
    state: &Arc<AppState>,
    ctx: &HandlerContext,
    preparation: ConsumerPreparation,
) -> Result<PreparedExternalProductConsumer> {
    ryeos_app::operator_authority::require_local_configured_operator(state, ctx)?;
    let product_owner_context = match preparation.product_owner_principal.as_deref() {
        Some(principal) => Some(
            ryeos_app::operator_authority::admitted_operator_authority_for_principal(
                state, principal,
            )?
            .handler_context(),
        ),
        None => None,
    };
    let selected = preparation.product_selections.is_some();
    let qualification_project_context_resolver =
        selected.then(|| super::qualification_project_context::resolver(state.as_ref()));
    let preparation_state = Arc::clone(state);
    let preparation_context = ctx.clone();
    let checkout_id = format!(
        "{}{}-{}",
        preparation.checkout_label,
        if selected { "-selected" } else { "" },
        uuid::Uuid::new_v4()
    );
    tokio::task::spawn_blocking(move || {
        let mut prepared = match preparation.consumer_kind {
            ryeos_app::operator_external_content::BindConsumerKind::InstalledBundle => {
                ryeos_executor::execution::project_source::prepare_installed_bundle_external_consumer(
                    &preparation_state,
                    &preparation.consumer_ref,
                    preparation_context.fingerprint.clone(),
                    preparation_context.scopes.clone(),
                )?
            }
            ryeos_app::operator_external_content::BindConsumerKind::PinnedProject => {
                ryeos_executor::execution::project_source::prepare_pinned_project_external_consumer(
                    &preparation_state,
                    &preparation.consumer_ref,
                    preparation
                        .project_snapshot_hash
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("pinned-project consumer requires a snapshot"))?,
                    preparation
                        .project_path
                        .ok_or_else(|| anyhow::anyhow!("pinned-project consumer requires a project path"))?,
                    preparation_context.fingerprint.clone(),
                    preparation_context.scopes.clone(),
                    &checkout_id,
                )?
            }
        };
        if let Some(selections) = preparation.product_selections.as_deref() {
            prepared.select_products_only(
                &preparation_state,
                product_owner_context
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("selected consumer requires a product owner"))?,
                selections,
                qualification_project_context_resolver,
            )?;
        }
        anyhow::Ok(prepared)
    })
    .await
    .map_err(|error| anyhow::anyhow!("external-content consumer preparation panicked: {error}"))?
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/bind",
    endpoint: "external-content.bind",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/bind"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};
