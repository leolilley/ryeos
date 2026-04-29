//! V5.3 Task -1 — Behavior-pinning tests.
//!
//! Freezes today's `/execute` HTTP surface so the V5.3 refactor cannot
//! drift behavior silently. Each test spawns a real `ryeosd` subprocess
//! (mirroring `cleanup_e2e.rs`) and asserts on the EXACT response shape
//! today produces.
//!
//! V5.3 Task 0b update — the three native-runtime rejection tests now
//! drive `runtime:*` refs through the schema-derived
//! `kind_registry::DispatchCapabilities` table (introduced by 0b),
//! instead of the deleted V5.2-shape `tool:native_pin` synth (whose
//! `tool_type: tool` + `__executor_id__: native:*` shape is no longer
//! a real V5.3 dispatch path now that every runtime is `kind: runtime`).
//! The wording of every rejection is preserved byte-identically — that
//! is the point of these pin tests, and `DispatchCapabilities`'
//! V5.2-line-cited table values are the source of truth for the move.
//! Test names are intentionally retained so the mapping back to the
//! historical V5.2 inline branches stays grep-able.

mod common;

use std::path::Path;

use common::DaemonHarness;

// ── Helpers ────────────────────────────────────────────────────────────

/// Fixed test signing key. Used to pre-sign a synthesized
/// `runtime:pin-fake-runtime` YAML in user space AND a matching
/// trusted-signer entry, so the runtime registry verifies it at
/// daemon startup. Deterministic seed → reproducible fingerprint.
fn pin_test_signing_key() -> lillux::crypto::SigningKey {
    lillux::crypto::SigningKey::from_bytes(&[0x5Au8; 32])
}

