//! Durable record of one graph node action's observed result.
//!
//! A record exists so a later run of the same program can replay the result
//! instead of re-executing the action. The replay identity is the node cache
//! key — effective definition digest, graph id, node name, canonical action —
//! so equal keys mean equal executable behavior for everything the program
//! declares. Records are written only by the daemon under the pinned state
//! authority: a sandboxed runtime can request replay but can never forge the
//! record it would be replayed from.

use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const GRAPH_NODE_EFFECT_RECORD_KIND: &str = "graph_node_effect_record";
pub const GRAPH_NODE_EFFECT_RECORD_SCHEMA_VERSION: u32 = 1;

/// Ceiling for the serialized result payload. The generic object store bound
/// also applies; this keeps validation meaningful in isolation and refuses
/// pathological results before they reach storage.
pub const MAX_EFFECT_RECORD_RESULT_BYTES: usize = 1024 * 1024;

/// Effect classes that may produce a durable record. `live` never records:
/// its absence here is the contract, not an oversight.
pub const RECORDABLE_EFFECT_CLASSES: &[&str] = &["sealed", "recorded"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphNodeEffectRecord {
    pub schema: u32,
    pub kind: String,
    /// The replay identity this record answers for.
    pub cache_key: String,
    pub effective_definition_digest: String,
    pub graph_id: String,
    pub node: String,
    /// Canonical digest of the exact dispatched action value.
    pub action_digest: String,
    /// `sealed` or `recorded`.
    pub class: String,
    /// The exact result envelope the daemon observed at first execution.
    /// Opaque here: it carries no CAS references and contributes no closure
    /// edges.
    pub result: Value,
    /// Thread that produced the recorded execution, for provenance.
    pub produced_by_thread: String,
}

fn require_hex64(field: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("graph node effect record {field} must be 64 lowercase hex characters");
    }
    Ok(())
}

impl GraphNodeEffectRecord {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.kind != GRAPH_NODE_EFFECT_RECORD_KIND {
            bail!("unexpected graph node effect record kind: {}", self.kind);
        }
        if self.schema != GRAPH_NODE_EFFECT_RECORD_SCHEMA_VERSION {
            bail!(
                "unexpected graph node effect record schema: {} (current {})",
                self.schema,
                GRAPH_NODE_EFFECT_RECORD_SCHEMA_VERSION
            );
        }
        require_hex64("cache_key", &self.cache_key)?;
        require_hex64("effective_definition_digest", &self.effective_definition_digest)?;
        require_hex64("action_digest", &self.action_digest)?;
        if !RECORDABLE_EFFECT_CLASSES.contains(&self.class.as_str()) {
            bail!(
                "graph node effect record class `{}` is not recordable; \
                 live results are never recorded",
                self.class
            );
        }
        for (field, value) in [
            ("graph_id", &self.graph_id),
            ("node", &self.node),
            ("produced_by_thread", &self.produced_by_thread),
        ] {
            super::validate_trimmed_control_free(
                &format!("graph node effect record {field}"),
                value,
                false,
            )?;
        }
        let result_bytes = serde_json::to_vec(&self.result)?.len();
        if result_bytes > MAX_EFFECT_RECORD_RESULT_BYTES {
            bail!(
                "graph node effect record result is {result_bytes} bytes; \
                 the bound is {MAX_EFFECT_RECORD_RESULT_BYTES}"
            );
        }
        Ok(())
    }

    /// Decode only the exact current wire contract, rejecting other schemas
    /// before serde interprets any field.
    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("graph node effect record has no string kind"))?;
        if kind != GRAPH_NODE_EFFECT_RECORD_KIND {
            bail!("unexpected graph node effect record kind: {kind}");
        }
        let schema = value
            .get("schema")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("graph node effect record has no numeric schema"))?;
        if schema != u64::from(GRAPH_NODE_EFFECT_RECORD_SCHEMA_VERSION) {
            return Err(super::IncompatibleCurrentObjectSchema::new(
                "graph node effect record",
                schema,
                GRAPH_NODE_EFFECT_RECORD_SCHEMA_VERSION,
            )
            .into());
        }
        let record: Self = serde_json::from_value(value.clone())?;
        record.validate()?;
        Ok(record)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> GraphNodeEffectRecord {
        GraphNodeEffectRecord {
            schema: GRAPH_NODE_EFFECT_RECORD_SCHEMA_VERSION,
            kind: GRAPH_NODE_EFFECT_RECORD_KIND.to_string(),
            cache_key: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            graph_id: "arc/game_solver".to_string(),
            node: "probe_grid".to_string(),
            action_digest: "c".repeat(64),
            class: "recorded".to_string(),
            result: json!({"outcome_code": null, "result": {"ok": true}}),
            produced_by_thread: "T-1234".to_string(),
        }
    }

    #[test]
    fn a_record_round_trips_through_the_current_contract() {
        let value = record().to_value().unwrap();
        let decoded = GraphNodeEffectRecord::from_current_value(&value).unwrap();
        assert_eq!(decoded, record());
    }

    #[test]
    fn live_results_are_never_recordable() {
        let mut invalid = record();
        invalid.class = "live".to_string();
        let error = invalid.validate().unwrap_err();
        assert!(error.to_string().contains("never recorded"));
    }

    #[test]
    fn a_predecessor_schema_is_rejected_before_field_interpretation() {
        let mut value = record().to_value().unwrap();
        value["schema"] = json!(0);
        // A field that would fail validation must not be reached: the schema
        // gate answers first.
        value["cache_key"] = json!("not-hex");
        let error = GraphNodeEffectRecord::from_current_value(&value).unwrap_err();
        assert!(error.to_string().contains("schema"));
    }
}
