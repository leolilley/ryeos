//! Shared rig for daemon e2e tests that drive a directive against the scripted
//! `ryeos-chat-stub` server.
//!
//! Include with `#[path = "common/stub_rig.rs"] mod stub_rig;`. `start_rig`
//! boots a fast-fixture node whose `stub` model provider points at a loopback
//! stub running the given script, plants a project (routing, an optional
//! execution config, `noop`/`bulk`/`nap`/`marker` python tools and the
//! `directive:probe/run` directive) and returns a `Rig`. `launch_on` launches
//! the directive live, `wait_terminal` waits on the thread, and
//! `event_histogram`/`chain_events` read its events. Individual tests use a
//! subset of these helpers.

#![allow(dead_code)]

use ryeosd::test_support as common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::fast_fixture::{
    FastFixture, register_config_fixture_bundle, register_workflow_bundles,
};
use common::{DaemonHarness, build_signed_headers_for_bytes};
use lillux::crypto::SigningKey;
use ryeos_chat_stub::{Script, StubServer};
use serde_json::{Value, json};

pub const TERMINAL: &[&str] = &[
    "completed",
    "failed",
    "cancelled",
    "killed",
    "timed_out",
    "continued",
];

pub fn trunc(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    if flat.chars().count() <= max {
        flat
    } else {
        let head: String = flat.chars().take(max).collect();
        format!("{head}...")
    }
}

pub fn plant_provider(root: &Path, base_url: &str, signer: &SigningKey) -> anyhow::Result<()> {
    plant_provider_priced(root, base_url, signer, false)
}

/// The stub provider with explicit-free pricing, or (`paid`) with a signed
/// tariff spend authority so a finite `limits.spend_usd` reserves and charges
/// money. At the default context window the reserved maximum per attempt is
/// $0.72768 (727_680_000 nanos).
pub fn plant_provider_priced(
    root: &Path,
    base_url: &str,
    signer: &SigningKey,
    paid: bool,
) -> anyhow::Result<()> {
    plant_provider_with(root, base_url, signer, paid, "")
}

/// The stub provider as `plant_provider_priced`, with `schemas_extra` (YAML
/// whose top-level keys are children of `schemas:`, for example a `reasoning:`
/// block) spliced in after the fixed streaming and output-limit blocks.
pub fn plant_provider_with(
    root: &Path,
    base_url: &str,
    signer: &SigningKey,
    paid: bool,
    schemas_extra: &str,
) -> anyhow::Result<()> {
    plant_provider_full(root, base_url, signer, paid, schemas_extra, "")
}

