//! Unique interactive inherited pipes. Interruption observes only local I/O;
//! it is not EOF, peer application, or process settlement.
use crate::time::MonotonicDeadline;
use std::{io, sync::Arc};

pub struct InheritedPipePair {
    input: InheritedPipeInput,
    output: InheritedPipeOutput,
    interrupt: PipeInterrupt,
}
pub struct InheritedPipeInput {
    #[cfg(target_os = "linux")]
    pipe: crate::exec::InheritedDescriptorAuthority,
    maximum: usize,
    poisoned: bool,
    interrupt: PipeInterrupt,
}
pub struct InheritedPipeOutput {
    #[cfg(target_os = "linux")]
    pipe: crate::exec::InheritedDescriptorAuthority,
    maximum: usize,
    poisoned: bool,
    interrupt: PipeInterrupt,
}
#[derive(Clone)]
pub struct PipeInterrupt(Arc<InterruptState>);
struct InterruptState {
    interrupted: std::sync::atomic::AtomicBool,
    #[cfg(target_os = "linux")]
    wake: crate::exec::InheritedDescriptorAuthority,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
#[cfg(not(target_os = "linux"))]
fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "inherited pipes require Linux")
}

impl InheritedPipePair {
    /// Adopt two explicitly selected unique pipe endpoints. Invalid bounds or
    /// coordinates refuse before adoption; later failures consume open endpoints.
    ///
    /// # Safety
    /// Exactly the exclusive single-threaded startup and no-alias requirements
    /// of [`crate::invocation::StartupInvocation::take_inherited_pipes`] apply.
    pub unsafe fn take_inherited_pipes(
        input_fd: u32,
        output_fd: u32,
        max_chunk_bytes: usize,
        startup_deadline: MonotonicDeadline,
    ) -> io::Result<Self> {
        if max_chunk_bytes == 0 || max_chunk_bytes > isize::MAX as usize {
            return Err(invalid("invalid inherited pipe chunk bound"));
        }
        #[cfg(target_os = "linux")]
        {
            let (input, output) = unsafe {
                crate::invocation::linux::acquire(input_fd, output_fd, startup_deadline)
            }?;
            Self::from_acquired(input, output, max_chunk_bytes, startup_deadline)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (input_fd, output_fd, startup_deadline);
            Err(unsupported())
        }
    }
    #[cfg(target_os = "linux")]
    fn from_acquired(
        input: crate::exec::InheritedDescriptorAuthority,
        output: crate::exec::InheritedDescriptorAuthority,
        maximum: usize,
        deadline: MonotonicDeadline,
    ) -> io::Result<Self> {
        let interrupt = PipeInterrupt::new(deadline)?;
        Ok(Self {
            input: InheritedPipeInput {
                pipe: input,
                maximum,
                poisoned: false,
                interrupt: interrupt.clone(),
            },
            output: InheritedPipeOutput {
                pipe: output,
                maximum,
                poisoned: false,
                interrupt: interrupt.clone(),
            },
            interrupt,
        })
    }
    pub fn split(self) -> (InheritedPipeInput, InheritedPipeOutput, PipeInterrupt) {
        (self.input, self.output, self.interrupt)
    }
}

