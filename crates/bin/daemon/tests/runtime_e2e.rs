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
//!   `ProtocolCapabilities` resolution rather than any old native
//!   branch — the old branch is gone).
//! - Multi-default conflict at startup is fail-closed: two runtimes
//!   declaring `serves: <kind>` AND `default: true` for the same kind
//!   prevent the daemon from starting (build_from_bundles errors
//!   propagate from `engine_init.rs`).
//! - **Grep gate**: `rg '"directive"|"service"|"runtime"|"tool"|"knowledge"'
//!   crates/bin/daemon/src/routes/response_modes/execute_mode.rs crates/bin/daemon/src/dispatch.rs` returns ZERO
//!   branching/string-prefix hits — the schema is the only route
//!   decision-maker.

mod common;

use std::path::Path;

use common::fast_fixture::FastFixture;
use common::DaemonHarness;
use lillux::crypto::SigningKey;

// ── Helpers (signing setup uses the fast fixture's publisher key) ──────

/// Install one signed runtime YAML and its complete signed binary provenance
/// at `<root>/.ai/runtimes/<name>.yaml`. `root` must be a registered bundle
/// root so `RuntimeRegistry::build_from_bundles` picks it up. Admission tests
/// that intentionally author a malformed ref use
/// [`install_runtime_with_binary_ref`] directly.
fn install_runtime(
    root: &Path,
    name: &str,
    serves: &str,
    default: bool,
    abi_version: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let binary_ref = common::fast_fixture::install_signed_bundle_binary(
        root,
        name,
        b"#!/bin/sh\nexit 70\n",
        signer,
    )?;
    install_runtime_with_binary_ref(
        root,
        name,
        serves,
        default,
        abi_version,
        &binary_ref,
        signer,
    )
}

