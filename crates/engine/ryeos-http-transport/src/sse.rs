//! Finite Server-Sent Events parser over any bounded byte reader.

use lillux::network::NetworkCancellation;
use lillux::time::MonotonicDeadline;
use std::io::{self, Read};

const MAX_SSE_LINE_BYTES: usize = 64 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_SSE_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SSE_EVENTS: u64 = 65_536;

/// Independent line, event, total-byte and event-count ceilings.
#[derive(Clone, Copy, Debug)]
pub struct SseLimits {
    /// Maximum bytes in one line, excluding its line ending.
    pub line_bytes: usize,
    /// Maximum wire bytes in one event, including field lines and comments.
    pub event_bytes: usize,
    /// Maximum wire bytes consumed across the whole stream.
    pub total_bytes: u64,
    /// Maximum number of dispatched events.
    pub events: u64,
}

impl SseLimits {
    /// Conservative finite defaults for control-plane event streams.
    pub const fn control_plane() -> Self {
        Self {
            line_bytes: 16 * 1024,
            event_bytes: 64 * 1024,
            total_bytes: 1024 * 1024,
            events: 4096,
        }
    }
}

/// One dispatched SSE event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    /// Event name, or `None` for the protocol's default `message` type.
    pub event: Option<String>,
    /// Data lines joined with a newline.
    pub data: String,
    /// Last event ID known at this point in the stream.
    pub id: Option<String>,
    /// Latest valid retry delay in milliseconds, if supplied.
    pub retry_ms: Option<u64>,
}

/// Bounded SSE parser. Use `with_operation_policy` for operation-scoped input;
/// it checks the caller's cancellation and absolute deadline even when this
/// parser still has prefetched bytes.
pub struct SseReader<R> {
    source: R,
    limits: SseLimits,
    cancellation: Option<NetworkCancellation>,
    absolute_deadline: Option<MonotonicDeadline>,
    input: [u8; 4096],
    input_len: usize,
    input_at: usize,
    source_eof: bool,
    total_bytes: u64,
    line: Vec<u8>,
    first_line: bool,
    event_data: Vec<u8>,
    has_data: bool,
    event_name: Option<String>,
    last_event_id: Option<String>,
    retry_ms: Option<u64>,
    event_wire_bytes: usize,
    dispatched: u64,
    finished: bool,
    failed: bool,
}

impl<R: Read> SseReader<R> {
    /// Wrap a streaming response body with finite parser budgets.
    pub fn new(source: R, limits: SseLimits) -> io::Result<Self> {
        Self::build(source, limits, None, None)
    }

    /// Wrap operation-scoped input and recheck cancellation/deadline before
    /// parsing or exposing each event, including events from prefetched bytes.
    /// Keep `limits.total_bytes` at or below the enclosing decoded HTTP body
    /// limit; account for HTTP framing separately in its wire-byte ceiling.
    pub fn with_operation_policy(
        source: R,
        limits: SseLimits,
        cancellation: NetworkCancellation,
        absolute_deadline: MonotonicDeadline,
    ) -> io::Result<Self> {
        Self::build(source, limits, Some(cancellation), Some(absolute_deadline))
    }

    fn build(
        source: R,
        limits: SseLimits,
        cancellation: Option<NetworkCancellation>,
        absolute_deadline: Option<MonotonicDeadline>,
    ) -> io::Result<Self> {
        if limits.line_bytes == 0
            || limits.event_bytes == 0
            || limits.total_bytes == 0
            || limits.events == 0
            || limits.line_bytes > MAX_SSE_LINE_BYTES
            || limits.event_bytes > MAX_SSE_EVENT_BYTES
            || limits.total_bytes > MAX_SSE_TOTAL_BYTES
            || limits.events > MAX_SSE_EVENTS
            || limits.line_bytes as u128 > limits.event_bytes as u128
            || limits.event_bytes as u128 > limits.total_bytes as u128
            || limits.events > limits.total_bytes
        {
            return Err(invalid("SSE limits are invalid or exceed their bounds"));
        }
        Ok(Self {
            source,
            limits,
            cancellation,
            absolute_deadline,
            input: [0; 4096],
            input_len: 0,
            input_at: 0,
            source_eof: false,
            total_bytes: 0,
            line: Vec::new(),
            first_line: true,
            event_data: Vec::new(),
            has_data: false,
            event_name: None,
            last_event_id: None,
            retry_ms: None,
            event_wire_bytes: 0,
            dispatched: 0,
            finished: false,
            failed: false,
        })
    }