impl PipeInterrupt {
    #[cfg(target_os = "linux")]
    fn new(deadline: MonotonicDeadline) -> io::Result<Self> {
        use std::os::fd::FromRawFd;
        let lease = crate::exec::retain_fork_sensitive_descriptors_until(deadline)?;
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let wake = crate::exec::InheritedDescriptorAuthority::from_owned_file(
            unsafe { std::fs::File::from_raw_fd(fd) },
            &lease,
        )
        .map_err(io::Error::other)?;
        let interrupt = PipeInterrupt(Arc::new(InterruptState {
            interrupted: false.into(),
            wake,
        }));
        interrupt.check(Some(deadline))?;
        Ok(interrupt)
    }
    /// Sticky broadcast: neither reader nor writer drains the wake descriptor.
    pub fn interrupt(&self) -> io::Result<()> {
        use std::sync::atomic::Ordering;
        self.0.interrupted.store(true, Ordering::Release);
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let value = 1u64;
            loop {
                let result = unsafe {
                    libc::write(
                        self.0.wake.file().as_raw_fd(),
                        (&value as *const u64).cast(),
                        8,
                    )
                };
                if result == 8 {
                    return Ok(());
                }
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => return Ok(()),
                    _ => return Err(error),
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        Err(unsupported())
    }
    pub fn is_interrupted(&self) -> bool {
        self.0
            .interrupted
            .load(std::sync::atomic::Ordering::Acquire)
    }
    fn check(&self, deadline: Option<MonotonicDeadline>) -> io::Result<()> {
        if self.is_interrupted() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "inherited pipe interrupted",
            ));
        }
        if deadline.is_some_and(|d| d.has_elapsed()) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "inherited pipe deadline elapsed",
            ));
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    fn wait(&self, fd: i32, events: i16, deadline: Option<MonotonicDeadline>) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        loop {
            self.check(deadline)?;
            let mut fds = [
                libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.0.wake.file().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let timeout = deadline
                .map(|d| {
                    d.remaining()
                        .as_millis()
                        .saturating_add(1)
                        .min(i32::MAX as u128) as i32
                })
                .unwrap_or(-1);
            let result = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
            self.check(deadline)?;
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if result == 0 {
                continue;
            }
            if fds.iter().any(|p| p.revents & libc::POLLNVAL != 0) {
                return Err(io::Error::other("inherited pipe descriptor invalid"));
            }
            if fds[0].revents & (events | libc::POLLERR | libc::POLLHUP) != 0 {
                return Ok(());
            }
        }
    }
}

