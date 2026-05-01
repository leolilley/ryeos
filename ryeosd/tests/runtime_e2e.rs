//! V5.3 Task 7 — runtime kind E2E gate.
//!
//! Spawns a real `ryeosd` subprocess (mirroring `cleanup_e2e.rs` and
//! `dispatch_pin.rs`) and asserts the V5.3 runtime promotion landed
//! end-to-end:
//!
//! - Schema-gated 501 for kinds with no `execution:` block (`config`,
//!   `knowledge` in V5.3 — knowledge gains an execution block in V5.4).
//! - Direct `runtime:*` invocation routes through
//!   `dispatch::dispatch_managed_subprocess` (proven via the synth
//!   pin-fake-runtime YAML reaching the protocol-derived
//!   `ProtocolCapabilities` resolution rather than any legacy native
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

use common::fast_fixture::FastFixture;
use common::DaemonHarness;
use lillux::crypto::SigningKey;

// ── Helpers (signing setup uses the fast fixture's publisher key) ──────

/// Install one signed runtime YAML in user space with a default
/// well-formed `binary_ref: bin/<host_triple>/<name>`. For tests that
/// need to exercise post-B1 gates (notably P1.5 below) use
/// [`install_runtime_with_binary_ref`] instead so a deliberately
/// malformed shape can drive `strip_binary_ref_prefix`.
fn install_runtime(
    user_space: &Path,
    name: &str,
    serves: &str,
    default: bool,
    abi_version: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    install_runtime_with_binary_ref(
        user_space,
        name,
        serves,
        default,
        abi_version,
        &format!("bin/x86_64-unknown-linux-gnu/{name}"),
        signer,
    )
}

