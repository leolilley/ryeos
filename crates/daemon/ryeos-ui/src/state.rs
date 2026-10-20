//! UI-specific state, owned by the UI crate.
//!
//! `UiState` holds browser session and session bus state that was previously
//! on `AppState`. The daemon composition root creates `UiState` and injects
//! it via the generic `AppState::extensions` typed state bag.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use crate::browser_session::BrowserSessionStore;
use crate::session_bus::SessionBus;
use ryeos_client_base::ui::composition_replay::UiCompositionReplay;
use ryeos_executor::execution::effective_program_projection::{
    EffectiveProgramProjection, EffectiveProgramProjectionCache,
};
use ryeos_ui_presentation_contract::seat_control::{UiSeatControlPredecessor, UiSeatProducer};

const FIELD_PROJECTION_CACHE_MAX_ENTRIES: usize = 128;
const COMPOSITION_CHECKPOINT_CACHE_MAX_ENTRIES: usize = 32;

/// Identity of one original presentation history, not a grant of authority.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CompositionCheckpointKey {
    pub chain_root_id: String,
    pub thread_id: String,
    pub presentation_id: String,
    pub initialization_digest: String,
}

/// A process-local acceleration point. Callers must prove this tip belongs to
/// the freshly captured signed history before using the derived replay.
#[derive(Clone)]
pub(crate) struct CompositionCheckpoint {
    pub tip_hash: String,
    pub thread_seq: u64,
    pub chain_seq: u64,
    pub replay: UiCompositionReplay,
    pub next_engine_seq: u64,
    pub control: Option<UiSeatProducer>,
    pub control_predecessor: UiSeatControlPredecessor,
    /// Signed transfer testimony was observed; not ordinary mutation authority.
    pub transfer_testimony_seen: bool,
}

#[derive(Default)]
struct CompositionCheckpointCacheState {
    entries: HashMap<CompositionCheckpointKey, CompositionCheckpointCacheEntry>,
    order: VecDeque<CompositionCheckpointKey>,
}

#[derive(Default)]
struct CompositionCheckpointCacheEntry {
    composition: Option<CompositionCheckpoint>,
}

#[derive(Default)]
struct CompositionCheckpointCache {
    state: Mutex<CompositionCheckpointCacheState>,
}

impl CompositionCheckpointCache {
    fn get(&self, key: &CompositionCheckpointKey) -> Option<CompositionCheckpoint> {
        let mut state = self.state.lock().ok()?;
        let checkpoint = state.entries.get(key)?.composition.clone()?;
        if let Some(position) = state.order.iter().position(|entry| entry == key) {
            state.order.remove(position);
        }
        state.order.push_back(key.clone());
        Some(checkpoint)
    }

    fn put(&self, key: CompositionCheckpointKey, checkpoint: CompositionCheckpoint) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.entries.contains_key(&key)
            && let Some(position) = state.order.iter().position(|entry| entry == &key)
        {
            state.order.remove(position);
        }
        state.entries.entry(key.clone()).or_default().composition = Some(checkpoint);
        state.order.push_back(key);
        while state.entries.len() > COMPOSITION_CHECKPOINT_CACHE_MAX_ENTRIES {
            let Some(evicted) = state.order.pop_front() else {
                break;
            };
            state.entries.remove(&evicted);
        }
    }

    fn invalidate(&self, key: &CompositionCheckpointKey) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.entries.remove(key).is_some()
            && let Some(position) = state.order.iter().position(|entry| entry == key)
        {
            state.order.remove(position);
        }
    }
}

/// Count-bounded LRU keyed by string. Deliberately count- rather than
/// byte-bounded: entries retain full source closures, so the cap trades a
/// bounded number of large entries for eviction simplicity on a
/// single-operator node.
#[derive(Default)]
struct BoundedLruState<K, V> {
    entries: HashMap<K, V>,
    order: VecDeque<K>,
}

#[derive(Default)]
struct BoundedLru<K, V> {
    state: Mutex<BoundedLruState<K, V>>,
    max_entries: usize,
}

