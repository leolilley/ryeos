//! Replays `seat-history.json` against the history consumer.
//!
//! The cases were generated from the browser's stream consumer. A case whose
//! `behaviour` exercises JavaScript callback aliasing (a callback rewriting
//! its argument, aborting an `AbortSignal`, throwing from an invalidation
//! handler) has no counterpart in Rust, where the consumer owns its state and
//! cancellation is dropping the future; those are skipped by name.

mod common;

use common::sim::{Chunk, FakePlatform, chunk_queue, poll_once};
use common::{load, name_of};
use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::history::{
    HistoryClaim, HistoryError, HistorySink, LiveEvent, SeatCoordinate, read_history,
};
use ryeos_client_runtime::platform::{Clock, Platform};
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Default)]
struct Recorder {
    order: Vec<String>,
    stage_calls: Vec<usize>,
    transfer: usize,
    cancel: usize,
    transfer_throws: bool,
    cancel_throws: bool,
}

impl HistorySink for Recorder {
    fn stage_events(&mut self, records: &[Value]) -> Result<(), String> {
        self.stage_calls.push(records.len());
        self.order.push(format!("stage:{}", records.len()));
        Ok(())
    }

    fn stage_transfer_proof(&mut self, _proof: &Value) -> Result<(), String> {
        if self.transfer_throws {
            return Err("shared proof refused".to_owned());
        }
        self.transfer += 1;
        self.order.push("transfer".to_owned());
        Ok(())
    }

    fn stage_cancellation_proof(&mut self, _proof: &Value) -> Result<(), String> {
        if self.cancel_throws {
            return Err("negative proof refused".to_owned());
        }
        self.cancel += 1;
        self.order.push("cancel".to_owned());
        Ok(())
    }
}

fn skipped(case: &Value) -> Option<&'static str> {
    let behaviour = case["input"].get("behaviour")?.as_object()?;
    for key in behaviour.keys() {
        match key.as_str() {
            "transfer_stage_throws" | "cancel_stage_throws" | "verified_unexpected" => {}
            _ => return Some("JavaScript callback aliasing or abort-signal behaviour"),
        }
    }
    None
}

fn chunks_of(input: &Value) -> Vec<Chunk> {
    let padding = input.get("padding");
    let text_of = |raw: &str| {
        let mut text = raw.to_owned();
        if let Some(padding) = padding {
            let character = padding["char"].as_str().expect("padding char");
            let count = padding["count"].as_u64().expect("padding count") as usize;
            text = text.replace("{PADDING}", &character.repeat(count));
        }
        text
    };
    // The browser client never checks `chain_seq`, so its fixtures repeat one
    // value on every event. The daemon emits it strictly increasing and the
    // runtime enforces that, so rewrite each token to a running counter in
    // stream order; no browser case is about `chain_seq` ordering.
    let mut next_chain_seq = 1u64;
    let all: Vec<String> = input["chunks"]
        .as_array()
        .expect("chunks")
        .iter()
        .map(|chunk| {
            renumber_chain_seq(
                &text_of(chunk.as_str().expect("chunk text")),
                &mut next_chain_seq,
            )
        })
        .collect();
    if input["split_every_char"].as_bool() == Some(true) {
        all.iter()
            .flat_map(|text| text.chars())
            .map(|c| Chunk::Bytes(c.to_string().into_bytes()))
            .collect()
    } else {
        all.into_iter()
            .map(|text| Chunk::Bytes(text.into_bytes()))
            .collect()
    }
}

