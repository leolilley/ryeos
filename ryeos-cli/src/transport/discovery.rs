use http_body_util::BodyExt;

use crate::error::CliTransportError;

/// Discover the daemon's principal_id by calling GET /public-key.
/// This is the audience value used in request signing.
pub async fn discover_audience(daemon_url: &str) -> Result<String, CliTransportError> {
    let url = format!("{}/public-key", daemon_url.trim_end_matches('/'));
    let uri: hyper::Uri = url
        .parse()
        .map_err(|e| CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("invalid URL: {e}"),
        })?;

    let host = uri.host().unwrap_or("127.0.0.1");
    let port = uri.port_u16().unwrap_or(80);
    let bind = format!("{host}:{port}");

    let req = hyper::Request::builder()
        .method("GET")
        .uri(uri.to_string())
        .header("host", &bind)
        .header("accept", "application/json")
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .map_err(|e| CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("failed to build request: {e}"),
        })?;

    let stream = tokio::net::TcpStream::connect(&bind)
        .await
        .map_err(|e| CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("TCP connect: {e}"),
        })?;

    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("HTTP handshake: {e}"),
        })?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::warn!("discovery connection task error: {e}");
        }
    });

    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("request send: {e}"),
        })?;

    let status = resp.status();
    let body_bytes = collect_body(resp.into_body()).await?;

    if !status.is_success() {
        let body_str = String::from_utf8_lossy(&body_bytes);
        return Err(CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("HTTP {}: {}", status, body_str.trim()),
        });
    }

    let value: serde_json::Value = serde_json::from_slice(&body_bytes).map_err(|e| {
        CliTransportError::AudienceDiscoveryFailed {
            url: url.clone(),
            detail: format!("JSON decode: {e}"),
        }
    })?;

    value
        .get("principal_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| CliTransportError::AudienceDiscoveryFailed {
            url,
            detail: "response missing 'principal_id' field".into(),
        })
}

async fn collect_body(
    body: hyper::body::Incoming,
) -> Result<Vec<u8>, CliTransportError> {
    use hyper::body::Body;
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
