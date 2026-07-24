use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::Notify;

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING: usize = 128;
const IDLE_TTL: Duration = Duration::from_secs(10 * 60);
const OPT_IN_ENV: &str = "RYEOS_COMPOSE_CONTEXT_CACHE";
const VERIFY_HITS_ENV: &str = "RYEOS_COMPOSE_CONTEXT_CACHE_VERIFY_HITS";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CachedComposeProjection {
    pub rendered_positions: BTreeMap<String, String>,
    pub rendered_meta: BTreeMap<String, Value>,
}

impl CachedComposeProjection {
    fn estimated_bytes(&self) -> usize {
        serde_json::to_vec(self)
            .map(|serialized| serialized.len())
            .unwrap_or(MAX_BYTES.saturating_add(1))
    }

    pub(super) fn digest(&self) -> Result<String, String> {
        let value = serde_json::to_value(self).map_err(|error| error.to_string())?;
        let canonical = lillux::canonical_json(&value).map_err(|error| error.to_string())?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }
}

#[derive(Debug)]
struct CacheEntry {
    projection: Arc<CachedComposeProjection>,
    estimated_bytes: usize,
    last_touched: Instant,
}

#[derive(Debug, Default)]
pub(super) struct PendingFill {
    /// `None` means pending, `Some(None)` means the builder failed, and
    /// `Some(Some(..))` is a successful fill shared with all waiters.
    result: Mutex<Option<Option<Arc<CachedComposeProjection>>>>,
    completed: Notify,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<String, CacheEntry>,
    lru: VecDeque<String>,
    pending: HashMap<String, Arc<PendingFill>>,
    total_bytes: usize,
}

#[derive(Debug, Default)]
pub(super) struct ComposeProjectionCache {
    state: Mutex<CacheState>,
}

pub(super) enum CacheLookup {
    Hit {
        projection: Arc<CachedComposeProjection>,
        entry_bytes: usize,
    },
    Wait {
        pending: Arc<PendingFill>,
    },
    Build(CacheFillGuard),
    /// The bounded in-flight table is full. Run this request cold without
    /// admitting another pending key or publishing its result to the cache.
    Bypass,
}

pub(super) struct CacheFillGuard {
    cache: &'static ComposeProjectionCache,
    key: String,
    pending: Arc<PendingFill>,
    completed: bool,
}

impl CacheFillGuard {
    pub(super) fn complete(
        mut self,
        projection: CachedComposeProjection,
    ) -> Arc<CachedComposeProjection> {
        let projection = Arc::new(projection);
        let estimated_bytes = projection.estimated_bytes().saturating_add(self.key.len());
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.pending.remove(&self.key);
        sweep_idle(&mut state);

        if estimated_bytes <= MAX_BYTES {
            evict_to_fit(&mut state, estimated_bytes);
            if state.entries.len() < MAX_ENTRIES
                && state.total_bytes.saturating_add(estimated_bytes) <= MAX_BYTES
            {
                state.total_bytes = state.total_bytes.saturating_add(estimated_bytes);
                state.lru.push_back(self.key.clone());
                state.entries.insert(
                    self.key.clone(),
                    CacheEntry {
                        projection: projection.clone(),
                        estimated_bytes,
                        last_touched: Instant::now(),
                    },
                );
            }
        } else {
            emit_metric("miss", "projection_too_large", estimated_bytes, 0);
        }

        let mut pending_result = self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pending_result = Some(Some(projection.clone()));
        drop(pending_result);
        self.pending.completed.notify_waiters();
        self.completed = true;
        projection
    }

    /// Settle a single-flight miss without publishing a projection. The
    /// current launch may still use its cold result, but waiters must rebuild
    /// rather than observe request-scoped identity from another launch.
    pub(super) fn skip(mut self, reason: &'static str) {
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .pending
            .get(&self.key)
            .is_some_and(|pending| Arc::ptr_eq(pending, &self.pending))
        {
            state.pending.remove(&self.key);
        }
        drop(state);

        let mut pending_result = self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pending_result = Some(None);
        drop(pending_result);
        self.pending.completed.notify_waiters();
        self.completed = true;
        emit_metric("bypass", reason, 0, 0);
    }
}

