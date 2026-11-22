//! `ui.ryeos.gc.status` — read-only GC state and recent history.
//!
//! Reports bounded sidecar/log observations, not proof of a live GC owner.
//! The maintenance owner remains responsible for actual lifecycle status.
//! This is a dedicated endpoint
//! so the ryeos-ui can poll/refresh GC status independently of the
//! snapshot.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct GcStatusResponse {
    pub schema_version: &'static str,
    /// Sidecars cannot establish whether an owner is currently alive.
    pub activity: &'static str,
    pub coverage: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_events: Vec<Value>,
    pub pending_head_transitions: ryeos_app::state_store::PendingHeadTransitionStatus,
}

/// Max recent events to return from the JSONL log.
const MAX_RECENT_EVENTS: usize = 10;
/// Max bytes to read from the tail of the GC JSONL log.
const MAX_LOG_TAIL_BYTES: u64 = 256 * 1024;
const MAX_STATE_BYTES: u64 = 64 * 1024;

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;

    let runtime_state_dir = state.config.runtime_state_dir();

    let state_sidecar = runtime_state_dir.join("gc.state.json");
    let gc_state = read_observed_state(&state_sidecar)?;

    // Read recent GC events from the JSONL log.
    let log_path = runtime_state_dir.join("logs").join("gc.jsonl");
    let recent_events = read_recent_gc_events(&log_path, MAX_RECENT_EVENTS)?;

    let response = GcStatusResponse {
        schema_version: "ryeos-ui.gc.status.v2",
        activity: "unknown",
        coverage: "sidecar_and_bounded_log_observations_not_live_or_exhaustive",
        state: gc_state,
        recent_events,
        pending_head_transitions: state.state_store.pending_head_transition_status()?,
    };

    serde_json::to_value(response).map_err(Into::into)
}

/// Read the last N lines from the GC event log as JSON values.
pub(super) fn read_recent_gc_events(
    log_path: &std::path::Path,
    limit: usize,
) -> Result<Vec<Value>> {
    use std::io::{Read, Seek, SeekFrom};

    let parent = log_path
        .parent()
        .context("GC observation log has no parent")?;
    let name = log_path
        .file_name()
        .context("GC observation log has no name")?;
    let Some(directory) = lillux::secure_fs::PinnedDirectory::open(parent)? else {
        return Ok(Vec::new());
    };
    let Some(mut file) = directory.open_pinned_regular(name, false)? else {
        return Ok(Vec::new());
    };
    let len = file.size().context("inspect GC observation log")?;
    let start = len.saturating_sub(MAX_LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))
        .context("seek GC observation log")?;
    let mut bytes = Vec::new();
    file.take(MAX_LOG_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .context("read bounded GC observation log")?;
    // The leading line may have been cut by the byte bound. It is not a
    // complete record and must not be parsed as one.
    let bytes = if start > 0 {
        bytes
            .splitn(2, |byte| *byte == b'\n')
            .nth(1)
            .context("GC log tail has no complete record within byte limit")?
    } else {
        &bytes[..]
    };
    let content = std::str::from_utf8(bytes).context("decode GC observation log")?;
    anyhow::ensure!(
        start == 0 || !content.trim().is_empty(),
        "GC log tail contains no complete record within byte limit"
    );

    let mut events: Vec<Value> = Vec::new();
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event = serde_json::from_str::<Value>(trimmed)
            .context("decode complete GC observation record")?;
        events.push(event);
        if events.len() >= limit {
            break;
        }
    }
    // Reverse so events are in chronological order (oldest first).
    events.reverse();
    Ok(events)
}

fn read_observed_state(path: &std::path::Path) -> Result<Option<Value>> {
    let Some(bytes) =
        lillux::secure_fs::read_optional_regular_file_bounded_no_follow(path, MAX_STATE_BYTES)
            .context("read GC state observation")?
    else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .context("decode GC state observation")
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/gc/status",
    endpoint: "ui.ryeos.gc.status",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_observation_is_distinct_from_corrupt_observation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert!(read_observed_state(&path).unwrap().is_none());
        std::fs::write(&path, b"broken").unwrap();
        assert!(read_observed_state(&path).is_err());
        std::fs::write(&path, vec![b' '; MAX_STATE_BYTES as usize + 1]).unwrap();
        assert!(read_observed_state(&path).is_err());
    }

    #[test]
    fn log_decode_failure_is_not_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gc.jsonl");
        assert!(read_recent_gc_events(&path, 10).unwrap().is_empty());
        std::fs::write(&path, b"{\"phase\":\"started\"}\nnot-json\n").unwrap();
        assert!(read_recent_gc_events(&path, 10).is_err());
        std::fs::write(
            &path,
            b"{\"phase\":\"started\"}\n{\"phase\":\"finished\"}\n",
        )
        .unwrap();
        let events = read_recent_gc_events(&path, 1).unwrap();
        assert_eq!(events[0]["phase"], "finished");
    }

    #[test]
    fn oversized_log_record_cannot_appear_as_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gc.jsonl");
        std::fs::write(&path, vec![b'x'; MAX_LOG_TAIL_BYTES as usize + 1]).unwrap();
        assert!(read_recent_gc_events(&path, 10).is_err());
        let mut bytes = vec![b'x'; MAX_LOG_TAIL_BYTES as usize + 1];
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_recent_gc_events(&path, 10).is_err());
        bytes.extend_from_slice(b"\n{\"phase\":\"finished\"}\n");
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(
            read_recent_gc_events(&path, 10).unwrap()[0]["phase"],
            "finished"
        );
    }
}
