//! Node-owned storage class of each progressive runtime event.
//!
//! The daemon stores every runtime-appended event under this section, not
//! under any class a runtime chooses; runtimes send none. The code floor is
//! [`RuntimeEventType::is_progressive`]: only progressive events (token,
//! stream and reasoning deltas, observations, foreach iteration results) are
//! named here, and each must be named exactly once. Every other event is
//! lifecycle, audit, launch or terminal evidence and is always `indexed`; a
//! policy that names one is refused rather than ignored. There is no code
//! fallback: a generation without this section does not load.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_runtime::events::{RuntimeEventType, StorageClass, cognition_out_is_progressive};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "event_storage";

/// Every progressive event type, in wire form. A policy names exactly these.
const PROGRESSIVE_EVENT_TYPES: [RuntimeEventType; 5] = [
    RuntimeEventType::TokenDelta,
    RuntimeEventType::StreamSnapshot,
    RuntimeEventType::CognitionReasoning,
    RuntimeEventType::Observation,
    RuntimeEventType::GraphForeachIteration,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventStoragePolicy {
    schema: u32,
    /// Progressive event type (wire name) → storage class. Private: a node's
    /// record comes only from [`EventStoragePolicySection::parse`], which
    /// validates it.
    classes: BTreeMap<String, StorageClass>,
}

impl EventStoragePolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node event_storage policy schema is not current");
        }
        for name in self.classes.keys() {
            let event_type = RuntimeEventType::parse(name)
                .with_context(|| format!("node event_storage policy names `{name}`"))?;
            if !event_type.is_progressive() {
                bail!(
                    "node event_storage policy names `{name}`, which is lifecycle, audit, launch \
                     or terminal evidence and is always indexed; only progressive events ({}) \
                     take a storage class from policy",
                    progressive_names().join(", ")
                );
            }
        }
        let missing = progressive_names()
            .into_iter()
            .filter(|name| !self.classes.contains_key(*name))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!(
                "node event_storage policy does not name {}; every progressive event needs an \
                 explicit storage class",
                missing.join(", ")
            );
        }
        Ok(())
    }

    /// The class the daemon stores `event_type` under. A progressive event
    /// takes this policy's class; a `cognition_out` streaming fragment is
    /// live-only, because the turn-complete `cognition_out` is its durable
    /// record; every other event is `indexed`.
    pub fn class_for(&self, event_type: RuntimeEventType, payload: &Value) -> StorageClass {
        if event_type.is_progressive() {
            // `validate` proves every progressive type is named, so this is
            // unreachable for a node's record. Were one missing, the most
            // durable class is the safe one.
            return self
                .classes
                .get(event_type.as_str())
                .copied()
                .unwrap_or(StorageClass::Indexed);
        }
        if event_type == RuntimeEventType::CognitionOut && cognition_out_is_progressive(payload) {
            return StorageClass::Ephemeral;
        }
        StorageClass::Indexed
    }

    /// The mapping every shipped init profile carries. Builds exact test
    /// snapshots only; a node's policy always comes from its signed
    /// generation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn as_shipped() -> Self {
        Self {
            schema: 1,
            classes: PROGRESSIVE_EVENT_TYPES
                .iter()
                .map(|event_type| (event_type.as_str().to_owned(), event_type.storage_class()))
                .collect(),
        }
    }
}

fn progressive_names() -> Vec<&'static str> {
    PROGRESSIVE_EVENT_TYPES
        .iter()
        .map(|event_type| event_type.as_str())
        .collect()
}

impl TypedNodePolicy for EventStoragePolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct EventStoragePolicySection;

