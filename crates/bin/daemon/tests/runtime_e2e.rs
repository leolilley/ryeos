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
use std::time::{Duration, Instant};

use anyhow::Context as _;
use common::DaemonHarness;
use common::fast_fixture::FastFixture;
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
  financial_authority:
    kind: none
description: "synth runtime for runtime_e2e"
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(runtimes_dir.join(format!("{name}.yaml")), signed)?;
    Ok(())
}

/// Install a runtime that reaches the real same-daemon native-resume handoff.
///
/// The tiny native fixture blocks after held-process attachment/release,
/// allowing the test to make one deliberate fixture-local corruption in the
/// disposable runtime database. It then emits the ordinary typed recovery
/// control envelope directly; no shell, interpreter fallback, or executor
/// fault-injection hook participates in the test.
fn install_rotated_recovery_runtime(
    root: &Path,
    marker: &Path,
    release: &Path,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let marker = marker
        .to_str()
        .filter(|path| {
            !path
                .chars()
                .any(|ch| matches!(ch, '\n' | '\r' | '"' | '\\'))
        })
        .ok_or_else(|| anyhow::anyhow!("test marker path is not safe C string text"))?;
    let release = release
        .to_str()
        .filter(|path| {
            !path
                .chars()
                .any(|ch| matches!(ch, '\n' | '\r' | '"' | '\\'))
        })
        .ok_or_else(|| anyhow::anyhow!("test release path is not safe C string text"))?;
    let process_control_schema = ryeos_runtime::process_outcome::RUNTIME_PROCESS_CONTROL_SCHEMA;
    let recovery_reason = serde_json::to_value(
        ryeos_runtime::process_outcome::RuntimeRecoveryReason::RetainedProgressOutcomeUnknown,
    )?;
    let recovery_reason = recovery_reason
        .as_str()
        .filter(|value| {
            !value
                .chars()
                .any(|ch| matches!(ch, '\n' | '\r' | '"' | '\\'))
        })
        .ok_or_else(|| anyhow::anyhow!("typed recovery reason is not safe C string text"))?;
    anyhow::ensure!(
        !process_control_schema
            .chars()
            .any(|ch| matches!(ch, '\n' | '\r' | '"' | '\\')),
        "typed process-control schema is not safe C string text"
    );
    let source = format!(
        r#"#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static int write_all(int fd, const char *bytes, size_t length) {{
    while (length > 0) {{
        ssize_t written = write(fd, bytes, length);
        if (written < 0 && errno == EINTR) continue;
        if (written <= 0) return -1;
        bytes += written;
        length -= (size_t)written;
    }}
    return 0;
}}

