use anyhow::Result;
use serde_json::Value;

use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::ReplayedEventRecord;
use ryeos_state::ThreadUsage;

use crate::directive::ProviderMessage;

pub struct ResumeState {
    pub messages: Vec<ProviderMessage>,
    pub turns_completed: u32,
    pub thread_usage: Option<ThreadUsage>,
    /// Whether the replayed events contained at least one `thread_usage`
    /// entry. Used by the resume gate in main.rs to refuse resume when
    /// prerequisites are unmet.
    pub has_thread_usage_event: bool,
}

/// Backstop on the continuation-path walk — far above any real conversation
/// length. Exceeding it is treated as a runaway/cyclic chain and errors loudly
/// rather than silently truncating (which would drop conversation history).
const MAX_CONTINUATION_PATH: usize = 10_000;

pub async fn load_resume_state(
    callback: &CallbackClient,
    previous_thread_id: &str,
) -> Result<ResumeState> {
    // Fold the linear CONTINUATION PATH (turn 1 → … → predecessor), not the
    // whole chain namespace: a conversation is a chain of turns, so turn N must
    // see turns 1..N-1 — but a chain root can also contain non-continuation
    // child threads (compose-context, sub-dispatch) that share `chain_root_id`
    // and emit transcript events. Walking `upstream_thread_id` from the
    // predecessor yields only conversation turns (a turn's upstream is always
    // its continuation predecessor, never a child), and replaying each turn
    // thread-scoped structurally excludes those children.
    let path = continuation_path(callback, previous_thread_id).await?;

    let mut events: Vec<ReplayedEventRecord> = Vec::new();
    for thread_id in &path {
        let page = callback.replay_thread(thread_id).await?;
        events.extend(page.events);
    }

    let messages = reconstruct_messages(&events)?;
    let turns_completed = count_turns(&messages);

    let trimmed = trim_to_token_budget(messages, 16_000);

    // Scan replayed events for the most recent thread_usage entry.
    // This is the runtime reconstructing its own budget from the
    // event stream — the daemon stays generic.
    let mut has_thread_usage_event = false;
    let thread_usage = extract_thread_usage_from_events(&events, &mut has_thread_usage_event);

    Ok(ResumeState {
        messages: trimmed,
        turns_completed,
        thread_usage,
        has_thread_usage_event,
    })
}

/// Resolve the linear continuation path ending at `predecessor_id`, ordered
/// root-first. Walks `upstream_thread_id` (the continuation predecessor link)
/// until the root (no upstream), guarding against cycles.
async fn continuation_path(
    callback: &CallbackClient,
    predecessor_id: &str,
) -> Result<Vec<String>> {
    let mut path = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut current = Some(predecessor_id.to_string());

    while let Some(thread_id) = current {
        if !seen.insert(thread_id.clone()) {
            anyhow::bail!("resume: continuation path cycle at {thread_id}");
        }
        let detail = callback.get_thread_by_id(&thread_id).await?;
        let upstream = detail
            .get("thread")
            .and_then(|thread| thread.get("upstream_thread_id"))
            .and_then(|value| value.as_str())
            .map(str::to_string);
        path.push(thread_id);
        if path.len() >= MAX_CONTINUATION_PATH && upstream.is_some() {
            anyhow::bail!(
                "resume: continuation path exceeds {MAX_CONTINUATION_PATH} turns \
                 (runaway or cyclic chain); refusing to fold a truncated history"
            );
        }
        current = upstream;
    }

    path.reverse(); // root first → turns fold in order
    Ok(path)
}

/// Extract the most recent `ThreadUsage` from replayed events by
/// scanning for `thread_usage` event_type entries. Sets the flag
/// if any such entry is found, even if deserialization fails.
fn extract_thread_usage_from_events(
    events: &[ReplayedEventRecord],
    found: &mut bool,
) -> Option<ThreadUsage> {
    // Walk in reverse to get the latest usage
    for event in events.iter().rev() {
        if event.event_type == "thread_usage" {
            *found = true;
            if let Ok(usage) = serde_json::from_value::<ThreadUsage>(event.payload.clone()) {
                return Some(usage);
            }
            // Found a thread_usage event but failed to deserialize — bail
            // is handled by the caller checking has_thread_usage_event
            return None;
        }
    }
    None
}