/// Install one signed runtime YAML with an explicit `binary_ref`.
/// This is reserved for admission tests that deliberately author malformed
/// descriptor data; runnable fixtures use [`install_runtime`] so they cannot
/// accidentally omit the installed binary's provenance chain.
fn install_runtime_with_binary_ref(
    root: &Path,
    name: &str,
    serves: &str,
    default: bool,
    abi_version: &str,
    binary_ref: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let runtimes_dir = root.join(".ai/runtimes");
    std::fs::create_dir_all(&runtimes_dir)?;
    let body = format!(
        r#"kind: runtime
serves: {serves}
default: {default}
binary_ref: {binary_ref}
abi_version: "{abi_version}"
required_caps:
  - runtime.execute
launch_contract:
  primary_allowed_kinds: [{serves}]
  primary_allowed_spaces: [bundle, project]
  primary_allowed_trust: [trusted_bundle, trusted_project]
  ref_bindings: {{}}
  preparation:
    kind: none
  config_inputs: {{}}
  secret_policy:
    max_requirements: 0
    allowed_names: []
  required_runtime_data: []
  runtime_facts: {{}}
description: "synth runtime for runtime_e2e"
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(runtimes_dir.join(format!("{name}.yaml")), signed)?;
    Ok(())
}

/// Install a minimal kind schema for `kind` at
/// `<root>/.ai/node/engine/kinds/<kind>/` so the engine's
/// RuntimeRegistry boot validation (ε.2) accepts a runtime that serves
/// it. `root` must be a registered bundle root. The schema declares an
/// executable kind that delegates to the
/// runtime registry — exactly the contract a synth `install_runtime`
/// line implies.
fn install_kind_schema(root: &Path, kind: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let kinds_dir = root.join(format!(".ai/node/engine/kinds/{kind}"));
    std::fs::create_dir_all(&kinds_dir)?;
    let body = format!(
        r##"category: "engine/kinds/{kind}"
version: "1.0.0"
resolution: []
effective_trust:
  include_references: false
location:
  directory: {kind}_items
execution:
  delegate:
    via: runtime_registry
  thread_profile:
    name: {kind}_run
    root_executable: true
    supports_interrupt: false
    supports_continuation: false
formats:
  - extensions: [".yaml"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: "#"
composer: handler:ryeos/core/identity
composed_value_contract:
  root_type: mapping
  required: {{}}
metadata:
  rules: {{}}
"##
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(kinds_dir.join(format!("{kind}.kind-schema.yaml")), signed)?;
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
    // trace and not a silent fallback to old code.
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
    // Plant a synth runtime in its own registered bundle; auth-disabled wildcard
    // scope satisfies `runtime.execute`, so dispatch_managed_subprocess
    // proceeds past the cap gate and reaches its exact manifest-anchored stub.
    // The stub exits non-zero, so we expect either a real thread envelope or a
    // runtime error, never a silent dispatch fallthrough.
    //
    // The plant below registers the standard bundle alongside core via
    // `common::fast_fixture::register_standard_bundle` — the signed
    // `.ai/node/bundles/standard.yaml` registration writer that closes the
    // former blocker — so the engine walks both `bundles/core` (kinds) and
    // `bundles/standard` (binaries). This case deliberately plants a
    // *synthetic* runtime whose binary and executor manifest live in a separate
    // synthetic bundle so its provenance cannot mutate or impersonate the
    // copied core bundle's manifest authority. Resolution
    // through the standard bundle's *real* directive/graph runtimes,
    // end-to-end, is covered by tests 5 / 5c / 5d below.
    let plant = |state: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        common::fast_fixture::register_standard_bundle(state, fixture)?;
        let bundle_root = state.join(".ai/bundles/runtime-e2e-direct");
        std::fs::create_dir_all(&bundle_root)?;
        install_kind_schema(&bundle_root, "e2e_kind", &fixture.publisher)?;
        install_runtime(
            &bundle_root,
            "e2e-direct-runtime",
            "e2e_kind",
            true,
            "v1",
            &fixture.publisher,
        )?;
        common::fast_fixture::register_fixture_bundle(
            state,
            "runtime-e2e-direct",
            &bundle_root,
            fixture,
        )
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with synth runtime");

    let (status, body) = h
        .post_execute("runtime:e2e-direct-runtime", ".", serde_json::json!({}))
        .await
        .expect("post /execute");

    // Either a clear runtime/protocol error from the signed stub OR a real
    // thread envelope is acceptable; only a dispatch fallthrough fails.
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
}

// ── 4. Multi-default conflict at startup → daemon refuses ──────────────

#[tokio::test(flavor = "multi_thread")]
async fn e2e_multi_default_conflict_aborts_startup() {
    // Plant TWO runtimes both declaring `serves: dup_kind, default: true`.
    // RuntimeRegistry::build_from_bundles must error; engine_init.rs
    // propagates a terminal readiness failure. `daemon.json` is only an early
    // discovery hint and is expected to exist before verified boot finishes.
    let plant = |state: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        common::fast_fixture::register_standard_bundle(state, fixture)?;
        let bundle_root = state.join(".ai/bundles/runtime-e2e-default-conflict");
        std::fs::create_dir_all(&bundle_root)?;
        install_kind_schema(&bundle_root, "dup_kind", &fixture.publisher)?;
        install_runtime(
            &bundle_root,
            "dup-runtime-a",
            "dup_kind",
            true,
            "v1",
            &fixture.publisher,
        )?;
        install_runtime(
            &bundle_root,
            "dup-runtime-b",
            "dup_kind",
            true,
            "v1",
            &fixture.publisher,
        )?;
        common::fast_fixture::register_fixture_bundle(
            state,
            "runtime-e2e-default-conflict",
            &bundle_root,
            fixture,
        )
    };

    let startup_error = match DaemonHarness::start_fast_with(plant, |_| {}).await {
        Ok((_h, _fixture)) => panic!("daemon became ready despite multi-default runtime conflict"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        startup_error.contains("node_startup_failed"),
        "multi-default conflict must become a terminal startup failure; got: {startup_error}"
    );
    let mentions_conflict = startup_error.contains("default")
        || startup_error.contains("dup_kind")
        || startup_error.contains("multiple")
        || startup_error.contains("conflict");
    assert!(
        mentions_conflict,
        "startup diagnostics must explain the multi-default conflict; got: {startup_error}"
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
// Uses the real standard bundle's directive runtime (no synth runtime).
// The directive item is a minimal YAML that resolves successfully;
// the dispatch loop follows the kind-schema delegation onto the real
// `runtime:directive-runtime`. The materialization step either succeeds
// (launches the directive runtime binary) or fails with a provider/runtime
// error, but the failure mode must NOT be a 403 — that would prove the
// cap broadened.
//
// Status assertion is permissive (anything except 403) because the
// downstream execution can fail in several legitimate ways (no provider
// configured, runtime error). The KEY assertion: NOT 403.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_via_registry_does_not_require_runtime_execute() {
    let plant = |state: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        common::fast_fixture::register_standard_bundle(state, fixture)
    };

    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with standard bundle");

    // Synth directive item planted in the PROJECT tier — minimal valid
    // YAML so engine resolution succeeds and the dispatch loop reaches
    // the kind-schema delegation → runtime:directive-runtime hop.
    let project = tempfile::tempdir().expect("project tempdir");
    let dir = project.path().join(".ai/directives/e2e_b1");
    std::fs::create_dir_all(&dir).expect("create project directive dir");
    let body = r#"---
name: flow
category: "e2e_b1"
description: "B1 indirect-alias e2e"
inputs: []
---
# E2E B1
"#;
    let signed = lillux::signature::sign_content(body, &fixture.publisher, "<!--", Some("-->"));
    std::fs::write(dir.join("flow.md"), signed).expect("write project directive");

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

// ── 5b. Malformed runtime binary refs fail boot admission ─────────────
//
// The runtime registry validates executor-reference shape while the installed
// bundle admission validates the signed executable set, both before external
// admission opens. A malformed `binary_ref` is therefore a boot error, not a
// dispatch-time 400. The B1 cap-gate ordering remains covered by dispatch unit
// tests with a fully admitted runtime.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_malformed_runtime_binary_ref_is_rejected_at_boot_admission() {
    let plant = |state: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        common::fast_fixture::register_standard_bundle(state, fixture)?;
        let bundle_root = state.join(".ai/bundles/runtime-e2e-malformed-ref");
        std::fs::create_dir_all(&bundle_root)?;
        install_kind_schema(&bundle_root, "p15_kind", &fixture.publisher)?;
        // Keep every other executor-provenance link valid so `badshape` is the
        // fixture's sole admission defect.
        common::fast_fixture::install_signed_bundle_binary(
            &bundle_root,
            "p15-bad-runtime",
            b"#!/bin/sh\nexit 70\n",
            &fixture.publisher,
        )?;
        install_runtime_with_binary_ref(
            &bundle_root,
            "p15-bad-runtime",
            "p15_kind",
            true,
            "v1",
            "badshape",
            &fixture.publisher,
        )?;
        common::fast_fixture::register_fixture_bundle(
            state,
            "runtime-e2e-malformed-ref",
            &bundle_root,
            fixture,
        )
    };

    let startup_error = match DaemonHarness::start_fast_with(plant, |_| {}).await {
        Ok((_h, _fixture)) => panic!("daemon became ready with a malformed runtime binary_ref"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        startup_error.contains("node_startup_failed"),
        "malformed runtime binary_ref must fail terminal boot admission; got: {startup_error}"
    );
    assert!(
        startup_error.contains("failed to build runtime registry"),
        "malformed runtime must be rejected while building the admitted runtime registry; got: {startup_error}"
    );
    assert!(
        startup_error.contains("badshape")
            || startup_error.contains("binary_ref")
            || startup_error.contains("unexpected shape"),
        "startup diagnostics must identify the malformed binary ref; got: {startup_error}"
    );
}

// ── 5c. P1.6 — root/runtime split pin: subject identity wins audit ─────
//
// Pre-V5.4 the indirect dispatch path (`directive:foo` → registry →
// `runtime:directive-runtime`) recorded the thread with the
// **runtime**'s `thread_profile` (`runtime_run`) and the runtime's
// `item_ref`. P1.1 introduced `RootSubject` so the audit captures the
// caller-typed subject's identity, not the executor's.
//
// This test pins that contract end-to-end using the real standard
// bundle's directive runtime. A synth directive item in project space
// dispatches through the real runtime using a bundle-owned, deliberately
// unreachable no-auth provider. Whether that provider call succeeds or fails,
// the thread row must record the SUBJECT's identity.
//
// We open the generation-selected projection directly and assert the thread row
// has the directive's kind/thread_profile/item_ref, not the runtime's.
//
// If the root/runtime split regresses, this test will see
// `kind == "runtime_run"` and `item_ref` starting with `runtime:` —
// failing loudly.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_indirect_directive_audit_records_subject_not_runtime() {
    let plant = |state: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        common::fast_fixture::register_standard_bundle(state, fixture)?;
        common::fast_fixture::register_config_fixture_bundle(
            state,
            "fixture-audit-model-config",
            fixture,
            |bundle_root| {
                let config_root = bundle_root.join(".ai/config/ryeos-runtime");
                let provider_dir = config_root.join("model-providers");
                std::fs::create_dir_all(&provider_dir)?;
                let provider = r#"base_url: "http://127.0.0.1:9"
family: chat_completions
body_template:
  model: "{{model}}"
  messages: "{{messages}}"
  tools: "{{tools}}"
  stream: "{{stream}}"
auth: {}
headers: {}
pricing:
  input_per_million: 0.0
  output_per_million: 0.0
"#;
                std::fs::write(
                    provider_dir.join("audit-noauth.yaml"),
                    lillux::signature::sign_content(provider, &fixture.publisher, "#", None),
                )?;
                let routing = r#"tiers:
  general:
    provider: audit-noauth
    model: audit-model
    context_window: 1024
"#;
                std::fs::write(
                    config_root.join("model_routing.yaml"),
                    lillux::signature::sign_content(routing, &fixture.publisher, "#", None),
                )?;
                Ok(())
            },
        )
    };

    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon");

    // Synth directive item planted in the PROJECT tier — minimal valid
    // YAML so engine resolution succeeds and the dispatch loop reaches
    // the real directive runtime via the registry hop.
    let project = tempfile::tempdir().expect("project tempdir");
    let dir = project.path().join(".ai/directives/p16");
    std::fs::create_dir_all(&dir).expect("create project directive dir");
    let body = r#"---
name: flow
category: "p16"
description: "P1.6 root/runtime split pin"
inputs: []
---
# P1.6
"#;
    let signed = lillux::signature::sign_content(body, &fixture.publisher, "<!--", Some("-->"));
    std::fs::write(dir.join("flow.md"), signed).expect("write project directive");

    let (status, body) = h
        .post_execute(
            "directive:p16/flow",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post /execute");

    // The dispatch may succeed or fail when the runtime contacts the
    // deliberately unreachable no-auth fixture provider. The KEY assertion is
    // the thread row identity below — regardless of dispatch outcome.
    let _ = (status, body);

    // Open the projection DB and find the thread row created for
    // this directive invocation. ProjectionDb writes happen on the
    // daemon side; give it a brief settle window so the row is
    // visible to a fresh read.
    let projection_path =
        common::selected_projection_path(&h.state_path).expect("resolve selected projection");
    for _ in 0..20 {
        if projection_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        projection_path.exists(),
        "selected projection must exist at {}",
        projection_path.display()
    );

    let db =
        ryeos_state::projection::ProjectionDb::open(&projection_path).expect("open projection db");
    let threads = ryeos_state::queries::list_threads(&db, 100).expect("list_threads");

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
// for the graph kind. Uses the real standard bundle's graph runtime.
// Pins that an indirect dispatch chain
//   graph:p4/flow → registry → runtime:graph-runtime
// records the SUBJECT's identity (`graph_run` thread_profile +
// `graph:p4/flow` item_ref), not the runtime's. If the V5.5 P4 B2
// subject/runtime split regresses for graphs specifically, this test
// will see `kind == "runtime_run"` and an item_ref starting with
// `runtime:` — and fail loud.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_indirect_graph_records_graph_thread_profile() {
    let plant = |state: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        common::fast_fixture::register_standard_bundle(state, fixture)
    };

    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon");

    // Synth graph item planted in the PROJECT tier.
    let project = tempfile::tempdir().expect("project tempdir");
    let dir = project.path().join(".ai/graphs/p4");
    std::fs::create_dir_all(&dir).expect("create project graph dir");
    let body = r#"category: "p4"
description: "P4 B2 graph subject/runtime split pin"
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
    // Graph YAMLs use `#` for signature comments (matching
    // `parser:ryeos/core/yaml/yaml`).
    let signed = lillux::signature::sign_content(body, &fixture.publisher, "#", None);
    std::fs::write(dir.join("flow.yaml"), signed).expect("write project graph");

    let (status, body) = h
        .post_execute(
            "graph:p4/flow",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post /execute");

    // The dispatch may succeed or fail (real runtime binary exists but
    // no LLM provider configured). The KEY assertion is the thread
    // row identity below — regardless of dispatch outcome.
    let _ = (status, body);

    let projection_path =
        common::selected_projection_path(&h.state_path).expect("resolve selected projection");
    for _ in 0..20 {
        if projection_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        projection_path.exists(),
        "selected projection must exist at {}",
        projection_path.display()
    );

    let db =
        ryeos_state::projection::ProjectionDb::open(&projection_path).expect("open projection db");
    let threads = ryeos_state::queries::list_threads(&db, 100).expect("list_threads");

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
    let execute_mode =
        workspace.join("crates/daemon/ryeos-api/src/routes/response_modes/execute_mode.rs");
    let dispatch_rs = workspace.join("crates/engine/ryeos-executor/src/dispatch.rs");

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
    for path in [&execute_mode, &dispatch_rs] {
        let content =
            std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read {}", path.display()));
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
            if trimmed.starts_with("///") || trimmed.starts_with("//!") || trimmed.starts_with("//")
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
