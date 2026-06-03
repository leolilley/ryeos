//! Shared types for the scheduler module.

use serde::Serialize;

/// Projection record for a schedule spec in `scheduler.sqlite3`.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleSpecRecord {
    pub schedule_id: String,
    pub item_ref: String,
    pub params: String,
    pub schedule_type: String,
    pub expression: String,
    pub timezone: String,
    pub misfire_policy: String,
    pub overlap_policy: String,
    pub lateness_grace_secs: i64,
    pub enabled: bool,
    pub project_root: Option<String>,
    pub signer_fingerprint: String,
    pub spec_hash: String,
    /// Immutable scheduling anchor (millis since epoch).
    /// Set once at creation, preserved on updates. Used by timer for fire-time calculation.
    pub registered_at: i64,
    /// Fingerprint of the principal who registered this schedule.
    /// Used as the acting principal at dispatch time.
    pub requester_fingerprint: String,
    /// Capabilities granted at registration time. The schedule runs with
    /// only these capabilities — least privilege.
    pub capabilities: Vec<String>,
}

/// Advisory projection cursor for a schedule.
///
/// This is rebuildable cache state, not a source of idempotency.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleCursorRecord {
    pub schedule_id: String,
    pub spec_hash: String,
    pub last_scheduled_at: Option<i64>,
    pub next_fire_at: Option<i64>,
    pub last_evaluated_at: Option<i64>,
    pub updated_at: i64,
}

/// Projection record for a fire event in `scheduler.sqlite3`.
#[derive(Debug, Clone, Serialize)]
pub struct FireRecord {
    pub fire_id: String,
    pub schedule_id: String,
    pub scheduled_at: i64,
    pub fired_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub thread_id: Option<String>,
    pub status: String,
    pub trigger_reason: String,
    pub outcome: Option<String>,
    pub signer_fingerprint: Option<String>,
}

/// Signal sent via mpsc to reload the timer loop.
#[derive(Debug, Clone)]
pub struct ReloadSignal {
    pub schedule_id: Option<String>,
}

/// A pending fire to dispatch (from misfire catch-up or recovery).
#[derive(Debug, Clone)]
pub struct PendingFire {
    pub fire_id: String,
    pub scheduled_at: i64,
    pub reason: &'static str,
}

/// Compute the deterministic fire_id for a schedule at a given time.
pub fn fire_id(schedule_id: &str, scheduled_at_ms: i64) -> String {
    format!("{}@{}", schedule_id, scheduled_at_ms)
}

/// Compute the deterministic thread_id from a fire_id.
/// Uses `lillux::cas::sha256_hex` (same hash used throughout ryeosd).
/// Takes the first 32 hex chars (128 bits) and formats them as a
/// canonical RyeOS execution thread id (`T-{uuid}`). Scheduler-specific
/// identity remains in `fire_id`; execution threads must stay in the
/// global `T-*` namespace enforced by thread lifecycle.
pub fn thread_id_from_fire(fire_id: &str) -> String {
    let hash = lillux::cas::sha256_hex(fire_id.as_bytes());
    format!(
        "T-{}-{}-{}-{}-{}",
        &hash[0..8],
        &hash[8..12],
        &hash[12..16],
        &hash[16..20],
        &hash[20..32],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fire_id_format() {
        let fid = fire_id("test-sched", 1700000000000);
        assert_eq!(fid, "test-sched@1700000000000");
    }

    #[test]
    fn fire_id_deterministic() {
        let a = fire_id("my-schedule", 1000);
        let b = fire_id("my-schedule", 1000);
        assert_eq!(a, b);
    }

    #[test]
    fn fire_id_different_times() {
        let a = fire_id("my-schedule", 1000);
        let b = fire_id("my-schedule", 2000);
        assert_ne!(a, b);
    }

    #[test]
    fn fire_id_different_schedules() {
        let a = fire_id("schedule-a", 1000);
        let b = fire_id("schedule-b", 1000);
        assert_ne!(a, b);
    }

    #[test]
    fn thread_id_has_canonical_thread_prefix() {
        let tid = thread_id_from_fire("test@1000");
        assert!(tid.starts_with("T-"));
    }

    #[test]
    fn thread_id_is_uuid_shaped() {
        let tid = thread_id_from_fire("test@1000");
        let uuid_part = tid.strip_prefix("T-").expect("T- prefix");
        let groups: Vec<&str> = uuid_part.split('-').collect();
        assert_eq!(groups.len(), 5);
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(groups
            .iter()
            .flat_map(|group| group.chars())
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn thread_id_deterministic() {
        let a = thread_id_from_fire("test@1000");
        let b = thread_id_from_fire("test@1000");
        assert_eq!(a, b);
    }

    #[test]
    fn thread_id_different_fires() {
        let a = thread_id_from_fire("sched@1000");
        let b = thread_id_from_fire("sched@2000");
        assert_ne!(a, b);
    }
}
