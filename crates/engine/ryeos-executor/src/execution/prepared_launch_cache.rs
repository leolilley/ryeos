//! Bounded single-flight cache for secret-free managed launch preparation.
//!
//! Entries contain only immutable launch-preparer output. The complete config
//! dependency proof is reconstructed and revalidated from the active
//! materialization before every lookup. Invocation authority (principal capabilities, effective
//! tool inventory, secret values, spend reservations, cancellation, thread
//! identity, and admitted capsules) is deliberately constructed after lookup.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::launch_preparation::PreparedRuntimeLaunch;
use crate::dispatch_error::DispatchError;

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING: usize = 128;
const IDLE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct PreparedCacheKey {
    pub(super) retirement_scope: String,
    pub(super) generation: String,
    pub(super) generation_epoch: Option<u64>,
    pub(super) identity: String,
}

impl PreparedCacheKey {
    fn estimated_bytes(&self) -> usize {
        self.retirement_scope
            .capacity()
            .saturating_add(self.generation.capacity())
            .saturating_add(self.identity.capacity())
    }
}

impl std::fmt::Debug for PreparedCacheKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedCacheKey")
            .field("retirement_scope", &self.retirement_scope)
            .field("generation", &self.generation)
            .field("generation_epoch", &self.generation_epoch)
            .field("identity", &self.identity)
            .finish()
    }
}

pub(super) struct PreparedManagedLaunchSkeleton {
    pub(super) prepared: PreparedRuntimeLaunch,
}

impl std::fmt::Debug for PreparedManagedLaunchSkeleton {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedManagedLaunchSkeleton")
            .field(
                "runtime_data_bytes",
                &serde_json::to_vec(&self.prepared.runtime_data)
                    .map(|bytes| bytes.len())
                    .unwrap_or(usize::MAX),
            )
            .field(
                "required_secret_count",
                &self.prepared.required_secrets.len(),
            )
            .field("runtime_fact_count", &self.prepared.runtime_facts.len())
            .field("binding_record_count", &self.prepared.binding_records.len())
            .field(
                "config_contributor_count",
                &self.prepared.config_contributors.len(),
            )
            .field(
                "has_financial_authority",
                &self.prepared.financial_authority.is_some(),
            )
            .finish()
    }
}

#[derive(Debug)]
struct CacheEntry {
    skeleton: Arc<PreparedManagedLaunchSkeleton>,
    estimated_bytes: usize,
    last_touched: Instant,
}

#[derive(Debug)]
enum PendingOutcome {
    Success(Arc<PreparedManagedLaunchSkeleton>),
    Failure(Arc<DispatchError>),
    Retry,
}

#[derive(Debug, Default)]
pub(super) struct PendingFill {
    result: Mutex<Option<PendingOutcome>>,
    completed: Notify,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<PreparedCacheKey, CacheEntry>,
    lru: VecDeque<PreparedCacheKey>,
    pending: HashMap<PreparedCacheKey, Arc<PendingFill>>,
    active_generations: HashMap<String, (u64, String)>,
    total_bytes: usize,
}

#[derive(Debug, Default)]
pub(super) struct PreparedLaunchCache {
    state: Mutex<CacheState>,
}

pub(super) enum Lookup {
    Hit {
        skeleton: Arc<PreparedManagedLaunchSkeleton>,
        entry_bytes: usize,
    },
    Wait {
        pending: Arc<PendingFill>,
    },
    Build(FillGuard),
    Bypass,
}

pub(super) struct FillGuard {
    cache: &'static PreparedLaunchCache,
    key: PreparedCacheKey,
    pending: Arc<PendingFill>,
    completed: bool,
}

