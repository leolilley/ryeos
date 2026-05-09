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
    /// Path to the YAML file. Set by loader.
    #[serde(skip)]
    pub source_file: std::path::PathBuf,
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

        // Validate overlap_policy
        if let Some(ref p) = record.overlap_policy {
            if !matches!(p.as_str(), "allow" | "skip" | "cancel_previous") {
                bail!("invalid overlap_policy '{}': must be allow, skip, or cancel_previous", p);
            }
        }

        // Validate misfire_policy
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
