//! `ui.ryeos.timeline.raw` — the raw durable events of one `chain_seq` range
//! of a chain, on demand. The timeline service returns folded items that each
//! carry the range of events they came from; a client that wants the events
//! behind one item asks for exactly that range here.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const SERVICE_REF: &str = "service:ui/ryeos-ui/timeline/raw";
const ENDPOINT: &str = "ui.ryeos.timeline.raw";
/// The most events one call returns; a longer range is cut and flagged.
const MAX_EVENTS: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequest {
    chain_root_id: String,
    from_chain_seq: Value,
    to_chain_seq: Value,
}

/// A `chain_seq` bound: a positive integer, or a numeric string of one.
fn parse_seq(name: &str, raw: &Value) -> Result<i64, HandlerError> {
    let invalid = || HandlerError::BadRequest(format!("{name} must be a positive integer"));
    let parsed = match raw {
        Value::Number(number) => number.as_i64().ok_or_else(invalid)?,
        Value::String(text) => text.trim().parse::<i64>().map_err(|_| invalid())?,
        _ => return Err(invalid()),
    };
    if parsed < 1 {
        return Err(invalid());
    }
    Ok(parsed)
}

/// The inclusive range requested, validated as ordered.
fn parse_range(from: &Value, to: &Value) -> Result<(i64, i64), HandlerError> {
    let from = parse_seq("from_chain_seq", from)?;
    let to = parse_seq("to_chain_seq", to)?;
    if to < from {
        return Err(HandlerError::BadRequest(
            "to_chain_seq must not precede from_chain_seq".to_string(),
        ));
    }
    Ok((from, to))
}

/// The events of `from..=to` in ascending `chain_seq`, at most
/// [`MAX_EVENTS`], and whether the range held more than were returned.
fn events_in_range(
    state: &AppState,
    chain_root_id: &str,
    from: i64,
    to: i64,
) -> Result<(Vec<Value>, bool)> {
    let page = state.events.replay(&EventReplayParams {
        thread_id: None,
        chain_root_id: Some(chain_root_id.to_string()),
        after_chain_seq: Some(from - 1),
        limit: MAX_EVENTS,
        event_types: None,
    })?;
    let mut events = Vec::new();
    for record in &page.events {
        if record.chain_seq > to {
            return Ok((events, false));
        }
        events.push(serde_json::to_value(record)?);
    }
    let last = page.events.last().map(|record| record.chain_seq);
    let truncated = page.next_cursor.is_some() && last.is_some_and(|seq| seq < to);
    Ok((events, truncated))
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let request: RawRequest = serde_json::from_value(params).map_err(|error| {
        HandlerError::BadRequest(format!("invalid timeline raw request: {error}"))
    })?;
    let chain_root_id = request.chain_root_id.trim().to_string();
    if chain_root_id.is_empty() {
        return Err(HandlerError::BadRequest("chain_root_id is required".to_string()).into());
    }
    let (from, to) = parse_range(&request.from_chain_seq, &request.to_chain_seq)?;

    // The caller must own the chain's root thread, judged by the same
    // exact-thread subject authority the folded timeline read uses.
    let subject = crate::thread_authorization::authorize_exact_thread_subjects(
        &ctx,
        &state,
        &caller,
        &[&chain_root_id],
    )?
    .pop()
    .expect("one exact thread authorization returns one signed subject");
    if subject.chain_root_id != chain_root_id {
        return Err(HandlerError::NotFound.into());
    }

    let (events, truncated) = events_in_range(&state, &chain_root_id, from, to)?;
    Ok(json!({
        "chain_root_id": chain_root_id,
        "from_chain_seq": from,
        "to_chain_seq": to,
        "truncated": truncated,
        "events": events,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: SERVICE_REF,
    endpoint: ENDPOINT,
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_accept_numbers_and_numeric_strings() {
        assert_eq!(parse_range(&json!(3), &json!(9)).unwrap(), (3, 9));
        assert_eq!(parse_range(&json!("3"), &json!(" 9 ")).unwrap(), (3, 9));
        assert_eq!(parse_range(&json!(4), &json!(4)).unwrap(), (4, 4));
    }

    #[test]
    fn bounds_refuse_malformed_and_unordered_ranges() {
        for (from, to) in [
            (json!(0), json!(5)),
            (json!(-1), json!(5)),
            (json!(1.5), json!(5)),
            (json!(""), json!(5)),
            (json!(true), json!(5)),
            (json!(5), json!(4)),
            (json!(1), Value::Null),
        ] {
            assert!(
                parse_range(&from, &to).is_err(),
                "{from} .. {to} should be refused"
            );
        }
    }

    #[test]
    fn request_refuses_unknown_fields() {
        let bad = json!({"chain_root_id": "c", "from_chain_seq": 1, "to_chain_seq": 2, "x": 1});
        assert!(serde_json::from_value::<RawRequest>(bad).is_err());
    }
}
