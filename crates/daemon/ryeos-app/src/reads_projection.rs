//! What a directive run read, projected from its chain's recorded events.
//!
//! The fact comes only from records the daemon wrote about itself plus the
//! model's own recorded call arguments:
//!
//! - `launch_tool_reads` (daemon-authored): which inventory tools declare
//!   `reads`, the argument that names what they read, and their space;
//! - `child_thread_spawned` (daemon-authored): the item the daemon actually
//!   executed for an operation;
//! - `tool_call_start` supplies only the operation id and call id; its
//!   `item_ref`, and any `reads` or `space` it might carry, are never read;
//! - `tool_call_result` says whether that operation succeeded;
//! - `cognition_out` holds the model's call arguments, by call id.
//!
//! A call counts only when the daemon executed a read-declaring tool for it and
//! it succeeded. The ref is what the model asked for: `requested`, never proof
//! the content reached or was understood. Shell, script and project-file reads
//! are not covered.

use std::collections::{BTreeMap, HashMap, HashSet};

use ryeos_engine::execution_contract::LaunchToolReadsPayload;
use ryeos_runtime::RuntimeEventType;
use ryeos_runtime::envelope::{
    FOLLOW_READS_BASIS_REQUESTED, FOLLOW_READS_COVERED, FollowReads, MAX_FOLLOW_READ_REF_BYTES,
    MAX_FOLLOW_READ_REFS, ReadEntry,
};

use crate::state_store::PersistedEventRecord;

/// The event types the projection reads.
pub fn projection_event_types() -> Vec<String> {
    [
        RuntimeEventType::LaunchToolReads,
        RuntimeEventType::ChildThreadSpawned,
        RuntimeEventType::ToolCallStart,
        RuntimeEventType::ToolCallResult,
        RuntimeEventType::CognitionOut,
    ]
    .into_iter()
    .map(|event_type| event_type.as_str().to_owned())
    .collect()
}