impl NodePolicySection for EventStoragePolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: EventStoragePolicy =
            serde_json::from_value(body.clone()).context("parse node event_storage policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(body: Value) -> anyhow::Result<EventStoragePolicy> {
        let record: EventStoragePolicy = serde_json::from_value(body)?;
        record.validate()?;
        Ok(record)
    }

    fn shipped_body() -> Value {
        json!({
            "schema": 1,
            "classes": {
                "cognition_reasoning": "ephemeral",
                "graph_foreach_iteration": "journal_only",
                "observation": "journal_only",
                "stream_snapshot": "ephemeral",
                "token_delta": "ephemeral",
            }
        })
    }

    #[test]
    fn the_progressive_set_is_the_runtime_floor() {
        for event_type in PROGRESSIVE_EVENT_TYPES {
            assert!(event_type.is_progressive(), "{}", event_type.as_str());
        }
    }

    #[test]
    fn the_shipped_body_parses_and_matches_the_shipped_classes() {
        let policy = parse(shipped_body()).unwrap();
        assert_eq!(policy, EventStoragePolicy::as_shipped());
        assert_eq!(
            policy.class_for(RuntimeEventType::Observation, &json!({})),
            StorageClass::JournalOnly
        );
        assert_eq!(
            policy.class_for(RuntimeEventType::TokenDelta, &json!({})),
            StorageClass::Ephemeral
        );
    }

    #[test]
    fn the_policy_chooses_each_progressive_class() {
        let mut body = shipped_body();
        body["classes"]["token_delta"] = json!("journal_only");
        body["classes"]["observation"] = json!("indexed");
        let policy = parse(body).unwrap();
        assert_eq!(
            policy.class_for(RuntimeEventType::TokenDelta, &json!({})),
            StorageClass::JournalOnly
        );
        assert_eq!(
            policy.class_for(RuntimeEventType::Observation, &json!({})),
            StorageClass::Indexed
        );
    }

    #[test]
    fn floor_events_cannot_be_named() {
        for name in [
            "thread_completed",
            "tool_call_result",
            "cognition_out",
            "milestone",
        ] {
            let mut body = shipped_body();
            body["classes"][name] = json!("ephemeral");
            let error = parse(body).unwrap_err().to_string();
            assert!(error.contains("always indexed"), "{name}: {error}");
        }
    }

    #[test]
    fn floor_events_are_always_indexed() {
        let policy = parse(shipped_body()).unwrap();
        for event_type in [
            RuntimeEventType::ThreadCompleted,
            RuntimeEventType::ToolCallStart,
            RuntimeEventType::RuntimeLaunchFacts,
            RuntimeEventType::Milestone,
        ] {
            assert_eq!(
                policy.class_for(event_type, &json!({"delta": "x"})),
                StorageClass::Indexed,
                "{}",
                event_type.as_str()
            );
        }
    }

    #[test]
    fn cognition_out_fragments_stay_live_only() {
        let policy = parse(shipped_body()).unwrap();
        assert_eq!(
            policy.class_for(RuntimeEventType::CognitionOut, &json!({"delta": "x"})),
            StorageClass::Ephemeral
        );
        assert_eq!(
            policy.class_for(
                RuntimeEventType::CognitionOut,
                &json!({"content": "done", "tool_calls": []})
            ),
            StorageClass::Indexed
        );
    }

    #[test]
    fn every_progressive_event_must_be_named() {
        let mut body = shipped_body();
        body["classes"]
            .as_object_mut()
            .unwrap()
            .remove("observation");
        let error = parse(body).unwrap_err().to_string();
        assert!(error.contains("does not name observation"), "{error}");
    }

    #[test]
    fn malformed_shapes_are_refused() {
        let mut wrong_schema = shipped_body();
        wrong_schema["schema"] = json!(2);
        assert!(parse(wrong_schema).is_err());
        let mut unknown_class = shipped_body();
        unknown_class["classes"]["token_delta"] = json!("discard");
        assert!(parse(unknown_class).is_err());
        let mut unknown_event = shipped_body();
        unknown_event["classes"]["not_an_event"] = json!("ephemeral");
        assert!(parse(unknown_event).is_err());
        let mut extra = shipped_body();
        extra["extra"] = json!(true);
        assert!(parse(extra).is_err());
    }
}
