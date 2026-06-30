//! Cross-layer e2e for `thread tail` — the daemon-mediated stream descriptor.
//!
//! Proves the full path the unit tests can't:
//!   1. Run a directive to completion → own a thread.
//!   2. POST /execute `service:threads/tail` → the daemon returns a stream
//!      descriptor (`result.stream = { transport: sse, method: GET, path, follow }`)
//!      pointing at a signed SSE route — it does NOT stream bytes. The default
//!      descriptor follows the braid (the chain route); `thread_only: true`
//!      narrows it to this one thread's route.
//!   3. Sign + GET the thread route → real SSE events (replay of the completed
//!      thread), ending on a terminal event.
//!   4. A non-owner asking for the descriptor gets 404 (no existence leak),
//!      matching the route's own ownership check.

mod common;

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use common::fast_fixture::{register_standard_bundle, write_authorized_key_signed_by, FastFixture};
use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::{Signer, SigningKey};

fn plant_mock_provider(
    project: &Path,
    mock_base_url: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = project.join(".ai/config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"base_url: "{mock_base_url}"
family: chat_completions
body_template:
  model: "{{model}}"
  messages: "{{messages}}"
  tools: "{{tools}}"
  stream: "{{stream}}"
auth: {{}}
headers: {{}}
pricing:
  input_per_million: 0.0
  output_per_million: 0.0
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

fn plant_model_routing(project: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = project.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = r#"tiers:
  general:
    provider: mock
    model: mock-model
    context_window: 200000
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

fn plant_directive(project: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let path = project.join(".ai/directives/test/tail_e2e.md");
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let body = r#"---
name: tail_e2e
category: "test"
description: "thread tail descriptor e2e fixture"
inputs:
  - name: name
    type: string
    required: true
model:
  tier: general
---
Say hello.
"#;
    let signed = lillux::signature::sign_content(body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

fn build_ryeos_signed_auth_headers(
    sk: &SigningKey,
    method: &str,
    path: &str,
    body: &[u8],
    audience: &str,
) -> Vec<(String, String)> {
    let fp = lillux::signature::compute_fingerprint(&sk.verifying_key());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let nonce = format!("{:016x}", rand::random::<u64>());

    let body_hash = lillux::cas::sha256_hex(body);
    let string_to_sign = format!(
        "ryeos-request-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        path,
        body_hash,
        timestamp,
        nonce,
        audience,
    );
    let content_hash = lillux::cas::sha256_hex(string_to_sign.as_bytes());
    let sig: lillux::crypto::Signature = sk.sign(content_hash.as_bytes());
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

    vec![
        ("x-ryeos-key-id".into(), format!("fp:{fp}")),
        ("x-ryeos-timestamp".into(), timestamp),
        ("x-ryeos-nonce".into(), nonce),
        ("x-ryeos-signature".into(), sig_b64),
    ]
}

struct SseEvent {
    event: String,
    id: Option<String>,
}

fn parse_sse_bytes(raw: &[u8]) -> Vec<SseEvent> {
    let text = String::from_utf8_lossy(raw);
    let mut events = Vec::new();
    let mut event = String::new();
    let mut id: Option<String> = None;
    let mut has_data = false;

    for line in text.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("id:") {
            id = Some(rest.trim().to_string());
        } else if line.strip_prefix("data:").is_some() {
            has_data = true;
        } else if line.is_empty() {
            if !event.is_empty() || has_data {
                events.push(SseEvent {
                    event: std::mem::take(&mut event),
                    id: id.take(),
                });
            }
            has_data = false;
        }
    }
    if !event.is_empty() || has_data {
        events.push(SseEvent { event, id });
    }
    events
}

const TERMINAL_EVENTS: [&str; 5] = [
    "thread_completed",
    "thread_failed",
    "thread_cancelled",
    "thread_killed",
    "thread_timed_out",
];

/// Boot a daemon with the standard bundle (provides the thread-events route +
/// the `service:threads/tail` item), run a directive to completion, and return
/// the harness, the owner signing key, the node fingerprint (audience), the
/// completed thread id, and the project path (reused for service dispatch).
/// `extra_keys` are pre-authorized so a non-owner request reaches the handler's
/// ownership branch instead of being rejected by the signature verifier.
async fn boot_and_run_directive(
    extra_keys: &[SigningKey],
) -> (DaemonHarness, SigningKey, String, String, String) {
    let mock = MockProvider::start(vec![
        MockResponse::Text("Hello ".into()),
        MockResponse::Text("world".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();

    let extra_key_bytes: Vec<[u8; 32]> = extra_keys.iter().map(|sk| sk.to_bytes()).collect();
    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_standard_bundle(state_path, fixture)?;
            write_authorized_key_signed_by(state_path, &fixture.user, &fixture.node)?;
            for bytes in &extra_key_bytes {
                let extra = SigningKey::from_bytes(bytes);
                write_authorized_key_signed_by(state_path, &extra, &fixture.node)?;
            }
            Ok(())
        };

    let (h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=warn".into()),
        );
        cmd.env("RYEOS_ALLOW_PROJECT_PROVIDER_CONFIG", "1");
    })
    .await
    .expect("start daemon with standard bundle");

    let user_sk = fixture.user.clone();
    let node_fp = fixture.node_fp();

    let project = tempfile::tempdir().expect("project tempdir");
    plant_mock_provider(project.path(), &mock_url, &fixture.publisher).expect("plant provider");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_directive(project.path(), &fixture.publisher).expect("plant directive");
    let project_path = project.path().to_str().unwrap().to_string();

    let (status, body) = tokio::time::timeout(
        Duration::from_secs(30),
        h.post_execute(
            "directive:test/tail_e2e",
            &project_path,
            serde_json::json!({"name": "World"}),
        ),
    )
    .await
    .expect("/execute timed out")
    .expect("/execute failed");
    assert_eq!(status, reqwest::StatusCode::OK, "/execute body={body:#}");

    let thread_id = body
        .get("thread")
        .and_then(|t| t.get("thread_id"))
        .and_then(|v| v.as_str())
        .expect("thread.thread_id")
        .to_string();

    std::mem::forget(project);
    std::mem::forget(mock);
    (h, user_sk, node_fp, thread_id, project_path)
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_tail_descriptor_round_trip() {
    let (h, user_sk, node_fp, thread_id, project_path) = boot_and_run_directive(&[]).await;

    // 1. Owner POSTs /execute service:threads/tail with thread_only=true → the
    //    daemon returns the single-thread descriptor. (The default follows the
    //    braid, whose chain stream stays open `tail -f`-style and so can't be
    //    read to EOF here — that default is asserted separately, below.)
    let (status, body) = h
        .post_execute(
            "service:threads/tail",
            &project_path,
            serde_json::json!({ "thread_id": thread_id, "thread_only": true }),
        )
        .await
        .expect("post threads.tail");
    assert_eq!(status, reqwest::StatusCode::OK, "threads.tail body={body:#}");

    let stream = body
        .get("result")
        .and_then(|r| r.get("stream"))
        .unwrap_or_else(|| panic!("no result.stream descriptor in {body:#}"));
    assert_eq!(stream.get("transport").and_then(|v| v.as_str()), Some("sse"));
    assert_eq!(stream.get("method").and_then(|v| v.as_str()), Some("GET"));
    assert_eq!(stream.get("follow").and_then(|v| v.as_str()), Some("thread"));
    let path = stream
        .get("path")
        .and_then(|v| v.as_str())
        .expect("descriptor path");
    assert_eq!(path, format!("/threads/{thread_id}/events/stream"));

    // 2. Sign and open the descriptor's path — it must be a real SSE stream that
    //    replays the completed thread's events.
    let audience = format!("fp:{node_fp}");
    let headers = build_ryeos_signed_auth_headers(&user_sk, "GET", path, b"", &audience);
    let url = format!("http://{}{}", h.bind, path);
    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.expect("SSE request failed");
    assert!(
        resp.status().is_success(),
        "descriptor SSE returned {}",
        resp.status()
    );
    let bytes = resp.bytes().await.expect("read SSE body");
    let events = parse_sse_bytes(&bytes);

    assert!(!events.is_empty(), "no SSE events from descriptor path");
    assert!(
        matches!(
            events[0].event.as_str(),
            "thread_created" | "stream_started"
        ),
        "first event should open the stream, got {}",
        events[0].event
    );
    assert!(
        events
            .iter()
            .any(|e| TERMINAL_EVENTS.contains(&e.event.as_str())),
        "expected a terminal event in the replayed stream; got {:?}",
        events.iter().map(|e| &e.event).collect::<Vec<_>>()
    );
    // Persisted events carry monotonic chain_seq ids.
    let mut prev: Option<i64> = None;
    for e in &events {
        if let Some(seq) = e.id.as_ref().and_then(|s| s.parse::<i64>().ok()) {
            if let Some(p) = prev {
                assert!(seq > p, "chain_seq must be monotonic: {seq} <= {p}");
            }
            prev = Some(seq);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_tail_descriptor_defaults_to_braid() {
    // With no flag, `thread tail` follows the braid: the descriptor points at the
    // chain-events route and declares `follow: chain`. The stream is not opened
    // here — a chain stream stays open across continuations and wouldn't EOF.
    let (h, _user_sk, _node_fp, thread_id, project_path) = boot_and_run_directive(&[]).await;

    let (status, body) = h
        .post_execute(
            "service:threads/tail",
            &project_path,
            serde_json::json!({ "thread_id": thread_id }),
        )
        .await
        .expect("post threads.tail");
    assert_eq!(status, reqwest::StatusCode::OK, "threads.tail body={body:#}");

    let stream = body
        .get("result")
        .and_then(|r| r.get("stream"))
        .unwrap_or_else(|| panic!("no result.stream descriptor in {body:#}"));
    assert_eq!(stream.get("follow").and_then(|v| v.as_str()), Some("chain"));
    let path = stream
        .get("path")
        .and_then(|v| v.as_str())
        .expect("descriptor path");
    assert!(
        path.starts_with("/chains/") && path.ends_with("/events/stream"),
        "default tail must point at the chain-events route, got {path}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_tail_descriptor_denied_for_non_owner() {
    let other_sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let (h, _user_sk, node_fp, thread_id, project_path) =
        boot_and_run_directive(std::slice::from_ref(&other_sk)).await;

    // A different (but authorized) principal asks for the descriptor of a thread
    // it does not own. The handler's ownership check must deny it before any
    // descriptor is issued — 404, not a leak of existence.
    let body = serde_json::json!({
        "item_ref": "service:threads/tail",
        "project_path": project_path,
        "parameters": { "thread_id": thread_id },
    });
    let body_bytes = serde_json::to_vec(&body).expect("serialize body");
    let audience = format!("fp:{node_fp}");
    let headers = build_ryeos_signed_auth_headers(&other_sk, "POST", "/execute", &body_bytes, &audience);

    let url = format!("http://{}/execute", h.bind);
    let mut req = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(body_bytes);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.expect("non-owner request failed");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "non-owner must get 404 from service:threads/tail, got {}",
        resp.status()
    );
}
