//! Test-only witness for the rejected namespace-membership capture approach.
//!
//! The namespace descriptor, not a parent-PID walk or a numeric process group,
//! identifies descendants, including double forks and nested PID namespaces.
//! Membership is not an atomic writer freeze. Keep the original diagnostic
//! evidence without exporting an unused alternate lifecycle API or making
//! namespace-translation ioctls a production host requirement.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

use super::{ExactProcessIdentity, PinnedMember, linux};

// Linux UAPI <linux/nsfs.h>. The translation ioctls take a PID value (not a
// pointer despite _IOR) and return the translated PID. ESRCH means outside
// the namespace or gone; unsupported ioctls are errors, never a group fallback.
const NS_GET_NSTYPE: libc::c_ulong = 0xb703;
const NS_GET_PID_FROM_PIDNS: libc::c_ulong = 0x8004_b706;
const NS_GET_PID_IN_PIDNS: libc::c_ulong = 0x8004_b708;

/// Kernel namespace coordinate. This is meaningful only alongside the existing
/// exact boot/target-birth identity, never as a standalone reusable inode token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PidNamespaceIdentity {
    pub device: u64,
    pub inode: u64,
}

/// A private descendant namespace and its exact, live PID-1 incarnation.
/// Captured while a trusted launcher holds the target before workload exec.
#[derive(Debug)]
pub struct PinnedPidNamespace {
    namespace: File,
    init: PinnedMember,
    identity: PidNamespaceIdentity,
}

impl PinnedPidNamespace {
    /// Prove that the exact target is PID 1 of a namespace which excludes the
    /// caller. A workload in the caller's namespace cannot grant tree authority.
    pub fn pin(process: &ExactProcessIdentity) -> Result<Self, String> {
        // Namespace authority consumes only the existing target incarnation.
        // Its caller's group is deliberately not a membership or death proof.
        if process.boot_id.is_empty()
            || process.target_pid <= 1
            || process.target_start_time_ticks == 0
        {
            return Err("PID namespace target identity is incomplete".to_owned());
        }
        if linux::read_boot_id()? != process.boot_id {
            return Err("PID namespace owner belongs to another boot".to_owned());
        }
        let init = pin_incarnation(process.target_pid, process.target_start_time_ticks)?;
        // Procfs namespace links are intentional kernel magic links. Pin the
        // target with its pidfd before opening and recheck it after all reads.
        let namespace = File::open(format!("/proc/{}/ns/pid", process.target_pid))
            .map_err(|error| format!("pin target PID namespace: {error}"))?;
        if unsafe { libc::ioctl(namespace.as_raw_fd(), NS_GET_NSTYPE) } != libc::CLONE_NEWPID {
            return Err("target namespace descriptor is not a PID namespace".to_owned());
        }
        if translate(&namespace, NS_GET_PID_IN_PIDNS, std::process::id())?.is_some() {
            return Err("target PID namespace includes the caller".to_owned());
        }
        if translate(&namespace, NS_GET_PID_FROM_PIDNS, 1)? != Some(process.target_pid) {
            return Err("exact target is not the PID namespace init".to_owned());
        }
        if translate(&namespace, NS_GET_PID_IN_PIDNS, process.target_pid)? != Some(1) {
            return Err("PID namespace init translation disagrees with target".to_owned());
        }
        let metadata = namespace
            .metadata()
            .map_err(|error| format!("inspect pinned PID namespace: {error}"))?;
        require_incarnation(&init, process.target_start_time_ticks)?;
        Ok(Self {
            namespace,
            init,
            identity: PidNamespaceIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        })
    }

    pub fn identity(&self) -> PidNamespaceIdentity {
        self.identity
    }

    /// Reattach only the same namespace and exact init incarnation. No namespace
    /// descriptor or live target may be replaced by an equal-looking path.
    pub fn pin_expected(
        process: &ExactProcessIdentity,
        expected: PidNamespaceIdentity,
    ) -> Result<Self, String> {
        let pinned = Self::pin(process)?;
        if pinned.identity != expected {
            return Err("exact target PID namespace identity changed".to_owned());
        }
        Ok(pinned)
    }

