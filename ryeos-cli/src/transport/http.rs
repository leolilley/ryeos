use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::Request;
use serde_json::Value;

use crate::error::{CliDispatchError, CliTransportError};
use crate::transport::signing::SignHeaders;

/// POST JSON to the daemon and return the response body as `Value`.
pub async fn post_json(
    bind: &str,
    headers: &SignHeaders,
    body: &[u8],
) -> Result<Value, CliDispatchError> {
    let url = format!("http://{bind}/execute");
    let req = Request::builder()
        .method("POST")
        .uri(&url)
        .header("content-type", "application/json")
        .header("x-rye-key-id", &headers.key_id)
        .header("x-rye-timestamp", &headers.timestamp)
        .header("x-rye-nonce", &headers.nonce)
        .header("x-rye-signature", &headers.signature)
        .body(Full::new(Bytes::from(body.to_vec())))
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.to_string(),
            detail: format!("failed to build request: {e}"),
        })?;

    let stream = tokio::net::TcpStream::connect(bind)
        .await
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.to_string(),
            detail: e.to_string(),
        })?;

    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.to_string(),
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
            bind: bind.to_string(),
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

    let value: Value = serde_json::from_slice(&body_bytes).map_err(|e| {
        CliTransportError::BodyDecode {
            detail: format!("{e}"),
        }
    })?;

    Ok(value)
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

/// Read `daemon.json` from the state dir and return the bind address.
pub async fn read_daemon_bind(state_dir: &std::path::Path) -> Result<String, CliTransportError> {
    let path = state_dir.join("daemon.json");
    let raw = std::fs::read_to_string(&path).map_err(|_| CliTransportError::DaemonJsonMissing {
        path: path.clone(),
    })?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| CliTransportError::DaemonJsonMalformed {
        detail: e.to_string(),
    })?;
    v.get("bind")
        .and_then(|x| x.as_str())
        .map(String::from)
        .ok_or_else(|| CliTransportError::DaemonJsonMalformed {
            detail: "missing 'bind' field".into(),
        })
}
