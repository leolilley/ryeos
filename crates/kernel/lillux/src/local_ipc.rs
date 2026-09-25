//! Owner-private local byte-stream endpoints.
//!
//! Higher RyeOS layers name protocol and authority. Lillux alone owns the
//! platform socket, descriptor-inheritance, pathname publication, and exact
//! cleanup mechanics used by a process-local broker.

use std::ffi::{CString, OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, bail, ensure};

use crate::{PinnedDirectory, protect_descriptor_from_exec};

// The most restrictive supported sockaddr_un pathname budget is 104 bytes
// including its trailing NUL. Keep the protocol-owned endpoint comfortably
// within that platform boundary before asking the OS to bind or connect it.
const MAX_ENDPOINT_PATH_BYTES: usize = 103;

/// Kernel-authenticated process that connected one Unix stream. Its PID is
/// expressed in the receiver's namespace, not in the peer's namespace. The
/// retained pidfd pins the incarnation; a caller's numeric PID is never proof.
/// Higher layers still authorize the thread/launch and validate birth/group
/// identity against their existing durable process owner.
#[derive(Debug)]
pub struct AuthenticatedUnixPeer {
    pid: i64,
    #[cfg(target_os = "linux")]
    pidfd: std::os::fd::OwnedFd,
}

impl AuthenticatedUnixPeer {
    /// Capture the durable birth/group coordinate from this already
    /// kernel-authenticated peer. Applications receive a portable Lillux
    /// value, never a borrowed pidfd or a platform branch.
    pub fn exact_process_identity(
        &self,
        expected_group_leader: Option<u32>,
    ) -> Result<crate::ExactProcessIdentity> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsFd as _;
            crate::process_control::capture_exact_process_identity_from_pidfd(
                u32::try_from(self.pid).context("authenticated peer PID is outside range")?,
                expected_group_leader,
                self.pidfd.as_fd(),
            )
            .map_err(anyhow::Error::msg)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = expected_group_leader;
            bail!("authenticated exact process identity is unavailable on this OS")
        }
    }
    /// Diagnostic executable-name guard on an already-pinned peer. This is
    /// not content authentication (use executable_digest_exact for that).
    /// Linux comm is mutable and becomes an FD number after descriptor exec;
    /// inspect the kernel executable link, fenced by the retained pidfd.
    pub fn require_executable_name(&self, expected: &OsStr) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt as _;
            if self.has_exited()? {
                bail!("peer exited before executable-name observation");
            }
            let executable = std::fs::read_link(format!("/proc/{}/exe", self.pid))
                .context("observe pinned peer executable")?;
            let name = executable
                .file_name()
                .context("peer executable has no filename")?;
            let bytes = name.as_bytes();
            // Kernel spelling for a still-running unlinked executable, not a
            // predecessor schema or alternate executable-name fallback.
            let bytes = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
            if bytes != expected.as_bytes() || self.has_exited()? {
                bail!("pinned peer does not have the expected live executable name");
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = expected;
            bail!("peer executable observation is unavailable on this OS")
        }
    }
    /// Observe the exact authenticated process, never a numeric-PID lookup.
    pub fn has_exited(&self) -> Result<bool> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            let mut descriptor = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if descriptor.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                bail!("authenticated process observation descriptor failed");
            }
            Ok(descriptor.revents & libc::POLLIN != 0)
        }
        #[cfg(not(target_os = "linux"))]
        bail!("authenticated process exit observation is unavailable on this OS")
    }

    pub fn request_termination(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.signal(libc::SIGTERM)
        }
        #[cfg(not(target_os = "linux"))]
        bail!("authenticated process termination is unavailable on this OS")
    }

    pub fn force_termination(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.signal(libc::SIGKILL)
        }
        #[cfg(not(target_os = "linux"))]
        bail!("authenticated process termination is unavailable on this OS")
    }

    #[cfg(target_os = "linux")]
    fn signal(&self, signal: libc::c_int) -> Result<()> {
        use std::os::fd::AsRawFd as _;
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0u32,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        Ok(())
    }

    /// Digest the actual executable image of this still-live peer at the
    /// caller-admitted size. Replacing the installed pathname cannot turn an
    /// old running daemon into evidence for a new installation. The executable
    /// is reobserved to reject exit/PID reuse or exec during the bounded read.
    pub fn executable_digest_exact(&self, expected_bytes: u64) -> Result<String> {
        #[cfg(target_os = "linux")]
        {
            if self.has_exited()? {
                bail!("authenticated process already exited");
            }
            // Deliberately follow the kernel executable link, not an authored
            // filesystem path. The independent pidfd fences PID reuse.
            let path = format!("/proc/{}/exe", self.pid);
            let image = std::fs::File::open(&path)?;
            let (digest, _) =
                crate::secure_fs::digest_open_regular_file_stable_exact(&image, expected_bytes)?;
            let current = std::fs::File::open(&path)?;
            if self.has_exited()? || !crate::secure_fs::same_open_file_identity(&image, &current)? {
                bail!("authenticated process executable changed during observation");
            }
            Ok(digest)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = expected_bytes;
            bail!("authenticated executable observation is unavailable on this OS")
        }
    }

    #[cfg(unix)]
    pub fn capture(stream: std::os::fd::BorrowedFd<'_>) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = stream;
            bail!("authenticated Unix process identity requires Linux SO_PEERPIDFD")
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::{AsRawFd as _, FromRawFd as _};

            let credentials = peer_credentials(&stream)?;
            if credentials.pid <= 0 {
                bail!("Unix peer process is not visible in the receiver's PID namespace");
            }
            let mut raw_pidfd: libc::c_int = -1;
            let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: the borrowed connected stream and output storage stay
            // live throughout the call. Linux installs a new CLOEXEC pidfd.
            if unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERPIDFD,
                    (&mut raw_pidfd as *mut libc::c_int).cast(),
                    &mut length,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("capture Unix peer pidfd with SO_PEERPIDFD");
            }
            if raw_pidfd < 0 {
                bail!("SO_PEERPIDFD returned an invalid descriptor");
            }
            // Own immediately so any subsequent validation error closes it.
            // SAFETY: successful SO_PEERPIDFD installed this new descriptor.
            let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_pidfd) };
            if length as usize != std::mem::size_of::<libc::c_int>() {
                bail!("SO_PEERPIDFD returned an invalid descriptor length");
            }
            protect_descriptor_from_exec(&pidfd).map_err(anyhow::Error::msg)?;
            Ok(Self {
                pid: i64::from(credentials.pid),
                pidfd,
            })
        }
    }

    pub fn pid(&self) -> i64 {
        self.pid
    }

    #[cfg(target_os = "linux")]
    pub fn pidfd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        self.pidfd.as_fd()
    }
}

