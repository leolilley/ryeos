//! The two live observation streams: session hints and the focused chain
//! tail.
//!
//! Both are lenient event streams that reconnect forever with backoff; neither
//! is truth. Hints say "look again", and a tail is the braid arriving now.
//! Frames are decoded here and pushed to the inbox under the stream's tag, so
//! a stream that has been replaced can never reach the reducer.
//!
//! Freshness is reported on every transition: the first open is `current`, a
//! later open (a gap in which messages were lost) is `gap_resnapshot_required`,
//! and a failed or ended connection is `reconnecting`. A hint stream that
//! comes back, or reports `snapshot_required`, also asks the reducer to
//! refetch everything once, because the core does not refetch on its own.

use std::future::Future;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use ryeos_client_base::ui::{RyeOsTransportChannel, RyeOsTransportFreshness};
use serde_json::Value;

use crate::api::HINT_BATCH_MS;
use crate::codec::decode_strict;
use crate::inbox::{Inbox, InboxPayload, StreamTag};
use crate::platform::{Clock, Platform, Spawner, Transport};
use crate::sse::{SseMode, SseReader};

/// A running stream task. Dropping it aborts the stream.
pub type TaskOf<P> = <<P as Platform>::Spawner as Spawner>::Handle;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_millis(15_000);

enum Either<A, B> {
    Left(A),
    Right(B),
}

async fn race<A: Future, B: Future>(first: A, second: B) -> Either<A::Output, B::Output> {
    let mut first = pin!(first);
    let mut second = pin!(second);
    std::future::poll_fn(move |context| {
        if let Poll::Ready(value) = first.as_mut().poll(context) {
            return Poll::Ready(Either::Left(value));
        }
        if let Poll::Ready(value) = second.as_mut().poll(context) {
            return Poll::Ready(Either::Right(value));
        }
        Poll::Pending
    })
    .await
}

fn state(
    platform: &impl Platform,
    inbox: &Inbox,
    tag: &StreamTag,
    channel: RyeOsTransportChannel,
    freshness: RyeOsTransportFreshness,
) -> bool {
    inbox.push(
        tag.clone(),
        InboxPayload::TransportState {
            channel,
            freshness,
            observed_at_ms: platform.clock().now_ms(),
        },
    )
}

/// Open the hint stream at `path` (the session's `events_url`) and forward
/// its frames until the task is dropped.
pub fn spawn_hints<P: Platform + 'static>(
    platform: &Rc<P>,
    inbox: &Rc<Inbox>,
    tag: StreamTag,
    path: String,
) -> TaskOf<P> {
    let task_platform = platform.clone();
    let inbox = inbox.clone();
    platform.spawner().spawn(async move {
        run_hints(&*task_platform, &inbox, tag, &path).await;
    })
}

/// Open the chain tail at `path` and forward its decoded frames until the
/// task is dropped. `chain_root_id` names the chain the route was opened for.
pub fn spawn_tail<P: Platform + 'static>(
    platform: &Rc<P>,
    inbox: &Rc<Inbox>,
    tag: StreamTag,
    path: String,
    chain_root_id: String,
) -> TaskOf<P> {
    let task_platform = platform.clone();
    let inbox = inbox.clone();
    platform.spawner().spawn(async move {
        run_tail(&*task_platform, &inbox, tag, &path, &chain_root_id).await;
    })
}

async fn run_hints<P: Platform>(platform: &P, inbox: &Inbox, tag: StreamTag, path: &str) {
    let channel = RyeOsTransportChannel::Hints;
    let mut backoff = RECONNECT_MIN;
    let mut opened_before = false;
    loop {
        if let Ok(source) = platform.transport().open_sse(path).await {
            backoff = RECONNECT_MIN;
            let freshness = if opened_before {
                RyeOsTransportFreshness::GapResnapshotRequired
            } else {
                RyeOsTransportFreshness::Current
            };
            if !state(platform, inbox, &tag, channel, freshness) {
                return;
            }
            if opened_before && !inbox.push(tag.clone(), InboxPayload::Resubscribed) {
                return;
            }
            opened_before = true;
            if !read_hints(platform, inbox, &tag, source).await {
                return;
            }
        }
        if !state(
            platform,
            inbox,
            &tag,
            channel,
            RyeOsTransportFreshness::Reconnecting,
        ) {
            return;
        }
        platform.clock().sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

enum HintStep {
    Frame(Result<Option<crate::sse::SseFrame>, crate::sse::StreamError>),
    BatchDue,
}

async fn next_hint_step<'a, S: crate::platform::ByteSource>(
    reader: &mut SseReader<S>,
    timer: &mut Option<Pin<Box<dyn Future<Output = ()> + 'a>>>,
) -> HintStep {
    match timer {
        None => HintStep::Frame(reader.next_frame().await),
        Some(timer) => match race(reader.next_frame(), timer.as_mut()).await {
            Either::Left(frame) => HintStep::Frame(frame),
            Either::Right(()) => HintStep::BatchDue,
        },
    }
}

/// Read one connection of the hint stream. Each hint is forwarded at once and
/// its kind joins a set; the first hint of a window arms a timer, and when it
/// fires the set is flushed as one batch in first-seen order. Returns false
/// when the stream's tag went stale and the task should end.
async fn read_hints<P: Platform>(
    platform: &P,
    inbox: &Inbox,
    tag: &StreamTag,
    source: <P::Transport as Transport>::Sse,
) -> bool {
    let mut reader = SseReader::new(source, SseMode::Lenient);
    let mut dirty: Vec<String> = Vec::new();
    let mut timer: Option<Pin<Box<dyn Future<Output = ()> + '_>>> = None;
    loop {
        let frame = match next_hint_step(&mut reader, &mut timer).await {
            HintStep::BatchDue => {
                timer = None;
                let kinds = std::mem::take(&mut dirty);
                if !kinds.is_empty() && !inbox.push(tag.clone(), InboxPayload::HintFlush(kinds)) {
                    return false;
                }
                continue;
            }
            HintStep::Frame(Ok(Some(frame))) => frame,
            HintStep::Frame(_) => return true,
        };
        match frame.event.as_str() {
            "snapshot_required" => {
                dirty.clear();
                timer = None;
                let freshness = RyeOsTransportFreshness::GapResnapshotRequired;
                if !state(
                    platform,
                    inbox,
                    tag,
                    RyeOsTransportChannel::Hints,
                    freshness,
                ) || !inbox.push(tag.clone(), InboxPayload::Resubscribed)
                {
                    return false;
                }
            }
            // Chosen behaviour: `message` frames are forwarded like the web does,
            // and an undecodable payload is skipped rather than dispatched as null.
            "ui_intent.applied" | "message" => {
                if let Ok(payload) = decode_strict(&frame.data)
                    && !inbox.push(tag.clone(), InboxPayload::DaemonEvent(payload))
                {
                    return false;
                }
            }
            "thread.hint" => {
                let Ok(payload) = decode_strict(&frame.data) else {
                    continue;
                };
                let Some(kind) = payload
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    continue;
                };
                if !inbox.push(tag.clone(), InboxPayload::Hint(payload)) {
                    return false;
                }
                if !dirty.contains(&kind) {
                    dirty.push(kind);
                }
                if timer.is_none() {
                    timer = Some(Box::pin(
                        platform.clock().sleep(Duration::from_millis(HINT_BATCH_MS)),
                    ));
                }
            }
            _ => {}
        }
    }
}

