//! Finite credential-free model response script for runtime qualification.
//!
//! This module authors response bytes only. The verifier must separately own
//! a bounded loopback transport, retain every request, and check its effects
//! against the native guest transcript before any claim can be emitted.

use crate::{CandidateFixture, GuestCommandFixture};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

pub const REQUEST_COUNT: usize = 5;
pub const GUEST_COMMAND_SCRIPT: &str = "pwd; rg --version";
#[cfg(test)]
pub const GUEST_ABSENT_CANDIDATE_SCRIPT: &str = r#"pwd; rg --version; if test -e candidate-strategy.txt || test -L candidate-strategy.txt; then exit 74; else printf "%s\n" candidate-absent; fi"#;
pub const PATCH_INPUT: &str = "*** Begin Patch\n*** Add File: candidate-strategy.txt\n+composed external candidate C\n*** End Patch";
pub const CANDIDATE_RELATIVE_PATH: &str = "candidate-strategy.txt";
pub const CANDIDATE_PATH: &str = "/workspace/candidate-strategy.txt";
pub const CANDIDATE_URI: &str = "file:///workspace/candidate-strategy.txt";
pub const CANDIDATE_CONTENT: &str = "composed external candidate C\n";
pub const GUEST_SHELL: &str = "/ryeos/realizations/authoring-tools/bin/zsh";

/// Shared finite outcome and routing constraints for fixture and consumer use.
/// The caller must separately establish the scripted peer's process identity.
pub fn validate_configuration(expected_command_output: &str, responses_origin: &str) -> Result<()> {
    ensure!(
        expected_command_output.len() <= 8192
            && expected_command_output.starts_with("/workspace\nripgrep ")
            && expected_command_output.ends_with('\n')
            && !expected_command_output.contains('\0'),
        "invalid authored command output"
    );
    let origin = responses_origin
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("scripted peer origin is not explicit HTTP"))?;
    let address: std::net::SocketAddr = origin.parse()?;
    ensure!(
        address.ip().is_loopback()
            && address.port() != 0
            && responses_origin == format!("http://{address}"),
        "scripted peer origin is not canonical loopback"
    );
    Ok(())
}

