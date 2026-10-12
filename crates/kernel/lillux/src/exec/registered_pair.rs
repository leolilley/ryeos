//! The retained clock's terminal two-descriptor operation. This is crate-private
//! host machinery, not a general grant or an ordinary inherited-handle close.

use super::{
    Arc, ForkChildCloseFile, InheritedDescriptorAuthority, direct_attachment_fork_barrier,
    reopen_settled_strand_locked,
};
use crate::time::MonotonicDeadline;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PairCloseObservation {
    pub(crate) first: Result<(), i32>,
    pub(crate) second: Result<(), i32>,
    pub(crate) within_interval: bool,
}

/// Error means no effect and leaves both exact owners untouched. Success means
/// the one terminal operation entered: each close was attempted exactly once,
/// with its result cached in the genuine owner. The consumer MUST NOT restore
/// ordinary descriptor/clock use after receiving that observation, even if a
/// syscall returned errno or the fixed operation finished after the deadline.
pub(crate) fn close_pair(
    first: &mut InheritedDescriptorAuthority,
    second: &mut InheritedDescriptorAuthority,
    original: MonotonicDeadline,
) -> Result<PairCloseObservation, &'static str> {
    let first = Arc::get_mut(&mut first.handle).ok_or("first clock descriptor still has another owner")?;
    let second = Arc::get_mut(&mut second.handle).ok_or("second clock descriptor still has another owner")?;
    // Refuse copied process state before touching a copied barrier mutex.
    let process = std::process::id();
    if first.parent_pid != process || second.parent_pid != process {
        return Err("clock descriptors belong to another process");
    }
    let barrier = direct_attachment_fork_barrier();
    let identity = barrier as *const super::DescriptorForkBarrier as usize;
    if first.barrier_identity != identity || second.barrier_identity != identity {
        return Err("clock descriptor barrier changed");
    }
    if original.has_elapsed() { return Err("original clock close interval exhausted"); }
    let mut state = match barrier.state.try_lock() {
        Ok(state) => state,
        Err(std::sync::TryLockError::WouldBlock) => return Err("clock close barrier busy"),
        Err(std::sync::TryLockError::Poisoned(_)) => return Err("clock close barrier poisoned"),
    };
    reopen_settled_strand_locked(barrier, &mut state);
    if state.fork_quiesced || original.has_elapsed() {
        return Err("clock close barrier unavailable within original interval");
    }
    let live = |file: &ForkChildCloseFile| {
        file.file.is_some() && file.explicit_close.is_none()
            && state.owned_registration_generations.get(&file.fd) == Some(&file.generation)
            && state.fork_child_close_fds.contains(&file.fd)
    };
    if first.fd == second.fd || !live(first) || !live(second) {
        return Err("clock close lacks two distinct original live registrations");
    }
    // The mutex protects the entire finite operation from fork snapshots and
    // registration reuse. No allocation, renewed deadline or ordinary clock
    // observation occurs between these two one-attempt syscalls. A failed close
    // does not prevent disposition of the other already-admitted exact owner.
    let first = first.close_registered_locked(&mut state);
    let second = second.close_registered_locked(&mut state);
    barrier.changed.notify_all();
    Ok(PairCloseObservation { first, second, within_interval: !original.has_elapsed() })
}

#[cfg(test)]
mod controls {
    use super::*;
    use std::time::Duration;

    fn pair() -> (InheritedDescriptorAuthority, InheritedDescriptorAuthority) {
        let lease = crate::exec::try_retain_fork_sensitive_descriptors().unwrap();
        let first = InheritedDescriptorAuthority::from_owned_file(tempfile::tempfile().unwrap(), &lease).unwrap();
        let second = InheritedDescriptorAuthority::from_owned_file(tempfile::tempfile().unwrap(), &lease).unwrap();
        (first, second)
    }

    #[test]
    fn shared_or_busy_before_effects_retains_both_live_descriptors() {
        let (mut first, mut second) = pair();
        let alias = second.clone();
        let deadline = MonotonicDeadline::after(Duration::from_secs(10));
        assert!(close_pair(&mut first, &mut second, deadline).is_err());
        assert!(first.handle.is_registered_live() && second.handle.is_registered_live());
        drop(alias);
        let barrier = direct_attachment_fork_barrier();
        let guard = barrier.state.lock().unwrap();
        assert!(close_pair(&mut first, &mut second, deadline).is_err());
        assert!(first.handle.is_registered_live() && second.handle.is_registered_live());
        drop(guard);
        let receipt = close_pair(&mut first, &mut second, deadline).unwrap();
        assert_eq!((receipt.first, receipt.second), (Ok(()), Ok(())));
        assert!(first.handle.explicitly_retired() && second.handle.explicitly_retired());
        assert!(close_pair(&mut first, &mut second, deadline).is_err());
    }

    #[test]
    fn ambiguous_close_is_cached_and_never_retried_by_coordinate() {
        let (mut first, mut second) = pair();
        // Inject an ambiguous result AFTER actually consuming the owned FD.
        // Never close a stale number or leave a File owning an already-closed
        // coordinate; unrelated threads are free to allocate descriptors.
        let barrier = direct_attachment_fork_barrier();
        let mut guard = barrier.state.lock().unwrap();
        let first = Arc::get_mut(&mut first.handle).unwrap();
        let failed = first.close_registered_locked_with(&mut guard, |fd| {
            assert_eq!(unsafe { libc::close(fd) }, 0);
            Err(libc::EIO)
        });
        assert_eq!(failed, Err(libc::EIO));
        assert_eq!(first.explicit_close, Some(Err(libc::EIO)));
        let second = Arc::get_mut(&mut second.handle).unwrap();
        assert_eq!(second.close_registered_locked(&mut guard), Ok(()));
        drop(guard);
        // Cached explicit failure returns before inspecting another FD; Drop
        // likewise sees no File. It cannot attempt the original number again.
        assert!(first.try_retire_registered(MonotonicDeadline::after(Duration::from_secs(1))).is_err());
        assert!(first.file.is_none());
    }
}
