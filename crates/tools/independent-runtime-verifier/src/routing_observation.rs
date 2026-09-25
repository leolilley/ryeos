//! Pure, finite checks of notifications observed from pinned Codex 0.147.0.
//!
//! The caller must collect these from its own app-server pipe, with bounded
//! reads, not from a scripted model server's summary. This module performs no
//! I/O and confers no runtime qualification, guest-wire provenance, candidate
//! content, writer exclusion, credential custody or worker authority.
//!
//! Source contract: rust-v0.147.0 app-server-protocol v2/{item,turn}.rs and
//! item_builders.rs. The latter maps exec/patch call_id to completed item.id.
//! It does NOT equate those IDs with guest exec-server processId values.
//! The live collector must initialize with experimentalApi:true and start the
//! thread with experimentalRawEvents:true; otherwise required raw tool results
//! are deliberately absent. Do not weaken these checks to cover missing opt-in.

use anyhow::{Context as _, Result, ensure};
use serde_json::Value;
use std::collections::BTreeSet;

pub const LOCAL_CALL: &str = "forbidden-local-command";
pub const COMMAND_CALL: &str = "guest-command";
pub const PATCH_CALL: &str = "candidate-edit";
pub const SECRET_CALL: &str = "guest-controller-secret-read";
pub const MAX_EVENTS: usize = 512;
pub const MAX_EVENT_BYTES: usize = 64 * 1024;
pub const MAX_TOTAL_BYTES: usize = 1024 * 1024;
const LOCAL_REFUSAL: &str = "unknown turn environment id `local`";

/// Authored expectations supplied before the turn, never learned from its
/// results. Commands are exact app-server presentation strings (shell quoting
/// included), not guessed reconstructions of guest argv. The independently
/// observed guest wire and frozen files require their own subsequent checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingScenario {
    pub thread_id: String,
    pub turn_id: String,
    pub guest_cwd: String,
    pub local_refusal_command: String,
    pub guest_command_script: String,
    pub secret_read_script: String,
    pub patch_input: String,
    pub guest_command: String,
    pub secret_read_command: String,
    pub candidate_path: String,
    /// For an Add, Codex fileChange.diff is the raw added file content, not
    /// unified-diff text or the apply_patch input (item_builders.rs:355).
    pub candidate_added_content: String,
    pub expected_command_output: String,
    pub secret_read_denial: String,
    pub controller_canary_value: String,
}

/// Describes only the checked notification stream, not a qualification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedNotifications {
    pub thread_id: String,
    pub turn_id: String,
    pub notification_count: usize,
    pub serialized_bytes: usize,
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string {field}"))
}

fn scenario_valid(s: &RoutingScenario) -> Result<()> {
    for value in [
        &s.thread_id,
        &s.turn_id,
        &s.guest_cwd,
        &s.local_refusal_command,
        &s.guest_command_script,
        &s.secret_read_script,
        &s.patch_input,
        &s.guest_command,
        &s.secret_read_command,
        &s.candidate_path,
        &s.candidate_added_content,
        &s.expected_command_output,
        &s.secret_read_denial,
        &s.controller_canary_value,
    ] {
        ensure!(
            !value.is_empty() && value.len() <= 8192,
            "scenario field exceeds bound"
        );
    }
    ensure!(
        s.thread_id.len() <= 128 && s.turn_id.len() <= 128,
        "scenario identity exceeds bound"
    );
    ensure!(
        s.guest_command != s.secret_read_command,
        "scenario commands must differ"
    );
    ensure!(
        s.controller_canary_value.len() >= 16,
        "canary must be a distinct synthetic value"
    );
    ensure!(
        s.guest_cwd.starts_with('/') && s.candidate_path.starts_with('/'),
        "scenario paths must be absolute"
    );
    ensure!(
        s.candidate_path[1..]
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != ".."),
        "candidate path must be normalized"
    );
    Ok(())
}

