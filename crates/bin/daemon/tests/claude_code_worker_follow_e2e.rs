//! End-to-end: a graph follows a bounded worker turn and consumes its outcome.
//!
//! The `claude-code` bundle's adapter runs in stub mode: every behaviour comes
//! from the turn goal's `stub` object (`duration_ms`, `outcome`, `text`,
//! `structured_output`), so no model and no network are involved. The test
//! installs the prepared bundle (signed manifests, populated adapter binary) as
//! it ships, plants the two delivered secrets its environment names in a sealed
//! vault, creates a delivered-secret credential profile, and launches a graph
//! whose `follow: true` node runs `worker_execution:claude-code/bounded-turn`.
//!
//! Cases:
//!
//! 1. A completed turn. The followed value is
//!    `{child_thread_id, status, outcome_kind, result, cost}`. The graph assigns
//!    from the worker's structured output, branches on `outcome_kind`, and
//!    subtracts the reported token cost from a budget held in state. The parent
//!    thread carries exactly one `child_thread_settled` event whose
//!    `child_thread_id` equals the one in its `child_thread_spawned`.
//! 2. A stub `crash`. The adapter still ends the turn with a completion
//!    notification, carrying the worker's own status `crashed`. The signed
//!    profile declares `crashed` a failure value, so the turn settles with
//!    `outcome_kind: worker_failure`; the worker node fails, the graph's
//!    `on_error` redirect reads `state._outcome` with the partial result, and
//!    the settled event carries the same kind.
//! 3. A turn that outlives the graph's duration limit. The bounded controller
//!    ends it with `outcome_kind: deadline`; the worker node fails, the graph's
//!    `on_error` redirect reads `state._outcome`, and the settled event carries
//!    the same kind.
//!
//! 4. Continuation. A second turn runs `bounded-turn-continue` with
//!    `continue_from` naming the first turn's child thread. It resumes the
//!    same upstream session id (the stub answers `idle` for an id an earlier
//!    process minted), starts no session, and the successor carries one
//!    `worker_continuation_admitted` event. A second continuation of the same
//!    predecessor, and a continuation of an unknown thread, are refused with
//!    their stable codes. A predecessor whose session the upstream reports
//!    `lost` (stub knob `resume: lost` on session start) settles the
//!    continuation as `worker_failure` with `continuation_conversation_lost`.
//!    The owner, profile, generation and outcome refusals are judged by the
//!    daemon's unit tests: this node has one operator and one profile.
//!
//! Prerequisites beyond the ordinary daemon suite: the Core and `claude-code`
//! bundles must be prepared (published with their binaries), and the host must
//! qualify process scopes for a dedicated worker session. A host that cannot
//! qualify them skips the case rather than failing it.

#![cfg(unix)]
#![recursion_limit = "256"]

use ryeosd::test_support as common;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use lillux::crypto::SigningKey;
use ryeos_app::runtime_db::NewCredentialProfile;
use ryeos_app::state_store::NodeIdentitySigner;
use ryeos_app::test_support::TestStateStore;
use ryeos_node_runtime::identity::NodeIdentity;
use serde_json::{Value, json};

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};

const BUNDLE_NAME: &str = "claude-code";
const TURN_ITEM: &str = "worker_execution:claude-code/bounded-turn";
const CONTINUE_ITEM: &str = "worker_execution:claude-code/bounded-turn-continue";
const ENVIRONMENT_REF: &str = "config:claude-code/environments/trusted-lane";
const CREDENTIAL_PROFILE_ID: &str = "credential:claude-code-follow-e2e";

/// The two host-provided names the bundle's environment delivers.
const KEY_NAME: &str = "ANTHROPIC_API_KEY";
const URL_NAME: &str = "ANTHROPIC_BASE_URL";
const KEY_VALUE: &str = "stub-key-follow-e2e-0123456789abcdef";
const URL_VALUE: &str = "http://127.0.0.1:9";

/// The turn prompt. The stub reports `prompt.len() / 4` input tokens and
/// `text.len() / 4` output tokens, which the cost assertions rely on.
const PROMPT: &str = "Summarise the project in one short line.";
const ANSWER_TEXT: &str = "answer text";

const STARTING_BUDGET: i64 = 1000;

fn principal_of_fingerprint(fingerprint: &str) -> String {
    format!("fp:{fingerprint}")
}