impl InheritedPipeInput {
    // Separate completion seam permits deterministic tests of cancellation or
    // expiry racing a successful kernel read without public testing controls.
    fn finish_read(
        &mut self,
        count: usize,
        deadline: Option<MonotonicDeadline>,
    ) -> io::Result<usize> {
        if let Err(error) = self.interrupt.check(deadline) {
            if count > 0 {
                self.poisoned = true;
                // These bytes were consumed but cannot be reported as a
                // successful operation. Distinguish this lost prefix from
                // cancellation/expiry before consumption on the first error,
                // not merely on the caller's next attempted read.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "inherited input consumed an unreported prefix",
                ));
            }
            return Err(error);
        }
        Ok(count)
    }
    /// One nonblocking read, without readiness waiting. WouldBlock means no
    /// bytes are available; only an actual zero-byte read reports EOF. Bounds,
    /// cancellation and deadlines are enforced even for an immediately ready
    /// pipe. A consumed-but-unreported prefix returns InvalidData and poisons
    /// this half; it cannot be mistaken for idle cancellation or expiry.
    pub fn try_read_chunk(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<MonotonicDeadline>,
    ) -> io::Result<usize> {
        if self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "inherited input poisoned after consumed prefix",
            ));
        }
        if buffer.is_empty() || buffer.len() > self.maximum {
            return Err(invalid("inherited input chunk exceeds bound or is empty"));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            self.interrupt.check(deadline)?;
            let result = unsafe {
                libc::read(
                    self.pipe.file().as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if result >= 0 {
                self.finish_read(result as usize, deadline)
            } else {
                Err(io::Error::last_os_error())
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = deadline;
            Err(unsupported())
        }
    }

    /// Reads at most one bounded chunk, waiting for readiness when necessary.
    /// None permits idle waiting, still interruptible. Errors never establish
    /// a fresh framing boundary. Uses the same one-attempt read as finite
    /// single-threaded relays; no second descriptor or process owner is made.
    pub fn read_chunk(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<MonotonicDeadline>,
    ) -> io::Result<usize> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            loop {
                match self.try_read_chunk(buffer, deadline) {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.interrupt
                            .wait(self.pipe.file().as_raw_fd(), libc::POLLIN, deadline)?
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        // A syscall interruption can retry, sticky caller
                        // cancellation cannot. Preserve the same deadline.
                        self.interrupt.check(deadline)?;
                    }
                    result => return result,
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.try_read_chunk(buffer, deadline)
        }
    }
}
impl InheritedPipeOutput {
    /// Adopt one uniquely owned write pipe at exclusive executable startup.
    /// No stdin requirement is introduced. Invalid bounds/coordinates refuse
    /// before adoption; later failures consume the selected endpoint.
    ///
    /// # Safety
    /// The process is single-threaded, has not used stdio, and no Rust object
    /// or other actor aliases this endpoint. The selected FD must be owned by
    /// this invocation; callers must never reconstruct it after failure.
    pub unsafe fn take_inherited_output(
        output_fd: u32,
        max_chunk_bytes: usize,
        startup_deadline: MonotonicDeadline,
    ) -> io::Result<Self> {
        if max_chunk_bytes == 0 || max_chunk_bytes > isize::MAX as usize {
            return Err(invalid("invalid inherited output chunk bound"));
        }
        #[cfg(target_os = "linux")]
        {
            let pipe =
                unsafe { crate::invocation::linux::acquire_output(output_fd, startup_deadline) }?;
            let interrupt = PipeInterrupt::new(startup_deadline)?;
            Ok(Self {
                pipe,
                maximum: max_chunk_bytes,
                poisoned: false,
                interrupt,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (output_fd, startup_deadline);
            Err(unsupported())
        }
    }

    #[cfg(target_os = "linux")]
    fn finish_written_prefix(
        &mut self,
        count: usize,
        deadline: MonotonicDeadline,
    ) -> io::Result<usize> {
        // A prefix has already reached the pipe. Interruption is terminal and
        // must preserve poisoning, never masquerade as zero-progress EINTR.
        self.interrupt.check(Some(deadline)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("inherited output prefix delivered before terminal interruption: {error}"),
            )
        })?;
        self.poisoned = false;
        Ok(count)
    }

