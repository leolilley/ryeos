use std::time::{Duration, Instant};

use ryeos_chat_stub::{Script, StubServer};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn serve(yaml: &str) -> StubServer {
    StubServer::start(Script::from_yaml_str(yaml).expect("script")).await
}

async fn open(stub: &StubServer, method: &str, path: &str, body: &str) -> TcpStream {
    let addr = stub.base_url.trim_start_matches("http://").to_string();
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: stub\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    s
}

async fn read_all(s: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn post(stub: &StubServer, body: Value) -> String {
    let mut s = open(stub, "POST", "/chat/completions", &body.to_string()).await;
    read_all(&mut s).await
}

fn json_body(raw: &str) -> Value {
    serde_json::from_str(raw.split("\r\n\r\n").nth(1).expect("body").trim()).expect("json body")
}

fn data_frames(raw: &str) -> Vec<Value> {
    raw.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|l| *l != "[DONE]")
        .map(|l| serde_json::from_str(l).expect("frame json"))
        .collect()
}

#[tokio::test]
async fn non_stream_text_with_usage_and_recorded_request() {
    let stub = serve(
        "steps:\n  - text: hi\n    usage: { prompt_tokens: 7, completion_tokens: 2, cached: 3 }\n",
    )
    .await;
    let req = json!({"model": "m", "messages": [{"role": "user", "content": "x"}]});
    let raw = post(&stub, req.clone()).await;
    assert!(raw.starts_with("HTTP/1.1 200"));
    let body = json_body(&raw);
    assert_eq!(body["choices"][0]["message"]["content"], "hi");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(
        body["usage"],
        json!({"prompt_tokens": 7, "completion_tokens": 2, "cached": 3})
    );
    assert_eq!(stub.request_bodies().await, vec![req]);
    let mut s = open(&stub, "GET", "/_stub/requests", "").await;
    let listed = json_body(&read_all(&mut s).await);
    assert_eq!(listed[0]["body"]["model"], "m");
    assert_eq!(listed[0]["seq"], 0);
}

