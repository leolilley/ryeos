//! One-shot, bounded executable startup I/O. No environment, child lifecycle,
//! request schema, or interpretation of arguments belongs to this capability.

use crate::time::MonotonicDeadline;
use std::{ffi::OsString, io};

#[derive(Debug, Clone, Copy)]
pub struct InvocationBounds {
    /// Includes argv[0]. Byte bounds use native OS argument bytes on Linux.
    pub max_arguments: usize,
    pub max_argument_bytes: usize,
    pub max_total_argument_bytes: usize,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
}

/// Unique invocation owner; neither the descriptors nor renewable deadlines
/// escape it. Reading is attempted once; writing consumes the owner even when
/// a partial write fails, so retry cannot accidentally duplicate output.
pub struct StartupInvocation {
    arguments: Vec<OsString>,
    bounds: InvocationBounds,
    deadline: MonotonicDeadline,
    read_attempted: bool,
    #[cfg(target_os = "linux")]
    input: crate::exec::InheritedDescriptorAuthority,
    #[cfg(target_os = "linux")]
    output: crate::exec::InheritedDescriptorAuthority,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn check_deadline(deadline: MonotonicDeadline) -> io::Result<()> {
    if deadline.has_elapsed() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "invocation deadline elapsed",
        ))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn bounded_arguments(
    arguments: impl IntoIterator<Item = OsString>,
    bounds: InvocationBounds,
    deadline: MonotonicDeadline,
) -> io::Result<Vec<OsString>> {
    use std::os::unix::ffi::OsStrExt;
    let mut result = Vec::new();
    let mut total = 0usize;
    for argument in arguments {
        check_deadline(deadline)?;
        let size = argument.as_os_str().as_bytes().len();
        total = total
            .checked_add(size)
            .ok_or_else(|| invalid("argument byte overflow"))?;
        if result.len() >= bounds.max_arguments
            || size > bounds.max_argument_bytes
            || total > bounds.max_total_argument_bytes
        {
            return Err(invalid("invocation arguments exceed their bounds"));
        }
        result.push(argument);
    }
    check_deadline(deadline)?;
    Ok(result)
}

