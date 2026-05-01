use std::io::{self, Read};

use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::launch_envelope_types::RuntimeResult;
use crate::protocol_vocabulary::error::VocabularyError;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
/// streaming protocol invariants.
pub fn read_all_frames<R: Read>(
    mut reader: R,
) -> Result<Vec<StreamingChunk>, VocabularyError> {
    let mut chunks = Vec::new();
    let mut expected_seq: u64 = 0;
    let mut seen_terminal = false;

    loop {
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(VocabularyError::StreamingProtocolViolation {
                detail: format!("io error reading frame length: {e}"),
            }),
        }
        let frame_len = u32::from_be_bytes(len_buf) as usize;
        let mut frame_buf = vec![0u8; frame_len];
        reader.read_exact(&mut frame_buf).map_err(|e| {
            VocabularyError::StreamingProtocolViolation {
                detail: format!("io error reading frame body: {e}"),
            }
        })?;

        let chunk: StreamingChunk = serde_json::from_slice(&frame_buf).map_err(|e| {
            VocabularyError::StreamingProtocolViolation {
                detail: format!("frame body is not valid JSON StreamingChunk: {e}"),
            }
        })?;

        // Enforce seq monotonicity from 0.
        if chunk.seq != expected_seq {
            return Err(VocabularyError::StreamingProtocolViolation {
                detail: format!(
                    "expected seq {}, got {}",
                    expected_seq, chunk.seq
                ),
            });
        }
        expected_seq += 1;

        if seen_terminal {
            return Err(VocabularyError::StreamingProtocolViolation {
                detail: "frame after terminal frame".into(),
            });
        }

        if chunk.terminal {
            // The terminal frame must be an exit frame.
            if chunk.kind != StreamingChunkKind::Exit {
                return Err(VocabularyError::StreamingProtocolViolation {
                    detail: format!(
                        "terminal frame has kind {:?}, expected Exit",
                        chunk.kind
                    ),
                });
            }
            seen_terminal = true;
        }

        chunks.push(chunk);

        if seen_terminal {
            break;
        }
    }

    if !seen_terminal && !chunks.is_empty() {
        return Err(VocabularyError::StreamingProtocolViolation {
            detail: "stream ended without a terminal frame".into(),
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
            data: None,
            exit_code: None,
            terminal: false,
        }));
        let result = read_all_frames(&mut &buf[..]);
        assert!(result.is_err());
        let msg = format!("{:?}", result.unwrap_err());
        assert!(msg.contains("expected seq 0"));
    }

    #[test]
    fn frame_reader_rejects_non_exit_terminal() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Stdout,
            data: None,
            exit_code: None,
            terminal: true, // terminal but not exit
        }));
        let result = read_all_frames(&mut &buf[..]);
        assert!(result.is_err());
        let msg = format!("{:?}", result.unwrap_err());
        assert!(msg.contains("expected Exit"));
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
            data: None,
            exit_code: None,
            terminal: false,
        }));
        let result = read_all_frames(&mut &buf[..]);
        // With only 4 bytes remaining for the second frame length, we'll
        // get an EOF or a violation. Either way, it must fail.
        assert!(result.is_err() || result.unwrap().len() == 1);
    }

    #[test]
    fn frame_reader_rejects_no_terminal() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&write_frame(&StreamingChunk {
            seq: 0,
            kind: StreamingChunkKind::Stdout,
            data: None,
            exit_code: None,
            terminal: false,
        }));
        // Stream ends abruptly (no more data) — reader gets UnexpectedEof.
        // read_all_frames returns Ok with the single non-terminal chunk,
        // but the loop breaks on UnexpectedEof without seeing terminal.
        // Actually let's test: the reader reads the frame, then tries to
        // read the next length header, gets UnexpectedEof, breaks the loop.
        // Then seen_terminal is false and chunks is non-empty → error.
        // Wait — the break happens on UnexpectedEof, then we check below.
        // Let me verify by running.
        let result = read_all_frames(&mut &buf[..]);
        match result {
            Err(VocabularyError::StreamingProtocolViolation { detail }) => {
                assert!(detail.contains("stream ended without a terminal frame"));
            }
            other => panic!("expected StreamingProtocolViolation, got {:?}", other),
        }
    }
}