/// Write a trusted-signer TOML for `vk` into
/// `<user_space>/.ai/config/keys/trusted/<fp>.toml`. Mirrors the
/// `bootstrap::write_self_trust` format so the engine's `TrustStore`
/// loader picks it up alongside the fixture trusted signers.
fn write_trusted_signer(user_space: &Path, vk: &lillux::crypto::VerifyingKey) -> anyhow::Result<()> {
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

/// Pre-init hook used by the three native-runtime pin tests: writes a
/// trust entry for `pin_test_signing_key()` into user-space, then
/// synthesizes `<user_space>/.ai/runtimes/pin-fake-runtime.yaml`
/// (`kind: runtime`, `serves: pin_fake_kind`,
/// `binary_ref: bin/<triple>/pin-fake-runtime`). Daemon engine init
/// scans user-space → finds the runtime → registers
/// `runtime:pin-fake-runtime`. The synthetic `serves` kind avoids
/// colliding with any default runtime served from a real bundle.
///
/// The binary need not exist on disk — every native pin test reaches
/// only the `DispatchCapabilities` rejection in
/// `dispatch_native_runtime`, which fires BEFORE
/// `launch::build_and_launch` materializes anything.
fn install_pin_runtime(_state_path: &Path, user_space: &Path) -> anyhow::Result<()> {
    let sk = pin_test_signing_key();
    let vk = sk.verifying_key();
    write_trusted_signer(user_space, &vk)?;

    let runtimes_dir = user_space.join(".ai/runtimes");
    std::fs::create_dir_all(&runtimes_dir)?;
    let body = r#"kind: runtime
serves: pin_fake_kind
binary_ref: bin/x86_64-unknown-linux-gnu/pin-fake-runtime
abi_version: "v1"
required_caps:
  - runtime.execute
description: "synth runtime for V5.3 dispatch_pin capability tests"
"#;
    let signed = lillux::signature::sign_content(body, &sk, "#", None);
    std::fs::write(runtimes_dir.join("pin-fake-runtime.yaml"), signed)?;
    Ok(())
}

/// POST /execute with arbitrary extra top-level fields (validate_only,
/// launch_mode, target_site_id, project_source) merged into the request body.
async fn post_execute_with_extras(
    h: &DaemonHarness,
    item_ref: &str,
    project_path: &str,
    parameters: serde_json::Value,
    extras: serde_json::Value,
) -> (reqwest::StatusCode, serde_json::Value) {
    let mut body = serde_json::json!({
        "item_ref": item_ref,
        "project_path": project_path,
        "parameters": parameters,
    });
    if let (Some(obj), Some(extra_obj)) = (body.as_object_mut(), extras.as_object()) {
        for (k, v) in extra_obj {
            obj.insert(k.clone(), v.clone());
        }
    }
    let resp = reqwest::Client::new()
        .post(format!("http://{}/execute", h.bind))
        .json(&body)
        .send()
        .await
        .expect("post /execute");
    let status = resp.status();
    let value = resp.json().await.unwrap_or(serde_json::json!({}));
    (status, value)
}

/// Synthesize a `.py` tool whose `__executor_id__` chains to the bundled
/// python script runtime (which itself aliases `@subprocess`). Mirrors the
/// in-process `hello_world_python.rs` setup but goes over HTTP. Used by
/// `pin_tool_over_tcp_succeeds` — gives a working terminal subprocess
/// invocation today without depending on workspace-relative bundle hashes.
async fn synth_tool_request(
    h: &DaemonHarness,
) -> (reqwest::StatusCode, serde_json::Value, tempfile::TempDir) {
    let project = tempfile::tempdir().expect("project tempdir");
    let tools_dir = project.path().join(".ai").join("tools");
    std::fs::create_dir_all(&tools_dir).expect("mkdir tools");
    let body = r#"#!/usr/bin/env python3
__version__ = "1.0.0"
__executor_id__ = "tool:rye/core/runtimes/python/script"
__category__ = "hello_pin"
__description__ = "V5.3 dispatch_pin tool"

import sys
print("pin")
sys.exit(0)
"#;
    let tool_dir = tools_dir.join("hello_pin");
    std::fs::create_dir_all(&tool_dir).expect("mkdir tool dir");
    std::fs::write(tool_dir.join("hello_pin.py"), body).expect("write tool");
    let (status, value) = post_execute_with_extras(
        h,
        "tool:hello_pin/hello_pin",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
        serde_json::json!({}),
    )
    .await;
    (status, value, project)
}

/// Spin up a daemon with a synth `runtime:pin-fake-runtime` registered
/// in user space, then POST /execute against that ref with the given
/// `extras` (launch_mode / target_site_id / project_source). Used by
/// the three native-runtime capability pin tests.
async fn native_synth_request(
    extras: serde_json::Value,
) -> (reqwest::StatusCode, serde_json::Value) {
    let h = DaemonHarness::start_with_pre_init(install_pin_runtime, |_| {})
        .await
        .expect("start daemon with pin runtime");
    let project = tempfile::tempdir().expect("project tempdir");
    let (status, value) = post_execute_with_extras(
        &h,
        "runtime:pin-fake-runtime",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
        extras,
    )
    .await;
    drop(project);
    (status, value)
}

// ── 1. service: + validate_only=true ───────────────────────────────────
//
// PIN: today the `service:` branch ignores `validate_only` entirely and
// returns the standard service envelope as if it weren't set. Task 0a
// (which migrates services to schema-driven dispatch) must preserve this
// shape for the no-op case OR explicitly change the contract; either way
// this assertion will catch the drift.

#[tokio::test(flavor = "multi_thread")]
async fn pin_service_validate_only() {
    let h = DaemonHarness::start().await.expect("start daemon");
    let (status, body) = post_execute_with_extras(
        &h,
        "service:bundle/list",
        ".",
        serde_json::json!({}),
        serde_json::json!({"validate_only": true}),
    )
    .await;

    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "expected 200, got {status}: {body}"
    );
    let thread = body.get("thread").expect("thread present");
    assert_eq!(
        thread.get("kind").and_then(|v| v.as_str()),
        Some("service_run"),
        "service envelope kind: {body}"
    );
    assert_eq!(
        thread.get("status").and_then(|v| v.as_str()),
        Some("completed"),
        "service ran to completion despite validate_only=true: {body}"
    );
    assert_eq!(
        thread.get("item_ref").and_then(|v| v.as_str()),
        Some("service:bundle/list"),
        "item_ref echo: {body}"
    );
    assert!(
        thread
            .get("thread_id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("svc-")),
        "thread_id has svc- prefix: {body}"
    );
    assert_eq!(
        thread.get("trust_class").and_then(|v| v.as_str()),
        Some("Trusted"),
        "core bundle service is Trusted: {body}"
    );
    assert!(
        thread.get("effective_caps").and_then(|v| v.as_array()).is_some(),
        "effective_caps array: {body}"
    );
    assert!(body.get("result").is_some(), "result key present: {body}");
}

// ── 2. native runtime + launch_mode=detached → 400 ─────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn pin_native_runtime_with_detached() {
    let (status, body) = native_synth_request(
        serde_json::json!({"launch_mode": "detached"}),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "expected 400, got {status}: {body}"
    );
    assert_eq!(
        body,
        serde_json::json!({
            "error": "detached mode not yet supported for native runtimes"
        }),
        "exact error body shape (V5.3 Task 0b's DispatchCapabilities table reproduces this)"
    );
}