int main(void) {{
    const char *thread_id = getenv("RYEOSD_THREAD_ID");
    if (thread_id == NULL || thread_id[0] == '\0' || strlen(thread_id) > 128) return 70;
    char marker_payload[256];
    int marker_length = snprintf(
        marker_payload,
        sizeof(marker_payload),
        "%s %ld %ld\n",
        thread_id,
        (long)getpid(),
        (long)getpgrp()
    );
    if (marker_length <= 0 || (size_t)marker_length >= sizeof(marker_payload)) return 79;
    int marker = open("{marker}", O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (marker < 0) return 72;
    if (write_all(marker, marker_payload, (size_t)marker_length) != 0 || fsync(marker) != 0 || close(marker) != 0) return 73;

    struct stat release_stat;
    struct timespec delay = {{.tv_sec = 0, .tv_nsec = 25000000L}};
    unsigned int attempt;
    for (attempt = 0; attempt < 400; ++attempt) {{
        if (lstat("{release}", &release_stat) == 0) {{
            if (!S_ISREG(release_stat.st_mode)) return 74;
            break;
        }}
        if (errno != ENOENT) return 75;
        while (nanosleep(&delay, &delay) != 0) {{
            if (errno != EINTR) return 76;
        }}
        delay.tv_sec = 0;
        delay.tv_nsec = 25000000L;
    }}
    if (attempt == 400) return 71;
    if (printf("{{\"process_outcome\":\"recovery_required\",\"schema\":\"{process_control_schema}\",\"thread_id\":\"%s\",\"reason\":\"{recovery_reason}\"}}\n", thread_id) < 0) return 77;
    return fflush(stdout) == 0 ? 0 : 78;
}}
"#
    );
    let build = tempfile::tempdir().context("create native recovery fixture build directory")?;
    let source_path = build.path().join("rotated-recovery-runtime.c");
    let binary_path = build.path().join("rotated-recovery-runtime");
    std::fs::write(&source_path, source).context("write native recovery fixture source")?;
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let output = std::process::Command::new(&compiler)
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(&source_path)
        .arg("-o")
        .arg(&binary_path)
        .output()
        .with_context(|| format!("run native recovery fixture compiler {compiler:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "native recovery fixture compiler failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let binary = std::fs::read(&binary_path).context("read native recovery fixture binary")?;
    anyhow::ensure!(
        binary.starts_with(b"\x7fELF"),
        "native recovery fixture compiler did not produce an ELF binary"
    );
    let binary_ref = common::fast_fixture::install_signed_bundle_binary(
        root,
        "e2e-rotated-recovery-runtime",
        &binary,
        signer,
    )?;
    let runtimes_dir = root.join(".ai/runtimes");
    std::fs::create_dir_all(&runtimes_dir)?;
    let body = format!(
        r#"kind: runtime
serves: e2e_recovery_kind
default: true
binary_ref: {binary_ref}
abi_version: "v3"
required_caps:
  - runtime.execute
native_resume:
  checkpoint_interval_secs: 1
  max_auto_resume_attempts: 1
launch_contract:
  primary_allowed_kinds: [e2e_recovery_kind]
  primary_allowed_spaces: [bundle, project]
  primary_allowed_trust: [trusted_bundle, trusted_project]
  ref_bindings: {{}}
  preparation:
    kind: none
  config_inputs: {{}}
  execution_dependencies:
    max_dependencies: 0
    allowed_kinds: []
    allowed_spaces: []
    allowed_trust: []
  content_dependencies:
    max_dependencies: 0
    allowed_bindings: []
    max_targets_per_dependency: 0
    max_executable_search_entries: 0
    external_content: null
  evidence_attachments:
    max_attachments: 0
    max_total_bytes: 0
    target: null
    destination_prefix: null
    allowed_access: []
  environment_contributions:
    max_contributions: 0
    max_targets_per_contribution: 0
    max_variables_per_contribution: 0
  secret_policy:
    max_requirements: 0
    allowed_names: []
  required_runtime_data: []
  runtime_facts: {{}}
  financial_authority:
    kind: none
  external_effect_authority:
    kind: none
description: "same-daemon rotated recovery fixture"
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(
        runtimes_dir.join("e2e-rotated-recovery-runtime.yaml"),
        signed,
    )?;
    Ok(())
}

#[derive(Debug)]
struct RotatedRecoveryMarker {
    thread_id: String,
    pid: i64,
    pgid: i64,
}

fn parse_rotated_recovery_marker(raw: &str) -> anyhow::Result<RotatedRecoveryMarker> {
    let mut fields = raw.split_whitespace();
    let marker = RotatedRecoveryMarker {
        thread_id: fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("native recovery marker omitted its thread ID"))?
            .to_owned(),
        pid: fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("native recovery marker omitted its PID"))?
            .parse()
            .context("parse native recovery marker PID")?,
        pgid: fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("native recovery marker omitted its PGID"))?
            .parse()
            .context("parse native recovery marker PGID")?,
    };
    anyhow::ensure!(
        fields.next().is_none() && !marker.thread_id.is_empty(),
        "native recovery marker has an invalid field count"
    );
    anyhow::ensure!(
        marker.pid > 0 && marker.pgid > 0,
        "native recovery marker has invalid PID coordinates"
    );
    Ok(marker)
}

