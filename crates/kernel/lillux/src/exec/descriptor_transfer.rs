//! One-shot inherited descriptor transfer. Linux owns packet framing and
//! descriptor installation; protocol payload meaning and descriptor roles are
//! supplied by the caller, never inferred here.
//! All launch and received descriptor aliases retain the existing registered
//! inheritance owner; unrelated held children close them before readiness.

use std::fs::File;
use std::io;
use std::mem::{size_of, size_of_val, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use super::duplex_deadline::wait_ready;
use super::{
    InheritedDescriptorAuthority, SubprocessRequest, bind_inherited_channel_to_subprocess_request,
    retain_fork_sensitive_descriptors,
};
use crate::time::MonotonicDeadline;

// Mechanical allocation ceilings, not application policy or descriptor roles.
// The caller supplies narrower payload and exact descriptor-count bounds.
const MAX_TRANSFER_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_TRANSFER_DESCRIPTORS: usize = 16;

#[derive(Clone, Copy, Debug)]
pub struct DescriptorTransferBounds {
    maximum_payload_bytes: usize,
    descriptor_count: usize,
}

impl DescriptorTransferBounds {
    pub fn new(maximum_payload_bytes: usize, descriptor_count: usize) -> io::Result<Self> {
        if !(1..=MAX_TRANSFER_PAYLOAD_BYTES).contains(&maximum_payload_bytes)
            || descriptor_count > MAX_TRANSFER_DESCRIPTORS
        {
            return Err(invalid(
                "descriptor transfer bounds exceed the mechanical ceiling",
            ));
        }
        Ok(Self {
            maximum_payload_bytes,
            descriptor_count,
        })
    }

    fn validate_packet(self, bytes: usize, descriptors: usize) -> io::Result<()> {
        if bytes == 0 || bytes > self.maximum_payload_bytes || descriptors != self.descriptor_count
        {
            return Err(invalid(
                "descriptor transfer payload or exact descriptor count differs",
            ));
        }
        Ok(())
    }
}

/// Parent endpoint, intentionally neither cloneable nor reconnectable.
pub struct InheritedDescriptorTransferReceiver {
    socket: InheritedDescriptorAuthority,
}

/// Launch-only ownership of the sender endpoint. Drop the parent's copy after
/// subprocess launch; retaining it would intentionally keep the peer alive.
pub struct InheritedDescriptorTransferChildAuthority {
    socket: InheritedDescriptorAuthority,
}

impl InheritedDescriptorTransferChildAuthority {
    /// Trusted Lillux-only fork probes already own this endpoint; moving it
    /// avoids unsafe raw-fd re-adoption and duplicate registration after fork.
    pub(crate) fn into_sender(self) -> InheritedDescriptorTransferSender {
        InheritedDescriptorTransferSender {
            socket: self.socket,
        }
    }

    pub fn inherited_descriptor(&self) -> Result<u32, String> {
        self.socket.inherited_descriptor()
    }

    pub fn retain_for_child(&self, inherited_fds: &mut Vec<InheritedDescriptorAuthority>) {
        inherited_fds.push(self.socket.clone());
    }

    pub fn bind_to_subprocess_request(
        &self,
        request: &mut SubprocessRequest,
        descriptor_env_name: &str,
        target_fd: u32,
    ) -> Result<(), String> {
        if target_fd <= 2 {
            return Err("inherited packet descriptor overlaps standard I/O".to_owned());
        }
        bind_inherited_channel_to_subprocess_request(
            &self.socket,
            request,
            descriptor_env_name,
            target_fd,
        )
    }
}

/// Child endpoint. Sending consumes it even on failure, so partial or
/// uncertain outcomes cannot accidentally be retried on the same channel.
pub struct InheritedDescriptorTransferSender {
    socket: InheritedDescriptorAuthority,
}

/// An installed descriptor whose raw handle never escapes Lillux. Long-lived
/// authority is closed in unrelated held fork children, rather than holding
/// the process-wide non-Send fork lease for its entire lifetime.
pub struct ReceivedDescriptorAuthority {
    descriptor: InheritedDescriptorAuthority,
}

impl ReceivedDescriptorAuthority {
    /// Share the exact registered owner; no unregistered duplicate is minted.
    pub fn for_child(&self) -> Result<InheritedDescriptorAuthority, String> {
        Ok(self.descriptor.clone())
    }
}

pub struct ReceivedDescriptorTransfer {
    payload: Vec<u8>,
    descriptors: Vec<ReceivedDescriptorAuthority>,
}

impl ReceivedDescriptorTransfer {
    pub fn into_parts(self) -> (Vec<u8>, Vec<ReceivedDescriptorAuthority>) {
        (self.payload, self.descriptors)
    }
}

/// Create an unnamed private connected packet pair. CLOEXEC is atomic for
/// both ends; no filesystem pathname, listener, or ambient peer lookup exists.
pub fn inherited_descriptor_transfer_pair() -> io::Result<(
    InheritedDescriptorTransferReceiver,
    InheritedDescriptorTransferChildAuthority,
)> {
    let lease = retain_fork_sensitive_descriptors();
    let mut descriptors = [-1; 2];
    // SAFETY: the output array has exactly two writable descriptor slots.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            descriptors.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // Adopt both before any fallible operation. The receiver must not be
    // retained in an unrelated direct attachment's pre-exec hold.
    let parent = unsafe { File::from_raw_fd(descriptors[0]) };
    let child = unsafe { File::from_raw_fd(descriptors[1]) };
    Ok((
        InheritedDescriptorTransferReceiver {
            socket: InheritedDescriptorAuthority::from_owned_file(parent, &lease)
                .map_err(io::Error::other)?,
        },
        InheritedDescriptorTransferChildAuthority {
            socket: InheritedDescriptorAuthority::from_owned_file(child, &lease)
                .map_err(io::Error::other)?,
        },
    ))
}

/// Adopt the exact endpoint supplied by trusted launch authority, before
/// installing any sandbox filter that disallows socket inspection.
///
/// # Safety
///
/// The caller grants unique ownership of this live inherited descriptor.
/// No other owning Rust handle may refer to it. The coordinate alone is not
/// authentication; only the trusted launch protocol may supply it.
pub unsafe fn take_inherited_descriptor_transfer_sender(
    descriptor: u32,
) -> io::Result<InheritedDescriptorTransferSender> {
    let descriptor = i32::try_from(descriptor)
        .map_err(|_| invalid("inherited packet descriptor exceeds platform range"))?;
    if descriptor <= libc::STDERR_FILENO {
        return Err(invalid("inherited packet descriptor overlaps standard I/O"));
    }
    let lease = retain_fork_sensitive_descriptors();
    // SAFETY: unique ownership is the caller's explicit precondition.
    let socket = unsafe { File::from_raw_fd(descriptor) };
    super::protect_descriptor_from_exec(&socket).map_err(io::Error::other)?;
    for (option, expected) in [
        (libc::SO_TYPE, libc::SOCK_SEQPACKET),
        (libc::SO_DOMAIN, libc::AF_UNIX),
    ] {
        let mut value = 0i32;
        let mut length = size_of::<i32>() as libc::socklen_t;
        // SAFETY: the value and its length point to valid writable storage.
        if unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&mut value as *mut i32).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if length as usize != size_of::<i32>() || value != expected {
            return Err(invalid("inherited endpoint is not a Unix packet socket"));
        }
    }
    let mut peer: libc::sockaddr_storage = unsafe { zeroed() };
    let mut peer_length = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: the address storage and length are valid for this syscall.
    if unsafe {
        libc::getpeername(
            socket.as_raw_fd(),
            (&mut peer as *mut libc::sockaddr_storage).cast(),
            &mut peer_length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(InheritedDescriptorTransferSender {
        socket: InheritedDescriptorAuthority::from_owned_file(socket, &lease)
            .map_err(io::Error::other)?,
    })
}

impl InheritedDescriptorTransferSender {
    pub fn send(
        self,
        payload: &[u8],
        descriptors: &[InheritedDescriptorAuthority],
        bounds: DescriptorTransferBounds,
        deadline: MonotonicDeadline,
    ) -> io::Result<()> {
        bounds.validate_packet(payload.len(), descriptors.len())?;
        let descriptors: Vec<_> = descriptors
            .iter()
            .map(|authority| {
                // Authority ownership stays alive for the complete send.
                authority.handle.as_raw_fd()
            })
            .collect();
        send_packet(
            self.socket.file().as_raw_fd(),
            payload,
            &descriptors,
            deadline,
        )
    }
}

impl InheritedDescriptorTransferReceiver {
    pub fn receive(
        self,
        bounds: DescriptorTransferBounds,
        deadline: MonotonicDeadline,
    ) -> io::Result<ReceivedDescriptorTransfer> {
        let mut payload = vec![0u8; bounds.maximum_payload_bytes];
        // Receive up to the mechanical ceiling, not merely the expected count:
        // extra delivered descriptors must be adopted and closed on rejection.
        let mut control = control_buffer(MAX_TRANSFER_DESCRIPTORS);
        loop {
            wait_ready(self.socket.file().as_raw_fd(), libc::POLLIN, deadline)?;
            // Never block while holding this lease: it would prevent the very
            // child launch whose packet we need. Readiness can race, so use
            // MSG_DONTWAIT and return to the same deadline on EAGAIN/EINTR.
            let lease = super::retain_fork_sensitive_descriptors_until(deadline)?;
            if deadline.has_elapsed() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "descriptor transfer deadline elapsed",
                ));
            }
            let mut files = Vec::with_capacity(MAX_TRANSFER_DESCRIPTORS);
            let mut iov = libc::iovec {
                iov_base: payload.as_mut_ptr().cast(),
                iov_len: payload.len(),
            };
            let mut message: libc::msghdr = unsafe { zeroed() };
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = control.len() * size_of::<usize>();
            // SAFETY: writable buffers and header remain live for recvmsg.
            // MSG_CMSG_CLOEXEC installs every received fd atomically CLOEXEC.
            let bytes = unsafe {
                libc::recvmsg(
                    self.socket.file().as_raw_fd(),
                    &mut message,
                    libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
                )
            };
            if bytes < 0 {
                let error = io::Error::last_os_error();
                drop(lease);
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    continue;
                }
                return Err(error);
            }
            // Always adopt ancillary rights before testing payload/flags/EOF.
            // Linux closes rights omitted by MSG_CTRUNC; this owns all rights
            // actually delivered in the control buffer.
            let ancillary = unsafe { collect_rights(&message, &mut files) };
            if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
                return Err(invalid(
                    "descriptor transfer packet or ancillary data was truncated",
                ));
            }
            ancillary?;
            if bytes == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "descriptor transfer peer closed or sent an empty packet",
                ));
            }
            let bytes = usize::try_from(bytes).map_err(|_| invalid("invalid packet size"))?;
            bounds.validate_packet(bytes, files.len())?;
            payload.truncate(bytes);
            let descriptors = files
                .into_iter()
                .map(|file| {
                    InheritedDescriptorAuthority::from_owned_file(file, &lease)
                        .map(|descriptor| ReceivedDescriptorAuthority { descriptor })
                        .map_err(io::Error::other)
                })
                .collect::<io::Result<Vec<_>>>()?;
            drop(lease);
            return Ok(ReceivedDescriptorTransfer {
                payload,
                descriptors,
            });
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn control_buffer(count: usize) -> Vec<usize> {
    // usize supplies native cmsghdr alignment. CMSG_SPACE includes padding.
    let bytes = unsafe { libc::CMSG_SPACE((count * size_of::<RawFd>()) as u32) } as usize;
    vec![0usize; bytes.div_ceil(size_of::<usize>())]
}

