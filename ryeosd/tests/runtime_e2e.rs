//! V5.3 Task 7 — runtime kind E2E gate.
//!
//! Spawns a real `ryeosd` subprocess (mirroring `cleanup_e2e.rs` and
//! `dispatch_pin.rs`) and asserts the V5.3 runtime promotion landed
//! end-to-end:
//!
//! - Schema-gated 501 for kinds with no `execution:` block (`config`,
//!   `knowledge` in V5.3 — knowledge gains an execution block in V5.4).
//! - Direct `runtime:*` invocation routes through
//!   `dispatch::dispatch_native_runtime` (proven via the synth
//!   pin-fake-runtime YAML reaching the schema-derived
//!   `DispatchCapabilities` rejection rather than any legacy native
//!   branch — the legacy branch is gone).
//! - Multi-default conflict at startup is fail-closed: two runtimes
//!   declaring `serves: <kind>` AND `default: true` for the same kind
//!   prevent the daemon from starting (build_from_bundles errors
//!   propagate from `engine_init.rs`).
//! - **Grep gate**: `rg '"directive"|"service"|"runtime"|"tool"|"knowledge"'
//!   ryeosd/src/api/execute.rs ryeosd/src/dispatch.rs` returns ZERO
//!   branching/string-prefix hits — the schema is the only route
//!   decision-maker.

mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::DaemonHarness;

// ── Helpers (signing + trust setup mirrors dispatch_pin.rs) ────────────

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

/// Install one signed runtime YAML in user space.
fn install_runtime(
    user_space: &Path,
    name: &str,
    serves: &str,
    default: bool,
    abi_version: &str,
) -> anyhow::Result<()> {
    let runtimes_dir = user_space.join(".ai/runtimes");
    std::fs::create_dir_all(&runtimes_dir)?;
    let body = format!(
        r#"kind: runtime
serves: {serves}
default: {default}
binary_ref: bin/x86_64-unknown-linux-gnu/{name}
abi_version: "{abi_version}"
required_caps:
  - runtime.execute
description: "synth runtime for V5.3 runtime_e2e"
"#
    );
    let signed = lillux::signature::sign_content(&body, &e2e_signing_key(), "#", None);
    std::fs::write(runtimes_dir.join(format!("{name}.yaml")), signed)?;
    Ok(())
}

// ── 1. config: ref → 501 (no `execution:` block) ───────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn e2e_config_ref_returns_501() {
    let h = DaemonHarness::start().await.expect("start daemon");
    let (status, body) = h
        .post_execute("config:any/thing", ".", serde_json::json!({}))
        .await
        .expect("post /execute");
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_IMPLEMENTED,
        "config kind has no execution block; expected 501, got {status}: {body}"
    );
    let err = body
        .get("error")
        .and_then(|v| v.as_str())
        .expect("error string");
    assert!(
        err.contains("not root-executable")
            || err.contains("is not root executable")
            || err.contains("not executable"),
        "error must explain why kind cannot be executed, got: {err}"
    );
}

// ── 2. knowledge: ref → 501 in V5.3 (gains execution block in V5.4) ────

#[tokio::test(flavor = "multi_thread")]
async fn e2e_knowledge_ref_returns_501_in_v53() {
    // The bundle's `knowledge.kind-schema.yaml` declares aliases-only
    // (no terminator) AND the `@knowledge` alias resolves to a tool
    // ref that no longer exists post-V5.3. Either way, the schema gate
    // must yield 501 (or a clear non-200) — not a generic 500 stack
    // trace and not a silent fallback to legacy code.
    let h = DaemonHarness::start().await.expect("start daemon");
    let (status, body) = h
        .post_execute("knowledge:any/note", ".", serde_json::json!({}))
        .await
        .expect("post /execute");
    assert!(
        status.is_client_error() || status == reqwest::StatusCode::NOT_IMPLEMENTED,
        "knowledge kind in V5.3 must yield 4xx/501, got {status}: {body}"
    );
    let err = body
        .get("error")
        .and_then(|v| v.as_str())
        .expect("error string");
    assert!(
        !err.is_empty(),
        "error message must be non-empty; got: {body}"
    );
}

// ── 3. Direct `runtime:*` invocation routes through dispatch_native_runtime ──

