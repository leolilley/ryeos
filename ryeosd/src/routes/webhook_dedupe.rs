//! Webhook delivery-id deduplication store.
//!
//! Replay protection layer for HMAC-authenticated routes. Routes
//! that opt in (via `auth_config.dedupe = { ttl_secs, max_entries }`)
//! reject any incoming request whose `(namespace, delivery_id)` pair
//! has already been seen within the TTL window.
//!
//! The first tuple element is a **namespace** supplied by the caller
//! (typically the `route_id`). This ensures per-route dedupe
//! isolation: the same `delivery_id` arriving on two unrelated
//! routes is NOT a replay — each route has its own seen-set.
//!
//! Some verifier configurations require dedupe unconditionally
//! (e.g. when no timestamp window is configured, dedupe is the
//! only replay defense). Compile-time validation enforces the
//! pairing of `delivery_id` extraction + `dedupe` configuration;
//! there is no silent fallback.
//!
//! Storage is in-memory and per-process. A daemon restart loses the
//! seen-set; that is acceptable because legitimate retries arrive
//! within the TTL window of producer-side semantics (configured
//! TTL trades retry tolerance against memory).
//!
//! Eviction strategy: lazy purge on insert. Entries with insert
//! timestamps older than `ttl_secs` are removed before the new
//! insertion. If the post-purge size still exceeds `max_entries`,
//! the oldest remaining entries are dropped to make room.
//! No background task; the cost is amortized into requests.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// Namespace for a single dedupe scope. Constructed only from a
/// route id; this makes it impossible to accidentally dedupe across
/// routes (which would let one route's traffic reject another
/// route's legitimate replay-within-window).
///
/// Hold by reference, not by value — the underlying storage
/// allocates strings only on insert.
#[derive(Debug, Clone, Copy)]
pub struct RouteDedupeNamespace<'a>(&'a str);

impl<'a> RouteDedupeNamespace<'a> {
    pub fn for_route(route_id: &'a str) -> Self {
        Self(route_id)
    }

    pub fn as_str(&self) -> &str {
        self.0
    }
}

/// Per-route dedupe configuration. Compiled from a route's
/// `auth_config.dedupe` block by the verifier at route-table build
/// time. `ttl_secs > 0` and `max_entries > 0` are enforced at compile.
#[derive(Debug, Clone, Copy)]
pub struct DedupeConfig {
    pub ttl_secs: u64,
    pub max_entries: usize,
}

/// In-memory dedupe store keyed by `(namespace, delivery_id)` →
/// insert-time. Process-wide; a single store serves all dedupe-
/// enabled routes. Per-route isolation is achieved by passing
/// `route_id` as the namespace — the same `delivery_id` on two
/// different routes does NOT collide. A single Mutex suffices for
/// current load (webhooks arrive at human rates).
pub struct WebhookDedupeStore {
    inner: Mutex<Inner>,
}

struct Inner {
    seen: HashMap<(String, String), u64>,
    /// Insertion-order queue used for cheap "evict oldest" without
    /// having to scan the HashMap. Each entry holds the same key as
    /// the corresponding `seen` entry. On purge we walk the front
    /// of the queue while it points at expired entries.
    order: VecDeque<(String, String)>,
}

/// Outcome of a `mark_seen` call.
#[derive(Debug, PartialEq, Eq)]
pub enum DedupeOutcome {
    /// First time this `(namespace, delivery_id)` was seen within the
    /// TTL window. Caller may proceed.
    Fresh,
    /// Already seen within TTL — caller MUST reject the request as
    /// a replay (return Unauthorized in the verifier, log + metric
    /// in the webhook telemetry surface).
    Replay,
}

impl WebhookDedupeStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                seen: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Record `(namespace, delivery_id)` and return whether it had
    /// been seen within `cfg.ttl_secs` already. Lazy purges entries
    /// older than `cfg.ttl_secs` and trims to `cfg.max_entries`.
    ///
    /// The caller (typically a verifier) MUST pass `route_id`
    /// via `RouteDedupeNamespace::for_route` so that dedupe is scoped
    /// per-route rather than per-verifier. Sharing a namespace across
    /// routes would cause false replay rejects when two unrelated
    /// routes receive the same `delivery_id` from their respective
    /// producers.
    pub fn mark_seen(
        &self,
        namespace: RouteDedupeNamespace<'_>,
        delivery_id: &str,
        now_unix: u64,
        cfg: DedupeConfig,
    ) -> DedupeOutcome {
        let key = (namespace.as_str().to_string(), delivery_id.to_string());
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        // Purge expired front entries first.
        purge_expired(&mut g, now_unix, cfg.ttl_secs);

        if let Some(&prev) = g.seen.get(&key) {
            // Re-check against TTL — purge above only walks the
            // front of the queue; older entries reachable via the
            // hash map still need an explicit window check.
            if now_unix.saturating_sub(prev) <= cfg.ttl_secs {
                return DedupeOutcome::Replay;
            }
            // Expired but still in the map (because the queue
            // didn't reach it yet). Treat as fresh and overwrite.
            g.seen.insert(key.clone(), now_unix);
            // Note: we don't remove the stale `order` entry; the
            // next purge / overflow trim will handle it.
            g.order.push_back(key);
            trim_to_capacity(&mut g, cfg.max_entries);
            return DedupeOutcome::Fresh;
        }

        g.seen.insert(key.clone(), now_unix);
        g.order.push_back(key);
        trim_to_capacity(&mut g, cfg.max_entries);
        DedupeOutcome::Fresh
    }

    /// Total entries currently held. Test/inspection use.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.seen.len()
    }
}

