//! HTTP end-to-end coverage for `response.mode: handler` routes.
//!
//! A handler route runs its bundle's own tool under a fixed route principal
//! (`route-handler:<bundle>:<route>`). Each request gets a request-owned
//! workspace (`ryeos-route-handler-*`) as the execution's project: the root
//! for resolution, state and callbacks, and the only directory that
//! principal's live-project write authority covers. The tool's process does
//! not run there: a script tool retains its source, so the executor places
//! the process in a daemon-owned admitted-input scratch under the node's
//! execution cache.
//!
//! What that process can reach depends on the node's isolation mode. Under
//! enforced isolation the sandbox confines it to the scratch. Under disabled
//! (direct-host) isolation, which is what the fast fixture's local class
//! runs, no sandbox is applied and the process has the node account's file
//! access: it can write the request workspace, its own installed bundle and
//! any other directory that account can write. The write-reach tests read the
//! node's mode and assert confinement under enforce and that unconfined
//! posture under disabled, so they state what the node actually does.
//!
//! These tests drive a real `ryeosd` over HTTP and check the properties that
//! only hold end to end:
//!
//!   * an anonymous request reaches the tool and its response envelope is
//!     relayed (the dispatch input is complete and the fixed authority is
//!     sufficient);
//!   * an authenticated, higher-privileged caller is visible to the tool as
//!     request data but its scopes and handler context are not the execution
//!     authority;
//!   * the tool's process runs in that scratch and can write it; under
//!     enforced isolation it writes nowhere else (not outside it, not in its
//!     bundle, not in any route workspace it can see), and under disabled
//!     isolation those writes succeed; in both modes the scratch and the
//!     workspace are gone after the request;
//!   * a same-named tool supplied by another bundle that sorts first is
//!     refused plainly (403, terse body) instead of run in the route tool's
//!     place;
//!   * a route whose source names another bundle is refused at startup.

use ryeosd::test_support as common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::DaemonHarness;
use common::fast_fixture::{FAST_FIXTURE_TIME, FastFixture};

const BUNDLE: &str = "handler-fixture";

/// Prints an HTTP response envelope that handler mode turns into a 302.
const CALLBACK_TOOL: &str = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: handler-fixture/oauth
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Callback handler that returns an HTTP redirect envelope"

import json
import sys

print(json.dumps({
    "response": {
        "status": 302,
        "headers": {"Location": "https://example.invalid/?callback=done"},
    }
}))
sys.exit(0)
"#;

/// Same ref as `CALLBACK_TOOL`, shipped by a different bundle. It must never
/// be the item a handler route runs.
const SHADOW_CALLBACK_TOOL: &str = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: handler-fixture/oauth
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Same-named callback from another bundle"

import json
import sys

print(json.dumps({
    "response": {
        "status": 302,
        "headers": {"Location": "https://shadow.invalid/?callback=shadowed"},
    }
}))
sys.exit(0)
"#;

/// Reports where it ran and which writes succeeded. It writes a marker into
/// the project path it was given (its process root), then tries to write into
/// the directory named by the `outside` query parameter, into its own
/// bundle's `.ai` directory and into every request workspace it can see in
/// the directory named by the `workspaces` query parameter (the daemon's
/// temporary directory) whose name starts with `workspace_prefix` (the
/// daemon's own workspaces, so concurrently running tests' daemons are left
/// alone). It reports the workspaces it saw as well as the ones it wrote, so
/// a test can tell an unreachable workspace from none.
const PROBE_TOOL: &str = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: handler-fixture/probe
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Reports its project, caller and write reach"

import glob
import json
import os
import sys
import tempfile


def find_envelope(value):
    if isinstance(value, dict):
        if "request" in value and "principal" in value:
            return value
        for child in value.values():
            found = find_envelope(child)
            if found is not None:
                return found
    return None


def try_write(path):
    try:
        with open(path, "w") as handle:
            handle.write("probe")
        return True
    except OSError:
        return False


project_path = sys.argv[sys.argv.index("--project-path") + 1]
raw = sys.stdin.read()
envelope = find_envelope(json.loads(raw) if raw.strip() else None) or {}
query = (envelope.get("request") or {}).get("query") or {}
principal = envelope.get("principal") or {}