fn contains_canary(
    value: &Value,
    canary: &str,
    depth: usize,
    remaining: &mut usize,
) -> Result<bool> {
    ensure!(depth <= 32, "notification nesting exceeds bound");
    *remaining = remaining
        .checked_sub(1)
        .context("notification value count exceeds bound")?;
    Ok(match value {
        Value::String(s) => {
            *remaining = remaining
                .checked_sub(s.len())
                .context("notification strings exceed bound")?;
            s.contains(canary)
        }
        Value::Array(values) => {
            let mut found = false;
            for v in values {
                found |= contains_canary(v, canary, depth + 1, remaining)?;
            }
            found
        }
        Value::Object(values) => {
            let mut found = false;
            for (k, v) in values {
                *remaining = remaining
                    .checked_sub(k.len())
                    .context("notification keys exceed bound")?;
                found |= k.contains(canary);
                found |= contains_canary(v, canary, depth + 1, remaining)?;
            }
            found
        }
        _ => false,
    })
}

fn same_identity(params: &Value, scenario: &RoutingScenario) -> Result<()> {
    ensure!(
        text(params, "threadId")? == scenario.thread_id
            && text(params, "turnId")? == scenario.turn_id,
        "notification changed thread or turn"
    );
    Ok(())
}

// Pinned core/src/tools/context.rs::UnifiedExecResult::response_text renders
// this header before Output:. Never accept an exit-looking line printed by
// the child itself as the tool's status.
fn raw_command_exit(output: &str) -> Result<i32> {
    let (header, _) = output
        .split_once("\nOutput:\n")
        .context("raw command header absent")?;
    let exits = header
        .lines()
        .filter_map(|line| line.strip_prefix("Process exited with code "))
        .collect::<Vec<_>>();
    ensure!(
        exits.len() == 1 && !header.contains("Process running with session ID"),
        "raw command is not exactly terminal"
    );
    Ok(exits[0].parse()?)
}

