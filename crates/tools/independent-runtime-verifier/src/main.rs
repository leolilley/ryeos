//! Bundle Tool entrypoint for exact selected-runtime preflight and a separate
//! bounded scenario-driver mode. Neither mode returns qualification claims;
//! the driver only records raw evidence for its enclosing verifier authority.

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_independent_runtime_verifier::{
    INPUT_LIMIT, PRODUCER_SCENARIO_ID, Parameters,
    app_server::{AppServerObservation, AppServerStopOutcome},
    guest_observation::{GuestScenario, check_guest_observation},
    native_guest,
    routing_observation::{RoutingScenario, check_notifications},
    scripted_peer::ScriptedPeer,
    scripted_provider, scripted_relay, staging,
};
use ryeos_runtime::callback::CallbackError;
use ryeos_runtime::callback_uds::UdsRuntimeClient;
use ryeos_runtime::scoped_relay_handoff::ScopedRelayHandoff;
use ryeos_state::external_content::products::producer_recipe::ProductProducerRecipe;
use ryeos_state::external_content::products::qualification::ProductProducerRecipeSourceIdentity;
use serde::Deserialize;
use serde_json::json;
use std::{ffi::OsStr, io::Read as _, path::Path};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next();
    ensure!(args.next().is_none(), "unexpected verifier arguments");
    ensure!(
        mode.is_none()
            || mode.as_deref() == Some(OsStr::new("--scenario-driver"))
            || mode.as_deref() == Some(OsStr::new("--scoped-resume-race-probe")),
        "unsupported verifier mode"
    );
    let mut input = Vec::new();
    std::io::stdin()
        .take((INPUT_LIMIT + 1) as u64)
        .read_to_end(&mut input)?;
    ensure!(input.len() <= INPUT_LIMIT, "verifier input exceeds bound");
    let parameters = Parameters::parse(&input)?;
    let realizations = std::env::var("RYEOS_EXTERNAL_REALIZATIONS")
        .context("daemon-protected realization set absent")?;
    let selected = parameters.select(&realizations)?;
    let project = lillux::PinnedDirectory::open(Path::new("."))?
        .context("admitted verifier project root absent")?;
    if mode.as_deref() == Some(OsStr::new("--scenario-driver")) {
        let staged = staging::stage_scoped_driver_probe(&selected, &project, &parameters)?;
        staged.recheck_preflight(&parameters)?;
        return run_scenario_driver(&parameters, &staged);
    }
    let (_, expected_request) = selected.prepare_native_probe_request(&project, &parameters)?;
    let expected_request_sha256 = lillux::sha256_hex(&expected_request);
    let challenge = staging::create_parent_challenge(&project)?;
    let commands = challenge.scripted_canary_commands()?;
    let (provider, socket_name) = ScriptedPeer::bind_pinned(
        &parameters.configuration.responses_origin,
        challenge.directory(),
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(180)),
        commands.forbidden_local_write,
        commands.guest_read,
        challenge.value().to_owned(),
    )?;
    let provider_endpoint = challenge.publish_provider_endpoint(&socket_name)?;
    let provider = provider.start()?;
    let thread_id = std::env::var("RYEOS_THREAD_ID")
        .context("admitted verifier root thread identity absent")?;
    let client = UdsRuntimeClient::from_env()?;
    let expected_source: ProductProducerRecipeSourceIdentity = serde_json::from_value(
        client
            .scoped_child_expected_source(&thread_id, PRODUCER_SCENARIO_ID)
            .await
            .context("admitted producer source point read refused")?,
    )?;
    check_admitted_producer_source(
        &expected_source,
        &parameters.configuration.expected_producer_recipe_ref,
        &parameters.configuration.expected_producer_recipe,
    )?;
    let expected_isolation_class = client
        .scoped_child_expected_isolation_class(&thread_id)
        .await
        .context("admitted isolation class point read refused")?;
    let race_probe = mode.as_deref() == Some(OsStr::new("--scoped-resume-race-probe"));
    let start = if race_probe {
        // Both requests use the same admitted root callback authority. RESUME
        // is an exact point read: it cannot choose a scenario or start work.
        // The daemon test gate reports only once RESUME has seen Reserved.
        // Each request needs its own UDS connection: one client serializes
        // requests across the full response and START is deliberately held.
        let resume_client = UdsRuntimeClient::from_env()?;
        let (start, resumed) = tokio::join!(
            client.start_scoped_child(&thread_id, PRODUCER_SCENARIO_ID),
            resume_client.resume_scoped_child(&thread_id),
        );
        let started = start.context("scoped race START did not acknowledge")?;
        let resumed = resumed.context("scoped race RESUME did not acknowledge")?;
        ensure!(
            started == resumed,
            "scoped race START and RESUME locators differ"
        );
        Ok(started)
    } else {
        client
            .start_scoped_child(&thread_id, PRODUCER_SCENARIO_ID)
            .await
    };
    // A transport error after release is ambiguous. Resume is an exact
    // owner-bound point read, never a second launch or a new scenario choice.
    let locator = match start {
        Ok(locator) => locator,
        Err(start_error @ CallbackError::Transport(_)) => client
            .resume_scoped_child(&thread_id)
            .await
            .map_err(|resume_error| {
                anyhow::anyhow!(
                    "scoped start unsettled and exact resume refused: start={start_error}; resume={resume_error}"
                )
            })?,
        Err(definite_refusal) => return Err(definite_refusal.into()),
    };
    let locator: ScopedAttemptLocator =
        serde_json::from_value(locator).context("scoped child returned a noncanonical locator")?;
    check_scoped_locator_source(&locator, &expected_source)?;
    let observed = client
        .observe_scoped_child(&thread_id, &locator.attempt_id)
        .await
        .context("scoped producer observation refused")?;
    let observation: ScopedObservationCut = serde_json::from_value(observed)
        .context("scoped producer returned an invalid observation envelope")?;
    check_observed_full_source(&expected_source, &observation.producer_source)?;
    check_observed_isolation_class(&expected_isolation_class, &observation.isolation_provenance)?;
    let expected_recipe = &parameters.configuration.expected_producer_recipe;
    check_observed_recipe_coordinate(
        expected_recipe,
        &locator.recipe_digest,
        &observation.recipe_digest,
        observation.maximum_stdout_bytes,
        observation.maximum_stderr_bytes,
    )?;
    match (
        expected_recipe.loopback_ingress.as_ref(),
        observation.relay_handoff.as_ref(),
    ) {
        (None, None) => {}
        (Some(ingress), Some(handoff)) => {
            handoff.validate_for_verifier(
                &thread_id,
                PRODUCER_SCENARIO_ID,
                &expected_source,
                ingress,
            )?;
            ensure!(
                handoff.attempt_id == locator.attempt_id
                    && handoff.expected_applied_launch_digest
                        == lillux::sha256_hex(
                            lillux::canonical_json(&serde_json::to_value(
                                &locator.expected_applied_launch,
                            )?)?
                            .as_bytes(),
                        )
                    && handoff.held_process_identity_digest
                        == lillux::sha256_hex(
                            lillux::canonical_json(&observation.process_identity)?.as_bytes(),
                        ),
                "scoped relay handoff differs from daemon target evidence"
            );
        }
        _ => bail!("scoped relay handoff presence differs from signed recipe"),
    }
    check_scoped_applied_target(&locator, &observation.applied_launch)?;
    ensure!(
        observation.schema == "ryeos.scoped_producer_observation.v4"
            && observation.attempt_id == locator.attempt_id
            && observation.launch_owner["thread_id"] == thread_id
            && observation.launch_owner["monotonic_launch_epoch"]
                .as_u64()
                .is_some_and(|epoch| epoch > 0)
            && observation.launch_owner["unpredictable_nonce"]
                .as_str()
                .is_some_and(|nonce| !nonce.is_empty() && nonce.len() <= 256)
            && observation.launch_owner["daemon_generation_id"]
                .as_str()
                .is_some_and(|generation| !generation.is_empty() && generation.len() <= 256)
            && observation.recipe_digest == locator.recipe_digest
            && observation.recipe_generation == locator.recipe_generation
            && observation.recipe_generation == expected_source.bundle_generation_identity
            && observation.scenario_digest == locator.scenario_digest
            && observation.process_identity["schema_version"] == 5
            && observation.process_identity["target_pid"]
                .as_i64()
                .is_some_and(|pid| pid > 0)
            && observation.process_identity["target_start_time_ticks"]
                .as_i64()
                .is_some_and(|ticks| ticks > 0)
            && observation.process_identity["group_leader_pid"]
                .as_i64()
                .is_some_and(|pid| pid > 0)
            && observation.process_identity["group_leader_start_time_ticks"]
                .as_i64()
                .is_some_and(|ticks| ticks > 0)
            && observation.process_identity["process_scope"]
                == serde_json::to_value(&observation.scope_recovery)?
            && observation.applied_launch.owned_child_pid as i64
                == observation.process_identity["target_pid"]
                    .as_i64()
                    .unwrap_or_default()
            && observation.applied_launch.namespace_pid == 1
            && observation.applied_launch.effective_uid == 1
            && observation.applied_launch.effective_gid == 1
            && observation.applied_launch.no_new_privs
            && observation.applied_launch.seccomp_mode == 2
            && observation.producer_exit_clean
            && lillux::valid_hash(&observation.natural_empty_receipt_digest)
            && observation.subprocess_success
            && observation.exit_code == 0
            && !observation.timed_out
            && !observation.stdout_truncated
            && !observation.stderr_truncated
            && observation.stdout_bytes == observation.stdout.len() as u64
            && observation.stderr_bytes == observation.stderr.len() as u64
            && observation.stdout_bytes <= observation.maximum_stdout_bytes
            && observation.stderr_bytes <= observation.maximum_stderr_bytes
            && observation.output_limit_exceeded.is_none()
            && observation.launcher_refusal.is_none()
            && observation.stdout_sha256 == lillux::sha256_hex(observation.stdout.as_bytes())
            && observation.stderr_sha256 == lillux::sha256_hex(observation.stderr.as_bytes()),
        "scoped producer did not prove an exact clean natural result"
    );
    let transcript: serde_json::Value = serde_json::from_str(&observation.stdout)
        .context("scoped producer stdout is not a bounded scenario transcript")?;
    ensure!(
        transcript["schema"] == "test.independent_routed_scenario_driver.v1"
            && transcript["request_sha256"] == expected_request_sha256
            && transcript["relay_contacts"] == scripted_provider::REQUEST_COUNT,
        "scoped producer returned a different scenario transcript"
    );
    let requests = provider
        .finish_after_producer_settlement(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(10),
        ))
        .map_err(|_| {
            anyhow::anyhow!("scripted provider did not settle after child scope death")
        })??;
    challenge.recheck()?;
    provider_endpoint.recheck()?;
    let expected_canary = format!("{}\n", challenge.value());
    scripted_provider::check_requests(
        &requests,
        expected_canary.as_bytes(),
        expected_canary.as_bytes(),
        staging::CONTROLLER_CANARY_DENIAL,
    )?;
    let reported_occurrence = transcript["occurrence"]
        .as_str()
        .context("scoped transcript has no occurrence locator")?;
    let frozen = staging::inspect_frozen_scoped_occurrence(
        &selected,
        &project,
        &parameters,
        reported_occurrence,
    )?;
    ensure!(
        transcript["guest_observation"] == frozen.guest_observation
            && transcript["environment_configuration_sha256"]
                == frozen.environment_configuration_sha256
            && frozen.candidate_sha256
                == lillux::sha256_hex(scripted_provider::CANDIDATE_CONTENT.as_bytes()),
        "scoped transcript and frozen candidate disagree with pinned post-scope files"
    );
    native_guest::check_applied_receipt_against_signed_request(
        &frozen.guest_observation,
        &expected_request,
    )?;
    ensure!(
        transcript["direct_child_exit"]["success"] == true
            && transcript["direct_child_exit"]["code"] == 0,
        "direct Codex child did not exit cleanly"
    );
    let thread_id = transcript["thread_id"]
        .as_str()
        .context("scoped transcript omitted Codex thread")?;
    let turn_id = transcript["turn_id"]
        .as_str()
        .context("scoped transcript omitted Codex turn")?;
    let canary_commands = challenge.scripted_canary_commands()?;
    let guest_command = scripted_provider::GUEST_COMMAND_SCRIPT;
    let routing = RoutingScenario {
        thread_id: thread_id.to_owned(),
        turn_id: turn_id.to_owned(),
        guest_cwd: "/workspace".into(),
        local_refusal_command: canary_commands.forbidden_local_write,
        guest_command_script: guest_command.into(),
        secret_read_script: canary_commands.guest_read.clone(),
        patch_input: scripted_provider::PATCH_INPUT.into(),
        guest_command: format!("{} -c '{guest_command}'", scripted_provider::GUEST_SHELL),
        secret_read_command: format!(
            "{} -c '{}'",
            scripted_provider::GUEST_SHELL,
            canary_commands.guest_read
        ),
        candidate_path: scripted_provider::CANDIDATE_PATH.into(),
        candidate_added_content: scripted_provider::CANDIDATE_CONTENT.into(),
        expected_command_output: parameters.configuration.expected_command_output.clone(),
        secret_read_denial: staging::CONTROLLER_CANARY_DENIAL.into(),
        controller_canary_value: challenge.value().to_owned(),
    };
    // The exact signed scenario driver owns the live app-server and native
    // guest pipes. These checks reject malformed child reports, but claims
    // also require proof that the daemon launched that collector under the
    // selected recipe and kept model-controlled writes inside the guest.
    let notifications = transcript["notifications"]
        .as_array()
        .context("scoped transcript omitted Codex notifications")?;
    check_notifications(&routing, notifications)?;
    let guest = GuestScenario {
        request_sha256: &expected_request_sha256,
        shell: scripted_provider::GUEST_SHELL,
        guest_cwd_uri: "file:///workspace",
        guest_command: scripted_provider::GUEST_COMMAND_SCRIPT,
        secret_read_command: &routing.secret_read_script,
        expected_command_output: &routing.expected_command_output,
        controller_canary_value: challenge.value(),
        secret_read_denial: staging::CONTROLLER_CANARY_DENIAL,
        candidate_uri: scripted_provider::CANDIDATE_URI,
        candidate_relative_path: scripted_provider::CANDIDATE_RELATIVE_PATH,
        candidate_content: scripted_provider::CANDIDATE_CONTENT.as_bytes(),
    };
    check_guest_observation(&frozen.guest_observation, &guest)?;
    if race_probe {
        bail!(
            "scoped race START/RESUME exact locators matched and full child observation settled; collector identity, isolation, terminal and runtime provenance is incomplete; no qualification claims issued"
        )
    }
    bail!(
        "scoped producer and frozen files settled; Codex and guest records are internally checked, but collector identity, isolation, terminal and runtime provenance is incomplete; no qualification claims issued"
    )
}