outside = query.get("outside")
workspaces_root = query.get("workspaces") or tempfile.gettempdir()
workspace_prefix = query.get("workspace_prefix") or "ryeos-route-handler-"
seen_workspaces = sorted(
    glob.glob(os.path.join(workspaces_root, glob.escape(workspace_prefix) + "*"))
)
bundle_ai = os.path.normpath(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..")
)

print(json.dumps({
    "response": {
        "status": 200,
        "json": {
            "project_path": os.path.realpath(project_path),
            "outside_received": outside,
            "bundle_ai": os.path.realpath(bundle_ai),
            "principal_id": principal.get("id"),
            "principal_verified": principal.get("verified"),
            "wrote_inside": try_write(os.path.join(project_path, "handler-probe.txt")),
            "wrote_outside": bool(outside) and try_write(os.path.join(outside, "escape.txt")),
            "wrote_bundle": try_write(os.path.join(bundle_ai, "handler-escape.txt")),
            "seen_workspaces": seen_workspaces,
            "wrote_workspaces": [
                workspace
                for workspace in seen_workspaces
                if try_write(os.path.join(workspace, "handler-probe.txt"))
            ],
        },
    }
}))
sys.exit(0)
"#;

const CALLBACK_ROUTE: &str = r#"id: oauth/callback
path: /oauth/callback
methods:
  - GET
auth: none
limits:
  body_bytes_max: 0
  timeout_ms: 30000
  concurrent_max: 4
response:
  mode: handler
  source: tool:handler-fixture/oauth/callback
  source_config:
    request:
      query: true
"#;

const PROBE_ROUTE: &str = r#"id: probe/anonymous
path: /probe
methods:
  - GET
auth: none
limits:
  body_bytes_max: 0
  timeout_ms: 30000
  concurrent_max: 4
response:
  mode: handler
  source: tool:handler-fixture/probe/report
  source_config:
    request:
      query: true
"#;

const SIGNED_PROBE_ROUTE: &str = r#"id: probe/signed
path: /probe/signed
methods:
  - GET
auth: ryeos_signed
limits:
  body_bytes_max: 0
  timeout_ms: 30000
  concurrent_max: 4
response:
  mode: handler
  source: tool:handler-fixture/probe/report
  source_config:
    request:
      query: true
"#;

fn sign(content: &str, fixture: &FastFixture) -> String {
    lillux::signature::sign_content_at(content, &fixture.publisher, "#", None, FAST_FIXTURE_TIME)
}

fn write_signed(path: &Path, content: &str, fixture: &FastFixture) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().expect("fixture file has a parent"))?;
    std::fs::write(path, sign(content, fixture))?;
    Ok(())
}

/// Plant a registered fixture bundle named `bundle_name` under the daemon
/// state root, holding the signed routes and (optionally) the signed tools.
///
/// Handler mode derives the route's bundle id from the bundle root's directory
/// name and requires it to equal the bundle segment of the `tool:` source, so
/// the directory name is the lever the cross-bundle test pulls.
fn plant_handler_bundle(
    state: &Path,
    bundle_name: &str,
    with_tools: bool,
    fixture: &FastFixture,
) -> anyhow::Result<()> {
    let bundle_root = state.join(".ai/bundles").join(bundle_name);
    let routes_dir = bundle_root.join(".ai/node/routes");
    write_signed(
        &routes_dir.join("oauth-callback.yaml"),
        CALLBACK_ROUTE,
        fixture,
    )?;

    if with_tools {
        write_signed(&routes_dir.join("probe.yaml"), PROBE_ROUTE, fixture)?;
        write_signed(
            &routes_dir.join("probe-signed.yaml"),
            SIGNED_PROBE_ROUTE,
            fixture,
        )?;
        // Project-owned executable source must carry a verified signer; the
        // fixture publisher is pinned in the node trust store.
        let tools = bundle_root.join(".ai/tools").join(BUNDLE);
        write_signed(&tools.join("oauth/callback.py"), CALLBACK_TOOL, fixture)?;
        write_signed(&tools.join("probe/report.py"), PROBE_TOOL, fixture)?;
    }

    common::fast_fixture::register_fixture_bundle(state, bundle_name, &bundle_root, fixture)
}

