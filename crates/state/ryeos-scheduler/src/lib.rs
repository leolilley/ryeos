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
pub mod planning;
pub mod projection;
pub mod reconcile;
pub mod result_outcome;
pub mod timer;
pub mod types;

// Re-export primary types
pub use result_outcome::{
    classify_result_payload, completed_fire_outcome, fire_outcome_for_terminal,
    fire_status_for_thread_status, ThreadResultOutcome,
};
pub use types::{FireRecord, PendingFire, ReloadSignal, ScheduleSpecRecord};

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use ryeos_engine::trust::TrustStore;
use tokio::sync::RwLock;

/// Trait that the daemon's `AppState` implements.
///
/// The scheduler defines what it needs; the daemon satisfies the interface.
/// Uses generic parameters (not `dyn`) for zero-cost abstraction.
pub trait SchedulerContext: Send + Sync + 'static {
    /// The app rootectory (`.ai` lives here).
    fn app_root(&self) -> &Path;

    /// The scheduler projection database.
    fn scheduler_db(&self) -> Arc<db::SchedulerDb>;

    /// Runtime gate that prevents dispatch while schedule/project deploy
    /// mutations are in progress.
    fn scheduler_runtime_gate(&self) -> Arc<RwLock<()>>;

    /// The trust store for schedule signature verification.
    fn trust_store(&self) -> &TrustStore;

    /// Check a thread's status. Returns `None` if thread doesn't exist.
    fn get_thread_status(&self, thread_id: &str) -> Result<Option<String>>;

    /// Classify a terminal thread's captured result payload, if available.
    ///
    /// Scheduler repair/reconcile uses this to avoid reporting a completed
    /// runtime as a successful fire when the tool result follows the
    /// `{ "success": false }` convention. Implementations should return
    /// `Ok(None)` when no result row exists.
    fn get_thread_result_outcome(&self, _thread_id: &str) -> Result<Option<ThreadResultOutcome>> {
        Ok(None)
    }

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
    fn app_root(&self) -> &Path {
        (**self).app_root()
    }

    fn scheduler_db(&self) -> Arc<db::SchedulerDb> {
        (**self).scheduler_db()
    }

    fn scheduler_runtime_gate(&self) -> Arc<RwLock<()>> {
        (**self).scheduler_runtime_gate()
    }

    fn trust_store(&self) -> &TrustStore {
        (**self).trust_store()
    }

    fn get_thread_status(&self, thread_id: &str) -> Result<Option<String>> {
        (**self).get_thread_status(thread_id)
    }

    fn get_thread_result_outcome(&self, thread_id: &str) -> Result<Option<ThreadResultOutcome>> {
        (**self).get_thread_result_outcome(thread_id)
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