    /// Attempts one bounded nonblocking write. Success reports the exact prefix
    /// delivered, not an application acknowledgement. Only known-zero transient
    /// failures permit retry on this same owner; uncertain failures poison it.
    pub fn try_write_chunk(
        &mut self,
        bytes: &[u8],
        deadline: MonotonicDeadline,
    ) -> io::Result<usize> {
        if self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "inherited output poisoned",
            ));
        }
        if bytes.len() > self.maximum {
            return Err(invalid("inherited output chunk exceeds bound"));
        }
        self.interrupt.check(Some(deadline)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("inherited output operation cancelled or expired: {error}"),
            )
        })?;
        if bytes.is_empty() {
            return Ok(0);
        }
        self.poisoned = true;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let signal = crate::invocation::linux::BlockedSigpipe::new()?;
            let result = unsafe {
                libc::write(
                    self.pipe.file().as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len().min(8192),
                )
            };
            if result > 0 {
                // Delivery happened. Cancellation now must not look like a
                // zero-byte EINTR that the enclosing relay may retry or ignore
                // while accepting completion. Keep this owner poisoned.
                return self.finish_written_prefix(result as usize, deadline);
            }
            if result == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "inherited output made no progress",
                ));
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EPIPE) {
                signal.consume_new_sigpipe();
            }
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                self.interrupt.check(Some(deadline)).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("inherited output retry forbidden after interruption: {error}"),
                    )
                })?;
                self.poisoned = false;
            }
            Err(error)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(unsupported())
        }
    }

    /// Failure after entering an operation permanently poisons this half.
    /// Bytes may already have reached the peer; success is not application ACK.
    pub fn write_all(&mut self, bytes: &[u8], deadline: MonotonicDeadline) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "inherited output poisoned",
            ));
        }
        if bytes.len() > self.maximum {
            return Err(invalid("inherited output chunk exceeds bound"));
        }
        self.poisoned = true;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let signal = crate::invocation::linux::BlockedSigpipe::new()?;
            let mut position = 0;
            while position < bytes.len() {
                self.interrupt.check(Some(deadline))?;
                let result = unsafe {
                    libc::write(
                        self.pipe.file().as_raw_fd(),
                        bytes[position..].as_ptr().cast(),
                        (bytes.len() - position).min(8192),
                    )
                };
                if result > 0 {
                    position += result as usize;
                    continue;
                }
                if result == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "inherited output made no progress",
                    ));
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EPIPE) {
                    signal.consume_new_sigpipe();
                    return Err(error);
                }
                match error.kind() {
                    io::ErrorKind::Interrupted => (),
                    io::ErrorKind::WouldBlock => self.interrupt.wait(
                        self.pipe.file().as_raw_fd(),
                        libc::POLLOUT,
                        Some(deadline),
                    )?,
                    _ => return Err(error),
                }
            }
            self.interrupt.check(Some(deadline))?;
            self.poisoned = false;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = deadline;
            Err(unsupported())
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        fs::File,
        io::{Read, Write},
        os::fd::{FromRawFd, IntoRawFd},
        time::Duration,
    };
    fn deadline() -> MonotonicDeadline {
        MonotonicDeadline::after(Duration::from_secs(3))
    }
    fn pipe() -> (File, File) {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }
    fn pair(maximum: usize) -> (InheritedPipePair, File, File) {
        let (input, parent_writer) = pipe();
        let (parent_reader, output) = pipe();
        // Private exclusive pipe acquisition, not the executable-startup API:
        // no assertion that parallel libtest owns process stdio or argv.
        let (input, output) = unsafe {
            crate::invocation::linux::acquire(
                input.into_raw_fd() as u32,
                output.into_raw_fd() as u32,
                deadline(),
            )
        }
        .unwrap();
        (
            InheritedPipePair::from_acquired(input, output, maximum, deadline()).unwrap(),
            parent_writer,
            parent_reader,
        )
    }
    #[test]
    fn output_only_owner_writes_without_selecting_an_input() {
        let (mut reader, writer) = pipe();
        // Test-owned unique endpoint, not adoption of libtest's stdout.
        let pipe = unsafe {
            crate::invocation::linux::acquire_output(writer.into_raw_fd() as u32, deadline())
        }
        .unwrap();
        let mut output = InheritedPipeOutput {
            pipe,
            maximum: 4,
            poisoned: false,
            interrupt: PipeInterrupt::new(deadline()).unwrap(),
        };
        assert_eq!(
            output.write_all(b"extra", deadline()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        output.write_all(b"done", deadline()).unwrap();
        drop(output);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"done");
    }

    #[test]
    fn output_only_acquisition_refuses_read_endpoint_and_regular_file() {
        let (reader, _writer) = pipe();
        assert!(
            unsafe {
                crate::invocation::linux::acquire_output(reader.into_raw_fd() as u32, deadline())
            }
            .is_err()
        );
        let file = tempfile::tempfile().unwrap();
        assert!(
            unsafe {
                crate::invocation::linux::acquire_output(file.into_raw_fd() as u32, deadline())
            }
            .is_err()
        );
    }

    #[test]
    fn delivered_prefix_then_cancellation_is_terminal_and_poisoned() {
        let (pair, _writer, mut reader) = pair(4);
        let (_, mut output, interrupt) = pair.split();
        // Deterministically exercise the post-syscall boundary after actual
        // pipe delivery, without a scheduling-dependent cancellation race.
        output.poisoned = true;
        let mut writer = output.pipe.file();
        writer.write_all(b"sent").unwrap();
        interrupt.interrupt().unwrap();
        assert_eq!(
            output
                .finish_written_prefix(4, deadline())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(output.poisoned);
        let mut delivered = [0; 4];
        reader.read_exact(&mut delivered).unwrap();
        assert_eq!(&delivered, b"sent");
        assert_eq!(
            output
                .try_write_chunk(b"redo", deadline())
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn nonblocking_output_cancellation_is_not_retryable_syscall_interruption() {
        let (pair, _writer, _reader) = pair(4);
        let (_, mut output, interrupt) = pair.split();
        interrupt.interrupt().unwrap();
        let error = output.try_write_chunk(b"data", deadline()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ));
    }

    #[test]
    fn nonblocking_output_reports_prefix_and_recovers_after_backpressure() {
        let (pair, _writer, mut reader) = pair(8192);
        let (_, mut output, _) = pair.split();
        let bytes = [7u8; 8192];
        let mut delivered = 0usize;
        loop {
            match output.try_write_chunk(&bytes, deadline()) {
                Ok(n) => {
                    assert!(n > 0 && n <= bytes.len());
                    delivered += n;
                    assert!(delivered <= 16 * 1024 * 1024);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                other => panic!("unexpected write: {other:?}"),
            }
        }
        let mut drained = vec![0; delivered];
        reader.read_exact(&mut drained).unwrap();
        assert!(drained.iter().all(|byte| *byte == 7));
        assert_eq!(output.try_write_chunk(b"done", deadline()).unwrap(), 4);
        let mut tail = [0; 4];
        reader.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"done");
        drop(reader);
        assert_eq!(
            output
                .try_write_chunk(b"lost", deadline())
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            output
                .try_write_chunk(b"retry", deadline())
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn interactive_chunks_actual_eof_and_bounds() {
        let (pair, mut writer, mut reader) = pair(4);
        let (mut input, mut output, interrupt) = pair.split();
        writer.write_all(b"abcd").unwrap();
        let mut bytes = [0; 4];
        assert_eq!(input.read_chunk(&mut bytes, Some(deadline())).unwrap(), 4);
        assert_eq!(&bytes, b"abcd");
        assert!(input.read_chunk(&mut [], None).is_err());
        assert!(input.read_chunk(&mut [0; 5], None).is_err());
        assert!(output.write_all(b"12345", deadline()).is_err());
        output.write_all(b"xy", deadline()).unwrap();
        output.write_all(b"zw", deadline()).unwrap();
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"xyzw");
        drop(writer);
        assert_eq!(input.read_chunk(&mut bytes, None).unwrap(), 0);
        assert!(!interrupt.is_interrupted());
    }
    #[test]
    fn sticky_interrupt_wakes_both_halves_with_parent_endpoints_open() {
        let (pair, _writer, _reader) = pair(1024 * 1024);
        let (mut input, mut output, interrupt) = pair.split();
        let read = crate::task::spawn_host_task("pipe-test-read", move || {
            input.read_chunk(&mut [0; 8], None)
        })
        .unwrap();
        let write = crate::task::spawn_host_task("pipe-test-write", move || {
            let error = output
                .write_all(&vec![1; 1024 * 1024], deadline())
                .unwrap_err();
            assert_eq!(
                output.write_all(b"retry", deadline()).unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
            error.kind()
        })
        .unwrap();
        // Exercise outstanding waits; correctness also covers cancellation
        // before either task reaches poll, with no lost broadcast wake.
        crate::time::sleep(Duration::from_millis(20));
        assert!(!read.is_finished());
        assert!(!write.is_finished());
        interrupt.interrupt().unwrap();
        interrupt.clone().interrupt().unwrap();
        assert!(interrupt.is_interrupted());
        assert_eq!(
            read.join_until(deadline())
                .unwrap_or_else(|_| panic!("reader did not settle"))
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            write
                .join_until(deadline())
                .unwrap_or_else(|_| panic!("writer did not settle"))
                .unwrap(),
            io::ErrorKind::Interrupted
        );
    }
    #[test]
    fn nonblocking_read_distinguishes_idle_data_and_actual_eof() {
        let (pair, mut writer, _reader) = pair(8);
        let (mut input, _, _) = pair.split();
        let mut bytes = [0; 8];
        assert_eq!(
            input
                .try_read_chunk(&mut bytes, Some(deadline()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        writer.write_all(b"hello").unwrap();
        assert_eq!(
            input.try_read_chunk(&mut bytes, Some(deadline())).unwrap(),
            5
        );
        assert_eq!(&bytes[..5], b"hello");
        assert_eq!(
            input
                .try_read_chunk(&mut bytes, Some(deadline()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(writer);
        assert_eq!(
            input.try_read_chunk(&mut bytes, Some(deadline())).unwrap(),
            0
        );
    }

    #[test]
    fn nonblocking_read_enforces_bounds_and_expiry_before_consumption() {
        let (pair, mut writer, _reader) = pair(8);
        let (mut input, _, interrupt) = pair.split();
        writer.write_all(b"x").unwrap();
        assert_eq!(
            input.try_read_chunk(&mut [0; 9], None).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            input
                .try_read_chunk(&mut [0; 1], Some(MonotonicDeadline::after(Duration::ZERO)))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let mut bytes = [0; 1];
        assert_eq!(
            input.try_read_chunk(&mut bytes, Some(deadline())).unwrap(),
            1
        );
        assert_eq!(&bytes, b"x");
        interrupt.interrupt().unwrap();
        assert_eq!(
            input.try_read_chunk(&mut bytes, None).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn read_deadline_is_not_eof_and_does_not_poison_future_input() {
        let (pair, mut writer, _reader) = pair(8);
        let (mut input, _, _) = pair.split();
        assert_eq!(
            input
                .read_chunk(
                    &mut [0; 8],
                    Some(MonotonicDeadline::after(Duration::from_millis(20)))
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        writer.write_all(b"a").unwrap();
        assert_eq!(input.read_chunk(&mut [0; 8], Some(deadline())).unwrap(), 1);
    }

    #[test]
    fn consumed_prefix_then_expiry_or_interrupt_poison_input() {
        for cancel in [false, true] {
            let (pair, mut writer, _reader) = pair(8);
            let (mut input, _, interrupt) = pair.split();
            writer.write_all(b"remaining").unwrap();
            let expired = MonotonicDeadline::after(Duration::ZERO);
            if cancel {
                interrupt.interrupt().unwrap();
            }
            let error = input
                .finish_read(1, (!cancel).then_some(expired))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(
                input.read_chunk(&mut [0; 8], None).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
    #[test]
    fn partial_output_timeout_poisons_without_resend() {
        let (pair, _writer, mut reader) = pair(1024 * 1024);
        let (_, mut output, _) = pair.split();
        assert_eq!(
            output
                .write_all(
                    &vec![42; 1024 * 1024],
                    MonotonicDeadline::after(Duration::from_millis(20))
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            output.write_all(b"retry", deadline()).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(output);
        let mut observed = Vec::new();
        reader.read_to_end(&mut observed).unwrap();
        assert!(!observed.is_empty());
        assert!(observed.len() < 1024 * 1024);
        assert!(observed.iter().all(|b| *b == 42));
    }
    #[test]
    fn broken_pipe_is_terminal_and_preinterrupt_is_not_eof() {
        let (pair, _writer, reader) = pair(8);
        let (mut input, mut output, interrupt) = pair.split();
        drop(reader);
        assert_eq!(
            output.write_all(b"hello", deadline()).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            output.write_all(b"again", deadline()).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        interrupt.interrupt().unwrap();
        assert_eq!(
            input.read_chunk(&mut [0; 8], None).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
}
