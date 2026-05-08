//! Write barrier for GC maintenance mode.
//!
//! The daemon's CAS write paths acquire a write permit before writing.
//! When GC is triggered, the daemon quiesces (blocks new permits),
//! waits for active writers to drain, then runs GC with all writes paused.
//!
//! If the daemon is not running, `ryeos gc` runs directly — no quiesce needed.

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::sync::Notify;

/// Write barrier states.
const NORMAL: u8 = 0;
const QUIESCING: u8 = 1;
const QUIESCED: u8 = 2;

/// RAII guard representing an active write permit.
///
/// While held, GC cannot quiesce. Released on drop.
pub struct WritePermit {
    barrier: std::sync::Arc<WriteBarrierInner>,
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        let prior = self.barrier.active_writers.fetch_sub(1, Ordering::SeqCst);
        tracing::trace!(active_writers_after = prior - 1, "write permit released");
        // Wake anyone waiting for writers to drain
        self.barrier.notify.notify_one();
    }
}

struct WriteBarrierInner {
    state: AtomicU8,
    active_writers: AtomicU32,
    notify: tokio::sync::Notify,
}

/// Global write barrier for CAS writes.
///
/// Thread-safe. Cloneable (all clones share the same inner state).
#[derive(Clone)]
pub struct WriteBarrier {
    inner: std::sync::Arc<WriteBarrierInner>,
}

