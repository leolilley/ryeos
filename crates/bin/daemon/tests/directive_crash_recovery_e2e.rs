//! OS-level recovery acceptance for the directive model/tool event braid.
//!
//! Each test parks the first directive process immediately after one durable
//! boundary, kills the daemon, and lets startup reconciliation native-resume
//! the same thread. The only recovery authorities under test are the existing
//! provider record, directive events, runtime-action intent, and child thread.

#![cfg(all(unix, feature = "crash-qualification-test-support"))]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::fast_fixture::{FastFixture, register_config_fixture_bundle, register_standard_bundle};
use common::mock_provider::{MockProvider, MockResponse, MockToolCallSpec};
use common::runtime_phase_cut::RuntimePhaseCutGate;
use common::{DaemonHarness, build_signed_headers_for_bytes};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

const DIRECTIVE_REF: &str = "directive:test/recovery";
const TOOL_NAME: &str = "marker_marker";
const DIRECTIVE_RETURN_NAME: &str = "directive_return";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashCut {
    ProviderEffectSettled,
    ProviderRetrySettled,
    ToolProposalRecorded,
    ChildBorn,
    ToolSettled,
    ToolObservationRecorded,
    FinalCognitionRecorded,
    DirectiveReturnRecorded,
    ToolLimitRefusalStartRecorded,
}

impl CrashCut {
    const fn runtime_block_stage(self) -> Option<&'static str> {
        match self {
            Self::ProviderEffectSettled => Some("provider_effect_settled"),
            Self::ProviderRetrySettled => Some("provider_retry_settled"),
            Self::ToolProposalRecorded => Some("tool_proposal_recorded"),
            Self::ChildBorn => None,
            Self::ToolSettled => Some("tool_settled"),
            Self::ToolObservationRecorded => Some("tool_observation_recorded"),
            Self::FinalCognitionRecorded => Some("final_cognition_recorded"),
            Self::DirectiveReturnRecorded => Some("directive_return_recorded"),
            Self::ToolLimitRefusalStartRecorded => Some("tool_limit_refusal_start_recorded"),
        }
    }
}