/// Kernel readiness wait, not a timer-based inspection loop. EINTR, readiness
/// races and every subsequent I/O attempt share the caller's original expiry.

fn send_packet(
    fd: RawFd,
    payload: &[u8],
    descriptors: &[RawFd],
    deadline: MonotonicDeadline,
) -> io::Result<()> {
    let mut control = control_buffer(descriptors.len());
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr().cast_mut().cast(),
        iov_len: payload.len(),
    };
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if !descriptors.is_empty() {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len() * size_of::<usize>();
        // SAFETY: the aligned control buffer holds the complete header and fd array.
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN((size_of_val(descriptors)) as u32) as usize;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                size_of_val(descriptors),
            );
        }
    }
    loop {
        wait_ready(fd, libc::POLLOUT, deadline)?;
        // SAFETY: immutable payload, owned descriptor sources and ancillary
        // storage outlive the syscall. MSG_NOSIGNAL prevents peer loss from
        // terminating the process; MSG_DONTWAIT bounds readiness races.
        let bytes = unsafe { libc::sendmsg(fd, &message, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
        if bytes < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                continue;
            }
            return Err(error);
        }
        if bytes as usize != payload.len() {
            return Err(invalid(
                "descriptor transfer packet was only partially sent",
            ));
        }
        return Ok(());
    }
}

