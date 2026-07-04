//! `commands.wait` — block until a command settles (`completed`/`rejected`).
//!
//! Awaits the command-settlement hub rather than polling; a woken waiter reads
//! the durable terminal row. The response carries the record plus `settled`
//! (terminal) and `timed_out` flags so a caller that hit the deadline can tell a
//! still-pending command from a settled one.
//!
//! Ownership is enforced through `commands.get`'s shared loader.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::runtime_db::CommandRecord;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

/// Cap on a single wait when the caller gives no `timeout_ms`. A wait must not
/// pin a connection indefinitely; the caller re-waits if it needs longer.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub command_id: i64,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    // Ownership + existence up front; also gives the current status.
    let record = super::commands_get::load_owned_command(&state, &ctx, req.command_id)?;
    if is_settled(&record.status) {
        return respond(record, false);
    }

    // Not settled yet: subscribe, THEN re-read to close the race between the read
    // above and the subscription — a settlement landing in that window is either
    // waiting on the lane (delivered by recv) or already visible in the re-read.
    let mut rx = ryeos_app::command_hub::global().subscribe(req.command_id);
    let record = super::commands_get::load_owned_command(&state, &ctx, req.command_id)?;
    if is_settled(&record.status) {
        return respond(record, false);
    }

    let timeout_ms = req.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    match tokio::time::timeout(Duration::from_millis(timeout_ms), rx.recv()).await {
        // Settlement delivered on the lane.
        Ok(Ok(settled)) => respond(settled, false),
        // Lane closed/lagged (sender dropped, or buffer overrun): fall back to a
        // fresh authoritative read of the row.
        Ok(Err(_)) => {
            let latest = super::commands_get::load_owned_command(&state, &ctx, req.command_id)?;
            respond(latest, false)
        }
        // Deadline hit: return the latest row; `settled`/`timed_out` tell the
        // caller it is still pending.
        Err(_elapsed) => {
            let latest = super::commands_get::load_owned_command(&state, &ctx, req.command_id)?;
            let timed_out = !is_settled(&latest.status);
            respond(latest, timed_out)
        }
    }
}

fn is_settled(status: &str) -> bool {
    matches!(status, "completed" | "rejected")
}

fn respond(record: CommandRecord, timed_out: bool) -> Result<Value, HandlerError> {
    let settled = is_settled(&record.status);
    let mut value =
        serde_json::to_value(&record).map_err(|e| HandlerError::Internal(e.to_string()))?;
    if let Value::Object(ref mut map) = value {
        map.insert("settled".to_string(), Value::Bool(settled));
        map.insert("timed_out".to_string(), Value::Bool(timed_out));
    }
    Ok(value)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:commands/wait",
    endpoint: "commands.wait",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.commands/wait"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settled_covers_terminal_statuses_only() {
        assert!(is_settled("completed"));
        assert!(is_settled("rejected"));
        assert!(!is_settled("pending"));
        assert!(!is_settled("claimed"));
    }

    #[test]
    fn request_defaults_timeout_to_none() {
        let req: Request = serde_json::from_value(json!({"command_id": 3})).unwrap();
        assert_eq!(req.command_id, 3);
        assert_eq!(req.timeout_ms, None);
        let req2: Request =
            serde_json::from_value(json!({"command_id": 3, "timeout_ms": 500})).unwrap();
        assert_eq!(req2.timeout_ms, Some(500));
    }

    #[test]
    fn respond_annotates_settled_and_timed_out() {
        let record = CommandRecord {
            command_id: 1,
            thread_id: "T-1".to_string(),
            command_type: "cancel".to_string(),
            status: "completed".to_string(),
            requested_by: None,
            params: None,
            result: None,
            created_at: "t0".to_string(),
            claimed_at: None,
            completed_at: Some("t2".to_string()),
        };
        let value = respond(record, false).unwrap();
        assert_eq!(value["settled"], json!(true));
        assert_eq!(value["timed_out"], json!(false));
        assert_eq!(value["status"], json!("completed"));
    }
}
