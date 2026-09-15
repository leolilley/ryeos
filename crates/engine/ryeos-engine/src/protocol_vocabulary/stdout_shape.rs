use std::io::{self, Read};

use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::launch_envelope_types::RuntimeResult;
#[cfg(test)]
use crate::launch_envelope_types::RuntimeResultStatus;
use crate::method_wire::MethodCallResult;

/// Maximum permitted size of a single streaming frame body.
///
/// Per-frame guard, not per-stream: cumulative-stream guards belong at
/// the pipe level (e.g. lillux's subprocess bridge). 1 MiB is large
/// enough for any reasonable structured chunk and small enough that a
/// runaway producer can't OOM the daemon.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StdoutShape {
    /// Captured verbatim. Daemon does no parsing. Returned as part of
    /// the ExecutionCompletion.
    OpaqueBytes,

    /// Daemon parses stdout as a single RuntimeResult JSON object at exit.
    /// Wire shape: `RuntimeResult` from `launch_envelope_types`.
    RuntimeResult,

    /// Daemon parses stdout as one method-runtime result object at exit.
    MethodCallResult,

    /// Daemon reads length-prefixed JSON frames during execution. Each
    /// frame is a StreamingChunk. The final frame's terminal: true bit
    /// ends the stream.
    StreamingChunks,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamingChunkKind {
    Stdout,
    Stderr,
    Exit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
    MethodCallResult(MethodCallResult),
    Streaming(Vec<StreamingChunk>),
}

#[derive(Debug)]
pub enum DecodedFrame {
    Streaming(StreamingChunk),
}

/// Typed errors surfaced by the streaming frame reader.
///
/// The ordinary subprocess output observer uses these categories for bounded
/// diagnostics without persisting arbitrary malformed frame content.
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
    #[error("Exit frame at seq {seq} must be terminal")]
    NonTerminalExit { seq: u64 },
    #[error("{kind:?} frame at seq {seq} missing required `data` field")]
    ChunkMissingData { kind: StreamingChunkKind, seq: u64 },
    #[error("unknown frame kind `{kind}` at seq {seq}")]
    UnknownKind { kind: String, seq: u64 },
    #[error("stream ended without a terminal Exit frame ({frames_seen} frames seen)")]
    StreamMissingExit { frames_seen: usize },
}

impl FrameReadError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::IoLength { .. } | Self::IoBody { .. } => "frame_io_failed",
            Self::FrameTooLarge { .. } => "frame_too_large",
            Self::InvalidJson { .. } => "frame_invalid_json",
            Self::SeqOutOfOrder { .. } => "frame_bad_sequence",
            Self::FrameAfterTerminal => "frame_after_terminal",
            Self::NonExitTerminal { .. } | Self::NonTerminalExit { .. } => "frame_invalid_terminal",
            Self::ExitMissingCode { .. } | Self::ChunkMissingData { .. } => "frame_missing_field",
            Self::UnknownKind { .. } => "frame_unknown_kind",
            Self::StreamMissingExit { .. } => "stream_missing_terminal",
        }
    }
}

/// Permissive intermediate parse so we can surface `UnknownKind` as a
/// typed variant instead of a serde enum-deserialization error string.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
        StdoutShape::RuntimeResult => {
            let parsed: RuntimeResult = serde_json::from_slice(raw_bytes).map_err(|e| {
                EngineError::Internal(format!("failed to parse RuntimeResult from stdout: {e}"))
            })?;
            Ok(DecodedStdout::RuntimeResult(parsed))
        }
        StdoutShape::MethodCallResult => {
            let parsed: MethodCallResult = serde_json::from_slice(raw_bytes).map_err(|e| {
                EngineError::Internal(format!("failed to parse MethodCallResult from stdout: {e}"))
            })?;
            parsed.validate().map_err(|reason| {
                EngineError::Internal(format!(
                    "invalid MethodCallResult semantics from stdout: {reason}"
                ))
            })?;
            Ok(DecodedStdout::MethodCallResult(parsed))
        }
        StdoutShape::StreamingChunks => Err(EngineError::Internal(
            "StreamingChunks cannot be decoded as terminal; use frame reader".into(),
        )),
    }
}

