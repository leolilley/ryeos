//! Finite interpretation of the actual admitted Q Worker's conversation.
//!
//! These checks do not establish native death, provider settlement, writer
//! exclusion or an issued qualification. Those facts have separate owners.
use anyhow::{Context as _, Result, bail, ensure};
use ryeos_state::objects::{QualificationWorkerTranscriptStream, canonical_value_digest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::consumer_record::ConsumerInputRecord;
use crate::consumer_worker_transcript::VerifiedConsumerWorkerTranscript;

const MAX_EVENTS: usize = 512;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_TOTAL_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QCommandExpectation {
    pub schema: String,
    pub environment_id: String,
    pub command: String,
    pub command_presentation: String,
    pub cwd: String,
    pub output: String,
    pub exit_code: i32,
}

impl QCommandExpectation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == "ryeos.codex.q_command.v1",
            "unsupported Q command expectation"
        );
        ensure!(
            self.environment_id == "ryeos-external-candidate",
            "Q command must name the external Codex environment"
        );
        for value in [&self.command, &self.command_presentation] {
            ensure!(
                !value.is_empty() && value.len() <= 4096 && !value.contains('\0'),
                "Q command expectation exceeds its text bound"
            );
        }
        ensure!(
            !self.output.is_empty() && self.output.len() <= 16384 && !self.output.contains('\0'),
            "Q output expectation exceeds its text bound"
        );
        ensure!(
            self.cwd == "/workspace",
            "Q command cwd differs from the admitted external workspace"
        );
        ensure!(
            self.exit_code == 0,
            "Q positive command expectation must require exit zero"
        );
        Ok(())
    }
}

/// Checked app-server semantics, not a qualification result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckedQConversation {
    pub thread_id: String,
    pub turn_id: String,
    pub command_call_id: String,
    pub notification_count: usize,
}

/// Interpret the transcript returned by the protected bounded reader. That
/// reader authenticates the complete chunk/batch stream; this function joins
/// product meaning to its retained frames and original signed expectations.
/// A caller-constructed public transcript is not an authority or qualification.
pub fn check_q_conversation(
    original: &ConsumerInputRecord,
    transcript: &VerifiedConsumerWorkerTranscript,
) -> Result<CheckedQConversation> {
    crate::consumer_worker_transcript::validate_reference_for_record(
        &transcript.reference,
        original,
    )?;
    let configuration = original.scripted_configuration()?;
    let expected = &configuration.qualification_worker;
    expected.validate()?;
    let request = original
        .purpose
        .policy_source
        .policy
        .producer_scenarios
        .get(&original.coordinate.scenario_id)
        .and_then(|scenario| scenario.consumer_worker_request.as_ref())
        .context("Q semantic check has no original signed conversation")?;
    request.validate()?;
    require_route(&request.session_setup, "session.start")?;
    require_route(&request.body, "turn.start")?;
    ensure!(
        request
            .session_setup
            .pointer("/payload/experimentalRawEvents")
            == Some(&Value::Bool(true)),
        "Q signed setup did not opt into actual raw tool observations"
    );
    let manifest = &transcript.reference.manifest;
    ensure!(
        canonical_value_digest(&transcript.stream.setup_final_response)?
            == manifest.setup_final_response_digest
            && canonical_value_digest(&transcript.stream.final_response)?
                == manifest.final_response_digest,
        "Q semantic frames changed their retained digests"
    );
    ensure!(
        transcript.stream.delta_response.is_some() == manifest.delta_digest.is_some(),
        "Q semantic Delta presence changed"
    );
    if let Some(delta) = &transcript.stream.delta_response {
        ensure!(
            Some(canonical_value_digest(delta)?) == manifest.delta_digest,
            "Q semantic Delta changed its retained digest"
        );
    }
    let (thread, start, completion) = check_lifecycle(&transcript.stream, &manifest.turn_id)?;
    ensure!(
        canonical_value_digest(&start)? == manifest.turn_start_digest
            && canonical_value_digest(&completion)? == manifest.turn_completion_digest,
        "Q lifecycle differs from the journaled turn"
    );
    let notifications = collect_notifications(&transcript.stream)?;
    check_notifications(expected, &thread, &manifest.turn_id, &notifications)
}