fn purge_expired(g: &mut Inner, now_unix: u64, ttl_secs: u64) {
    while let Some(front_key) = g.order.front().cloned() {
        match g.seen.get(&front_key) {
            Some(&t) if now_unix.saturating_sub(t) > ttl_secs => {
                g.seen.remove(&front_key);
                g.order.pop_front();
            }
            Some(_) => break,
            None => {
                // Entry was overwritten / orphaned in `order`.
                // Drop it without touching `seen`.
                g.order.pop_front();
            }
        }
    }
}

fn trim_to_capacity(g: &mut Inner, max_entries: usize) {
    while g.seen.len() > max_entries {
        let Some(front_key) = g.order.pop_front() else {
            break;
        };
        g.seen.remove(&front_key);
    }
}

impl Default for WebhookDedupeStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(ttl: u64, cap: usize) -> DedupeConfig {
        DedupeConfig {
            ttl_secs: ttl,
            max_entries: cap,
        }
    }

    #[test]
    fn first_seen_is_fresh() {
        let store = WebhookDedupeStore::new();
        assert_eq!(
            store.mark_seen(RouteDedupeNamespace::for_route("r1"), "evt_1", 1000, cfg(60, 10)),
            DedupeOutcome::Fresh
        );
    }

    #[test]
    fn second_within_ttl_is_replay() {
        let store = WebhookDedupeStore::new();
        store.mark_seen(RouteDedupeNamespace::for_route("r1"), "evt_1", 1000, cfg(60, 10));
        assert_eq!(
            store.mark_seen(RouteDedupeNamespace::for_route("r1"), "evt_1", 1010, cfg(60, 10)),
            DedupeOutcome::Replay
        );
    }

    #[test]
    fn second_after_ttl_is_fresh() {
        let store = WebhookDedupeStore::new();
        store.mark_seen(RouteDedupeNamespace::for_route("r1"), "evt_1", 1000, cfg(60, 10));
        assert_eq!(
            store.mark_seen(RouteDedupeNamespace::for_route("r1"), "evt_1", 2000, cfg(60, 10)),
            DedupeOutcome::Fresh
        );
    }

    #[test]
    fn different_namespaces_do_not_collide() {
        let store = WebhookDedupeStore::new();
        let c = cfg(60, 10);
        assert_eq!(store.mark_seen(RouteDedupeNamespace::for_route("route_a"), "evt_1", 1000, c), DedupeOutcome::Fresh);
        assert_eq!(store.mark_seen(RouteDedupeNamespace::for_route("route_b"), "evt_1", 1010, c), DedupeOutcome::Fresh);
        // Replay only within the same namespace.
        assert_eq!(store.mark_seen(RouteDedupeNamespace::for_route("route_a"), "evt_1", 1020, c), DedupeOutcome::Replay);
    }

    #[test]
    fn different_delivery_ids_same_namespace() {
        let store = WebhookDedupeStore::new();
        store.mark_seen(RouteDedupeNamespace::for_route("r1"), "evt_1", 1000, cfg(60, 10));
        assert_eq!(
            store.mark_seen(RouteDedupeNamespace::for_route("r1"), "evt_2", 1010, cfg(60, 10)),
            DedupeOutcome::Fresh
        );
    }

    #[test]
    fn capacity_evicts_oldest() {
        let store = WebhookDedupeStore::new();
        let c = cfg(60, 2);
        store.mark_seen(RouteDedupeNamespace::for_route("p"), "a", 1000, c);
        store.mark_seen(RouteDedupeNamespace::for_route("p"), "b", 1001, c);
        store.mark_seen(RouteDedupeNamespace::for_route("p"), "c", 1002, c);
        assert_eq!(store.len(), 2);
        // 'a' is the oldest and should have been evicted; re-seen
        // is fresh because not present.
        assert_eq!(
            store.mark_seen(RouteDedupeNamespace::for_route("p"), "a", 1003, c),
            DedupeOutcome::Fresh
        );
        // 'c' is still inside; second insert is replay.
        assert_eq!(
            store.mark_seen(RouteDedupeNamespace::for_route("p"), "c", 1004, c),
            DedupeOutcome::Replay
        );
    }

    #[test]
    fn ttl_purges_on_subsequent_insert() {
        let store = WebhookDedupeStore::new();
        let c = cfg(10, 100);
        store.mark_seen(RouteDedupeNamespace::for_route("p"), "a", 1000, c);
        store.mark_seen(RouteDedupeNamespace::for_route("p"), "b", 1001, c);
        store.mark_seen(RouteDedupeNamespace::for_route("p"), "c", 2000, c);
        // After the third insert, purge_expired should have removed
        // a and b (both > 10s old at t=2000).
        assert_eq!(store.len(), 1);
    }
}
