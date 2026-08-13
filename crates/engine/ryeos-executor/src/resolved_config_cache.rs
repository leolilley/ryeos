//! Bounded, generation-retiring, single-flight cache for verified config snapshots.
//!
//! Values are immutable parsed configuration plus complete positive/negative
//! dependency proofs. The caller revalidates mutable LiveFs authority against
//! the active roots before consuming a hit; contradictions against projectless
//! or exact pinned/COW authority fail closed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::dispatch_error::DispatchError;

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_PENDING: usize = 128;
const IDLE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct SnapshotCacheKey {
    pub(crate) namespace: &'static str,
    pub(crate) retirement_scope: String,
    pub(crate) generation: String,
    pub(crate) generation_epoch: Option<u64>,
    pub(crate) identity: String,
}

impl std::fmt::Debug for SnapshotCacheKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnapshotCacheKey")
            .field("namespace", &self.namespace)
            .field("retirement_scope", &self.retirement_scope)
            .field("generation", &self.generation)
            .field("generation_epoch", &self.generation_epoch)
            .field("identity", &self.identity)
            .finish()
    }
}

struct CacheEntry<T> {
    value: Arc<T>,
    estimated_bytes: usize,
    last_touched: Instant,
}

enum PendingOutcome<T> {
    Success(Arc<T>),
    Failure(Arc<DispatchError>),
    Retry,
}

pub(crate) struct PendingFill<T> {
    result: Mutex<Option<PendingOutcome<T>>>,
    completed: Notify,
}

impl<T> Default for PendingFill<T> {
    fn default() -> Self {
        Self {
            result: Mutex::new(None),
            completed: Notify::new(),
        }
    }
}

struct CacheState<T> {
    entries: HashMap<SnapshotCacheKey, CacheEntry<T>>,
    lru: VecDeque<SnapshotCacheKey>,
    pending: HashMap<SnapshotCacheKey, Arc<PendingFill<T>>>,
    active_generations: HashMap<(&'static str, String), (u64, String)>,
    total_bytes: usize,
}

impl<T> Default for CacheState<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::new(),
            pending: HashMap::new(),
            active_generations: HashMap::new(),
            total_bytes: 0,
        }
    }
}

pub(crate) struct SnapshotCache<T> {
    state: Mutex<CacheState<T>>,
}

impl<T> Default for SnapshotCache<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(CacheState::default()),
        }
    }
}

pub(crate) enum Lookup<'a, T> {
    Hit { value: Arc<T>, entry_bytes: usize },
    Wait { pending: Arc<PendingFill<T>> },
    Build(FillGuard<'a, T>),
    Bypass,
}

pub(crate) struct FillGuard<'a, T> {
    cache: &'a SnapshotCache<T>,
    key: SnapshotCacheKey,
    pending: Arc<PendingFill<T>>,
    completed: bool,
}

impl<T> FillGuard<'_, T> {
    pub(crate) fn complete(mut self, value: T, estimated_bytes: usize) -> Arc<T> {
        let value = Arc::new(value);
        let estimated_bytes = estimated_bytes
            .saturating_add(self.key.retirement_scope.len())
            .saturating_add(self.key.generation.len())
            .saturating_add(self.key.identity.len());
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sweep_idle(&mut state);
        let generation_is_current = state
            .active_generations
            .get(&(self.key.namespace, self.key.retirement_scope.clone()))
            .map(|(epoch, generation)| {
                self.key.generation_epoch == Some(*epoch) && generation == &self.key.generation
            })
            .unwrap_or(true);
        if generation_is_current && estimated_bytes <= MAX_BYTES {
            evict_to_fit(&mut state, estimated_bytes);
            if state.entries.len() < MAX_ENTRIES
                && state.total_bytes.saturating_add(estimated_bytes) <= MAX_BYTES
            {
                state.total_bytes = state.total_bytes.saturating_add(estimated_bytes);
                state.lru.push_back(self.key.clone());
                state.entries.insert(
                    self.key.clone(),
                    CacheEntry {
                        value: value.clone(),
                        estimated_bytes,
                        last_touched: Instant::now(),
                    },
                );
            }
        } else if !generation_is_current {
            emit_metric(
                self.key.namespace,
                CacheOutcome::Bypass,
                CacheReason::GenerationRetired,
                estimated_bytes,
                0,
            );
        } else if estimated_bytes > MAX_BYTES {
            emit_metric(
                self.key.namespace,
                CacheOutcome::Bypass,
                CacheReason::EntryTooLarge,
                estimated_bytes,
                0,
            );
        }
        *self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(PendingOutcome::Success(value.clone()));
        remove_pending_if_same(&mut state, &self.key, &self.pending);
        prune_generation_metadata(&mut state);
        drop(state);
        self.pending.completed.notify_waiters();
        self.completed = true;
        value
    }

    pub(crate) fn fail(mut self, error: DispatchError) -> Arc<DispatchError> {
        let error = Arc::new(error);
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(PendingOutcome::Failure(error.clone()));
        remove_pending_if_same(&mut state, &self.key, &self.pending);
        prune_generation_metadata(&mut state);
        drop(state);
        emit_metric(
            self.key.namespace,
            CacheOutcome::Miss,
            CacheReason::FillFailed,
            0,
            0,
        );
        self.pending.completed.notify_waiters();
        self.completed = true;
        error
    }

    pub(crate) fn cancel(mut self) {
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(PendingOutcome::Retry);
        remove_pending_if_same(&mut state, &self.key, &self.pending);
        prune_generation_metadata(&mut state);
        drop(state);
        self.pending.completed.notify_waiters();
        self.completed = true;
    }
}