/// Plant a second registered bundle that ships a tool under the handler
/// bundle's namespace. Bundle registrations are read from
/// `.ai/node/bundles/*.yaml` in sorted name order and registered bundle roots
/// are searched in that order after the project, so `a-shadow-fixture` wins
/// `tool:handler-fixture/oauth/callback` whenever the project (the empty
/// request workspace) does not supply it.
fn plant_shadow_bundle(state: &Path, fixture: &FastFixture) -> anyhow::Result<()> {
    let bundle_root = state.join(".ai/bundles/a-shadow-fixture");
    write_signed(
        &bundle_root
            .join(".ai/tools")
            .join(BUNDLE)
            .join("oauth/callback.py"),
        SHADOW_CALLBACK_TOOL,
        fixture,
    )?;
    common::fast_fixture::register_fixture_bundle(state, "a-shadow-fixture", &bundle_root, fixture)
}

/// Plant a registered bundle that sorts ahead of the handler bundle and ships
/// the route's tool ref unsigned, so dispatch's normal-order resolution meets
/// an item it cannot verify.
fn plant_unverifiable_shadow_bundle(state: &Path, fixture: &FastFixture) -> anyhow::Result<()> {
    let bundle_root = state.join(".ai/bundles/a-unverifiable-fixture");
    let tool = bundle_root
        .join(".ai/tools")
        .join(BUNDLE)
        .join("oauth/callback.py");
    std::fs::create_dir_all(tool.parent().expect("fixture file has a parent"))?;
    std::fs::write(&tool, SHADOW_CALLBACK_TOOL)?;
    common::fast_fixture::register_fixture_bundle(
        state,
        "a-unverifiable-fixture",
        &bundle_root,
        fixture,
    )
}

fn non_following_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build non-following client")
}

async fn get_json(
    request: reqwest::RequestBuilder,
) -> (reqwest::StatusCode, serde_json::Value, String) {
    let resp = request.send().await.expect("send handler request");
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, value, text)
}

