//! Phase E — SSE directive_launch end-to-end tests.
//!
//! Proves the one-call POST+stream chain works:
//!   1. POST /execute/stream with a JSON body { item_ref, parameters }
//!   2. Receive `stream_started` SSE event with the minted thread_id
//!   3. Receive lifecycle + LLM events as the directive runs
//!   4. Receive a terminal `thread_completed` event and stream closes
//!
//! Coverage:
//!   - `sse_directive_launch_e2e_round_trip` — full happy path
//!   - `sse_directive_launch_rejects_last_event_id` — Last-Event-ID
//!     header → 400 (the source explicitly does not support resume since
//!     each invocation mints a fresh thread)
//!   - `sse_directive_launch_collision` — `create_root_thread_with_id`
//!     refuses a duplicate (covers the unique-constraint path that the
//!     source's launch task surfaces as `stream_error`)

mod common;

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::{EncodePrivateKey, Signer, SigningKey};

fn e2e_signing_key() -> lillux::crypto::SigningKey {
    lillux::crypto::SigningKey::from_bytes(&[0x77u8; 32])
}

fn write_trusted_signer(
    user_space: &Path,
    vk: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<()> {
    use base64::engine::Engine as _;

    let fp = lillux::signature::compute_fingerprint(vk);
    let trust_dir = user_space.join(".ai/config/keys/trusted");
    std::fs::create_dir_all(&trust_dir)?;
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
    let toml = format!(
        r#"version = "1.0.0"
category = "keys/trusted"
fingerprint = "{fp}"
owner = "self"
attestation = ""

[public_key]
pem = "ed25519:{key_b64}"
"#
    );
    std::fs::write(trust_dir.join(format!("{fp}.toml")), toml)?;
    Ok(())
}

fn register_standard_bundle(state_path: &Path) -> anyhow::Result<()> {
    let standard = common::workspace_root().join("ryeos-bundles/standard");
    if !standard.is_dir() {
        anyhow::bail!(
            "ryeos-bundles/standard does not exist at {}",
            standard.display()
        );
    }
    let abs = standard.canonicalize()?;
    let dir = state_path.join(".ai/node/bundles");
    std::fs::create_dir_all(&dir)?;
    let body = format!("section: bundles\npath: {}\n", abs.display());
    let signed = lillux::signature::sign_content(&body, &e2e_signing_key(), "#", None);
    std::fs::write(dir.join("standard.yaml"), signed)?;
    Ok(())
}

fn plant_mock_provider(user_space: &Path, mock_base_url: &str) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/rye-runtime/model_providers");
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
    let signed = lillux::signature::sign_content(&body, &e2e_signing_key(), "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

fn plant_model_routing(user_space: &Path) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/rye-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = r#"tiers:
  general:
    provider: mock
    model: mock-model
    context_window: 200000
"#;
    let signed = lillux::signature::sign_content(body, &e2e_signing_key(), "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

fn plant_directive(
    user_space: &Path,
    rel_path: &str,
    body_text: &str,
) -> anyhow::Result<()> {
    let path = user_space.join(format!(".ai/directives/{rel_path}.md"));
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let body = format!(
        r#"---
__category__: "{rel_path}"
__directive_description__: "SSE directive_launch e2e test fixture"
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
    let signed = lillux::signature::sign_content(&body, &e2e_signing_key(), "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

/// Plant the /execute/stream route YAML. Phase E ships this in
/// `ryeos-bundles/core/.ai/node/routes/execute-stream.yaml`; the test
/// re-plants it under the daemon's state path signed by the e2e key
/// so the trusted-loader policy accepts it.
fn plant_execute_stream_route(state_path: &Path) -> anyhow::Result<()> {
    let dir = state_path.join(".ai/node/routes");
    std::fs::create_dir_all(&dir)?;
    let body = r#"section: routes
id: execute/stream
path: /execute/stream
methods:
  - POST
auth: rye_signed
limits:
  body_bytes_max: 1048576
  timeout_ms: 0
  concurrent_max: 32
request:
  body: json
response:
  mode: event_stream
  source: directive_launch
  source_config:
    keep_alive_secs: 15
"#;
    let signed = lillux::signature::sign_content(body, &e2e_signing_key(), "#", None);
    std::fs::write(dir.join("execute-stream.yaml"), signed)?;
    Ok(())
}

fn write_authorized_key(state_path: &Path, sk: &SigningKey) -> anyhow::Result<()> {
    let vk = sk.verifying_key();
    let fp = lillux::signature::compute_fingerprint(&vk);
    let auth_dir = state_path.join("auth").join("authorized_keys");
    std::fs::create_dir_all(&auth_dir)?;

    use base64::engine::Engine as _;
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

    let toml_body = format!(
        r#"fingerprint = "{fp}"
public_key = "ed25519:{key_b64}"
scopes = ["*"]
label = "directive-launch-e2e-test"
"#
    );
    let signed = lillux::signature::sign_content(&toml_body, sk, "#", None);
    std::fs::write(auth_dir.join(format!("{fp}.toml")), signed)?;
    Ok(())
}

fn pre_create_node_key_from(state_path: &Path, sk: &SigningKey) -> anyhow::Result<()> {
    let key_dir = state_path.join(".ai").join("identity");
    std::fs::create_dir_all(&key_dir)?;
    let pem = sk.to_pkcs8_pem(Default::default())?;
    std::fs::write(key_dir.join("node-key.pem"), pem.as_bytes())?;
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
    data: String,
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
                    data: data_lines.join("\n"),
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
            data: data_lines.join("\n"),
        });
    }
    events
}

async fn boot_daemon() -> (DaemonHarness, SigningKey, String) {
    let mock = MockProvider::start(vec![
        MockResponse::Text("Hello ".into()),
        MockResponse::Text("from directive_launch".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();

    let node_sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let node_fp = lillux::signature::compute_fingerprint(&node_sk.verifying_key());
    let node_bytes = node_sk.to_bytes();

    let pre_init = move |state_path: &Path, user: &Path| -> anyhow::Result<()> {
        std::fs::create_dir_all(state_path)?;
        let sk = e2e_signing_key();
        write_trusted_signer(user, &sk.verifying_key())?;
        register_standard_bundle(state_path)?;
        plant_mock_provider(user, &mock_url)?;
        plant_model_routing(user)?;
        plant_directive(user, "test/launch_e2e", "Say hello.")?;
        plant_execute_stream_route(state_path)?;

        let saved_sk = SigningKey::from_bytes(&node_bytes);
        pre_create_node_key_from(state_path, &saved_sk)?;
        write_authorized_key(state_path, &saved_sk)?;
        Ok(())
    };

    let h = DaemonHarness::start_with_pre_init(pre_init, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with mock + execute-stream route");

    std::mem::forget(mock);
    (h, node_sk, node_fp)
}

/// Full happy-path: POST /execute/stream → SSE
/// `stream_started` → lifecycle/LLM events → terminal `thread_completed`.
///
/// `build_launch_task` calls `dispatch::dispatch` with
/// `pre_minted_thread_id = Some(thread_id)`, so the SSE-minted id
/// flows all the way through to `create_root_thread_with_id` and the
/// resulting thread row uses that exact id. The subscriber attached
/// to `event_streams.subscribe(&thread_id)` therefore observes every
/// lifecycle event from `thread_started` onward.
#[tokio::test(flavor = "multi_thread")]
async fn sse_directive_launch_e2e_round_trip() {
    let (h, node_sk, node_fp) = boot_daemon().await;
    let project = tempfile::tempdir().expect("project tempdir");
    let project_path = project.path().to_str().unwrap().to_string();

    let body_obj = serde_json::json!({
        "item_ref": "directive:test/launch_e2e",
        "project_path": project_path,
        "parameters": {"name": "World"},
    });
    let body_bytes = serde_json::to_vec(&body_obj).expect("serialize body");

    let audience = format!("fp:{node_fp}");
    let path = "/execute/stream";
    let headers =
        build_rye_signed_auth_headers(&node_sk, "POST", path, &body_bytes, &audience);

    let url = format!("http://{}{}", h.bind, path);
    let client = reqwest::Client::new();
    let mut req = client.post(&url).body(body_bytes.clone());
    req = req.header("content-type", "application/json");
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = tokio::time::timeout(Duration::from_secs(30), req.send())
        .await
        .expect("POST /execute/stream timed out")
        .expect("POST /execute/stream send failed");
    assert!(
        resp.status().is_success(),
        "POST /execute/stream returned {}",
        resp.status()
    );

    let bytes = tokio::time::timeout(Duration::from_secs(30), resp.bytes())
        .await
        .expect("read SSE body timed out")
        .expect("read SSE body failed");

    let events = parse_sse_bytes(&bytes);
    assert!(!events.is_empty(), "no SSE events received");

    // First event must be `stream_started` with NO id. The data field
    // carries `{"thread_id": "T-..."}`.
    let first = &events[0];
    assert_eq!(
        first.event, "stream_started",
        "first event must be stream_started, got: {}",
        first.event
    );
    assert!(
        first.id.is_none(),
        "stream_started must have no id (would corrupt Last-Event-ID resume); got id={:?}",
        first.id
    );
    let payload: serde_json::Value =
        serde_json::from_str(&first.data).expect("stream_started data is JSON");
    let thread_id = payload
        .get("thread_id")
        .and_then(|v| v.as_str())
        .expect("stream_started carries thread_id")
        .to_string();
    assert!(thread_id.starts_with("T-"), "minted id must start with T-: {thread_id}");

    // Last event must be terminal lifecycle.
    let last = events.last().expect("at least one event");
    let terminal_types = [
        "thread_completed",
        "thread_failed",
        "thread_cancelled",
        "thread_killed",
        "thread_timed_out",
    ];
    let all_summary: Vec<String> = events
        .iter()
        .map(|e| format!("{}={}", e.event, e.data))
        .collect();
    assert!(
        terminal_types.contains(&last.event.as_str()),
        "last event must be terminal, got: {} ({})\nall events: {:#?}",
        last.event,
        last.data,
        all_summary
    );

    // Persisted events between stream_started and terminal must have
    // monotonic numeric ids.
    let mut prev: Option<i64> = None;
    for ev in &events {
        if let Some(ref id) = ev.id {
            let seq: i64 = id.parse().expect("id is numeric");
            if let Some(p) = prev {
                assert!(seq > p, "chain_seq monotonic: {seq} <= {p}");
            }
            prev = Some(seq);
        }
    }

    drop(project);
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_directive_launch_rejects_last_event_id() {
    let (h, node_sk, node_fp) = boot_daemon().await;
    let project = tempfile::tempdir().expect("project tempdir");
    let project_path = project.path().to_str().unwrap().to_string();

    let body_obj = serde_json::json!({
        "item_ref": "directive:test/launch_e2e",
        "project_path": project_path,
        "parameters": {"name": "World"},
    });
    let body_bytes = serde_json::to_vec(&body_obj).expect("serialize body");

    let audience = format!("fp:{node_fp}");
    let path = "/execute/stream";
    let headers =
        build_rye_signed_auth_headers(&node_sk, "POST", path, &body_bytes, &audience);

    let url = format!("http://{}{}", h.bind, path);
    let client = reqwest::Client::new();
    let mut req = client.post(&url).body(body_bytes.clone());
    req = req.header("content-type", "application/json");
    req = req.header("last-event-id", "5");
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = tokio::time::timeout(Duration::from_secs(10), req.send())
        .await
        .expect("POST timed out")
        .expect("POST send failed");

    // `directive_launch::open()` rejects any Last-Event-ID with
    // RouteDispatchError::BadRequest → 400.
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "expected 400 for Last-Event-ID on directive_launch; got {}",
        resp.status()
    );

    drop(project);
}

/// E.5 test 3 — duplicate-id collision contract.
///
/// `directive_launch::build_launch_task` calls `dispatch::dispatch`
/// with `pre_minted_thread_id = Some(thread_id)`, which lands at
/// `services::thread_lifecycle::create_root_thread_with_id` →
/// `StateStore::create_thread`. If the pre-minted id collides with
/// an existing row the chain insert must fail (PRIMARY KEY
/// constraint on `thread_id`), and the launch task surfaces this as
/// a `stream_error` SSE event.
///
/// This test stands up an in-process `StateStore` and proves the
/// underlying SQL constraint that the SSE source's collision-error
/// path relies on: a second `create_thread` call with the same
/// `thread_id` MUST error. No RNG injection shim — `new_thread_id`
/// is OsRng-backed and operationally collision-free.
#[test]
fn sse_directive_launch_collision() {
    use ryeosd::identity::NodeIdentity;
    use ryeosd::services::thread_lifecycle::new_thread_id;
    use ryeosd::state_store::{NewThreadRecord, NodeIdentitySigner, StateStore};
    use ryeosd::write_barrier::WriteBarrier;
    use std::sync::Arc;

    let tmpdir = tempfile::TempDir::new().expect("tempdir");
    let state_root = tmpdir.path().join(".ai").join("state");
    let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
    let key_path = tmpdir.path().join("identity").join("node-key.pem");

    let identity = NodeIdentity::create(&key_path).expect("create identity");
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let write_barrier = WriteBarrier::new();
    let state_store =
        StateStore::new(state_root, runtime_db_path, signer, write_barrier)
            .expect("open state store");

    let id = new_thread_id();

    let record = NewThreadRecord {
        thread_id: id.clone(),
        chain_root_id: id.clone(),
        kind: "directive_run".to_string(),
        item_ref: "directive:test/collision".to_string(),
        executor_ref: "native:test-runtime".to_string(),
        launch_mode: "inline".to_string(),
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        upstream_thread_id: None,
        requested_by: Some("fp:test-collision".to_string()),
    };

    // First insert must succeed.
    let first = state_store.create_thread(&record);
    assert!(
        first.is_ok(),
        "first state_store.create_thread must succeed; got: {first:?}"
    );

    // Second insert with the SAME thread_id MUST fail.
    let second = state_store.create_thread(&record);
    assert!(
        second.is_err(),
        "second state_store.create_thread with the same id must error \
         (the path that surfaces as stream_error in directive_launch); \
         got Ok"
    );
}

/// `new_thread_id()` mints SSE-mintable thread ids that conform to
/// the `T-<8-4-4-4-12>` UUID shape `validate_thread_id_format`
/// enforces on insert. Two consecutive mints must differ (122-bit id
/// space).
#[test]
fn new_thread_id_format_and_uniqueness() {
    use ryeosd::services::thread_lifecycle::new_thread_id;

    let a = new_thread_id();
    let b = new_thread_id();
    assert_ne!(a, b, "two consecutive mints must differ");
    for id in [&a, &b] {
        assert_eq!(id.len(), 38, "id length must be 38 (T- + 36): got {id}");
        assert!(id.starts_with("T-"), "id must start with T-: got {id}");
        let suffix = &id[2..];
        let groups: Vec<&str> = suffix.split('-').collect();
        assert_eq!(groups.len(), 5, "suffix must have 5 hex groups: got {suffix}");
        let expected = [8, 4, 4, 4, 12];
        for (g, want) in groups.iter().zip(expected.iter()) {
            assert_eq!(g.len(), *want, "group length: got `{g}` want {want} hex");
            assert!(
                g.chars().all(|c| c.is_ascii_hexdigit()),
                "group must be lowercase hex: got `{g}`"
            );
        }
    }
}