/// Capture a peer from an accepted Unix stream without exposing the native
/// descriptor type to an application. The platform-specific socket authority
/// remains entirely in Lillux.
#[cfg(unix)]
pub fn authenticated_unix_peer_from_stream<S: std::os::fd::AsFd>(
    stream: &S,
) -> Result<AuthenticatedUnixPeer> {
    AuthenticatedUnixPeer::capture(stream.as_fd())
}

#[cfg(not(unix))]
pub fn authenticated_unix_peer_from_stream<S>(_stream: &S) -> Result<AuthenticatedUnixPeer> {
    bail!("authenticated Unix peer capture is unavailable on this OS")
}

/// Probe the same peer-identity mechanism used by runtime attachment. No
/// synthetic descriptor or numeric-PID fallback may qualify this capability.
pub fn validate_peer_process_control_support() -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    bail!("authenticated Unix process control requires Linux SO_PEERPIDFD");
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsFd as _;
        let (stream, _other) = std::os::unix::net::UnixStream::pair()
            .context("create Unix socket pair for SO_PEERPIDFD probe")?;
        AuthenticatedUnixPeer::capture(stream.as_fd())?;
        Ok(())
    }
}

/// One connected local byte stream whose descriptor cannot leak across exec.
pub struct LocalDuplexStream {
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
}

/// One-way cancellation of a pending local connection. Cancellation never
/// converts a partially established connection into successful protocol
/// evidence; the connecting descriptor is closed on refusal.
#[derive(Clone, Default)]
pub struct LocalConnectInterrupt(Arc<AtomicBool>);