#[tokio::test]
async fn stream_parallel_tool_calls_without_index_or_ids() {
    let stub = serve(
        "steps:\n  - tool_calls:\n      - { name: a, arguments: { k: 1 } }\n      - { name: b, arguments: '{\"z\":2}' }\n    stream: { tool_call_index: false, tool_call_ids: false, argument_chunk_bytes: 4 }\n",
    )
    .await;
    let raw = post(&stub, json!({"stream": true})).await;
    assert!(raw.contains("text/event-stream"));
    assert!(raw.trim_end().ends_with("0") || raw.contains("[DONE]"));
    let frames = data_frames(&raw);
    let calls: Vec<&Value> = frames
        .iter()
        .filter_map(|f| f["choices"][0]["delta"]["tool_calls"].get(0))
        .collect();
    assert!(calls.len() > 2, "arguments split over several events");
    assert!(
        calls
            .iter()
            .all(|c| c.get("index").is_none() && c.get("id").is_none())
    );
    let args_a: String = calls
        .iter()
        .take_while(|c| c["function"]["name"] != "b")
        .map(|c| c["function"]["arguments"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(args_a, "{\"k\":1}");
    let last = frames.last().unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(last["usage"]["prompt_tokens"], 10);
    assert_eq!(frames[0]["choices"][0]["delta"]["role"], "assistant");
}

#[tokio::test]
async fn streamed_usage_reports_cache_fields_top_level_and_nested() {
    let stub = serve(
        "steps:\n  - text: hi\n    usage:\n      prompt_tokens: 2100\n      completion_tokens: 20\n      cache_read_input_tokens: 2013\n      cache_creation_input_tokens: 0\n      prompt_tokens_details: { cached_tokens: 2013 }\n",
    )
    .await;
    let raw = post(&stub, json!({"stream": true})).await;
    let frames = data_frames(&raw);
    let usage = &frames.last().unwrap()["usage"];
    assert_eq!(usage["prompt_tokens"], 2100);
    assert_eq!(usage["cache_read_input_tokens"], 2013);
    assert_eq!(usage["cache_creation_input_tokens"], 0);
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 2013);
}

#[tokio::test]
async fn stream_tool_calls_carry_index_and_ids_by_default() {
    let stub =
        serve("steps:\n  - tool_calls:\n      - { name: a }\n      - { id: x, name: b }\n").await;
    let frames = data_frames(&post(&stub, json!({"stream": true})).await);
    let c0 = &frames[0]["choices"][0]["delta"]["tool_calls"][0];
    let c1 = &frames[1]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(
        (c0["index"].as_u64(), c0["id"].as_str()),
        (Some(0), Some("call_0_0"))
    );
    assert_eq!(
        (c1["index"].as_u64(), c1["id"].as_str()),
        (Some(1), Some("x"))
    );
}

#[tokio::test]
async fn http_failures_carry_status_and_body() {
    let stub = serve(
        "steps:\n  - fail: { status: 504, body: gw }\n  - fail: { status: 529, body: busy }\n  - fail: { status: 500, body: boom }\n  - fail: { status: 429, body: slow }\n",
    )
    .await;
    for (status, body) in [(504, "gw"), (529, "busy"), (500, "boom"), (429, "slow")] {
        let raw = post(&stub, json!({"stream": true})).await;
        assert!(raw.starts_with(&format!("HTTP/1.1 {status}")), "{raw}");
        assert!(raw.contains(body));
    }
}

#[tokio::test]
async fn a_failure_can_carry_a_retry_after_header() {
    let stub = serve(
        "steps:\n  - fail: { status: 429, body: slow, retry_after: \"7\" }\n  - fail: { status: 429, body: slow }\n",
    )
    .await;
    let with = post(&stub, json!({"stream": true}))
        .await
        .to_ascii_lowercase();
    assert!(with.contains("retry-after: 7"), "{with}");
    let without = post(&stub, json!({"stream": true}))
        .await
        .to_ascii_lowercase();
    assert!(!without.contains("retry-after"), "{without}");
}

#[tokio::test]
async fn length_finish_reason_streams_and_non_streams() {
    let stub = serve("steps:\n  - { text: cut, finish_reason: length }\n  - { text: cut, finish_reason: length }\n").await;
    assert_eq!(
        json_body(&post(&stub, json!({})).await)["choices"][0]["finish_reason"],
        "length"
    );
    let frames = data_frames(&post(&stub, json!({"stream": true})).await);
    assert_eq!(
        frames.last().unwrap()["choices"][0]["finish_reason"],
        "length"
    );
}

#[tokio::test]
async fn mid_stream_cut_aborts_without_done_or_chunk_terminator() {
    let stub = serve("steps:\n  - text: abcdefghij\n    cut_after_chunks: 2\n    stream: { text_chunk_bytes: 2 }\n").await;
    let raw = post(&stub, json!({"stream": true})).await;
    assert_eq!(data_frames(&raw).len(), 2);
    assert!(!raw.contains("[DONE]"));
    assert!(!raw.ends_with("0\r\n\r\n"));
}

#[tokio::test]
async fn stall_sends_headers_then_pauses_then_resumes() {
    let stub = serve("steps:\n  - text: late\n    stall: { after_chunks: 0, ms: 600 }\n").await;
    let started = Instant::now();
    let mut s = open(
        &stub,
        "POST",
        "/chat/completions",
        &json!({"stream": true}).to_string(),
    )
    .await;
    let mut buf = [0u8; 1024];
    let n = s.read(&mut buf).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "headers arrive before the stall ends"
    );
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    let rest = read_all(&mut s).await;
    assert!(started.elapsed() >= Duration::from_millis(600));
    assert!(rest.contains("late") && rest.contains("[DONE]"));
}

