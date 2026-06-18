use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::Request;
use serde_json::Value;

use crate::error::{CliDispatchError, CliTransportError};
use crate::transport::signing::SignHeaders;

/// POST JSON to the daemon and return the response body as `Value`.
///
/// `url` is a full URL like `http://127.0.0.1:7400/execute`.
pub async fn post_json(
    url: &str,
    headers: &SignHeaders,
    body: &[u8],
) -> Result<Value, CliDispatchError> {
    let uri: hyper::Uri = url.parse().map_err(|e| CliTransportError::Unreachable {
        bind: url.to_string(),
        detail: format!("invalid URL: {e}"),
    })?;

    let host = uri.host().unwrap_or("127.0.0.1");
    let port = uri.port_u16().unwrap_or(80);
    let bind = format!("{host}:{port}");

    let req = Request::builder()
        .method("POST")
        .uri(uri.to_string())
        .header("content-type", "application/json")
        .header("host", &bind)
        .header("x-ryeos-key-id", &headers.key_id)
        .header("x-ryeos-timestamp", &headers.timestamp)
        .header("x-ryeos-nonce", &headers.nonce)
        .header("x-ryeos-signature", &headers.signature)
        .body(Full::new(Bytes::from(body.to_vec())))
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: format!("failed to build request: {e}"),
        })?;

    let stream = tokio::net::TcpStream::connect(&bind).await.map_err(|e| {
        CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: e.to_string(),
        }
    })?;

    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: format!("HTTP handshake: {e}"),
        })?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::warn!("connection task error: {e}");
        }
    });

    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: format!("request send: {e}"),
        })?;

    let status = resp.status();
    let body_bytes = collect_body(resp.into_body()).await?;

    if !status.is_success() {
        let body_str = String::from_utf8_lossy(&body_bytes);
        return Err(CliTransportError::HttpError {
            status: status.as_u16(),
            body: body_str.into_owned(),
        }
        .into());
    }

    let value: Value =
        serde_json::from_slice(&body_bytes).map_err(|e| CliTransportError::BodyDecode {
            detail: format!("{e}"),
        })?;

    Ok(value)
}

/// Signed GET to the daemon, returning the response body as `Value`.
pub async fn get_json(url: &str, headers: &SignHeaders) -> Result<Value, CliDispatchError> {
    let uri: hyper::Uri = url.parse().map_err(|e| CliTransportError::Unreachable {
        bind: url.to_string(),
        detail: format!("invalid URL: {e}"),
    })?;
    let host = uri.host().unwrap_or("127.0.0.1");
    let port = uri.port_u16().unwrap_or(80);
    let bind = format!("{host}:{port}");

    let req = Request::builder()
        .method("GET")
        .uri(uri.to_string())
        .header("host", &bind)
        .header("x-ryeos-key-id", &headers.key_id)
        .header("x-ryeos-timestamp", &headers.timestamp)
        .header("x-ryeos-nonce", &headers.nonce)
        .header("x-ryeos-signature", &headers.signature)
        .body(Full::new(Bytes::new()))
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: format!("failed to build request: {e}"),
        })?;

    let stream = tokio::net::TcpStream::connect(&bind).await.map_err(|e| {
        CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: e.to_string(),
        }
    })?;
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: format!("HTTP handshake: {e}"),
        })?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::warn!("connection task error: {e}");
        }
    });

    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: format!("request send: {e}"),
        })?;
    let status = resp.status();
    let body_bytes = collect_body(resp.into_body()).await?;
    if !status.is_success() {
        return Err(CliTransportError::HttpError {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&body_bytes).into_owned(),
        }
        .into());
    }
    serde_json::from_slice(&body_bytes)
        .map_err(|e| CliTransportError::BodyDecode { detail: format!("{e}") }.into())
}

/// One parsed Server-Sent Event.
#[derive(Debug, Clone, Default)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
}

