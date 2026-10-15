//! ryeos-api: HTTP route table, service handlers, remote outbound client,
//! and the API-side daemon state.
//!
//! Owns the `ApiState` (router state), the compiled route table, response
//! modes, invokers, per-endpoint service handlers, the daemon-side
//! `ServiceRegistry` builder, and the remote-daemon client.

/// Owner token carried by the secure temporaries this crate publishes.
pub(crate) const API_TEMP_PURPOSE: lillux::TempPurpose = lillux::TempPurpose::new("api");

#[cfg(test)]
extern crate self as ryeos_api;

pub mod api_state;
pub mod auth;
pub mod handler_context;
pub mod handler_error;
pub mod handlers;
pub mod maintenance;
pub mod project_deploy;
pub(crate) mod project_namespace;
pub mod publisher_server;
pub mod registry;
pub mod remote;
pub mod route_error;
pub mod routes;
pub mod scheduler_selection;
pub mod surface_views;

pub use api_state::ApiState;
pub use registry::{RawHandlerFn, ServiceAvailability, ServiceDescriptor, build_service_registry};

/// Build the axum router for the API surface.
///
/// All requests are dispatched through the route-table fallback; per-route
/// auth/invocation/response-mode is compiled from `NodeConfigSnapshot.routes`.
pub fn build_router(state: ApiState) -> axum::Router {
    axum::Router::new()
        .fallback(routes::dispatcher::route_dispatcher)
        .with_state(state)
}
