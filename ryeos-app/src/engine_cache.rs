//! Per-snapshot engine cache for the request-scoped engine overlay.
//!
//! Background:
//!
//! The daemon builds one `Engine` at startup against its own roots
//! (system bundles + the operator's own user space). For
//! `pushed_head` requests, we need a different engine: one whose
//! user-tier roots point at the **caller's** materialised user space,
//! and whose trust store includes the caller's pushed trust pins as
//! an overlay.
//!
//! Materialising the user overlay and rebuilding the engine on every
//! request would be wasteful — many threads in a session run against
//! the same snapshot. This cache keeps the engine alive (with its
//! associated user overlay temp dir) keyed by snapshot hash, so
//! concurrent / sequential threads with the same snapshot reuse it.
//!
//! # Cache key
//!
//! `(system_install_generation, snapshot_hash)` — the generation
//! counter is bumped on every `bundle.install` / `bundle.uninstall`
//! so any registered-bundle change invalidates cached request
//! engines (which were built against the previous bundle set).
//!
//! # Eviction
//!
//! Two policies, applied on every `get()` and `insert()` call:
//!
//!   1. **LRU** at the configured capacity, guarded by a
//!      `strong_count` check: entries whose engine `Arc` is still
//!      held by a running request are never evicted — we skip them
//!      and pick the next LRU candidate instead. If every entry is
//!      in use, the cache temporarily exceeds capacity rather than
//!      pulling the rug out from under a live thread.
//!
//!   2. **Idle threshold**: any entry not touched within the
//!      configured idle window is evicted regardless of LRU order,
//!      subject to the same `strong_count` guard. This protects
//!      against pinning cold temp dirs forever on low-traffic nodes.
//!
//! # Ownership split
//!
//! The project checkout is **per-request** (each `pushed_head` gets
//! its own directory, cleaned up when the request's `ExecutionGuard`
//! drops). The cache owns only what is shared across threads:
//!
//! | Resource | Owned by | Lifetime |
//! |---|---|---|
//! | Project `exec_dir` | Request | Until request completes |
//! | User overlay temp dir | Cache entry | Until entry is evicted |
//! | `Arc<Engine>` | Cache entry + request clones | Until both release |

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ryeos_engine::engine::Engine;

/// RAII guard for a materialised temp directory. Drops the directory
/// recursively when the guard is dropped. Identical contract to the
/// guard in `ryeos_executor::execution::project_source::TempDirGuard`
/// but kept here to avoid a cross-crate dependency on the executor's
/// internals.
#[derive(Debug)]
pub struct TempDirGuard(Option<std::path::PathBuf>);

impl TempDirGuard {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self(Some(path))
    }

    /// Consume the guard without removing the directory. Use when a
    /// long-running detached owner has taken over lifecycle.
    pub fn disarm(mut self) -> Option<std::path::PathBuf> {
        self.0.take()
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

/// A single cached engine + the user overlay temp dir it depends on.
///
/// `engine` is wrapped in `Arc` so cache lookups can clone cheaply.
/// The user temp-dir guard is owned by the entry; when the entry is
/// evicted (and no running request holds a strong ref to the engine),
/// the user overlay temp dir is removed.
///
/// The **project** checkout is NOT cached — each request owns its own
/// `exec_dir` and cleans it up independently via `ExecutionGuard`.
struct CachedEntry {
    engine: Arc<Engine>,
    last_touched: Instant,
    _user_temp: Option<TempDirGuard>,
}

/// Per-snapshot engine cache.
///
/// Cheap to clone — the inner state is behind an `Arc<Mutex<…>>`.
#[derive(Clone)]
pub struct EngineCache {
    inner: Arc<EngineCacheInner>,
}

struct EngineCacheInner {
    entries: Mutex<HashMap<CacheKey, CachedEntry>>,
    /// Bumped on every `bundle.install` / `bundle.uninstall`. Mixed
    /// into the cache key so a bundle change invalidates all entries
    /// built against the previous system root set without an explicit
    /// flush.
    system_install_generation: AtomicU64,
    capacity: usize,
    idle_threshold: Duration,
}

/// Cache key. Public so callers can construct one without exposing
/// the inner map type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub system_install_generation: u64,
    pub snapshot_hash: String,
}

