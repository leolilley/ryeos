//! Exact, deadline-bound loopback socket mechanics.
//!
//! This grants no HTTP semantics or peer identity. Another same-host process
//! can connect first; callers must separately authenticate and validate the
//! protocol carried by a connection. A failed write may have sent a prefix.

use crate::exec::{
    DescriptorTransferBounds, InheritedDescriptorAuthority, InheritedDescriptorTransferSender,
    ReceivedDescriptorAuthority, retain_fork_sensitive_descriptors,
};
use crate::time::{Duration, MonotonicDeadline};
use std::io::{self, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub struct ExactLoopbackListener {
    listener: ListenerOwner,
    address: SocketAddr,
    interrupted: Arc<AtomicBool>,
}

enum ListenerOwner {
    Local(TcpListener),
    Transferred(ReceivedDescriptorAuthority),
}

/// A registered listener bound in the caller's current network namespace.
/// The caller must prove that namespace is the admitted isolated target
/// namespace before binding; this type does not grant namespace authority.
pub struct LoopbackListenerTransferSource {
    authority: InheritedDescriptorAuthority,
    address: SocketAddr,
}

/// Bring up only `lo` in the caller's current network namespace. The sandbox
/// owner calls this immediately after a successful CLONE_NEWNET; invoking it
/// outside that boundary would mutate the wrong namespace.
pub(crate) fn activate_loopback_in_current_namespace() -> io::Result<()> {
    #[repr(C)]
    struct InterfaceRequest {
        name: [libc::c_char; libc::IFNAMSIZ],
        // Linux ifreq's largest union member occupies 24 bytes on supported
        // 64-bit targets. The flags field starts at offset IFNAMSIZ.
        data: [u8; 24],
    }
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socket returned a uniquely owned descriptor.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut request = InterfaceRequest {
        name: [0; libc::IFNAMSIZ],
        data: [0; 24],
    };
    request.name[0] = b'l' as libc::c_char;
    request.name[1] = b'o' as libc::c_char;
    // SAFETY: request has the Linux ifreq name and union layout and is writable.
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS, &mut request) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = i16::from_ne_bytes([request.data[0], request.data[1]]);
    let enabled = flags | libc::IFF_UP as i16;
    request.data[..2].copy_from_slice(&enabled.to_ne_bytes());
    // SAFETY: request contains the exact `lo` name and valid flags field.
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS, &request) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl LoopbackListenerTransferSource {
    pub fn bind_exact(address: SocketAddr) -> io::Result<Self> {
        // Acquire the fork lease before creating the descriptor, so unrelated
        // held children can never inherit an unregistered listener.
        let lease = retain_fork_sensitive_descriptors();
        let listener = ExactLoopbackListener::bind_exact(address)?;
        let ListenerOwner::Local(listener) = listener.listener else {
            unreachable!("bind_exact always creates a local listener")
        };
        let file = std::fs::File::from(OwnedFd::from(listener));
        let authority = InheritedDescriptorAuthority::from_owned_file(file, &lease)
            .map_err(io::Error::other)?;
        Ok(Self { authority, address })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The transfer is one-shot; failure cannot be retried with an ambiguous
    /// receiver. The payload must bind the socket to the caller's attempt.
    pub fn transfer(
        self,
        sender: InheritedDescriptorTransferSender,
        payload: &[u8],
        deadline: MonotonicDeadline,
    ) -> io::Result<()> {
        sender.send(
            payload,
            &[self.authority],
            DescriptorTransferBounds::new(payload.len(), 1)?,
            deadline,
        )
    }
}

pub struct BoundedLoopbackStream {
    stream: TcpStream,
    deadline: MonotonicDeadline,
    interrupted: Arc<AtomicBool>,
}

/// A cloneable, one-way interruption handle for one exact listener and its
/// accepted streams. Interruption settles no process or protocol outcome.
#[derive(Clone)]
pub struct LoopbackInterrupt {
    interrupted: Arc<AtomicBool>,
}

impl LoopbackInterrupt {
    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::Release);
    }
}

fn check_interrupted(interrupted: &AtomicBool) -> io::Result<()> {
    if interrupted.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "loopback operation interrupted",
        ));
    }
    Ok(())
}

