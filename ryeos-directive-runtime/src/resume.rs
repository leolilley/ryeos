use anyhow::Result;
use serde_json::Value;

use ryeos_runtime::callback_client::CallbackClient;

use crate::directive::ProviderMessage;

pub struct ResumeState {
    pub messages: Vec<ProviderMessage>,
    pub turns_completed: u32,
}

pub async fn load_resume_state(
    callback: &CallbackClient,
    previous_thread_id: &str,
) -> Result<ResumeState> {
    let response = callback.replay_events_for(previous_thread_id).await?;

    let events = response
        .get("events")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();

    let messages = reconstruct_messages(&events);
    let turns_completed = count_turns(&messages);

    let trimmed = trim_to_token_budget(messages, 16_000);

    Ok(ResumeState {
        messages: trimmed,
        turns_completed,
    })
}

fn reconstruct_messages(events: &[Value]) -> Vec<ProviderMessage> {
    let mut messages = Vec::new();

    for event in events {
        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            "user_message" => {
                if let Some(content) = event.get("data").and_then(|d| d.get("content")) {
                    messages.push(ProviderMessage {
                        role: "user".to_string(),
                        content: Some(content.clone()),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
            "assistant_message" => {
                let data = event.get("data").cloned().unwrap_or(Value::Null);
                let content = data.get("content").cloned();
                let tool_calls = data.get("tool_calls").and_then(|tc| tc.as_array()).map(
                    |arr| {
                        arr.iter()
                            .filter_map(|tc| {
                                let id = tc.get("id").and_then(|v| v.as_str()).map(String::from);
                                let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let arguments = tc.get("arguments").cloned().unwrap_or(Value::Null);
                                Some(crate::directive::ToolCall { id, name, arguments })
                            })
                            .collect::<Vec<_>>()
                    },
                );

                messages.push(ProviderMessage {
                    role: "assistant".to_string(),
                    content,
                    tool_calls,
                    tool_call_id: None,
                });
            }
            "tool_result" => {
                let call_id = event
                    .get("data")
                    .and_then(|d| d.get("call_id"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let content = event
                    .get("data")
                    .and_then(|d| d.get("result"))
                    .cloned();

                messages.push(ProviderMessage {
                    role: "tool".to_string(),
                    content,
                    tool_calls: None,
                    tool_call_id: call_id,
                });
            }
            _ => {}
        }
    }

    messages
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

    let mut total: u64 = messages.iter().map(|m| estimate_tokens(m)).sum();
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
    use ryeos_runtime::callback::{CallbackError, DispatchActionRequest, RuntimeCallbackAPI};
    use serde_json::json;
    use std::sync::Arc;

    struct MockCallback {
        events: Vec<Value>,
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
            Ok(json!({"events": self.events.clone()}))
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn complete_command(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
    }

    fn make_callback(events: Vec<Value>) -> CallbackClient {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(MockCallback { events });
        CallbackClient::from_inner(inner, "T-test", "/tmp/test")
    }

    #[tokio::test]
    async fn load_empty_replay_returns_empty() {
        let callback = make_callback(vec![]);
        let state = load_resume_state(&callback, "nonexistent").await.unwrap();
        assert!(state.messages.is_empty());
        assert_eq!(state.turns_completed, 0);
    }

    #[test]
    fn reconstruct_messages_from_events() {
        let events = vec![
            json!({
                "type": "user_message",
                "data": {"content": "Hello"}
            }),
            json!({
                "type": "assistant_message",
                "data": {"content": "Hi there!"}
            }),
            json!({
                "type": "user_message",
                "data": {"content": "Do something"}
            }),
            json!({
                "type": "assistant_message",
                "data": {
                    "content": null,
                    "tool_calls": [{"id": "c1", "name": "read_file", "arguments": {"path": "/tmp"}}]
                }
            }),
            json!({
                "type": "tool_result",
                "data": {"call_id": "c1", "result": "file contents"}
            }),
        ];

        let messages = reconstruct_messages(&events);
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].role, "user");
        assert!(messages[3].tool_calls.is_some());
        assert_eq!(messages[4].role, "tool");
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
            json!({"type": "user_message", "data": {"content": "Do task"}}),
            json!({"type": "assistant_message", "data": {"content": "Done!"}}),
        ];
        let callback = make_callback(events);
        let state = load_resume_state(&callback, "T-prev").await.unwrap();
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.turns_completed, 1);
    }
}