impl LocalConnectInterrupt {
    pub fn interrupt(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_interrupted(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl LocalDuplexStream {
    /// Capture the kernel-authenticated peer for this exact connected local
    /// channel. Applications do not receive a Unix stream or descriptor.
    pub fn authenticated_peer(&self) -> Result<AuthenticatedUnixPeer> {
        #[cfg(unix)]
        {
            use std::os::fd::AsFd as _;
            AuthenticatedUnixPeer::capture(self.stream.as_fd())
        }
        #[cfg(not(unix))]
        bail!("authenticated local duplex peer capture is unavailable on this OS")
    }

    pub fn with_deadline(
        &mut self,
        deadline: crate::time::MonotonicDeadline,
    ) -> crate::exec::DeadlineDuplexStream<'_> {
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            crate::exec::DeadlineDuplexStream::new(self.stream.as_fd(), deadline)
        }
        #[cfg(not(unix))]
        {
            crate::exec::DeadlineDuplexStream::unsupported(deadline)
        }
    }

    /// Connect to an endpoint minted by [`OwnerPrivateLocalDuplexListener`].
    pub fn connect(endpoint: &Path) -> Result<Self> {
        validate_endpoint_path(endpoint)?;
        #[cfg(not(unix))]
        {
            let _ = endpoint;
            bail!("local duplex endpoints are unavailable on this platform")
        }
        #[cfg(unix)]
        {
            let stream = std::os::unix::net::UnixStream::connect(endpoint)
                .with_context(|| format!("connect local endpoint {}", endpoint.display()))?;
            protect_descriptor_from_exec(&stream).map_err(anyhow::Error::msg)?;
            Ok(Self { stream })
        }
    }

    /// Connect to a socket name beneath an already-pinned directory. The
    /// process-local descriptor path avoids both ambient directory lookup and
    /// the Unix sockaddr length of a potentially deep workspace pathname.
    /// The caller must separately authenticate the protocol peer.
    pub fn connect_at(directory: &PinnedDirectory, name: &OsStr) -> Result<Self> {
        validate_endpoint_name(name.to_str().context("non-UTF8 local endpoint name")?)?;
        directory.ensure_path_binding()?;
        let endpoint = directory.descriptor_path()?.join(name);
        let stream = Self::connect(&endpoint)?;
        directory.ensure_path_binding()?;
        Ok(stream)
    }

    /// Deadline- and cancellation-bound descriptor-rooted Unix connection.
    /// This is the correct form for a retained worker relay: an unresponsive
    /// listener cannot trap its owner in a blocking `connect` after failure.
    pub fn connect_at_until(
        directory: &PinnedDirectory,
        name: &OsStr,
        deadline: crate::time::MonotonicDeadline,
        interrupt: &LocalConnectInterrupt,
    ) -> Result<Self> {
        validate_endpoint_name(name.to_str().context("non-UTF8 local endpoint name")?)?;
        directory.ensure_path_binding()?;
        let endpoint = directory.descriptor_path()?.join(name);
        validate_endpoint_path(&endpoint)?;
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (endpoint, deadline, interrupt);
            bail!("deadline-bound local connections require Linux")
        }
        #[cfg(target_os = "linux")]
        {
            let stream = connect_local_until(&endpoint, deadline, interrupt)?;
            directory.ensure_path_binding()?;
            Ok(Self { stream })
        }
    }

    /// Connect to an endpoint minted by [`OwnerPrivateLocalDuplexListener`]
    /// without allowing a full Unix listen queue to block past `deadline`.
    pub fn connect_until(
        endpoint: &Path,
        deadline: crate::time::MonotonicDeadline,
    ) -> Result<Self> {
        validate_endpoint_path(endpoint)?;
        #[cfg(target_os = "linux")]
        {
            let stream = connect_unix_until(endpoint, deadline)
                .with_context(|| format!("connect local endpoint {}", endpoint.display()))?;
            protect_descriptor_from_exec(&stream).map_err(anyhow::Error::msg)?;
            Ok(Self { stream })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (endpoint, deadline);
            bail!("deadline-bounded local duplex connect requires Linux")
        }
    }

    /// Connect to the trusted PID-1 broker in the caller's isolated runtime.
    ///
    /// A same-UID descendant can unlink and replace a pathname even below the
    /// private tmpfs. Proving the connected peer is namespace PID 1 prevents
    /// that replacement from impersonating the retained broker listener.
    pub fn connect_isolated_runtime_broker(endpoint: &Path) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = endpoint;
            bail!("isolated local broker connections require Linux SO_PEERCRED")
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: getpid has no pointer arguments or caller-owned memory.
            if unsafe { libc::getpid() } <= 1 {
                bail!("isolated local broker client is not a descendant process");
            }
            let connected = Self::connect(endpoint)?;
            let credentials = peer_credentials(&connected.stream)?;
            // SAFETY: geteuid has no pointer arguments or caller-owned memory.
            let client_uid = unsafe { libc::geteuid() };
            if credentials.pid != 1 || credentials.uid != client_uid {
                bail!("local endpoint server is not the isolated runtime broker");
            }
            Ok(connected)
        }
    }

    pub fn try_clone(&self) -> Result<Self> {
        #[cfg(not(unix))]
        bail!("local duplex endpoints are unavailable on this platform");
        #[cfg(unix)]
        {
            let stream = self.stream.try_clone()?;
            protect_descriptor_from_exec(&stream).map_err(anyhow::Error::msg)?;
            Ok(Self { stream })
        }
    }

    /// Wake every alias blocked in local-channel I/O. The descriptor remains
    /// owned by this value; shutdown is used only to coordinate terminal
    /// protocol failure between the connector's reader and writer.
    pub fn shutdown(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: `self.stream` retains the connected socket for the
            // complete call and shutdown neither closes nor transfers it.
            if unsafe { libc::shutdown(self.stream.as_raw_fd(), libc::SHUT_RDWR) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "local duplex shutdown is unavailable",
            ))
        }
    }

    /// Finish only the response direction while retaining the read side to
    /// detect additional bytes until the producer's terminal fence.
    pub fn shutdown_write(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            self.stream.shutdown(std::net::Shutdown::Write)
        }
        #[cfg(not(unix))]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "local duplex write shutdown is unavailable",
            ))
        }
    }
}