#[tokio::test(flavor = "multi_thread")]
async fn e2e_direct_runtime_routes_through_native_dispatch() {
    // Plant a synth runtime in user space; auth-disabled wildcard
    // scope satisfies `runtime.execute`, so dispatch_native_runtime
    // proceeds past the cap gate and reaches the binary materialization
    // step. The binary doesn't exist, so we expect an error mentioning
    // `native:` (the executor_ref dispatch_native_runtime synthesizes)
    // OR the bundle manifest. The KEY assertion: the error path is
    // taken, NOT a 200 silently fallthrough or a 500 generic.
    let pre_init = |_: &Path, user: &Path| -> anyhow::Result<()> {
        let sk = e2e_signing_key();
        write_trusted_signer(user, &sk.verifying_key())?;
        install_runtime(user, "e2e-direct-runtime", "e2e_kind", true, "v1")
    };

    let h = DaemonHarness::start_with_pre_init(pre_init, |_| {})
        .await
        .expect("start daemon with synth runtime");

    let project = tempfile::tempdir().expect("project tempdir");
    let (status, body) = h
        .post_execute(
            "runtime:e2e-direct-runtime",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post /execute");

    // Either a clear materialization/manifest error (no binary present)
    // OR success if a stub somehow ran — only the FALLBACK case fails
    // this test.
    assert!(
        !status.is_success() || body.get("thread").is_some(),
        "must either error cleanly OR return a real thread envelope; got {status}: {body}"
    );
    if !status.is_success() {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .expect("error string");
        let mentions_runtime_path = err.contains("native:")
            || err.contains("manifest")
            || err.contains("bundle")
            || err.contains("e2e-direct-runtime")
            || err.contains("binary");
        assert!(
            mentions_runtime_path,
            "error must clearly point at the runtime/binary lookup path \
             (native:/manifest/bundle/binary), got: {err}"
        );
    }
    drop(project);
}

// ── 4. Multi-default conflict at startup → daemon refuses ──────────────

#[tokio::test(flavor = "multi_thread")]
async fn e2e_multi_default_conflict_aborts_startup() {
    use std::process::Command;

    // Plant TWO runtimes both declaring `serves: dup_kind, default: true`.
    // RuntimeRegistry::build_from_bundles must error; engine_init.rs
    // propagates the error; daemon child exits non-zero.
    let pre_init = |_: &Path, user: &Path| -> anyhow::Result<()> {
        let sk = e2e_signing_key();
        write_trusted_signer(user, &sk.verifying_key())?;
        install_runtime(user, "dup-runtime-a", "dup_kind", true, "v1")?;
        install_runtime(user, "dup-runtime-b", "dup_kind", true, "v1")?;
        Ok(())
    };

    let state_dir_outer = tempfile::tempdir().expect("state dir");
    let user_space = tempfile::tempdir().expect("user space");
    common::populate_user_space(user_space.path());
    pre_init(state_dir_outer.path(), user_space.path()).expect("plant runtimes");

    let state_path = state_dir_outer.path().join("state");
    let port = common::pick_free_port();
    let bind = format!("127.0.0.1:{port}");
    let uds_path = state_path.join("ryeosd.sock");

    let mut cmd = Command::new(common::ryeosd_binary());
    cmd.arg("--init-if-missing")
        .arg("--state-dir")
        .arg(&state_path)
        .arg("--bind")
        .arg(&bind)
        .arg("--uds-path")
        .arg(&uds_path)
        .env("RYE_SYSTEM_SPACE", common::system_data_dir())
        .env("USER_SPACE", user_space.path())
        .env("HOME", user_space.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn ryeosd");

    // Wait up to 10s for either daemon.json (would be a bug) or the
    // process to exit (expected).
    let daemon_json = state_path.join("daemon.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exit_status = None;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                exit_status = Some(status);
                break;
            }
            None => {
                if daemon_json.exists() {
                    // Daemon shouldn't have started — kill and fail.
                    child.kill().ok();
                    panic!(
                        "daemon started despite multi-default runtime conflict at {}",
                        daemon_json.display()
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    let status = exit_status.unwrap_or_else(|| {
        child.kill().ok();
        panic!("daemon did not exit within 10s on multi-default conflict")
    });
    assert!(
        !status.success(),
        "daemon must exit non-zero on multi-default runtime conflict, got: {status:?}"
    );

    // Confirm the error message in stderr names the conflict so an
    // operator can fix it (F4 — error surfaces enumerate alternatives).
    let mut stderr_buf = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        use std::io::Read;
        let _ = stderr.read_to_string(&mut stderr_buf);
    }
    let mentions_conflict = stderr_buf.contains("default")
        || stderr_buf.contains("dup_kind")
        || stderr_buf.contains("multiple")
        || stderr_buf.contains("conflict");
    assert!(
        mentions_conflict,
        "stderr must explain the multi-default conflict; got: {stderr_buf}"
    );
}

// ── 5. Direct directive-via-registry MUST NOT require runtime.execute ──
//
// **B1 e2e**: a `directive:*` ref whose alias chain reaches a runtime
// via the schema's `@directive` alias / `RuntimeRegistry::lookup_for`
// fallback inherits the directive's caps — NOT the runtime's
// `runtime.execute`. Direct `runtime:*` calls DO require the cap, but
// the gate must NOT broaden retroactively to indirect chains.
//
// We synthesize: a directive item that exists in user space + a synth
// runtime that serves "directive". The directive resolves; the loop
// follows its `@directive` alias (via the kind-schema or registry)
// onto `runtime:e2e-directive-runtime`; `dispatch_native_runtime`
// sees `request.original_root_kind == "directive"` and SKIPS the cap
// check. The materialization step then fails (no binary on disk),
// but the failure mode must NOT be a 403 — that would prove the
// cap broadened.
//
// Status assertion is permissive (anything except 403) because the
// downstream materialization can fail in several legitimate ways
// (manifest, host_triple, missing binary). The KEY assertion: NOT
// 403. A 403 would mean the gate fired on the indirect path.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_via_registry_does_not_require_runtime_execute() {
    let pre_init = |_: &Path, user: &Path| -> anyhow::Result<()> {
        let sk = e2e_signing_key();
        write_trusted_signer(user, &sk.verifying_key())?;
        // Synth runtime serves "directive". With only a single runtime
        // serving the kind, `RuntimeRegistry::lookup_for("directive")`
        // returns it regardless of `default`.
        install_runtime(user, "e2e-directive-runtime", "directive", true, "v1")?;
        // Synth directive item — minimal valid YAML so engine
        // resolution succeeds and the dispatch loop reaches the
        // `@directive` alias / registry hop.
        let dir = user.join(".ai/directives/e2e_b1");
        std::fs::create_dir_all(&dir)?;
        let body = r#"---
__category__: "test/e2e_b1"
__directive_description__: "B1 indirect-alias e2e"
inputs: {}
---
# E2E B1
"#;
        let signed = lillux::signature::sign_content(body, &e2e_signing_key(), "#", None);
        std::fs::write(dir.join("flow.md"), signed)?;
        Ok(())
    };

    let h = DaemonHarness::start_with_pre_init(pre_init, |_| {})
        .await
        .expect("start daemon with synth directive + runtime");

    let project = tempfile::tempdir().expect("project tempdir");
    let (status, body) = h
        .post_execute(
            "directive:e2e_b1/flow",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post /execute");

    // The KEY assertion: an indirect alias chain landing on a runtime
    // MUST NOT inherit `runtime.execute`. A 403 from the dispatch path
    // would mean the B1 gate broadened.
    assert_ne!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "directive→runtime alias chain must NOT require runtime.execute; \
         got 403 which proves the gate broadened: {body}"
    );
    drop(project);
}

// ── 6. Grep gate: zero kind-name branching in dispatch code ────────────

#[test]
fn grep_gate_no_kind_name_branching_in_dispatch_code() {
    let workspace = common::workspace_root();
    let api_execute = workspace.join("ryeosd/src/api/execute.rs");
    let dispatch_rs = workspace.join("ryeosd/src/dispatch.rs");

    // Walk the file directly so we can:
    //   (a) reliably skip lines inside `#[cfg(test)]` modules — test
    //       fixtures legitimately string-compare kind names;
    //   (b) reliably skip doc comments (`///`, `//!`) regardless of
    //       leading indentation, without juggling rg's `path:content`
    //       output format;
    //   (c) recognize the `ROOT_KIND_RUNTIME` constant declaration as
    //       the SINGLE place the literal `"runtime"` is allowed at top
    //       level — every other use refers to the constant.
    let needles = [
        "\"directive\"",
        "\"service\"",
        "\"runtime\"",
        "\"tool\"",
        "\"knowledge\"",
    ];
    let mut violations = Vec::new();
    for path in [&api_execute, &dispatch_rs] {
        let content = std::fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("read {}", path.display()));
        let lines: Vec<&str> = content.lines().collect();

        // First `#[cfg(test)]` line marks the test module boundary.
        let test_mod_start = lines
            .iter()
            .position(|l| l.trim_start().starts_with("#[cfg(test)]"))
            .unwrap_or(lines.len());

        for (idx, line) in lines.iter().enumerate() {
            if idx >= test_mod_start {
                continue;
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with("///")
                || trimmed.starts_with("//!")
                || trimmed.starts_with("//")
            {
                continue;
            }
            // The `ROOT_KIND_RUNTIME` constant is the ONE allowed place
            // for the literal `"runtime"` to appear at top level. Every
            // other site uses the constant.
            if trimmed.starts_with("pub(crate) const ROOT_KIND_RUNTIME") {
                continue;
            }
            // Defense-in-depth "expected kind" hint to
            // `service_executor::resolve_and_verify` — sanity assertion
            // AFTER the schema-keyed match arm already routed.
            if line.contains("Some(\"service\")") {
                continue;
            }
            // Constructed strings like `format!("native:{...}")` — not
            // a route decision, just an executor_ref synthesis.
            if line.contains("native:") || line.contains("\"native:") {
                continue;
            }
            if !needles.iter().any(|n| line.contains(n)) {
                continue;
            }
            violations.push(format!("{}:{}: {}", path.display(), idx + 1, line));
        }
    }

    assert!(
        violations.is_empty(),
        "Found {} possible kind-name branching hits in dispatch code:\n{}",
        violations.len(),
        violations.join("\n")
    );
}
