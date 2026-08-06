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

/// Count-bounded LRU keyed by string. Deliberately count- rather than
/// byte-bounded: entries retain full source closures, so the cap trades a
/// bounded number of large entries for eviction simplicity on a
/// single-operator node.
#[derive(Default)]
struct BoundedLruState<V> {
    entries: HashMap<String, V>,
    order: VecDeque<String>,
}

#[derive(Default)]
struct BoundedLru<V> {
    state: Mutex<BoundedLruState<V>>,
    max_entries: usize,
}

impl<V: Clone> BoundedLru<V> {
    fn with_capacity(max_entries: usize) -> Self {
        Self {
            state: Mutex::new(BoundedLruState {
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
            max_entries,
        }
    }

    fn get(&self, key: &str) -> Option<V> {
        let mut state = self.state.lock().ok()?;
        let value = state.entries.get(key)?.clone();
        if let Some(position) = state.order.iter().position(|entry| entry == key) {
            state.order.remove(position);
        }
        state.order.push_back(key.to_string());
        Some(value)
    }

    fn insert(&self, key: String, value: V) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(position) = state.order.iter().position(|entry| entry == &key) {
            state.order.remove(position);
        }
        state.entries.insert(key.clone(), value);
        state.order.push_back(key);
        while state.entries.len() > self.max_entries {
            let Some(evicted) = state.order.pop_front() else {
                break;
            };
            state.entries.remove(&evicted);
        }
    }
}

struct FieldProjectionCache {
    lru: BoundedLru<EffectiveProgramProjection>,
}

impl Default for FieldProjectionCache {
    fn default() -> Self {
        Self {
            lru: BoundedLru::with_capacity(FIELD_PROJECTION_CACHE_MAX_ENTRIES),
        }
    }
}

impl EffectiveProgramProjectionCache for FieldProjectionCache {
    fn get(&self, key: &str) -> Option<EffectiveProgramProjection> {
        self.lru.get(key)
    }

    fn insert(&self, key: String, projection: EffectiveProgramProjection) {
        self.lru.insert(key, projection)
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

#[cfg(test)]
mod tests {
    use super::BoundedLru;

    #[test]
    fn lru_evicts_least_recently_used_at_capacity() {
        let lru: BoundedLru<u32> = BoundedLru::with_capacity(2);
        lru.insert("a".into(), 1);
        lru.insert("b".into(), 2);
        // Touch `a` so `b` becomes least recently used.
        assert_eq!(lru.get("a"), Some(1));
        lru.insert("c".into(), 3);
        assert_eq!(lru.get("b"), None, "least-recently-used entry must evict");
        assert_eq!(lru.get("a"), Some(1));
        assert_eq!(lru.get("c"), Some(3));
    }

    #[test]
    fn reinserting_a_key_refreshes_without_duplicating() {
        let lru: BoundedLru<u32> = BoundedLru::with_capacity(2);
        lru.insert("a".into(), 1);
        lru.insert("a".into(), 10);
        lru.insert("b".into(), 2);
        lru.insert("c".into(), 3);
        assert_eq!(lru.get("a"), None, "refreshed key still evicts in order");
        assert_eq!(lru.get("b"), Some(2));
        assert_eq!(lru.get("c"), Some(3));
    }
}
