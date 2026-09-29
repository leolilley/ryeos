//! Local acceptance of signed profile selection through the real parameter,
//! response and observation validators. The observations are synthetic; this
//! test neither launches a guest nor constructs accepted qualification evidence.

use super::{GuestCommandFixture, POLICY_REF, TOOL_REF, tests::scenario_for_fixture};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::crypto::SigningKey;
use ryeos_codex_runtime_verifier::{
    CandidateFixture, Parameters,
    guest_observation::{GuestScenario, check_guest_observation},
    routing_observation::{
        COMMAND_CALL, LOCAL_CALL, PATCH_CALL, RoutingScenario, SECRET_CALL, check_notifications,
    },
    scripted_provider::{self, GUEST_SHELL},
};
use ryeos_engine::runtime::config_schema::validate_caller_params;
use ryeos_state::external_content::products::qualification::ProductQualificationPolicy;
use serde_json::{Value, json};

const REQUEST_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const LOCAL_COMMAND: &str = "printf forbidden > /controller/canary";
const SECRET_COMMAND: &str = "read < /controller/canary";
const SECRET_DENIAL: &str = "no such file or directory: /controller/canary";
const CANARY: &str = "synthetic-controller-secret-never-exposed";

fn decode_signed(bytes: &[u8], key: &SigningKey) -> (Value, String) {
    let text = std::str::from_utf8(bytes).unwrap();
    let header =
        lillux::signature::parse_signature_line(text.lines().next().unwrap(), "#", None).unwrap();
    let body = lillux::signature::strip_signature_lines(text);
    assert!(lillux::signature::is_valid_signature_for(
        &header.content_hash,
        &header.signature_b64,
        &header.signer_fingerprint,
        &body,
        &key.verifying_key(),
        &lillux::signature::compute_fingerprint(&key.verifying_key()),
    ));
    (serde_yaml::from_str(&body).unwrap(), header.content_hash)
}

fn expected_routing(parameters: &Parameters) -> RoutingScenario {
    let command = parameters.configuration.profile.command_script();
    RoutingScenario {
        thread_id: "thread-profile-acceptance".into(),
        turn_id: "turn-profile-acceptance".into(),
        guest_cwd: "/workspace".into(),
        local_refusal_command: LOCAL_COMMAND.into(),
        guest_command_script: command.clone(),
        secret_read_script: SECRET_COMMAND.into(),
        patch_input: parameters.configuration.profile.candidate.patch_input(),
        guest_command: format!("{GUEST_SHELL} -c '{command}'"),
        secret_read_command: format!("{GUEST_SHELL} -c '{SECRET_COMMAND}'"),
        candidate_path: parameters.configuration.profile.candidate.path(),
        candidate_added_content: parameters.configuration.profile.candidate.content.clone(),
        expected_command_output: parameters.configuration.expected_command_output.clone(),
        secret_read_denial: SECRET_DENIAL.into(),
        controller_canary_value: CANARY.into(),
    }
}

fn expected_guest<'a>(parameters: &'a Parameters) -> GuestScenario<'a> {
    GuestScenario {
        request_sha256: REQUEST_HASH,
        shell: GUEST_SHELL,
        guest_cwd_uri: "file:///workspace",
        guest_command: parameters.configuration.profile.command_script(),
        secret_read_command: SECRET_COMMAND,
        expected_command_output: &parameters.configuration.expected_command_output,
        controller_canary_value: CANARY,
        secret_read_denial: SECRET_DENIAL,
        candidate_uri: parameters.configuration.profile.candidate.uri(),
        candidate_relative_path: &parameters.configuration.profile.candidate.relative_path,
        candidate_content: parameters
            .configuration
            .profile
            .candidate
            .content
            .as_bytes(),
    }
}

fn json_lines(frames: &[Value]) -> String {
    frames.iter().map(|frame| format!("{frame}\n")).collect()
}

