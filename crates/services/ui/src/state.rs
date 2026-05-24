//! UI-specific state, owned by the UI crate.
//!
//! `UiState` holds browser session and session bus state that was previously
//! on `AppState`. The daemon composition root creates `UiState` and injects
//! it via the generic `AppState::extensions` typed state bag.

use std::sync::Arc;

use crate::browser_session::BrowserSessionStore;
use crate::session_bus::SessionBus;

#[derive(Clone)]
pub struct UiState {
    pub browser_sessions: Arc<BrowserSessionStore>,
    pub session_bus: Arc<SessionBus>,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            browser_sessions: Arc::new(BrowserSessionStore::new()),
            session_bus: Arc::new(SessionBus::new()),
        }
    }
}

/// Retrieve `UiState` from the typed extension bag on `AppState`.
///
/// Returns `None` if the extension is not set (e.g., in API-only tests).
pub fn get_ui_state(state: &ryeos_app::state::AppState) -> Option<Arc<UiState>> {
    state.extensions.get::<UiState>()
}
