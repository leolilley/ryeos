//! Runtime callbacks driven from a real tool, end to end.
//!
//! A planted tool calls back into the daemon over the UDS callback protocol
//! through the shipped runtime client, so these tests cover what unit tests of
//! the callback services cannot: the token minting from a signed manifest, the
//! UDS routing and the acting principal the daemon records.

use ryeosd::test_support as common;

use std::path::{Path, PathBuf};

use common::DaemonHarness;
use common::callback_probe::{ProbeAuthority, ProbeOutcome, plant_probe_tool, run_callback};
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use serde_json::{Value, json};

const PROBE_BINARY: &str = env!("CARGO_BIN_EXE_ryeos-callback-probe");
const EVENT_KIND: &str = "probe_event";
const CHAIN_ID: &str = "chain-1";

fn project_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("project tempdir");
    let path = dir.path().canonicalize().expect("canonical project path");
    (dir, path)
}

fn plant(project: &Path, fixture: &FastFixture, authority: &ProbeAuthority) {
    plant_probe_tool(
        project,
        &fixture.publisher,
        Path::new(PROBE_BINARY),
        authority,
    )
    .expect("plant callback probe tool");
}

// ── bundle events ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_appends_a_bundle_event_and_reads_it_back_attributed_to_the_caller() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (_guard, project) = project_dir();
    plant(
        &project,
        &fixture,
        &ProbeAuthority::new().bundle_event(EVENT_KIND, &["append", "scan"]),
    );
    let caller = format!("fp:{}", fixture.user_fp());

    let appended = run_callback(
        &h,
        &project,
        "bundle_events_append",
        json!({
            "event_kind": EVENT_KIND,
            "chain_id": CHAIN_ID,
            "event_type": "probe.recorded",
            "payload": {"note": "hello"},
        }),
    )
    .await
    .expect("append callback")
    .accepted();
    assert_eq!(
        appended.pointer("/event/attribution/actor"),
        Some(&json!(caller)),
        "the append must record the caller as actor: {appended:#}"
    );

    // A second launch is a different thread: the event is durable state, not
    // something the first tool still holds.
    let read = run_callback(
        &h,
        &project,
        "bundle_events_read_chain",
        json!({"event_kind": EVENT_KIND, "chain_id": CHAIN_ID}),
    )
    .await
    .expect("read_chain callback")
    .accepted();
    let events = read
        .get("events")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("read_chain must return events: {read:#}"));
    assert_eq!(events.len(), 1, "{read:#}");
    let event = &events[0]["event"];
    assert_eq!(event["event_type"], "probe.recorded", "{read:#}");
    assert_eq!(event["payload"], json!({"note": "hello"}), "{read:#}");
    assert_eq!(
        event.pointer("/attribution/actor"),
        Some(&json!(caller)),
        "the stored event must name the caller as actor: {read:#}"
    );
}

// ── item authoring ──────────────────────────────────────────────────────

const COVERED_CAP: &str = "ryeos.execute.tool.read/file";

fn graph_body(extra: &str) -> String {
    format!(
        "category: authored\nversion: \"1.0.0\"\n{extra}config:\n  start: done\n  nodes:\n    done:\n      node_type: return\n"
    )
}

fn author_params(bare_id: &str, content: String) -> Value {
    json!({
        "item_ref": format!("graph:{bare_id}"),
        "content": content,
        "mode": "create",
        "format_ext": ".yaml",
    })
}

async fn start_authoring_daemon(
    authority: &ProbeAuthority,
) -> (DaemonHarness, FastFixture, tempfile::TempDir, PathBuf) {
    let (h, fixture) = DaemonHarness::start_fast_with(
        |state, _, fixture| register_workflow_bundles(state, fixture),
        |_| {},
    )
    .await
    .expect("start daemon with the workflow bundles");
    let (guard, project) = project_dir();
    plant(&project, &fixture, authority);
    (h, fixture, guard, project)
}

