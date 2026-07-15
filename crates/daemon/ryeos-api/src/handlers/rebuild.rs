//! Offline projection verification and rebuild from signed heads plus CAS.
//!
//! OfflineOnly: caller (run-service standalone path) holds the state lock;
//! this handler just ports the rebuild logic from `actions::rebuild`.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Verify,
    Rebuild,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {}

#[derive(Debug, serde::Serialize)]
pub struct ProjectionMaintenanceReport {
    pub operation: Operation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chains_rebuilt: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads_restored: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events_projected: Option<usize>,
    pub signed_heads_verified: usize,
    pub projection_cursors_verified: usize,
    pub projection_tables_verified: usize,
    pub pending_retirements_recovered: usize,
}

async fn handle(operation: Operation, state: Arc<AppState>) -> Result<Value> {
    let _scheduler_guard = match operation {
        Operation::Verify => None,
        Operation::Rebuild => Some(state.scheduler_runtime_gate.clone().write_owned().await),
    };
    let state_store = state.state_store.clone();
    let scheduler_db = state.scheduler_db.clone();
    let app_root = state.config.app_root.clone();

    let report = tokio::task::spawn_blocking(move || -> Result<ProjectionMaintenanceReport> {
        let rebuilt = match operation {
            Operation::Verify => None,
            Operation::Rebuild => Some(state_store.rebuild_projection_generation()?),
        };
        let pending_retirements_recovered = match operation {
            Operation::Verify => 0,
            Operation::Rebuild => {
                state_store
                    .recover_pending_terminal_chain_removals(
                        &lillux::time::iso8601_now(),
                        &app_root,
                        false,
                        |thread_ids| {
                            let mut pins = 0_u64;
                            for thread_id in thread_ids {
                                if scheduler_db.find_fire_by_thread(thread_id)?.is_some() {
                                    pins = pins.checked_add(1).ok_or_else(|| {
                                        anyhow::anyhow!("scheduler recovery pin count overflow")
                                    })?;
                                }
                            }
                            Ok(pins)
                        },
                    )?
                    .pending_retirements_recovered
            }
        };
        // Verification is deliberately a separate, non-mutating full scan.
        // Running it after rebuild also validates the selected installed
        // generation rather than trusting the temporary rebuild connection.
        let verified = state_store.verify_projection_generation()?;
        Ok(ProjectionMaintenanceReport {
            operation,
            chains_rebuilt: rebuilt.as_ref().map(|report| report.chains_rebuilt),
            threads_restored: rebuilt.as_ref().map(|report| report.threads_restored),
            events_projected: rebuilt.as_ref().map(|report| report.events_projected),
            signed_heads_verified: verified.signed_heads_verified,
            projection_cursors_verified: verified.projection_cursors_verified,
            projection_tables_verified: verified.projection_tables_verified,
            pending_retirements_recovered,
        })
    })
    .await??;

    serde_json::to_value(report).map_err(Into::into)
}

pub const VERIFY_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:projection/verify",
    endpoint: "projection.verify",
    availability: ServiceAvailability::OfflineOnly,
    required_caps: &["ryeos.execute.service.projection/verify"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let _: Request = serde_json::from_value(params)?;
            handle(Operation::Verify, state).await
        })
    },
};

pub const REBUILD_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:projection/rebuild",
    endpoint: "projection.rebuild",
    availability: ServiceAvailability::OfflineOnly,
    required_caps: &["ryeos.execute.service.projection/rebuild"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let _: Request = serde_json::from_value(params)?;
            handle(Operation::Rebuild, state).await
        })
    },
};