impl<T> Drop for FillGuard<'_, T> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let error = Arc::new(DispatchError::Internal(anyhow::anyhow!(
            "cache fill ended without publishing its result"
        )));
        *self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(PendingOutcome::Failure(error));
        remove_pending_if_same(&mut state, &self.key, &self.pending);
        prune_generation_metadata(&mut state);
        drop(state);
        emit_metric(
            self.key.namespace,
            CacheOutcome::Miss,
            CacheReason::FillFailed,
            0,
            0,
        );
        self.pending.completed.notify_waiters();
    }
}

impl<T> SnapshotCache<T> {
    pub(crate) fn begin(&self, key: SnapshotCacheKey) -> Lookup<'_, T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sweep_idle(&mut state);
        prune_generation_metadata(&mut state);
        retire_previous_generation(&mut state, &key);
        if let Some((value, entry_bytes)) = state.entries.get_mut(&key).map(|entry| {
            entry.last_touched = Instant::now();
            (entry.value.clone(), entry.estimated_bytes)
        }) {
            touch_lru(&mut state.lru, &key);
            return Lookup::Hit { value, entry_bytes };
        }
        if let Some(pending) = state.pending.get(&key) {
            return Lookup::Wait {
                pending: pending.clone(),
            };
        }
        if state.pending.len() >= MAX_PENDING {
            return Lookup::Bypass;
        }
        let pending = Arc::new(PendingFill::default());
        state.pending.insert(key.clone(), pending.clone());
        Lookup::Build(FillGuard {
            cache: self,
            key,
            pending,
            completed: false,
        })
    }

    pub(crate) fn discard_if_same(&self, key: &SnapshotCacheKey, value: &Arc<T>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .entries
            .get(key)
            .is_some_and(|entry| Arc::ptr_eq(&entry.value, value))
        {
            remove_entry(&mut state, key);
        }
        prune_generation_metadata(&mut state);
    }
}

impl<T> PendingFill<T> {
    pub(crate) async fn wait(&self) -> Result<Option<Arc<T>>, Arc<DispatchError>> {
        loop {
            let notified = self.completed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
            {
                return match result {
                    PendingOutcome::Success(value) => Ok(Some(value.clone())),
                    PendingOutcome::Failure(error) => Err(error.clone()),
                    PendingOutcome::Retry => Ok(None),
                };
            }
            notified.as_mut().await;
        }
    }
}

fn retire_previous_generation<T>(state: &mut CacheState<T>, key: &SnapshotCacheKey) {
    let Some(epoch) = key.generation_epoch else {
        return;
    };
    let scope = (key.namespace, key.retirement_scope.clone());
    if let Some((active_epoch, active_generation)) = state.active_generations.get(&scope)
        && (*active_epoch > epoch
            || (*active_epoch == epoch && active_generation == &key.generation))
    {
        return;
    }
    state
        .active_generations
        .insert(scope.clone(), (epoch, key.generation.clone()));
    let stale = state
        .entries
        .keys()
        .filter(|candidate| {
            candidate.namespace == scope.0
                && candidate.retirement_scope == scope.1
                && (candidate.generation_epoch != Some(epoch)
                    || candidate.generation != key.generation)
        })
        .cloned()
        .collect::<Vec<_>>();
    for stale_key in stale {
        let entry_bytes = remove_entry(state, &stale_key);
        emit_metric(
            key.namespace,
            CacheOutcome::Eviction,
            CacheReason::GenerationRetired,
            entry_bytes,
            0,
        );
    }
}

