//! Retention sweeps: age/count-bound append-only runtime files.
//!
//! GC's mark-and-sweep collects unreachable CAS objects, and
//! [`super::purge_runtime_state`] drops disposable runtime history under the
//! deep profile. Neither bounds the *append-only* logs that grow one line per
//! event forever:
//!
//! - **Schedule fire JSONL** (`.ai/state/schedules/<id>/fires.jsonl`) — one
//!   group of lines per fire; never pruned by any other pass.
//! - **Sync jobs / sync-job attempts** (stable operational tables) — one row
//!   per remote op; retention for those lives on [`crate::OperationalDb`].
//!   This module holds the shared cutoff computation.
//!
//! Every operational bound comes from [`super::GcParams`]. Rust supplies no
//! retention age or count; omitting both schedule-fire bounds disables this
//! sweep.

#[cfg(test)]
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::GcResult;

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// The one current durable scheduler-fire snapshot shape. Scheduler writes,
/// projection replay, and retention all validate this exact contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FireRecord {
    pub fire_id: String,
    pub schedule_id: String,
    pub scheduled_at: i64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub fired_at: Option<i64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub completed_at: Option<i64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub thread_id: Option<String>,
    pub status: String,
    pub trigger_reason: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub outcome: Option<String>,
    pub signer_fingerprint: String,
}

impl FireRecord {
    pub fn validate(&self) -> Result<()> {
        validate_schedule_id(&self.schedule_id)?;
        if self.scheduled_at < 0 {
            anyhow::bail!("scheduler fire scheduled_at must not be negative");
        }
        let expected_fire_id = format!("{}@{}", self.schedule_id, self.scheduled_at);
        if self.fire_id != expected_fire_id {
            anyhow::bail!(
                "scheduler fire identity {} does not match canonical {}",
                self.fire_id,
                expected_fire_id
            );
        }
        let fired_at = self
            .fired_at
            .context("scheduler fire fired_at must be present")?;
        if fired_at < self.scheduled_at {
            anyhow::bail!("scheduler fire fired_at precedes scheduled_at");
        }
        require_non_empty(&self.trigger_reason, "scheduler fire trigger_reason")?;
        if !lillux::cas::valid_hash(&self.signer_fingerprint)
            || self
                .signer_fingerprint
                .bytes()
                .any(|byte| byte.is_ascii_uppercase())
        {
            anyhow::bail!("scheduler fire signer_fingerprint must be lowercase SHA-256 hex");
        }
        if let Some(thread_id) = &self.thread_id {
            let hash = lillux::cas::sha256_hex(self.fire_id.as_bytes());
            let expected_thread_id = format!(
                "T-{}-{}-{}-{}-{}",
                &hash[0..8],
                &hash[8..12],
                &hash[12..16],
                &hash[16..20],
                &hash[20..32]
            );
            if thread_id != &expected_thread_id {
                anyhow::bail!(
                    "scheduler fire {} thread_id is not deterministic",
                    self.fire_id
                );
            }
        }
        match self.status.as_str() {
            "dispatched" => {
                if self.completed_at.is_some() || self.outcome.is_some() {
                    anyhow::bail!(
                        "dispatched scheduler fire must not have completed_at or outcome"
                    );
                }
                if self.thread_id.is_none() {
                    anyhow::bail!(
                        "dispatched scheduler fire must have its deterministic thread_id"
                    );
                }
            }
            "completed" | "failed" | "cancelled" | "skipped" => {
                let completed_at = self
                    .completed_at
                    .context("terminal scheduler fire must have completed_at")?;
                if completed_at < fired_at {
                    anyhow::bail!("scheduler fire completed_at precedes fired_at");
                }
                require_non_empty(
                    self.outcome
                        .as_deref()
                        .context("terminal scheduler fire must have outcome")?,
                    "scheduler fire outcome",
                )?;
                if self.status == "skipped" && self.thread_id.is_some() {
                    anyhow::bail!("skipped scheduler fire must not have a thread_id");
                }
                if self.status != "skipped" && self.thread_id.is_none() {
                    anyhow::bail!("non-skipped terminal scheduler fire must retain thread_id");
                }
                let outcome = self.outcome.as_deref().expect("checked above");
                let valid_outcome = match self.status.as_str() {
                    "completed" => matches!(outcome, "success" | "result_failed"),
                    "cancelled" => outcome == "thread_cancelled",
                    "skipped" => outcome == self.trigger_reason,
                    // Failure codes are runtime-defined (`handler_error`,
                    // `exit:1`, `required_secret_missing`, ...). They must not
                    // claim one of the meanings reserved for another status.
                    "failed" => {
                        !matches!(outcome, "success" | "result_failed" | "thread_cancelled")
                    }
                    _ => false,
                };
                if !valid_outcome {
                    anyhow::bail!(
                        "scheduler fire status {} has incompatible outcome {}",
                        self.status,
                        outcome
                    );
                }
            }
            status => anyhow::bail!("invalid scheduler fire status: {status}"),
        }
        Ok(())
    }