impl<K: Clone + Eq + Hash, V: Clone> BoundedLru<K, V> {
    fn with_capacity(max_entries: usize) -> Self {
        Self {
            state: Mutex::new(BoundedLruState {
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
            max_entries,
        }
    }

    fn get(&self, key: &K) -> Option<V> {
        let mut state = self.state.lock().ok()?;
        let value = state.entries.get(key)?.clone();
        if let Some(position) = state.order.iter().position(|entry| entry == key) {
            state.order.remove(position);
        }
        state.order.push_back(key.clone());
        Some(value)
    }

    fn insert(&self, key: K, value: V) {
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

    #[cfg(test)]
    fn remove(&self, key: &K) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.entries.remove(key);
        if let Some(position) = state.order.iter().position(|entry| entry == key) {
            state.order.remove(position);
        }
    }
}

struct FieldProjectionCache {
    lru: BoundedLru<String, EffectiveProgramProjection>,
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
        self.lru.get(&key.to_owned())
    }

    fn insert(&self, key: String, projection: EffectiveProgramProjection) {
        self.lru.insert(key, projection)
    }
}

#[derive(Clone)]
pub struct UiState {
    pub browser_sessions: Arc<BrowserSessionStore>,
    pub session_bus: Arc<SessionBus>,
    seat_transition: Arc<Mutex<()>>,
    field_token_key: Arc<[u8; 32]>,
    field_projection_cache: Arc<FieldProjectionCache>,
    composition_checkpoints: Arc<CompositionCheckpointCache>,
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
            seat_transition: Arc::new(Mutex::new(())),
            field_token_key: Arc::new(rand::random()),
            field_projection_cache: Arc::new(FieldProjectionCache::default()),
            composition_checkpoints: Arc::new(CompositionCheckpointCache::default()),
        }
    }

    /// Serialize exact-session seat creation with predecessor activation.
    /// This is deliberately one bounded process-local lock, not an unbounded
    /// per-session lock registry; seat lifecycle operations are rare.
    pub(crate) fn lock_seat_transition(
        &self,
    ) -> std::sync::LockResult<std::sync::MutexGuard<'_, ()>> {
        self.seat_transition.lock()
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

    pub(crate) fn composition_checkpoint_get(
        &self,
        key: &CompositionCheckpointKey,
    ) -> Option<CompositionCheckpoint> {
        self.composition_checkpoints.get(key)
    }

    pub(crate) fn composition_checkpoint_put(
        &self,
        key: CompositionCheckpointKey,
        checkpoint: CompositionCheckpoint,
    ) {
        self.composition_checkpoints.put(key, checkpoint);
    }

    pub(crate) fn composition_checkpoint_invalidate(&self, key: &CompositionCheckpointKey) {
        self.composition_checkpoints.invalidate(key);
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
    use super::{BoundedLru, CompositionCheckpointKey};

    #[test]
    fn lru_evicts_least_recently_used_at_capacity() {
        let lru: BoundedLru<String, u32> = BoundedLru::with_capacity(2);
        lru.insert("a".into(), 1);
        lru.insert("b".into(), 2);
        // Touch `a` so `b` becomes least recently used.
        assert_eq!(lru.get(&"a".into()), Some(1));
        lru.insert("c".into(), 3);
        assert_eq!(
            lru.get(&"b".into()),
            None,
            "least-recently-used entry must evict"
        );
        assert_eq!(lru.get(&"a".into()), Some(1));
        assert_eq!(lru.get(&"c".into()), Some(3));
    }

    #[test]
    fn reinserting_a_key_refreshes_without_duplicating() {
        let lru: BoundedLru<String, u32> = BoundedLru::with_capacity(2);
        lru.insert("a".into(), 1);
        lru.insert("a".into(), 10);
        lru.insert("b".into(), 2);
        lru.insert("c".into(), 3);
        assert_eq!(
            lru.get(&"a".into()),
            None,
            "refreshed key still evicts in order"
        );
        assert_eq!(lru.get(&"b".into()), Some(2));
        assert_eq!(lru.get(&"c".into()), Some(3));
    }

    #[test]
    fn checkpoint_keys_separate_every_identity_component_and_evict() {
        let lru: BoundedLru<CompositionCheckpointKey, u32> = BoundedLru::with_capacity(2);
        let base = CompositionCheckpointKey {
            chain_root_id: "root".into(),
            thread_id: "thread".into(),
            presentation_id: "presentation".into(),
            initialization_digest: "digest".into(),
        };
        let mut other_thread = base.clone();
        other_thread.thread_id = "other".into();
        let mut other_digest = base.clone();
        other_digest.initialization_digest = "other".into();
        let mut other_root = base.clone();
        other_root.chain_root_id = "other".into();
        let mut other_presentation = base.clone();
        other_presentation.presentation_id = "other".into();
        assert_ne!(base, other_root);
        assert_ne!(base, other_thread);
        assert_ne!(base, other_presentation);
        assert_ne!(base, other_digest);
        lru.insert(base.clone(), 1);
        lru.insert(other_thread.clone(), 2);
        assert_eq!(lru.get(&base), Some(1));
        lru.insert(other_digest.clone(), 3);
        assert_eq!(lru.get(&other_thread), None);
        assert_eq!(lru.get(&base), Some(1));
        assert_eq!(lru.get(&other_digest), Some(3));
        lru.remove(&base);
        assert_eq!(lru.get(&base), None);
    }
}