fn open_daemon_state(state_path: &Path) -> Result<TestStateStore> {
    let identity_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    let identity = NodeIdentity::load(&identity_path)
        .with_context(|| format!("load daemon identity {}", identity_path.display()))?;
    let signer = std::sync::Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    TestStateStore::open(state_path, signer, std::sync::Arc::new(head_trust))
}

/// Replace the node-signed `sealed_vault_names` section so that `operator` is
/// the only recipient and writer of both delivered names.
fn write_sealed_vault_names(state_path: &Path, node: &SigningKey, operator: &str) -> Result<()> {
    let list = json!({"principals": [operator], "capabilities": []});
    let entry = |name: &str| json!({"name": name, "recipients": list, "writers": list});
    let body = serde_yaml::to_string(&json!({
        "schema": 1,
        "names": [entry(KEY_NAME), entry(URL_NAME)],
    }))?;
    let signed = lillux::signature::sign_content(&body, node, "#", None);
    std::fs::write(
        state_path.join(".ai/node/policies/sealed_vault_names.yaml"),
        signed,
    )?;
    Ok(())
}

fn plant_sealed_vault_secrets(state_path: &Path, secrets: &HashMap<String, String>) -> Result<()> {
    let vault_dir = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault");
    let sk = lillux::vault::VaultSecretKey::generate();
    lillux::vault::write_secret_key(&vault_dir.join("private_key.pem"), &sk)?;
    let pk = sk.public_key();
    lillux::vault::write_public_key(&vault_dir.join("public_key.pem"), &pk)?;
    let store_path = ryeos_app::vault::default_sealed_store_path(state_path);
    ryeos_app::vault::write_sealed_secrets(&store_path, &pk, secrets)?;
    Ok(())
}

/// Install the prepared `claude-code` bundle exactly as published, the sealed
/// vault holding its two delivered names, and an operator-owned credential
/// profile that activates from them.
fn plant_claude_code(state_path: &Path, fixture: &FastFixture) -> Result<()> {
    common::require_bundles_prepared(&["core", BUNDLE_NAME])
        .context("the claude-code bundle must be prepared (populated and published) first")?;
    register_workflow_bundles(state_path, fixture)?;
    let bundle_root = common::workspace_root().join("bundles").join(BUNDLE_NAME);
    common::fast_fixture::register_presigned_fixture_bundle(
        state_path,
        BUNDLE_NAME,
        &bundle_root,
        fixture,
    )?;
    plant_sealed_vault_secrets(
        state_path,
        &HashMap::from([
            (KEY_NAME.to_owned(), KEY_VALUE.to_owned()),
            (URL_NAME.to_owned(), URL_VALUE.to_owned()),
        ]),
    )?;
    let operator = principal_of_fingerprint(&fixture.user_fp());
    write_sealed_vault_names(state_path, &fixture.node, &operator)?;

    ryeos_app::private_artifact_home::create(
        &state_path.join(ryeos_engine::AI_DIR).join("state"),
        BUNDLE_NAME,
        &BTreeMap::new(),
    )?;
    let store = open_daemon_state(state_path)?;
    store.create_credential_profile(NewCredentialProfile {
        activation: ryeos_app::runtime_db::CredentialActivation::delivered_secret([
            KEY_NAME, URL_NAME,
        ])?,
        profile_id: CREDENTIAL_PROFILE_ID,
        owner_principal: &operator,
        home_id: BUNDLE_NAME,
    })?;
    Ok(())
}

/// The graph's declared authority: the turn item, its environment, and the
/// three dedicated-session operations the worker execution declares.
const GRAPH_REQUIRES: &str = r#"requires:
  capabilities:
    declared:
      - ryeos.execute.worker_execution.claude-code/bounded-turn
      - ryeos.execute.config.claude-code/environments/trusted-lane
      - ryeos.runtime.dedicated_session.start
      - ryeos.runtime.dedicated_session.command
      - ryeos.runtime.dedicated_session.terminate
"#;

/// The follow node's action, with the stub goal substituted as flow-style JSON.
fn follow_action(stub: &Value) -> String {
    format!(
        r#"      node_type: action
      follow: true
      action:
        item_id: "{TURN_ITEM}"
        ref_bindings:
          environment: "{ENVIRONMENT_REF}"
        params:
          credential_profile_id: "{CREDENTIAL_PROFILE_ID}"
          goal:
            session_start_payload: {{}}
            turn_start_payload:
              prompt: "{PROMPT}"
              stub: {stub}
"#
    )
}

