use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::DynamicServerState;
use crate::uds::protocol::RpcRequest;

pub(super) async fn handle_connection(
    mut stream: UnixStream,
    state: DynamicServerState,
) -> Result<()> {
    loop {
        let Some(frame) = read_frame(&mut stream).await? else {
            return Ok(());
        };

        let request: RpcRequest = rmp_serde::from_slice(&frame).context("invalid rpc frame")?;

        // INFO so the ndjson sink records span NEW/CLOSE per request — a
        // request that arrives and never closes is then attributable by
        // method + request_id + thread_id from the trace alone. Entered via
        // `instrument` (not a held `enter()` guard, which detaches from the
        // task across `.await`).
        let span = tracing::info_span!(
            "uds:request",
            method = %request.method,
            request_id = %request.request_id,
            thread_id = tracing::field::Empty,
        );
        if let Some(tid) = request.params.get("thread_id").and_then(|v| v.as_str()) {
            span.record("thread_id", tid);
        }

        let response = tracing::Instrument::instrument(
            super::routing::dispatch_dynamic(request, &state),
            span,
        )
        .await;
        let encoded = rmp_serde::to_vec_named(&response).context("failed to encode response")?;
        write_frame(&mut stream, &encoded).await?;
    }
}

async fn read_frame(stream: &mut UnixStream) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).context("failed to read rpc frame length"),
    }

    let frame_len = validate_frame_len(u32::from_be_bytes(len_buf))?;
    let mut frame = vec![0u8; frame_len as usize];
    stream
        .read_exact(&mut frame)
        .await
        .context("failed to read rpc frame body")?;
    Ok(Some(frame))
}

fn validate_frame_len(frame_len: u32) -> Result<u32> {
    if frame_len > ryeos_node::LIFECYCLE_FRAME_MAX_BYTES {
        return Err(anyhow!(
            "frame too large: {} bytes (max {})",
            frame_len,
            ryeos_node::LIFECYCLE_FRAME_MAX_BYTES
        ));
    }
    Ok(frame_len)
}

async fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> Result<()> {
    let len: u32 = bytes
        .len()
        .try_into()
        .map_err(|_| anyhow!("frame too large"))?;
    let len = validate_frame_len(len)?.to_be_bytes();
    stream
        .write_all(&len)
        .await
        .context("failed to write rpc frame length")?;
    stream
        .write_all(bytes)
        .await
        .context("failed to write rpc frame body")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_boundary_is_inclusive() {
        assert_eq!(
            validate_frame_len(ryeos_node::LIFECYCLE_FRAME_MAX_BYTES).unwrap(),
            ryeos_node::LIFECYCLE_FRAME_MAX_BYTES
        );
    }

    #[test]
    fn oversized_frame_preserves_wire_error_wording() {
        let frame_len = ryeos_node::LIFECYCLE_FRAME_MAX_BYTES + 1;
        assert_eq!(
            validate_frame_len(frame_len).unwrap_err().to_string(),
            format!(
                "frame too large: {frame_len} bytes (max {})",
                ryeos_node::LIFECYCLE_FRAME_MAX_BYTES
            )
        );
    }
}
