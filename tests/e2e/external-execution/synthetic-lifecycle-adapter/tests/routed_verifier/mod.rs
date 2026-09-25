//! Direct verifier-owned Codex observation, separate from Worker admission.
//! The HTTP peer scripts a finite model conversation; only the real app-server
//! pipe and settled native guest supply the checked routing/candidate evidence.
//! This fixture mints no qualification claims, product witnesses or authority.

use super::*;
use anyhow::{Context as _, Result, ensure};
use lillux::subordinate_process::{SubordinateProcess, SubordinateProcessRequest};
use lillux::time::MonotonicDeadline;
use ryeos_independent_runtime_verifier::app_server::AppServerObservation;
use ryeos_synthetic_external_lifecycle_adapter::routing_observation::{
    MAX_EVENT_BYTES, MAX_TOTAL_BYTES, RoutingScenario, check_notifications,
};
use serde_json::{Value, json};

mod direct_guest;

const SHELL: &str = "/ryeos/realizations/authoring-tools/bin/zsh";
const CONTENT: &[u8] = b"composed external candidate C\n";

// Mechanism-only fixture selection. Production qualification must read the
// exact admitted source and independently observe the native guest; a live
// repository file is not itself runtime authority.
fn fixture_guest_environment() -> Result<BTreeMap<String, String>> {
    let source = std::fs::read_to_string(
        fixture_repository_source_root()
            .join("bundles/codex/.ai/config/codex/environments/external-authoring.yaml"),
    )?;
    let definition: Value = serde_yaml::from_str(&source)?;
    let configuration = &definition["configuration"];
    let search: Vec<ryeos_state::objects::ExecutableSearchPathEntry> =
        serde_json::from_value(configuration["executable_search"].clone())?;
    let process_environment: BTreeMap<
        String,
        ryeos_state::objects::SessionProcessEnvironmentValue,
    > = serde_json::from_value(configuration["process_environment"].clone())?;
    let destinations = BTreeMap::from([
        ("guest-runtime".into(), "/runtime".into()),
        (
            "authoring-tools".into(),
            "/ryeos/realizations/authoring-tools".into(),
        ),
    ]);
    Ok(
        ryeos_state::external_execution::admission::ExternalCandidateGuestEnvironment::derive(
            &real_codex_requirement().runtime_recipe,
            &process_environment,
            &search,
            &destinations,
        )?
        .environment,
    )
}

#[test]
fn routed_guest_fixture_uses_authored_environment_projection() -> Result<()> {
    assert_eq!(
        fixture_guest_environment()?,
        BTreeMap::from([
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_PAGER".into(), "cat".into()),
            (
                "PATH".into(),
                "/ryeos/realizations/authoring-tools/bin".into(),
            ),
            ("TMPDIR".into(), "/ryeos/runtime-views/TMPDIR".into()),
            ("TZ".into(), "UTC".into()),
        ])
    );
    Ok(())
}

mod transport_failures;

fn copy_tree(source: &Path, target: &Path) -> Result<BTreeMap<String, Value>> {
    fn visit(
        source: &Path,
        target: &Path,
        prefix: &str,
        files: &mut BTreeMap<String, Value>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF8 member"))?;
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            ensure!(
                relative.len() <= 512 && relative.split('/').count() <= 8,
                "member bound"
            );
            let kind = entry.file_type()?;
            if kind.is_dir() {
                std::fs::create_dir(target.join(&name))?;
                visit(&entry.path(), &target.join(name), &relative, files)?;
            } else {
                ensure!(
                    kind.is_file() && files.len() < 64,
                    "nonregular or excessive input"
                );
                let bytes = std::fs::read(entry.path())?;
                let mode = if entry.metadata()?.permissions().mode() & 0o111 == 0 {
                    0o644
                } else {
                    0o755
                };
                std::fs::write(target.join(&name), &bytes)?;
                std::fs::set_permissions(target.join(name), std::fs::Permissions::from_mode(mode))?;
                files.insert(
                    relative,
                    json!({"sha256":lillux::sha256_hex(&bytes),"mode":mode}),
                );
            }
        }
        Ok(())
    }
    std::fs::create_dir(target)?;
    let mut files = BTreeMap::new();
    visit(source, target, "", &mut files)?;
    Ok(files)
}