fn sweep_idle<T>(state: &mut CacheState<T>) {
    let now = Instant::now();
    let stale = state
        .entries
        .iter()
        .filter(|(_, entry)| now.saturating_duration_since(entry.last_touched) >= IDLE_TTL)
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in stale {
        let entry_bytes = remove_entry(state, &key);
        emit_metric(
            key.namespace,
            CacheOutcome::Eviction,
            CacheReason::IdleTtl,
            entry_bytes,
            0,
        );
    }
}

fn evict_to_fit<T>(state: &mut CacheState<T>, incoming_bytes: usize) {
    while state.entries.len() >= MAX_ENTRIES
        || state.total_bytes.saturating_add(incoming_bytes) > MAX_BYTES
    {
        let Some(oldest) = state.lru.pop_front() else {
            break;
        };
        if let Some(entry) = state.entries.remove(&oldest) {
            state.total_bytes = state.total_bytes.saturating_sub(entry.estimated_bytes);
            emit_metric(
                oldest.namespace,
                CacheOutcome::Eviction,
                CacheReason::Capacity,
                entry.estimated_bytes,
                0,
            );
        }
    }
}

fn touch_lru<K: Eq + Clone>(lru: &mut VecDeque<K>, key: &K) {
    if let Some(position) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(position);
    }
    lru.push_back(key.clone());
}

fn remove_entry<T>(state: &mut CacheState<T>, key: &SnapshotCacheKey) -> usize {
    let mut removed_bytes = 0;
    if let Some(entry) = state.entries.remove(key) {
        state.total_bytes = state.total_bytes.saturating_sub(entry.estimated_bytes);
        removed_bytes = entry.estimated_bytes;
    }
    if let Some(position) = state.lru.iter().position(|candidate| candidate == key) {
        state.lru.remove(position);
    }
    removed_bytes
}

fn remove_pending_if_same<T>(
    state: &mut CacheState<T>,
    key: &SnapshotCacheKey,
    pending: &Arc<PendingFill<T>>,
) {
    if state
        .pending
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, pending))
    {
        state.pending.remove(key);
    }
}

fn prune_generation_metadata<T>(state: &mut CacheState<T>) {
    let live_scopes = state
        .entries
        .keys()
        .chain(state.pending.keys())
        .map(|key| (key.namespace, key.retirement_scope.clone()))
        .collect::<HashSet<_>>();
    state
        .active_generations
        .retain(|scope, _| live_scopes.contains(scope));
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CacheOutcome {
    Hit,
    Miss,
    Bypass,
    Eviction,
}

impl CacheOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Bypass => "bypass",
            Self::Eviction => "eviction",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CacheReason {
    Ready,
    SingleFlight,
    Cold,
    StaleProof,
    PendingCapacity,
    EntryTooLarge,
    FillFailed,
    IdleTtl,
    Capacity,
    GenerationRetired,
}

impl CacheReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::SingleFlight => "single_flight",
            Self::Cold => "cold",
            Self::StaleProof => "stale_proof",
            Self::PendingCapacity => "pending_capacity",
            Self::EntryTooLarge => "entry_too_large",
            Self::FillFailed => "fill_failed",
            Self::IdleTtl => "idle_ttl",
            Self::Capacity => "capacity",
            Self::GenerationRetired => "generation_retired",
        }
    }
}

