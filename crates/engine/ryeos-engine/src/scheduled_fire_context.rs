//! Canonical daemon-authored identity for one scheduled execution fire.
//!
//! This type lives in the engine because it crosses scheduler, admission,
//! recovery, launch-envelope, and runtime boundaries.  The scheduler owns
//! minting the values; consumers may only validate and carry them.

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};

pub const SCHEDULED_FIRE_CONTEXT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScheduledFireContext {
    pub schema_version: u32,
    pub schedule_id: String,
    /// Canonical recovery/redrive coordinate: `<schedule_id>@<scheduled_at_ms>`.
    pub fire_id: String,
    pub scheduled_at_ms: i64,
    /// Timestamp of the first durable dispatch claim. Recovery preserves it.
    pub first_dispatch_at_ms: i64,
    pub trigger_reason: String,
    pub schedule_spec_hash: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduledFireContextWire {
    schema_version: u32,
    schedule_id: String,
    fire_id: String,
    scheduled_at_ms: i64,
    first_dispatch_at_ms: i64,
    trigger_reason: String,
    schedule_spec_hash: String,
}

impl ScheduledFireContext {
    pub fn new(
        schedule_id: String,
        fire_id: String,
        scheduled_at_ms: i64,
        first_dispatch_at_ms: i64,
        trigger_reason: String,
        schedule_spec_hash: String,
    ) -> Result<Self> {
        let context = Self {
            schema_version: SCHEDULED_FIRE_CONTEXT_SCHEMA_VERSION,
            schedule_id,
            fire_id,
            scheduled_at_ms,
            first_dispatch_at_ms,
            trigger_reason,
            schedule_spec_hash,
        };
        context.validate()?;
        Ok(context)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEDULED_FIRE_CONTEXT_SCHEMA_VERSION {
            anyhow::bail!(
                "scheduled fire context schema mismatch: received {}, expected {}",
                self.schema_version,
                SCHEDULED_FIRE_CONTEXT_SCHEMA_VERSION
            );
        }
        validate_schedule_id(&self.schedule_id)?;
        if self.scheduled_at_ms < 0 {
            anyhow::bail!("scheduled fire scheduled_at_ms must not be negative");
        }
        if self.first_dispatch_at_ms < self.scheduled_at_ms {
            anyhow::bail!("scheduled fire first_dispatch_at_ms precedes scheduled_at_ms");
        }
        let expected_fire_id = format!("{}@{}", self.schedule_id, self.scheduled_at_ms);
        if self.fire_id != expected_fire_id {
            anyhow::bail!(
                "scheduled fire identity {} does not match canonical {}",
                self.fire_id,
                expected_fire_id
            );
        }
        if self.trigger_reason.is_empty()
            || self.trigger_reason.len() > 128
            || self.trigger_reason.trim() != self.trigger_reason
            || self.trigger_reason.chars().any(char::is_control)
        {
            anyhow::bail!(
                "scheduled fire trigger_reason must be 1..=128 trimmed, control-free characters"
            );
        }
        if !is_lower_sha256(&self.schedule_spec_hash) {
            anyhow::bail!("scheduled fire schedule_spec_hash must be lowercase SHA-256 hex");
        }
        Ok(())
    }
}

/// Project the daemon-authored scheduled-fire authority into the one reserved
/// runtime execution-context namespace. This is a view of already-admitted
/// authority, not a second input or an independently trusted context.
///
/// Ordinary executions deliberately receive an explicit `null` schedule so
/// tools, directives, and graphs share one stable shape.
pub fn execution_context_value(schedule: Option<&ScheduledFireContext>) -> serde_json::Value {
    serde_json::json!({
        "schedule": schedule,
    })
}

impl<'de> Deserialize<'de> for ScheduledFireContext {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ScheduledFireContextWire::deserialize(deserializer)?;
        let context = Self {
            schema_version: wire.schema_version,
            schedule_id: wire.schedule_id,
            fire_id: wire.fire_id,
            scheduled_at_ms: wire.scheduled_at_ms,
            first_dispatch_at_ms: wire.first_dispatch_at_ms,
            trigger_reason: wire.trigger_reason,
            schedule_spec_hash: wire.schedule_spec_hash,
        };
        context.validate().map_err(serde::de::Error::custom)?;
        Ok(context)
    }
}

fn validate_schedule_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 {
        anyhow::bail!("scheduled fire schedule_id must contain 1..=128 bytes");
    }
    let bytes = id.as_bytes();
    let is_alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    if !is_alphanumeric(bytes[0]) || !is_alphanumeric(bytes[bytes.len() - 1]) {
        anyhow::bail!(
            "scheduled fire schedule_id must begin and end with a lowercase ASCII letter or digit"
        );
    }
    if bytes
        .iter()
        .any(|byte| !is_alphanumeric(*byte) && !matches!(*byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!(
            "scheduled fire schedule_id may contain only lowercase ASCII letters, digits, '-', '_', and '.'"
        );
    }
    Ok(())
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> ScheduledFireContext {
        ScheduledFireContext::new(
            "campaign.nightly".to_owned(),
            "campaign.nightly@1700000000000".to_owned(),
            1_700_000_000_000,
            1_700_000_000_123,
            "normal".to_owned(),
            "a".repeat(64),
        )
        .unwrap()
    }

    #[test]
    fn round_trip_preserves_exact_fire_coordinate() {
        let expected = context();
        let value = serde_json::to_value(&expected).unwrap();
        let observed: ScheduledFireContext = serde_json::from_value(value).unwrap();
        assert_eq!(observed, expected);
    }

    #[test]
    fn decode_rejects_unknown_fields() {
        let mut value = serde_json::to_value(context()).unwrap();
        value["ambient_now"] = json!(1_700_000_000_999_i64);
        assert!(serde_json::from_value::<ScheduledFireContext>(value).is_err());
    }

    #[test]
    fn decode_rejects_noncanonical_fire_identity() {
        let mut value = serde_json::to_value(context()).unwrap();
        value["fire_id"] = json!("campaign.nightly@1700000000001");
        assert!(serde_json::from_value::<ScheduledFireContext>(value).is_err());
    }

    #[test]
    fn decode_rejects_dispatch_before_schedule() {
        let mut value = serde_json::to_value(context()).unwrap();
        value["first_dispatch_at_ms"] = json!(1_699_999_999_999_i64);
        assert!(serde_json::from_value::<ScheduledFireContext>(value).is_err());
    }

    #[test]
    fn decode_rejects_noncanonical_spec_hash() {
        let mut value = serde_json::to_value(context()).unwrap();
        value["schedule_spec_hash"] = json!("A".repeat(64));
        assert!(serde_json::from_value::<ScheduledFireContext>(value).is_err());
    }

    #[test]
    fn execution_context_projection_has_one_stable_reserved_shape() {
        let scheduled = execution_context_value(Some(&context()));
        assert_eq!(
            scheduled["schedule"]["fire_id"],
            "campaign.nightly@1700000000000"
        );

        let ordinary = execution_context_value(None);
        assert!(ordinary["schedule"].is_null());
        assert_eq!(ordinary.as_object().unwrap().len(), 1);
    }
}
