//! ryeos-api: HTTP route table, service handlers, remote outbound client,
//! and the API-side daemon state.
//!
//! Owns the `ApiState` (router state), the compiled route table, response
//! modes, invokers, per-endpoint service handlers, the daemon-side
//! `ServiceRegistry` builder, and the remote-daemon client.

pub mod api_state;
pub mod auth;
pub mod handler_context;
pub mod handler_error;
pub mod handlers;
pub mod maintenance;
pub mod registry;
pub mod remote;
pub mod route_error;
pub mod routes;

pub use api_state::ApiState;
pub use registry::{build_service_registry, RawHandlerFn, ServiceAvailability, ServiceDescriptor};

/// Build the axum router for the API surface.
///
/// All requests are dispatched through the route-table fallback; per-route
/// auth/invocation/response-mode is compiled from `NodeConfigSnapshot.routes`.
pub fn build_router(state: ApiState) -> axum::Router {
    axum::Router::new()
        .fallback(routes::dispatcher::route_dispatcher)
        .with_state(state)
}
