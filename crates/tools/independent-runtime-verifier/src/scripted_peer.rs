//! Finite local Responses peer for the Codex verifier scenario.
//!
//! The listener is bound to the already-signed origin, not an ephemeral port.
//! Its request log is secondary evidence: loopback transport does not prove
//! that the connected process was the selected Codex executable.

use crate::scripted_provider::{REQUEST_COUNT, response_sse};
use anyhow::{Context as _, Result, ensure};
use lillux::loopback::{BoundedLoopbackStream, ExactLoopbackListener, LoopbackInterrupt};
use lillux::task::{HostTask, spawn_host_task};
use lillux::time::MonotonicDeadline;
use lillux::{LocalDuplexStream, OwnerPrivateLocalDuplexListener, PinnedDirectory};
use serde_json::Value;
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const HEADER_LIMIT: usize = 16 * 1024;
const BODY_LIMIT: usize = 1024 * 1024;

enum PeerListener {
    Unbound,
    Loopback(ExactLoopbackListener),
    Local(OwnerPrivateLocalDuplexListener),
}

enum PeerStream {
    Loopback(BoundedLoopbackStream),
    Local {
        stream: LocalDuplexStream,
        deadline: MonotonicDeadline,
        interrupted: Arc<AtomicBool>,
    },
}

impl PeerListener {
    fn try_accept_until(
        &self,
        deadline: MonotonicDeadline,
        interrupted: &Arc<AtomicBool>,
    ) -> Result<Option<PeerStream>> {
        ensure!(
            !interrupted.load(Ordering::Acquire),
            "scripted peer interrupted"
        );
        ensure!(!deadline.has_elapsed(), "scripted peer deadline elapsed");
        match self {
            Self::Unbound => anyhow::bail!("scripted peer has no bound listener"),
            Self::Loopback(listener) => Ok(listener
                .try_accept_until(deadline)?
                .map(PeerStream::Loopback)),
            Self::Local(listener) => {
                let slice = deadline.min(MonotonicDeadline::after(
                    lillux::time::Duration::from_millis(10),
                ));
                let accepted = listener
                    .accept_before(slice)?
                    .map(|stream| PeerStream::Local {
                        stream,
                        deadline,
                        interrupted: Arc::clone(interrupted),
                    });
                ensure!(!deadline.has_elapsed(), "scripted peer deadline elapsed");
                Ok(accepted)
            }
        }
    }
}

impl PeerStream {
    fn shutdown_write(&self) -> std::io::Result<()> {
        match self {
            Self::Loopback(stream) => stream.shutdown_write(),
            Self::Local { stream, .. } => stream.shutdown_write(),
        }
    }

    fn read_chunk_until(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Loopback(stream) => stream.read_chunk_until(output),
            Self::Local {
                stream,
                deadline,
                interrupted,
            } => loop {
                if interrupted.load(Ordering::Acquire) || deadline.has_elapsed() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "local scripted read interrupted or expired",
                    ));
                }
                let slice = (*deadline).min(MonotonicDeadline::after(
                    lillux::time::Duration::from_millis(10),
                ));
                match stream.with_deadline(slice).read(output) {
                    Err(error) if error.kind() == std::io::ErrorKind::TimedOut => continue,
                    result => return result,
                }
            },
        }
    }

    fn write_all_until(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Loopback(stream) => stream.write_all_until(bytes),
            Self::Local {
                stream,
                deadline,
                interrupted,
            } => {
                let mut written = 0;
                while written < bytes.len() {
                    if interrupted.load(Ordering::Acquire) || deadline.has_elapsed() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "local scripted write interrupted or expired; bytes may have been sent",
                        ));
                    }
                    let slice = (*deadline).min(MonotonicDeadline::after(
                        lillux::time::Duration::from_millis(10),
                    ));
                    match stream.with_deadline(slice).write(&bytes[written..]) {
                        Ok(0) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::WriteZero,
                                "local scripted response peer closed",
                            ));
                        }
                        Ok(count) => written += count,
                        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => continue,
                        Err(error) => return Err(error),
                    }
                }
                Ok(())
            }
        }
    }
}

pub struct ScriptedPeer {
    listener: PeerListener,
    address: SocketAddr,
    deadline: MonotonicDeadline,
    forbidden_local_command: String,
    secret_read_command: String,
    controller_canary: String,
}

