//! Two-phase binding in the EXISTING occupancy watchdog. Fork before native
//! user/PID transition; bind one actual fork-owned pidfd before child setup.
//! No serialized release authority, timer renewal or signal-as-death receipt.
use super::*;
use sha2::Digest as _;
use std::os::unix::fs::MetadataExt as _;

const PACKET_BYTES: usize = 57;
const READY: u8 = 1;
const BIND: u8 = 2;
const BOUND: u8 = 3;

pub(crate) struct PendingNamespaceWatchdog {
    watchdog: Option<OccupancyWatchdog>,
    control: std::fs::File,
    limit: crate::time::OccupancyLimit,
    cleanup: Duration,
    original_deadline: crate::time::MonotonicDeadline,
    tag: [u8; 32],
    fork_may_exist: bool,
    bind_attempted: bool,
    bound: bool,
}

impl PendingNamespaceWatchdog {
    pub(crate) fn prepare(
        limit: &crate::time::OccupancyLimit,
        cleanup: Duration,
        original_deadline: crate::time::MonotonicDeadline,
    ) -> Result<Self, String> {
        require_original_watchdog_fork_boundary()?;
        let mut tasks = std::fs::read_dir("/proc/self/task")
            .map_err(|error| format!("inspect dedicated namespace launcher: {error}"))?;
        if tasks
            .next()
            .transpose()
            .map_err(|e| e.to_string())?
            .is_none()
            || tasks
                .next()
                .transpose()
                .map_err(|e| e.to_string())?
                .is_some()
        {
            return Err(
                "namespace occupancy requires the original dedicated single-thread launcher".into(),
            );
        }
        limit.validate()?;
        require_window(limit, cleanup, original_deadline)?;
        let cleanup_ns = u64::try_from(cleanup.as_nanos()).map_err(|_| "cleanup overflow")?;
        let service_tick = occupancy_service_cutoff_tick(limit, cleanup, Some(original_deadline))?;
        let mut hash = sha2::Sha256::new();
        hash.update(b"lillux.native-namespace-watchdog/v2");
        hash.update(limit.start.contract_digest.as_bytes());
        hash.update(limit.start.incarnation_digest.as_bytes());
        hash.update(limit.start.tick_ns.to_le_bytes());
        hash.update(limit.maximum_occupancy_ns.to_le_bytes());
        hash.update(cleanup_ns.to_le_bytes());
        hash.update(service_tick.to_le_bytes());
        let tag: [u8; 32] = hash.finalize().into();
        let timer_raw = unsafe { libc::timerfd_create(libc::CLOCK_BOOTTIME, libc::TFD_CLOEXEC) };
        if timer_raw < 0 {
            return Err(format!(
                "create original namespace timer: {}",
                std::io::Error::last_os_error()
            ));
        }
        let timer = unsafe { OwnedFd::from_raw_fd(timer_raw) };
        arm_absolute_boottime_timer(timer.as_raw_fd(), service_tick)?;
        let mut cancellation = [-1; 2];
        if unsafe { libc::pipe2(cancellation.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!(
                "create namespace cancellation: {}",
                std::io::Error::last_os_error()
            ));
        }
        let cancel_reader = unsafe { OwnedFd::from_raw_fd(cancellation[0]) };
        let cancel_writer = unsafe { OwnedFd::from_raw_fd(cancellation[1]) };
        let mut sockets = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                sockets.as_mut_ptr(),
            )
        } != 0
        {
            return Err(format!(
                "create private namespace binding channel: {}",
                std::io::Error::last_os_error()
            ));
        }
        let parent = unsafe { OwnedFd::from_raw_fd(sockets[0]) };
        let child_control = unsafe { OwnedFd::from_raw_fd(sockets[1]) };
        let duplicate = |fd| -> Result<OwnedFd, String> {
            let value = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 8) };
            if value < 0 {
                return Err(format!(
                    "retain namespace watchdog descriptor: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(unsafe { OwnedFd::from_raw_fd(value) })
        };
        // The actual dedicated setup owner is protected even before a native
        // fork exists. Never use a supplied numeric PID or group as fallback.
        let launcher_raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0_u32) };
        if launcher_raw < 0 {
            return Err(format!(
                "pin original native setup owner: {}",
                std::io::Error::last_os_error()
            ));
        }
        let launcher = unsafe { OwnedFd::from_raw_fd(launcher_raw as i32) };
        let inputs = [
            duplicate(cancel_reader.as_raw_fd())?,
            duplicate(timer.as_raw_fd())?,
            duplicate(child_control.as_raw_fd())?,
            duplicate(cancel_writer.as_raw_fd())?,
            duplicate(launcher.as_raw_fd())?,
        ];
        let raw_inputs = inputs.each_ref().map(AsRawFd::as_raw_fd);
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(format!(
                "prefork original namespace watchdog: {}",
                std::io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            unsafe { child_main(raw_inputs, tag) }
        }
        drop(inputs);
        drop(child_control);
        drop(cancel_reader);
        let mut pending = Self {
            watchdog: Some(OccupancyWatchdog {
                pid,
                cancel: Some(std::fs::File::from(cancel_writer)),
            }),
            control: std::fs::File::from(parent),
            limit: limit.clone(),
            cleanup,
            original_deadline,
            tag,
            fork_may_exist: false,
            bind_attempted: false,
            bound: false,
        };
        let expected = packet(READY, tag, pid as u32, 0, 0, 0);
        if let Err(error) = pending.read_ack(&expected) {
            // No native fork can have occurred before this constructor returns.
            // Cancellation here is the distinct, proved prebirth failure.
            pending.cancel_proven_prebirth()?;
            return Err(error);
        }
        Ok(pending)
    }

    pub(crate) fn target_scrub_descriptors(&self) -> [i32; 2] {
        [
            self.control.as_raw_fd(),
            self.watchdog
                .as_ref()
                .and_then(|w| w.cancel.as_ref())
                .map_or(-1, AsRawFd::as_raw_fd),
        ]
    }
    pub(crate) fn mark_fork_intent(&mut self) {
        self.fork_may_exist = true;
    }
    /// Only the actual failing fork syscall permits this prebirth transition.
    pub(crate) fn prove_fork_failed(&mut self) {
        self.fork_may_exist = false;
    }
    pub(crate) fn original_deadline(&self) -> crate::time::MonotonicDeadline {
        self.original_deadline
    }
    pub(crate) fn require_original_window(&self) -> Result<(), String> {
        require_window(&self.limit, self.cleanup, self.original_deadline)
    }

    pub(crate) fn bind_actual_fork(
        &mut self,
        authority: crate::sandbox::NativeNamespaceOccupancyAuthority,
    ) -> Result<(), String> {
        if self.bind_attempted || !self.fork_may_exist {
            return Err("original namespace binding cannot replay or precede fork".into());
        }
        self.bind_attempted = true; // Even an ambiguous send or ACK cannot replay.
        self.require_original_window()?;
        let (lifetime, target) = authority.into_parts();
        let identity = std::fs::File::from(lifetime.try_clone().map_err(|e| e.to_string())?)
            .metadata()
            .map_err(|e| format!("identify actual namespace pidfd: {e}"))?;
        let pid = self
            .watchdog
            .as_ref()
            .ok_or("original watchdog absent")?
            .pid as u32;
        let message = packet(BIND, self.tag, pid, target, identity.dev(), identity.ino());
        let sent = unsafe {
            send_packet(
                self.control.as_raw_fd(),
                &message,
                Some(lifetime.as_raw_fd()),
            )
        };
        if sent != PACKET_BYTES as isize {
            return Err(format!(
                "original namespace transfer uncertain; no retry: {}",
                std::io::Error::last_os_error()
            ));
        }
        let expected = packet(BOUND, self.tag, pid, target, identity.dev(), identity.ino());
        self.read_ack(&expected)?;
        self.require_original_window()?;
        self.bound = true;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<OccupancyWatchdog, String> {
        if !self.bound {
            return Err("native namespace watchdog binding remains unproved".into());
        }
        self.require_original_window()?;
        self.watchdog
            .take()
            .ok_or("original namespace watchdog absent".into())
        // Dropping control is owner loss, NOT cancellation. Child keeps its timer.
    }
    pub(crate) fn cancel_proven_prebirth(&mut self) -> Result<(), String> {
        if self.fork_may_exist || self.bind_attempted {
            return Err("native fork uncertainty cannot cancel original watchdog".into());
        }
        if let Some(watchdog) = self.watchdog.as_mut() {
            watchdog.cancel_and_reap()?;
        }
        self.watchdog = None;
        Ok(())
    }
    fn read_ack(&mut self, expected: &[u8; PACKET_BYTES]) -> Result<(), String> {
        loop {
            self.require_original_window()?;
            let remaining = self.original_deadline.remaining();
            let mut poll = libc::pollfd {
                fd: self.control.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = remaining.as_millis().max(1).min(5_000) as i32;
            let ready = unsafe { libc::poll(&mut poll, 1, ms) };
            if ready < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if ready < 0 {
                return Err(format!(
                    "observe original watchdog ACK: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if ready == 0 {
                continue;
            } // SAME deadline/window, no renewed clock.
            let mut message = [0; PACKET_BYTES];
            match unsafe { receive_packet(self.control.as_raw_fd(), &mut message, 0) } {
                Ok(_) if &message == expected => {
                    self.require_original_window()?;
                    return Ok(());
                }
                Ok(_) => {
                    return Err("original watchdog ACK changed limit, occurrence or target".into());
                }
                Err(libc::EAGAIN | libc::EINTR) => continue,
                Err(error) => {
                    return Err(format!(
                        "original watchdog ACK uncertain: errno {error}; no replay"
                    ));
                }
            }
        }
    }
}

impl Drop for PendingNamespaceWatchdog {
    fn drop(&mut self) {
        // This state comes only from actual fork control, never missing ACK.
        // Once fork intent is entered, Drop/owner loss cannot cancel enforcement.
        if !self.fork_may_exist && !self.bind_attempted {
            let _ = self.cancel_proven_prebirth();
        }
    }
}

fn require_window(
    limit: &crate::time::OccupancyLimit,
    cleanup: Duration,
    deadline: crate::time::MonotonicDeadline,
) -> Result<(), String> {
    if deadline.has_elapsed()
        || !matches!(
            limit.window(cleanup)?,
            crate::time::OccupancyWindowState::Service { .. }
        )
    {
        return Err("original namespace contact/service window expired".into());
    }
    Ok(())
}

fn packet(
    kind: u8,
    tag: [u8; 32],
    watcher: u32,
    target: u32,
    dev: u64,
    ino: u64,
) -> [u8; PACKET_BYTES] {
    let mut message = [0; PACKET_BYTES];
    message[0] = kind;
    message[1..33].copy_from_slice(&tag);
    message[33..37].copy_from_slice(&watcher.to_le_bytes());
    message[37..41].copy_from_slice(&target.to_le_bytes());
    message[41..49].copy_from_slice(&dev.to_le_bytes());
    message[49..57].copy_from_slice(&ino.to_le_bytes());
    message
}

#[repr(C)]
union Control {
    aligned: libc::cmsghdr,
    bytes: [u8; 128],
}

// Below is used after fork: fixed stack buffers, libc syscalls, no allocator,
// locks, formatting, Rust destruction or public provider protocol.
unsafe fn send_packet(fd: i32, message: &[u8; PACKET_BYTES], transfer: Option<i32>) -> isize {
    let mut iov = libc::iovec {
        iov_base: message.as_ptr().cast_mut().cast(),
        iov_len: message.len(),
    };
    let mut control = Control { bytes: [0; 128] };
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    if let Some(transfer) = transfer {
        header.msg_control = (&mut control as *mut Control).cast();
        header.msg_controllen =
            unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&header) };
        unsafe {
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
            libc::CMSG_DATA(cmsg)
                .cast::<i32>()
                .write_unaligned(transfer);
        }
    }
    unsafe { libc::sendmsg(fd, &header, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) }
}

/// Closes every visible surplus FD and the first FD on any malformed packet.
/// Linux closes uninstalled excess rights on control truncation; no FD leaks.
unsafe fn receive_packet(
    fd: i32,
    message: &mut [u8; PACKET_BYTES],
    expected_rights: usize,
) -> Result<i32, i32> {
    let mut iov = libc::iovec {
        iov_base: message.as_mut_ptr().cast(),
        iov_len: message.len(),
    };
    let mut control = Control { bytes: [0; 128] };
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = (&mut control as *mut Control).cast();
    header.msg_controllen = 128;
    let count =
        unsafe { libc::recvmsg(fd, &mut header, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC) };
    if count < 0 {
        return Err(unsafe { *libc::__errno_location() });
    }
    let mut invalid = count != PACKET_BYTES as isize
        || header.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || header.msg_controllen > 128;
    header.msg_controllen = header.msg_controllen.min(128);
    let mut first = -1;
    let mut rights = 0;
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&header) };
    while !cmsg.is_null() {
        let length = unsafe { (*cmsg).cmsg_len };
        let minimum = unsafe { libc::CMSG_LEN(0) } as usize;
        let offset = (cmsg as usize).saturating_sub(header.msg_control as usize);
        if offset > header.msg_controllen
            || length < minimum
            || length > header.msg_controllen.saturating_sub(offset)
        {
            invalid = true;
            break;
        }
        if unsafe {
            (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS
        } {
            let data = length - minimum;
            if data % std::mem::size_of::<i32>() != 0 {
                invalid = true;
            }
            for index in 0..data / std::mem::size_of::<i32>() {
                let value = unsafe {
                    libc::CMSG_DATA(cmsg)
                        .cast::<i32>()
                        .add(index)
                        .read_unaligned()
                };
                rights += 1;
                if first < 0 {
                    first = value;
                } else {
                    unsafe { libc::close(value) };
                }
            }
        } else {
            invalid = true;
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(&header, cmsg) };
    }
    if rights != expected_rights {
        invalid = true;
    }
    if invalid {
        if first >= 0 {
            unsafe { libc::close(first) };
        }
        return Err(libc::EPROTO);
    }
    Ok(first)
}

