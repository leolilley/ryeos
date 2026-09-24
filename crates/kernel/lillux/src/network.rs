//! Deadline-owned byte transport. No URLs, HTTP, TLS policy or provider state.
//!
//! Each stream owns a current-thread reactor. DNS uses async UDP/TCP on that
//! same reactor, never a system getaddrinfo task or blocking worker pool. Drop
//! closes the socket and destroys all resolver tasks; no detached runtime or
//! connection pool survives the capability. Closing local resources does not
//! attest whether a remote peer committed a request.

use std::future::Future;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use hickory_resolver::config::{ResolveHosts, ResolverConfig, ResolverOpts};
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_resolver::{Hosts, Resolver};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::watch;

use crate::time::MonotonicDeadline;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_ADDRESSES: usize = 32;

/// A cancellation signal carries no socket or remote execution authority.
#[derive(Clone, Debug)]
pub struct NetworkCancellation(watch::Sender<bool>);

impl Default for NetworkCancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}

impl NetworkCancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Observe whether the owner has cancelled future and in-flight I/O.
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
}

/// Explicit captured DNS inputs, with no filesystem/proxy/CA/environment
/// discovery. This supports hosts + DNS, not arbitrary NSS plugins.
#[derive(Clone)]
pub struct NetworkContext {
    config: ResolverConfig,
    options: ResolverOpts,
    hosts: Arc<Hosts>,
}

impl NetworkContext {
    #[cfg(unix)]
    pub fn from_config_bytes(resolver: &[u8], hosts: &[u8]) -> io::Result<Self> {
        if resolver.len() as u64 > MAX_CONFIG_BYTES || hosts.len() as u64 > MAX_CONFIG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "network configuration exceeds its bound",
            ));
        }
        // Hickory 0.25's resolv-conf parser otherwise derives an absent domain
        // from gethostname (or /proc/sys/kernel/hostname in static builds).
        // Seed only the parse buffer with the DNS root; later authored domain
        // and search directives retain their normal precedence. Connections
        // below query absolute names, never ambient search suffixes. Captured
        // source bytes and their evidence digest are not rewritten.
        let mut parse_input = Vec::with_capacity(b"domain .\n".len() + resolver.len());
        parse_input.extend_from_slice(b"domain .\n");
        parse_input.extend_from_slice(resolver);
        let (config, mut options) = hickory_resolver::system_conf::parse_resolv_conf(&parse_input)
            .map_err(|_| io::Error::other("unsupported host DNS configuration"))?;
        // Never allow a library to re-read ambient files during a request.
        options.use_hosts_file = ResolveHosts::Never;
        options.cache_size = 0;
        options.attempts = 1;
        options.num_concurrent_reqs = 2;
        if config.name_servers().len() > 16 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many host DNS servers",
            ));
        }
        // Hickory Hosts::default is an empty in-memory map; only from_system
        // reads ambient files. Keep that distinction explicit here.
        let mut captured_hosts = Hosts::default();
        captured_hosts.read_hosts_conf(hosts).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid captured hosts configuration",
            )
        })?;
        Ok(Self {
            config,
            options,
            hosts: Arc::new(captured_hosts),
        })
    }

    #[cfg(not(unix))]
    pub fn from_config_bytes(_resolver: &[u8], _hosts: &[u8]) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "bounded host DNS configuration is unsupported on this platform",
        ))
    }

    pub fn connect(
        &self,
        host: &str,
        port: u16,
        connection_deadline: MonotonicDeadline,
        request_deadline: MonotonicDeadline,
        idle_timeout: crate::time::Duration,
        cancellation: NetworkCancellation,
    ) -> io::Result<NetworkStream> {
        if host.is_empty() || host.len() > 253 || port == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid network endpoint",
            ));
        }
        if idle_timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "network idle timeout must be nonzero",
            ));
        }
        // A synchronous host capability cannot nest a reactor. Its caller must
        // enter through the existing blocking execution boundary.
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "network stream requires a synchronous host context",
            ));
        }
        let runtime = Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        let deadline = connection_deadline.min(request_deadline);
        let socket = runtime.block_on(bounded(deadline, &cancellation, async {
            let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
                vec![SocketAddr::new(ip, port)]
            } else {
                let mut resolver = Resolver::builder_with_config(
                    self.config.clone(),
                    TokioConnectionProvider::default(),
                )
                .with_options(self.options.clone())
                .build();
                resolver.set_hosts(self.hosts.clone());
                let absolute_host = format!("{}.", host.trim_end_matches('.'));
                let lookup = resolver
                    .lookup_ip(absolute_host)
                    .await
                    .map_err(|_| io::Error::other("network name resolution failed"))?;
                lookup
                    .iter()
                    .take(MAX_ADDRESSES)
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect()
            };
            let mut last =
                io::Error::new(io::ErrorKind::NotFound, "network endpoint has no address");
            for address in addresses {
                // Every attempt shares the original deadline. No HTTP bytes
                // are sent by this capability and it never retries a request.
                match tokio::net::TcpStream::connect(address).await {
                    Ok(socket) => return Ok(socket),
                    Err(error) => last = error,
                }
            }
            Err(last)
        }))?;
        socket.set_nodelay(true)?;
        Ok(NetworkStream {
            socket: Some(socket),
            runtime: Some(runtime),
            deadline: request_deadline,
            setup_deadline: Some(deadline),
            idle_timeout,
            cancellation,
        })
    }
}

