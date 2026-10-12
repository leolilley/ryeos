//! `ui.ryeos.schedules.list` — schedule listing for the ryeos-ui.
//!
//! Wraps the existing scheduler listing, providing a ryeos-ui-friendly
//! view with last-fire and fire-count data.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let specs = state
        .scheduler_db
        .list_specs_filtered(false, None, Some(caller.principal_id()))?;

    // Batch-load last fires to avoid N+1 queries
    let schedule_ids: Vec<String> = specs.iter().map(|s| s.schedule_id.clone()).collect();
    let last_fires = state.scheduler_db.get_last_fires_batch(&schedule_ids)?;

    // Read each fire count; a query failure must not be presented as zero.
    let fire_counts: std::collections::HashMap<String, usize> = schedule_ids
        .iter()
        .map(|id| {
            state
                .scheduler_db
                .count_fires(id, None)
                .map(|count| (id.clone(), count))
        })
        .collect::<std::result::Result<_, _>>()?;

    let schedules: Vec<Value> = specs
        .iter()
        .map(|spec| {
            let last_fire = last_fires.get(&spec.schedule_id);
            let total_fires = fire_counts.get(&spec.schedule_id).copied().unwrap_or(0);
            serde_json::json!({
                "schedule_id": spec.schedule_id,
                "item_ref": spec.item_ref,
                "schedule_type": spec.schedule_type,
                "expression": spec.expression,
                "timezone": spec.timezone,
                "enabled": spec.enabled,
                "signer_fingerprint": spec.signer_fingerprint,
                "last_fire_at": last_fire.and_then(|f| f.dispatched_at),
                "last_fire_status": last_fire.map(|f| f.status.clone()),
                "total_fires": total_fires,
            })
        })
        .collect();

    Ok(serde_json::json!({ "schedules": schedules,
        "identity_filtered": true, "owner_principal_id": caller.principal_id(),
        "coverage":"current_caller_owned_schedules_not_node_or_project_totals"
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/schedules/list",
    endpoint: "ui.ryeos.schedules.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
pub(super) fn owner_fixture_spec(
    id: &str,
    principal: &str,
    enabled: bool,
) -> ryeos_scheduler::ScheduleSpecRecord {
    ryeos_scheduler::ScheduleSpecRecord {
        schedule_id: id.into(),
        item_ref: "directive:test".into(),
        ref_bindings: Default::default(),
        params: "{}".into(),
        schedule_type: "interval".into(),
        expression: "60".into(),
        timezone: "UTC".into(),
        misfire_policy: "skip".into(),
        overlap_policy: "skip".into(),
        lateness_grace_secs: 60,
        enabled,
        project_root: None,
        signer_fingerprint: "44".repeat(32),
        spec_hash: "22".repeat(32),
        registered_at: 1000,
        execution: ryeos_scheduler::types::ScheduleExecution {
            authority: ryeos_scheduler::types::ScheduleExecutionAuthority::Node {
                principal_id: principal.into(),
                effective_origin_site_id: "site:test".into(),
            },
            capabilities: vec!["ryeos.execute.*".into()],
            policy: ryeos_engine::execution_contract::ExecutionPolicy::projectless(
                ryeos_engine::execution_contract::ExecutionResponse::Accepted,
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schedules_follow_execution_owner_not_signer_or_all_node_scope() {
        let db = ryeos_scheduler::SchedulerDb::new_in_memory().unwrap();
        let own = format!("fp:{}", "11".repeat(32));
        let foreign = format!("fp:{}", "33".repeat(32));
        db.upsert_spec(&owner_fixture_spec("own", &own, true))
            .unwrap();
        db.upsert_spec(&owner_fixture_spec("foreign", &foreign, true))
            .unwrap();
        let rows = db.list_specs_filtered(false, None, Some(&own)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].schedule_id, "own");
        assert!(
            db.list_specs_filtered(false, None, Some(&format!("fp:{}", "44".repeat(32))))
                .unwrap()
                .is_empty(),
            "source signer is not execution owner"
        );
    }
}