/// Fold the chain's transcript-bearing events into provider messages. The
/// substrate vocabulary is the cognition transcript — there is no "user":
///
/// - `cognition_in`     → an input/stimulus to cognition (the rendered prompt);
///   the bare `{turn}` turn-boundary markers carry no content and are skipped;
/// - `cognition_out`    → the cognition's output (content + tool_calls + reasoning);
/// - `tool_call_result` → a tool result, keyed back by `call_id`.
///
/// Every other chain event (lifecycle, usage settlement, streaming deltas,
/// tool-dispatch starts, graph milestones) carries no message and is skipped —
/// folding a chain is lossy by design, not an error. `role` is the provider-wire
/// mapping applied here, not a substrate concept.
fn reconstruct_messages(events: &[ReplayedEventRecord]) -> Result<Vec<ProviderMessage>> {
    let mut messages = Vec::new();

    for event in events {
        match event.event_type.as_str() {
            // A content-bearing cognition_in is a stimulus; a content-less one
            // is a turn-boundary marker (skip it).
            "cognition_in" if event.payload.get("content").is_some() => {
                messages.push(ProviderMessage {
                    role: "user".to_string(),
                    content: event.payload.get("content").cloned(),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                });
            }
            "cognition_out" => {
                let tool_calls = match event.payload.get("tool_calls").and_then(|tc| tc.as_array())
                {
                    Some(arr) => {
                        let calls: Vec<crate::directive::ToolCall> = arr
                            .iter()
                            .map(|tc| {
                                let id = tc.get("id")
                                    .and_then(|v| v.as_str())
                                    .map(String::from);
                                let name = tc.get("name")
                                    .and_then(|v| v.as_str())
                                    .ok_or_else(|| anyhow::anyhow!(
                                        "malformed tool_call in replay: missing or non-string 'name' field"
                                    ))?
                                    .to_string();
                                let arguments = tc.get("arguments").cloned()
                                    .ok_or_else(|| anyhow::anyhow!(
                                        "malformed tool_call in replay: missing 'arguments' field on tool '{name}'"
                                    ))?;
                                Ok::<_, anyhow::Error>(crate::directive::ToolCall { id, name, arguments })
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        Some(calls)
                    }
                    None => None,
                };

                messages.push(ProviderMessage {
                    role: "assistant".to_string(),
                    content: event.payload.get("content").cloned(),
                    tool_calls,
                    tool_call_id: None,
                    reasoning_content: event
                        .payload
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                });
            }
            "tool_call_result" => {
                let call_id = event
                    .payload
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                // The model-visible body is `result` (parsed JSON) or
                // `result_text` (non-JSON body preserved verbatim).
                let content = event
                    .payload
                    .get("result")
                    .or_else(|| event.payload.get("result_text"))
                    .cloned();

                messages.push(ProviderMessage {
                    role: "tool".to_string(),
                    content,
                    tool_calls: None,
                    tool_call_id: call_id,
                    reasoning_content: None,
                });
            }
            // Non-conversational chain events carry no message — skip.
            _ => {}
        }
    }

    Ok(messages)
}

fn count_turns(messages: &[ProviderMessage]) -> u32 {
    messages.iter().filter(|m| m.role == "assistant").count() as u32
}

fn trim_to_token_budget(
    mut messages: Vec<ProviderMessage>,
    max_tokens: u64,
) -> Vec<ProviderMessage> {
    if messages.is_empty() {
        return messages;
    }

    let mut total: u64 = messages.iter().map(estimate_tokens).sum();
    while total > max_tokens && messages.len() > 1 {
        let removed = messages.remove(1);
        total -= estimate_tokens_from_value(&removed.content);
    }

    messages
}

fn estimate_tokens(msg: &ProviderMessage) -> u64 {
    let mut count = estimate_tokens_from_value(&msg.content);
    for tc in msg.tool_calls.iter().flatten() {
        count += estimate_tokens_from_value(&Some(tc.arguments.clone()));
    }
    count
}

fn estimate_tokens_from_value(v: &Option<Value>) -> u64 {
    match v {
        Some(Value::String(s)) => (s.len() as u64) / 4,
        Some(Value::Number(_)) => 1,
        Some(Value::Bool(_)) => 1,
        Some(Value::Null) | None => 0,
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| estimate_tokens_from_value(&Some(v.clone())))
            .sum(),
        Some(Value::Object(obj)) => obj
            .values()
            .map(|v| estimate_tokens_from_value(&Some(v.clone())))
            .sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ryeos_runtime::callback::{
        CallbackError, DispatchActionRequest, ReplayResponse, RuntimeCallbackAPI,
        TerminalCompletion,
    };
    use ryeos_runtime::ReplayedEventRecord;
    use serde_json::json;
    use std::sync::Arc;

    struct MockCallback {
        events: Vec<ReplayedEventRecord>,
    }

    #[async_trait]
    impl RuntimeCallbackAPI for MockCallback {
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _: &str,
            _: TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> {
            // load_resume_state resolves the chain root from the predecessor's
            // thread detail before paging the chain.
            Ok(json!({"thread": {"chain_root_id": "C-test-chain"}}))
        }
        async fn request_continuation(&self, _: &str, _: Option<&str>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_event(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _: Value) -> Result<Value, CallbackError> {
            Ok(serde_json::to_value(ReplayResponse {
                events: self.events.clone(),
                next_cursor: None,
            })
            .unwrap())
        }
        async fn bundle_events_append(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_read_chain(
            &self,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_get(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_delete(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_list(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"keys": []}))
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _: &str,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
    }

    fn make_callback(events: Vec<ReplayedEventRecord>) -> CallbackClient {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(MockCallback { events });
        CallbackClient::from_inner(inner, "T-test", "/tmp/test", "tat-test")
    }

    #[tokio::test]
    async fn load_empty_replay_returns_empty() {
        let callback = make_callback(vec![]);
        let state = load_resume_state(&callback, "nonexistent").await.unwrap();
        assert!(state.messages.is_empty());
        assert_eq!(state.turns_completed, 0);
        assert!(state.thread_usage.is_none());
    }

    #[test]
    fn reconstruct_messages_from_cognition_transcript() {
        let events = vec![
            // turn-boundary marker (no content) — skipped
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"turn": 1}),
            },
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "Hello"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"turn": 1, "content": "Hi there!"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "Do something"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({
                    "turn": 2,
                    "content": null,
                    "tool_calls": [{"id": "c1", "name": "read_file", "arguments": {"path": "/tmp"}}]
                }),
            },
            ReplayedEventRecord {
                event_type: "tool_call_result".to_string(),
                payload: json!({"call_id": "c1", "tool": "read_file", "result": "file contents"}),
            },
        ];

        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].role, "user");
        assert!(messages[3].tool_calls.is_some());
        assert_eq!(messages[4].role, "tool");
        assert_eq!(messages[4].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn non_conversational_events_are_skipped() {
        // Lifecycle / usage / turn-marker / streaming events carry no message;
        // folding skips them rather than failing.
        let events = vec![
            ReplayedEventRecord {
                event_type: "thread_created".to_string(),
                payload: json!({}),
            },
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"turn": 1}),
            },
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "Hello"}),
            },
            ReplayedEventRecord {
                event_type: "thread_usage".to_string(),
                payload: json!({"completed_turns": 1}),
            },
            ReplayedEventRecord {
                event_type: "tool_call_start".to_string(),
                payload: json!({"tool": "read_file"}),
            },
        ];
        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn count_turns_correct() {
        let messages = vec![
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("hello")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("hi")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("again")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("there")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
        ];
        assert_eq!(count_turns(&messages), 2);
    }

    #[test]
    fn trim_to_token_budget_works() {
        let mut messages = Vec::new();
        for i in 0..100 {
            messages.push(ProviderMessage {
                role: "user".to_string(),
                content: Some(json!(format!("message {} with some content here", i))),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
        let trimmed = trim_to_token_budget(messages, 200);
        assert!(trimmed.len() < 100);
        assert!(!trimmed.is_empty());
    }

    #[tokio::test]
    async fn full_roundtrip_via_callback() {
        let events = vec![
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "Do task"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"turn": 1, "content": "Done!"}),
            },
        ];
        let callback = make_callback(events);
        let state = load_resume_state(&callback, "T-prev").await.unwrap();
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.turns_completed, 1);
    }

    /// A chain where turn T2 continues T1, and T1 also has a non-continuation
    /// child `T1-child` sharing the chain root that emits its own
    /// `cognition_out`. Resume must fold ONLY the linear continuation path
    /// (T1, T2), never the child — proving the path-scoped (per-thread) fold.
    struct PathMock;

    #[async_trait]
    impl RuntimeCallbackAPI for PathMock {
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn finalize_thread(&self, _: &str, _: TerminalCompletion) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_thread(&self, thread_id: &str) -> Result<Value, CallbackError> {
            // T2's continuation predecessor is T1; T1 is the root (no upstream).
            let upstream = if thread_id == "T2" { Some("T1") } else { None };
            Ok(json!({"thread": {"chain_root_id": "T1", "upstream_thread_id": upstream}}))
        }
        async fn request_continuation(&self, _: &str, _: Option<&str>) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_event(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn replay_events(&self, params: Value) -> Result<Value, CallbackError> {
            let tid = params.get("thread_id").and_then(|v| v.as_str()).unwrap_or("");
            let ev = |t: &str, p: Value| ReplayedEventRecord { event_type: t.to_string(), payload: p };
            let events = match tid {
                "T1" => vec![
                    ev("cognition_in", json!({"content": "turn1 in"})),
                    ev("cognition_out", json!({"content": "turn1 out"})),
                    ev("thread_usage", json!({"completed_turns": 1})),
                ],
                "T2" => vec![
                    ev("cognition_in", json!({"content": "turn2 in"})),
                    ev("cognition_out", json!({"content": "turn2 out"})),
                    ev("thread_usage", json!({"completed_turns": 2})),
                ],
                // Non-continuation child sharing the chain root — must NOT fold.
                "T1-child" => vec![ev("cognition_out", json!({"content": "POLLUTION"}))],
                _ => vec![],
            };
            Ok(serde_json::to_value(ReplayResponse { events, next_cursor: None }).unwrap())
        }
        async fn bundle_events_append(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn bundle_events_read_chain(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({"events": []})) }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({"events": []})) }
        async fn vault_put(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn vault_get(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn vault_delete(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn vault_list(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({"keys": []})) }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn complete_command(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
    }

    #[tokio::test]
    async fn resume_folds_only_continuation_path_not_chain_children() {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(PathMock);
        let callback = CallbackClient::from_inner(inner, "T3", "/tmp/test", "tat-test");
        let state = load_resume_state(&callback, "T2").await.unwrap();

        let contents: Vec<String> = state
            .messages
            .iter()
            .filter_map(|m| m.content.as_ref().and_then(|c| c.as_str()).map(String::from))
            .collect();
        // Both turns, root-first, in order; the chain-sharing child is excluded.
        assert_eq!(
            contents,
            vec!["turn1 in", "turn1 out", "turn2 in", "turn2 out"]
        );
        assert!(
            !contents.iter().any(|c| c.contains("POLLUTION")),
            "non-continuation child events must not be folded"
        );
        assert_eq!(state.turns_completed, 2);
        assert!(state.has_thread_usage_event);
    }
}