/// Owns the local provider task until its finite request log or refusal is
/// joined. A timeout returns this same owner; dropping it interrupts and
/// joins as a last resort, but is not successful contact evidence.
#[must_use = "join or cancel and join the scripted peer before qualification"]
pub struct RunningScriptedPeer {
    interrupt: Option<LoopbackInterrupt>,
    local_interrupt: Arc<AtomicBool>,
    finish: Arc<AtomicBool>,
    task: Option<HostTask<Result<Vec<Value>>>>,
}

impl Drop for RunningScriptedPeer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // A verifier cannot leave a credential-free provider listener
            // serving after it abandons a turn. The listener and streams use
            // nonblocking deadline loops, so interruption makes them runnable.
            // This last-resort join is not successful contact evidence.
            self.local_interrupt.store(true, Ordering::Release);
            if let Some(interrupt) = &self.interrupt {
                interrupt.interrupt();
            }
            let _ = task.join();
        }
    }
}

impl RunningScriptedPeer {
    /// A completed task returns its exact result, including provider refusal.
    /// Expiry returns the unchanged owner rather than detaching a live peer.
    pub fn join_until(
        mut self,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<Result<Vec<Value>>, Self> {
        let task = self.task.take().expect("running peer owns task");
        match task.join_until(deadline) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Ok(Err(anyhow::anyhow!("scripted peer task panicked"))),
            Err(task) => {
                self.task = Some(task);
                Err(self)
            }
        }
    }

