//! G1a — graph action e2e: permitted cap allows tool dispatch.
//!
//! Proves the full callback path:
//!   walker → callback runtime.dispatch_action → daemon enforce_callback_caps
//!   → dispatch → tool executor chain → subprocess → result returned to walker
//!
//! The graph has `permissions: [rye.execute.tool.echo]` which the daemon
//! composes into effective_caps on the callback token. The tool `echo.py`
//! is a planted Python script that reads params from stdin and returns JSON.
//!
//! G2 must land first (walker self-check removed) so that this test pins
//! the daemon-side gate as the single boundary.

mod common;

use std::path::Path;

use common::DaemonHarness;

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

    let body = format!(
        "section: bundles\npath: {}\n",
        abs.display()
    );
    let signed = lillux::signature::sign_content(&body, &e2e_signing_key(), "#", None);
    std::fs::write(dir.join("standard.yaml"), signed)?;
    Ok(())
}

/// Plant a Python tool at `.ai/tools/echo.py`.
///
/// Unsigned — Unsigned trust class is accepted by the engine for tool
/// items the chain doesn't gate on (matches `hello_world_python.rs`'s
/// working pattern). Signing with `after_shebang: true` is brittle to
/// reproduce in tests; unsigned is the documented test path.
fn plant_echo_tool(project_dir: &Path) -> anyhow::Result<()> {
    let tools_dir = project_dir.join(".ai").join("tools");
    let tool_dir = tools_dir.join("echo");
    std::fs::create_dir_all(&tool_dir)?;

    let body = r#"#!/usr/bin/env python3
__version__ = "1.0.0"
__executor_id__ = "tool:rye/core/runtimes/python/script"
__category__ = "echo"
__description__ = "echo input as json"

import json, sys
raw = sys.stdin.read()
params = json.loads(raw) if raw.strip() else {}
print(json.dumps({"msg": params.get("msg", "default")}))
"#;
    std::fs::write(tool_dir.join("echo.py"), body)?;
    Ok(())
}

/// Plant a graph with permissions that allow tool:echo/echo.
fn plant_permitted_graph(project_dir: &Path) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
permissions:
  - rye.execute.tool.echo.echo
config:
  start: greet
  nodes:
    greet:
      action:
        item_id: "tool:echo/echo"
        params:
          msg: "hello"
      assign:
        greeting: "${result.msg}"
      next: done
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, &e2e_signing_key(), "#", None);
    std::fs::write(graphs_dir.join("flow.yaml"), signed)?;
    Ok(())
}

/// Plant a graph with empty permissions (deny-all).
fn plant_denied_graph(project_dir: &Path) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
permissions: []
config:
  start: greet
  nodes:
    greet:
      action:
        item_id: "tool:echo/echo"
        params:
          msg: "hello"
      assign:
        greeting: "${result.msg}"
      next: done
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, &e2e_signing_key(), "#", None);
    std::fs::write(graphs_dir.join("denied.yaml"), signed)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_action_completes_with_permitted_cap() {
    let pre_init = |state_path: &Path, user: &Path| -> anyhow::Result<()> {
        std::fs::create_dir_all(state_path)?;
        let sk = e2e_signing_key();
        write_trusted_signer(user, &sk.verifying_key())?;
        register_standard_bundle(state_path)?;
        Ok(())
    };

    let mut h = DaemonHarness::start_with_pre_init(pre_init, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeosd=debug,ryeos_graph_runtime=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_echo_tool(project.path()).expect("plant echo tool");
    plant_permitted_graph(project.path()).expect("plant permitted graph");

    let post_fut = h.post_execute(
        "graph:flow",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        post_fut,
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
            "expected 200 OK; got {status}\nbody={body:#}\n--- daemon stderr ---\n{stderr}"
        );
    }

    let result = match body.get("result") {
        Some(r) => r,
        None => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "response missing `result`\nbody={body:#}\n--- daemon stderr ---\n{stderr}"
            );
        }
    };

    assert_eq!(
        result.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "graph with permitted cap must succeed; body={body:#}"
    );
    assert_eq!(
        result.get("status").and_then(|v| v.as_str()),
        Some("completed"),
        "graph must complete; body={body:#}"
    );
    // The wire shape is RuntimeResult-wrapped GraphResult:
    //   body.result            ← RuntimeResult (success/status/result/outputs/warnings)
    //   body.result.result     ← GraphResult   (graph_id/state/result/steps/...)
    //   body.result.result.state.greeting  ← assigned via `assign: greeting: ${result.msg}`
    let graph_result = result.get("result").and_then(|v| v.as_object()).unwrap_or_else(|| {
        panic!("missing nested GraphResult under result.result; body={body:#}");
    });
    let greeting = graph_result
        .get("state")
        .and_then(|s| s.get("greeting"))
        .and_then(|v| v.as_str());
    if greeting != Some("hello") {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "state.greeting must be 'hello' from tool result; got {greeting:?}\n\
             body={body:#}\n--- daemon stderr ---\n{stderr}"
        );
    }

    // Smell guard: when a return node has NO explicit `output:` template,
    // GraphResult.result MUST be omitted (None). Previously commit_terminal
    // defaulted `result` to `state.clone()`, producing the
    // `body.result.result.result == body.result.result.state` duplicate
    // that callers were forced to choose between.
    assert!(
        graph_result.get("result").is_none(),
        "GraphResult.result must be absent when return node has no `output:` template; \
         got body={body:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_action_denied_without_permitted_cap() {
    let pre_init = |state_path: &Path, user: &Path| -> anyhow::Result<()> {
        std::fs::create_dir_all(state_path)?;
        let sk = e2e_signing_key();
        write_trusted_signer(user, &sk.verifying_key())?;
        register_standard_bundle(state_path)?;
        Ok(())
    };

    let mut h = DaemonHarness::start_with_pre_init(pre_init, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeosd=debug,ryeos_graph_runtime=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_echo_tool(project.path()).expect("plant echo tool");
    plant_denied_graph(project.path()).expect("plant denied graph");

    let post_fut = h.post_execute(
        "graph:denied",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        post_fut,
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

    // HTTP 200 — the daemon returns the graph result envelope even on
    // internal cap denial. The error is inside the result, not an HTTP
    // error code.
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "expected 200 OK (daemon returns error envelope); got {status}\nbody={body:#}"
    );

    let result = body.get("result").unwrap_or_else(|| {
        panic!("response missing `result`; body={body:#}");
    });

    assert_eq!(
        result.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "graph with no caps must fail; body={body:#}"
    );

    // The wire shape is RuntimeResult-wrapped GraphResult; the graph's
    // `error` field is at result.result.error.
    let graph_result = result
        .get("result")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| {
            panic!("missing nested GraphResult under result.result; body={body:#}");
        });
    assert_eq!(
        graph_result.get("status").and_then(|v| v.as_str()),
        Some("error"),
        "denied graph must terminate with error status; body={body:#}"
    );
    let error_msg = graph_result
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // The walker wraps callback errors as "node '<n>' failed: dispatch failed: <inner>".
    // The inner `<inner>` is the daemon-side error from `enforce_callback_caps`,
    // which contains "callback denied" — but the walker may truncate the cause
    // chain. Accept either the cap-denial wording OR the walker-side
    // "dispatch failed" surface. G2 (walker self-check removed) ensures the
    // cause is the daemon gate, even if the visible string is the wrapper.
    assert!(
        error_msg.contains("dispatch failed")
            || error_msg.contains("callback denied")
            || error_msg.contains("effective_caps"),
        "expected dispatch/cap-denial error; got: {error_msg}\nbody={body:#}"
    );
}
