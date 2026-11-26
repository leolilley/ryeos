//! Event streams a seat opens, under the same seat read policy as every read.
//!
//! A stream is read like a service is: opening it passes [`decide`] for the
//! route's own ref (`route:` plus the route id) with the stream's request
//! parameters, and every event it carries passes the disclosure half of the
//! entry that admitted the open. An event that cannot be shaped under the
//! entry's bounds ends the stream with a refusal. Nothing is dropped or cut
//! silently, and no event reaches the seat unshaped.

use std::collections::BTreeMap;
use std::pin::Pin;

use ryeos_api::route_error::RouteDispatchError;
use ryeos_app::state::AppState;
use ryeos_app::stream_envelope::RouteStreamEnvelope;
use ryeos_node_runtime::node_policy::sections::read_disclosure::{
    ReadDisclosurePolicy, TARGET_KIND_ROUTE,
};
use serde_json::{Value, json};
use tokio_stream::{Stream, StreamExt};

use crate::browser_session::BrowserSession;
use crate::seat_read_policy::{
    DetachedShaping, RequestFacts, SeatReadRefusal, SessionFacts, decide,
};

/// The event name a stream refusal travels under.
const REFUSAL_EVENT: &str = "stream_error";

/// How a shaped event is put back on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameMode {
    /// One unnamed `message` whose payload is the shaped
    /// `{event_type, payload}` frame.
    WrapAsMessage,
    /// The event keeps its own name; only its payload is replaced by the
    /// shaped payload.
    KeepEventName,
}

/// A stream the policy admitted to open.
pub(crate) struct AdmittedStream {
    /// The request parameters after pinning.
    pub params: serde_json::Map<String, Value>,
    pub shaping: DetachedShaping,
    pub target: String,
}

/// The policy target of a stream route: the route's own id.
pub(crate) fn stream_target(route_id: &str) -> String {
    format!("{TARGET_KIND_ROUTE}:{route_id}")
}

/// Judge the opening of one stream. `parameter_schema` lists the parameters
/// the stream's request carries, so an entry can name only those.
pub(crate) fn admit_open(
    state: &AppState,
    route_id: &str,
    session: &BrowserSession,
    params: Value,
    parameter_schema: &BTreeMap<String, String>,
) -> Result<AdmittedStream, RouteDispatchError> {
    let policy = state
        .node_policy
        .require::<ReadDisclosurePolicy>()
        .map_err(|error| RouteDispatchError::Internal(format!("{error:#}")))?;
    let target = stream_target(route_id);
    let admitted = decide(
        policy,
        &target,
        parameter_schema,
        Some(RequestFacts {
            params,
            session: SessionFacts {
                principal_id: &session.principal_id,
                project_root: None,
            },
        }),
    )
    .map_err(|refusal| RouteDispatchError::Forbidden(refusal.to_string()))?;
    let (Some(Value::Object(params)), Some(shaping)) = (admitted.params, admitted.shaping) else {
        return Err(RouteDispatchError::Forbidden(
            SeatReadRefusal::NoEntry { target }.to_string(),
        ));
    };
    Ok(AdmittedStream {
        params,
        shaping: shaping.detach(),
        target,
    })
}

type EnvelopeStream =
    Pin<Box<dyn Stream<Item = Result<RouteStreamEnvelope, std::convert::Infallible>> + Send>>;

/// Pass every event of `events` through the admitted entry. The shaped value
/// is the event as a seat would see it, `{event_type, payload}`; a refusal is
/// the stream's last frame.
pub(crate) fn shape_events(
    mut events: EnvelopeStream,
    admitted: AdmittedStream,
    mode: FrameMode,
) -> EnvelopeStream {
    let AdmittedStream {
        shaping, target, ..
    } = admitted;
    Box::pin(async_stream::stream! {
        while let Some(Ok(envelope)) = events.next().await {
            match shape_event(&shaping, &target, mode, envelope) {
                Ok(shaped) => yield Ok(shaped),
                Err(refusal) => {
                    yield Ok(refusal_envelope(&refusal, mode));
                    return;
                }
            }
        }
    })
}

fn shape_event(
    shaping: &DetachedShaping,
    target: &str,
    mode: FrameMode,
    envelope: RouteStreamEnvelope,
) -> Result<RouteStreamEnvelope, SeatReadRefusal> {
    let RouteStreamEnvelope {
        id,
        event_type,
        payload,
    } = envelope;
    let shaped = shaping.apply(json!({"event_type": event_type, "payload": payload}))?;
    match mode {
        FrameMode::WrapAsMessage => Ok(RouteStreamEnvelope {
            id,
            event_type: "message".to_owned(),
            payload: shaped,
        }),
        FrameMode::KeepEventName => {
            let Some(payload @ Value::Object(_)) = shaped.get("payload").cloned() else {
                return Err(SeatReadRefusal::NothingDisclosable {
                    target: target.to_owned(),
                });
            };
            Ok(RouteStreamEnvelope {
                id,
                event_type,
                payload,
            })
        }
    }
}