/// Install one signed runtime YAML with an explicit `binary_ref`.
/// Lets a test feed deliberately malformed shapes (e.g. `"badshape"`)
/// so the dispatcher progresses past B1 / the cap gate and the
/// failure mode is unambiguously `strip_binary_ref_prefix` rejecting
/// the YAML.
fn install_runtime_with_binary_ref(
    user_space: &Path,
    name: &str,
    serves: &str,
    default: bool,
    abi_version: &str,
    binary_ref: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let runtimes_dir = user_space.join(".ai/runtimes");
    std::fs::create_dir_all(&runtimes_dir)?;
    let body = format!(
        r#"kind: runtime
serves: {serves}
default: {default}
binary_ref: {binary_ref}
abi_version: "{abi_version}"
required_caps:
  - runtime.execute
description: "synth runtime for runtime_e2e"
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(runtimes_dir.join(format!("{name}.yaml")), signed)?;
    Ok(())
}

// ── 1. config: ref → 501 (no `execution:` block) ───────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn e2e_config_ref_returns_501() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
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
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
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

// ── 3. Direct `runtime:*` invocation routes through dispatch_managed_subprocess ──

#[tokio::test(flavor = "multi_thread")]
async fn e2e_direct_runtime_routes_through_native_dispatch() {
    // Plant a synth runtime in user space; auth-disabled wildcard
    // scope satisfies `runtime.execute`, so dispatch_managed_subprocess
    // proceeds past the cap gate and reaches the binary materialization
    // step. The binary doesn't exist, so we expect an error mentioning
    // `native:` (the executor_ref dispatch_managed_subprocess synthesizes)
    // OR the bundle manifest. The KEY assertion: the error path is
    // taken, NOT a 200 silently fallthrough or a 500 generic.
    //
    // V5.4 P2.1 note: the `standard` bundle now ships
    // `ryeos-directive-runtime` and `ryeos-graph-runtime` in its
    // SourceManifest, but this harness uses `ryeos-bundles/core` as
    // RYE_SYSTEM_SPACE (see `ryeosd/tests/common/mod.rs::system_data_dir`)
    // and `core` has no `bin/` directory. Real coverage would plant a
    // signed `bundles` registration under
    // `<system_data_dir>/.ai/node/bundles/standard.yaml` so the engine
    // also walks `ryeos-bundles/standard` (kinds from `core`, binaries
    // from `standard`). TODO(V5.4): wire that registration into the
    // harness once the bundle install/registration writer is exposed
    // to tests; for now this test pins only that the dispatch loop
    // reaches the materialization step and surfaces a clean lookup
    // error rather than a silent fallthrough.
    let plant = |_: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        install_runtime(user, "e2e-direct-runtime", "e2e_kind", true, "v1", &fixture.publisher)
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
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
    let state_dir_outer = tempfile::tempdir().expect("state dir");
    let user_space = tempfile::tempdir().expect("user space");
    let state_path = state_dir_outer.path().join("state");

    // Pre-populate via fast fixture (no --init-if-missing on the daemon
    // command below). populate_initialized_state writes the deterministic
    // node identity, vault keypair, user identity, and trust docs, plus
    // imports the system-bundle signer trust.
    let fixture =
        common::fast_fixture::populate_initialized_state(&state_path, user_space.path())
            .expect("fast fixture populate");

    install_runtime(
        user_space.path(),
        "dup-runtime-a",
        "dup_kind",
        true,
        "v1",
        &fixture.publisher,
    )
    .expect("plant dup-runtime-a");
    install_runtime(
        user_space.path(),
        "dup-runtime-b",
        "dup_kind",
        true,
        "v1",
        &fixture.publisher,
    )
    .expect("plant dup-runtime-b");

    let port = common::pick_free_port();
    let bind = format!("127.0.0.1:{port}");
    let uds_path = state_path.join("ryeosd.sock");

    let mut cmd = Command::new(common::ryeosd_binary());
    cmd.arg("--state-dir")
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
// onto `runtime:e2e-directive-runtime`; `dispatch_managed_subprocess`
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
    let plant = |_: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        // Synth runtime serves "directive". With only a single runtime
        // serving the kind, `RuntimeRegistry::lookup_for("directive")`
        // returns it regardless of `default`.
        install_runtime(user, "e2e-directive-runtime", "directive", true, "v1", &fixture.publisher)?;
        // Synth directive item — minimal valid YAML so engine
        // resolution succeeds and the dispatch loop reaches the
        // `@directive` alias / registry hop.
        let dir = user.join(".ai/directives/e2e_b1");
        std::fs::create_dir_all(&dir)?;
        let body = r#"---
name: flow
category: "e2e_b1"
description: "B1 indirect-alias e2e"
inputs: {}
---
# E2E B1
"#;
        let signed = lillux::signature::sign_content(body, &fixture.publisher, "#", None);
        std::fs::write(dir.join("flow.md"), signed)?;
        Ok(())
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
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

// ── 5b. P1.5 — paired B1 e2e: direct vs indirect on a malformed runtime ─
//
// The pre-V5.4 B1 e2e (`e2e_directive_via_registry_does_not_require_runtime_execute`)
// asserted only `status != 403`. That assertion is structurally weak
// because the daemon's auth surface is off in the test harness, so
// caller_scopes default to `["*"]` and the runtime.execute cap is
// auto-satisfied — the test cannot tell "B1 was reached and skipped"
// from "B1 fired but `*` made it pass".
//
// This pair fixes that ambiguity by feeding a deliberately malformed
// `binary_ref: badshape`. The dispatcher's order is:
//
//   1. resolve_dispatch_hop attaches VerifiedRuntime         (succeeds)
    //   2. dispatch_managed_subprocess: B1 cap gate (gated on
//      original_root_kind == "runtime")                       (skipped on indirect)
//   3. check_dispatch_capabilities                            (succeeds)
//   4. strip_binary_ref_prefix("badshape")                    (FAILS with
//      `SchemaMisconfigured { detail: "...unexpected shape..." }` → 400)
//
// So an indirect call must surface a 400 whose body contains
// `"unexpected shape"`. That precise wording is what proves the
// dispatch loop walked PAST the B1 cap-check site to reach
// strip_binary_ref_prefix.
//
// The "direct must 403 without cap" half of the pair lives in
// dispatch.rs's unit tests (`enforce_runtime_caps_*` — covers the
// gate-fires-when-cap-missing contract) because the e2e harness has
// no facility to install a Principal with limited scopes. When the
// daemon gains an auth-on test harness (post-P2.x), this comment
// should be replaced by the live e2e variant.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_via_registry_reaches_strip_binary_ref_prefix() {
    let plant = |_: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        // Synth runtime serves "directive" with a deliberately
        // malformed binary_ref. P1.5: the dispatcher must walk past
        // B1's cap-gate site and hit strip_binary_ref_prefix.
        install_runtime_with_binary_ref(
            user,
            "p15-bad-directive-runtime",
            "directive",
            true,
            "v1",
            "badshape",
            &fixture.publisher,
        )?;
        let dir = user.join(".ai/directives/p15");
        std::fs::create_dir_all(&dir)?;
        let body = r#"---
name: flow
category: "p15"
description: "P1.5 reach-past-B1 e2e"
inputs: {}
---
# P1.5
"#;
        let signed = lillux::signature::sign_content(body, &fixture.publisher, "#", None);
        std::fs::write(dir.join("flow.md"), signed)?;
        Ok(())
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with synth bad-binary-ref runtime");

    let project = tempfile::tempdir().expect("project tempdir");
    let (status, body) = h
        .post_execute(
            "directive:p15/flow",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post /execute");

    // Must be 400 (SchemaMisconfigured → BAD_REQUEST), NOT 403
    // (which would mean B1 fired) and NOT 502 (which would mean
    // materialization failed before strip_binary_ref_prefix).
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "indirect path with malformed binary_ref must return 400 from \
         strip_binary_ref_prefix; got {status}: {body}"
    );

    // The body MUST mention "unexpected shape" — that string is
    // emitted only by `strip_binary_ref_prefix` rejecting the
    // `binary_ref` shape. Its presence proves the dispatch loop walked
    // past the B1 cap-gate location (B1 fires earlier in
    // dispatch_managed_subprocess than strip_binary_ref_prefix).
    let err_str = body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        err_str.contains("unexpected shape"),
        "body must contain 'unexpected shape' from strip_binary_ref_prefix \
         to prove dispatch reached past B1; got: {body}"
    );

    drop(project);
}

