//! Unwind protection for a checkout held across fallible native effects.
//!
//! A frame that moves an owner through several native steps must never let
//! a panic drop it implicitly: the owner may hold live handles whose only
//! release is a supervised terminal. This guard keeps the owner reachable
//! for the steps and, if the frame unwinds before the owner is handed on,
//! retains it whole through `lillux::custody::abandon`, where it is counted
//! and kept until the process exits.

/// Holds one owner for a frame; `into_inner` hands it on, and any other end
/// of the guard retains it under `kind`.
pub(crate) struct UnwindRetained<T: Send + 'static> {
    kind: &'static str,
    owner: Option<T>,
}

impl<T: Send + 'static> UnwindRetained<T> {
    pub(crate) fn new(kind: &'static str, owner: T) -> Self {
        Self { kind, owner: Some(owner) }
    }

    /// Hand the owner on; the emptied guard retains nothing.
    pub(crate) fn into_inner(mut self) -> T {
        self.owner.take().expect("unwind guard holds its owner until handed on")
    }
}

impl<T: Send + 'static> std::ops::Deref for UnwindRetained<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.owner.as_ref().expect("unwind guard holds its owner until handed on")
    }
}

impl<T: Send + 'static> std::ops::DerefMut for UnwindRetained<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.owner.as_mut().expect("unwind guard holds its owner until handed on")
    }
}

impl<T: Send + 'static> Drop for UnwindRetained<T> {
    fn drop(&mut self) {
        // A forked child's copy is relinquished inside `abandon` before any
        // lock or allocation; the original process counts and keeps it.
        if let Some(owner) = self.owner.take() {
            lillux::custody::abandon(self.kind, owner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::UnwindRetained;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Observed(Arc<AtomicBool>);
    impl Drop for Observed {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    const KIND: &str = "ryeos-state::unwind-custody-test";

    #[test]
    fn handed_on_owner_is_not_retained() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let dropped = Arc::new(AtomicBool::new(false));
        let before = lillux::custody::abandoned_custody().count(KIND);
        let guard = UnwindRetained::new(KIND, Observed(Arc::clone(&dropped)));
        drop(guard.into_inner());
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn unwound_owner_is_retained_and_counted() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let dropped = Arc::new(AtomicBool::new(false));
        let before = lillux::custody::abandoned_custody().count(KIND);
        let observed = Arc::clone(&dropped);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = UnwindRetained::new(KIND, Observed(observed));
            panic!("unwind through the guarded frame");
        }));
        assert!(unwound.is_err());
        assert!(!dropped.load(Ordering::Acquire), "the unwound owner was never dropped");
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
    }
}
