//! One verification of the registered bundle generation per operation.
//!
//! The guarantee a guarded operation gives is "the installed bundles did not
//! change while this ran": verify the generation, do the work, verify it
//! again, and refuse the result if either verification fails. Work inside the
//! operation reaches two further kinds of verification point for the same
//! generation that commit nothing themselves: a nested guarded engine call,
//! and the launch of a handler subprocess, which only returns a value. Those
//! add nothing to the guarantee, because the enclosing after-verification
//! already covers everything that ran before it, and each of them re-verifies
//! every installed bundle. Verification points in front of an effect (a
//! workload launch, a resource allocation, a write) are not covered and keep
//! verifying for themselves.
//!
//! This module lets those two kinds of inner point see that they are enclosed:
//!
//! * [`VerifiedGeneration`] is the token of one enclosing operation. Only
//!   [`run_verified`] creates it, and only after the before-verification has
//!   passed. It names the exact generation that was verified, by the
//!   process-local identity minted when that generation was retained. The
//!   operation receives it by reference for the length of its closure and
//!   cannot keep it.
//! * While the operation's closure runs, the calling thread carries a marker
//!   for that identity. [`is_covered`] answers whether the calling thread is
//!   inside an operation for a given identity. A point for a different
//!   generation is never covered.
//! * The marker is removed when the closure returns or unwinds, before the
//!   after-verification runs. The after-verification is therefore always a
//!   real one.
//!
//! Why a per-thread marker is sound here. The operation is a synchronous
//! closure: it cannot be suspended at an `.await`, so no other task can run on
//! this thread between the moment the marker is set and the moment it is
//! removed. Everything that observes the marker is therefore called, directly
//! or indirectly, by the operation itself. Other threads never see it: a
//! thread the operation starts (a blocking-pool job, a scoped worker) has its
//! own empty marker list and verifies for itself, unless the operation hands
//! it the token explicitly through [`VerifiedGeneration::cover_current_thread`].
//! That is only possible on a thread that is joined before the operation's
//! closure returns, because the token is borrowed from that closure.
//!
//! Markers are only ever set and removed inside this module, by a value that
//! is created and dropped within one function body. No caller can leak a
//! marker past the end of its operation.

use std::cell::RefCell;