#[tokio::test]
async fn delay_holds_back_the_first_byte() {
    let stub = serve("steps:\n  - { text: slow, delay_ms: 500 }\n").await;
    let started = Instant::now();
    let mut s = open(&stub, "POST", "/chat/completions", "{}").await;
    let mut buf = [0u8; 16];
    let n = s.read(&mut buf).await.unwrap();
    assert!(n > 0);
    assert!(started.elapsed() >= Duration::from_millis(500));
}

#[tokio::test]
async fn repeat_substitutes_ordinal_and_exhaustion_is_500() {
    let stub = serve("steps:\n  - repeat: 3\n    tool_calls: [{ id: 'c{n}', name: noop }]\n").await;
    for i in 0..3 {
        let body = json_body(&post(&stub, json!({})).await);
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["id"],
            format!("c{i}")
        );
    }
    let raw = post(&stub, json!({})).await;
    assert!(raw.starts_with("HTTP/1.1 500") && raw.contains("exhausted"));
    assert_eq!(stub.requests().await.len(), 4);
}

#[test]
fn checked_in_example_script_parses() {
    let script = Script::from_yaml_str(include_str!("../examples/failure-menu.yaml")).unwrap();
    assert_eq!(script.steps.last().unwrap().repeat, 100);
}

fn thinking_items(frames: &[Value]) -> Vec<Value> {
    frames
        .iter()
        .filter_map(|f| f["choices"][0]["delta"]["thinking_blocks"].get(0).cloned())
        .collect()
}

#[tokio::test]
async fn reasoning_field_names_the_delta_key() {
    let stub = serve(
        "steps:\n  - { reasoning: why, reasoning_field: reasoning, text: ok }\n  - { reasoning: why, text: ok }\n  - { reasoning: why, reasoning_field: reasoning }\n",
    )
    .await;
    let frames = data_frames(&post(&stub, json!({"stream": true})).await);
    assert_eq!(frames[0]["choices"][0]["delta"]["reasoning"], "why");
    assert!(
        frames[0]["choices"][0]["delta"]
            .get("reasoning_content")
            .is_none()
    );
    let frames = data_frames(&post(&stub, json!({"stream": true})).await);
    assert_eq!(frames[0]["choices"][0]["delta"]["reasoning_content"], "why");
    let body = json_body(&post(&stub, json!({})).await);
    assert_eq!(body["choices"][0]["message"]["reasoning"], "why");
}

#[tokio::test]
async fn thinking_block_signature_rides_the_last_piece_or_its_own_chunk() {
    let stub = serve(
        "steps:\n  - thinking_blocks: [{ thinking: abcd, signature: sig }]\n    stream: { thinking_chunk_bytes: 2 }\n  - thinking_blocks: [{ thinking: abcd, signature: sig }]\n    stream: { thinking_chunk_bytes: 2, signature_last: true }\n",
    )
    .await;
    let items = thinking_items(&data_frames(&post(&stub, json!({"stream": true})).await));
    assert_eq!(
        items,
        vec![
            json!({"type": "thinking", "thinking": "ab"}),
            json!({"type": "thinking", "thinking": "cd", "signature": "sig"}),
        ]
    );
    let items = thinking_items(&data_frames(&post(&stub, json!({"stream": true})).await));
    assert_eq!(
        items,
        vec![
            json!({"type": "thinking", "thinking": "ab"}),
            json!({"type": "thinking", "thinking": "cd"}),
            json!({"type": "thinking", "thinking": "", "signature": "sig"}),
        ]
    );
}