    pub fn validate_transition_from(&self, previous: &Self) -> Result<()> {
        previous.validate()?;
        self.validate()?;
        if self.fire_id != previous.fire_id
            || self.schedule_id != previous.schedule_id
            || self.scheduled_at != previous.scheduled_at
            || self.trigger_reason != previous.trigger_reason
            || self.signer_fingerprint != previous.signer_fingerprint
        {
            anyhow::bail!("scheduler fire transition changes immutable identity or authority");
        }
        if self == previous {
            return Ok(());
        }
        if previous.status != "dispatched" {
            anyhow::bail!(
                "terminal scheduler fire {} cannot transition from {} to {}",
                self.fire_id,
                previous.status,
                self.status
            );
        }
        if self.fired_at != previous.fired_at || self.thread_id != previous.thread_id {
            anyhow::bail!(
                "scheduler fire transition changes immutable dispatch timestamp or thread identity"
            );
        }
        Ok(())
    }

    pub fn canonical_json_line(&self) -> Result<String> {
        self.validate()?;
        Ok(lillux::cas::canonical_json(&serde_json::to_value(self)?)?)
    }

    fn latest_timestamp(&self) -> i64 {
        [self.completed_at, self.fired_at, Some(self.scheduled_at)]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(0)
    }
}

/// Canonical schedule identity shared by source paths, fire IDs, SQLite, and
/// retained journals. Lowercase ASCII slugs make the `id@timestamp` fire-id
/// delimiter unambiguous on every filesystem.
pub fn validate_schedule_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 {
        anyhow::bail!("schedule_id must contain 1..=128 bytes");
    }
    let bytes = id.as_bytes();
    let is_alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    if !is_alphanumeric(bytes[0]) || !is_alphanumeric(bytes[bytes.len() - 1]) {
        anyhow::bail!("schedule_id must begin and end with a lowercase ASCII letter or digit");
    }
    if bytes
        .iter()
        .any(|byte| !is_alphanumeric(*byte) && !matches!(*byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!(
            "schedule_id may contain only lowercase ASCII letters, digits, '-', '_', and '.'"
        );
    }
    Ok(())
}

fn require_non_empty(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    Ok(())
}

/// Fully resolved schedule-fire retention bounds for one maintenance run.
///
/// The age cutoff is captured once and then shared by the JSONL authority and
/// its SQLite projection. This keeps both stores on the same boundary while
/// still making every bound originate in signed maintenance parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FireRetentionPolicy {
    pub cutoff_ms: Option<i64>,
    pub max_count: Option<usize>,
}

impl FireRetentionPolicy {
    pub fn from_bounds(max_age_days: Option<u64>, max_count: Option<usize>) -> Option<Self> {
        if max_age_days.is_none() && max_count.is_none() {
            return None;
        }
        Some(Self {
            cutoff_ms: max_age_days.map(millis_days_ago),
            max_count,
        })
    }
}

