//! Shared test-only mutex for process-global test state.
//!
//! Tests that temporarily change the process working directory acquire this
//! mutex so parallel tests in the same binary cannot observe that transition.
#![cfg(test)]

use std::sync::{Mutex, MutexGuard};

static TEST_ENV_MUTEX: Mutex<()> = Mutex::new(());

pub fn lock() -> MutexGuard<'static, ()> {
    TEST_ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner())
}
