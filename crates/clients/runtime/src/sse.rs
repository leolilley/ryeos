//! One byte-stream Server-Sent Events parser for every stream the runtime
//! reads: session hints, chain tails and seat history.
//!
//! Chunks arrive from a [`ByteSource`] exactly as the transport read them, so
//! frame delimiters and multi-byte code points can both straddle chunks. The
//! parser buffers bytes and decodes a frame only when its closing blank line
//! has arrived.
//!
//! Two modes share the framing:
//!
//! * [`SseMode::Lenient`] (hints, tails) follows the SSE field grammar: lines
//!   beginning `:` are comments, `event`, `data` are read with at most one
//!   leading space stripped, other fields (`id`, `retry`) are ignored, a frame
//!   with no data line is skipped, and the buffer may hold up to 32 MiB.
//! * [`SseMode::Strict`] (seat history) accepts exactly one `event: ` and one
//!   `data: ` line per frame (a single space after the field name), refuses
//!   every other field including `id:` and `retry:`, skips comment-only
//!   frames, and bounds one raw frame at 16 MiB.

use std::fmt;

use crate::error::RuntimeError;
use crate::platform::ByteSource;

/// Largest unparsed buffer a lenient stream may hold.
pub const LENIENT_BUFFER_LIMIT: usize = 32 * 1024 * 1024;

/// Largest raw frame (before JSON decoding) a strict stream may carry.
pub const STRICT_FRAME_LIMIT: usize = 16 * 1024 * 1024;

/// How a stream's frames are parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseMode {
    /// Hints and tails.
    Lenient,
    /// Seat history.
    Strict,
}

/// One decoded frame: the `event` field (`message` when a lenient frame has
/// none) and the `data` payload, verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: String,
    pub data: String,
}

/// Why a frame could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseError {
    /// A complete frame was not valid UTF-8.
    InvalidUtf8,
    /// A lenient stream buffered more than [`LENIENT_BUFFER_LIMIT`] bytes
    /// without completing a frame.
    BufferTooLarge,
    /// A strict raw frame exceeded [`STRICT_FRAME_LIMIT`].
    FrameTooLarge,
    /// A strict frame repeated `event`/`data` or carried another field.
    UnsupportedField,
    /// A strict frame lacked its `event` or its `data` line.
    MissingField,
}

impl fmt::Display for SseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidUtf8 => "event stream frame is not valid UTF-8",
            Self::BufferTooLarge => "event stream buffer exceeds its bound",
            Self::FrameTooLarge => "event stream frame exceeds its bound",
            Self::UnsupportedField => "event stream frame has a duplicate or unsupported field",
            Self::MissingField => "event stream frame omitted its event or data",
        })
    }
}

impl std::error::Error for SseError {}

/// Locate the first blank-line boundary `\r?\n\r?\n` at or after `from`.
/// Returns the index where the boundary starts and the index just past it.
fn find_boundary(buffer: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut start = from;
    while start < buffer.len() {
        let mut cursor = start;
        if buffer[cursor] == b'\r' {
            cursor += 1;
        }
        if buffer.get(cursor) == Some(&b'\n') {
            cursor += 1;
            if buffer.get(cursor) == Some(&b'\r') {
                cursor += 1;
            }
            if buffer.get(cursor) == Some(&b'\n') {
                return Some((start, cursor + 1));
            }
        }
        start += 1;
    }
    None
}

/// Incremental frame parser. Feed chunks with [`SseParser::push`] and pull
/// frames with [`SseParser::next_frame`].
#[derive(Debug)]
pub struct SseParser {
    mode: SseMode,
    buffer: Vec<u8>,
    scan_from: usize,
}

impl SseParser {
    pub fn new(mode: SseMode) -> Self {
        Self {
            mode,
            buffer: Vec::new(),
            scan_from: 0,
        }
    }

    /// Append bytes exactly as read.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// True when unparsed non-whitespace bytes remain: the stream ended in
    /// the middle of a frame.
    pub fn has_partial_frame(&self) -> bool {
        self.buffer.iter().any(|byte| !byte.is_ascii_whitespace())
    }

