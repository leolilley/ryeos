//! Pure exact routed-guest transcript/capture agreement. No host access,
//! qualification publication or authority construction.
use crate::routing_observation::{MAX_EVENT_BYTES, MAX_TOTAL_BYTES};
use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub struct GuestScenario<'a> {
    pub request_sha256: &'a str,
    pub shell: &'a str,
    pub guest_cwd_uri: &'a str,
    pub guest_command: &'a str,
    pub secret_read_command: &'a str,
    pub expected_command_output: &'a str,
    pub controller_canary_value: &'a str,
    pub secret_read_denial: &'a str,
    pub candidate_uri: &'a str,
    pub candidate_relative_path: &'a str,
    pub candidate_content: &'a [u8],
}

fn json_lines(encoded: &Value, forwarded_prefix: Option<usize>) -> Result<Vec<Value>> {
    let encoded = encoded.as_str().context("transcript absent")?;
    ensure!(encoded.len() <= 1_400_000, "encoded transcript bound");
    let bytes = STANDARD.decode(encoded)?;
    ensure!(
        bytes.len() <= MAX_TOTAL_BYTES,
        "observed transcript exceeds bound"
    );
    let bytes = bytes
        .get(..forwarded_prefix.unwrap_or(bytes.len()))
        .context("forwarded prefix exceeds observed bytes")?;
    ensure!(
        bytes.len() <= MAX_TOTAL_BYTES && bytes.last() == Some(&b'\n'),
        "incomplete transcript"
    );
    let lines = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    ensure!(
        lines.len() <= 1024 && lines.iter().all(|line| line.len() <= MAX_EVENT_BYTES),
        "guest frame bound"
    );
    lines
        .into_iter()
        .map(|line| Ok(serde_json::from_slice(line)?))
        .collect()
}

