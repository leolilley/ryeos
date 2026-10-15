//! Process-lifetime retention of owners dropped without settlement.
//!
//! An owner whose native handles, descriptors or locks may still be live
//! cannot be released from a destructor: that would bypass its supervised
//! terminal. Instead its `Drop` hands the whole owner here, where it stays
//! reachable until the process exits. Nothing is ever removed, so any nonzero
//! count is a standing fact about this process.
//!
//! [`Held`] carries that decision with the owner: its end drops a settled
//! owner, retains an unsettled one here, and relinquishes a copy reached in a
//! foreign process. A `Held` created inside a forked child is foreign there
//! for its whole life, since the child is never the issuing process of
//! anything: it can be relinquished, never settled, taken or counted.

use std::any::Any;
use std::collections::BTreeMap;
use std::mem::ManuallyDrop;
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
        relinquish_in_foreign_process(owner);
        return;
    }
    let owner: Box<dyn Any + Send> = Box::new(owner);
    ABANDONED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((kind, owner));
}

/// Give up a copied process's copy of an owner without running its
/// destructor.
///
/// Use this only where the caller has already established that the running
/// process is not the one that issued `owner`, such as a forked child holding
/// a copy of its parent's memory. Every native handle, descriptor or lock
/// inside the copy belongs to the original process, which retains the real
/// owner; running the copy's destructor would act on custody this process does
/// not hold, and recording it would allocate or enter a mutex copied
/// mid-operation by the fork. This is the only place an owner's destructor is
/// deliberately skipped: the named binding below holds the copy until the end
/// of this frame and is never dropped. Allocation-free and lock-free, so it is
/// safe between fork and exec. Never call it in the issuing process; an owner
/// dropped there unsettled goes to [`abandon`].
pub fn relinquish_in_foreign_process<T>(owner: T) {
    let _relinquished_foreign_copy = std::mem::ManuallyDrop::new(owner);
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

/// Whether an owner issued in process `issuer` is only a copy here: this is
/// the child of a protected fork, which holds even when a nested PID namespace
/// hands the child its parent's pid, or another process altogether, such as
/// the child of a fork lillux did not supervise. Reads one atomic and the
/// process id: no allocation and no lock, so a destructor may consult it
/// before touching anything copied.
pub fn foreign_process(issuer: u32) -> bool {
    crate::exec::in_forked_child() || issuer != std::process::id()
}

/// Custody of one whole owner whose release is supervised.
///
/// The owner is released only through [`Held::settle`], [`Held::take_whole`]
/// or [`Held::abandon`]; any other end of the `Held` (an early return, an
/// unwind, a containing value dropped or destructured) decides in this order:
///
/// 1. In a process other than the issuer the owner is only a copy. It is
///    relinquished without allocating, entering a lock or consulting the
///    predicate, and its destructor never runs.
/// 2. In the issuer, if the `settled` predicate holds, the owner is dropped
///    normally, its fields in declaration order.
/// 3. Otherwise the owner is retained whole and counted under `kind` until the
///    process exits, exactly as [`abandon`] does.
///
/// The predicate must not panic. Should it panic anyway at the end of a
/// `Held`, the panic is caught and the owner is treated as unsettled: retained
/// and counted, its destructor never run. A panic inside [`Held::settle`] is
/// not caught there; the `Held` then ends during that unwind, calls the
/// predicate a second time, and a second panic aborts the process.
///
/// Containers of a `Held` need no `Drop` of their own, so destructuring them
/// with `..` still ends the `Held` through this order. The `Held` itself
/// cannot be destructured, emptied or overwritten, and there is no `DerefMut`:
/// mutation goes through the explicit [`Held::get_mut`]. That access is the
/// one remaining hole: replacing the whole owner through it
/// (`*held.get_mut() = x`, `mem::replace`, `mem::swap`) drops or hands out the
/// old owner unsupervised. `T` must therefore be constructible only inside its
/// owning module, and parts that carry custody of their own must not sit
/// behind interior mutability; nest each in its own `Held`.
///
/// ```compile_fail,E0451
/// use lillux::custody::Held;
/// let held = Held::until_taken("doc", vec![1u8]);
/// let Held { whole, .. } = held;
/// ```
///
/// ```compile_fail,E0596
/// use lillux::custody::Held;
/// let mut held = Held::until_taken("doc", vec![1u8]);
/// let _emptied = std::mem::take(&mut *held);
/// ```
///
/// ```compile_fail,E0594
/// use lillux::custody::Held;
/// let mut held = Held::until_taken("doc", vec![1u8]);
/// *held = vec![2u8];
/// ```
///
/// ```compile_fail,E0277
/// use lillux::custody::Held;
/// let held = Held::until_taken("doc", vec![1u8]);
/// let _copy = Clone::clone(&held);
/// ```
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// use lillux::custody::Held;
/// Held::until_taken("doc", vec![1u8]);
/// ```
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// use lillux::custody::Held;
/// Held::until_taken("doc", vec![1u8]).abandon();
/// ```
#[must_use = "the end of a Held drops a settled owner, retains and counts an unsettled one, and relinquishes a foreign copy"]
pub struct Held<T: Send + 'static> {
    kind: &'static str,
    issuer: u32,
    settled: fn(&T) -> bool,
    whole: ManuallyDrop<T>,
}

