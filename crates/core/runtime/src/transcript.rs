use std::path::PathBuf;

use lillux::crypto::SigningKey;
use lillux::time::timestamp_millis;
use serde_json::Value;

use crate::daemon_rpc::ThreadLifecycleClient;
use crate::paths;

/// Maximum characters retained per tool output in transcript files.
/// Tool results exceeding this are truncated with a marker.
const MAX_TRANSCRIPT_OUTPUT_CHARS: usize = 2000;

fn truncate_str(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[derive(Debug, Clone, Default)]
pub struct KnowledgeRenderOptions {
    pub directive: String,
    pub status: String,
    pub model: String,
    pub cost: Option<Value>,
    pub permissions: Option<Value>,
}

pub struct Transcript {
    thread_id: String,
    project_path: PathBuf,
    daemon: Option<ThreadLifecycleClient>,
    signing_key: Option<SigningKey>,
}

impl Transcript {
    pub fn new(
        thread_id: impl Into<String>,
        project_path: PathBuf,
        daemon: Option<ThreadLifecycleClient>,
    ) -> anyhow::Result<Self> {
        let thread_id = thread_id.into();
        let dir = paths::thread_state_dir(&project_path, &thread_id)?;
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            thread_id,
            project_path,
            daemon,
            signing_key: None,
        })
    }

    pub fn with_signing_key(mut self, key: SigningKey) -> Self {
        self.signing_key = Some(key);
        self
    }

    pub async fn write_event(
        &self,
        event_type: &str,
        payload: Value,
    ) -> anyhow::Result<()> {
        let timestamp = timestamp_millis();
        let event = serde_json::json!({
            "event_type": event_type,
            "timestamp": timestamp,
            "thread_id": self.thread_id,
            "payload": payload,
        });

        if let Some(daemon) = &self.daemon {
            let storage_class = if event_type == "token_delta" {
                "journal_only"
            } else {
                "indexed"
            };
            daemon
                .append_event(&self.thread_id, event_type, payload, storage_class)
                .await?;
        } else {
            let path = paths::thread_transcript_path(&self.project_path, &self.thread_id)?;
            let line = serde_json::to_string(&event)?;
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            writeln!(file, "{line}")?;
        }

        Ok(())
    }

    pub async fn get_events(&self) -> anyhow::Result<Vec<Value>> {
        if let Some(daemon) = &self.daemon {
            let result = daemon
                .replay_events(&self.thread_id)
                .await
                .map_err(|e| anyhow::anyhow!("daemon replay failed: {e}"))?;
            return result
                .get("events")
                .and_then(|e| e.as_array()).cloned()
                .map_or_else(
                    || Err(anyhow::anyhow!("malformed daemon response")),
                    Ok,
                );
        }

        let path = paths::thread_transcript_path(&self.project_path, &self.thread_id)?;
        if !path.exists() {
            return Ok(vec![]);
        }

        let content = std::fs::read_to_string(&path)?;
        let mut events = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let event: Value =
                serde_json::from_str(line).map_err(|e| anyhow::anyhow!("corrupt JSONL: {e}"))?;
            events.push(event);
        }
        Ok(events)
    }

    pub async fn write_capabilities(
        &self,
        tool_defs: &[Value],
        tree: Option<&str>,
    ) -> anyhow::Result<PathBuf> {
        let dir = paths::thread_state_dir(&self.project_path, &self.thread_id)?;
        let caps = serde_json::json!({
            "thread_id": self.thread_id,
            "tools": tool_defs,
            "tree": tree,
        });
        let caps_path = dir.join("capabilities.json");
        let json = serde_json::to_string_pretty(&caps)?;
        std::fs::write(&caps_path, &json)?;
        Ok(caps_path)
    }

    pub async fn reconstruct_messages(&self) -> anyhow::Result<Option<Vec<Value>>> {
        let events = self.get_events().await?;
        if events.is_empty() {
            return Ok(None);
        }

        let mut messages: Vec<Value> = Vec::new();
        let mut pending_tool_calls: Vec<Value> = Vec::new();

        for event in &events {
            let event_type = event.get("event_type").and_then(|t| t.as_str()).unwrap_or("");
            let payload = event.get("payload").cloned().unwrap_or(Value::Null);

            match event_type {
                "cognition_in" => {
                    flush_tool_calls(&mut messages, &mut pending_tool_calls);
                    let role = payload.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                    let text = payload.get("text").or_else(|| payload.get("content"));
                    let mut msg = serde_json::json!({"role": role});
                    if let Some(t) = text {
                        msg.as_object_mut()
                            .unwrap()
                            .insert("content".into(), t.clone());
                    }
                    messages.push(msg);
                }
                "cognition_out" => {
                    flush_tool_calls(&mut messages, &mut pending_tool_calls);
                    let text = payload.get("text").or_else(|| payload.get("content"));
                    let mut msg = serde_json::json!({"role": "assistant"});
                    if let Some(t) = text {
                        msg.as_object_mut()
                            .unwrap()
                            .insert("content".into(), t.clone());
                    }
                    messages.push(msg);
                }
                "tool_call_start" => {
                    let call_id = payload
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    let tool = payload
                        .get("tool")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    let input = payload.get("input").cloned().unwrap_or(Value::Null);
                    pending_tool_calls.push(serde_json::json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": tool,
                            "arguments": input,
                        }
                    }));
                }
                "tool_call_result" => {
                    let call_id = payload
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    let output = clean_tool_output(&payload);
                    flush_tool_calls(&mut messages, &mut pending_tool_calls);
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": output,
                    }));
                }
                _ => {}
            }
        }

        flush_tool_calls(&mut messages, &mut pending_tool_calls);
        Ok(Some(messages))
    }

    pub async fn render_knowledge_transcript(
        &self,
        options: &KnowledgeRenderOptions,
    ) -> anyhow::Result<Option<PathBuf>> {
        let events = self.get_events().await?;
        if events.is_empty() {
            return Ok(None);
        }

        let mut md = String::new();
        md.push_str(&format!("# Thread: {}\n\n", self.thread_id));
        md.push_str(&format!("directive: {}\n", options.directive));
        md.push_str(&format!("status: {}\n", options.status));
        md.push_str(&format!("model: {}\n", options.model));
        md.push_str(&format!("date: {}\n", lillux::time::iso8601_now()));

        if let Some(cost) = &options.cost {
            md.push_str(&format!("cost: {}\n", cost));
        }
        if let Some(perms) = &options.permissions {
            md.push_str(&format!("permissions: {}\n", perms));
        }
        md.push('\n');

        md.push_str("## Events\n\n");
        for event in &events {
            let event_type = event
                .get("event_type")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown");
            let payload = event.get("payload").cloned().unwrap_or(Value::Null);

            match event_type {
                "cognition_in" => {
                    let role = payload
                        .get("role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("user");
                    let text = payload
                        .get("text")
                        .or_else(|| payload.get("content"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    md.push_str(&format!("### {role}\n\n{text}\n\n"));
                }
                "cognition_out" => {
                    let text = payload
                        .get("text")
                        .or_else(|| payload.get("content"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    md.push_str(&format!("### assistant\n\n{text}\n\n"));
                }
                "tool_call_start" => {
                    let tool = payload
                        .get("tool")
                        .and_then(|t| t.as_str())
                        .unwrap_or("unknown");
                    let input = condense_tool_input(tool, &payload);
                    md.push_str(&format!("### tool: {tool}\n\n```\n{input}\n```\n\n"));
                }
                "tool_call_result" => {
                    let output = clean_tool_output(&payload);
                    md.push_str(&format!("**result:** {}\n\n", truncate_str(&output, 500)));
                }
                _ => {
                    md.push_str(&format!("### {event_type}\n\n{payload}\n\n"));
                }
            }
        }

        let signed = self.sign_markdown(&md);
        let path =
            paths::thread_knowledge_path(&self.project_path, &self.thread_id)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &signed)?;
        Ok(Some(path))
    }

    fn sign_markdown(&self, body: &str) -> String {
        match &self.signing_key {
            Some(key) => {
                lillux::signature::sign_content(body, key, "<!--", Some("-->"))
            }
            None => body.to_string(),
        }
    }
}

fn flush_tool_calls(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let calls: Vec<Value> = std::mem::take(pending);
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            last.as_object_mut()
                .unwrap()
                .insert("tool_calls".into(), Value::Array(calls));
            return;
        }
    }
    let mut msg = serde_json::json!({"role": "assistant", "content": ""});
    msg.as_object_mut()
        .unwrap()
        .insert("tool_calls".into(), Value::Array(calls));
    messages.push(msg);
}

fn clean_tool_output(payload: &Value) -> String {
    let raw = payload
        .get("output")
        .or_else(|| payload.get("data").and_then(|d| d.get("output")))
        .or_else(|| payload.get("stdout"));

    let text = match raw {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(map)) => {
            let cleaned: serde_json::Map<String, Value> = map
                .iter()
                .filter(|(k, _)| *k != "_artifact_ref" && *k != "_artifact_note")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            serde_json::to_string(&Value::Object(cleaned)).unwrap_or_else(|e| {
                tracing::warn!("failed to serialize tool output object for transcript: {e}");
                String::new()
            })
        }
        Some(other) => {
            let s = other.to_string();
            s.chars().take(MAX_TRANSCRIPT_OUTPUT_CHARS).collect()
        }
        None => String::new(),
    };

    if text.len() > MAX_TRANSCRIPT_OUTPUT_CHARS {
        truncate_str(&text, MAX_TRANSCRIPT_OUTPUT_CHARS).to_string()
    } else {
        text
    }
}

