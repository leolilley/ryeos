//! Layout-preference persistence.
//!
//! The core projects an opaque scope key and an opaque export of the current
//! arrangement. This module decides when an export is written to the platform
//! [`Store`]: only after a deliberate accepted edit, debounced, and flushed
//! when the scope changes or the host closes. A different admitted baseline is
//! never an edit, so a cached arrangement stays a restore candidate until a
//! later observed change replaces it. The rules are those of the browser
//! host's persistence, so both hosts write the same bytes at the same moments.
//!
//! Time is passed in, never read: the host's loop supplies `now_ms`, which
//! keeps the behaviour deterministic under a virtual clock.

use crate::platform::Store;

/// How long an accepted edit waits before it is written.
pub const LAYOUT_DEBOUNCE_MS: u64 = 400;

/// One scope's persistence state.
struct Scope {
    key: String,
    last_observed: String,
    last_persisted: Option<String>,
    pending: Option<String>,
    due_ms: Option<u64>,
}

/// Scoped, debounced persistence of the opaque layout export.
pub struct LayoutPersistence {
    delay_ms: u64,
    key: Option<String>,
    active: Option<Scope>,
    errors: Vec<String>,
    delays: Vec<u64>,
}

impl Default for LayoutPersistence {
    fn default() -> Self {
        Self::new(LAYOUT_DEBOUNCE_MS)
    }
}

impl LayoutPersistence {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            key: None,
            active: None,
            errors: Vec::new(),
            delays: Vec::new(),
        }
    }

    /// Errors reported since the last call (an export or a write that
    /// failed). A failed write keeps its value pending for the next flush.
    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    /// Debounce delay of every write queued so far, in queue order.
    pub fn queued_delays_ms(&self) -> &[u64] {
        &self.delays
    }

    /// Writes waiting for their debounce.
    pub fn pending_timers(&self) -> usize {
        usize::from(
            self.active
                .as_ref()
                .is_some_and(|scope| scope.due_ms.is_some()),
        )
    }

    /// Observe the core's accepted state. `key` is the scope the core
    /// currently projects (`None` when it projects none); `current` exports
    /// the arrangement. Returns whether a write was queued.
    pub fn observe(
        &mut self,
        now_ms: u64,
        key: Option<String>,
        current: impl FnOnce() -> Result<String, String>,
        store: &impl Store,
    ) -> bool {
        if key != self.key || (key.is_some() && self.active.is_none()) {
            // Flush what the predecessor scope captured; never export for it
            // again once the core selected another scope.
            self.flush(store);
            self.active = None;
            self.key = key.clone();
            let Some(key) = key else {
                return false;
            };
            return match current() {
                Ok(baseline) => {
                    self.active = Some(Scope {
                        last_persisted: store.get(&key),
                        key,
                        last_observed: baseline,
                        pending: None,
                        due_ms: None,
                    });
                    false
                }
                Err(message) => {
                    self.errors.push(message);
                    false
                }
            };
        }
        let Some(scope) = self.active.as_mut() else {
            return false;
        };
        let value = match current() {
            Ok(value) => value,
            Err(message) => {
                self.errors.push(message);
                return false;
            }
        };
        if value == scope.last_observed {
            return false;
        }
        scope.last_observed = value.clone();
        if scope.last_persisted.as_deref() == Some(value.as_str()) {
            scope.pending = None;
            scope.due_ms = None;
            return false;
        }
        scope.pending = Some(value);
        scope.due_ms = Some(now_ms.saturating_add(self.delay_ms));
        self.delays.push(self.delay_ms);
        true
    }

    /// Write the pending value now. Returns whether a write succeeded.
    pub fn flush(&mut self, store: &impl Store) -> bool {
        let Some(scope) = self.active.as_mut() else {
            return false;
        };
        scope.due_ms = None;
        let Some(value) = scope.pending.take() else {
            return false;
        };
        match store.set(&scope.key, &value) {
            Ok(()) => {
                scope.last_persisted = Some(value);
                true
            }
            Err(error) => {
                scope.pending = Some(value);
                self.errors.push(error.to_string());
                false
            }
        }
    }

    /// Write the pending value if its debounce has elapsed.
    pub fn write_due(&mut self, now_ms: u64, store: &impl Store) -> bool {
        let due = self
            .active
            .as_ref()
            .and_then(|scope| scope.due_ms)
            .is_some_and(|due| now_ms >= due);
        due && self.flush(store)
    }
}
