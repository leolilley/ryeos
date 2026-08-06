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
    field_token_key: Arc<[u8; 32]>,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        Self {
            browser_sessions: Arc::new(BrowserSessionStore::new()),
            session_bus: Arc::new(SessionBus::new()),
            field_token_key: Arc::new(rand::random()),
        }
    }

    pub(crate) fn sign_field_token(&self, message: &[u8]) -> String {
        lillux::crypto::hmac_sha256_hex(self.field_token_key.as_ref(), message)
    }

    pub(crate) fn verify_field_token_mac(&self, message: &[u8], supplied: &str) -> bool {
        let expected = self.sign_field_token(message);
        if expected.len() != supplied.len() {
            return false;
        }
        expected
            .bytes()
            .zip(supplied.bytes())
            .fold(0u8, |different, (left, right)| different | (left ^ right))
            == 0
    }
}

/// Retrieve `UiState` from the typed extension bag on `AppState`.
///
/// Returns `None` if the extension is not set (e.g., in API-only tests).
pub fn get_ui_state(state: &ryeos_app::state::AppState) -> Option<Arc<UiState>> {
    state.extensions.get::<UiState>()
}