#[cfg(target_os = "linux")]
fn connect_unix_until(
    endpoint: &Path,
    deadline: crate::time::MonotonicDeadline,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;

    if deadline.has_elapsed() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "local connect deadline elapsed",
        ));
    }
    let _descriptor_lease = crate::exec::retain_fork_sensitive_descriptors_until(deadline)
        .map_err(std::io::Error::other)?;
    let descriptor = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let raw = descriptor.as_raw_fd();
    let status = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if status < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, status | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let descriptor_flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if descriptor_flags < 0
        || unsafe { libc::fcntl(raw, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }

    let path = endpoint.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if path.len() >= address.sun_path.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "local endpoint path exceeds sockaddr bound",
        ));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            path.as_ptr(),
            address.sun_path.as_mut_ptr().cast::<u8>(),
            path.len(),
        );
    }
    address.sun_path[path.len()] = 0;
    let address_length = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        .saturating_add(path.len())
        .saturating_add(1);
    let result = unsafe {
        libc::connect(
            raw,
            (&address as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            address_length as libc::socklen_t,
        )
    };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS)
            && error.raw_os_error() != Some(libc::EAGAIN)
        {
            return Err(error);
        }
        wait_unix_connect(raw, deadline)?;
    }
    if deadline.has_elapsed() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "local connect deadline elapsed",
        ));
    }
    // Nonblocking mode is required only while connect/poll owns the raw fd.
    // LocalDuplexStream's ordinary Read/Write implementations are blocking;
    // bounded callers use DeadlineDuplexStream, which adds MSG_DONTWAIT.
    let connected_status = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if connected_status < 0
        || unsafe { libc::fcntl(raw, libc::F_SETFL, connected_status & !libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let stream = std::os::unix::net::UnixStream::from(descriptor);
    Ok(stream)
}

#[cfg(target_os = "linux")]
fn wait_unix_connect(
    descriptor: std::os::fd::RawFd,
    deadline: crate::time::MonotonicDeadline,
) -> std::io::Result<()> {
    loop {
        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "local connect deadline elapsed",
            ));
        }
        let timeout_ms = remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
            .min(i32::MAX as u128) as i32;
        let mut readiness = libc::pollfd {
            fd: descriptor,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut readiness, 1, timeout_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        if deadline.has_elapsed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "local connect deadline elapsed",
            ));
        }
        if readiness.revents & libc::POLLNVAL != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "local connect descriptor is not live",
            ));
        }
        if readiness.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) == 0 {
            continue;
        }
        let mut socket_error: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                descriptor,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut socket_error as *mut libc::c_int).cast(),
                &mut length,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if length as usize != std::mem::size_of_val(&socket_error) {
            return Err(std::io::Error::other(
                "local connect returned an invalid socket error length",
            ));
        }
        if socket_error != 0 {
            return Err(std::io::Error::from_raw_os_error(socket_error));
        }
        return Ok(());
    }
}

impl Read for LocalDuplexStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(not(unix))]
        {
            let _ = buffer;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "local duplex endpoints are unavailable on this platform",
            ))
        }
        #[cfg(unix)]
        self.stream.read(buffer)
    }
}

impl Write for LocalDuplexStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        #[cfg(not(unix))]
        {
            let _ = buffer;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "local duplex endpoints are unavailable on this platform",
            ))
        }
        #[cfg(unix)]
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        #[cfg(not(unix))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "local duplex endpoints are unavailable on this platform",
        ));
        #[cfg(unix)]
        self.stream.flush()
    }
}

