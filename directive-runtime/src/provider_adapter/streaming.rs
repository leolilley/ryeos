use std::collections::HashMap;

use serde_json::{json, Value};

use crate::directive::StreamEvent;

pub fn parse_sse_events(data: &str, mode: Option<&str>) -> Vec<StreamEvent> {
    let raw_events = split_sse_events(data);
    let mut events = Vec::new();
    let mut tool_call_state: HashMap<String, String> = HashMap::new();

    for (event_type, payload) in raw_events {
        if payload == "[DONE]" {
            events.push(StreamEvent::Done);
            continue;
        }

        let parsed: Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match mode {
            Some("event_typed") => {
                parse_event_typed(&event_type, &parsed, &mut events, &mut tool_call_state);
            }
            Some("delta_merge") => {
                parse_delta_merge(&parsed, &mut events, &mut tool_call_state);
            }
            Some("complete_chunks") => {
                parse_complete_chunks(&parsed, &mut events);
            }
            _ => {}
        }
    }

    events
}

fn split_sse_events(data: &str) -> Vec<(String, String)> {
    let mut events = Vec::new();
    let mut current_event_type = String::new();
    let mut current_data = String::new();

    for line in data.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if !current_data.is_empty() {
                events.push((current_event_type.clone(), current_data.trim().to_string()));
                current_event_type.clear();
                current_data.clear();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            current_event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !current_data.is_empty() {
                current_data.push('\n');
            }
            current_data.push_str(rest.trim());
        }
        if line.strip_prefix("id:").is_some() || line.strip_prefix("retry:").is_some() {
        }
    }
    if !current_data.is_empty() {
        events.push((current_event_type, current_data.trim().to_string()));
    }

    events
}

fn parse_event_typed(
    event_type: &str,
    parsed: &Value,
    events: &mut Vec<StreamEvent>,
    tool_call_state: &mut HashMap<String, String>,
) {
    match event_type {
        "content_block_delta" => {
            if let Some(delta) = parsed.get("delta") {
                if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                    events.push(StreamEvent::Delta(text.to_string()));
                } else if let Some(input) = delta.get("partial_json").and_then(|j| j.as_str()) {
                    if let Some(id) = tool_call_state.get("current_tool_id").cloned() {
                        if let Some(name) = tool_call_state.get("current_tool_name").cloned() {
                            tool_call_state
                                .entry("current_tool_args".to_string())
                                .and_modify(|e| e.push_str(input))
                                .or_insert_with(|| input.to_string());
                        }
                    }
                }
            }
        }
        "content_block_start" => {
            if let Some(content_block) = parsed.get("content_block") {
                if let Some("tool_use") = content_block.get("type").and_then(|t| t.as_str()) {
                    let id = content_block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = content_block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    tool_call_state.insert("current_tool_id".to_string(), id);
                    tool_call_state.insert("current_tool_name".to_string(), name);
                    tool_call_state.remove("current_tool_args");
                }
            }
        }
        "content_block_stop" => {
            if tool_call_state.contains_key("current_tool_id") {
                let id = tool_call_state
                    .get("current_tool_id")
                    .cloned()
                    .unwrap_or_default();
                let name = tool_call_state
                    .get("current_tool_name")
                    .cloned()
                    .unwrap_or_default();
                let arguments = tool_call_state
                    .get("current_tool_args")
                    .cloned()
                    .unwrap_or_else(|| "{}".to_string());
                events.push(StreamEvent::ToolUse { id, name, arguments });
                tool_call_state.remove("current_tool_id");
                tool_call_state.remove("current_tool_name");
                tool_call_state.remove("current_tool_args");
            }
        }
        "message_stop" => {
            events.push(StreamEvent::Done);
        }
        _ => {}
    }
}

fn parse_delta_merge(
    parsed: &Value,
    events: &mut Vec<StreamEvent>,
    tool_call_state: &mut HashMap<String, String>,
) {
    if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
        if let Some(choice) = choices.first() {
            if let Some(content) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str())
            {
                events.push(StreamEvent::Delta(content.to_string()));
            }

            if let Some(tool_calls) = choice
                .get("delta")
                .and_then(|d| d.get("tool_calls"))
                .and_then(|tc| tc.as_array())
            {
                for tc in tool_calls {
                    let index = tc
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                        .to_string();

                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        tool_call_state.insert(format!("tool_id_{}", index), id.to_string());
                    }
                    if let Some(func) = tc.get("function") {
                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                            tool_call_state.insert(format!("tool_name_{}", index), name.to_string());
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                            tool_call_state
                                .entry(format!("tool_args_{}", index))
                                .and_modify(|e| e.push_str(args))
                                .or_insert_with(|| args.to_string());
                        }
                    }
                }
            }

            if let Some("stop") = choice.get("finish_reason").and_then(|f| f.as_str()) {
                let mut indices: Vec<u64> = Vec::new();
                for (key, _) in tool_call_state.iter() {
                    if let Some(idx) = key.strip_prefix("tool_id_") {
                        if let Ok(n) = idx.parse::<u64>() {
                            indices.push(n);
                        }
                    }
                }
                indices.sort();
                for idx in indices.iter() {
                    let id = tool_call_state
                        .get(&format!("tool_id_{}", idx))
                        .cloned()
                        .unwrap_or_default();
                    let name = tool_call_state
                        .get(&format!("tool_name_{}", idx))
                        .cloned()
                        .unwrap_or_default();
                    let arguments = tool_call_state
                        .get(&format!("tool_args_{}", idx))
                        .cloned()
                        .unwrap_or_else(|| "{}".to_string());
                    events.push(StreamEvent::ToolUse { id, name, arguments });
                }
                for idx in indices.iter() {
                    let idx_s = idx.to_string();
                    tool_call_state.remove(&format!("tool_id_{}", idx_s));
                    tool_call_state.remove(&format!("tool_name_{}", idx_s));
                    tool_call_state.remove(&format!("tool_args_{}", idx_s));
                }
                events.push(StreamEvent::Done);
            }
        }
    }
}

