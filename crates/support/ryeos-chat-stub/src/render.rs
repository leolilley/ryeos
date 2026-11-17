//! Turns a script step into the wire bodies: one JSON document for
//! non-streaming requests, or an ordered list of SSE data payloads.

use serde_json::{Value, json};

use crate::script::{Step, ToolCall, Usage};

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
        message["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(
            calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "type": "function",
                        "function": {"name": c.spec.name, "arguments": c.arguments},
                    })
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

/// The ordered SSE data payloads of a streamed response (without the
/// `data: ` prefix, without the `[DONE]` sentinel).
pub fn stream_frames(step: &Step, n: u64) -> Vec<Value> {
    let shape = &step.stream;
    let mut deltas: Vec<Value> = Vec::new();
    if let Some(reasoning) = &step.reasoning {
        for piece in split_bytes(reasoning, shape.text_chunk_bytes) {
            deltas.push(json!({"reasoning_content": piece}));
        }
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
