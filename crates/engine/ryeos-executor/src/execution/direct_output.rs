//! Output interpretation for the ordinary admitted subprocess plan.
//! Do not launch processes here or reconstruct framing from lossy completion
//! strings. Lillux retains the sole pipe drainer, deadline and reap owner.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ryeos_app::{
    state::AppState,
    state_store::{NewEventRecord, PersistedEventRecord},
    temp_dir_guard::TempDirGuard,
    thread_lifecycle::RunningItem,
};
use ryeos_engine::{
    contracts::{ExecutionCompletion, ThreadTerminalStatus},
    protocol_vocabulary::{StdoutShape, StreamingChunk, StreamingFrameReader},
};
use serde_json::{Value, json};

/// Observe and settle a released process without taking ownership from its
/// ordinary launch path. Recovery calls the same function with its retained
/// protocol shape and fresh launch-owner coordinate.
pub(super) async fn wait(
    process: RunningItem,
    shape: StdoutShape,
    state: AppState,
    chain_root_id: String,
    thread_id: String,
    launch_owner: String,
    workspace_lifeline: Option<Arc<TempDirGuard>>,
) -> Result<ExecutionCompletion> {
    // Exactly one blocking job owns process settlement. Lillux scopes its byte
    // observer internally so a saturated pool cannot queue the wait behind a
    // decoder which is itself waiting for that process's deadline/EOF.
    tokio::task::spawn_blocking(move || {
        let _workspace_lifeline = workspace_lifeline;
        if shape != StdoutShape::StreamingChunks {
            return process.wait();
        }
        let (mut completion, observed) = process.wait_with_stdout(|reader| {
            observe(reader, &launch_owner, |events| {
                state
                    .threads
                    .append_thread_events_owned(&chain_root_id, &thread_id, &launch_owner, events)?
                    .context("thread stopped before stdout frame publication")
            })
        });
        settle(&mut completion, observed.map_err(observation_failure));
        completion
    })
    .await
    .context("subprocess wait task failed")
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct ObserverFailure(&'static str);

fn observation_failure(failure: lillux::ProcessObservationError<anyhow::Error>) -> anyhow::Error {
    use lillux::ProcessObservationError;
    ObserverFailure(match failure {
        ProcessObservationError::Observation(error) => return error,
        ProcessObservationError::AlreadyConsumed => "stdout_observer_already_consumed",
        ProcessObservationError::Start(_) => "stdout_observer_start_failed",
        ProcessObservationError::Panicked => "stdout_observer_panicked",
    })
    .into()
}

fn frame_events(frame: &StreamingChunk, launch_owner: &str) -> Result<Vec<NewEventRecord>> {
    let frame_digest = ryeos_state::objects::canonical_value_digest(&serde_json::to_value(frame)?)?;
    let payloads = ryeos_runtime::encode_bounded_text_payloads(
        frame.data.as_deref().unwrap_or(""),
        |part, parts, data| {
            json!({
                "schema_version": 1,
                "launch_owner": launch_owner,
                "frame_seq": frame.seq,
                "frame_digest": frame_digest,
                "kind": frame.kind,
                "exit_code": frame.exit_code,
                "terminal": frame.terminal,
                "part": part,
                "parts": parts,
                "data": frame.data.as_ref().map(|_| data),
            })
        },
    )?;
    Ok(payloads
        .into_iter()
        .map(|payload| NewEventRecord {
            event_type: ryeos_state::event_types::SUBPROCESS_OUTPUT_OBSERVED.into(),
            storage_class: "indexed".into(),
            payload,
        })
        .collect())
}

#[derive(Debug)]
struct StreamSummary {
    frames: u64,
    data_bytes: u64,
    exit_code: i32,
    last_event: PersistedEventRecord,
}

fn observe(
    reader: impl std::io::Read,
    launch_owner: &str,
    mut publish: impl FnMut(&[NewEventRecord]) -> Result<Vec<PersistedEventRecord>>,
) -> Result<StreamSummary> {
    let mut frames = 0;
    let mut data_bytes = 0;
    let mut terminal = None;
    for frame in StreamingFrameReader::new(reader) {
        let frame = frame?;
        let events = frame_events(&frame, launch_owner)?;
        // One bounded logical frame is one existing atomic append. Publishing
        // pieces independently would expose incomplete frames after a crash.
        let mut records = publish(&events)?;
        if records.len() != events.len() {
            bail!("stdout publication count mismatch");
        }
        let last_event = records.pop().context("stdout publication has no event")?;
        if last_event.event_hash.is_none() {
            bail!("stdout publication is not durable");
        }
        frames += 1;
        data_bytes += frame.data.as_ref().map_or(0, |data| data.len() as u64);
        if frame.terminal {
            terminal = Some((
                frame.exit_code.context("terminal frame lacks exit code")?,
                last_event,
            ));
        }
    }
    let (exit_code, last_event) = terminal.context("stdout stream lacks terminal frame")?;
    Ok(StreamSummary {
        frames,
        data_bytes,
        exit_code,
        last_event,
    })
}

fn settle(completion: &mut ExecutionCompletion, observed: Result<StreamSummary>) {
    match observed {
        Err(failure) => {
            // Do not persist arbitrary malformed frame text as an unbounded
            // diagnostic. Already committed output stays in the event chain.
            completion.result = None;
            if completion.status == ThreadTerminalStatus::Completed {
                completion.status = ThreadTerminalStatus::Failed;
                completion.outcome_code = Some("stdout_protocol_failed".into());
            }
            let error = completion.error.get_or_insert_with(|| json!({}));
            if let Some(error) = error.as_object_mut() {
                let category = if let Some(frame) =
                    failure.downcast_ref::<ryeos_engine::protocol_vocabulary::FrameReadError>()
                {
                    frame.code()
                } else if let Some(observer) = failure.downcast_ref::<ObserverFailure>() {
                    observer.0
                } else {
                    "stdout_publication_failed"
                };
                error.insert(
                    "stdout_protocol_failure".into(),
                    Value::String(category.into()),
                );
            }
        }
        Ok(summary) => {
            // No raw frames in the terminal result: the bounded signed event
            // braid owns them, including streams larger than the result ceiling.
            completion.result = Some(json!({"streaming": {
                "frame_count": summary.frames,
                "data_bytes": summary.data_bytes,
                "exit_code": summary.exit_code,
                "last_observation": {
                    "chain_root_id": summary.last_event.chain_root_id,
                    "thread_id": summary.last_event.thread_id,
                    "chain_seq": summary.last_event.chain_seq,
                    "thread_seq": summary.last_event.thread_seq,
                    "event_hash": summary.last_event.event_hash,
                },
            }}));
            if completion.status == ThreadTerminalStatus::Completed && summary.exit_code != 0 {
                completion.status = ThreadTerminalStatus::Failed;
                completion.outcome_code = Some("stdout_reported_failure".into());
                completion.error = Some(json!({"exit_code": summary.exit_code}));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::protocol_vocabulary::StreamingChunkKind;

    #[cfg(target_os = "linux")]
    #[test]
    fn one_blocking_slot_does_not_starve_silent_process_supervision() {
        // Low-level host process fixture only; no admitted Tool host fallback.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (completion, observation) = tokio::task::spawn_blocking(|| {
                let process = lillux::spawn(lillux::SubprocessRequest {
                    cmd: "/bin/sleep".into(),
                    argv0: None,
                    args: vec!["30".into()],
                    cwd: None,
                    envs: vec![],
                    stdin_data: None,
                    timeout: 0.1,
                    limits: None,
                    inherited_fds: vec![],
                    inherited_fd_mappings: vec![],
                    supervised_status: None,
                })
                .unwrap();
                process.wait_with_stdout(|reader| {
                    observe(reader, "owner", |_| {
                        panic!("silent process cannot publish frames")
                    })
                })
            })
            .await
            .unwrap();
            assert!(completion.timed_out);
            assert!(matches!(
                observation,
                Err(lillux::ProcessObservationError::Observation(_))
            ));
        });
    }

    fn chunk(data: &str) -> StreamingChunk {
        StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Stdout,
            data: Some(data.into()),
            exit_code: None,
            terminal: false,
        }
    }

    fn record(payload: Value) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: 1,
            event_hash: Some("a".repeat(64)),
            chain_root_id: "root".into(),
            chain_seq: 2,
            thread_id: "thread".into(),
            thread_seq: 2,
            event_type: ryeos_state::event_types::SUBPROCESS_OUTPUT_OBSERVED.into(),
            storage_class: "indexed".into(),
            ts: "2026-09-06T00:00:00Z".into(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload,
        }
    }

    fn encode(frames: &[StreamingChunk]) -> Vec<u8> {
        let mut bytes = vec![];
        for frame in frames {
            let body = serde_json::to_vec(frame).unwrap();
            bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
            bytes.extend(body);
        }
        bytes
    }

    fn terminal() -> StreamingChunk {
        StreamingChunk {
            seq: 1,
            kind: StreamingChunkKind::Exit,
            data: None,
            exit_code: Some(0),
            terminal: true,
        }
    }

    #[test]
    fn escaped_multibyte_frame_is_exact_bounded_atomic_projection() {
        let data = "\"\\\n🙂".repeat(70_000);
        let frame = chunk(&data);
        let events = frame_events(&frame, "launch").unwrap();
        assert!(events.len() > 1);
        let mut recovered = String::new();
        for (index, event) in events.iter().enumerate() {
            assert!(
                serde_json::to_vec(&event.payload).unwrap().len()
                    <= ryeos_runtime::MAX_RUNTIME_EVENT_PAYLOAD_BYTES
            );
            assert_eq!(event.payload["part"], index);
            assert_eq!(event.payload["parts"], events.len());
            assert_eq!(event.payload["launch_owner"], "launch");
            recovered.push_str(event.payload["data"].as_str().unwrap());
        }
        assert_eq!(recovered, data);
        let expected =
            ryeos_state::objects::canonical_value_digest(&serde_json::to_value(frame).unwrap())
                .unwrap();
        assert_eq!(events[0].payload["frame_digest"], expected);
        let mut calls = 0;
        let summary = observe(
            encode(&[chunk(&data), terminal()]).as_slice(),
            "launch",
            |batch| {
                calls += 1;
                if calls == 1 {
                    assert_eq!(batch.len(), events.len());
                }
                Ok(batch
                    .iter()
                    .map(|event| record(event.payload.clone()))
                    .collect())
            },
        )
        .unwrap();
        assert_eq!(calls, 2, "one atomic append per logical frame");
        assert_eq!(summary.frames, 2);
        assert_eq!(summary.data_bytes, data.len() as u64);
    }

    #[test]
    fn publication_failure_stops_observation_without_accepting_terminal() {
        let mut calls = 0;
        let result = observe(
            encode(&[chunk("hello"), terminal()]).as_slice(),
            "launch",
            |_| {
                calls += 1;
                bail!("thread closed");
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn terminal_frame_does_not_permit_trailing_bytes() {
        let mut bytes = encode(&[chunk("hello"), terminal()]);
        bytes.push(0x80);
        assert!(
            observe(bytes.as_slice(), "launch", |events| {
                Ok(events
                    .iter()
                    .map(|event| record(event.payload.clone()))
                    .collect())
            })
            .is_err()
        );
    }

    fn completion(status: ThreadTerminalStatus) -> ExecutionCompletion {
        ExecutionCompletion {
            status,
            outcome_code: Some("exit:7".into()),
            result: None,
            error: Some(json!({"exit_code": 7})),
            artifacts: vec![],
            final_cost: None,
            continuation_request: None,
            metadata: None,
        }
    }

    #[test]
    fn protocol_success_never_overrides_process_failure() {
        for status in [ThreadTerminalStatus::Failed, ThreadTerminalStatus::Killed] {
            let mut completion = completion(status);
            let summary = StreamSummary {
                frames: 2,
                data_bytes: 3,
                exit_code: 0,
                last_event: record(json!({})),
            };
            settle(&mut completion, Ok(summary));
            assert_eq!(completion.status, status);
            assert_eq!(completion.outcome_code.as_deref(), Some("exit:7"));
        }
    }

    #[test]
    fn successful_process_requires_valid_successful_stream() {
        let mut invalid = completion(ThreadTerminalStatus::Completed);
        settle(&mut invalid, Err(anyhow::anyhow!("invalid frame")));
        assert_eq!(invalid.status, ThreadTerminalStatus::Failed);
        let mut failed = completion(ThreadTerminalStatus::Completed);
        settle(
            &mut failed,
            Ok(StreamSummary {
                frames: 2,
                data_bytes: 3,
                exit_code: 3,
                last_event: record(json!({})),
            }),
        );
        assert_eq!(failed.status, ThreadTerminalStatus::Failed);
        assert_eq!(
            failed.outcome_code.as_deref(),
            Some("stdout_reported_failure")
        );
    }
}
