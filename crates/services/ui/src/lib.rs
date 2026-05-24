//! RyeOS UI service layer.
//!
//! Browser sessions, `/ui` service handlers, and UI-specific route invokers
//! live here so `ryeos-api` can remain generic HTTP/route substrate.

pub mod browser_session;
pub mod handlers;
pub mod invokers;
pub mod session_bus;

pub use browser_session::BrowserSessionStore;
pub use session_bus::SessionBus;

pub fn route_extensions() -> ryeos_api::routes::RouteExtensionRegistry {
    ryeos_api::routes::RouteExtensionRegistry {
        auth: ryeos_api::routes::invokers::AuthInvokerRegistry {
            browser_session: Some(std::sync::Arc::new(
                invokers::browser_session_invocation::CompiledBrowserSessionVerifier,
            )),
        },
    }
}

pub fn response_mode_registry() -> ryeos_api::routes::response_modes::ResponseModeRegistry {
    ryeos_api::routes::response_modes::ResponseModeRegistry::with_builtins_and_session_events(
        std::sync::Arc::new(invokers::session_events_invocation::CompiledSessionEventsInvocation {
            keep_alive_secs: 15,
        }),
    )
}