// ── 3. native runtime + target_site_id → 400 ───────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn pin_native_runtime_with_target_site_id() {
    let (status, body) = native_synth_request(
        serde_json::json!({"target_site_id": "site:other"}),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "expected 400, got {status}: {body}"
    );
    assert_eq!(
        body,
        serde_json::json!({
            "error": "remote execution not yet supported for native runtimes"
        }),
        "exact error body shape (V5.3 Task 0b's DispatchCapabilities table reproduces this)"
    );
}

// ── 4. native runtime + project_source=pushed_head → 409 ───────────────
//
// SURPRISE relative to V5.3-PLAN.md: the plan describes this as a
// "current 400" produced by the inline `is_native_executor` branch's
// pushed_head rejection. In reality the native rejection is unreachable
// in this code path: `resolve_project_context()` for `pushed_head` runs
// BEFORE the dispatch capability check (see `api/execute.rs` ~lines
// 100-120) and fails with 409 "push first" because no CAS HEAD is
// pushed for the synth project.
//
// V5.3 Task 0b preserves the ordering: project-source resolution still
// runs before any schema-driven dispatch call, so the 409 still wins.

#[tokio::test(flavor = "multi_thread")]
async fn pin_native_runtime_with_pushed_head() {
    let (status, body) = native_synth_request(
        serde_json::json!({"project_source": {"kind": "pushed_head"}}),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::CONFLICT,
        "expected 409 (project_source resolution rejects before native check), got {status}: {body}"
    );
    let err = body
        .get("error")
        .and_then(|v| v.as_str())
        .expect("error str");
    assert!(
        err.starts_with("no pushed HEAD for project ") && err.ends_with(" — push first"),
        "exact error shape (modulo project tempdir path): {body}"
    );
}

// ── 5. service over TCP succeeds with V5.2 envelope shape ──────────────

#[tokio::test(flavor = "multi_thread")]
async fn pin_service_over_tcp_succeeds() {
    let h = DaemonHarness::start().await.expect("start daemon");
    let (status, body) = h
        .post_execute("service:bundle/list", ".", serde_json::json!({}))
        .await
        .expect("post /execute");
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "expected 200, got {status}: {body}"
    );
    let thread = body.get("thread").expect("thread envelope present");
    assert!(
        thread
            .get("thread_id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("svc-")),
        "thread.thread_id has svc- prefix: {body}"
    );
    assert_eq!(
        thread.get("kind").and_then(|v| v.as_str()),
        Some("service_run"),
        "thread.kind == service_run: {body}"
    );
    assert_eq!(
        thread.get("status").and_then(|v| v.as_str()),
        Some("completed"),
        "thread.status == completed: {body}"
    );
    assert_eq!(
        thread.get("item_ref").and_then(|v| v.as_str()),
        Some("service:bundle/list"),
        "thread.item_ref echo: {body}"
    );
    assert_eq!(
        thread.get("trust_class").and_then(|v| v.as_str()),
        Some("Trusted"),
        "thread.trust_class: {body}"
    );
    assert!(
        thread.get("effective_caps").and_then(|v| v.as_array()).is_some(),
        "thread.effective_caps array present: {body}"
    );
    assert!(
        body.get("result").is_some(),
        "top-level result present: {body}"
    );
    // Service envelope deliberately does NOT include the thread-lifecycle
    // fields (chain_root_id, executor_ref, runtime{pid,pgid}, ...). Pin
    // that absence so Task 0a doesn't accidentally homogenize them.
    for forbidden in [
        "chain_root_id",
        "executor_ref",
        "launch_mode",
        "runtime",
        "current_site_id",
        "origin_site_id",
        "requested_by",
    ] {
        assert!(
            thread.get(forbidden).is_none(),
            "service envelope must NOT include `{forbidden}`: {body}"
        );
    }
}