fn verify_transferred_listener(fd: std::os::fd::RawFd, expected: SocketAddr) -> io::Result<()> {
    let SocketAddr::V4(expected) = expected else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "transferred listener requires IPv4",
        ));
    };
    let mut socket_type: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    // SAFETY: both output pointers reference writable objects of the supplied length.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut length,
        )
    } != 0
        || length as usize != std::mem::size_of_val(&socket_type)
        || socket_type != libc::SOCK_STREAM
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transferred descriptor is not a stream socket",
        ));
    }
    let mut listening: libc::c_int = 0;
    length = std::mem::size_of_val(&listening) as libc::socklen_t;
    // SAFETY: both output pointers reference writable objects of the supplied length.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            (&mut listening as *mut libc::c_int).cast(),
            &mut length,
        )
    } != 0
        || length as usize != std::mem::size_of_val(&listening)
        || listening != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transferred socket is not listening",
        ));
    }
    // SAFETY: fcntl reads flags and does not modify the descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || flags & libc::O_NONBLOCK == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transferred listener is blocking",
        ));
    }
    // SAFETY: zero is a valid initial sockaddr_in representation.
    let mut bound: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    length = std::mem::size_of_val(&bound) as libc::socklen_t;
    // SAFETY: bound is writable and length specifies its exact capacity.
    if unsafe {
        libc::getsockname(
            fd,
            (&mut bound as *mut libc::sockaddr_in).cast(),
            &mut length,
        )
    } != 0
        || length as usize != std::mem::size_of_val(&bound)
        || bound.sin_family != libc::AF_INET as libc::sa_family_t
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transferred listener is not IPv4",
        ));
    }
    let actual = std::net::SocketAddrV4::new(
        std::net::Ipv4Addr::from(bound.sin_addr.s_addr.to_ne_bytes()),
        u16::from_be(bound.sin_port),
    );
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "transferred listener address differs",
        ));
    }
    Ok(())
}

impl ExactLoopbackListener {
    /// Transfer the exact listener over a registered inherited channel that
    /// the caller already bound to the intended peer launch. This consumes
    /// the sender's listener even on uncertainty; no retry or alternate peer
    /// is inferred from a successful kernel send.
    #[cfg(target_os = "linux")]
    pub fn transfer_over_inherited_duplex(
        self,
        channel: &mut crate::InheritedDuplexChannel,
        deadline: MonotonicDeadline,
    ) -> io::Result<()> {
        let fd = match &self.listener {
            ListenerOwner::Local(listener) => listener.as_raw_fd(),
            ListenerOwner::Transferred(authority) => authority.file().as_raw_fd(),
        };
        channel.send_descriptor_frame(fd, deadline)
    }

    /// Receive and revalidate one exact listener. Application-level source,
    /// attempt and relay-readiness checks are still required before releasing
    /// the producer; this proves only the transferred socket's local facts.
    #[cfg(target_os = "linux")]
    pub fn receive_over_inherited_duplex(
        channel: &mut crate::InheritedDuplexChannel,
        address: SocketAddr,
        deadline: MonotonicDeadline,
    ) -> io::Result<Self> {
        let descriptor = channel.receive_descriptor_frame(deadline)?;
        Self::from_transferred(descriptor, address)
    }

    /// Bind the exact admitted endpoint. Port zero is deliberately unsupported:
    /// callers must not silently choose a new provider origin after admission.
    pub fn bind_exact(address: SocketAddr) -> io::Result<Self> {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "loopback listener requires an exact nonzero loopback address",
            ));
        }
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        if listener.local_addr()? != address {
            return Err(io::Error::other(
                "loopback listener bound a different address",
            ));
        }
        Ok(Self {
            listener: ListenerOwner::Local(listener),
            address,
            interrupted: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Adopt a registered SCM_RIGHTS listener without reopening a host
    /// endpoint. The exact bound address and listening/nonblocking socket
    /// state are checked before any connection can be accepted.
    pub fn from_transferred(
        authority: ReceivedDescriptorAuthority,
        address: SocketAddr,
    ) -> io::Result<Self> {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid loopback address",
            ));
        }
        verify_transferred_listener(authority.file().as_raw_fd(), address)?;
        Ok(Self {
            listener: ListenerOwner::Transferred(authority),
            address,
            interrupted: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn interrupt_handle(&self) -> LoopbackInterrupt {
        LoopbackInterrupt {
            interrupted: Arc::clone(&self.interrupted),
        }
    }

    pub fn accept_until(&self, deadline: MonotonicDeadline) -> io::Result<BoundedLoopbackStream> {
        loop {
            if let Some(stream) = self.try_accept_until(deadline)? {
                return Ok(stream);
            }
            crate::time::sleep(Duration::from_millis(1));
        }
    }

    /// Probe one pending connection without waiting. `None` means the kernel
    /// had no accepted connection at this instant, not that a live producer
    /// can never connect later; callers need separate producer settlement.
    pub fn try_accept_until(
        &self,
        deadline: MonotonicDeadline,
    ) -> io::Result<Option<BoundedLoopbackStream>> {
        check_interrupted(&self.interrupted)?;
        if deadline.has_elapsed() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "loopback accept deadline elapsed",
            ));
        }
        let accepted = match &self.listener {
            ListenerOwner::Local(listener) => listener.accept(),
            ListenerOwner::Transferred(authority) => {
                let fd = unsafe {
                    libc::accept4(
                        authority.file().as_raw_fd(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    )
                };
                if fd < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    // SAFETY: accept4 returned a new uniquely owned fd.
                    let stream = TcpStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
                    let peer = stream.peer_addr()?;
                    Ok((stream, peer))
                }
            }
        };
        match accepted {
            Ok((stream, peer)) => {
                check_interrupted(&self.interrupted)?;
                if deadline.has_elapsed() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "loopback accept completed after deadline",
                    ));
                }
                if !peer.ip().is_loopback() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "nonloopback peer on loopback listener",
                    ));
                }
                stream.set_nonblocking(true)?;
                Ok(Some(BoundedLoopbackStream {
                    stream,
                    deadline,
                    interrupted: Arc::clone(&self.interrupted),
                }))
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                check_interrupted(&self.interrupted)?;
                if deadline.has_elapsed() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "loopback accept deadline elapsed",
                    ));
                }
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

