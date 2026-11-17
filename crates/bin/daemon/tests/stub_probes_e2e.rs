//! Substrate probes against a scripted chat-completions stub.
//!
//! These are measurements, not gates: every test is `#[ignore]` and prints
//! `PROBE <name>: ...` result lines. The directive's model is a
//! `ryeos-chat-stub` server on loopback, so a run spends nothing and talks to
//! no real provider. Timings depend on the host that runs them; each probe
//! prints the host's CPU count and memory next to its numbers.

use ryeosd::test_support as common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::fast_fixture::{
    FastFixture, register_config_fixture_bundle, register_workflow_bundles,
};
use common::{DaemonHarness, build_signed_headers_for_bytes};
use lillux::crypto::SigningKey;
use ryeos_chat_stub::{Script, StubServer};
use serde_json::{Value, json};

const TERMINAL: &[&str] = &[
    "completed",
    "failed",
    "cancelled",
    "killed",
    "timed_out",
    "continued",
];

fn say(name: &str, line: &str) {
    println!("PROBE {name}: {line}");
}

fn trunc(text: &str, max: usize) -> String {
    let flat: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.chars().count() <= max {
        flat
    } else {
        let head: String = flat.chars().take(max).collect();
        format!("{head}...")
    }
}

fn secs(d: Duration) -> String {
    format!("{:.2}", d.as_secs_f64())
}

/// CPU count, CPU model and memory of the machine running the probe.
fn host_line() -> String {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let mem = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .map(|l| l.trim_start_matches("MemTotal:").trim().to_string())
        })
        .unwrap_or_default();
    let model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|m| m.trim().to_string())
        })
        .unwrap_or_default();
    format!("host_cpus={cpus} host_mem=\"{mem}\" host_cpu_model=\"{model}\"")
}

// ── fixture planting ────────────────────────────────────────────────────

fn plant_provider(root: &Path, base_url: &str, signer: &SigningKey) -> anyhow::Result<()> {
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
pricing:
  explicitly_free: true
  input_per_million: "0"
  output_per_million: "0"
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("stub.yaml"), signed)?;
    Ok(())
}

fn plant_routing(root: &Path, context_window: u64, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        "tiers:\n  general:\n    provider: stub\n    model: stub-model\n    context_window: {context_window}\n"
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

fn plant_execution(root: &Path, yaml: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let signed = lillux::signature::sign_content(yaml, signer, "#", None);
    std::fs::write(dir.join("execution.yaml"), signed)?;
    Ok(())
}

/// The shipped execution defaults with only the request timeout changed.
fn execution_with_timeout(timeout_seconds: u64) -> String {
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
retry_on_timeout: false
retry_mid_stream: false
"#
    )
}

/// A python tool under `.ai/tools/<name>/<name>.py` whose body is `code`.
fn plant_python_tool(
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

fn plant_directive(
    root: &Path,
    header_extra: &str,
    limits: Option<(u64, u64)>,
    signer: &SigningKey,
) -> anyhow::Result<()> {
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
requires:
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

const DIRECTIVE_REF: &str = "directive:probe/run";

struct RigOptions {
    exec_yaml: Option<String>,
    context_window: u64,
    header_extra: &'static str,
    limits: Option<(u64, u64)>,
    nap_seconds: f64,
    marker_sleep_seconds: u64,
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
        }
    }
}

struct Rig {
    h: DaemonHarness,
    stub: StubServer,
    project: tempfile::TempDir,
    aux: tempfile::TempDir,
    /// Instants captured while the harness started the node: when the signed
    /// fixture state was ready (just before the daemon was spawned).
    spawn_at: Instant,
    started_at: Instant,
}

impl Rig {
    fn marker_path(&self) -> PathBuf {
        self.aux.path().join("marker.log")
    }

    fn marker_count(&self) -> usize {
        std::fs::read_to_string(self.marker_path())
            .map(|t| t.lines().filter(|l| *l == "executed").count())
            .unwrap_or(0)
    }

    fn project_str(&self) -> String {
        self.project.path().to_str().expect("utf-8 path").to_string()
    }
}

async fn start_rig(script_yaml: &str, opts: RigOptions) -> anyhow::Result<Rig> {
    let stub = StubServer::start(Script::from_yaml_str(script_yaml)?).await;
    let url = stub.base_url.clone();
    let spawn_cell: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let spawn_cell_tweak = spawn_cell.clone();
    let started_at = Instant::now();
    let plant = move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_config_fixture_bundle(
            state_path,
            "fixture-stub-model-config",
            fixture,
            |bundle_root| plant_provider(bundle_root, &url, &fixture.publisher),
        )
    };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, move |_cmd| {
        *spawn_cell_tweak.lock().expect("spawn cell") = Some(Instant::now());
    })
    .await?;
    let spawn_at = spawn_cell
        .lock()
        .expect("spawn cell")
        .unwrap_or(started_at);

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
        &format!("time.sleep({})\nprint(json.dumps({{'ok': True}}))", opts.nap_seconds),
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
    plant_directive(root, opts.header_extra, opts.limits, &fixture.publisher)?;
    Ok(Rig {
        h,
        stub,
        project,
        aux,
        spawn_at,
        started_at,
    })
}

// ── launching and observing ─────────────────────────────────────────────