    /// Read-only numeric membership observation; this grants no signal. Callers
    /// which act on a member must additionally pin and verify its incarnation.
    pub fn contains_pid(&self, pid: u32) -> Result<bool, String> {
        Ok(translate(&self.namespace, NS_GET_PID_IN_PIDNS, pid)?.is_some())
    }

    /// Kill the exact namespace init and wait for kernel exit testimony.
    /// Group/session changes cannot escape that lifetime boundary. This proves
    /// exit, not that the launcher's wait/reap obligation has been settled.
    pub fn terminate_and_wait(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "PID namespace termination deadline overflow".to_owned())?;
        match linux::pidfd_signal(self.init.pidfd.as_raw_fd(), libc::SIGKILL, 0) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => return Err(format!("terminate exact namespace init: {error}")),
        }
        linux::wait_pidfd_exit(self.init.pidfd.as_raw_fd(), deadline)
    }

    /// Observe natural or externally requested namespace termination without
    /// sending a signal. The non-thread pidfd becomes readable only after the
    /// init thread group exits. In Linux, find_child_reaper() calls
    /// zap_pid_ns_processes() before exit_notify() publishes that exit: new PID
    /// allocation is disabled, descendants are killed, and their teardown is
    /// awaited. The init may still have an unreaped zombie PID afterwards.
    ///
    /// Do not replace this retained descriptor proof with absence of a numeric
    /// PID, a process-group scan, or absence of a procfs namespace link.
    pub fn wait_for_exit(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "PID namespace exit deadline overflow".to_owned())?;
        linux::wait_pidfd_exit(self.init.pidfd.as_raw_fd(), deadline)
    }
}

fn translate(namespace: &File, operation: libc::c_ulong, pid: u32) -> Result<Option<u32>, String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err("PID namespace translation requires a positive PID".to_owned());
    }
    let result = unsafe { libc::ioctl(namespace.as_raw_fd(), operation, libc::c_ulong::from(pid)) };
    if result > 0 {
        return Ok(Some(result as u32));
    }
    let error = std::io::Error::last_os_error();
    if result == -1 && error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(None);
    }
    Err(format!("kernel PID namespace translation failed: {error}"))
}

fn pin_incarnation(pid: u32, start: u64) -> Result<PinnedMember, String> {
    let member = PinnedMember {
        pid,
        pidfd: linux::open_pidfd(pid)?,
    };
    require_incarnation(&member, start)?;
    Ok(member)
}