async fn bounded<T>(
    deadline: MonotonicDeadline,
    cancellation: &NetworkCancellation,
    operation: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    let mut cancelled = cancellation.0.subscribe();
    if *cancelled.borrow() {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "network operation cancelled",
        ));
    }
    if deadline.has_elapsed() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "network deadline elapsed",
        ));
    }
    tokio::select! {
        biased;
        _ = cancelled.changed() => Err(io::Error::new(io::ErrorKind::ConnectionAborted, "network operation cancelled")),
        _ = tokio::time::sleep(deadline.remaining()) => Err(io::Error::new(io::ErrorKind::TimedOut, "network deadline elapsed")),
        result = operation => result,
    }
}

/// An owned byte stream. No raw socket, cloning, blocking-mode switch, or
/// deadline extension is exposed. I/O errors remain remote-contact ambiguous.
pub struct NetworkStream {
    socket: Option<tokio::net::TcpStream>,
    runtime: Option<Runtime>,
    deadline: MonotonicDeadline,
    setup_deadline: Option<MonotonicDeadline>,
    idle_timeout: crate::time::Duration,
    cancellation: NetworkCancellation,
}

impl NetworkStream {
    /// The protocol owner has completed its connection setup (for example TLS
    /// negotiation). This never renews the original operation deadline.
    pub fn complete_setup(&mut self) -> io::Result<()> {
        if self.effective_deadline().has_elapsed() {
            self.socket.take();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "network setup deadline elapsed",
            ));
        }
        self.setup_deadline = None;
        Ok(())
    }

    fn effective_deadline(&self) -> MonotonicDeadline {
        self.setup_deadline
            .map_or(self.deadline, |setup| setup.min(self.deadline))
    }

    fn io_deadline(&self) -> MonotonicDeadline {
        self.effective_deadline()
            .min(MonotonicDeadline::after(self.idle_timeout))
    }

    pub fn narrow_deadline(&mut self, deadline: MonotonicDeadline) {
        self.deadline = self.deadline.min(deadline);
    }
}

impl Read for NetworkStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.socket.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "network stream is settled",
            ));
        }
        let result = self.runtime.as_ref().unwrap().block_on(bounded(
            self.io_deadline(),
            &self.cancellation,
            self.socket.as_mut().unwrap().read(bytes),
        ));
        if result.is_err() {
            self.socket.take();
        }
        result
    }
}

impl Write for NetworkStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.socket.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "network stream is settled",
            ));
        }
        let result = self.runtime.as_ref().unwrap().block_on(bounded(
            self.io_deadline(),
            &self.cancellation,
            self.socket.as_mut().unwrap().write(bytes),
        ));
        if result.is_err() {
            self.socket.take();
        }
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.socket.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "network stream is settled",
            ));
        }
        let result = self.runtime.as_ref().unwrap().block_on(bounded(
            self.io_deadline(),
            &self.cancellation,
            self.socket.as_mut().unwrap().flush(),
        ));
        if result.is_err() {
            self.socket.take();
        }
        result
    }
}