async fn launch_live(
    bind: SocketAddr,
    user: &SigningKey,
    node: &SigningKey,
    project: &str,
) -> anyhow::Result<String> {
    let path = "/execute/launch";
    let body = json!({
        "item_ref": DIRECTIVE_REF,
        "launch_id": format!("L-{}", uuid::Uuid::new_v4().simple()),
        "ref_bindings": { "model": DIRECTIVE_REF },
        "project_path": project,
        "parameters": {"name": "probe"},
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

async fn launch_on(rig: &Rig) -> anyhow::Result<String> {
    launch_live(
        rig.h.bind,
        rig.h.user_key.as_ref().expect("user key"),
        rig.h.node_key.as_ref().expect("node key"),
        &rig.project_str(),
    )
    .await
}

async fn status_of(h: &DaemonHarness, thread_id: &str) -> String {
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

async fn wait_terminal(h: &DaemonHarness, thread_id: &str, max: Duration) -> (String, Duration) {
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

async fn event_histogram(h: &DaemonHarness, chain_root: &str) -> String {
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
            format!("threads={} total={} {{{body}}}", threads.len(), events.len())
        }
        Ok(None) => "none".to_string(),
        Err(e) => format!("error({})", trunc(&format!("{e:#}"), 120)),
    }
}

async fn thread_detail(h: &DaemonHarness, thread_id: &str, max: usize) -> String {
    match h.get_thread(thread_id).await {
        Ok(Some(v)) => trunc(&v.to_string(), max),
        other => format!("{other:?}"),
    }
}

async fn node_status_compact(h: &DaemonHarness, max: usize) -> String {
    match h
        .post_execute("service:node/status", ".", json!({}))
        .await
    {
        Ok((_, body)) => trunc(&body.to_string(), max),
        Err(e) => format!("error({e:#})"),
    }
}

/// Stop the node, then summarise its accounting ledger. Falls back to a hard
/// kill when the orderly stop is refused.
async fn stop_and_ledger(h: &mut DaemonHarness) -> String {
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
                    Ok(format!("{}:{}", row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
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

fn stderr_hits(h: &DaemonHarness, needles: &[&str], limit: usize) -> String {
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

// ── process sampling ────────────────────────────────────────────────────

#[derive(Default, Clone, Copy)]
struct Peaks {
    daemon_rss_kb: u64,
    daemon_threads: u64,
    runtimes: u64,
    tools: u64,
    runtime_rss_kb: u64,
    procs: u64,
}

fn proc_status_value(pid: u32, key: &str) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

fn cmdline_pids(needle: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if String::from_utf8_lossy(&raw).replace('\0', " ").contains(needle) {
            out.push(pid);
        }
    }
    out
}

fn sample_once(daemon_pid: u32, peaks: &mut Peaks) {
    let runtimes = cmdline_pids("ryeos-directive-runtime");
    let tools: u64 = ["nap.py", "noop.py", "bulk.py", "marker.py"]
        .iter()
        .map(|n| cmdline_pids(n).len() as u64)
        .sum();
    let runtime_rss: u64 = runtimes
        .iter()
        .map(|p| proc_status_value(*p, "VmRSS:"))
        .sum();
    let procs = std::fs::read_dir("/proc")
        .map(|d| {
            d.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
                })
                .count() as u64
        })
        .unwrap_or(0);
    peaks.daemon_rss_kb = peaks.daemon_rss_kb.max(proc_status_value(daemon_pid, "VmRSS:"));
    peaks.daemon_threads = peaks.daemon_threads.max(proc_status_value(daemon_pid, "Threads:"));
    peaks.runtimes = peaks.runtimes.max(runtimes.len() as u64);
    peaks.tools = peaks.tools.max(tools);
    peaks.runtime_rss_kb = peaks.runtime_rss_kb.max(runtime_rss);
    peaks.procs = peaks.procs.max(procs);
}

struct Sampler {
    stop: Arc<AtomicBool>,
    peaks: Arc<Mutex<Peaks>>,
    join: tokio::task::JoinHandle<()>,
}

impl Sampler {
    fn start(daemon_pid: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peaks = Arc::new(Mutex::new(Peaks::default()));
        let (s, p) = (stop.clone(), peaks.clone());
        let join = tokio::spawn(async move {
            while !s.load(Ordering::Relaxed) {
                let mut local = *p.lock().expect("peaks");
                sample_once(daemon_pid, &mut local);
                *p.lock().expect("peaks") = local;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
        Self { stop, peaks, join }
    }

    async fn finish(self) -> String {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.join.await;
        let p = *self.peaks.lock().expect("peaks");
        format!(
            "peak_daemon_rss_mb={} peak_daemon_threads={} peak_runtime_procs={} peak_tool_procs={} peak_runtime_rss_mb={} peak_system_procs={}",
            p.daemon_rss_kb / 1024,
            p.daemon_threads,
            p.runtimes,
            p.tools,
            p.runtime_rss_kb / 1024,
            p.procs
        )
    }
}

fn pid_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit(')')
                .next()
                .and_then(|rest| rest.trim().chars().next())
        })
        .is_some_and(|state| state != 'Z')
}

fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status();
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ── probe 1: node start ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn probe_node_start() {
    let host = host_line();
    let t0 = Instant::now();
    let mut rig = start_rig(
        "steps:\n  - repeat: 4\n    text: hello\n",
        RigOptions::default(),
    )
    .await
    .expect("start rig");
    let total = rig.spawn_at.duration_since(rig.started_at);
    say(
        "node_start",
        &format!(
            "bundles=standard-profile(core,knowledge,directive,graph,ryeos-docs,scheduling,browser,codex,opencode,ryeos-ui,web) fixture_and_bundle_registration_s={} t0_to_ready_s={} {host}",
            secs(total),
            secs(t0.elapsed()),
        ),
    );
    let daemon_ready = rig.spawn_at.elapsed();
    say(
        "node_start",
        &format!(
            "daemon_spawn_to_ready_s={} (the harness waits for GET /_ryeos/ready)",
            secs(daemon_ready)
        ),
    );

    let t = Instant::now();
    let first = rig
        .h
        .post_execute("service:node/status", ".", json!({}))
        .await;
    say(
        "node_start",
        &format!(
            "first_signed_request_ms={} ok={}",
            t.elapsed().as_millis(),
            first.as_ref().map(|(s, _)| s.is_success()).unwrap_or(false)
        ),
    );
    let t = Instant::now();
    let _ = rig
        .h
        .post_execute("service:node/status", ".", json!({}))
        .await;
    say(
        "node_start",
        &format!("second_signed_request_ms={}", t.elapsed().as_millis()),
    );

    // Cold then warm first directive execution (engine build, bundle
    // verification, runtime spawn).
    let t = Instant::now();
    let id = launch_on(&rig).await.expect("launch cold");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(300)).await;
    say(
        "node_start",
        &format!(
            "cold_first_directive_launch_to_terminal_s={} status={status}",
            secs(t.elapsed())
        ),
    );
    let t = Instant::now();
    let id = launch_on(&rig).await.expect("launch warm");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(300)).await;
    say(
        "node_start",
        &format!(
            "warm_second_directive_launch_to_terminal_s={} status={status}",
            secs(t.elapsed())
        ),
    );

    // Daemon-only start on already initialised state (kill, respawn, ready).
    let t = Instant::now();
    let restarted = rig.h.restart().await;
    say(
        "node_start",
        &format!(
            "restart_kill_to_ready_s={} ok={}",
            secs(t.elapsed()),
            restarted.is_ok()
        ),
    );
    let t = Instant::now();
    let after = rig
        .h
        .post_execute("service:node/status", ".", json!({}))
        .await;
    say(
        "node_start",
        &format!(
            "first_signed_request_after_restart_ms={} ok={}",
            t.elapsed().as_millis(),
            after.as_ref().map(|(s, _)| s.is_success()).unwrap_or(false)
        ),
    );

    // Real `ryeos init` only when a CLI binary sits beside the test binary.
    let cli = common::ryeosd_binary()
        .parent()
        .map(|p| p.join("ryeos"))
        .filter(|p| p.is_file());
    match cli {
        None => say(
            "node_start",
            "init_alone=not measured: the ryeos CLI binary is not built by `cargo test -p ryeosd`; the harness replaces init with signed fixture state",
        ),
        Some(cli) => {
            let app_root = tempfile::tempdir().expect("init root");
            let home = tempfile::tempdir().expect("init home");
            let t = Instant::now();
            let out = std::process::Command::new(cli)
                .arg("init")
                .arg("--app-root")
                .arg(app_root.path())
                .arg("--source")
                .arg(common::workspace_root().join("bundles"))
                .arg("--node-profile")
                .arg("core")
                .arg("--host-class")
                .arg("local")
                .arg("--trust-file")
                .arg(common::workspace_root().join(".dev-keys/PUBLISHER_DEV_TRUST.toml"))
                .env("HOME", home.path())
                .output();
            match out {
                Ok(out) => say(
                    "node_start",
                    &format!(
                        "init_alone_s={} exit={:?} stderr_tail=\"{}\"",
                        secs(t.elapsed()),
                        out.status.code(),
                        trunc(
                            &String::from_utf8_lossy(&out.stderr)
                                .lines()
                                .rev()
                                .take(3)
                                .collect::<Vec<_>>()
                                .join(" | "),
                            300
                        )
                    ),
                ),
                Err(e) => say("node_start", &format!("init_alone=spawn failed: {e}")),
            }
        }
    }
}


/// Leaf values of `value` whose key path mentions capacity, window or
/// incarnation accounting, as `path=value` pairs.
fn capacity_fields(value: &Value, path: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                capacity_fields(inner, &format!("{path}/{key}"), out);
            }
        }
        Value::Array(items) => {
            for (i, inner) in items.iter().enumerate() {
                capacity_fields(inner, &format!("{path}/{i}"), out);
            }
        }
        leaf => {
            let lower = path.to_lowercase();
            if ["incarnation", "occupancy", "window", "allocator", "remaining", "epoch", "ticket", "capacity"]
                .iter()
                .any(|k| lower.contains(k))
            {
                out.push(format!("{path}={leaf}"));
            }
        }
    }
}

