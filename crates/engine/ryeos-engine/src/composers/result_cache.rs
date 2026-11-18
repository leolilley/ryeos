//! Bounded in-memory cache of composer handler responses.
//!
//! A compose launch is a subprocess. For a composer whose signed handler
//! descriptor declares `cache: { mode: content_addressed }`, the response is
//! a function of the handler binary and the request alone, so a response
//! already computed for the same binary and the same request can stand in for
//! the launch.
//!
//! What the cache does not replace: every item is still discovered, read and
//! signature-verified on every resolution, and the request is rebuilt from
//! those freshly verified items before the key is computed. A changed,
//! re-signed or re-trusted item therefore produces a different request and a
//! different key. The engine's own checks on a composed value run on every
//! compose, on cached and fresh responses alike.
//!
//! Nothing is persisted. The cache lives in one [`super::ComposerRegistry`]
//! and its clones, so it is never shared across handler registries or launch
//! runtimes, and a daemon restart starts empty. Failed composes are never
//! stored.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ryeos_handler_protocol::{ComposeRequest, ComposeSuccess};

// The same bounds as the parser result cache.
const MAX_CACHE_ENTRIES: usize = 2048;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHE_ENTRY_BYTES: usize = 4 * 1024 * 1024;
// One entry never exceeds the whole cache, so the per-entry bound is the only
// admission bound an insert needs.
const _: () = assert!(MAX_CACHE_ENTRY_BYTES <= MAX_CACHE_BYTES);

/// Everything a cached compose response depends on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ComposeResultCacheKey {
    /// Fingerprint of the whole verified handler registry: every handler's
    /// signed descriptor, signer and binary digest.
    handler_registry_fingerprint: String,
    /// Canonical ref of the composer handler.
    composer_ref: String,
    /// SHA-256 of the signed descriptor that declares the handler cacheable.
    descriptor_content_digest: String,
    /// SHA-256 of the exact executable bytes that would be launched.
    binary_content_digest: String,
    /// SHA-256 of the serialized request: the composer config and, for the
    /// root and each ancestor, its identity, trust class and parsed value.
    request_digest: String,
}

impl ComposeResultCacheKey {
    /// The key of one compose request to one resolved handler. `None` when
    /// the request cannot be serialized, in which case nothing is cached.
    pub(super) fn new(
        handler_registry_fingerprint: &str,
        composer_ref: &str,
        descriptor_content_digest: &str,
        binary_content_digest: &str,
        request: &ComposeRequest,
    ) -> Option<Self> {
        let request_bytes = serde_json::to_vec(request).ok()?;
        Some(Self {
            handler_registry_fingerprint: handler_registry_fingerprint.to_owned(),
            composer_ref: composer_ref.to_owned(),
            descriptor_content_digest: descriptor_content_digest.to_owned(),
            binary_content_digest: binary_content_digest.to_owned(),
            request_digest: lillux::cas::sha256_hex(&request_bytes),
        })
    }

    fn estimated_bytes(&self) -> usize {
        self.handler_registry_fingerprint
            .len()
            .saturating_add(self.composer_ref.len())
            .saturating_add(self.descriptor_content_digest.len())
            .saturating_add(self.binary_content_digest.len())
            .saturating_add(self.request_digest.len())
    }
}

#[derive(Debug)]
struct CacheEntry {
    response: Arc<ComposeSuccess>,
    estimated_bytes: usize,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<ComposeResultCacheKey, CacheEntry>,
    /// Least recently used first.
    lru: VecDeque<ComposeResultCacheKey>,
    total_bytes: usize,
}

/// Counts of one cache since it was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposeResultCacheStats {
    /// Composes answered from the cache.
    pub hits: u64,
    /// Cacheable composes that launched the handler.
    pub misses: u64,
    /// Responses currently held.
    pub entries: usize,
    /// Estimated bytes currently held.
    pub bytes: usize,
}

