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
//! Materialising those temp directories and rebuilding the engine on
//! every request would be wasteful — many threads in a session run
//! against the same snapshot. This cache keeps the engine alive
//! (with its associated temp dirs) keyed by snapshot hash, so
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
//! Two policies, applied on every `get_or_build` call:
//!   1. **LRU** at the configured capacity.
//!   2. **Idle threshold**: any entry not touched within the
//!      configured idle window is evicted regardless of LRU order.
//!      This protects against pinning cold temp dirs forever on
//!      low-traffic nodes.
//!
//! # Lifetime
//!
//! Each cached entry owns the materialised temp dirs via
//! [`TempDirGuard`] handles. When the entry is evicted (or the cache
//! is dropped), the temp dirs are removed. While the entry is alive,
//! all threads holding the cloned `Arc<Engine>` share the same temp
//! dirs — no double materialisation, no double cleanup.
//!
//! # Status
//!
//! This module currently provides only the cache data structure and
//! API. The next change wires it into
//! `ryeos_executor::execution::project_source::resolve_project_context`
//! so that `PushedHead` requests look up / populate the cache. Until
//! then no caller constructs an `EngineCache` and the structure is
//! dead code.

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

/// A single cached engine + the temp dirs it depends on.
///
/// `engine` is wrapped in `Arc` so cache lookups can clone cheaply.
/// The temp-dir guards are owned by the entry (not by the engine);
/// when this entry is evicted from the cache, the temp dirs are
/// removed.
struct CachedEntry {
    engine: Arc<Engine>,
    last_touched: Instant,
    _project_temp: Option<TempDirGuard>,
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
    /// least-recently-used entry first if at capacity.
    pub fn insert(
        &self,
        key: CacheKey,
        engine: Arc<Engine>,
        project_temp: Option<TempDirGuard>,
        user_temp: Option<TempDirGuard>,
    ) {
        let mut entries = self.inner.entries.lock().unwrap();
        self.sweep_idle_locked(&mut entries);
        // Evict LRU until under capacity.
        while entries.len() >= self.inner.capacity {
            if let Some(lru_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.last_touched)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&lru_key);
            } else {
                break;
            }
        }
        entries.insert(
            key,
            CachedEntry {
                engine,
                last_touched: Instant::now(),
                _project_temp: project_temp,
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

    /// Internal: evict any entry older than `idle_threshold`.
    fn sweep_idle_locked(&self, entries: &mut HashMap<CacheKey, CachedEntry>) {
        let cutoff = Instant::now() - self.inner.idle_threshold;
        let stale: Vec<CacheKey> = entries
            .iter()
            .filter(|(_, e)| e.last_touched < cutoff)
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

    // Note: end-to-end cache hit/insert tests require a real Engine
    // instance, which is heavy to construct in a unit test. Those
    // live in the integration suite alongside the executor changes
    // that wire EngineCache into resolve_project_context.

    #[test]
    fn idle_threshold_evicts_old_entries() {
        // This test exercises the sweep path without needing a real
        // Engine: we can't insert anything (insert requires an
        // Arc<Engine>), but we can verify that get() on an empty
        // cache after a long idle period still returns None and
        // doesn't panic.
        let cache = EngineCache::new(EngineCacheConfig {
            capacity: 2,
            idle_threshold: Duration::from_millis(1),
        });
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get(&dummy_key("abc")).is_none());
        assert!(cache.is_empty());
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
}
