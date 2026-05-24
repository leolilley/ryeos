//! RyeOS UI service layer.
//!
//! Browser sessions, `/ui` service handlers, and UI-specific route invokers
//! live here so `ryeos-api` can remain generic HTTP/route substrate.

pub mod assets;
pub mod browser_session;
pub mod handlers;
pub mod invokers;
pub mod session_bus;
pub mod state;

pub use browser_session::{BrowserSession, BrowserSessionStore, LaunchContext};
pub use session_bus::SessionBus;
pub use state::UiState;

/// Register UI extensions into the provided registries.
///
/// The daemon calls this during composition to add UI-specific auth
/// verifiers, stream sources, and static asset providers to the generic
/// API registries.
pub fn register_extensions(
    route_extensions: &mut ryeos_api::routes::RouteExtensionRegistry,
    response_modes: &mut ryeos_api::routes::response_modes::ResponseModeRegistry,
    ui: std::sync::Arc<UiState>,
) {
    // Register browser_session auth verifier.
    route_extensions.auth.register(
        "browser_session",
        std::sync::Arc::new(
            invokers::browser_session_invocation::BrowserSessionAuthFactory {
                ui: ui.clone(),
            },
        ),
    );

    // Register session_events stream source + web asset provider.
    response_modes.register_event_stream_source(
        "session_events",
        std::sync::Arc::new(
            invokers::session_events_invocation::SessionEventsSourceFactory {
                ui: ui.clone(),
            },
        ),
    );
    response_modes.set_static_asset_provider(
        std::sync::Arc::new(assets::WebAssetProvider),
    );
}