fn observed_streams(
    fixture: GuestCommandFixture,
    candidate: &CandidateFixture,
    routing: &RoutingScenario,
) -> (Vec<Value>, Value) {
    // Fixture outputs are authored independently of the policy under test.
    let output = match fixture {
        GuestCommandFixture::WorkspaceRgVersion => "/workspace\nripgrep fixture\n",
        GuestCommandFixture::WorkspaceRgVersionRequireAbsentCandidate => {
            "/workspace\nripgrep fixture\ncandidate-absent\n"
        }
    };
    let mut notifications = Vec::new();
    let mut emitted_command = String::new();
    for number in 0..4 {
        let item = scripted_provider::response_item(
            number,
            fixture,
            candidate,
            LOCAL_COMMAND,
            SECRET_COMMAND,
        )
        .unwrap();
        if number == 1 {
            let arguments: Value =
                serde_json::from_str(item["arguments"].as_str().unwrap()).unwrap();
            emitted_command = arguments["cmd"].as_str().unwrap().to_owned();
        }
        notifications.push(json!({"method":"rawResponseItem/completed","params":{
            "threadId":routing.thread_id,"turnId":routing.turn_id,"item":item}}));
    }
    for (id, kind, result) in [
        (
            LOCAL_CALL,
            "function_call_output",
            "unknown turn environment id `local`".to_owned(),
        ),
        (
            COMMAND_CALL,
            "function_call_output",
            format!("Process exited with code 0\nOutput:\n{output}"),
        ),
        (
            PATCH_CALL,
            "custom_tool_call_output",
            format!("Success. {}", candidate.path()),
        ),
        (
            SECRET_CALL,
            "function_call_output",
            format!("Process exited with code 1\nOutput:\n{SECRET_DENIAL}"),
        ),
    ] {
        notifications.push(json!({"method":"rawResponseItem/completed","params":{
            "threadId":routing.thread_id,"turnId":routing.turn_id,
            "item":{"type":kind,"call_id":id,"output":result}}}));
    }
    for (id, command, status, exit, result) in [
        (
            COMMAND_CALL,
            format!("{GUEST_SHELL} -c '{emitted_command}'"),
            "completed",
            0,
            output,
        ),
        (
            SECRET_CALL,
            format!("{GUEST_SHELL} -c '{SECRET_COMMAND}'"),
            "failed",
            1,
            SECRET_DENIAL,
        ),
    ] {
        notifications.push(json!({"method":"item/completed","params":{
            "threadId":routing.thread_id,"turnId":routing.turn_id,"completedAtMs":1,
            "item":{"type":"commandExecution","id":id,"command":command,"cwd":"/workspace",
            "status":status,"exitCode":exit,"aggregatedOutput":result}}}));
    }
    notifications.push(json!({"method":"item/completed","params":{
        "threadId":routing.thread_id,"turnId":routing.turn_id,"completedAtMs":1,
        "item":{"type":"fileChange","id":PATCH_CALL,"status":"completed","changes":[{
            "path":candidate.path(),"kind":{"type":"add"},"diff":candidate.content}]}}}));
    notifications.push(json!({"method":"turn/completed","params":{
        "threadId":routing.thread_id,"turn":{"id":routing.turn_id,"status":"completed","error":null}}}));

    let input = json_lines(&[
        json!({"id":1,"method":"process/start","params":{"processId":"p","argv":[GUEST_SHELL,"-c",emitted_command],"cwd":"file:///workspace"}}),
        json!({"id":2,"method":"process/start","params":{"processId":"s","argv":[GUEST_SHELL,"-c",SECRET_COMMAND],"cwd":"file:///workspace"}}),
        json!({"id":3,"method":"fs/writeFile","params":{"path":candidate.uri(),"dataBase64":STANDARD.encode(candidate.content.as_bytes())}}),
    ]);
    let mut frames = Vec::new();
    for (request, process, text, exit) in [(1, "p", output, 0), (2, "s", SECRET_DENIAL, 73)] {
        frames.push(json!({"id":request,"result":{"processId":process}}));
        frames.extend([
            json!({"method":"process/output","params":{"processId":process,"seq":1,"chunk":STANDARD.encode(text)}}),
            json!({"method":"process/exited","params":{"processId":process,"seq":2,"exitCode":exit}}),
            json!({"method":"process/closed","params":{"processId":process,"seq":3}}),
        ]);
    }
    frames.push(json!({"id":3,"result":{}}));
    let wire = json_lines(&frames);
    let candidate_file = ryeos_state::objects::ProjectFile {
        blob_hash: lillux::sha256_hex(candidate.content.as_bytes()),
        size: candidate.content.len() as u64,
        normalized_mode: 0o644,
    };
    let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
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
    };
    let guest = json!({"schema":"test.routed_guest_observation.v1","request_sha256":REQUEST_HASH,
        "namespace_exit":"Signal(9)","applied_receipt":receipt,
        "guest_input_base64":STANDARD.encode(input),"guest_output_base64":STANDARD.encode(&wire),
        "forwarded_output_bytes":wire.len(),"captured_files":{
            (candidate.relative_path.as_str()):ryeos_state::objects::canonical_value_digest(&candidate_file.to_value()).unwrap()}});
    (notifications, guest)
}

