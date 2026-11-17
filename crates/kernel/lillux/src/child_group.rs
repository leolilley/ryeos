//! Child processes that lead their own process group and die with their
//! parent, plus the matching group signalling and parent-termination hooks.
//!
//! These are small helpers for workload binaries that supervise one external
//! program: start it so that nothing it spawns outlives the supervisor, tear
//! the whole group down on request, and shut down cleanly when the supervisor
//! itself is asked to terminate.

use std::io;
use std::process::{Child, Command};

/// A signal sent to every member of a process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupSignal {
    /// Ask the members to exit (SIGTERM).
    Terminate,
    /// End the members without giving them a chance to react (SIGKILL).
    Kill,
}

/// Start `command` as the leader of a new process group that dies with this
/// process. `PR_SET_PDEATHSIG` is tied to the thread that forks, so the
/// caller must keep that thread alive for as long as the child runs.
#[cfg(unix)]
pub fn spawn_in_own_group(command: &mut Command) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;

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
/// members left is not an error; a leader of 0 or 1, or one that does not fit
/// a pid, is ignored.
#[cfg(unix)]
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
/// own thread, so the callback may block while other work winds down.
#[cfg(unix)]
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn pgid_of(pid: u32) -> libc::pid_t {
        // SAFETY: getpgid takes a plain integer.
        unsafe { libc::getpgid(pid as libc::pid_t) }
    }

    #[test]
    fn spawned_child_leads_its_own_group_and_group_signal_ends_it() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut child = spawn_in_own_group(&mut command).unwrap();
        let pid = child.id();
        assert_eq!(pgid_of(pid), pid as libc::pid_t);
        assert_ne!(pgid_of(pid), pgid_of(std::process::id()));

        signal_group(pid, GroupSignal::Terminate);
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn kill_signal_ends_the_group_and_repeat_signal_is_harmless() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut child = spawn_in_own_group(&mut command).unwrap();
        let pid = child.id();
        signal_group(pid, GroupSignal::Kill);
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        // The group is gone and the pid reaped; this must not panic.
        signal_group(pid, GroupSignal::Kill);
    }

    #[test]
    fn signalling_leader_zero_or_one_or_out_of_range_is_a_no_op() {
        signal_group(0, GroupSignal::Kill);
        signal_group(1, GroupSignal::Kill);
        signal_group(u32::MAX, GroupSignal::Kill);
    }
}