impl Drop for CacheFillGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .pending
            .get(&self.key)
            .is_some_and(|pending| Arc::ptr_eq(pending, &self.pending))
        {
            state.pending.remove(&self.key);
        }
        drop(state);
        let mut pending_result = self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pending_result = Some(None);
        drop(pending_result);
        self.pending.completed.notify_waiters();
        emit_metric("miss", "fill_failed", 0, 0);
    }
}

impl ComposeProjectionCache {
    pub(super) fn begin(&'static self, key: &str) -> CacheLookup {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sweep_idle(&mut state);

        if let Some(projection) = state.entries.get_mut(key).map(|entry| {
            entry.last_touched = Instant::now();
            (entry.projection.clone(), entry.estimated_bytes)
        }) {
            touch_lru(&mut state.lru, key);
            return CacheLookup::Hit {
                projection: projection.0,
                entry_bytes: projection.1,
            };
        }
        if let Some(pending) = state.pending.get(key) {
            return CacheLookup::Wait {
                pending: pending.clone(),
            };
        }
        if state.pending.len() >= MAX_PENDING {
            return CacheLookup::Bypass;
        }

        let pending = Arc::new(PendingFill::default());
        state.pending.insert(key.to_string(), pending.clone());
        CacheLookup::Build(CacheFillGuard {
            cache: self,
            key: key.to_string(),
            pending,
            completed: false,
        })
    }

    /// Remove only the projection this caller actually observed. A concurrent
    /// replacement under the same authority key is left intact.
    pub(super) fn discard_if_same(
        &self,
        key: &str,
        projection: &Arc<CachedComposeProjection>,
        reason: &'static str,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let observed_entry = state
            .entries
            .get(key)
            .is_some_and(|entry| Arc::ptr_eq(&entry.projection, projection));
        let entry_bytes = if observed_entry {
            remove_entry(&mut state, key)
        } else {
            0
        };
        drop(state);
        emit_metric("eviction", reason, entry_bytes, 0);
    }
}

impl PendingFill {
    pub(super) async fn wait(&self) -> Option<Arc<CachedComposeProjection>> {
        loop {
            let notified = self.completed.notified();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
            {
                return result.clone();
            }
            notified.await;
        }
    }
}

pub(super) fn cache() -> &'static ComposeProjectionCache {
    static CACHE: OnceLock<ComposeProjectionCache> = OnceLock::new();
    CACHE.get_or_init(ComposeProjectionCache::default)
}

/// Security-sensitive and deliberately default-off. Only the exact value `1`
/// opts a node process into history semantic (a).
pub(super) fn explicitly_enabled() -> bool {
    matches!(std::env::var(OPT_IN_ENV).as_deref(), Ok("1"))
}