fn require_route(request: &Value, route: &str) -> Result<()> {
    let object = request.as_object().context("Q request is not an object")?;
    ensure!(
        object.len() == 2
            && object.get("route_id").and_then(Value::as_str) == Some(route)
            && object.get("payload").is_some_and(Value::is_object),
        "Q request changed its finite route envelope"
    );
    Ok(())
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Q observation lacks textual {key}"))
}

fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 256
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "Q observation has an invalid identifier"
    );
    Ok(())
}

fn result(body: &Value) -> Result<&Value> {
    let response = body
        .get("response")
        .filter(|value| value.is_object())
        .context("Q Final has no app-server response")?;
    ensure!(
        response.get("error").is_none_or(Value::is_null),
        "Q app-server response failed"
    );
    response
        .get("result")
        .filter(|value| value.is_object())
        .context("Q app-server response has no result")
}

fn observations(body: &Value) -> Result<&Vec<Value>> {
    body.get("session_observations")
        .and_then(Value::as_array)
        .context("Q frame has no observations")
}

fn check_lifecycle(
    stream: &QualificationWorkerTranscriptStream,
    turn: &str,
) -> Result<(String, Value, Value)> {
    identifier(turn)?;
    let setup = result(&stream.setup_final_response)?;
    let thread = setup.get("thread").context("Q setup has no thread")?;
    let thread = text(thread, "id")?.to_owned();
    identifier(&thread)?;
    ensure!(
        observations(&stream.setup_final_response)?
            == &vec![json!({"kind":"remote_thread", "id":thread})],
        "Q setup binding differs from the actual app-server thread"
    );
    let start = json!({"kind":"state","expected":"idle","next":"turn_running","turn_id":turn});
    let completion =
        json!({"kind":"state","expected":"turn_running","next":"idle","completed_turn_id":turn});
    let final_turn = result(&stream.final_response)?
        .get("turn")
        .context("Q turn Final has no turn")?;
    ensure!(
        text(final_turn, "id")? == turn
            && matches!(text(final_turn, "status")?, "inProgress" | "completed")
            && final_turn.get("error").is_none_or(Value::is_null),
        "Q turn Final changed its actual turn or reported an error"
    );
    ensure!(
        observations(&stream.final_response)? == &vec![start.clone()],
        "Q turn Final lacks its exact start observation"
    );
    if let Some(delta) = &stream.delta_response {
        ensure!(
            observations(delta)? == &vec![start.clone()],
            "Q turn Delta changed its request-correlated start"
        );
    }
    let mut completions = 0;
    for batch in &stream.batches {
        for observation in observations(batch)? {
            if observation.get("kind").and_then(Value::as_str) == Some("state") {
                ensure!(
                    observation == &completion || observation == &start,
                    "Q pushed State is not this exact turn"
                );
                if observation == &completion {
                    completions += 1;
                }
            }
        }
    }
    ensure!(
        completions == 1,
        "Q transcript lacks exactly one completed-turn observation"
    );
    Ok((thread, start, completion))
}

fn collect_notifications(stream: &QualificationWorkerTranscriptStream) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    let mut total = 0usize;
    for batch in &stream.batches {
        let events = batch
            .get("events")
            .and_then(Value::as_array)
            .context("Q batch has no events")?;
        for event in events {
            let Some(raw) = event.pointer("/payload/notification_json") else {
                continue;
            };
            let raw = raw
                .as_str()
                .context("Q raw notification projection is not text")?;
            total = total
                .checked_add(raw.len())
                .context("Q notification bytes overflow")?;
            ensure!(
                raw.len() <= MAX_EVENT_BYTES
                    && total <= MAX_TOTAL_BYTES
                    && result.len() < MAX_EVENTS,
                "Q raw notifications exceed finite bounds"
            );
            let message: Value =
                serde_json::from_str(raw).context("decode actual Q notification")?;
            ensure!(
                lillux::canonical_json(&message)? == raw,
                "Q notification projection is not canonical"
            );
            result.push(message);
        }
    }
    ensure!(
        !result.is_empty(),
        "Q transcript has no actual raw notifications"
    );
    Ok(result)
}