/// The request workspace is removed when its last owner releases it; give
/// that release a bounded moment after the response.
async fn wait_until_absent(path: &Path) -> bool {
    for _ in 0..100 {
        if !path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !path.exists()
}

/// The probe's process root: a daemon-owned admitted-input scratch directly
/// under the node's execution cache, never a request workspace, the bundle
/// or a directory outside the node.
fn assert_handler_scratch(report: &serde_json::Value, body: &str, state_path: &Path) -> PathBuf {
    let project_path = report
        .get("project_path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("probe must report its project path; body: {body}"));
    let scratch = PathBuf::from(project_path);
    let name = scratch
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    assert!(
        name.starts_with("admitted-input-"),
        "handler tool must run in a daemon-owned admitted-input scratch, not {project_path}"
    );
    let executions = state_path
        .join(".ai/state/cache/executions")
        .canonicalize()
        .expect("canonical node execution cache");
    assert_eq!(
        scratch.parent(),
        Some(executions.as_path()),
        "the scratch must sit directly under the node's execution cache; body: {body}"
    );
    scratch
}

/// Whether the node runs launched tools in the sandbox, read from its signed
/// isolation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolConfinement {
    /// `enforce`: the sandbox grants the tool its scratch and nothing else.
    Enforced,
    /// `disabled`: the tool runs on the host with the node account's file
    /// access.
    DirectHost,
}

fn tool_confinement(state_path: &Path) -> ToolConfinement {
    let mode = common::fast_fixture::node_isolation_mode(state_path)
        .expect("read the node's isolation mode");
    match mode.as_str() {
        "enforce" => ToolConfinement::Enforced,
        "disabled" => ToolConfinement::DirectHost,
        other => panic!("node isolation mode {other:?} is neither enforce nor disabled"),
    }
}

/// The probe query naming where this daemon creates its request workspaces:
/// the daemon's temporary directory (it inherits this process's environment,
/// so both name the same directory) and the name prefix carrying the
/// daemon's pid, so the probe never touches another test daemon's workspace.
fn workspaces_query(h: &DaemonHarness) -> String {
    let root = std::env::temp_dir()
        .canonicalize()
        .expect("canonical temporary directory");
    format!(
        "workspace_prefix=ryeos-route-handler-{}-&workspaces={}",
        h.child.id().expect("the daemon under test is running"),
        root.display()
    )
}

/// The probe wrote into its scratch. Under enforced isolation every other
/// write it tried failed and none of them is on disk; the sandbox may hide
/// the daemon's temporary directory, so seeing no workspace is allowed there.
/// Under direct-host isolation it saw at least one request workspace (its own
/// request's, which exists for the whole execution), so the check is not
/// vacuous, and the bundle and every workspace it saw were writable, because
/// nothing confines a tool's process to its scratch there.
fn assert_probe_write_reach(
    report: &serde_json::Value,
    body: &str,
    mode: ToolConfinement,
    h: &DaemonHarness,
) {
    assert_eq!(
        report.get("wrote_inside"),
        Some(&serde_json::Value::Bool(true)),
        "the handler must be able to write its own scratch; body: {body}"
    );
    let seen = report
        .get("seen_workspaces")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("probe must report the workspaces it saw; body: {body}"));
    match mode {
        ToolConfinement::Enforced => {
            assert_eq!(
                report.get("wrote_bundle"),
                Some(&serde_json::Value::Bool(false)),
                "under enforced isolation the handler must not write into its own bundle; \
                 body: {body}"
            );
            assert_eq!(
                report.get("wrote_workspaces"),
                Some(&serde_json::json!([])),
                "under enforced isolation the handler's process must not write into any \
                 request workspace; body: {body}"
            );
            let bundle_escape = h
                .state_path
                .join(".ai/bundles")
                .join(BUNDLE)
                .join(".ai/handler-escape.txt");
            assert!(
                !bundle_escape.exists(),
                "under enforced isolation the handler wrote into its bundle root"
            );
            // Every workspace of this daemon still present, plus any the probe
            // named, must hold no probe marker.
            let mut workspaces = seen
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let prefix = format!(
                "ryeos-route-handler-{}-",
                h.child.id().expect("the daemon under test is running")
            );
            let root = std::env::temp_dir()
                .canonicalize()
                .expect("canonical temporary directory");
            for entry in std::fs::read_dir(&root).expect("read temporary directory") {
                let entry = entry.expect("temporary directory entry");
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    workspaces.push(entry.path());
                }
            }
            for workspace in workspaces {
                assert!(
                    !workspace.join("handler-probe.txt").exists(),
                    "under enforced isolation the handler wrote into request workspace {}",
                    workspace.display()
                );
            }
        }
        ToolConfinement::DirectHost => {
            assert!(
                !seen.is_empty(),
                "the probe must see at least one request workspace (its own request's); \
                 body: {body}"
            );
            assert_eq!(
                report.get("wrote_bundle"),
                Some(&serde_json::Value::Bool(true)),
                "under direct-host isolation the tool has the node account's file access, \
                 so its installed bundle is writable; body: {body}"
            );
            assert_eq!(
                report
                    .get("wrote_workspaces")
                    .and_then(serde_json::Value::as_array),
                Some(seen),
                "under direct-host isolation every request workspace the tool sees is \
                 writable; body: {body}"
            );
        }
    }
}

/// The probe received `outside_dir`, a directory outside the node, and
/// tried to write into it. Under enforced isolation the write failed and the
/// directory is still empty; under direct-host isolation the write succeeded
/// and its file is on disk.
fn assert_outside_write(
    report: &serde_json::Value,
    body: &str,
    mode: ToolConfinement,
    outside_dir: &Path,
) {
    assert_eq!(
        report
            .get("outside_received")
            .and_then(serde_json::Value::as_str),
        Some(outside_dir.to_str().expect("UTF-8 outside dir")),
        "the probe must receive the outside target it is asked to write; body: {body}"
    );
    let outside_entries = std::fs::read_dir(outside_dir)
        .expect("read outside directory")
        .map(|entry| entry.expect("outside directory entry").file_name())
        .collect::<Vec<_>>();
    match mode {
        ToolConfinement::Enforced => {
            assert_eq!(
                report.get("wrote_outside"),
                Some(&serde_json::Value::Bool(false)),
                "under enforced isolation the handler must not write a directory outside its \
                 scratch; body: {body}"
            );
            assert!(
                outside_entries.is_empty(),
                "the handler left something in the outside directory: {outside_entries:?}"
            );
        }
        ToolConfinement::DirectHost => {
            assert_eq!(
                report.get("wrote_outside"),
                Some(&serde_json::Value::Bool(true)),
                "under direct-host isolation the tool can write any directory the node \
                 account can; body: {body}"
            );
            assert_eq!(
                outside_entries,
                vec![std::ffi::OsString::from("escape.txt")],
                "the direct-host tool's outside write must be on disk"
            );
        }
    }
}

