//! Kernel-owned OS fixture for startup environment capture. The child is this
//! dedicated harness=false executable, so no libtest thread exists when it
//! scrubs and removes its own environment entries. The parent only installs
//! the environment and judges the child's exit status.

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;
    use std::process::Command;

    const CHILD_FLAG: &str = "--environment-capture-child";
    const PRESENT: &str = "LILLUX_CAPTURE_PRESENT";
    const ABSENT: &str = "LILLUX_CAPTURE_ABSENT";
    const NOT_UTF8: &str = "LILLUX_CAPTURE_NOT_UTF8";
    const VALUE: &str = "capture-probe-value-7f3a";
    const RAW_MARKER: &[u8] = b"capture-raw-marker-9b1c";

    fn proc_environment_contains(needle: &[u8]) -> Result<bool, String> {
        let environment =
            std::fs::read("/proc/self/environ").map_err(|error| error.to_string())?;
        Ok(environment
            .windows(needle.len())
            .any(|window| window == needle))
    }

    fn child() -> Result<(), String> {
        if !proc_environment_contains(VALUE.as_bytes())? {
            return Err("fixture value was not installed in the initial environment".into());
        }
        let names = vec![PRESENT.to_owned(), ABSENT.to_owned()];
        // SAFETY: dedicated harness=false executable before any thread spawn.
        let taken = unsafe { lillux::take_environment_values(&names) }?;
        if taken.len() != 1 || taken[0].0 != PRESENT || taken[0].1.as_str() != VALUE {
            return Err("captured values differ from the installed environment".into());
        }
        if std::env::var_os(PRESENT).is_some() {
            return Err("captured name remains in the environment".into());
        }
        if proc_environment_contains(VALUE.as_bytes())? {
            return Err("captured value remains readable through /proc/self/environ".into());
        }

        // A refused value is still scrubbed and removed, and the refusal
        // names the variable without carrying its bytes.
        // SAFETY: as above.
        let refused = unsafe { lillux::take_environment_values(&[NOT_UTF8.to_owned()]) };
        let Err(error) = refused else {
            return Err("a non-UTF-8 value was accepted".into());
        };
        if !error.contains(NOT_UTF8)
            || error
                .as_bytes()
                .windows(RAW_MARKER.len())
                .any(|window| window == RAW_MARKER)
        {
            return Err("refusal does not name only the variable".into());
        }
        if std::env::var_os(NOT_UTF8).is_some() {
            return Err("refused name remains in the environment".into());
        }
        if proc_environment_contains(RAW_MARKER)? {
            return Err("refused value remains readable through /proc/self/environ".into());
        }

        // SAFETY: as above; both calls refuse before touching the environment.
        if unsafe { lillux::take_environment_values(&["A=B".to_owned()]) }.is_ok() {
            return Err("a name containing `=` was accepted".into());
        }
        // SAFETY: as above.
        if unsafe { lillux::take_environment_values(&[ABSENT.to_owned(), ABSENT.to_owned()]) }
            .is_ok()
        {
            return Err("a repeated name was accepted".into());
        }
        // SAFETY: as above.
        if !unsafe { lillux::take_environment_values(&[]) }?.is_empty() {
            return Err("an empty request captured a value".into());
        }
        Ok(())
    }

    pub(super) fn run() {
        // Argument inspection is test startup setup; it spawns no thread.
        let arguments: Vec<_> = std::env::args_os().collect();
        if arguments.get(1).is_some_and(|argument| argument == CHILD_FLAG) {
            std::process::exit(match child() {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("{error}");
                    101
                }
            });
        }
        let mut raw = vec![0xff];
        raw.extend_from_slice(RAW_MARKER);
        let output = Command::new(std::env::current_exe().unwrap())
            .arg(CHILD_FLAG)
            .env(PRESENT, VALUE)
            .env(NOT_UTF8, OsString::from_vec(raw))
            .env_remove(ABSENT)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "environment capture child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    linux::run();
    #[cfg(not(target_os = "linux"))]
    eprintln!("environment capture fixture skipped: capability is Linux-only");
}
