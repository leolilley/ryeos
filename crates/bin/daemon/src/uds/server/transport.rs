use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

use crate::uds::protocol::RpcRequest;
use ryeos_app::state::AppState;

const MAX_FRAME_SIZE: u32 = 10 * 1024 * 1024;
const FRAME_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(super) async fn handle_connection(
    mut stream: UnixStream,
    state: Arc<AppState>,
    frame_bytes: Arc<Semaphore>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        // A persistent callback client keeps this stream open between requests.
        // Once shutdown is visible, stop before admitting another frame. If a
        // frame wins this biased race first, it is the connection's one
        // already-admitted request and is allowed to drain below.
        if *shutdown.borrow() {
            return Ok(());
        }
        let frame = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
                continue;
            }
            frame = read_frame(&mut stream, &frame_bytes) => frame?,
        };
        let Some((frame, frame_permit)) = frame else {
            return Ok(());
        };

        let request: RpcRequest = rmp_serde::from_slice(&frame).context("invalid rpc frame")?;

        // Acquire the peer pidfd only for the one method that persists a
        // signalable process identity. Health/lifecycle traffic remains usable
        // on a host that cannot satisfy the runtime-attachment kernel contract.
        #[cfg(target_os = "linux")]
        let peer = if request.method == "runtime.attach_process" {
            Some(authenticated_peer(&stream)?)
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let peer: Option<super::AuthenticatedUnixPeer> = None;

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

        // The decoded request, its memory charge, and any kernel-authenticated
        // peer identity belong to an execution task rather than to the socket
        // waiter. A forced connection-task abort drops this JoinHandle, which
        // detaches (rather than aborts) the owner; shutdown's process-admission
        // fence and exact-identity drain can then settle any subprocess it owns.
        let request_state = Arc::clone(&state);
        let request_owner = tokio::spawn(tracing::Instrument::instrument(
            async move {
                let _frame_permit = frame_permit;
                super::routing::dispatch_with_peer(request, &request_state, peer.as_ref()).await
            },
            span,
        ));
        let response = request_owner
            .await
            .context("UDS request owner task failed")?;
        let encoded = rmp_serde::to_vec_named(&response).context("failed to encode response")?;
        write_frame(&mut stream, &encoded).await?;
    }
}

#[cfg(target_os = "linux")]
fn authenticated_peer(stream: &UnixStream) -> Result<super::AuthenticatedUnixPeer> {
    let pid = stream
        .peer_cred()
        .context("read Unix peer credentials")?
        .pid()
        .map(i64::from)
        .ok_or_else(|| anyhow!("Unix peer credentials did not include a PID"))?;

    let mut raw_pidfd: libc::c_int = -1;
    let mut value_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut raw_pidfd as *mut libc::c_int).cast(),
            &mut value_len,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("capture Unix peer pidfd with SO_PEERPIDFD");
    }
    if value_len as usize != std::mem::size_of::<libc::c_int>() || raw_pidfd < 0 {
        anyhow::bail!("SO_PEERPIDFD returned an invalid descriptor");
    }
    // SAFETY: successful SO_PEERPIDFD installs a new descriptor in this
    // process, and ownership transfers to the connection identity.
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw_pidfd) };
    Ok(super::AuthenticatedUnixPeer { pid, pidfd })
}

async fn read_frame(
    stream: &mut UnixStream,
    frame_bytes: &Arc<Semaphore>,
) -> Result<Option<(Vec<u8>, OwnedSemaphorePermit)>> {
    let mut len_buf = [0u8; 4];
    let length_read = tokio::time::timeout(FRAME_IO_TIMEOUT, stream.read_exact(&mut len_buf))
        .await
        .map_err(|_| anyhow!("timed out reading rpc frame length"))?;
    match length_read {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).context("failed to read rpc frame length"),
    }

    let frame_len = validate_frame_len(u32::from_be_bytes(len_buf))?;
    let permit = tokio::time::timeout(
        FRAME_IO_TIMEOUT,
        Arc::clone(frame_bytes).acquire_many_owned(frame_len),
    )
    .await
    .map_err(|_| anyhow!("timed out waiting for rpc frame memory budget"))?
    .context("rpc frame memory budget closed")?;
    let mut frame = vec![0u8; frame_len as usize];
    tokio::time::timeout(FRAME_IO_TIMEOUT, stream.read_exact(&mut frame))
        .await
        .map_err(|_| anyhow!("timed out reading rpc frame body"))?
        .context("failed to read rpc frame body")?;
    Ok(Some((frame, permit)))
}

fn validate_frame_len(frame_len: u32) -> Result<u32> {
    if frame_len > MAX_FRAME_SIZE {
        return Err(anyhow!(
            "frame too large: {} bytes (max {})",
            frame_len,
            MAX_FRAME_SIZE
        ));
    }
    Ok(frame_len)
}

async fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> Result<()> {
    let frame_len = u32::try_from(bytes.len()).context("rpc response exceeds u32 framing")?;
    validate_frame_len(frame_len).context("rpc response exceeds frame limit")?;
    let len = frame_len.to_be_bytes();
    tokio::time::timeout(FRAME_IO_TIMEOUT, stream.write_all(&len))
        .await
        .map_err(|_| anyhow!("timed out writing rpc frame length"))?
        .context("failed to write rpc frame length")?;
    tokio::time::timeout(FRAME_IO_TIMEOUT, stream.write_all(bytes))
        .await
        .map_err(|_| anyhow!("timed out writing rpc frame body"))?
        .context("failed to write rpc frame body")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_boundary_is_inclusive() {
        assert_eq!(validate_frame_len(MAX_FRAME_SIZE).unwrap(), MAX_FRAME_SIZE);
    }

    #[test]
    fn oversized_frame_preserves_wire_error_wording() {
        let frame_len = MAX_FRAME_SIZE + 1;
        assert_eq!(
            validate_frame_len(frame_len).unwrap_err().to_string(),
            format!("frame too large: {frame_len} bytes (max {MAX_FRAME_SIZE})")
        );
    }
}
