//! Request-local launch timing collection.
//!
//! This is deliberately process-local observability state. It is neither
//! persisted execution authority nor part of a launch envelope, and its
//! monotonic offsets must never be subtracted from timestamps emitted by a
//! runtime child or client process.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Instant;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LaunchStageInterval {
    pub stage: &'static str,
    pub start_us: u64,
    pub end_us: u64,
    pub elapsed_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LaunchStageTimingSnapshot {
    pub schema_version: u32,
    pub clock_domain: &'static str,
    pub request_trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_ref_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_class: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub augmentation_child_thread_ids: Vec<String>,
    pub total_us: u64,
    pub accounted_union_us: u64,
    pub critical_path_us: u64,
    pub unattributed_us: u64,
    pub top_level: Vec<LaunchStageInterval>,
    pub nested: Vec<LaunchStageInterval>,
    pub milestones_us: BTreeMap<&'static str, u64>,
}

#[derive(Default)]
struct LaunchStageTimingState {
    thread_id: Option<String>,
    item_ref_kind: Option<String>,
    launch_class: Option<String>,
    augmentation_child_thread_ids: Vec<String>,
    top_level: Vec<LaunchStageInterval>,
    nested: Vec<LaunchStageInterval>,
    milestones_us: BTreeMap<&'static str, u64>,
}

struct RegisteredLaunchTiming {
    request_trace_id: Weak<str>,
    started_at: Instant,
    state: Weak<Mutex<LaunchStageTimingState>>,
    sequence: u64,
}

const MAX_REGISTERED_THREAD_TIMINGS: usize = 4096;
static REGISTERED_THREAD_TIMINGS: LazyLock<Mutex<HashMap<String, RegisteredLaunchTiming>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static REGISTERED_THREAD_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Shared timing state for one daemon-side launch request.
///
/// The mutex is held only while appending a small in-memory record or taking a
/// snapshot. No guard crosses an async suspension point.
#[derive(Clone)]
pub struct LaunchStageTimings {
    request_trace_id: Arc<str>,
    started_at: Instant,
    state: Arc<Mutex<LaunchStageTimingState>>,
}

impl std::fmt::Debug for LaunchStageTimings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaunchStageTimings")
            .field("request_trace_id", &self.request_trace_id)
            .finish_non_exhaustive()
    }
}

impl LaunchStageTimings {
    pub fn new_request() -> Self {
        Self::new(uuid::Uuid::new_v4().simple().to_string(), Instant::now())
    }

    pub fn new(request_trace_id: impl Into<Arc<str>>, started_at: Instant) -> Self {
        Self {
            request_trace_id: request_trace_id.into(),
            started_at,
            state: Arc::new(Mutex::new(LaunchStageTimingState::default())),
        }
    }

    pub fn request_trace_id(&self) -> &str {
        &self.request_trace_id
    }

    pub fn elapsed_us(&self) -> u64 {
        duration_us(self.started_at.elapsed())
    }

    pub fn top_level(&self, stage: &'static str) -> LaunchStageTimer {
        LaunchStageTimer::new(self.clone(), stage, None)
    }

    pub fn nested(&self, parent: &'static str, stage: &'static str) -> LaunchStageTimer {
        LaunchStageTimer::new(self.clone(), stage, Some(parent))
    }

    pub fn mark(&self, milestone: &'static str) {
        self.mark_once(milestone);
    }

