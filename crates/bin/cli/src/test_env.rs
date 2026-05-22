//! Shared test-only mutex for USER_SPACE env-var mutations.
//!
//! Multiple modules (`project_resolve`, `dispatcher`) mutate the
//! process-wide `USER_SPACE` environment variable in their tests. To
//! prevent races when tests run in parallel inside the same binary,
//! they all acquire this single shared mutex via `test_env::lock()`.
#![cfg(test)]

use std::sync::{Mutex, MutexGuard};

static TEST_ENV_MUTEX: Mutex<()> = Mutex::new(());

pub fn lock() -> MutexGuard<'static, ()> {
    TEST_ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner())
}
