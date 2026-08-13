//! Bounded cache telemetry for hot execution paths.
//!
//! Per-call events remain available at `debug`. This module adds exact,
//! low-frequency `info` aggregates so the default metrics filter preserves
//! hit/miss/revalidation visibility without amplifying one log per lookup.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SHARDS: usize = 16;
const REPORT_INTERVAL: Duration = Duration::from_secs(5);
const MAX_KEYS_PER_SHARD: usize = 64;

#[derive(Debug, Clone, Copy)]
pub struct CacheMetricSample {
    pub metric: &'static str,
    pub namespace: Option<&'static str>,
    pub outcome: &'static str,
    pub reason: Option<&'static str>,
    pub source_bytes: usize,
    pub entry_bytes: usize,
    pub wait_microseconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MetricKey {
    metric: &'static str,
    namespace: Option<&'static str>,
    outcome: &'static str,
    reason: Option<&'static str>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Aggregate {
    count: u64,
    source_bytes: u64,
    entry_bytes: u64,
    max_entry_bytes: u64,
    max_wait_microseconds: u64,
}

#[derive(Debug)]
struct Shard {
    last_report: Instant,
    rows: HashMap<MetricKey, Aggregate>,
    dropped_samples: u64,
}

fn shards() -> &'static [Mutex<Shard>; SHARDS] {
    static SHARDS_CELL: OnceLock<[Mutex<Shard>; SHARDS]> = OnceLock::new();
    SHARDS_CELL.get_or_init(|| {
        std::array::from_fn(|_| {
            Mutex::new(Shard {
                last_report: Instant::now(),
                rows: HashMap::new(),
                dropped_samples: 0,
            })
        })
    })
}

fn stable_shard(key: &MetricKey) -> usize {
    let mut hash = 0xcbf29ce484222325u64;
    for value in [
        Some(key.metric),
        key.namespace,
        Some(key.outcome),
        key.reason,
    ] {
        for byte in value.unwrap_or("").bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
    }
    usize::try_from(hash % SHARDS as u64).expect("shard index fits usize")
}

fn saturating_usize(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

pub fn record_cache_metric(sample: CacheMetricSample) {
    let key = MetricKey {
        metric: sample.metric,
        namespace: sample.namespace,
        outcome: sample.outcome,
        reason: sample.reason,
    };
    let now = Instant::now();
    let mut shard = shards()[stable_shard(&key)]
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // All dimensions are platform-owned static strings. Keep a hard ceiling
    // anyway so future call sites cannot turn telemetry into unbounded state.
    if shard.rows.contains_key(&key) || shard.rows.len() < MAX_KEYS_PER_SHARD {
        let row = shard.rows.entry(key).or_default();
        row.count = row.count.saturating_add(1);
        row.source_bytes = row
            .source_bytes
            .saturating_add(saturating_usize(sample.source_bytes));
        row.entry_bytes = row
            .entry_bytes
            .saturating_add(saturating_usize(sample.entry_bytes));
        row.max_entry_bytes = row
            .max_entry_bytes
            .max(saturating_usize(sample.entry_bytes));
        row.max_wait_microseconds = row.max_wait_microseconds.max(sample.wait_microseconds);
    } else {
        shard.dropped_samples = shard.dropped_samples.saturating_add(1);
    }
    let Some((rows, dropped_samples)) = drain_shard(&mut shard, now, false) else {
        return;
    };
    drop(shard);

    emit_aggregate(rows, dropped_samples);
}

fn drain_shard(
    shard: &mut Shard,
    now: Instant,
    force: bool,
) -> Option<(HashMap<MetricKey, Aggregate>, u64)> {
    if !force && now.duration_since(shard.last_report) < REPORT_INTERVAL {
        return None;
    }
    shard.last_report = now;
    let rows = std::mem::take(&mut shard.rows);
    let dropped_samples = std::mem::take(&mut shard.dropped_samples);
    if rows.is_empty() && dropped_samples == 0 {
        None
    } else {
        Some((rows, dropped_samples))
    }
}

fn emit_aggregate(rows: HashMap<MetricKey, Aggregate>, dropped_samples: u64) {
    for (key, row) in rows {
        tracing::info!(
            target: "ryeos.metrics",
            metric = key.metric,
            namespace = key.namespace,
            outcome = key.outcome,
            reason = key.reason,
            count = row.count,
            source_bytes = row.source_bytes,
            entry_bytes = row.entry_bytes,
            max_entry_bytes = row.max_entry_bytes,
            max_wait_microseconds = row.max_wait_microseconds,
            "cache metric aggregate"
        );
    }
    if dropped_samples != 0 {
        tracing::info!(
            target: "ryeos.metrics",
            metric = "cache_metric_aggregate_overflow",
            outcome = "dropped",
            count = dropped_samples,
            max_keys_per_shard = MAX_KEYS_PER_SHARD,
            "cache metric aggregate overflow"
        );
    }
}

fn flush_cache_metrics_inner(force: bool) {
    let now = Instant::now();
    for shard in shards() {
        let drained = {
            let mut shard = shard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            drain_shard(&mut shard, now, force)
        };
        if let Some((rows, dropped_samples)) = drained {
            emit_aggregate(rows, dropped_samples);
        }
    }
}

/// Flush every aggregate whose reporting interval has elapsed.
///
/// The daemon calls this from a fixed telemetry loop so a cold-only lookup or
/// the last lookup in a workload cannot remain invisible forever.
pub fn flush_cache_metrics_due() {
    flush_cache_metrics_inner(false);
}

/// Flush every pending aggregate regardless of age.
///
/// This is the shutdown boundary: after it returns, every cache sample either
/// appeared in its exact dimension row or in the explicit overflow row.
pub fn flush_cache_metrics() {
    flush_cache_metrics_inner(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_sharding_is_stable_and_dimensions_remain_distinct() {
        let hit = MetricKey {
            metric: "test_cache",
            namespace: None,
            outcome: "hit",
            reason: Some("ready"),
        };
        let miss = MetricKey {
            outcome: "miss",
            ..hit
        };
        assert_eq!(stable_shard(&hit), stable_shard(&hit));
        assert_ne!(hit, miss);
    }

    #[test]
    fn forced_flush_drains_a_single_recent_sample() {
        let now = Instant::now();
        let key = MetricKey {
            metric: "forced_flush_test",
            namespace: None,
            outcome: "miss",
            reason: Some("cold"),
        };
        let mut shard = Shard {
            last_report: now,
            rows: HashMap::from([(
                key,
                Aggregate {
                    count: 1,
                    ..Aggregate::default()
                },
            )]),
            dropped_samples: 0,
        };

        assert!(drain_shard(&mut shard, now, false).is_none());
        let (rows, dropped) = drain_shard(&mut shard, now, true).expect("forced flush");
        assert_eq!(rows.get(&key).map(|row| row.count), Some(1));
        assert_eq!(dropped, 0);
        assert!(shard.rows.is_empty());
    }
}
