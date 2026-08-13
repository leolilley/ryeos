use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use serde_json::Value;

use crate::error::EngineError;

const MAX_CACHE_ENTRIES: usize = 256;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHE_ENTRY_BYTES: usize = 4 * 1024 * 1024;
const MAX_IN_FLIGHT_BUILDS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ParseResultCacheKey {
    dispatcher_fingerprint: String,
    parser_ref: String,
    content_digest: String,
}

impl ParseResultCacheKey {
    pub(super) fn new(
        dispatcher_fingerprint: String,
        parser_ref: String,
        content_digest: String,
    ) -> Self {
        Self {
            dispatcher_fingerprint,
            parser_ref,
            content_digest,
        }
    }

    fn estimated_bytes(&self) -> usize {
        self.dispatcher_fingerprint
            .len()
            .saturating_add(self.parser_ref.len())
            .saturating_add(self.content_digest.len())
    }
}

#[derive(Debug)]
struct CacheEntry {
    value: Arc<Value>,
    estimated_bytes: usize,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<ParseResultCacheKey, CacheEntry>,
    lru: VecDeque<ParseResultCacheKey>,
    in_flight: HashMap<ParseResultCacheKey, Arc<InFlight>>,
    total_bytes: usize,
}

#[derive(Debug, Default)]
struct InFlight {
    result: Mutex<Option<Result<Arc<Value>, Arc<EngineError>>>>,
    completed: Condvar,
}

/// A bounded cache for parser descriptors that explicitly promise deterministic,
/// side-effect-free content-addressed execution.
#[derive(Debug, Default)]
pub(super) struct ParseResultCache {
    state: Mutex<CacheState>,
}

impl ParseResultCache {
    pub(super) fn get_or_build(
        &self,
        key: ParseResultCacheKey,
        source_bytes: usize,
        build: impl FnOnce() -> Result<Value, EngineError>,
    ) -> Result<Value, EngineError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(value) = state
            .entries
            .get(&key)
            .map(|entry| Arc::clone(&entry.value))
        {
            touch_lru(&mut state.lru, &key);
            emit_metric("hit", "ready", source_bytes, 0, 0);
            return Ok(value.as_ref().clone());
        }

        if let Some(in_flight) = state.in_flight.get(&key).cloned() {
            drop(state);
            let started = Instant::now();
            let mut result = in_flight
                .result
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while result.is_none() {
                result = in_flight
                    .completed
                    .wait(result)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            emit_metric("hit", "single_flight", source_bytes, 0, elapsed_us(started));
            return match result.as_ref().expect("parse result completed above") {
                Ok(value) => Ok(value.as_ref().clone()),
                Err(error) => Err(EngineError::Shared(Arc::clone(error))),
            };
        }

        if state.in_flight.len() >= MAX_IN_FLIGHT_BUILDS {
            drop(state);
            emit_metric("bypass", "pending_capacity", source_bytes, 0, 0);
            return build();
        }

        let in_flight = Arc::new(InFlight::default());
        state.in_flight.insert(key.clone(), Arc::clone(&in_flight));
        drop(state);

        emit_metric("miss", "cold", source_bytes, 0, 0);
        let build_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build));
        let build_result = match build_result {
            Ok(result) => result.map(Arc::new).map_err(Arc::new),
            Err(panic_payload) => {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *in_flight
                    .result
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(Err(Arc::new(EngineError::Internal(
                        "parser result build panicked before producing a result".to_string(),
                    ))));
                in_flight.completed.notify_all();
                state.in_flight.remove(&key);
                drop(state);
                std::panic::resume_unwind(panic_payload);
            }
        };

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Ok(value) = &build_result {
            let value_bytes = serde_json::to_vec(value.as_ref())
                .map(|encoded| encoded.len())
                .unwrap_or(MAX_CACHE_ENTRY_BYTES.saturating_add(1));
            let estimated_bytes = key.estimated_bytes().saturating_add(value_bytes);
            if estimated_bytes <= MAX_CACHE_ENTRY_BYTES && estimated_bytes <= MAX_CACHE_BYTES {
                insert_entry(&mut state, &key, value, estimated_bytes);
                emit_metric("fill", "success", source_bytes, estimated_bytes, 0);
            } else {
                emit_metric("bypass", "entry_capacity", source_bytes, estimated_bytes, 0);
            }
        }

        *in_flight
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(build_result.clone());
        in_flight.completed.notify_all();
        state.in_flight.remove(&key);
        drop(state);

        match build_result {
            Ok(value) => Ok(value.as_ref().clone()),
            Err(error) => Err(EngineError::Shared(error)),
        }
    }
}