    pub fn mark_once(&self, milestone: &'static str) -> bool {
        let elapsed_us = self.elapsed_us();
        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            tracing::warn!(
                request_trace_id = %self.request_trace_id,
                "launch timing mutex was poisoned; recovering"
            );
            poisoned.into_inner()
        });
        if state.milestones_us.contains_key(milestone) {
            return false;
        }
        state.milestones_us.insert(milestone, elapsed_us);
        true
    }

    pub fn record_top_level_since_start(&self, stage: &'static str) {
        self.record(stage, None, 0, self.elapsed_us());
    }

    pub fn record_top_level_from_milestone(
        &self,
        stage: &'static str,
        start_milestone: &'static str,
    ) -> bool {
        self.record_from_milestone(stage, None, start_milestone)
    }

    pub fn record_nested_from_milestone(
        &self,
        parent: &'static str,
        stage: &'static str,
        start_milestone: &'static str,
    ) -> bool {
        self.record_from_milestone(stage, Some(parent), start_milestone)
    }

    pub fn set_launch_dimensions(&self, item_ref_kind: &str, launch_class: &str) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            tracing::warn!(
                request_trace_id = %self.request_trace_id,
                "launch timing mutex was poisoned; recovering"
            );
            poisoned.into_inner()
        });
        state
            .item_ref_kind
            .get_or_insert_with(|| item_ref_kind.to_owned());
        state
            .launch_class
            .get_or_insert_with(|| launch_class.to_owned());
    }

    /// Record an augmentation worker identity so daemon and child timing
    /// records can be joined without comparing timestamps across processes.
    pub fn record_augmentation_child_thread_id(&self, thread_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            tracing::warn!(
                request_trace_id = %self.request_trace_id,
                "launch timing mutex was poisoned; recovering"
            );
            poisoned.into_inner()
        });
        if !state
            .augmentation_child_thread_ids
            .iter()
            .any(|existing| existing == thread_id)
        {
            state
                .augmentation_child_thread_ids
                .push(thread_id.to_owned());
        }
    }

    /// Bind the request trace to the launch's internal thread identity.
    ///
    /// This mapping is daemon observability only. It does not expose the
    /// pre-minted identity to an HTTP caller or make it cancellable.
    pub fn bind_thread_id(&self, thread_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            tracing::warn!(
                request_trace_id = %self.request_trace_id,
                "launch timing mutex was poisoned; recovering"
            );
            poisoned.into_inner()
        });
        match state.thread_id.as_deref() {
            Some(existing) if existing != thread_id => {
                tracing::error!(
                    request_trace_id = %self.request_trace_id,
                    existing_thread_id = %existing,
                    attempted_thread_id = %thread_id,
                    "refusing to remap launch timing trace to a different thread"
                );
            }
            Some(_) => {}
            None => {
                state.thread_id = Some(thread_id.to_owned());
                tracing::info!(
                    event = "launch_trace_thread_mapped",
                    request_trace_id = %self.request_trace_id,
                    thread_id = %thread_id,
                    "launch request trace mapped to internal thread identity"
                );
            }
        }
        drop(state);
        register_thread_timing(thread_id, self);
    }

    pub fn snapshot(&self) -> LaunchStageTimingSnapshot {
        let total_us = self.elapsed_us();
        let state = self.state.lock().unwrap_or_else(|poisoned| {
            tracing::warn!(
                request_trace_id = %self.request_trace_id,
                "launch timing mutex was poisoned; recovering"
            );
            poisoned.into_inner()
        });
        let accounted_union_us = interval_union_us(&state.top_level);
        LaunchStageTimingSnapshot {
            schema_version: 2,
            clock_domain: "daemon_monotonic",
            request_trace_id: self.request_trace_id.to_string(),
            thread_id: state.thread_id.clone(),
            item_ref_kind: state.item_ref_kind.clone(),
            launch_class: state.launch_class.clone(),
            augmentation_child_thread_ids: state.augmentation_child_thread_ids.clone(),
            total_us,
            accounted_union_us,
            // Top-level intervals describe the daemon critical path. Nested
            // intervals are diagnostic and intentionally excluded.
            critical_path_us: accounted_union_us,
            unattributed_us: total_us.saturating_sub(accounted_union_us),
            top_level: state.top_level.clone(),
            nested: state.nested.clone(),
            milestones_us: state.milestones_us.clone(),
        }
    }

    pub fn emit(&self, observation: &'static str) {
        let snapshot = self.snapshot();
        match serde_json::to_string(&snapshot) {
            Ok(timings) => tracing::info!(
                event = "launch_stage_timings",
                schema_version = snapshot.schema_version,
                clock_domain = snapshot.clock_domain,
                observation,
                request_trace_id = %self.request_trace_id,
                thread_id = snapshot.thread_id.as_deref(),
                item_ref_kind = snapshot.item_ref_kind.as_deref(),
                launch_class = snapshot.launch_class.as_deref(),
                total_us = snapshot.total_us,
                accounted_union_us = snapshot.accounted_union_us,
                critical_path_us = snapshot.critical_path_us,
                unattributed_us = snapshot.unattributed_us,
                timings_json = %timings,
                "launch stage timing snapshot"
            ),
            Err(error) => tracing::warn!(
                request_trace_id = %self.request_trace_id,
                %error,
                "failed to encode launch stage timing snapshot"
            ),
        }
    }

    fn record(
        &self,
        stage: &'static str,
        parent: Option<&'static str>,
        start_us: u64,
        end_us: u64,
    ) {
        let interval = LaunchStageInterval {
            stage,
            start_us,
            end_us,
            elapsed_us: end_us.saturating_sub(start_us),
            parent,
        };
        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            tracing::warn!(
                request_trace_id = %self.request_trace_id,
                "launch timing mutex was poisoned; recovering"
            );
            poisoned.into_inner()
        });
        if parent.is_some() {
            state.nested.push(interval);
        } else {
            state.top_level.push(interval);
        }
    }

    fn record_from_milestone(
        &self,
        stage: &'static str,
        parent: Option<&'static str>,
        start_milestone: &'static str,
    ) -> bool {
        let end_us = self.elapsed_us();
        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            tracing::warn!(
                request_trace_id = %self.request_trace_id,
                "launch timing mutex was poisoned; recovering"
            );
            poisoned.into_inner()
        });
        let Some(start_us) = state.milestones_us.get(start_milestone).copied() else {
            return false;
        };
        let destination = if parent.is_some() {
            &mut state.nested
        } else {
            &mut state.top_level
        };
        if destination.iter().any(|interval| interval.stage == stage) {
            return false;
        }
        destination.push(LaunchStageInterval {
            stage,
            start_us,
            end_us,
            elapsed_us: end_us.saturating_sub(start_us),
            parent,
        });
        true
    }
}

