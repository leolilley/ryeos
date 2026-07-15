//! Node-config section handler for `schedules`.
//!
//! Parses schedule spec YAML files from `.ai/node/schedules/<name>.yaml`.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, NodeItemContext, SectionRecord, SectionSourcePolicy};

pub use ryeos_scheduler::types::{
    ScheduleExecution, ScheduleManagedBy, ScheduleSourceRecord as ScheduleRecord,
};

pub struct ScheduleSection;

impl NodeConfigSection for ScheduleSection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::SystemAndState
    }

    fn parse(&self, ctx: &NodeItemContext, body: &Value) -> Result<Box<dyn SectionRecord>> {
        let record: ScheduleRecord =
            serde_json::from_value(body.clone()).context("failed to parse schedule record")?;
        record.validate(Some(&ctx.id))?;

        // at schedules in the past are rejected at registration time (service),
        // but not at load time (they may have been valid when registered).

        Ok(Box::new(record))
    }
}

impl SectionRecord for ScheduleRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_config::{NodeConfigSection, NodeItemContext, SectionSourcePolicy};

    fn ctx(id: &str) -> NodeItemContext {
        NodeItemContext {
            section: "schedules".into(),
            id: id.into(),
            stem: id.into(),
            rel_path: format!("{id}.yaml").into(),
            source_file: format!("/tmp/{id}.yaml").into(),
            signer_fingerprint: "test".into(),
        }
    }

    fn valid_body() -> serde_json::Value {
        serde_json::json!({
            "spec_version": 1,
            "schedule_id": "my-schedule",
            "item_ref": "directive:test/hello",
            "ref_bindings": {},
            "schedule_type": "interval",
            "expression": "60",
            "timezone": "UTC",
            "misfire_policy": "fire_once_now",
            "overlap_policy": "skip",
            "lateness_grace_secs": 60,
            "enabled": true,
            "params": {},
            "project_root": null,
            "registered_at": 1_700_000_000_000_i64,
            "execution": {
                "requester_fingerprint": "fp:test",
                "capabilities": ["ryeos.execute.directive.*"],
            },
            "managed_by": null,
        })
    }

    #[test]
    fn source_policy_is_system_and_state() {
        let section = ScheduleSection;
        assert!(matches!(
            section.source_policy(),
            SectionSourcePolicy::SystemAndState
        ));
    }

    #[test]
    fn parse_valid_minimal() {
        let section = ScheduleSection;
        let result = section.parse(&ctx("my-schedule"), &valid_body());
        assert!(result.is_ok(), "expected ok, got {:?}", result.err());
        let boxed = result.unwrap();
        let record = boxed.as_any().downcast_ref::<ScheduleRecord>().unwrap();
        assert_eq!(record.schedule_id, "my-schedule");
        assert_eq!(record.schedule_type, "interval");
        assert_eq!(record.expression, "60");
        assert!(record.enabled);
        assert_eq!(record.misfire_policy, "fire_once_now");
        assert_eq!(record.overlap_policy, "skip");
    }

    #[test]
    fn parse_with_policies() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["misfire_policy"] = serde_json::json!("fire_once_now");
        body["overlap_policy"] = serde_json::json!("cancel_previous");
        body["lateness_grace_secs"] = serde_json::json!(30);

        let result = section.parse(&ctx("my-schedule"), &body).unwrap();
        let record = result.as_any().downcast_ref::<ScheduleRecord>().unwrap();
        assert_eq!(record.misfire_policy, "fire_once_now");
        assert_eq!(record.overlap_policy, "cancel_previous");
        assert_eq!(record.lateness_grace_secs, 30);
    }

    #[test]
    fn parse_cron_expression() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["schedule_type"] = serde_json::json!("cron");
        body["expression"] = serde_json::json!("0 * * * * *");

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_at_expression() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["schedule_type"] = serde_json::json!("at");
        body["expression"] = serde_json::json!("2028-01-01T00:00:00Z");

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_rejects_wrong_schedule_id() {
        let section = ScheduleSection;
        let result = section.parse(&ctx("different-name"), &valid_body());
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("declares schedule_id"));
    }

    #[test]
    fn parse_rejects_wrong_section() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["section"] = serde_json::json!("routes");

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_wrong_spec_version() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["spec_version"] = serde_json::json!(2);

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bad_item_ref() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["item_ref"] = serde_json::json!("not a valid ref!!!");

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bad_expression() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["expression"] = serde_json::json!("not-a-cron");

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bad_timezone() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["timezone"] = serde_json::json!("Invalid/Zone");

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bad_overlap_policy() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["overlap_policy"] = serde_json::json!("invalid");

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bad_misfire_policy() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["misfire_policy"] = serde_json::json!("invalid");

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_non_positive_lateness_grace() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["lateness_grace_secs"] = serde_json::json!(0);

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_allows_catch_up_bounded() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["misfire_policy"] = serde_json::json!("catch_up_bounded:5");

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_allows_catch_up_within_secs() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["misfire_policy"] = serde_json::json!("catch_up_within_secs:300");

        let result = section.parse(&ctx("my-schedule"), &body);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_with_project_root() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["project_root"] = serde_json::json!("/tmp/my-project");

        let result = section.parse(&ctx("my-schedule"), &body).unwrap();
        let record = result.as_any().downcast_ref::<ScheduleRecord>().unwrap();
        assert_eq!(record.project_root.as_deref(), Some("/tmp/my-project"));
    }

    #[test]
    fn parse_project_managed_shape() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["project_root"] = serde_json::json!("/tmp/my-project");
        body["managed_by"] = serde_json::json!({
            "type": "project_ai_sync",
            "project_root": "/tmp/my-project",
            "project_key": "11".repeat(32),
            "source_snapshot_hash": "22".repeat(32),
            "source_path": ".ai/config/schedules/report.yaml",
            "source_body_hash": "33".repeat(32),
        });

        let result = section.parse(&ctx("my-schedule"), &body).unwrap();
        let record = result.as_any().downcast_ref::<ScheduleRecord>().unwrap();
        assert!(matches!(
            &record.managed_by,
            Some(ScheduleManagedBy::ProjectAiSync { .. })
        ));
    }

    #[test]
    fn parse_maintenance_managed_shape() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["managed_by"] = serde_json::json!({
            "type": "node_maintenance_declaration",
            "source": "maintenance/schedules.yaml",
        });

        let result = section.parse(&ctx("my-schedule"), &body).unwrap();
        let record = result.as_any().downcast_ref::<ScheduleRecord>().unwrap();
        assert!(matches!(
            &record.managed_by,
            Some(ScheduleManagedBy::NodeMaintenanceDeclaration { .. })
        ));
    }

    #[test]
    fn parse_rejects_unknown_managed_shape() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["managed_by"] = serde_json::json!({
            "type": "other",
            "source": "maintenance/schedules.yaml",
        });

        assert!(section.parse(&ctx("my-schedule"), &body).is_err());
    }

    #[test]
    fn parse_rejects_cross_shape_managed_fields() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["managed_by"] = serde_json::json!({
            "type": "node_maintenance_declaration",
            "source": "maintenance/schedules.yaml",
            "project_key": "unexpected",
        });

        assert!(section.parse(&ctx("my-schedule"), &body).is_err());
    }

    #[test]
    fn parse_with_params() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["params"] = serde_json::json!({"key": "value", "num": 42});

        let result = section.parse(&ctx("my-schedule"), &body).unwrap();
        let record = result.as_any().downcast_ref::<ScheduleRecord>().unwrap();
        assert_eq!(record.params["key"], "value");
        assert_eq!(record.params["num"], 42);
    }
}