/// As `plant_provider_with`, plus `top_extra`: YAML spliced at the top level
/// of the provider entry, for example an `image_input:` declaration.
pub fn plant_provider_full(
    root: &Path,
    base_url: &str,
    signer: &SigningKey,
    paid: bool,
    schemas_extra: &str,
    top_extra: &str,
) -> anyhow::Result<()> {
    let schemas_extra: String = schemas_extra
        .lines()
        .map(|line| format!("  {line}\n"))
        .collect();
    let top_extra: String = top_extra.lines().map(|line| format!("{line}\n")).collect();
    let pricing = if paid {
        r#"pricing:
  input_per_million: "2"
  output_per_million: "10"
spend_authority:
  billing_principal: e2e-stub
  credential_authority_generation: gen-1
  pricing_contract_subject: e2e-stub-2026-11
  tariff:
    schema_version: 1
    currency: usd
    pricing_generation: e2e-2026-11
    input_per_million: "2"
    output_per_million: "10"
    covered_dimensions: [input_tokens, output_tokens]
"#
    } else {
        r#"pricing:
  explicitly_free: true
  input_per_million: "0"
  output_per_million: "0"
"#
    };
    let dir = root.join(".ai/config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"family: chat_completions
transport:
  kind: remote_http
  base_url: "{base_url}"
body_template:
  model: "{{model}}"
  messages: "{{messages}}"
  tools: "{{tools}}"
  stream: "{{stream}}"
auth: {{}}
headers: {{}}
schemas:
  streaming:
    mode: delta_merge
  output_limit: {{path: max_tokens, semantics: provider_native_output_tokens}}
{schemas_extra}{top_extra}{pricing}"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("stub.yaml"), signed)?;
    Ok(())
}

pub fn plant_routing(root: &Path, context_window: u64, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        "tiers:\n  general:\n    provider: stub\n    model: stub-model\n    context_window: {context_window}\n"
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

pub fn plant_execution(root: &Path, yaml: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let signed = lillux::signature::sign_content(yaml, signer, "#", None);
    std::fs::write(dir.join("execution.yaml"), signed)?;
    Ok(())
}

/// The shipped execution defaults with only the request timeout changed.
pub fn execution_with_timeout(timeout_seconds: u64) -> String {
    format!(
        r#"category: "ryeos-runtime"
retries: 2
retry_status_codes: [429, 500, 502, 503]
never_retry: ["401", "403", "404"]
backoff_base_ms: 1000
timeout_seconds: {timeout_seconds}
max_provider_output_tokens_per_turn: 32768
max_stream_output_bytes_per_turn: 131072
max_provider_stream_frame_bytes: 1048576
accounting:
  failure_policy: auto
  budget_mode: settled
tool_preload: false
"#
    )
}

/// A python tool under `.ai/tools/<name>/<name>.py` whose body is `code`.
pub fn plant_python_tool(
    root: &Path,
    name: &str,
    code: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(format!(".ai/tools/{name}"));
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "{name}"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "stub probe tool {name}"

import json, os, time
{code}
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join(format!("{name}.py")), signed)?;
    Ok(())
}

/// Python for a tool that returns images in the `result_parts` envelope. Each
/// call makes a different 1x1 PNG (a counter file in `aux` varies its colour)
/// and appends its base64 to `aux/shots.log`, one line per good image, so a
/// test can compare what the model was sent with what the tool returned.
/// Variants: `good` (text and one PNG), `bad` (a GIF declared as PNG, then a
/// good PNG) and `big` (a PNG padded past the model's per-image limit).
pub fn image_tool_code(aux: &Path, variant: &str) -> String {
    let counter = aux.join("shot.count");
    let log = aux.join("shots.log");
    let body = r#"
import zlib, struct, base64
def chunk(kind, data):
    out = struct.pack('>I', len(data)) + kind + data
    return out + struct.pack('>I', zlib.crc32(kind + data) & 0xffffffff)
def png(r, g, b):
    raw = b'\x00' + bytes([r, g, b])
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 1, 1, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b''))
n = 1
try:
    n = int(open(@COUNTER@).read()) + 1
except Exception:
    pass
open(@COUNTER@, 'w').write(str(n))
good = png((n * 37) % 256, 20, 200)
def part(media, data):
    return {'type': 'image', 'media_type': media, 'data_b64': base64.b64encode(data).decode()}
def log(data):
    with open(@LOG@, 'a') as f:
        f.write(base64.b64encode(data).decode() + '\n')
@VARIANT@
"#;
    let variant_code = match variant {
        "good" => {
            "log(good)
parts = [{'type': 'text', 'text': 'shot %d' % n}, part('image/png', good)]"
        }
        "bad" => {
            "log(good)
parts = [{'type': 'text', 'text': 'bad %d' % n}, part('image/png', b'GIF89a\\x01\\x00\\x01\\x00'), part('image/png', good)]"
        }
        "big" => {
            "parts = [{'type': 'text', 'text': 'big %d' % n}, part('image/png', good + b'\\x00' * 8000)]"
        }
        other => panic!("unknown image tool variant {other}"),
    };
    format!(
        "{}\nprint(json.dumps({{'result_parts': parts}}))",
        body.replace(
            "@COUNTER@",
            &format!("{:?}", counter.to_str().expect("utf-8 path"))
        )
        .replace("@LOG@", &format!("{:?}", log.to_str().expect("utf-8 path")))
        .replace("@VARIANT@", variant_code)
    )
}

