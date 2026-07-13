//! Per-snapshot engine cache for the request-scoped engine overlay.
//!
//! Background:
//!
//! The daemon builds one `Engine` at startup against its own roots
//! (bundle roots + the operator's own app root). For
//! `pushed_head` requests, we need a different engine: one whose
//! project-tier roots point at the **caller's** materialised content,
//! and whose trust store includes the caller's pushed trust pins as
//! an overlay.
//!
//! Materialising the caller overlay and rebuilding the engine on every
//! request would be wasteful — many threads in a session run against
//! the same snapshot. This cache keeps the engine alive (with its
//! associated caller overlay temp dir) keyed by snapshot hash, so
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
//! Two policies, applied on every `get()` and cache-fill call:
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
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ryeos_engine::engine::Engine;

use crate::temp_dir_guard::TempDirGuard;

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
    _user_temp: Option<Arc<TempDirGuard>>,
}

/// Per in-flight build. Concurrent misses for the same key wait on the
/// Condvar; the first builder wins and broadcasts.
struct PendingBuild {
    done: Condvar,
    result: Mutex<Option<Result<Arc<Engine>, BuildWaitError>>>,
}

/// Slot state: either a ready cache entry or an in-flight build.
enum CacheSlot {
    Ready(CachedEntry),
    Building(Arc<PendingBuild>),
}

impl Clone for CacheSlot {
    fn clone(&self) -> Self {
        match self {
            CacheSlot::Ready(_) => panic!("CacheSlot::Ready should not be cloned"),
            CacheSlot::Building(pb) => CacheSlot::Building(Arc::clone(pb)),
        }
    }
}

/// Error returned to waiters when the in-flight build fails.
#[derive(Debug, Clone)]
pub struct BuildWaitError {
    pub message: String,
}

impl std::fmt::Display for BuildWaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cache build failed: {}", self.message)
    }
}

impl std::error::Error for BuildWaitError {}

/// Per-snapshot engine cache.
///
/// Cheap to clone — the inner state is behind an `Arc`.
#[derive(Clone)]
pub struct EngineCache {
    inner: Arc<EngineCacheInner>,
}

struct EngineCacheInner {
    slots: Mutex<HashMap<CacheKey, CacheSlot>>,
    /// Bumped on every `bundle.install` / `bundle.uninstall`. Mixed
    /// into the cache key so a bundle change invalidates all entries
    /// built against the previous bundle root set without an explicit
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
                slots: Mutex::new(HashMap::new()),
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
    /// ordering reflects recency. Returns `None` for misses and
    /// `Building` slots (callers should use `get_or_insert_with`
    /// instead).
    pub fn get(&self, key: &CacheKey) -> Option<Arc<Engine>> {
        let mut slots = self.inner.slots.lock().unwrap();
        self.sweep_idle_locked(&mut slots);
        if let Some(CacheSlot::Ready(entry)) = slots.get_mut(key) {
            entry.last_touched = Instant::now();
            Some(entry.engine.clone())
        } else {
            None
        }
    }

    /// Single-flight cache lookup + build. Returns the cached engine
    /// if one exists for `key`, or calls `build_fn` exactly once to
    /// produce one. Concurrent callers for the same key block on the
    /// first builder's Condvar.
    ///
    /// `build_fn` returns `Ok((engine, user_temp_guard))` on success.
    /// On error, the slot is removed so a future caller can retry.
    pub fn get_or_insert_with<F, E>(&self, key: CacheKey, build_fn: F) -> Result<Arc<Engine>, E>
    where
        F: FnOnce() -> Result<(Arc<Engine>, Option<Arc<TempDirGuard>>), E>,
        E: From<BuildWaitError> + std::fmt::Display,
    {
        let mut slots = self.inner.slots.lock().unwrap();
        self.sweep_idle_locked(&mut slots);

        // Fast path: Ready hit.
        if let Some(CacheSlot::Ready(entry)) = slots.get_mut(&key) {
            entry.last_touched = Instant::now();
            return Ok(entry.engine.clone());
        }

        // Check for an in-flight build.
        if let Some(CacheSlot::Building(pending)) = slots.get(&key).cloned() {
            // Wait path: drop the lock, wait on the Condvar.
            drop(slots);
            return Self::wait_for_build(&pending);
        }

        // Build path: insert a Building slot, release the lock, run
        // build_fn, then transition to Ready.
        let pending = Arc::new(PendingBuild {
            done: Condvar::new(),
            result: Mutex::new(None),
        });
        slots.insert(key.clone(), CacheSlot::Building(pending.clone()));
        drop(slots);

        // Run the build outside the lock. If the build panics, remove
        // the in-flight slot and wake waiters before resuming unwind so
        // a later caller can retry instead of deadlocking on `Building`.
        let build_result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(build_fn)) {
            Ok(result) => result,
            Err(payload) => {
                let mut slots = self.inner.slots.lock().unwrap();
                slots.remove(&key);
                {
                    let mut r = pending.result.lock().unwrap();
                    *r = Some(Err(BuildWaitError {
                        message: "build panicked".into(),
                    }));
                }
                pending.done.notify_all();
                drop(slots);
                std::panic::resume_unwind(payload);
            }
        };