/// ISO8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for `days` before now.
///
/// Same wire shape as [`lillux::time::iso8601_now`], so the result is directly
/// comparable (lexicographically) against timestamps stored operationally.
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
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    let age_ms = days.saturating_mul(86_400_000).min(i64::MAX as u64) as i64;
    now.saturating_sub(age_ms)
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
/// live fire. When present, the age bound prunes terminal groups older than
/// `max_age_days`; when present, the count bound then keeps the newest
/// `max_count` terminal groups. With both omitted this function is a no-op.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FireRetentionTarget {
    pub schedule_id: String,
    pub fire_id: String,
}

pub fn sweep_fire_jsonl(
    state_dir: &Path,
    policy: Option<FireRetentionPolicy>,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<Vec<FireRetentionTarget>> {
    let state_directory = lillux::PinnedDirectory::open(state_dir)?.ok_or_else(|| {
        anyhow::anyhow!(
            "schedule state directory is absent: {}",
            state_dir.display()
        )
    })?;
    sweep_fire_jsonl_in_directory(&state_directory, policy, dry_run, result)
}

/// Age/count-bound schedule journals beneath one already-pinned runtime root.
pub fn sweep_fire_jsonl_in_directory(
    state_directory: &lillux::PinnedDirectory,
    policy: Option<FireRetentionPolicy>,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<Vec<FireRetentionTarget>> {
    let Some(policy) = policy else {
        return Ok(Vec::new());
    };
    let Some(schedules_directory) =
        state_directory.open_child_directory(std::ffi::OsStr::new("schedules"))?
    else {
        return Ok(Vec::new());
    };
    let schedule_names = schedules_directory.entry_names()?;

    let mut targets = std::collections::BTreeSet::new();
    for schedule_name in schedule_names {
        let schedule_directory = schedules_directory
            .open_child_directory(&schedule_name)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "schedule retention root contains unsupported entry {}",
                    schedules_directory.path().join(&schedule_name).display()
                )
            })?;
        let schedule_id = schedule_name
            .to_str()
            .context("schedule history directory name is not UTF-8")?;
        validate_schedule_id(schedule_id)?;
        let fires_name = std::ffi::OsStr::new("fires.jsonl");
        let fires_path = schedule_directory.path().join(fires_name);
        let Some(_fires_file) = schedule_directory
            .open_regular(fires_name, false)
            .with_context(|| {
                format!(
                    "schedule fire journal must be a regular non-symlink file: {}",
                    fires_path.display()
                )
            })?
        else {
            continue;
        };
        let lock = if dry_run {
            lillux::ExclusiveFileLock::acquire_existing_in(&schedule_directory, fires_name)?
        } else {
            lillux::ExclusiveFileLock::acquire_in(&schedule_directory, fires_name)?
        };
        let selected =
            sweep_one_fires_file(&lock, &fires_path, schedule_id, policy, dry_run, result)?;
        for target in selected {
            if !targets.insert(target.clone()) {
                anyhow::bail!(
                    "duplicate schedule-fire retention target {} for {}",
                    target.fire_id,
                    target.schedule_id,
                );
            }
        }
    }

    Ok(targets.into_iter().collect())
}

/// Terminal fire statuses — a group whose newest line has one of these is
/// settled and eligible for retention.
pub fn is_terminal_fire_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "skipped")
}