pub(crate) fn emit_metric(
    namespace: &'static str,
    outcome: CacheOutcome,
    reason: CacheReason,
    entry_bytes: usize,
    wait_milliseconds: u64,
) {
    ryeos_tracing::record_cache_metric(ryeos_tracing::CacheMetricSample {
        metric: "resolved_config_snapshot_cache",
        namespace: Some(namespace),
        outcome: outcome.as_str(),
        reason: Some(reason.as_str()),
        source_bytes: 0,
        entry_bytes,
        wait_microseconds: wait_milliseconds.saturating_mul(1_000),
    });
    tracing::debug!(
        target: "ryeos.metrics",
        metric = "resolved_config_snapshot_cache",
        namespace,
        outcome = outcome.as_str(),
        reason = reason.as_str(),
        entry_bytes,
        wait_milliseconds,
        "resolved config snapshot cache metric"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(generation: &str, identity: &str) -> SnapshotCacheKey {
        SnapshotCacheKey {
            namespace: "test",
            retirement_scope: "scope".to_owned(),
            generation: generation.to_owned(),
            generation_epoch: generation.parse().ok(),
            identity: identity.to_owned(),
        }
    }

    #[test]
    fn completed_fill_is_a_ready_hit() {
        let cache = SnapshotCache::<String>::default();
        let key = key("1", "ready");
        let Lookup::Build(fill) = cache.begin(key.clone()) else {
            panic!("cold lookup must build");
        };
        fill.complete("value".to_owned(), 5);
        assert!(matches!(cache.begin(key), Lookup::Hit { .. }));
    }

    #[tokio::test]
    async fn concurrent_lookup_waits_for_single_flight_result() {
        let cache = SnapshotCache::<String>::default();
        let key = key("1", "single-flight");
        let Lookup::Build(fill) = cache.begin(key.clone()) else {
            panic!("leader must build");
        };
        let Lookup::Wait { pending } = cache.begin(key) else {
            panic!("concurrent lookup must wait");
        };
        fill.complete("value".to_owned(), 5);
        assert_eq!(
            pending.wait().await.unwrap().as_deref().map(String::as_str),
            Some("value")
        );
    }

    #[tokio::test]
    async fn oversize_leader_result_is_shared_with_waiters_but_not_cached() {
        let cache = SnapshotCache::<String>::default();
        let key = key("1", "oversize-single-flight");
        let Lookup::Build(fill) = cache.begin(key.clone()) else {
            panic!("leader must build");
        };
        let Lookup::Wait { pending } = cache.begin(key.clone()) else {
            panic!("concurrent lookup must wait");
        };
        let published = fill.complete("value".to_owned(), MAX_BYTES.saturating_add(1));
        let waited = pending
            .wait()
            .await
            .unwrap()
            .expect("the admitted waiter receives the leader result");
        assert!(Arc::ptr_eq(&published, &waited));
        assert!(
            matches!(cache.begin(key), Lookup::Build(_)),
            "oversize results must not become later cache hits"
        );
    }

    #[tokio::test]
    async fn concurrent_lookup_receives_the_exact_leader_failure_without_caching_it() {
        let cache = SnapshotCache::<String>::default();
        let key = key("1", "failure");
        let Lookup::Build(fill) = cache.begin(key.clone()) else {
            panic!("leader must build");
        };
        let Lookup::Wait { pending } = cache.begin(key.clone()) else {
            panic!("concurrent lookup must wait");
        };
        let published = fill.fail(DispatchError::Internal(anyhow::anyhow!("exact failure")));
        let waited = pending.wait().await.unwrap_err();
        assert!(Arc::ptr_eq(&published, &waited));
        assert!(matches!(cache.begin(key), Lookup::Build(_)));
    }

    #[test]
    fn lru_capacity_evicts_oldest_entry() {
        let cache = SnapshotCache::<String>::default();
        for index in 0..=MAX_ENTRIES {
            let key = key("1", &format!("entry-{index}"));
            let Lookup::Build(fill) = cache.begin(key) else {
                panic!("distinct key must build");
            };
            fill.complete(index.to_string(), 1);
        }
        assert!(matches!(cache.begin(key("1", "entry-0")), Lookup::Build(_)));
        assert!(matches!(
            cache.begin(key("1", &format!("entry-{MAX_ENTRIES}"))),
            Lookup::Hit { .. }
        ));
    }

    #[test]
    fn byte_budget_evicts_the_least_recently_used_entry() {
        let cache = SnapshotCache::<String>::default();
        let first = key("1", "first");
        let second = key("1", "second");
        let third = key("1", "third");
        let estimated = (MAX_BYTES / 2).saturating_sub(1024);
        for current in [&first, &second] {
            let Lookup::Build(fill) = cache.begin(current.clone()) else {
                panic!("distinct key must build");
            };
            fill.complete(current.identity.clone(), estimated);
        }
        assert!(matches!(cache.begin(first.clone()), Lookup::Hit { .. }));
        let Lookup::Build(fill) = cache.begin(third.clone()) else {
            panic!("third key must build");
        };
        fill.complete("third".to_owned(), estimated);

        assert!(matches!(cache.begin(first), Lookup::Hit { .. }));
        assert!(matches!(cache.begin(second), Lookup::Build(_)));
        assert!(matches!(cache.begin(third), Lookup::Hit { .. }));
    }

    #[test]
    fn idle_entry_and_oversize_value_are_not_served() {
        let cache = SnapshotCache::<String>::default();
        let idle = key("1", "idle");
        let Lookup::Build(fill) = cache.begin(idle.clone()) else {
            panic!("cold lookup must build");
        };
        fill.complete("value".to_owned(), 5);
        cache
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut(&idle)
            .unwrap()
            .last_touched = Instant::now() - IDLE_TTL;
        assert!(matches!(cache.begin(idle), Lookup::Build(_)));

        let oversize = key("1", "oversize");
        let Lookup::Build(fill) = cache.begin(oversize.clone()) else {
            panic!("oversize lookup must build");
        };
        fill.complete("value".to_owned(), MAX_BYTES.saturating_add(1));
        assert!(matches!(cache.begin(oversize), Lookup::Build(_)));
    }

    #[test]
    fn same_fingerprint_new_epoch_retires_in_flight_fill() {
        let cache = SnapshotCache::<String>::default();
        let mut older = key("1", "same");
        older.generation = "same-fingerprint".to_owned();
        let mut newer = older.clone();
        newer.generation_epoch = Some(2);
        let Lookup::Build(older_fill) = cache.begin(older.clone()) else {
            panic!("older lookup must build");
        };
        let Lookup::Build(newer_fill) = cache.begin(newer.clone()) else {
            panic!("new epoch must build");
        };
        newer_fill.complete("newer".to_owned(), 5);
        older_fill.complete("older".to_owned(), 5);
        assert!(matches!(cache.begin(newer), Lookup::Hit { .. }));
        assert!(matches!(cache.begin(older), Lookup::Build(_)));
    }

    #[test]
    fn generation_advance_retires_prior_entries() {
        let cache = SnapshotCache::<String>::default();
        let first = key("1", "same");
        let Lookup::Build(fill) = cache.begin(first.clone()) else {
            panic!("first lookup must build");
        };
        fill.complete("one".to_owned(), 3);
        let second = key("2", "same");
        let Lookup::Build(fill) = cache.begin(second) else {
            panic!("new generation must build");
        };
        fill.complete("two".to_owned(), 3);
        assert!(matches!(cache.begin(first), Lookup::Build(_)));
    }

    #[test]
    fn older_in_flight_generation_cannot_retire_newer_generation() {
        let cache = SnapshotCache::<String>::default();
        let newer = key("2", "same");
        let Lookup::Build(fill) = cache.begin(newer.clone()) else {
            panic!("newer lookup must build");
        };
        fill.complete("newer".to_owned(), 5);
        let older = key("1", "same");
        let Lookup::Build(fill) = cache.begin(older) else {
            panic!("older generation may build for its retained caller");
        };
        fill.complete("older".to_owned(), 5);
        assert!(matches!(cache.begin(newer), Lookup::Hit { .. }));
    }

    #[test]
    fn fill_from_retired_generation_is_not_published() {
        let cache = SnapshotCache::<String>::default();
        let older = key("1", "same");
        let Lookup::Build(older_fill) = cache.begin(older.clone()) else {
            panic!("older lookup must build");
        };
        let newer = key("2", "same");
        let Lookup::Build(newer_fill) = cache.begin(newer.clone()) else {
            panic!("newer lookup must build");
        };
        newer_fill.complete("newer".to_owned(), 5);
        older_fill.complete("older".to_owned(), 5);

        let state = cache.state.lock().unwrap();
        assert!(!state.entries.contains_key(&older));
        assert!(state.entries.contains_key(&newer));
    }

    #[test]
    fn failed_fill_is_not_cached() {
        let cache = SnapshotCache::<String>::default();
        let key = key("1", "same");
        let Lookup::Build(fill) = cache.begin(key.clone()) else {
            panic!("first lookup must build");
        };
        drop(fill);
        assert!(matches!(cache.begin(key), Lookup::Build(_)));
    }

    #[test]
    fn pending_keys_are_bounded_and_cleanup_after_failure() {
        let cache = SnapshotCache::<String>::default();
        let mut fills = Vec::new();
        for index in 0..MAX_PENDING {
            let Lookup::Build(fill) = cache.begin(key("1", &format!("pending-{index}"))) else {
                panic!("pending slot {index} must be admitted");
            };
            fills.push(fill);
        }
        assert!(matches!(
            cache.begin(key("1", "pending-overflow")),
            Lookup::Bypass
        ));
        drop(fills);
        assert!(cache.state.lock().unwrap().pending.is_empty());
        assert!(matches!(
            cache.begin(key("1", "pending-overflow")),
            Lookup::Build(_)
        ));
    }

    #[test]
    fn completed_or_failed_scopes_do_not_accumulate_generation_metadata() {
        let cache = SnapshotCache::<String>::default();
        for index in 0..(MAX_ENTRIES * 4) {
            let mut key = key("1", &format!("identity-{index}"));
            key.retirement_scope = format!("scope-{index}");
            let Lookup::Build(fill) = cache.begin(key) else {
                panic!("distinct scope must build");
            };
            drop(fill);
        }
        let state = cache.state.lock().unwrap();
        assert!(state.active_generations.is_empty());
        assert!(state.pending.is_empty());
    }
}