/// Timestamp the first runtime-originated callback in the daemon clock domain.
///
/// UDS transport invokes this after decoding a request with a thread id.
/// Authentication remains authoritative in routing; this marker is advisory
/// observability only. Child-process monotonic offsets remain separate.
pub fn observe_child_callback(thread_id: &str) {
    let registered = {
        let registry = REGISTERED_THREAD_TIMINGS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.get(thread_id).map(|registered| {
            (
                registered.request_trace_id.clone(),
                registered.started_at,
                registered.state.clone(),
                registered.sequence,
            )
        })
    };
    let Some((request_trace_id, started_at, state, sequence)) = registered else {
        return;
    };
    let timing =
        request_trace_id
            .upgrade()
            .zip(state.upgrade())
            .map(|(request_trace_id, state)| LaunchStageTimings {
                request_trace_id,
                started_at,
                state,
            });
    let recorded = timing
        .as_ref()
        .is_some_and(|timing| timing.mark_once("first_child_callback_received"));

    let mut registry = REGISTERED_THREAD_TIMINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let still_exact_entry = registry
        .get(thread_id)
        .is_some_and(|registered| registered.sequence == sequence);
    if still_exact_entry {
        registry.remove(thread_id);
    }
    drop(registry);

    if recorded {
        if let Some(timing) = timing {
            timing.emit("first_child_callback_received");
        }
    }
}