fn parse_complete_chunks(parsed: &Value, events: &mut Vec<StreamEvent>) {
    if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
        if let Some(choice) = choices.first() {
            if let Some(message) = choice.get("message") {
                if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
                    events.push(StreamEvent::Delta(content.to_string()));
                }
                if let Some(tool_calls) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
                    for tc in tool_calls {
                        let id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name").and_then(|v| v.as_str()))
                            .unwrap_or("")
                            .to_string();
                        let arguments = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}")
                            .to_string();
                        events.push(StreamEvent::ToolUse { id, name, arguments });
                    }
                }
            }
            if let Some("stop") = choice.get("finish_reason").and_then(|f| f.as_str()) {
                events.push(StreamEvent::Done);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_event_typed_anthropic() {
        let data = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","role":"assistant"}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_stop
data: {"type":"message_stop"}
"#;
        let events = parse_sse_events(data, Some("event_typed"));
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], StreamEvent::Delta(s) if s == "Hello"));
        assert!(matches!(&events[1], StreamEvent::Delta(s) if s == " world"));
        assert!(matches!(&events[2], StreamEvent::Done));
    }

    #[test]
    fn sse_event_typed_tool_use() {
        let data = r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"ls\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_stop
data: {"type":"message_stop"}
"#;
        let events = parse_sse_events(data, Some("event_typed"));
        let tool_uses: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::ToolUse { .. })).collect();
        assert_eq!(tool_uses.len(), 1);
        if let StreamEvent::ToolUse { id, name, arguments } = &tool_uses[0] {
            assert_eq!(id, "toolu_1");
            assert_eq!(name, "bash");
            assert_eq!(arguments, r#"{"cmd":"ls"}"#);
        }
    }

    #[test]
    fn sse_delta_merge_openai() {
        let data = r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]
"#;
        let events = parse_sse_events(data, Some("delta_merge"));
        let deltas: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::Delta(_))).collect();
        let dones: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::Done)).collect();
        assert_eq!(deltas.len(), 2);
        assert!(matches!(&deltas[0], StreamEvent::Delta(s) if s == "Hello"));
        assert!(matches!(&deltas[1], StreamEvent::Delta(s) if s == " world"));
        assert!(!dones.is_empty());
    }

    #[test]
    fn sse_delta_merge_tool_calls() {
        let data = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\""}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"ls\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"stop"}]}
"#;
        let events = parse_sse_events(data, Some("delta_merge"));
        let tool_uses: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::ToolUse { .. })).collect();
        assert_eq!(tool_uses.len(), 1);
        if let StreamEvent::ToolUse { id, name, arguments } = &tool_uses[0] {
            assert_eq!(id, "call_1");
            assert_eq!(name, "bash");
            assert_eq!(arguments, r#"{"cmd":"ls"}"#);
        }
    }

    #[test]
    fn sse_complete_chunks() {
        let data = r#"data: {"choices":[{"message":{"role":"assistant","content":"Full response"},"finish_reason":"stop"}]}
"#;
        let events = parse_sse_events(data, Some("complete_chunks"));
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Delta(s) if s == "Full response"));
        assert!(matches!(&events[1], StreamEvent::Done));
    }

    #[test]
    fn done_sentinel() {
        let data = "data: [DONE]\n\ndata: [DONE]\n\n";
        let events = parse_sse_events(data, Some("delta_merge"));
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Done));
        assert!(matches!(&events[1], StreamEvent::Done));
    }

    #[test]
    fn blank_line_splitting() {
        let data = "data: {\"a\":1}\n\ndata: {\"b\":2}\n\n";
        let events = split_sse_events(data);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, r#"{"a":1}"#);
        assert_eq!(events[1].1, r#"{"b":2}"#);
    }

    #[test]
    fn data_line_concatenation() {
        let data = "data: part1\ndata: part2\n\ndata: second\n\n";
        let events = split_sse_events(data);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, "part1\npart2");
        assert_eq!(events[1].1, "second");
    }

    #[test]
    fn no_mode_returns_empty() {
        let data = "data: {\"a\":1}\n\n";
        let events = parse_sse_events(data, None);
        assert!(events.is_empty());
    }

    #[test]
    fn event_type_tracking() {
        let data = "event: my_event\ndata: {\"x\":1}\n\n";
        let events = split_sse_events(data);
        assert_eq!(events[0].0, "my_event");
    }

    #[test]
    fn incomplete_data_no_final_event() {
        let data = "data: {\"a\":1}";
        let events = split_sse_events(data);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, r#"{"a":1}"#);
    }
}
