use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::LIFECYCLE_FRAME_MAX_BYTES;

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Serialize)]
struct RpcRequest<'a> {
    request_id: u64,
    method: &'a str,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: String,
    message: String,
}

/// Marker error for a lifecycle RPC that exhausted its bound — a
/// live-but-busy peer (or a full listener backlog). Callers downcast to
/// keep this distinct from refused/missing-socket failures, which fail
/// fast with io errors: the two conditions have opposite remediations,
/// and collapsing them misreports a busy daemon as a dead one.
#[derive(Debug)]
pub struct ControlCallTimeout {
    pub timeout: Duration,
    pub method: String,
    pub uds_path: std::path::PathBuf,
}

impl std::fmt::Display for ControlCallTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "lifecycle rpc timed out after {:?} for {} on {}",
            self.timeout,
            self.method,
            self.uds_path.display()
        )
    }
}

impl std::error::Error for ControlCallTimeout {}

/// Classified connect failure. Only `NotFound` and `ConnectionRefused` prove
/// that no listener accepted the probe; permission/resource failures remain
/// uncertain and must not authorize replacement startup.
#[derive(Debug)]
pub struct ControlConnectError {
    pub kind: std::io::ErrorKind,
    pub uds_path: std::path::PathBuf,
    pub detail: String,
}

impl std::fmt::Display for ControlConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "connect lifecycle control at {} failed: {}",
            self.uds_path.display(),
            self.detail
        )
    }
}

impl std::error::Error for ControlConnectError {}

/// Marker error for a lifecycle RPC that connected to a peer but could not
/// complete a usable exchange. Once connect succeeds, replacement startup is
/// unsafe even if the peer closes, emits a malformed frame, or rejects the
/// current protocol.
#[derive(Debug)]
pub struct ControlLivePeerError {
    pub method: String,
    pub uds_path: std::path::PathBuf,
    pub detail: String,
}

impl std::fmt::Display for ControlLivePeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "live lifecycle peer failed {} on {}: {}",
            self.method,
            self.uds_path.display(),
            self.detail
        )
    }
}

impl std::error::Error for ControlLivePeerError {}

/// Perform a single lifecycle RPC bounded by `timeout`.
///
/// The timeout wraps the entire round trip (connect + write + read +
/// decode); a wedged peer cannot stall the caller past `timeout`. An
/// elapsed bound surfaces as [`ControlCallTimeout`] so callers can
/// classify busy separately from dead.
pub async fn call(
    uds_path: &Path,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    tokio::time::timeout(timeout, call_inner(uds_path, method, params))
        .await
        .map_err(|_| {
            anyhow::Error::new(ControlCallTimeout {
                timeout,
                method: method.to_string(),
                uds_path: uds_path.to_path_buf(),
            })
        })?
}

async fn call_inner(uds_path: &Path, method: &str, params: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(uds_path).await.map_err(|error| {
        anyhow::Error::new(ControlConnectError {
            kind: error.kind(),
            uds_path: uds_path.to_path_buf(),
            detail: error.to_string(),
        })
    })?;

    call_connected(&mut stream, method, params)
        .await
        .map_err(|error| {
            anyhow::Error::new(ControlLivePeerError {
                method: method.to_owned(),
                uds_path: uds_path.to_path_buf(),
                detail: format!("{error:#}"),
            })
        })
}

async fn call_connected(stream: &mut UnixStream, method: &str, params: Value) -> Result<Value> {
    let request = RpcRequest {
        request_id: REQUEST_ID.fetch_add(1, Ordering::Relaxed),
        method,
        params,
    };
    let encoded = rmp_serde::to_vec_named(&request).context("encode lifecycle rpc")?;
    write_frame(stream, &encoded).await?;
    let frame = read_frame(stream).await?;
    let response: RpcResponse = rmp_serde::from_slice(&frame).context("decode lifecycle rpc")?;
    if let Some(error) = response.error {
        return Err(anyhow!("{}: {}", error.code, error.message));
    }
    Ok(response.result.unwrap_or(Value::Null))
}

async fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> Result<()> {
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("frame too large"))?;
    let len = validate_frame_len(len)?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = validate_frame_len(u32::from_be_bytes(len_buf))?;
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

fn validate_frame_len(frame_len: u32) -> Result<u32> {
    if frame_len > LIFECYCLE_FRAME_MAX_BYTES {
        return Err(anyhow!(
            "frame too large: {} bytes (max {})",
            frame_len,
            LIFECYCLE_FRAME_MAX_BYTES
        ));
    }
    Ok(frame_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_boundary_is_inclusive() {
        assert_eq!(
            validate_frame_len(LIFECYCLE_FRAME_MAX_BYTES).unwrap(),
            LIFECYCLE_FRAME_MAX_BYTES
        );
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        let frame_len = LIFECYCLE_FRAME_MAX_BYTES + 1;
        assert_eq!(
            validate_frame_len(frame_len).unwrap_err().to_string(),
            format!("frame too large: {frame_len} bytes (max {LIFECYCLE_FRAME_MAX_BYTES})")
        );
    }
}
