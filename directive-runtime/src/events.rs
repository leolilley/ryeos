use anyhow::Result;
use serde_json::{json, Value};

use crate::launch_envelope::EnvelopeCallback;

pub struct EventEmitter {
    socket_path: std::path::PathBuf,
    token: String,
    thread_id: String,
}

impl EventEmitter {
    pub fn new(callback: &EnvelopeCallback, thread_id: &str) -> Self {
        Self {
            socket_path: callback.socket_path.clone(),
            token: callback.token.clone(),
            thread_id: thread_id.to_string(),
        }
    }

    pub async fn append_event(&self, event_type: &str, data: Value) -> Result<()> {
        let _event = json!({
            "type": event_type,
            "thread_id": self.thread_id,
            "data": data,
            "token": self.token,
        });
        Ok(())
    }

    pub async fn emit_turn_start(&self, turn: u32) -> Result<()> {
        self.append_event("turn_start", json!({"turn": turn}))
            .await
    }

    pub async fn emit_turn_complete(&self, turn: u32, tokens: Option<(u64, u64)>) -> Result<()> {
        let mut data = json!({"turn": turn});
        if let Some((input, output)) = tokens {
            data["input_tokens"] = json!(input);
            data["output_tokens"] = json!(output);
        }
        self.append_event("turn_complete", data).await
    }

    pub async fn emit_tool_dispatch(&self, tool_name: &str, call_id: Option<&str>) -> Result<()> {
        let mut data = json!({"tool": tool_name});
        if let Some(id) = call_id {
            data["call_id"] = json!(id);
        }
        self.append_event("tool_dispatch", data).await
    }

    pub async fn emit_tool_result(&self, call_id: &str, truncated: bool) -> Result<()> {
        self.append_event(
            "tool_result",
            json!({"call_id": call_id, "truncated": truncated}),
        )
        .await
    }

    pub async fn emit_error(&self, error: &str) -> Result<()> {
        self.append_event("error", json!({"message": error})).await
    }

    pub async fn emit_thread_continued(&self, previous_id: &str) -> Result<()> {
        self.append_event(
            "thread_continued",
            json!({"previous_thread_id": previous_id}),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_emitter() -> EventEmitter {
        let cb = EnvelopeCallback {
            socket_path: PathBuf::from("/tmp/test.sock"),
            token: "tok".to_string(),
            allowed_primaries: vec!["execute".to_string()],
        };
        EventEmitter::new(&cb, "T-test")
    }

    #[tokio::test]
    async fn append_event_succeeds() {
        let emitter = make_emitter();
        emitter
            .append_event("test_event", json!({"key": "value"}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn emit_turn_start_succeeds() {
        let emitter = make_emitter();
        emitter.emit_turn_start(1).await.unwrap();
    }

    #[tokio::test]
    async fn emit_tool_dispatch_succeeds() {
        let emitter = make_emitter();
        emitter
            .emit_tool_dispatch("read_file", Some("call_123"))
            .await
            .unwrap();
    }
}