async fn run_tail<P: Platform>(
    platform: &P,
    inbox: &Inbox,
    tag: StreamTag,
    path: &str,
    chain_root_id: &str,
) {
    let channel = RyeOsTransportChannel::FocusedTail;
    let mut backoff = RECONNECT_MIN;
    let mut opened_before = false;
    loop {
        if let Ok(source) = platform.transport().open_sse(path).await {
            backoff = RECONNECT_MIN;
            let freshness = if opened_before {
                RyeOsTransportFreshness::GapResnapshotRequired
            } else {
                RyeOsTransportFreshness::Current
            };
            opened_before = true;
            if !state(platform, inbox, &tag, channel, freshness) {
                return;
            }
            let mut reader = SseReader::new(source, SseMode::Lenient);
            while let Ok(Some(frame)) = reader.next_frame().await {
                // A frame that is not JSON is not a tail record; skip it.
                let Ok(frame) = decode_strict(&frame.data) else {
                    continue;
                };
                let message = InboxPayload::ThreadTail {
                    chain_root_id: chain_root_id.to_owned(),
                    frame,
                };
                if !inbox.push(tag.clone(), message) {
                    return;
                }
            }
        }
        if !state(
            platform,
            inbox,
            &tag,
            channel,
            RyeOsTransportFreshness::Reconnecting,
        ) {
            return;
        }
        platform.clock().sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

/// A chain stream is wider than the currently focused route thread. Accept a
/// frame only when it is a `{event_type, payload}` envelope whose inner record
/// belongs to exactly this chain and thread and repeats the envelope's event
/// type; return that type and the inner record. A frame that arrives after
/// the route moved to another thread of the same chain is dropped, never
/// relabelled with the new route's identity.
pub fn focused_tail_payload(
    frame: &Value,
    chain_root_id: Option<&str>,
    thread_id: Option<&str>,
) -> Option<(String, Value)> {
    let chain_root_id = chain_root_id.filter(|chain| !chain.is_empty())?;
    let thread_id = thread_id.filter(|thread| !thread.is_empty())?;
    let envelope = frame.as_object()?;
    let event_type = envelope.get("event_type")?.as_str()?;
    let record = envelope.get("payload")?.as_object()?;
    if record.get("chain_root_id").and_then(Value::as_str) != Some(chain_root_id)
        || record.get("thread_id").and_then(Value::as_str) != Some(thread_id)
        || record.get("event_type").and_then(Value::as_str) != Some(event_type)
    {
        return None;
    }
    Some((
        event_type.to_owned(),
        envelope.get("payload").cloned().unwrap_or(Value::Null),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tail_frames_must_match_chain_thread_and_type() {
        let frame = json!({
            "event_type": "cognition_out",
            "payload": {"chain_root_id": "c", "thread_id": "t", "event_type": "cognition_out"}
        });
        assert!(focused_tail_payload(&frame, Some("c"), Some("t")).is_some());
        assert!(focused_tail_payload(&frame, Some("c"), Some("other")).is_none());
        assert!(focused_tail_payload(&frame, Some("other"), Some("t")).is_none());
        assert!(focused_tail_payload(&frame, None, Some("t")).is_none());
        assert!(focused_tail_payload(&frame, Some("c"), Some("")).is_none());
        assert!(focused_tail_payload(&json!([]), Some("c"), Some("t")).is_none());
    }
}
