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

    let stream = tokio::net::TcpStream::connect(&bind)
        .await
        .map_err(|e| CliTransportError::Unreachable {
            bind: bind.clone(),
            detail: e.to_string(),
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

/// Resolve the daemon URL. Priority:
///   1. RYEOSD_URL env var
///   2. daemon.json bind discovery (existing path)
pub async fn resolve_daemon_url(system_space_dir: &std::path::Path) -> Result<String, CliTransportError> {
    if let Ok(url) = std::env::var("RYEOSD_URL") {
        return Ok(url.trim_end_matches('/').to_string());
    }
    let bind = read_daemon_bind(system_space_dir).await?;
    Ok(format!("http://{bind}"))
}

/// Read `daemon.json` from the system space dir and return the bind address.
pub async fn read_daemon_bind(system_space_dir: &std::path::Path) -> Result<String, CliTransportError> {
    let path = system_space_dir.join("daemon.json");
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
