//! Operating-system specifics of the adapter, kept in one place.
//!
//! Everything that touches signals, process groups or parent-death behaviour
//! lives here so it can move into the platform crate as one unit. The adapter
//! cannot depend on that crate today: it is a small workload binary and the
//! platform crate pulls in the whole kernel dependency set.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

/// A signal the adapter sends to Claude Code's process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupSignal {
    Terminate,
    Kill,
}

/// Start `command` as the leader of a new process group that dies with this
/// process. `PR_SET_PDEATHSIG` is tied to the thread that forks, so the
/// caller must keep that thread alive for as long as the child runs; the turn
/// thread does.
pub fn spawn_in_own_group(command: &mut Command) -> io::Result<Child> {
    // SAFETY: getpid has no preconditions.
    let parent = unsafe { libc::getpid() };
    command.process_group(0);
    // SAFETY: the closure calls only async-signal-safe functions (prctl,
    // getppid, _exit) and touches no allocator or lock.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            // The parent may have died between fork and prctl.
            if libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
    command.spawn()
}

/// Signal every member of the process group led by `leader`. A group with no
/// members left is not an error.
pub fn signal_group(leader: u32, signal: GroupSignal) {
    let signal = match signal {
        GroupSignal::Terminate => libc::SIGTERM,
        GroupSignal::Kill => libc::SIGKILL,
    };
    let Ok(leader) = libc::pid_t::try_from(leader) else {
        return;
    };
    if leader <= 1 {
        return;
    }
    // SAFETY: kill with a negative pid addresses a process group; both
    // arguments are plain integers.
    unsafe {
        libc::kill(-leader, signal);
    }
}

/// Run `on_terminate` on SIGTERM or SIGINT, then exit with 143. Runs on its
/// own thread, so the callback may block while turns wind down.
pub fn exit_on_terminate(on_terminate: impl FnOnce() + Send + 'static) -> io::Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT])?;
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            on_terminate();
            std::process::exit(143);
        }
    });
    Ok(())
}