impl Drop for NetworkStream {
    fn drop(&mut self) {
        // This reactor never owns blocking jobs. Drop cancels its async DNS
        // tasks and releases their sockets synchronously; no shutdown join.
        drop(self.socket.take());
        if let Some(runtime) = self.runtime.take() {
            // No blocking jobs exist in this reactor. This performs async task
            // destruction synchronously and is also safe if the capability is
            // dropped by a caller currently entered into another runtime.
            runtime.shutdown_timeout(crate::time::Duration::ZERO);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Duration;
    use hickory_resolver::config::NameServerConfig;
    use hickory_resolver::proto::xfer::Protocol;
    use std::net::{TcpListener, UdpSocket};
    use std::time::Instant;

    fn context(dns: SocketAddr) -> NetworkContext {
        let config = ResolverConfig::from_parts(
            None,
            vec![],
            vec![NameServerConfig::new(dns, Protocol::Udp)],
        );
        let mut options = ResolverOpts::default();
        options.use_hosts_file = ResolveHosts::Never;
        options.cache_size = 0;
        options.attempts = 1;
        NetworkContext {
            config,
            options,
            hosts: Arc::new(Hosts::default()),
        }
    }

    #[cfg(unix)]
    #[test]
    fn captured_configuration_uses_only_explicit_hosts_and_resolver_bytes() {
        use hickory_resolver::proto::{
            op::Query,
            rr::{Name, RecordType},
        };

        let mut hosts = b"192.0.2.42 explicit-capture.invalid\n".to_vec();
        let context =
            NetworkContext::from_config_bytes(b"nameserver 192.0.2.53\n", &hosts).unwrap();
        hosts.fill(b'x');
        assert!(
            context
                .config
                .name_servers()
                .iter()
                .all(|server| server.socket_addr.ip() == "192.0.2.53".parse::<IpAddr>().unwrap())
        );
        assert!(!context.config.name_servers().is_empty());
        assert_eq!(context.options.use_hosts_file, ResolveHosts::Never);
        let query = Query::query(
            Name::from_ascii("explicit-capture.invalid.").unwrap(),
            RecordType::A,
        );
        let lookup = context.hosts.lookup_static_host(&query).unwrap();
        assert_eq!(
            lookup
                .iter()
                .filter_map(|record| record.ip_addr())
                .collect::<Vec<_>>(),
            vec!["192.0.2.42".parse::<IpAddr>().unwrap()]
        );
        let empty = NetworkContext::from_config_bytes(b"nameserver 192.0.2.53\n", b"").unwrap();
        assert!(empty.hosts.lookup_static_host(&query).is_none());
        let localhost = Query::query(Name::from_ascii("localhost.").unwrap(), RecordType::A);
        assert!(empty.hosts.lookup_static_host(&localhost).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn captured_configuration_root_seed_avoids_hostname_fallback_and_preserves_authored_search() {
        let root = NetworkContext::from_config_bytes(b"nameserver 192.0.2.53\n", b"").unwrap();
        assert_eq!(root.config.domain().unwrap().to_utf8(), ".");
        assert_eq!(
            root.config
                .search()
                .iter()
                .map(|name| name.to_utf8())
                .collect::<Vec<_>>(),
            vec!["."]
        );
        for (directives, domain, search) in [
            (
                "domain authored.invalid\n",
                "authored.invalid",
                vec!["authored.invalid"],
            ),
            (
                "search first.invalid second.invalid\n",
                ".",
                vec!["first.invalid", "second.invalid"],
            ),
            (
                "domain authored.invalid\nsearch later.invalid\n",
                "authored.invalid",
                vec!["later.invalid"],
            ),
            (
                "search first.invalid\ndomain later.invalid\n",
                "later.invalid",
                vec!["later.invalid"],
            ),
        ] {
            let resolver = format!("nameserver 192.0.2.53\n{directives}");
            let context = NetworkContext::from_config_bytes(resolver.as_bytes(), b"").unwrap();
            assert_eq!(context.config.domain().unwrap().to_utf8(), domain);
            assert_eq!(
                context
                    .config
                    .search()
                    .iter()
                    .map(|name| name.to_utf8())
                    .collect::<Vec<_>>(),
                search
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn captured_configuration_refuses_bounds_and_sanitizes_parse_errors() {
        assert!(
            NetworkContext::from_config_bytes(b"", b"127.0.0.1 localhost\n").is_err(),
            "an empty resolver input must not select implicit DNS servers"
        );
        for (resolver, hosts) in [
            (vec![b'x'; MAX_CONFIG_BYTES as usize + 1], Vec::new()),
            (
                b"nameserver 192.0.2.53\n".to_vec(),
                vec![b'x'; MAX_CONFIG_BYTES as usize + 1],
            ),
        ] {
            assert_eq!(
                NetworkContext::from_config_bytes(&resolver, &hosts)
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        let error = NetworkContext::from_config_bytes(b"nameserver PRIVATE_CONFIG_SENTINEL\n", b"")
            .err()
            .unwrap();
        assert!(!format!("{error:?}").contains("PRIVATE_CONFIG_SENTINEL"));
        let error = NetworkContext::from_config_bytes(
            b"nameserver 192.0.2.53\n",
            b"\xffPRIVATE_CONFIG_SENTINEL",
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!format!("{error:?}").contains("PRIVATE_CONFIG_SENTINEL"));
    }

    #[test]
    fn silent_dns_deadline_settles_without_runtime_thread_or_late_queries() {
        let dns = UdpSocket::bind("127.0.0.1:0").unwrap();
        dns.set_nonblocking(true).unwrap();
        let context = context(dns.local_addr().unwrap());
        let start = Instant::now();
        let deadline = MonotonicDeadline::after(Duration::from_millis(80));
        let error = context
            .connect(
                "silent.example",
                443,
                deadline,
                deadline,
                Duration::from_secs(5),
                NetworkCancellation::default(),
            )
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
        let mut bytes = [0; 4096];
        let mut contacted = false;
        while dns.recv_from(&mut bytes).is_ok() {
            contacted = true;
        }
        assert!(contacted);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            dns.recv_from(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn cancellation_interrupts_dns_and_prevents_contact_when_already_cancelled() {
        let dns = UdpSocket::bind("127.0.0.1:0").unwrap();
        let context = context(dns.local_addr().unwrap());
        let cancellation = NetworkCancellation::default();
        let cancel = cancellation.clone();
        let task = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            cancel.cancel();
        });
        let deadline = MonotonicDeadline::after(Duration::from_secs(10));
        let start = Instant::now();
        let error = context
            .connect(
                "silent.example",
                443,
                deadline,
                deadline,
                Duration::from_secs(5),
                cancellation.clone(),
            )
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(start.elapsed() < Duration::from_secs(2));
        task.join().unwrap();
        let error = context
            .connect(
                "127.0.0.1",
                443,
                deadline,
                deadline,
                Duration::from_secs(5),
                cancellation,
            )
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
    }

    #[test]
    fn partial_progress_does_not_renew_deadline_and_stream_cannot_be_reused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            for _ in 0..30 {
                if peer.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let context = context("127.0.0.1:53".parse().unwrap());
        let deadline = MonotonicDeadline::after(Duration::from_millis(80));
        let mut stream = context
            .connect(
                "127.0.0.1",
                port,
                deadline,
                deadline,
                Duration::from_secs(5),
                NetworkCancellation::default(),
            )
            .unwrap();
        let start = Instant::now();
        assert_eq!(
            stream.read_exact(&mut [0; 100]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(
            stream.write(b"no retry").unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        drop(stream);
        server.join().unwrap();
    }

    #[test]
    fn cancellation_after_stream_progress_closes_peer_and_refuses_reuse() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (settled, observe_settlement) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let accept_deadline = Instant::now() + Duration::from_secs(2);
            let mut peer = loop {
                match listener.accept() {
                    Ok((peer, _)) => break peer,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < accept_deadline,
                            "stream test accept timed out"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("stream test accept failed: {error}"),
                }
            };
            peer.set_nonblocking(false).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            peer.write_all(b"first-event").unwrap();
            // Deliberately stop producing events until the client has observed
            // cancellation. This is a silent stream, not an EOF or DNS test.
            observe_settlement
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            assert_eq!(
                peer.read(&mut [0; 1]).unwrap(),
                0,
                "cancelled stream retained its peer connection"
            );
        });
        let cancellation = NetworkCancellation::default();
        let deadline = MonotonicDeadline::after(Duration::from_secs(10));
        let mut stream = context("127.0.0.1:53".parse().unwrap())
            .connect(
                "127.0.0.1",
                port,
                deadline,
                deadline,
                Duration::from_secs(5),
                cancellation.clone(),
            )
            .unwrap();
        stream.complete_setup().unwrap();
        let mut first = [0; 11];
        stream.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"first-event");
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            cancellation.cancel();
        });
        let start = Instant::now();
        assert_eq!(
            stream.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(
            stream.write(b"must-not-resume").unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        settled.send(()).unwrap();
        cancel.join().unwrap();
        server.join().unwrap();
        // Local socket settlement is deliberately not a remote non-commit proof.
    }

    #[test]
    fn idle_timeout_closes_a_silent_stream_without_renewing_the_absolute_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (settled, observe_settlement) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            observe_settlement
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
        });
        let deadline = MonotonicDeadline::after(Duration::from_secs(2));
        let mut stream = context("127.0.0.1:53".parse().unwrap())
            .connect(
                "127.0.0.1",
                port,
                deadline,
                deadline,
                Duration::from_millis(50),
                NetworkCancellation::default(),
            )
            .unwrap();
        stream.complete_setup().unwrap();
        let start = Instant::now();
        assert_eq!(
            stream.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(stream.socket.is_none(), "idle timeout retained its socket");
        assert_eq!(
            stream.write(b"must-not-resume").unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        settled.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn zero_idle_timeout_refuses_before_dns_or_socket_creation() {
        let dns = UdpSocket::bind("127.0.0.1:0").unwrap();
        let context = context(dns.local_addr().unwrap());
        let deadline = MonotonicDeadline::after(Duration::from_secs(1));
        let error = context
            .connect(
                "silent.example",
                443,
                deadline,
                deadline,
                Duration::ZERO,
                NetworkCancellation::default(),
            )
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            dns.recv_from(&mut [0; 512]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    fn exercise_stalled_write(cancel: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (settled, observe_settlement) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let accept_deadline = Instant::now() + Duration::from_secs(2);
            let mut peer = loop {
                match listener.accept() {
                    Ok((peer, _)) => break peer,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < accept_deadline,
                            "write test accept timed out"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("write test accept failed: {error}"),
                }
            };
            peer.set_nonblocking(false).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            peer.write_all(b"ready").unwrap();
            // Keep the peer alive without draining any request bytes. This
            // exercises send-side backpressure, not a closed-socket error.
            observe_settlement
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            drop(peer);
        });
        let cancellation = NetworkCancellation::default();
        let deadline = MonotonicDeadline::after(Duration::from_secs(10));
        let mut stream = context("127.0.0.1:53".parse().unwrap())
            .connect(
                "127.0.0.1",
                port,
                deadline,
                deadline,
                Duration::from_secs(5),
                cancellation.clone(),
            )
            .unwrap();
        stream.complete_setup().unwrap();
        let mut ready = [0; 5];
        stream.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        // Test-only host setup: fill the actual socket until it reports
        // backpressure BEFORE arming expiry/cancellation. Otherwise a delayed
        // first write could pass by testing only already-expired admission.
        let buffer = [0x5a; 64 * 1024];
        let mut sent = 0;
        let fill_deadline = MonotonicDeadline::after(Duration::from_secs(1));
        stream
            .runtime
            .as_ref()
            .unwrap()
            .block_on(bounded(
                fill_deadline,
                &cancellation,
                stream.socket.as_ref().unwrap().writable(),
            ))
            .unwrap();
        loop {
            assert!(!fill_deadline.has_elapsed(), "write test prefill timed out");
            match stream.socket.as_ref().unwrap().try_write(&buffer) {
                Ok(written) => {
                    assert_ne!(written, 0);
                    sent += written;
                    assert!(
                        sent < 64 * 1024 * 1024,
                        "stalled peer did not apply backpressure"
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("write test prefill failed: {error}"),
            }
        }
        assert!(sent > 0, "backpressure was not preceded by actual progress");
        let start = Instant::now();
        let cancel_task = if cancel {
            Some(std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                cancellation.cancel();
            }))
        } else {
            stream.narrow_deadline(MonotonicDeadline::after(Duration::from_millis(80)));
            None
        };
        // Reuse one bounded buffer. The finite aggregate ceiling also makes
        // this fixture fail rather than flooding an unexpectedly draining peer.
        let error = loop {
            match stream.write(&buffer) {
                Ok(written) => {
                    assert_ne!(written, 0, "write made no progress without refusing");
                    sent += written;
                    assert!(
                        sent < 64 * 1024 * 1024,
                        "stalled peer did not apply backpressure"
                    );
                }
                Err(error) => break error,
            }
        };
        assert_eq!(
            error.kind(),
            if cancel {
                io::ErrorKind::ConnectionAborted
            } else {
                io::ErrorKind::TimedOut
            }
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(stream.socket.is_none(), "failed write retained its socket");
        assert_eq!(
            stream.write(b"must-not-resume").unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            stream.flush().unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        settled.send(()).unwrap();
        if let Some(task) = cancel_task {
            task.join().unwrap();
        }
        server.join().unwrap();
        // Buffered bytes may have reached the peer: never infer remote
        // non-commit from local cancellation, timeout, or socket release.
    }

    #[test]
    fn stalled_write_obeys_absolute_deadline_and_refuses_reuse() {
        exercise_stalled_write(false);
    }

    #[test]
    fn stalled_write_is_cancellable_and_refuses_reuse() {
        exercise_stalled_write(true);
    }

    #[test]
    fn incompatible_async_context_refuses_before_connecting() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let deadline = MonotonicDeadline::after(Duration::from_secs(1));
            let error = context("127.0.0.1:53".parse().unwrap())
                .connect(
                    "127.0.0.1",
                    1,
                    deadline,
                    deadline,
                    Duration::from_secs(5),
                    NetworkCancellation::default(),
                )
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        });
    }
}