impl BoundedLoopbackStream {
    /// Finish the response side but keep the request side available until the
    /// producer's terminal fence, so later pipelined bytes cannot be hidden.
    pub fn shutdown_write(&self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Write)
    }

    pub fn read_chunk_until(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read_chunk_until_after_pending(output, || {})
    }

    fn read_chunk_until_after_pending(
        &mut self,
        output: &mut [u8],
        mut on_pending: impl FnMut(),
    ) -> io::Result<usize> {
        if output.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "loopback read needs a nonempty buffer",
            ));
        }
        loop {
            check_interrupted(&self.interrupted)?;
            if self.deadline.has_elapsed() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "loopback read deadline elapsed",
                ));
            }
            match self.stream.read(output) {
                Ok(count) => {
                    check_interrupted(&self.interrupted)?;
                    if self.deadline.has_elapsed() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "loopback read completed after deadline",
                        ));
                    }
                    return Ok(count);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    if error.kind() == io::ErrorKind::WouldBlock {
                        on_pending();
                    }
                    crate::time::sleep(Duration::from_millis(1))
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn write_all_until(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < bytes.len() {
            check_interrupted(&self.interrupted)?;
            if self.deadline.has_elapsed() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "loopback write deadline elapsed",
                ));
            }
            match self.stream.write(&bytes[written..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "loopback peer closed before response completed",
                    ));
                }
                Ok(count) => {
                    written += count;
                    check_interrupted(&self.interrupted)?;
                    if self.deadline.has_elapsed() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "loopback write completed after deadline; bytes may have been sent",
                        ));
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    crate::time::sleep(Duration::from_millis(1))
                }
                Err(error) => return Err(error),
            }
        }
        check_interrupted(&self.interrupted)?;
        if self.deadline.has_elapsed() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "loopback write completed after deadline; bytes may have been sent",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_binding_refuses_ephemeral_and_nonloopback_endpoints() {
        assert!(ExactLoopbackListener::bind_exact("127.0.0.1:0".parse().unwrap()).is_err());
        assert!(ExactLoopbackListener::bind_exact("0.0.0.0:1234".parse().unwrap()).is_err());
        assert!(
            LoopbackListenerTransferSource::bind_exact("127.0.0.1:0".parse().unwrap()).is_err()
        );
    }

    #[test]
    fn transferred_listener_refuses_a_stream_that_is_not_a_listening_tcp_socket() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let error =
            verify_transferred_listener(stream.as_raw_fd(), "127.0.0.1:7411".parse().unwrap())
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    #[ignore = "requires loopback socket binding on the host"]
    fn transferred_listener_checks_the_exact_bound_address() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let actual = listener.local_addr().unwrap();
        verify_transferred_listener(listener.as_raw_fd(), actual).unwrap();
        let other = SocketAddr::new(actual.ip(), actual.port().saturating_sub(1));
        assert!(verify_transferred_listener(listener.as_raw_fd(), other).is_err());
    }

    #[test]
    #[ignore = "requires loopback socket binding on the host"]
    fn one_shot_transfer_keeps_the_original_listener_live() {
        use std::io::{Read as _, Write as _};

        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let (receiver, child) = crate::inherited_descriptor_transfer_pair().unwrap();
        let source = LoopbackListenerTransferSource::bind_exact(address).unwrap();
        assert_eq!(source.address(), address);
        source
            .transfer(
                child.into_sender(),
                b"attempt-1",
                MonotonicDeadline::after(Duration::from_secs(2)),
            )
            .unwrap();
        let (payload, mut descriptors) = receiver
            .receive(
                DescriptorTransferBounds::new(32, 1).unwrap(),
                MonotonicDeadline::after(Duration::from_secs(2)),
            )
            .unwrap()
            .into_parts();
        assert_eq!(payload, b"attempt-1");
        assert_eq!(descriptors.len(), 1);
        let listener =
            ExactLoopbackListener::from_transferred(descriptors.pop().unwrap(), address).unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(b"ping").unwrap();
            let mut response = [0; 4];
            stream.read_exact(&mut response).unwrap();
            assert_eq!(&response, b"pong");
        });
        let mut accepted = listener
            .accept_until(MonotonicDeadline::after(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0; 4];
        assert_eq!(accepted.read_chunk_until(&mut request).unwrap(), 4);
        assert_eq!(&request, b"ping");
        accepted.write_all_until(b"pong").unwrap();
        client.join().unwrap();
    }

    #[test]
    fn interruption_is_one_way_and_refuses_operations() {
        let flag = AtomicBool::new(false);
        check_interrupted(&flag).unwrap();
        flag.store(true, Ordering::Release);
        assert_eq!(
            check_interrupted(&flag).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    #[ignore = "requires loopback socket binding on the host"]
    fn interrupt_settles_blocked_accept_without_waiting_for_deadline() {
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let listener = ExactLoopbackListener::bind_exact(address).unwrap();
        let interrupt = listener.interrupt_handle();
        let task = crate::task::spawn_host_task("loopback-interrupt", move || {
            listener
                .accept_until(MonotonicDeadline::after(Duration::from_secs(30)))
                .err()
                .unwrap()
                .kind()
        })
        .unwrap();
        interrupt.interrupt();
        match task.join_until(MonotonicDeadline::after(Duration::from_secs(2))) {
            Ok(Ok(kind)) => assert_eq!(kind, io::ErrorKind::Interrupted),
            Ok(Err(_)) => panic!("interrupted listener task panicked"),
            Err(task) => {
                task.detach();
                panic!("interrupted listener task did not settle");
            }
        }
    }

    #[test]
    #[ignore = "requires loopback socket binding on the host"]
    fn interrupt_settles_partial_request_read_without_waiting_for_deadline() {
        use std::io::Write as _;

        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let listener = ExactLoopbackListener::bind_exact(address).unwrap();
        let interrupt = listener.interrupt_handle();
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(b"POST /responses HTTP/1.1\r\n").unwrap();
        let mut accepted = listener
            .accept_until(MonotonicDeadline::after(Duration::from_secs(30)))
            .unwrap();
        let (ready, started) = std::sync::mpsc::sync_channel(0);
        let task = crate::task::spawn_host_task("loopback-read-interrupt", move || {
            let expected = b"POST /responses HTTP/1.1\r\n";
            let mut first = [0u8; 128];
            let mut count = 0;
            while count < expected.len() {
                count += accepted
                    .read_chunk_until(&mut first[count..expected.len()])
                    .unwrap();
            }
            assert_eq!(&first[..count], expected);
            let mut pending = [0u8; 1];
            let mut ready = Some(ready);
            accepted
                .read_chunk_until_after_pending(&mut pending, || {
                    if let Some(sender) = ready.take() {
                        sender.send(()).unwrap();
                    }
                })
                .unwrap_err()
                .kind()
        })
        .unwrap();
        started
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        interrupt.interrupt();
        match task.join_until(MonotonicDeadline::after(Duration::from_secs(2))) {
            Ok(Ok(kind)) => assert_eq!(kind, io::ErrorKind::Interrupted),
            Ok(Err(_)) => panic!("interrupted loopback reader task panicked"),
            Err(task) => {
                task.detach();
                panic!("interrupted loopback reader task did not settle");
            }
        }
    }

    #[test]
    #[ignore = "requires loopback socket binding on the host"]
    fn exact_binding_refuses_an_occupied_endpoint() {
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(ExactLoopbackListener::bind_exact(reserved.local_addr().unwrap()).is_err());
    }

    #[test]
    #[ignore = "requires loopback socket binding on the host"]
    fn exact_loopback_round_trip_and_accept_timeout() {
        use std::io::{Read as _, Write as _};
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let listener = ExactLoopbackListener::bind_exact(address).unwrap();
        assert_eq!(listener.address(), address);
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(b"ping").unwrap();
            let mut response = [0; 4];
            stream.read_exact(&mut response).unwrap();
            assert_eq!(&response, b"pong");
        });
        let mut accepted = listener
            .accept_until(MonotonicDeadline::after(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0; 4];
        assert_eq!(accepted.read_chunk_until(&mut request).unwrap(), 4);
        assert_eq!(&request, b"ping");
        accepted.write_all_until(b"pong").unwrap();
        client.join().unwrap();
        assert_eq!(
            listener
                .accept_until(MonotonicDeadline::after(Duration::from_millis(5)))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
}