/// One exact, uniquely named local listener inside a pinned private root.
///
/// The endpoint name is random per worker boot, is never used as daemon
/// authentication, and is removed only if the published directory entry still
/// names the socket inode created here. The retained listener remains the
/// communication authority if an untrusted same-UID workload later unlinks or
/// replaces the pathname; isolated clients additionally prove their connected
/// server is namespace PID 1 before exchanging protocol bytes.
pub struct OwnerPrivateLocalDuplexListener {
    #[cfg(unix)]
    listener: std::os::unix::net::UnixListener,
    endpoint: PathBuf,
    #[cfg(unix)]
    parent: std::fs::File,
    #[cfg(unix)]
    name: OsString,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl OwnerPrivateLocalDuplexListener {
    /// Bind below the sandbox's private tmpfs runtime view.
    ///
    /// This constructor is intentionally unavailable to an ordinary host
    /// process: the trusted sandbox target must be PID 1 in its fresh PID
    /// namespace, and accepted peers must later prove visibility inside that
    /// same namespace. The endpoint therefore never enters a project tree or
    /// a host-visible same-UID directory.
    pub fn bind_isolated_runtime(directory_name: &str, stem: &str) -> Result<Self> {
        require_isolated_runtime_init()?;
        validate_private_directory_name(directory_name)?;
        let tmp = PinnedDirectory::open(Path::new("/tmp"))?
            .ok_or_else(|| anyhow::anyhow!("isolated private tmp is absent"))?;
        let root = tmp.open_or_create_child(OsStr::new(directory_name), 0o700)?;
        root.tighten_owner_private_directory()?;
        Self::bind(&root, stem)
    }

    /// Create a fresh socket below `directory` without replacing any entry.
    /// The caller creates and retains the private directory through Lillux.
    pub fn bind(directory: &PinnedDirectory, stem: &str) -> Result<Self> {
        Self::bind_in(directory, stem, false)
    }

    /// Bind through the retained directory descriptor rather than its ambient
    /// pathname. This supports deep private workspaces without granting the
    /// listener authority to resolve a replacement parent directory. Clients
    /// use [`LocalDuplexStream::connect_at`] with the exact returned name.
    pub fn bind_pinned(directory: &PinnedDirectory, stem: &str) -> Result<Self> {
        Self::bind_in(directory, stem, true)
    }

    #[cfg(unix)]
    pub fn endpoint_name(&self) -> &OsStr {
        &self.name
    }

    fn bind_in(directory: &PinnedDirectory, stem: &str, descriptor_rooted: bool) -> Result<Self> {
        validate_stem(stem)?;
        directory.ensure_path_binding()?;
        #[cfg(not(unix))]
        {
            let _ = directory;
            bail!("local duplex endpoints are unavailable on this platform")
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

            let random = crate::crypto::generate_random_bytes::<32>();
            let name = OsString::from(format!("{stem}-{}.sock", &crate::sha256_hex(&random)[..32]));
            let endpoint = directory.path().join(&name);
            let binding_path = if descriptor_rooted {
                directory.descriptor_path()?.join(&name)
            } else {
                endpoint.clone()
            };
            validate_endpoint_path(&binding_path)?;
            let listener = std::os::unix::net::UnixListener::bind(&binding_path)
                .with_context(|| format!("bind fresh local endpoint {}", binding_path.display()))?;
            protect_descriptor_from_exec(&listener).map_err(anyhow::Error::msg)?;
            let metadata = std::fs::symlink_metadata(&binding_path)
                .with_context(|| format!("inspect local endpoint {}", binding_path.display()))?;
            if !metadata.file_type().is_socket() {
                bail!("published local endpoint is not a Unix socket");
            }
            directory.ensure_path_binding()?;
            Ok(Self {
                listener,
                endpoint,
                parent: directory.try_clone_descriptor()?,
                name,
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
    }

    /// Published host pathname for legacy direct-path clients and diagnostics.
    /// For a descriptor-rooted bind this may exceed sockaddr limits; use
    /// `endpoint_name` with `LocalDuplexStream::connect_at` instead.
    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    pub fn accept(&self) -> Result<LocalDuplexStream> {
        #[cfg(not(unix))]
        bail!("local duplex endpoints are unavailable on this platform");
        #[cfg(unix)]
        {
            let (stream, _) = self.listener.accept().context("accept local endpoint")?;
            protect_descriptor_from_exec(&stream).map_err(anyhow::Error::msg)?;
            Ok(LocalDuplexStream { stream })
        }
    }

    /// Wait for one connection until the caller's absolute process-local
    /// deadline. A timeout returns `None`; it never changes the listener or
    /// manufactures a connection outcome. This is the cancellation boundary
    /// for owners that must retain the listener while another process starts.
    pub fn accept_before(
        &self,
        deadline: crate::time::MonotonicDeadline,
    ) -> Result<Option<LocalDuplexStream>> {
        #[cfg(not(unix))]
        {
            let _ = deadline;
            bail!("local duplex endpoints are unavailable on this platform")
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;

            loop {
                let remaining = deadline.remaining();
                if remaining.is_zero() {
                    return Ok(None);
                }
                let timeout_ms = remaining
                    .as_millis()
                    .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
                    .min(i32::MAX as u128) as i32;
                let mut descriptor = libc::pollfd {
                    fd: self.listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: poll receives one live listener descriptor and a
                // bounded timeout derived from the opaque Lillux deadline.
                let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error).context("wait for local endpoint connection");
                }
                if ready == 0 || deadline.has_elapsed() {
                    return Ok(None);
                }
                if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    bail!("local endpoint listener failed while awaiting a connection");
                }
                let (stream, _) = self.listener.accept().context("accept local endpoint")?;
                protect_descriptor_from_exec(&stream).map_err(anyhow::Error::msg)?;
                return Ok(Some(LocalDuplexStream { stream }));
            }
        }
    }

    /// Accept only a descendant visible inside a fresh isolated PID namespace.
    ///
    /// The trusted broker must be namespace PID 1. Linux reports a peer PID
    /// of zero when that peer has no PID mapping in the receiver's namespace;
    /// requiring a nonzero, non-init peer therefore excludes same-UID host
    /// processes that can discover the filesystem pathname. Higher layers do
    /// not inspect raw credentials or infer namespace membership themselves.
    pub fn accept_isolated_descendant(&self) -> Result<LocalDuplexStream> {
        #[cfg(not(target_os = "linux"))]
        bail!("isolated-descendant local endpoints require Linux SO_PEERCRED");
        #[cfg(target_os = "linux")]
        {
            require_isolated_runtime_init()?;
            let (stream, _) = self.listener.accept().context("accept local endpoint")?;
            let credentials = peer_credentials(&stream)?;
            // SAFETY: geteuid has no pointer arguments or caller-owned memory.
            let broker_uid = unsafe { libc::geteuid() };
            if credentials.pid <= 1 || credentials.uid != broker_uid {
                bail!("local endpoint peer is outside the isolated workload process tree");
            }
            protect_descriptor_from_exec(&stream).map_err(anyhow::Error::msg)?;
            Ok(LocalDuplexStream { stream })
        }
    }
}

#[cfg(target_os = "linux")]
fn peer_credentials(stream: &impl std::os::fd::AsFd) -> Result<libc::ucred> {
    use std::os::fd::AsRawFd as _;

    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::zeroed();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the connected descriptor is live and the credential
    // buffer/length pointers remain writable for the complete call.
    if unsafe {
        libc::getsockopt(
            stream.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("read local peer credentials");
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        bail!("local peer credentials have an unexpected size");
    }
    // SAFETY: successful getsockopt initialized the full ucred value.
    Ok(unsafe { credentials.assume_init() })
}

fn require_isolated_runtime_init() -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    bail!("isolated local runtime endpoints require Linux PID namespaces");
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getpid has no pointer arguments or caller-owned memory.
        if unsafe { libc::getpid() } != 1 {
            bail!("isolated local broker is not PID 1 in its namespace");
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for OwnerPrivateLocalDuplexListener {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd as _;

        let Ok(name) = os_name_cstring(&self.name) else {
            return;
        };
        let mut observed = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: the parent descriptor and NUL-terminated child name remain
        // live for this call; the output points at writable stat storage.
        if unsafe {
            libc::fstatat(
                self.parent.as_raw_fd(),
                name.as_ptr(),
                observed.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return;
        }
        // SAFETY: successful fstatat initialized the complete stat value.
        let observed = unsafe { observed.assume_init() };
        if observed.st_dev as u64 != self.device
            || observed.st_ino as u64 != self.inode
            || observed.st_mode & libc::S_IFMT != libc::S_IFSOCK
        {
            return;
        }
        // SAFETY: exact identity was checked descriptor-relatively above.
        let _ = unsafe { libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), 0) };
    }
}

fn validate_stem(stem: &str) -> Result<()> {
    if stem.is_empty()
        || stem.len() > 64
        || !stem
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("local endpoint stem is not canonical");
    }
    Ok(())
}

fn validate_private_directory_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name == "."
        || name == ".."
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        bail!("private local endpoint directory name is not canonical");
    }
    Ok(())
}

