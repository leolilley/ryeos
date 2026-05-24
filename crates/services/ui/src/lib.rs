//! RyeOS UI service layer.
//!
//! Browser sessions, `/ui` service handlers, and UI-specific route invokers
//! live here so `ryeos-api` can remain generic HTTP/route substrate.

pub mod browser_session;
pub mod handlers;
pub mod invokers;
pub mod session_bus;
pub mod state;

pub use browser_session::{BrowserSession, BrowserSessionStore, LaunchContext};
pub use session_bus::SessionBus;
pub use state::UiState;

pub fn route_extensions(
    ui: std::sync::Arc<UiState>,
) -> ryeos_api::routes::RouteExtensionRegistry {
    ryeos_api::routes::RouteExtensionRegistry {
        auth: ryeos_api::routes::invokers::AuthInvokerRegistry {
            browser_session: Some(std::sync::Arc::new(
                invokers::browser_session_invocation::CompiledBrowserSessionVerifier { ui },
            )),
        },
    }
}

/// Build a response mode registry with UI extensions and the provided
/// service descriptor set.
///
/// The daemon passes the full composed descriptor table (API + UI) so
/// that `service:` refs in route YAML resolve against all known services.
pub fn response_mode_registry(
    service_descriptors: &'static [ryeos_api::registry::ServiceDescriptor],
    ui: std::sync::Arc<UiState>,
) -> ryeos_api::routes::response_modes::ResponseModeRegistry {
    ryeos_api::routes::response_modes::ResponseModeRegistry::with_builtins_and_session_events_from(
        service_descriptors,
        std::sync::Arc::new(invokers::session_events_invocation::CompiledSessionEventsInvocation {
            ui,
            keep_alive_secs: 15,
        }),
    )
}
