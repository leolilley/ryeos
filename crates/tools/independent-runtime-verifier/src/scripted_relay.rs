//! Bounded fixture-only bridge from an isolated child's signed loopback
//! origin to the controller-owned, pinned Unix scripted provider.
//!
//! This carries no credentials and grants no general host network access.
//! Partial forwarding is terminal: neither side is retried after a byte may
//! have crossed the bridge.

use crate::scripted_provider::REQUEST_COUNT;
use anyhow::{Context as _, Result, ensure};
use lillux::loopback::{BoundedLoopbackStream, ExactLoopbackListener, LoopbackInterrupt};
use lillux::task::{HostTask, spawn_host_task};
use lillux::time::{Duration, MonotonicDeadline};
use lillux::{LocalConnectInterrupt, LocalDuplexStream, PinnedDirectory};
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const HEADER_LIMIT: usize = 16 * 1024;
const REQUEST_LIMIT: usize = 1024 * 1024 + HEADER_LIMIT;
const RESPONSE_LIMIT: usize = 2 * 1024 * 1024 + HEADER_LIMIT;

pub struct RunningScriptedRelay {
    interrupt: LoopbackInterrupt,
    local_interrupt: LocalConnectInterrupt,
    finish: Arc<AtomicBool>,
    task: Option<HostTask<Result<usize>>>,
}

impl Drop for RunningScriptedRelay {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            self.interrupt.interrupt();
            self.local_interrupt.interrupt();
            let _ = task.join();
        }
    }
}