fn validate_endpoint_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_ENDPOINT_PATH_BYTES
        || name == "."
        || name == ".."
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        bail!("local endpoint name is not canonical");
    }
    Ok(())
}

fn validate_endpoint_path(endpoint: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let bytes = endpoint.as_os_str().as_bytes();
        if !endpoint.is_absolute() || bytes.is_empty() || bytes.len() > MAX_ENDPOINT_PATH_BYTES {
            bail!("local endpoint path is not a bounded absolute path");
        }
        if bytes.contains(&0) {
            bail!("local endpoint path contains NUL");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = endpoint;
        bail!("local duplex endpoints are unavailable on this platform")
    }
}

#[cfg(target_os = "linux")]
fn connect_local_until(
    endpoint: &Path,
    deadline: crate::time::MonotonicDeadline,
    interrupt: &LocalConnectInterrupt,
) -> Result<std::os::unix::net::UnixStream> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;

    ensure!(!interrupt.is_interrupted(), "local connect was interrupted");
    ensure!(!deadline.has_elapsed(), "local connect deadline elapsed");
    let path = endpoint.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    ensure!(
        path.len() < address.sun_path.len(),
        "local endpoint exceeds sockaddr capacity"
    );
    for (target, byte) in address.sun_path.iter_mut().zip(path.iter().copied()) {
        *target = byte as libc::c_char;
    }
    let address_len = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        .checked_add(path.len())
        .and_then(|size| size.checked_add(1))
        .context("local endpoint sockaddr length overflow")?;
    // SAFETY: socket takes no caller-owned pointers and returns one owned FD.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("create bounded local socket");
    }
    // SAFETY: a successful socket call returned a fresh FD owned here.
    let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: address is initialized, NUL-terminated, and its exact supplied
    // length fits sockaddr_un. The live descriptor retains the socket.
    let connected = unsafe {
        libc::connect(
            descriptor.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            address_len as libc::socklen_t,
        )
    };
    if connected != 0 {
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
        ) {
            return Err(error).context("connect bounded local socket");
        }
        loop {
            ensure!(!interrupt.is_interrupted(), "local connect was interrupted");
            let remaining = deadline.remaining();
            ensure!(!remaining.is_zero(), "local connect deadline elapsed");
            let timeout_ms = remaining.as_millis().min(10).max(1) as i32;
            let mut poll = libc::pollfd {
                fd: descriptor.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: poll sees one retained descriptor and writable pollfd.
            let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("wait for bounded local connect");
            }
            if ready == 0 {
                continue;
            }
            let mut socket_error = 0;
            let mut error_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: getsockopt writes one initialized c_int and its length.
            if unsafe {
                libc::getsockopt(
                    descriptor.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&raw mut socket_error).cast(),
                    &raw mut error_len,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error()).context("inspect local connect");
            }
            ensure!(
                error_len as usize == std::mem::size_of::<libc::c_int>(),
                "local connect returned an invalid socket error length"
            );
            if socket_error != 0 {
                return Err(std::io::Error::from_raw_os_error(socket_error))
                    .context("bounded local connect failed");
            }
            break;
        }
    }
    ensure!(!interrupt.is_interrupted(), "local connect was interrupted");
    ensure!(
        !deadline.has_elapsed(),
        "local connect completed after deadline"
    );
    let stream = std::os::unix::net::UnixStream::from(descriptor);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

