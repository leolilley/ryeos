use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

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

/// Perform a single lifecycle RPC bounded by `timeout`.
///
/// The timeout wraps the entire round trip (connect + write + read +
/// decode); a wedged peer cannot stall the caller past `timeout`.
pub async fn call(
    uds_path: &Path,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    tokio::time::timeout(timeout, call_inner(uds_path, method, params))
        .await
        .map_err(|_| {
            anyhow!(
                "lifecycle rpc timed out after {:?} for {} on {}",
                timeout,
                method,
                uds_path.display()
            )
        })?
}

async fn call_inner(uds_path: &Path, method: &str, params: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(uds_path)
        .await
        .with_context(|| format!("connect lifecycle control at {}", uds_path.display()))?;

    let request = RpcRequest {
        request_id: REQUEST_ID.fetch_add(1, Ordering::Relaxed),
        method,
        params,
    };
    let encoded = rmp_serde::to_vec_named(&request).context("encode lifecycle rpc")?;
    write_frame(&mut stream, &encoded).await?;
    let frame = read_frame(&mut stream).await?;
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
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}
