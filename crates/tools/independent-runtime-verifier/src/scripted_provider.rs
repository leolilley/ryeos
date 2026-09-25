//! Finite credential-free model response script for runtime qualification.
//!
//! This module authors response bytes only. The verifier must separately own
//! a bounded loopback transport, retain every request, and check its effects
//! against the native guest transcript before any claim can be emitted.

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

pub const REQUEST_COUNT: usize = 5;
pub const GUEST_COMMAND_SCRIPT: &str = "pwd; rg --version";
pub const PATCH_INPUT: &str = "*** Begin Patch\n*** Add File: candidate-strategy.txt\n+composed external candidate C\n*** End Patch";
pub const CANDIDATE_RELATIVE_PATH: &str = "candidate-strategy.txt";
pub const CANDIDATE_PATH: &str = "/workspace/candidate-strategy.txt";
pub const CANDIDATE_URI: &str = "file:///workspace/candidate-strategy.txt";
pub const CANDIDATE_CONTENT: &str = "composed external candidate C\n";
pub const GUEST_SHELL: &str = "/ryeos/realizations/authoring-tools/bin/zsh";

pub fn response_item(
    number: usize,
    forbidden_local_command: &str,
    secret_read_command: &str,
) -> Result<Value> {
    ensure!(
        number < REQUEST_COUNT,
        "scripted provider request count exceeded"
    );
    Ok(match number {
        0 => json!({
            "type":"function_call",
            "call_id":"forbidden-local-command",
            "name":"exec_command",
            "arguments":json!({
                "environment_id":"local", "cmd":forbidden_local_command,
                "login":false, "yield_time_ms":10000
            }).to_string()
        }),
        1 => json!({
            "type":"function_call",
            "call_id":"guest-command",
            "name":"exec_command",
            "arguments":json!({
                "environment_id":"ryeos-external-candidate",
                "cmd":GUEST_COMMAND_SCRIPT,
                "workdir":"/workspace",
                "login":false,
                "yield_time_ms":10000
            }).to_string()
        }),
        2 => json!({
            "type":"custom_tool_call",
            "call_id":"candidate-edit",
            "name":"apply_patch",
            "input":PATCH_INPUT
        }),
        3 => json!({
            "type":"function_call",
            "call_id":"guest-controller-secret-read",
            "name":"exec_command",
            "arguments":json!({
                "environment_id":"ryeos-external-candidate",
                "cmd":secret_read_command,
                "workdir":"/workspace", "login":false,
                "yield_time_ms":10000
            }).to_string()
        }),
        4 => json!({
            "type":"message",
            "id":"scripted-turn-done",
            "role":"assistant",
            "content":[{"type":"output_text","text":"Candidate edit complete."}]
        }),
        _ => bail!("scripted provider request count exceeded"),
    })
}

pub fn response_sse(
    number: usize,
    forbidden_local_command: &str,
    secret_read_command: &str,
) -> Result<Vec<u8>> {
    let item = response_item(number, forbidden_local_command, secret_read_command)?;
    let response_id = format!("scripted-{number}");
    let events = [
        json!({"type":"response.created","response":{"id":&response_id}}),
        json!({"type":"response.output_item.done","item":item}),
        json!({"type":"response.completed","response":{
            "id":&response_id,
            "usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}
        }}),
    ];
    let mut body = Vec::new();
    for event in events {
        body.extend_from_slice(
            format!(
                "event: {}\ndata: {}\n\n",
                event["type"].as_str().unwrap(),
                event
            )
            .as_bytes(),
        );
    }
    ensure!(body.len() <= 64 * 1024, "scripted response exceeds bound");
    Ok(body)
}

