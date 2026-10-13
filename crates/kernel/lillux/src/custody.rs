//! Process-lifetime retention of owners dropped without settlement.
//!
//! An owner whose native handles, descriptors or locks may still be live
//! cannot be released from a destructor: that would bypass its supervised
//! terminal. Instead its `Drop` hands the whole owner here, where it stays
//! reachable until the process exits. Nothing is ever removed, so any nonzero
//! count is a standing fact about this process.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

/// Every owner abandoned in the original process, with the kind its `Drop`
/// named. Only ever appended to.
static ABANDONED: Mutex<Vec<(&'static str, Box<dyn Any + Send>)>> = Mutex::new(Vec::new());

/// Retain `owner` until the process exits without running its destructor.
///
/// In the original process the owner moves whole into the registry, recovering
/// a poisoned lock so a panic elsewhere never releases or loses an owner. In
/// the child of a protected fork the value is only a copy of an owner the
/// original process retains; the child relinquishes it without allocating or
/// entering the copied registry lock.
pub fn abandon<T: Send + 'static>(kind: &'static str, owner: T) {
    if crate::exec::in_forked_child() {
        crate::exec::relinquish_forked_child_owner(owner);
        return;
    }
    let owner: Box<dyn Any + Send> = Box::new(owner);
    ABANDONED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((kind, owner));
}

/// Counts of owners this process retains because they were dropped without
/// settlement. Any nonzero total means that custody stays held until exit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AbandonedCustodyReport {
    pub by_kind: BTreeMap<&'static str, usize>,
    pub total: usize,
}

impl AbandonedCustodyReport {
    /// Retained owners of one kind.
    pub fn count(&self, kind: &str) -> usize {
        self.by_kind.get(kind).copied().unwrap_or(0)
    }
}

/// Report the owners this process retains, per kind and in total.
pub fn abandoned_custody() -> AbandonedCustodyReport {
    let retained = ABANDONED.lock().unwrap_or_else(PoisonError::into_inner);
    let mut report = AbandonedCustodyReport::default();
    for (kind, _) in retained.iter() {
        *report.by_kind.entry(*kind).or_insert(0) += 1;
        report.total += 1;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Observed(Arc<AtomicBool>);
    impl Drop for Observed {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn abandoned_owner_is_counted_and_never_dropped() {
        const KIND: &str = "lillux::custody::test-counted";
        let dropped = Arc::new(AtomicBool::new(false));
        let before = abandoned_custody();
        abandon(KIND, Observed(Arc::clone(&dropped)));
        let after = abandoned_custody();
        assert_eq!(after.count(KIND), before.count(KIND) + 1);
        assert!(after.total > before.total);
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&dropped), 2);
    }

    #[test]
    fn poisoned_registry_still_retains_and_reports() {
        const KIND: &str = "lillux::custody::test-poisoned";
        let poisoner = std::thread::spawn(|| {
            let _held = ABANDONED.lock().unwrap_or_else(PoisonError::into_inner);
            panic!("poison the custody registry");
        });
        assert!(poisoner.join().is_err());
        assert!(ABANDONED.is_poisoned());
        let before = abandoned_custody().count(KIND);
        let dropped = Arc::new(AtomicBool::new(false));
        abandon(KIND, Observed(Arc::clone(&dropped)));
        assert_eq!(abandoned_custody().count(KIND), before + 1);
        assert!(!dropped.load(Ordering::Acquire));
    }

    #[test]
    fn original_process_is_not_a_forked_child() {
        assert!(!crate::exec::in_forked_child());
    }

    /// Ends the forked child with a distinct status if its destructor runs
    /// there; in the original process it is inert.
    #[cfg(target_os = "linux")]
    struct ExitsIfDroppedInChild {
        original: libc::pid_t,
    }
    #[cfg(target_os = "linux")]
    impl Drop for ExitsIfDroppedInChild {
        fn drop(&mut self) {
            if unsafe { libc::getpid() } != self.original {
                unsafe { libc::_exit(3) };
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn forked_child_relinquishes_without_registry_lock_or_destructor() {
        use std::collections::BTreeSet;
        use std::time::{Duration, Instant};
        const KIND: &str = "lillux::custody::test-forked-child";
        let before = abandoned_custody().count(KIND);
        let guard = crate::exec::quiesce_fork_sensitive_descriptors(
            Instant::now() + Duration::from_secs(5),
        )
        .expect("fork window must be available");
        let child_close_fds = guard.fork_child_close_fds(&BTreeSet::new()).unwrap();
        let child_barrier = guard.prepare_child_descriptor_barrier(&BTreeSet::new()).unwrap();
        let (custody, witness_writer) = guard.witness_creation("custody test child").unwrap();
        let owner = ExitsIfDroppedInChild { original: unsafe { libc::getpid() } };
        // The child's copy of this lock stays held: entering it there would
        // hang until the reaper kills the child.
        let held_registry = ABANDONED.lock().unwrap_or_else(PoisonError::into_inner);
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            for fd in &child_close_fds {
                unsafe { libc::close(*fd) };
            }
            unsafe { child_barrier.install_in_actual_fork_child() };
            if !crate::exec::in_forked_child() {
                unsafe { libc::_exit(2) };
            }
            abandon(KIND, owner);
            unsafe { libc::_exit(0) };
        }
        drop(held_registry);
        drop(witness_writer);
        drop(child_barrier);
        let status = crate::exec::reap_fork_holder_until(
            child,
            Instant::now() + Duration::from_secs(5),
            "custody test child",
        )
        .expect("the child relinquishes without entering the registry lock")
        .expect("an exited child is reaped");
        assert!(libc::WIFEXITED(status), "status {status}");
        assert_eq!(libc::WEXITSTATUS(status), 0);
        custody.settle().unwrap();
        drop(owner);
        assert!(!crate::exec::in_forked_child());
        assert_eq!(abandoned_custody().count(KIND), before);
    }
}
