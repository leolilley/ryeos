//! Deadline-bounded duplex I/O. The protocol owner supplies one absolute
//! deadline; partial frames and interrupted syscalls never restart that clock.

use crate::time::MonotonicDeadline;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

/// A borrowed stream view with an absolute deadline shared by every read/write.
/// No descriptor or platform readiness mechanics escape Lillux.
pub struct DeadlineDuplexStream<'a> {
    #[cfg(unix)]
    descriptor: BorrowedFd<'a>,
    #[cfg(not(unix))]
    lifetime: std::marker::PhantomData<&'a mut ()>,
    deadline: MonotonicDeadline,
}

impl<'a> DeadlineDuplexStream<'a> {
    #[cfg(unix)]
    pub(crate) fn new(descriptor: BorrowedFd<'a>, deadline: MonotonicDeadline) -> Self {
        Self {
            descriptor,
            deadline,
        }
    }
    #[cfg(not(unix))]
    pub(crate) fn unsupported(deadline: MonotonicDeadline) -> Self {
        Self {
            lifetime: std::marker::PhantomData,
            deadline,
        }
    }
}

#[cfg(unix)]
pub(super) fn wait_ready(fd: RawFd, events: i16, deadline: MonotonicDeadline) -> io::Result<()> {
    loop {
        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "duplex I/O deadline elapsed",
            ));
        }
        let timeout_ms = remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
            .min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: poll receives exactly one valid pollfd.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "duplex I/O endpoint is not live",
            ));
        }
        if ready != 0 {
            if deadline.has_elapsed() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "duplex I/O deadline elapsed",
                ));
            }
            return Ok(());
        }
    }
}

impl Read for DeadlineDuplexStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            if buffer.is_empty() {
                return Ok(0);
            }
            loop {
                wait_ready(self.descriptor.as_raw_fd(), libc::POLLIN, self.deadline)?;
                // SAFETY: the borrowed descriptor remains live and buffer is writable.
                let count = unsafe {
                    libc::recv(
                        self.descriptor.as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
                if count >= 0 {
                    return Ok(count as usize);
                }
                let error = io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Err(error);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (buffer, self.deadline);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "deadline duplex I/O is unavailable",
            ))
        }
    }
}

impl Write for DeadlineDuplexStream<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            if buffer.is_empty() {
                return Ok(0);
            }
            loop {
                wait_ready(self.descriptor.as_raw_fd(), libc::POLLOUT, self.deadline)?;
                // SAFETY: the borrowed descriptor and immutable buffer remain live.
                let count = unsafe {
                    libc::send(
                        self.descriptor.as_raw_fd(),
                        buffer.as_ptr().cast(),
                        buffer.len(),
                        libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                    )
                };
                if count >= 0 {
                    return Ok(count as usize);
                }
                let error = io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Err(error);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (buffer, self.deadline);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "deadline duplex I/O is unavailable",
            ))
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::exec::inherited_duplex_channel_pair;
    use crate::time::Duration;

    #[test]
    fn duplex_deadline_partial_read_does_not_restart_the_clock() {
        let (mut owner, peer) = inherited_duplex_channel_pair().unwrap();
        let mut peer_file = peer.channel.file();
        peer_file.write_all(b"x").unwrap();
        let deadline = MonotonicDeadline::after(Duration::from_millis(20));
        let mut bounded = owner.with_deadline(deadline);
        let mut bytes = [0; 2];
        assert_eq!(bounded.read(&mut bytes).unwrap(), 1);
        assert_eq!(
            bounded.read_exact(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        peer_file.write_all(b"y").unwrap();
        assert_eq!(
            bounded.read(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn duplex_deadline_write_backpressure_and_shutdown_are_bounded() {
        let (mut owner, _peer) = inherited_duplex_channel_pair().unwrap();
        let mut bounded = owner.with_deadline(MonotonicDeadline::after(Duration::from_millis(20)));
        let bytes = vec![0; 4 * 1024 * 1024];
        assert_eq!(
            bounded.write_all(&bytes).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        let interrupt = owner.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            owner
                .with_deadline(MonotonicDeadline::after(Duration::from_secs(2)))
                .read(&mut [0])
        });
        interrupt.shutdown().unwrap();
        assert_eq!(reader.join().unwrap().unwrap(), 0);
    }
}