#[tokio::test]
async fn unsigned_and_redacted_blocks_have_their_own_shapes() {
    let stub = serve(
        "steps:\n  - thinking_blocks: [{ thinking: open }, { redacted: 'zz==' }]\n  - thinking_blocks: [{ thinking: t, signature: s }, { redacted: 'zz==' }]\n",
    )
    .await;
    let items = thinking_items(&data_frames(&post(&stub, json!({"stream": true})).await));
    assert_eq!(
        items,
        vec![
            json!({"type": "thinking", "thinking": "open"}),
            json!({"type": "redacted_thinking", "data": "zz=="}),
        ]
    );
    let body = json_body(&post(&stub, json!({})).await);
    assert_eq!(
        body["choices"][0]["message"]["thinking_blocks"],
        json!([
            {"type": "thinking", "thinking": "t", "signature": "s"},
            {"type": "redacted_thinking", "data": "zz=="}
        ])
    );
}

#[tokio::test]
async fn tool_call_fields_ride_the_first_streamed_entry_only() {
    let stub = serve(
        "steps:\n  - tool_calls:\n      - { name: a, arguments: { k: 1 }, fields: { extra_content: { sig: abc } } }\n      - { name: b }\n    stream: { argument_chunk_bytes: 3 }\n",
    )
    .await;
    let frames = data_frames(&post(&stub, json!({"stream": true})).await);
    let entries: Vec<&Value> = frames
        .iter()
        .filter_map(|f| f["choices"][0]["delta"]["tool_calls"].get(0))
        .collect();
    let with_extra: Vec<&&Value> = entries
        .iter()
        .filter(|e| e.get("extra_content").is_some())
        .collect();
    assert_eq!(with_extra.len(), 1);
    assert_eq!(with_extra[0]["function"]["name"], "a");
    assert_eq!(with_extra[0]["extra_content"], json!({"sig": "abc"}));
}

#[tokio::test]
async fn a_violated_requirement_is_a_recorded_400() {
    let stub = serve(
        "steps:\n  - { text: a, require: { assistant_tool_turn_has: [thinking_blocks] } }\n  - { text: b, require: { assistant_tool_turn_has: [thinking_blocks] } }\n  - { text: c, require: { assistant_lacks: [reasoning_content] } }\n  - { text: d, require: { tool_call_has: [extra_content] } }\n",
    )
    .await;
    let tool_turn = |extra: Value| {
        let mut message = json!({
            "role": "assistant",
            "tool_calls": [{"id": "c", "type": "function", "function": {"name": "n", "arguments": "{}"}}]
        });
        for (k, v) in extra.as_object().unwrap() {
            message[k] = v.clone();
        }
        message
    };
    let request =
        |assistant: Value| json!({"messages": [{"role": "user", "content": "go"}, assistant]});
    let raw = post(&stub, request(tool_turn(json!({})))).await;
    assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
    assert!(raw.contains("thinking_blocks"));
    let raw = post(&stub, request(tool_turn(json!({"thinking_blocks": []})))).await;
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    let raw = post(&stub, request(tool_turn(json!({"reasoning_content": "x"})))).await;
    assert!(raw.starts_with("HTTP/1.1 400") && raw.contains("reasoning_content"));
    let raw = post(&stub, request(tool_turn(json!({})))).await;
    assert!(raw.starts_with("HTTP/1.1 400") && raw.contains("extra_content"));
    let recorded = stub.requests().await;
    assert!(recorded[0].rejected.is_some() && recorded[1].rejected.is_none());
}

#[tokio::test]
async fn requirements_check_only_the_open_loop_unless_asked() {
    let stub = serve(
        "steps:\n  - { text: a, require: { assistant_tool_turn_has: [thinking_blocks] } }\n  - { text: b, require: { assistant_tool_turn_has: [thinking_blocks], all_history: true } }\n",
    )
    .await;
    let old_turn = json!({
        "role": "assistant",
        "tool_calls": [{"id": "c", "type": "function", "function": {"name": "n", "arguments": "{}"}}]
    });
    let request = json!({"messages": [old_turn, {"role": "user", "content": "next"}]});
    assert!(
        post(&stub, request.clone())
            .await
            .starts_with("HTTP/1.1 200")
    );
    assert!(post(&stub, request).await.starts_with("HTTP/1.1 400"));
}