fn register_thread_timing(thread_id: &str, timings: &LaunchStageTimings) {
    let mut registry = REGISTERED_THREAD_TIMINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.retain(|_, registered| registered.state.strong_count() > 0);
    registry.insert(
        thread_id.to_owned(),
        RegisteredLaunchTiming {
            request_trace_id: Arc::downgrade(&timings.request_trace_id),
            started_at: timings.started_at,
            state: Arc::downgrade(&timings.state),
            sequence: REGISTERED_THREAD_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        },
    );
    while registry.len() > MAX_REGISTERED_THREAD_TIMINGS {
        let Some(oldest) = registry
            .iter()
            .min_by_key(|(_, registered)| registered.sequence)
            .map(|(thread_id, _)| thread_id.clone())
        else {
            break;
        };
        registry.remove(&oldest);
    }
}

/// An async-safe monotonic interval guard.
///
/// Unlike a `tracing::Span::enter` guard, this value may be held across an
/// `.await`: it contains no thread-local subscriber state.
#[must_use = "dropping the timer records the interval; bind it to keep the intended boundary"]
pub struct LaunchStageTimer {
    timings: LaunchStageTimings,
    stage: &'static str,
    parent: Option<&'static str>,
    start_us: u64,
    finished: bool,
}

impl LaunchStageTimer {
    fn new(timings: LaunchStageTimings, stage: &'static str, parent: Option<&'static str>) -> Self {
        let start_us = timings.elapsed_us();
        Self {
            timings,
            stage,
            parent,
            start_us,
            finished: false,
        }
    }

    pub fn finish(mut self) -> u64 {
        let end_us = self.timings.elapsed_us();
        self.timings
            .record(self.stage, self.parent, self.start_us, end_us);
        self.finished = true;
        end_us.saturating_sub(self.start_us)
    }
}

impl Drop for LaunchStageTimer {
    fn drop(&mut self) {
        if !self.finished {
            let end_us = self.timings.elapsed_us();
            self.timings
                .record(self.stage, self.parent, self.start_us, end_us);
            self.finished = true;
        }
    }
}

fn duration_us(duration: std::time::Duration) -> u64 {
    duration.as_micros().try_into().unwrap_or(u64::MAX)
}