fn check_observed_recipe_coordinate(
    expected: &ProductProducerRecipe,
    locator_digest: &str,
    observed_digest: &str,
    maximum_stdout_bytes: u64,
    maximum_stderr_bytes: u64,
) -> Result<()> {
    expected.validate()?;
    let signed_digest = expected.digest()?;
    ensure!(
        locator_digest == signed_digest
            && observed_digest == signed_digest
            && maximum_stdout_bytes == expected.bounds.maximum_stdout_bytes
            && maximum_stderr_bytes == expected.bounds.maximum_stderr_bytes,
        "scoped producer recipe coordinate differs from signed verifier parameters"
    );
    Ok(())
}

/// Compare the entire node-owned admission class retained before START with
/// the later launch record. Only the concrete per-attempt plan is projected
/// away; partial field checks would admit a different policy or adapter.
fn check_observed_isolation_class(
    expected: &serde_json::Value,
    observed: &serde_json::Value,
) -> Result<()> {
    let expected = expected
        .as_object()
        .context("protected expected isolation class is not an object")?;
    let observed = observed
        .as_object()
        .context("scoped isolation provenance is not an object")?;
    ensure!(
        expected.get("plan_digest") == Some(&serde_json::Value::Null)
            && expected.get("mode").is_some_and(|mode| mode == "enforce")
            && expected
                .get("backend_status")
                .is_some_and(|status| status == "available")
            && observed
                .get("plan_digest")
                .and_then(serde_json::Value::as_str)
                .and_then(|digest| digest.strip_prefix("sha256:"))
                .is_some_and(lillux::valid_hash),
        "scoped isolation class or concrete launch plan is not qualified"
    );
    let mut observed_class = observed.clone();
    observed_class.insert("plan_digest".to_owned(), serde_json::Value::Null);
    ensure!(
        &observed_class == expected,
        "scoped isolation provenance differs from pre-launch admitted class"
    );
    Ok(())
}