impl StartupInvocation {
    /// Adopt explicitly selected inherited pipe coordinates (normally 0 and 1)
    /// and capture argv. Linux only; other platforms refuse without adoption.
    ///
    /// Invalid/equal/out-of-range coordinates and an expired deadline are
    /// rejected before ownership transfer. Otherwise each open coordinate is
    /// consumed, including on later validation failure; absent coordinates are
    /// not consumed. Successful adoption relocates stdio above 2 and registers
    /// both descriptors in Lillux's fork-child-close inventory.
    ///
    /// # Safety
    /// Call during exclusive, single-threaded executable startup, before using
    /// Rust stdio or starting host tasks. Each open coordinate must be uniquely
    /// owned, with no other Rust owner, borrowed handle, or inherited duplicate
    /// of its open-file description (in this process or another). The parent
    /// may retain the opposite pipe endpoints, not aliases of these endpoints.
    /// This contract permits setting O_NONBLOCK without racing an alias's I/O.
    /// Numeric equality and same-pipe checks do NOT prove arbitrary alias
    /// absence. Do not use this function for ambient descriptor discovery.
    pub unsafe fn take_inherited_pipes(
        input_fd: u32,
        output_fd: u32,
        bounds: InvocationBounds,
        deadline: MonotonicDeadline,
    ) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let (input, output) = unsafe { linux::acquire(input_fd, output_fd, deadline) }?;
            let arguments = bounded_arguments(std::env::args_os(), bounds, deadline)?;
            Ok(Self {
                arguments,
                bounds,
                deadline,
                read_attempted: false,
                input,
                output,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (input_fd, output_fd, bounds, deadline);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "startup pipe invocation requires Linux",
            ))
        }
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    /// EOF is required even at the exact bound. One extra byte is overflow;
    /// filling the bound without EOF remains subject to the original deadline.
    pub fn read_input(&mut self) -> io::Result<Vec<u8>> {
        if self.read_attempted {
            return Err(invalid("invocation input was already attempted"));
        }
        self.read_attempted = true;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let fd = self.input.file().as_raw_fd();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                check_deadline(self.deadline)?;
                // Always probe one byte past the limit, without bound+1 overflow.
                let capacity =
                    (self.bounds.max_input_bytes - bytes.len()).min(buffer.len() - 1) + 1;
                let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), capacity) };
                if count >= 0 {
                    check_deadline(self.deadline)?;
                    let count = count as usize;
                    if count > self.bounds.max_input_bytes - bytes.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invocation input exceeds its bound",
                        ));
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if count == 0 {
                        return Ok(bytes);
                    }
                } else {
                    linux::retry(fd, libc::POLLIN, self.deadline)?;
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "startup pipe invocation requires Linux",
        ))
    }

    /// Write at most the declared bound under the same deadline as input.
    /// Success means all bytes reached the pipe, not that a consumer applied
    /// them. Pipes have no userspace flush buffer. Failure may follow a prefix.
    pub fn write_output(self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > self.bounds.max_output_bytes {
            return Err(invalid("invocation output exceeds its bound"));
        }
        check_deadline(self.deadline)?;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let fd = self.output.file().as_raw_fd();
            let signal = linux::BlockedSigpipe::new()?;
            let mut position = 0;
            while position < bytes.len() {
                check_deadline(self.deadline)?;
                let count = unsafe {
                    libc::write(
                        fd,
                        bytes[position..].as_ptr().cast(),
                        (bytes.len() - position).min(8192),
                    )
                };
                if count > 0 {
                    position += count as usize;
                } else if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "invocation pipe made no progress",
                    ));
                } else {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EPIPE) {
                        signal.consume_new_sigpipe();
                        return Err(error);
                    }
                    linux::retry_error(fd, libc::POLLOUT, self.deadline, error)?;
                }
            }
            check_deadline(self.deadline)
        }
        #[cfg(not(target_os = "linux"))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "startup pipe invocation requires Linux",
        ))
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod linux {
    use super::*;
    use crate::exec::{InheritedDescriptorAuthority, retain_fork_sensitive_descriptors_until};
    use std::{
        fs::File,
        os::fd::{AsRawFd, FromRawFd},
    };

    pub(crate) unsafe fn acquire(
        input: u32,
        output: u32,
        deadline: MonotonicDeadline,
    ) -> io::Result<(InheritedDescriptorAuthority, InheritedDescriptorAuthority)> {
        if input == output || input > i32::MAX as u32 || output > i32::MAX as u32 {
            return Err(invalid(
                "invocation pipe coordinates overlap or exceed fd range",
            ));
        }
        check_deadline(deadline)?;
        let lease = retain_fork_sensitive_descriptors_until(deadline)?;
        // Adopt BOTH before validating either, so partial validation cannot leak
        // the other valid endpoint. Never construct File around an absent fd.
        let input = unsafe { adopt_pipe_endpoint(input) };
        let output = unsafe { adopt_pipe_endpoint(output) };
        let input = input?;
        let output = output?;
        let input_identity = inspect_pipe_endpoint(&input, libc::O_RDONLY)?;
        let output_identity = inspect_pipe_endpoint(&output, libc::O_WRONLY)?;
        if input_identity == output_identity {
            return Err(invalid("invocation pipes alias the same pipe"));
        }
        for file in [&input, &output] {
            make_pipe_nonblocking(file)?;
        }
        let input = InheritedDescriptorAuthority::from_owned_file(input, &lease)
            .map_err(io::Error::other)?;
        let output = InheritedDescriptorAuthority::from_owned_file(output, &lease)
            .map_err(io::Error::other)?;
        check_deadline(deadline)?;
        Ok((input, output))
    }

    /// Same exclusive startup ownership as acquire, but no input is selected.
    pub(crate) unsafe fn acquire_output(
        output: u32,
        deadline: MonotonicDeadline,
    ) -> io::Result<InheritedDescriptorAuthority> {
        if output > i32::MAX as u32 {
            return Err(invalid("invocation output exceeds fd range"));
        }
        check_deadline(deadline)?;
        let lease = retain_fork_sensitive_descriptors_until(deadline)?;
        let output = unsafe { adopt_pipe_endpoint(output) }?;
        inspect_pipe_endpoint(&output, libc::O_WRONLY)?;
        make_pipe_nonblocking(&output)?;
        let output = InheritedDescriptorAuthority::from_owned_file(output, &lease)
            .map_err(io::Error::other)?;
        check_deadline(deadline)?;
        Ok(output)
    }

    unsafe fn adopt_pipe_endpoint(fd: u32) -> io::Result<File> {
        if unsafe { libc::fcntl(fd as i32, libc::F_GETFD) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }

    fn inspect_pipe_endpoint(file: &File, access: i32) -> io::Result<(libc::dev_t, libc::ino_t)> {
        let fd = file.as_raw_fd();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFIFO || flags & libc::O_ACCMODE != access {
            return Err(invalid(
                "invocation requires a read pipe and a distinct write pipe",
            ));
        }
        Ok((stat.st_dev, stat.st_ino))
    }

    fn make_pipe_nonblocking(file: &File) -> io::Result<()> {
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn retry(fd: i32, events: i16, deadline: MonotonicDeadline) -> io::Result<()> {
        retry_error(fd, events, deadline, io::Error::last_os_error())
    }
    pub(super) fn retry_error(
        fd: i32,
        events: i16,
        deadline: MonotonicDeadline,
        error: io::Error,
    ) -> io::Result<()> {
        match error.kind() {
            io::ErrorKind::Interrupted => check_deadline(deadline),
            io::ErrorKind::WouldBlock => {
                crate::exec::duplex_deadline::wait_ready(fd, events, deadline)
            }
            _ => Err(error),
        }
    }

    // A pipe write must report BrokenPipe even if the executable inherited the
    // default SIGPIPE disposition. Do not change the process-wide disposition.
    pub(crate) struct BlockedSigpipe {
        previous: libc::sigset_t,
        set: libc::sigset_t,
        already_pending: bool,
    }
    impl BlockedSigpipe {
        pub(crate) fn new() -> io::Result<Self> {
            unsafe {
                let mut set = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGPIPE);
                let mut previous = std::mem::zeroed();
                let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut previous);
                if error != 0 {
                    return Err(io::Error::from_raw_os_error(error));
                }
                let mut owner = Self {
                    previous,
                    set,
                    already_pending: true,
                };
                let mut pending = std::mem::zeroed();
                if libc::sigpending(&mut pending) < 0 {
                    return Err(io::Error::last_os_error());
                }
                owner.already_pending = libc::sigismember(&pending, libc::SIGPIPE) == 1;
                Ok(owner)
            }
        }
        pub(crate) fn consume_new_sigpipe(&self) {
            if !self.already_pending {
                let zero = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                loop {
                    let result =
                        unsafe { libc::sigtimedwait(&self.set, std::ptr::null_mut(), &zero) };
                    if result >= 0
                        || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
            }
        }
    }
    impl Drop for BlockedSigpipe {
        fn drop(&mut self) {
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::*;
    #[cfg(target_os = "linux")]
    use std::{
        fs::File,
        io::{Read, Write},
        os::fd::{FromRawFd, IntoRawFd},
        time::Duration,
    };

    #[cfg(target_os = "linux")]
    fn bounds() -> InvocationBounds {
        InvocationBounds {
            max_arguments: 2,
            max_argument_bytes: 4,
            max_total_argument_bytes: 6,
            max_input_bytes: 4,
            max_output_bytes: 4,
        }
    }

    #[cfg(target_os = "linux")]
    fn pipe() -> (File, File) {
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pair.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        unsafe { (File::from_raw_fd(pair[0]), File::from_raw_fd(pair[1])) }
    }

    // Private acquisition seam supplies exclusive fixture endpoints; it does
    // not touch process stdio/argv or claim that a parallel test harness is
    // single-threaded executable startup.
    #[cfg(target_os = "linux")]
    fn invocation(
        bounds: InvocationBounds,
        deadline: MonotonicDeadline,
    ) -> (StartupInvocation, File, File) {
        let (input, writer) = pipe();
        let (reader, output) = pipe();
        let (input, output) = unsafe {
            linux::acquire(
                input.into_raw_fd() as u32,
                output.into_raw_fd() as u32,
                deadline,
            )
        }
        .unwrap();
        (
            StartupInvocation {
                arguments: vec![],
                bounds,
                deadline,
                read_attempted: false,
                input,
                output,
            },
            writer,
            reader,
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exact_argument_bounds_and_each_overflow() {
        let deadline = MonotonicDeadline::after(Duration::from_secs(2));
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            bounded_arguments(args(&["abcd", "ef"]), bounds(), deadline)
                .unwrap()
                .len(),
            2
        );
        for bad in [
            args(&["abcde"]),
            args(&["abcd", "efg"]),
            args(&["a", "b", "c"]),
        ] {
            assert_eq!(
                bounded_arguments(bad, bounds(), deadline)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn input_exact_eof_empty_and_plus_one_are_distinct_and_one_shot() {
        for bytes in [&b""[..], &b"abcd"[..], &b"abcde"[..]] {
            let (mut owner, mut writer, _reader) =
                invocation(bounds(), MonotonicDeadline::after(Duration::from_secs(2)));
            writer.write_all(bytes).unwrap();
            drop(writer);
            let result = owner.read_input();
            if bytes.len() <= 4 {
                assert_eq!(result.unwrap(), bytes);
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
            }
            assert_eq!(
                owner.read_input().unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn output_exact_bound_and_plus_one() {
        for bytes in [&b"abcd"[..], &b"abcde"[..]] {
            let (owner, _writer, mut reader) =
                invocation(bounds(), MonotonicDeadline::after(Duration::from_secs(2)));
            let result = owner.write_output(bytes);
            let mut observed = Vec::new();
            reader.read_to_end(&mut observed).unwrap();
            if bytes.len() <= 4 {
                result.unwrap();
                assert_eq!(observed, bytes);
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
                assert!(observed.is_empty());
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn blocked_read_and_exact_without_eof_expire() {
        for bytes in [&b""[..], &b"abcd"[..]] {
            let (mut owner, mut writer, _reader) = invocation(
                bounds(),
                MonotonicDeadline::after(Duration::from_millis(80)),
            );
            writer.write_all(bytes).unwrap();
            assert_eq!(
                owner.read_input().unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
            assert_eq!(
                owner.write_output(b"x").unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn output_partial_progress_then_blocked_does_not_renew_deadline() {
        let mut limits = bounds();
        limits.max_output_bytes = 1024 * 1024;
        let deadline = MonotonicDeadline::after(Duration::from_millis(100));
        let (owner, _writer, mut reader) = invocation(limits, deadline);
        assert_eq!(
            owner
                .write_output(&vec![7; limits.max_output_bytes])
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(deadline.has_elapsed());
        let mut observed = Vec::new();
        reader.read_to_end(&mut observed).unwrap();
        assert!(!observed.is_empty());
        assert!(observed.len() < limits.max_output_bytes);
        assert!(observed.iter().all(|byte| *byte == 7));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broken_pipe_is_an_error_not_a_successful_delivery() {
        let (owner, _writer, reader) =
            invocation(bounds(), MonotonicDeadline::after(Duration::from_secs(2)));
        drop(reader);
        assert_eq!(
            owner.write_output(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn same_coordinate_refuses_without_consuming_and_same_pipe_refuses() {
        use std::os::fd::AsRawFd;
        let (input, output) = pipe();
        let deadline = MonotonicDeadline::after(Duration::from_secs(2));
        let raw = input.as_raw_fd() as u32;
        assert!(unsafe { linux::acquire(raw, raw, deadline) }.is_err());
        assert!(unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFD) } >= 0);
        assert!(
            unsafe {
                linux::acquire(
                    input.into_raw_fd() as u32,
                    output.into_raw_fd() as u32,
                    deadline,
                )
            }
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn absent_or_wrong_input_cleans_up_the_other_acquired_endpoint() {
        for absent in [true, false] {
            let deadline = MonotonicDeadline::after(Duration::from_secs(2));
            let (mut reader, output) = pipe();
            // i32::MAX cannot be an open descriptor under the process limit.
            let input = if absent {
                i32::MAX as u32
            } else {
                File::open("/dev/null").unwrap().into_raw_fd() as u32
            };
            assert!(
                unsafe { linux::acquire(input, output.into_raw_fd() as u32, deadline) }.is_err()
            );
            let mut byte = [0];
            assert_eq!(
                reader.read(&mut byte).unwrap(),
                0,
                "failed acquisition must close its valid output"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrong_access_mode_refuses_and_closes_output() {
        let (_input_reader, wrong_input_writer) = pipe();
        let (mut reader, output) = pipe();
        assert!(
            unsafe {
                linux::acquire(
                    wrong_input_writer.into_raw_fd() as u32,
                    output.into_raw_fd() as u32,
                    MonotonicDeadline::after(Duration::from_secs(2)),
                )
            }
            .is_err()
        );
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }
}
