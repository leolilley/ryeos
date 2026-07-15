//! Live SSE delivery end-to-end tests.
//!
//! The other SSE e2e files run the directive to completion *before* reading the
//! stream, so they prove replay + terminal delivery but not that events reach a
//! subscriber *incrementally, while the thread is still running*. These tests
//! pin that live property by giving the mock provider an artificial response
//! delay and asserting on wall-clock arrival times:
//!
//!   * the stream's opening event arrives promptly on connect, and
//!   * the terminal event arrives only *later* — once the delayed provider
//!     call settles and the lifecycle service publishes it live.
//!
//! If delivery were buffered until the thread completed, the opening event and
//! the terminal would arrive together (no gap). The enforced gap is the proof
//! of live, incremental publish.
//!
//! Item layout follows the working `directive_runtime_e2e` pattern: provider,
//! model routing, and the directive are planted into the *project* (passed as
//! `project_path`), since dispatch resolves items from the project + installed
//! bundles, not from the user HOME.

mod common;

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use common::fast_fixture::{register_standard_bundle, write_authorized_key_signed_by, FastFixture};
use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::{Signer, SigningKey};
use tokio::time::Instant;

/// Provider latency that keeps the thread observably `running` while we attach
/// a subscriber. Large enough that the terminal cannot arrive bundled with the
/// opening event on any reasonable machine.
const PROVIDER_DELAY: Duration = Duration::from_millis(2000);

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

fn plant_directive(
    project: &Path,
    rel_path: &str,
    body_text: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = project.join(format!(".ai/directives/{rel_path}.md"));
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
description: "live SSE e2e test fixture"
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

/// Plant the `/execute/stream` (gateway / `dispatch_launch`) route.
fn plant_execute_stream_route(state_path: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = state_path.join(".ai/node/routes");
    std::fs::create_dir_all(&dir)?;
    let body = r#"id: execute/stream
path: /execute/stream
methods:
  - POST
auth: ryeos_signed
limits:
  body_bytes_max: 1048576
  timeout_ms: 0
  concurrent_max: 32
request:
  body: json
response:
  mode: event_stream
  source: dispatch_launch
  source_config:
    keep_alive_secs: 15
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("execute-stream.yaml"), signed)?;
    Ok(())
}

/// Plant provider + routing + directive into the project that will be passed as
/// `project_path`, so dispatch resolves the directive and the runtime resolves
/// the provider from the same root.
fn plant_project_items(project: &Path, mock_url: &str, signer: &SigningKey) {
    plant_mock_provider(project, mock_url, signer).expect("plant provider");
    plant_model_routing(project, signer).expect("plant routing");
    plant_directive(project, "test/live_e2e", "Say hello.", signer).expect("plant directive");
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
struct TimedEvent {
    event: String,
    data: String,
    /// When the chunk that completed this event arrived.
    at: Instant,
}

const TERMINAL_EVENTS: [&str; 5] = [
    "thread_completed",
    "thread_failed",
    "thread_cancelled",
    "thread_killed",
    "thread_timed_out",
];

/// Parse complete SSE events out of an accumulating byte buffer, stamping each
/// with `now`. Only whole `event:`/`data:` blocks (terminated by a blank line)
/// are emitted, so an event's timestamp reflects when its final chunk landed.
fn parse_complete_events(buf: &[u8], now: Instant) -> Vec<TimedEvent> {
    let text = String::from_utf8_lossy(buf);
    let mut events = Vec::new();
    let mut event = String::new();
    let mut data_lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim().to_string());
        } else if line.is_empty() && (!event.is_empty() || !data_lines.is_empty()) {
            events.push(TimedEvent {
                event: std::mem::take(&mut event),
                data: std::mem::take(&mut data_lines).join("\n"),
                at: now,
            });
        }
    }
    events
}

/// Read the SSE response incrementally, stamping each completed event with its
/// arrival time, until a terminal event is seen or the deadline elapses.
async fn read_timed_until_terminal(
    mut resp: reqwest::Response,
    deadline: Duration,
) -> Vec<TimedEvent> {
    let mut buf: Vec<u8> = Vec::new();
    let mut out: Vec<TimedEvent> = Vec::new();
    let overall = Instant::now() + deadline;
    loop {
        let remaining = overall.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining.min(Duration::from_secs(2)), resp.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                buf.extend_from_slice(&chunk);
                // Re-parse the whole buffer, but keep each already-seen event's
                // original timestamp — only newly-completed events (the tail)
                // get the current arrival time. Re-stamping all of them would
                // collapse every gap to zero.
                let all = parse_complete_events(&buf, Instant::now());
                if all.len() > out.len() {
                    out.extend_from_slice(&all[out.len()..]);
                }
                if out
                    .iter()
                    .any(|e| TERMINAL_EVENTS.contains(&e.event.as_str()))
                {
                    return out;
                }
            }
            Ok(Ok(None)) => break, // stream closed
            Ok(Err(_)) => break,   // transport error
            Err(_) => continue,    // per-chunk timeout; re-check deadline
        }
    }
    out
}