/// An anonymous GET runs the fixed tool under route-handler authority and
/// relays its 302 envelope.
#[tokio::test(flavor = "multi_thread")]
async fn anonymous_get_to_handler_route_returns_tool_redirect() {
    let (h, _fixture) = DaemonHarness::start_fast_with(
        |state, _user, fixture| plant_handler_bundle(state, BUNDLE, true, fixture),
        |_cmd| {},
    )
    .await
    .expect("start daemon with handler fixture bundle");

    let resp = non_following_client()
        .get(format!("http://{}/oauth/callback?code=x", h.bind))
        .send()
        .await
        .expect("GET /oauth/callback");
    let status = resp.status();
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = resp.text().await.unwrap_or_default();

    assert_eq!(
        status,
        reqwest::StatusCode::FOUND,
        "handler route must relay the tool's 302 envelope; got {status} with body: {body}"
    );
    assert_eq!(
        location.as_deref(),
        Some("https://example.invalid/?callback=done"),
        "handler route must relay the tool's Location header; body: {body}"
    );
}

/// The tool's process runs in a daemon-owned admitted-input scratch: it can
/// write there, and the scratch is removed once the request completes. Under
/// enforced isolation it cannot write into a directory outside it, into its
/// own bundle or into any request workspace it can see; under direct-host
/// isolation all of those writes succeed, and the test asserts that they do.
#[tokio::test(flavor = "multi_thread")]
async fn handler_writes_stay_in_a_scratch_removed_after_the_request() {
    let (h, _fixture) = DaemonHarness::start_fast_with(
        |state, _user, fixture| plant_handler_bundle(state, BUNDLE, true, fixture),
        |_cmd| {},
    )
    .await
    .expect("start daemon with handler fixture bundle");

    let mode = tool_confinement(&h.state_path);
    let outside = tempfile::tempdir().expect("outside directory");
    let outside_dir = outside
        .path()
        .canonicalize()
        .expect("canonical outside dir");
    let (status, report, body) = get_json(non_following_client().get(format!(
        "http://{}/probe?outside={}&{}",
        h.bind,
        outside_dir.display(),
        workspaces_query(&h)
    )))
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "probe failed: {body}");

    let scratch = assert_handler_scratch(&report, &body, &h.state_path);
    let installed_bundle_ai = h
        .state_path
        .join(".ai/bundles")
        .join(BUNDLE)
        .join(".ai")
        .canonicalize()
        .expect("canonical installed bundle .ai");
    assert_eq!(
        report.get("bundle_ai").and_then(serde_json::Value::as_str),
        Some(installed_bundle_ai.to_str().expect("UTF-8 bundle path")),
        "the probe must aim its bundle write at the installed bundle; body: {body}"
    );
    assert!(
        !scratch.starts_with(&installed_bundle_ai) && !scratch.starts_with(&outside_dir),
        "the scratch must be neither the bundle nor the outside directory: {}",
        scratch.display()
    );
    assert_probe_write_reach(&report, &body, mode, &h);
    assert_outside_write(&report, &body, mode, &outside_dir);
    let bundle_escape = installed_bundle_ai.join("handler-escape.txt");
    match mode {
        ToolConfinement::Enforced => assert!(
            !bundle_escape.exists(),
            "under enforced isolation the handler wrote into its bundle root"
        ),
        ToolConfinement::DirectHost => assert!(
            bundle_escape.exists(),
            "the direct-host tool's write into its installed bundle must be on disk"
        ),
    }
    // The scratch's guard is released when the execution ends, which removes
    // it with whatever the tool wrote there.
    assert!(
        wait_until_absent(&scratch).await,
        "the handler scratch {} must be removed after the request",
        scratch.display()
    );
}