fn same_turn(params: &Value, thread: &str, turn: &str) -> Result<()> {
    ensure!(
        text(params, "threadId")? == thread && text(params, "turnId")? == turn,
        "Q notification changed thread or turn"
    );
    Ok(())
}

fn check_raw_output(raw: &str, expected: &QCommandExpectation) -> Result<()> {
    let (header, output) = raw
        .split_once("\nOutput:\n")
        .context("Q raw command result has no output boundary")?;
    let exits = header
        .lines()
        .filter_map(|line| line.strip_prefix("Process exited with code "))
        .collect::<Vec<_>>();
    ensure!(
        exits.len() == 1
            && exits[0].parse::<i32>()? == expected.exit_code
            && !header.contains("Process running with session ID")
            && output == expected.output,
        "Q raw command result is nonterminal or differs from expected exit/output"
    );
    ensure!(
        header.lines().all(|line| line.starts_with("Chunk ID: ")
            || line.starts_with("Wall time: ")
            || line.starts_with("Process exited with code ")
            || line.starts_with("Original token count: ")),
        "Q raw command result contains an unknown header"
    );
    Ok(())
}

fn check_command_item(
    item: &Value,
    expected: &QCommandExpectation,
    completed: bool,
) -> Result<String> {
    let id = text(item, "id")?;
    identifier(id)?;
    ensure!(
        text(item, "command")? == expected.command_presentation
            && text(item, "cwd")? == expected.cwd,
        "Q command item changed presentation or cwd"
    );
    if completed {
        ensure!(
            text(item, "status")? == "completed"
                && item.get("exitCode").and_then(Value::as_i64)
                    == Some(i64::from(expected.exit_code))
                && text(item, "aggregatedOutput")? == expected.output,
            "Q completed command differs from expected success"
        );
    } else {
        ensure!(
            text(item, "status")? == "inProgress",
            "Q started command has an invalid status"
        );
    }
    Ok(id.to_owned())
}