/// Success-path graph. It assigns from the followed value, branches on the
/// outcome kind and subtracts the reported token cost from `state.budget`.
/// `state.probe` is returned so the test can read the final state from the
/// resumed thread's persisted result.
fn plant_outcome_graph(project: &Path, signer: &SigningKey, stub: &Value) -> Result<()> {
    let body = format!(
        r#"category: test
version: "1.0.0"
{GRAPH_REQUIRES}config:
  start: turn
  state:
    budget: {STARTING_BUDGET}
  nodes:
    turn:
{action}      assign:
        child_thread_id: "${{result.child_thread_id}}"
        status: "${{result.status}}"
        outcome_kind: "${{result.outcome_kind}}"
        worker_status: "${{result.result.status}}"
        verdict: "${{result.result.result.structured_output.verdict}}"
        text: "${{result.result.result.text}}"
        cost_input_tokens: "${{result.cost.input_tokens}}"
        cost_output_tokens: "${{result.cost.output_tokens}}"
        budget: "${{state.budget - result.cost.input_tokens - result.cost.output_tokens}}"
      on_error: errored
      next:
        type: conditional
        branches:
          - when: 'result.outcome_kind == "completed"'
            to: completed
          - to: other
    completed:
      node_type: return
      output:
        probe:
          route: completed
          child_thread_id: "${{state.child_thread_id}}"
          status: "${{state.status}}"
          outcome_kind: "${{state.outcome_kind}}"
          worker_status: "${{state.worker_status}}"
          verdict: "${{state.verdict}}"
          text: "${{state.text}}"
          cost_input_tokens: "${{state.cost_input_tokens}}"
          cost_output_tokens: "${{state.cost_output_tokens}}"
          budget: "${{state.budget}}"
    other:
      node_type: return
      output:
        probe:
          route: other
    errored:
      node_type: return
      output:
        probe:
          route: errored
"#,
        action = follow_action(stub),
    );
    write_graph(project, signer, "outcome", &body)
}

/// Failure-path graph under an `on_error` redirect. The graph's own duration
/// limit is inherited by the followed child as its aggregate deadline, so a
/// turn that outlives it ends as a `deadline` outcome.
fn plant_deadline_graph(project: &Path, signer: &SigningKey, stub: &Value) -> Result<()> {
    let body = format!(
        r#"category: test
version: "1.0.0"
{GRAPH_REQUIRES}limits:
  duration_seconds: 8
config:
  start: turn
  nodes:
    turn:
{action}      on_error: recover
      next:
        type: unconditional
        to: unreached
    recover:
      node_type: return
      output:
        probe:
          route: recovered
          outcome_kind: "${{state._outcome.outcome_kind}}"
          status: "${{state._outcome.status}}"
          child_thread_id: "${{state._outcome.child_thread_id}}"
          result: "${{state._outcome.result}}"
    unreached:
      node_type: return
      output:
        probe:
          route: unreached
"#,
        action = follow_action(stub),
    );
    write_graph(project, signer, "deadline", &body)
}

/// Failure-path graph for a turn the workload reports as crashed: the node's
/// `on_error` redirect reads the outcome and the partial result.
fn plant_crash_graph(project: &Path, signer: &SigningKey, stub: &Value) -> Result<()> {
    let body = format!(
        r#"category: test
version: "1.0.0"
{GRAPH_REQUIRES}config:
  start: turn
  nodes:
    turn:
{action}      on_error: recover
      next:
        type: unconditional
        to: unreached
    recover:
      node_type: return
      output:
        probe:
          route: recovered
          outcome_kind: "${{state._outcome.outcome_kind}}"
          status: "${{state._outcome.status}}"
          worker_status: "${{state._outcome.result.status}}"
          text: "${{state._outcome.result.result.text}}"
    unreached:
      node_type: return
      output:
        probe:
          route: unreached
"#,
        action = follow_action(stub),
    );
    write_graph(project, signer, "crash", &body)
}

fn write_graph(project: &Path, signer: &SigningKey, name: &str, body: &str) -> Result<()> {
    let graphs = project.join(".ai/graphs");
    std::fs::create_dir_all(&graphs)?;
    std::fs::write(
        graphs.join(format!("{name}.yaml")),
        lillux::signature::sign_content(body, signer, "#", None),
    )?;
    Ok(())
}