        let mut slots = self.inner.slots.lock().unwrap();

        // Transition Building → Ready.
        let (engine, user_temp) = build_result.inspect_err(|user_err| {
            // Remove slot so retry works.
            slots.remove(&key);
            // Signal waiters with the builder's failure detail so they
            // see the same error class/message as the builder (modulo
            // the BuildWaitError wrapping required because `E` is not
            // Clone). Callers can map BuildWaitError back to their
            // domain error type via the `From` impls.
            {
                let mut r = pending.result.lock().unwrap();
                *r = Some(Err(BuildWaitError {
                    message: user_err.to_string(),
                }));
            }
            pending.done.notify_all();
        })?;

        // Remove our own Building slot BEFORE evicting for capacity so
        // the in-flight slot is not counted against the LRU budget.
        // Otherwise a miss at capacity would evict one extra Ready
        // entry (len = N ready + 1 building → evict to N-1, then insert
        // ready → N). Removing first restores len = N before eviction.
        slots.remove(&key);
        self.evict_for_capacity_locked(&mut slots);
        slots.insert(
            key,
            CacheSlot::Ready(CachedEntry {
                engine: engine.clone(),
                last_touched: Instant::now(),
                _user_temp: user_temp,
            }),
        );
        // Signal waiters.
        {
            let mut r = pending.result.lock().unwrap();
            *r = Some(Ok(engine.clone()));
        }
        pending.done.notify_all();
        Ok(engine)
    }

    /// Wait for an in-flight build to complete and return its result.
    fn wait_for_build<E>(pending: &Arc<PendingBuild>) -> Result<Arc<Engine>, E>
    where
        E: From<BuildWaitError>,
    {
        let mut result = pending.result.lock().unwrap();
        while result.is_none() {
            result = pending.done.wait(result).unwrap();
        }
        // Clone the result so multiple waiters can all read it.
        match result.as_ref().unwrap() {
            Ok(engine) => Ok(engine.clone()),
            Err(e) => Err(E::from(BuildWaitError {
                message: e.message.clone(),
            })),
        }
    }

    /// Bypass the single-flight guard. ONLY for tests that need
    /// deterministic insert ordering. Production code MUST use
    /// `get_or_insert_with` — using direct insertion can silently
    /// drop a `_user_temp` while an `Arc<Engine>` clone still resolves
    /// against it.
    #[cfg(test)]
    fn insert_unchecked(
        &self,
        key: CacheKey,
        engine: Arc<Engine>,
        user_temp: Option<Arc<TempDirGuard>>,
    ) {
        let mut slots = self.inner.slots.lock().unwrap();
        self.sweep_idle_locked(&mut slots);
        self.evict_for_capacity_locked(&mut slots);
        slots.insert(
            key,
            CacheSlot::Ready(CachedEntry {
                engine,
                last_touched: Instant::now(),
                _user_temp: user_temp,
            }),
        );
    }

    /// Test-only direct insert helper.
    #[cfg(test)]
    pub fn insert_for_test(
        &self,
        key: CacheKey,
        engine: Arc<Engine>,
        user_temp: Option<Arc<TempDirGuard>>,
    ) {
        self.insert_unchecked(key, engine, user_temp);
    }

    /// Number of cached Ready entries. Test/diagnostic helper.
    pub fn len(&self) -> usize {
        self.inner
            .slots
            .lock()
            .unwrap()
            .values()
            .filter(|s| matches!(s, CacheSlot::Ready(_)))
            .count()
    }

    /// True if cache has no Ready entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop every entry. The temp dirs are removed by `Drop`.
    pub fn clear(&self) {
        self.inner.slots.lock().unwrap().clear();
    }

    /// Internal: evict any entry older than `idle_threshold` that is
    /// not currently held by a running request. Only touches Ready slots.
    fn sweep_idle_locked(&self, slots: &mut HashMap<CacheKey, CacheSlot>) {
        let cutoff = Instant::now() - self.inner.idle_threshold;
        let stale: Vec<CacheKey> = slots
            .iter()
            .filter_map(|(k, s)| match s {
                CacheSlot::Ready(e)
                    if e.last_touched < cutoff && Arc::strong_count(&e.engine) <= 1 =>
                {
                    Some(k.clone())
                }
                _ => None,
            })
            .collect();
        for k in stale {
            slots.remove(&k);
        }
    }

    /// Evict LRU Ready entries that are not held by any running
    /// request until below capacity.
    fn evict_for_capacity_locked(&self, slots: &mut HashMap<CacheKey, CacheSlot>) {
        while slots.len() >= self.inner.capacity {
            let candidate = slots
                .iter()
                .filter_map(|(k, s)| match s {
                    CacheSlot::Ready(e) if Arc::strong_count(&e.engine) <= 1 => {
                        Some((k.clone(), e.last_touched))
                    }
                    _ => None,
                })
                .min_by_key(|(_, t)| *t)
                .map(|(k, _)| k);
            match candidate {
                Some(k) => {
                    slots.remove(&k);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a minimal `Arc<Engine>` suitable for cache tests — no
    /// bundles, no kinds, no parsers. Just enough to satisfy the type.
    fn minimal_engine() -> Arc<Engine> {
        Arc::new(Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                std::sync::Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
            ),
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
        cache.insert_for_test(key.clone(), eng.clone(), None);
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
        cache.insert_for_test(key.clone(), eng.clone(), None);

        let hit1 = cache.get(&key).expect("first hit");
        let hit2 = cache.get(&key).expect("second hit");
        assert!(
            Arc::ptr_eq(&hit1, &hit2),
            "same cache entry returns same Arc"
        );
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

        cache.insert_for_test(key1.clone(), eng1.clone(), None);
        assert_eq!(cache.len(), 1);

        // Hold a strong ref outside the cache.
        let held = cache.get(&key1).expect("hit");

        // Insert a second entry — key1's engine has strong_count > 1
        // so it should NOT be evicted.
        cache.insert_for_test(key2.clone(), eng2.clone(), None);
        assert_eq!(
            cache.len(),
            2,
            "cache should be over-capacity, not evict in-use entry"
        );

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
        cache.insert_for_test(key3.clone(), eng3, None);
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
        cache.insert_for_test(key.clone(), eng.clone(), None);

        // Hold a strong ref.
        let _held = cache.get(&key).expect("hit");

        // Wait for idle threshold to pass.
        std::thread::sleep(Duration::from_millis(5));

        // get() triggers sweep_idle_locked internally.
        // The entry should NOT be evicted because strong_count > 1.
        let hit = cache
            .get(&key)
            .expect("in-use entry should survive idle sweep");
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
        cache.insert_for_test(key.clone(), eng.clone(), None);

        // Don't hold any external refs.
        drop(eng);

        // Wait for idle threshold.
        std::thread::sleep(Duration::from_millis(5));

        // get() triggers sweep. The entry has strong_count == 1 (cache only)
        // and is past idle — should be evicted, resulting in a miss.
        assert!(
            cache.get(&key).is_none(),
            "idle entry with no external refs should be evicted"
        );
    }

    #[test]
    fn generation_bump_invalidates_key() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_secs(9999),
        });
        // Insert at generation 0.
        let key_gen0 = CacheKey {
            system_install_generation: 0,
            snapshot_hash: "snap-1".into(),
        };
        let eng = minimal_engine();
        cache.insert_for_test(key_gen0.clone(), eng, None);
        assert!(cache.get(&key_gen0).is_some());

        // Bump generation.
        let new_gen = cache.bump_system_install_generation();
        assert_eq!(new_gen, 1);

        // Old key should still be in cache (it's just a key mismatch
        // for new requests, not an active eviction).
        assert!(
            cache.get(&key_gen0).is_some(),
            "old entry still physically present"
        );

        // New key (gen 1) should miss.
        let key_gen1 = CacheKey {
            system_install_generation: 1,
            snapshot_hash: "snap-1".into(),
        };
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
        cache.insert_for_test(
            key1,
            eng1,
            Some(Arc::new(TempDirGuard::new(user_path.clone()))),
        );

        // User dir must still exist while the cache holds the entry.
        assert!(
            user_path.join("overlay.dat").exists(),
            "user dir must exist while cached"
        );

        // Evict by inserting a second entry (capacity=1).
        let key2 = dummy_key("snap-B");
        let eng2 = minimal_engine();
        cache.insert_for_test(key2, eng2, None);

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
        cache.insert_for_test(key.clone(), eng.clone(), None);

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

    // ── Step B: single-flight tests ────────────────────────────────

    #[test]
    fn get_or_insert_with_builds_on_miss() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_secs(9999),
        });
        let key = dummy_key("snap-build");
        let eng = minimal_engine();
        let eng_clone = eng.clone();
        let result = cache
            .get_or_insert_with::<_, BuildWaitError>(key.clone(), || Ok((eng_clone, None)))
            .unwrap();
        assert!(Arc::ptr_eq(&result, &eng));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn get_or_insert_with_returns_cached_on_hit() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_secs(9999),
        });
        let key = dummy_key("snap-hit");
        let eng = minimal_engine();
        cache.insert_for_test(key.clone(), eng.clone(), None);

        let called = std::sync::atomic::AtomicBool::new(false);
        let result = cache
            .get_or_insert_with::<_, BuildWaitError>(key, || {
                called.store(true, Ordering::SeqCst);
                Ok((minimal_engine(), None))
            })
            .unwrap();
        assert!(
            Arc::ptr_eq(&result, &eng),
            "must return cached, not rebuild"
        );
        assert!(
            !called.load(Ordering::SeqCst),
            "build_fn must not be called on hit"
        );
    }

    #[test]
    fn concurrent_same_key_misses_serialize_on_build() {
        // Invariant: only one build_fn call per key; all callers get
        // the same Arc.
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_secs(9999),
        });
        let key = dummy_key("concurrent-build");
        let build_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let eng = minimal_engine();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let cache = cache.clone();
                let key = key.clone();
                let eng = eng.clone();
                let barrier = barrier.clone();
                let build_count = build_count.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    
                    cache
                        .get_or_insert_with::<_, BuildWaitError>(key.clone(), || {
                            build_count.fetch_add(1, Ordering::SeqCst);
                            // Simulate slow build.
                            std::thread::sleep(Duration::from_millis(50));
                            Ok((eng.clone(), None))
                        })
                        .unwrap()
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let build_calls = build_count.load(Ordering::SeqCst);
        assert_eq!(
            build_calls, 1,
            "build_fn must be called exactly once, got {build_calls}"
        );
        for r in &results {
            assert!(Arc::ptr_eq(r, &eng), "all callers must get the same Arc");
        }
    }

    #[test]
    fn same_key_insert_does_not_drop_in_use_user_temp() {
        // Invariant: an in-use Arc<Engine> survives same-key cache
        // operations; the user temp dir is not dropped while held.
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_secs(9999),
        });
        let key = dummy_key("same-key-survives");
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();
        std::fs::write(path.join("user.dat"), "user-content").unwrap();

        cache
            .get_or_insert_with::<_, BuildWaitError>(key.clone(), || {
                Ok((
                    minimal_engine(),
                    Some(Arc::new(TempDirGuard::new(path.clone()))),
                ))
            })
            .unwrap();
        let held = cache.get(&key).unwrap();

        // Force another same-key build path attempt; correct
        // single-flight semantics return the existing entry and do not
        // overwrite/drop the cached user temp guard.
        let again = cache
            .get_or_insert_with::<_, BuildWaitError>(key.clone(), || {
                panic!("should not rebuild — entry exists");
            })
            .unwrap();

        assert!(Arc::ptr_eq(&held, &again));
        assert!(path.exists(), "user temp dir survived");
    }

    #[test]
    fn build_failure_releases_slot_for_retry() {
        // Invariant: a failed build does not poison the slot; a second
        // call rebuilds successfully.
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_secs(9999),
        });
        let key = dummy_key("retry-slot");
        let attempt = std::sync::atomic::AtomicUsize::new(0);

        // First call fails.
        let err = cache.get_or_insert_with::<_, BuildWaitError>(key.clone(), || {
            attempt.fetch_add(1, Ordering::SeqCst);
            Err(BuildWaitError {
                message: "intentional".into(),
            })
        });
        assert!(err.is_err());

        // Second call succeeds.
        let eng = minimal_engine();
        let result = cache
            .get_or_insert_with::<_, BuildWaitError>(key.clone(), || {
                attempt.fetch_add(1, Ordering::SeqCst);
                Ok((eng.clone(), None))
            })
            .unwrap();
        assert_eq!(attempt.load(Ordering::SeqCst), 2);
        assert!(Arc::ptr_eq(&result, &eng));
    }

    #[test]
    fn miss_at_capacity_evicts_exactly_one_ready_entry() {
        // Regression: prior to the fix, the in-flight `Building` slot
        // was counted against capacity during eviction, so a miss at
        // capacity N evicted N-1 ready entries then inserted ready,
        // leaving the cache at N-1 instead of N. After the fix the
        // Building slot is removed before eviction, so a miss at
        // capacity evicts exactly one ready entry.
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 2,
            idle_threshold: Duration::from_secs(9999),
        });

        // Fill to capacity with releasable (strong_count==1) entries.
        cache.insert_for_test(dummy_key("snap-A"), minimal_engine(), None);
        cache.insert_for_test(dummy_key("snap-B"), minimal_engine(), None);
        assert_eq!(cache.len(), 2);

        // Cache miss for a new key triggers eviction.
        let new_eng = minimal_engine();
        let new_eng_clone = new_eng.clone();
        cache
            .get_or_insert_with::<_, BuildWaitError>(dummy_key("snap-C"), || {
                Ok((new_eng_clone, None))
            })
            .unwrap();

        // Expect exactly capacity (2) ready entries — one evicted,
        // one inserted. If the Building slot was incorrectly counted,
        // we would end at len = 1 (two evicted, one inserted).
        assert_eq!(
            cache.len(),
            2,
            "miss at capacity must evict exactly one ready entry"
        );
    }

    #[test]
    fn concurrent_waiter_sees_builder_failure_detail() {
        // Regression: the prior implementation stored the literal
        // string "build failed" in the waiter's BuildWaitError,
        // discarding the builder's Display detail. Waiters must see
        // the builder's error message so HTTP-layer error classes
        // line up across concurrent callers.
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_secs(9999),
        });
        let key = dummy_key("waiter-failure-detail");

        // Two threads race on the same key. The builder sleeps so the
        // second thread reliably becomes a waiter, then the builder
        // fails with a distinctive message.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let cache_b = cache.clone();
        let key_b = key.clone();
        let barrier_b = barrier.clone();
        let builder = std::thread::spawn(move || {
            barrier_b.wait();
            cache_b.get_or_insert_with::<_, BuildWaitError>(key_b, || {
                std::thread::sleep(Duration::from_millis(50));
                Err(BuildWaitError {
                    message: "SENTINEL-DETAIL-checkout-snap-not-in-cas".into(),
                })
            })
        });

        let cache_w = cache.clone();
        let key_w = key.clone();
        let barrier_w = barrier.clone();
        let waiter = std::thread::spawn(move || {
            barrier_w.wait();
            // Tiny delay so we hit the Building slot rather than racing
            // for the build path ourselves.
            std::thread::sleep(Duration::from_millis(10));
            cache_w.get_or_insert_with::<_, BuildWaitError>(key_w, || {
                Err(BuildWaitError {
                    message: "waiter-build-should-not-run".into(),
                })
            })
        });

        let builder_res = builder.join().unwrap();
        let waiter_res = waiter.join().unwrap();

        let builder_err = builder_res.unwrap_err();
        let waiter_err = waiter_res.unwrap_err();

        assert!(
            builder_err
                .message
                .contains("SENTINEL-DETAIL-checkout-snap-not-in-cas"),
            "builder must see its own message; got: {}",
            builder_err.message
        );
        assert!(
            waiter_err
                .message
                .contains("SENTINEL-DETAIL-checkout-snap-not-in-cas"),
            "waiter must see the builder's detail, not a generic 'build failed'; got: {}",
            waiter_err.message
        );
    }

    #[test]
    fn panic_in_build_fn_releases_slot() {
        // Invariant: a panic during build removes the Building slot so
        // the same key can be retried without deadlock.
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 4,
            idle_threshold: Duration::from_secs(9999),
        });
        let key = dummy_key("panic-slot");

        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let cache = cache.clone();
            let key = key.clone();
            move || {
                let _ = cache.get_or_insert_with::<_, BuildWaitError>(key, || {
                    panic!("intentional build panic");
                });
            }
        }));
        assert!(panic_result.is_err());

        let eng = minimal_engine();
        let result = cache
            .get_or_insert_with::<_, BuildWaitError>(key, || Ok((eng.clone(), None)))
            .unwrap();
        assert!(Arc::ptr_eq(&result, &eng));
    }

    #[test]
    fn callback_token_keeps_cache_entry_alive_across_eviction_attempts() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 1,
            idle_threshold: Duration::from_secs(9999),
        });
        let key = dummy_key("callback-held");
        let engine = minimal_engine();
        cache.insert_for_test(key.clone(), engine.clone(), None);

        let provenance = crate::execution_provenance::ExecutionProvenance::root_live_fs(
            PathBuf::from("/borrowed"),
            engine.clone(),
        );
        drop(engine);

        cache.insert_for_test(dummy_key("pressure"), minimal_engine(), None);
        let hit = cache
            .get(&key)
            .expect("callback-held entry survives eviction pressure");
        assert!(Arc::ptr_eq(&hit, provenance.request_engine()));
    }

    #[test]
    fn dropping_callback_token_releases_cache_entry() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 1,
            idle_threshold: Duration::from_secs(9999),
        });
        let key = dummy_key("callback-dropped");
        let engine = minimal_engine();
        cache.insert_for_test(key.clone(), engine.clone(), None);
        let provenance = crate::execution_provenance::ExecutionProvenance::root_live_fs(
            PathBuf::from("/borrowed"),
            engine.clone(),
        );
        drop(provenance);
        drop(engine);

        cache.insert_for_test(dummy_key("pressure-1"), minimal_engine(), None);
        cache.insert_for_test(dummy_key("pressure-2"), minimal_engine(), None);
        assert!(cache.get(&key).is_none(), "released entry can be evicted");
    }

    #[test]
    fn bundle_install_during_active_callback_does_not_evict_pinned_engine() {
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 1,
            idle_threshold: Duration::from_secs(9999),
        });
        let old_key = dummy_key("pre-install");
        let old_engine = minimal_engine();
        cache.insert_for_test(old_key.clone(), old_engine.clone(), None);
        let provenance = crate::execution_provenance::ExecutionProvenance::root_live_fs(
            PathBuf::from("/borrowed"),
            old_engine.clone(),
        );
        drop(old_engine);

        let generation = cache.bump_system_install_generation();
        cache.insert_for_test(
            CacheKey {
                system_install_generation: generation,
                snapshot_hash: "post-install".into(),
            },
            minimal_engine(),
            None,
        );

        let hit = cache
            .get(&old_key)
            .expect("old engine pinned by callback survives generation bump");
        assert!(Arc::ptr_eq(&hit, provenance.request_engine()));
    }
}