impl FillGuard {
    pub(super) fn complete(
        mut self,
        skeleton: PreparedManagedLaunchSkeleton,
        serialized_bytes: usize,
    ) -> Arc<PreparedManagedLaunchSkeleton> {
        let skeleton = Arc::new(skeleton);
        let estimated_bytes = serialized_bytes.saturating_add(self.key.estimated_bytes());
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sweep_idle(&mut state);
        let generation_is_current = state
            .active_generations
            .get(&self.key.retirement_scope)
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
                        skeleton: skeleton.clone(),
                        estimated_bytes,
                        last_touched: Instant::now(),
                    },
                );
            }
        } else if !generation_is_current {
            emit_metric(
                CacheOutcome::Bypass,
                CacheReason::GenerationRetired,
                estimated_bytes,
                0,
            );
        } else {
            emit_metric(
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
            Some(PendingOutcome::Success(skeleton.clone()));
        state.pending.remove(&self.key);
        prune_generation_metadata(&mut state);
        drop(state);
        self.pending.completed.notify_waiters();
        self.completed = true;
        skeleton
    }

    pub(super) fn fail(mut self, error: DispatchError) -> Arc<DispatchError> {
        let error = Arc::new(error);
        self.finish_pending(PendingOutcome::Failure(error.clone()));
        self.completed = true;
        emit_metric(CacheOutcome::Miss, CacheReason::FillFailed, 0, 0);
        error
    }

    pub(super) fn cancel(mut self) {
        self.finish_pending(PendingOutcome::Retry);
        self.completed = true;
    }

    fn finish_pending(&self, outcome: PendingOutcome) {
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *self
            .pending
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
        if state
            .pending
            .get(&self.key)
            .is_some_and(|pending| Arc::ptr_eq(pending, &self.pending))
        {
            state.pending.remove(&self.key);
        }
        prune_generation_metadata(&mut state);
        drop(state);
        self.pending.completed.notify_waiters();
    }
}

impl Drop for FillGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.finish_pending(PendingOutcome::Failure(Arc::new(DispatchError::Internal(
            anyhow::anyhow!("prepared launch cache fill ended without publishing its result"),
        ))));
        emit_metric(CacheOutcome::Miss, CacheReason::FillFailed, 0, 0);
    }
}

impl PreparedLaunchCache {
    pub(super) fn begin(&'static self, key: PreparedCacheKey) -> Lookup {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sweep_idle(&mut state);
        prune_generation_metadata(&mut state);
        retire_previous_generation(&mut state, &key);
        if let Some((skeleton, entry_bytes)) = state.entries.get_mut(&key).map(|entry| {
            entry.last_touched = Instant::now();
            (entry.skeleton.clone(), entry.estimated_bytes)
        }) {
            touch_lru(&mut state.lru, &key);
            return Lookup::Hit {
                skeleton,
                entry_bytes,
            };
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

    pub(super) fn discard_if_same(
        &self,
        key: &PreparedCacheKey,
        skeleton: &Arc<PreparedManagedLaunchSkeleton>,
        reason: CacheReason,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let observed = state
            .entries
            .get(key)
            .is_some_and(|entry| Arc::ptr_eq(&entry.skeleton, skeleton));
        let entry_bytes = if observed {
            remove_entry(&mut state, key)
        } else {
            0
        };
        drop(state);
        emit_metric(CacheOutcome::Eviction, reason, entry_bytes, 0);
    }
}

impl PendingFill {
    pub(super) async fn wait(
        &self,
    ) -> Result<Option<Arc<PreparedManagedLaunchSkeleton>>, Arc<DispatchError>> {
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
                    PendingOutcome::Success(skeleton) => Ok(Some(skeleton.clone())),
                    PendingOutcome::Failure(error) => Err(error.clone()),
                    PendingOutcome::Retry => Ok(None),
                };
            }
            notified.as_mut().await;
        }
    }
}

pub(super) fn cache() -> &'static PreparedLaunchCache {
    static CACHE: OnceLock<PreparedLaunchCache> = OnceLock::new();
    CACHE.get_or_init(PreparedLaunchCache::default)
}

