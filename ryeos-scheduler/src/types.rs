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
/// Takes the first 32 hex chars (128 bits) with a `sched-` prefix.
pub fn thread_id_from_fire(fire_id: &str) -> String {
    let hash = lillux::cas::sha256_hex(fire_id.as_bytes());
    format!("sched-{}", &hash[..32])
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
    fn thread_id_has_sched_prefix() {
        let tid = thread_id_from_fire("test@1000");
        assert!(tid.starts_with("sched-"));
    }

    #[test]
    fn thread_id_is_32_hex_chars() {
        let tid = thread_id_from_fire("test@1000");
        let hex_part = &tid[6..]; // after "sched-"
        assert_eq!(hex_part.len(), 32);
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
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
