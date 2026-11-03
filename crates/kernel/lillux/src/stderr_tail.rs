//! A bounded tail of what a child process writes to its standard error.
//!
//! The parent keeps the read end and hands the write end to the child as its
//! stderr. Draining is explicit and non-blocking, so a single-threaded owner
//! stays single-threaded: it drains between its own polls. Every drained
//! byte is also forwarded to this process's own stderr so local diagnosis
//! keeps seeing what the child wrote.

use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::os::fd::FromRawFd as _;

use crate::time::{Duration, MonotonicDeadline};

/// Read end of a child's stderr pipe, retaining only the most recent bytes.
pub struct StderrTail {
    reader: std::fs::File,
    tail: VecDeque<u8>,
    max_bytes: usize,
    closed: bool,
}

impl StderrTail {
    /// Create the pipe. The returned write end must be given to the child and
    /// then dropped here, so the read end reaches end of file when the child
    /// and everything that inherited its stderr are gone.
    pub fn open(max_bytes: usize) -> Result<(Self, std::fs::File), String> {
        if max_bytes == 0 {
            return Err("stderr tail bound must be positive".into());
        }
        let mut descriptors = [-1; 2];
        // SAFETY: pipe2 writes two descriptors into the provided array.
        if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0
        {
            return Err(format!(
                "create stderr tail pipe: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: pipe2 returned two distinct newly owned descriptors.
        let (reader, writer) = unsafe {
            (
                std::fs::File::from_raw_fd(descriptors[0]),
                std::fs::File::from_raw_fd(descriptors[1]),
            )
        };
        // The child must see a blocking stderr.
        // SAFETY: fcntl on a descriptor this function owns.
        let flags = unsafe { libc::fcntl(descriptors[1], libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(descriptors[1], libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
        {
            return Err(format!(
                "make stderr tail writer blocking: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok((
            Self {
                reader,
                tail: VecDeque::new(),
                max_bytes,
                closed: false,
            },
            writer,
        ))
    }

    /// Take whatever is readable now. Returns true once every writer closed.
    pub fn drain(&mut self) -> bool {
        let mut buffer = [0_u8; 4096];
        while !self.closed {
            match self.reader.read(&mut buffer) {
                Ok(0) => self.closed = true,
                Ok(count) => {
                    let _ = std::io::stderr().write_all(&buffer[..count]);
                    self.tail.extend(&buffer[..count]);
                    let excess = self.tail.len().saturating_sub(self.max_bytes);
                    self.tail.drain(..excess);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => self.closed = true,
            }
        }
        self.closed
    }

    /// Drain until every writer closed or `grace` elapsed, then return the
    /// retained tail. A writer that outlives the grace is not waited for.
    pub fn finish(&mut self, grace: Duration) -> Vec<u8> {
        let deadline = MonotonicDeadline::after(grace);
        while !self.drain() && !deadline.has_elapsed() {
            crate::time::sleep(Duration::from_millis(10).min(deadline.remaining()));
        }
        self.tail.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_only_the_bounded_tail_and_ends_when_writers_close() {
        let (mut tail, mut writer) = StderrTail::open(8).unwrap();
        writer.write_all(b"0123456789abcdef").unwrap();
        assert!(!tail.drain());
        drop(writer);
        assert_eq!(tail.finish(Duration::from_secs(2)), b"89abcdef");
        assert!(tail.drain());
    }

    #[test]
    fn an_open_writer_does_not_outlast_the_grace() {
        let (mut tail, writer) = StderrTail::open(16).unwrap();
        let started = std::time::Instant::now();
        assert!(tail.finish(Duration::from_millis(50)).is_empty());
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(writer);
    }
}
