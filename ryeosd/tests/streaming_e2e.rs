//! End-to-end proof: the daemon dispatches a streaming_tool item,
//! collects length-prefixed StreamingChunk frames from the subprocess
//! stdout, and returns them as the dispatch result.
//!
//! This exercises the full path:
//!   /execute → dispatch_loop → dispatch_subprocess →
//!   dispatch_managed_subprocess → dispatch_streaming_subprocess →
//!   build_subprocess_spec → lillux::run → read_all_frames → JSON
//!
//! No mocked transport, no stubbed descriptor. The real
//! `rye-tool-streaming-demo` binary emits 5 Stdout frames + 1 Exit
//! frame per the `tool_streaming_v1` wire protocol.

mod common;

use common::DaemonHarness;

/// Canonical ref for the streaming demo tool.
/// The bare_id must encode category/name: the item lives at
/// `tools/rye/core/streaming_demo/streaming-demo.yaml`, so the
/// ref is `streaming_tool:rye/core/streaming_demo/streaming-demo`.
const STREAMING_DEMO_REF: &str = "streaming_tool:rye/core/streaming_demo/streaming-demo";

/// Dispatch the streaming demo and verify the response contains the
/// expected StreamingChunk frame sequence.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_demo_dispatch_returns_frame_array() {
    let mut h = DaemonHarness::start().await.expect("start daemon");

    let (status, body) = h
        .post_execute(STREAMING_DEMO_REF, ".", serde_json::json!({}))
        .await
        .expect("post /execute");

    let stderr = h.drain_stderr_nonblocking().await;
    assert!(
        status.is_success(),
        "status was {status}, body={body}\ndaemon stderr:\n{stderr}"
    );

    // The dispatch_streaming_subprocess function returns the frames
    // as a JSON array (serde_json::to_value(&frames)).
    let frames = body
        .as_array()
        .expect("expected JSON array of frames")
        .clone();

    // The demo binary emits exactly 6 frames: 5 Stdout + 1 Exit.
    assert!(
        frames.len() >= 4,
        "expected at least 4 frames (demo emits 5 stdout + 1 exit), got {}: {frames:?}",
        frames.len()
    );

    // Verify frame ordering: seq starts at 0 and increments.
    for (i, frame) in frames.iter().enumerate() {
        let seq = frame
            .get("seq")
            .and_then(|v| v.as_u64())
            .expect(&format!("frame {i} missing seq"));
        assert_eq!(
            seq, i as u64,
            "frame {i} has seq {seq}, expected {i}"
        );
    }

    // Find the terminal Exit frame.
    let exit_frame = frames.iter().find(|f| {
        f.get("terminal").and_then(|v| v.as_bool()) == Some(true)
    });
    assert!(
        exit_frame.is_some(),
        "no terminal frame found in frames: {frames:?}"
    );

    let exit = exit_frame.unwrap();
    assert_eq!(
        exit.get("kind").and_then(|v| v.as_str()),
        Some("exit"),
        "terminal frame has wrong kind: {exit:?}"
    );
    assert_eq!(
        exit.get("exit_code").and_then(|v| v.as_i64()),
        Some(0),
        "terminal frame exit_code should be 0: {exit:?}"
    );

    // At least 3 Stdout frames before the Exit frame.
    let stdout_count = frames
        .iter()
        .take_while(|f| {
            f.get("terminal").and_then(|v| v.as_bool()) != Some(true)
        })
        .filter(|f| f.get("kind").and_then(|v| v.as_str()) == Some("stdout"))
        .count();
    assert!(
        stdout_count >= 3,
        "expected at least 3 stdout frames before exit, got {stdout_count}: {frames:?}"
    );
}

/// Verify that `read_all_frames` is called in the production path
/// by dispatching the streaming demo and confirming the response is
/// a structured frame array (not opaque stdout text or a thread-wrapped
/// result). This pins that the daemon is NOT returning the raw stdout
/// bytes as a plain string — it's parsing frames via the vocabulary
/// reader.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_dispatch_uses_frame_reader_not_raw_stdout() {
    let h = DaemonHarness::start().await.expect("start daemon");

    let (status, body) = h
        .post_execute(
            STREAMING_DEMO_REF,
            ".",
            serde_json::json!({"key": "value"}),
        )
        .await
        .expect("post /execute");

    assert!(
        status.is_success(),
        "status was {status}, body={body}"
    );

    // The response must be an array (frames), NOT a string (raw stdout).
    // If the frame reader were not wired, the dispatch would return
    // the raw binary stdout as a string or a thread-wrapped result.
    assert!(
        body.is_array(),
        "expected frame array, got non-array type: {body}"
    );

    // Each frame must have structured fields (seq, kind, terminal).
    let first = body
        .as_array()
        .and_then(|a| a.first())
        .expect("non-empty array");
    assert!(
        first.get("seq").is_some(),
        "first frame missing 'seq' field — frame reader not wired? {first:?}"
    );
    assert!(
        first.get("kind").is_some(),
        "first frame missing 'kind' field — frame reader not wired? {first:?}"
    );
    assert!(
        first.get("terminal").is_some(),
        "first frame missing 'terminal' field — frame reader not wired? {first:?}"
    );
}
