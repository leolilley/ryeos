//! Host threads and deadline-bound condition waits.
//!
//! Callers own work selection, cancellation policy and durable lifecycle state.
//! This module owns local host execution and waiting. A joined task proves only
//! that its Rust closure terminated; it says nothing about child processes,
//! filesystem writers or durable execution completion. No task is forcibly
//! cancelled and no join is implicitly performed during destruction.

use std::sync::{Condvar, LockResult, MutexGuard, PoisonError};

use crate::time::MonotonicDeadline;

/// One joinable thread in the current process. Dropping the handle detaches it,
/// as does `detach`; neither operation stops the task. Callers with cleanup
/// obligations must interrupt their I/O or signal their protocol before joining.
#[must_use = "retain and join the host task, or explicitly detach it"]
pub struct HostTask<T> {
    thread: std::thread::JoinHandle<T>,
}

impl<T> HostTask<T> {
    /// Join only after observing task termination within the caller's deadline.
    /// Expiry returns the original owner; it neither cancels nor detaches work.
    pub fn join_until(self, deadline: MonotonicDeadline) -> Result<std::thread::Result<T>, Self> {
        while !self.is_finished() {
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err(self);
            }
            crate::time::sleep(remaining.min(crate::time::Duration::from_millis(1)));
        }
        Ok(self.join())
    }

    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Wait for this closure to terminate. This has no timeout or cancellation
    /// guarantee; a caller must arrange for blocked work to become runnable.
    pub fn join(self) -> std::thread::Result<T> {
        self.thread.join()
    }

    /// Release join ownership while the closure may continue in this process.
    pub fn detach(self) {
        drop(self);
    }
}

pub fn spawn_host_task<T, F>(name: &str, task: F) -> std::io::Result<HostTask<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "host task name must be nonempty, bounded and free of control characters",
        ));
    }
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(task)
        .map(|thread| HostTask { thread })
}

/// Condition notification with Lillux-owned host timing. The protected state
/// and predicate belong to the caller. Reuse the same mutex for all waits on
/// one condition; notification is a wake hint, never application completion.
#[derive(Default)]
pub struct HostCondition {
    condition: Condvar,
}

impl HostCondition {
    pub fn notify_all(&self) {
        self.condition.notify_all();
    }

    /// Release the mutex and wait for a wake or the remaining deadline window.
    /// Spurious wakes are permitted. Mutex reacquisition can outlast the time
    /// window; callers must keep critical sections bounded and recheck state.
    pub fn wait_until<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        deadline: MonotonicDeadline,
    ) -> LockResult<MutexGuard<'a, T>> {
        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return Ok(guard);
        }
        self.condition
            .wait_timeout(guard, remaining)
            .map(|(guard, _)| guard)
            .map_err(|error| PoisonError::new(error.into_inner().0))
    }

    /// Wait while the predicate holds, without renewing the deadline on wakes.
    /// Return the locked state on expiry too: only the caller can decide
    /// whether the predicate's final state meets its completion requirement.
    pub fn wait_while_until<'a, T, F>(
        &self,
        mut guard: MutexGuard<'a, T>,
        deadline: MonotonicDeadline,
        mut predicate: F,
    ) -> LockResult<MutexGuard<'a, T>>
    where
        F: FnMut(&mut T) -> bool,
    {
        while predicate(&mut guard) && !deadline.has_elapsed() {
            guard = self.wait_until(guard, deadline)?;
        }
        Ok(guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{Duration, MonotonicTimer};
    use std::sync::{Arc, Mutex, mpsc};

    #[test]
    fn deadline_join_retains_unfinished_owner_and_returns_exact_result() {
        let (release, wait) = mpsc::channel();
        let task = spawn_host_task("deadline-join", move || {
            wait.recv().unwrap();
            42
        })
        .unwrap();
        let task = match task.join_until(MonotonicDeadline::after(Duration::ZERO)) {
            Err(task) => task,
            Ok(_) => panic!("blocked task reported completion"),
        };
        release.send(()).unwrap();
        match task.join_until(MonotonicDeadline::after(Duration::from_secs(2))) {
            Ok(Ok(value)) => assert_eq!(value, 42),
            _ => panic!("released task did not settle"),
        }
    }

    #[test]
    fn deadline_join_preserves_panic_as_failure() {
        let task = spawn_host_task("deadline-panic", || panic!("fixture failure")).unwrap();
        assert!(matches!(
            task.join_until(MonotonicDeadline::after(Duration::from_secs(2))),
            Ok(Err(_))
        ));
    }

    #[test]
    fn detach_does_not_cancel_the_task_and_join_reports_panics() {
        let (release, wait) = mpsc::channel();
        let (completed, result) = mpsc::channel();
        let task = spawn_host_task("detached-test", move || {
            wait.recv().unwrap();
            completed.send(7).unwrap();
        })
        .unwrap();
        assert!(!task.is_finished());
        task.detach();
        release.send(()).unwrap();
        assert_eq!(result.recv_timeout(Duration::from_secs(2)).unwrap(), 7);
        assert!(
            spawn_host_task("panicking-test", || panic!("task failure"))
                .unwrap()
                .join()
                .is_err()
        );
    }

    #[test]
    fn notifications_do_not_renew_the_wait_deadline() {
        let condition = Arc::new(HostCondition::default());
        let state = Arc::new(Mutex::new(false));
        let notifier_condition = Arc::clone(&condition);
        let notifier_state = Arc::clone(&state);
        let notifier = spawn_host_task("condition-wakes", move || {
            let bound = MonotonicDeadline::after(Duration::from_secs(2));
            while !bound.has_elapsed() && !*notifier_state.lock().unwrap() {
                notifier_condition.notify_all();
                crate::time::sleep(Duration::from_millis(1));
            }
        })
        .unwrap();
        let timer = MonotonicTimer::start();
        let deadline = MonotonicDeadline::after(Duration::from_millis(30));
        let mut guard = condition
            .wait_while_until(state.lock().unwrap(), deadline, |done| !*done)
            .unwrap();
        assert!(!*guard, "notification must not fabricate completion");
        assert!(deadline.has_elapsed());
        let elapsed = timer.elapsed();
        *guard = true;
        drop(guard);
        notifier.join().unwrap();
        assert!(
            elapsed < Duration::from_secs(1),
            "wake renewed the deadline"
        );
    }

    #[test]
    fn expired_wait_preserves_state_and_successful_wait_reacquires_it() {
        let condition = Arc::new(HostCondition::default());
        let state = Arc::new(Mutex::new(false));
        let deadline = MonotonicDeadline::after(Duration::ZERO);
        let guard = condition
            .wait_while_until(state.lock().unwrap(), deadline, |done| !*done)
            .unwrap();
        assert!(!*guard);
        drop(guard);
        let writer_state = Arc::clone(&state);
        let writer_condition = Arc::clone(&condition);
        let writer = spawn_host_task("condition-writer", move || {
            *writer_state.lock().unwrap() = true;
            writer_condition.notify_all();
        })
        .unwrap();
        let guard = condition
            .wait_while_until(
                state.lock().unwrap(),
                MonotonicDeadline::after(Duration::from_secs(2)),
                |done| !*done,
            )
            .unwrap();
        assert!(*guard);
        drop(guard);
        writer.join().unwrap();
    }
}
