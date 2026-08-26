//! Durable state-anchor milestone contract.
//!
//! State anchors are daemon-authored indexed events, not generic domain
//! payloads. Graph and execution checkpoints share one envelope and manifest
//! edge while retaining typed, independently validated subjects.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const STATE_ANCHOR_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateAnchorPayload {
    pub schema_version: u32,
    pub label: String,
    pub state_digest: String,
    pub manifest_ref: String,
    pub runtime: Value,
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateAnchorSubject {
    Graph {
        graph_run_id: String,
        definition_ref: String,
        effective_definition_digest: String,
        node: String,
        step: u32,
    },
    Execution {
        chain_root_id: String,
        placement_thread_id: String,
        item_ref: String,
        exact_program_hash: String,
        launch_capsule_hash: String,
        source_chain_seq: u64,
        source_event_hash: String,
    },
}

impl StateAnchorSubject {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Graph {
                graph_run_id,
                definition_ref,
                effective_definition_digest,
                node,
                ..
            } => {
                for (label, value) in [
                    ("graph_run_id", graph_run_id.as_str()),
                    ("definition_ref", definition_ref.as_str()),
                    ("node", node.as_str()),
                ] {
                    validate_string(label, value)?;
                }
                validate_lower_sha256("effective_definition_digest", effective_definition_digest)?;
            }
            Self::Execution {
                chain_root_id,
                placement_thread_id,
                item_ref,
                exact_program_hash,
                launch_capsule_hash,
                source_chain_seq,
                source_event_hash,
            } => {
                for (label, value) in [
                    ("chain_root_id", chain_root_id.as_str()),
                    ("placement_thread_id", placement_thread_id.as_str()),
                    ("item_ref", item_ref.as_str()),
                ] {
                    validate_string(label, value)?;
                }
                if *source_chain_seq == 0 {
                    bail!("state-anchor execution source_chain_seq must be nonzero");
                }
                validate_lower_sha256("exact_program_hash", exact_program_hash)?;
                validate_lower_sha256("launch_capsule_hash", launch_capsule_hash)?;
                validate_lower_sha256("source_event_hash", source_event_hash)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateAnchorMilestone {
    pub kind: String,
    pub payload: StateAnchorPayload,
    pub subject: StateAnchorSubject,
}

impl StateAnchorMilestone {
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
        validate_string("label", &self.payload.label)?;
        self.subject.validate()?;
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

fn validate_string(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.trim() != value
        || value.chars().any(char::is_control)
        || value.len() > 4 * 1024
    {
        bail!("state-anchor {label} must be a bounded, trimmed, control-free string");
    }
    Ok(())
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
            "subject": {
                "kind": "graph",
                "graph_run_id": "G-test",
                "definition_ref": "graph:test/solve",
                "effective_definition_digest": "b".repeat(64),
                "node": "solve",
                "step": 4,
            }
        })
    }

    #[test]
    fn current_contract_round_trips_and_predecessor_is_rejected() {
        let parsed = StateAnchorMilestone::from_value(anchor(STATE_ANCHOR_SCHEMA_VERSION))
            .expect("v3 anchor");
        assert_eq!(
            StateAnchorMilestone::from_value(parsed.to_value().unwrap()).unwrap(),
            parsed
        );
        assert!(
            StateAnchorMilestone::from_value(anchor(STATE_ANCHOR_SCHEMA_VERSION - 1))
                .unwrap_err()
                .to_string()
                .contains("not the exact current contract")
        );
    }

    #[test]
    fn execution_subject_requires_exact_position_and_program() {
        let mut value = anchor(STATE_ANCHOR_SCHEMA_VERSION);
        value["subject"] = json!({
            "kind": "execution",
            "chain_root_id": "T-root",
            "placement_thread_id": "T-placement",
            "item_ref": "worker_execution:test/session",
            "exact_program_hash": "c".repeat(64),
            "launch_capsule_hash": "d".repeat(64),
            "source_chain_seq": 8,
            "source_event_hash": "e".repeat(64),
        });
        StateAnchorMilestone::from_value(value.clone()).unwrap();
        value["subject"]["source_chain_seq"] = json!(0);
        assert!(StateAnchorMilestone::from_value(value).is_err());
    }

    #[test]
    fn identity_and_manifest_commitment_are_required() {
        let mut value = anchor(STATE_ANCHOR_SCHEMA_VERSION);
        value["subject"]["effective_definition_digest"] = json!("not-a-digest");
        assert!(StateAnchorMilestone::from_value(value).is_err());

        let mut value = anchor(STATE_ANCHOR_SCHEMA_VERSION);
        value["payload"]["state_digest"] = json!(format!("sha256:{}", "c".repeat(64)));
        assert!(StateAnchorMilestone::from_value(value).is_err());
    }
}