fn plant_recorded_mock_provider(
    root: &Path,
    mock_base_url: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"family: chat_completions
transport:
  kind: remote_http
  base_url: "{mock_base_url}"
body_template:
  model: "{{model}}"
  messages: "{{messages}}"
  tools: "{{tools}}"
  stream: "{{stream}}"
auth: {{}}
headers: {{}}
schemas:
  streaming:
    mode: delta_merge
  output_limit: {{path: max_tokens, semantics: provider_native_output_tokens}}
pricing:
  explicitly_free: true
  input_per_million: "0"
  output_per_million: "0"
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

fn register_recorded_mock_provider_bundle(
    state_path: &Path,
    mock_base_url: &str,
    fixture: &FastFixture,
) -> anyhow::Result<()> {
    register_config_fixture_bundle(
        state_path,
        "fixture-directive-recovery-model-config",
        fixture,
        |bundle_root| plant_recorded_mock_provider(bundle_root, mock_base_url, &fixture.publisher),
    )
}

fn plant_model_routing(root: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = r#"tiers:
  general:
    provider: mock
    model: mock-model
    context_window: 200000
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

fn plant_retry_execution_policy(
    root: &Path,
    signer: &SigningKey,
    backoff_base_ms: u64,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"category: "ryeos-runtime"
retries: 2
retry_status_codes: [503]
never_retry: []
backoff_base_ms: {backoff_base_ms}
timeout_seconds: 300
max_provider_output_tokens_per_turn: 32768
max_stream_output_bytes_per_turn: 131072
max_provider_stream_frame_bytes: 1048576
accounting:
  failure_policy: auto
  budget_mode: settled
tool_preload: false
retry_on_timeout: false
retry_mid_stream: false
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("execution.yaml"), signed)?;
    Ok(())
}

fn plant_recovery_directive(
    root: &Path,
    signer: &SigningKey,
    tool_call_limit: u32,
    declares_outputs: bool,
) -> anyhow::Result<()> {
    let path = root.join(".ai/directives/test/recovery.md");
    std::fs::create_dir_all(path.parent().expect("directive parent"))?;
    let outputs = if declares_outputs {
        "outputs:\n  - name: answer\n    type: string\n"
    } else {
        ""
    };
    let body = format!(
        r#"---
name: recovery
category: "test"
description: "Recorded directive crash-recovery fixture"
effects: recorded
inputs:
  - name: name
    type: string
    required: true
model:
  tier: general
requires:
  capabilities:
    declared:
      - "ryeos.execute.tool.*"
limits:
  turns: 4
  tool_calls: {tool_call_limit}
{outputs}return_nudge: false
---
Call the marker tool exactly once, then report completion.
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(path, signed)?;
    Ok(())
}

fn plant_marker_tool(
    root: &Path,
    marker_path: &Path,
    sleep_after_marker: Duration,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/tools/marker");
    std::fs::create_dir_all(&dir)?;
    let marker_literal = serde_json::to_string(
        marker_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("marker path is not UTF-8"))?,
    )?;
    let body = format!(
        r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "marker"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   description: "Crash-recovery execution marker"

import json
import os
import time

MARKER = {marker_literal}
with open(MARKER, "a", encoding="utf-8") as marker:
    marker.write("executed\n")
    marker.flush()
    os.fsync(marker.fileno())
time.sleep({sleep_seconds})
print(json.dumps({{"executed": True}}))
"#,
        sleep_seconds = sleep_after_marker.as_secs_f64(),
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("marker.py"), signed)?;
    Ok(())
}

fn projection_events(state_path: &Path, thread_id: &str) -> Vec<(String, Value)> {
    let db_path = match common::selected_projection_path(state_path) {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };
    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(conn) => conn,
        Err(_) => return Vec::new(),
    };
    let mut statement = match conn
        .prepare("SELECT event_type, payload FROM events WHERE thread_id=?1 ORDER BY chain_seq ASC")
    {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    statement
        .query_map(rusqlite::params![thread_id], |row| {
            let event_type: String = row.get(0)?;
            let payload: Vec<u8> = row.get(1)?;
            Ok((
                event_type,
                serde_json::from_slice(&payload).unwrap_or(Value::Null),
            ))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default()
}

fn projection_thread_status(state_path: &Path, thread_id: &str) -> Option<String> {
    let db_path = common::selected_projection_path(state_path).ok()?;
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.query_row(
        "SELECT status FROM threads WHERE thread_id=?1",
        rusqlite::params![thread_id],
        |row| row.get(0),
    )
    .ok()
}

fn root_thread_id(state_path: &Path) -> Option<String> {
    let db_path = common::selected_projection_path(state_path).ok()?;
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.query_row(
        "SELECT thread_id FROM threads WHERE item_ref=?1 ORDER BY created_at ASC LIMIT 1",
        rusqlite::params![DIRECTIVE_REF],
        |row| row.get(0),
    )
    .ok()
}

fn runtime_action_intents(state_path: &Path, chain_root_id: &str) -> Vec<(String, String)> {
    let db_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("state/runtime.sqlite3");
    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(conn) => conn,
        Err(_) => return Vec::new(),
    };
    let mut statement = match conn.prepare(
        "SELECT chain_root_id, operation_id, child_thread_id FROM runtime_action_intent \
         ORDER BY created_at_ms ASC",
    ) {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(retained_chain, operation_id, child_thread_id)| {
            (retained_chain == chain_root_id).then_some((operation_id, child_thread_id))
        })
        .collect()
}

fn provider_retry_advance(
    state_path: &Path,
    thread_id: &str,
) -> Option<ryeos_accounting::ProviderRetryAdvance> {
    let db_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join(ryeos_app::accounting_db::ACCOUNTING_DB_FILENAME);
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let raw: String = conn
        .query_row(
            "SELECT operation.response_json
               FROM accounting_operation AS operation
               JOIN provider_attempt_reservation AS attempt
                 ON attempt.attempt_id=operation.attempt_id
              WHERE attempt.thread_id=?1 AND operation.operation_kind='retry_advance'
              ORDER BY attempt.turn, attempt.attempt_number
              LIMIT 1",
            rusqlite::params![thread_id],
            |row| row.get(0),
        )
        .ok()?;
    serde_json::from_str(&raw).ok()
}

fn provider_attempts(state_path: &Path, thread_id: &str) -> Vec<(u32, u32, String)> {
    let db_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join(ryeos_app::accounting_db::ACCOUNTING_DB_FILENAME);
    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(conn) => conn,
        Err(_) => return Vec::new(),
    };
    let mut statement = match conn.prepare(
        "SELECT turn, attempt_number, state
           FROM provider_attempt_reservation
          WHERE thread_id=?1
          ORDER BY turn, attempt_number",
    ) {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    statement
        .query_map(rusqlite::params![thread_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default()
}

fn event_count(events: &[(String, Value)], event_type: &str) -> usize {
    events.iter().filter(|(kind, _)| kind == event_type).count()
}

fn transcript_cognition_out_count(events: &[(String, Value)]) -> usize {
    events
        .iter()
        .filter(|(kind, payload)| {
            kind == "cognition_out"
                && (payload.get("content").is_some() || payload.get("tool_calls").is_some())
        })
        .count()
}

fn marker_execution_count(marker_path: &Path) -> usize {
    std::fs::read_to_string(marker_path)
        .map(|body| body.lines().filter(|line| *line == "executed").count())
        .unwrap_or(0)
}

async fn start_execute_request(
    harness: &DaemonHarness,
    project_path: &Path,
) -> tokio::task::JoinHandle<Result<(u16, String), String>> {
    let url = format!("http://{}/execute", harness.bind);
    let body = json!({
        "item_ref": DIRECTIVE_REF,
        "ref_bindings": {"model": DIRECTIVE_REF},
        "project_path": project_path.to_str().expect("UTF-8 project path"),
        "parameters": {"name": "recovery"},
        "execution_policy": {
            "schema_version": 2,
            "ownership": "daemon_owned",
            "recovery": "restart_recoverable",
            "response": "wait",
            "target": {"kind": "here"},
            "environment": {
                "kind": "project_overlay",
                "include_operator_vault": true,
                "name_policy": {"kind": "declared_required"}
            },
            "project": {
                "kind": "pinned",
                "source": {"kind": "capture_live", "scope": "full_project"},
                "realization": {"kind": "read_only"},
                "child_policy": {"kind": "inherit"}
            }
        }
    });
    let body_bytes = serde_json::to_vec(&body).expect("serialize execute request");
    let headers = build_signed_headers_for_bytes(
        harness.user_key.as_ref().expect("operator key"),
        harness.node_key.as_ref().expect("node key"),
        "POST",
        "/execute",
        &body_bytes,
    );
    tokio::spawn(async move {
        let mut request = reqwest::Client::new()
            .post(url)
            .header("content-type", "application/json")
            .body(body_bytes);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|error| error.to_string())?;
        Ok((status, body))
    })
}

async fn await_root_thread(
    harness: &mut DaemonHarness,
    request: &mut tokio::task::JoinHandle<Result<(u16, String), String>>,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(thread_id) = root_thread_id(&harness.state_path) {
            return thread_id;
        }
        assert!(
            Instant::now() < deadline,
            "directive root thread did not appear"
        );
        tokio::select! {
            result = &mut *request => {
                let stderr = harness.drain_stderr_nonblocking().await;
                panic!("execute request settled before root admission: {result:?}\n--- daemon stderr ---\n{stderr}");
            },
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

async fn await_child_born_marker(
    harness: &mut DaemonHarness,
    marker_path: &Path,
    request: &mut tokio::task::JoinHandle<Result<(u16, String), String>>,
) {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if marker_execution_count(marker_path) == 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "directive child did not durably write its execution marker"
        );
        tokio::select! {
            result = &mut *request => {
                let stderr = harness.drain_stderr_nonblocking().await;
                let daemon_status = harness.child.try_wait();
                panic!(
                    "execute request settled before the parked marker child: {result:?}; \
                     daemon_status={daemon_status:?}; marker_count={}\n\
                     --- daemon stderr ---\n{stderr}",
                    marker_execution_count(marker_path),
                );
            },
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

async fn await_completed(harness: &mut DaemonHarness, thread_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match projection_thread_status(&harness.state_path, thread_id).as_deref() {
            Some("completed") => return,
            Some("failed" | "cancelled" | "killed" | "timed_out" | "continued") => {
                let events = projection_events(&harness.state_path, thread_id);
                let stderr = harness.drain_stderr_nonblocking().await;
                panic!(
                    "resumed directive reached a non-completed terminal; events={events:#?}\n\
                     --- daemon stderr ---\n{stderr}"
                );
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            let events = projection_events(&harness.state_path, thread_id);
            let stderr = harness.drain_stderr_nonblocking().await;
            panic!(
                "resumed directive did not complete; events={events:#?}\n\
                 --- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn run_crash_cut(cut: CrashCut) {
    let responses = match cut {
        CrashCut::ProviderRetrySettled => vec![
            MockResponse::HttpError {
                status: 503,
                body: "transient provider failure".to_owned(),
            },
            MockResponse::ToolCall {
                id: "marker-call".to_string(),
                name: TOOL_NAME.to_string(),
                arguments: "{}".to_string(),
            },
            MockResponse::Text("recovery complete".to_string()),
        ],
        CrashCut::DirectiveReturnRecorded => vec![
            MockResponse::ToolCall {
                id: "marker-call".to_string(),
                name: TOOL_NAME.to_string(),
                arguments: "{}".to_string(),
            },
            MockResponse::ToolCalls(vec![
                MockToolCallSpec {
                    id: "return-call".to_string(),
                    name: DIRECTIVE_RETURN_NAME.to_string(),
                    arguments: r#"{"answer":"recovery complete"}"#.to_string(),
                },
                MockToolCallSpec {
                    id: "abandoned-marker-call".to_string(),
                    name: TOOL_NAME.to_string(),
                    arguments: "{}".to_string(),
                },
            ]),
        ],
        CrashCut::ToolLimitRefusalStartRecorded => vec![
            MockResponse::ToolCalls(vec![
                MockToolCallSpec {
                    id: "marker-call".to_string(),
                    name: TOOL_NAME.to_string(),
                    arguments: "{}".to_string(),
                },
                MockToolCallSpec {
                    id: "refused-marker-call".to_string(),
                    name: TOOL_NAME.to_string(),
                    arguments: "{}".to_string(),
                },
            ]),
            MockResponse::Text("recovery complete".to_string()),
        ],
        _ => vec![
            MockResponse::ToolCall {
                id: "marker-call".to_string(),
                name: TOOL_NAME.to_string(),
                arguments: "{}".to_string(),
            },
            MockResponse::Text("recovery complete".to_string()),
        ],
    };
    let mock = MockProvider::start(responses).await;
    let mock_url = mock.base_url.clone();
    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_standard_bundle(state_path, fixture)?;
            register_recorded_mock_provider_bundle(state_path, &mock_url, fixture)
        };
    let (mut phase_gate, mut phase_child) = match cut.runtime_block_stage() {
        Some(stage) => {
            let (gate, child) =
                RuntimePhaseCutGate::pair(stage).expect("create exact directive phase-cut gate");
            (Some(gate), Some(child))
        }
        None => (None, None),
    };
    let (mut harness, fixture) = DaemonHarness::start_fast_with(plant, |command| {
        if let Some(stage) = cut.runtime_block_stage() {
            phase_child
                .as_mut()
                .expect("phase child exists for selected runtime stage")
                .configure_command(command, stage)
                .expect("configure exact directive phase-cut channel");
            command.env("RYEOS_DIRECTIVE_TEST_BLOCK_AT", stage);
        }
        command.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start directive recovery daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    let marker_dir = tempfile::tempdir().expect("marker tempdir");
    let marker_path = marker_dir.path().join("executions.log");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    if cut == CrashCut::ProviderRetrySettled {
        // Long enough that daemon termination and respawn still reach the
        // ledger before the durable earliest-admission boundary.
        plant_retry_execution_policy(project.path(), &fixture.publisher, 4_000)
            .expect("plant exact retry execution policy");
    }
    plant_recovery_directive(
        project.path(),
        &fixture.publisher,
        if cut == CrashCut::ToolLimitRefusalStartRecorded {
            1
        } else {
            2
        },
        cut == CrashCut::DirectiveReturnRecorded,
    )
    .expect("plant directive");
    plant_marker_tool(
        project.path(),
        &marker_path,
        if cut == CrashCut::ChildBorn {
            Duration::from_secs(120)
        } else {
            Duration::ZERO
        },
        &fixture.publisher,
    )
    .expect("plant marker tool");

    let mut execute_request = start_execute_request(&harness, project.path()).await;
    let root_thread_id = await_root_thread(&mut harness, &mut execute_request).await;
    if cut == CrashCut::ChildBorn {
        // This child fsyncs the marker and then remains inside its admitted
        // process for two minutes. The marker is the exact child-owned effect
        // boundary; runtime-action authority is inspected only after shutdown.
        await_child_born_marker(&mut harness, &marker_path, &mut execute_request).await;
    } else {
        let gate = phase_gate
            .as_mut()
            .expect("runtime-owned cut has an inherited phase gate");
        tokio::select! {
            result = gate.wait_reached() => {
                if let Err(error) = result {
                    let stderr = harness.drain_stderr_nonblocking().await;
                    panic!("directive did not report exact {cut:?} boundary: {error:#}\n--- daemon stderr ---\n{stderr}");
                }
            }
            result = &mut execute_request => {
                let stderr = harness.drain_stderr_nonblocking().await;
                panic!("execute request settled before exact {cut:?} boundary: {result:?}\n--- daemon stderr ---\n{stderr}");
            }
        }
    }

    harness.kill_daemon().await.expect("kill daemon at cut");
    let retry_advance = (cut == CrashCut::ProviderRetrySettled).then(|| {
        provider_retry_advance(&harness.state_path, &root_thread_id)
            .expect("atomic retry advancement must exist at the selected crash cut")
    });
    execute_request.abort();
    harness
        .respawn_with(|command| {
            command.env(
                "RUST_LOG",
                std::env::var("RUST_LOG")
                    .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
            );
        })
        .await
        .expect("respawn daemon without cut hook");
    await_completed(&mut harness, &root_thread_id).await;
    harness
        .kill_daemon()
        .await
        .expect("stop recovered directive daemon before evidence inspection");

    let events = projection_events(&harness.state_path, &root_thread_id);
    assert_eq!(
        event_count(&events, "thread_started"),
        1,
        "native resume must not duplicate thread_started; events={events:#?}"
    );
    assert_eq!(
        transcript_cognition_out_count(&events),
        2,
        "the retained tool proposal and final answer must each settle once; events={events:#?}"
    );
    let expected_tool_events = if matches!(
        cut,
        CrashCut::DirectiveReturnRecorded | CrashCut::ToolLimitRefusalStartRecorded
    ) {
        2
    } else {
        1
    };
    assert_eq!(
        event_count(&events, "tool_call_start"),
        expected_tool_events,
        "one proposal has one start; events={events:#?}"
    );
    assert_eq!(
        event_count(&events, "tool_call_result"),
        expected_tool_events,
        "one proposal has one result; events={events:#?}"
    );
    let start_operation = events
        .iter()
        .find(|(kind, _)| kind == "tool_call_start")
        .and_then(|(_, payload)| payload.get("operation_id"))
        .and_then(Value::as_str)
        .expect("start operation id");
    let result_operation = events
        .iter()
        .find(|(kind, _)| kind == "tool_call_result")
        .and_then(|(_, payload)| payload.get("operation_id"))
        .and_then(Value::as_str)
        .expect("result operation id");
    assert_eq!(start_operation, result_operation);
    assert!(ryeos_runtime::callback::valid_action_operation_id(
        start_operation
    ));

    let intents = runtime_action_intents(&harness.state_path, &root_thread_id);
    assert_eq!(
        intents.len(),
        1,
        "one logical tool occurrence must own one child; intents={intents:?}"
    );
    assert_eq!(intents[0].0, start_operation);
    assert_eq!(
        marker_execution_count(&marker_path),
        1,
        "the child mutation marker must execute exactly once"
    );
    if cut == CrashCut::DirectiveReturnRecorded {
        assert!(events.iter().all(|(kind, payload)| {
            kind != "tool_call_start"
                || payload.get("call_id").and_then(Value::as_str) != Some("abandoned-marker-call")
        }));
    }
    if cut == CrashCut::ToolLimitRefusalStartRecorded {
        assert!(events.iter().any(|(kind, payload)| {
            kind == "tool_call_result"
                && payload.get("call_id").and_then(Value::as_str) == Some("refused-marker-call")
                && payload.get("truncated_reason").and_then(Value::as_str) == Some("error_envelope")
        }));
    }

    let bodies = mock.captured_bodies().await;
    let expected_provider_calls = if cut == CrashCut::ProviderRetrySettled {
        3
    } else {
        2
    };
    assert_eq!(
        bodies.len(),
        expected_provider_calls,
        "restart must issue only the exact required provider calls; bodies={bodies:#?}"
    );
    let final_body_index = if cut == CrashCut::ProviderRetrySettled {
        2
    } else {
        1
    };
    assert!(
        bodies[final_body_index]
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| messages.iter().any(|message| {
                message.get("role").and_then(Value::as_str) == Some("tool")
                    && message.get("tool_call_id").and_then(Value::as_str) == Some("marker-call")
            })),
        "the final request must consume the one retained tool observation; body={:#?}",
        bodies[final_body_index]
    );
    let provider_observations = events
        .iter()
        .filter(|(kind, _)| kind == "provider_call_observation_recorded")
        .map(|(_, payload)| payload)
        .collect::<Vec<_>>();
    let unique_provider_records = provider_observations
        .iter()
        .filter_map(|payload| payload.get("record_hash").and_then(Value::as_str))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique_provider_records.len(),
        2,
        "the tool proposal and final answer must resolve to two exact provider records; observations={provider_observations:#?}"
    );
    if cut == CrashCut::ProviderEffectSettled {
        assert!(
            provider_observations.iter().any(|payload| {
                payload.get("source").and_then(Value::as_str) == Some("replay")
                    || payload.get("source").and_then(Value::as_str) == Some("replayed")
            }) && events.iter().any(|(kind, payload)| {
                kind == "cognition_out"
                    && payload
                        .pointer("/provider_accounting/replayed_from")
                        .and_then(Value::as_str)
                        .is_some()
            }),
            "the first cognition and provider observation must testify that they replayed the already-published effect; observations={provider_observations:#?}"
        );
    }
    if cut == CrashCut::ProviderRetrySettled {
        let advance = retry_advance.expect("retry advancement selected for retry crash cut");
        assert_eq!(
            event_count(&events, "provider_retry"),
            1,
            "the recovered atomic advancement must produce one retry testimony; events={events:#?}"
        );
        assert_eq!(
            bodies[0], bodies[1],
            "the exact successor must preserve provider-visible behavior bytes"
        );
        let request_times = mock.captured_request_times_ms().await;
        assert_eq!(request_times.len(), 3);
        assert!(
            request_times[1] >= advance.not_before_ms,
            "successor contacted provider at {} before durable not-before {}",
            request_times[1],
            advance.not_before_ms
        );
        let turn_one_attempts = provider_attempts(&harness.state_path, &root_thread_id)
            .into_iter()
            .filter(|(turn, _, _)| *turn == 1)
            .collect::<Vec<_>>();
        assert_eq!(
            turn_one_attempts
                .iter()
                .map(|(_, attempt, _)| *attempt)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "failed attempt must not be recontacted or skipped; attempts={turn_one_attempts:?}"
        );
        assert_eq!(advance.decision.failed_attempt_number, 1);
        assert_eq!(advance.decision.next_attempt_number, 2);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resumes_once_across_every_durable_directive_cut() {
    for cut in [
        CrashCut::ProviderEffectSettled,
        CrashCut::ProviderRetrySettled,
        CrashCut::ToolProposalRecorded,
        CrashCut::ChildBorn,
        CrashCut::ToolSettled,
        CrashCut::ToolObservationRecorded,
        CrashCut::FinalCognitionRecorded,
        CrashCut::DirectiveReturnRecorded,
        CrashCut::ToolLimitRefusalStartRecorded,
    ] {
        eprintln!("[directive-recovery] exercising {cut:?}");
        run_crash_cut(cut).await;
    }
}