fn renumber_chain_seq(text: &str, next: &mut u64) -> String {
    const TOKEN: &str = "\"chain_seq\":";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(TOKEN) {
        let after = &rest[at + TOKEN.len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            out.push_str(&rest[..at + TOKEN.len()]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..at + TOKEN.len()]);
        out.push_str(&next.to_string());
        *next += 1;
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

#[test]
fn seat_history_stream_vectors() {
    let mut checked = 0;
    for case in load("seat-history") {
        let name = name_of(&case).to_owned();
        if skipped(&case).is_some() {
            continue;
        }
        let input = &case["input"];
        let expected = &case["expected"];
        let coordinate = SeatCoordinate {
            presentation_id: input["coordinate"]["presentation_id"]
                .as_str()
                .unwrap()
                .to_owned(),
            chain_root_id: input["coordinate"]["chain_root_id"]
                .as_str()
                .unwrap()
                .to_owned(),
            thread_id: input["coordinate"]["thread_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        let claim = HistoryClaim {
            operation_id: input["claim"]["operation_id"].as_str().unwrap().to_owned(),
            producer_incarnation: input["claim"]["producer_incarnation"]
                .as_str()
                .unwrap()
                .to_owned(),
            next_engine_seq: input["claim"]["next_engine_seq"].as_u64().unwrap(),
            request: None,
        };
        let keep_open = input["keep_open"].as_bool() == Some(true);

        let platform = FakePlatform::new();
        let queue = chunk_queue(chunks_of(input));
        match input["content_type"].as_str() {
            Some(content_type) if content_type != "text/event-stream" => {
                platform.transport.push_history(Err(RuntimeError::Fatal(
                    "seat history did not return an event stream".to_owned(),
                )));
            }
            _ => platform
                .transport
                .push_history(Ok((queue.clone(), keep_open))),
        }
        let mut sink = Recorder::default();
        if let Some(behaviour) = input["behaviour"].as_object() {
            sink.transfer_throws = behaviour.contains_key("transfer_stage_throws");
            sink.cancel_throws = behaviour.contains_key("cancel_stage_throws");
        }
        let sim = platform.sim();
        let outcome = sim.run(read_history(
            &platform.transport,
            &coordinate,
            &claim,
            &mut sink,
        ));

        // The request is exactly the coordinate pair.
        let calls = platform.transport.calls.borrow().clone();
        assert_eq!(calls.len(), 1, "{name}");
        assert_eq!(calls[0].0, "/ui/api/session/seat/history", "{name}");
        let body: Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(body, expected["requests"][0]["body"], "{name}");

        match expected["outcome"].as_str().unwrap() {
            "reject" => {
                let error = match outcome {
                    Ok(_) => panic!("{name}: expected a rejection"),
                    Err(error) => error,
                };
                assert_eq!(
                    error.to_string(),
                    expected["error_message"].as_str().unwrap(),
                    "{name}"
                );
                assert_eq!(
                    matches!(error, HistoryError::Incomplete(_)),
                    expected["is_incomplete"].as_bool() == Some(true),
                    "{name}"
                );
            }
            _ => {
                let mut live = match outcome {
                    Ok(live) => live,
                    Err(error) => panic!("{name}: {error}"),
                };
                let completion = live.completion().clone();
                assert_eq!(
                    completion.tip_hash,
                    expected["completion"]["tip_hash"].as_str().unwrap(),
                    "{name}"
                );
                assert_eq!(
                    completion.thread_seq.to_string(),
                    expected["completion"]["thread_seq"].as_str().unwrap(),
                    "{name}"
                );
                assert_eq!(
                    completion.event_count.to_string(),
                    expected["completion"]["event_count"].as_str().unwrap(),
                    "{name}"
                );

                // Whatever follows completion arrives as live events.
                if let Some(after) = input["after_handoff_chunks"].as_array() {
                    let mut pending: VecDeque<Chunk> = after
                        .iter()
                        .map(|text| Chunk::Bytes(text.as_str().unwrap().as_bytes().to_vec()))
                        .collect();
                    queue.borrow_mut().append(&mut pending);
                }
                let mut verified = 0;
                let mut invalidations: Vec<String> = Vec::new();
                while let Some(event) = poll_once(live.next()) {
                    match event {
                        LiveEvent::Verified(append) => {
                            verified += 1;
                            sink.order.push(format!("verified:{}", append.thread_seq));
                        }
                        LiveEvent::ReproofRequired => {
                            invalidations
                                .push("seat history requires a fresh paired proof".to_owned());
                            sink.order.push("invalidated".to_owned());
                            break;
                        }
                        LiveEvent::Invalidated(reason) => {
                            invalidations.push(reason);
                            sink.order.push("invalidated".to_owned());
                            break;
                        }
                    }
                }
                let wanted: Vec<String> = expected["invalidations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|reason| reason.as_str().unwrap().to_owned())
                    .collect();
                assert_eq!(invalidations, wanted, "{name}");
                assert_eq!(
                    verified,
                    expected["verified"].as_array().unwrap().len(),
                    "{name}"
                );
            }
        }
        let wanted_stages: Vec<usize> = expected["stage_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap() as usize)
            .collect();
        assert_eq!(sink.stage_calls, wanted_stages, "{name}");
        assert_eq!(
            sink.transfer,
            expected["transfer_proofs"].as_array().unwrap().len(),
            "{name}"
        );
        assert_eq!(
            sink.cancel,
            expected["cancel_proofs"].as_array().unwrap().len(),
            "{name}"
        );
        let wanted_order: Vec<&str> = expected["order"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry.as_str().unwrap())
            .collect();
        assert_eq!(sink.order, wanted_order, "{name}");
        checked += 1;
    }
    assert!(checked > 60, "only {checked} history cases ran");
}

#[test]
fn a_transport_failure_before_the_fence_is_an_incomplete_proof() {
    let platform = FakePlatform::new();
    let queue = chunk_queue([Chunk::Fail]);
    platform.transport.push_history(Ok((queue, false)));
    let coordinate = SeatCoordinate {
        presentation_id: "P".into(),
        chain_root_id: "R".into(),
        thread_id: "T".into(),
    };
    let claim = HistoryClaim {
        operation_id: "o".into(),
        producer_incarnation: "p".into(),
        next_engine_seq: 0,
        request: None,
    };
    let mut sink = Recorder::default();
    let outcome = platform.sim().run(read_history(
        &platform.transport,
        &coordinate,
        &claim,
        &mut sink,
    ));
    assert!(matches!(outcome, Err(HistoryError::Incomplete(_))));
    let _ = platform.clock().now_ms();
}