/// Boot a daemon whose mock provider sleeps `PROVIDER_DELAY` before each
/// response. Returns the harness, the user signing key (HTTP principal), the
/// publisher signing key (signs project items), the node fingerprint
/// (audience), and the mock base URL.
async fn boot_live_daemon() -> (DaemonHarness, SigningKey, SigningKey, String, String) {
    let mock = MockProvider::start_with_response_delay(
        vec![
            MockResponse::Text("Hello ".into()),
            MockResponse::Text("world".into()),
        ],
        PROVIDER_DELAY,
    )
    .await;
    let mock_url = mock.base_url.clone();

    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_standard_bundle(state_path, fixture)?;
            plant_execute_stream_route(state_path, &fixture.publisher)?;
            write_authorized_key_signed_by(state_path, &fixture.user, &fixture.node)?;
            Ok(())
        };

    let (h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
        // Provider/routing config lives under the project root.
        cmd.env("RYEOS_ALLOW_PROJECT_PROVIDER_CONFIG", "1");
    })
    .await
    .expect("start daemon with delayed mock + execute-stream route");

    std::mem::forget(mock);
    let user_sk = fixture.user.clone();
    let publisher_sk = fixture.publisher.clone();
    let node_fp = fixture.node_fp();
    (h, user_sk, publisher_sk, node_fp, mock_url)
}

/// Open the gateway `/execute/stream` and return the live response plus the
/// minted thread id. Reads until BOTH `stream_started` (carries the id) and
/// `thread_created` (the thread row now exists) are seen — the latter so a
/// follow-up chain-tail attach does not race the row's creation and 404.
async fn open_gateway_stream(
    h: &DaemonHarness,
    user_sk: &SigningKey,
    node_fp: &str,
    project_path: &str,
) -> (reqwest::Response, String) {
    let body_obj = serde_json::json!({
        "item_ref": "directive:test/live_e2e",
        "ref_bindings": {},
        "project_path": project_path,
        "parameters": {"name": "World"},
    });
    let body_bytes = serde_json::to_vec(&body_obj).expect("serialize body");
    let path = "/execute/stream";
    let audience = format!("fp:{node_fp}");
    let headers = build_ryeos_signed_auth_headers(user_sk, "POST", path, &body_bytes, &audience);

    let url = format!("http://{}{}", h.bind, path);
    let client = reqwest::Client::new();
    let mut req = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body_bytes);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let mut resp = req.send().await.expect("gateway stream request failed");
    assert!(
        resp.status().is_success(),
        "gateway stream returned {}",
        resp.status()
    );

    // Read until stream_started (id) AND thread_created (row exists).
    let mut buf: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "gateway did not emit stream_started + thread_created before deadline"
        );
        match tokio::time::timeout(Duration::from_secs(2), resp.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                buf.extend_from_slice(&chunk);
                let events = parse_complete_events(&buf, Instant::now());
                let thread_id = events
                    .iter()
                    .find(|e| e.event == "stream_started")
                    .map(|ev| {
                        let payload: serde_json::Value =
                            serde_json::from_str(&ev.data).expect("stream_started data is JSON");
                        payload
                            .get("thread_id")
                            .and_then(|v| v.as_str())
                            .expect("stream_started carries thread_id")
                            .to_string()
                    });
                let created = events.iter().any(|e| e.event == "thread_created");
                if let (Some(thread_id), true) = (thread_id, created) {
                    return (resp, thread_id);
                }
            }
            Ok(Ok(None)) => panic!("gateway stream closed before thread_created"),
            Ok(Err(e)) => panic!("gateway stream transport error: {e}"),
            Err(_) => continue,
        }
    }
}