/// POST JSON to the daemon and stream the SSE response, invoking `on_event` for
/// each complete event as it arrives. `on_event` returns `true` to stop reading
/// (e.g. on a terminal event) — the terminal-event policy lives in the caller,
/// not the transport. Returns when the caller stops or the stream closes.
pub async fn post_json_streaming(
    url: &str,
    headers: &SignHeaders,
    body: &[u8],
    mut on_event: impl FnMut(&SseEvent) -> bool,
) -> Result<(), CliDispatchError> {
    let uri: hyper::Uri = url.parse().map_err(|e| CliTransportError::Unreachable {
        bind: url.to_string(),
        detail: format!("invalid URL: {e}"),
    })?;
    let host = uri.host().unwrap_or("127.0.0.1");
    let port = uri.port_u16().unwrap_or(80);
    let bind = format!("{host}:{port}");

    let req = Request::builder()
        .method("POST")
        .uri(uri.to_string())
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("host", &bind)
        .header("x-ryeos-key-id", &headers.key_id)
        .header("x-ryeos-timestamp", &headers.timestamp)
        .header("x-ryeos-nonce", &headers.nonce)
        .header("x-ryeos-signature", &headers.signature)
        .body(Full::new(Bytes::from(body.to_vec())))
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: format!("failed to build request: {e}"),
        })?;

    let stream = tokio::net::TcpStream::connect(&bind).await.map_err(|e| {
        CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: e.to_string(),
        }
    })?;
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: format!("HTTP handshake: {e}"),
        })?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::warn!("connection task error: {e}");
        }
    });

    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: format!("request send: {e}"),
        })?;
    let status = resp.status();
    let is_event_stream = resp
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("text/event-stream"))
        .unwrap_or(false);
    if !status.is_success() {
        let body_bytes = collect_body(resp.into_body()).await?;
        return Err(CliTransportError::HttpError {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&body_bytes).into_owned(),
        }
        .into());
    }
    // A 2xx that is not an event stream (e.g. a JSON error body) would otherwise
    // be consumed with zero frames and exit success — surface it instead.
    if !is_event_stream {
        let body_bytes = collect_body(resp.into_body()).await?;
        return Err(CliTransportError::BodyDecode {
            detail: format!(
                "expected text/event-stream, got non-SSE 2xx response: {}",
                String::from_utf8_lossy(&body_bytes)
            ),
        }
        .into());
    }

    let mut body = resp.into_body();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| CliTransportError::BodyDecode {
            detail: format!("stream frame: {e}"),
        })?;
        let Some(data) = frame.data_ref() else {
            continue;
        };
        buf.extend_from_slice(data);
        // Drain every complete event (terminated by a blank line, LF or CRLF)
        // from the front of the buffer, leaving any partial tail for the next
        // frame.
        while let Some(end) = find_event_end(&buf) {
            let block: Vec<u8> = buf.drain(..end).collect();
            if let Some(ev) = parse_sse_block(&block) {
                if on_event(&ev) {
                    return Ok(());
                }
            }
        }
    }
    // Tolerate a final event not terminated by a blank line at EOF.
    if !buf.is_empty() {
        if let Some(ev) = parse_sse_block(&buf) {
            on_event(&ev);
        }
    }
    Ok(())
}

/// Match an SSE line terminator at `i`: `\r\n` (2 bytes), or a bare `\n`/`\r`
/// (1 byte). `None` if no terminator starts at `i`.
fn match_terminator(buf: &[u8], i: usize) -> Option<usize> {
    if i + 1 < buf.len() && buf[i] == b'\r' && buf[i + 1] == b'\n' {
        Some(2)
    } else if i < buf.len() && (buf[i] == b'\n' || buf[i] == b'\r') {
        Some(1)
    } else {
        None
    }
}

/// Index just past the first SSE event boundary — a blank line, i.e. two
/// consecutive line terminators. Each terminator may be `\r\n`, `\n`, or `\r`,
/// so this accepts `\n\n`, `\r\n\r\n`, and mixed forms like `\n\r\n`. Returns
/// `None` when no complete boundary is present yet (partial frame).
fn find_event_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < buf.len() {
        if let Some(first) = match_terminator(buf, i) {
            if let Some(second) = match_terminator(buf, i + first) {
                return Some(i + first + second);
            }
            i += first;
        } else {
            i += 1;
        }
    }
    None
}