fn check_admitted_producer_source(
    source: &ProductProducerRecipeSourceIdentity,
    expected_ref: &str,
    expected_recipe: &ProductProducerRecipe,
) -> Result<()> {
    source.validate()?;
    expected_recipe.validate()?;
    ensure!(
        source.canonical_ref == expected_ref && source.recipe_digest == expected_recipe.digest()?,
        "admitted producer source differs from signed verifier expectation"
    );
    Ok(())
}

fn check_scoped_locator_source(
    locator: &ScopedAttemptLocator,
    source: &ProductProducerRecipeSourceIdentity,
) -> Result<()> {
    locator.validate()?;
    source.validate()?;
    ensure!(
        locator.recipe_digest == source.recipe_digest
            && locator.recipe_generation == source.bundle_generation_identity,
        "scoped locator differs from pre-launch admitted producer source"
    );
    Ok(())
}

fn check_scoped_applied_target(
    locator: &ScopedAttemptLocator,
    receipt: &lillux::LinuxSandboxAppliedLaunchReceipt,
) -> Result<()> {
    locator.validate()?;
    ensure!(
        receipt.matches_commitments(&locator.expected_applied_launch),
        "scoped producer applied target differs from prelaunch compiler commitment"
    );
    Ok(())
}

fn check_observed_full_source(
    expected: &ProductProducerRecipeSourceIdentity,
    observed: &ProductProducerRecipeSourceIdentity,
) -> Result<()> {
    expected.validate()?;
    observed.validate()?;
    ensure!(
        observed == expected,
        "scoped observation full source differs from pre-launch admitted source"
    );
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedAttemptLocator {
    schema: String,
    attempt_id: String,
    recipe_digest: String,
    recipe_generation: String,
    scenario_digest: String,
    expected_applied_launch: lillux::LinuxSandboxAppliedLaunchCommitments,
}

impl ScopedAttemptLocator {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == "ryeos.scoped_producer_locator.v2"
                && self.attempt_id.starts_with("scoped-")
                && self.attempt_id.len() == 71
                && self.attempt_id[7..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                && self.scenario_digest == self.attempt_id[7..]
                && lillux::valid_hash(&self.recipe_digest)
                && !self.recipe_generation.is_empty()
                && self.recipe_generation.len() <= 256,
            "scoped child attempt identity is invalid"
        );
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedObservationCut {
    schema: String,
    attempt_id: String,
    launch_owner: serde_json::Value,
    recipe_digest: String,
    recipe_generation: String,
    producer_source: ProductProducerRecipeSourceIdentity,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    relay_handoff: Option<ScopedRelayHandoff>,
    scenario_digest: String,
    process_identity: serde_json::Value,
    scope_recovery: lillux::ProcessScopeRecovery,
    isolation_provenance: serde_json::Value,
    applied_launch: lillux::LinuxSandboxAppliedLaunchReceipt,
    producer_exit_clean: bool,
    natural_empty_receipt_digest: String,
    subprocess_success: bool,
    exit_code: i32,
    timed_out: bool,
    stdout: String,
    stdout_sha256: String,
    stdout_bytes: u64,
    maximum_stdout_bytes: u64,
    stdout_truncated: bool,
    stderr: String,
    stderr_sha256: String,
    stderr_bytes: u64,
    maximum_stderr_bytes: u64,
    stderr_truncated: bool,
    output_limit_exceeded: Option<String>,
    launcher_refusal: Option<String>,
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Evidence collection only. The enclosing daemon scope and verifier parent
/// retain authority over descendants, the provider peer, and frozen outputs.
fn run_scenario_driver(parameters: &Parameters, staged: &staging::StagedNativeProbe) -> Result<()> {
    // The app-server is a direct subordinate of this scoped producer and
    // shares its isolated network namespace. Its separately sandboxed guest
    // command is not the process that sends Responses HTTP requests.
    let relay = scripted_relay::start(
        &parameters.configuration.responses_origin,
        staged.provider_directory()?,
        staged.provider_endpoint_name()?.into(),
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(130)),
    )?;
    let request = staged.prepare_codex_launch(parameters)?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(120));
    let mut app = match AppServerObservation::launch(request, deadline) {
        Ok(app) => app,
        Err(failure) => {
            return Err(failure
                .settle(deadline)
                .map_err(|_| anyhow::anyhow!("app-server launch settlement uncertain"))?);
        }
    };
    let turn = (|| -> Result<(String, String)> {
        app.initialize_scripted()?;
        let thread = app.start_scripted_thread()?;
        let turn_id = app.start_scripted_turn(&thread)?;
        app.await_scripted_turn_completed(&thread, &turn_id)?;
        Ok((thread.thread_id, turn_id))
    })();
    let notifications = app.notifications().to_vec();
    let stop = app.stop();
    let (thread_id, turn_id, exit) = require_clean_driver_stop(turn, stop)?;
    let relay_contacts = relay
        .finish_after_codex_stop(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(5),
        ))
        .map_err(|_| anyhow::anyhow!("scripted relay did not settle after Codex stop"))??;
    let guest_file = staged
        .guest
        .open_pinned_regular(OsStr::new("guest-observation.json"), false)?
        .context("guest observation absent after direct-child stop")?;
    let observed = guest_file.observation()?;
    ensure!(
        observed.size() <= 4 * 1024 * 1024,
        "guest observation exceeds bound"
    );
    let guest_bytes = guest_file.read_stable_bounded(&observed, 4 * 1024 * 1024)?;
    let guest: serde_json::Value = serde_json::from_slice(&guest_bytes)?;
    let transcript = json!({
        "schema": "test.independent_routed_scenario_driver.v1",
        "occurrence": staged.occurrence.path(),
        "request_sha256": staged.request_sha256(),
        "environment_configuration_sha256": staged.environment_configuration_sha256(),
        "thread_id": thread_id,
        "turn_id": turn_id,
        "notifications": notifications,
        "guest_observation": guest,
        "direct_child_exit": {"success": exit.success, "code": exit.code},
        "relay_contacts": relay_contacts,
    });
    let bytes = serde_json::to_vec(&transcript)?;
    ensure!(
        bytes.len() <= 5 * 1024 * 1024,
        "driver transcript exceeds bound"
    );
    std::io::Write::write_all(&mut std::io::stdout().lock(), &bytes)?;
    std::io::Write::write_all(&mut std::io::stdout().lock(), b"\n")?;
    Ok(())
}

