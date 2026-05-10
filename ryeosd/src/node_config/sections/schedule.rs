//! Node-config section handler for `schedules`.
//!
//! Parses schedule spec YAML files from `.ai/node/schedules/<name>.yaml`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, SectionRecord, SectionSourcePolicy};

/// A parsed schedule spec loaded from `.ai/node/schedules/<name>.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRecord {
    pub spec_version: u32,
    pub section: String,
    pub schedule_id: String,
    pub item_ref: String,
    pub schedule_type: String,
    pub expression: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default = "default_utc")]
    pub timezone: String,
    #[serde(default)]
    pub misfire_policy: Option<String>,
    #[serde(default)]
    pub overlap_policy: Option<String>,
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

fn default_utc() -> String {
    "UTC".to_string()
}

fn default_true() -> bool {
    true
}

pub struct ScheduleSection;

impl NodeConfigSection for ScheduleSection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::SystemAndState
    }

    fn parse(&self, name: &str, body: &Value) -> Result<Box<dyn SectionRecord>> {
        let record: ScheduleRecord = serde_json::from_value(body.clone())
            .context("failed to parse schedule record")?;

        if record.schedule_id != name {
            bail!(
                "schedule record declares schedule_id '{}' but filename is '{}'",
                record.schedule_id,
                name
            );
        }

        if record.section != "schedules" {
            bail!(
                "schedule record declares section '{}' but must be 'schedules'",
                record.section
            );
        }

        if record.spec_version != 1 {
            bail!(
                "schedule record declares spec_version {} but only 1 is supported",
                record.spec_version
            );
        }

        // Validate schedule_id syntax
        crate::scheduler::crontab::validate_schedule_id(&record.schedule_id)?;

        // Validate item_ref is a parseable canonical ref
        ryeos_engine::canonical_ref::CanonicalRef::parse(&record.item_ref).with_context(|| {
            format!("invalid item_ref '{}' in schedule record", record.item_ref)
        })?;

        // Validate expression parses for the given type
        crate::scheduler::crontab::validate_expression(&record.schedule_type, &record.expression)?;

        // Validate timezone
        crate::scheduler::crontab::validate_timezone(&record.timezone)?;

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

        // at schedules in the past are rejected at registration time (service),
        // but not at load time (they may have been valid when registered).

        Ok(Box::new(record))
    }
}

fn is_valid_misfire_policy(p: &str) -> bool {
    matches!(
        p,
        "skip" | "fire_once_now"
    ) || p.starts_with("catch_up_bounded:") || p.starts_with("catch_up_within_secs:")
}

impl SectionRecord for ScheduleRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_config::{NodeConfigSection, SectionSourcePolicy};

    fn valid_body() -> serde_json::Value {
        serde_json::json!({
            "spec_version": 1,
            "section": "schedules",
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
        let result = section.parse("my-schedule", &valid_body());
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

        let result = section.parse("my-schedule", &body).unwrap();
        let record = result.as_any().downcast_ref::<ScheduleRecord>().unwrap();
        assert_eq!(record.misfire_policy.as_deref(), Some("fire_once_now"));
        assert_eq!(record.overlap_policy.as_deref(), Some("cancel_previous"));
    }

    #[test]
    fn parse_cron_expression() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["schedule_type"] = serde_json::json!("cron");
        body["expression"] = serde_json::json!("0 * * * * *");

        let result = section.parse("my-schedule", &body);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_at_expression() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["schedule_type"] = serde_json::json!("at");
        body["expression"] = serde_json::json!("2028-01-01T00:00:00Z");

        let result = section.parse("my-schedule", &body);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_rejects_wrong_schedule_id() {
        let section = ScheduleSection;
        let result = section.parse("different-name", &valid_body());
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("declares schedule_id"));
    }

    #[test]
    fn parse_rejects_wrong_section() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["section"] = serde_json::json!("routes");

        let result = section.parse("my-schedule", &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_wrong_spec_version() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["spec_version"] = serde_json::json!(2);

        let result = section.parse("my-schedule", &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bad_item_ref() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["item_ref"] = serde_json::json!("not a valid ref!!!");

        let result = section.parse("my-schedule", &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bad_expression() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["expression"] = serde_json::json!("not-a-cron");

        let result = section.parse("my-schedule", &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bad_timezone() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["timezone"] = serde_json::json!("Invalid/Zone");

        let result = section.parse("my-schedule", &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bad_overlap_policy() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["overlap_policy"] = serde_json::json!("invalid");

        let result = section.parse("my-schedule", &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_bad_misfire_policy() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["misfire_policy"] = serde_json::json!("invalid");

        let result = section.parse("my-schedule", &body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_allows_catch_up_bounded() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["misfire_policy"] = serde_json::json!("catch_up_bounded:5");

        let result = section.parse("my-schedule", &body);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_allows_catch_up_within_secs() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["misfire_policy"] = serde_json::json!("catch_up_within_secs:300");

        let result = section.parse("my-schedule", &body);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_with_project_root() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["project_root"] = serde_json::json!("/tmp/my-project");

        let result = section.parse("my-schedule", &body).unwrap();
        let record = result.as_any().downcast_ref::<ScheduleRecord>().unwrap();
        assert_eq!(record.project_root.as_deref(), Some("/tmp/my-project"));
    }

    #[test]
    fn parse_with_params() {
        let section = ScheduleSection;
        let mut body = valid_body();
        body["params"] = serde_json::json!({"key": "value", "num": 42});

        let result = section.parse("my-schedule", &body).unwrap();
        let record = result.as_any().downcast_ref::<ScheduleRecord>().unwrap();
        assert_eq!(record.params["key"], "value");
        assert_eq!(record.params["num"], 42);
    }
}
