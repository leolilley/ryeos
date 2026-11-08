//! Hint batching, freshness reporting and reconnects, on a virtual clock.

mod common;

use std::rc::Rc;

use common::sim::{Chunk, FakePlatform, chunk_queue};
use ryeos_client_base::ui::{RyeOsTransportChannel, RyeOsTransportFreshness};
use ryeos_client_runtime::inbox::{Inbox, InboxPayload, Stream, StreamTag};
use ryeos_client_runtime::streams::{spawn_hints, spawn_tail};

fn hint(kind: &str) -> Vec<u8> {
    format!("event: thread.hint\ndata: {{\"kind\":\"{kind}\"}}\n\n").into_bytes()
}

fn inbox_for(stream: Stream) -> (Rc<Inbox>, StreamTag) {
    let inbox = Rc::new(Inbox::new(Rc::new(|| {})));
    let tag = StreamTag {
        producer: Some("p".to_owned()),
        generation: 1,
    };
    inbox.set_current(stream, tag.clone());
    (inbox, tag)
}

fn label(payload: &InboxPayload) -> String {
    match payload {
        InboxPayload::Hint(value) => format!("hint:{}", value["kind"].as_str().unwrap()),
        InboxPayload::HintFlush(kinds) => format!("flush:{}", kinds.join(",")),
        InboxPayload::Resubscribed => "resubscribed".to_owned(),
        InboxPayload::DaemonEvent(_) => "daemon".to_owned(),
        InboxPayload::ThreadTail { chain_root_id, .. } => format!("tail:{chain_root_id}"),
        InboxPayload::TransportState {
            channel, freshness, ..
        } => format!(
            "state:{}:{}",
            match channel {
                RyeOsTransportChannel::Hints => "hints",
                RyeOsTransportChannel::FocusedTail => "focused_tail",
                RyeOsTransportChannel::Session => "session",
            },
            match freshness {
                RyeOsTransportFreshness::Current => "current",
                RyeOsTransportFreshness::Reconnecting => "reconnecting",
                RyeOsTransportFreshness::GapResnapshotRequired => "gap",
                RyeOsTransportFreshness::Connecting => "connecting",
                RyeOsTransportFreshness::ExpiredOrRevoked => "expired",
            }
        ),
        other => format!("{other:?}"),
    }
}

fn drained(inbox: &Inbox) -> Vec<String> {
    inbox.drain().iter().map(|m| label(&m.payload)).collect()
}

#[test]
fn hints_forward_at_once_and_flush_once_per_window_in_first_seen_order() {
    let platform = FakePlatform::new();
    let (inbox, tag) = inbox_for(Stream::Hints);
    let queue = chunk_queue([
        Chunk::Bytes(hint("a")),
        Chunk::Wait(100),
        Chunk::Bytes(hint("b")),
        Chunk::Wait(100),
        Chunk::Bytes(hint("a")),
    ]);
    platform
        .transport
        .push_stream("/ui/events/session/s", queue, true);
    let _task = spawn_hints(&platform, &inbox, tag, "/ui/events/session/s".to_owned());
    let sim = platform.sim();

    sim.run_for(499);
    assert_eq!(
        drained(&inbox),
        ["state:hints:current", "hint:a", "hint:b", "hint:a"]
    );
    sim.run_for(2);
    assert_eq!(drained(&inbox), ["flush:a,b"]);
    // No further hints: no further flush.
    sim.run_for(2_000);
    assert!(drained(&inbox).is_empty());
}

#[test]
fn a_reconnect_reports_the_gap_and_asks_for_one_refetch() {
    let platform = FakePlatform::new();
    let (inbox, tag) = inbox_for(Stream::Hints);
    platform.transport.push_stream(
        "/ui/events/session/s",
        chunk_queue([Chunk::Bytes(hint("a"))]),
        false,
    );
    platform
        .transport
        .push_stream("/ui/events/session/s", chunk_queue([]), true);
    let _task = spawn_hints(&platform, &inbox, tag, "/ui/events/session/s".to_owned());
    let sim = platform.sim();

    sim.run_for(1_000);
    assert_eq!(
        drained(&inbox),
        [
            "state:hints:current",
            "hint:a",
            "state:hints:reconnecting",
            "state:hints:gap",
            "resubscribed",
        ]
    );
    assert_eq!(*platform.clock.sleeps.borrow().first().unwrap(), 500);
}

#[test]
fn snapshot_required_is_a_gap_and_a_refetch_without_reconnecting() {
    let platform = FakePlatform::new();
    let (inbox, tag) = inbox_for(Stream::Hints);
    let frames = b"event: snapshot_required\ndata: {}\n\n".to_vec();
    platform.transport.push_stream(
        "/ui/events/session/s",
        chunk_queue([Chunk::Bytes(frames)]),
        true,
    );
    let _task = spawn_hints(&platform, &inbox, tag, "/ui/events/session/s".to_owned());
    platform.sim().run_for(10);
    assert_eq!(
        drained(&inbox),
        ["state:hints:current", "state:hints:gap", "resubscribed"]
    );
}

#[test]
fn a_replaced_stream_cannot_reach_the_reducer() {
    let platform = FakePlatform::new();
    let (inbox, tag) = inbox_for(Stream::Hints);
    platform.transport.push_stream(
        "/ui/events/session/s",
        chunk_queue([Chunk::Wait(50), Chunk::Bytes(hint("late"))]),
        true,
    );
    let _task = spawn_hints(&platform, &inbox, tag, "/ui/events/session/s".to_owned());
    let sim = platform.sim();
    sim.run_for(10);
    inbox.set_current(
        Stream::Hints,
        StreamTag {
            producer: Some("p".to_owned()),
            generation: 2,
        },
    );
    sim.run_for(100);
    assert!(drained(&inbox).is_empty());
}

#[test]
fn the_tail_forwards_decoded_frames_for_its_chain() {
    let platform = FakePlatform::new();
    let (inbox, tag) = inbox_for(Stream::FocusedTail);
    let frame = b"data: {\"event_type\":\"x\",\"payload\":{}}\n\n".to_vec();
    platform.transport.push_stream(
        "/ui/events/chain/c1",
        chunk_queue([Chunk::Bytes(frame)]),
        true,
    );
    let _task = spawn_tail(
        &platform,
        &inbox,
        tag,
        "/ui/events/chain/c1".to_owned(),
        "c1".to_owned(),
    );
    platform.sim().run_for(10);
    assert_eq!(drained(&inbox), ["state:focused_tail:current", "tail:c1"]);
}
