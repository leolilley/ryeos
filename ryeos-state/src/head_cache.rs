//! In-memory verified head cache for chain states.
//!
//! The daemon owns the HeadCache and passes it to state operations for
//! fast reads. Updated on local writes, invalidated on sync/import/startup.

use std::collections::HashMap;
use std::time::Instant;

use crate::objects::ChainState;

/// A cached chain head entry.
#[derive(Debug, Clone)]
pub struct CachedHead {
    /// CAS content hash of the chain state.
    pub chain_state_hash: String,
    /// The chain state object itself.
    pub chain_state: ChainState,
    /// When this cache entry was verified/updated.
    pub verified_at: Instant,
}

impl CachedHead {
    /// Create a new cached head entry.
    pub fn new(chain_state_hash: impl Into<String>, chain_state: ChainState) -> Self {
        Self {
            chain_state_hash: chain_state_hash.into(),
            chain_state,
            verified_at: Instant::now(),
        }
    }
}

/// In-memory cache of verified chain heads.
///
/// Keyed by `chain_root_id`. The daemon updates this on local writes
/// and invalidates entries on sync/import/startup.
pub struct HeadCache {
    heads: HashMap<String, CachedHead>,
}

impl HeadCache {
    /// Create a new empty head cache.
    pub fn new() -> Self {
        Self {
            heads: HashMap::new(),
        }
    }

    /// Get a cached head by chain_root_id.
    pub fn get(&self, chain_root_id: &str) -> Option<&CachedHead> {
        self.heads.get(chain_root_id)
    }

    /// Get just the chain state hash for a cached chain.
    pub fn get_hash(&self, chain_root_id: &str) -> Option<&str> {
        self.heads.get(chain_root_id).map(|h| h.chain_state_hash.as_str())
    }

    /// Check if a chain_root_id is in the cache.
    pub fn contains(&self, chain_root_id: &str) -> bool {
        self.heads.contains_key(chain_root_id)
    }

    /// Update (insert or replace) a cached head.
    pub fn update(&mut self, chain_root_id: impl Into<String>, cached: CachedHead) {
        self.heads.insert(chain_root_id.into(), cached);
    }

    /// Insert a new cached head. Returns `false` if already present.
    pub fn insert(
        &mut self,
        chain_root_id: impl Into<String>,
        cached: CachedHead,
    ) -> bool {
        self.heads
            .insert(chain_root_id.into(), cached)
            .is_none()
    }

    /// Invalidate (remove) a cached head. Returns the removed entry if present.
    pub fn invalidate(&mut self, chain_root_id: &str) -> Option<CachedHead> {
        self.heads.remove(chain_root_id)
    }

    /// Clear all cached heads.
    pub fn clear(&mut self) {
        self.heads.clear();
    }

    /// Number of cached chain heads.
    pub fn len(&self) -> usize {
        self.heads.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }

    /// Get all chain_root_ids in the cache.
    pub fn chain_ids(&self) -> Vec<&str> {
        self.heads.keys().map(|s| s.as_str()).collect()
    }

    /// Get all cached entries as (chain_root_id, CachedHead) pairs.
    pub fn entries(&self) -> Vec<(&str, &CachedHead)> {
        self.heads
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    }
}

impl Default for HeadCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::chain_state::{ChainStateBuilder, ChainThreadEntry};
    use crate::objects::thread_snapshot::ThreadStatus;

    fn make_cached_head(suffix: &str) -> CachedHead {
        let entry = ChainThreadEntry {
            snapshot_hash: "a".repeat(64),
            last_event_hash: None,
            last_thread_seq: 1,
            status: ThreadStatus::Running,
        };
        let state = ChainStateBuilder::new(format!("T-root-{suffix}"))
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread(format!("T-root-{suffix}"), entry)
            .build();
        let hash = crate::objects::chain_state::hash_chain_state(&state);
        CachedHead::new(hash, state)
    }

    #[test]
    fn cache_new_is_empty() {
        let cache = HeadCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_default_is_empty() {
        let cache = HeadCache::default();
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_insert_and_get() {
        let mut cache = HeadCache::new();
        let head = make_cached_head("1");
        assert!(cache.insert("T-root-1", head));

        let retrieved = cache.get("T-root-1").unwrap();
        assert_eq!(retrieved.chain_state.chain_root_id, "T-root-1");
    }

    #[test]
    fn cache_insert_duplicate_returns_false() {
        let mut cache = HeadCache::new();
        let head = make_cached_head("1");
        assert!(cache.insert("T-root-1", head));
        assert!(!cache.insert("T-root-1", make_cached_head("1")));
    }

    #[test]
    fn cache_update_overwrites() {
        let mut cache = HeadCache::new();
        cache.insert("T-root-1", make_cached_head("1"));
        cache.update("T-root-1", make_cached_head("1"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_get_hash() {
        let mut cache = HeadCache::new();
        let head = make_cached_head("1");
        let hash = head.chain_state_hash.clone();
        cache.insert("T-root-1", head);
        assert_eq!(cache.get_hash("T-root-1"), Some(hash.as_str()));
    }

    #[test]
    fn cache_get_missing_returns_none() {
        let cache = HeadCache::new();
        assert!(cache.get("missing").is_none());
        assert!(cache.get_hash("missing").is_none());
    }

    #[test]
    fn cache_contains() {
        let mut cache = HeadCache::new();
        assert!(!cache.contains("T-root-1"));
        cache.insert("T-root-1", make_cached_head("1"));
        assert!(cache.contains("T-root-1"));
    }

    #[test]
    fn cache_invalidate() {
        let mut cache = HeadCache::new();
        cache.insert("T-root-1", make_cached_head("1"));
        let removed = cache.invalidate("T-root-1");
        assert!(removed.is_some());
        assert!(!cache.contains("T-root-1"));
    }

    #[test]
    fn cache_invalidate_missing() {
        let mut cache = HeadCache::new();
        let removed = cache.invalidate("missing");
        assert!(removed.is_none());
    }

    #[test]
    fn cache_clear() {
        let mut cache = HeadCache::new();
        cache.insert("T-root-1", make_cached_head("1"));
        cache.insert("T-root-2", make_cached_head("2"));
        cache.insert("T-root-3", make_cached_head("3"));
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_len() {
        let mut cache = HeadCache::new();
        assert_eq!(cache.len(), 0);
        cache.insert("T-root-1", make_cached_head("1"));
        assert_eq!(cache.len(), 1);
        cache.insert("T-root-2", make_cached_head("2"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn cache_chain_ids() {
        let mut cache = HeadCache::new();
        cache.insert("T-root-2", make_cached_head("2"));
        cache.insert("T-root-1", make_cached_head("1"));
        cache.insert("T-root-3", make_cached_head("3"));

        let mut ids = cache.chain_ids();
        ids.sort();
        assert_eq!(ids, vec!["T-root-1", "T-root-2", "T-root-3"]);
    }

    #[test]
    fn cache_entries() {
        let mut cache = HeadCache::new();
        cache.insert("T-root-1", make_cached_head("1"));
        cache.insert("T-root-2", make_cached_head("2"));

        let entries = cache.entries();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn cached_head_new() {
        let entry = ChainThreadEntry {
            snapshot_hash: "a".repeat(64),
            last_event_hash: None,
            last_thread_seq: 0,
            status: ThreadStatus::Created,
        };
        let state = ChainStateBuilder::new("T-root")
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", entry)
            .build();
        let cached = CachedHead::new("hash123", state.clone());
        assert_eq!(cached.chain_state_hash, "hash123");
        assert_eq!(cached.chain_state.chain_root_id, "T-root");
    }
}