#[test]
#[ignore = "exact pinned Codex + closed tool inputs + native namespace support; no paid model contact"]
fn actual_codex_notifications_and_native_guest_agree() -> Result<()> {
    let (codex, codex_hash) = pinned_codex_artifact();
    let tools = stage_pinned_codex_command_tools(&codex, true);
    // Retain this occurrence on both success and refusal, including exact
    // requests and transcript. Never relaunch automatically after uncertainty.
    let root = tempfile::tempdir()?.keep();
    eprintln!("routed verifier retained occurrence: {}", root.display());
    let guest = root.join("guest");
    std::fs::create_dir(&guest)?;
    std::fs::create_dir(guest.join("runtime"))?;
    std::fs::create_dir(guest.join("runtime/bin"))?;
    std::fs::copy(&codex, guest.join("runtime/bin/codex"))?;
    std::fs::set_permissions(
        guest.join("runtime/bin/codex"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    std::fs::create_dir(guest.join("candidate"))?;
    let tools_identity = copy_tree(tools.path(), &guest.join("tools"))?;
    let request = serde_json::to_vec(
        &json!({"schema":"test.routed_guest.v1","recipe":real_codex_requirement().runtime_recipe,
            "effective_environment":fixture_guest_environment()?,
            "runtime":{"bin/codex":{"sha256":codex_hash,"mode":0o755}},"tools":tools_identity,"capture_limit":4096}),
    )?;
    std::fs::write(guest.join("guest-request.json"), &request)?;
    let canary = root.join("controller-canary");
    let secret_command = scripted_canary_read_command(&canary);
    let rg = lillux::run(lillux::SubprocessRequest {
        cmd: tools.path().join("bin/rg").to_string_lossy().into_owned(),
        argv0: None,
        args: vec!["--version".into()],
        cwd: None,
        envs: vec![("LANG".into(), "C".into()), ("LC_ALL".into(), "C".into())],
        stdin_data: None,
        timeout: 5.0,
        limits: None,
        inherited_fds: vec![],
        inherited_fd_mappings: vec![],
        supervised_status: None,
    });
    ensure!(rg.success, "exact pinned rg version observation refused");
    let expected_output = format!("/workspace\n{}", rg.stdout);
    let home = root.join("codex-home");
    std::fs::create_dir(&home)?;
    let guest_bin = Path::new(env!("CARGO_BIN_EXE_ryeos-synthetic-routed-guest"));
    ensure!(
        guest_bin.is_absolute() && std::fs::symlink_metadata(guest_bin)?.is_file(),
        "routed guest executable absent"
    );
    let guest_bin_hash = lillux::sha256_hex(&std::fs::read(guest_bin)?);
    let (origin, ready, peer) = start_scripted_responses_fixture(&canary);
    let mut profile = Value::Null;
    let mut sources = BTreeMap::new();
    configure_real_codex_scripted_turn_profile(
        fixture_repository_source_root(),
        &mut profile,
        &mut sources,
        &origin,
    );
    std::fs::write(home.join("config.toml"), &sources["scripted.config.toml"])?;
    std::fs::write(
        home.join("environments.toml"),
        toml::to_string(
            &json!({"default":"ryeos-external-candidate","include_local":false,"environments":[{"id":"ryeos-external-candidate","program":guest_bin,"cwd":guest,"env":{}}]}),
        )?,
    )?;
    let mut child = SubordinateProcess::spawn(SubordinateProcessRequest {
        cmd: codex.to_string_lossy().into_owned(),
        argv0: None,
        args: vec![
            "--strict-config".into(),
            "-c".into(),
            "check_for_update_on_startup=false".into(),
            "app-server".into(),
        ],
        cwd: root.to_string_lossy().into_owned(),
        envs: vec![
            ("CODEX_HOME".into(), home.to_string_lossy().into_owned()),
            ("HOME".into(), home.to_string_lossy().into_owned()),
            ("PATH".into(), String::new()),
            ("LANG".into(), "C".into()),
            ("LC_ALL".into(), "C".into()),
        ],
        limits: None,
        inherited_fds: vec![],
    })
    .map_err(anyhow::Error::msg)?;
    let input = child.take_input().map_err(anyhow::Error::msg)?;
    let output = child.take_output().map_err(anyhow::Error::msg)?;
    let diagnostics = child
        .take_error()
        .map_err(anyhow::Error::msg)?
        .start_discarding()?;
    let mut app = AppServerObservation::new(
        child,
        input,
        output,
        diagnostics,
        MonotonicDeadline::after(lillux::time::Duration::from_secs(100)),
    );
    let result = (|| -> Result<Value> {
        ready
            .send(())
            .map_err(|_| anyhow::anyhow!("scripted peer stopped"))?;
        app.request(1,"initialize",json!({"clientInfo":{"name":"ryeos-routed-fixture","version":"1"},"capabilities":{"experimentalApi":true}}))?;
        app.send(json!({"method":"initialized","params":{}}))?;
        let thread = app.request(
            2,
            "thread/start",
            json!({"cwd":"/workspace","experimentalRawEvents":true,
                "environments":[{"environmentId":"ryeos-external-candidate",
                    "cwd":"/workspace","runtimeWorkspaceRoots":["/workspace"]}]}),
        )?;
        let thread_id = thread["thread"]["id"]
            .as_str()
            .context("thread identity absent")?
            .to_owned();
        let turn = app.request(3,"turn/start",json!({"threadId":thread_id,"input":[{"type":"text","text":"Perform the bounded scripted routing scenario.","text_elements":[]}]}))?;
        let turn_id = turn["turn"]["id"]
            .as_str()
            .context("turn identity absent")?
            .to_owned();
        while !app
            .notifications()
            .iter()
            .any(|v| v["method"] == "turn/completed")
        {
            app.next()?;
        }
        std::fs::write(
            root.join("app-server-notifications.json"),
            serde_json::to_vec(app.notifications())?,
        )?;
        // For these two authored commands shlex uses a single-quoted script:
        // neither contains a single quote, backslash or caret. Do not infer an
        // expected presentation from the observed app-server result.
        ensure!(
            !secret_command.contains(['\'', '\\', '^']),
            "scenario quoting changed"
        );
        let checked = check_notifications(
            &RoutingScenario {
                thread_id,
                turn_id,
                guest_cwd: "/workspace".into(),
                local_refusal_command: scripted_local_write_command(&canary),
                guest_command_script: "pwd; rg --version".into(),
                secret_read_script: secret_command.clone(),
                patch_input: "*** Begin Patch\n*** Add File: candidate-strategy.txt\n+composed external candidate C\n*** End Patch".into(),
                guest_command: format!("{SHELL} -c 'pwd; rg --version'"),
                secret_read_command: format!("{SHELL} -c '{secret_command}'"),
                candidate_path: "/workspace/candidate-strategy.txt".into(),
                candidate_added_content: std::str::from_utf8(CONTENT)?.into(),
                expected_command_output: expected_output.clone(),
                secret_read_denial: CONTROLLER_CANARY_DENIAL.into(),
                controller_canary_value: std::str::from_utf8(CONTROLLER_CANARY)?.trim_end().into(),
            },
            app.notifications(),
        )?;
        std::fs::write(guest.join("finish"), b"capture")?;
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(20));
        let observation = loop {
            match std::fs::read(guest.join("guest-observation.json")) {
                Ok(bytes) => {
                    ensure!(
                        bytes.len() <= 3 * MAX_TOTAL_BYTES,
                        "guest observation bound"
                    );
                    break serde_json::from_slice::<Value>(&bytes)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ensure!(
                        !deadline.has_elapsed(),
                        "guest settlement observation absent"
                    );
                    lillux::time::sleep(lillux::time::Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        };
        ryeos_synthetic_external_lifecycle_adapter::guest_observation::check_guest_observation(
            &observation,
            &ryeos_synthetic_external_lifecycle_adapter::guest_observation::GuestScenario {
                request_sha256: &lillux::sha256_hex(&request),
                shell: SHELL,
                guest_cwd_uri: "file:///workspace",
                guest_command: "pwd; rg --version",
                secret_read_command: &secret_command,
                expected_command_output: &expected_output,
                controller_canary_value: std::str::from_utf8(CONTROLLER_CANARY)?.trim_end(),
                secret_read_denial: CONTROLLER_CANARY_DENIAL,
                candidate_uri: "file:///workspace/candidate-strategy.txt",
                candidate_relative_path: "candidate-strategy.txt",
                candidate_content: CONTENT,
            },
        )?;
        ensure!(
            std::fs::read(&canary)? == CONTROLLER_CANARY,
            "controller canary changed"
        );
        ensure!(
            std::fs::read(guest.join("candidate/candidate-strategy.txt"))? == CONTENT,
            "candidate bytes changed"
        );
        Ok(
            json!({"scope":"native_routed_mechanism_only","codex_sha256":codex_hash,"relay_sha256":guest_bin_hash,"request_sha256":lillux::sha256_hex(&request),"thread_id":checked.thread_id,"turn_id":checked.turn_id,"notifications":checked.notification_count,"namespace_exit":observation["namespace_exit"],"captured_files":observation["captured_files"],"paid_model_contact":false}),
        )
    })();
    // Keep app-server alive through the relay's authoritative native cleanup.
    // On refusal, still request settlement; absence of a retained observation
    // is reported as uncertainty, never papered over by killing just Codex.
    let transcript_retained = std::fs::write(
        root.join("app-server-notifications.json"),
        serde_json::to_vec(app.notifications()).expect("JSON values serialize"),
    );
    let finish_requested = if !guest.join("finish").exists() {
        std::fs::write(guest.join("finish"), b"capture")
    } else {
        Ok(())
    };
    let settlement_deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(110));
    while guest.join("guest-started").exists()
        && !guest.join("guest-observation.json").exists()
        && !guest.join("guest-failure.json").exists()
        && !settlement_deadline.has_elapsed()
    {
        lillux::time::sleep(lillux::time::Duration::from_millis(20));
    }
    let stopped = app.stop();
    let requests = peer
        .join()
        .map_err(|_| anyhow::anyhow!("scripted model peer failed"));
    stopped?;
    transcript_retained?;
    finish_requested?;
    let evidence = match result {
        Ok(value) => value,
        Err(error) => {
            // This is private synthetic-fixture diagnosis, not a settlement
            // claim. Preserve the original protocol refusal and native cause.
            if let Ok(bytes) = std::fs::read(guest.join("guest-failure.json")) {
                ensure!(bytes.len() <= 16 * 1024, "guest diagnostic bound");
                let failure: Value = serde_json::from_slice(&bytes)?;
                return Err(error.context(format!("native guest refusal: {}", failure["reason"])));
            }
            return Err(error);
        }
    };
    verify_scripted_routing_results(&requests?, &std::fs::read(&canary)?)?;
    eprintln!("routed-verifier-evidence: {evidence}");
    Ok(())
}