pub fn plant_directive(
    root: &Path,
    header_extra: &str,
    limits: Option<(u64, u64)>,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    plant_directive_with(root, header_extra, limits, None, signer)
}

/// The probe directive with an optional provider-neutral reasoning policy,
/// given as the lines under `model.reasoning:` (for example
/// `"mode: enabled\neffort: high"`).
pub fn plant_directive_with(
    root: &Path,
    header_extra: &str,
    limits: Option<(u64, u64)>,
    reasoning: Option<&str>,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let reasoning: String = reasoning
        .map(|lines| {
            let body: String = lines.lines().map(|line| format!("    {line}\n")).collect();
            format!("  reasoning:\n{body}")
        })
        .unwrap_or_default();
    let path = root.join(".ai/directives/probe/run.md");
    std::fs::create_dir_all(path.parent().expect("directive parent"))?;
    let limits = limits
        .map(|(turns, calls)| format!("limits:\n  turns: {turns}\n  tool_calls: {calls}\n"))
        .unwrap_or_default();
    let body = format!(
        r#"---
name: run
category: "probe"
description: "stub probe directive"
inputs:
  - name: name
    type: string
    required: false
model:
  tier: general
{reasoning}requires:
  capabilities:
    declared:
      - "ryeos.execute.tool.*"
{limits}{header_extra}---
Call the tools as the model directs, then report completion.
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

pub const DIRECTIVE_REF: &str = "directive:probe/run";

pub struct RigOptions {
    pub exec_yaml: Option<String>,
    pub context_window: u64,
    pub header_extra: &'static str,
    pub limits: Option<(u64, u64)>,
    pub nap_seconds: f64,
    pub marker_sleep_seconds: u64,
    /// Plant the provider with a signed tariff so spend limits reserve money.
    pub paid_provider: bool,
    /// Seal the key the graph runtime's preflight needs, for tests that launch
    /// a graph.
    pub graph_vault: bool,
    /// YAML spliced under the provider's `schemas:` (see
    /// `plant_provider_with`), for example a `reasoning:` capture and replay
    /// declaration. Empty plants the fixed provider unchanged.
    pub provider_schemas_extra: String,
    /// YAML spliced at the top level of the provider entry, for example an
    /// `image_input:` declaration. Empty declares none.
    pub provider_top_extra: String,
    /// Plant the image-returning tools `shot`, `shot_bad` and `shot_big`.
    pub image_tools: bool,
    /// Lines under the directive's `model.reasoning:`, when it sets a
    /// reasoning policy.
    pub directive_reasoning: Option<String>,
}

impl Default for RigOptions {
    fn default() -> Self {
        Self {
            exec_yaml: None,
            context_window: 200_000,
            header_extra: "",
            limits: None,
            nap_seconds: 1.0,
            marker_sleep_seconds: 25,
            paid_provider: false,
            graph_vault: false,
            provider_schemas_extra: String::new(),
            provider_top_extra: String::new(),
            image_tools: false,
            directive_reasoning: None,
        }
    }
}

pub struct Rig {
    pub h: DaemonHarness,
    pub stub: StubServer,
    pub project: tempfile::TempDir,
    pub aux: tempfile::TempDir,
    /// The key that signs everything planted in the fixture, for tests that
    /// plant more items into `project`.
    pub publisher: SigningKey,
    /// Instants captured while the harness started the node: when the signed
    /// fixture state was ready (just before the daemon was spawned).
    pub spawn_at: Instant,
    pub started_at: Instant,
}

impl Rig {
    pub fn marker_path(&self) -> PathBuf {
        self.aux.path().join("marker.log")
    }

    pub fn marker_count(&self) -> usize {
        std::fs::read_to_string(self.marker_path())
            .map(|t| t.lines().filter(|l| *l == "executed").count())
            .unwrap_or(0)
    }

    pub fn project_str(&self) -> String {
        self.project
            .path()
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }
}

pub async fn start_rig(script_yaml: &str, opts: RigOptions) -> anyhow::Result<Rig> {
    let stub = StubServer::start(Script::from_yaml_str(script_yaml)?).await;
    let url = stub.base_url.clone();
    let paid_provider = opts.paid_provider;
    let graph_vault = opts.graph_vault;
    let schemas_extra = opts.provider_schemas_extra.clone();
    let top_extra = opts.provider_top_extra.clone();
    let spawn_cell: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let spawn_cell_tweak = spawn_cell.clone();
    let started_at = Instant::now();
    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_workflow_bundles(state_path, fixture)?;
            if graph_vault {
                plant_vault_with_zen_key(state_path)?;
            }
            register_config_fixture_bundle(
                state_path,
                "fixture-stub-model-config",
                fixture,
                |bundle_root| {
                    plant_provider_full(
                        bundle_root,
                        &url,
                        &fixture.publisher,
                        paid_provider,
                        &schemas_extra,
                        &top_extra,
                    )
                },
            )
        };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, move |cmd| {
        if let Ok(filter) = std::env::var("PROBE_RUST_LOG") {
            cmd.env("RUST_LOG", filter);
        }
        *spawn_cell_tweak.lock().expect("spawn cell") = Some(Instant::now());
    })
    .await?;
    let spawn_at = spawn_cell.lock().expect("spawn cell").unwrap_or(started_at);

    let project = tempfile::tempdir()?;
    let aux = tempfile::tempdir()?;
    let root = project.path();
    plant_routing(root, opts.context_window, &fixture.publisher)?;
    if let Some(yaml) = &opts.exec_yaml {
        plant_execution(root, yaml, &fixture.publisher)?;
    }
    plant_python_tool(
        root,
        "noop",
        "print(json.dumps({'ok': True}))",
        &fixture.publisher,
    )?;
    plant_python_tool(
        root,
        "bulk",
        "print(json.dumps({'ok': True, 'data': 'x' * 600}))",
        &fixture.publisher,
    )?;
    plant_python_tool(
        root,
        "nap",
        &format!(
            "time.sleep({})\nprint(json.dumps({{'ok': True}}))",
            opts.nap_seconds
        ),
        &fixture.publisher,
    )?;
    let marker = aux.path().join("marker.log");
    plant_python_tool(
        root,
        "marker",
        &format!(
            "with open({marker:?}, 'a') as f:\n    f.write('executed\\n')\n    f.flush()\n    os.fsync(f.fileno())\ntime.sleep({})\nprint(json.dumps({{'executed': True}}))",
            opts.marker_sleep_seconds,
            marker = marker.to_str().expect("utf-8 path"),
        ),
        &fixture.publisher,
    )?;
    if opts.image_tools {
        for (name, variant) in [("shot", "good"), ("shot_bad", "bad"), ("shot_big", "big")] {
            plant_python_tool(
                root,
                name,
                &image_tool_code(aux.path(), variant),
                &fixture.publisher,
            )?;
        }
    }
    plant_directive_with(
        root,
        opts.header_extra,
        opts.limits,
        opts.directive_reasoning.as_deref(),
        &fixture.publisher,
    )?;
    Ok(Rig {
        h,
        stub,
        project,
        aux,
        publisher: fixture.publisher.clone(),
        spawn_at,
        started_at,
    })
}