fn check_notifications(
    expected: &QCommandExpectation,
    thread: &str,
    turn: &str,
    notifications: &[Value],
) -> Result<CheckedQConversation> {
    expected.validate()?;
    ensure!(
        !notifications.is_empty() && notifications.len() <= MAX_EVENTS,
        "Q notification count exceeds bound"
    );
    let mut call = None::<String>;
    let mut output_seen = false;
    let mut started = None::<String>;
    let mut completed = None::<String>;
    let mut terminal = false;
    let mut turn_started = false;
    let mut thread_started = false;
    let mut response_ids = BTreeSet::new();
    let mut message_item_ids = BTreeSet::new();
    let mut output_delta = String::new();
    let mut output_delta_id = None::<String>;
    let mut total = 0usize;
    for message in notifications {
        let bytes = lillux::canonical_json(message)?.len();
        total = total
            .checked_add(bytes)
            .context("Q notification bytes overflow")?;
        ensure!(
            bytes <= MAX_EVENT_BYTES && total <= MAX_TOTAL_BYTES,
            "Q notification bytes exceed bound"
        );
        ensure!(
            message.is_object() && message.get("id").is_none(),
            "Q message is not a notification"
        );
        let method = text(message, "method")?;
        ensure!(
            !terminal || method == "thread/status/changed",
            "Q observation followed terminal turn"
        );
        let params = message
            .get("params")
            .filter(|p| p.is_object())
            .context("Q notification has no parameters")?;
        match method {
            "rawResponseItem/completed" => {
                same_turn(params, thread, turn)?;
                let item = params.get("item").context("Q raw item is absent")?;
                match text(item, "type")? {
                    "function_call" => {
                        ensure!(
                            call.is_none() && text(item, "name")? == "exec_command",
                            "Q has an additional or unexpected tool call"
                        );
                        let id = text(item, "call_id")?;
                        identifier(id)?;
                        let arguments: Value = serde_json::from_str(text(item, "arguments")?)?;
                        ensure!(
                            arguments
                                == json!({"environment_id":expected.environment_id,"cmd":expected.command,
                            "workdir":expected.cwd,"login":false,"yield_time_ms":10000}),
                            "Q raw command arguments changed the exact remote invocation"
                        );
                        call = Some(id.to_owned());
                    }
                    "function_call_output" => {
                        ensure!(
                            !output_seen && Some(text(item, "call_id")?) == call.as_deref(),
                            "Q raw output has no unique matching invocation"
                        );
                        check_raw_output(text(item, "output")?, expected)?;
                        output_seen = true;
                    }
                    "message" | "reasoning" => (),
                    other => bail!("unexpected Q raw item type {other}"),
                }
            }
            "item/started" | "item/completed" => {
                same_turn(params, thread, turn)?;
                let item = params.get("item").context("Q item is absent")?;
                match text(item, "type")? {
                    "commandExecution" => {
                        let complete = method == "item/completed";
                        let id = check_command_item(item, expected, complete)?;
                        let destination = if complete {
                            &mut completed
                        } else {
                            &mut started
                        };
                        ensure!(
                            destination.replace(id).is_none(),
                            "Q command item is repeated"
                        );
                    }
                    "userMessage" | "agentMessage" | "reasoning" | "plan" => {
                        let id = text(item, "id")?;
                        identifier(id)?;
                        message_item_ids.insert(id.to_owned());
                    }
                    other => bail!("unexpected Q effect item {other}"),
                }
            }
            "turn/started" | "turn/completed" => {
                ensure!(
                    text(params, "threadId")? == thread,
                    "Q turn notification changed thread"
                );
                let observed = params
                    .get("turn")
                    .context("Q turn notification lacks turn")?;
                ensure!(
                    text(observed, "id")? == turn
                        && observed.get("error").is_none_or(Value::is_null),
                    "Q turn notification changed turn or failed"
                );
                if method == "turn/completed" {
                    ensure!(
                        text(observed, "status")? == "completed"
                            && output_seen
                            && call.is_some()
                            && completed.as_ref() == call.as_ref(),
                        "Q turn completed without its expected command evidence"
                    );
                    terminal = true;
                } else {
                    ensure!(
                        !turn_started && text(observed, "status")? == "inProgress",
                        "Q turn did not start uniquely and successfully"
                    );
                    turn_started = true;
                }
            }
            "thread/started" => {
                ensure!(!thread_started, "Q thread setup notification was repeated");
                thread_started = true;
                ensure!(
                    text(
                        params
                            .get("thread")
                            .context("Q thread notification has no thread")?,
                        "id"
                    )? == thread,
                    "Q thread notification changed setup binding"
                );
            }
            "thread/status/changed" => {
                ensure!(
                    text(params, "threadId")? == thread,
                    "Q status changed thread"
                );
                let status = params.get("status").context("Q thread status is absent")?;
                let kind = text(status, "type")?;
                ensure!(
                    matches!(kind, "idle" | "active") && (!terminal || kind == "idle"),
                    "Q thread entered an unsuccessful or postterminal active state"
                );
            }
            "item/agentMessage/delta" => {
                same_turn(params, thread, turn)?;
                let id = text(params, "itemId")?;
                identifier(id)?;
                message_item_ids.insert(id.to_owned());
                text(params, "delta")?;
            }
            "item/commandExecution/outputDelta" => {
                same_turn(params, thread, turn)?;
                let id = text(params, "itemId")?;
                identifier(id)?;
                ensure!(
                    output_delta_id.as_deref().is_none_or(|prior| prior == id),
                    "Q output deltas name multiple commands"
                );
                output_delta_id = Some(id.into());
                let delta = text(params, "delta")?;
                ensure!(
                    output_delta
                        .len()
                        .checked_add(delta.len())
                        .is_some_and(|len| len <= expected.output.len()),
                    "Q output deltas exceed expected output"
                );
                output_delta.push_str(delta);
            }
            "turn/diff/updated" => {
                same_turn(params, thread, turn)?;
                ensure!(
                    text(params, "diff")?.is_empty(),
                    "Q read-only command produced a file diff"
                );
            }
            "rawResponse/completed" => {
                same_turn(params, thread, turn)?;
                let id = text(params, "responseId")?;
                identifier(id)?;
                ensure!(
                    response_ids.insert(id.to_owned()),
                    "Q raw model response was repeated"
                );
            }
            other => bail!("unclassified Q notification {other}"),
        }
    }
    let call = call.context("Q command invocation is absent")?;
    ensure!(
        !message_item_ids.contains(&call),
        "Q command identity also appeared as an ancillary message"
    );
    ensure!(
        output_delta_id
            .as_deref()
            .is_none_or(|id| id == call.as_str() && output_delta == expected.output),
        "Q output deltas differ from the completed command"
    );
    ensure!(
        terminal
            && completed.as_deref() == Some(call.as_str())
            && started.as_deref().is_none_or(|id| id == call),
        "Q conversation lacks a consistent successful terminal"
    );
    Ok(CheckedQConversation {
        thread_id: thread.into(),
        turn_id: turn.into(),
        command_call_id: call,
        notification_count: notifications.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::objects::QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA;

    fn expectation() -> QCommandExpectation {
        QCommandExpectation {
            schema: "ryeos.codex.q_command.v1".into(),
            environment_id: "ryeos-external-candidate".into(),
            command: "printf probe".into(),
            command_presentation: "zsh -c 'printf probe'".into(),
            cwd: "/workspace".into(),
            output: "probe".into(),
            exit_code: 0,
        }
    }

    fn notification(method: &str, extra: Value) -> Value {
        let mut params = json!({"threadId":"thread-one","turnId":"turn-one"});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"method":method,"params":params})
    }

    fn notifications(expected: &QCommandExpectation) -> Vec<Value> {
        vec![
            notification(
                "rawResponseItem/completed",
                json!({"item":{
                    "type":"function_call","name":"exec_command","call_id":"learned-call",
                    "arguments":serde_json::to_string(&json!({"environment_id":expected.environment_id,
                        "cmd":expected.command,"workdir":expected.cwd,"login":false,"yield_time_ms":10000})).unwrap()
                }}),
            ),
            notification(
                "item/started",
                json!({"item":{
                    "type":"commandExecution","id":"learned-call","command":expected.command_presentation,
                    "cwd":expected.cwd,"status":"inProgress"
                }}),
            ),
            notification(
                "item/commandExecution/outputDelta",
                json!({"itemId":"learned-call","delta":expected.output}),
            ),
            notification(
                "item/completed",
                json!({"item":{
                    "type":"commandExecution","id":"learned-call","command":expected.command_presentation,
                    "cwd":expected.cwd,"status":"completed","exitCode":0,"aggregatedOutput":expected.output
                }}),
            ),
            notification(
                "rawResponseItem/completed",
                json!({"item":{
                    "type":"function_call_output","call_id":"learned-call",
                    "output":format!("Chunk ID: 123\nWall time: 0.0010 seconds\nProcess exited with code 0\nOriginal token count: 2\nOutput:\n{}",expected.output)
                }}),
            ),
            notification(
                "rawResponse/completed",
                json!({"responseId":"response-one"}),
            ),
            notification(
                "turn/completed",
                json!({"turn":{"id":"turn-one","status":"completed","error":null}}),
            ),
            json!({"method":"thread/status/changed","params":{"threadId":"thread-one","status":{"type":"idle"}}}),
        ]
    }

    fn stream() -> QualificationWorkerTranscriptStream {
        QualificationWorkerTranscriptStream {
            schema: QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA.into(),
            setup_final_response: json!({"response":{"result":{"thread":{"id":"thread-one"}}},
                "session_observations":[{"kind":"remote_thread","id":"thread-one"}]}),
            final_response: json!({"response":{"result":{"turn":{"id":"turn-one","status":"inProgress"}}},
                "session_observations":[{"kind":"state","expected":"idle","next":"turn_running","turn_id":"turn-one"}]}),
            delta_response: Some(json!({"events":[],"session_observations":[{
                "kind":"state","expected":"idle","next":"turn_running","turn_id":"turn-one"
            }]})),
            batches: vec![json!({"events":[],"session_observations":[{
                "kind":"state","expected":"turn_running","next":"idle","completed_turn_id":"turn-one"
            }]})],
        }
    }

    #[test]
    fn actual_ids_and_terminal_outputs_are_correlated_without_scripted_call_ids() {
        let expected = expectation();
        let events = notifications(&expected);
        let checked = check_notifications(&expected, "thread-one", "turn-one", &events).unwrap();
        assert_eq!(checked.command_call_id, "learned-call");
        // App-server item completion and raw model-visible result are separate
        // projections; either relative order is acceptable for the same call.
        let mut reversed_results = events.clone();
        reversed_results.swap(3, 4);
        check_notifications(&expected, "thread-one", "turn-one", &reversed_results).unwrap();
        for index in [0, 3, 4, 6] {
            let mut missing = events.clone();
            missing.remove(index);
            assert!(
                check_notifications(&expected, "thread-one", "turn-one", &missing).is_err(),
                "accepted missing {index}"
            );
        }
    }

    #[test]
    fn stale_swapped_extra_effects_and_contradictory_results_refuse() {
        let expected = expectation();
        let events = notifications(&expected);
        for (index, pointer, replacement) in [
            (0, "/params/threadId", json!("stale-thread")),
            (3, "/params/turnId", json!("stale-turn")),
            (3, "/params/item/id", json!("other-call")),
            (3, "/params/item/command", json!("different command")),
            (3, "/params/item/cwd", json!("/elsewhere")),
            (
                3,
                "/params/item/aggregatedOutput",
                json!("different output"),
            ),
            (3, "/params/item/exitCode", json!(1)),
            (3, "/params/item/type", json!("fileChange")),
            (4, "/params/item/call_id", json!("other-call")),
            (6, "/params/turn/status", json!("failed")),
            (6, "/params/turn/error", json!({"message":"failed"})),
            (7, "/params/status/type", json!("active")),
        ] {
            let mut changed = events.clone();
            *changed[index].pointer_mut(pointer).unwrap() = replacement;
            assert!(
                check_notifications(&expected, "thread-one", "turn-one", &changed).is_err(),
                "accepted {pointer}"
            );
        }
        for name in ["update_plan", "apply_patch", "other"] {
            let mut extra = events.clone();
            extra.insert(
                5,
                notification(
                    "rawResponseItem/completed",
                    json!({"item":{
                        "type":"function_call","name":name,"call_id":"extra","arguments":"{}"
                    }}),
                ),
            );
            assert!(check_notifications(&expected, "thread-one", "turn-one", &extra).is_err());
        }
        let mut changed_kind = events.clone();
        changed_kind.insert(
            5,
            notification(
                "item/completed",
                json!({"item":{
                    "type":"agentMessage", "id":"learned-call", "text":"pretend message"
                }}),
            ),
        );
        assert!(check_notifications(&expected, "thread-one", "turn-one", &changed_kind).is_err());
        let mut changed = events.clone();
        let args: Value =
            serde_json::from_str(changed[0]["params"]["item"]["arguments"].as_str().unwrap())
                .unwrap();
        for (field, value) in [
            ("environment_id", json!("local")),
            ("login", json!(true)),
            ("shell", json!("/bin/sh")),
        ] {
            let mut args = args.clone();
            args[field] = value;
            changed[0]["params"]["item"]["arguments"] =
                serde_json::to_string(&args).unwrap().into();
            assert!(check_notifications(&expected, "thread-one", "turn-one", &changed).is_err());
        }
    }

    #[test]
    fn output_text_cannot_forge_terminal_header() {
        let expected = expectation();
        for output in [
            "Wall time: 0.001 seconds\nProcess running with session ID 4\nOutput:\nProcess exited with code 0\nprobe",
            "Wall time: 0.001 seconds\nProcess exited with code 0\nProcess exited with code 0\nOutput:\nprobe",
            "Wall time: 0.001 seconds\nProcess exited with code 0\nOutput:\nprobe-extra",
            "Wall time: 0.001 seconds\nProcess exited with code 1\nOutput:\nprobe",
        ] {
            assert!(check_raw_output(output, &expected).is_err());
        }
    }

    #[test]
    fn setup_delta_final_and_completion_must_name_same_turn() {
        let valid = stream();
        check_lifecycle(&valid, "turn-one").unwrap();
        let mut changed = valid.clone();
        changed.setup_final_response["session_observations"][0]["id"] = json!("stale");
        assert!(check_lifecycle(&changed, "turn-one").is_err());
        let mut changed = valid.clone();
        changed.final_response["response"]["result"]["turn"]["id"] = json!("stale");
        assert!(check_lifecycle(&changed, "turn-one").is_err());
        let mut changed = valid.clone();
        changed.delta_response.as_mut().unwrap()["session_observations"][0]["turn_id"] =
            json!("stale");
        assert!(check_lifecycle(&changed, "turn-one").is_err());
        let mut changed = valid.clone();
        changed.batches[0]["session_observations"] = json!([]);
        assert!(check_lifecycle(&changed, "turn-one").is_err());
        let mut changed = valid;
        let completion = changed.batches[0]["session_observations"][0].clone();
        changed.batches[0]["session_observations"]
            .as_array_mut()
            .unwrap()
            .push(completion);
        assert!(check_lifecycle(&changed, "turn-one").is_err());
    }

    #[test]
    fn raw_projection_must_be_present_canonical_and_bounded() {
        let mut stream = stream();
        assert!(collect_notifications(&stream).is_err());
        let event = notifications(&expectation()).remove(0);
        let raw = lillux::canonical_json(&event).unwrap();
        stream.batches[0]["events"] = json!([{"payload":{"notification_json":raw}}]);
        assert_eq!(collect_notifications(&stream).unwrap(), vec![event]);
        stream.batches[0]["events"][0]["payload"]["notification_json"] = json!(format!(" {raw}"));
        assert!(collect_notifications(&stream).is_err());
        stream.batches[0]["events"][0]["payload"]["notification_json"] =
            json!("x".repeat(MAX_EVENT_BYTES + 1));
        assert!(collect_notifications(&stream).is_err());
        stream.batches[0]["events"] = json!(vec![
            json!({"payload":{"notification_json":raw}});
            MAX_EVENTS + 1
        ]);
        assert!(collect_notifications(&stream).is_err());
    }

    #[test]
    fn expectation_is_closed_and_current_positive_only() {
        let valid = expectation();
        valid.validate().unwrap();
        let mut wire = serde_json::to_value(&valid).unwrap();
        wire["extra"] = json!(true);
        assert!(serde_json::from_value::<QCommandExpectation>(wire).is_err());
        for (key, value) in [
            ("schema", json!("ryeos.codex.q_command.v0")),
            ("exit_code", json!(1)),
            ("environment_id", json!("local")),
            ("cwd", json!("/tmp")),
            ("command", json!("x".repeat(4097))),
        ] {
            let mut wire = serde_json::to_value(&valid).unwrap();
            wire[key] = value;
            let invalid: QCommandExpectation = serde_json::from_value(wire).unwrap();
            assert!(invalid.validate().is_err());
        }
    }
}