/// Diagnostic mode for the cache-equivalence gate. A hit still executes the
/// child and compares its normalized projection with the cached projection;
/// only digests are logged.
pub(super) fn verify_hits_enabled() -> bool {
    matches!(std::env::var(VERIFY_HITS_ENV).as_deref(), Ok("1"))
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct ComposeCacheHitAudit {
    schema_version: u32,
    augmentation: &'static str,
    cache_key_digest: String,
    projection_digest: String,
    waited_for_fill: bool,
    child_execution: &'static str,
}

/// Record history semantic (a) as its own launch-audit event. It deliberately
/// does not mutate the composed resolution handed to the parent runtime.
pub(super) fn record_hit_audit(
    cache_key_digest: &str,
    projection: &CachedComposeProjection,
    waited_for_fill: bool,
) -> Result<super::LaunchAugmentationAudit, String> {
    let payload = serde_json::to_value(ComposeCacheHitAudit {
        schema_version: 1,
        augmentation: "compose_context_positions",
        cache_key_digest: cache_key_digest.to_string(),
        projection_digest: projection.digest()?,
        waited_for_fill,
        child_execution: "not_run_cache_hit",
    })
    .map_err(|error| error.to_string())?;
    Ok(super::LaunchAugmentationAudit {
        event_type: ryeos_runtime::events::RuntimeEventType::LaunchAugmentationCacheHit,
        payload,
    })
}

pub(super) fn emit_metric(
    outcome: &'static str,
    reason: &'static str,
    entry_bytes: usize,
    wait_milliseconds: u64,
) {
    tracing::info!(
        target: "ryeos.metrics",
        metric = "compose_context_positions_cache",
        outcome,
        reason,
        entry_bytes,
        wait_milliseconds,
        "compose context cache metric"
    );
}

fn sweep_idle(state: &mut CacheState) {
    let now = Instant::now();
    let stale = state
        .entries
        .iter()
        .filter(|(_, entry)| now.duration_since(entry.last_touched) >= IDLE_TTL)
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in stale {
        let entry_bytes = remove_entry(state, &key);
        emit_metric("eviction", "idle_ttl", entry_bytes, 0);
    }
}

fn evict_to_fit(state: &mut CacheState, incoming_bytes: usize) {
    while state.entries.len() >= MAX_ENTRIES
        || state.total_bytes.saturating_add(incoming_bytes) > MAX_BYTES
    {
        let Some(oldest) = state.lru.pop_front() else {
            break;
        };
        if let Some(entry) = state.entries.remove(&oldest) {
            state.total_bytes = state.total_bytes.saturating_sub(entry.estimated_bytes);
            emit_metric("eviction", "capacity", entry.estimated_bytes, 0);
        }
    }
}

fn remove_entry(state: &mut CacheState, key: &str) -> usize {
    let entry_bytes = if let Some(entry) = state.entries.remove(key) {
        state.total_bytes = state.total_bytes.saturating_sub(entry.estimated_bytes);
        entry.estimated_bytes
    } else {
        0
    };
    if let Some(position) = state.lru.iter().position(|candidate| candidate == key) {
        state.lru.remove(position);
    }
    entry_bytes
}

fn touch_lru(lru: &mut VecDeque<String>, key: &str) {
    if let Some(position) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(position);
    }
    lru.push_back(key.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection(value: &str) -> CachedComposeProjection {
        CachedComposeProjection {
            rendered_positions: BTreeMap::from([("system".to_string(), value.to_string())]),
            rendered_meta: BTreeMap::new(),
        }
    }

    #[test]
    fn failed_fill_is_not_cached() {
        let cache = Box::leak(Box::new(ComposeProjectionCache::default()));
        let CacheLookup::Build(fill) = cache.begin("key") else {
            panic!("first lookup must build");
        };
        drop(fill);
        let CacheLookup::Build(_) = cache.begin("key") else {
            panic!("failed fill must be retried");
        };
    }

    #[test]
    fn successful_fill_is_shared_and_cached() {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let cache = Box::leak(Box::new(ComposeProjectionCache::default()));
                let CacheLookup::Build(fill) = cache.begin("key") else {
                    panic!("first lookup must build");
                };
                let CacheLookup::Wait { pending } = cache.begin("key") else {
                    panic!("second lookup must wait");
                };
                fill.complete(projection("cached"));
                assert_eq!(
                    pending.wait().await.unwrap().rendered_positions["system"],
                    "cached"
                );
                assert!(matches!(cache.begin("key"), CacheLookup::Hit { .. }));
            });
    }

    #[test]
    fn skipped_sensitive_fill_wakes_waiters_without_caching() {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let cache = Box::leak(Box::new(ComposeProjectionCache::default()));
                let CacheLookup::Build(fill) = cache.begin("key") else {
                    panic!("first lookup must build");
                };
                let CacheLookup::Wait { pending } = cache.begin("key") else {
                    panic!("second lookup must wait");
                };
                fill.skip("request_scoped_projection");
                assert!(pending.wait().await.is_none());
                assert!(matches!(cache.begin("key"), CacheLookup::Build(_)));
            });
    }

    #[test]
    fn authority_revalidation_discards_only_the_observed_projection() {
        let cache = Box::leak(Box::new(ComposeProjectionCache::default()));
        let CacheLookup::Build(fill) = cache.begin("key") else {
            panic!("first lookup must build");
        };
        let observed = fill.complete(projection("stale"));
        cache.discard_if_same("key", &observed, "authority_revalidation_failed");
        assert!(matches!(cache.begin("key"), CacheLookup::Build(_)));
    }

    #[test]
    fn pending_keys_are_bounded() {
        let cache = Box::leak(Box::new(ComposeProjectionCache::default()));
        let mut fills = Vec::new();
        for index in 0..MAX_PENDING {
            let CacheLookup::Build(fill) = cache.begin(&format!("key-{index}")) else {
                panic!("pending slot {index} must be admitted");
            };
            fills.push(fill);
        }
        assert!(matches!(cache.begin("overflow"), CacheLookup::Bypass));
        drop(fills);
    }
}