fn require_incarnation(member: &PinnedMember, start: u64) -> Result<(), String> {
    linux::pidfd_signal(member.pidfd.as_raw_fd(), 0, 0)
        .map_err(|error| format!("probe namespace process incarnation: {error}"))?;
    let stat = linux::read_process_stat(member.pid)
        .map_err(|error| format!("read namespace process incarnation: {error}"))?;
    if stat.start_time_ticks != start || matches!(stat.state, 'Z' | 'X') {
        return Err("PID namespace process incarnation changed or exited".to_owned());
    }
    linux::pidfd_signal(member.pidfd.as_raw_fd(), 0, 0)
        .map_err(|error| format!("reprobe namespace process incarnation: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(pid: u32) -> ExactProcessIdentity {
        let stat = linux::read_process_stat(pid).unwrap();
        let leader = linux::read_process_stat(stat.process_group as u32).unwrap();
        ExactProcessIdentity {
            boot_id: linux::read_boot_id().unwrap(),
            target_pid: pid,
            target_start_time_ticks: stat.start_time_ticks,
            group_leader_pid: stat.process_group as u32,
            group_leader_start_time_ticks: leader.start_time_ticks,
        }
    }

    #[test]
    fn pid_namespace_authority_refuses_callers_own_namespace() {
        let error = PinnedPidNamespace::pin(&identity(std::process::id())).unwrap_err();
        assert!(error.contains("includes the caller"), "{error}");
    }

    #[test]
    fn pid_namespace_authority_refuses_changed_birth_and_boot() {
        let mut process = identity(std::process::id());
        process.target_start_time_ticks += 1;
        assert!(
            PinnedPidNamespace::pin(&process)
                .unwrap_err()
                .contains("incarnation")
        );
        process.boot_id = "not-this-boot".to_owned();
        assert!(
            PinnedPidNamespace::pin(&process)
                .unwrap_err()
                .contains("another boot")
        );
    }

    #[test]
    fn pid_namespace_translation_has_no_unsupported_descriptor_fallback() {
        let file = tempfile::tempfile().unwrap();
        assert!(translate(&file, NS_GET_PID_IN_PIDNS, std::process::id()).is_err());
    }

    #[test]
    #[ignore = "native Linux namespace qualification; uses host util-linux only as a test fixture"]
    fn pid_namespace_authority_covers_detached_and_nested_descendants() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        use std::process::{Child, Command, Stdio};

        struct Fixture(Child);
        impl Drop for Fixture {
            fn drop(&mut self) {
                // The Child retains the exact unreaped wrapper. util-linux's
                // explicit kill-child option binds namespace-init lifetime.
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        // Host executables are fixture construction, not launch dependencies
        // or a substitute for the native sandbox's separate E2E acceptance.
        let mut fixture = Fixture(
            Command::new("unshare")
                .args([
                    "--user", "--map-root-user", "--pid", "--fork", "--kill-child=KILL",
                    "/bin/sh", "-c",
                    "setsid unshare --pid --fork --kill-child=KILL /bin/sh -c 'echo ready; exec sleep 60' & wait",
                ])
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn().unwrap(),
        );
        let output = fixture.0.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(output).read_line(&mut line).map(|_| line);
            let _ = tx.send(result);
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            "ready\n"
        );
        reader.join().unwrap();
        let children = |pid: u32| -> Vec<u32> {
            std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
                .unwrap()
                .split_whitespace()
                .map(|pid| pid.parse().unwrap())
                .collect()
        };
        let init_pid = children(fixture.0.id())[0];
        let process = identity(init_pid);
        let owner = PinnedPidNamespace::pin(&process).unwrap();
        let detached = children(init_pid)[0];
        let nested = children(detached)[0];
        let detached_fd = linux::open_pidfd(detached).unwrap();
        let nested_fd = linux::open_pidfd(nested).unwrap();
        assert_ne!(
            linux::read_process_stat(detached).unwrap().process_group,
            linux::read_process_stat(init_pid).unwrap().process_group
        );
        assert!(owner.contains_pid(init_pid).unwrap());
        assert!(owner.contains_pid(detached).unwrap());
        assert!(owner.contains_pid(nested).unwrap());
        assert!(!owner.contains_pid(std::process::id()).unwrap());
        assert!(!owner.contains_pid(fixture.0.id()).unwrap());
        // A successful group barrier is NOT a whole-namespace barrier. Keep
        // this counterexample beside the primitive so future callers cannot
        // allow setsid/setpgid and silently retain the old capture authority.
        let group =
            super::super::quiesce_exact_process_group(&process, Duration::from_secs(2)).unwrap();
        assert!(matches!(
            linux::read_process_stat(init_pid).unwrap().state,
            'T' | 't'
        ));
        assert!(!matches!(
            linux::read_process_stat(nested).unwrap().state,
            'T' | 't'
        ));
        group.resume().unwrap();
        PinnedPidNamespace::pin_expected(&process, owner.identity()).unwrap();
        let mut wrong = owner.identity();
        wrong.inode += 1;
        assert!(PinnedPidNamespace::pin_expected(&process, wrong).is_err());
        assert!(owner.wait_for_exit(Duration::ZERO).is_err());
        owner.terminate_and_wait(Duration::from_secs(5)).unwrap();
        owner.wait_for_exit(Duration::ZERO).unwrap();
        // Assert exact descendant exit, not disappearance of reusable numeric
        // PIDs. The init can legitimately remain a zombie until its parent
        // reaps it; that does not imply a live descendant or pending writes.
        linux::wait_pidfd_exit(detached_fd.as_raw_fd(), Instant::now()).unwrap();
        linux::wait_pidfd_exit(nested_fd.as_raw_fd(), Instant::now()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while fixture.0.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "launcher did not reap namespace init"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // After the parent has reaped init, even a retained namespace fd cannot
        // translate PID 1 into a resurrected or reused process incarnation.
        assert_eq!(
            translate(&owner.namespace, NS_GET_PID_FROM_PIDNS, 1).unwrap(),
            None
        );
        assert!(!owner.contains_pid(detached).unwrap());
        assert!(!owner.contains_pid(nested).unwrap());
    }
}