fn condense_tool_input(tool: &str, payload: &Value) -> String {
    let input = payload.get("input").cloned().unwrap_or(Value::Null);
    if tool.contains("file-system/write") || tool.contains("file-system/create") {
        if let Value::Object(mut map) = input {
            if let Some(content) = map.get("content").and_then(|c| c.as_str()) {
                if content.len() > 200 {
                    map.insert(
                        "content".into(),
                        Value::String(format!("{}...(truncated)", truncate_str(content, 200))),
                    );
                }
            }
            return serde_json::to_string_pretty(&Value::Object(map))
                .unwrap_or_else(|e| {
                    tracing::warn!("failed to serialize truncated tool input for transcript: {e}");
                    String::new()
                });
        }
    }
    serde_json::to_string_pretty(&input).unwrap_or_else(|e| {
        tracing::warn!("failed to serialize tool input for transcript: {e}");
        String::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_and_read_events_round_trip_without_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Transcript::new("thread-1", tmp.path().to_path_buf(), None).unwrap();
        t.write_event(
            "cognition_in",
            serde_json::json!({"role": "user", "text": "hello"}),
        )
        .await
        .unwrap();
        let events = t.get_events().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "cognition_in");
        assert!(events[0]["timestamp"].is_number());
    }

    #[tokio::test]
    async fn reconstruct_messages_groups_tool_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Transcript::new("thread-1", tmp.path().to_path_buf(), None).unwrap();
        t.write_event(
            "cognition_in",
            serde_json::json!({"role": "user", "text": "find files"}),
        )
        .await
        .unwrap();
        t.write_event(
            "cognition_out",
            serde_json::json!({"text": "I'll search."}),
        )
        .await
        .unwrap();
        t.write_event(
            "tool_call_start",
            serde_json::json!({
                "tool": "file-system/read",
                "call_id": "c1",
                "input": {"path": "README.md"}
            }),
        )
        .await
        .unwrap();
        t.write_event(
            "tool_call_result",
            serde_json::json!({"call_id": "c1", "output": "done"}),
        )
        .await
        .unwrap();
        let msgs = t.reconstruct_messages().await.unwrap().unwrap();
        assert_eq!(msgs.len(), 3);
        assert!(msgs[1]["tool_calls"].is_array());
    }

    #[tokio::test]
    async fn render_knowledge_transcript_produces_signed_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        let sk = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let t = Transcript::new("thread-1", tmp.path().to_path_buf(), None)
            .unwrap()
            .with_signing_key(sk);
        t.write_event(
            "cognition_in",
            serde_json::json!({"role": "user", "text": "Hello"}),
        )
        .await
        .unwrap();
        t.write_event(
            "cognition_out",
            serde_json::json!({"text": "Hi"}),
        )
        .await
        .unwrap();
        let path = t
            .render_knowledge_transcript(&KnowledgeRenderOptions {
                directive: "test/directive".into(),
                status: "completed".into(),
                model: "test-model".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("ryeos:signed:"));
        assert!(content.contains("# Thread: thread-1"));
    }

    #[tokio::test]
    async fn get_events_empty_when_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Transcript::new("t-empty", tmp.path().to_path_buf(), None).unwrap();
        let events = t.get_events().await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn write_capabilities_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Transcript::new("t-caps", tmp.path().to_path_buf(), None).unwrap();
        let tools = vec![serde_json::json!({"name": "read"})];
        let path = t.write_capabilities(&tools, Some("default")).await.unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("read"));
    }
}