thread_local! {
    /// Generation identities of the guarded operations the current thread is
    /// inside, outermost first.
    static ENCLOSING_OPERATIONS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Token of one guarded operation: the generation named here was verified
/// before the operation's closure was called and will be verified again after
/// it returns.
///
/// An inert token, from a runtime with no retained generation, names no
/// generation and covers nothing.
pub struct VerifiedGeneration {
    identity: Option<u64>,
}

impl VerifiedGeneration {
    /// Process-local identity of the verified generation.
    pub fn identity(&self) -> Option<u64> {
        self.identity
    }

    /// Run `work` on the calling thread as part of this token's operation, so
    /// nested guarded operations and handler launches for the same generation
    /// reached by `work` are covered by the operation's own before and after
    /// verification.
    ///
    /// The engine uses this for worker threads that it starts and joins inside
    /// the operation. The token's borrow keeps `work` from outliving the
    /// operation.
    pub(crate) fn cover_current_thread<T>(&self, work: impl FnOnce() -> T) -> T {
        match self.identity {
            Some(identity) => {
                let _marker = Marker::set(identity);
                work()
            }
            None => work(),
        }
    }
}

/// Whether the calling thread is inside a guarded operation for exactly this
/// generation identity.
pub(super) fn is_covered(identity: u64) -> bool {
    ENCLOSING_OPERATIONS.with(|operations| operations.borrow().contains(&identity))
}

/// Verify, run `operation`, verify again.
///
/// `verify` is the real verification of the generation named by `identity`.
/// When the calling thread is already inside an operation for the same
/// identity, the enclosing operation's verification covers this one and
/// `verify` is not called. Otherwise `verify` runs before `operation` and
/// again after it, and a failure of either refuses the result.
pub(super) fn run_verified<T, E>(
    identity: u64,
    verify: impl Fn() -> Result<(), E>,
    operation: impl FnOnce(&VerifiedGeneration) -> Result<T, E>,
) -> Result<T, E> {
    let token = VerifiedGeneration {
        identity: Some(identity),
    };
    if is_covered(identity) {
        return operation(&token);
    }
    verify()?;
    let outcome = {
        let _marker = Marker::set(identity);
        operation(&token)
    };
    // The marker is gone: this verification is never covered.
    debug_assert!(!is_covered(identity));
    let value = outcome?;
    verify()?;
    Ok(value)
}

/// Run `operation` for a runtime that retains no generation. There is nothing
/// to verify and nothing is covered.
pub(super) fn run_unregistered<T, E>(
    operation: impl FnOnce(&VerifiedGeneration) -> Result<T, E>,
) -> Result<T, E> {
    operation(&VerifiedGeneration { identity: None })
}

/// One entry in the calling thread's list, removed on drop. It is neither
/// `Send` nor nameable outside this module, so it is always dropped on the
/// thread that set it, at the end of the function body that created it.
struct Marker {
    identity: u64,
    _on_this_thread: std::marker::PhantomData<*const ()>,
}

impl Marker {
    fn set(identity: u64) -> Self {
        ENCLOSING_OPERATIONS.with(|operations| operations.borrow_mut().push(identity));
        Self {
            identity,
            _on_this_thread: std::marker::PhantomData,
        }
    }
}

impl Drop for Marker {
    fn drop(&mut self) {
        ENCLOSING_OPERATIONS.with(|operations| {
            let removed = operations.borrow_mut().pop();
            debug_assert_eq!(removed, Some(self.identity));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn counting(count: &Cell<usize>) -> impl Fn() -> Result<(), String> + '_ {
        move || {
            count.set(count.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn an_operation_verifies_once_before_and_once_after() {
        let verifications = Cell::new(0);
        let seen_inside = run_verified(7, counting(&verifications), |token| {
            assert_eq!(token.identity(), Some(7));
            assert!(is_covered(7));
            Ok::<_, String>(verifications.get())
        })
        .unwrap();
        assert_eq!(seen_inside, 1, "the before-verification ran first");
        assert_eq!(verifications.get(), 2);
        assert!(!is_covered(7), "the marker ends with the operation");
    }

    #[test]
    fn a_nested_operation_for_the_same_generation_does_not_verify_again() {
        let verifications = Cell::new(0);
        run_verified(7, counting(&verifications), |_| {
            run_verified(7, counting(&verifications), |_| {
                assert!(is_covered(7));
                Ok::<_, String>(())
            })?;
            assert!(is_covered(7), "the inner operation leaves the outer marker");
            Ok(())
        })
        .unwrap();
        assert_eq!(verifications.get(), 2);
        assert!(!is_covered(7));
    }

    #[test]
    fn a_nested_operation_for_another_generation_verifies_for_itself() {
        let outer = Cell::new(0);
        let inner = Cell::new(0);
        run_verified(7, counting(&outer), |_| {
            assert!(!is_covered(8));
            run_verified(8, counting(&inner), |_| {
                assert!(is_covered(7) && is_covered(8));
                Ok::<_, String>(())
            })?;
            assert!(!is_covered(8));
            Ok(())
        })
        .unwrap();
        assert_eq!(outer.get(), 2);
        assert_eq!(inner.get(), 2);
    }

    #[test]
    fn a_failed_before_verification_never_runs_the_operation() {
        let ran = Cell::new(false);
        let error = run_verified(
            7,
            || Err("changed".to_string()),
            |_| {
                ran.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error, "changed");
        assert!(!ran.get());
        assert!(!is_covered(7));
    }

    #[test]
    fn a_failed_after_verification_refuses_the_result() {
        let verifications = Cell::new(0);
        let result = run_verified(
            7,
            || {
                verifications.set(verifications.get() + 1);
                if verifications.get() == 1 {
                    Ok(())
                } else {
                    Err("changed during the operation".to_string())
                }
            },
            |_| Ok("value"),
        );
        assert_eq!(result, Err("changed during the operation".to_string()));
        assert_eq!(verifications.get(), 2);
    }

    #[test]
    fn a_failed_or_unwinding_operation_removes_its_marker() {
        let verifications = Cell::new(0);
        let error = run_verified(7, counting(&verifications), |_| {
            Err::<(), _>("operation failed".to_string())
        })
        .unwrap_err();
        assert_eq!(error, "operation failed");
        assert_eq!(
            verifications.get(),
            1,
            "a failed operation is not re-verified"
        );
        assert!(!is_covered(7));

        let unwound = std::panic::catch_unwind(|| {
            let _ = run_verified(
                7,
                || Ok::<(), String>(()),
                |_| -> Result<(), String> { panic!("operation panicked") },
            );
        });
        assert!(unwound.is_err());
        assert!(!is_covered(7));
    }

    #[test]
    fn a_panicking_or_failing_operation_leaves_the_next_call_to_verify_for_itself() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let verifications = Cell::new(0);

        // The operation's closure panics while the marker is set.
        let unwound = catch_unwind(AssertUnwindSafe(|| {
            let _ = run_verified(7, counting(&verifications), |_| -> Result<(), String> {
                assert!(is_covered(7));
                panic!("operation panicked")
            });
        }));
        assert!(unwound.is_err());
        assert_eq!(verifications.get(), 1, "only the before-verification ran");
        assert!(!is_covered(7), "the unwind removed the marker");

        // The same thread, the same generation, no enclosing operation: the
        // next guarded call verifies before and after, skipping nothing.
        run_verified(7, counting(&verifications), |_| Ok::<_, String>(())).unwrap();
        assert_eq!(verifications.get(), 3);

        // The operation's closure returns an error early.
        let failed = run_verified(7, counting(&verifications), |_| {
            assert!(is_covered(7));
            Err::<(), _>("operation failed".to_string())
        });
        assert_eq!(failed, Err("operation failed".to_string()));
        assert_eq!(verifications.get(), 4, "only the before-verification ran");
        assert!(!is_covered(7), "the early return removed the marker");

        run_verified(7, counting(&verifications), |_| Ok::<_, String>(())).unwrap();
        assert_eq!(verifications.get(), 6);
    }

    #[test]
    fn a_panicking_worker_hand_off_leaves_that_worker_uncovered() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let verifications = Cell::new(0);
        run_verified(7, counting(&verifications), |token| {
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        // The worker's covered closure panics.
                        let unwound = catch_unwind(AssertUnwindSafe(|| {
                            token.cover_current_thread::<()>(|| {
                                assert!(is_covered(7));
                                panic!("worker panicked")
                            })
                        }));
                        assert!(unwound.is_err());
                        assert!(!is_covered(7), "the unwind removed the worker's marker");

                        // The same worker thread afterwards verifies for
                        // itself, before and after.
                        let on_worker = Cell::new(0);
                        run_verified(7, counting(&on_worker), |_| Ok::<_, String>(())).unwrap();
                        assert_eq!(on_worker.get(), 2);
                    })
                    .join()
                    .unwrap();
            });
            // The operation's own thread is still covered.
            assert!(is_covered(7));
            Ok::<_, String>(())
        })
        .unwrap();
        assert_eq!(verifications.get(), 2);
    }

    #[test]
    fn another_thread_is_not_covered_unless_handed_the_token() {
        let verifications = Cell::new(0);
        run_verified(7, counting(&verifications), |token| {
            std::thread::scope(|scope| {
                scope
                    .spawn(|| assert!(!is_covered(7), "a new thread starts uncovered"))
                    .join()
                    .unwrap();
                scope
                    .spawn(|| {
                        token.cover_current_thread(|| assert!(is_covered(7)));
                        assert!(!is_covered(7), "cover ends with its closure");
                    })
                    .join()
                    .unwrap();
            });
            Ok::<_, String>(())
        })
        .unwrap();
    }

    #[test]
    fn an_unregistered_runtime_covers_nothing() {
        run_unregistered(|token| {
            assert_eq!(token.identity(), None);
            token.cover_current_thread(|| assert!(!is_covered(0)));
            Ok::<_, String>(())
        })
        .unwrap();
    }
}