/// Secondary evidence from the verifier-owned scripted peer. This does not
/// replace native app-server notifications or the frozen guest observation.
pub fn check_requests(
    requests: &[Value],
    expected_controller_canary: &[u8],
    observed_controller_canary: &[u8],
    secret_read_denial: &str,
) -> Result<()> {
    ensure!(
        requests.len() == REQUEST_COUNT,
        "unexpected scripted contact count"
    );
    ensure!(
        !expected_controller_canary.is_empty()
            && expected_controller_canary.len() <= 4096
            && !secret_read_denial.is_empty()
            && secret_read_denial.len() <= 256,
        "scripted canary or denial is invalid"
    );
    ensure!(
        observed_controller_canary == expected_controller_canary,
        "controller canary was modified"
    );
    let canary = std::str::from_utf8(expected_controller_canary)?.trim_end();
    let encoded = serde_json::to_vec(requests)?;
    ensure!(
        encoded.len() <= 1024 * 1024,
        "scripted requests exceed bound"
    );
    ensure!(
        !std::str::from_utf8(&encoded)?.contains(canary),
        "scripted model peer received controller canary content"
    );
    let output = |request: usize, kind: &str, call_id: &str| -> Result<&str> {
        let items = requests[request]["input"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("scripted request has no input array"))?;
        let mut matching = items
            .iter()
            .filter(|item| item["type"] == kind && item["call_id"] == call_id);
        let result = matching
            .next()
            .and_then(|item| item["output"].as_str())
            .ok_or_else(|| anyhow::anyhow!("missing exact scripted tool result for {call_id}"))?;
        ensure!(
            matching.next().is_none(),
            "duplicate scripted tool result for {call_id}"
        );
        Ok(result)
    };
    ensure!(
        output(1, "function_call_output", "forbidden-local-command")?
            .contains("unknown turn environment id `local`"),
        "local environment was not authoritatively refused"
    );
    let command = output(2, "function_call_output", "guest-command")?;
    ensure!(
        command.contains("/workspace") && command.contains("ripgrep"),
        "remote command did not return the expected guest evidence"
    );
    let patch = output(3, "custom_tool_call_output", "candidate-edit")?;
    ensure!(
        patch.contains("Success.") && patch.contains("candidate-strategy.txt"),
        "remote patch did not return its exact completion"
    );
    let secret = output(4, "function_call_output", "guest-controller-secret-read")?;
    let (header, body) = secret
        .split_once("\nOutput:\n")
        .ok_or_else(|| anyhow::anyhow!("guest read lacks terminal command framing"))?;
    let exit_lines = header
        .lines()
        .filter(|line| line.starts_with("Process exited with code "))
        .collect::<Vec<_>>();
    ensure!(
        exit_lines == ["Process exited with code 73"]
            && !header.contains("Process running with session ID")
            && body.lines().any(|line| line == secret_read_denial),
        "guest controller-canary read was not explicitly denied"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_script_has_only_five_credential_free_responses() {
        for number in 0..REQUEST_COUNT {
            let body = response_sse(number, "forbidden-local", "secret-read").unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            assert!(text.contains("response.completed"));
            assert!(!text.contains("authorization"));
        }
        assert!(response_sse(REQUEST_COUNT, "forbidden-local", "secret-read").is_err());
        let command = response_item(1, "forbidden-local", "secret-read").unwrap();
        let arguments: Value =
            serde_json::from_str(command["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments["cmd"], GUEST_COMMAND_SCRIPT);
        let patch = response_item(2, "forbidden-local", "secret-read").unwrap();
        assert_eq!(patch["input"], PATCH_INPUT);
        assert!(PATCH_INPUT.contains(&format!("*** Add File: {CANDIDATE_RELATIVE_PATH}")));
        assert!(PATCH_INPUT.contains(&format!("+{}", CANDIDATE_CONTENT.trim_end())));
        assert_eq!(
            CANDIDATE_PATH,
            format!("/workspace/{CANDIDATE_RELATIVE_PATH}")
        );
        assert_eq!(CANDIDATE_URI, format!("file://{CANDIDATE_PATH}"));
    }

    #[test]
    fn scripted_request_check_refuses_missing_or_leaking_results() {
        let canary = b"private-controller-canary\n";
        let requests = vec![
            json!({"input":[]}),
            json!({"input":[{"type":"function_call_output","call_id":"forbidden-local-command","output":"unknown turn environment id `local`"}]}),
            json!({"input":[{"type":"function_call_output","call_id":"guest-command","output":"/workspace\nripgrep"}]}),
            json!({"input":[{"type":"custom_tool_call_output","call_id":"candidate-edit","output":"Success. candidate-strategy.txt"}]}),
            json!({"input":[{"type":"function_call_output","call_id":"guest-controller-secret-read","output":"Process exited with code 73\nOutput:\ncontroller-canary-read-denied\n"}]}),
        ];
        check_requests(&requests, canary, canary, "controller-canary-read-denied").unwrap();
        assert!(
            check_requests(
                &requests[..4],
                canary,
                canary,
                "controller-canary-read-denied"
            )
            .is_err()
        );
        assert!(
            check_requests(
                &requests,
                canary,
                b"changed",
                "controller-canary-read-denied"
            )
            .is_err()
        );
        let mut leaked = requests.clone();
        leaked[4]["input"][0]["output"] = "private-controller-canary".into();
        assert!(check_requests(&leaked, canary, canary, "controller-canary-read-denied").is_err());
        let mut success = requests;
        success[4]["input"][0]["output"] =
            "Process exited with code 0\nOutput:\ncontroller-canary-read-denied\n".into();
        assert!(check_requests(&success, canary, canary, "controller-canary-read-denied").is_err());
        let mut duplicate = success;
        duplicate[4]["input"][0]["output"] =
            "Process exited with code 73\nOutput:\ncontroller-canary-read-denied\n".into();
        let repeated = duplicate[4]["input"][0].clone();
        duplicate[4]["input"].as_array_mut().unwrap().push(repeated);
        assert!(
            check_requests(&duplicate, canary, canary, "controller-canary-read-denied").is_err()
        );
    }
}