fn sweep_one_fires_file(
    lock: &lillux::ExclusiveFileLock,
    path: &Path,
    expected_schedule_id: &str,
    policy: FireRetentionPolicy,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<Vec<FireRetentionTarget>> {
    let mut raw = String::new();
    lock.open_target_read()
        .with_context(|| format!("open retained fire journal {}", path.display()))?
        .read_to_string(&mut raw)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let original_len = raw.len() as u64;
    if !raw.is_empty() && !raw.ends_with('\n') {
        anyhow::bail!(
            "fire journal {} has a crash-truncated final line; scheduler append recovery must repair it before retention",
            path.display()
        );
    }

    struct Group {
        latest: FireRecord,
        newest_ts: i64,
        order: usize,
        line_count: usize,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut journal_lines = Vec::new();

    for (line_number, line) in raw.lines().enumerate() {
        if line.is_empty() {
            anyhow::bail!(
                "fire journal {} contains an empty line at {}",
                path.display(),
                line_number + 1
            );
        }
        let record: FireRecord = serde_json::from_str(line).with_context(|| {
            format!(
                "decode fire snapshot in {} at line {}",
                path.display(),
                line_number + 1
            )
        })?;
        record.validate()?;
        if record.schedule_id != expected_schedule_id {
            anyhow::bail!(
                "fire {} belongs to schedule {} but is stored under {}",
                record.fire_id,
                record.schedule_id,
                expected_schedule_id
            );
        }
        if record.canonical_json_line()? != line {
            anyhow::bail!(
                "fire journal {} line {} is not canonical JSON",
                path.display(),
                line_number + 1
            );
        }
        let fire_id = record.fire_id.clone();
        journal_lines.push((fire_id.clone(), line.to_owned()));
        match index.get(&fire_id) {
            Some(&gi) => {
                record.validate_transition_from(&groups[gi].latest)?;
                groups[gi].newest_ts = record.latest_timestamp();
                groups[gi].latest = record;
                groups[gi].line_count += 1;
            }
            None => {
                let order = groups.len();
                index.insert(fire_id, order);
                groups.push(Group {
                    newest_ts: record.latest_timestamp(),
                    latest: record,
                    order,
                    line_count: 1,
                });
            }
        }
    }

    // The lexicographically greatest valid (scheduled_at, fire_id) is a durable
    // planning watermark. It is retained regardless of authored age/count
    // bounds, even if already terminal.
    let watermark_order = groups
        .iter()
        .max_by(|left, right| {
            left.latest
                .scheduled_at
                .cmp(&right.latest.scheduled_at)
                .then(left.latest.fire_id.cmp(&right.latest.fire_id))
        })
        .map(|group| group.order);

    let mut drop_orders = std::collections::BTreeSet::new();
    for group in &groups {
        if is_terminal_fire_status(&group.latest.status)
            && Some(group.order) != watermark_order
            && policy
                .cutoff_ms
                .is_some_and(|cutoff| group.newest_ts < cutoff)
        {
            drop_orders.insert(group.order);
        }
    }

    // Count bound: keep the newest `max_count` terminal groups; non-terminal
    // (in-flight) groups are always retained and don't count against the bound.
    let terminal_count = groups
        .iter()
        .filter(|group| {
            is_terminal_fire_status(&group.latest.status) && !drop_orders.contains(&group.order)
        })
        .count();
    if let Some(max_count) = policy.max_count.filter(|bound| terminal_count > *bound) {
        let mut terminal_refs: Vec<(i64, String, usize)> = groups
            .iter()
            .filter(|group| {
                is_terminal_fire_status(&group.latest.status)
                    && Some(group.order) != watermark_order
                    && !drop_orders.contains(&group.order)
            })
            .map(|g| (g.newest_ts, g.latest.fire_id.clone(), g.order))
            .collect();
        // Oldest first, with fire identity as the durable tie-break shared by
        // the scheduler projection retention pass.
        terminal_refs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let to_drop = terminal_count - max_count;
        for (_, _, order) in terminal_refs.into_iter().take(to_drop) {
            drop_orders.insert(order);
        }
    }

    if drop_orders.is_empty() {
        return Ok(Vec::new());
    }

    let mut selected = Vec::with_capacity(drop_orders.len());
    let mut dropped_fire_ids = std::collections::HashSet::new();
    let mut dropped_lines = 0usize;
    for group in &groups {
        if drop_orders.contains(&group.order) {
            dropped_lines += group.line_count;
            dropped_fire_ids.insert(group.latest.fire_id.clone());
            selected.push(FireRetentionTarget {
                schedule_id: group.latest.schedule_id.clone(),
                fire_id: group.latest.fire_id.clone(),
            });
        }
    }
    let mut rebuilt = String::new();
    for (fire_id, line) in journal_lines {
        if !dropped_fire_ids.contains(&fire_id) {
            rebuilt.push_str(&line);
            rebuilt.push('\n');
        }
    }
    let new_len = rebuilt.len() as u64;

    if !dry_run {
        lock.replace_target(rebuilt.as_bytes())
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

    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fire_record_requires_every_nullable_wire_key() {
        let complete = serde_json::json!({
            "fire_id": "sched@1",
            "schedule_id": "sched",
            "scheduled_at": 1,
            "fired_at": 1,
            "completed_at": null,
            "thread_id": "T-a",
            "status": "dispatched",
            "trigger_reason": "normal",
            "outcome": null,
            "signer_fingerprint": "11".repeat(32),
        });
        for key in ["fired_at", "completed_at", "thread_id", "outcome"] {
            let mut missing = complete.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<FireRecord>(missing).is_err(),
                "missing nullable key {key} must fail exact fire decoding"
            );
        }
    }

    fn fire_snapshot_line(
        fire_id: &str,
        schedule_id: &str,
        scheduled_at: i64,
        completed_at: Option<i64>,
        status: &str,
    ) -> String {
        let hash = lillux::cas::sha256_hex(fire_id.as_bytes());
        let thread_id = format!(
            "T-{}-{}-{}-{}-{}",
            &hash[0..8],
            &hash[8..12],
            &hash[12..16],
            &hash[16..20],
            &hash[20..32]
        );
        FireRecord {
            fire_id: fire_id.to_owned(),
            schedule_id: schedule_id.to_owned(),
            scheduled_at,
            fired_at: Some(scheduled_at),
            completed_at,
            thread_id: (status != "skipped").then_some(thread_id),
            status: status.to_owned(),
            trigger_reason: "normal".to_owned(),
            outcome: match status {
                "dispatched" => None,
                "completed" => Some("success".to_owned()),
                "cancelled" => Some("thread_cancelled".to_owned()),
                "skipped" => Some("normal".to_owned()),
                _ => Some("thread_failed".to_owned()),
            },
            signer_fingerprint: "11".repeat(32),
        }
        .canonical_json_line()
        .unwrap()
    }

    #[test]
    fn fire_retention_is_disabled_without_authored_bounds() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path();
        let sched_dir = state_dir.join("schedules").join("sched");
        fs::create_dir_all(&sched_dir).unwrap();
        let fires = sched_dir.join("fires.jsonl");
        let content = format!(
            "{}\n",
            fire_snapshot_line("sched@0", "sched", 0, Some(0), "completed")
        );
        fs::write(&fires, &content).unwrap();

        let mut result = GcResult::default();
        sweep_fire_jsonl(state_dir, None, false, &mut result).unwrap();

        assert_eq!(result.deleted_fire_records, 0);
        assert_eq!(fs::read_to_string(&fires).unwrap(), content);
    }

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
        let old_id = format!("maintenance-gc@{old}");
        let recent_id = format!("maintenance-gc@{recent}");
        let current_id = format!("maintenance-gc@{now}");
        let content = [
            fire_snapshot_line(&old_id, "maintenance-gc", old, None, "dispatched"),
            fire_snapshot_line(&old_id, "maintenance-gc", old, Some(old), "completed"),
            fire_snapshot_line(
                &recent_id,
                "maintenance-gc",
                recent,
                Some(recent),
                "completed",
            ),
            fire_snapshot_line(&current_id, "maintenance-gc", now, None, "dispatched"),
        ]
        .join("\n")
            + "\n";
        fs::write(&fires, content).unwrap();

        let mut result = GcResult::default();
        sweep_fire_jsonl(
            state_dir,
            FireRetentionPolicy::from_bounds(Some(30), Some(500)),
            false,
            &mut result,
        )
        .unwrap();

        let out = fs::read_to_string(&fires).unwrap();
        assert!(
            !out.contains(&old_id),
            "old terminal group should be pruned"
        );
        assert!(out.contains(&recent_id), "recent terminal group retained");
        assert!(out.contains(&current_id), "in-flight group always retained");
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
            content.push_str(&fire_snapshot_line(
                &format!("sched@{ts}"),
                "sched",
                ts,
                Some(ts),
                "completed",
            ));
            content.push('\n');
        }
        fs::write(&fires, content).unwrap();

        let mut result = GcResult::default();
        // Keep only the newest 2 terminal groups.
        let targets = sweep_fire_jsonl(
            state_dir,
            FireRetentionPolicy::from_bounds(Some(3650), Some(2)),
            false,
            &mut result,
        )
        .unwrap();

        let out = fs::read_to_string(&fires).unwrap();
        assert!(out.contains(&format!("sched@{}", base + 4_000)));
        assert!(out.contains(&format!("sched@{}", base + 3_000)));
        assert!(
            !out.contains(&format!("sched@{base}"))
                && !out.contains(&format!("sched@{}", base + 1_000))
                && !out.contains(&format!("sched@{}", base + 2_000)),
            "oldest dropped: {out}"
        );
        assert_eq!(result.deleted_fire_records, 3);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.fire_id.clone())
                .collect::<Vec<_>>(),
            vec![
                format!("sched@{base}"),
                format!("sched@{}", base + 1_000),
                format!("sched@{}", base + 2_000),
            ]
        );
    }

    #[test]
    fn fire_retention_always_preserves_latest_scheduled_watermark() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path();
        let sched_dir = state_dir.join("schedules").join("sched");
        fs::create_dir_all(&sched_dir).unwrap();
        let fires = sched_dir.join("fires.jsonl");
        let old = millis_days_ago(100);
        let content = [
            fire_snapshot_line("sched@1", "sched", 1, Some(old), "completed"),
            fire_snapshot_line("sched@2", "sched", 2, Some(old), "completed"),
        ]
        .join("\n")
            + "\n";
        fs::write(&fires, content).unwrap();

        let mut result = GcResult::default();
        let targets = sweep_fire_jsonl(
            state_dir,
            FireRetentionPolicy {
                cutoff_ms: Some(millis_days_ago(30)),
                max_count: Some(0),
            }
            .into(),
            false,
            &mut result,
        )
        .unwrap();

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].fire_id, "sched@1");
        let retained = fs::read_to_string(&fires).unwrap();
        assert!(!retained.contains("sched@1"));
        assert!(retained.contains("sched@2"));
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
            "{}\n",
            fire_snapshot_line(
                &format!("sched@{base}"),
                "sched",
                base,
                Some(base),
                "completed",
            )
        );
        fs::write(&fires, &content).unwrap();

        let mut result = GcResult::default();
        sweep_fire_jsonl(
            state_dir,
            FireRetentionPolicy::from_bounds(Some(30), Some(500)),
            false,
            &mut result,
        )
        .unwrap();

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
        let content = [
            fire_snapshot_line(
                &format!("sched@{}", old - 1),
                "sched",
                old - 1,
                Some(old - 1),
                "completed",
            ),
            fire_snapshot_line(
                &format!("sched@{old}"),
                "sched",
                old,
                Some(old),
                "completed",
            ),
        ]
        .join("\n")
            + "\n";
        fs::write(&fires, &content).unwrap();
        // Current scheduler publication establishes the persistent append-lock
        // anchor before a dry-run ever inspects this journal.
        drop(lillux::ExclusiveFileLock::acquire(&fires).unwrap());

        let mut result = GcResult::default();
        sweep_fire_jsonl(
            state_dir,
            FireRetentionPolicy::from_bounds(Some(30), Some(500)),
            true,
            &mut result,
        )
        .unwrap();

        assert_eq!(result.deleted_fire_records, 1, "dry run still reports");
        assert_eq!(
            fs::read_to_string(&fires).unwrap(),
            content,
            "dry run must not modify the file"
        );
    }
}