/// Read-only proof that the marker-writing process is the exact live process
/// durably attached to this thread and owned by its current launch claim.
/// Missing fields are transient while attachment commits; contradictory fields
/// are hard test failures. This deliberately mirrors the typed runtime-store
/// invariants without adding a production corruption or observation API.
fn prove_rotated_recovery_process_attached(
    state_path: &Path,
    marker: &RotatedRecoveryMarker,
) -> anyhow::Result<Option<usize>> {
    let db_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("state/runtime.sqlite3");
    let connection =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let row = connection.query_row(
        "SELECT runtime.pid, runtime.pgid, runtime.process_identity, runtime.launch_metadata, \
                claim.claim_id, claim.claimed_by, epoch.last_epoch \
           FROM thread_runtime AS runtime \
           LEFT JOIN thread_launch_claim AS claim ON claim.thread_id=runtime.thread_id \
           LEFT JOIN thread_launch_epoch AS epoch ON epoch.thread_id=runtime.thread_id \
          WHERE runtime.thread_id=?1",
        rusqlite::params![&marker.thread_id],
        |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        },
    );
    let (pid, pgid, process_identity, launch_metadata, claim_id, claimed_by, launch_epoch) =
        match row {
            Ok(row) => row,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
    let (
        Some(pid),
        Some(pgid),
        Some(process_identity),
        Some(launch_metadata),
        Some(claim_id),
        Some(claimed_by),
        Some(launch_epoch),
    ) = (
        pid,
        pgid,
        process_identity,
        launch_metadata,
        claim_id,
        claimed_by,
        launch_epoch,
    )
    else {
        return Ok(None);
    };

    anyhow::ensure!(
        pid == marker.pid && pgid == marker.pgid,
        "marker PID/PGID {}/{} contradict durable attachment {pid}/{pgid}",
        marker.pid,
        marker.pgid
    );
    let identity: ryeos_app::process::ExecutionProcessIdentity =
        serde_json::from_str(&process_identity)
            .context("decode attached native recovery process identity")?;
    ryeos_app::process::validate_execution_process_identity_shape(&identity)
        .context("validate attached native recovery process identity")?;
    anyhow::ensure!(
        identity.target_pid == marker.pid && identity.group_leader_pid == marker.pgid,
        "marker coordinates contradict the attached exact process identity"
    );
    anyhow::ensure!(
        ryeos_app::process::execution_alive(&identity),
        "marker-writing native process is not the exact live attached incarnation"
    );

    let owner: ryeos_app::runtime_db::LaunchOwner = serde_json::from_str(&claimed_by)
        .context("decode attached native recovery launch owner")?;
    let canonical_owner = lillux::canonical_json(&serde_json::to_value(&owner)?)?;
    anyhow::ensure!(
        canonical_owner == claimed_by
            && owner.thread_id == marker.thread_id
            && owner.unpredictable_nonce == claim_id
            && owner.monotonic_launch_epoch == u64::try_from(launch_epoch)?
            && owner.monotonic_launch_epoch > 0
            && !owner.daemon_generation_id.is_empty(),
        "durable launch owner contradicts its exact native recovery claim"
    );
    let metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata =
        serde_json::from_str(&launch_metadata)
            .context("decode attached native recovery launch metadata")?;
    metadata
        .validate()
        .context("validate attached native recovery launch metadata")?;
    let sealed = metadata.sealed_root_request.as_ref();
    anyhow::ensure!(
        metadata.native_resume.is_some() && metadata.resume_context.is_some() && sealed.is_some(),
        "attached native recovery process lacks its fully sealed resume metadata"
    );
    let source_bytes = sealed
        .expect("sealed request presence was proved")
        .admitted_program_subject()
        .context("read exact admitted native recovery subject")?
        .source_content
        .into_bytes();
    Ok(Some(source_bytes.len()))
}