// ── launching and observing ─────────────────────────────────────────────

pub async fn launch_live(
    bind: SocketAddr,
    user: &SigningKey,
    node: &SigningKey,
    project: &str,
) -> anyhow::Result<String> {
    launch_live_with(bind, user, node, project, json!({"name": "probe"})).await
}

/// Launch the probe directive with explicit launch parameters.
pub async fn launch_live_with(
    bind: SocketAddr,
    user: &SigningKey,
    node: &SigningKey,
    project: &str,
    parameters: Value,
) -> anyhow::Result<String> {
    launch_item_with(
        bind,
        user,
        node,
        project,
        DIRECTIVE_REF,
        json!({ "model": DIRECTIVE_REF }),
        parameters,
    )
    .await
}

/// Launch any item live with explicit reference bindings and parameters.
pub async fn launch_item_with(
    bind: SocketAddr,
    user: &SigningKey,
    node: &SigningKey,
    project: &str,
    item_ref: &str,
    ref_bindings: Value,
    parameters: Value,
) -> anyhow::Result<String> {
    let path = "/execute/launch";
    let body = json!({
        "item_ref": item_ref,
        "launch_id": format!("L-{}", uuid::Uuid::new_v4().simple()),
        "ref_bindings": ref_bindings,
        "project_path": project,
        "parameters": parameters,
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::local_live(
            ryeos_app::execution_policy::ExecutionResponse::Accepted
        ),
    });
    let bytes = serde_json::to_vec(&body)?;
    let mut request = reqwest::Client::new()
        .post(format!("http://{bind}{path}"))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in build_signed_headers_for_bytes(user, node, "POST", path, &bytes) {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or_else(|_| json!({}));
    anyhow::ensure!(
        status == reqwest::StatusCode::ACCEPTED,
        "launch answered {status}: {}",
        trunc(&value.to_string(), 300)
    );
    value
        .get("thread_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("accepted launch carried no thread_id: {value}"))
}

pub async fn launch_on(rig: &Rig) -> anyhow::Result<String> {
    launch_live(
        rig.h.bind,
        rig.h.user_key.as_ref().expect("user key"),
        rig.h.node_key.as_ref().expect("node key"),
        &rig.project_str(),
    )
    .await
}

/// Launch the probe directive with extra launch parameters merged over the
/// default `{"name": "probe"}`.
pub async fn launch_on_with(rig: &Rig, extra: Value) -> anyhow::Result<String> {
    let mut parameters = json!({"name": "probe"});
    if let (Some(base), Some(extra)) = (parameters.as_object_mut(), extra.as_object()) {
        base.extend(extra.clone());
    }
    launch_live_with(
        rig.h.bind,
        rig.h.user_key.as_ref().expect("user key"),
        rig.h.node_key.as_ref().expect("node key"),
        &rig.project_str(),
        parameters,
    )
    .await
}

/// Launch a graph live on the rig's node, optionally with launch parameters.
pub async fn launch_graph_on(
    rig: &Rig,
    item_ref: &str,
    parameters: Value,
) -> anyhow::Result<String> {
    launch_item_with(
        rig.h.bind,
        rig.h.user_key.as_ref().expect("user key"),
        rig.h.node_key.as_ref().expect("node key"),
        &rig.project_str(),
        item_ref,
        json!({}),
        parameters,
    )
    .await
}

/// Seal the key the graph runtime's preflight requires.
pub fn plant_vault_with_zen_key(state_path: &Path) -> anyhow::Result<()> {
    use std::collections::HashMap;
    let pub_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("public_key.pem");
    let pub_key = lillux::vault::read_public_key(&pub_path)?;
    let store_path = ryeos_app::vault::default_sealed_store_path(state_path);
    let secrets = HashMap::from([(
        "ZEN_API_KEY".to_string(),
        "test-zen-api-key-value".to_string(),
    )]);
    ryeos_app::vault::write_sealed_secrets(&store_path, &pub_key, &secrets)?;
    Ok(())
}

/// Plant a signed graph under `.ai/graphs/<name>.yaml` in the rig's project.
pub fn plant_graph(rig: &Rig, name: &str, body: &str) -> anyhow::Result<()> {
    let dir = rig.project.path().join(".ai/graphs");
    std::fs::create_dir_all(&dir)?;
    let signed = lillux::signature::sign_content(body, &rig.publisher, "#", None);
    std::fs::write(dir.join(format!("{name}.yaml")), signed)?;
    Ok(())
}

pub async fn status_of(h: &DaemonHarness, thread_id: &str) -> String {
    match h.get_thread(thread_id).await {
        Ok(Some(v)) => v
            .pointer("/thread/status")
            .and_then(Value::as_str)
            .unwrap_or("no-status")
            .to_string(),
        Ok(None) => "unknown-thread".to_string(),
        Err(e) => format!("unreadable({})", trunc(&format!("{e:#}"), 80)),
    }
}

pub async fn wait_terminal(
    h: &DaemonHarness,
    thread_id: &str,
    max: Duration,
) -> (String, Duration) {
    let start = Instant::now();
    loop {
        let status = status_of(h, thread_id).await;
        if TERMINAL.contains(&status.as_str()) {
            return (status, start.elapsed());
        }
        if start.elapsed() >= max {
            return (format!("timeout(last={status})"), start.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub async fn event_histogram(h: &DaemonHarness, chain_root: &str) -> String {
    match h.chain_events(chain_root).await {
        Ok(Some(events)) => {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            let mut threads: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            for (thread, kind, _) in &events {
                *counts.entry(kind.as_str()).or_default() += 1;
                threads.insert(thread.as_str());
            }
            let body = counts
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "threads={} total={} {{{body}}}",
                threads.len(),
                events.len()
            )
        }
        Ok(None) => "none".to_string(),
        Err(e) => format!("error({})", trunc(&format!("{e:#}"), 120)),
    }
}

/// Every persisted event of one chain as `(thread_id, event_type, payload)`
/// in chain order; empty when the daemon does not know the chain yet or the
/// read fails.
pub async fn chain_event_list(h: &DaemonHarness, chain_root: &str) -> Vec<(String, String, Value)> {
    h.chain_events(chain_root)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// The payloads of one chain's events of `event_type`, in chain order.
pub async fn chain_payloads(h: &DaemonHarness, chain_root: &str, event_type: &str) -> Vec<Value> {
    chain_event_list(h, chain_root)
        .await
        .into_iter()
        .filter(|(_, kind, _)| kind == event_type)
        .map(|(_, _, payload)| payload)
        .collect()
}

pub async fn thread_detail(h: &DaemonHarness, thread_id: &str, max: usize) -> String {
    match h.get_thread(thread_id).await {
        Ok(Some(v)) => trunc(&v.to_string(), max),
        other => format!("{other:?}"),
    }
}

pub async fn node_status_compact(h: &DaemonHarness, max: usize) -> String {
    match h.post_execute("service:node/status", ".", json!({})).await {
        Ok((_, body)) => trunc(&body.to_string(), max),
        Err(e) => format!("error({e:#})"),
    }
}

/// Stop the node, then summarise its accounting ledger. Falls back to a hard
/// kill when the orderly stop is refused.
pub async fn stop_and_ledger(h: &mut DaemonHarness) -> String {
    let stopped = match h.stop_gracefully().await {
        Ok(()) => "graceful".to_string(),
        Err(e) => {
            let _ = h.kill_daemon().await;
            format!("killed(graceful failed: {})", trunc(&format!("{e:#}"), 100))
        }
    };
    let db = h
        .state_path
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join(ryeos_app::accounting_db::ACCOUNTING_DB_FILENAME);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ryeos_app::test_support::read_existing_accounting(&db, |connection| {
            let mut attempts = connection.prepare(
                "SELECT state, COUNT(*), MAX(attempt_number) FROM provider_attempt_reservation GROUP BY state",
            )?;
            let attempts = attempts
                .query_map([], |row| {
                    Ok(format!(
                        "{}:{}(max_attempt={})",
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut operations = connection.prepare(
                "SELECT operation_kind, COUNT(*) FROM accounting_operation GROUP BY operation_kind",
            )?;
            let operations = operations
                .query_map([], |row| {
                    Ok(format!(
                        "{}:{}",
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!(
                "attempt_states=[{}] operations=[{}]",
                attempts.join(","),
                operations.join(",")
            ))
        })
    }));
    let ledger = outcome.unwrap_or_else(|_| "ledger unreadable".to_string());
    format!("stop={stopped} ledger={ledger}")
}

pub fn stderr_hits(h: &DaemonHarness, needles: &[&str], limit: usize) -> String {
    let log = h.stderr_so_far();
    let hits: Vec<String> = log
        .lines()
        .filter(|l| {
            let lower = l.to_lowercase();
            needles.iter().any(|n| lower.contains(n))
        })
        .take(limit)
        .map(|l| trunc(l, 220))
        .collect();
    format!("{hits:?}")
}