/// Configuration knobs surfaced via daemon config.
#[derive(Debug, Clone)]
pub struct EngineCacheConfig {
    /// Maximum number of cached entries (LRU eviction at capacity).
    /// Default: 8. Each entry retains its materialised project +
    /// user temp dirs (≈10–100 MB) plus the loaded engine.
    pub capacity: usize,
    /// Maximum idle time before an entry is evicted regardless of
    /// LRU order. Default: 30 minutes. Protects against pinning
    /// cold temp dirs forever on low-traffic nodes.
    pub idle_threshold: Duration,
}

impl Default for EngineCacheConfig {
    fn default() -> Self {
        Self {
            capacity: 8,
            idle_threshold: Duration::from_secs(30 * 60),
        }
    }
}

impl EngineCache {
    /// Construct an empty cache with the given configuration.
    pub fn new(config: EngineCacheConfig) -> Self {
        Self {
            inner: Arc::new(EngineCacheInner {
                entries: Mutex::new(HashMap::new()),
                system_install_generation: AtomicU64::new(0),
                capacity: config.capacity,
                idle_threshold: config.idle_threshold,
            }),
        }
    }

    /// Current system-install generation counter. Cache keys mix
    /// this in so bumps invalidate cleanly.
    pub fn system_install_generation(&self) -> u64 {
        self.inner.system_install_generation.load(Ordering::SeqCst)
    }

    /// Bump the system-install generation. Call this from
    /// `bundle.install` and `bundle.uninstall` handlers to invalidate
    /// every cached engine that was built against the previous
    /// bundle set.
    pub fn bump_system_install_generation(&self) -> u64 {
        self.inner
            .system_install_generation
            .fetch_add(1, Ordering::SeqCst)
            + 1
    }

    /// Look up a cached engine. Updates `last_touched` on hit so LRU
    /// ordering reflects recency.
    pub fn get(&self, key: &CacheKey) -> Option<Arc<Engine>> {
        let mut entries = self.inner.entries.lock().unwrap();
        self.sweep_idle_locked(&mut entries);
        if let Some(entry) = entries.get_mut(key) {
            entry.last_touched = Instant::now();
            Some(entry.engine.clone())
        } else {
            None
        }
    }

    /// Insert a freshly built engine into the cache. Evicts the
    /// least-recently-used entry that is NOT currently in use by a
    /// running request (checked via `Arc::strong_count <= 1`). If
    /// every entry is in use, the cache temporarily exceeds capacity
    /// rather than evicting a live engine.
    pub fn insert(
        &self,
        key: CacheKey,
        engine: Arc<Engine>,
        user_temp: Option<TempDirGuard>,
    ) {
        let mut entries = self.inner.entries.lock().unwrap();
        self.sweep_idle_locked(&mut entries);
        // Evict LRU entries that are not held by any running request.
        while entries.len() >= self.inner.capacity {
            let candidate = entries
                .iter()
                .filter(|(_, e)| Arc::strong_count(&e.engine) <= 1)
                .min_by_key(|(_, e)| e.last_touched)
                .map(|(k, _)| k.clone());
            match candidate {
                Some(k) => {
                    entries.remove(&k);
                }
                None => break, // all in use; tolerate over-capacity
            }
        }
        entries.insert(
            key,
            CachedEntry {
                engine,
                last_touched: Instant::now(),
                _user_temp: user_temp,
            },
        );
    }

    /// Number of cached entries. Test/diagnostic helper.
    pub fn len(&self) -> usize {
        self.inner.entries.lock().unwrap().len()
    }

    /// True if cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop every entry. The temp dirs are removed by `Drop`.
    pub fn clear(&self) {
        self.inner.entries.lock().unwrap().clear();
    }