#[cfg(unix)]
fn os_name_cstring(name: &OsStr) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt as _;
    CString::new(name.as_bytes()).context("local endpoint name contains NUL")
}

#[cfg(all(test, target_os = "linux"))]
mod local_connect_tests {
    use super::*;
    use crate::time::{Duration, MonotonicDeadline};

    #[test]
    fn connect_until_shares_its_deadline_with_local_handshake_io() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let listener = OwnerPrivateLocalDuplexListener::bind(&directory, "connect-test").unwrap();
        let endpoint = listener.endpoint().to_path_buf();
        let deadline = MonotonicDeadline::after(Duration::from_secs(2));
        let accept = std::thread::spawn(move || {
            listener
                .accept_before(deadline)
                .unwrap()
                .expect("deadline-bounded connect did not arrive")
        });

        let mut client = LocalDuplexStream::connect_until(&endpoint, deadline).unwrap();
        let mut server = accept.join().unwrap();
        client
            .with_deadline(deadline)
            .write_all(b"handshake")
            .unwrap();
        let mut received = [0; 9];
        server
            .with_deadline(deadline)
            .read_exact(&mut received)
            .unwrap();
        assert_eq!(&received, b"handshake");
    }

    #[test]
    fn connect_until_refuses_an_expired_deadline_before_contact() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let listener = OwnerPrivateLocalDuplexListener::bind(&directory, "expired-test").unwrap();
        let error = LocalDuplexStream::connect_until(
            listener.endpoint(),
            MonotonicDeadline::after(Duration::ZERO),
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("deadline"));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod peer_process_tests {
    use super::*;
    use std::os::fd::{AsFd as _, AsRawFd as _};

    #[test]
    fn pinned_local_endpoint_works_below_deep_private_root() {
        let temporary = tempfile::tempdir().unwrap();
        let mut directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        for _ in 0..8 {
            directory = directory
                .create_child(OsStr::new("deep-private-root"), 0o700)
                .unwrap();
        }
        assert!(directory.path().join("peer.sock").as_os_str().len() > MAX_ENDPOINT_PATH_BYTES);
        let listener = OwnerPrivateLocalDuplexListener::bind_pinned(&directory, "peer").unwrap();
        let mut client =
            LocalDuplexStream::connect_at(&directory, listener.endpoint_name()).unwrap();
        let mut server = listener
            .accept_before(crate::time::MonotonicDeadline::after(
                crate::time::Duration::from_secs(1),
            ))
            .unwrap()
            .unwrap();
        client.write_all(b"pinned endpoint").unwrap();
        let mut received = [0u8; 15];
        server.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"pinned endpoint");
    }

    #[test]
    fn pinned_listener_name_is_always_accepted_by_pinned_connector() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let prefix = directory.descriptor_path().unwrap().as_os_str().len() + 1;
        let stem = "a".repeat(MAX_ENDPOINT_PATH_BYTES - prefix - 38);
        let listener = OwnerPrivateLocalDuplexListener::bind_pinned(&directory, &stem).unwrap();
        assert!(listener.endpoint_name().len() > 85);
        let _client = LocalDuplexStream::connect_at(&directory, listener.endpoint_name()).unwrap();
        assert!(
            listener
                .accept_before(crate::time::MonotonicDeadline::after(
                    crate::time::Duration::from_secs(1),
                ))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn bounded_pinned_connect_refuses_cancel_and_expiry() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let listener = OwnerPrivateLocalDuplexListener::bind_pinned(&directory, "peer").unwrap();
        let interrupt = LocalConnectInterrupt::default();
        let _connected = LocalDuplexStream::connect_at_until(
            &directory,
            listener.endpoint_name(),
            crate::time::MonotonicDeadline::after(crate::time::Duration::from_secs(1)),
            &interrupt,
        )
        .unwrap();
        assert!(
            listener
                .accept_before(crate::time::MonotonicDeadline::after(
                    crate::time::Duration::from_secs(1),
                ))
                .unwrap()
                .is_some()
        );
        interrupt.interrupt();
        assert!(
            LocalDuplexStream::connect_at_until(
                &directory,
                listener.endpoint_name(),
                crate::time::MonotonicDeadline::after(crate::time::Duration::from_secs(1)),
                &interrupt,
            )
            .is_err()
        );
        assert!(
            LocalDuplexStream::connect_at_until(
                &directory,
                listener.endpoint_name(),
                crate::time::MonotonicDeadline::after(crate::time::Duration::ZERO),
                &LocalConnectInterrupt::default(),
            )
            .is_err()
        );
    }

    #[test]
    #[ignore = "native Linux SO_PEERPIDFD qualification; sandboxed kernels may deny the socket option"]
    fn unix_peer_executable_digest_is_exact_and_size_bounded() {
        let (stream, _other) = std::os::unix::net::UnixStream::pair().unwrap();
        let peer = AuthenticatedUnixPeer::capture(stream.as_fd()).unwrap();
        assert!(!peer.has_exited().unwrap());
        let image_path = std::env::current_exe().unwrap();
        peer.require_executable_name(image_path.file_name().unwrap())
            .unwrap();
        assert!(
            peer.require_executable_name(OsStr::new("not-this-executable"))
                .is_err()
        );
        let executable = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let size = executable.metadata().unwrap().len();
        assert!(
            peer.executable_digest_exact(size.saturating_add(1))
                .is_err()
        );
        let expected = crate::secure_fs::digest_open_regular_file_stable_exact(&executable, size)
            .unwrap()
            .0;
        assert_eq!(peer.executable_digest_exact(size).unwrap(), expected);
    }

    #[test]
    #[ignore = "native Linux SO_PEERPIDFD qualification; sandboxed kernels may deny the socket option"]
    fn unix_peer_retains_kernel_pidfd_and_receiver_namespace_coordinate() {
        let (stream, other) = std::os::unix::net::UnixStream::pair().unwrap();
        let peer = AuthenticatedUnixPeer::capture(stream.as_fd()).unwrap();
        assert_eq!(peer.pid(), i64::from(std::process::id()));
        drop(stream);
        drop(other);
        // Socket closure does not release the independently pinned process.
        let flags = unsafe { libc::fcntl(peer.pidfd().as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    peer.pidfd().as_raw_fd(),
                    0,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                )
            },
            0
        );
    }

    #[test]
    fn unix_peer_refuses_non_socket_descriptors() {
        let file = tempfile::tempfile().unwrap();
        assert!(AuthenticatedUnixPeer::capture(file.as_fd()).is_err());
    }

    #[test]
    #[ignore = "native Unix-socket shutdown qualification; tool sandboxes may deny shutdown"]
    fn local_duplex_shutdown_wakes_blocked_alias() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut reader = LocalDuplexStream { stream };
        let interrupt = reader.try_clone().unwrap();
        let blocked = std::thread::spawn(move || reader.read(&mut [0_u8; 1]));
        interrupt.shutdown().unwrap();
        assert_eq!(blocked.join().unwrap().unwrap(), 0);
    }
}
