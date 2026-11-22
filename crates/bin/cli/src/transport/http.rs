//! CLI view of the signed HTTP transport: the shared client transport, with
//! the request helpers returning [`CliDispatchError`] so command dispatch
//! composes transport and configuration failures in one error.

use serde_json::Value;

use crate::error::CliDispatchError;
use crate::transport::signing::SignHeaders;

pub use ryeos_client_transport::http::{
    SseEvent, read_daemon_bind, resolve_daemon_url, signed_client,
};

/// POST JSON to the daemon and return the response body as `Value`.
pub async fn post_json(
    url: &str,
    headers: &SignHeaders,
    body: &[u8],
) -> Result<Value, CliDispatchError> {
    Ok(ryeos_client_transport::http::post_json(url, headers, body).await?)
}

/// Signed GET to the daemon, returning the response body as `Value`.
pub async fn get_json(url: &str, headers: &SignHeaders) -> Result<Value, CliDispatchError> {
    Ok(ryeos_client_transport::http::get_json(url, headers).await?)
}

/// POST JSON to the daemon and stream the SSE response; `on_event` returns
/// `true` to stop reading.
pub async fn post_json_streaming(
    url: &str,
    headers: &SignHeaders,
    body: &[u8],
    on_event: impl FnMut(&SseEvent) -> bool,
) -> Result<(), CliDispatchError> {
    Ok(ryeos_client_transport::http::post_json_streaming(url, headers, body, on_event).await?)
}

/// Signed GET that streams an SSE route; `on_event` returns `true` to stop
/// reading.
pub async fn get_streaming(
    url: &str,
    headers: &SignHeaders,
    on_event: impl FnMut(&SseEvent) -> bool,
) -> Result<(), CliDispatchError> {
    Ok(ryeos_client_transport::http::get_streaming(url, headers, on_event).await?)
}