/// Inspect only a recvmsg-owned, kernel-produced control buffer. Adopt each
/// SCM_RIGHTS entry before recording format errors; every rejected packet
/// then drops its delivered descriptors. Do not return early on an unknown
/// ancillary header, since a later header may still own received descriptors.
unsafe fn collect_rights(message: &libc::msghdr, files: &mut Vec<File>) -> io::Result<()> {
    let mut malformed = false;
    let mut header = unsafe { libc::CMSG_FIRSTHDR(message) };
    while !header.is_null() {
        let start = header as usize - message.msg_control as usize;
        let length = unsafe { (*header).cmsg_len };
        let prefix = unsafe { libc::CMSG_LEN(0) } as usize;
        if length < prefix
            || start > message.msg_controllen
            || length > message.msg_controllen - start
        {
            return Err(invalid("malformed descriptor ancillary header"));
        }
        let rights = unsafe {
            (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS
        };
        let bytes = length - prefix;
        if rights {
            let count = bytes / size_of::<RawFd>();
            malformed |= bytes % size_of::<RawFd>() != 0;
            for index in 0..count {
                let fd = unsafe {
                    std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>().add(index))
                };
                if fd < 0 {
                    malformed = true;
                } else {
                    // SAFETY: successful recvmsg grants one new owned fd per
                    // kernel-produced SCM_RIGHTS entry. No duplicate owner exists.
                    files.push(unsafe { File::from_raw_fd(fd) });
                }
            }
        } else {
            malformed = true;
        }
        header = unsafe { libc::CMSG_NXTHDR(message, header) };
    }
    if malformed {
        Err(invalid("unexpected or partial descriptor ancillary data"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::inherited_descriptor_path;
    use crate::time::Duration;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::IntoRawFd;

    fn deadline() -> MonotonicDeadline {
        MonotonicDeadline::after(Duration::from_secs(2))
    }

    fn sender(
        child: InheritedDescriptorTransferChildAuthority,
    ) -> InheritedDescriptorTransferSender {
        InheritedDescriptorTransferSender {
            socket: child.socket,
        }
    }

    fn pipe_probe() -> (File, InheritedDescriptorAuthority) {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        let read = unsafe { File::from_raw_fd(descriptors[0]) };
        let write = unsafe { File::from_raw_fd(descriptors[1]) };
        (read, inherited_descriptor_path(write).unwrap())
    }

    fn assert_no_writer_leaked(mut reader: File) {
        // No process-global fd counting: EOF proves every duplicate of this
        // exact pipe's writer is closed. A leak produces nonblocking EAGAIN.
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn packet_transfers_exact_bytes_cloexec_authority_and_child_copy() {
        let (receiver, child) = inherited_descriptor_transfer_pair().unwrap();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"retained authority").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let authority = inherited_descriptor_path(file).unwrap();
        sender(child)
            .send(
                b"created",
                &[authority],
                DescriptorTransferBounds::new(32, 1).unwrap(),
                deadline(),
            )
            .unwrap();
        let (payload, descriptors) = receiver
            .receive(DescriptorTransferBounds::new(32, 1).unwrap(), deadline())
            .unwrap()
            .into_parts();
        assert_eq!(payload, b"created");
        assert_eq!(descriptors.len(), 1);
        let descriptor = &descriptors[0];
        assert_ne!(
            unsafe { libc::fcntl(descriptor.descriptor.file().as_raw_fd(), libc::F_GETFD) }
                & libc::FD_CLOEXEC,
            0
        );
        let mut read = descriptor.descriptor.file();
        let mut bytes = Vec::new();
        read.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"retained authority");
        let inherited = descriptor.for_child().unwrap();
        let coordinate = inherited.inherited_descriptor().unwrap();
        let mut retained = Vec::new();
        inherited.retain_for_child(&mut retained);
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].file().as_raw_fd() as u32, coordinate);
        assert_ne!(
            unsafe { libc::fcntl(retained[0].file().as_raw_fd(), libc::F_GETFD) }
                & libc::FD_CLOEXEC,
            0
        );
    }

    fn reject_packet_without_leaking(payload: &[u8], sent_count: usize, expected_count: usize) {
        let (receiver, child) = inherited_descriptor_transfer_pair().unwrap();
        let child = sender(child);
        let (probe, writer) = pipe_probe();
        let descriptors = vec![writer.handle.as_raw_fd(); sent_count];
        send_packet(
            child.socket.file().as_raw_fd(),
            payload,
            &descriptors,
            deadline(),
        )
        .unwrap();
        drop(child);
        drop(writer);
        let result = receiver.receive(
            DescriptorTransferBounds::new(4, expected_count).unwrap(),
            deadline(),
        );
        assert!(result.is_err());
        assert_no_writer_leaked(probe);
    }

    #[test]
    fn wrong_and_extra_descriptor_counts_close_every_received_right() {
        reject_packet_without_leaking(b"ok", 0, 1);
        reject_packet_without_leaking(b"ok", 1, 0);
        reject_packet_without_leaking(b"ok", 2, 1);
    }

    #[test]
    fn payload_and_ancillary_truncation_close_received_and_undelivered_rights() {
        reject_packet_without_leaking(b"oversized", 1, 1);
        // Test-only raw sending bypasses the public mechanical ceiling.
        reject_packet_without_leaking(b"ok", MAX_TRANSFER_DESCRIPTORS + 1, 1);
    }

    #[test]
    fn unexpected_ancillary_does_not_hide_later_rights_from_cleanup() {
        let (receiver, child) = inherited_descriptor_transfer_pair().unwrap();
        let enabled = 1i32;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    receiver.socket.file().as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PASSCRED,
                    (&enabled as *const i32).cast(),
                    size_of::<i32>() as libc::socklen_t,
                )
            },
            0,
            "enable kernel credentials: {}",
            io::Error::last_os_error()
        );
        let (probe, writer) = pipe_probe();
        sender(child)
            .send(
                b"ok",
                &[writer],
                DescriptorTransferBounds::new(4, 1).unwrap(),
                deadline(),
            )
            .unwrap();
        assert!(
            receiver
                .receive(DescriptorTransferBounds::new(4, 1).unwrap(), deadline())
                .is_err()
        );
        assert_no_writer_leaked(probe);
    }

    #[test]
    fn sender_rejects_overflow_before_transfer_and_receiver_observes_eof() {
        let (receiver, child) = inherited_descriptor_transfer_pair().unwrap();
        let (probe, writer) = pipe_probe();
        assert!(
            sender(child)
                .send(
                    b"too long",
                    &[writer],
                    DescriptorTransferBounds::new(4, 1).unwrap(),
                    deadline()
                )
                .is_err()
        );
        let error = match receiver.receive(DescriptorTransferBounds::new(4, 1).unwrap(), deadline())
        {
            Err(error) => error,
            Ok(_) => panic!("rejected sender cannot return a packet"),
        };
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_no_writer_leaked(probe);
    }

    #[test]
    fn empty_packet_with_rights_is_rejected_without_a_leak() {
        reject_packet_without_leaking(b"", 1, 1);
    }

    #[test]
    fn disconnected_peer_is_bounded_eof_and_peer_loss_never_raises_sigpipe() {
        let (receiver, child) = inherited_descriptor_transfer_pair().unwrap();
        drop(child);
        let error = match receiver.receive(DescriptorTransferBounds::new(4, 0).unwrap(), deadline())
        {
            Err(error) => error,
            Ok(_) => panic!("disconnected peer cannot return a packet"),
        };
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        let (receiver, child) = inherited_descriptor_transfer_pair().unwrap();
        drop(receiver);
        assert!(
            sender(child)
                .send(
                    b"ok",
                    &[],
                    DescriptorTransferBounds::new(4, 0).unwrap(),
                    deadline()
                )
                .is_err()
        );
    }

    #[test]
    fn expired_deadline_refuses_readiness_without_waiting_for_peer() {
        let (receiver, _child) = inherited_descriptor_transfer_pair().unwrap();
        let error = match receiver.receive(
            DescriptorTransferBounds::new(4, 0).unwrap(),
            MonotonicDeadline::after(Duration::ZERO),
        ) {
            Err(error) => error,
            Ok(_) => panic!("expired receive cannot succeed"),
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn adopting_wrong_descriptor_type_closes_the_transferred_owner() {
        let (probe, writer) = pipe_probe();
        let lease = retain_fork_sensitive_descriptors();
        let file = writer.file().try_clone().unwrap();
        drop(writer);
        // SAFETY: this test transfers its uniquely owned live pipe descriptor.
        assert!(
            unsafe { take_inherited_descriptor_transfer_sender(file.into_raw_fd() as u32) }
                .is_err()
        );
        drop(lease);
        assert_no_writer_leaked(probe);
    }

    #[test]
    fn child_endpoint_uses_existing_exact_subprocess_mapping_authority() {
        let (_receiver, child) = inherited_descriptor_transfer_pair().unwrap();
        let source = child.inherited_descriptor().unwrap();
        let mut request = SubprocessRequest {
            cmd: "unused-typed-request".to_owned(),
            argv0: None,
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: 1.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        };
        child
            .bind_to_subprocess_request(&mut request, "TEST_DESCRIPTOR_TRANSFER", 64)
            .unwrap();
        assert_eq!(
            request.envs,
            vec![("TEST_DESCRIPTOR_TRANSFER".to_owned(), "64".to_owned())]
        );
        assert_eq!(request.inherited_fd_mappings.len(), 1);
        assert_eq!(
            request.inherited_fd_mappings[0]
                .source_descriptor()
                .unwrap(),
            source
        );
        assert_ne!(
            unsafe { libc::fcntl(child.socket.file().as_raw_fd(), libc::F_GETFD) }
                & libc::FD_CLOEXEC,
            0
        );
        assert!(
            child
                .bind_to_subprocess_request(&mut request, "OTHER", 64)
                .is_err()
        );
        assert!(
            child
                .bind_to_subprocess_request(&mut request, "OTHER", 65)
                .is_err()
        );
        assert_eq!(request.inherited_fd_mappings.len(), 1);
        assert_eq!(request.envs.len(), 1);
    }

    fn held_request() -> SubprocessRequest {
        SubprocessRequest {
            // The target is never released or executed: this tests Lillux's
            // actual held pre-exec boundary without an external test program.
            cmd: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            argv0: None,
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: 10.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        }
    }

    fn received_pipe_authority() -> (File, ReceivedDescriptorAuthority) {
        let (receiver, child) = inherited_descriptor_transfer_pair().unwrap();
        let (reader, writer) = pipe_probe();
        sender(child)
            .send(
                b"ok",
                &[writer],
                DescriptorTransferBounds::new(4, 1).unwrap(),
                deadline(),
            )
            .unwrap();
        let (_, mut descriptors) = receiver
            .receive(DescriptorTransferBounds::new(4, 1).unwrap(), deadline())
            .unwrap()
            .into_parts();
        (reader, descriptors.pop().unwrap())
    }

    #[test]
    fn unrelated_held_child_must_not_retain_received_inheritance_copy() {
        let (mut reader, received) = received_pipe_authority();
        let inherited = received.for_child().unwrap();
        let mut retained = Vec::new();
        inherited.retain_for_child(&mut retained);
        drop(inherited);
        drop(received);
        let unrelated = crate::spawn_awaiting_attachment(held_request())
            .expect("hold unrelated child before exec");
        drop(retained);
        // Observe BEFORE reaping the unrelated child. Reaping first would
        // hide precisely the lifetime leak this regression must detect.
        let observation = reader.read(&mut [0]);
        unrelated
            .abort_and_reap()
            .expect("reap unrelated held child");
        assert_eq!(
            observation.expect("unrelated child retained an authority copy"),
            0
        );
    }

    #[test]
    fn unrelated_held_child_must_not_keep_sender_peer_alive() {
        let (receiver, child) = inherited_descriptor_transfer_pair().unwrap();
        let unrelated = crate::spawn_awaiting_attachment(held_request())
            .expect("hold unrelated child before exec");
        drop(child);
        let observation = receiver.receive(
            DescriptorTransferBounds::new(4, 0).unwrap(),
            MonotonicDeadline::after(Duration::from_millis(50)),
        );
        unrelated
            .abort_and_reap()
            .expect("reap unrelated held child");
        let error = match observation {
            Err(error) => error,
            Ok(_) => panic!("closed sender cannot return a packet"),
        };
        assert_eq!(
            error.kind(),
            io::ErrorKind::UnexpectedEof,
            "unrelated child retained the sender endpoint instead of permitting EOF"
        );
    }

    #[test]
    fn intended_held_child_retains_only_requested_registered_authority() {
        let (mut reader, received) = received_pipe_authority();
        let mut request = held_request();
        request.inherited_fds.push(received.for_child().unwrap());
        drop(received);
        let intended =
            crate::spawn_awaiting_attachment(request).expect("preserve requested authority");
        let observation = reader.read(&mut [0]);
        intended.abort_and_reap().expect("reap intended child");
        assert_eq!(observation.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_no_writer_leaked(reader);
    }

    #[test]
    fn pre_fork_failure_releases_registered_request_and_mapping_without_join_deadlock() {
        let (reader, received) = received_pipe_authority();
        let mut request = held_request();
        request.cmd = "invalid\0executable".to_owned();
        request
            .inherited_fd_mappings
            .push(crate::exec::InheritedDescriptorMapping {
                source: received.for_child().unwrap(),
                target_fd: 64,
            });
        drop(received);
        assert!(crate::spawn_awaiting_attachment(request).is_err());
        assert_no_writer_leaked(reader);
    }

    #[test]
    fn intended_mapping_closes_source_and_temporary_copies_before_hold() {
        let (reader, received) = received_pipe_authority();
        let pipe_identity =
            std::fs::read_link(format!("/proc/self/fd/{}", reader.as_raw_fd())).unwrap();
        drop(reader);
        let mut request = held_request();
        request
            .inherited_fd_mappings
            .push(crate::exec::InheritedDescriptorMapping {
                source: received.for_child().unwrap(),
                target_fd: 64,
            });
        drop(received);
        let intended = crate::spawn_awaiting_attachment(request).unwrap();
        let copies = (|| -> io::Result<Vec<String>> {
            let mut matching = Vec::new();
            for entry in std::fs::read_dir(format!("/proc/{}/fd", intended.pid()))? {
                let entry = entry?;
                if std::fs::read_link(entry.path())? == pipe_identity {
                    matching.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
            Ok(matching)
        })();
        intended.abort_and_reap().unwrap();
        assert_eq!(copies.unwrap(), vec!["64"]);
    }

    #[test]
    fn mapping_refuses_unrelated_occupied_target_without_overwriting_it() {
        let (reader, received) = received_pipe_authority();
        let (other_reader, other) = received_pipe_authority();
        let occupied = other.for_child().unwrap();
        let identity = occupied.file_identity().unwrap();
        let mappings = [crate::exec::InheritedDescriptorMapping {
            source: received.for_child().unwrap(),
            target_fd: occupied.inherited_descriptor().unwrap(),
        }];
        let failure = match super::super::prepare_inherited_fd_mappings(
            &mappings,
            &[],
            &std::collections::BTreeSet::new(),
        ) {
            Ok(_) => panic!("unrelated occupied mapping target must be refused"),
            Err(error) => error,
        };
        assert!(
            failure.contains("occupied without request-owned authority"),
            "{failure}"
        );
        assert_eq!(occupied.file_identity().unwrap(), identity);
        drop((mappings, received, occupied, other));
        assert_no_writer_leaked(reader);
        assert_no_writer_leaked(other_reader);
    }

    #[test]
    fn adopted_stdin_channel_does_not_close_later_child_stdin() {
        const ROLE: &str = "LILLUX_STDIN_ADOPTION_TEST_ROLE";
        const CHANNEL: &str = "LILLUX_STDIN_ADOPTION_TEST_CHANNEL";
        const TEST: &str = "exec::descriptor_transfer::tests::adopted_stdin_channel_does_not_close_later_child_stdin";
        match std::env::var(ROLE).ok().as_deref() {
            Some("reader") => {
                let mut value = String::new();
                std::io::stdin().read_to_string(&mut value).unwrap();
                assert_eq!(
                    value,
                    "exact later stdin",
                    "stdin {:?}",
                    std::fs::read_link("/proc/self/fd/0")
                );
                return;
            }
            Some("adopter") => {
                // SAFETY: the parent bound this uniquely owned channel to
                // fd0; no other owning handle exists in this subprocess.
                let channel =
                    unsafe { crate::take_inherited_duplex_channel_from_env(CHANNEL) }.unwrap();
                assert!(channel.stream.inherited_descriptor().unwrap() > 2);
                let mut request = held_request();
                request.args = vec![
                    "--exact".to_owned(),
                    TEST.to_owned(),
                    "--nocapture".to_owned(),
                ];
                request.envs.push((ROLE.to_owned(), "reader".to_owned()));
                request.stdin_data = Some("exact later stdin".to_owned());
                let pending = crate::spawn_awaiting_attachment(request).unwrap();
                let held_stdin = std::fs::read_link(format!("/proc/{}/fd/0", pending.pid()));
                let held_flags =
                    std::fs::read_to_string(format!("/proc/{}/fdinfo/0", pending.pid()));
                let result = pending.release_after_attachment().unwrap().wait();
                assert!(
                    result.success,
                    "held stdin {held_stdin:?}, {held_flags:?}: {}",
                    result.stderr
                );
                assert!(held_stdin.unwrap().to_string_lossy().starts_with("pipe:["));
                let held_flags = held_flags.unwrap();
                let flags = held_flags
                    .lines()
                    .find_map(|line| line.strip_prefix("flags:"))
                    .map(|flags| u64::from_str_radix(flags.trim(), 8).unwrap())
                    .unwrap();
                assert_eq!(flags & libc::O_CLOEXEC as u64, 0);
                drop(channel);
                return;
            }
            None => {}
            Some(role) => panic!("unknown stdin adoption test role {role}"),
        }
        let (_parent, child) = crate::inherited_duplex_channel_pair().unwrap();
        let mut request = held_request();
        request.args = vec![
            "--exact".to_owned(),
            TEST.to_owned(),
            "--nocapture".to_owned(),
        ];
        request.envs.push((ROLE.to_owned(), "adopter".to_owned()));
        child
            .bind_to_subprocess_request(&mut request, CHANNEL, 0)
            .unwrap();
        drop(child);
        let result = crate::run(request);
        assert!(result.success, "{}\n{}", result.stdout, result.stderr);
    }

    #[test]
    fn unrelated_held_child_does_not_retain_duplex_parent_or_clones() {
        let (parent, child) = crate::inherited_duplex_channel_pair().unwrap();
        let parent_clone = parent.try_clone().unwrap();
        super::super::configure_nonblocking_fd(child.channel.file()).unwrap();
        let unrelated = crate::spawn_awaiting_attachment(held_request()).unwrap();
        drop(parent);
        drop(parent_clone);
        let observation = child.channel.file().read(&mut [0]);
        unrelated.abort_and_reap().unwrap();
        assert_eq!(
            observation.expect("unrelated child retained duplex parent"),
            0
        );
    }

    #[test]
    fn unrelated_held_child_does_not_retain_parent_status_reader() {
        let status = crate::supervised_launcher_status_pipe().unwrap();
        let unrelated = crate::spawn_awaiting_attachment(held_request()).unwrap();
        drop(status.reader);
        let observation = status.writer.file().write(b"status");
        unrelated.abort_and_reap().unwrap();
        assert_eq!(observation.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn receive_deadline_returns_during_quiescence_and_deferred_close_settles_queued_rights() {
        let (receiver, child) = inherited_descriptor_transfer_pair().unwrap();
        let endpoint_fd = receiver.socket.file().as_raw_fd();
        let (mut probe, writer) = pipe_probe();
        sender(child)
            .send(
                b"queued",
                &[writer],
                DescriptorTransferBounds::new(8, 1).unwrap(),
                deadline(),
            )
            .unwrap();
        let quiescence = super::super::quiesce_fork_sensitive_descriptors(
            std::time::Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let error = match receiver.receive(
                DescriptorTransferBounds::new(8, 1).unwrap(),
                MonotonicDeadline::after(Duration::from_millis(30)),
            ) {
                Err(error) => error,
                Ok(_) => panic!("receive cannot acquire the quiesced barrier"),
            };
            tx.send(error.kind()).unwrap();
        });
        // The result must arrive while quiescence is still held, including
        // destruction of the consumed receiver. No close/reuse is yet safe.
        let result = rx.recv_timeout(Duration::from_secs(2));
        let live_during_quiescence = unsafe { libc::fcntl(endpoint_fd, libc::F_GETFD) } >= 0;
        let queued_writer = probe.read(&mut [0]);
        drop(quiescence);
        waiter.join().unwrap();
        // The completed consumed-receiver Drop is a prerequisite. Acquiring
        // the public existing barrier now proves its queued rights settled,
        // not merely that the receive returned an error.
        let settlement = super::super::retain_fork_sensitive_descriptors_until(deadline()).unwrap();
        assert_eq!(result.unwrap(), io::ErrorKind::TimedOut);
        assert!(live_during_quiescence);
        assert_eq!(queued_writer.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_no_writer_leaked(probe);
        assert_eq!(unsafe { libc::fcntl(endpoint_fd, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        drop(settlement);
        // Release removed the exact registration, so reuse and re-registration
        // at this coordinate cannot collide with a stale deferred owner.
        let lease = retain_fork_sensitive_descriptors();
        let source = tempfile::tempfile().unwrap();
        let reused = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, endpoint_fd) };
        assert!(reused >= endpoint_fd);
        let reused = InheritedDescriptorAuthority::from_owned_file(
            unsafe { File::from_raw_fd(reused) },
            &lease,
        )
        .unwrap();
        drop(reused);
        drop(lease);
    }

    #[test]
    fn last_owner_close_is_physical_and_clone_contention_returns_authority() {
        let (mut probe, owner) = pipe_probe();
        let clone = owner.clone();
        let (owner, error) = owner.try_close_last_owner(deadline()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            probe.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(clone);
        owner.try_close_last_owner(deadline()).unwrap();
        assert_no_writer_leaked(probe);
    }

    #[test]
    fn last_owner_close_timeout_retains_original_owner_until_explicit_close() {
        let (mut probe, owner) = pipe_probe();
        let coordinate = owner.inherited_descriptor().unwrap();
        let quiescence = super::super::quiesce_fork_sensitive_descriptors(
            std::time::Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            tx.send(
                owner.try_close_last_owner(MonotonicDeadline::after(Duration::from_millis(30))),
            )
            .unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(2));
        drop(quiescence);
        waiter.join().unwrap();
        let (owner, error) = result.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(owner.inherited_descriptor().unwrap(), coordinate);
        assert_eq!(
            probe.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        owner.try_close_last_owner(deadline()).unwrap();
        assert_no_writer_leaked(probe);
    }

    #[test]
    fn unrelated_held_child_must_not_retain_prepared_mapping_copies() {
        let (mut reader, received) = received_pipe_authority();
        let inherited = received.for_child().unwrap();
        let mut request = held_request();
        request
            .inherited_fd_mappings
            .push(crate::exec::InheritedDescriptorMapping {
                source: inherited.clone(),
                target_fd: 64,
            });
        drop(inherited);
        drop(received);
        let intended = crate::spawn_awaiting_attachment(request)
            .expect("hold intended mapped child before exec");
        let unrelated = crate::spawn_awaiting_attachment(held_request())
            .expect("hold unrelated child while mapping lifelines remain in parent");
        // This releases the intended child and its parent's Command-owned
        // source copies/reservations. The unrelated child must own none.
        intended
            .abort_and_reap()
            .expect("reap intended mapped child");
        let observation = reader.read(&mut [0]);
        unrelated
            .abort_and_reap()
            .expect("reap unrelated held child");
        assert_eq!(
            observation.expect("unrelated child retained mapping authority"),
            0
        );
    }

    #[test]
    fn bounds_are_explicit_and_mechanically_finite() {
        assert!(DescriptorTransferBounds::new(0, 0).is_err());
        assert!(DescriptorTransferBounds::new(MAX_TRANSFER_PAYLOAD_BYTES + 1, 0).is_err());
        assert!(DescriptorTransferBounds::new(1, MAX_TRANSFER_DESCRIPTORS + 1).is_err());
        assert!(
            DescriptorTransferBounds::new(MAX_TRANSFER_PAYLOAD_BYTES, MAX_TRANSFER_DESCRIPTORS)
                .is_ok()
        );
    }
}
