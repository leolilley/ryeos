//! Kernel-owned OS fixture: no libtest runtime or background test threads in
//! the child before unsafe startup adoption. The parent deliberately controls
//! exact pipe endpoint installation/disposal using std::process, rather than
//! the normal Lillux runner (whose output collector would keep stdout open and
//! defeat the default-SIGPIPE case). This is not an application host wrapper.

#[cfg(target_os = "linux")]
mod linux {
    use lillux::invocation::{InvocationBounds, StartupInvocation};
    use lillux::time::{Duration, MonotonicDeadline};
    use std::io::{self, Read, Write};
    use std::process::{Child, Command, ExitStatus, Stdio};

    fn child(mode: &str) -> Result<(), String> {
        // Rust startup normally ignores SIGPIPE. Restore the actual default
        // here, before acquisition, to exercise invocation's per-thread mask.
        if mode == "broken" {
            let result = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
            if result == libc::SIG_ERR {
                return Err("cannot set default SIGPIPE".into());
            }
        }
        let budget = if mode == "trickle" {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(3)
        };
        let bounds = InvocationBounds {
            max_arguments: 3,
            max_argument_bytes: 16384,
            max_total_argument_bytes: 32768,
            max_input_bytes: 4096,
            max_output_bytes: 16,
        };
        // SAFETY: dedicated harness=false executable, single-threaded startup,
        // no Rust stdio use, and parent installed distinct pipes without aliases
        // of the child's endpoints. Parent holds only the opposite endpoints.
        let mut invocation = unsafe {
            StartupInvocation::take_inherited_pipes(0, 1, bounds, MonotonicDeadline::after(budget))
        }
        .map_err(|error| error.to_string())?;
        for fd in [0, 1] {
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1
                || io::Error::last_os_error().raw_os_error() != Some(libc::EBADF)
            {
                return Err("startup coordinate was not relocated and closed".into());
            }
        }
        if invocation.arguments().len() != 3 {
            return Err("startup argument capture differs".into());
        }
        if mode == "trickle" {
            return match invocation.read_input() {
                Err(error) if error.kind() == io::ErrorKind::TimedOut => Ok(()),
                _ => Err("trickling input renewed deadline or returned EOF".into()),
            };
        }
        let input = invocation.read_input().map_err(|error| error.to_string())?;
        if input != b"ready" {
            return Err("startup pipe input differs".into());
        }
        if mode == "broken" {
            match invocation.write_output(b"answer") {
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                _ => Err("default SIGPIPE did not return BrokenPipe".into()),
            }
        } else if mode == "echo" {
            invocation
                .write_output(b"relocated:ready")
                .map_err(|error| error.to_string())
        } else {
            Err("unknown child mode".into())
        }
    }

    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            // Exact fixture child only; preserve reap ownership on assertion or
            // I/O failure. This parent never launches grandchildren.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn spawn(mode: &str) -> OwnedChild {
        OwnedChild(
            Command::new(std::env::current_exe().unwrap())
                .args(["--invocation-child", mode])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn wait(child: &mut OwnedChild) -> ExitStatus {
        let deadline = MonotonicDeadline::after(Duration::from_secs(5));
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status;
            }
            assert!(!deadline.has_elapsed(), "startup fixture child stalled");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn echo() {
        let mut child = spawn("echo");
        child.0.stdin.take().unwrap().write_all(b"ready").unwrap();
        assert!(
            wait(&mut child).success(),
            "exclusive startup acquisition failed"
        );
        let mut response = Vec::new();
        child
            .0
            .stdout
            .take()
            .unwrap()
            .take(32)
            .read_to_end(&mut response)
            .unwrap();
        assert_eq!(response, b"relocated:ready");
    }
    fn broken_pipe() {
        let mut child = spawn("broken");
        // Close the only reader BEFORE releasing the child input handshake.
        drop(child.0.stdout.take().unwrap());
        child.0.stdin.take().unwrap().write_all(b"ready").unwrap();
        assert!(
            wait(&mut child).success(),
            "child died by SIGPIPE or failed to report BrokenPipe"
        );
    }
    fn trickle() {
        let mut child = spawn("trickle");
        let mut input = child.0.stdin.take().unwrap();
        // Continue supplying progress longer than the entire child budget.
        // A renewed-on-progress reader reaches EOF, which the child refuses.
        for _ in 0..80 {
            match input.write_all(b"x") {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
                Err(error) => panic!("trickle write: {error}"),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(input);
        assert!(
            wait(&mut child).success(),
            "trickle did not retain absolute input deadline"
        );
    }
    pub(super) fn run() {
        // Argument inspection is kernel test startup setup, not consumer-side
        // invocation parsing; it performs no stdio operation or thread spawn.
        let arguments: Vec<_> = std::env::args_os().collect();
        if arguments
            .get(1)
            .is_some_and(|arg| arg == "--invocation-child")
        {
            let mode = arguments.get(2).and_then(|arg| arg.to_str()).unwrap_or("");
            std::process::exit(if child(mode).is_ok() { 0 } else { 101 });
        }
        echo();
        broken_pipe();
        trickle();
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    linux::run();
    #[cfg(not(target_os = "linux"))]
    eprintln!("invocation startup fixture skipped: capability is Linux-only");
}
