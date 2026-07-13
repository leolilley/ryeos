//! Retention sweeps: age/count-bound append-only runtime files.
//!
//! GC's mark-and-sweep collects unreachable CAS objects, and
//! [`super::purge_runtime_state`] drops disposable runtime history under the
//! deep profile. Neither bounds the *append-only* logs that grow one line per
//! event forever:
//!
//! - **Schedule fire JSONL** (`.ai/state/schedules/<id>/fires.jsonl`) — one
//!   group of lines per fire; never pruned by any other pass.
//! - **Sync jobs / sync-job attempts** (projection tables) — one row per remote
//!   op; retention for those lives on [`crate::ProjectionDb`] because it owns
//!   the DB handle. This module holds the shared cutoff computation.
//!
//! Retention runs only under the deep profile (`deep || prune_runtime_history`)
//! so a bare `ryeos gc` stays a pure CAS sweep — see the flag docs on
//! [`super::GcParams`].

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use super::GcResult;

/// Default retention window for terminal sync jobs (completed/failed/cancelled).
pub const SYNC_JOB_RETENTION_DAYS: u64 = 14;

/// Default age bound for schedule fire history. Fire groups whose newest
/// timestamp is older than this are dropped.
pub const FIRE_JSONL_MAX_AGE_DAYS: u64 = 30;

/// Default count bound for schedule fire history. After age pruning, at most
/// this many terminal fire groups are retained per schedule (newest kept).
pub const FIRE_JSONL_MAX_COUNT: usize = 500;

/// ISO8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for `days` before now.
///
/// Same wire shape as [`lillux::time::iso8601_now`], so the result is directly
/// comparable (lexicographically) against timestamps stored by the projection.
pub fn iso8601_days_ago(days: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cutoff = now.saturating_sub(days.saturating_mul(86_400));
    iso8601_from_unix_secs(cutoff)
}

/// Unix milliseconds for `days` before now.
pub fn millis_days_ago(days: u64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    now.saturating_sub((days as i64).saturating_mul(86_400_000))
}

fn iso8601_from_unix_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let day_secs = secs % 86_400;
    let hours = day_secs / 3_600;
    let minutes = (day_secs % 3_600) / 60;
    let seconds = day_secs % 60;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

// Howard Hinnant's days-from-civil inverse; mirrors the conversion used by
// `lillux::time::iso8601_now` so retention cutoffs share the stored format.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// Age/count-bound every `.ai/state/schedules/<id>/fires.jsonl`.
///
/// Fire histories interleave multiple lines per `fire_id` (dispatch → terminal),
/// so pruning is group-aware: a `fire_id`'s lines are dropped together, and
/// only *terminal* groups (whose newest line reports a settled status) are
/// eligible. In-flight groups are always retained so a rebuild never loses a
/// live fire. Age prunes terminal groups older than `max_age_days`; the count
/// bound then keeps the newest `max_count` terminal groups.
pub fn sweep_fire_jsonl(
    state_dir: &Path,
    max_age_days: u64,
    max_count: usize,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<()> {
    let schedules_dir = state_dir.join("schedules");
    if !schedules_dir.is_dir() {
        return Ok(());
    }

    let cutoff_ms = millis_days_ago(max_age_days);

    for entry in fs::read_dir(&schedules_dir)
        .with_context(|| format!("failed to read {}", schedules_dir.display()))?
    {
        let entry = entry.context("failed to read schedules dir entry")?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let fires_path = entry.path().join("fires.jsonl");
        if !fires_path.is_file() {
            continue;
        }
        sweep_one_fires_file(&fires_path, cutoff_ms, max_count, dry_run, result)?;
    }

    Ok(())
}

/// Terminal fire statuses — a group whose newest line has one of these is
/// settled and eligible for retention.
fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "skipped")
}

fn line_timestamp(v: &serde_json::Value) -> i64 {
    ["completed_at", "fired_at", "scheduled_at"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(|t| t.as_i64()))
        .max()
        .unwrap_or(0)
}

