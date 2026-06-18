//! Chain-tail SSE end-to-end test.
//!
//! The `chain_tail` source tails a whole chain (`/chains/{chain_root_id}/
//! events/stream`): it replays chain-scoped events, follows the live head,
//! and — unlike a single-thread subscription — does NOT close when one head
//! settles, because the chain may continue. So this test reads incrementally
//! (per-chunk, timeout-bounded) and stops once it has seen the terminal
//! event, rather than blocking on a full-body read that would never end.

mod common;

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use common::fast_fixture::{register_standard_bundle, write_authorized_key_signed_by, FastFixture};
use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::{Signer, SigningKey};

fn plant_mock_provider(
    user_space: &Path,
    mock_base_url: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/ryeos-runtime/model-providers");
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

fn plant_model_routing(user_space: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/ryeos-runtime");
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

fn plant_directive(
    user_space: &Path,
    rel_path: &str,
    body_text: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = user_space.join(format!(".ai/directives/{rel_path}.md"));
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let body = format!(
        r#"---
name: {stem}
category: "{dir_relative}"
description: "chain-tail e2e test fixture"
inputs:
  - name: name
    type: string
    required: true
model:
  tier: general
---
{body_text}
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
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

#[derive(Clone)]
struct SseEvent {
    event: String,
    id: Option<String>,
}

fn parse_sse_bytes(raw: &[u8]) -> Vec<SseEvent> {
    let text = String::from_utf8_lossy(raw);
    let mut events = Vec::new();
    let mut event = String::new();
    let mut id: Option<String> = None;
    let mut have_data = false;

    for line in text.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("id:") {
            id = Some(rest.trim().to_string());
        } else if line.strip_prefix("data:").is_some() {
            have_data = true;
        } else if line.is_empty() && (!event.is_empty() || have_data) {
            events.push(SseEvent {
                event: std::mem::take(&mut event),
                id: id.take(),
            });
            have_data = false;
        }
    }
    if !event.is_empty() || have_data {
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

/// Read the SSE response incrementally until a terminal event is seen or the
/// overall deadline elapses, then return the parsed events. Chain-tail keeps
/// the stream open after a terminal head, so a full-body read would hang.
async fn read_until_terminal(mut resp: reqwest::Response, deadline: Duration) -> Vec<SseEvent> {
    let mut buf: Vec<u8> = Vec::new();
    let overall = tokio::time::Instant::now() + deadline;
    loop {
        let remaining = overall.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining.min(Duration::from_secs(2)), resp.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                buf.extend_from_slice(&chunk);
                let events = parse_sse_bytes(&buf);
                if events
                    .iter()
                    .any(|e| TERMINAL_EVENTS.contains(&e.event.as_str()))
                {
                    return events;
                }
            }
            Ok(Ok(None)) => break, // stream closed
            Ok(Err(_)) => break,   // transport error
            Err(_) => continue,    // per-chunk timeout; re-check deadline
        }
    }
    parse_sse_bytes(&buf)
}

async fn boot_and_run_directive_with_extra_keys(
    extra_keys: &[SigningKey],
) -> (DaemonHarness, SigningKey, String, String) {
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
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
        // Provider/routing/directive are resolved from the project root.
        cmd.env("RYEOS_ALLOW_PROJECT_PROVIDER_CONFIG", "1");
    })
    .await
    .expect("start daemon with mock + route YAML");

    let user_sk = fixture.user.clone();
    let node_fp = fixture.node_fp();

    // Dispatch resolves items from the project root (+ installed bundles), not
    // from HOME — plant provider, routing, and the directive into the project
    // that we pass as `project_path`.
    let project = tempfile::tempdir().expect("project tempdir");
    plant_mock_provider(project.path(), &mock_url, &fixture.publisher).expect("plant provider");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_directive(
        project.path(),
        "test/chain_tail_e2e",
        "Say hello.",
        &fixture.publisher,
    )
    .expect("plant directive");
    let project_path = project.path().to_str().unwrap().to_string();
    let (status, body) = tokio::time::timeout(
        Duration::from_secs(30),
        h.post_execute(
            "directive:test/chain_tail_e2e",
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
    (h, user_sk, node_fp, thread_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn chain_tail_tails_root_chain_and_delivers_terminal() {
    let (h, user_sk, node_fp, thread_id) = boot_and_run_directive_with_extra_keys(&[]).await;

    // For a root thread, chain_root_id == thread_id.
    let audience = format!("fp:{node_fp}");
    let sse_path = format!("/chains/{thread_id}/events/stream");
    let headers = build_ryeos_signed_auth_headers(&user_sk, "GET", &sse_path, b"", &audience);

    let url = format!("http://{}{}", h.bind, sse_path);
    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.expect("chain SSE request failed");
    assert!(resp.status().is_success(), "chain SSE: {}", resp.status());

    let events = read_until_terminal(resp, Duration::from_secs(20)).await;
    assert!(!events.is_empty(), "no chain SSE events received");

    assert_eq!(
        events[0].event, "stream_started",
        "chain tail opens with stream_started, got: {}",
        events[0].event
    );
    assert!(
        events
            .iter()
            .any(|e| TERMINAL_EVENTS.contains(&e.event.as_str())),
        "chain tail must deliver a terminal event; got: {:?}",
        events.iter().map(|e| &e.event).collect::<Vec<_>>()
    );

    // chain_seq carried on `id` must be monotonic.
    let mut prev: Option<i64> = None;
    for ev in &events {
        if let Some(seq) = ev.id.as_ref().and_then(|s| s.parse::<i64>().ok()) {
            if let Some(p) = prev {
                assert!(seq > p, "chain_seq must be monotonic: {seq} <= {p}");
            }
            prev = Some(seq);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn chain_tail_returns_404_for_non_owner() {
    let other_sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let (h, _user_sk, node_fp, thread_id) =
        boot_and_run_directive_with_extra_keys(std::slice::from_ref(&other_sk)).await;

    let audience = format!("fp:{node_fp}");
    let sse_path = format!("/chains/{thread_id}/events/stream");
    let headers = build_ryeos_signed_auth_headers(&other_sk, "GET", &sse_path, b"", &audience);

    let url = format!("http://{}{}", h.bind, sse_path);
    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.expect("chain SSE request failed");

    // Principal mismatch → 404 (do not leak chain existence).
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "expected 404 for non-owner on /chains/{{id}}/events/stream; got {}",
        resp.status()
    );
}