#[derive(Debug, Default)]
pub(super) struct ComposeResultCache {
    state: Mutex<CacheState>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ComposeResultCache {
    /// The response stored for `key`, or the response of `launch`, which is
    /// stored when it succeeds and fits. A failed `launch` stores nothing and
    /// its error is returned as it is. Two threads that miss the same key at
    /// once both launch; each response is correct and the later one replaces
    /// the earlier.
    pub(super) fn get_or_launch<E>(
        &self,
        key: ComposeResultCacheKey,
        launch: impl FnOnce() -> Result<ComposeSuccess, E>,
    ) -> Result<ComposeSuccess, E> {
        {
            let mut state = self.lock();
            if let Some(response) = state
                .entries
                .get(&key)
                .map(|entry| Arc::clone(&entry.response))
            {
                touch_lru(&mut state.lru, &key);
                drop(state);
                self.hits.fetch_add(1, Ordering::Relaxed);
                emit_metric("hit", "ready", 0);
                return Ok(response.as_ref().clone());
            }
        }

        self.misses.fetch_add(1, Ordering::Relaxed);
        let response = launch()?;

        let response_bytes = serde_json::to_vec(&response)
            .map(|encoded| encoded.len())
            .unwrap_or(MAX_CACHE_ENTRY_BYTES.saturating_add(1));
        let estimated_bytes = key.estimated_bytes().saturating_add(response_bytes);
        if estimated_bytes <= MAX_CACHE_ENTRY_BYTES {
            insert_entry(
                &mut self.lock(),
                key,
                Arc::new(response.clone()),
                estimated_bytes,
            );
            emit_metric("fill", "success", estimated_bytes);
        } else {
            emit_metric("bypass", "entry_capacity", estimated_bytes);
        }
        Ok(response)
    }

    pub(super) fn stats(&self) -> ComposeResultCacheStats {
        let state = self.lock();
        ComposeResultCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            entries: state.entries.len(),
            bytes: state.total_bytes,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn insert_entry(
    state: &mut CacheState,
    key: ComposeResultCacheKey,
    response: Arc<ComposeSuccess>,
    estimated_bytes: usize,
) {
    if let Some(replaced) = state.entries.remove(&key) {
        state.total_bytes = state.total_bytes.saturating_sub(replaced.estimated_bytes);
        if let Some(position) = state.lru.iter().position(|candidate| candidate == &key) {
            state.lru.remove(position);
        }
    }

    state.total_bytes = state.total_bytes.saturating_add(estimated_bytes);
    state.lru.push_back(key.clone());
    state.entries.insert(
        key,
        CacheEntry {
            response,
            estimated_bytes,
        },
    );

    while state.entries.len() > MAX_CACHE_ENTRIES || state.total_bytes > MAX_CACHE_BYTES {
        let Some(evicted_key) = state.lru.pop_front() else {
            break;
        };
        if let Some(evicted) = state.entries.remove(&evicted_key) {
            state.total_bytes = state.total_bytes.saturating_sub(evicted.estimated_bytes);
            emit_metric("eviction", "capacity", evicted.estimated_bytes);
        }
    }
}

fn touch_lru(lru: &mut VecDeque<ComposeResultCacheKey>, key: &ComposeResultCacheKey) {
    if let Some(position) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(position);
    }
    lru.push_back(key.clone());
}

fn emit_metric(outcome: &'static str, reason: &'static str, entry_bytes: usize) {
    ryeos_tracing::record_cache_metric(ryeos_tracing::CacheMetricSample {
        metric: "composer_result_cache",
        namespace: None,
        outcome,
        reason: Some(reason),
        source_bytes: 0,
        entry_bytes,
        wait_microseconds: 0,
    });
    tracing::debug!(
        target: "ryeos.metrics",
        metric = "composer_result_cache",
        outcome,
        reason,
        entry_bytes,
        "composer result cache metric"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_handler_protocol::{ComposeInput, ComposeItemContext, TrustClassWire};
    use serde_json::{Value, json};
    use std::cell::Cell;

    fn input(resolved_ref: &str, trust_class: TrustClassWire, parsed: Value) -> ComposeInput {
        ComposeInput {
            item: ComposeItemContext {
                requested_id: resolved_ref.to_owned(),
                resolved_ref: resolved_ref.to_owned(),
                trust_class,
            },
            parsed,
        }
    }

    fn request(config: Value, root: Value, ancestor: Value) -> ComposeRequest {
        ComposeRequest {
            composer_config: config,
            root: input("view:test/root", TrustClassWire::TrustedBundle, root),
            ancestors: vec![input(
                "view:test/base",
                TrustClassWire::TrustedBundle,
                ancestor,
            )],
        }
    }

    fn baseline_request() -> ComposeRequest {
        request(
            json!({"extends_field": "extends"}),
            json!({"title": "Root", "extends": "view:test/base"}),
            json!({"title": "Base"}),
        )
    }

    fn key_for(request: &ComposeRequest) -> ComposeResultCacheKey {
        ComposeResultCacheKey::new(
            "registry-one",
            "handler:test/composer",
            "descriptor-one",
            "binary-one",
            request,
        )
        .unwrap()
    }

    fn response(marker: &str) -> ComposeSuccess {
        ComposeSuccess {
            composed: json!({ "marker": marker }),
            derived: HashMap::new(),
            policy_facts: HashMap::new(),
        }
    }

    /// Compose through `cache`, counting launches.
    fn compose(
        cache: &ComposeResultCache,
        launches: &Cell<usize>,
        key: ComposeResultCacheKey,
        marker: &str,
    ) -> ComposeSuccess {
        cache
            .get_or_launch(key, || {
                launches.set(launches.get() + 1);
                Ok::<_, String>(response(marker))
            })
            .unwrap()
    }

    #[test]
    fn the_same_request_is_launched_once() {
        let cache = ComposeResultCache::default();
        let launches = Cell::new(0);
        let first = compose(&cache, &launches, key_for(&baseline_request()), "first");
        // An equal request built again from the same items.
        let second = compose(&cache, &launches, key_for(&baseline_request()), "second");
        assert_eq!(launches.get(), 1);
        assert_eq!(first.composed, json!({"marker": "first"}));
        assert_eq!(second.composed, first.composed);
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.entries), (1, 1, 1));
    }

    #[test]
    fn a_changed_item_misses() {
        let cache = ComposeResultCache::default();
        let launches = Cell::new(0);
        compose(&cache, &launches, key_for(&baseline_request()), "original");

        // The root item's content changed.
        let mut edited_root = baseline_request();
        edited_root.root.parsed["title"] = json!("Root, edited");
        // An ancestor's content changed.
        let mut edited_ancestor = baseline_request();
        edited_ancestor.ancestors[0].parsed["title"] = json!("Base, edited");
        // The same content now verifies under a different trust class.
        let mut retrusted = baseline_request();
        retrusted.root.item.trust_class = TrustClassWire::UntrustedProject;
        // A different item supplies the ancestor.
        let mut other_ancestor = baseline_request();
        other_ancestor.ancestors[0].item.resolved_ref = "view:test/other-base".into();
        // The ancestor is gone.
        let mut no_ancestor = baseline_request();
        no_ancestor.ancestors.clear();

        for (changed, marker) in [
            (edited_root, "edited root"),
            (edited_ancestor, "edited ancestor"),
            (retrusted, "retrusted"),
            (other_ancestor, "other ancestor"),
            (no_ancestor, "no ancestor"),
        ] {
            let before = launches.get();
            let composed = compose(&cache, &launches, key_for(&changed), marker);
            assert_eq!(launches.get(), before + 1, "{marker} must launch");
            assert_eq!(composed.composed, json!({ "marker": marker }));
        }
        // The original is still served for the original request.
        let original = compose(&cache, &launches, key_for(&baseline_request()), "unused");
        assert_eq!(original.composed, json!({"marker": "original"}));
        assert_eq!(launches.get(), 6);
    }

    #[test]
    fn an_item_re_signed_with_different_content_under_the_same_ref_misses() {
        let cache = ComposeResultCache::default();
        let launches = Cell::new(0);
        compose(&cache, &launches, key_for(&baseline_request()), "original");

        // Same ref, same requested id, same trust class after re-signing; only
        // the signed content differs.
        let mut re_signed = baseline_request();
        re_signed.root.parsed = json!({"title": "Replaced", "extends": "view:test/base"});
        assert_eq!(
            re_signed.root.item.resolved_ref,
            baseline_request().root.item.resolved_ref
        );
        let composed = compose(&cache, &launches, key_for(&re_signed), "re-signed");
        assert_eq!(launches.get(), 2);
        assert_eq!(composed.composed, json!({"marker": "re-signed"}));
    }

    #[test]
    fn a_changed_composer_config_binary_descriptor_or_registry_misses() {
        let cache = ComposeResultCache::default();
        let launches = Cell::new(0);
        let request = baseline_request();
        compose(&cache, &launches, key_for(&request), "original");

        let mut reconfigured = baseline_request();
        reconfigured.composer_config = json!({"extends_field": "parent"});
        let changed_keys = [
            ("composer config", key_for(&reconfigured)),
            (
                "binary digest",
                ComposeResultCacheKey::new(
                    "registry-one",
                    "handler:test/composer",
                    "descriptor-one",
                    "binary-two",
                    &request,
                )
                .unwrap(),
            ),
            (
                "descriptor digest",
                ComposeResultCacheKey::new(
                    "registry-one",
                    "handler:test/composer",
                    "descriptor-two",
                    "binary-one",
                    &request,
                )
                .unwrap(),
            ),
            (
                "handler registry",
                ComposeResultCacheKey::new(
                    "registry-two",
                    "handler:test/composer",
                    "descriptor-one",
                    "binary-one",
                    &request,
                )
                .unwrap(),
            ),
            (
                "composer ref",
                ComposeResultCacheKey::new(
                    "registry-one",
                    "handler:test/other-composer",
                    "descriptor-one",
                    "binary-one",
                    &request,
                )
                .unwrap(),
            ),
        ];
        for (what, key) in changed_keys {
            let before = launches.get();
            let composed = compose(&cache, &launches, key, what);
            assert_eq!(launches.get(), before + 1, "a changed {what} must launch");
            assert_eq!(composed.composed, json!({ "marker": what }));
        }
    }

    #[test]
    fn a_failed_or_timed_out_compose_is_never_stored() {
        let cache = ComposeResultCache::default();
        let launches = Cell::new(0);
        let key = key_for(&baseline_request());
        for failure in [
            "composer reported an invalid definition",
            "timed out after 30s",
        ] {
            let result = cache.get_or_launch(key.clone(), || {
                launches.set(launches.get() + 1);
                Err::<ComposeSuccess, _>(failure.to_string())
            });
            assert_eq!(result.unwrap_err(), failure);
            assert_eq!(cache.stats().entries, 0);
        }
        assert_eq!(launches.get(), 2, "each failure launched again");

        // The next attempt launches again, and its success is then stored.
        let composed = compose(&cache, &launches, key.clone(), "recovered");
        assert_eq!(launches.get(), 3);
        assert_eq!(composed.composed, json!({"marker": "recovered"}));
        compose(&cache, &launches, key, "unused");
        assert_eq!(launches.get(), 3);
    }

    #[test]
    fn entry_count_stays_within_its_bound_and_evicts_least_recently_used() {
        let cache = ComposeResultCache::default();
        let launches = Cell::new(0);
        let numbered = |index: usize| {
            key_for(&request(
                json!({}),
                json!({ "index": index }),
                json!({"title": "Base"}),
            ))
        };
        for index in 0..MAX_CACHE_ENTRIES {
            compose(&cache, &launches, numbered(index), "filled");
        }
        assert_eq!(cache.stats().entries, MAX_CACHE_ENTRIES);

        // Use the oldest entry, then add one more: the second oldest goes.
        compose(&cache, &launches, numbered(0), "unused");
        assert_eq!(launches.get(), MAX_CACHE_ENTRIES);
        compose(&cache, &launches, numbered(MAX_CACHE_ENTRIES), "one more");
        assert_eq!(cache.stats().entries, MAX_CACHE_ENTRIES);

        let before = launches.get();
        compose(&cache, &launches, numbered(0), "unused");
        assert_eq!(launches.get(), before, "the recently used entry was kept");
        compose(&cache, &launches, numbered(1), "relaunched");
        assert_eq!(launches.get(), before + 1, "the least recently used went");
        assert_eq!(cache.stats().entries, MAX_CACHE_ENTRIES);
    }

    #[test]
    fn total_bytes_stay_within_their_bound() {
        let cache = ComposeResultCache::default();
        let launches = Cell::new(0);
        // Each response is a little under a mebibyte, so the byte bound is
        // reached long before the entry bound.
        let large = "x".repeat(1024 * 1024 - 4096);
        for index in 0..40 {
            let key = key_for(&request(
                json!({}),
                json!({ "index": index }),
                json!({"title": "Base"}),
            ));
            cache
                .get_or_launch(key, || {
                    launches.set(launches.get() + 1);
                    Ok::<_, String>(ComposeSuccess {
                        composed: json!({ "body": large }),
                        derived: HashMap::new(),
                        policy_facts: HashMap::new(),
                    })
                })
                .unwrap();
            assert!(cache.stats().bytes <= MAX_CACHE_BYTES);
        }
        let stats = cache.stats();
        assert!(stats.entries < 40, "older responses were evicted");
        assert!(stats.entries >= 15);
    }

    #[test]
    fn a_response_over_the_entry_bound_is_returned_but_not_stored() {
        let cache = ComposeResultCache::default();
        let launches = Cell::new(0);
        let key = key_for(&baseline_request());
        let oversized = "x".repeat(MAX_CACHE_ENTRY_BYTES + 1);
        for _ in 0..2 {
            let composed = cache
                .get_or_launch(key.clone(), || {
                    launches.set(launches.get() + 1);
                    Ok::<_, String>(ComposeSuccess {
                        composed: json!({ "body": oversized }),
                        derived: HashMap::new(),
                        policy_facts: HashMap::new(),
                    })
                })
                .unwrap();
            assert!(composed.composed["body"].as_str().unwrap().len() > MAX_CACHE_ENTRY_BYTES);
        }
        assert_eq!(launches.get(), 2);
        assert_eq!(cache.stats().entries, 0);
    }
}