    fn check_policy(&self) -> io::Result<()> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(NetworkCancellation::is_cancelled)
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "SSE operation was cancelled",
            ));
        }
        if self
            .absolute_deadline
            .is_some_and(|deadline| deadline.has_elapsed())
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSE operation deadline elapsed",
            ));
        }
        Ok(())
    }

    /// Read the next event, returning `None` after a clean end of stream.
    pub fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "SSE parser is settled after an earlier error",
            ));
        }
        if let Err(error) = self.check_policy() {
            self.failed = true;
            return Err(error);
        }
        let result = self.next_event_inner();
        match result {
            Ok(event) => {
                if let Err(error) = self.check_policy() {
                    self.failed = true;
                    return Err(error);
                }
                Ok(event)
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn next_event_inner(&mut self) -> io::Result<Option<SseEvent>> {
        if self.finished {
            return Ok(None);
        }
        loop {
            self.check_policy()?;
            let Some((mut line, wire_bytes)) = self.read_line()? else {
                self.finished = true;
                return self.dispatch_at_eof();
            };
            if self.first_line {
                self.first_line = false;
                if line.starts_with(&[0xef, 0xbb, 0xbf]) {
                    line.drain(..3);
                }
            }
            self.event_wire_bytes = self.event_wire_bytes.saturating_add(wire_bytes);
            if self.event_wire_bytes > self.limits.event_bytes {
                return Err(invalid("SSE event exceeds its bound"));
            }
            if line.is_empty() {
                let event = self.dispatch()?;
                self.event_wire_bytes = 0;
                if event.is_some() {
                    return Ok(event);
                }
                continue;
            }
            if line[0] == b':' {
                continue;
            }
            self.consume_field(&line)?;
        }
    }

    fn read_line(&mut self) -> io::Result<Option<(Vec<u8>, usize)>> {
        self.line.clear();
        let mut wire_bytes = 0usize;
        loop {
            let Some(byte) = self.next_byte()? else {
                if self.line.is_empty() {
                    return Ok(None);
                }
                return Ok(Some((std::mem::take(&mut self.line), wire_bytes)));
            };
            wire_bytes = wire_bytes.saturating_add(1);
            if byte == b'\n' {
                return Ok(Some((std::mem::take(&mut self.line), wire_bytes)));
            }
            if byte == b'\r' {
                if self.peek_byte()?.is_some_and(|next| next == b'\n') {
                    let _ = self.next_byte()?;
                    wire_bytes = wire_bytes.saturating_add(1);
                }
                return Ok(Some((std::mem::take(&mut self.line), wire_bytes)));
            }
            self.line.push(byte);
            if self.line.len() > self.limits.line_bytes {
                return Err(invalid("SSE line exceeds its bound"));
            }
        }
    }

    fn next_byte(&mut self) -> io::Result<Option<u8>> {
        let Some(byte) = self.peek_byte()? else {
            return Ok(None);
        };
        self.input_at += 1;
        if self.total_bytes >= self.limits.total_bytes {
            return Err(invalid("SSE stream exceeds its byte bound"));
        }
        self.total_bytes += 1;
        Ok(Some(byte))
    }

    fn peek_byte(&mut self) -> io::Result<Option<u8>> {
        if self.input_at == self.input_len {
            if self.source_eof {
                return Ok(None);
            }
            let remaining = self.limits.total_bytes.saturating_sub(self.total_bytes);
            if remaining == 0 {
                return Err(invalid(
                    "SSE stream reached its byte bound before clean EOF",
                ));
            }
            let capacity = self
                .input
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            self.input_len = self.source.read(&mut self.input[..capacity])?;
            self.input_at = 0;
            if self.input_len == 0 {
                self.source_eof = true;
                return Ok(None);
            }
        }
        Ok(Some(self.input[self.input_at]))
    }

    fn consume_field(&mut self, bytes: &[u8]) -> io::Result<()> {
        let line =
            std::str::from_utf8(bytes).map_err(|_| invalid("SSE field is not valid UTF-8"))?;
        let (field, mut value) = match line.split_once(':') {
            Some((field, rest)) => (field, rest),
            None => (line, ""),
        };
        if let Some(rest) = value.strip_prefix(' ') {
            value = rest;
        }
        match field {
            "data" => {
                self.event_data.extend_from_slice(value.as_bytes());
                self.event_data.push(b'\n');
                self.has_data = true;
            }
            "event" => self.event_name = Some(value.to_owned()),
            "id" if !value.contains('\0') => self.last_event_id = Some(value.to_owned()),
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                if let Ok(retry) = value.parse::<u64>() {
                    self.retry_ms = Some(retry);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self) -> io::Result<Option<SseEvent>> {
        if !self.has_data {
            self.event_name = None;
            return Ok(None);
        }
        self.dispatched = self.dispatched.saturating_add(1);
        if self.dispatched > self.limits.events {
            return Err(invalid("SSE event count exceeds its bound"));
        }
        if self.event_data.last() == Some(&b'\n') {
            self.event_data.pop();
        }
        let data = String::from_utf8(std::mem::take(&mut self.event_data))
            .map_err(|_| invalid("SSE event data is not valid UTF-8"))?;
        self.has_data = false;
        let event = SseEvent {
            event: self.event_name.take().filter(|name| !name.is_empty()),
            data,
            id: self.last_event_id.clone(),
            retry_ms: self.retry_ms,
        };
        Ok(Some(event))
    }

    fn dispatch_at_eof(&mut self) -> io::Result<Option<SseEvent>> {
        if self.has_data {
            self.dispatch()
        } else {
            Ok(None)
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_crlf_fields_and_persistent_id_with_finite_budgets() {
        let bytes = b"id: run-4\r\nevent: ready\r\ndata: one\r\ndata: two\r\n\r\ndata: last";
        let mut reader = SseReader::new(Cursor::new(bytes), SseLimits::control_plane()).unwrap();
        assert_eq!(
            reader.next_event().unwrap(),
            Some(SseEvent {
                event: Some("ready".into()),
                data: "one\ntwo".into(),
                id: Some("run-4".into()),
                retry_ms: None,
            })
        );
        assert_eq!(
            reader.next_event().unwrap(),
            Some(SseEvent {
                event: None,
                data: "last".into(),
                id: Some("run-4".into()),
                retry_ms: None,
            })
        );
        assert_eq!(reader.next_event().unwrap(), None);
    }

    #[test]
    fn rejects_line_event_total_and_event_count_overflows() {
        let mut limits = SseLimits::control_plane();
        limits.line_bytes = 3;
        assert!(
            SseReader::new(Cursor::new(b"data: four\n\n"), limits)
                .unwrap()
                .next_event()
                .is_err()
        );

        let mut limits = SseLimits::control_plane();
        limits.event_bytes = 8;
        limits.line_bytes = 8;
        assert!(
            SseReader::new(Cursor::new(b"data: x\n\n"), limits)
                .unwrap()
                .next_event()
                .is_err()
        );

        let mut limits = SseLimits::control_plane();
        limits.total_bytes = 3;
        limits.event_bytes = 3;
        limits.line_bytes = 3;
        limits.events = 3;
        assert!(
            SseReader::new(Cursor::new(b": \n"), limits)
                .unwrap()
                .next_event()
                .is_err()
        );

        let mut limits = SseLimits::control_plane();
        limits.events = 1;
        let mut reader = SseReader::new(Cursor::new(b"data: a\n\ndata: b\n\n"), limits).unwrap();
        assert!(reader.next_event().unwrap().is_some());
        assert!(reader.next_event().is_err());
    }
}