/// The first value anywhere under `value` stored at key `name`.
fn find_key<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(name)
            .or_else(|| map.values().find_map(|inner| find_key(inner, name))),
        Value::Array(items) => items.iter().find_map(|inner| find_key(inner, name)),
        _ => None,
    }
}

/// What a finished run shows.
struct Finished {
    parent_thread_id: String,
    /// `(thread_id, event_type, payload)` of every chain the principal sees.
    events: Vec<(String, String, Value)>,
    /// The resumed graph thread's returned `probe` object.
    probe: Value,
}

enum Run {
    Finished(Finished),
    /// The host cannot qualify process scopes, so no worker session can start.
    HostUnqualified,
}

/// Start the graph pinned, wait for its resumed successor to complete, and
/// read the successor's returned `probe`.
async fn run_graph(
    daemon: &mut DaemonHarness,
    project: &Path,
    graph: &str,
    timeout: Duration,
) -> Result<Run> {
    // An earlier run on the same node left its own completed successor
    // behind; this run's successor is the one that was not there before.
    let already_completed: std::collections::BTreeSet<String> = daemon
        .principal_events()
        .await
        .into_iter()
        .filter(|(_, event, _)| event == "graph_completed")
        .map(|(thread, _, _)| thread)
        .collect();
    let (status, body) = daemon
        .post_json(
            "/execute",
            json!({
                "item_ref": format!("graph:{graph}"),
                "ref_bindings": {},
                "project_path": project,
                "parameters": {},
                "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::local_pinned_capture(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                ),
            }),
        )
        .await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "graph launch returned {status}: {body:#}"
    );
    anyhow::ensure!(
        body.pointer("/result/status").and_then(Value::as_str) == Some("continued"),
        "the graph must suspend at its follow node: {body:#}"
    );
    let parent_thread_id = body
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .context("graph launch returned no parent thread id")?
        .to_owned();

    let deadline = Instant::now() + timeout;
    let (events, successor) = loop {
        let events = daemon.principal_events().await;
        let successor = events
            .iter()
            .find(|(thread, event, _)| {
                event == "graph_completed"
                    && *thread != parent_thread_id
                    && !already_completed.contains(thread)
            })
            .map(|(thread, _, _)| thread.clone());
        if let Some(successor) = successor {
            break (events, successor);
        }
        if Instant::now() >= deadline {
            let stderr = daemon.drain_stderr_nonblocking().await;
            if stderr.contains("qualified node process-scope support") {
                eprintln!("claude-code follow suite skipped: host cannot qualify process scopes");
                return Ok(Run::HostUnqualified);
            }
            anyhow::bail!(
                "the resumed graph never completed.\nevents={events:#?}\n--- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let (status, thread) = daemon
        .post_execute(
            "service:threads/get",
            project.to_str().context("project path is not UTF-8")?,
            json!({ "thread_id": successor }),
        )
        .await?;
    anyhow::ensure!(
        status.is_success(),
        "threads/get failed: {status} {thread:#}"
    );
    let probe = find_key(&thread, "probe")
        .cloned()
        .with_context(|| format!("the resumed thread returned no `probe`: {thread:#}"))?;
    Ok(Run::Finished(Finished {
        parent_thread_id,
        events,
        probe,
    }))
}

/// The parent thread's `child_thread_spawned` and `child_thread_settled`
/// events, each required exactly once, returned as their payloads.
fn parent_lineage_events(finished: &Finished) -> Result<(Value, Value)> {
    let on_parent = |event_type: &str| {
        finished
            .events
            .iter()
            .filter(|(thread, event, _)| {
                *thread == finished.parent_thread_id && event == event_type
            })
            .map(|(_, _, payload)| payload.clone())
            .collect::<Vec<_>>()
    };
    let spawned = on_parent("child_thread_spawned");
    let settled = on_parent("child_thread_settled");
    anyhow::ensure!(
        spawned.len() == 1,
        "the parent must have exactly one child_thread_spawned: {spawned:#?}"
    );
    anyhow::ensure!(
        settled.len() == 1,
        "the parent must have exactly one child_thread_settled: {settled:#?}"
    );
    Ok((spawned[0].clone(), settled[0].clone()))
}

async fn start_node() -> Result<(DaemonHarness, FastFixture)> {
    DaemonHarness::start_fast_with(
        |state_path: &Path, _user: &Path, fixture: &FastFixture| {
            plant_claude_code(state_path, fixture)
        },
        |_| {},
    )
    .await
}

/// The token counts the stub reports for `PROMPT` and `text`.
fn stub_tokens(text: &str) -> (i64, i64) {
    (
        i64::try_from(PROMPT.len() / 4).expect("prompt length fits"),
        i64::try_from(text.len() / 4).expect("text length fits"),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_turn_flows_result_outcome_and_cost_into_the_graph() -> Result<()> {
    let (mut daemon, fixture) = start_node().await?;
    let project = tempfile::tempdir()?;
    let stub = json!({
        "outcome": "completed",
        "text": ANSWER_TEXT,
        "structured_output": {"verdict": "ok"},
    });
    plant_outcome_graph(project.path(), &fixture.publisher, &stub)?;

    let Run::Finished(finished) = run_graph(
        &mut daemon,
        project.path(),
        "outcome",
        Duration::from_secs(120),
    )
    .await?
    else {
        return Ok(());
    };

    // The graph took the `completed` branch and assigned from the worker's
    // own result: the structured output and the text sit two `result` levels
    // down (follow record, then the turn-completion payload, then the turn's
    // result object).
    let probe = &finished.probe;
    assert_eq!(probe["route"], json!("completed"), "{probe:#}");
    assert_eq!(probe["status"], json!("completed"), "{probe:#}");
    assert_eq!(probe["outcome_kind"], json!("completed"), "{probe:#}");
    assert_eq!(probe["worker_status"], json!("completed"), "{probe:#}");
    assert_eq!(probe["verdict"], json!("ok"), "{probe:#}");
    assert_eq!(probe["text"], json!(ANSWER_TEXT), "{probe:#}");

    // The reported cost is the stub's usage, mapped through the pointers the
    // profile declares, and the graph subtracted it from the budget.
    let (input, output) = stub_tokens(ANSWER_TEXT);
    assert_eq!(probe["cost_input_tokens"], json!(input), "{probe:#}");
    assert_eq!(probe["cost_output_tokens"], json!(output), "{probe:#}");
    assert_eq!(
        probe["budget"],
        json!(STARTING_BUDGET - input - output),
        "{probe:#}"
    );

    // The parent's lineage: one spawned, one settled, same child.
    let (spawned, settled) = parent_lineage_events(&finished)?;
    let spawned_child = spawned["child_thread_id"]
        .as_str()
        .context("child_thread_spawned names no child")?;
    assert_eq!(
        settled["child_thread_id"],
        json!(spawned_child),
        "settled must name the spawned child: {settled:#}"
    );
    assert_eq!(
        probe["child_thread_id"],
        json!(spawned_child),
        "the followed value must name the spawned child: {probe:#}"
    );
    assert_eq!(settled["status"], json!("completed"), "{settled:#}");
    assert_eq!(settled["outcome_kind"], json!("completed"), "{settled:#}");
    assert_eq!(settled["result_summary"], json!(ANSWER_TEXT), "{settled:#}");
    assert_eq!(settled["truncated"], json!(false), "{settled:#}");
    assert_eq!(settled["cost"]["input_tokens"], json!(input), "{settled:#}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stub_crash_fails_the_node_as_a_worker_failure() -> Result<()> {
    let (mut daemon, fixture) = start_node().await?;
    let project = tempfile::tempdir()?;
    // No text: the stub then reports its default crash sentence.
    let stub = json!({"outcome": "crash", "structured_output": null});
    plant_crash_graph(project.path(), &fixture.publisher, &stub)?;

    let Run::Finished(finished) = run_graph(
        &mut daemon,
        project.path(),
        "crash",
        Duration::from_secs(120),
    )
    .await?
    else {
        return Ok(());
    };

    // The profile declares the worker's `crashed` status a failure value, so
    // the worker node fails and the `on_error` redirect reads the outcome with
    // the partial result still attached.
    let probe = &finished.probe;
    assert_eq!(probe["route"], json!("recovered"), "{probe:#}");
    assert_eq!(probe["outcome_kind"], json!("worker_failure"), "{probe:#}");
    assert_eq!(probe["status"], json!("failed"), "{probe:#}");
    assert_eq!(probe["worker_status"], json!("crashed"), "{probe:#}");
    assert_eq!(
        probe["text"],
        json!("stub: claude code crashed"),
        "{probe:#}"
    );

    let (spawned, settled) = parent_lineage_events(&finished)?;
    assert_eq!(
        settled["child_thread_id"], spawned["child_thread_id"],
        "settled must name the spawned child"
    );
    assert_eq!(
        settled["outcome_kind"],
        json!("worker_failure"),
        "{settled:#}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_that_outlives_the_duration_limit_fails_the_node_as_a_deadline() -> Result<()> {
    let (mut daemon, fixture) = start_node().await?;
    let project = tempfile::tempdir()?;
    // The graph's 8 second limit is inherited by the child; the stub turn
    // would run for two minutes.
    let stub = json!({
        "outcome": "completed",
        "duration_ms": 120_000,
        "progress_events": 0,
        "text": "never reported",
    });
    plant_deadline_graph(project.path(), &fixture.publisher, &stub)?;

    let Run::Finished(finished) = run_graph(
        &mut daemon,
        project.path(),
        "deadline",
        Duration::from_secs(120),
    )
    .await?
    else {
        return Ok(());
    };

    let probe = &finished.probe;
    assert_eq!(probe["route"], json!("recovered"), "{probe:#}");
    assert_eq!(probe["outcome_kind"], json!("deadline"), "{probe:#}");
    // No turn completed, so there is no worker result.
    assert!(
        probe["result"].is_null() || probe["result"] == json!(""),
        "a deadline carries no worker result: {probe:#}"
    );

    let (spawned, settled) = parent_lineage_events(&finished)?;
    assert_eq!(
        settled["child_thread_id"], spawned["child_thread_id"],
        "settled must name the spawned child"
    );
    assert_eq!(settled["outcome_kind"], json!("deadline"), "{settled:#}");
    assert_eq!(
        probe["child_thread_id"], spawned["child_thread_id"],
        "{probe:#}"
    );
    Ok(())
}

/// A one-node graph that follows `item` once and returns a `probe`: the
/// followed value on success, the node's structured outcome on failure.
/// `session_start` is the sealed session-start payload (absent for a
/// continuation) and `continue_from` the predecessor a continuation names.
#[allow(clippy::too_many_arguments)]
fn plant_turn_graph(
    project: &Path,
    signer: &SigningKey,
    name: &str,
    item: &str,
    session_start: Option<&Value>,
    continue_from: Option<&str>,
    stub: &Value,
) -> Result<()> {
    let capability = item
        .strip_prefix("worker_execution:")
        .context("turn item is not a worker execution")?;
    let continue_line = continue_from
        .map(|id| format!("          continue_from: \"{id}\"\n"))
        .unwrap_or_default();
    let session_start_line = session_start
        .map(|payload| format!("            session_start_payload: {payload}\n"))
        .unwrap_or_default();
    let body = format!(
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.worker_execution.{capability}
      - ryeos.execute.config.claude-code/environments/trusted-lane
      - ryeos.runtime.dedicated_session.start
      - ryeos.runtime.dedicated_session.command
      - ryeos.runtime.dedicated_session.terminate
config:
  start: turn
  nodes:
    turn:
      node_type: action
      follow: true
      action:
        item_id: "{item}"
        ref_bindings:
          environment: "{ENVIRONMENT_REF}"
        params:
          credential_profile_id: "{CREDENTIAL_PROFILE_ID}"
{continue_line}          goal:
{session_start_line}            turn_start_payload:
              prompt: "{PROMPT}"
              stub: {stub}
      assign:
        child_thread_id: "${{result.child_thread_id}}"
        status: "${{result.status}}"
        outcome_kind: "${{result.outcome_kind}}"
        text: "${{result.result.result.text}}"
      on_error: recover
      next:
        type: unconditional
        to: done
    done:
      node_type: return
      output:
        probe:
          route: completed
          child_thread_id: "${{state.child_thread_id}}"
          status: "${{state.status}}"
          outcome_kind: "${{state.outcome_kind}}"
          text: "${{state.text}}"
    recover:
      node_type: return
      output:
        probe:
          route: recovered
          child_thread_id: "${{state._outcome.child_thread_id}}"
          status: "${{state._outcome.status}}"
          outcome_kind: "${{state._outcome.outcome_kind}}"
          outcome_dump: "${{state._outcome}}"
"#
    );
    write_graph(project, signer, name, &body)
}

/// Run one `plant_turn_graph` graph to its end. `None` means the host cannot
/// qualify process scopes and the case is skipped.
async fn run_turn(
    daemon: &mut DaemonHarness,
    project: &Path,
    name: &str,
) -> Result<Option<Finished>> {
    match run_graph(daemon, project, name, Duration::from_secs(120)).await? {
        Run::Finished(finished) => Ok(Some(finished)),
        Run::HostUnqualified => Ok(None),
    }
}

/// Every upstream session id the thread's own events carry.
fn upstream_session_ids(events: &[(String, String, Value)], thread_id: &str) -> Vec<String> {
    let mut ids: Vec<String> = events
        .iter()
        .filter(|(thread, _, _)| thread == thread_id)
        .filter_map(|(_, _, payload)| find_key(payload, "session_id"))
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The thread's `worker_continuation_admitted` events, as payloads.
fn continuation_admissions(events: &[(String, String, Value)], thread_id: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|(thread, event, _)| thread == thread_id && event == "worker_continuation_admitted")
        .map(|(_, _, payload)| payload.clone())
        .collect()
}

/// Whether `needle` appears in the failed turn's outcome or in its thread.
async fn failure_names(
    daemon: &mut DaemonHarness,
    project: &Path,
    probe: &Value,
    needle: &str,
) -> Result<bool> {
    if probe.to_string().contains(needle) {
        return Ok(true);
    }
    let child = probe["child_thread_id"]
        .as_str()
        .context("the failed turn names no child thread")?;
    let (status, thread) = daemon
        .post_execute(
            "service:threads/get",
            project.to_str().context("project path is not UTF-8")?,
            json!({ "thread_id": child }),
        )
        .await?;
    anyhow::ensure!(
        status.is_success(),
        "threads/get failed: {status} {thread:#}"
    );
    Ok(thread.to_string().contains(needle))
}

fn completed_stub() -> Value {
    json!({"outcome": "completed", "text": ANSWER_TEXT})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_turn_continues_the_first_turns_upstream_conversation() -> Result<()> {
    let (mut daemon, fixture) = start_node().await?;
    let project = tempfile::tempdir()?;
    let publisher = &fixture.publisher;
    plant_turn_graph(
        project.path(),
        publisher,
        "first",
        TURN_ITEM,
        Some(&json!({})),
        None,
        &completed_stub(),
    )?;
    let Some(first) = run_turn(&mut daemon, project.path(), "first").await? else {
        return Ok(());
    };
    assert_eq!(
        first.probe["outcome_kind"],
        json!("completed"),
        "{:#}",
        first.probe
    );
    let first_child = first.probe["child_thread_id"]
        .as_str()
        .context("the first turn named no child")?
        .to_owned();
    let first_sessions = upstream_session_ids(&first.events, &first_child);
    assert_eq!(
        first_sessions.len(),
        1,
        "the first turn runs in exactly one upstream session: {first_sessions:?}"
    );
    assert!(
        continuation_admissions(&first.events, &first_child).is_empty(),
        "a fresh turn is not a continuation"
    );

    plant_turn_graph(
        project.path(),
        publisher,
        "second",
        CONTINUE_ITEM,
        None,
        Some(&first_child),
        &json!({"outcome": "completed", "text": "second answer"}),
    )?;
    let Some(second) = run_turn(&mut daemon, project.path(), "second").await? else {
        return Ok(());
    };
    let probe = &second.probe;
    assert_eq!(probe["route"], json!("completed"), "{probe:#}");
    assert_eq!(probe["outcome_kind"], json!("completed"), "{probe:#}");
    assert_eq!(probe["text"], json!("second answer"), "{probe:#}");
    let second_child = probe["child_thread_id"]
        .as_str()
        .context("the second turn named no child")?;
    assert_ne!(
        second_child, first_child,
        "a continuation is its own thread"
    );

    // The same upstream session carried both turns, and the successor says so
    // durably, once, naming its predecessor.
    assert_eq!(
        upstream_session_ids(&second.events, second_child),
        first_sessions,
        "the continuation must resume the predecessor's upstream session"
    );
    let admitted = continuation_admissions(&second.events, second_child);
    assert_eq!(admitted.len(), 1, "{admitted:#?}");
    assert_eq!(admitted[0]["predecessor_thread_id"], json!(first_child));
    assert_eq!(admitted[0]["upstream_session_id"], json!(first_sessions[0]));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_predecessor_is_continued_at_most_once_and_must_exist() -> Result<()> {
    let (mut daemon, fixture) = start_node().await?;
    let project = tempfile::tempdir()?;
    let publisher = &fixture.publisher;
    plant_turn_graph(
        project.path(),
        publisher,
        "first",
        TURN_ITEM,
        Some(&json!({})),
        None,
        &completed_stub(),
    )?;
    let Some(first) = run_turn(&mut daemon, project.path(), "first").await? else {
        return Ok(());
    };
    let first_child = first.probe["child_thread_id"]
        .as_str()
        .context("the first turn named no child")?
        .to_owned();

    plant_turn_graph(
        project.path(),
        publisher,
        "second",
        CONTINUE_ITEM,
        None,
        Some(&first_child),
        &completed_stub(),
    )?;
    let Some(second) = run_turn(&mut daemon, project.path(), "second").await? else {
        return Ok(());
    };
    assert_eq!(
        second.probe["route"],
        json!("completed"),
        "{:#}",
        second.probe
    );

    // A second continuation of the same predecessor is refused before any
    // worker exists.
    plant_turn_graph(
        project.path(),
        publisher,
        "again",
        CONTINUE_ITEM,
        None,
        Some(&first_child),
        &completed_stub(),
    )?;
    let Some(again) = run_turn(&mut daemon, project.path(), "again").await? else {
        return Ok(());
    };
    assert_eq!(
        again.probe["route"],
        json!("recovered"),
        "{:#}",
        again.probe
    );
    assert_ne!(again.probe["outcome_kind"], json!("completed"));
    assert!(
        failure_names(
            &mut daemon,
            project.path(),
            &again.probe,
            "continuation_predecessor_already_continued"
        )
        .await?,
        "the refusal must name its code: {:#}",
        again.probe
    );

    // A predecessor that never existed is refused too.
    plant_turn_graph(
        project.path(),
        publisher,
        "unknown",
        CONTINUE_ITEM,
        None,
        Some("T-no-such-predecessor"),
        &completed_stub(),
    )?;
    let Some(unknown) = run_turn(&mut daemon, project.path(), "unknown").await? else {
        return Ok(());
    };
    assert_eq!(
        unknown.probe["route"],
        json!("recovered"),
        "{:#}",
        unknown.probe
    );
    assert!(
        failure_names(
            &mut daemon,
            project.path(),
            &unknown.probe,
            "continuation_predecessor_unknown"
        )
        .await?,
        "the refusal must name its code: {:#}",
        unknown.probe
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_conversation_settles_the_continuation_as_a_worker_failure() -> Result<()> {
    let (mut daemon, fixture) = start_node().await?;
    let project = tempfile::tempdir()?;
    let publisher = &fixture.publisher;
    // The first turn completes normally, but its upstream session can never
    // be resumed by a later process.
    plant_turn_graph(
        project.path(),
        publisher,
        "first",
        TURN_ITEM,
        Some(&json!({"stub": {"resume": "lost"}})),
        None,
        &completed_stub(),
    )?;
    let Some(first) = run_turn(&mut daemon, project.path(), "first").await? else {
        return Ok(());
    };
    assert_eq!(
        first.probe["outcome_kind"],
        json!("completed"),
        "{:#}",
        first.probe
    );
    let first_child = first.probe["child_thread_id"]
        .as_str()
        .context("the first turn named no child")?
        .to_owned();

    plant_turn_graph(
        project.path(),
        publisher,
        "second",
        CONTINUE_ITEM,
        None,
        Some(&first_child),
        &completed_stub(),
    )?;
    let Some(second) = run_turn(&mut daemon, project.path(), "second").await? else {
        return Ok(());
    };
    let probe = &second.probe;
    assert_eq!(probe["route"], json!("recovered"), "{probe:#}");
    assert_eq!(probe["outcome_kind"], json!("worker_failure"), "{probe:#}");
    // No turn ran, and no fresh session stood in for the lost one.
    let second_child = probe["child_thread_id"]
        .as_str()
        .context("the failed continuation named no child")?;
    assert_eq!(
        upstream_session_ids(&second.events, second_child)
            .into_iter()
            .filter(|id| !upstream_session_ids(&first.events, &first_child).contains(id))
            .count(),
        0,
        "the continuation must not have started another session"
    );
    assert!(
        failure_names(
            &mut daemon,
            project.path(),
            probe,
            "continuation_conversation_lost"
        )
        .await?,
        "the failure must name its reason: {probe:#}"
    );
    Ok(())
}
