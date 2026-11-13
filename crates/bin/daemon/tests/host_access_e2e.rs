//! The host-direct posture, end to end through the daemon.
//!
//! A tool that declares `host_authority: required` runs only when the node's
//! lane offers host access, the operator granted that exact item (signer,
//! source closure and runtime), and the configured operator asks. Every
//! refusal happens before a thread or process exists.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use lillux::crypto::SigningKey;
use serde_json::Value;

const PROBE_REF: &str = "tool:hostdirect/probe";

/// Prints a marker and its PID namespace. Declares host authority and nothing
/// that contradicts it.
const PROBE_TOOL: &str = "category: hostdirect
executor_id: \"@subprocess\"
execution_protocol: \"protocol:ryeos/core/opaque\"
effects: live
host_authority: required
config:
  command: /bin/sh
  args: [\"-c\", \"echo host-direct-probe; readlink /proc/self/ns/pid\"]
";

/// The same tool claiming an isolated network as well, which cannot be true
/// on the host.
const CONTRADICTORY_TOOL: &str = "category: hostdirect
executor_id: \"@subprocess\"
execution_protocol: \"protocol:ryeos/core/opaque\"
host_authority: required
network_authority: isolated
config:
  command: /bin/echo
  args: [\"never-runs\"]
";

fn plant(project: &Path, name: &str, body: &str, signer: &SigningKey) {
    let dir = project.join(".ai/tools/hostdirect");
    std::fs::create_dir_all(&dir).expect("create project tools dir");
    std::fs::write(
        dir.join(format!("{name}.yaml")),
        lillux::signature::sign_content(body, signer, "#", None),
    )
    .expect("write tool");
}

/// Rewrite one node-signed policy member before (re)start, as
/// `ryeos node policy-apply` would.
fn write_policy(state_path: &Path, node: &SigningKey, section: &str, body: &str) {
    std::fs::write(
        state_path.join(format!(".ai/node/policies/{section}.yaml")),
        lillux::signature::sign_content(body, node, "#", None),
    )
    .expect("write node policy");
}

/// The fixture's local lane, offering host-direct execution.
fn grant_host_direct_on_the_lane(state_path: &Path, node: &SigningKey) {
    let path = state_path.join(".ai/node/policies/isolation.yaml");
    let raw = std::fs::read_to_string(&path).expect("read isolation policy");
    let mut body: Value =
        serde_yaml::from_str(&lillux::signature::strip_signature_lines(&raw)).expect("parse");
    body["policy"]["host_classes"]["local"]["host_direct"] = "operator_granted".into();
    write_policy(
        state_path,
        node,
        "isolation",
        &serde_yaml::to_string(&body).expect("serialize isolation policy"),
    );
}

fn grant_body(signer: &str, source_closure: Option<&str>, runtime: (&str, &str)) -> String {
    let grant = serde_json::json!({
        "schema": 1,
        "grants": [{
            "id": "hostdirect-probe",
            "item": PROBE_REF,
            "signer": signer,
            "source_closure": source_closure,
            "runtime": {"ref": runtime.0, "content_hash": runtime.1},
            "origins": ["direct"],
            "host_env": ["PATH"],
            "containment": "group_allowed",
        }],
    });
    serde_yaml::to_string(&grant).expect("serialize grant")
}

async fn execute(h: &DaemonHarness, project: &Path, item: &str) -> (reqwest::StatusCode, Value) {
    h.post_execute(item, project.to_str().unwrap(), serde_json::json!({}))
        .await
        .expect("post execute")
}

fn assert_refused(status: reqwest::StatusCode, body: &Value, code: &str) -> String {
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body={body:#}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some(code),
        "body={body:#}"
    );
    assert!(
        !body.to_string().contains("host-direct-probe\\n"),
        "the refused tool must not have run: {body:#}"
    );
    body.to_string()
}

const SUBPROCESS_RUNTIME: &str = "tool:ryeos/core/subprocess/execute";

/// The exact runtime bytes the node resolves `@subprocess` to: the installed
/// Core item, hashed as the plan builder hashes a chain hop.
fn subprocess_runtime_hash(state_path: &Path) -> String {
    let path = state_path.join(".ai/bundles/core/.ai/tools/ryeos/core/subprocess/execute.yaml");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    lillux::signature::content_hash(&content)
}

async fn start_with_grant(
    runtime_hash: Option<&'static str>,
    grant: bool,
) -> (DaemonHarness, common::fast_fixture::FastFixture) {
    let plant = move |state_path: &Path,
                      _user: &Path,
                      fixture: &common::fast_fixture::FastFixture|
          -> anyhow::Result<()> {
        grant_host_direct_on_the_lane(state_path, &fixture.node);
        if grant {
            let signer = lillux::signature::compute_fingerprint(&fixture.user.verifying_key());
            let hash = match runtime_hash {
                Some(hash) => hash.to_owned(),
                None => subprocess_runtime_hash(state_path),
            };
            write_policy(
                state_path,
                &fixture.node,
                "host_access",
                &grant_body(&signer, None, (SUBPROCESS_RUNTIME, &hash)),
            );
        }
        Ok(())
    };
    DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with a host-direct lane")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lane_that_does_not_offer_host_access_refuses_before_anything_runs() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "probe", PROBE_TOOL, &fixture.user);
    plant(
        project.path(),
        "contradictory",
        CONTRADICTORY_TOOL,
        &fixture.user,
    );

    let (status, body) = execute(&h, project.path(), PROBE_REF).await;
    let message = assert_refused(status, &body, "host_access_lane_unavailable");
    assert!(message.contains("unavailable"), "{message}");
    assert!(
        body.get("thread").is_none(),
        "no thread is minted: {body:#}"
    );

    // Host authority with a projection that cannot be true on the host is
    // refused by the kind, whatever the lane says.
    let (status, body) = execute(&h, project.path(), "tool:hostdirect/contradictory").await;
    assert!(!status.is_success(), "status={status}; body={body:#}");
    let message = body.to_string();
    assert!(
        message.contains("network_authority") && message.contains("cannot be true"),
        "{message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ungranted_host_authority_tool_is_refused_naming_what_to_grant() {
    let (h, fixture) = start_with_grant(None, false).await;
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "probe", PROBE_TOOL, &fixture.user);

    let (status, body) = execute(&h, project.path(), PROBE_REF).await;
    let message = assert_refused(status, &body, "host_access_not_granted");
    assert!(
        message.contains("ryeos node host-access grant"),
        "{message}"
    );
    assert!(
        message.contains(&lillux::signature::compute_fingerprint(
            &fixture.user.verifying_key()
        )),
        "the refusal names the signer to pin: {message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_grant_pinned_to_other_runtime_bytes_is_stale() {
    let zeros: &'static str = "0000000000000000000000000000000000000000000000000000000000000000";
    let (h, fixture) = start_with_grant(Some(zeros), true).await;
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "probe", PROBE_TOOL, &fixture.user);

    let (status, body) = execute(&h, project.path(), PROBE_REF).await;
    let message = assert_refused(status, &body, "host_access_grant_stale");
    assert!(
        message.contains(zeros),
        "the pinned value is named: {message}"
    );
    assert!(
        message.contains(&subprocess_runtime_hash(&h.state_path)),
        "the seen value is named: {message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exact_operator_grant_runs_the_tool_and_records_what_ran() {
    let (h, fixture) = start_with_grant(None, true).await;
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "probe", PROBE_TOOL, &fixture.user);

    let (status, body) = execute(&h, project.path(), PROBE_REF).await;
    assert!(status.is_success(), "status={status}; body={body:#}");
    assert!(
        body.to_string().contains("host-direct-probe"),
        "the granted tool must run: {body:#}"
    );
    let thread_id = body["thread"]["thread_id"]
        .as_str()
        .expect("thread id")
        .to_owned();
    let thread = h
        .get_thread(&thread_id)
        .await
        .expect("get thread")
        .expect("thread exists");
    assert_eq!(
        thread
            .pointer("/project_authority/live_access/confinement/kind")
            .and_then(Value::as_str),
        Some("unconfined_host"),
        "the live project authority records what ran: {thread:#}"
    );
}

// ── An enforcing lane ───────────────────────────────────────────────────

/// Prints the PID namespace it runs in, nothing else.
const ORDINARY_PROBE_TOOL: &str = "category: hostdirect
executor_id: \"@subprocess\"
execution_protocol: \"protocol:ryeos/core/opaque\"
effects: live
config:
  command: /bin/sh
  args: [\"-c\", \"echo ordinary-probe; readlink /proc/self/ns/pid\"]
";

/// A graph whose one action runs the host-authority probe as a child.
const HOST_CHILD_GRAPH: &str = "category: test
version: \"1.0.0\"
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.hostdirect/probe
config:
  start: probe
  nodes:
    probe:
      action:
        item_id: \"tool:hostdirect/probe\"
        ref_bindings: {}
        params: {}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
";

/// The fixture's local lane switched to enforced isolation through the
/// signed Core backend, as the other enforced e2e fixtures do, and offering
/// host-direct execution.
fn enforce_with_host_direct(state_path: &Path, node: &SigningKey) -> anyhow::Result<()> {
    let path = state_path.join(".ai/node/policies/isolation.yaml");
    let mut document: Value = serde_yaml::from_str(&lillux::signature::strip_signature_lines(
        &std::fs::read_to_string(&path)?,
    ))?;
    let document_policy: ryeos_engine::isolation::IsolationPolicyDocument =
        serde_json::from_value(document["policy"].clone())?;
    let mut isolation = document_policy
        .into_selected_policy()
        .map_err(anyhow::Error::msg)?;
    isolation.mode = ryeos_engine::isolation::IsolationMode::Enforce;
    isolation.backend = Some(ryeos_isolation_protocol::IsolationBackendSelection {
        bundle: "core".into(),
        implementation: "linux-lillux".into(),
    });
    isolation.filesystem.proc_filesystem =
        ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespace;
    isolation.host_direct = ryeos_engine::isolation::IsolationHostDirect::OperatorGranted;
    ryeos_engine::isolation::IsolationRuntime::validate_policy(&isolation)?;
    document["policy"] = serde_json::to_value(
        ryeos_engine::isolation::IsolationPolicyDocument::from_selected_policy(isolation),
    )?;
    write_policy(
        state_path,
        node,
        "isolation",
        &serde_yaml::to_string(&document)?,
    );
    Ok(())
}

fn daemon_pid_namespace() -> String {
    // The harness spawns the daemon directly: it shares this PID namespace.
    std::fs::read_link("/proc/self/ns/pid")
        .expect("read this process's PID namespace")
        .to_string_lossy()
        .into_owned()
}

fn confinement_kind(thread: &Value) -> Option<&str> {
    thread
        .pointer("/project_authority/live_access/confinement/kind")
        .and_then(Value::as_str)
}

#[tokio::test(flavor = "multi_thread")]
async fn on_an_enforcing_lane_only_granted_host_tools_leave_the_sandbox() {
    let setup = |state_path: &Path,
                 _user: &Path,
                 fixture: &common::fast_fixture::FastFixture|
     -> anyhow::Result<()> {
        common::fast_fixture::register_workflow_bundles(state_path, fixture)?;
        enforce_with_host_direct(state_path, &fixture.node)?;
        let signer = lillux::signature::compute_fingerprint(&fixture.user.verifying_key());
        let grant = serde_json::json!({
            "schema": 1,
            "grants": [{
                "id": "hostdirect-probe",
                "item": PROBE_REF,
                "signer": signer,
                "source_closure": null,
                "runtime": {
                    "ref": SUBPROCESS_RUNTIME,
                    "content_hash": subprocess_runtime_hash(state_path),
                },
                "origins": ["direct", "graph_child"],
                "host_env": ["PATH"],
                "containment": "group_allowed",
            }],
        });
        write_policy(
            state_path,
            &fixture.node,
            "host_access",
            &serde_yaml::to_string(&grant)?,
        );
        Ok(())
    };
    let (h, fixture) = DaemonHarness::start_fast_with(setup, |_| {})
        .await
        .expect("start an enforcing daemon with a host-direct lane");
    assert_eq!(
        common::fast_fixture::node_isolation_mode(&h.state_path).expect("node isolation mode"),
        "enforce"
    );
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "probe", PROBE_TOOL, &fixture.user);
    plant(
        project.path(),
        "ordinary",
        ORDINARY_PROBE_TOOL,
        &fixture.user,
    );
    let graphs = project.path().join(".ai/graphs");
    std::fs::create_dir_all(&graphs).expect("create graphs dir");
    std::fs::write(
        graphs.join("hostflow.yaml"),
        lillux::signature::sign_content(HOST_CHILD_GRAPH, &fixture.user, "#", None),
    )
    .expect("write graph");
    let host_namespace = daemon_pid_namespace();

    // An ordinary tool on this node stays in the sandbox: its own PID
    // namespace, and a confined live project.
    let (status, body) = execute(&h, project.path(), "tool:hostdirect/ordinary").await;
    assert!(status.is_success(), "status={status}; body={body:#}");
    let text = body.to_string();
    assert!(text.contains("ordinary-probe"), "{text}");
    assert!(text.contains("pid:["), "{text}");
    assert!(
        !text.contains(&host_namespace),
        "an ordinary tool must not share the daemon's PID namespace: {text}"
    );

    // The granted host tool runs on the host, in the daemon's namespace.
    let (status, body) = execute(&h, project.path(), PROBE_REF).await;
    assert!(status.is_success(), "status={status}; body={body:#}");
    let text = body.to_string();
    assert!(text.contains("host-direct-probe"), "{text}");
    assert!(
        text.contains(&host_namespace),
        "the granted host tool runs in the daemon's PID namespace: {text}"
    );
    let direct_thread = body["thread"]["thread_id"].as_str().expect("thread id");
    let direct = h
        .get_thread(direct_thread)
        .await
        .expect("get thread")
        .expect("thread exists");
    assert_eq!(
        confinement_kind(&direct),
        Some("unconfined_host"),
        "{direct:#}"
    );

    // A graph child: the child runs on the host and is sealed unconfined,
    // while its graph parent stays confined.
    let (status, body) = execute(&h, project.path(), "graph:hostflow").await;
    assert!(status.is_success(), "status={status}; body={body:#}");
    assert_eq!(
        body.pointer("/result/success").and_then(Value::as_bool),
        Some(true),
        "{body:#}"
    );
    let graph_thread = body["thread"]["thread_id"]
        .as_str()
        .expect("graph thread id")
        .to_owned();
    let threads = h.list_threads(100).await.expect("list threads");
    let child = threads
        .iter()
        .find(|thread| thread.item_ref == PROBE_REF && thread.chain_root_id == graph_thread)
        .unwrap_or_else(|| panic!("graph child thread not listed: {threads:#?}"));
    let child = h
        .get_thread(&child.thread_id)
        .await
        .expect("get child thread")
        .expect("child thread exists");
    assert_eq!(
        confinement_kind(&child),
        Some("unconfined_host"),
        "{child:#}"
    );
    let parent = h
        .get_thread(&graph_thread)
        .await
        .expect("get graph thread")
        .expect("graph thread exists");
    assert_eq!(
        confinement_kind(&parent),
        Some("descriptor_rooted_fixed_parents"),
        "the graph parent keeps its confinement: {parent:#}"
    );
}
