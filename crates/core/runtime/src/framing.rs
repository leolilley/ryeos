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

pub async fn recv_frame(
    stream: &mut (impl AsyncReadExt + Unpin),
) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

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
