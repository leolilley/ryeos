//! UI-specific state, owned by the UI crate.
//!
//! `UiState` holds browser session and session bus state that was previously
//! on `AppState`. The daemon composition root creates `UiState` and injects
//! it via the generic `AppState::extensions` typed state bag.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::browser_session::BrowserSessionStore;
use crate::session_bus::SessionBus;
use ryeos_executor::execution::effective_program_projection::{
    EffectiveProgramProjection, EffectiveProgramProjectionCache,
};

const FIELD_PROJECTION_CACHE_MAX_ENTRIES: usize = 128;

#[derive(Default)]
struct FieldProjectionCacheState {
    entries: HashMap<String, EffectiveProgramProjection>,
    order: VecDeque<String>,
}

#[derive(Default)]
struct FieldProjectionCache {
    state: Mutex<FieldProjectionCacheState>,
}

impl EffectiveProgramProjectionCache for FieldProjectionCache {
    fn get(&self, key: &str) -> Option<EffectiveProgramProjection> {
        let mut state = self.state.lock().ok()?;
        let projection = state.entries.get(key)?.clone();
        if let Some(position) = state.order.iter().position(|entry| entry == key) {
            state.order.remove(position);
        }
        state.order.push_back(key.to_string());
        Some(projection)
    }

    fn insert(&self, key: String, projection: EffectiveProgramProjection) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(position) = state.order.iter().position(|entry| entry == &key) {
            state.order.remove(position);
        }
        state.entries.insert(key.clone(), projection);
        state.order.push_back(key);
        while state.entries.len() > FIELD_PROJECTION_CACHE_MAX_ENTRIES {
            let Some(evicted) = state.order.pop_front() else {
                break;
            };
            state.entries.remove(&evicted);
        }
    }
}

#[derive(Clone)]
pub struct UiState {
    pub browser_sessions: Arc<BrowserSessionStore>,
    pub session_bus: Arc<SessionBus>,
    field_token_key: Arc<[u8; 32]>,
    field_projection_cache: Arc<FieldProjectionCache>,
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
            field_projection_cache: Arc::new(FieldProjectionCache::default()),
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

    pub(crate) fn field_projection_cache(&self) -> &dyn EffectiveProgramProjectionCache {
        self.field_projection_cache.as_ref()
    }
}

/// Retrieve `UiState` from the typed extension bag on `AppState`.
///
/// Returns `None` if the extension is not set (e.g., in API-only tests).
pub fn get_ui_state(state: &ryeos_app::state::AppState) -> Option<Arc<UiState>> {
    state.extensions.get::<UiState>()
}