// ── 5c. P1.6 — root/runtime split pin: subject identity wins audit ─────
//
// Pre-V5.4 the indirect dispatch path (`directive:foo` → registry →
// `runtime:directive-runtime`) recorded the thread with the
// **runtime**'s `thread_profile` (`runtime_run`) and the runtime's
// `item_ref`. P1.1 introduced `RootSubject` so the audit captures the
// caller-typed subject's identity, not the executor's.
//
// This test pins that contract end-to-end: synth a directive served
// by a synth runtime whose well-shaped `binary_ref` points to a
// non-existent binary. The dispatcher walks
//   directive:p16/flow
//     → registry hop → runtime:p16-directive-runtime
//     → strip_binary_ref_prefix succeeds (well-formed shape)
//     → build_and_launch step 1 creates the thread DB row
//     → step 7 resolve_native_executor_path FAILS (no binary)
//
// The thread row therefore persists at status="created" with the
// SUBJECT's `kind` and `item_ref`. We open `projection.sqlite3`
// directly and assert.
//
// If the root/runtime split regresses, this test will see
// `kind == "runtime_run"` and `item_ref` starting with `runtime:` —
// failing loudly.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_indirect_directive_audit_records_subject_not_runtime() {
    let plant = |_: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        // Well-formed binary_ref pointing at a nonexistent binary.
        // Dispatch progresses through strip_binary_ref_prefix and into
        // build_and_launch, which creates the thread DB row before
        // failing at native-executor materialization.
        install_runtime(
            user,
            "p16-directive-runtime",
            "directive",
            true,
            "v1",
            &fixture.publisher,
        )?;
        let dir = user.join(".ai/directives/p16");
        std::fs::create_dir_all(&dir)?;
        let body = r#"---
name: flow
category: "p16"
description: "P1.6 root/runtime split pin"
inputs: {}
---
# P1.6
"#;
        let signed =
            lillux::signature::sign_content(body, &fixture.publisher, "<!--", Some("-->"));
        std::fs::write(dir.join("flow.md"), signed)?;
        Ok(())
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    let (status, body) = h
        .post_execute(
            "directive:p16/flow",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post /execute");

    // We expect a downstream failure (no binary in manifest) — could
    // be 502 (RuntimeMaterializationFailed) or 500. The KEY is that
    // dispatch reached build_and_launch's thread-create step.
    assert!(
        !status.is_success(),
        "expected dispatch to fail at materialization; got {status}: {body}"
    );

    // Open the projection DB and find the thread row created for
    // this directive invocation. ProjectionDb writes happen on the
    // daemon side; give it a brief settle window so the row is
    // visible to a fresh read.
    let projection_path = h.state_path.join(".ai/state/projection.sqlite3");
    for _ in 0..20 {
        if projection_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        projection_path.exists(),
        "projection.sqlite3 must exist at {}",
        projection_path.display()
    );

    let db = ryeos_state::projection::ProjectionDb::open(&projection_path)
        .expect("open projection db");
    let threads = ryeos_state::queries::list_threads(&db, 100)
        .expect("list_threads");

    let subject_thread = threads
        .iter()
        .find(|t| t.item_ref == "directive:p16/flow")
        .unwrap_or_else(|| {
            panic!(
                "no thread row with subject item_ref 'directive:p16/flow' \
                 — root/runtime split regressed. All rows: {:#?}",
                threads
            )
        });

    // ── P1.1 contract assertions ────────────────────────────────────
    assert_eq!(
        subject_thread.kind, "directive_run",
        "thread.kind must be the SUBJECT's thread_profile ('directive_run'), \
         not the runtime's ('runtime_run'). Got: {:#?}",
        subject_thread
    );
    assert_eq!(
        subject_thread.item_ref, "directive:p16/flow",
        "thread.item_ref must echo the user-typed subject ref, not the \
         runtime ref. Got: {:#?}",
        subject_thread
    );
    assert!(
        subject_thread.executor_ref.starts_with("native:"),
        "thread.executor_ref records the runtime executor binary; got: {:?}",
        subject_thread.executor_ref
    );

    // Defense in depth: there must NOT be a separate row recorded
    // against the runtime ref — that would mean the loop was
    // double-recording.
    let runtime_rows: Vec<_> = threads
        .iter()
        .filter(|t| t.item_ref.starts_with("runtime:"))
        .collect();
    assert!(
        runtime_rows.is_empty(),
        "no thread row should be recorded against the runtime ref; got: {:#?}",
        runtime_rows
    );

    drop(project);
}

