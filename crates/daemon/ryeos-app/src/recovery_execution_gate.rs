//! Process-local release gate for work durably admitted during daemon startup.
//!
//! Recovery preparation may acquire persistent launch ownership before the node
//! is Ready, but the owned task must not execute until callback publication and
//! the final projection readiness snapshot are complete. Live callers outside
//! daemon startup see an unarmed gate and proceed immediately.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::sync::Notify;

const UNARMED: u8 = 0;
const CLOSED: u8 = 1;
const OPEN: u8 = 2;
const CANCELLED: u8 = 3;

struct RecoveryExecutionGateInner {
    state: AtomicU8,
    notify: Notify,
}

fn global() -> &'static Arc<RecoveryExecutionGateInner> {
    static GATE: OnceLock<Arc<RecoveryExecutionGateInner>> = OnceLock::new();
    GATE.get_or_init(|| {
        Arc::new(RecoveryExecutionGateInner {
            state: AtomicU8::new(UNARMED),
            notify: Notify::new(),
        })
    })
}

/// Owned authority to release recovery execution after readiness publication.
#[derive(Clone)]
pub struct RecoveryExecutionRelease {
    inner: Arc<RecoveryExecutionGateInner>,
}

impl RecoveryExecutionRelease {
    pub fn open(&self) {
        self.inner.state.store(OPEN, Ordering::Release);
        self.inner.notify.notify_waiters();
    }

    /// Cancel an unreleased startup epoch. Waiting tasks wake and abandon
    /// their in-memory launch ownership; durable recovery records remain for
    /// the next daemon boot. Calling this after release is a harmless no-op.
    pub fn cancel(&self) {
        if self
            .inner
            .state
            .compare_exchange(CLOSED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.inner.notify.notify_waiters();
        }
    }
}

/// Arm the process-wide startup gate before any recovered work is prepared.
///
/// A daemon process has one startup. Re-arming an already armed process is a
/// programming error rather than a second recovery epoch with ambiguous task
/// ownership.
pub fn arm() -> RecoveryExecutionRelease {
    let inner = global().clone();
    inner
        .state
        .compare_exchange(UNARMED, CLOSED, Ordering::AcqRel, Ordering::Acquire)
        .expect("startup recovery execution gate may only be armed once");
    RecoveryExecutionRelease { inner }
}

/// Wait only when daemon startup has explicitly armed and not released the
/// gate. Returns false when startup was cancelled, telling the detached owner
/// to abandon execution and release its in-memory claim. The notification is
/// registered before inspecting state, preventing a transition between the
/// check and the await from being lost.
pub async fn wait_if_armed() -> bool {
    let inner = global();
    loop {
        let notified = inner.notify.notified();
        match inner.state.load(Ordering::Acquire) {
            CLOSED => notified.await,
            UNARMED | OPEN => return true,
            CANCELLED => return false,
            state => panic!("unknown recovery execution gate state: {state}"),
        }
    }
}