#[test]
fn two_signed_profiles_share_the_observation_pipeline_and_refuse_substitution() {
    let key = SigningKey::from_bytes(&[47; 32]);
    let executable = std::env::current_exe().unwrap();
    let fixtures = [
        GuestCommandFixture::WorkspaceRgVersion,
        GuestCommandFixture::WorkspaceRgVersionRequireAbsentCandidate,
    ];
    let mut cases = Vec::new();
    for fixture in fixtures {
        let started = std::time::Instant::now();
        let authored = scenario_for_fixture(fixture);
        let sources = authored
            .signed_sources(&key, "2026-09-29T00:00:00Z")
            .unwrap();
        let (tool, _) = decode_signed(
            &sources[".ai/tools/fixtures/codex-runtime/verify.yaml"],
            &key,
        );
        let (policy_wire, policy_hash) = decode_signed(
            &sources[".ai/config/fixtures/codex-runtime/qualification.yaml"],
            &key,
        );
        assert_eq!(tool["version"], "3.0.0");
        assert_eq!(tool["version"], policy_wire["version"]);
        let policy =
            ProductQualificationPolicy::from_value(&policy_wire["product_qualification_policy"])
                .unwrap();
        let recipe_ref =
            &policy.producer_scenarios[&authored.profile.producer_scenario_id].recipe_ref;
        let recipe_path = format!(
            ".ai/config/{}.yaml",
            recipe_ref.strip_prefix("config:").unwrap()
        );
        let (recipe, recipe_hash) = decode_signed(&sources[&recipe_path], &key);
        assert_eq!(
            recipe["product_producer_recipe"],
            serde_json::to_value(&authored.expected_producer_recipe).unwrap()
        );
        let wire = validate_caller_params(&tool, &policy.verifier_parameters, TOOL_REF).unwrap();
        let parameters = Parameters::parse(&serde_json::to_vec(&wire).unwrap()).unwrap();
        let sealed = json!([
            {"id":"configurations","kind":"tree","mode":"pinned","manifest_hash":"d".repeat(64),"entry_count":1,"total_bytes":1,"mount_root":"project","mount":"qualification/configurations"},
            {"id":"controller","kind":"tree","mode":"pinned","manifest_hash":"b".repeat(64),"entry_count":1,"total_bytes":1,"mount_root":"project","mount":"qualification/controller"},
            {"id":"subject","kind":"tree","mode":"pinned","manifest_hash":"a".repeat(64),"entry_count":1,"total_bytes":1,"mount_root":"project","mount":"qualification/subject"},
            {"id":"tools","kind":"tree","mode":"pinned","manifest_hash":"c".repeat(64),"entry_count":1,"total_bytes":1,"mount_root":"project","mount":"qualification/tools"},
        ]);
        parameters.select(&sealed.to_string()).unwrap();
        let mut wrong_subject = sealed;
        wrong_subject[2]["manifest_hash"] = json!("0".repeat(64));
        assert!(parameters.select(&wrong_subject.to_string()).is_err());
        let routing = expected_routing(&parameters);
        let (notifications, guest) = observed_streams(
            fixture,
            &parameters.configuration.profile.candidate,
            &routing,
        );
        check_notifications(&routing, &notifications).unwrap();
        check_guest_observation(&guest, &expected_guest(&parameters)).unwrap();
        eprintln!(
            "{}",
            json!({"stage":"signed_source_and_synthetic_observation_validation",
            "test_executable":executable,"policy_ref":POLICY_REF,"policy_source_digest":policy_hash,
            "parameters_digest":policy.admitted_parameters_digest().unwrap(),"recipe_ref":recipe_ref,
            "recipe_source_digest":recipe_hash,"subject_manifest_hash":parameters.configuration.subject_manifest_hash,
            "profile":parameters.configuration.profile,"command":routing.guest_command_script,
            "expected_output":routing.expected_command_output,"elapsed_ms":started.elapsed().as_millis(),
            "qualification_claims_issued":false})
        );
        cases.push((tool, wire, parameters, routing, notifications, guest));
    }
    for index in 0..2 {
        let (tool, wire, parameters, routing, notifications, guest) = &cases[index];
        let (_, other_wire, _, _, other_notifications, other_guest) = &cases[1 - index];
        for field in [
            "profile",
            "expected_producer_recipe_ref",
            "expected_command_output",
        ] {
            let mut changed = wire.clone();
            changed["configuration"][field] = other_wire["configuration"][field].clone();
            assert!(
                validate_caller_params(tool, &changed, TOOL_REF).is_err(),
                "{field}"
            );
        }
        for field in ["producer_scenario_id", "command_fixture", "candidate"] {
            let mut changed = wire.clone();
            changed["configuration"]["profile"][field] =
                other_wire["configuration"]["profile"][field].clone();
            assert!(
                validate_caller_params(tool, &changed, TOOL_REF).is_err(),
                "{field}"
            );
        }
        let mut recipe_swap = wire.clone();
        recipe_swap["configuration"]["expected_producer_recipe"]["bounds"]["maximum_stdout_bytes"] =
            json!(1024);
        assert!(validate_caller_params(tool, &recipe_swap, TOOL_REF).is_err());
        assert!(check_notifications(routing, other_notifications).is_err());
        assert!(check_guest_observation(other_guest, &expected_guest(parameters)).is_err());
        let mut changed = parameters.clone();
        changed
            .configuration
            .expected_command_output
            .push_str("changed\n");
        assert!(check_notifications(&expected_routing(&changed), notifications).is_err());
        assert!(check_guest_observation(guest, &expected_guest(&changed)).is_err());
        // Correct provider/app-server presentation cannot substitute a wrong
        // guest result or an unsettled guest namespace.
        let mut unsettled = guest.clone();
        unsettled["namespace_exit"] = json!("Exit(0)");
        assert!(check_guest_observation(&unsettled, &expected_guest(parameters)).is_err());
    }
}