unsafe fn child_main(inputs: [i32; 5], tag: [u8; 32]) -> ! {
    for (source, destination) in inputs.into_iter().zip([3, 4, 5, 6, 7]) {
        if unsafe { libc::dup2(source, destination) } < 0 {
            unsafe { libc::_exit(125) }
        }
    }
    if unsafe { libc::syscall(libc::SYS_close_range, 8_u32, u32::MAX, 2_u32) } != 0 {
        unsafe { libc::_exit(125) }
    }
    unsafe {
        libc::close(0);
        libc::close(1);
        libc::close(2);
        libc::prctl(libc::PR_SET_PDEATHSIG, 0, 0, 0, 0);
    }
    let watcher = unsafe { libc::getpid() } as u32;
    let ready = packet(READY, tag, watcher, 0, 0, 0);
    let _ = unsafe { send_packet(5, &ready, None) };
    let mut target = -1;
    let mut control_alive = true;
    loop {
        let mut polls = [
            libc::pollfd {
                fd: 3,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: 4,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if control_alive { 5 } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, -1) };
        if result < 0 {
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            if target >= 0 {
                unsafe { kill_occupancy_target_and_exit(target, true) }
            }
            unsafe { kill_occupancy_target_and_exit(7, true) }
        }
        if polls[0].revents != 0 {
            let mut byte = [0_u8; 1];
            if unsafe { libc::read(3, byte.as_mut_ptr().cast(), 1) } == 1 && byte[0] == 1 {
                unsafe { libc::_exit(0) }
            }
            if target >= 0 {
                unsafe { kill_occupancy_target_and_exit(target, true) }
            }
            unsafe { kill_occupancy_target_and_exit(7, true) }
        }
        if polls[1].revents != 0 {
            if target >= 0 {
                unsafe { kill_occupancy_target_and_exit(target, true) }
            }
            // Protect stalled native setup before target transfer with the
            // original launcher's actual pidfd. This is not a death receipt.
            unsafe { kill_occupancy_target_and_exit(7, true) }
        }
        if polls[2].revents != 0 {
            let mut message = [0; PACKET_BYTES];
            let received = unsafe { receive_packet(5, &mut message, 1) };
            let descriptor = match received {
                Ok(fd) => fd,
                Err(libc::EAGAIN | libc::EINTR) => continue,
                Err(_) => {
                    unsafe { libc::close(5) };
                    control_alive = false;
                    continue;
                }
            };
            let kind_ok = message[0] == BIND
                && message[1..33] == tag
                && message[33..37] == watcher.to_le_bytes()
                && message[37..41] != [0; 4];
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            let stat_ok = unsafe { libc::fstat(descriptor, stat.as_mut_ptr()) } == 0;
            let signalled = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    descriptor,
                    0,
                    std::ptr::null::<libc::siginfo_t>(),
                    0_u32,
                )
            };
            let pidfd_ok = signalled == 0 || unsafe { *libc::__errno_location() } == libc::ESRCH;
            let identity_ok = if stat_ok {
                let stat = unsafe { stat.assume_init() };
                message[41..49] == (stat.st_dev as u64).to_le_bytes()
                    && message[49..57] == (stat.st_ino as u64).to_le_bytes()
            } else {
                false
            };
            if target >= 0 || !kind_ok || !pidfd_ok || !identity_ok {
                unsafe { libc::close(descriptor) };
                if target >= 0 {
                    unsafe { kill_occupancy_target_and_exit(target, true) }
                }
                // Invalid or ambiguous binding is not proved prebirth. Keep
                // the original timer and original launcher lifetime pinned.
                unsafe { libc::close(5) };
                control_alive = false;
                continue;
            }
            target = descriptor; // Consumed once, never replaced/reconstructed.
            message[0] = BOUND;
            if unsafe { send_packet(5, &message, None) } != PACKET_BYTES as isize {
                // Lost ACK or controller death cannot cancel enforcement.
                unsafe { libc::close(5) };
                control_alive = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_timer_caps_earlier_original_directive_and_preserves_legacy_resource_boundary() {
        let start = crate::time::occupancy_now().unwrap();
        let limit = crate::time::OccupancyLimit::new(start.clone(), 120_000_000_000).unwrap();
        let cleanup = Duration::from_secs(20);
        let original = crate::time::MonotonicDeadline::after(Duration::from_secs(60));
        let cutoff = occupancy_service_cutoff_tick(&limit, cleanup, Some(original)).unwrap();
        assert_eq!(
            occupancy_service_cutoff_tick(&limit, cleanup, None).unwrap(),
            start.tick_ns + 100_000_000_000
        );
        let raw = unsafe { libc::timerfd_create(libc::CLOCK_BOOTTIME, libc::TFD_CLOEXEC) };
        assert!(raw >= 0);
        let timer = unsafe { OwnedFd::from_raw_fd(raw) };
        arm_absolute_boottime_timer(timer.as_raw_fd(), cutoff).unwrap();
        let original_service_remaining = original.remaining().checked_sub(cleanup).unwrap();
        let mut actual: libc::itimerspec = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::timerfd_gettime(timer.as_raw_fd(), &mut actual) },
            0
        );
        let actual_remaining = Duration::from_secs(u64::try_from(actual.it_value.tv_sec).unwrap())
            + Duration::from_nanos(u64::try_from(actual.it_value.tv_nsec).unwrap());
        assert!(!actual_remaining.is_zero());
        assert!(actual_remaining <= original_service_remaining);
        // This timer control is not a target birth/death/driver receipt.
    }

    #[test]
    fn original_cutoff_refuses_insufficient_cleanup_before_watchdog_birth() {
        let limit = crate::time::OccupancyLimit::new(
            crate::time::occupancy_now().unwrap(),
            120_000_000_000,
        )
        .unwrap();
        let cleanup = Duration::from_secs(1);
        let original = crate::time::MonotonicDeadline::after(cleanup);
        assert!(occupancy_service_cutoff_tick(&limit, cleanup, Some(original)).is_err());
        let expired = crate::time::MonotonicDeadline::after(Duration::ZERO);
        assert!(occupancy_service_cutoff_tick(&limit, cleanup, Some(expired)).is_err());
    }

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut raw = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    raw.as_mut_ptr(),
                )
            },
            0
        );
        unsafe { (OwnedFd::from_raw_fd(raw[0]), OwnedFd::from_raw_fd(raw[1])) }
    }

    #[test]
    fn binding_transport_requires_exact_packet_and_exact_one_right() {
        let (sender, receiver) = pair();
        let source = std::fs::File::open("/dev/null").unwrap();
        let message = packet(BIND, [7; 32], 13, 17, 19, 23);
        assert_eq!(
            unsafe { send_packet(sender.as_raw_fd(), &message, Some(source.as_raw_fd())) },
            PACKET_BYTES as isize
        );
        let mut actual = [0; PACKET_BYTES];
        let accepted = unsafe { receive_packet(receiver.as_raw_fd(), &mut actual, 1) }.unwrap();
        let accepted = unsafe { OwnedFd::from_raw_fd(accepted) };
        assert_eq!(actual, message);
        assert_ne!(
            unsafe { libc::fcntl(accepted.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        drop(accepted);
        assert_eq!(
            unsafe { send_packet(sender.as_raw_fd(), &message, None) },
            PACKET_BYTES as isize
        );
        assert_eq!(
            unsafe { receive_packet(receiver.as_raw_fd(), &mut actual, 1) },
            Err(libc::EPROTO)
        );
        assert_eq!(
            unsafe { send_packet(sender.as_raw_fd(), &message, Some(source.as_raw_fd())) },
            PACKET_BYTES as isize
        );
        // ACKs may not smuggle an authority descriptor.
        assert_eq!(
            unsafe { receive_packet(receiver.as_raw_fd(), &mut actual, 0) },
            Err(libc::EPROTO)
        );
        assert_ne!(
            unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETFD) },
            -1
        );
    }

    #[test]
    fn binding_transport_refuses_short_and_truncated_records() {
        let (sender, receiver) = pair();
        let mut actual = [0; PACKET_BYTES];
        for length in [1, PACKET_BYTES - 1, PACKET_BYTES + 1] {
            let bytes = [0u8; PACKET_BYTES + 1];
            assert_eq!(
                unsafe {
                    libc::send(
                        sender.as_raw_fd(),
                        bytes.as_ptr().cast(),
                        length,
                        libc::MSG_NOSIGNAL,
                    )
                },
                length as isize
            );
            assert_eq!(
                unsafe { receive_packet(receiver.as_raw_fd(), &mut actual, 0) },
                Err(libc::EPROTO)
            );
        }
    }
    #[test]
    fn binding_transport_closes_surplus_and_truncated_rights() {
        let (sender, receiver) = pair();
        let source = tempfile::tempfile().unwrap();
        let identity = source.metadata().unwrap();
        let count_copies = || {
            std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| std::fs::metadata(entry.path()).ok())
                .filter(|value| value.dev() == identity.dev() && value.ino() == identity.ino())
                .count()
        };
        let original_copies = count_copies();
        let message = packet(BIND, [7; 32], 13, 17, 19, 23);
        for count in [2usize, 32] {
            let mut control = [0usize; 128];
            let mut iov = libc::iovec {
                iov_base: message.as_ptr().cast_mut().cast(),
                iov_len: message.len(),
            };
            let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
            header.msg_iov = &mut iov;
            header.msg_iovlen = 1;
            header.msg_control = control.as_mut_ptr().cast();
            header.msg_controllen =
                unsafe { libc::CMSG_SPACE((count * std::mem::size_of::<i32>()) as u32) } as usize;
            let cmsg = unsafe { libc::CMSG_FIRSTHDR(&header) };
            unsafe {
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len =
                    libc::CMSG_LEN((count * std::mem::size_of::<i32>()) as u32) as usize;
                for index in 0..count {
                    libc::CMSG_DATA(cmsg)
                        .cast::<i32>()
                        .add(index)
                        .write_unaligned(source.as_raw_fd());
                }
            }
            assert_eq!(
                unsafe { libc::sendmsg(sender.as_raw_fd(), &header, libc::MSG_NOSIGNAL) },
                PACKET_BYTES as isize
            );
            let mut actual = [0; PACKET_BYTES];
            assert_eq!(
                unsafe { receive_packet(receiver.as_raw_fd(), &mut actual, 1) },
                Err(libc::EPROTO)
            );
            assert_eq!(
                count_copies(),
                original_copies,
                "surplus/truncated rights leaked original inode copies"
            );
        }
    }
}
