//! SSE event parser — parses Server-Sent Events from the daemon stream.

use serde::Deserialize;

/// Parsed SSE event from the daemon.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SseEvent {
    TextDelta {
        text: String,
    },
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        name: String,
        output: String,
        duration_ms: Option<u64>,
    },
    Done {
        status: String,
    },
    Error {
        message: String,
    },
    Unknown {
        event_type: String,
        data: String,
    },
}

impl SseEvent {
    /// Parse an SSE event from event type and data strings.
    #[allow(dead_code)]
    pub fn parse(event_type: &str, data: &str) -> Self {
        match event_type {
            "text_delta" => {
                #[derive(Deserialize)]
                struct Data {
                    #[allow(dead_code)]
                    text: String,
                }
                let data: Data = serde_json::from_str(data).unwrap_or(Data {
                    text: data.to_string(),
                });
                SseEvent::TextDelta { text: data.text }
            }
            "tool_call" => {
                #[derive(Deserialize)]
                struct Data {
                    name: String,
                    #[allow(dead_code)]
                    input: serde_json::Value,
                }
                let data: Data = serde_json::from_str(data).unwrap_or(Data {
                    name: "unknown".into(),
                    input: serde_json::Value::Null,
                });
                SseEvent::ToolCall {
                    name: data.name,
                    input: data.input,
                }
            }
            "tool_result" => {
                #[derive(Deserialize)]
                struct Data {
                    name: String,
                    #[allow(dead_code)]
                    output: String,
                    #[allow(dead_code)]
                    duration_ms: Option<u64>,
                }
                let data: Data = serde_json::from_str(data).unwrap_or(Data {
                    name: "unknown".into(),
                    output: String::new(),
                    duration_ms: None,
                });
                SseEvent::ToolResult {
                    name: data.name,
                    output: data.output,
                    duration_ms: data.duration_ms,
                }
            }
            "done" => {
                #[derive(Deserialize)]
                struct Data {
                    #[allow(dead_code)]
                    status: String,
                }
                let data: Data = serde_json::from_str(data).unwrap_or(Data {
                    status: "unknown".into(),
                });
                SseEvent::Done {
                    status: data.status,
                }
            }
            "error" => {
                #[derive(Deserialize)]
                struct Data {
                    #[allow(dead_code)]
                    message: String,
                }
                let data: Data = serde_json::from_str(data).unwrap_or(Data {
                    message: data.to_string(),
                });
                SseEvent::Error {
                    message: data.message,
                }
            }
            _ => SseEvent::Unknown {
                event_type: event_type.to_string(),
                data: data.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_maps_text_delta_tool_result_done_error() {
        let delta = SseEvent::parse("text_delta", r#"{"text":"hello"}"#);
        assert!(matches!(delta, SseEvent::TextDelta { .. }));

        let tool_call = SseEvent::parse("tool_call", r#"{"name":"read","input":{}}"#);
        assert!(matches!(tool_call, SseEvent::ToolCall { name, .. } if name == "read"));

        let tool_result = SseEvent::parse(
            "tool_result",
            r#"{"name":"read","output":"ok","duration_ms":150}"#,
        );
        assert!(
            matches!(tool_result, SseEvent::ToolResult { name, duration_ms: Some(150), .. } if name == "read")
        );

        let done = SseEvent::parse("done", r#"{"status":"completed"}"#);
        assert!(matches!(done, SseEvent::Done { status } if status == "completed"));

        let error = SseEvent::parse("error", r#"{"message":"timeout"}"#);
        assert!(matches!(error, SseEvent::Error { message } if message == "timeout"));
    }

    #[test]
    fn sse_parser_handles_malformed_json() {
        let delta = SseEvent::parse("text_delta", "not json");
        // Should not panic, falls back to raw data
        assert!(matches!(delta, SseEvent::TextDelta { .. }));
    }

    #[test]
    fn sse_parser_handles_unknown_event() {
        let unknown = SseEvent::parse("custom_event", r#"{"foo":"bar"}"#);
        assert!(
            matches!(unknown, SseEvent::Unknown { event_type, .. } if event_type == "custom_event")
        );
    }
}