impl RunningScriptedRelay {
    /// Interrupt a pending accept or local provider exchange and join only
    /// within the caller's deadline. Cancellation never supplies a successful
    /// scripted-contact result; expiry returns the same live owner.
    pub fn cancel_until(
        self,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<Result<usize>, Self> {
        self.interrupt.interrupt();
        self.local_interrupt.interrupt();
        self.finish.store(true, Ordering::Release);
        self.join_until(deadline)
    }

    /// Call only after the direct Codex child and its diagnostic reader have
    /// settled. A clean result proves no sixth connection or pipelined bytes
    /// were visible on any retained socket at that terminal fence.
    pub fn finish_after_codex_stop(
        self,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<Result<usize>, Self> {
        self.finish.store(true, Ordering::Release);
        self.join_until(deadline)
    }

    fn join_until(
        mut self,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<Result<usize>, Self> {
        let task = self.task.take().expect("running relay owns task");
        match task.join_until(deadline) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Ok(Err(anyhow::anyhow!("scripted relay task panicked"))),
            Err(task) => {
                self.task = Some(task);
                Err(self)
            }
        }
    }
}

pub fn start(
    origin: &str,
    directory: PinnedDirectory,
    socket_name: OsString,
    deadline: MonotonicDeadline,
) -> Result<RunningScriptedRelay> {
    let address: SocketAddr = origin
        .strip_prefix("http://")
        .context("scripted relay origin is not HTTP")?
        .parse()?;
    ensure!(
        address.ip().is_loopback() && address.port() != 0 && origin == format!("http://{address}"),
        "scripted relay origin is not an exact loopback address"
    );
    let listener = ExactLoopbackListener::bind_exact(address)?;
    start_from_transferred(origin, listener, directory, socket_name, deadline)
}

/// Start the same bounded scripted relay using the exact listener transferred
/// from the daemon-owned held target. The caller must separately authenticate
/// the handoff and acknowledge readiness before that target is released.
pub fn start_from_transferred(
    origin: &str,
    listener: ExactLoopbackListener,
    directory: PinnedDirectory,
    socket_name: OsString,
    deadline: MonotonicDeadline,
) -> Result<RunningScriptedRelay> {
    let address: SocketAddr = origin
        .strip_prefix("http://")
        .context("scripted relay origin is not HTTP")?
        .parse()?;
    ensure!(
        address.ip().is_loopback()
            && address.port() != 0
            && origin == format!("http://{address}")
            && listener.address() == address,
        "transferred scripted relay listener differs from exact origin"
    );
    directory.ensure_path_binding()?;
    let interrupt = listener.interrupt_handle();
    let local_interrupt = LocalConnectInterrupt::default();
    let finish = Arc::new(AtomicBool::new(false));
    let task_finish = Arc::clone(&finish);
    let task_local_interrupt = local_interrupt.clone();
    let task = spawn_host_task("verifier-scripted-relay", move || {
        serve(
            listener,
            directory,
            socket_name,
            deadline,
            task_finish,
            task_local_interrupt,
        )
    })?;
    Ok(RunningScriptedRelay {
        interrupt,
        local_interrupt,
        finish,
        task: Some(task),
    })
}

fn serve(
    listener: ExactLoopbackListener,
    directory: PinnedDirectory,
    socket_name: OsString,
    deadline: MonotonicDeadline,
    finish: Arc<AtomicBool>,
    local_interrupt: LocalConnectInterrupt,
) -> Result<usize> {
    let mut connections = Vec::with_capacity(REQUEST_COUNT);
    for _ in 0..REQUEST_COUNT {
        let mut client = loop {
            ensure!(
                !finish.load(Ordering::Acquire),
                "Codex stopped before all relay contacts"
            );
            if let Some(client) = listener.try_accept_until(deadline)? {
                break client;
            }
            lillux::time::sleep(Duration::from_millis(1));
        };
        let request = read_request(&mut client, address_from_listener(&listener)?)?;
        let mut provider = LocalDuplexStream::connect_at_until(
            &directory,
            &socket_name,
            deadline,
            &local_interrupt,
        )?;
        write_local_all(&mut provider, &request, deadline, &local_interrupt)?;
        let response = read_response(&mut provider, deadline, &local_interrupt)?;
        client.write_all_until(&response)?;
        client.shutdown_write()?;
        connections.push(client);
    }
    loop {
        ensure!(
            listener.try_accept_until(deadline)?.is_none(),
            "unexpected sixth scripted relay connection"
        );
        if finish.load(Ordering::Acquire) {
            ensure!(
                !deadline.has_elapsed(),
                "scripted relay expired before Codex settlement"
            );
            break;
        }
        lillux::time::sleep(Duration::from_millis(1));
    }
    for mut client in connections {
        let mut extra = [0u8; 1];
        ensure!(
            client.read_chunk_until(&mut extra)? == 0,
            "extra bytes arrived on a scripted relay connection"
        );
    }
    directory.ensure_path_binding()?;
    Ok(REQUEST_COUNT)
}

fn address_from_listener(listener: &ExactLoopbackListener) -> Result<SocketAddr> {
    let address = listener.address();
    ensure!(address.ip().is_loopback(), "scripted relay listener moved");
    Ok(address)
}

fn read_request(stream: &mut BoundedLoopbackStream, address: SocketAddr) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let header_end = loop {
        ensure!(
            bytes.len() <= HEADER_LIMIT,
            "scripted relay request header exceeds bound"
        );
        let mut chunk = [0u8; 4096];
        let count = stream.read_chunk_until(&mut chunk)?;
        ensure!(count > 0, "scripted relay request ended before headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            ensure!(
                index + 4 <= HEADER_LIMIT,
                "scripted relay request header exceeds bound"
            );
            break index + 4;
        }
    };
    let body_len = request_content_length(&bytes[..header_end], address)?;
    let total = header_end
        .checked_add(body_len)
        .context("scripted relay request length overflow")?;
    ensure!(
        total <= REQUEST_LIMIT && bytes.len() <= total,
        "scripted relay request has excess bytes"
    );
    while bytes.len() < total {
        let mut chunk = [0u8; 4096];
        let count = stream.read_chunk_until(&mut chunk)?;
        ensure!(count > 0, "scripted relay request ended before body");
        bytes.extend_from_slice(&chunk[..count]);
        ensure!(
            bytes.len() <= total,
            "scripted relay request has trailing bytes"
        );
    }
    Ok(bytes)
}

fn request_content_length(header: &[u8], address: SocketAddr) -> Result<usize> {
    let text = std::str::from_utf8(header)?;
    ensure!(
        text.starts_with("POST /responses HTTP/1.1\r\n"),
        "scripted relay request changed route"
    );
    let mut host = None;
    let mut length = None;
    for line in text.split("\r\n").skip(1).filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .context("scripted relay malformed request header")?;
        if name.eq_ignore_ascii_case("host") {
            ensure!(
                host.replace(value.trim()).is_none(),
                "duplicate scripted relay host"
            );
        } else if name.eq_ignore_ascii_case("content-length") {
            ensure!(
                length.replace(value.trim().parse::<usize>()?).is_none(),
                "duplicate scripted relay length"
            );
        }
    }
    ensure!(
        host == Some(address.to_string().as_str()),
        "scripted relay host changed"
    );
    let length = length.context("scripted relay request has no content length")?;
    ensure!(
        (1..=1024 * 1024).contains(&length),
        "scripted relay request body exceeds bound"
    );
    Ok(length)
}