/// The gateway stream must deliver its opening event promptly and the terminal
/// only after the delayed provider call settles — proving incremental live
/// delivery rather than a single buffered flush at completion.
#[tokio::test(flavor = "multi_thread")]
async fn gateway_stream_delivers_events_incrementally_not_buffered() {
    let (mut h, user_sk, publisher_sk, node_fp, mock_url) = boot_live_daemon().await;
    let project = tempfile::tempdir().expect("project tempdir");
    plant_project_items(project.path(), &mock_url, &publisher_sk);
    let project_path = project.path().to_str().unwrap().to_string();

    let body_obj = serde_json::json!({
        "item_ref": "directive:test/live_e2e",
        "ref_bindings": {},
        "project_path": project_path,
        "parameters": {"name": "World"},
    });
    let body_bytes = serde_json::to_vec(&body_obj).expect("serialize body");
    let path = "/execute/stream";
    let audience = format!("fp:{node_fp}");
    let headers = build_ryeos_signed_auth_headers(&user_sk, "POST", path, &body_bytes, &audience);
    let url = format!("http://{}{}", h.bind, path);
    let client = reqwest::Client::new();
    let mut req = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body_bytes);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.expect("gateway stream request failed");
    assert!(resp.status().is_success(), "gateway: {}", resp.status());

    let events = read_timed_until_terminal(resp, Duration::from_secs(30)).await;
    assert!(!events.is_empty(), "no gateway SSE events received");

    if events.iter().any(|e| e.event == "stream_error") {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "gateway stream produced stream_error: {:#?}\n--- daemon stderr ---\n{stderr}",
            events
                .iter()
                .map(|e| (&e.event, &e.data))
                .collect::<Vec<_>>()
        );
    }

    let first = events.first().expect("at least one event");
    assert_eq!(
        first.event, "stream_started",
        "first event is stream_started"
    );

    let terminal = events
        .iter()
        .find(|e| TERMINAL_EVENTS.contains(&e.event.as_str()))
        .expect("gateway stream must deliver a terminal event");

    // The terminal landed at least ~half the provider delay after the opening
    // event. A buffered-until-complete implementation would deliver both in the
    // same flush (gap ~0).
    let gap = terminal.at.duration_since(first.at);
    assert!(
        gap >= PROVIDER_DELAY / 2,
        "terminal must arrive well after the opening event (live delivery); \
         gap={gap:?}, provider_delay={PROVIDER_DELAY:?}"
    );

    drop(project);
}

/// A chain tail attached *while the thread is still running* must receive the
/// terminal event live. We open the tail right after launch (only created/
/// started are persisted), see the tail's opening event promptly, then receive
/// the terminal only later — once the delayed provider settles.
#[tokio::test(flavor = "multi_thread")]
async fn chain_tail_attached_before_terminal_receives_terminal_live() {
    let (h, user_sk, publisher_sk, node_fp, mock_url) = boot_live_daemon().await;
    let project = tempfile::tempdir().expect("project tempdir");
    plant_project_items(project.path(), &mock_url, &publisher_sk);
    let project_path = project.path().to_str().unwrap().to_string();

    // Launch via the gateway and learn the thread id. Keep the gateway stream
    // draining in the background so the inline run is not cancelled by a client
    // disconnect.
    let (gw_resp, thread_id) = open_gateway_stream(&h, &user_sk, &node_fp, &project_path).await;
    let drain = tokio::spawn(async move {
        let mut resp = gw_resp;
        while let Ok(Some(_)) = resp.chunk().await {}
    });

    // For a root thread, chain_root_id == thread_id. Attach the chain tail now,
    // while the provider call is still in flight.
    let audience = format!("fp:{node_fp}");
    let sse_path = format!("/chains/{thread_id}/events/stream");
    let headers = build_ryeos_signed_auth_headers(&user_sk, "GET", &sse_path, b"", &audience);
    let url = format!("http://{}{}", h.bind, sse_path);
    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let opened_at = Instant::now();
    let resp = req.send().await.expect("chain tail request failed");
    assert!(resp.status().is_success(), "chain tail: {}", resp.status());

    let events = read_timed_until_terminal(resp, Duration::from_secs(30)).await;
    assert!(!events.is_empty(), "no chain-tail SSE events received");

    // The tail's opening event arrives promptly on connect.
    let first = events.first().expect("at least one event");
    assert_eq!(
        first.event, "stream_started",
        "tail opens with stream_started"
    );
    assert!(
        first.at.duration_since(opened_at) < PROVIDER_DELAY,
        "opening event must arrive promptly, not bundled with the terminal"
    );

    // The terminal arrives live, only after the delayed provider settles —
    // a clear gap after the opening event.
    let terminal = events
        .iter()
        .find(|e| TERMINAL_EVENTS.contains(&e.event.as_str()))
        .expect("chain tail must deliver the terminal live");
    let gap = terminal.at.duration_since(first.at);
    assert!(
        gap >= PROVIDER_DELAY / 2,
        "terminal must arrive well after the opening event (live, not replay); \
         gap={gap:?}, provider_delay={PROVIDER_DELAY:?}"
    );

    drain.abort();
    drop(project);
}
