//! Turns a script step into the wire bodies: one JSON document for
//! non-streaming requests, or an ordered list of SSE data payloads.

use serde_json::{Value, json};

use crate::script::{Require, Step, ThinkingBlock, ToolCall, Usage};

struct ResolvedCall<'a> {
    id: String,
    spec: &'a ToolCall,
    arguments: String,
}

fn subst(text: &str, n: u64) -> String {
    text.replace("{n}", &n.to_string())
}

fn resolve_calls(step: &Step, n: u64) -> Vec<ResolvedCall<'_>> {
    step.tool_calls
        .iter()
        .enumerate()
        .map(|(i, spec)| ResolvedCall {
            id: spec
                .id
                .as_deref()
                .map(|id| subst(id, n))
                .unwrap_or_else(|| format!("call_{n}_{i}")),
            arguments: match &spec.arguments {
                Value::String(s) => subst(s, n),
                other => other.to_string(),
            },
            spec,
        })
        .collect()
}

fn reasoning_field(step: &Step) -> &str {
    step.reasoning_field
        .as_deref()
        .unwrap_or("reasoning_content")
}

fn finish_reason(step: &Step) -> String {
    step.finish_reason.clone().unwrap_or_else(|| {
        if step.tool_calls.is_empty() {
            "stop".to_string()
        } else {
            "tool_calls".to_string()
        }
    })
}

fn usage_value(step: &Step) -> Value {
    let usage: Usage = step.usage.clone().unwrap_or_default();
    serde_json::to_value(usage).expect("usage serialises")
}

/// The single JSON body of a non-streaming response.
pub fn non_stream_body(step: &Step, n: u64) -> Value {
    let calls = resolve_calls(step, n);
    let mut message = json!({
        "role": "assistant",
        "content": step.text.as_deref().map(|t| subst(t, n)),
    });
    if let Some(reasoning) = &step.reasoning {
        message[reasoning_field(step)] = json!(reasoning);
    }
    if !step.thinking_blocks.is_empty() {
        message["thinking_blocks"] = Value::Array(
            step.thinking_blocks
                .iter()
                .map(|block| match &block.redacted {
                    Some(data) => json!({"type": "redacted_thinking", "data": data}),
                    None => {
                        let mut item = json!({"type": "thinking", "thinking": block.thinking});
                        if let Some(signature) = &block.signature {
                            item["signature"] = json!(signature);
                        }
                        item
                    }
                })
                .collect(),
        );
    }
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(
            calls
                .iter()
                .map(|c| {
                    let mut call = json!({
                        "id": c.id,
                        "type": "function",
                        "function": {"name": c.spec.name, "arguments": c.arguments},
                    });
                    for (key, value) in &c.spec.fields {
                        call[key] = value.clone();
                    }
                    call
                })
                .collect(),
        );
    }
    json!({
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason(step)}],
        "usage": usage_value(step),
    })
}

fn split_bytes(text: &str, size: Option<usize>) -> Vec<String> {
    let Some(size) = size.filter(|s| *s > 0) else {
        return vec![text.to_string()];
    };
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if !cur.is_empty() && cur.len() + ch.len_utf8() > size {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

fn delta_frame(delta: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]})
}

fn thinking_block_deltas(block: &ThinkingBlock, shape: &crate::script::StreamShape) -> Vec<Value> {
    if let Some(data) = &block.redacted {
        return vec![json!({"thinking_blocks": [{"type": "redacted_thinking", "data": data}]})];
    }
    let mut pieces = split_bytes(&block.thinking, shape.thinking_chunk_bytes);
    if block.thinking.is_empty() {
        pieces.clear();
    }
    let mut items: Vec<Value> = pieces
        .into_iter()
        .map(|piece| json!({"type": "thinking", "thinking": piece}))
        .collect();
    if let Some(signature) = &block.signature {
        if shape.signature_last || items.is_empty() {
            items.push(json!({"type": "thinking", "thinking": "", "signature": signature}));
        } else if let Some(last) = items.last_mut() {
            last["signature"] = json!(signature);
        }
    }
    items
        .into_iter()
        .map(|item| json!({"thinking_blocks": [item]}))
        .collect()
}

/// The first request violation of `require`, or `None`. Looks at the
/// assistant messages after the last user message unless the requirement
/// asks for the whole history.
pub fn violated_requirement(require: &Require, request: &Value) -> Option<String> {
    let messages = request.get("messages").and_then(Value::as_array)?;
    let start = if require.all_history {
        0
    } else {
        messages
            .iter()
            .rposition(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .map_or(0, |index| index + 1)
    };
    for (offset, message) in messages[start..].iter().enumerate() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let at = start + offset;
        let calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .filter(|calls| !calls.is_empty());
        for key in &require.assistant_lacks {
            if message.get(key).is_some() {
                return Some(format!(
                    "assistant message {at} must not carry field `{key}`"
                ));
            }
        }
        let Some(calls) = calls else { continue };
        for key in &require.assistant_tool_turn_has {
            if message.get(key).is_none() {
                return Some(format!(
                    "assistant tool-call message {at} is missing required field `{key}`"
                ));
            }
        }
        for key in &require.tool_call_has {
            if calls[0].get(key).is_none() {
                return Some(format!(
                    "first tool call of assistant message {at} is missing required field `{key}`"
                ));
            }
        }
    }
    None
}

/// The ordered SSE data payloads of a streamed response (without the
/// `data: ` prefix, without the `[DONE]` sentinel).
pub fn stream_frames(step: &Step, n: u64) -> Vec<Value> {
    let shape = &step.stream;
    let mut deltas: Vec<Value> = Vec::new();
    if let Some(reasoning) = &step.reasoning {
        for piece in split_bytes(reasoning, shape.text_chunk_bytes) {
            deltas.push(json!({reasoning_field(step): piece}));
        }
    }
    for block in &step.thinking_blocks {
        deltas.extend(thinking_block_deltas(block, shape));
    }
    if let Some(text) = &step.text {
        for piece in split_bytes(&subst(text, n), shape.text_chunk_bytes) {
            deltas.push(json!({"content": piece}));
        }
    }
    for (position, call) in resolve_calls(step, n).iter().enumerate() {
        let pieces = split_bytes(&call.arguments, shape.argument_chunk_bytes);
        for (k, piece) in pieces.into_iter().enumerate() {
            let mut entry = json!({"function": {}});
            if shape.tool_call_index {
                entry["index"] = json!(position);
            }
            if k == 0 {
                entry["type"] = json!("function");
                if shape.tool_call_ids {
                    entry["id"] = json!(call.id);
                }
                entry["function"]["name"] = json!(call.spec.name);
                for (key, value) in &call.spec.fields {
                    entry[key] = value.clone();
                }
            }
            entry["function"]["arguments"] = json!(piece);
            deltas.push(json!({"tool_calls": [entry]}));
        }
    }
    if deltas.is_empty() {
        deltas.push(json!({"content": ""}));
    }
    deltas[0]["role"] = json!("assistant");
    let mut frames: Vec<Value> = deltas.into_iter().map(delta_frame).collect();
    let mut last = json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason(step)}],
    });
    if shape.usage_chunk {
        last["usage"] = usage_value(step);
    }
    frames.push(last);
    frames
}