fn write_local_all(
    provider: &mut LocalDuplexStream,
    bytes: &[u8],
    deadline: MonotonicDeadline,
    interrupt: &LocalConnectInterrupt,
) -> Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        ensure!(
            !interrupt.is_interrupted(),
            "scripted relay local write interrupted"
        );
        ensure!(
            !deadline.has_elapsed(),
            "scripted relay local write expired"
        );
        let slice = deadline.min(MonotonicDeadline::after(Duration::from_millis(10)));
        match provider.with_deadline(slice).write(&bytes[written..]) {
            Ok(0) => anyhow::bail!("scripted relay provider closed during request"),
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn read_local(
    provider: &mut LocalDuplexStream,
    output: &mut [u8],
    deadline: MonotonicDeadline,
    interrupt: &LocalConnectInterrupt,
) -> Result<usize> {
    loop {
        ensure!(
            !interrupt.is_interrupted(),
            "scripted relay local read interrupted"
        );
        ensure!(!deadline.has_elapsed(), "scripted relay local read expired");
        let slice = deadline.min(MonotonicDeadline::after(Duration::from_millis(10)));
        match provider.with_deadline(slice).read(output) {
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => continue,
            result => return Ok(result?),
        }
    }
}

fn read_response(
    provider: &mut LocalDuplexStream,
    deadline: MonotonicDeadline,
    interrupt: &LocalConnectInterrupt,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let header_end = loop {
        ensure!(
            bytes.len() <= HEADER_LIMIT,
            "scripted relay response header exceeds bound"
        );
        let mut chunk = [0u8; 4096];
        let count = read_local(provider, &mut chunk, deadline, interrupt)?;
        ensure!(
            count > 0,
            "scripted relay provider ended before response headers"
        );
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            ensure!(
                index + 4 <= HEADER_LIMIT,
                "scripted relay response header exceeds bound"
            );
            break index + 4;
        }
    };
    let body_len = response_content_length(&bytes[..header_end])?;
    let total = header_end
        .checked_add(body_len)
        .context("scripted relay response length overflow")?;
    ensure!(
        total <= RESPONSE_LIMIT && bytes.len() <= total,
        "scripted relay provider sent excess bytes"
    );
    while bytes.len() < total {
        let mut chunk = [0u8; 4096];
        let count = read_local(provider, &mut chunk, deadline, interrupt)?;
        ensure!(
            count > 0,
            "scripted relay provider ended before response body"
        );
        bytes.extend_from_slice(&chunk[..count]);
        ensure!(
            bytes.len() <= total,
            "scripted relay provider sent trailing bytes"
        );
    }
    let mut extra = [0u8; 1];
    ensure!(
        read_local(provider, &mut extra, deadline, interrupt)? == 0,
        "scripted relay provider sent another response"
    );
    Ok(bytes)
}

