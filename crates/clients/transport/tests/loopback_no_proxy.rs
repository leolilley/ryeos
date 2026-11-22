//! A call to the node's own local daemon never goes through an environment
//! proxy. A container with an egress proxy and a `NO_PROXY` that does not
//! cover loopback used to send every local daemon call to the proxy, where it
//! hung. The environment is set on a child process running this same test, so
//! the test never mutates its own.

use std::process::Command;

const CHILD_MARKER: &str = "RYEOS_TEST_LOOPBACK_NO_PROXY_CHILD";

fn dead_proxy() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

#[test]
fn a_local_daemon_call_ignores_a_dead_proxy_in_the_environment() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        return;
    }
    let proxy = dead_proxy();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "the_child_discovers_a_local_daemon",
            "--nocapture",
        ])
        .env(CHILD_MARKER, "1")
        .env("HTTP_PROXY", &proxy)
        .env("HTTPS_PROXY", &proxy)
        .env("ALL_PROXY", &proxy)
        .env("http_proxy", &proxy)
        .env("https_proxy", &proxy)
        .env("all_proxy", &proxy)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_child_discovers_a_local_daemon() {
    if std::env::var_os(CHILD_MARKER).is_none() {
        return;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                let _ = stream.read(&mut buffer).await;
                let body = br#"{"principal_id":"fp:local"}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body).await;
            });
        }
    });
    let discovered = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        ryeos_client_transport::discovery::discover_audience(&format!("http://127.0.0.1:{port}")),
    )
    .await
    .expect("the local call must not hang on a proxy")
    .expect("the local daemon answers");
    assert_eq!(discovered.principal_id, "fp:local");
    // The signed client takes the same path.
    let response = ryeos_client_transport::http::signed_client()
        .unwrap()
        .get(format!("http://localhost:{port}/"))
        .send()
        .await
        .expect("localhost is never proxied");
    assert!(response.status().is_success());
}
