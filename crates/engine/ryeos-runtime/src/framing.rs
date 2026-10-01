use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub async fn send_frame(
    stream: &mut (impl AsyncWriteExt + Unpin),
    payload: &[u8],
) -> std::io::Result<()> {
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(payload).await
}

pub async fn recv_frame(stream: &mut (impl AsyncReadExt + Unpin)) -> std::io::Result<Vec<u8>> {
    recv_frame_with_limit(stream, usize::MAX).await
}

/// Reject an oversized length prefix before allocating or reading its body.
/// The caller must discard this connection on refusal; the body is unread.
pub async fn recv_frame_with_limit(
    stream: &mut (impl AsyncReadExt + Unpin),
    maximum_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > maximum_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("response frame length {len} exceeds byte limit {maximum_bytes}"),
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_prefix_is_rejected_without_waiting_for_body() {
        let (mut client, mut server) = tokio::io::duplex(4);
        client.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        // Keep the sender open and provide no body. Reading any body would
        // stall, and allocating the declared length would attempt four GiB.
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            recv_frame_with_limit(&mut server, 2 * 1024 * 1024),
        )
        .await
        .expect("oversized prefix must fail before reading body")
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn limited_frame_accepts_exact_boundary_and_empty_body() {
        let (mut client, mut server) = tokio::io::duplex(32);
        send_frame(&mut client, b"boundary").await.unwrap();
        assert_eq!(
            recv_frame_with_limit(&mut server, 8).await.unwrap(),
            b"boundary"
        );
        send_frame(&mut client, b"").await.unwrap();
        assert!(
            recv_frame_with_limit(&mut server, 0)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn frame_round_trip() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        send_frame(&mut client, b"hello world").await.unwrap();
        let received = recv_frame(&mut server).await.unwrap();
        assert_eq!(received, b"hello world");
    }

    #[tokio::test]
    async fn empty_frame() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        send_frame(&mut client, b"").await.unwrap();
        let received = recv_frame(&mut server).await.unwrap();
        assert!(received.is_empty());
    }

    #[tokio::test]
    async fn large_frame() {
        let (mut client, mut server) = tokio::io::duplex(65536);
        let data = vec![0xAB_u8; 10000];
        send_frame(&mut client, &data).await.unwrap();
        let received = recv_frame(&mut server).await.unwrap();
        assert_eq!(received.len(), 10000);
        assert!(received.iter().all(|&b| b == 0xAB));
    }
}
