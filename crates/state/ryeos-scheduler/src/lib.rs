//! ryeos-scheduler — internal timer module for ryeosd.
//!
//! Owns exactly one problem: *when to fire*. Schedule specs are signed
//! node-config YAML at `.ai/node/schedules/`. Fire history is append-only
//! JSONL at `.ai/state/schedules/`. The projection DB (`scheduler.sqlite3`)
//! is rebuildable from CAS.
//!
//! The scheduler receives everything it needs from the daemon through the
//! [`SchedulerContext`] trait. The daemon's `AppState` implements this trait.
//! The scheduler never imports `ryeosd`.

pub mod crontab;
pub mod db;
pub mod misfire;
pub mod overlap;
pub mod projection;
pub mod reconcile;
pub mod timer;
pub mod types;

// Re-export primary types
pub use types::{FireRecord, PendingFire, ReloadSignal, ScheduleSpecRecord};

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use ryeos_engine::trust::TrustStore;

/// Trait that the daemon's `AppState` implements.
///
/// The scheduler defines what it needs; the daemon satisfies the interface.
/// Uses generic parameters (not `dyn`) for zero-cost abstraction.
pub trait SchedulerContext: Send + Sync + 'static {
    /// The system space directory (`.ai` lives here).
    fn system_space_dir(&self) -> &Path;

    /// The scheduler projection database.
    fn scheduler_db(&self) -> Arc<db::SchedulerDb>;

    /// The trust store for schedule signature verification.
    fn trust_store(&self) -> &TrustStore;

    /// Check a thread's status. Returns `None` if thread doesn't exist.
    fn get_thread_status(&self, thread_id: &str) -> Result<Option<String>>;

    /// Submit a cancel command for a thread.
    fn submit_cancel(&self, thread_id: &str) -> Result<()>;

    /// Dispatch a scheduled item for execution.
    ///
    /// The daemon constructs the `DispatchRequest` and `ExecutionContext`,
    /// then calls `dispatch::dispatch()`. The scheduler doesn't know about
    /// those types.
    fn dispatch_scheduled_item(
        &self,
        spec: &ScheduleSpecRecord,
        fire_id: &str,
        thread_id: &str,
        scheduled_at: i64,
        trigger_reason: &str,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}

// Blanket impl: Arc<T> where T: SchedulerContext delegates to T.
// Needed because timer::run takes Arc<Ctx> and passes &ctx to functions
// that expect &Ctx.
impl<T: SchedulerContext> SchedulerContext for Arc<T> {
    fn system_space_dir(&self) -> &Path {
        (**self).system_space_dir()
    }

    fn scheduler_db(&self) -> Arc<db::SchedulerDb> {
        (**self).scheduler_db()
    }

    fn trust_store(&self) -> &TrustStore {
        (**self).trust_store()
    }

    fn get_thread_status(&self, thread_id: &str) -> Result<Option<String>> {
        (**self).get_thread_status(thread_id)
    }

    fn submit_cancel(&self, thread_id: &str) -> Result<()> {
        (**self).submit_cancel(thread_id)
    }

    async fn dispatch_scheduled_item(
        &self,
        spec: &ScheduleSpecRecord,
        fire_id: &str,
        thread_id: &str,
        scheduled_at: i64,
        trigger_reason: &str,
    ) -> Result<()> {
        (**self)
            .dispatch_scheduled_item(spec, fire_id, thread_id, scheduled_at, trigger_reason)
            .await
    }
}