/// Project `events` (one chain, in chain order). `scan_cut` says the scan
/// stopped before the end of the chain, so the result is marked truncated.
pub fn project_reads(events: &[PersistedEventRecord], scan_cut: bool) -> FollowReads {
    // item ref -> (argument, space), the newest launch record winning.
    let mut table: BTreeMap<String, (String, String)> = BTreeMap::new();
    // operation id -> item the daemon executed for it.
    let mut executed: HashMap<String, String> = HashMap::new();
    // (thread, call id) -> the model's arguments for that call.
    let mut arguments: HashMap<(String, String), serde_json::Value> = HashMap::new();
    // operation ids whose tool result was ok.
    let mut succeeded: HashSet<String> = HashSet::new();

    for event in events {
        let kind = event.event_type.as_str();
        if kind == RuntimeEventType::LaunchToolReads.as_str() {
            if let Ok(payload) =
                serde_json::from_value::<LaunchToolReadsPayload>(event.payload.clone())
            {
                for entry in payload.tools {
                    table.insert(
                        entry.item_ref,
                        (entry.argument, entry.space.as_str().to_owned()),
                    );
                }
            }
        } else if kind == RuntimeEventType::ChildThreadSpawned.as_str() {
            if let (Some(operation_id), Some(item_id)) = (
                event.payload.get("operation_id").and_then(|v| v.as_str()),
                event.payload.get("item_id").and_then(|v| v.as_str()),
            ) {
                executed.insert(operation_id.to_owned(), item_id.to_owned());
            }
        } else if kind == RuntimeEventType::ToolCallResult.as_str() {
            if event.payload.get("status").and_then(|v| v.as_str()) == Some("ok")
                && let Some(operation_id) =
                    event.payload.get("operation_id").and_then(|v| v.as_str())
            {
                succeeded.insert(operation_id.to_owned());
            }
        } else if kind == RuntimeEventType::CognitionOut.as_str() {
            let Some(parts) = event.payload.get("parts").and_then(|v| v.as_array()) else {
                continue;
            };
            for part in parts {
                if part.get("type").and_then(|v| v.as_str()) == Some("tool_call")
                    && let (Some(call_id), Some(args)) = (
                        part.get("call_id").and_then(|v| v.as_str()),
                        part.get("arguments"),
                    )
                {
                    arguments.insert((event.thread_id.clone(), call_id.to_owned()), args.clone());
                }
            }
        }
    }

    let mut refs: Vec<ReadEntry> = Vec::new();
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let mut truncated = scan_cut;
    for event in events {
        if event.event_type != RuntimeEventType::ToolCallStart.as_str() {
            continue;
        }
        let (Some(operation_id), Some(call_id)) = (
            event.payload.get("operation_id").and_then(|v| v.as_str()),
            event.payload.get("call_id").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        if !succeeded.contains(operation_id) {
            continue;
        }
        let Some(item) = executed.get(operation_id) else {
            continue;
        };
        let Some((argument, space)) = table.get(item) else {
            continue;
        };
        let Some(reference) = arguments
            .get(&(event.thread_id.clone(), call_id.to_owned()))
            .and_then(|args| args.get(argument))
            .and_then(|value| value.as_str())
        else {
            continue;
        };
        if reference.is_empty() || reference.len() > MAX_FOLLOW_READ_REF_BYTES {
            continue;
        }
        let key = (item.clone(), space.clone(), reference.to_owned());
        if !seen.insert(key) {
            continue;
        }
        if refs.len() >= MAX_FOLLOW_READ_REFS {
            truncated = true;
            break;
        }
        refs.push(ReadEntry {
            tool: item.clone(),
            space: space.clone(),
            reference: reference.to_owned(),
            basis: FOLLOW_READS_BASIS_REQUESTED.to_owned(),
        });
    }
    FollowReads {
        refs,
        truncated,
        covered: FOLLOW_READS_COVERED.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const OP: &str = "op1";

    fn event(seq: i64, event_type: &str, payload: Value) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: seq,
            event_hash: None,
            chain_root_id: "C".to_owned(),
            chain_seq: seq,
            thread_id: "T".to_owned(),
            thread_seq: seq,
            event_type: event_type.to_owned(),
            storage_class: "indexed".to_owned(),
            ts: "2026-11-28T00:00:00Z".to_owned(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload,
        }
    }

    fn table(tools: Value) -> PersistedEventRecord {
        event(1, "launch_tool_reads", json!({ "tools": tools }))
    }

    fn fetch_table() -> PersistedEventRecord {
        table(json!([
            {"item_ref": "tool:ryeos/core/fetch", "argument": "item_ref", "space": "bundle"},
            {"item_ref": "tool:mine/read", "argument": "path", "space": "project"},
        ]))
    }

    /// One call: the model's arguments, the start, the daemon's spawn record
    /// and the result.
    fn call(
        base: i64,
        op: &str,
        call_id: &str,
        item: &str,
        args: Value,
        status: &str,
    ) -> Vec<PersistedEventRecord> {
        vec![
            event(
                base,
                "cognition_out",
                json!({"turn": 1, "finish": "tool_calls", "parts": [
                    {"type": "tool_call", "call_id": call_id, "name": "x", "arguments": args}
                ]}),
            ),
            event(
                base + 1,
                "tool_call_start",
                json!({"operation_id": op, "call_id": call_id, "tool": "x", "turn": 1, "call_index": 0}),
            ),
            event(
                base + 2,
                "child_thread_spawned",
                json!({"child_thread_id": "C1", "item_id": item, "operation_id": op}),
            ),
            event(
                base + 3,
                "tool_call_result",
                json!({"operation_id": op, "call_id": call_id, "tool": "x", "status": status}),
            ),
        ]
    }

    #[test]
    fn a_successful_declared_call_is_a_requested_read_in_its_declared_space() {
        let mut events = vec![fetch_table()];
        events.extend(call(
            10,
            OP,
            "c1",
            "tool:ryeos/core/fetch",
            json!({"item_ref": "knowledge:a/b"}),
            "ok",
        ));
        let reads = project_reads(&events, false);
        assert_eq!(reads.refs.len(), 1);
        assert_eq!(reads.refs[0].reference, "knowledge:a/b");
        assert_eq!(reads.refs[0].space, "bundle");
        assert_eq!(reads.refs[0].basis, "requested");
        assert_eq!(reads.covered, "declared-read-tools-only");
        assert!(!reads.truncated);
    }

    #[test]
    fn a_start_event_cannot_name_the_tool_or_claim_a_space() {
        // The daemon executed a project-space tool; the runtime's start event
        // claims fetch and a bundle space. Neither is read.
        let mut events = vec![fetch_table()];
        let mut forged = call(
            10,
            OP,
            "c1",
            "tool:mine/read",
            json!({"path": "secret", "item_ref": "knowledge:a/b"}),
            "ok",
        );
        forged[1].payload["item_ref"] = json!("tool:ryeos/core/fetch");
        forged[1].payload["space"] = json!("bundle");
        forged[1].payload["reads"] = json!({"argument": "item_ref"});
        events.extend(forged);
        let reads = project_reads(&events, false);
        assert_eq!(reads.refs.len(), 1, "{reads:?}");
        assert_eq!(reads.refs[0].tool, "tool:mine/read");
        assert_eq!(reads.refs[0].space, "project");
        assert_eq!(reads.refs[0].reference, "secret");
    }

    #[test]
    fn a_call_the_daemon_did_not_execute_a_failed_call_and_an_undeclared_tool_earn_nothing() {
        let mut events = vec![fetch_table()];
        // No spawn record for the operation: the daemon executed nothing.
        let mut none = call(
            10,
            "op-none",
            "c1",
            "tool:ryeos/core/fetch",
            json!({"item_ref": "k:one"}),
            "ok",
        );
        none.remove(2);
        events.extend(none);
        events.extend(call(
            20,
            "op-failed",
            "c2",
            "tool:ryeos/core/fetch",
            json!({"item_ref": "k:two"}),
            "error",
        ));
        events.extend(call(
            30,
            "op-other",
            "c3",
            "tool:other/run",
            json!({"item_ref": "k:three"}),
            "ok",
        ));
        assert!(project_reads(&events, false).refs.is_empty());
    }

    #[test]
    fn a_repeated_read_is_listed_once_and_the_list_is_bounded() {
        let mut events = vec![fetch_table()];
        events.extend(call(
            10,
            "op-a",
            "c1",
            "tool:ryeos/core/fetch",
            json!({"item_ref": "k:same"}),
            "ok",
        ));
        events.extend(call(
            20,
            "op-b",
            "c2",
            "tool:ryeos/core/fetch",
            json!({"item_ref": "k:same"}),
            "ok",
        ));
        assert_eq!(project_reads(&events, false).refs.len(), 1);

        let mut many = vec![fetch_table()];
        for n in 0..(MAX_FOLLOW_READ_REFS + 3) {
            many.extend(call(
                10 + 10 * i64::try_from(n).unwrap(),
                &format!("op{n}"),
                &format!("c{n}"),
                "tool:ryeos/core/fetch",
                json!({"item_ref": format!("k:{n}")}),
                "ok",
            ));
        }
        let reads = project_reads(&many, false);
        assert_eq!(reads.refs.len(), MAX_FOLLOW_READ_REFS);
        assert!(reads.truncated);
        assert!(reads.validate().is_ok());
        assert!(project_reads(&[], true).truncated);
    }

    #[test]
    fn an_over_long_ref_and_a_non_string_argument_are_not_reads() {
        let mut events = vec![fetch_table()];
        let long = "x".repeat(MAX_FOLLOW_READ_REF_BYTES + 1);
        events.extend(call(
            10,
            "op-a",
            "c1",
            "tool:ryeos/core/fetch",
            json!({"item_ref": long}),
            "ok",
        ));
        events.extend(call(
            20,
            "op-b",
            "c2",
            "tool:ryeos/core/fetch",
            json!({"item_ref": 7}),
            "ok",
        ));
        assert!(project_reads(&events, false).refs.is_empty());
    }
}