fn authoring_authority() -> ProbeAuthority {
    ProbeAuthority::new()
        .item_authoring("graph", "authored/*")
        .declared_cap("ryeos.execute.tool.read/*")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_covered_author_writes_a_machine_draft_item() {
    let (h, fixture, _guard, project) = start_authoring_daemon(&authoring_authority()).await;
    let body = graph_body(&format!(
        "requires:\n  capabilities:\n    declared:\n      - {COVERED_CAP}\n"
    ));

    let authored = run_callback(
        &h,
        &project,
        "author_item",
        author_params("authored/drafted", body),
    )
    .await
    .expect("author_item callback")
    .accepted();
    assert_eq!(
        authored["item_ref"], "graph:authored/drafted",
        "{authored:#}"
    );

    let path = project.join(".ai/graphs/authored/drafted.yaml");
    let written = std::fs::read_to_string(&path).expect("the authored item must be written");
    assert!(
        written.contains("ryeos:authored:"),
        "the item must carry runtime provenance: {written}"
    );

    // Inspection tells a machine draft from an operator-signed item.
    let (status, effective) = h
        .post_execute(
            "service:items/effective",
            ".",
            json!({
                "canonical_ref": "graph:authored/drafted",
                "project_path": project.to_str().unwrap(),
            }),
        )
        .await
        .expect("items/effective");
    assert!(status.is_success(), "{status}: {effective:#}");
    let root = effective
        .pointer("/result/authorship")
        .and_then(Value::as_array)
        .and_then(|rows| rows.iter().find(|row| row["position"] == "root"))
        .unwrap_or_else(|| panic!("authorship must include the root: {effective:#}"));
    assert_eq!(root["runtime_authored"], true, "{effective:#}");
    // The item is signed by the authoring key, not the node or operator key,
    // and the key's default scope admits no kind: the draft is untrusted.
    assert_eq!(root["signer_role"], "authoring", "{effective:#}");
    assert_eq!(
        authored["signer_fingerprint"],
        fixture.authoring_fp(),
        "{authored:#}"
    );
    assert_eq!(
        effective.pointer("/result/trust_class"),
        Some(&json!("untrusted_project")),
        "{effective:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_author_is_refused_a_capability_it_does_not_hold() {
    let (h, _fixture, _guard, project) = start_authoring_daemon(&authoring_authority()).await;
    let body = graph_body(
        "requires:\n  capabilities:\n    declared:\n      - ryeos.execute.tool.shell/run\n",
    );

    let outcome = run_callback(
        &h,
        &project,
        "author_item",
        author_params("authored/too-wide", body),
    )
    .await
    .expect("author_item callback");

    assert_eq!(outcome.refusal_code(), "authored_capability_not_covered");
    let ProbeOutcome::Refused { message, .. } = &outcome else {
        unreachable!("refusal_code returned for a refusal")
    };
    assert!(
        message.contains("ryeos.execute.tool.shell/run"),
        "the refusal must name the uncovered capability: {message}"
    );
    assert!(
        !project.join(".ai/graphs/authored/too-wide.yaml").exists(),
        "a refused item must not be written"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_alias_extends_parent_is_refused() {
    let (h, _fixture, _guard, project) = start_authoring_daemon(&authoring_authority()).await;
    let body = graph_body("extends: \"@base\"\n");

    let outcome = run_callback(
        &h,
        &project,
        "author_item",
        author_params("authored/aliased", body),
    )
    .await
    .expect("author_item callback");

    let ProbeOutcome::Refused { message, .. } = &outcome else {
        panic!("an `@alias` extends parent must be refused: {outcome:?}");
    };
    assert!(
        message.contains("aliased parent"),
        "the refusal must name the aliased parent: {message}"
    );
    assert!(
        !project.join(".ai/graphs/authored/aliased.yaml").exists(),
        "a refused item must not be written"
    );
}