fn insert_entry(
    state: &mut CacheState,
    key: &ParseResultCacheKey,
    value: &Arc<Value>,
    estimated_bytes: usize,
) {
    if let Some(replaced) = state.entries.remove(key) {
        state.total_bytes = state.total_bytes.saturating_sub(replaced.estimated_bytes);
        if let Some(position) = state.lru.iter().position(|candidate| candidate == key) {
            state.lru.remove(position);
        }
    }

    state.total_bytes = state.total_bytes.saturating_add(estimated_bytes);
    state.entries.insert(
        key.clone(),
        CacheEntry {
            value: Arc::clone(value),
            estimated_bytes,
        },
    );
    state.lru.push_back(key.clone());

    while state.entries.len() > MAX_CACHE_ENTRIES || state.total_bytes > MAX_CACHE_BYTES {
        let Some(evicted_key) = state.lru.pop_front() else {
            break;
        };
        if let Some(evicted) = state.entries.remove(&evicted_key) {
            state.total_bytes = state.total_bytes.saturating_sub(evicted.estimated_bytes);
            emit_metric("eviction", "capacity", 0, evicted.estimated_bytes, 0);
        }
    }
}

fn touch_lru(lru: &mut VecDeque<ParseResultCacheKey>, key: &ParseResultCacheKey) {
    if let Some(position) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(position);
    }
    lru.push_back(key.clone());
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn emit_metric(
    outcome: &'static str,
    reason: &'static str,
    source_bytes: usize,
    entry_bytes: usize,
    wait_us: u64,
) {
    ryeos_tracing::record_cache_metric(ryeos_tracing::CacheMetricSample {
        metric: "parser_result_cache",
        namespace: None,
        outcome,
        reason: Some(reason),
        source_bytes,
        entry_bytes,
        wait_microseconds: wait_us,
    });
    tracing::debug!(
        target: "ryeos.metrics",
        metric = "parser_result_cache",
        outcome,
        reason,
        source_bytes,
        entry_bytes,
        wait_us,
        "parser result cache metric"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(content: &str) -> ParseResultCacheKey {
        ParseResultCacheKey::new(
            "dispatcher".to_string(),
            "parser:test/example".to_string(),
            content.to_string(),
        )
    }

    #[test]
    fn successful_content_is_reused_independently_of_diagnostic_source_path() {
        let cache = ParseResultCache::default();
        let builds = AtomicUsize::new(0);
        // ParserDispatcher deliberately keeps source_path out of this key.
        // These two lookups model identical signed bytes reached through two
        // pinned workspaces with different diagnostic paths.
        for _ in 0..2 {
            let value = cache
                .get_or_build(key("content-one"), 11, || {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::json!({"value": 1}))
                })
                .unwrap();
            assert_eq!(value, serde_json::json!({"value": 1}));
        }
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn content_digest_distinguishes_cache_keys() {
        let cache = ParseResultCache::default();
        let builds = AtomicUsize::new(0);
        for cache_key in [key("content-one"), key("content-two")] {
            cache
                .get_or_build(cache_key, 11, || {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(Value::Null)
                })
                .unwrap();
        }
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failures_are_not_cached() {
        let cache = ParseResultCache::default();
        let builds = AtomicUsize::new(0);
        for _ in 0..2 {
            let result = cache.get_or_build(key("content"), 7, || {
                builds.fetch_add(1, Ordering::SeqCst);
                Err(EngineError::Internal("expected failure".to_string()))
            });
            assert!(result.is_err());
        }
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn concurrent_identical_keys_single_flight() {
        let cache = Arc::new(ParseResultCache::default());
        let builds = Arc::new(AtomicUsize::new(0));
        let build_started = Arc::new(Barrier::new(2));
        let release_build = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let cache = Arc::clone(&cache);
                let builds = Arc::clone(&builds);
                let build_started = Arc::clone(&build_started);
                let release_build = Arc::clone(&release_build);
                scope.spawn(move || {
                    cache
                        .get_or_build(key("content"), 7, || {
                            builds.fetch_add(1, Ordering::SeqCst);
                            build_started.wait();
                            release_build.wait();
                            Ok(serde_json::json!({"value": true}))
                        })
                        .unwrap()
                });
            }
            build_started.wait();
            release_build.wait();
        });
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }
}