    /// Interrupt a pending accept/read/write, then retain ownership unless
    /// the task has actually joined. Cancellation is never successful contact
    /// evidence and cannot authorize a qualification claim.
    pub fn cancel_until(
        self,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<Result<Vec<Value>>, Self> {
        self.local_interrupt.store(true, Ordering::Release);
        if let Some(interrupt) = &self.interrupt {
            interrupt.interrupt();
        }
        self.join_until(deadline)
    }

    /// Call only after independently proving the Codex and guest producers
    /// can no longer make a request. The peer checks its pending accept queue
    /// once more before releasing the exact five-request log.
    pub fn finish_after_producer_settlement(
        self,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<Result<Vec<Value>>, Self> {
        self.finish.store(true, Ordering::Release);
        self.join_until(deadline)
    }
}

impl ScriptedPeer {
    pub fn bind_exact(
        origin: &str,
        deadline: MonotonicDeadline,
        forbidden_local_command: String,
        secret_read_command: String,
        controller_canary: String,
    ) -> Result<Self> {
        let mut peer = Self::bind_exact_configuration(
            origin,
            deadline,
            forbidden_local_command,
            secret_read_command,
            controller_canary,
        )?;
        peer.listener = PeerListener::Loopback(ExactLoopbackListener::bind_exact(peer.address)?);
        Ok(peer)
    }

    /// The controller owns this listener in the exact private verifier root.
    /// The child only receives its canonical socket name and cannot select an
    /// alternate provider origin. A separate relay is required inside the
    /// child's isolated network namespace.
    pub fn bind_pinned(
        origin: &str,
        directory: &PinnedDirectory,
        deadline: MonotonicDeadline,
        forbidden_local_command: String,
        secret_read_command: String,
        controller_canary: String,
    ) -> Result<(Self, OsString)> {
        let mut peer = Self::bind_exact_configuration(
            origin,
            deadline,
            forbidden_local_command,
            secret_read_command,
            controller_canary,
        )?;
        let listener = OwnerPrivateLocalDuplexListener::bind_pinned(directory, "provider")?;
        let name = listener.endpoint_name().to_os_string();
        peer.listener = PeerListener::Local(listener);
        Ok((peer, name))
    }

    fn bind_exact_configuration(
        origin: &str,
        deadline: MonotonicDeadline,
        forbidden_local_command: String,
        secret_read_command: String,
        controller_canary: String,
    ) -> Result<Self> {
        let address: SocketAddr = origin
            .strip_prefix("http://")
            .context("scripted origin is not HTTP")?
            .parse()?;
        ensure!(
            address.ip().is_loopback()
                && address.port() != 0
                && origin == format!("http://{address}"),
            "scripted origin is not an exact loopback address"
        );
        ensure!(
            (1..=8192).contains(&forbidden_local_command.len())
                && (1..=8192).contains(&secret_read_command.len())
                && !forbidden_local_command.chars().any(char::is_control)
                && !secret_read_command.chars().any(char::is_control),
            "scripted commands are absent, oversized or contain control characters"
        );
        ensure!(
            (16..=4096).contains(&controller_canary.len())
                && !controller_canary.chars().any(char::is_control),
            "scripted controller canary is invalid"
        );
        Ok(Self {
            // Replaced before any task starts by the pinned constructor.
            listener: PeerListener::Unbound,
            address,
            deadline,
            forbidden_local_command,
            secret_read_command,
            controller_canary,
        })
    }

    pub fn start(self) -> Result<RunningScriptedPeer> {
        ensure!(
            !matches!(&self.listener, PeerListener::Unbound),
            "scripted peer has no listener"
        );
        let interrupt = match &self.listener {
            PeerListener::Loopback(listener) => Some(listener.interrupt_handle()),
            _ => None,
        };
        let local_interrupt = Arc::new(AtomicBool::new(false));
        let finish = Arc::new(AtomicBool::new(false));
        let task_finish = Arc::clone(&finish);
        let task_interrupt = Arc::clone(&local_interrupt);
        let task = spawn_host_task("verifier-scripted-peer", move || {
            self.serve(task_finish, task_interrupt)
        })?;
        Ok(RunningScriptedPeer {
            interrupt,
            local_interrupt,
            finish,
            task: Some(task),
        })
    }

    /// Serve the finite script and retain the listener until the producer
    /// settlement fence. Five accepted requests alone do not prove there was
    /// no sixth or late attempt; the caller must settle the exact Codex/guest
    /// occurrence before signalling the finish fence.
    /// A write failure is ambiguous because a response prefix may have gone
    /// out. The caller owns this task and must join it on every path.
    fn serve(self, finish: Arc<AtomicBool>, interrupted: Arc<AtomicBool>) -> Result<Vec<Value>> {
        let mut requests = Vec::with_capacity(REQUEST_COUNT);
        // Retain every accepted connection until the producer is proven dead.
        // Dropping a socket after its first response would hide a later
        // pipelined sixth request on that same connection.
        let mut connections = Vec::with_capacity(REQUEST_COUNT);
        for number in 0..REQUEST_COUNT {
            let mut stream = loop {
                if finish.load(Ordering::Acquire) {
                    anyhow::bail!("scripted producer settled before all expected contacts");
                }
                if let Some(stream) = self
                    .listener
                    .try_accept_until(self.deadline, &interrupted)?
                {
                    break stream;
                }
                lillux::time::sleep(lillux::time::Duration::from_millis(1));
            };
            let request = read_request(&mut stream, self.address, &self.controller_canary)?;
            requests.push(request);
            let body = response_sse(
                number,
                &self.forbidden_local_command,
                &self.secret_read_command,
            )?;
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            stream.write_all_until(&response)?;
            stream.shutdown_write()?;
            connections.push(stream);
        }
        loop {
            ensure!(
                self.listener
                    .try_accept_until(self.deadline, &interrupted)?
                    .is_none(),
                "unexpected extra scripted provider contact"
            );
            if finish.load(Ordering::Acquire) {
                ensure!(
                    !self.deadline.has_elapsed(),
                    "scripted peer deadline elapsed before settlement"
                );
                break;
            }
            lillux::time::sleep(lillux::time::Duration::from_millis(1));
        }
        for mut stream in connections {
            let mut extra = [0u8; 1];
            ensure!(
                stream.read_chunk_until(&mut extra)? == 0,
                "extra scripted provider bytes arrived on an accepted connection"
            );
        }
        Ok(requests)
    }
}

fn read_request(
    stream: &mut PeerStream,
    address: SocketAddr,
    controller_canary: &str,
) -> Result<Value> {
    let mut bytes = Vec::new();
    let header_end = loop {
        ensure!(
            bytes.len() <= HEADER_LIMIT,
            "scripted request headers exceed bound"
        );
        let mut chunk = [0; 4096];
        let count = stream.read_chunk_until(&mut chunk)?;
        ensure!(count > 0, "scripted request ended before headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            ensure!(index + 4 <= HEADER_LIMIT, "scripted headers exceed bound");
            break index + 4;
        }
    };
    let content_length = parse_headers(&bytes[..header_end], address, controller_canary)?;
    let expected_total = header_end
        .checked_add(content_length)
        .context("scripted request length overflow")?;
    ensure!(
        bytes.len() <= expected_total,
        "scripted request has trailing bytes"
    );
    while bytes.len() < expected_total {
        let mut chunk = [0; 4096];
        let count = stream.read_chunk_until(&mut chunk)?;
        ensure!(count > 0, "scripted request ended before body");
        bytes.extend_from_slice(&chunk[..count]);
        ensure!(
            bytes.len() <= expected_total,
            "scripted request has trailing bytes"
        );
    }
    let request: Value = serde_json::from_slice(&bytes[header_end..])?;
    ensure!(request.is_object(), "scripted request is not a JSON object");
    ensure!(
        !contains_canary(&request, controller_canary),
        "controller canary appeared in scripted request body"
    );
    Ok(request)
}

fn contains_canary(value: &Value, canary: &str) -> bool {
    match value {
        Value::String(text) => text.contains(canary),
        Value::Array(items) => items.iter().any(|item| contains_canary(item, canary)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains(canary) || contains_canary(value, canary)),
        _ => false,
    }
}

fn parse_headers(header: &[u8], address: SocketAddr, controller_canary: &str) -> Result<usize> {
    ensure!(
        header.len() <= HEADER_LIMIT,
        "scripted headers exceed bound"
    );
    let header = std::str::from_utf8(header)?;
    ensure!(
        header.ends_with("\r\n\r\n"),
        "scripted headers are incomplete"
    );
    ensure!(
        !header.contains(controller_canary),
        "controller canary appeared in scripted request headers"
    );
    let mut lines = header.split("\r\n");
    ensure!(
        lines.next() == Some("POST /responses HTTP/1.1"),
        "scripted request is not the exact Responses route"
    );
    let mut content_length = None;
    let mut host = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .context("scripted request has malformed header")?;
        let name = name.trim();
        let value = value.trim();
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !value.chars().any(char::is_control),
            "scripted request has invalid header"
        );
        if name.eq_ignore_ascii_case("content-length") {
            ensure!(content_length.is_none(), "duplicate content length");
            let parsed: usize = value.parse()?;
            ensure!(
                (1..=BODY_LIMIT).contains(&parsed),
                "scripted body exceeds bound"
            );
            content_length = Some(parsed);
        } else if name.eq_ignore_ascii_case("host") {
            ensure!(host.replace(value).is_none(), "duplicate host header");
        } else {
            // Deny unknown headers instead of logging only the JSON body:
            // a custom header could otherwise carry a credential or canary
            // unseen by the secondary request checker.
            ensure!(
                [
                    "accept",
                    "accept-encoding",
                    "connection",
                    "content-type",
                    "user-agent",
                ]
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed))
                    && value.len() <= 1024,
                "scripted request has an unapproved header"
            );
        }
    }
    let expected_host = address.to_string();
    ensure!(
        host == Some(expected_host.as_str()),
        "scripted request changed host"
    );
    content_length.context("scripted request has no content length")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_origin_rejects_nonloopback_and_ephemeral_ports() {
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(1));
        for origin in [
            "http://127.0.0.1:0",
            "http://0.0.0.0:1234",
            "https://127.0.0.1:1234",
        ] {
            assert!(
                ScriptedPeer::bind_exact(
                    origin,
                    deadline,
                    "local".into(),
                    "secret".into(),
                    "private-controller-canary".into(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn header_parser_refuses_credentials_wrong_route_and_duplicate_length() {
        let address = "127.0.0.1:1234".parse().unwrap();
        let canary = "private-controller-canary";
        let clean =
            b"POST /responses HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Length: 2\r\n\r\n";
        assert_eq!(parse_headers(clean, address, canary).unwrap(), 2);
        for invalid in [
            "POST /wrong HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Length: 2\r\n\r\n",
            "POST /responses HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Length: 2\r\nAuthorization: Bearer secret\r\n\r\n",
            "POST /responses HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n",
            "POST /responses HTTP/1.1\r\nHost: 127.0.0.1:5678\r\nContent-Length: 2\r\n\r\n",
            "POST /responses HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST /responses HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Length: 2\r\nX-Api-Key: secret\r\n\r\n",
            "POST /responses HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Length: 2\r\nUser-Agent: private-controller-canary\r\n\r\n",
        ] {
            assert!(parse_headers(invalid.as_bytes(), address, canary).is_err());
        }
    }

    #[test]
    fn decoded_request_body_refuses_controller_canary() {
        let canary = "private-controller-canary";
        let clean: Value = serde_json::from_str(r#"{"input":[{"text":"ordinary"}]}"#).unwrap();
        assert!(!contains_canary(&clean, canary));
        for body in [
            r#"{"input":[{"text":"private-controller-canary"}]}"#,
            r#"{"input":[{"text":"private-controller-\u0063anary"}]}"#,
            r#"{"private-controller-canary":"value"}"#,
        ] {
            let decoded: Value = serde_json::from_str(body).unwrap();
            assert!(contains_canary(&decoded, canary));
        }
    }

    #[test]
    #[ignore = "requires native Unix socket authority"]
    fn pinned_peer_serves_exact_five_bounded_responses() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let address = "127.0.0.1:18765";
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        let (peer, name) = ScriptedPeer::bind_pinned(
            &format!("http://{address}"),
            &directory,
            deadline,
            "forbidden-local".into(),
            "secret-read".into(),
            "private-controller-canary".into(),
        )
        .unwrap();
        let server = peer.start().unwrap();
        for number in 0..REQUEST_COUNT {
            let body = format!("{{\"number\":{number}}}");
            let request = format!(
                "POST /responses HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let mut client = LocalDuplexStream::connect_at(&directory, &name).unwrap();
            client.write_all(request.as_bytes()).unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).unwrap();
            let response = std::str::from_utf8(&response).unwrap();
            assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
            assert!(response.contains("response.completed"));
        }
        let requests = server
            .finish_after_producer_settlement(deadline)
            .unwrap_or_else(|_| panic!("pinned scripted peer did not settle"))
            .unwrap();
        assert_eq!(requests.len(), REQUEST_COUNT);
    }

    #[test]
    #[ignore = "requires native Unix socket authority"]
    fn pinned_peer_refuses_late_pipelined_sixth_request() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let address = "127.0.0.1:18765";
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        let (peer, name) = ScriptedPeer::bind_pinned(
            &format!("http://{address}"),
            &directory,
            deadline,
            "forbidden-local".into(),
            "secret-read".into(),
            "private-controller-canary".into(),
        )
        .unwrap();
        let server = peer.start().unwrap();
        let request =
            format!("POST /responses HTTP/1.1\r\nHost: {address}\r\nContent-Length: 2\r\n\r\n{{}}");
        let mut first = LocalDuplexStream::connect_at(&directory, &name).unwrap();
        first.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        first.read_to_end(&mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        first.write_all(request.as_bytes()).unwrap();
        for _ in 1..REQUEST_COUNT {
            let mut client = LocalDuplexStream::connect_at(&directory, &name).unwrap();
            client.write_all(request.as_bytes()).unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        }
        let result = server
            .finish_after_producer_settlement(deadline)
            .unwrap_or_else(|_| panic!("pinned scripted peer did not settle"));
        assert!(result.is_err());
    }

    #[test]
    #[ignore = "requires local loopback socket authority"]
    fn exact_peer_serves_five_bounded_responses() {
        use std::io::{Read as _, Write as _};
        use std::net::{TcpListener, TcpStream};

        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        let peer = ScriptedPeer::bind_exact(
            &format!("http://{address}"),
            deadline,
            "forbidden-local".into(),
            "secret-read".into(),
            "private-controller-canary".into(),
        )
        .unwrap();
        let server = peer.start().unwrap();
        for number in 0..REQUEST_COUNT {
            let body = format!("{{\"number\":{number}}}");
            let request = format!(
                "POST /responses HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let mut client = TcpStream::connect(address).unwrap();
            client.write_all(request.as_bytes()).unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).unwrap();
            let response = std::str::from_utf8(&response).unwrap();
            assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
            assert!(response.contains("response.completed"));
        }
        let requests = match server.finish_after_producer_settlement(deadline) {
            Ok(Ok(requests)) => requests,
            Ok(Err(error)) => panic!("scripted peer refused: {error:#}"),
            Err(server) => {
                match server.cancel_until(MonotonicDeadline::after(
                    lillux::time::Duration::from_secs(2),
                )) {
                    Ok(_) => {}
                    Err(server) => drop(server),
                }
                panic!("scripted peer did not complete exactly");
            }
        };
        assert_eq!(requests.len(), REQUEST_COUNT);
        for (number, request) in requests.iter().enumerate() {
            assert_eq!(request["number"], number);
        }
    }

    #[test]
    #[ignore = "requires local loopback socket authority"]
    fn sixth_contact_refuses_before_finish_fence() {
        use std::io::{Read as _, Write as _};
        use std::net::{TcpListener, TcpStream};

        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        let peer = ScriptedPeer::bind_exact(
            &format!("http://{address}"),
            deadline,
            "forbidden-local".into(),
            "secret-read".into(),
            "private-controller-canary".into(),
        )
        .unwrap()
        .start()
        .unwrap();
        for number in 0..=REQUEST_COUNT {
            let mut client = TcpStream::connect(address).unwrap();
            let request = format!(
                "POST /responses HTTP/1.1\r\nHost: {address}\r\nContent-Length: 2\r\n\r\n{{}}"
            );
            client.write_all(request.as_bytes()).unwrap();
            let mut response = Vec::new();
            let read = client.read_to_end(&mut response);
            if number < REQUEST_COUNT {
                read.unwrap();
                assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
            } else {
                let refused = match read {
                    Ok(_) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => true,
                    Err(_) => false,
                };
                assert!(
                    response.is_empty() && refused,
                    "sixth request unexpectedly received a response"
                );
            }
        }
        match peer.join_until(deadline) {
            Ok(Err(error)) => assert!(format!("{error:#}").contains("extra scripted provider")),
            Ok(Ok(_)) => panic!("sixth provider contact was accepted"),
            Err(peer) => {
                drop(peer);
                panic!("sixth provider contact was not observed before deadline");
            }
        }
    }

    #[test]
    #[ignore = "requires local loopback socket authority"]
    fn finish_before_five_contacts_refuses_incomplete_script() {
        use std::net::TcpListener;

        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let peer = ScriptedPeer::bind_exact(
            &format!("http://{address}"),
            MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            "forbidden-local".into(),
            "secret-read".into(),
            "private-controller-canary".into(),
        )
        .unwrap()
        .start()
        .unwrap();
        match peer.finish_after_producer_settlement(MonotonicDeadline::after(
            lillux::time::Duration::from_secs(2),
        )) {
            Ok(Err(error)) => {
                assert!(format!("{error:#}").contains("before all expected contacts"))
            }
            Ok(Ok(_)) => panic!("incomplete scripted contact was accepted"),
            Err(peer) => {
                drop(peer);
                panic!("incomplete scripted peer did not settle");
            }
        }
    }

    #[test]
    #[ignore = "requires local loopback socket authority"]
    fn queued_sixth_contact_refuses_even_when_finish_is_signalled() {
        use std::io::{Read as _, Write as _};
        use std::net::{TcpListener, TcpStream};

        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        let peer = ScriptedPeer::bind_exact(
            &format!("http://{address}"),
            deadline,
            "forbidden-local".into(),
            "secret-read".into(),
            "private-controller-canary".into(),
        )
        .unwrap()
        .start()
        .unwrap();
        let request =
            format!("POST /responses HTTP/1.1\r\nHost: {address}\r\nContent-Length: 2\r\n\r\n{{}}");
        for _ in 0..REQUEST_COUNT {
            let mut client = TcpStream::connect(address).unwrap();
            client.write_all(request.as_bytes()).unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        }
        let mut queued = TcpStream::connect(address).unwrap();
        queued.write_all(request.as_bytes()).unwrap();
        match peer.finish_after_producer_settlement(deadline) {
            Ok(Err(error)) => assert!(format!("{error:#}").contains("extra scripted provider")),
            Ok(Ok(_)) => panic!("queued sixth provider contact was missed"),
            Err(peer) => {
                drop(peer);
                panic!("queued sixth provider contact was not observed");
            }
        }
    }

    #[test]
    #[ignore = "requires local loopback socket authority"]
    fn owned_peer_cancels_pending_accept_and_joins() {
        use std::net::TcpListener;

        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let peer = ScriptedPeer::bind_exact(
            &format!("http://{address}"),
            MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            "forbidden-local".into(),
            "secret-read".into(),
            "private-controller-canary".into(),
        )
        .unwrap()
        .start()
        .unwrap();
        match peer.cancel_until(MonotonicDeadline::after(lillux::time::Duration::from_secs(
            2,
        ))) {
            Ok(Err(error)) => assert!(format!("{error:#}").contains("interrupted")),
            Ok(Ok(_)) => panic!("cancelled peer reported complete scripted contact"),
            Err(_) => panic!("cancelled peer did not settle"),
        }
    }
}