    /// The next complete frame, or `None` when more bytes are needed. Frames
    /// that carry nothing (comment-only, or lenient frames with no data) are
    /// consumed silently.
    pub fn next_frame(&mut self) -> Result<Option<SseFrame>, SseError> {
        loop {
            let Some((end, next)) = find_boundary(&self.buffer, self.scan_from) else {
                let limit = match self.mode {
                    SseMode::Lenient => LENIENT_BUFFER_LIMIT,
                    SseMode::Strict => STRICT_FRAME_LIMIT + 4,
                };
                if self.buffer.len() > limit {
                    return Err(match self.mode {
                        SseMode::Lenient => SseError::BufferTooLarge,
                        SseMode::Strict => SseError::FrameTooLarge,
                    });
                }
                // A boundary may straddle the end of the buffer: rescan the
                // last bytes next time.
                self.scan_from = self.buffer.len().saturating_sub(3);
                return Ok(None);
            };
            if self.mode == SseMode::Strict && end > STRICT_FRAME_LIMIT {
                return Err(SseError::FrameTooLarge);
            }
            let raw: Vec<u8> = self.buffer.drain(..next).take(end).collect();
            self.scan_from = 0;
            let text = String::from_utf8(raw).map_err(|_| SseError::InvalidUtf8)?;
            let frame = match self.mode {
                SseMode::Lenient => parse_lenient(&text),
                SseMode::Strict => parse_strict(&text)?,
            };
            if let Some(frame) = frame {
                return Ok(Some(frame));
            }
        }
    }
}

fn strip_one_space(value: &str) -> &str {
    value.strip_prefix(' ').unwrap_or(value)
}

fn parse_lenient(text: &str) -> Option<SseFrame> {
    let mut event = "message";
    let mut data = String::new();
    let mut has_data = false;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, strip_one_space(value)),
            None => (line, ""),
        };
        match field {
            "event" => event = value,
            "data" => {
                if has_data {
                    data.push('\n');
                }
                data.push_str(value);
                has_data = true;
            }
            _ => {}
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseFrame {
        event: event.to_owned(),
        data,
    })
}

fn parse_strict(text: &str) -> Result<Option<SseFrame>, SseError> {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if text.is_empty() || lines.iter().all(|line| line.starts_with(':')) {
        return Ok(None);
    }
    let mut event: Option<&str> = None;
    let mut data: Option<&str> = None;
    for line in lines {
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event: ")
            && event.is_none()
        {
            event = Some(value);
        } else if let Some(value) = line.strip_prefix("data: ")
            && data.is_none()
        {
            data = Some(value);
        } else {
            return Err(SseError::UnsupportedField);
        }
    }
    match (event, data) {
        (Some(event), Some(data)) if !event.is_empty() => Ok(Some(SseFrame {
            event: event.to_owned(),
            data: data.to_owned(),
        })),
        _ => Err(SseError::MissingField),
    }
}

/// Why a stream read stopped with an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// The bytes were not a valid stream.
    Parse(SseError),
    /// The transport failed mid-stream.
    Transport(RuntimeError),
}

