//! Daemon-side `SchedulerContext` implementation for `AppState`.
//!
//! The trait lives in `ryeos_scheduler` and AppState lives in `ryeos_app`.
//! Rust orphan rules: only crates that define one of the two can implement
//! the trait. Both `AppState` and `SchedulerContext` are external to this
//! crate, but the `dispatch_scheduled_item` method calls `ryeos_executor::dispatch`
//! and `ryeos_executor::executor` (both daemon/executor-private) — so we use the
//! orphan-friendly approach of expressing the impl through a newtype:
//! daemon code wraps `AppState` in a tiny adapter and the impl is on that
//! wrapper.

use std::sync::Arc;

use anyhow::Result;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_scheduler::db::SchedulerDb;
use ryeos_scheduler::types::ScheduleSpecRecord;
use ryeos_scheduler::{SchedulerContext, ThreadResultOutcome};

/// Newtype wrapper around `AppState` so we can implement
/// `SchedulerContext` from `ryeos_scheduler` for state defined in
/// `ryeos_app` — Rust orphan rules require either the trait or the type
/// to be local to this crate.
#[derive(Clone)]
pub struct AppSchedulerContext(pub Arc<AppState>);

impl SchedulerContext for AppSchedulerContext {
    fn app_root(&self) -> &std::path::Path {
        &self.0.config.app_root
    }

    fn scheduler_db(&self) -> Arc<SchedulerDb> {
        self.0.scheduler_db.clone()
    }

    fn scheduler_runtime_gate(&self) -> Arc<tokio::sync::RwLock<()>> {
        self.0.scheduler_runtime_gate.clone()
    }

    fn trust_store(&self) -> &ryeos_engine::trust::TrustStore {
        &self.0.engine.trust_store
    }

    fn get_thread_status(&self, thread_id: &str) -> Result<Option<String>> {
        match self.0.threads.get_thread(thread_id) {
            Ok(Some(thread)) => Ok(Some(thread.status.clone())),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn get_thread_result_outcome(&self, thread_id: &str) -> Result<Option<ThreadResultOutcome>> {
        let Some(record) = self.0.threads.get_thread_result(thread_id)? else {
            return Ok(None);
        };
        let Some(result) = record.result.as_ref() else {
            return Ok(Some(ThreadResultOutcome::Success));
        };
        Ok(Some(ryeos_scheduler::classify_result_payload(result)))
    }

    fn submit_cancel(&self, thread_id: &str) -> Result<()> {
        self.0
            .commands
            .submit(&ryeos_app::command_service::CommandSubmitParams {
                thread_id: thread_id.to_string(),
                command_type: "cancel".to_string(),
                requested_by: None,
                params: None,
            })?;
        // Scheduler timeouts use the same cooperative stop path as an operator
        // command. The durable command remains the settlement record; SIGTERM
        // wakes a runtime immediately if it is inside a bounded retry backoff.
        let (_report, cancelled_roots) = ryeos_app::cascade::stop_thread_and_descendants(
            &self.0,
            thread_id,
            ryeos_app::cascade::CascadeMode::Graceful,
        )?;
        for root in cancelled_roots {
            ryeos_executor::execution::launch::kick_follow_resume_if_ready(&self.0, &root);
        }
        Ok(())
    }

    async fn dispatch_scheduled_item(
        &self,
        spec: &ScheduleSpecRecord,
        _fire_id: &str,
        thread_id: &str,
        _scheduled_at: i64,
        _trigger_reason: &str,
    ) -> Result<()> {
        let params: serde_json::Value = serde_json::from_str(&spec.params)?;
        let project_path =
            spec.project_root.as_deref().unwrap_or_else(|| {
                self.0.config.app_root.to_str().expect(
                    "app_root must be valid UTF-8 — it is configured from a known directory",
                )
            });
        let project_path_buf = std::path::PathBuf::from(project_path);
        let original_root_kind = CanonicalRef::parse(&spec.item_ref)
            .map(|ref_| ref_.kind)
            .unwrap_or_else(|_| "item".to_string());

        let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
            project_path_buf.clone(),
            self.0.engine.clone(),
        );

        let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
            launch_mode: "inline",
            target_site_id: None,
            validate_only: false,
            params,
            acting_principal: &spec.requester_fingerprint,
            project_path: std::path::Path::new(project_path),
            provenance,
            original_root_kind: &original_root_kind,
            pre_minted_thread_id: Some(thread_id.to_string()),
            usage_subject: None,
            usage_subject_asserted_by: None,
            previous_thread_id: None,
            parent_execution_context: None,
        };

        let exec_ctx = ryeos_executor::executor::ExecutionContext {
            principal_fingerprint: spec.requester_fingerprint.clone(),
            caller_scopes: spec.capabilities.clone(),
            engine: self.0.engine.clone(),
            plan_ctx: ryeos_engine::contracts::PlanContext {
                requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                    ryeos_engine::contracts::Principal {
                        fingerprint: spec.requester_fingerprint.clone(),
                        scopes: vec![],
                    },
                ),
                project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
                    path: project_path_buf,
                },
                current_site_id: "site:local".into(),
                origin_site_id: "site:local".into(),
                execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
                validate_only: false,
            },
            requested_call: None,
        };

        ryeos_executor::dispatch::dispatch(&spec.item_ref, &dispatch_req, &exec_ctx, &self.0)
            .await?;
        Ok(())
    }
}