/// Proof that a [`Held`] owner was settled and dropped in its issuing process.
#[must_use]
#[derive(Debug)]
pub struct Settled(());

/// Proof that a [`Held`] owner was retained and counted in its issuing
/// process, or relinquished as a copy in any other.
#[must_use]
#[derive(Debug)]
pub struct Abandoned(());

fn never_settled<T>(_: &T) -> bool {
    false
}

impl<T: Send + 'static> Held<T> {
    /// Hold `whole` for this process under `kind`; its end drops it only when
    /// `settled` holds. `settled` must not panic.
    pub fn new(kind: &'static str, whole: T, settled: fn(&T) -> bool) -> Self {
        Self { kind, issuer: std::process::id(), settled, whole: ManuallyDrop::new(whole) }
    }

    /// Hold `whole` under `kind` until [`Held::take_whole`] hands it on; any
    /// other end retains it.
    pub fn until_taken(kind: &'static str, whole: T) -> Self {
        Self::new(kind, whole, never_settled::<T>)
    }

    /// Hold `whole` as if issued by `issuer`, standing in for a copy reached
    /// in another process.
    #[cfg(test)]
    fn as_issued_by(kind: &'static str, whole: T, settled: fn(&T) -> bool, issuer: u32) -> Self {
        Self { kind, issuer, settled, whole: ManuallyDrop::new(whole) }
    }

    /// The kind an unsettled end is counted under.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// The process that issued this custody.
    pub fn issuer(&self) -> u32 {
        self.issuer
    }

    /// Whether the running process holds only a copy of this custody.
    pub fn is_foreign(&self) -> bool {
        foreign_process(self.issuer)
    }

    /// Mutable access to the owner's contents, for changing fields the
    /// predicate reads. Replacing the whole owner through this reference
    /// (`*held.get_mut() = x`, `mem::replace`, `mem::swap`) releases the old
    /// owner unsupervised, so `T` must be constructible only inside its owning
    /// module. A field that carries custody of its own belongs in a nested
    /// `Held`, never behind interior mutability, so replacing it here cannot
    /// release it.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.whole
    }

    /// Hand the owner on whole. A copy in a foreign process is refused and
    /// comes back still held.
    pub fn take_whole(self) -> Result<T, Self> {
        if self.is_foreign() {
            return Err(self);
        }
        Ok(self.into_whole())
    }

    /// Drop the owner now if `settled` holds in its issuing process;
    /// otherwise it comes back still held. The predicate is called here
    /// without catching a panic; a panic ends this `Held` during the unwind,
    /// which calls the predicate again, and a second panic aborts.
    pub fn settle(self) -> Result<Settled, Self> {
        if self.is_foreign() || !(self.settled)(&self.whole) {
            return Err(self);
        }
        drop(self.into_whole());
        Ok(Settled(()))
    }

    /// Hand the owner on whole, or end the process. A foreign copy is
    /// relinquished and the process aborts. For arms made unreachable by an
    /// earlier foreign check, where [`Held::take_whole`]'s refusal has nowhere
    /// to go.
    pub fn take_or_abort(self) -> T {
        if self.is_foreign() {
            relinquish_in_foreign_process(self.into_whole());
            std::process::abort();
        }
        self.into_whole()
    }

    /// Retain the owner and count it under `kind` in its issuing process, or
    /// relinquish a foreign copy without allocating or locking.
    pub fn abandon(self) -> Abandoned {
        let foreign = self.is_foreign();
        let kind = self.kind;
        let whole = self.into_whole();
        if foreign {
            relinquish_in_foreign_process(whole);
        } else {
            abandon(kind, whole);
        }
        Abandoned(())
    }

    /// Move the owner out, leaving nothing for this `Held`'s end to decide.
    fn into_whole(self) -> T {
        let mut ended = ManuallyDrop::new(self);
        // SAFETY: `ended` is never dropped and `whole` is never read again;
        // every other field is `Copy`, so nothing else is left behind.
        unsafe { ManuallyDrop::take(&mut ended.whole) }
    }
}

