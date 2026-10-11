//! Deterministic interruption points for crash-qualification tests.
//!
//! The hook point is compiled into every build and is inert until test code
//! installs a hook: an unset `OnceLock` costs one atomic load per secure
//! publication. Only the installer is gated behind `test-support`, so a
//! production build has no way to arm it, and Lillux never consults the
//! environment to decide whether a hook is armed.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::TempPurpose;

/// Which secure publication a hook interrupts: the writer's purpose token,
/// the exact target child name, and the 1-based occurrence among matching
/// publications in this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtomicCreateHookSelector {
    pub purpose: TempPurpose,
    pub target: OsString,
    pub occurrence: u64,
}

/// What happens when the selected publication reaches the point between its
/// complete temporary write and its rename into place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicCreateHookAction {
    /// Terminate the process immediately, leaving the temporary as debris.
    Abort,
    /// Fail the publication as an ordinary error. `atomic_create_regular`
    /// returns its unpublished temporary to the caller through its creation
    /// failure; the other writers unlink it themselves. The hook is process
    /// global, so tests that arm it must not run concurrently with each other.
    Fail,
}

struct ArmedHook {
    selector: AtomicCreateHookSelector,
    action: AtomicCreateHookAction,
    matched: u64,
}

static ATOMIC_CREATE_HOOK: OnceLock<Mutex<Option<ArmedHook>>> = OnceLock::new();

/// Arm one hook, replacing any hook armed earlier in this process. The hook
/// disarms itself when it fires.
#[cfg(any(test, feature = "test-support"))]
pub fn set_atomic_create_hook(selector: AtomicCreateHookSelector, action: AtomicCreateHookAction) {
    let slot = ATOMIC_CREATE_HOOK.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(ArmedHook {
        selector,
        action,
        matched: 0,
    });
}

/// Disarm any hook armed in this process.
#[cfg(any(test, feature = "test-support"))]
pub fn clear_atomic_create_hook() {
    if let Some(slot) = ATOMIC_CREATE_HOOK.get() {
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Called by every secure temporary writer after the temporary holds its
/// complete bytes and before the rename that publishes it.
pub(crate) fn atomic_create_hook_point(purpose: TempPurpose, target: &OsStr) -> anyhow::Result<()> {
    let Some(slot) = ATOMIC_CREATE_HOOK.get() else {
        return Ok(());
    };
    let mut armed = slot.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(hook) = armed.as_mut() else {
        return Ok(());
    };
    if hook.selector.purpose != purpose || hook.selector.target != target {
        return Ok(());
    }
    hook.matched = hook.matched.saturating_add(1);
    if hook.matched != hook.selector.occurrence {
        return Ok(());
    }
    let action = hook.action;
    *armed = None;
    drop(armed);
    match action {
        AtomicCreateHookAction::Abort => std::process::abort(),
        AtomicCreateHookAction::Fail => anyhow::bail!(
            "secure publication interrupted by an armed test hook before rename"
        ),
    }
}