fn response_content_length(header: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(header)?;
    ensure!(
        text.starts_with("HTTP/1.1 200 OK\r\n"),
        "scripted relay provider changed status"
    );
    let mut length = None;
    for line in text.split("\r\n").skip(1).filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .context("scripted relay malformed response header")?;
        if name.eq_ignore_ascii_case("content-length") {
            ensure!(
                length.replace(value.trim().parse::<usize>()?).is_none(),
                "duplicate scripted relay response length"
            );
        }
    }
    let length = length.context("scripted relay response has no content length")?;
    ensure!(
        (1..=2 * 1024 * 1024).contains(&length),
        "scripted relay response body exceeds bound"
    );
    Ok(length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripted_peer::ScriptedPeer;
    use std::net::{TcpListener, TcpStream};

    #[test]
    #[ignore = "requires native loopback socket authority"]
    fn cancellation_joins_pending_accept_without_provider_contact() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let origin = format!("http://{address}");
        let listener = ExactLoopbackListener::bind_exact(address).unwrap();
        let relay = start_from_transferred(
            &origin,
            listener,
            directory,
            OsString::from("absent-provider"),
            MonotonicDeadline::after(Duration::from_secs(10)),
        )
        .unwrap();
        let outcome = relay
            .cancel_until(MonotonicDeadline::after(Duration::from_secs(2)))
            .unwrap_or_else(|_| panic!("interrupted relay accept did not join"));
        assert!(outcome.is_err());
    }

    #[test]
    #[ignore = "requires native loopback and Unix socket authority"]
    fn finite_relay_reaches_parent_owned_pinned_provider() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let origin = format!("http://{address}");
        let deadline = MonotonicDeadline::after(Duration::from_secs(10));
        let (peer, name) = ScriptedPeer::bind_pinned(
            &origin,
            &directory,
            deadline,
            "forbidden-local".into(),
            "secret-read".into(),
            "private-controller-canary".into(),
        )
        .unwrap();
        let parent = peer.start().unwrap();
        let listener = ExactLoopbackListener::bind_exact(address).unwrap();
        let relay = start_from_transferred(
            &origin,
            listener,
            directory.try_clone().unwrap(),
            name,
            deadline,
        )
        .unwrap();
        for number in 0..REQUEST_COUNT {
            let body = format!("{{\"number\":{number}}}");
            let request = format!(
                "POST /responses HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let mut client = TcpStream::connect(address).unwrap();
            client.write_all(request.as_bytes()).unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(
                response
                    .windows(18)
                    .any(|part| part == b"response.completed")
            );
        }
        assert_eq!(
            relay
                .finish_after_codex_stop(deadline)
                .unwrap_or_else(|_| panic!("relay did not settle"))
                .unwrap(),
            REQUEST_COUNT
        );
        assert_eq!(
            parent
                .finish_after_producer_settlement(deadline)
                .unwrap_or_else(|_| panic!("parent peer did not settle"))
                .unwrap()
                .len(),
            REQUEST_COUNT
        );
    }

    #[test]
    #[ignore = "requires native loopback authority"]
    fn transferred_listener_must_match_exact_origin() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let listener = ExactLoopbackListener::bind_exact(address).unwrap();
        assert!(
            start_from_transferred(
                "http://127.0.0.1:1",
                listener,
                directory,
                OsString::from("unused"),
                MonotonicDeadline::after(Duration::from_secs(1)),
            )
            .is_err()
        );
    }

    #[test]
    #[ignore = "requires native loopback and Unix socket authority"]
    fn relay_refuses_late_pipelined_sixth_request() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let origin = format!("http://{address}");
        let deadline = MonotonicDeadline::after(Duration::from_secs(10));
        let (peer, name) = ScriptedPeer::bind_pinned(
            &origin,
            &directory,
            deadline,
            "forbidden-local".into(),
            "secret-read".into(),
            "private-controller-canary".into(),
        )
        .unwrap();
        let parent = peer.start().unwrap();
        let relay = start(&origin, directory.try_clone().unwrap(), name, deadline).unwrap();
        let request =
            format!("POST /responses HTTP/1.1\r\nHost: {address}\r\nContent-Length: 2\r\n\r\n{{}}");
        let mut first = TcpStream::connect(address).unwrap();
        first.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        first.read_to_end(&mut response).unwrap();
        first.write_all(request.as_bytes()).unwrap();
        for _ in 1..REQUEST_COUNT {
            let mut client = TcpStream::connect(address).unwrap();
            client.write_all(request.as_bytes()).unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).unwrap();
        }
        assert!(
            relay
                .finish_after_codex_stop(deadline)
                .unwrap_or_else(|_| panic!("relay did not settle"))
                .is_err()
        );
        assert_eq!(
            parent
                .finish_after_producer_settlement(deadline)
                .unwrap_or_else(|_| panic!("parent peer did not settle"))
                .unwrap()
                .len(),
            REQUEST_COUNT
        );
    }

    #[test]
    #[ignore = "requires native loopback and Unix socket authority"]
    fn stalled_provider_cancellation_joins_relay_promptly() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let fake =
            lillux::OwnerPrivateLocalDuplexListener::bind_pinned(&directory, "stalled").unwrap();
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let deadline = MonotonicDeadline::after(Duration::from_secs(10));
        let relay = start(
            &format!("http://{address}"),
            directory.try_clone().unwrap(),
            fake.endpoint_name().to_os_string(),
            deadline,
        )
        .unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(
                format!(
                    "POST /responses HTTP/1.1\r\nHost: {address}\r\nContent-Length: 2\r\n\r\n{{}}"
                )
                .as_bytes(),
            )
            .unwrap();
        let _stalled = fake
            .accept_before(MonotonicDeadline::after(Duration::from_secs(1)))
            .unwrap()
            .unwrap();
        let timer = lillux::time::MonotonicTimer::start();
        let relay = relay
            .finish_after_codex_stop(MonotonicDeadline::after(Duration::from_millis(20)))
            .err()
            .expect("stalled provider unexpectedly completed");
        drop(relay);
        assert!(timer.elapsed() < Duration::from_secs(1));
    }
}