pub fn response_item(
    number: usize,
    fixture: GuestCommandFixture,
    candidate: &CandidateFixture,
    forbidden_local_command: &str,
    secret_read_command: &str,
) -> Result<Value> {
    candidate.validate()?;
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
                "cmd":fixture.render_command(candidate),
                "workdir":"/workspace",
                "login":false,
                "yield_time_ms":10000
            }).to_string()
        }),
        2 => json!({
            "type":"custom_tool_call",
            "call_id":"candidate-edit",
            "name":"apply_patch",
            "input":candidate.patch_input()
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
    fixture: GuestCommandFixture,
    candidate: &CandidateFixture,
    forbidden_local_command: &str,
    secret_read_command: &str,
) -> Result<Vec<u8>> {
    let item = response_item(
        number,
        fixture,
        candidate,
        forbidden_local_command,
        secret_read_command,
    )?;
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
    candidate: &CandidateFixture,
    expected_controller_canary: &[u8],
    observed_controller_canary: &[u8],
    secret_read_denial: &str,
) -> Result<()> {
    candidate.validate()?;
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
        patch.contains("Success.") && patch.contains(&candidate.relative_path),
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
    fn production_fixture_matches_finite_script() {
        let fixture = CandidateFixture::production_fixture();
        assert_eq!(fixture.patch_input(), PATCH_INPUT);
        assert_eq!(fixture.path(), CANDIDATE_PATH);
        assert_eq!(fixture.uri(), CANDIDATE_URI);
        assert_eq!(
            GuestCommandFixture::WorkspaceRgVersion.render_command(&fixture),
            GUEST_COMMAND_SCRIPT
        );
    }

    #[test]
    fn finite_configuration_refuses_ambient_routes_and_unplanned_outcomes() {
        let expected = "/workspace\nripgrep fixture\n";
        validate_configuration(expected, "http://127.0.0.1:1234").unwrap();
        validate_configuration(expected, "http://[::1]:1234").unwrap();
        for origin in [
            "https://example.com",
            "http://example.com:1234",
            "http://0.0.0.0:1234",
            "http://127.0.0.1:0",
            "http://127.0.0.1:1234/",
            "http://127.0.0.1:01234",
        ] {
            assert!(
                validate_configuration(expected, origin).is_err(),
                "{origin}"
            );
        }
        for output in [
            "observed output",
            "/workspace\nripgrep fixture",
            "/workspace\nripgrep \0\n",
        ] {
            assert!(validate_configuration(output, "http://127.0.0.1:1234").is_err());
        }
    }

    #[test]
    fn authored_candidate_changes_payload_without_changing_mandatory_operations() {
        let first = CandidateFixture::test_fixture();
        let second = CandidateFixture {
            relative_path: "alternate-candidate.txt".into(),
            content: "first line\n\n*** End Patch\n*** Add File: should-be-content\n".into(),
        };
        second.validate().unwrap();
        assert_eq!(
            second.patch_input(),
            "*** Begin Patch\n*** Add File: alternate-candidate.txt\n+first line\n+\n+*** End Patch\n+*** Add File: should-be-content\n*** End Patch"
        );
        let fixture = GuestCommandFixture::WorkspaceRgVersionRequireAbsentCandidate;
        for number in 0..REQUEST_COUNT {
            let a = response_item(number, fixture, &first, "local-refusal", "secret-read").unwrap();
            let b =
                response_item(number, fixture, &second, "local-refusal", "secret-read").unwrap();
            if number == 1 || number == 2 {
                assert_ne!(a, b);
            } else {
                assert_eq!(a, b);
            }
        }
        let command = response_item(1, fixture, &second, "local-refusal", "secret-read").unwrap();
        let args: Value = serde_json::from_str(command["arguments"].as_str().unwrap()).unwrap();
        assert!(
            args["cmd"]
                .as_str()
                .unwrap()
                .contains("test -e alternate-candidate.txt")
        );
        assert_eq!(
            response_item(2, fixture, &second, "local-refusal", "secret-read").unwrap()["input"],
            second.patch_input()
        );
    }

    #[test]
    fn authored_candidate_rejects_path_shell_patch_and_size_expansion() {
        for leaf in [
            "",
            ".",
            "..",
            "-flag",
            ".hidden",
            "a/b",
            "a\\b",
            "a b",
            "a;true",
            "a$(true)",
            "a'",
            "a\n*** Delete File: x",
            "a\0",
        ] {
            let candidate = CandidateFixture {
                relative_path: leaf.into(),
                ..CandidateFixture::test_fixture()
            };
            assert!(
                response_item(
                    2,
                    GuestCommandFixture::WorkspaceRgVersion,
                    &candidate,
                    "local",
                    "secret"
                )
                .is_err(),
                "{leaf:?}"
            );
        }
        for content in [
            "".to_owned(),
            "no-newline".into(),
            "tab\t\n".into(),
            "cr\r\n".into(),
            "nul\0\n".into(),
            format!("{}\n", "x".repeat(1024)),
        ] {
            let candidate = CandidateFixture {
                content,
                ..CandidateFixture::test_fixture()
            };
            assert!(candidate.validate().is_err());
        }
        let oversized = CandidateFixture {
            relative_path: "x".repeat(65),
            ..CandidateFixture::test_fixture()
        };
        assert!(oversized.validate().is_err());
        let unknown =
            json!({"relative_path":"candidate.txt","content":"valid\n","script":"touch elsewhere"});
        assert!(serde_json::from_value::<CandidateFixture>(unknown).is_err());
    }

    #[test]
    fn finite_script_has_only_five_credential_free_responses() {
        for number in 0..REQUEST_COUNT {
            let body = response_sse(
                number,
                GuestCommandFixture::WorkspaceRgVersion,
                &CandidateFixture::test_fixture(),
                "forbidden-local",
                "secret-read",
            )
            .unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            assert!(text.contains("response.completed"));
            assert!(!text.contains("authorization"));
        }
        assert!(
            response_sse(
                REQUEST_COUNT,
                GuestCommandFixture::WorkspaceRgVersion,
                &CandidateFixture::test_fixture(),
                "forbidden-local",
                "secret-read"
            )
            .is_err()
        );
        let command = response_item(
            1,
            GuestCommandFixture::WorkspaceRgVersion,
            &CandidateFixture::test_fixture(),
            "forbidden-local",
            "secret-read",
        )
        .unwrap();
        let arguments: Value =
            serde_json::from_str(command["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments["cmd"], GUEST_COMMAND_SCRIPT);
        let patch = response_item(
            2,
            GuestCommandFixture::WorkspaceRgVersion,
            &CandidateFixture::test_fixture(),
            "forbidden-local",
            "secret-read",
        )
        .unwrap();
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
    fn finite_command_selection_preserves_all_other_mandatory_calls() {
        let a = GuestCommandFixture::WorkspaceRgVersion;
        let b = GuestCommandFixture::WorkspaceRgVersionRequireAbsentCandidate;
        for number in 0..REQUEST_COUNT {
            let baseline = response_item(
                number,
                a,
                &crate::CandidateFixture::test_fixture(),
                "forbidden-local",
                "secret-read",
            )
            .unwrap();
            let absent = response_item(
                number,
                b,
                &crate::CandidateFixture::test_fixture(),
                "forbidden-local",
                "secret-read",
            )
            .unwrap();
            if number == 1 {
                let arguments: Value =
                    serde_json::from_str(absent["arguments"].as_str().unwrap()).unwrap();
                assert_eq!(arguments["cmd"], GUEST_ABSENT_CANDIDATE_SCRIPT);
                assert_ne!(baseline, absent);
                assert_eq!(arguments["environment_id"], "ryeos-external-candidate");
            } else {
                assert_eq!(baseline, absent);
            }
        }
        assert!(
            response_sse(
                REQUEST_COUNT,
                b,
                &crate::CandidateFixture::test_fixture(),
                "forbidden-local",
                "secret-read"
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn absent_candidate_command_executes_and_refuses_files_and_dangling_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        // This exercises the finite shell operation in a disposable local
        // workspace, with authored rg output. It grants no native guest or
        // executable identity evidence and issues no qualification claims.
        let run = |fixture: GuestCommandFixture| {
            let command = response_item(
                1,
                fixture,
                &crate::CandidateFixture::test_fixture(),
                "local-refusal",
                "secret-read",
            )
            .unwrap();
            let arguments: Value =
                serde_json::from_str(command["arguments"].as_str().unwrap()).unwrap();
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "rg() {{ printf '%s\\n' 'ripgrep fixture'; }}; {}",
                    arguments["cmd"].as_str().unwrap()
                ))
                .current_dir(directory.path())
                .env_clear()
                .output()
                .unwrap()
        };
        let expected = format!("{}\nripgrep fixture\n", directory.path().display());
        let a = GuestCommandFixture::WorkspaceRgVersion;
        let b = GuestCommandFixture::WorkspaceRgVersionRequireAbsentCandidate;
        let baseline = run(a);
        assert_eq!(baseline.status.code(), Some(0));
        assert_eq!(baseline.stdout, expected.as_bytes());
        let absent = run(b);
        assert_eq!(absent.status.code(), Some(0));
        assert_eq!(
            absent.stdout,
            format!("{expected}candidate-absent\n").as_bytes()
        );
        let candidate = directory.path().join(CANDIDATE_RELATIVE_PATH);
        std::fs::write(&candidate, b"existing candidate").unwrap();
        let existing = run(b);
        assert_eq!(existing.status.code(), Some(74));
        assert_eq!(existing.stdout, expected.as_bytes());
        assert_eq!(run(a).status.code(), Some(0));
        assert_eq!(std::fs::read(&candidate).unwrap(), b"existing candidate");
        std::fs::remove_file(&candidate).unwrap();
        std::os::unix::fs::symlink("missing-target", &candidate).unwrap();
        let dangling = run(b);
        assert_eq!(dangling.status.code(), Some(74));
        assert_eq!(dangling.stdout, expected.as_bytes());
        assert_eq!(
            std::fs::read_link(candidate).unwrap(),
            std::path::Path::new("missing-target")
        );
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
        check_requests(
            &requests,
            &CandidateFixture::test_fixture(),
            canary,
            canary,
            "controller-canary-read-denied",
        )
        .unwrap();
        assert!(
            check_requests(
                &requests[..4],
                &CandidateFixture::test_fixture(),
                canary,
                canary,
                "controller-canary-read-denied"
            )
            .is_err()
        );
        assert!(
            check_requests(
                &requests,
                &CandidateFixture::test_fixture(),
                canary,
                b"changed",
                "controller-canary-read-denied"
            )
            .is_err()
        );
        let mut leaked = requests.clone();
        leaked[4]["input"][0]["output"] = "private-controller-canary".into();
        assert!(
            check_requests(
                &leaked,
                &CandidateFixture::test_fixture(),
                canary,
                canary,
                "controller-canary-read-denied"
            )
            .is_err()
        );
        let mut success = requests;
        success[4]["input"][0]["output"] =
            "Process exited with code 0\nOutput:\ncontroller-canary-read-denied\n".into();
        assert!(
            check_requests(
                &success,
                &CandidateFixture::test_fixture(),
                canary,
                canary,
                "controller-canary-read-denied"
            )
            .is_err()
        );
        let mut duplicate = success;
        duplicate[4]["input"][0]["output"] =
            "Process exited with code 73\nOutput:\ncontroller-canary-read-denied\n".into();
        let repeated = duplicate[4]["input"][0].clone();
        duplicate[4]["input"].as_array_mut().unwrap().push(repeated);
        assert!(
            check_requests(
                &duplicate,
                &CandidateFixture::test_fixture(),
                canary,
                canary,
                "controller-canary-read-denied"
            )
            .is_err()
        );
    }
}