#[derive(Debug, Clone, Copy)]
pub(super) enum CacheOutcome {
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
pub(super) enum CacheReason {
    Ready,
    SingleFlight,
    Cold,
    PendingCapacity,
    AuthorityRevalidationFailed,
    FillFailed,
    EntryTooLarge,
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
            Self::PendingCapacity => "pending_capacity",
            Self::AuthorityRevalidationFailed => "authority_revalidation_failed",
            Self::FillFailed => "fill_failed",
            Self::EntryTooLarge => "entry_too_large",
            Self::IdleTtl => "idle_ttl",
            Self::Capacity => "capacity",
            Self::GenerationRetired => "generation_retired",
        }
    }
}

pub(super) fn emit_metric(
    outcome: CacheOutcome,
    reason: CacheReason,
    entry_bytes: usize,
    wait_milliseconds: u64,
) {
    tracing::info!(
        target: "ryeos.metrics",
        metric = "prepared_managed_launch_skeleton_cache",
        outcome = outcome.as_str(),
        reason = reason.as_str(),
        entry_bytes,
        wait_milliseconds,
        "prepared managed launch skeleton cache metric"
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
        emit_metric(CacheOutcome::Eviction, CacheReason::IdleTtl, entry_bytes, 0);
    }
}

fn retire_previous_generation(state: &mut CacheState, key: &PreparedCacheKey) {
    let Some(epoch) = key.generation_epoch else {
        return;
    };
    if let Some((active_epoch, active_generation)) =
        state.active_generations.get(&key.retirement_scope)
    {
        if *active_epoch > epoch || (*active_epoch == epoch && active_generation == &key.generation)
        {
            return;
        }
    }
    state.active_generations.insert(
        key.retirement_scope.clone(),
        (epoch, key.generation.clone()),
    );
    let stale = state
        .entries
        .keys()
        .filter(|candidate| {
            candidate.retirement_scope == key.retirement_scope
                && (candidate.generation_epoch != Some(epoch)
                    || candidate.generation != key.generation)
        })
        .cloned()
        .collect::<Vec<_>>();
    for stale_key in stale {
        let entry_bytes = remove_entry(state, &stale_key);
        emit_metric(
            CacheOutcome::Eviction,
            CacheReason::GenerationRetired,
            entry_bytes,
            0,
        );
    }
}

fn prune_generation_metadata(state: &mut CacheState) {
    let live_scopes = state
        .entries
        .keys()
        .chain(state.pending.keys())
        .map(|key| key.retirement_scope.clone())
        .collect::<HashSet<_>>();
    state
        .active_generations
        .retain(|scope, _| live_scopes.contains(scope));
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
            emit_metric(
                CacheOutcome::Eviction,
                CacheReason::Capacity,
                entry.estimated_bytes,
                0,
            );
        }
    }
}

