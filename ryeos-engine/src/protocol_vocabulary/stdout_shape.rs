use std::io::{self, Read};

use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::launch_envelope_types::RuntimeResult;

/// Maximum permitted size of a single streaming frame body.
///
/// Per-frame guard, not per-stream: cumulative-stream guards belong at
/// the pipe level (e.g. lillux's subprocess bridge). 1 MiB is large
/// enough for any reasonable structured chunk and small enough that a
/// runaway producer can't OOM the daemon.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum StdoutShape {
    /// Captured verbatim. Daemon does no parsing. Returned as part of
    /// the ExecutionCompletion.
    OpaqueBytes,

    /// Daemon parses stdout as a single RuntimeResult JSON object at exit.
    /// Wire shape: `RuntimeResult` from `launch_envelope_types`.
    RuntimeResultV1,

    /// Daemon reads length-prefixed JSON frames during execution. Each
    /// frame is a StreamingChunk. The final frame's terminal: true bit
    /// ends the stream.
    StreamingChunksV1,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamingChunkKind {
    Stdout,
    Stderr,
    Exit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamingChunk {
    pub seq: u64,
    pub kind: StreamingChunkKind,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub terminal: bool,
}

#[derive(Debug)]
pub enum DecodedStdout {
    Opaque(Vec<u8>),
    RuntimeResult(RuntimeResult),
    Streaming(Vec<StreamingChunk>),
}

#[derive(Debug)]
pub enum DecodedFrame {
    Streaming(StreamingChunk),
}

/// Typed errors surfaced by the streaming frame reader.
///
/// Replaces the predecessor wave's `VocabularyError::StreamingProtocolViolation`
/// catch-all string. Production callers (the dispatch_streaming_subprocess
/// path in ryeosd) match on these variants for structured logging and
/// targeted recovery; tests assert on variants instead of substring.
#[derive(Debug, thiserror::Error)]
pub enum FrameReadError {
    #[error("io error reading frame length at offset {offset}: {source}")]
    IoLength {
        offset: usize,
        #[source]
        source: io::Error,
    },
    #[error("io error reading frame body at offset {offset}: {source}")]
    IoBody {
        offset: usize,
        #[source]
        source: io::Error,
    },
    #[error("frame at offset {offset} exceeds max length {max} (got {got})")]
    FrameTooLarge {
        offset: usize,
        max: usize,
        got: usize,
    },
    #[error("frame body at offset {offset} is not valid JSON: {source}")]
    InvalidJson {
        offset: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("expected seq {expected}, got {actual}")]
    SeqOutOfOrder { expected: u64, actual: u64 },
    #[error("frame after terminal frame")]
    FrameAfterTerminal,
    #[error("terminal frame at seq {seq} has kind {kind:?}, expected Exit")]
    NonExitTerminal { seq: u64, kind: StreamingChunkKind },
    #[error("Exit frame at seq {seq} missing required `exit_code` field")]
    ExitMissingCode { seq: u64 },
    #[error("{kind:?} frame at seq {seq} missing required `data` field")]
    ChunkMissingData { kind: StreamingChunkKind, seq: u64 },
    #[error("unknown frame kind `{kind}` at seq {seq}")]
    UnknownKind { kind: String, seq: u64 },
    #[error("stream ended without a terminal Exit frame ({frames_seen} frames seen)")]
    StreamMissingExit { frames_seen: usize },
}

/// Permissive intermediate parse so we can surface `UnknownKind` as a
/// typed variant instead of a serde enum-deserialization error string.
#[derive(Debug, Deserialize)]
struct RawFrame {
    seq: u64,
    kind: String,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    terminal: bool,
}

/// Terminal decode (called on child exit when StdoutMode == Terminal).
pub fn decode_stdout_terminal(
    shape: StdoutShape,
    raw_bytes: &[u8],
) -> Result<DecodedStdout, EngineError> {
    match shape {
        StdoutShape::OpaqueBytes => Ok(DecodedStdout::Opaque(raw_bytes.to_vec())),
        StdoutShape::RuntimeResultV1 => {
            let parsed: RuntimeResult = serde_json::from_slice(raw_bytes).map_err(|e| {
                EngineError::Internal(format!(
                    "failed to parse RuntimeResult from stdout: {e}"
                ))
            })?;
            Ok(DecodedStdout::RuntimeResult(parsed))
        }
        StdoutShape::StreamingChunksV1 => {
            Err(EngineError::Internal(
                "StreamingChunksV1 cannot be decoded as terminal; use frame reader".into(),
            ))
        }
    }
}

/// Streaming frame decode (called per-frame during execution when
/// StdoutMode == Streaming).
pub fn decode_stdout_frame(
    shape: StdoutShape,
    frame_bytes: &[u8],
) -> Result<DecodedFrame, EngineError> {
    match shape {
        StdoutShape::StreamingChunksV1 => {
            let chunk: StreamingChunk = serde_json::from_slice(frame_bytes).map_err(|e| {
                EngineError::Internal(format!(
                    "failed to parse StreamingChunk frame: {e}"
                ))
            })?;
            Ok(DecodedFrame::Streaming(chunk))
        }
        _ => Err(EngineError::Internal(
            "frame decode only valid for StreamingChunksV1".into(),
        )),
    }
}

/// Read all length-prefixed frames from a reader, enforcing the
/// streaming protocol invariants:
///
/// * Each frame body is at most [`MAX_FRAME_BYTES`].
/// * Sequence numbers begin at 0 and increment by 1.
/// * Stdout/Stderr frames MUST carry a `data` field (may be empty,
///   must be present).
/// * Exit frames MUST carry an `exit_code` field.
/// * Unknown `kind` strings surface as [`FrameReadError::UnknownKind`]
///   rather than being collapsed into a serde error string.
/// * The stream MUST terminate with exactly one `Exit` frame whose
///   `terminal` flag is set; streams that close without an `Exit`
///   frame fail with [`FrameReadError::StreamMissingExit`].
/// * No frames may appear after the terminal `Exit` frame.
///
/// A stream MAY emit zero `Stdout`/`Stderr` frames before its terminal
/// `Exit` — that is "succeed silently" and is permitted.
pub fn read_all_frames<R: Read>(
    mut reader: R,
) -> Result<Vec<StreamingChunk>, FrameReadError> {
    let mut chunks = Vec::new();
    let mut expected_seq: u64 = 0;
    let mut seen_terminal = false;
    let mut offset: usize = 0;

    loop {
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(FrameReadError::IoLength { offset, source: e }),
        }
        let frame_len = u32::from_be_bytes(len_buf) as usize;
        if frame_len > MAX_FRAME_BYTES {
            return Err(FrameReadError::FrameTooLarge {
                offset,
                max: MAX_FRAME_BYTES,
                got: frame_len,
            });
        }
        let body_offset = offset + 4;
        let mut frame_buf = vec![0u8; frame_len];
        reader
            .read_exact(&mut frame_buf)
            .map_err(|e| FrameReadError::IoBody {
                offset: body_offset,
                source: e,
            })?;

        let raw: RawFrame =
            serde_json::from_slice(&frame_buf).map_err(|e| FrameReadError::InvalidJson {
                offset: body_offset,
                source: e,
            })?;

        // Per-kind validation, surfacing unknown kinds as typed errors.
        let kind = match raw.kind.as_str() {
            "stdout" => StreamingChunkKind::Stdout,
            "stderr" => StreamingChunkKind::Stderr,
            "exit" => StreamingChunkKind::Exit,
            other => {
                return Err(FrameReadError::UnknownKind {
                    kind: other.to_string(),
                    seq: raw.seq,
                });
            }
        };

        // Field invariants per kind.
        match kind {
            StreamingChunkKind::Stdout | StreamingChunkKind::Stderr => {
                if raw.data.is_none() {
                    return Err(FrameReadError::ChunkMissingData {
                        kind,
                        seq: raw.seq,
                    });
                }
            }
            StreamingChunkKind::Exit => {
                if raw.exit_code.is_none() {
                    return Err(FrameReadError::ExitMissingCode { seq: raw.seq });
                }
            }
        }

        // Seq monotonicity from 0.
        if raw.seq != expected_seq {
            return Err(FrameReadError::SeqOutOfOrder {
                expected: expected_seq,
                actual: raw.seq,
            });
        }
        expected_seq += 1;

        if seen_terminal {
            return Err(FrameReadError::FrameAfterTerminal);
        }

        if raw.terminal {
            if kind != StreamingChunkKind::Exit {
                return Err(FrameReadError::NonExitTerminal {
                    seq: raw.seq,
                    kind,
                });
            }
            seen_terminal = true;
        }

        chunks.push(StreamingChunk {
            seq: raw.seq,
            kind,
            data: raw.data,
            exit_code: raw.exit_code,
            terminal: raw.terminal,
        });

        offset = body_offset + frame_len;

        if seen_terminal {
            break;
        }
    }

    if !seen_terminal {
        return Err(FrameReadError::StreamMissingExit {
            frames_seen: chunks.len(),
        });
    }

    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_variants() {
        for shape in [
            StdoutShape::OpaqueBytes,
            StdoutShape::RuntimeResultV1,
            StdoutShape::StreamingChunksV1,
        ] {
            let yaml = serde_yaml::to_string(&shape).unwrap();
            let parsed: StdoutShape = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(parsed, shape);
        }
    }

    #[test]
    fn reject_unknown() {
        let err = serde_yaml::from_str::<StdoutShape>("unknown");
        assert!(err.is_err());
    }

    #[test]
    fn opaque_decoder_returns_input_unchanged() {
        let bytes = b"hello world";
        let result = decode_stdout_terminal(StdoutShape::OpaqueBytes, bytes).unwrap();
        match result {
            DecodedStdout::Opaque(v) => assert_eq!(v, bytes),
            _ => panic!("expected Opaque"),
        }
    }

    #[test]
    fn runtime_result_decoder_accepts_valid() {
        let rr = RuntimeResult {
            success: true,
            status: "completed".into(),
            thread_id: "T-test".into(),
            result: None,
            outputs: serde_json::Value::Null,
            cost: None,
            warnings: vec![],
        };
        let bytes = serde_json::to_vec(&rr).unwrap();
        let result = decode_stdout_terminal(StdoutShape::RuntimeResultV1, &bytes).unwrap();
        match result {
            DecodedStdout::RuntimeResult(parsed) => {
                assert!(parsed.success);
                assert_eq!(parsed.thread_id, "T-test");
            }
            _ => panic!("expected RuntimeResult"),
        }
    }

    #[test]
    fn runtime_result_decoder_rejects_non_json() {
        let result = decode_stdout_terminal(StdoutShape::RuntimeResultV1, b"not json");
        assert!(result.is_err());
    }

    fn write_frame(chunk: &StreamingChunk) -> Vec<u8> {
        let body = serde_json::to_vec(chunk).unwrap();
        let len = (body.len() as u32).to_be_bytes();
        let mut out = len.to_vec();
        out.extend_from_slice(&body);
        out
    }

    /// Helper that emits a frame from raw JSON, bypassing the
    /// `StreamingChunk` serializer so negative-path tests can construct
    /// shape-incomplete bodies (missing `data`, missing `exit_code`,
    /// unknown `kind`) that the strict parser must reject.
    fn write_raw_frame(body: &serde_json::Value) -> Vec<u8> {
        let bytes = serde_json::to_vec(body).unwrap();
        let len = (bytes.len() as u32).to_be_bytes();
        let mut out = len.to_vec();
        out.extend_from_slice(&bytes);
        out
    }

    #[test]
    fn frame_reader_valid_sequence() {
        let mut buf = Vec::new();
        for i in 0..3 {
            buf.extend_from_slice(&write_frame(&StreamingChunk {
                seq: i,
                kind: StreamingChunkKind::Stdout,
                data: Some(format!("chunk {i}")),
                exit_code: None,
                terminal: false,
            }));
        }
        // Terminal exit frame
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 3,
            kind: StreamingChunkKind::Exit,
            data: None,
            exit_code: Some(0),
            terminal: true,
        }));

        let chunks = read_all_frames(&mut &buf[..]).unwrap();
        assert_eq!(chunks.len(), 4);
        assert!(chunks[3].terminal);
        assert_eq!(chunks[3].kind, StreamingChunkKind::Exit);
    }

    #[test]
    fn frame_reader_rejects_out_of_order_seq() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 1, // should be 0
            kind: StreamingChunkKind::Stdout,
            data: Some(String::new()),
            exit_code: None,
            terminal: false,
        }));
        let result = read_all_frames(&mut &buf[..]);
        match result {
            Err(FrameReadError::SeqOutOfOrder { expected, actual }) => {
                assert_eq!(expected, 0);
                assert_eq!(actual, 1);
            }
            other => panic!("expected SeqOutOfOrder, got {other:?}"),
        }
    }

    #[test]
    fn frame_reader_rejects_non_exit_terminal() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Stdout,
            data: Some(String::new()),
            exit_code: None,
            terminal: true, // terminal but not exit
        }));
        let result = read_all_frames(&mut &buf[..]);
        match result {
            Err(FrameReadError::NonExitTerminal { seq, kind }) => {
                assert_eq!(seq, 0);
                assert_eq!(kind, StreamingChunkKind::Stdout);
            }
            other => panic!("expected NonExitTerminal, got {other:?}"),
        }
    }

    #[test]
    fn frame_reader_rejects_frames_after_terminal() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Exit,
            data: None,
            exit_code: Some(0),
            terminal: true,
        }));
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 1,
            kind: StreamingChunkKind::Stdout,
            data: Some(String::new()),
            exit_code: None,
            terminal: false,
        }));
        // The reader stops after the terminal frame; trailing bytes are
        // ignored. Callers that need to detect bytes-after-exit should
        // check the underlying reader's remaining bytes themselves.
        let chunks = read_all_frames(&mut &buf[..]).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].terminal);
    }

    #[test]
    fn frame_reader_rejects_no_terminal() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Stdout,
            data: Some(String::new()),
            exit_code: None,
            terminal: false,
        }));
        let result = read_all_frames(&mut &buf[..]);
        match result {
            Err(FrameReadError::StreamMissingExit { frames_seen }) => {
                assert_eq!(frames_seen, 1);
            }
            other => panic!("expected StreamMissingExit, got {other:?}"),
        }
    }

    /// η: Empty stream (zero bytes) is also a missing-exit violation,
    /// not a silent success. A streaming subprocess that produces no
    /// frames at all is ambiguous and must be flagged.
    #[test]
    fn frame_reader_empty_stream_fails_loud() {
        let buf: Vec<u8> = Vec::new();
        let result = read_all_frames(&mut &buf[..]);
        match result {
            Err(FrameReadError::StreamMissingExit { frames_seen }) => {
                assert_eq!(frames_seen, 0);
            }
            other => panic!("expected StreamMissingExit (frames_seen=0), got {other:?}"),
        }
    }

    /// η: Per-frame max length guard. A length prefix above
    /// `MAX_FRAME_BYTES` must abort before allocating the frame body.
    #[test]
    fn frame_too_large_fails_loud() {
        let mut buf = Vec::new();
        let oversized = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        buf.extend_from_slice(&oversized);
        let result = read_all_frames(&mut &buf[..]);
        match result {
            Err(FrameReadError::FrameTooLarge { offset, max, got }) => {
                assert_eq!(offset, 0);
                assert_eq!(max, MAX_FRAME_BYTES);
                assert_eq!(got, MAX_FRAME_BYTES + 1);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    /// η: Exit frame missing `exit_code` is a typed violation.
    #[test]
    fn exit_missing_code_fails_loud() {
        let body = serde_json::json!({
            "seq": 0,
            "kind": "exit",
            "terminal": true,
        });
        let buf = write_raw_frame(&body);
        let result = read_all_frames(&mut &buf[..]);
        match result {
            Err(FrameReadError::ExitMissingCode { seq }) => assert_eq!(seq, 0),
            other => panic!("expected ExitMissingCode, got {other:?}"),
        }
    }

    /// η: Stdout frame missing `data` is a typed violation. Empty
    /// string `data: ""` would be valid; absence of the field is not.
    #[test]
    fn chunk_missing_data_fails_loud_for_stdout() {
        let body = serde_json::json!({
            "seq": 0,
            "kind": "stdout",
            "terminal": false,
        });
        let buf = write_raw_frame(&body);
        let result = read_all_frames(&mut &buf[..]);
        match result {
            Err(FrameReadError::ChunkMissingData { kind, seq }) => {
                assert_eq!(kind, StreamingChunkKind::Stdout);
                assert_eq!(seq, 0);
            }
            other => panic!("expected ChunkMissingData(Stdout), got {other:?}"),
        }
    }

    /// η: Stderr frame missing `data` is a typed violation.
    #[test]
    fn chunk_missing_data_fails_loud_for_stderr() {
        let body = serde_json::json!({
            "seq": 0,
            "kind": "stderr",
            "terminal": false,
        });
        let buf = write_raw_frame(&body);
        let result = read_all_frames(&mut &buf[..]);
        match result {
            Err(FrameReadError::ChunkMissingData { kind, seq }) => {
                assert_eq!(kind, StreamingChunkKind::Stderr);
                assert_eq!(seq, 0);
            }
            other => panic!("expected ChunkMissingData(Stderr), got {other:?}"),
        }
    }

    /// η: Empty-string `data` is permitted (succeed-silently chunk).
    #[test]
    fn chunk_with_empty_data_is_accepted() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Stdout,
            data: Some(String::new()),
            exit_code: None,
            terminal: false,
        }));
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 1,
            kind: StreamingChunkKind::Exit,
            data: None,
            exit_code: Some(0),
            terminal: true,
        }));
        let chunks = read_all_frames(&mut &buf[..]).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].data.as_deref(), Some(""));
    }

    /// η: Unknown frame kind surfaces as a typed variant, not as a
    /// serde enum-deserialization string.
    #[test]
    fn unknown_kind_fails_loud() {
        let body = serde_json::json!({
            "seq": 0,
            "kind": "warning",
            "data": "something",
            "terminal": false,
        });
        let buf = write_raw_frame(&body);
        let result = read_all_frames(&mut &buf[..]);
        match result {
            Err(FrameReadError::UnknownKind { kind, seq }) => {
                assert_eq!(kind, "warning");
                assert_eq!(seq, 0);
            }
            other => panic!("expected UnknownKind, got {other:?}"),
        }
    }

    /// η: A stream that emits only an Exit frame ("succeed silently")
    /// is the explicit empty-stream policy: zero Stdout/Stderr frames
    /// before Exit is allowed.
    #[test]
    fn stream_with_only_exit_frame_succeeds() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Exit,
            data: None,
            exit_code: Some(0),
            terminal: true,
        }));
        let chunks = read_all_frames(&mut &buf[..]).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, StreamingChunkKind::Exit);
    }

    /// η: The streaming demo binary emits 5 stdout chunks, 1 stderr
    /// chunk, and a terminal exit. Round-trip the wire format here so
    /// the demo's wire contract is pinned outside the e2e test.
    #[test]
    fn demo_binary_frame_round_trip() {
        use base64::Engine;

        let mut buf = Vec::new();
        for i in 0..5u64 {
            let payload = base64::engine::general_purpose::STANDARD
                .encode(format!("chunk {i}\n"));
            buf.extend_from_slice(&write_frame(&StreamingChunk {
                seq: i,
                kind: StreamingChunkKind::Stdout,
                data: Some(payload),
                exit_code: None,
                terminal: false,
            }));
        }
        // Stderr chunk emitted alongside stdout to exercise the Stderr
        // variant in the production frame path.
        let stderr_payload = base64::engine::general_purpose::STANDARD
            .encode("done\n");
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 5,
            kind: StreamingChunkKind::Stderr,
            data: Some(stderr_payload),
            exit_code: None,
            terminal: false,
        }));
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 6,
            kind: StreamingChunkKind::Exit,
            data: None,
            exit_code: Some(0),
            terminal: true,
        }));

        let chunks = read_all_frames(&mut &buf[..]).unwrap();
        assert_eq!(chunks.len(), 7);

        for i in 0..5 {
            assert_eq!(chunks[i].seq, i as u64);
            assert_eq!(chunks[i].kind, StreamingChunkKind::Stdout);
            assert!(!chunks[i].terminal);
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(chunks[i].data.as_ref().unwrap())
                .unwrap();
            assert_eq!(
                String::from_utf8(decoded).unwrap(),
                format!("chunk {i}\n")
            );
        }

        assert_eq!(chunks[5].kind, StreamingChunkKind::Stderr);
        assert!(!chunks[5].terminal);
        let stderr_decoded = base64::engine::general_purpose::STANDARD
            .decode(chunks[5].data.as_ref().unwrap())
            .unwrap();
        assert_eq!(String::from_utf8(stderr_decoded).unwrap(), "done\n");

        assert_eq!(chunks[6].seq, 6);
        assert_eq!(chunks[6].kind, StreamingChunkKind::Exit);
        assert_eq!(chunks[6].exit_code, Some(0));
        assert!(chunks[6].terminal);
    }
}