/// A signed request from the node operator (a principal with far broader
/// scopes than the route) reaches the handler as request data only: the tool
/// sees the verified caller id, yet it runs in a daemon-owned scratch, its
/// execution's project is a route-owned request workspace, and no execution
/// is attributed to the operator.
#[tokio::test(flavor = "multi_thread")]
async fn authenticated_caller_authority_is_not_the_handler_authority() {
    let (mut h, _fixture) = DaemonHarness::start_fast_with(
        |state, _user, fixture| plant_handler_bundle(state, BUNDLE, true, fixture),
        |_cmd| {},
    )
    .await
    .expect("start daemon with handler fixture bundle");

    let mode = tool_confinement(&h.state_path);
    let user_key = h.user_key.as_ref().expect("fast fixture operator key");
    let node_key = h.node_key.as_ref().expect("fast fixture node key");
    let operator_fp = lillux::signature::compute_fingerprint(&user_key.verifying_key());
    // The signature covers the request path and its query parameters in
    // sorted order: `outside` sorts first, and `workspaces_query` already
    // lists its own parameters sorted.
    let outside = tempfile::tempdir().expect("outside directory");
    let outside_dir = outside
        .path()
        .canonicalize()
        .expect("canonical outside dir");
    let target = format!(
        "/probe/signed?outside={}&{}",
        outside_dir.display(),
        workspaces_query(&h)
    );
    let mut request = non_following_client().get(format!("http://{}{target}", h.bind));
    for (name, value) in
        common::build_signed_headers_for_bytes(user_key, node_key, "GET", &target, b"")
    {
        request = request.header(name, value);
    }
    let (status, report, body) = get_json(request).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "signed probe failed: {body}"
    );

    let principal_id = report
        .get("principal_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        principal_id.contains(&operator_fp),
        "the handler must see the authenticated caller as request data; got {principal_id}"
    );
    assert_eq!(
        report.get("principal_verified"),
        Some(&serde_json::Value::Bool(true)),
        "the caller must arrive verified; body: {body}"
    );
    let scratch = assert_handler_scratch(&report, &body, &h.state_path);
    assert_probe_write_reach(&report, &body, mode, &h);
    assert_outside_write(&report, &body, mode, &outside_dir);
    assert!(
        wait_until_absent(&scratch).await,
        "the handler scratch {} must be removed after the request",
        scratch.display()
    );

    let threads = h.list_threads(500).await.expect("list operator threads");
    let attributed: Vec<_> = threads
        .iter()
        .filter(|thread| thread.item_ref.contains("handler-fixture/probe/report"))
        .filter(|thread| {
            thread
                .requested_by
                .as_deref()
                .is_some_and(|requested_by| requested_by.contains(&operator_fp))
        })
        .collect();
    assert!(
        attributed.is_empty(),
        "a handler execution must never be requested by the authenticated caller: {attributed:?}"
    );

    // The caller cannot list threads it does not own, and no route exposes
    // the handler's thread, so read the stopped node's Store directly.
    h.kill_daemon().await.expect("stop daemon");
    let store = common::open_stopped_node_store(&h.state_path).expect("open stopped node store");
    let handler_threads: Vec<_> = store
        .list_threads_filtered(500, None)
        .expect("list every thread")
        .into_iter()
        .filter(|thread| thread.item_ref.contains("handler-fixture/probe/report"))
        .collect();
    let [thread] = handler_threads.as_slice() else {
        panic!("exactly one handler thread must be recorded for the request: {handler_threads:?}");
    };
    assert_eq!(
        thread.requested_by.as_deref(),
        Some("route-handler:handler-fixture:probe/signed"),
        "the handler execution must be requested by the fixed route principal: {thread:?}"
    );
    // The sealed project is the request workspace, a different directory
    // from the scratch the process ran in, and it is gone after the request.
    let workspace = PathBuf::from(
        thread
            .project_root
            .as_deref()
            .unwrap_or_else(|| panic!("the handler thread must record its project: {thread:?}")),
    );
    assert!(
        workspace
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("ryeos-route-handler-")),
        "the handler's sealed project must be its request workspace: {thread:?}"
    );
    assert_ne!(
        workspace, scratch,
        "the process scratch must not be the request workspace"
    );
    assert!(
        wait_until_absent(&workspace).await,
        "the request workspace {} must be removed after the request",
        workspace.display()
    );
}