pub fn check_guest_observation(observation: &Value, scenario: &GuestScenario<'_>) -> Result<()> {
    let request_hash = scenario.request_sha256;
    let secret_command = scenario.secret_read_command;
    let expected_output = scenario.expected_command_output;
    ensure!(
        observation["schema"] == "test.routed_guest_observation.v1"
            && observation["request_sha256"] == request_hash,
        "guest observation changed input identity"
    );
    ensure!(
        observation["namespace_exit"] == "Signal(9)",
        "guest namespace was not exactly settled"
    );
    let applied: lillux::LinuxSandboxAppliedLaunchReceipt = serde_json::from_value(
        observation
            .get("applied_receipt")
            .context("guest applied-launch receipt absent")?
            .clone(),
    )?;
    ensure!(
        applied.owned_child_pid > 0
            && applied.namespace_pid == 1
            && applied.effective_uid == 1
            && applied.effective_gid == 1
            && applied.no_new_privs
            && applied.seccomp_mode == 2,
        "guest did not report native applied-launch controls"
    );
    let input = json_lines(&observation["guest_input_base64"], None)?;
    let forwarded = usize::try_from(
        observation["forwarded_output_bytes"]
            .as_u64()
            .context("forwarded output count absent")?,
    )?;
    // A post-settlement tail is diagnostic evidence, not a response delivered
    // to Codex. All agreement checks use the exact forwarded prefix only.
    let output = json_lines(&observation["guest_output_base64"], Some(forwarded))?;
    ensure!(
        !scenario.candidate_relative_path.is_empty()
            && scenario
                .candidate_relative_path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && scenario.candidate_uri
                == format!(
                    "{}/{}",
                    scenario.guest_cwd_uri.trim_end_matches('/'),
                    scenario.candidate_relative_path
                ),
        "candidate URI does not match the captured relative path"
    );
    let mut processes = BTreeMap::new();
    let mut patch = None;
    let mut requests = BTreeMap::new();
    for message in &input {
        let method = message["method"]
            .as_str()
            .context("guest request method absent")?;
        if method == "initialized" {
            ensure!(
                message.get("id").is_none_or(Value::is_null),
                "initialized has an ID"
            );
            continue;
        }
        let request_id = message.get("id").context("guest request ID absent")?;
        ensure!(
            request_id.is_string() || request_id.is_u64(),
            "guest request ID is not a string or unsigned integer"
        );
        ensure!(
            requests.insert(request_id.to_string(), method).is_none(),
            "duplicate guest request ID"
        );
        match method {
            "initialize" | "environment/info" => {}
            "fs/getMetadata" => {
                let path = message["params"]["path"]
                    .as_str()
                    .context("metadata path absent")?;
                let cwd = scenario.guest_cwd_uri.trim_end_matches('/');
                ensure!(
                    path == scenario.candidate_uri
                        || path == "file:///.git"
                        || [".git", "AGENTS.md", "AGENTS.override.md", ".agents/skills"]
                            .iter()
                            .any(|name| path == format!("{cwd}/{name}")),
                    "unplanned metadata path"
                );
            }
            "fs/readFile" => ensure!(
                patch.is_some() && message["params"]["path"] == scenario.candidate_uri,
                "unplanned or premature guest file read"
            ),
            "process/start" => {
                let params = &message["params"];
                let argv = params["argv"].as_array().context("guest argv absent")?;
                ensure!(
                    argv.len() == 3
                        && argv[0] == scenario.shell
                        && argv[1] == "-c"
                        && params["cwd"] == scenario.guest_cwd_uri,
                    "unexpected guest launch recipe"
                );
                let command = argv[2].as_str().context("guest command absent")?;
                ensure!(
                    command == scenario.guest_command || command == secret_command,
                    "unplanned guest command"
                );
                let id = params["processId"]
                    .as_str()
                    .context("guest process ID absent")?;
                ensure!(
                    processes
                        .insert(id.to_owned(), command.to_owned())
                        .is_none(),
                    "duplicate guest process ID"
                );
            }
            "process/terminate" => ensure!(
                message["params"]["processId"]
                    .as_str()
                    .is_some_and(|id| processes.contains_key(id)),
                "unplanned guest process termination"
            ),
            "fs/writeFile" => {
                ensure!(
                    patch.is_none() && message["params"]["path"] == scenario.candidate_uri,
                    "unexpected guest file write"
                );
                ensure!(
                    STANDARD.decode(
                        message["params"]["dataBase64"]
                            .as_str()
                            .context("patch bytes absent")?
                    )? == scenario.candidate_content,
                    "guest patch content changed"
                );
                patch = Some(message["id"].clone());
            }
            _ => anyhow::bail!("unplanned guest RPC: {method}"),
        }
    }
    ensure!(
        processes.len() == 2 && processes.values().collect::<BTreeSet<_>>().len() == 2,
        "exact guest commands missing"
    );
    let patch_id = patch.context("guest patch write absent")?;
    let mut responses = BTreeSet::new();
    for message in &output {
        if let Some(id) = message.get("id") {
            ensure!(id.is_string() || id.is_u64(), "guest response ID invalid");
            let key = id.to_string();
            let method = requests.get(&key).context("unrequested guest response")?;
            ensure!(responses.insert(key), "duplicate guest response");
            ensure!(
                message.get("result").is_some() != message.get("error").is_some(),
                "guest response has no single outcome"
            );
            match *method {
                "process/start" => {
                    let process_id = input
                        .iter()
                        .find(|request| request.get("id") == Some(id))
                        .and_then(|request| request["params"]["processId"].as_str())
                        .context("guest launch request missing process ID")?;
                    ensure!(
                        message["result"]["processId"] == process_id,
                        "guest launch acknowledgement changed process ID"
                    );
                }
                "process/terminate" => ensure!(
                    message["result"]["running"] == false,
                    "guest termination not settled"
                ),
                "fs/writeFile" => ensure!(
                    id == &patch_id && message.get("result").is_some(),
                    "guest patch write not acknowledged"
                ),
                "initialize" | "environment/info" => {
                    ensure!(message.get("result").is_some(), "guest setup failed")
                }
                "fs/readFile" => ensure!(
                    message["result"]["dataBase64"]
                        .as_str()
                        .is_some_and(|encoded| STANDARD.decode(encoded).ok().as_deref()
                            == Some(scenario.candidate_content)),
                    "candidate file read did not return the exact authored bytes"
                ),
                "fs/getMetadata" => {}
                _ => anyhow::bail!("unexpected guest response method"),
            }
        } else {
            ensure!(
                message["params"]["processId"]
                    .as_str()
                    .is_some_and(|id| processes.contains_key(id)),
                "event for unknown guest process"
            );
        }
    }
    ensure!(
        responses.len() == requests.len() && responses.contains(&patch_id.to_string()),
        "guest request acknowledgement absent"
    );
    let output_text = serde_json::to_string(&output)?;
    ensure!(
        !output_text.contains(scenario.controller_canary_value)
            && !output_text.contains(&STANDARD.encode(scenario.controller_canary_value)),
        "forwarded guest response leaked controller content"
    );
    for (id, command) in processes {
        let mut text = Vec::new();
        let mut exit = None;
        let mut closed = false;
        let mut previous = None;
        for message in &output {
            if message["params"]["processId"] != id {
                continue;
            }
            let method = message["method"]
                .as_str()
                .context("guest event method absent")?;
            let seq = message["params"]["seq"]
                .as_u64()
                .context("guest sequence absent")?;
            ensure!(
                previous.is_none_or(|p| seq > p) && !closed,
                "guest event order changed"
            );
            previous = Some(seq);
            match method {
                "process/output" => text.extend(
                    STANDARD.decode(
                        message["params"]["chunk"]
                            .as_str()
                            .context("guest output absent")?,
                    )?,
                ),
                "process/exited" => {
                    ensure!(exit.is_none(), "duplicate guest exit");
                    exit = Some(
                        message["params"]["exitCode"]
                            .as_i64()
                            .context("guest exit absent")?,
                    );
                }
                "process/closed" => closed = true,
                _ => anyhow::bail!("unexpected guest process event"),
            }
        }
        ensure!(closed, "guest process closure absent");
        let text = std::str::from_utf8(&text)?;
        ensure!(
            !text.contains(scenario.controller_canary_value),
            "guest read leaked controller content"
        );
        if command == secret_command {
            ensure!(
                exit == Some(73) && text.contains(scenario.secret_read_denial),
                "guest canary read was not denied"
            );
        } else {
            ensure!(
                exit == Some(0) && text == expected_output,
                "guest command result changed"
            );
        }
    }
    let file = ryeos_state::objects::ProjectFile {
        blob_hash: lillux::sha256_hex(scenario.candidate_content),
        size: scenario.candidate_content.len() as u64,
        normalized_mode: 0o644,
    };
    let expected = ryeos_state::objects::canonical_value_digest(&file.to_value())?;
    ensure!(
        observation["captured_files"] == json!({(scenario.candidate_relative_path):expected}),
        "settled candidate capture differs"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied_receipt() -> Value {
        serde_json::to_value(lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 42,
            namespace_pid: 1,
            effective_uid: 1,
            effective_gid: 1,
            no_new_privs: true,
            seccomp_mode: 2,
            executable_sha256: [0; 32],
            argv_sha256: [0; 32],
            environment_sha256: [0; 32],
            cwd_sha256: [0; 32],
            post_release_mount_view: lillux::LinuxSandboxMountPreparationCommitments {
                schema: 1,
                mount_count: 0,
                destination_access_sha256: [0; 32],
            },
        })
        .unwrap()
    }

    #[test]
    fn exact_agreement_rejects_substitution_and_unforwarded_ack() {
        let request_hash = "a".repeat(64);
        let scenario = GuestScenario {
            request_sha256: &request_hash,
            shell: "/tools/zsh",
            guest_cwd_uri: "file:///workspace",
            guest_command: "probe",
            secret_read_command: "secret",
            expected_command_output: "expected",
            controller_canary_value: "private-canary",
            secret_read_denial: "denied",
            candidate_uri: "file:///workspace/result",
            candidate_relative_path: "result",
            candidate_content: b"candidate",
        };
        let encode = |frames: &[Value]| {
            frames
                .iter()
                .map(|v| format!("{}\n", serde_json::to_string(v).unwrap()))
                .collect::<String>()
        };
        let input = encode(&[
            json!({"id":1,"method":"process/start","params":{"processId":"p","argv":[scenario.shell,"-c","probe"],"cwd":scenario.guest_cwd_uri}}),
            json!({"id":2,"method":"process/start","params":{"processId":"s","argv":[scenario.shell,"-c","secret"],"cwd":scenario.guest_cwd_uri}}),
            json!({"id":3,"method":"fs/writeFile","params":{"path":scenario.candidate_uri,"dataBase64":STANDARD.encode(scenario.candidate_content)}}),
        ]);
        let mut frames = Vec::new();
        for (request_id, id, text, exit) in [(1, "p", "expected", 0), (2, "s", "denied", 73)] {
            frames.push(json!({"id":request_id,"result":{"processId":id}}));
            frames.extend([
            json!({"method":"process/output","params":{"processId":id,"seq":1,"chunk":STANDARD.encode(text)}}),
            json!({"method":"process/exited","params":{"processId":id,"seq":2,"exitCode":exit}}),
            json!({"method":"process/closed","params":{"processId":id,"seq":3}}),
        ]);
        }
        let before_ack = encode(&frames).len();
        frames.push(json!({"id":3,"result":{}}));
        let output = encode(&frames);
        let file = ryeos_state::objects::ProjectFile {
            blob_hash: lillux::sha256_hex(scenario.candidate_content),
            size: 9,
            normalized_mode: 0o644,
        };
        let hash = ryeos_state::objects::canonical_value_digest(&file.to_value()).unwrap();
        let observation = json!({"schema":"test.routed_guest_observation.v1","request_sha256":request_hash,
        "namespace_exit":"Signal(9)","applied_receipt":applied_receipt(),
        "guest_input_base64":STANDARD.encode(&input),"guest_output_base64":STANDARD.encode(&output),
        "forwarded_output_bytes":output.len(),"captured_files":{"result":hash}});
        check_guest_observation(&observation, &scenario).unwrap();
        let mut changed = observation.clone();
        changed["request_sha256"] = json!("b".repeat(64));
        assert!(check_guest_observation(&changed, &scenario).is_err());
        changed = observation.clone();
        changed["captured_files"]["result"] = json!("b".repeat(64));
        assert!(check_guest_observation(&changed, &scenario).is_err());
        changed = observation.clone();
        changed["applied_receipt"]["namespace_pid"] = json!(2);
        assert!(check_guest_observation(&changed, &scenario).is_err());
        changed = observation.clone();
        changed["applied_receipt"]
            .as_object_mut()
            .unwrap()
            .remove("post_release_mount_view");
        assert!(check_guest_observation(&changed, &scenario).is_err());
        changed = observation.clone();
        changed["applied_receipt"]["effective_uid"] = json!(0);
        assert!(check_guest_observation(&changed, &scenario).is_err());
        changed = observation.clone();
        changed["forwarded_output_bytes"] = json!(before_ack);
        assert!(check_guest_observation(&changed, &scenario).is_err());
        let mut changed = changed;
        changed["forwarded_output_bytes"] = json!(output.len());
        let unexpected = format!(
            "{}{}\n",
            input,
            json!({"id":4,"method":"fs/deleteFile","params":{"path":scenario.candidate_uri}})
        );
        changed["guest_input_base64"] = json!(STANDARD.encode(unexpected));
        assert!(check_guest_observation(&changed, &scenario).is_err());
        let mut changed = observation.clone();
        let contradictory = format!(
            "{}{}\n",
            output,
            json!({"id":3,"error":{"code":-1,"message":"failed"}})
        );
        changed["guest_output_base64"] = json!(STANDARD.encode(&contradictory));
        changed["forwarded_output_bytes"] = json!(contradictory.len());
        assert!(check_guest_observation(&changed, &scenario).is_err());
        let mut changed = observation.clone();
        let unknown = format!(
            "{}{}\n",
            output,
            json!({"method":"process/output","params":{"processId":"unknown","seq":1,"chunk":""}})
        );
        changed["guest_output_base64"] = json!(STANDARD.encode(&unknown));
        changed["forwarded_output_bytes"] = json!(unknown.len());
        assert!(check_guest_observation(&changed, &scenario).is_err());
        let mut wrong_uri = scenario;
        wrong_uri.candidate_uri = "file:///workspace/another";
        assert!(check_guest_observation(&observation, &wrong_uri).is_err());
    }
    #[test]
    fn guest_agreement_uses_only_complete_forwarded_prefix() {
        let first = b"{\"id\":1,\"result\":{}}\n";
        let tail = b"{\"id\":2,\"result\":{}}\n";
        let all = [first.as_slice(), tail.as_slice()].concat();
        let encoded = json!(STANDARD.encode(&all));
        let messages = json_lines(&encoded, Some(first.len())).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["id"], 1);
        assert!(json_lines(&encoded, Some(first.len() - 1)).is_err());
        assert!(json_lines(&encoded, Some(all.len() + 1)).is_err());
    }

    #[test]
    fn candidate_read_response_must_match_the_frozen_content() {
        let request_hash = "a".repeat(64);
        let scenario = GuestScenario {
            request_sha256: &request_hash,
            shell: "/tools/zsh",
            guest_cwd_uri: "file:///workspace",
            guest_command: "probe",
            secret_read_command: "secret",
            expected_command_output: "expected",
            controller_canary_value: "private-canary-value",
            secret_read_denial: "denied",
            candidate_uri: "file:///workspace/result",
            candidate_relative_path: "result",
            candidate_content: b"candidate",
        };
        let encode = |frames: &[Value]| {
            STANDARD.encode(
                frames
                    .iter()
                    .map(|frame| format!("{}\n", serde_json::to_string(frame).unwrap()))
                    .collect::<String>(),
            )
        };
        let input = [
            json!({"id":1,"method":"process/start","params":{"processId":"p","argv":[scenario.shell,"-c","probe"],"cwd":scenario.guest_cwd_uri}}),
            json!({"id":2,"method":"process/start","params":{"processId":"s","argv":[scenario.shell,"-c","secret"],"cwd":scenario.guest_cwd_uri}}),
            json!({"id":3,"method":"fs/writeFile","params":{"path":scenario.candidate_uri,"dataBase64":STANDARD.encode(scenario.candidate_content)}}),
            json!({"id":4,"method":"fs/readFile","params":{"path":scenario.candidate_uri}}),
        ];
        let mut output = vec![
            json!({"id":1,"result":{"processId":"p"}}),
            json!({"method":"process/output","params":{"processId":"p","seq":1,"chunk":STANDARD.encode("expected")}}),
            json!({"method":"process/exited","params":{"processId":"p","seq":2,"exitCode":0}}),
            json!({"method":"process/closed","params":{"processId":"p","seq":3}}),
            json!({"id":2,"result":{"processId":"s"}}),
            json!({"method":"process/output","params":{"processId":"s","seq":1,"chunk":STANDARD.encode("denied")}}),
            json!({"method":"process/exited","params":{"processId":"s","seq":2,"exitCode":73}}),
            json!({"method":"process/closed","params":{"processId":"s","seq":3}}),
            json!({"id":3,"result":{}}),
            json!({"id":4,"result":{"dataBase64":STANDARD.encode(scenario.candidate_content)}}),
        ];
        let file = ryeos_state::objects::ProjectFile {
            blob_hash: lillux::sha256_hex(scenario.candidate_content),
            size: scenario.candidate_content.len() as u64,
            normalized_mode: 0o644,
        };
        let captured = ryeos_state::objects::canonical_value_digest(&file.to_value()).unwrap();
        let observation = |input: &[Value], output: &[Value]| {
            let encoded_output = encode(output);
            json!({
                "schema":"test.routed_guest_observation.v1",
                "request_sha256":request_hash,
                "namespace_exit":"Signal(9)",
                "applied_receipt":applied_receipt(),
                "guest_input_base64":encode(input),
                "guest_output_base64":encoded_output,
                "forwarded_output_bytes":STANDARD.decode(&encoded_output).unwrap().len(),
                "captured_files":{"result":captured},
            })
        };
        check_guest_observation(&observation(&input, &output), &scenario).unwrap();
        let mut premature = input.clone();
        premature.swap(2, 3);
        assert!(check_guest_observation(&observation(&premature, &output), &scenario).is_err());
        output[9]["result"]["dataBase64"] = json!(STANDARD.encode(b"other"));
        assert!(check_guest_observation(&observation(&input, &output), &scenario).is_err());
        output[9] = json!({"id":4,"error":{"code":-1,"message":"missing"}});
        assert!(check_guest_observation(&observation(&input, &output), &scenario).is_err());
    }
}
