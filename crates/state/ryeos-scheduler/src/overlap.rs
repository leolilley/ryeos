//! Overlap policy evaluation.
//!
//! Three policies: allow, skip, cancel_previous.
//! Every schedule authors one explicitly; invalid or absent policy holds the
//! schedule rather than selecting behavior in Rust.

use anyhow::{Context, Result};

use super::types::ScheduleSpecRecord;
use crate::SchedulerContext;

/// All three overlap policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlapPolicy {
    Allow,
    Skip,
    CancelPrevious,
}

pub fn parse_overlap_policy(raw: &str) -> Result<OverlapPolicy> {
    match raw {
        "allow" => Ok(OverlapPolicy::Allow),
        "cancel_previous" => Ok(OverlapPolicy::CancelPrevious),
        "skip" => Ok(OverlapPolicy::Skip),
        _ => anyhow::bail!("invalid overlap_policy: {raw}"),
    }
}

pub fn resolve_overlap_policy(spec: &ScheduleSpecRecord) -> Result<OverlapPolicy> {
    parse_overlap_policy(&spec.overlap_policy)
}

/// Check overlap for a schedule. Returns `true` if the fire should proceed.
/// Store/command failures are fatal to the timer rather than permission to
/// double-run work whose prior state could not be established.
pub async fn check_overlap<Ctx: SchedulerContext>(
    spec: &ScheduleSpecRecord,
    ctx: &Ctx,
) -> Result<bool> {
    let policy = resolve_overlap_policy(spec)?;

    let last_fire = match ctx
        .scheduler_db()
        .get_inflight_for_schedule(&spec.schedule_id)
    {
        Ok(Some(f)) => f,
        Ok(None) => return Ok(true), // no in-flight fire
        Err(error) => return Err(error),
    };
    last_fire.validate()?;

    let previous_thread_id = last_fire
        .thread_id
        .clone()
        .context("validated dispatched fire lost its deterministic thread id")?;

    let proceed = match policy {
        OverlapPolicy::Allow => true,

        OverlapPolicy::Skip => match ctx.get_thread_status(&previous_thread_id) {
            Ok(Some(status)) if thread_is_terminal(&status) => true,
            Ok(Some(_)) => {
                tracing::info!(
                    schedule_id = %spec.schedule_id,
                    previous_thread = %previous_thread_id,
                    overlap = "skip",
                    "previous fire still running — skipping"
                );
                false
            }
            // In-flight fire with no thread row yet: dispatch is
            // detached, so the claimed fire's thread may not have been
            // created at evaluation time. Treat as still-pending — the
            // next tick re-evaluates, and the repair sweep terminalizes
            // genuinely lost threads.
            Ok(None) => {
                tracing::info!(
                    schedule_id = %spec.schedule_id,
                    previous_thread = %previous_thread_id,
                    overlap = "skip",
                    "previous fire claimed but thread not yet visible — skipping"
                );
                false
            }
            Err(error) => return Err(error),
        },

        OverlapPolicy::CancelPrevious => match ctx.get_thread_status(&previous_thread_id) {
            Ok(Some(status)) if thread_is_terminal(&status) => true,
            Ok(Some(_)) => {
                tracing::info!(
                    schedule_id = %spec.schedule_id,
                    previous_thread = %previous_thread_id,
                    overlap = "cancel_previous",
                    "cancelling previous fire"
                );
                ctx.submit_cancel(&previous_thread_id)?;
                true
            }
            // Thread row not visible yet (detached dispatch in flight):
            // there is nothing to cancel, and proceeding would race the
            // previous fire into a double-run. Hold this boundary; the
            // next tick will see the thread and cancel it properly.
            Ok(None) => {
                tracing::info!(
                    schedule_id = %spec.schedule_id,
                    previous_thread = %previous_thread_id,
                    overlap = "cancel_previous",
                    "previous fire claimed but thread not yet visible — holding this boundary"
                );
                false
            }
            Err(error) => return Err(error),
        },
    };
    Ok(proceed)
}

pub fn thread_is_terminal(status: &str) -> bool {
    crate::thread_status_is_terminal(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec(overlap_policy: &str) -> ScheduleSpecRecord {
        ScheduleSpecRecord {
            schedule_id: "test".to_string(),
            item_ref: "directive:test".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            params: "{}".to_string(),
            schedule_type: "cron".to_string(),
            expression: "* * * * * *".to_string(),
            timezone: "UTC".to_string(),
            misfire_policy: "skip".to_string(),
            overlap_policy: overlap_policy.to_string(),
            lateness_grace_secs: 60,
            enabled: true,
            project_root: None,
            signer_fingerprint: "11".repeat(32),
            spec_hash: "22".repeat(32),
            registered_at: 0,
            requester_fingerprint: "fp:test".to_string(),
            capabilities: vec!["ryeos.execute.*".to_string()],
        }
    }

    #[test]
    fn resolve_allow() {
        assert_eq!(
            resolve_overlap_policy(&make_spec("allow")).unwrap(),
            OverlapPolicy::Allow
        );
    }

    #[test]
    fn resolve_skip() {
        assert_eq!(
            resolve_overlap_policy(&make_spec("skip")).unwrap(),
            OverlapPolicy::Skip
        );
    }

    #[test]
    fn resolve_cancel_previous() {
        assert_eq!(
            resolve_overlap_policy(&make_spec("cancel_previous")).unwrap(),
            OverlapPolicy::CancelPrevious
        );
    }

    #[test]
    fn empty_policy_is_rejected() {
        assert!(resolve_overlap_policy(&make_spec("")).is_err());
    }

    #[test]
    fn unknown_policy_is_rejected() {
        assert!(resolve_overlap_policy(&make_spec("invalid")).is_err());
    }

    #[test]
    fn overlap_terminal_classification_delegates_to_canonical_vocabulary() {
        for status in [
            "completed",
            "failed",
            "cancelled",
            "killed",
            "timed_out",
            "continued",
        ] {
            assert!(thread_is_terminal(status));
        }
        for status in ["created", "running", "pending", ""] {
            assert!(!thread_is_terminal(status));
        }
    }
}