fn remove_entry(state: &mut CacheState, key: &PreparedCacheKey) -> usize {
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

fn touch_lru(lru: &mut VecDeque<PreparedCacheKey>, key: &PreparedCacheKey) {
    if let Some(position) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(position);
    }
    lru.push_back(key.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(generation: u64, identity: &str) -> PreparedCacheKey {
        PreparedCacheKey {
            retirement_scope: "test".to_owned(),
            generation: generation.to_string(),
            generation_epoch: Some(generation),
            identity: identity.to_owned(),
        }
    }

    fn skeleton() -> PreparedManagedLaunchSkeleton {
        PreparedManagedLaunchSkeleton {
            prepared: PreparedRuntimeLaunch {
                runtime_data: Default::default(),
                required_secrets: Vec::new(),
                runtime_facts: Default::default(),
                binding_records: Default::default(),
                config_contributors: Vec::new(),
                financial_authority: None,
            },
        }
    }

    #[test]
    fn completed_fill_is_a_ready_hit() {
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        let key = key(1, "ready");
        let Lookup::Build(fill) = cache.begin(key.clone()) else {
            panic!("cold lookup must build");
        };
        fill.complete(skeleton(), 1);
        assert!(matches!(cache.begin(key), Lookup::Hit { .. }));
    }

    #[tokio::test]
    async fn concurrent_lookup_waits_for_single_flight_result() {
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        let key = key(1, "single-flight");
        let Lookup::Build(fill) = cache.begin(key.clone()) else {
            panic!("leader must build");
        };
        let Lookup::Wait { pending } = cache.begin(key) else {
            panic!("concurrent lookup must wait");
        };
        fill.complete(skeleton(), 1);
        assert!(pending.wait().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn oversize_leader_result_is_shared_with_waiters_but_not_cached() {
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        let key = key(1, "oversize-single-flight");
        let Lookup::Build(fill) = cache.begin(key.clone()) else {
            panic!("leader must build");
        };
        let Lookup::Wait { pending } = cache.begin(key.clone()) else {
            panic!("concurrent lookup must wait");
        };
        let published = fill.complete(skeleton(), MAX_BYTES.saturating_add(1));
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
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        let key = key(1, "failure");
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
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        for index in 0..=MAX_ENTRIES {
            let key = key(1, &format!("entry-{index}"));
            let Lookup::Build(fill) = cache.begin(key) else {
                panic!("distinct key must build");
            };
            fill.complete(skeleton(), 1);
        }
        assert!(matches!(cache.begin(key(1, "entry-0")), Lookup::Build(_)));
        assert!(matches!(
            cache.begin(key(1, &format!("entry-{MAX_ENTRIES}"))),
            Lookup::Hit { .. }
        ));
    }

    #[test]
    fn byte_budget_evicts_the_least_recently_used_entry() {
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        let first = key(1, "first");
        let second = key(1, "second");
        let third = key(1, "third");
        let estimated = (MAX_BYTES / 2).saturating_sub(1024);
        for current in [&first, &second] {
            let Lookup::Build(fill) = cache.begin(current.clone()) else {
                panic!("distinct key must build");
            };
            fill.complete(skeleton(), estimated);
        }
        assert!(matches!(cache.begin(first.clone()), Lookup::Hit { .. }));
        let Lookup::Build(fill) = cache.begin(third.clone()) else {
            panic!("third key must build");
        };
        fill.complete(skeleton(), estimated);

        assert!(matches!(cache.begin(first), Lookup::Hit { .. }));
        assert!(matches!(cache.begin(second), Lookup::Build(_)));
        assert!(matches!(cache.begin(third), Lookup::Hit { .. }));
    }

    #[test]
    fn idle_entry_and_oversize_value_are_not_served() {
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        let idle = key(1, "idle");
        let Lookup::Build(fill) = cache.begin(idle.clone()) else {
            panic!("cold lookup must build");
        };
        fill.complete(skeleton(), 1);
        cache
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut(&idle)
            .unwrap()
            .last_touched = Instant::now() - IDLE_TTL;
        assert!(matches!(cache.begin(idle), Lookup::Build(_)));

        let oversize = key(1, "oversize");
        let Lookup::Build(fill) = cache.begin(oversize.clone()) else {
            panic!("oversize lookup must build");
        };
        fill.complete(skeleton(), MAX_BYTES.saturating_add(1));
        assert!(matches!(cache.begin(oversize), Lookup::Build(_)));
    }

    #[test]
    fn same_fingerprint_new_epoch_retires_in_flight_fill() {
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        let mut older = key(1, "same");
        older.generation = "same-fingerprint".to_owned();
        let mut newer = older.clone();
        newer.generation_epoch = Some(2);
        let Lookup::Build(older_fill) = cache.begin(older.clone()) else {
            panic!("older lookup must build");
        };
        let Lookup::Build(newer_fill) = cache.begin(newer.clone()) else {
            panic!("new epoch must build");
        };
        newer_fill.complete(skeleton(), 1);
        older_fill.complete(skeleton(), 1);
        assert!(matches!(cache.begin(newer), Lookup::Hit { .. }));
        assert!(matches!(cache.begin(older), Lookup::Build(_)));
    }

    #[test]
    fn failed_fill_is_not_cached() {
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        let key = key(1, "key");
        let Lookup::Build(fill) = cache.begin(key.clone()) else {
            panic!("first lookup must build");
        };
        drop(fill);
        assert!(matches!(cache.begin(key), Lookup::Build(_)));
    }

    #[test]
    fn pending_keys_are_bounded() {
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        let mut fills = Vec::new();
        for index in 0..MAX_PENDING {
            let Lookup::Build(fill) = cache.begin(key(1, &format!("key-{index}"))) else {
                panic!("pending slot {index} must be admitted");
            };
            fills.push(fill);
        }
        assert!(matches!(cache.begin(key(1, "overflow")), Lookup::Bypass));
        drop(fills);
        assert!(cache.state.lock().unwrap().active_generations.is_empty());
    }

    #[test]
    fn generation_advance_retires_prior_entries() {
        let cache = Box::leak(Box::new(PreparedLaunchCache::default()));
        let first = key(1, "same");
        let Lookup::Build(fill) = cache.begin(first.clone()) else {
            panic!("first lookup must build");
        };
        fill.complete(
            PreparedManagedLaunchSkeleton {
                prepared: PreparedRuntimeLaunch {
                    runtime_data: Default::default(),
                    required_secrets: Vec::new(),
                    runtime_facts: Default::default(),
                    binding_records: Default::default(),
                    config_contributors: Vec::new(),
                    financial_authority: None,
                },
            },
            1,
        );
        let second = key(2, "same");
        let Lookup::Build(new_fill) = cache.begin(second.clone()) else {
            panic!("new generation must build");
        };
        let Lookup::Build(old_fill) = cache.begin(first.clone()) else {
            panic!("retained old caller may build without publishing");
        };
        old_fill.complete(
            PreparedManagedLaunchSkeleton {
                prepared: PreparedRuntimeLaunch {
                    runtime_data: Default::default(),
                    required_secrets: Vec::new(),
                    runtime_facts: Default::default(),
                    binding_records: Default::default(),
                    config_contributors: Vec::new(),
                    financial_authority: None,
                },
            },
            1,
        );
        new_fill.complete(
            PreparedManagedLaunchSkeleton {
                prepared: PreparedRuntimeLaunch {
                    runtime_data: Default::default(),
                    required_secrets: Vec::new(),
                    runtime_facts: Default::default(),
                    binding_records: Default::default(),
                    config_contributors: Vec::new(),
                    financial_authority: None,
                },
            },
            1,
        );
        assert!(matches!(cache.begin(second), Lookup::Hit { .. }));
        assert!(matches!(cache.begin(first), Lookup::Build(_)));
    }

    #[test]
    fn debug_output_redacts_prepared_runtime_values_and_secret_names() {
        let mut runtime_data = std::collections::BTreeMap::new();
        runtime_data.insert(
            "opaque".to_string(),
            serde_json::Value::String("do-not-log-runtime-value".to_string()),
        );
        let skeleton = PreparedManagedLaunchSkeleton {
            prepared: PreparedRuntimeLaunch {
                runtime_data,
                required_secrets: vec![super::super::launch_preparation::PreparedSecret {
                    name: "DO_NOT_LOG_SECRET_NAME".to_string(),
                    origin: ryeos_handler_protocol::LaunchSecretOriginWire::Binding {
                        name: "binding".to_string(),
                    },
                }],
                runtime_facts: Default::default(),
                binding_records: Default::default(),
                config_contributors: Vec::new(),
                financial_authority: None,
            },
        };
        let debug = format!("{skeleton:?}");
        assert!(!debug.contains("do-not-log-runtime-value"));
        assert!(!debug.contains("DO_NOT_LOG_SECRET_NAME"));
    }
}
