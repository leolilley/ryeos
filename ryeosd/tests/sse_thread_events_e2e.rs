//! Phase D — SSE thread-events end-to-end test.
//!
//! Proves the full SSE chain works:
//!   1. Start mock provider
//!   2. Pre-init: trusted signer, standard bundle, mock provider, model routing,
//!      directive, route YAML, node signing key, authorized key file
//!   3. Start daemon
//!   4. POST /execute to run a directive, capture thread_id
//!   5. GET /threads/{thread_id}/events/stream with rye_signed auth
//!   6. Read SSE events until terminal
//!   7. Assert events received correctly

mod common;

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use common::fast_fixture::{register_standard_bundle, write_authorized_key_signed_by, FastFixture};
use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::{Signer, SigningKey};

fn plant_mock_provider(user_space: &Path, mock_base_url: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/rye-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"base_url: "{mock_base_url}"
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
    let dir = user_space.join(".ai/config/rye-runtime");
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
description: "SSE e2e test fixture"
inputs:
  name:
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

fn plant_route_yaml(state_path: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = state_path.join(".ai/node/routes");
    std::fs::create_dir_all(&dir)?;
    let body = r#"section: routes
id: thread/events-stream
path: /threads/{thread_id}/events/stream
methods:
  - GET
auth: rye_signed
limits:
  body_bytes_max: 0
  timeout_ms: 0
  concurrent_max: 64
response:
  mode: event_stream
  source: thread_events
  source_config:
    thread_id: "${path.thread_id}"
    keep_alive_secs: 15
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("thread-events-stream.yaml"), signed)?;
    Ok(())
}

fn build_rye_signed_auth_headers(
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
        ("x-rye-key-id".into(), format!("fp:{fp}")),
        ("x-rye-timestamp".into(), timestamp),
        ("x-rye-nonce".into(), nonce),
        ("x-rye-signature".into(), sig_b64),
    ]
}

struct SseEvent {
    event: String,
    id: Option<String>,
    _data: String,
}

fn parse_sse_bytes(raw: &[u8]) -> Vec<SseEvent> {
    let text = String::from_utf8_lossy(raw);
    let mut events = Vec::new();
    let mut event = String::new();
    let mut id: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();

    for line in text.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("id:") {
            id = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim().to_string());
        } else if line.is_empty() {
            if !event.is_empty() || !data_lines.is_empty() {
                events.push(SseEvent {
                    event: event.clone(),
                    id: id.take(),
                    _data: data_lines.join("\n"),
                });
            }
            event.clear();
            data_lines.clear();
        }
    }

    if !event.is_empty() || !data_lines.is_empty() {
        events.push(SseEvent {
            event,
            id,
            _data: data_lines.join("\n"),
        });
    }

    events
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_thread_events_e2e_live_directive_round_trip() {
    let mock = MockProvider::start(vec![
        MockResponse::Text("Hello ".into()),
        MockResponse::Text("world".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();

    let plant = move |state_path: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_mock_provider(user, &mock_url, &fixture.publisher)?;
        plant_model_routing(user, &fixture.publisher)?;
        plant_directive(user, "test/sse_e2e", "Say hello.", &fixture.publisher)?;
        plant_route_yaml(state_path, &fixture.publisher)?;
        // The hardcoded `/execute` route runs through
        // `auth::auth_middleware`, which only extracts a `Principal`
        // when the daemon was started with `--require-auth`. Tests
        // here don't enable that flag (it would double-replay-check
        // every request because both the middleware AND the
        // `rye_signed` route verifier record nonces in the same
        // `REPLAY_GUARD`). So the unauthenticated POST `/execute`
        // gets the daemon's own identity as the caller principal,
        // and the SSE GET — verified by the route's `rye_signed`
        // verifier — must be signed by that same identity for the
        // thread_events source's `principal_id == requested_by`
        // ownership check to pass.
        write_authorized_key_signed_by(state_path, &fixture.node, &fixture.node)?;
        Ok(())
    };

    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeosd=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with mock + route YAML");

    let node_fp = fixture.node_fp();
    let node_sk = fixture.node;

    let project = tempfile::tempdir().expect("project tempdir");
    let (status, body) = match tokio::time::timeout(
        Duration::from_secs(30),
        h.post_execute(
            "directive:test/sse_e2e",
            project.path().to_str().unwrap(),
            serde_json::json!({"name": "World"}),
        ),
    )
    .await
    {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => panic!("post /execute failed: {e}"),
        Err(_) => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "POST /execute timed out after 30s.\n\
                 --- daemon stderr ---\n{stderr}"
            );
        }
    };

    if status != reqwest::StatusCode::OK {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "expected 200 from execute; got {status}\nbody={body:#}\n--- stderr ---\n{stderr}"
        );
    }

    let result = match body.get("result").cloned() {
        Some(r) => r,
        None => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!("response missing `result`\nbody={body:#}\n--- stderr ---\n{stderr}");
        }
    };
    if result.get("success").and_then(|v| v.as_bool()) != Some(true) {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!("result.success must be true\nbody={body:#}\n--- stderr ---\n{stderr}");
    }

    let thread_id = body
        .get("thread")
        .and_then(|t| t.get("thread_id"))
        .and_then(|v| v.as_str())
        .expect("thread.thread_id")
        .to_string();

    let audience = format!("fp:{node_fp}");
    let sse_path = format!("/threads/{thread_id}/events/stream");
    let headers =
        build_rye_signed_auth_headers(&node_sk, "GET", &sse_path, b"", &audience);

    let url = format!("http://{}{}", h.bind, sse_path);
    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req.send().await.expect("SSE request failed");
    assert!(
        resp.status().is_success(),
        "SSE returned {}: check daemon stderr",
        resp.status()
    );

    let bytes = resp.bytes().await.expect("read SSE body");
    let events = parse_sse_bytes(&bytes);

    assert!(!events.is_empty(), "no SSE events received");

    let first = &events[0];
    assert!(
        matches!(
            first.event.as_str(),
            "thread_created" | "stream_started"
        ),
        "first event must be thread_created or stream_started, got: {}",
        first.event
    );

    let last = events.last().expect("at least one event");
    let terminal_types = [
        "thread_completed",
        "thread_failed",
        "thread_cancelled",
        "thread_killed",
        "thread_timed_out",
    ];
    assert!(
        terminal_types.contains(&last.event.as_str()),
        "last event must be terminal, got: {}",
        last.event
    );

    let mut prev_seq: Option<i64> = None;
    for ev in &events {
        if let Some(ref id) = ev.id {
            let seq: i64 = id.parse().expect("id is numeric");
            if let Some(p) = prev_seq {
                assert!(seq > p, "chain_seq must be monotonic: {seq} <= {p}");
            }
            prev_seq = Some(seq);
        }
    }

    drop(project);
    drop(mock);
}