/// Plant one conflicting recovery-only source materialization after the exact
/// admitted process is attached but before it requests rotation. Runtime
/// metadata and the authoritative CAS capsule remain valid and unchanged;
/// reconstruction must reject this operational path rather than overwrite it.
fn plant_conflicting_rotated_recovery_source(
    app_root: &Path,
    thread_id: &str,
    admitted_source_bytes: usize,
) -> anyhow::Result<()> {
    let capsule_root = ryeos_app::launch_metadata::daemon_thread_state_dir(app_root, thread_id)
        .join("launch-capsule");
    let root = lillux::PinnedDirectory::open_or_create(&capsule_root)
        .context("open recovery capsule materialization root")?;
    root.set_mode(0o700)?;
    root.ensure_path_binding()?;
    anyhow::ensure!(
        root.atomic_create_regular(
            std::ffi::OsStr::new("subject.source"),
            &vec![b'!'; admitted_source_bytes],
            0o600,
        )?
        .is_some(),
        "recovery capsule source was materialized before the deliberate conflict"
    );
    root.ensure_path_binding()?;
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
  # Managed launch always captures a signed hook plan before minting callback
  # authority. This synthetic kind has no authored hooks, but it still owns a
  # finite event contract so the captured plan is an authenticated empty plan
  # rather than an implicit hook-free exception.
  hooks:
    authored_path: [hooks]
    plan_derived: effective_hook_plan
    events:
      fixture_terminal:
        context_contract:
          schema: ryeos.hooks.context.v1
          allowed_roots: [event, status]
        allowed_results: [discard]
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
            "v2",
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

#[tokio::test(flavor = "multi_thread")]
async fn e2e_rotated_native_resume_reconstruction_failure_retains_launch_failure() {
    let control = tempfile::tempdir().expect("recovery fixture control directory");
    let marker = control.path().join("attached-thread-id");
    let release = control.path().join("release-runtime");
    let planted_marker = marker.clone();
    let planted_release = release.clone();
    let plant = move |state: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        common::fast_fixture::register_standard_bundle(state, fixture)?;
        let bundle_root = state.join(".ai/bundles/runtime-e2e-rotated-recovery");
        std::fs::create_dir_all(&bundle_root)?;
        install_kind_schema(&bundle_root, "e2e_recovery_kind", &fixture.publisher)?;
        let items = bundle_root.join(".ai/e2e_recovery_kind_items");
        std::fs::create_dir_all(&items)?;
        std::fs::write(
            items.join("rotated-recovery.yaml"),
            lillux::signature::sign_content("{}\n", &fixture.publisher, "#", None),
        )?;
        install_rotated_recovery_runtime(
            &bundle_root,
            &planted_marker,
            &planted_release,
            &fixture.publisher,
        )?;
        common::fast_fixture::register_fixture_bundle(
            state,
            "runtime-e2e-rotated-recovery",
            &bundle_root,
            fixture,
        )
    };
    let (mut h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with rotated-recovery runtime");

    let execute = h.post_execute(
        "e2e_recovery_kind:rotated-recovery",
        ".",
        serde_json::json!({}),
    );
    let corrupt = async {
        let deadline = Instant::now() + Duration::from_secs(10);
        let marker = loop {
            if let Ok(raw) = std::fs::read_to_string(&marker)
                && !raw.is_empty()
            {
                break parse_rotated_recovery_marker(&raw)?;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "fixture runtime never published its native process marker"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let admitted_source_bytes = loop {
            if let Some(admitted_source_bytes) =
                prove_rotated_recovery_process_attached(&h.state_path, &marker)?
            {
                break admitted_source_bytes;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "marker-writing process never acquired its exact durable process attachment and launch owner"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        plant_conflicting_rotated_recovery_source(
            &h.state_path,
            &marker.thread_id,
            admitted_source_bytes,
        )?;
        anyhow::ensure!(
            prove_rotated_recovery_process_attached(&h.state_path, &marker)?.is_some(),
            "native recovery attachment changed while planting its reconstruction conflict"
        );
        std::fs::write(&release, b"release")?;
        Ok::<String, anyhow::Error>(marker.thread_id)
    };
    let (response, thread_id) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(execute, corrupt)
    })
    .await
    .expect("rotated recovery request timed out");
    let (status, body) = response.expect("post rotated-recovery runtime");
    let thread_id = match thread_id {
        Ok(thread_id) => thread_id,
        Err(error) => {
            let daemon_stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "plant conflicting recovery source: {error:#}; exact execute response={status} \
                 {body:#}; daemon stderr before fixture teardown:\n{daemon_stderr}"
            );
        }
    };

    let projection_path =
        common::selected_projection_path(&h.state_path).expect("resolve selected projection");
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = loop {
        if let Ok(db) = ryeos_state::projection::ProjectionDb::open(&projection_path)
            && let Ok(Some(result)) = ryeos_state::queries::get_thread_result(&db, &thread_id)
        {
            break result;
        }
        assert!(
            Instant::now() < deadline,
            "rotated failure was not projected; response={status} {body:#}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(result.status, "failed", "response={status} {body:#}");
    assert!(
        result.outcome_code.is_none(),
        "generic managed-launch finalization must retain its typed cause in error"
    );
    let error: serde_json::Value = serde_json::from_str(
        result
            .error
            .as_deref()
            .expect("rotated reconstruction failure retains structured error"),
    )
    .expect("projected failure error is JSON");
    assert_eq!(error["code"].as_str(), Some("launch_failure"));
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("materialization has conflicting content")),
        "wrong reconstruction failure: {error:#}; response={status} {body:#}"
    );

    // This fixture deliberately uses the projectless lane: it proves the real
    // attached process was compare-cleared and that rotation did not invent or
    // retain workspace membership. Physical cleanup of an existing pinned-COW
    // view belongs to its separate workspace lifecycle integration fixtures.
    let runtime_db_path = h
        .state_path
        .join(ryeos_engine::AI_DIR)
        .join("state/runtime.sqlite3");
    let connection = rusqlite::Connection::open_with_flags(
        runtime_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open runtime DB read-only");
    let runtime: (
        i64,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = connection
        .query_row(
            "SELECT resume_attempts, pid, pgid, process_identity, workspace_id, \
                    workspace_view_identity, workspace_borrower_launch_owner \
               FROM thread_runtime WHERE thread_id=?1",
            rusqlite::params![thread_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .expect("read settled runtime row");
    assert_eq!(runtime.0, 1, "recovery did not rotate exactly once");
    assert!(
        runtime.1.is_none() && runtime.2.is_none() && runtime.3.is_none(),
        "settled recovery retained a process attachment"
    );
    assert!(
        runtime.4.is_none() && runtime.5.is_none() && runtime.6.is_none(),
        "projectless recovery retained workspace membership"
    );
    let claims: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM thread_launch_claim WHERE thread_id=?1",
            rusqlite::params![thread_id],
            |row| row.get(0),
        )
        .expect("count retained launch claims");
    assert_eq!(claims, 0, "rotated launch claim leaked after settlement");
    let workspaces: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM execution_workspace WHERE thread_id=?1",
            rusqlite::params![thread_id],
            |row| row.get(0),
        )
        .expect("count projectless workspaces");
    assert_eq!(
        workspaces, 0,
        "workspace-less fixture unexpectedly acquired a workspace"
    );
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
            "v2",
            &fixture.publisher,
        )?;
        install_runtime(
            &bundle_root,
            "dup-runtime-b",
            "dup_kind",
            true,
            "v2",
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
            "v2",
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
  model: "{model}"
  messages: "{messages}"
  tools: "{tools}"
  stream: "{stream}"
auth: {}
headers: {}
schemas:
  streaming:
    mode: delta_merge
  output_limit: {path: max_tokens, semantics: provider_native_output_tokens}
pricing:
  input_per_million: "0.0"
  output_per_million: "0.0"
"#;
                std::fs::write(
                    provider_dir.join("audit-noauth.yaml"),
                    lillux::signature::sign_content(provider, &fixture.publisher, "#", None),
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
    let routing_dir = project.path().join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&routing_dir).expect("create project routing dir");
    let routing = r#"tiers:
  general:
    provider: audit-noauth
    model: audit-model
    context_window: 1024
"#;
    std::fs::write(
        routing_dir.join("model_routing.yaml"),
        lillux::signature::sign_content(routing, &fixture.publisher, "#", None),
    )
    .expect("write project model routing");
    let dir = project.path().join(".ai/directives/p16");
    std::fs::create_dir_all(&dir).expect("create project directive dir");
    let body = r#"---
name: flow
category: "p16"
description: "P1.6 root/runtime split pin"
inputs: []
model:
  tier: general
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

    // The deliberately unreachable provider may make the execution response a
    // success or an error. The persisted subject row below is the authoritative
    // proof that launch preparation reached the managed thread boundary; retain
    // the response so any pre-launch fixture regression is explicit in failure
    // diagnostics rather than discarded.

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
                 — root/runtime split regressed. response_status={status}; \
                 response={body:#}; all rows: {:#?}",
                threads,
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

#[tokio::test(flavor = "multi_thread")]
async fn e2e_graph_validate_is_threadless_and_never_enters_the_runtime() {
    let plant = |state: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        common::fast_fixture::register_standard_bundle(state, fixture)
    };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    let dir = project.path().join(".ai/graphs/validate_only");
    std::fs::create_dir_all(&dir).expect("create project graph dir");
    let body = r#"version: "1.0.0"
category: validate_only
description: "threadless managed validation fixture"
config:
  start: done
  nodes:
    done:
      node_type: return
      output: {ok: true}
"#;
    let signed = lillux::signature::sign_content(body, &fixture.publisher, "#", None);
    std::fs::write(dir.join("threadless.yaml"), signed).expect("write project graph");

    let (status, response) = h
        .post_json(
            "/execute",
            serde_json::json!({
                "item_ref": "graph:validate_only/threadless",
                "ref_bindings": {},
                "project_path": project.path(),
                "parameters": {},
                "validate_only": true,
                "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::local_live(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                ),
            }),
        )
        .await
        .expect("post validation request");
    assert_eq!(status, reqwest::StatusCode::OK, "response={response:#}");
    assert_eq!(response.get("validated"), Some(&serde_json::json!(true)));
    assert_eq!(
        response.get("item_ref"),
        Some(&serde_json::json!("graph:validate_only/threadless"))
    );
    assert!(response.get("thread").is_none(), "response={response:#}");

    let projection_path =
        common::selected_projection_path(&h.state_path).expect("resolve selected projection");
    if projection_path.exists() {
        let db = ryeos_state::projection::ProjectionDb::open(&projection_path)
            .expect("open projection db");
        let threads = ryeos_state::queries::list_threads(&db, 100).expect("list threads");
        assert!(
            threads
                .iter()
                .all(|thread| thread.item_ref != "graph:validate_only/threadless"),
            "validate_only created a durable graph thread: {threads:#?}"
        );
    }
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
