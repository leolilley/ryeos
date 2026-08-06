//! Durable state-anchor milestone contract.
//!
//! State anchors are daemon-authored indexed events, not generic domain
//! payloads. This module is the one v2 writer/reader contract used by event
//! publication, trace, closure discovery, and execution-field projection.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const STATE_ANCHOR_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateAnchorPayloadV2 {
    pub schema_version: u32,
    pub label: String,
    pub state_digest: String,
    pub manifest_ref: String,
    pub runtime: Value,
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateAnchorMilestoneV2 {
    pub kind: String,
    pub payload: StateAnchorPayloadV2,
    pub graph_run_id: String,
    pub definition_ref: String,
    pub effective_definition_digest: String,
    pub node: String,
    pub step: u32,
}

impl StateAnchorMilestoneV2 {
    pub fn from_value(value: Value) -> Result<Self> {
        let anchor: Self = serde_json::from_value(value)?;
        anchor.validate()?;
        Ok(anchor)
    }

    pub fn to_value(&self) -> Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn validate(&self) -> Result<()> {
        if self.kind != "state_anchor" {
            bail!("state-anchor milestone kind must be `state_anchor`");
        }
        if self.payload.schema_version != STATE_ANCHOR_SCHEMA_VERSION {
            bail!(
                "state-anchor milestone is not the exact current contract: stored schema={}, current schema={STATE_ANCHOR_SCHEMA_VERSION}",
                self.payload.schema_version
            );
        }
        for (label, value) in [
            ("label", self.payload.label.as_str()),
            ("graph_run_id", self.graph_run_id.as_str()),
            ("definition_ref", self.definition_ref.as_str()),
            ("node", self.node.as_str()),
        ] {
            if value.is_empty()
                || value.trim() != value
                || value.chars().any(char::is_control)
                || value.len() > 4 * 1024
            {
                bail!("state-anchor {label} must be a bounded, trimmed, control-free string");
            }
        }
        validate_lower_sha256(
            "effective_definition_digest",
            &self.effective_definition_digest,
        )?;
        let manifest_hash = self
            .payload
            .manifest_ref
            .strip_prefix("cas:")
            .ok_or_else(|| anyhow::anyhow!("state-anchor manifest_ref must use cas:<hash>"))?;
        validate_lower_sha256("manifest_ref hash", manifest_hash)?;
        if self.payload.state_digest != format!("sha256:{manifest_hash}") {
            bail!("state-anchor state_digest must commit to manifest_ref");
        }
        if !self.payload.runtime.is_object() {
            bail!("state-anchor runtime must be an object");
        }
        if !self.payload.metadata.is_object() {
            bail!("state-anchor metadata must be an object");
        }
        Ok(())
    }
}

fn validate_lower_sha256(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
    {
        bail!("state-anchor {label} must be a canonical lowercase SHA-256 digest");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn anchor(schema_version: u32) -> Value {
        json!({
            "kind": "state_anchor",
            "payload": {
                "schema_version": schema_version,
                "label": "checkpoint",
                "state_digest": format!("sha256:{}", "a".repeat(64)),
                "manifest_ref": format!("cas:{}", "a".repeat(64)),
                "runtime": {"kind": "tool", "item_ref": "tool:test/restore"},
                "metadata": {},
            },
            "graph_run_id": "G-test",
            "definition_ref": "graph:test/solve",
            "effective_definition_digest": "b".repeat(64),
            "node": "solve",
            "step": 4,
        })
    }

    #[test]
    fn current_contract_round_trips_and_predecessor_is_rejected() {
        let parsed = StateAnchorMilestoneV2::from_value(anchor(STATE_ANCHOR_SCHEMA_VERSION))
            .expect("v2 anchor");
        assert_eq!(
            StateAnchorMilestoneV2::from_value(parsed.to_value().unwrap()).unwrap(),
            parsed
        );
        assert!(
            StateAnchorMilestoneV2::from_value(anchor(STATE_ANCHOR_SCHEMA_VERSION - 1))
                .unwrap_err()
                .to_string()
                .contains("not the exact current contract")
        );
    }

    #[test]
    fn identity_and_manifest_commitment_are_required() {
        let mut value = anchor(STATE_ANCHOR_SCHEMA_VERSION);
        value["effective_definition_digest"] = json!("not-a-digest");
        assert!(StateAnchorMilestoneV2::from_value(value).is_err());

        let mut value = anchor(STATE_ANCHOR_SCHEMA_VERSION);
        value["payload"]["state_digest"] = json!(format!("sha256:{}", "c".repeat(64)));
        assert!(StateAnchorMilestoneV2::from_value(value).is_err());
    }
}