/// Another bundle ships a tool under the handler bundle's namespace and sorts
/// ahead of it, so dispatch's normal-order resolution selects the shadow
/// rather than the item the route admitted from its own bundle. The route
/// refuses with a terse 403 and the shadow never runs.
#[tokio::test(flavor = "multi_thread")]
async fn shadowing_tool_from_another_bundle_is_refused() {
    let (h, _fixture) = DaemonHarness::start_fast_with(
        |state, _user, fixture| {
            plant_shadow_bundle(state, fixture)?;
            plant_handler_bundle(state, BUNDLE, true, fixture)
        },
        |_cmd| {},
    )
    .await
    .expect("start daemon with handler and shadowing bundles");

    // Control: the shadow is a real contender. It is registered ahead of the
    // handler bundle (registrations load in sorted name order, and registered
    // bundles are searched in that order) and ships the exact same tool path,
    // so ordinary resolution under the empty workspace would select it.
    let registrations = h.state_path.join(".ai/node/bundles");
    let mut registered = std::fs::read_dir(&registrations)
        .expect("read bundle registrations")
        .map(|entry| entry.expect("bundle registration entry").file_name())
        .filter_map(|name| name.to_str()?.strip_suffix(".yaml").map(str::to_owned))
        .collect::<Vec<_>>();
    registered.sort();
    let position = |name: &str| {
        registered
            .iter()
            .position(|registered| registered == name)
            .unwrap_or_else(|| panic!("{name} must be registered; found {registered:?}"))
    };
    assert!(
        position("a-shadow-fixture") < position(BUNDLE),
        "the shadow bundle must be registered ahead of the handler bundle; found {registered:?}"
    );
    let tool = Path::new(".ai/tools")
        .join(BUNDLE)
        .join("oauth/callback.py");
    assert!(
        h.state_path
            .join(".ai/bundles/a-shadow-fixture")
            .join(&tool)
            .is_file()
    );
    assert!(
        h.state_path
            .join(".ai/bundles")
            .join(BUNDLE)
            .join(&tool)
            .is_file()
    );

    let resp = non_following_client()
        .get(format!("http://{}/oauth/callback?code=x", h.bind))
        .send()
        .await
        .expect("GET /oauth/callback");
    let status = resp.status();
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = resp.text().await.unwrap_or_default();

    assert_eq!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "a shadowed handler source must be refused; body: {body}"
    );
    assert_eq!(
        location, None,
        "a refused handler route must not redirect; body: {body}"
    );
    assert!(
        body.contains("handler route refused: the route's tool could not be admitted"),
        "the refusal must carry the fixed terse text; body: {body}"
    );
    assert!(
        !body.contains("shadow.invalid"),
        "the shadow tool must never run; body: {body}"
    );
    assert!(
        !body.contains(".ai/"),
        "the refusal must not disclose filesystem paths; body: {body}"
    );
}

/// A same-named tool that cannot be verified, shipped by a bundle that sorts
/// first, makes dispatch's admission fail. The failure detail names bundle
/// paths and verification causes, so the caller gets only the fixed refusal.
#[tokio::test(flavor = "multi_thread")]
async fn unverifiable_shadow_is_refused_without_detail() {
    let (h, _fixture) = DaemonHarness::start_fast_with(
        |state, _user, fixture| {
            plant_unverifiable_shadow_bundle(state, fixture)?;
            plant_handler_bundle(state, BUNDLE, true, fixture)
        },
        |_cmd| {},
    )
    .await
    .expect("start daemon with handler and unverifiable shadow bundles");

    let resp = non_following_client()
        .get(format!("http://{}/oauth/callback?code=x", h.bind))
        .send()
        .await
        .expect("GET /oauth/callback");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    assert_eq!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "an unverifiable shadow must be refused; body: {body}"
    );
    let value: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("refusal must be JSON: {body}"));
    assert_eq!(
        value,
        serde_json::json!({
            "error": "handler route refused: the route's tool could not be admitted"
        }),
        "the refusal must be exactly the fixed body"
    );
}

/// The route bundle id comes from the bundle root directory name, so a route
/// planted in `handler-route-e2e` that names `tool:handler-fixture/...` is a
/// cross-bundle handler. Route compilation runs at startup, so the daemon
/// must refuse to come up with the exact mismatch text.
#[tokio::test(flavor = "multi_thread")]
async fn cross_bundle_handler_source_is_refused_at_startup() {
    let result = DaemonHarness::start_fast_with(
        |state, _user, fixture| plant_handler_bundle(state, "handler-route-e2e", false, fixture),
        |_cmd| {},
    )
    .await;

    let startup_error = match result {
        Ok((_h, _fixture)) => {
            panic!("daemon must not start with a handler route whose source names another bundle")
        }
        Err(error) => format!("{error:#}"),
    };
    assert!(
        startup_error.contains(
            "handler source bundle 'handler-fixture' must match route bundle 'handler-route-e2e'"
        ),
        "startup must report the exact bundle mismatch; got: {startup_error}"
    );
}