#[test]
fn signed_candidate_data_changes_under_one_operation_and_refuses_cross_use() {
    let key = SigningKey::from_bytes(&[47; 32]);
    let first = scenario_for_fixture(GuestCommandFixture::WorkspaceRgVersionRequireAbsentCandidate);
    let mut second = first.clone();
    second.profile.candidate = CandidateFixture {
        relative_path: "different-signed-candidate.txt".into(),
        content: "a distinct authored candidate\n\n*** End Patch\nthis is file content\n".into(),
    };
    let mut cases = Vec::new();
    for authored in [first, second] {
        let sources = authored
            .signed_sources(&key, "2026-09-29T00:00:00Z")
            .unwrap();
        let (tool, _) = decode_signed(
            &sources[".ai/tools/fixtures/codex-runtime/verify.yaml"],
            &key,
        );
        let (policy, _) = decode_signed(
            &sources[".ai/config/fixtures/codex-runtime/qualification.yaml"],
            &key,
        );
        let policy =
            ProductQualificationPolicy::from_value(&policy["product_qualification_policy"])
                .unwrap();
        let wire = validate_caller_params(&tool, &policy.verifier_parameters, TOOL_REF).unwrap();
        let parameters = Parameters::parse(&serde_json::to_vec(&wire).unwrap()).unwrap();
        let routing = expected_routing(&parameters);
        let (notifications, guest) = observed_streams(
            parameters.configuration.profile.command_fixture,
            &parameters.configuration.profile.candidate,
            &routing,
        );
        check_notifications(&routing, &notifications).unwrap();
        check_guest_observation(&guest, &expected_guest(&parameters)).unwrap();
        cases.push((tool, wire, parameters, routing, notifications, guest));
    }
    assert_eq!(
        cases[0].2.configuration.profile.command_fixture,
        cases[1].2.configuration.profile.command_fixture
    );
    assert_eq!(
        cases[0].2.configuration.expected_producer_recipe,
        cases[1].2.configuration.expected_producer_recipe
    );
    assert_eq!(cases[0].0["config"], cases[1].0["config"]);
    for index in 0..2 {
        let (tool, wire, parameters, routing, notifications, guest) = &cases[index];
        let (_, other_wire, _, _, other_notifications, other_guest) = &cases[1 - index];
        for field in ["relative_path", "content"] {
            let mut substituted = wire.clone();
            substituted["configuration"]["profile"]["candidate"][field] =
                other_wire["configuration"]["profile"]["candidate"][field].clone();
            assert!(validate_caller_params(tool, &substituted, TOOL_REF).is_err());
            let changed = Parameters::parse(&serde_json::to_vec(&substituted).unwrap()).unwrap();
            assert!(check_notifications(&expected_routing(&changed), notifications).is_err());
            assert!(check_guest_observation(guest, &expected_guest(&changed)).is_err());
        }
        assert!(check_notifications(routing, other_notifications).is_err());
        assert!(check_guest_observation(other_guest, &expected_guest(parameters)).is_err());
        let without_patch = notifications
            .iter()
            .filter(|event| event["params"]["item"]["call_id"] != PATCH_CALL)
            .cloned()
            .collect::<Vec<_>>();
        assert!(check_notifications(routing, &without_patch).is_err());
    }
}