fn interval_union_us(intervals: &[LaunchStageInterval]) -> u64 {
    let mut ranges: Vec<(u64, u64)> = intervals
        .iter()
        .map(|interval| (interval.start_us, interval.end_us))
        .collect();
    ranges.sort_unstable_by_key(|range| (range.0, range.1));

    let mut total = 0u64;
    let mut current: Option<(u64, u64)> = None;
    for (start, end) in ranges {
        current = match current {
            None => Some((start, end)),
            Some((current_start, current_end)) if start <= current_end => {
                Some((current_start, current_end.max(end)))
            }
            Some((current_start, current_end)) => {
                total = total.saturating_add(current_end.saturating_sub(current_start));
                Some((start, end))
            }
        };
    }
    if let Some((start, end)) = current {
        total = total.saturating_add(end.saturating_sub(start));
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_union_does_not_double_count_overlap() {
        let intervals = vec![
            LaunchStageInterval {
                stage: "one",
                start_us: 10,
                end_us: 30,
                elapsed_us: 20,
                parent: None,
            },
            LaunchStageInterval {
                stage: "two",
                start_us: 20,
                end_us: 40,
                elapsed_us: 20,
                parent: None,
            },
            LaunchStageInterval {
                stage: "three",
                start_us: 50,
                end_us: 60,
                elapsed_us: 10,
                parent: None,
            },
        ];
        assert_eq!(interval_union_us(&intervals), 40);
    }

    #[test]
    fn snapshot_carries_distinct_augmentation_child_thread_ids() {
        let timings = LaunchStageTimings::new_request();
        timings.record_augmentation_child_thread_id("T-child-a");
        timings.record_augmentation_child_thread_id("T-child-a");
        timings.record_augmentation_child_thread_id("T-child-b");

        let snapshot = timings.snapshot();
        assert_eq!(snapshot.schema_version, 2);
        assert_eq!(
            snapshot.augmentation_child_thread_ids,
            vec!["T-child-a".to_string(), "T-child-b".to_string()]
        );
    }

    #[test]
    fn launch_dimensions_are_first_write_wins() {
        let timings = LaunchStageTimings::new_request();
        timings.set_launch_dimensions("directive", "gateway_stream");
        timings.set_launch_dimensions("runtime", "managed_runtime");

        let snapshot = timings.snapshot();
        assert_eq!(snapshot.item_ref_kind.as_deref(), Some("directive"));
        assert_eq!(snapshot.launch_class.as_deref(), Some("gateway_stream"));
    }

    #[test]
    #[ignore = "operator-run Phase 0 instrumentation overhead gate"]
    fn launch_stage_timing_overhead_p50_stays_below_five_milliseconds() {
        const SAMPLES: usize = 1_001;
        let mut baseline_us = Vec::with_capacity(SAMPLES);
        let mut instrumented_us = Vec::with_capacity(SAMPLES);

        for sample in 0..SAMPLES {
            let started = Instant::now();
            std::hint::black_box(sample);
            baseline_us.push(duration_us(started.elapsed()));

            let started = Instant::now();
            let timings =
                LaunchStageTimings::new(format!("overhead-sample-{sample}"), Instant::now());
            timings.set_launch_dimensions("directive", "overhead_gate");
            timings.bind_thread_id(&format!("T-overhead-{sample}"));
            for stage in [
                "project_context_resolution",
                "preflight_admission",
                "background_dispatch",
                "spawn_scheduled_to_handoff",
                "handoff_to_stream_started_yield",
                "runtime_spawn_worker",
            ] {
                drop(timings.top_level(stage));
            }
            for stage in [
                "preflight_blocking_queue_wait",
                "preflight_blocking_work",
                "root_admission_reverify",
                "root_admission_resolution_compose",
                "ref_binding_resolution",
                "launch_augmentation",
                "executor_manifest_verify",
                "executor_blob_fetch_hash",
                "executor_materialize_verify",
                "runtime_preparation",
                "runtime_prep_to_row_publication",
            ] {
                drop(timings.nested("overhead_gate", stage));
            }
            timings.mark("http_response_constructed");
            let snapshot = timings.snapshot();
            std::hint::black_box(
                serde_json::to_string(&snapshot).expect("serialize overhead timing snapshot"),
            );
            instrumented_us.push(duration_us(started.elapsed()));
        }

        baseline_us.sort_unstable();
        instrumented_us.sort_unstable();
        let median = SAMPLES / 2;
        let overhead_p50_us = instrumented_us[median].saturating_sub(baseline_us[median]);
        assert!(
            overhead_p50_us < 5_000,
            "launch timing instrumentation p50 overhead was {overhead_p50_us}us"
        );
    }

    #[test]
    fn child_callback_observation_removes_only_the_exact_recorded_entry() {
        let observed_thread_id = format!("T-observed-{}", uuid::Uuid::new_v4().simple());
        let other_thread_id = format!("T-other-{}", uuid::Uuid::new_v4().simple());
        let stale_thread_id = format!("T-stale-{}", uuid::Uuid::new_v4().simple());
        let observed = LaunchStageTimings::new_request();
        let other = LaunchStageTimings::new_request();
        observed.bind_thread_id(&observed_thread_id);
        other.bind_thread_id(&other_thread_id);
        {
            let stale = LaunchStageTimings::new_request();
            stale.bind_thread_id(&stale_thread_id);
        }

        observe_child_callback(&observed_thread_id);

        assert!(observed
            .snapshot()
            .milestones_us
            .contains_key("first_child_callback_received"));
        let mut registry = REGISTERED_THREAD_TIMINGS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(!registry.contains_key(&observed_thread_id));
        assert!(registry.contains_key(&other_thread_id));
        assert!(
            registry.contains_key(&stale_thread_id),
            "callback observation must not perform a whole-registry stale sweep"
        );
        registry.remove(&other_thread_id);
        registry.remove(&stale_thread_id);
    }
}