fn sweep_one_fires_file(
    path: &Path,
    cutoff_ms: i64,
    max_count: usize,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<()> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let original_len = raw.len() as u64;

    // Group lines by fire_id, preserving first-seen order. Malformed or
    // id-less lines are pinned to a stable synthetic group so they're never
    // silently dropped by retention.
    struct Group {
        lines: Vec<String>,
        newest_ts: i64,
        terminal: bool,
        order: usize,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Option<serde_json::Value> = serde_json::from_str(line).ok();
        let (key, ts, terminal) = match &parsed {
            Some(v) => {
                let key = v
                    .get("fire_id")
                    .and_then(|f| f.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| format!("__line_{i}"));
                let ts = line_timestamp(v);
                let terminal = v
                    .get("status")
                    .and_then(|s| s.as_str())
                    .map(is_terminal_status)
                    .unwrap_or(false);
                (key, ts, terminal)
            }
            None => (format!("__line_{i}"), 0, false),
        };
        match index.get(&key) {
            Some(&gi) => {
                let g = &mut groups[gi];
                g.lines.push(line.to_string());
                g.newest_ts = g.newest_ts.max(ts);
                // A group is terminal iff its newest line is terminal; recompute
                // by tracking whether this (latest-appended) line is terminal.
                g.terminal = terminal;
            }
            None => {
                let order = groups.len();
                index.insert(key, order);
                groups.push(Group {
                    lines: vec![line.to_string()],
                    newest_ts: ts,
                    terminal,
                    order,
                });
            }
        }
    }

    // Age prune: drop terminal groups older than the cutoff.
    let mut dropped_lines = 0usize;
    let mut retained: Vec<Group> = Vec::with_capacity(groups.len());
    for g in groups {
        if g.terminal && g.newest_ts < cutoff_ms {
            dropped_lines += g.lines.len();
        } else {
            retained.push(g);
        }
    }

    // Count bound: keep the newest `max_count` terminal groups; non-terminal
    // (in-flight) groups are always retained and don't count against the bound.
    let terminal_count = retained.iter().filter(|g| g.terminal).count();
    if terminal_count > max_count {
        let mut terminal_refs: Vec<(i64, usize)> = retained
            .iter()
            .filter(|g| g.terminal)
            .map(|g| (g.newest_ts, g.order))
            .collect();
        // Oldest first.
        terminal_refs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let to_drop = terminal_count - max_count;
        let drop_orders: std::collections::HashSet<usize> = terminal_refs
            .iter()
            .take(to_drop)
            .map(|(_, o)| *o)
            .collect();
        let mut kept: Vec<Group> = Vec::with_capacity(retained.len());
        for g in retained {
            if g.terminal && drop_orders.contains(&g.order) {
                dropped_lines += g.lines.len();
            } else {
                kept.push(g);
            }
        }
        retained = kept;
    }

    if dropped_lines == 0 {
        return Ok(());
    }

    // Rewrite in original group order.
    retained.sort_by_key(|g| g.order);
    let mut rebuilt = String::new();
    for g in &retained {
        for line in &g.lines {
            rebuilt.push_str(line);
            rebuilt.push('\n');
        }
    }
    let new_len = rebuilt.len() as u64;

    if !dry_run {
        lillux::atomic_write(path, rebuilt.as_bytes())
            .with_context(|| format!("failed to rewrite {}", path.display()))?;
    }

    result.deleted_fire_records += dropped_lines;
    result.freed_bytes += original_len.saturating_sub(new_len);
    tracing::info!(
        path = %path.display(),
        dropped_lines,
        freed_bytes = original_len.saturating_sub(new_len),
        dry_run,
        "retention: pruned schedule fire history"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_days_ago_is_earlier_and_well_formed() {
        let past = iso8601_days_ago(30);
        let now = lillux::time::iso8601_now();
        assert!(past < now, "cutoff {past} must sort before now {now}");
        assert_eq!(past.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ, got {past}");
        assert!(past.ends_with('Z'));
        assert_eq!(&past[4..5], "-");
        assert_eq!(&past[10..11], "T");
    }

    #[test]
    fn fire_retention_drops_old_terminal_groups_and_bounds_count() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path();
        let sched_dir = state_dir.join("schedules").join("maintenance-gc");
        fs::create_dir_all(&sched_dir).unwrap();
        let fires = sched_dir.join("fires.jsonl");

        let now = millis_days_ago(0);
        let old = now - 100 * 86_400_000; // 100 days old
        let recent = now - 86_400_000; // 1 day old

        // Old terminal group (2 lines) + recent terminal group (1 line) +
        // in-flight group (dispatched, no terminal status).
        let content = format!(
            concat!(
                "{{\"fire_id\":\"s@1\",\"scheduled_at\":{old},\"status\":\"dispatched\"}}\n",
                "{{\"fire_id\":\"s@1\",\"scheduled_at\":{old},\"completed_at\":{old},\"status\":\"completed\"}}\n",
                "{{\"fire_id\":\"s@2\",\"scheduled_at\":{recent},\"completed_at\":{recent},\"status\":\"completed\"}}\n",
                "{{\"fire_id\":\"s@3\",\"scheduled_at\":{now},\"status\":\"dispatched\"}}\n",
            ),
            old = old,
            recent = recent,
            now = now,
        );
        fs::write(&fires, content).unwrap();

        let mut result = GcResult::default();
        sweep_fire_jsonl(state_dir, 30, 500, false, &mut result).unwrap();

        let out = fs::read_to_string(&fires).unwrap();
        assert!(!out.contains("s@1"), "old terminal group should be pruned");
        assert!(out.contains("s@2"), "recent terminal group retained");
        assert!(out.contains("s@3"), "in-flight group always retained");
        assert_eq!(result.deleted_fire_records, 2, "two lines of s@1 dropped");
        assert!(result.freed_bytes > 0);
    }

    #[test]
    fn fire_retention_count_bound_keeps_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path();
        let sched_dir = state_dir.join("schedules").join("sched");
        fs::create_dir_all(&sched_dir).unwrap();
        let fires = sched_dir.join("fires.jsonl");

        let base = millis_days_ago(0);
        let mut content = String::new();
        for i in 0..5 {
            let ts = base + i * 1000; // all recent, so age never prunes
            content.push_str(&format!(
                "{{\"fire_id\":\"s@{i}\",\"scheduled_at\":{ts},\"completed_at\":{ts},\"status\":\"completed\"}}\n"
            ));
        }
        fs::write(&fires, content).unwrap();

        let mut result = GcResult::default();
        // Keep only the newest 2 terminal groups.
        sweep_fire_jsonl(state_dir, 3650, 2, false, &mut result).unwrap();

        let out = fs::read_to_string(&fires).unwrap();
        assert!(
            out.contains("s@4") && out.contains("s@3"),
            "newest kept: {out}"
        );
        assert!(
            !out.contains("s@0") && !out.contains("s@1") && !out.contains("s@2"),
            "oldest dropped: {out}"
        );
        assert_eq!(result.deleted_fire_records, 3);
    }

    #[test]
    fn fire_retention_noop_when_within_bounds() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path();
        let sched_dir = state_dir.join("schedules").join("sched");
        fs::create_dir_all(&sched_dir).unwrap();
        let fires = sched_dir.join("fires.jsonl");
        let base = millis_days_ago(0);
        let content = format!(
            "{{\"fire_id\":\"s@1\",\"scheduled_at\":{base},\"completed_at\":{base},\"status\":\"completed\"}}\n"
        );
        fs::write(&fires, &content).unwrap();

        let mut result = GcResult::default();
        sweep_fire_jsonl(state_dir, 30, 500, false, &mut result).unwrap();

        assert_eq!(result.deleted_fire_records, 0);
        assert_eq!(fs::read_to_string(&fires).unwrap(), content);
    }

    #[test]
    fn fire_retention_dry_run_leaves_file() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path();
        let sched_dir = state_dir.join("schedules").join("sched");
        fs::create_dir_all(&sched_dir).unwrap();
        let fires = sched_dir.join("fires.jsonl");
        let old = millis_days_ago(0) - 100 * 86_400_000;
        let content = format!(
            "{{\"fire_id\":\"s@1\",\"scheduled_at\":{old},\"completed_at\":{old},\"status\":\"completed\"}}\n"
        );
        fs::write(&fires, &content).unwrap();

        let mut result = GcResult::default();
        sweep_fire_jsonl(state_dir, 30, 500, true, &mut result).unwrap();

        assert_eq!(result.deleted_fire_records, 1, "dry run still reports");
        assert_eq!(
            fs::read_to_string(&fires).unwrap(),
            content,
            "dry run must not modify the file"
        );
    }
}