fn parse_sse_block(block: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(block);
    let mut ev = SseEvent::default();
    let mut data_lines: Vec<&str> = Vec::new();
    let mut saw_field = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            ev.event = rest.trim().to_string();
            saw_field = true;
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
            saw_field = true;
        } else if let Some(rest) = line.strip_prefix("id:") {
            ev.id = Some(rest.trim().to_string());
            saw_field = true;
        }
        // ignore comments (`:`) and unknown fields
    }
    if !saw_field {
        return None;
    }
    ev.data = data_lines.join("\n");
    Some(ev)
}

async fn collect_body(body: Incoming) -> Result<Vec<u8>, CliTransportError> {
    let mut bufs = Vec::new();
    let mut body = body;
    while let Some(chunk) = body.frame().await {
        let frame = chunk.map_err(|e| CliTransportError::BodyDecode {
            detail: format!("stream frame: {e}"),
        })?;
        if let Some(data) = frame.data_ref() {
            bufs.extend_from_slice(data);
        }
    }
    Ok(bufs)
}

/// Resolve the daemon URL. Priority:
///   1. RYEOSD_URL env var
///   2. daemon.json bind discovery (existing path)
pub async fn resolve_daemon_url(app_root: &std::path::Path) -> Result<String, CliTransportError> {
    if let Ok(url) = std::env::var("RYEOSD_URL") {
        return Ok(url.trim_end_matches('/').to_string());
    }
    let bind = read_daemon_bind(app_root).await?;
    Ok(format!("http://{bind}"))
}

/// Read `daemon.json` from the app root and return the bind address.
pub async fn read_daemon_bind(app_root: &std::path::Path) -> Result<String, CliTransportError> {
    let path = app_root.join("daemon.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|_| CliTransportError::DaemonJsonMissing { path: path.clone() })?;
    let v: Value =
        serde_json::from_str(&raw).map_err(|e| CliTransportError::DaemonJsonMalformed {
            detail: e.to_string(),
        })?;
    v.get("bind")
        .and_then(|x| x.as_str())
        .map(String::from)
        .ok_or_else(|| CliTransportError::DaemonJsonMalformed {
            detail: "missing 'bind' field".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_block_reads_event_data_id() {
        let ev = parse_sse_block(b"event: thread_completed\ndata: {\"ok\":true}\nid: 7\n\n")
            .expect("event");
        assert_eq!(ev.event, "thread_completed");
        assert_eq!(ev.data, "{\"ok\":true}");
        assert_eq!(ev.id.as_deref(), Some("7"));
    }

    #[test]
    fn parse_sse_block_joins_multiline_data() {
        let ev = parse_sse_block(b"event: x\ndata: a\ndata: b\n\n").expect("event");
        assert_eq!(ev.data, "a\nb");
    }

    #[test]
    fn parse_sse_block_ignores_comment_only() {
        assert!(parse_sse_block(b": keep-alive\n\n").is_none());
    }

    #[test]
    fn find_event_end_supports_lf_crlf_and_mixed() {
        let cases: &[&[u8]] = &[
            b"data: y\n\nrest",        // LF
            b"data: y\r\n\r\nrest",    // CRLF
            b"data: y\n\r\nrest",      // mixed LF then CRLF
            b"data: y\r\n\nrest",      // mixed CRLF then LF
            b"data: y\r\rrest",        // bare CR pair
        ];
        for raw in cases {
            let end = find_event_end(raw).unwrap_or_else(|| panic!("boundary in {raw:?}"));
            // Everything before the boundary, with terminators stripped, is the event.
            assert!(
                String::from_utf8_lossy(&raw[..end]).contains("data: y"),
                "boundary too short for {raw:?}"
            );
            assert_eq!(&raw[end..], b"rest", "boundary wrong for {raw:?}");
        }
        // No complete boundary yet (single trailing terminator).
        assert_eq!(find_event_end(b"data: y\n"), None);
    }
}