async fn capacity_line(h: &DaemonHarness) -> String {
    match h.post_execute("service:node/status", ".", json!({})).await {
        Ok((_, body)) => {
            let mut out = Vec::new();
            capacity_fields(&body, "", &mut out);
            trunc(&out.join(" "), 2500)
        }
        Err(e) => format!("error({e:#})"),
    }
}

/// Wait for a thread to settle, giving up early once the daemon process has
/// exited. Returns the status, the elapsed time and the daemon exit status
/// when it died.
async fn wait_terminal_or_daemon_exit(
    h: &mut DaemonHarness,
    thread_id: &str,
    max: Duration,
) -> (String, Duration, Option<String>) {
    let start = Instant::now();
    loop {
        if let Ok(Some(exit)) = h.child.try_wait() {
            return (
                "daemon-exited".to_string(),
                start.elapsed(),
                Some(format!("{exit:?}")),
            );
        }
        let status = status_of(h, thread_id).await;
        if TERMINAL.contains(&status.as_str()) {
            return (status, start.elapsed(), None);
        }
        if start.elapsed() >= max {
            return (format!("timeout(last={status})"), start.elapsed(), None);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ── probe 2: sequential no-op tool calls ────────────────────────────────

/// Redirect every tempdir the harness creates (app root, state, project) to
/// tmpfs when `/dev/shm` exists, so durable writes cost no disk fsync.
fn use_tmpfs_if_present(name: &str) -> &'static str {
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        say(name, "tmpfs=unavailable (/dev/shm missing); running on the default temp dir");
        return "disk";
    }
    if let Ok(out) = std::process::Command::new("df").args(["-k", "/dev/shm"]).output() {
        say(name, &format!("tmpfs_df={:?}", String::from_utf8_lossy(&out.stdout)));
    }
    // SAFETY: called at the top of a single probe test (--test-threads=1)
    // before any harness thread reads the environment.
    unsafe { std::env::set_var("TMPDIR", "/dev/shm") };
    "tmpfs"
}

fn field_of(line: &str, key: &str) -> Option<String> {
    let eq = format!("{key}=");
    if let Some(i) = line.find(&eq) {
        let rest = &line[i + eq.len()..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        return Some(rest[..end].trim_matches('"').to_string());
    }
    let js = format!("\"{key}\":");
    let i = line.find(&js)?;
    let rest = line[i + js.len()..].trim_start();
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim_matches('"').to_string())
}

/// Group `StateStore contention summary` lines by contention kind and latest
/// caller site. Each summary covers up to a 5 s interval and names only the
/// latest caller, so attribution is a sample, not an exact split.
fn contention_report(name: &str, log: &str) {
    let mut groups: BTreeMap<(String, String), (u64, u64, u64, u64)> = BTreeMap::new();
    let mut lines = 0u64;
    for l in log.lines().filter(|l| l.contains("StateStore contention summary")) {
        lines += 1;
        let kind = field_of(l, "contention").unwrap_or_default();
        let file = field_of(l, "latest_caller_file").unwrap_or_default();
        let line = field_of(l, "latest_caller_line").unwrap_or_default();
        let num = |k: &str| field_of(l, k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let e = groups.entry((kind, format!("{file}:{line}"))).or_default();
        e.0 += 1;
        e.1 += num("samples");
        e.2 += num("total_ms");
        e.3 = e.3.max(num("max_ms"));
    }
    say(name, &format!("contention_summary_lines={lines}"));
    let mut rows: Vec<_> = groups.into_iter().collect();
    rows.sort_by(|a, b| b.1.2.cmp(&a.1.2));
    for ((kind, site), (summaries, samples, total, max)) in rows {
        say(
            name,
            &format!(
                "contention kind={kind} site={site} summaries={summaries} samples={samples} total_ms={total} avg_ms={} max_ms={max}",
                total.checked_div(samples).unwrap_or(0)
            ),
        );
    }
    let raw: Vec<String> = log
        .lines()
        .filter(|l| l.contains("StateStore contention summary"))
        .take(60)
        .map(|l| trunc(l, 420))
        .collect();
    say(name, &format!("contention_raw_first60={raw:#?}"));
    let prof: Vec<String> = log
        .lines()
        .filter(|l| {
            let lower = l.to_lowercase();
            lower.contains("latency") || lower.contains("_us=") || lower.contains("profile")
        })
        .take(120)
        .map(|l| trunc(l, 420))
        .collect();
    say(name, &format!("latency_profile_lines={} sample={prof:#?}", prof.len()));
}

async fn noop_tool_calls(n: u64, limits: Option<(u64, u64)>) {
    noop_tool_calls_on(n, limits, false).await;
}

async fn noop_tool_calls_on(n: u64, limits: Option<(u64, u64)>, tmpfs: bool) {
    let storage = if tmpfs {
        use_tmpfs_if_present("noop_storage")
    } else {
        "disk"
    };
    let name = format!("noop_tool_calls_{n}_{storage}_{}", if cfg!(debug_assertions) { "debug" } else { "release" });
    let script = format!(
        "steps:\n  - repeat: {n}\n    tool_calls:\n      - {{ id: \"call_{{n}}\", name: noop_noop, arguments: {{}} }}\n  - repeat: 3\n    text: done\n"
    );
    let mut rig = start_rig(
        &script,
        RigOptions {
            limits,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    say(&name, &format!("limits={limits:?} {}", host_line()));
    let before = capacity_line(&rig.h).await;
    let sampler = Sampler::start(rig.h.child.id().unwrap_or(0));
    let t = Instant::now();
    let id = launch_on(&rig).await.expect("launch");
    let (status, _, daemon_exit) =
        wait_terminal_or_daemon_exit(&mut rig.h, &id, Duration::from_secs(4500)).await;
    let wall = t.elapsed();
    let peaks = sampler.finish().await;

    let requests = rig.stub.requests().await;
    let mut gaps: Vec<u64> = requests
        .windows(2)
        .map(|w| (w[1].received_ms.saturating_sub(w[0].received_ms)) as u64)
        .collect();
    gaps.sort_unstable();
    let span_ms = match (requests.first(), requests.last()) {
        (Some(a), Some(b)) => b.received_ms.saturating_sub(a.received_ms) as u64,
        _ => 0,
    };
    let calls = requests.len().saturating_sub(1).max(1) as u64;
    let body_sizes: Vec<usize> = requests
        .iter()
        .map(|r| r.body.to_string().len())
        .collect();
    say(
        &name,
        &format!(
            "completed={} status={status} launch_to_terminal_s={} stub_requests={} first_to_last_request_s={} mean_per_call_ms={} p50_ms={} p95_ms={} max_ms={} last_request_body_kb={} {peaks}",
            status == "completed",
            secs(wall),
            requests.len(),
            secs(Duration::from_millis(span_ms)),
            span_ms / calls,
            percentile(&gaps, 0.5),
            percentile(&gaps, 0.95),
            gaps.last().copied().unwrap_or(0),
            body_sizes.last().copied().unwrap_or(0) / 1024,
        ),
    );
    let after = if daemon_exit.is_some() {
        "daemon exited".to_string()
    } else {
        capacity_line(&rig.h).await
    };
    say(&name, &format!("daemon_exit={daemon_exit:?}"));
    say(&name, &format!("capacity_before={before}"));
    say(&name, &format!("capacity_after={after}"));
    let log = rig.h.stderr_so_far();
    contention_report(&name, &log);
    {
        // Growth: mean gap per tenth of the run (gaps in request order).
        let ordered: Vec<u64> = requests
            .windows(2)
            .map(|w| (w[1].received_ms.saturating_sub(w[0].received_ms)) as u64)
            .collect();
        let tenth = (ordered.len() / 10).max(1);
        let deciles: Vec<u64> = ordered
            .chunks(tenth)
            .map(|c| c.iter().sum::<u64>() / c.len() as u64)
            .collect();
        say(&name, &format!("mean_gap_ms_per_decile={deciles:?}"));
    }
    let tail: Vec<String> = log
        .lines()
        .rev()
        .take(25)
        .map(|l| trunc(l, 260))
        .collect();
    say(&name, &format!("daemon_stderr_tail_newest_first={tail:#?}"));
    say(
        &name,
        &format!("thread_detail={}", thread_detail(&rig.h, &id, 600).await),
    );
    say(
        &name,
        &format!("events={}", event_histogram(&rig.h, &id).await),
    );
    let ledger = stop_and_ledger(&mut rig.h).await;
    say(&name, &ledger);
    say(
        &name,
        &format!(
            "stderr_limit_lines={}",
            stderr_hits(&rig.h, &["limit", "exhaust", "refus", "capacity", "budget"], 5)
        ),
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn probe_noop_tool_calls_100() {
    // No declared limits: shows what the directive kind enforces by default.
    noop_tool_calls(100, None).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn probe_noop_tool_calls_100_tmpfs() {
    noop_tool_calls_on(100, None, true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn probe_noop_tool_calls_2000() {
    noop_tool_calls(2000, Some((5000, 5000))).await;
}

// ── probe 3: concurrency ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn probe_concurrency() {
    let host = host_line();
    let calls_each = 3u64;
    for n in [1u64, 2, 4, 8, 16, 32, 64, 128, 256] {
        let name = format!("concurrency_{n}");
        let total_calls = n * calls_each;
        let script = format!(
            "steps:\n  - repeat: {total_calls}\n    tool_calls:\n      - {{ id: \"call_{{n}}\", name: nap_nap, arguments: {{}} }}\n  - repeat: {}\n    text: done\n",
            n * 2
        );
        let mut rig = match start_rig(&script, RigOptions::default()).await {
            Ok(rig) => rig,
            Err(e) => {
                say(&name, &format!("rig failed to start: {e:#}"));
                return;
            }
        };
        let sampler = Sampler::start(rig.h.child.id().unwrap_or(0));
        let t = Instant::now();
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..n {
            let bind = rig.h.bind;
            let user = rig.h.user_key.clone().expect("user key");
            let node = rig.h.node_key.clone().expect("node key");
            let project = rig.project_str();
            set.spawn(async move { launch_live(bind, &user, &node, &project).await });
        }
        let mut ids = Vec::new();
        let mut refusals: Vec<String> = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok(id)) => ids.push(id),
                Ok(Err(e)) => refusals.push(trunc(&format!("{e:#}"), 200)),
                Err(e) => refusals.push(format!("join: {e}")),
            }
        }
        let launched_in = t.elapsed();
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        loop {
            counts.clear();
            match rig.h.list_threads(2000).await {
                Ok(listed) => {
                    for thread in listed {
                        if ids.contains(&thread.thread_id) {
                            *counts.entry(thread.status).or_default() += 1;
                        }
                    }
                }
                Err(e) => {
                    counts.insert(format!("list-error({})", trunc(&format!("{e:#}"), 60)), 1);
                }
            }
            let terminal: usize = counts
                .iter()
                .filter(|(k, _)| TERMINAL.contains(&k.as_str()))
                .map(|(_, v)| *v)
                .sum();
            if terminal >= ids.len() || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let wall = t.elapsed();
        let peaks = sampler.finish().await;
        let completed = counts.get("completed").copied().unwrap_or(0);
        say(
            &name,
            &format!(
                "launched={}/{} launch_calls_s={} all_terminal_s={} statuses={counts:?} stub_requests={} refusals={:?} {peaks} {host}",
                ids.len(),
                n,
                secs(launched_in),
                secs(wall),
                rig.stub.requests().await.len(),
                refusals.iter().take(3).collect::<Vec<_>>(),
            ),
        );
        let degraded = completed != n as usize;
        if degraded {
            say(
                &name,
                &format!(
                    "DEGRADED at {n}: completed={completed}; stderr_hits={}",
                    stderr_hits(
                        &rig.h,
                        &["limit", "refus", "exhaust", "capacity", "too many", "ticket", "error"],
                        8
                    )
                ),
            );
            say(
                &name,
                &format!("node_status={}", node_status_compact(&rig.h, 1500).await),
            );
        }
        let _ = stop_and_ledger(&mut rig.h).await;
        if degraded {
            return;
        }
    }
}

// ── probe 4: provider failure baseline ──────────────────────────────────

struct Case {
    name: &'static str,
    timeout_seconds: Option<u64>,
    max_wait_secs: u64,
}

const RECOVERED: &str = "  - repeat: 5\n    text: recovered\n";

fn cases() -> Vec<(Case, String)> {
    let mk = |name, script: &str, timeout, wait| {
        (
            Case {
                name,
                timeout_seconds: timeout,
                max_wait_secs: wait,
            },
            format!("steps:\n{script}{RECOVERED}"),
        )
    };
    vec![
        mk(
            "http_504",
            "  - repeat: 5\n    fail: { status: 504, body: gateway timeout }\n",
            None,
            120,
        ),
        mk(
            "http_529",
            "  - repeat: 5\n    fail: { status: 529, body: overloaded }\n",
            None,
            120,
        ),
        mk(
            "http_500_then_success",
            "  - fail: { status: 500, body: internal error }\n",
            None,
            120,
        ),
        mk(
            "http_429",
            "  - repeat: 5\n    fail: { status: 429, body: slow down }\n",
            None,
            120,
        ),
        mk(
            "midstream_cut",
            "  - repeat: 3\n    text: a longer reply that is cut after the first chunk\n    cut_after_chunks: 1\n    stream: { text_chunk_bytes: 5 }\n",
            None,
            120,
        ),
        mk(
            "stall_20s_default_timeout",
            "  - text: stalled then resumed\n    stall: { after_chunks: 0, ms: 20000 }\n",
            None,
            180,
        ),
        mk(
            "stall_30s_timeout_10s",
            "  - text: stalled then resumed\n    stall: { after_chunks: 0, ms: 30000 }\n",
            Some(10),
            180,
        ),
        mk(
            "first_byte_60s_default_timeout",
            "  - text: late reply\n    delay_ms: 60000\n",
            None,
            240,
        ),
        mk(
            "first_byte_20s_timeout_10s",
            "  - text: late reply\n    delay_ms: 20000\n",
            Some(10),
            180,
        ),
        mk(
            "finish_length",
            "  - text: a partial reply cut by the output limit\n    finish_reason: length\n    usage: { prompt_tokens: 900, completion_tokens: 4096 }\n",
            None,
            120,
        ),
        mk("text_only_reply", "  - text: hello, nothing to do\n", None, 120),
    ]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn probe_failure_baseline() {
    let host = host_line();
    for (case, script) in cases() {
        let name = format!("failure_baseline[{}]", case.name);
        let opts = RigOptions {
            exec_yaml: case.timeout_seconds.map(execution_with_timeout),
            ..RigOptions::default()
        };
        let mut rig = match start_rig(&script, opts).await {
            Ok(rig) => rig,
            Err(e) => {
                say(&name, &format!("rig failed to start: {e:#}"));
                continue;
            }
        };
        let t = Instant::now();
        let id = match launch_on(&rig).await {
            Ok(id) => id,
            Err(e) => {
                say(&name, &format!("launch failed: {e:#}"));
                continue;
            }
        };
        let (status, _) =
            wait_terminal(&rig.h, &id, Duration::from_secs(case.max_wait_secs)).await;
        let elapsed = t.elapsed();
        let requests = rig.stub.requests().await;
        let gaps: Vec<u128> = requests
            .windows(2)
            .map(|w| w[1].received_ms.saturating_sub(w[0].received_ms))
            .collect();
        say(
            &name,
            &format!(
                "timeout_seconds={:?} terminal={status} elapsed_s={} stub_requests={} retried={} request_gaps_ms={gaps:?} {host}",
                case.timeout_seconds.unwrap_or(300),
                secs(elapsed),
                requests.len(),
                requests.len() > 1,
            ),
        );
        say(
            &name,
            &format!("thread_detail={}", thread_detail(&rig.h, &id, 700).await),
        );
        say(
            &name,
            &format!("events={}", event_histogram(&rig.h, &id).await),
        );
        let ledger = stop_and_ledger(&mut rig.h).await;
        say(&name, &ledger);
        say(
            &name,
            &format!(
                "stderr_provider_lines={}",
                stderr_hits(&rig.h, &["retry", "provider", "timeout", "stream"], 4)
            ),
        );
    }
}

// ── probe 5: continuation and crash ─────────────────────────────────────

async fn chain_threads(h: &DaemonHarness, root: &str) -> Vec<(String, String)> {
    h.list_threads(2000)
        .await
        .map(|listed| {
            listed
                .into_iter()
                .filter(|t| t.chain_root_id == root || t.thread_id == root)
                .map(|t| (t.thread_id, t.status))
                .collect()
        })
        .unwrap_or_default()
}

async fn continuation_probe() {
    let name = "continuation";
    // A 2,000-token window puts the boundary near 1,800 tokens; each bulk
    // result is about 600 characters, so the boundary arrives within about a
    // dozen calls.
    let script = "steps:\n  - repeat: 40\n    tool_calls:\n      - { id: \"call_{n}\", name: bulk_bulk, arguments: {} }\n  - repeat: 5\n    text: done\n";
    let mut rig = start_rig(
        script,
        RigOptions {
            context_window: 2000,
            header_extra: "continuation:\n  carry_turns: 2\n",
            limits: Some((200, 200)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let t = Instant::now();
    let id = match launch_on(&rig).await {
        Ok(id) => id,
        Err(e) => {
            say(name, &format!("launch failed: {e:#}"));
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut chain;
    loop {
        chain = chain_threads(&rig.h, &id).await;
        let all_terminal =
            !chain.is_empty() && chain.iter().all(|(_, s)| TERMINAL.contains(&s.as_str()));
        if (all_terminal && t.elapsed() > Duration::from_secs(3)) || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    say(
        name,
        &format!(
            "elapsed_s={} chain_threads={} statuses={:?} {}",
            secs(t.elapsed()),
            chain.len(),
            chain.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
            host_line()
        ),
    );
    let requests = rig.stub.requests().await;
    let mut prev = 0usize;
    for (i, request) in requests.iter().enumerate() {
        let messages = request
            .body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m.get("role").and_then(Value::as_str).unwrap_or("?"))
            .collect();
        let first_non_system = roles.iter().find(|r| **r != "system").copied().unwrap_or("-");
        if i == 0 || messages.len() <= prev {
            say(
                name,
                &format!(
                    "request_{i}: messages={} first_roles={:?} first_non_system_role={first_non_system} tools_in_body={}",
                    messages.len(),
                    roles.iter().take(4).collect::<Vec<_>>(),
                    request.body.get("tools").map(|t| t.as_array().map_or(0, Vec::len)).unwrap_or(0),
                ),
            );
        }
        prev = messages.len();
    }
    say(
        name,
        &format!("stub_requests={} message_counts_by_request={:?}", requests.len(), requests
            .iter()
            .map(|r| r.body.get("messages").and_then(Value::as_array).map_or(0, Vec::len))
            .collect::<Vec<_>>()),
    );
    say(name, &format!("events={}", event_histogram(&rig.h, &id).await));
    say(
        name,
        &format!(
            "root_detail={}",
            thread_detail(&rig.h, &id, 700).await
        ),
    );
    let ledger = stop_and_ledger(&mut rig.h).await;
    say(name, &ledger);
}

fn recoverable_execute_body(project: &str) -> Value {
    json!({
        "item_ref": DIRECTIVE_REF,
        "ref_bindings": {"model": DIRECTIVE_REF},
        "project_path": project,
        "parameters": {"name": "probe"},
        "execution_policy": {
            "schema_version": 2,
            "ownership": "daemon_owned",
            "recovery": "restart_recoverable",
            "response": "wait",
            "target": {"kind": "here"},
            "environment": {
                "kind": "project_overlay",
                "include_operator_vault": true,
                "name_policy": {"kind": "declared_required"}
            },
            "project": {
                "kind": "pinned",
                "source": {"kind": "capture_live", "scope": "full_project"},
                "realization": {"kind": "read_only"},
                "child_policy": {"kind": "inherit"}
            }
        }
    })
}

/// Start a restart-recoverable execute in the background and return the
/// request task and the root thread id once the daemon lists it.
async fn start_recoverable(
    rig: &Rig,
) -> Option<(tokio::task::JoinHandle<()>, String)> {
    let bytes = serde_json::to_vec(&recoverable_execute_body(&rig.project_str())).ok()?;
    let headers = build_signed_headers_for_bytes(
        rig.h.user_key.as_ref()?,
        rig.h.node_key.as_ref()?,
        "POST",
        "/execute",
        &bytes,
    );
    let url = format!("http://{}/execute", rig.h.bind);
    let task = tokio::spawn(async move {
        let mut request = reqwest::Client::new()
            .post(url)
            .header("content-type", "application/json")
            .body(bytes);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let _ = request.send().await;
    });
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok(listed) = rig.h.list_threads(500).await
            && let Some(thread) = listed.into_iter().find(|t| t.item_ref == DIRECTIVE_REF)
        {
            return Some((task, thread.thread_id));
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_marker(rig: &Rig, want: usize, max: Duration) -> bool {
    let deadline = Instant::now() + max;
    while Instant::now() < deadline {
        if rig.marker_count() >= want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Poll a thread, recording each status change with its offset.
async fn watch_status(
    h: &DaemonHarness,
    id: &str,
    max: Duration,
    rig_marker: impl Fn() -> usize,
) -> String {
    let start = Instant::now();
    let mut last = String::new();
    let mut trail: Vec<String> = Vec::new();
    while start.elapsed() < max {
        let status = status_of(h, id).await;
        if status != last {
            trail.push(format!("{}s:{status}(markers={})", secs(start.elapsed()), rig_marker()));
            last = status.clone();
        }
        if TERMINAL.contains(&status.as_str()) && status != "continued" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    trail.join(" -> ")
}

fn crash_script() -> &'static str {
    "steps:\n  - tool_calls:\n      - { id: \"call_a\", name: marker_marker, arguments: {} }\n  - repeat: 5\n    text: done\n"
}

fn crash_options() -> RigOptions {
    RigOptions {
        header_extra: "effects: recorded\nreturn_nudge: false\n",
        limits: Some((6, 6)),
        marker_sleep_seconds: 25,
        ..RigOptions::default()
    }
}

async fn runtime_kill_probe() {
    let name = "crash_runtime_kill";
    let mut rig = start_rig(crash_script(), crash_options())
        .await
        .expect("start rig");
    let Some((_task, id)) = start_recoverable(&rig).await else {
        say(name, "root thread never appeared");
        return;
    };
    if !wait_marker(&rig, 1, Duration::from_secs(120)).await {
        say(name, "tool never started");
        return;
    }
    let runtimes = cmdline_pids("ryeos-directive-runtime");
    say(name, &format!("runtime_pids_before_kill={runtimes:?} thread_status={}", status_of(&rig.h, &id).await));
    for pid in &runtimes {
        kill_pid(*pid);
    }
    let marker_path = rig.marker_path();
    let marker = move || {
        std::fs::read_to_string(&marker_path)
            .map(|t| t.lines().filter(|l| *l == "executed").count())
            .unwrap_or(0)
    };
    let trail = watch_status(&rig.h, &id, Duration::from_secs(120), &marker).await;
    say(name, &format!("status_trail={trail}"));
    let daemon_alive = rig.h.child.try_wait().map(|s| s.is_none()).unwrap_or(false);
    say(
        name,
        &format!(
            "daemon_alive={daemon_alive} markers={} stub_requests={} runtime_pids_after={:?}",
            rig.marker_count(),
            rig.stub.requests().await.len(),
            cmdline_pids("ryeos-directive-runtime")
        ),
    );
    say(name, &format!("thread_detail={}", thread_detail(&rig.h, &id, 700).await));
    say(name, &format!("events={}", event_histogram(&rig.h, &id).await));
    let ledger = stop_and_ledger(&mut rig.h).await;
    say(name, &ledger);
    say(
        name,
        &format!("stderr_lines={}", stderr_hits(&rig.h, &["resum", "recover", "crash", "exited", "orphan"], 6)),
    );
    for pid in cmdline_pids("marker.py") {
        kill_pid(pid);
    }
}

async fn daemon_kill_probe() {
    let name = "crash_daemon_kill";
    let mut rig = start_rig(crash_script(), crash_options())
        .await
        .expect("start rig");
    let Some((_task, id)) = start_recoverable(&rig).await else {
        say(name, "root thread never appeared");
        return;
    };
    if !wait_marker(&rig, 1, Duration::from_secs(120)).await {
        say(name, "tool never started");
        return;
    }
    let runtimes = cmdline_pids("ryeos-directive-runtime");
    let tools = cmdline_pids("marker.py");
    say(name, &format!("runtime_pids={runtimes:?} tool_pids={tools:?} requests_before_kill={}", rig.stub.requests().await.len()));
    let killed = rig.h.kill_daemon().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let survivors: Vec<u32> = runtimes
        .iter()
        .chain(tools.iter())
        .copied()
        .filter(|p| pid_alive(*p))
        .collect();
    say(name, &format!("daemon_killed={} survivors_after_daemon_kill={survivors:?}", killed.is_ok()));
    for pid in &survivors {
        kill_pid(*pid);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t = Instant::now();
    let respawn = rig.h.respawn_with(|_| {}).await;
    say(name, &format!("respawn_to_ready_s={} ok={}", secs(t.elapsed()), respawn.is_ok()));
    let marker_path = rig.marker_path();
    let marker = move || {
        std::fs::read_to_string(&marker_path)
            .map(|t| t.lines().filter(|l| *l == "executed").count())
            .unwrap_or(0)
    };
    let trail = watch_status(&rig.h, &id, Duration::from_secs(150), &marker).await;
    say(name, &format!("status_trail={trail}"));
    say(
        name,
        &format!(
            "markers={} stub_requests={}",
            rig.marker_count(),
            rig.stub.requests().await.len()
        ),
    );
    say(name, &format!("thread_detail={}", thread_detail(&rig.h, &id, 700).await));
    say(name, &format!("events={}", event_histogram(&rig.h, &id).await));
    let ledger = stop_and_ledger(&mut rig.h).await;
    say(name, &ledger);
    say(
        name,
        &format!("stderr_lines={}", stderr_hits(&rig.h, &["resum", "recover", "reconcil", "orphan"], 6)),
    );
    for pid in cmdline_pids("marker.py") {
        kill_pid(pid);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn probe_continuation_and_crash() {
    continuation_probe().await;
    runtime_kill_probe().await;
    daemon_kill_probe().await;
}

// ── diagnostics ─────────────────────────────────────────────────────────

fn newer_files(dir: &Path, since: std::time::SystemTime, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if path.file_name().is_some_and(|n| n == "target" || n == ".git") {
                continue;
            }
            newer_files(&path, since, out);
        } else if meta.modified().is_ok_and(|m| m > since) && out.len() < 25 {
            out.push(path.display().to_string());
        }
    }
}

/// Lists the files that make the prepared-bundle staleness gate refuse.
#[test]
#[ignore = "measurement"]
fn probe_stale_diagnosis() {
    let root = common::workspace_root();
    let manifest = root.join("bundles/browser/.ai/refs/bundles/manifest");
    let published = std::fs::metadata(&manifest)
        .and_then(|m| m.modified())
        .expect("browser manifest ref");
    let mut newer = Vec::new();
    newer_files(&root.join("crates"), published, &mut newer);
    newer_files(&root.join("bundles/browser"), published, &mut newer);
    for name in ["Cargo.toml", "Cargo.lock"] {
        let modified = std::fs::metadata(root.join(name)).and_then(|m| m.modified());
        say("stale_diagnosis", &format!("{name} mtime={modified:?}"));
    }
    say(
        "stale_diagnosis",
        &format!("ref_mtime={published:?} newer_files={newer:#?}"),
    );
}