/// Check an already bounded collection. The caller must also bound transport
/// frames/counts while reading; post-collection validation cannot undo memory
/// allocated by an unbounded reader. Failure is final: callers must not omit a
/// failing event and retry the same stream as though it had never occurred.
pub fn check_notifications(
    scenario: &RoutingScenario,
    notifications: &[Value],
) -> Result<CheckedNotifications> {
    scenario_valid(scenario)?;
    ensure!(
        !notifications.is_empty() && notifications.len() <= MAX_EVENTS,
        "notification count exceeds bound"
    );
    let mut serialized_bytes = 0usize;
    let mut raw_outputs = BTreeSet::new();
    let mut raw_calls = BTreeSet::new();
    let mut response_ids = BTreeSet::new();
    let mut started_items = BTreeSet::new();
    let mut completed_items = BTreeSet::new();
    let mut terminal = false;
    for notification in notifications {
        ensure!(
            notification.is_object() && notification.get("id").is_none(),
            "expected notification, not request/response"
        );
        // Bound depth, nodes and unescaped text before invoking the serializer.
        // Its maximum temporary expansion is then finite even for quoted text.
        let mut value_budget = MAX_EVENT_BYTES;
        ensure!(
            !contains_canary(
                notification,
                &scenario.controller_canary_value,
                0,
                &mut value_budget
            )?,
            "controller canary appeared in app-server events"
        );
        let bytes = serde_json::to_vec(notification)?.len();
        serialized_bytes = serialized_bytes
            .checked_add(bytes)
            .context("notification size overflow")?;
        ensure!(
            bytes <= MAX_EVENT_BYTES && serialized_bytes <= MAX_TOTAL_BYTES,
            "notification bytes exceed bound"
        );
        let method = text(notification, "method")?;
        let params = notification
            .get("params")
            .filter(|p| p.is_object())
            .context("notification params must be object")?;
        for (name, expected) in [
            ("threadId", &scenario.thread_id),
            ("turnId", &scenario.turn_id),
        ] {
            if let Some(value) = params.get(name) {
                ensure!(
                    value.as_str() == Some(expected.as_str()),
                    "notification identity differs"
                );
            }
        }
        match method {
            "configWarning" => ensure!(
                params.get("details").is_none_or(Value::is_null)
                    && text(params, "summary")?
                        .starts_with("Project-local config, hooks, and exec policies are disabled"),
                "unexpected configuration warning"
            ),
            "remoteControl/status/changed" => ensure!(
                params.get("environmentId").is_none_or(Value::is_null)
                    && text(params, "status")? == "disabled",
                "remote control became available"
            ),
            "account/rateLimits/updated" => {
                let limits = params
                    .get("rateLimits")
                    .filter(|value| value.is_object())
                    .context("rate-limit notification absent")?;
                ensure!(
                    text(limits, "limitId")? == "codex"
                        && [
                            "credits",
                            "individualLimit",
                            "limitName",
                            "planType",
                            "primary",
                            "rateLimitReachedType",
                            "secondary",
                            "spendControlReached",
                        ]
                        .iter()
                        .all(|key| limits.get(*key).is_none_or(Value::is_null)),
                    "scripted peer acquired account quota"
                );
            }
            "thread/started" => {
                let thread = params
                    .get("thread")
                    .filter(|value| value.is_object())
                    .context("started thread absent")?;
                ensure!(
                    text(thread, "id")? == scenario.thread_id
                        && text(thread, "cwd")? == scenario.guest_cwd
                        && text(thread, "modelProvider")? == "routing-fixture"
                        && thread["status"]["type"] == "idle",
                    "started thread differs from closed scenario"
                );
            }
            "thread/status/changed" => {
                ensure!(
                    text(params, "threadId")? == scenario.thread_id
                        && matches!(params["status"]["type"].as_str(), Some("active" | "idle"))
                        && params["status"].get("activeFlags").is_none_or(|flags| flags
                            .as_array()
                            .is_some_and(|values| values.is_empty())),
                    "thread status changed outside closed scenario"
                );
            }
            "thread/tokenUsage/updated" => {
                same_identity(params, scenario)?;
                let usage = params
                    .get("tokenUsage")
                    .filter(|value| value.is_object())
                    .context("token usage absent")?;
                ensure!(
                    usage["last"]["totalTokens"] == 0 && usage["total"]["totalTokens"] == 0,
                    "scripted peer reported nonzero model spend"
                );
            }
            "turn/started" => {
                ensure!(
                    text(params, "threadId")? == scenario.thread_id
                        && params["turn"]["id"] == scenario.turn_id
                        && params["turn"]["status"] == "inProgress",
                    "started turn differs from closed scenario"
                );
            }
            "rawResponse/completed" => {
                same_identity(params, scenario)?;
                let id = text(params, "responseId")?;
                ensure!(
                    id.starts_with("scripted-")
                        && response_ids.insert(id.to_owned())
                        && params["usage"]["totalTokens"] == 0,
                    "unplanned or duplicate scripted response"
                );
            }
            "turn/diff/updated" => {
                same_identity(params, scenario)?;
                // This is a UI projection, not a candidate or file-inventory
                // authority. Codex currently renders it as a Git-style diff,
                // but neither that header nor its text can prove frozen bytes.
                // The completed file-change item and native frozen capture are
                // checked independently; only classify this event's shape.
                text(params, "diff")?;
            }
            "item/started" => {
                same_identity(params, scenario)?;
                ensure!(!terminal, "item started after terminal turn");
                let item = params
                    .get("item")
                    .filter(|value| value.is_object())
                    .context("started item absent")?;
                match text(item, "type")? {
                    "commandExecution" => {
                        let id = text(item, "id")?;
                        ensure!(
                            started_items.insert(id.to_owned()) && !completed_items.contains(id),
                            "duplicate or late command start"
                        );
                        let expected = match id {
                            COMMAND_CALL => &scenario.guest_command,
                            SECRET_CALL => &scenario.secret_read_command,
                            _ => anyhow::bail!("unplanned command start"),
                        };
                        ensure!(
                            text(item, "command")? == expected
                                && text(item, "cwd")? == scenario.guest_cwd
                                && text(item, "status")? == "inProgress"
                                && text(item, "source")? == "unifiedExecStartup",
                            "command start differs from planned guest call"
                        );
                    }
                    "fileChange" => {
                        let id = text(item, "id")?;
                        ensure!(
                            started_items.insert(id.to_owned()) && !completed_items.contains(id),
                            "duplicate or late file-change start"
                        );
                        ensure!(
                            id == PATCH_CALL
                                && text(item, "status")? == "inProgress"
                                && item["changes"].as_array().is_some_and(|changes| {
                                    changes.len() == 1
                                        && changes[0]["path"] == scenario.candidate_path
                                        && changes[0]["kind"]["type"] == "add"
                                        && changes[0]["diff"] == scenario.candidate_added_content
                                }),
                            "file-change start differs from planned patch"
                        );
                    }
                    "userMessage" | "agentMessage" => ensure!(
                        ![LOCAL_CALL, COMMAND_CALL, SECRET_CALL, PATCH_CALL]
                            .contains(&text(item, "id")?),
                        "tool call changed into message"
                    ),
                    _ => anyhow::bail!("unplanned started item"),
                }
            }
            "rawResponseItem/completed" => {
                same_identity(params, scenario)?;
                ensure!(!terminal, "raw item arrived after terminal turn");
                let item = params
                    .get("item")
                    .filter(|p| p.is_object())
                    .context("raw item absent")?;
                match text(item, "type")? {
                    "function_call_output" | "custom_tool_call_output" => {
                        let id = text(item, "call_id")?;
                        ensure!(raw_calls.contains(id), "raw result preceded its invocation");
                        ensure!(
                            raw_outputs.insert(id.to_owned()),
                            "duplicate raw tool result"
                        );
                        let output = text(item, "output")?;
                        match id {
                            LOCAL_CALL => ensure!(
                                text(item, "type")? == "function_call_output"
                                    && output.trim() == LOCAL_REFUSAL,
                                "explicit local refusal missing"
                            ),
                            COMMAND_CALL => ensure!(
                                text(item, "type")? == "function_call_output"
                                    && raw_command_exit(output)? == 0
                                    && output.contains(&scenario.expected_command_output),
                                "guest command raw output differs"
                            ),
                            PATCH_CALL => ensure!(
                                text(item, "type")? == "custom_tool_call_output"
                                    && output.contains("Success.")
                                    && output.contains(
                                        scenario
                                            .candidate_path
                                            .rsplit('/')
                                            .next()
                                            .context("candidate basename absent")?
                                    ),
                                "patch raw completion differs"
                            ),
                            SECRET_CALL => ensure!(
                                text(item, "type")? == "function_call_output"
                                    && raw_command_exit(output)? != 0
                                    && output.contains(&scenario.secret_read_denial),
                                "guest secret-read refusal absent"
                            ),
                            _ => anyhow::bail!("unexpected tool result in closed routing scenario"),
                        }
                    }
                    "function_call" | "custom_tool_call" => {
                        let id = text(item, "call_id")?;
                        ensure!(
                            raw_calls.insert(id.to_owned()),
                            "duplicate raw tool invocation"
                        );
                        let (kind, name) = match id {
                            LOCAL_CALL | COMMAND_CALL | SECRET_CALL => {
                                ("function_call", "exec_command")
                            }
                            PATCH_CALL => ("custom_tool_call", "apply_patch"),
                            _ => anyhow::bail!("unplanned raw tool invocation"),
                        };
                        ensure!(
                            text(item, "type")? == kind && text(item, "name")? == name,
                            "raw tool invocation changed type or name"
                        );
                        if kind == "function_call" {
                            let arguments: Value = serde_json::from_str(text(item, "arguments")?)?;
                            let expected = match id {
                                LOCAL_CALL => serde_json::json!({
                                    "environment_id":"local",
                                    "cmd":scenario.local_refusal_command,
                                    "login":false,
                                    "yield_time_ms":10000,
                                }),
                                COMMAND_CALL | SECRET_CALL => serde_json::json!({
                                    "environment_id":"ryeos-external-candidate",
                                    "cmd":if id == COMMAND_CALL {&scenario.guest_command_script} else {&scenario.secret_read_script},
                                    "workdir":scenario.guest_cwd,
                                    "login":false,
                                    "yield_time_ms":10000,
                                }),
                                _ => unreachable!("closed command call set"),
                            };
                            ensure!(arguments == expected, "raw command invocation changed");
                        } else {
                            ensure!(
                                text(item, "input")? == scenario.patch_input,
                                "raw patch invocation changed"
                            );
                        }
                    }
                    // Context/model text is not execution evidence.
                    "message" | "reasoning" => (),
                    _ => anyhow::bail!("unexpected raw response item type"),
                }
            }
            "item/completed" => {
                same_identity(params, scenario)?;
                ensure!(
                    !terminal
                        && params
                            .get("completedAtMs")
                            .and_then(Value::as_i64)
                            .is_some_and(|v| v >= 0),
                    "malformed or late completed item"
                );
                let item = params
                    .get("item")
                    .filter(|p| p.is_object())
                    .context("completed item absent")?;
                let kind = text(item, "type")?;
                let id = text(item, "id")?;
                ensure!(
                    !id.is_empty() && completed_items.insert(id.to_owned()),
                    "duplicate completed item"
                );
                match kind {
                    "commandExecution" => {
                        ensure!(
                            text(item, "cwd")? == scenario.guest_cwd,
                            "command cwd differs"
                        );
                        let output = text(item, "aggregatedOutput")?;
                        let exit = item
                            .get("exitCode")
                            .and_then(Value::as_i64)
                            .context("command exit absent")?;
                        match id {
                            COMMAND_CALL => ensure!(
                                text(item, "command")? == scenario.guest_command
                                    && text(item, "status")? == "completed"
                                    && exit == 0
                                    && output == scenario.expected_command_output,
                                "guest command completion differs"
                            ),
                            SECRET_CALL => ensure!(
                                text(item, "command")? == scenario.secret_read_command
                                    && text(item, "status")? == "failed"
                                    && exit != 0
                                    && output.contains(&scenario.secret_read_denial),
                                "secret read did not fail explicitly"
                            ),
                            _ => anyhow::bail!(
                                "unexpected command execution, including forbidden local"
                            ),
                        }
                    }
                    "fileChange" => {
                        ensure!(
                            id == PATCH_CALL && text(item, "status")? == "completed",
                            "patch completion differs"
                        );
                        let changes = item
                            .get("changes")
                            .and_then(Value::as_array)
                            .context("patch changes absent")?;
                        ensure!(
                            changes.len() == 1
                                && text(&changes[0], "path")? == scenario.candidate_path
                                && changes[0].get("kind")
                                    == Some(&serde_json::json!({"type":"add"}))
                                && text(&changes[0], "diff")? == scenario.candidate_added_content,
                            "patch path/kind/diff differs"
                        );
                    }
                    "userMessage" | "agentMessage" | "reasoning" | "plan" => {
                        ensure!(
                            ![LOCAL_CALL, COMMAND_CALL, SECRET_CALL, PATCH_CALL].contains(&id),
                            "scenario item changed type"
                        );
                    }
                    _ => anyhow::bail!("unexpected completed item in closed routing scenario"),
                }
            }
            "turn/completed" => {
                ensure!(
                    !terminal && text(params, "threadId")? == scenario.thread_id,
                    "duplicate or foreign completed turn"
                );
                let turn = params
                    .get("turn")
                    .filter(|v| v.is_object())
                    .context("completed turn absent")?;
                ensure!(
                    text(turn, "id")? == scenario.turn_id
                        && text(turn, "status")? == "completed"
                        && turn.get("error").is_none_or(Value::is_null),
                    "turn did not complete cleanly"
                );
                terminal = true;
            }
            // The verifier must classify the entire observed stream before
            // issuing a closed-routing claim. Unknown notifications may
            // describe another execution path, even when this scenario's
            // expected calls are also present.
            "error" => anyhow::bail!("app-server reported an error during routing observation"),
            _ => anyhow::bail!("unclassified app-server notification: {method}"),
        }
    }
    ensure!(terminal, "turn completion missing");
    ensure!(
        raw_calls
            == [LOCAL_CALL, COMMAND_CALL, PATCH_CALL, SECRET_CALL]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        "required raw tool invocations missing"
    );
    ensure!(
        raw_outputs
            == [LOCAL_CALL, COMMAND_CALL, PATCH_CALL, SECRET_CALL]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        "required raw tool outcomes missing"
    );
    ensure!(
        [COMMAND_CALL, PATCH_CALL, SECRET_CALL]
            .iter()
            .all(|id| completed_items.contains(*id)),
        "required completed tool items missing"
    );
    Ok(CheckedNotifications {
        thread_id: scenario.thread_id.clone(),
        turn_id: scenario.turn_id.clone(),
        notification_count: notifications.len(),
        serialized_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (RoutingScenario, Vec<Value>) {
        let s = RoutingScenario {
            thread_id: "thread-1".into(),
            turn_id: "turn-1".into(),
            guest_cwd: "/workspace".into(),
            local_refusal_command: "printf forbidden > /controller/canary".into(),
            guest_command_script: "pwd; rg --version".into(),
            secret_read_script: "read < /controller/canary".into(),
            patch_input:
                "*** Begin Patch\n*** Add File: candidate-strategy.txt\n+candidate\n*** End Patch"
                    .into(),
            guest_command: "zsh -c 'pwd; rg --version'".into(),
            secret_read_command: "zsh -c 'read < /controller/canary'".into(),
            candidate_path: "/workspace/candidate-strategy.txt".into(),
            candidate_added_content: "candidate\n".into(),
            expected_command_output: "/workspace\nripgrep fixture\n".into(),
            secret_read_denial: "no such file or directory: /controller/canary".into(),
            controller_canary_value: "synthetic-controller-secret-never-exposed".into(),
        };
        let mut events = vec![];
        for (id, kind, name, input) in [
            (
                LOCAL_CALL,
                "function_call",
                "exec_command",
                json!({"environment_id":"local","cmd":s.local_refusal_command,
                    "login":false,"yield_time_ms":10000})
                .to_string(),
            ),
            (
                COMMAND_CALL,
                "function_call",
                "exec_command",
                json!({"environment_id":"ryeos-external-candidate",
                    "cmd":s.guest_command_script,"workdir":s.guest_cwd,
                    "login":false,"yield_time_ms":10000})
                .to_string(),
            ),
            (
                PATCH_CALL,
                "custom_tool_call",
                "apply_patch",
                s.patch_input.clone(),
            ),
            (
                SECRET_CALL,
                "function_call",
                "exec_command",
                json!({"environment_id":"ryeos-external-candidate",
                    "cmd":s.secret_read_script,"workdir":s.guest_cwd,
                    "login":false,"yield_time_ms":10000})
                .to_string(),
            ),
        ] {
            let item = if kind == "function_call" {
                json!({"type":kind,"call_id":id,"name":name,"arguments":input})
            } else {
                json!({"type":kind,"call_id":id,"name":name,"input":input})
            };
            events.push(json!({"method":"rawResponseItem/completed","params":{
                "threadId":s.thread_id,"turnId":s.turn_id,"item":item}}));
        }
        for (id, kind, output) in [
            (LOCAL_CALL, "function_call_output", LOCAL_REFUSAL.to_owned()),
            (
                COMMAND_CALL,
                "function_call_output",
                format!(
                    "Wall time: 0.001 seconds\nProcess exited with code 0\nOutput:\n{}",
                    s.expected_command_output
                ),
            ),
            (
                PATCH_CALL,
                "custom_tool_call_output",
                format!("Success. {}", s.candidate_path),
            ),
            (
                SECRET_CALL,
                "function_call_output",
                format!(
                    "Wall time: 0.001 seconds\nProcess exited with code 1\nOutput:\n{}",
                    s.secret_read_denial
                ),
            ),
        ] {
            events.push(json!({"method":"rawResponseItem/completed","params":{"threadId":s.thread_id,"turnId":s.turn_id,
                "item":{"type":kind,"call_id":id,"output":output}}}));
        }
        for (id, command, status, exit, output) in [
            (
                COMMAND_CALL,
                &s.guest_command,
                "completed",
                0,
                &s.expected_command_output,
            ),
            (
                SECRET_CALL,
                &s.secret_read_command,
                "failed",
                1,
                &s.secret_read_denial,
            ),
        ] {
            events.push(json!({"method":"item/completed","params":{"threadId":s.thread_id,"turnId":s.turn_id,"completedAtMs":1,
                "item":{"type":"commandExecution","id":id,"command":command,"cwd":s.guest_cwd,"status":status,
                "exitCode":exit,"aggregatedOutput":output}}}));
        }
        events.push(json!({"method":"item/completed","params":{"threadId":s.thread_id,"turnId":s.turn_id,"completedAtMs":1,
            "item":{"type":"fileChange","id":PATCH_CALL,"status":"completed","changes":[{"path":s.candidate_path,
                "kind":{"type":"add"},"diff":s.candidate_added_content}]}}}));
        events.push(
            json!({"method":"turn/completed","params":{"threadId":s.thread_id,
            "turn":{"id":s.turn_id,"status":"completed","error":null}}}),
        );
        (s, events)
    }

    #[test]
    fn complete_exact_notification_stream_is_only_stream_evidence() {
        let (s, events) = fixture();
        let checked = check_notifications(&s, &events).unwrap();
        assert_eq!(checked.notification_count, 12);
        assert_eq!(checked.thread_id, s.thread_id);
    }

    #[test]
    fn diff_notification_is_presentation_only() {
        let (s, mut events) = fixture();
        events.insert(
            8,
            json!({"method":"turn/diff/updated","params":{
                "threadId":s.thread_id,"turnId":s.turn_id,
                "diff":"a presentation format with no Git header"
            }}),
        );
        assert!(check_notifications(&s, &events).is_ok());
        events[8]["params"]["diff"] = json!(null);
        assert!(check_notifications(&s, &events).is_err());
    }

    #[test]
    fn every_required_event_is_required_and_duplicates_refuse() {
        let (s, events) = fixture();
        for index in 0..events.len() {
            let mut missing = events.clone();
            missing.remove(index);
            assert!(
                check_notifications(&s, &missing).is_err(),
                "missing {index}"
            );
            let mut repeated = events.clone();
            repeated.insert(index, events[index].clone());
            assert!(
                check_notifications(&s, &repeated).is_err(),
                "duplicate {index}"
            );
        }
    }

    #[test]
    fn wrong_identity_malformed_known_event_and_late_result_refuse() {
        let (s, events) = fixture();
        for index in 0..events.len() {
            let mut wrong = events.clone();
            wrong[index]["params"]["threadId"] = json!("foreign");
            assert!(check_notifications(&s, &wrong).is_err());
            let mut malformed = events.clone();
            malformed[index]["params"] = json!({});
            assert!(check_notifications(&s, &malformed).is_err());
        }
        let mut late = events.clone();
        late.swap(10, 11);
        assert!(check_notifications(&s, &late).is_err());
        let mut wrong = events;
        wrong[0]["params"]["turnId"] = json!("foreign");
        assert!(check_notifications(&s, &wrong).is_err());
    }

    #[test]
    fn optional_item_start_is_unique_and_preterminal() {
        let (scenario, mut events) = fixture();
        let start = json!({"method":"item/started","params":{
            "threadId":scenario.thread_id,"turnId":scenario.turn_id,
            "item":{"type":"commandExecution","id":COMMAND_CALL,
                "command":scenario.guest_command,"cwd":scenario.guest_cwd,
                "status":"inProgress","source":"unifiedExecStartup"}
        }});
        events.insert(8, start.clone());
        check_notifications(&scenario, &events).unwrap();
        let mut duplicated = events.clone();
        duplicated.insert(9, start.clone());
        assert!(check_notifications(&scenario, &duplicated).is_err());
        let mut late = events;
        late.push(start);
        assert!(check_notifications(&scenario, &late).is_err());
    }

    #[test]
    fn successful_or_leaking_secret_read_and_wrong_patch_refuse() {
        let (s, events) = fixture();
        for (index, pointer, value) in [
            (
                4,
                "/params/item/output",
                json!("Process exited with code 0"),
            ),
            (7, "/params/item/output", json!(s.controller_canary_value)),
            (9, "/params/item/exitCode", json!(0)),
            (9, "/params/item/status", json!("completed")),
            (8, "/params/item/command", json!("different")),
            (10, "/params/item/changes/0/path", json!("/elsewhere")),
            (10, "/params/item/changes/0/diff", json!("different")),
            (10, "/params/item/changes/0/diff", json!("+candidate\n")),
            (11, "/params/turn/status", json!("failed")),
            (11, "/params/turn/error", json!({"message":"failed"})),
        ] {
            let mut changed = events.clone();
            *changed[index].pointer_mut(pointer).unwrap() = value;
            assert!(
                check_notifications(&s, &changed).is_err(),
                "{index} {pointer}"
            );
        }
    }

    #[test]
    fn bounded_input_and_unknown_tool_outcomes_refuse() {
        let (s, events) = fixture();
        assert!(check_notifications(&s, &vec![events[0].clone(); MAX_EVENTS + 1]).is_err());
        let mut huge = events.clone();
        huge[0]["padding"] = json!("x".repeat(MAX_EVENT_BYTES));
        assert!(check_notifications(&s, &huge).is_err());
        let mut cumulative = vec![
            json!({"method":"rawResponseItem/completed","params":{
                "threadId":s.thread_id,"turnId":s.turn_id,
                "item":{"type":"message","content":"x".repeat(4096)}
            }});
            300
        ];
        cumulative.extend(events.clone());
        assert!(
            check_notifications(&s, &cumulative)
                .unwrap_err()
                .to_string()
                .contains("notification bytes exceed bound")
        );
        let mut deep = json!(null);
        for _ in 0..34 {
            deep = json!([deep]);
        }
        let mut nested = events.clone();
        nested[0]["nested"] = deep;
        assert!(check_notifications(&s, &nested).is_err());
        let mut unknown = events.clone();
        unknown[0]["params"]["item"]["call_id"] = json!("unplanned");
        assert!(check_notifications(&s, &unknown).is_err());
        let mut unclassified = events.clone();
        unclassified.insert(
            0,
            json!({"method":"unexpectedExecution/started","params":{"threadId":s.thread_id,"turnId":s.turn_id}}),
        );
        assert!(check_notifications(&s, &unclassified).is_err());
        let mut hidden = events;
        hidden.insert(
            0,
            json!({"method":"warning","params":{"message":s.controller_canary_value}}),
        );
        assert!(check_notifications(&s, &hidden).is_err());
    }

    #[test]
    fn unplanned_or_malformed_invocation_and_explicit_error_refuse() {
        let (s, events) = fixture();
        let call = events[1].clone();
        let mut duplicate = events.clone();
        duplicate.insert(1, call.clone());
        assert!(check_notifications(&s, &duplicate).is_err());
        for (field, value) in [
            ("call_id", json!("extra-command")),
            ("name", json!("different-tool")),
            ("arguments", json!("not-json")),
        ] {
            let mut bad_call = call.clone();
            bad_call["params"]["item"][field] = value;
            let mut bad = events.clone();
            bad[1] = bad_call;
            assert!(check_notifications(&s, &bad).is_err());
        }
        for arguments in [
            json!({"cmd":"pwd; rg --version","workdir":"/workspace","login":false,"yield_time_ms":10000}),
            json!({"environment_id":"local","cmd":"pwd; rg --version","workdir":"/workspace","login":false,"yield_time_ms":10000}),
            json!({"environment_id":"ryeos-external-candidate","cmd":"different","workdir":"/workspace","login":false,"yield_time_ms":10000}),
            json!({"environment_id":"ryeos-external-candidate","cmd":"pwd; rg --version","workdir":"/workspace","login":true,"yield_time_ms":10000}),
        ] {
            let mut bad = events.clone();
            bad[1]["params"]["item"]["arguments"] = json!(arguments.to_string());
            assert!(check_notifications(&s, &bad).is_err());
        }
        let mut bad_patch = events.clone();
        bad_patch[2]["params"]["item"]["input"] = json!("different patch");
        assert!(check_notifications(&s, &bad_patch).is_err());
        let mut error = events;
        error.insert(
            0,
            json!({"method":"error","params":{"threadId":s.thread_id,"turnId":s.turn_id,
            "willRetry":true,"error":{"message":"fixture error"}}}),
        );
        assert!(check_notifications(&s, &error).is_err());
    }

    #[test]
    fn raw_command_success_cannot_be_disguised_by_denial_text() {
        let (s, mut events) = fixture();
        events[5]["params"]["item"]["output"] = json!(format!(
            "Output:\nProcess exited with code 0\n{}",
            s.expected_command_output
        ));
        assert!(check_notifications(&s, &events).is_err());
        let (s, mut events) = fixture();
        events[7]["params"]["item"]["output"] = json!(format!(
            "Process exited with code 0\nOutput:\n{}",
            s.secret_read_denial
        ));
        assert!(check_notifications(&s, &events).is_err());
        assert!(raw_command_exit("Output:\nProcess exited with code 1").is_err());
        assert!(
            raw_command_exit("Process exited with code 0\nProcess exited with code 1\nOutput:\nx")
                .is_err()
        );
    }
}