impl Default for WriteBarrier {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteBarrier {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(WriteBarrierInner {
                state: AtomicU8::new(NORMAL),
                active_writers: AtomicU32::new(0),
                notify: Notify::new(),
            }),
        }
    }

    /// Try to acquire a write permit. Fails if quiescing or quiesced.
    ///
    /// Returns a guard that releases the permit on drop.
    pub fn try_acquire(&self) -> Result<WritePermit> {
        let state = self.inner.state.load(Ordering::SeqCst);
        match state {
            NORMAL => {
                self.inner.active_writers.fetch_add(1, Ordering::SeqCst);
                // Re-check: state may have changed between load and increment
                let state_now = self.inner.state.load(Ordering::SeqCst);
                if state_now != NORMAL {
                    self.inner.active_writers.fetch_sub(1, Ordering::SeqCst);
                    bail!("write barrier is quiescing/quiesced, cannot acquire write permit");
                }
                tracing::trace!("write permit acquired");
                Ok(WritePermit {
                    barrier: self.inner.clone(),
                })
            }
            QUIESCING | QUIESCED => {
                tracing::trace!(state = state, "write permit denied — barrier quiescing/quiesced");
                bail!("write barrier is quiescing/quiesced, cannot acquire write permit");
            }
            _ => bail!("write barrier in unknown state: {state}"),
        }
    }

    /// Begin quiesce: set state to QUIESCING, block new permits, wait for
    /// active writers to drain. Returns error on timeout.
    pub async fn quiesce(&self, timeout: Duration) -> Result<()> {
        // Set to quiescing — new try_acquire calls will fail
        self.inner.state.store(QUIESCING, Ordering::SeqCst);

        // Fast path: already no writers
        if self.inner.active_writers.load(Ordering::SeqCst) == 0 {
            self.inner.state.store(QUIESCED, Ordering::SeqCst);
            tracing::info!("write barrier quiesced (no active writers)");
            return Ok(());
        }

        let deadline = std::time::Instant::now() + timeout;

        loop {
            let writers = self.inner.active_writers.load(Ordering::SeqCst);
            if writers == 0 {
                self.inner.state.store(QUIESCED, Ordering::SeqCst);
                tracing::info!("write barrier quiesced");
                return Ok(());
            }

            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                // Timeout — resume normal operation
                self.inner.state.store(NORMAL, Ordering::SeqCst);
                bail!(
                    "write barrier quiesce timed out after {:?} ({} writers still active)",
                    timeout, writers
                );
            }

            // Wait for a writer to finish (with timeout granularity)
            let wait_dur = remaining.min(Duration::from_millis(100));
            let _ = tokio::time::timeout(wait_dur, self.inner.notify.notified()).await;
        }
    }

    /// Resume normal operation after GC.
    pub fn resume(&self) {
        self.inner.state.store(NORMAL, Ordering::SeqCst);
        tracing::info!("write barrier resumed normal operation");
    }

    /// Check if currently quiesced (test-only diagnostic).
    #[cfg(test)]
    pub fn is_quiesced(&self) -> bool {
        self.inner.state.load(Ordering::SeqCst) == QUIESCED
    }

    /// Get the number of active writers (test-only diagnostic).
    #[cfg(test)]
    pub fn active_writers(&self) -> u32 {
        self.inner.active_writers.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_allows_writes() {
        let barrier = WriteBarrier::new();
        let _permit = barrier.try_acquire().unwrap();
        assert_eq!(barrier.active_writers(), 1);
    }

    #[test]
    fn quiesced_blocks_writes() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let barrier = WriteBarrier::new();

            // Quiesce
            barrier.quiesce(Duration::from_millis(100)).await.unwrap();

            // Writes should fail
            assert!(barrier.try_acquire().is_err());

            barrier.resume();
        });
    }

    #[test]
    fn quiesce_timeout_fails() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let barrier = WriteBarrier::new();

            // Acquire a permit
            let _permit = barrier.try_acquire().unwrap();

            // Quiesce with very short timeout should fail
            let result = barrier.quiesce(Duration::from_millis(10)).await;
            assert!(result.is_err());

            // Should be back to normal after timeout
            let _permit2 = barrier.try_acquire().unwrap();
        });
    }

    #[test]
    fn resume_allows_writes_after_quiesce() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let barrier = WriteBarrier::new();

            barrier.quiesce(Duration::from_millis(100)).await.unwrap();
            assert!(barrier.try_acquire().is_err());

            barrier.resume();
            let _permit = barrier.try_acquire().unwrap();
        });
    }

    #[test]
    fn concurrent_writers_counted() {
        let barrier = WriteBarrier::new();
        let p1 = barrier.try_acquire().unwrap();
        let p2 = barrier.try_acquire().unwrap();
        let p3 = barrier.try_acquire().unwrap();
        assert_eq!(barrier.active_writers(), 3);
        drop(p1);
        assert_eq!(barrier.active_writers(), 2);
        drop(p2);
        drop(p3);
        assert_eq!(barrier.active_writers(), 0);
    }

    #[test]
    fn quiesce_drains_all_writers() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let barrier = WriteBarrier::new();

            // Acquire a permit
            let p1 = barrier.try_acquire().unwrap();
            assert_eq!(barrier.active_writers(), 1);

            // Start quiesce in background
            let barrier_clone = barrier.clone();
            let quiesce_handle = tokio::spawn(async move {
                barrier_clone.quiesce(Duration::from_secs(5)).await
            });

            // Give it a moment to start waiting
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Should not be quiesced yet (p1 still held)
            assert!(!barrier.is_quiesced());

            // Drop p1 — should allow quiesce to complete
            drop(p1);

            // Wait for quiesce (polling granularity is 100ms)
            let result = quiesce_handle.await.unwrap();
            assert!(result.is_ok());
            assert!(barrier.is_quiesced());

            barrier.resume();
        });
    }

    #[test]
    fn write_permit_prevents_concurrent_gc() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let barrier = WriteBarrier::new();

            // Acquire a write permit
            let _permit = barrier.try_acquire().unwrap();

            // GC quiesce should timeout since permit is held
            let result = barrier.quiesce(Duration::from_millis(50)).await;
            assert!(result.is_err());

            // After timeout, barrier should be back to normal
            assert!(!barrier.is_quiesced());
            let _permit2 = barrier.try_acquire().unwrap();
        });
    }
}