// ── 7. tool subprocess flows through unified dispatch::dispatch ────────
//
// PIN: post-V5.3 Task 7, `dispatch::dispatch` is the SOLE path from
// `/execute` to any terminator. This test documents that a `tool:*`
// ref (Subprocess terminator) round-trips through the unified entry
// and produces the V5.2 inline envelope shape — i.e. no silent
// fallback into a legacy code path. Any future rerouting that
// short-circuits dispatch.rs would change either the response shape
// or the thread.kind/thread.executor_ref values asserted here.

#[tokio::test(flavor = "multi_thread")]
async fn pin_subprocess_via_unified_dispatch_succeeds_for_tool_ref() {
    let h = DaemonHarness::start().await.expect("start daemon");
    let (status, body, _project) = synth_tool_request(&h).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "tool execute via dispatch::dispatch should succeed; got {status}: {body}"
    );
    let thread = body.get("thread").expect("thread envelope present");
    assert_eq!(
        thread.get("kind").and_then(|v| v.as_str()),
        Some("tool_run"),
        "thread.kind == tool_run (subprocess terminator preserves V5.2 shape): {body}"
    );
    assert_eq!(
        thread.get("item_ref").and_then(|v| v.as_str()),
        Some("tool:hello_pin/hello_pin"),
        "thread.item_ref echoes the original ref (alias chain not followed for tool/Subprocess): {body}"
    );
    assert!(
        body.get("result").is_some(),
        "inline envelope carries top-level result: {body}"
    );
    assert!(
        body.get("detached").is_none(),
        "inline (default launch_mode) must not set `detached`: {body}"
    );
}

// ── 6. tool over TCP succeeds with V5.2 thread envelope shape ──────────

#[tokio::test(flavor = "multi_thread")]
async fn pin_tool_over_tcp_succeeds() {
    let h = DaemonHarness::start().await.expect("start daemon");
    let (status, body, _project) = synth_tool_request(&h).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "tool execute should succeed; got {status}: {body}"
    );
    let thread = body.get("thread").expect("thread envelope present");
    assert!(
        thread
            .get("thread_id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("T-")),
        "thread.thread_id has T- prefix (NOT svc-): {body}"
    );
    assert_eq!(
        thread.get("kind").and_then(|v| v.as_str()),
        Some("tool_run"),
        "thread.kind == tool_run: {body}"
    );
    assert_eq!(
        thread.get("status").and_then(|v| v.as_str()),
        Some("completed"),
        "thread.status == completed: {body}"
    );
    assert_eq!(
        thread.get("item_ref").and_then(|v| v.as_str()),
        Some("tool:hello_pin/hello_pin"),
        "thread.item_ref echo: {body}"
    );
    assert_eq!(
        thread.get("executor_ref").and_then(|v| v.as_str()),
        Some("tool:rye/core/runtimes/python/script"),
        "thread.executor_ref points at the runtime: {body}"
    );
    assert_eq!(
        thread.get("launch_mode").and_then(|v| v.as_str()),
        Some("inline"),
        "thread.launch_mode default: {body}"
    );
    // Tool envelope MUST include lifecycle fields the service envelope
    // does not — these are the V5.2 thread shape Task 0b must preserve.
    for required in [
        "chain_root_id",
        "created_at",
        "started_at",
        "finished_at",
        "current_site_id",
        "origin_site_id",
        "requested_by",
        "runtime",
    ] {
        assert!(
            thread.get(required).is_some(),
            "tool envelope must include `{required}`: {body}"
        );
    }
}