/// Streaming frame decode (called per-frame during execution when
/// StdoutMode == Streaming).
pub fn decode_stdout_frame(
    shape: StdoutShape,
    frame_bytes: &[u8],
) -> Result<DecodedFrame, EngineError> {
    match shape {
        StdoutShape::StreamingChunks => {
            let chunk: StreamingChunk = serde_json::from_slice(frame_bytes).map_err(|e| {
                EngineError::Internal(format!("failed to parse StreamingChunk frame: {e}"))
            })?;
            Ok(DecodedFrame::Streaming(chunk))
        }
        _ => Err(EngineError::Internal(
            "frame decode only valid for StreamingChunks".into(),
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
pub fn read_all_frames<R: Read>(reader: R) -> Result<Vec<StreamingChunk>, FrameReadError> {
    StreamingFrameReader::new(reader).collect()
}

/// Incremental observation of the existing framed protocol. Returning a
/// terminal frame is not process settlement or even proof of clean EOF: keep
/// consuming to validate the trailing boundary and independently wait for the
/// exact process. The reader owns no process, queue, event store, or policy.
pub struct StreamingFrameReader<R> {
    reader: R,
    expected_seq: u64,
    seen_terminal: bool,
    offset: usize,
    finished: bool,
}

impl<R: Read> StreamingFrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            expected_seq: 0,
            seen_terminal: false,
            offset: 0,
            finished: false,
        }
    }

    fn read_next(&mut self) -> Result<Option<StreamingChunk>, FrameReadError> {
        let offset = self.offset;
        let mut len_buf = [0u8; 4];
        // Reading the first byte separately distinguishes clean EOF from a
        // truncated binary length header. Neither goes through a UTF-8 string.
        match self.reader.read_exact(&mut len_buf[..1]) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return if self.seen_terminal {
                    Ok(None)
                } else {
                    Err(FrameReadError::StreamMissingExit {
                        frames_seen: self.expected_seq as usize,
                    })
                };
            }
            Err(e) => return Err(FrameReadError::IoLength { offset, source: e }),
        }
        if self.seen_terminal {
            return Err(FrameReadError::FrameAfterTerminal);
        }
        self.reader
            .read_exact(&mut len_buf[1..])
            .map_err(|source| FrameReadError::IoLength { offset, source })?;
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
        self.reader
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
                    return Err(FrameReadError::ChunkMissingData { kind, seq: raw.seq });
                }
            }
            StreamingChunkKind::Exit => {
                if raw.exit_code.is_none() {
                    return Err(FrameReadError::ExitMissingCode { seq: raw.seq });
                }
                if !raw.terminal {
                    return Err(FrameReadError::NonTerminalExit { seq: raw.seq });
                }
            }
        }

        // Seq monotonicity from 0.
        if raw.seq != self.expected_seq {
            return Err(FrameReadError::SeqOutOfOrder {
                expected: self.expected_seq,
                actual: raw.seq,
            });
        }
        self.expected_seq += 1;

        if raw.terminal {
            if kind != StreamingChunkKind::Exit {
                return Err(FrameReadError::NonExitTerminal { seq: raw.seq, kind });
            }
            self.seen_terminal = true;
        }

        self.offset = body_offset + frame_len;
        Ok(Some(StreamingChunk {
            seq: raw.seq,
            kind,
            data: raw.data,
            exit_code: raw.exit_code,
            terminal: raw.terminal,
        }))
    }
}