impl<T: Send + 'static> std::ops::Deref for Held<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.whole
    }
}

impl<T: Send + 'static> Drop for Held<T> {
    fn drop(&mut self) {
        let foreign = foreign_process(self.issuer);
        // SAFETY: this destructor runs once and `whole` is never read again.
        let whole = unsafe { ManuallyDrop::take(&mut self.whole) };
        if foreign {
            relinquish_in_foreign_process(whole);
            return;
        }
        // A panicking predicate counts as unsettled, so the owner is retained
        // rather than dropped by the unwind.
        let settled = self.settled;
        let settled =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| settled(&whole)))
                .unwrap_or(false);
        if settled {
            drop(whole);
        } else {
            abandon(self.kind, whole);
        }
    }
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

    fn always_settled<T>(_: &T) -> bool {
        true
    }

    #[test]
    fn settled_held_drops_its_owner_uncounted() {
        const KIND: &str = "lillux::custody::test-held-settled";
        let dropped = Arc::new(AtomicBool::new(false));
        let before = abandoned_custody().count(KIND);
        let held = Held::new(KIND, Observed(Arc::clone(&dropped)), always_settled);
        assert_eq!(held.kind(), KIND);
        assert_eq!(held.issuer(), std::process::id());
        assert!(!held.is_foreign());
        drop(held);
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(abandoned_custody().count(KIND), before);
    }

    #[test]
    fn take_or_abort_hands_its_owner_on_in_the_issuing_process() {
        const KIND: &str = "lillux::custody::test-held-take-or-abort";
        let dropped = Arc::new(AtomicBool::new(false));
        let before = abandoned_custody().count(KIND);
        let owner = Held::until_taken(KIND, Observed(Arc::clone(&dropped))).take_or_abort();
        assert!(!dropped.load(Ordering::Acquire));
        drop(owner);
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(abandoned_custody().count(KIND), before);
    }

    #[test]
    fn unsettled_held_is_counted_and_its_destructor_never_runs() {
        const KIND: &str = "lillux::custody::test-held-unsettled";
        let dropped = Arc::new(AtomicBool::new(false));
        let before = abandoned_custody().count(KIND);
        drop(Held::new(KIND, Observed(Arc::clone(&dropped)), never_settled));
        assert_eq!(abandoned_custody().count(KIND), before + 1);
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&dropped), 2);

        let explicit = Arc::new(AtomicBool::new(false));
        let _abandoned = Held::new(KIND, Observed(Arc::clone(&explicit)), always_settled).abandon();
        assert_eq!(abandoned_custody().count(KIND), before + 2);
        assert!(!explicit.load(Ordering::Acquire));
    }

    #[test]
    fn until_taken_hands_its_owner_on_through_take_whole() {
        const KIND: &str = "lillux::custody::test-held-until-taken";
        let dropped = Arc::new(AtomicBool::new(false));
        let before = abandoned_custody().count(KIND);
        let held = Held::until_taken(KIND, Observed(Arc::clone(&dropped)));
        let owner = match held.take_whole() {
            Ok(owner) => owner,
            Err(_) => panic!("the issuing process takes its own owner"),
        };
        assert!(!dropped.load(Ordering::Acquire));
        drop(owner);
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(abandoned_custody().count(KIND), before);

        let kept = Arc::new(AtomicBool::new(false));
        drop(Held::until_taken(KIND, Observed(Arc::clone(&kept))));
        assert_eq!(abandoned_custody().count(KIND), before + 1);
        assert!(!kept.load(Ordering::Acquire));
    }

    struct Closable {
        closed: bool,
        _observed: Observed,
    }

    fn closed(owner: &Closable) -> bool {
        owner.closed
    }

    #[test]
    fn settle_refusal_returns_the_owner_still_held() {
        const KIND: &str = "lillux::custody::test-held-settle-refused";
        let dropped = Arc::new(AtomicBool::new(false));
        let before = abandoned_custody().count(KIND);
        let owner = Closable { closed: false, _observed: Observed(Arc::clone(&dropped)) };
        let mut held = match Held::new(KIND, owner, closed).settle() {
            Ok(_) => panic!("an open owner does not settle"),
            Err(held) => held,
        };
        assert!(!dropped.load(Ordering::Acquire));
        assert!(!held.closed);
        held.get_mut().closed = true;
        let _settled = held.settle().map_err(|_| ()).expect("a closed owner settles");
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(abandoned_custody().count(KIND), before);
    }

    #[test]
    fn unwound_held_is_abandoned() {
        const KIND: &str = "lillux::custody::test-held-unwound";
        let dropped = Arc::new(AtomicBool::new(false));
        let before = abandoned_custody().count(KIND);
        let observed = Arc::clone(&dropped);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = Held::until_taken(KIND, Observed(observed));
            panic!("unwind through the held frame");
        }));
        assert!(unwound.is_err());
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(abandoned_custody().count(KIND), before + 1);
    }

    #[test]
    fn foreign_issuer_relinquishes_uncounted() {
        const KIND: &str = "lillux::custody::test-held-foreign";
        let other = std::process::id().wrapping_add(1);
        assert!(foreign_process(other));
        assert!(!foreign_process(std::process::id()));
        let before = abandoned_custody().count(KIND);

        let dropped = Arc::new(AtomicBool::new(false));
        let held = Held::as_issued_by(KIND, Observed(Arc::clone(&dropped)), always_settled, other);
        assert!(held.is_foreign());
        assert_eq!(held.issuer(), other);
        let held = match held.take_whole() {
            Ok(_) => panic!("a foreign copy is never handed on"),
            Err(held) => held,
        };
        let held = match held.settle() {
            Ok(_) => panic!("a foreign copy never settles"),
            Err(held) => held,
        };
        drop(held);
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&dropped), 2);

        let explicit = Arc::new(AtomicBool::new(false));
        let _abandoned =
            Held::as_issued_by(KIND, Observed(Arc::clone(&explicit)), never_settled, other)
                .abandon();
        assert!(!explicit.load(Ordering::Acquire));
        assert_eq!(abandoned_custody().count(KIND), before);
    }

    fn panics(_: &Observed) -> bool {
        panic!("a predicate that breaks its contract")
    }

    #[test]
    fn panicking_predicate_at_the_end_counts_as_unsettled() {
        const KIND: &str = "lillux::custody::test-held-panicking-predicate";
        let dropped = Arc::new(AtomicBool::new(false));
        let before = abandoned_custody().count(KIND);
        drop(Held::new(KIND, Observed(Arc::clone(&dropped)), panics));
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&dropped), 2);
        assert_eq!(abandoned_custody().count(KIND), before + 1);
    }

    struct DropLessOwner {
        inner: Held<Observed>,
        label: String,
    }

    #[test]
    fn destructuring_a_drop_less_owner_still_ends_its_held() {
        const KIND: &str = "lillux::custody::test-held-destructured";
        let dropped = Arc::new(AtomicBool::new(false));
        let before = abandoned_custody().count(KIND);
        {
            let owner = DropLessOwner {
                inner: Held::until_taken(KIND, Observed(Arc::clone(&dropped))),
                label: String::from("plain"),
            };
            assert_eq!(owner.inner.kind(), KIND);
            let DropLessOwner { label, .. } = owner;
            assert_eq!(label, "plain");
        }
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(abandoned_custody().count(KIND), before + 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn forked_child_relinquishes_held_without_registry_lock_or_destructor() {
        use std::collections::BTreeSet;
        use std::time::{Duration, Instant};
        const KIND: &str = "lillux::custody::test-held-forked-child";
        let before = abandoned_custody().count(KIND);
        let guard = crate::exec::quiesce_fork_sensitive_descriptors(
            Instant::now() + Duration::from_secs(5),
        )
        .expect("fork window must be available");
        let child_close_fds = guard.fork_child_close_fds(&BTreeSet::new()).unwrap();
        let child_barrier = guard.prepare_child_descriptor_barrier(&BTreeSet::new()).unwrap();
        let (custody, witness_writer) = guard.witness_creation("custody held test child").unwrap();
        let original = unsafe { libc::getpid() };
        let settled = Held::new(KIND, ExitsIfDroppedInChild { original }, always_settled);
        let unsettled = Held::until_taken(KIND, ExitsIfDroppedInChild { original });
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
            if !settled.is_foreign() || !unsettled.is_foreign() {
                unsafe { libc::_exit(2) };
            }
            drop(settled);
            drop(unsettled);
            unsafe { libc::_exit(0) };
        }
        drop(held_registry);
        drop(witness_writer);
        drop(child_barrier);
        let status = crate::exec::reap_fork_holder_until(
            child,
            Instant::now() + Duration::from_secs(5),
            "custody held test child",
        )
        .expect("the child relinquishes without entering the registry lock")
        .expect("an exited child is reaped");
        assert!(libc::WIFEXITED(status), "status {status}");
        assert_eq!(libc::WEXITSTATUS(status), 0);
        custody.settle().unwrap();
        assert!(!settled.is_foreign());
        let _settled =
            settled.settle().map_err(|_| ()).expect("the issuer settles its own owner");
        drop(unsettled.take_whole().map_err(|_| ()).expect("the issuer takes its own owner"));
        assert_eq!(abandoned_custody().count(KIND), before);
    }
}