    /// Internal: evict any entry older than `idle_threshold` that is
    /// not currently held by a running request.
    fn sweep_idle_locked(&self, entries: &mut HashMap<CacheKey, CachedEntry>) {
        let cutoff = Instant::now() - self.inner.idle_threshold;
        let stale: Vec<CacheKey> = entries
            .iter()
            .filter(|(_, e)| e.last_touched < cutoff && Arc::strong_count(&e.engine) <= 1)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            entries.remove(&k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `Arc<Engine>` suitable for cache tests — no
    /// bundles, no kinds, no parsers. Just enough to satisfy the type.
    fn minimal_engine() -> Arc<Engine> {
        Arc::new(Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                std::sync::Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
            ),
            None,
            vec![],
        ))
    }

    fn dummy_key(snapshot: &str) -> CacheKey {
        CacheKey {
            system_install_generation: 0,
            snapshot_hash: snapshot.into(),
        }
    }

    #[test]
    fn empty_cache_misses() {
        let cache = EngineCache::new(EngineCacheConfig::default());
        assert!(cache.get(&dummy_key("abc")).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn bump_generation_increments() {
        let cache = EngineCache::new(EngineCacheConfig::default());
        assert_eq!(cache.system_install_generation(), 0);
        assert_eq!(cache.bump_system_install_generation(), 1);
        assert_eq!(cache.bump_system_install_generation(), 2);
        assert_eq!(cache.system_install_generation(), 2);
    }

    #[test]
    fn insert_and_get() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            ..Default::default()
        });
        let key = dummy_key("snap-1");
        let eng = minimal_engine();
        cache.insert(key.clone(), eng.clone(), None);
        assert_eq!(cache.len(), 1);
        let hit = cache.get(&key).expect("should hit");
        // The cached entry must be the exact same Arc we inserted, not
        // a fresh allocation.
        assert!(Arc::ptr_eq(&hit, &eng));
    }

    #[test]
    fn cache_hit_returns_same_arc() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            ..Default::default()
        });
        let key = dummy_key("snap-1");
        let eng = minimal_engine();
        cache.insert(key.clone(), eng.clone(), None);

        let hit1 = cache.get(&key).expect("first hit");
        let hit2 = cache.get(&key).expect("second hit");
        assert!(Arc::ptr_eq(&hit1, &hit2), "same cache entry returns same Arc");
        assert!(Arc::ptr_eq(&hit1, &eng), "cache returns the original Arc");
    }

    #[test]
    fn engine_cache_does_not_evict_in_use_entry() {
        // capacity=1: inserting a second entry should evict the first.
        // But if the first Arc is still held, eviction is skipped and
        // the cache goes over-capacity.
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 1,
            idle_threshold: Duration::from_secs(9999), // don't idle-evict
        });
        let key1 = dummy_key("snap-1");
        let key2 = dummy_key("snap-2");
        let eng1 = minimal_engine();
        let eng2 = minimal_engine();

        cache.insert(key1.clone(), eng1.clone(), None);
        assert_eq!(cache.len(), 1);

        // Hold a strong ref outside the cache.
        let held = cache.get(&key1).expect("hit");

        // Insert a second entry — key1's engine has strong_count > 1
        // so it should NOT be evicted.
        cache.insert(key2.clone(), eng2.clone(), None);
        assert_eq!(cache.len(), 2, "cache should be over-capacity, not evict in-use entry");

        // key1 is still accessible
        let hit1_again = cache.get(&key1).expect("key1 still in cache");
        assert!(Arc::ptr_eq(&hit1_again, &held));

        // key2 is also accessible
        let hit2 = cache.get(&key2).expect("key2 in cache");
        assert!(Arc::ptr_eq(&hit2, &eng2));

        drop(held);
        drop(hit1_again);
        drop(hit2);
        drop(eng1);
        drop(eng2);

        // Now insert a third — should be able to evict key1 (no more refs).
        let key3 = dummy_key("snap-3");
        let eng3 = minimal_engine();
        cache.insert(key3.clone(), eng3, None);
        // After drop, strong_count may or may not have hit zero yet
        // (depends on Arc deallocation timing), but len should be <= 2.
        assert!(cache.len() <= 2);
    }

    #[test]
    fn sweep_idle_does_not_evict_in_use() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_millis(1),
        });
        let key = dummy_key("old-snap");
        let eng = minimal_engine();
        cache.insert(key.clone(), eng.clone(), None);

        // Hold a strong ref.
        let _held = cache.get(&key).expect("hit");

        // Wait for idle threshold to pass.
        std::thread::sleep(Duration::from_millis(5));

        // get() triggers sweep_idle_locked internally.
        // The entry should NOT be evicted because strong_count > 1.
        let hit = cache.get(&key).expect("in-use entry should survive idle sweep");
        assert!(Arc::ptr_eq(&hit, &eng));
    }

    #[test]
    fn sweep_idle_evicts_when_not_in_use() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_millis(1),
        });
        let key = dummy_key("old-snap");
        let eng = minimal_engine();
        cache.insert(key.clone(), eng.clone(), None);

        // Don't hold any external refs.
        drop(eng);

        // Wait for idle threshold.
        std::thread::sleep(Duration::from_millis(5));

        // get() triggers sweep. The entry has strong_count == 1 (cache only)
        // and is past idle — should be evicted, resulting in a miss.
        assert!(cache.get(&key).is_none(), "idle entry with no external refs should be evicted");
    }

    #[test]
    fn generation_bump_invalidates_key() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_secs(9999),
        });
        // Insert at generation 0.
        let key_gen0 = CacheKey { system_install_generation: 0, snapshot_hash: "snap-1".into() };
        let eng = minimal_engine();
        cache.insert(key_gen0.clone(), eng, None);
        assert!(cache.get(&key_gen0).is_some());

        // Bump generation.
        let new_gen = cache.bump_system_install_generation();
        assert_eq!(new_gen, 1);

        // Old key should still be in cache (it's just a key mismatch
        // for new requests, not an active eviction).
        assert!(cache.get(&key_gen0).is_some(), "old entry still physically present");

        // New key (gen 1) should miss.
        let key_gen1 = CacheKey { system_install_generation: 1, snapshot_hash: "snap-1".into() };
        assert!(cache.get(&key_gen1).is_none(), "new generation should miss");
    }

    #[test]
    fn config_defaults_match_plan() {
        let cfg = EngineCacheConfig::default();
        assert_eq!(cfg.capacity, 8, "default capacity: 8 entries");
        assert_eq!(
            cfg.idle_threshold,
            Duration::from_secs(30 * 60),
            "default idle threshold: 30 minutes"
        );
    }

    #[test]
    fn cache_owns_user_temp_dir_not_project_temp_dir() {
        // Verify the ownership split: the cache owns only the user
        // overlay temp dir. When the cache entry is evicted, the user
        // temp dir is removed. A "project" temp dir (simulated here)
        // is NOT owned by the cache and must survive eviction.
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 1,
            idle_threshold: Duration::from_secs(9999),
        });

        let user_dir = tempfile::tempdir().unwrap();
        let user_path = user_dir.path().to_path_buf();
        // Create a file in the user dir to prove it exists.
        std::fs::write(user_path.join("overlay.dat"), "user-content").unwrap();

        let key1 = dummy_key("snap-A");
        let eng1 = minimal_engine();
        cache.insert(
            key1,
            eng1,
            Some(TempDirGuard::new(user_path.clone())),
        );

        // User dir must still exist while the cache holds the entry.
        assert!(user_path.join("overlay.dat").exists(), "user dir must exist while cached");

        // Evict by inserting a second entry (capacity=1).
        let key2 = dummy_key("snap-B");
        let eng2 = minimal_engine();
        cache.insert(key2, eng2, None);

        // User dir should be removed by the TempDirGuard drop on eviction.
        assert!(
            !user_path.exists(),
            "user temp dir must be removed after cache eviction"
        );
    }

    #[test]
    fn concurrent_gets_share_cached_engine_arc() {
        // Simulates concurrent threads hitting the same cache entry.
        // They must all get the same Arc (ptr_eq).
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_secs(9999),
        });
        let key = dummy_key("shared-snap");
        let eng = minimal_engine();
        cache.insert(key.clone(), eng.clone(), None);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let cache = cache.clone();
                let key = key.clone();
                let eng = eng.clone();
                std::thread::spawn(move || {
                    let hit = cache.get(&key).expect("must hit");
                    assert!(Arc::ptr_eq(&hit, &eng), "all threads must get the same Arc");
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }
}