/// Set up a daemon with the standard SSE thread-events fixture and run a
/// directive end-to-end. Returns `(harness, node_sk, node_fp, thread_id)`.
/// Reused by D.3 reconnect + non-owner tests.
async fn boot_and_run_directive() -> (DaemonHarness, SigningKey, String, String) {
    boot_and_run_directive_with_extra_keys(&[]).await
}

/// Variant of `boot_and_run_directive` that pre-installs additional
/// authorized keys at fixture-plant time. Used by the non-owner test
/// so a second key is recognized by the verifier (avoiding 401 on the
/// rye_signed verifier path) and the request reaches the source's
/// principal-mismatch 404 branch deterministically.
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

    let plant = move |state_path: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_mock_provider(user, &mock_url, &fixture.publisher)?;
        plant_model_routing(user, &fixture.publisher)?;
        plant_directive(user, "test/sse_e2e", "Say hello.", &fixture.publisher)?;
        plant_route_yaml(state_path, &fixture.publisher)?;

        // The hardcoded `/execute` route runs through
        // `auth::auth_middleware`, which only extracts a `Principal`
        // when `--require-auth` is on. The tests here can't enable
        // it (the global middleware + per-route `rye_signed`
        // verifier would record the same nonce twice and 401 the
        // second check). So /execute runs as the daemon's identity
        // and the SSE GET below — verified by the route's
        // `rye_signed` verifier — must be signed by that same
        // identity for `principal_id == requested_by` to pass.
        write_authorized_key_signed_by(state_path, &fixture.node, &fixture.node)?;
        for bytes in &extra_key_bytes {
            let extra = SigningKey::from_bytes(bytes);
            // Authorized-key files MUST be signed by the node
            // identity (auth.rs::load_authorized_key checks
            // signer_fp == node_identity.fingerprint). When the
            // subject is a different key, sign with the node key.
            write_authorized_key_signed_by(state_path, &extra, &fixture.node)?;
        }
        Ok(())
    };

    let (h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with mock + route YAML");

    let node_fp = fixture.node_fp();
    let node_sk = fixture.node;

    let project = tempfile::tempdir().expect("project tempdir");
    let project_path = project.path().to_str().unwrap().to_string();
    let (status, body) = tokio::time::timeout(
        Duration::from_secs(30),
        h.post_execute(
            "directive:test/sse_e2e",
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

    // Hold the project tempdir alive for the duration of the test by
    // leaking it. Tests are short-lived; tempdir cleanup is OS-on-exit.
    std::mem::forget(project);
    std::mem::forget(mock);

    (h, node_sk, node_fp, thread_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_thread_events_returns_404_for_non_owner() {
    // Pre-install BOTH keys at pre-init time so the rye_signed
    // verifier accepts the second key's request. That puts the
    // request through the source's principal-mismatch branch, which
    // is the path we want to assert returns 404 (not 401).
    let other_sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let (h, _node_sk, node_fp, thread_id) =
        boot_and_run_directive_with_extra_keys(std::slice::from_ref(&other_sk)).await;

    // Sign the SSE GET with the OTHER (authorized) key. The
    // `audience` is the daemon's fingerprint; the principal_id is
    // the OTHER key's fingerprint.
    let audience = format!("fp:{node_fp}");
    let sse_path = format!("/threads/{thread_id}/events/stream");
    let headers =
        build_rye_signed_auth_headers(&other_sk, "GET", &sse_path, b"", &audience);

    let url = format!("http://{}{}", h.bind, sse_path);
    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.expect("SSE request failed");

    // Per CONVENTIONS / Phase C invariant: principal mismatch → 404
    // (not 403) so existence of the thread is not leaked.
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "expected 404 for principal mismatch on /threads/{{id}}/events/stream; \
         got {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_thread_events_reconnect_resumes_from_last_event_id() {
    let (h, node_sk, node_fp, thread_id) = boot_and_run_directive().await;

    let audience = format!("fp:{node_fp}");
    let sse_path = format!("/threads/{thread_id}/events/stream");

    // First connect: drain all events, capture their IDs.
    let headers_1 =
        build_rye_signed_auth_headers(&node_sk, "GET", &sse_path, b"", &audience);
    let url = format!("http://{}{}", h.bind, sse_path);
    let client = reqwest::Client::new();

    let mut req = client.get(&url);
    for (k, v) in &headers_1 {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.expect("first SSE request failed");
    assert!(resp.status().is_success(), "first SSE: {}", resp.status());
    let bytes = resp.bytes().await.expect("read first SSE body");
    let events_1 = parse_sse_bytes(&bytes);
    assert!(
        events_1.len() >= 2,
        "need at least 2 events to test resume; got {}",
        events_1.len()
    );

    // Find the ID of the FIRST event that has a numeric id (skip
    // synthetic stream_started which has no id). We'll resume from there;
    // expected outcome = events with chain_seq > resume_from.
    let resume_from: i64 = events_1
        .iter()
        .find_map(|e| e.id.as_ref().and_then(|s| s.parse::<i64>().ok()))
        .expect("at least one persisted event with numeric id");

    // Compute the set of IDs that should still be visible after resume.
    let all_ids: Vec<i64> = events_1
        .iter()
        .filter_map(|e| e.id.as_ref().and_then(|s| s.parse::<i64>().ok()))
        .collect();
    let expected_after: Vec<i64> = all_ids.iter().copied().filter(|&id| id > resume_from).collect();

    // Second connect: same path, with Last-Event-ID = resume_from.
    let headers_2 =
        build_rye_signed_auth_headers(&node_sk, "GET", &sse_path, b"", &audience);
    let mut req2 = client.get(&url);
    for (k, v) in &headers_2 {
        req2 = req2.header(k.as_str(), v.as_str());
    }
    req2 = req2.header("last-event-id", resume_from.to_string());
    let resp2 = req2.send().await.expect("reconnect SSE request failed");
    assert!(
        resp2.status().is_success(),
        "reconnect SSE: {}",
        resp2.status()
    );
    let bytes2 = resp2.bytes().await.expect("read reconnect body");
    let events_2 = parse_sse_bytes(&bytes2);

    // The resumed stream should NOT include any event with id <= resume_from.
    for ev in &events_2 {
        if let Some(ref id_str) = ev.id {
            if let Ok(id) = id_str.parse::<i64>() {
                assert!(
                    id > resume_from,
                    "resumed stream should not yield id={id} (resume_from={resume_from})"
                );
            }
        }
    }

    // The resumed stream MUST yield every persisted id in expected_after.
    let yielded: Vec<i64> = events_2
        .iter()
        .filter_map(|e| e.id.as_ref().and_then(|s| s.parse::<i64>().ok()))
        .collect();
    for id in &expected_after {
        assert!(
            yielded.contains(id),
            "resumed stream missing expected id={id} (yielded={yielded:?})"
        );
    }
}
