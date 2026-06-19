//! Node-config section handler for `schedules`.
//!
//! Parses schedule spec YAML files from `.ai/node/schedules/<name>.yaml`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, NodeItemContext, SectionRecord, SectionSourcePolicy};

/// A parsed schedule spec loaded from `.ai/node/schedules/<name>.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRecord {
    pub spec_version: u32,
    pub schedule_id: String,
    pub item_ref: String,
    pub schedule_type: String,
    pub expression: String,
    #[serde(default = "default_params")]
    pub params: Value,
    #[serde(default = "default_utc")]
    pub timezone: String,
    #[serde(default)]
    pub misfire_policy: Option<String>,
    #[serde(default)]
    pub overlap_policy: Option<String>,
    #[serde(default)]
    pub lateness_grace_secs: Option<i64>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub project_root: Option<String>,
    /// Canonical registration timestamp (millis since epoch).
    /// Set once at creation, preserved on updates. Used as anchor for first-fire calculation.
    #[serde(default)]
    pub registered_at: Option<i64>,
    /// Execution authority — who this schedule runs as and with what capabilities.
    /// Set at registration time by scheduler_register. Read by projection on rebuild.
    #[serde(default)]
    pub execution: Option<ScheduleExecution>,
    /// Ownership metadata for specs reconciled from project-authored intent.
    /// Node-owned runtime specs use this only for admission/reconciliation;
    /// dispatch authority still comes from `execution`.
    #[serde(default)]
    pub managed_by: Option<ScheduleManagedBy>,
    /// Path to the YAML file. Set by loader.
    #[serde(skip)]
    pub source_file: std::path::PathBuf,
}

/// Execution authority persisted in schedule YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleExecution {
    pub requester_fingerprint: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleManagedBy {
    #[serde(rename = "type")]
    pub managed_type: String,
    pub project_root: String,
    pub project_key: String,
    pub source_snapshot_hash: String,
    pub source_path: String,
    pub source_body_hash: String,
    #[serde(default)]
    pub source_signer_fingerprint: Option<String>,
}

fn default_utc() -> String {
    "UTC".to_string()
}

fn default_true() -> bool {
    true
}

fn default_params() -> Value {
    serde_json::json!({})
}

pub struct ScheduleSection;

impl NodeConfigSection for ScheduleSection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::SystemAndState
    }

    fn parse(&self, ctx: &NodeItemContext, body: &Value) -> Result<Box<dyn SectionRecord>> {
        let record: ScheduleRecord = serde_json::from_value(body.clone())
            .context("failed to parse schedule record")?;

        if record.schedule_id != ctx.id {
            bail!(
                "schedule record declares schedule_id '{}' but filename is '{}'",
                record.schedule_id,
                ctx.id
            );
        }

        if record.spec_version != 1 {
            bail!(
                "schedule record declares spec_version {} but only 1 is supported",
                record.spec_version
            );
        }

        // Validate schedule_id syntax
        ryeos_scheduler::crontab::validate_schedule_id(&record.schedule_id)?;

        // Validate item_ref is a parseable canonical ref
        ryeos_engine::canonical_ref::CanonicalRef::parse(&record.item_ref).with_context(|| {
            format!("invalid item_ref '{}' in schedule record", record.item_ref)
        })?;

        // Validate expression parses for the given type
        ryeos_scheduler::crontab::validate_expression(&record.schedule_type, &record.expression)?;

        // Validate timezone
        ryeos_scheduler::crontab::validate_timezone(&record.timezone)?;

        // Validate overlap_policy (all policies ship day 1: allow, skip, cancel_previous)
        if let Some(ref p) = record.overlap_policy {
            if !matches!(p.as_str(), "allow" | "skip" | "cancel_previous") {
                bail!("invalid overlap_policy '{}': must be allow, skip, or cancel_previous", p);
            }
        }

        // Validate misfire_policy (all policies ship day 1: skip, fire_once_now, catch_up_bounded:N, catch_up_within_secs:S)
        if let Some(ref p) = record.misfire_policy {
            if !is_valid_misfire_policy(p) {
                bail!(
                    "invalid misfire_policy '{}': must be skip, fire_once_now, catch_up_bounded:N, or catch_up_within_secs:S",
                    p
                );
            }
        }

        if let Some(secs) = record.lateness_grace_secs {
            if secs <= 0 {
                bail!("lateness_grace_secs must be positive, got: {}", secs);
            }
        }

        // at schedules in the past are rejected at registration time (service),
        // but not at load time (they may have been valid when registered).

        Ok(Box::new(record))
    }
}

fn is_valid_misfire_policy(p: &str) -> bool {
    match p {
        "skip" | "fire_once_now" => true,
        s if s.starts_with("catch_up_bounded:") => s
            .strip_prefix("catch_up_bounded:")
            .and_then(|n| n.parse::<usize>().ok())
            .is_some(),
        s if s.starts_with("catch_up_within_secs:") => s
            .strip_prefix("catch_up_within_secs:")
            .and_then(|n| n.parse::<u64>().ok())
            .is_some(),
        _ => false,
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
            "schedule_type": "interval",
            "expression": "60",
            "timezone": "UTC",
            "enabled": true,
        })
    }

    #[test]
    fn source_policy_is_system_and_state() {
        let section = ScheduleSection;
        assert!(matches!(section.source_policy(), SectionSourcePolicy::SystemAndState));
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
        assert_eq!(record.misfire_policy, None);
        assert_eq!(record.overlap_policy, None);
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
        assert_eq!(record.misfire_policy.as_deref(), Some("fire_once_now"));
        assert_eq!(record.overlap_policy.as_deref(), Some("cancel_previous"));
        assert_eq!(record.lateness_grace_secs, Some(30));
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