impl fmt::Display for StreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => error.fmt(formatter),
            Self::Transport(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for StreamError {}

/// A [`ByteSource`] read as frames.
pub struct SseReader<S> {
    source: S,
    parser: SseParser,
    ended: bool,
}

impl<S: ByteSource> SseReader<S> {
    pub fn new(source: S, mode: SseMode) -> Self {
        Self {
            source,
            parser: SseParser::new(mode),
            ended: false,
        }
    }

    /// The next frame; `Ok(None)` at a clean end of stream. Cancel-safe: a
    /// dropped call loses no bytes (they stay in the parser's buffer).
    pub async fn next_frame(&mut self) -> Result<Option<SseFrame>, StreamError> {
        loop {
            if let Some(frame) = self.parser.next_frame().map_err(StreamError::Parse)? {
                return Ok(Some(frame));
            }
            if self.ended {
                return Ok(None);
            }
            match self.source.next_chunk().await {
                Ok(Some(chunk)) => self.parser.push(&chunk),
                Ok(None) => self.ended = true,
                Err(error) => return Err(StreamError::Transport(error)),
            }
        }
    }

    /// True when the stream ended inside a frame.
    pub fn ended_mid_frame(&self) -> bool {
        self.ended && self.parser.has_partial_frame()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lenient(chunks: &[&[u8]]) -> Vec<SseFrame> {
        let mut parser = SseParser::new(SseMode::Lenient);
        let mut frames = Vec::new();
        for chunk in chunks {
            parser.push(chunk);
            while let Some(frame) = parser.next_frame().unwrap() {
                frames.push(frame);
            }
        }
        frames
    }

    #[test]
    fn waits_for_split_utf8_and_accepts_lf_or_crlf_frames() {
        let mut parser = SseParser::new(SseMode::Lenient);
        let mut head = b"event: history\ndata: caf".to_vec();
        head.push(0xc3);
        parser.push(&head);
        assert_eq!(parser.next_frame().unwrap(), None);
        parser.push(&[0xa9, b'\n', b'\n']);
        let first = parser.next_frame().unwrap().unwrap();
        assert_eq!(first.event, "history");
        assert_eq!(first.data, "café");
        parser.push(b"event: next\r\ndata: {\"ok\":true}\r\n\r");
        assert_eq!(parser.next_frame().unwrap(), None);
        parser.push(b"\n");
        let second = parser.next_frame().unwrap().unwrap();
        assert_eq!(second.event, "next");
        assert_eq!(second.data, "{\"ok\":true}");
    }

    #[test]
    fn lenient_frames_default_to_message_and_join_data_lines() {
        let frames =
            lenient(&[b"data: a\ndata: b\nid: 7\nretry: 5\n\n: keepalive\n\nevent: x\n\n"]);
        assert_eq!(
            frames,
            [SseFrame {
                event: "message".into(),
                data: "a\nb".into()
            }]
        );
    }

    #[test]
    fn lenient_delimiter_split_across_chunks() {
        let frames = lenient(&[b"event: a\ndata: 1\n", b"\nevent: b\ndata: 2\r", b"\n\r\n"]);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].event, "b");
    }

    #[test]
    fn lenient_buffer_is_bounded() {
        let mut parser = SseParser::new(SseMode::Lenient);
        parser.push(&vec![b'x'; LENIENT_BUFFER_LIMIT + 1]);
        assert_eq!(parser.next_frame(), Err(SseError::BufferTooLarge));
    }

    fn strict(text: &str) -> Result<Option<SseFrame>, SseError> {
        let mut parser = SseParser::new(SseMode::Strict);
        parser.push(text.as_bytes());
        parser.next_frame()
    }

    #[test]
    fn strict_accepts_one_event_and_one_data_line() {
        let frame = strict("event: e\ndata: {\"a\":1}\n\n").unwrap().unwrap();
        assert_eq!(
            (frame.event.as_str(), frame.data.as_str()),
            ("e", "{\"a\":1}")
        );
        assert_eq!(strict(": keepalive\n\n").unwrap(), None);
    }

    #[test]
    fn strict_refuses_duplicate_and_foreign_fields() {
        assert_eq!(
            strict("event: a\nevent: b\ndata: {}\n\n"),
            Err(SseError::UnsupportedField)
        );
        assert_eq!(
            strict("id: 1\nevent: a\ndata: {}\n\n"),
            Err(SseError::UnsupportedField)
        );
        assert_eq!(
            strict("retry: 5\nevent: a\ndata: {}\n\n"),
            Err(SseError::UnsupportedField)
        );
        assert_eq!(
            strict("event: a\ndata:{}\n\n"),
            Err(SseError::UnsupportedField)
        );
        assert_eq!(strict("event: a\n\n"), Err(SseError::MissingField));
    }

    #[test]
    fn strict_frame_bound_applies_before_decoding() {
        let mut text = String::from("event: a\ndata: ");
        text.push_str(&"x".repeat(STRICT_FRAME_LIMIT));
        text.push_str("\n\n");
        assert_eq!(strict(&text), Err(SseError::FrameTooLarge));
    }

    #[test]
    fn strict_crlf_split_one_byte_at_a_time() {
        let mut parser = SseParser::new(SseMode::Strict);
        let mut frames = Vec::new();
        for byte in "event: a\r\ndata: {}\r\n\r\n".bytes() {
            parser.push(&[byte]);
            while let Some(frame) = parser.next_frame().unwrap() {
                frames.push(frame);
            }
        }
        assert_eq!(frames.len(), 1);
    }
}