/// The last frame of a stream that could not disclose an event. It carries
/// the refusal's code and text, never any part of the event.
fn refusal_envelope(refusal: &SeatReadRefusal, mode: FrameMode) -> RouteStreamEnvelope {
    let payload = json!({
        "code": refusal.code(),
        "error": refusal.to_string(),
        "retryable": false,
    });
    match mode {
        FrameMode::WrapAsMessage => RouteStreamEnvelope::new(
            "message",
            json!({"event_type": REFUSAL_EVENT, "payload": payload}),
        ),
        FrameMode::KeepEventName => RouteStreamEnvelope::new(REFUSAL_EVENT, payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_node_runtime::node_policy::sections::read_disclosure::ReadEntry;

    fn shaping(keep: Value, max_bytes: u64) -> DetachedShaping {
        let entry: ReadEntry = serde_json::from_value(json!({
            "target": "route:test/stream",
            "keep": keep,
            "max_result_bytes": max_bytes,
            "max_items": 4
        }))
        .unwrap();
        let policy: ReadDisclosurePolicy =
            serde_json::from_value(json!({"schema": 1, "reads": [entry]})).unwrap();
        policy.validate().unwrap();
        let admitted = decide(
            &policy,
            "route:test/stream",
            &BTreeMap::new(),
            Some(RequestFacts {
                params: json!({}),
                session: SessionFacts {
                    principal_id: "fp:seat",
                    project_root: None,
                },
            }),
        )
        .unwrap();
        admitted.shaping.unwrap().detach()
    }

    fn stream_of(events: Vec<RouteStreamEnvelope>) -> EnvelopeStream {
        Box::pin(tokio_stream::iter(events.into_iter().map(Ok)))
    }

    async fn run(
        shaping: DetachedShaping,
        mode: FrameMode,
        events: Vec<RouteStreamEnvelope>,
    ) -> Vec<RouteStreamEnvelope> {
        let admitted = AdmittedStream {
            params: Default::default(),
            shaping,
            target: "route:test/stream".to_owned(),
        };
        shape_events(stream_of(events), admitted, mode)
            .map(|item| item.unwrap())
            .collect()
            .await
    }

    #[tokio::test]
    async fn unknown_fields_are_dropped_from_every_event() {
        let shaping = shaping(json!(["/event_type", "/payload/a"]), 1024);
        let out = run(
            shaping,
            FrameMode::WrapAsMessage,
            vec![
                RouteStreamEnvelope::with_id("1", "x", json!({"a": 1, "secret": 2})),
                RouteStreamEnvelope::new("y", json!({"a": 3, "later_field": {"k": 1}})),
            ],
        )
        .await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id.as_deref(), Some("1"));
        assert_eq!(out[0].event_type, "message");
        assert_eq!(
            out[0].payload,
            json!({"event_type": "x", "payload": {"a": 1}})
        );
        assert_eq!(
            out[1].payload,
            json!({"event_type": "y", "payload": {"a": 3}})
        );
    }

    #[tokio::test]
    async fn an_oversize_event_ends_the_stream_with_a_refusal() {
        let shaping = shaping(json!(["/event_type", "/payload/a"]), 64);
        let big = "z".repeat(200);
        let out = run(
            shaping,
            FrameMode::WrapAsMessage,
            vec![
                RouteStreamEnvelope::new("x", json!({"a": 1})),
                RouteStreamEnvelope::new("x", json!({"a": big})),
                RouteStreamEnvelope::new("x", json!({"a": 2})),
            ],
        )
        .await;
        assert_eq!(out.len(), 2, "nothing follows the refusal");
        assert_eq!(out[0].payload["payload"], json!({"a": 1}));
        assert_eq!(out[1].payload["event_type"], "stream_error");
        assert_eq!(
            out[1].payload["payload"]["code"],
            "seat_read_result_too_large"
        );
        assert!(!out[1].payload.to_string().contains("zzzz"));
    }

    #[tokio::test]
    async fn too_many_items_in_an_event_is_a_refusal_not_a_cut() {
        let shaping = shaping(json!(["/event_type", "/payload/list"]), 4096);
        let out = run(
            shaping,
            FrameMode::KeepEventName,
            vec![RouteStreamEnvelope::new(
                "x",
                json!({"list": [1, 2, 3, 4, 5]}),
            )],
        )
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event_type, "stream_error");
        assert_eq!(out[0].payload["code"], "seat_read_too_many_items");
    }

    #[tokio::test]
    async fn a_named_event_keeps_its_name_and_loses_unkept_fields() {
        let shaping = shaping(json!(["/event_type", "/payload/kind"]), 1024);
        let out = run(
            shaping,
            FrameMode::KeepEventName,
            vec![RouteStreamEnvelope::with_id(
                "7",
                "thread.hint",
                json!({"kind": "thread", "thread_ids": ["a", "b"]}),
            )],
        )
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id.as_deref(), Some("7"));
        assert_eq!(out[0].event_type, "thread.hint");
        assert_eq!(out[0].payload, json!({"kind": "thread"}));
    }

    #[tokio::test]
    async fn a_non_object_payload_of_a_named_event_is_refused() {
        let shaping = shaping(json!(["/event_type", "/payload/kind"]), 1024);
        let out = run(
            shaping,
            FrameMode::KeepEventName,
            vec![RouteStreamEnvelope::new("x", json!("plain text"))],
        )
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event_type, "stream_error");
        assert_eq!(out[0].payload["code"], "seat_read_nothing_disclosable");
    }

    #[test]
    fn a_stream_is_named_by_its_route_id() {
        assert_eq!(stream_target("ui/events/chain"), "route:ui/events/chain");
    }
}
