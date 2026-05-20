use anyhow::{bail, Result};
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

pub async fn load_resume_state(
    callback: &CallbackClient,
    previous_thread_id: &str,
) -> Result<ResumeState> {
    let response = callback.replay_events_for(previous_thread_id).await?;

    let messages = reconstruct_messages(&response.events)?;
    let turns_completed = count_turns(&messages);

    let trimmed = trim_to_token_budget(messages, 16_000);

    // Scan replayed events for the most recent thread_usage entry.
    // This is the runtime reconstructing its own budget from the
    // event stream — the daemon stays generic.
    let mut has_thread_usage_event = false;
    let thread_usage = extract_thread_usage_from_events(&response.events, &mut has_thread_usage_event);

    Ok(ResumeState {
        messages: trimmed,
        turns_completed,
        thread_usage,
        has_thread_usage_event,
    })
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

fn reconstruct_messages(events: &[ReplayedEventRecord]) -> Result<Vec<ProviderMessage>> {
    let mut messages = Vec::new();

    for event in events {
        match event.event_type.as_str() {
            "user_message" => {
                let content = event.payload.get("content").cloned();
                messages.push(ProviderMessage {
                    role: "user".to_string(),
                    content,
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            "assistant_message" => {
                let content = event.payload.get("content").cloned();
                let tool_calls = match event.payload.get("tool_calls").and_then(|tc| tc.as_array()) {
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
                    content,
                    tool_calls,
                    tool_call_id: None,
                });
            }
            "tool_result" => {
                let call_id = event.payload.get("call_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let content = event.payload.get("result").cloned();

                messages.push(ProviderMessage {
                    role: "tool".to_string(),
                    content,
                    tool_calls: None,
                    tool_call_id: call_id,
                });
            }
            other => {
                bail!("unknown event_type in replay: {other}");
            }
        }
    }

    Ok(messages)
}

fn count_turns(messages: &[ProviderMessage]) -> u32 {
    messages
        .iter()
        .filter(|m| m.role == "assistant")
        .count() as u32
}

fn trim_to_token_budget(mut messages: Vec<ProviderMessage>, max_tokens: u64) -> Vec<ProviderMessage> {
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
        Some(Value::Array(arr)) => arr.iter().map(|v| estimate_tokens_from_value(&Some(v.clone()))).sum(),
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
    use ryeos_runtime::callback::{CallbackError, DispatchActionRequest, ReplayResponse, RuntimeCallbackAPI};
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
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn finalize_thread(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn request_continuation(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_event(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn replay_events(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(serde_json::to_value(ReplayResponse { events: self.events.clone() }).unwrap())
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn complete_command(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
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
    fn reconstruct_messages_from_typed_events() {
        let events = vec![
            ReplayedEventRecord {
                event_type: "user_message".to_string(),
                payload: json!({"content": "Hello"}),
            },
            ReplayedEventRecord {
                event_type: "assistant_message".to_string(),
                payload: json!({"content": "Hi there!"}),
            },
            ReplayedEventRecord {
                event_type: "user_message".to_string(),
                payload: json!({"content": "Do something"}),
            },
            ReplayedEventRecord {
                event_type: "assistant_message".to_string(),
                payload: json!({
                    "content": null,
                    "tool_calls": [{"id": "c1", "name": "read_file", "arguments": {"path": "/tmp"}}]
                }),
            },
            ReplayedEventRecord {
                event_type: "tool_result".to_string(),
                payload: json!({"call_id": "c1", "result": "file contents"}),
            },
        ];

        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].role, "user");
        assert!(messages[3].tool_calls.is_some());
        assert_eq!(messages[4].role, "tool");
    }

    #[test]
    fn unknown_event_type_bails() {
        let events = vec![
            ReplayedEventRecord {
                event_type: "user_message".to_string(),
                payload: json!({"content": "Hello"}),
            },
            ReplayedEventRecord {
                event_type: "unknown_kind".to_string(),
                payload: json!({}),
            },
        ];
        let result = reconstruct_messages(&events);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown event_type"), "expected bail on unknown type, got: {err}");
    }

    #[test]
    fn count_turns_correct() {
        let messages = vec![
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("hello")),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("hi")),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("again")),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("there")),
                tool_calls: None,
                tool_call_id: None,
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
                event_type: "user_message".to_string(),
                payload: json!({"content": "Do task"}),
            },
            ReplayedEventRecord {
                event_type: "assistant_message".to_string(),
                payload: json!({"content": "Done!"}),
            },
        ];
        let callback = make_callback(events);
        let state = load_resume_state(&callback, "T-prev").await.unwrap();
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.turns_completed, 1);
    }
}