impl<R: Read> Iterator for StreamingFrameReader<R> {
    type Item = Result<StreamingChunk, FrameReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match self.read_next() {
            Ok(Some(chunk)) => Some(Ok(chunk)),
            Ok(None) => {
                self.finished = true;
                None
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_variants() {
        for shape in [
            StdoutShape::OpaqueBytes,
            StdoutShape::RuntimeResult,
            StdoutShape::MethodCallResult,
            StdoutShape::StreamingChunks,
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
            status: RuntimeResultStatus::Completed,
            thread_id: "T-test".into(),
            result: None,
            outputs: serde_json::Value::Null,
            cost: None,
            warnings: vec![],
        };
        let bytes = serde_json::to_vec(&rr).unwrap();
        let result = decode_stdout_terminal(StdoutShape::RuntimeResult, &bytes).unwrap();
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
        let result = decode_stdout_terminal(StdoutShape::RuntimeResult, b"not json");
        assert!(result.is_err());
    }

    #[test]
    fn runtime_result_decoder_rejects_success_status_contradiction() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "success": true,
            "status": "failed",
            "thread_id": "T-test",
            "outputs": null,
            "warnings": [],
        }))
        .unwrap();

        let error = decode_stdout_terminal(StdoutShape::RuntimeResult, &bytes)
            .unwrap_err()
            .to_string();
        assert!(error.contains("success"));
        assert!(error.contains("contradicts `status` `failed`"));
    }

    #[test]
    fn method_result_decoder_accepts_valid() {
        let method_result = MethodCallResult {
            success: true,
            kind: "knowledge".to_string(),
            method: "compose".to_string(),
            output: Some(serde_json::json!({"rendered": "context"})),
            error: None,
            warnings: Vec::new(),
        };
        let bytes = serde_json::to_vec(&method_result).unwrap();
        let decoded = decode_stdout_terminal(StdoutShape::MethodCallResult, &bytes).unwrap();
        match decoded {
            DecodedStdout::MethodCallResult(parsed) => {
                assert!(parsed.success);
                assert_eq!(parsed.kind, "knowledge");
                assert_eq!(parsed.method, "compose");
            }
            _ => panic!("expected MethodCallResult"),
        }
    }

    #[test]
    fn method_result_decoder_rejects_incoherent_shape() {
        let method_result = MethodCallResult {
            success: true,
            kind: "knowledge".to_string(),
            method: "compose".to_string(),
            output: None,
            error: None,
            warnings: Vec::new(),
        };
        let bytes = serde_json::to_vec(&method_result).unwrap();
        assert!(decode_stdout_terminal(StdoutShape::MethodCallResult, &bytes).is_err());
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
    fn incremental_reader_delivers_binary_frame_before_reading_the_next_byte() {
        struct NoReadAhead {
            bytes: std::io::Cursor<Vec<u8>>,
        }
        impl Read for NoReadAhead {
            fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
                assert!(
                    self.bytes.position() < self.bytes.get_ref().len() as u64,
                    "a delivered frame must not wait for the next frame or process EOF"
                );
                self.bytes.read(target)
            }
        }
        let mut body = serde_json::to_vec(&StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Stdout,
            data: Some("first".into()),
            exit_code: None,
            terminal: false,
        })
        .unwrap();
        body.resize(128, b' ');
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        assert_eq!(bytes[3], 0x80);
        bytes.extend(body);
        let mut frames = StreamingFrameReader::new(NoReadAhead {
            bytes: io::Cursor::new(bytes),
        });
        let first = frames.next().unwrap().unwrap();
        assert_eq!(first.data.as_deref(), Some("first"));
        assert!(!first.terminal);
    }

    #[test]
    fn incremental_terminal_frame_is_not_clean_eof_authority() {
        let mut bytes = write_frame(&StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Exit,
            data: None,
            exit_code: Some(0),
            terminal: true,
        });
        bytes.push(0x80);
        let mut frames = StreamingFrameReader::new(io::Cursor::new(bytes));
        assert!(frames.next().unwrap().unwrap().terminal);
        assert!(matches!(
            frames.next(),
            Some(Err(FrameReadError::FrameAfterTerminal))
        ));
        assert!(frames.next().is_none());
        let mut truncated = StreamingFrameReader::new(io::Cursor::new(vec![0, 0]));
        assert!(matches!(
            truncated.next(),
            Some(Err(FrameReadError::IoLength { .. }))
        ));
        assert!(truncated.next().is_none());
    }

    #[test]
    fn exit_frame_requires_terminal_marker() {
        let body = br#"{"seq":0,"kind":"exit","exit_code":0,"terminal":false}"#;
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend(body);
        assert!(matches!(
            read_all_frames(bytes.as_slice()),
            Err(FrameReadError::NonTerminalExit { seq: 0 })
        ));
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
        assert!(matches!(
            read_all_frames(&mut &buf[..]),
            Err(FrameReadError::FrameAfterTerminal)
        ));
    }

    #[test]
    fn frame_reader_rejects_partial_bytes_after_terminal() {
        let mut buf = write_frame(&StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Exit,
            data: None,
            exit_code: Some(0),
            terminal: true,
        });
        buf.push(0xff);

        assert!(matches!(
            read_all_frames(&mut &buf[..]),
            Err(FrameReadError::FrameAfterTerminal)
        ));
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
            let payload = base64::engine::general_purpose::STANDARD.encode(format!("chunk {i}\n"));
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
        let stderr_payload = base64::engine::general_purpose::STANDARD.encode("done\n");
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

        for (i, chunk) in chunks.iter().enumerate().take(5) {
            assert_eq!(chunk.seq, i as u64);
            assert_eq!(chunk.kind, StreamingChunkKind::Stdout);
            assert!(!chunk.terminal);
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(chunk.data.as_ref().unwrap())
                .unwrap();
            assert_eq!(String::from_utf8(decoded).unwrap(), format!("chunk {i}\n"));
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