fn require_clean_driver_stop(
    turn: Result<(String, String)>,
    stop: Result<AppServerStopOutcome>,
) -> Result<(
    String,
    String,
    lillux::subordinate_process::SubordinateProcessExit,
)> {
    match (turn, stop) {
        (Ok((thread_id, turn_id)), Ok(AppServerStopOutcome::Exited(exit))) if exit.success => {
            Ok((thread_id, turn_id, exit))
        }
        (Err(turn_error), Err(stop_error)) => {
            return Err(turn_error.context(format!(
                "direct Codex child or diagnostic reader also unsettled: {stop_error:#}"
            )));
        }
        (Err(turn_error), _) => return Err(turn_error),
        (_, Err(stop_error)) => {
            return Err(stop_error).context("direct Codex child or diagnostic reader not settled");
        }
        (_, Ok(AppServerStopOutcome::Forced(_))) => {
            bail!("forced Codex child stop cannot produce a scenario transcript");
        }
        (_, Ok(AppServerStopOutcome::Exited(_))) => {
            bail!("Codex child did not exit successfully");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::subordinate_process::SubordinateProcessExit;

    #[test]
    fn observed_isolation_requires_the_complete_prelaunch_class() {
        let expected = json!({
            "mode": "enforce",
            "backend_status": "available",
            "policy_digest": "first",
            "adapter_digest": "second",
            "effective_capabilities": ["filesystem"],
            "plan_digest": null
        });
        let mut observed = expected.clone();
        observed["plan_digest"] = json!(format!("sha256:{}", "a".repeat(64)));
        assert!(check_observed_isolation_class(&expected, &observed).is_ok());
        for field in ["policy_digest", "adapter_digest", "effective_capabilities"] {
            let mut drifted = observed.clone();
            drifted[field] = json!("changed");
            assert!(check_observed_isolation_class(&expected, &drifted).is_err());
        }
        observed["plan_digest"] = serde_json::Value::Null;
        assert!(check_observed_isolation_class(&expected, &observed).is_err());
        observed["plan_digest"] = json!("a".repeat(64));
        assert!(check_observed_isolation_class(&expected, &observed).is_err());
    }

    #[test]
    fn observed_recipe_must_match_prelaunch_signed_bytes_and_stream_bounds() {
        let expected = ProductProducerRecipe::from_value(json!({
            "schema":"ryeos.product_producer_recipe.v3",
            "executable_source":{"kind":"admitted_verifier_executable"},
            "argv":["--scenario-driver"],
            "stdin_source":{"kind":"signed_verifier_parameters"},
            "cwd_source":"verifier_private_workspace",
            "environment_sources":["admitted_realizations"],
            "loopback_ingress":null,
            "bounds":{"maximum_wall_time_ms":170000,
                "maximum_stdout_bytes":6291456,
                "maximum_stderr_bytes":1048576,
                "maximum_memory_bytes":2147483648_u64,
                "maximum_processes":64}
        }))
        .unwrap();
        let digest = expected.digest().unwrap();
        assert!(
            check_observed_recipe_coordinate(
                &expected,
                &digest,
                &digest,
                expected.bounds.maximum_stdout_bytes,
                expected.bounds.maximum_stderr_bytes,
            )
            .is_ok()
        );
        assert!(
            check_observed_recipe_coordinate(
                &expected,
                &"a".repeat(64),
                &digest,
                expected.bounds.maximum_stdout_bytes,
                expected.bounds.maximum_stderr_bytes,
            )
            .is_err()
        );
        let source = ProductProducerRecipeSourceIdentity {
            bundle_generation_identity: "signed-generation-one".into(),
            canonical_ref: "config:fixtures/independent-runtime/scenario-driver".into(),
            raw_content_digest: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            recipe_digest: digest.clone(),
        };
        let locator = ScopedAttemptLocator {
            schema: "ryeos.scoped_producer_locator.v2".into(),
            attempt_id: format!("scoped-{}", "d".repeat(64)),
            recipe_digest: digest.clone(),
            recipe_generation: source.bundle_generation_identity.clone(),
            scenario_digest: "d".repeat(64),
            expected_applied_launch: lillux::LinuxSandboxAppliedLaunchCommitments {
                executable_sha256: [1; 32],
                argv_sha256: [2; 32],
                environment_sha256: [3; 32],
                cwd_sha256: [4; 32],
            },
        };
        assert!(
            check_admitted_producer_source(
                &source,
                "config:fixtures/independent-runtime/scenario-driver",
                &expected,
            )
            .is_ok()
        );
        assert!(check_scoped_locator_source(&locator, &source).is_ok());
        assert!(check_observed_full_source(&source, &source).is_ok());
        assert!(check_admitted_producer_source(&source, "config:other/recipe", &expected).is_err());
        let mut drifted_source = source.clone();
        drifted_source.canonical_ref = "config:other/recipe".into();
        assert!(check_observed_full_source(&source, &drifted_source).is_err());
        let mut drifted_source = source.clone();
        drifted_source.raw_content_digest = "e".repeat(64);
        assert!(check_observed_full_source(&source, &drifted_source).is_err());
        let mut drifted_source = source.clone();
        drifted_source.effective_definition_digest = "e".repeat(64);
        assert!(check_observed_full_source(&source, &drifted_source).is_err());
        let mut drifted_source = source.clone();
        drifted_source.publisher_fingerprint = "e".repeat(64);
        assert!(check_observed_full_source(&source, &drifted_source).is_err());
        let mut moved = locator;
        moved.recipe_generation = "different-generation".into();
        assert!(check_scoped_locator_source(&moved, &source).is_err());
        assert!(
            check_observed_recipe_coordinate(
                &expected,
                &digest,
                &"b".repeat(64),
                expected.bounds.maximum_stdout_bytes,
                expected.bounds.maximum_stderr_bytes,
            )
            .is_err()
        );
        assert!(
            check_observed_recipe_coordinate(
                &expected,
                &digest,
                &digest,
                expected.bounds.maximum_stdout_bytes - 1,
                expected.bounds.maximum_stderr_bytes,
            )
            .is_err()
        );
        assert!(
            check_observed_recipe_coordinate(
                &expected,
                &digest,
                &digest,
                expected.bounds.maximum_stdout_bytes,
                expected.bounds.maximum_stderr_bytes - 1,
            )
            .is_err()
        );
    }

    #[test]
    fn scoped_locator_binds_attempt_to_retained_recipe_coordinate() {
        let valid = json!({
            "schema": "ryeos.scoped_producer_locator.v2",
            "attempt_id": format!("scoped-{}", "a".repeat(64)),
            "recipe_digest": "b".repeat(64),
            "recipe_generation": "signed-generation-one",
            "scenario_digest": "a".repeat(64),
            "expected_applied_launch": {
                "executable_sha256": vec![1; 32],
                "argv_sha256": vec![2; 32],
                "environment_sha256": vec![3; 32],
                "cwd_sha256": vec![4; 32],
            },
        });
        let locator: ScopedAttemptLocator = serde_json::from_value(valid.clone()).unwrap();
        locator.validate().unwrap();
        let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 42,
            namespace_pid: 1,
            effective_uid: 1,
            effective_gid: 1,
            no_new_privs: true,
            seccomp_mode: 2,
            executable_sha256: [1; 32],
            argv_sha256: [2; 32],
            environment_sha256: [3; 32],
            cwd_sha256: [4; 32],
        };
        check_scoped_applied_target(&locator, &receipt).unwrap();
        for mut altered in [
            {
                let mut value = receipt.clone();
                value.executable_sha256[0] ^= 1;
                value
            },
            {
                let mut value = receipt.clone();
                value.argv_sha256[0] ^= 1;
                value
            },
            {
                let mut value = receipt.clone();
                value.environment_sha256[0] ^= 1;
                value
            },
            {
                let mut value = receipt.clone();
                value.cwd_sha256[0] ^= 1;
                value
            },
        ] {
            assert!(check_scoped_applied_target(&locator, &altered).is_err());
            altered.owned_child_pid = 0;
            assert!(check_scoped_applied_target(&locator, &altered).is_err());
        }
        for field in ["recipe_digest", "scenario_digest"] {
            let mut changed = valid.clone();
            changed[field] = json!("wrong");
            assert!(
                serde_json::from_value::<ScopedAttemptLocator>(changed)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{field} drift was accepted"
            );
        }
        let mut missing_generation = valid.clone();
        missing_generation["recipe_generation"] = json!("");
        assert!(
            serde_json::from_value::<ScopedAttemptLocator>(missing_generation)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut legacy = valid;
        legacy.as_object_mut().unwrap().remove("recipe_digest");
        assert!(serde_json::from_value::<ScopedAttemptLocator>(legacy).is_err());
        let mut no_prelaunch_target = json!({
            "schema": "ryeos.scoped_producer_locator.v2",
            "attempt_id": format!("scoped-{}", "a".repeat(64)),
            "recipe_digest": "b".repeat(64),
            "recipe_generation": "signed-generation-one",
            "scenario_digest": "a".repeat(64),
        });
        assert!(
            serde_json::from_value::<ScopedAttemptLocator>(no_prelaunch_target.clone()).is_err()
        );
        no_prelaunch_target["schema"] = json!("ryeos.scoped_producer_locator.v1");
        assert!(serde_json::from_value::<ScopedAttemptLocator>(no_prelaunch_target).is_err());
    }

    #[test]
    fn driver_never_emits_success_after_forced_or_failed_child_stop() {
        let turn = || Ok(("thread".to_owned(), "turn".to_owned()));
        assert!(
            require_clean_driver_stop(
                turn(),
                Ok(AppServerStopOutcome::Forced(SubordinateProcessExit {
                    success: true,
                    code: Some(0),
                })),
            )
            .is_err()
        );
        assert!(
            require_clean_driver_stop(
                turn(),
                Ok(AppServerStopOutcome::Exited(SubordinateProcessExit {
                    success: false,
                    code: Some(1),
                })),
            )
            .is_err()
        );
        assert!(
            require_clean_driver_stop(
                turn(),
                Ok(AppServerStopOutcome::Exited(SubordinateProcessExit {
                    success: true,
                    code: Some(0),
                })),
            )
            .is_ok()
        );
    }
}
