//! Streaming tool demonstrator binary.
//!
//! Reads `parameters_json` on stdin (ignored), then writes 7
//! length-prefixed JSON frames to stdout: 5 stdout chunks, 1 stderr
//! chunk, and a terminal exit frame. This exercises the
//! `tool_streaming_v1` protocol end-to-end including the Stderr kind.

use std::io::{Read, Write};

use base64::Engine;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Minimal streaming frame types (matches ryeos_engine vocabulary).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StreamingChunkKind {
    Stdout,
    Stderr,
    Exit,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct StreamingChunk {
    seq: u64,
    kind: StreamingChunkKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    terminal: bool,
}

fn emit_frame(chunk: &StreamingChunk) {
    let body = serde_json::to_vec(chunk).unwrap();
    let len = (body.len() as u32).to_be_bytes();
    let mut out = std::io::stdout().lock();
    out.write_all(&len).unwrap();
    out.write_all(&body).unwrap();
    out.flush().unwrap();
}

fn main() {
    // Drain stdin (tool_streaming_v1 sends parameters_json).
    let mut stdin = String::new();
    std::io::stdin().read_to_string(&mut stdin).unwrap();

    // 5 stdout chunks.
    for i in 0..5u64 {
        emit_frame(&StreamingChunk {
            seq: i,
            kind: StreamingChunkKind::Stdout,
            data: Some(base64::engine::general_purpose::STANDARD
                .encode(format!("chunk {i}\n"))),
            exit_code: None,
            terminal: false,
        });
    }

    // 1 stderr chunk so the demo exercises every non-terminal frame
    // kind in the `tool_streaming_v1` protocol.
    emit_frame(&StreamingChunk {
        seq: 5,
        kind: StreamingChunkKind::Stderr,
        data: Some(base64::engine::general_purpose::STANDARD
            .encode("done\n")),
        exit_code: None,
        terminal: false,
    });

    // Terminal exit frame.
    emit_frame(&StreamingChunk {
        seq: 6,
        kind: StreamingChunkKind::Exit,
        data: None,
        exit_code: Some(0),
        terminal: true,
    });
}
