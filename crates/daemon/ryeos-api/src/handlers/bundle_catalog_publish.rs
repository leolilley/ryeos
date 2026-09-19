//! Publisher-authenticated bundle catalog head publication.
//!
//! Catalog construction and signing remain producer-authority operations. This
//! endpoint is the distinct bundle-source mutation boundary: it derives the
//! publisher principal exclusively from authenticated handler context and
//! admits the already publisher-authored candidate through the catalog CAS.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use ryeos_app::{
    bundle_publication::catalog::{CatalogPublicationRequest, publish_catalog},
    handler_context::HandlerContext,
    state::AppState,
};
use ryeos_executor::executor::ServiceAvailability;
use serde_json::Value;

use crate::registry::ServiceDescriptor;

#[derive(Debug, serde::Deserialize)]
#[serde(transparent)]
pub struct ExplicitExpectedCatalogHead(Option<String>);

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub catalog_namespace: String,
    pub candidate_publication_attestation_hash: String,
    /// Mandatory and nullable: `null` is the explicit genesis fence.
    pub expected_catalog_head: ExplicitExpectedCatalogHead,
    pub upload_session_id: String,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ctx.require_verified().map_err(|error| anyhow!(error))?;
    let outcome = publish_catalog(
        &state,
        CatalogPublicationRequest {
            authenticated_principal: ctx.fingerprint,
            catalog_namespace: req.catalog_namespace,
            candidate_attestation_hash: req.candidate_publication_attestation_hash,
            expected_current: req.expected_catalog_head.0,
            upload_session_id: req.upload_session_id,
        },
    )?;
    Ok(serde_json::to_value(outcome)?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-catalog/publish",
    endpoint: "bundle_catalog.publish",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-catalog/publish"],
    handler: |params, context, state| {
        Box::pin(async move {
            let request = crate::handler_error::parse_request(params)?;
            handle(request, context, state).await
        })
    },
};
