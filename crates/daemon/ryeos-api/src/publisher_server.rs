//! Narrow HTTP transport for an externally-custodied bundle publisher.
//!
//! This router deliberately exposes only the three typed operations consumed
//! by `AuthenticatedPublisherClient`. It is not part of the ordinary daemon
//! API and must be served on loopback or behind an HTTPS terminator.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use ryeos_app::bundle_publication::producer::{
    BundlePublisherAuthority, BundleReleaseOperation, CatalogRequestPublicationRequest,
    RequestAuthorizationRequest, RequestTreeSigningRequest,
};
use serde_json::json;
use subtle::ConstantTimeEq as _;
use zeroize::Zeroizing;

const MAX_BODY_BYTES: usize = 256 * 1024;

pub struct PublisherServerState {
    authority: Arc<dyn BundlePublisherAuthority>,
    bearer: Arc<Zeroizing<String>>,
}

impl Clone for PublisherServerState {
    fn clone(&self) -> Self {
        Self {
            authority: Arc::clone(&self.authority),
            bearer: Arc::clone(&self.bearer),
        }
    }
}

impl PublisherServerState {
    pub fn new(
        authority: Arc<dyn BundlePublisherAuthority>,
        bearer: String,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !bearer.is_empty() && bearer.len() <= 4096,
            "publisher bearer must contain 1..=4096 bytes"
        );
        anyhow::ensure!(
            !bearer.bytes().any(|byte| byte.is_ascii_control()),
            "publisher bearer must not contain control characters"
        );
        Ok(Self {
            authority,
            bearer: Arc::new(Zeroizing::new(bearer)),
        })
    }
}

/// Construct the complete publisher surface. Callers cannot add a generic
/// signing route through this API.
pub fn router(state: PublisherServerState) -> Router {
    Router::new()
        .route("/v1/bundle-tree/sign", post(sign_tree))
        .route(
            "/v1/bundle-generation/authorize",
            post(authorize_generation),
        )
        .route(
            "/v1/bundle-catalog/authorize-successor",
            post(authorize_catalog_successor),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn sign_tree(
    State(state): State<PublisherServerState>,
    headers: HeaderMap,
    Json(request): Json<RequestTreeSigningRequest>,
) -> Response {
    execute(
        &state,
        &headers,
        BundleReleaseOperation::RequestTreeSigning(request.clone()),
        state.authority.sign_tree(request),
    )
    .await
}

async fn authorize_generation(
    State(state): State<PublisherServerState>,
    headers: HeaderMap,
    Json(request): Json<RequestAuthorizationRequest>,
) -> Response {
    execute(
        &state,
        &headers,
        BundleReleaseOperation::RequestAuthorization(request.clone()),
        state.authority.authorize_generation(request),
    )
    .await
}

async fn authorize_catalog_successor(
    State(state): State<PublisherServerState>,
    headers: HeaderMap,
    Json(request): Json<CatalogRequestPublicationRequest>,
) -> Response {
    execute(
        &state,
        &headers,
        BundleReleaseOperation::CatalogRequestPublication(request.clone()),
        state.authority.authorize_catalog_successor(request),
    )
    .await
}

async fn execute(
    state: &PublisherServerState,
    headers: &HeaderMap,
    operation: BundleReleaseOperation,
    future: ryeos_app::bundle_publication::producer::ProducerFuture<'_>,
) -> Response {
    if !authorized(headers, state.bearer.as_str()) {
        return error(StatusCode::UNAUTHORIZED, "publisher authentication failed");
    }
    if let Err(error) = operation.validate() {
        return error(StatusCode::BAD_REQUEST, &error.to_string());
    }
    match future.await {
        Ok(value) => Json(value).into_response(),
        Err(error_value) => error(StatusCode::UNPROCESSABLE_ENTITY, &error_value.to_string()),
    }
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(candidate) = value.strip_prefix("Bearer ") else {
        return false;
    };
    candidate.len() == expected.len() && bool::from(candidate.as_bytes().ct_eq(expected.as_bytes()))
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