// ── 5d. P4.B2 — graph indirect: subject identity wins audit ────────────
//
// Mirror of `e2e_indirect_directive_audit_records_subject_not_runtime`
// for the graph kind. Pins that an indirect dispatch chain
//   graph:p4/flow → registry → runtime:p4-graph-runtime
// records the SUBJECT's identity (`graph_run` thread_profile +
// `graph:p4/flow` item_ref), not the runtime's. If the V5.5 P4 B2
// subject/runtime split regresses for graphs specifically, this test
// will see `kind == "runtime_run"` and an item_ref starting with
// `runtime:` — and fail loud.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_indirect_graph_records_graph_thread_profile() {
    let plant = |_: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        // Synth runtime serving "graph" with a well-formed binary_ref
        // pointing at a non-existent binary. Dispatch progresses into
        // build_and_launch and creates the thread DB row before
        // failing at native-executor materialization — the row's
        // subject identity is what we assert.
        install_runtime(user, "p4-graph-runtime", "graph", true, "v1", &fixture.publisher)?;
        let dir = user.join(".ai/graphs/p4");
        std::fs::create_dir_all(&dir)?;
        let body = r#"category: "p4"
description: "P4 B2 graph subject/runtime split pin"
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
        // Graph YAMLs use `#` for signature comments (matching
        // `parser:rye/core/yaml/yaml`).
        let signed = lillux::signature::sign_content(body, &fixture.publisher, "#", None);
        std::fs::write(dir.join("flow.yaml"), signed)?;
        Ok(())
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    let (status, body) = h
        .post_execute(
            "graph:p4/flow",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post /execute");

    // Dispatch is expected to fail at materialization (no binary for
    // p4-graph-runtime). The KEY assertion is that the dispatch loop
    // reached build_and_launch's thread-create step.
    assert!(
        !status.is_success(),
        "expected dispatch to fail at materialization; got {status}: {body}"
    );

    let projection_path = h.state_path.join(".ai/state/projection.sqlite3");
    for _ in 0..20 {
        if projection_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        projection_path.exists(),
        "projection.sqlite3 must exist at {}",
        projection_path.display()
    );

    let db = ryeos_state::projection::ProjectionDb::open(&projection_path)
        .expect("open projection db");
    let threads = ryeos_state::queries::list_threads(&db, 100)
        .expect("list_threads");

    let subject_thread = threads
        .iter()
        .find(|t| t.item_ref == "graph:p4/flow")
        .unwrap_or_else(|| {
            panic!(
                "no thread row with subject item_ref 'graph:p4/flow' \
                 — root/runtime split regressed for graph kind. \
                 All rows: {:#?}",
                threads
            )
        });

    // ── B2 contract assertions ──────────────────────────────────────
    assert_eq!(
        subject_thread.kind, "graph_run",
        "thread.kind must be the graph subject's thread_profile \
         ('graph_run'), not the runtime's ('runtime_run'). Got: {:#?}",
        subject_thread
    );
    assert_eq!(
        subject_thread.item_ref, "graph:p4/flow",
        "thread.item_ref must echo the user-typed graph subject ref, \
         not the runtime ref. Got: {:#?}",
        subject_thread
    );
    assert!(
        subject_thread.executor_ref.starts_with("native:"),
        "thread.executor_ref records the runtime executor binary; got: {:?}",
        subject_thread.executor_ref
    );

    // Defense in depth: no parallel thread row recorded against the
    // graph runtime ref.
    let runtime_rows: Vec<_> = threads
        .iter()
        .filter(|t| t.item_ref.starts_with("runtime:"))
        .collect();
    assert!(
        runtime_rows.is_empty(),
        "no thread row should be recorded against the runtime ref; got: {:#?}",
        runtime_rows
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
