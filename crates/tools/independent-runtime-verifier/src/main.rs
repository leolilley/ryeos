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
    scoped_relay,
    scripted_peer::{RunningScriptedPeer, ScriptedPeer},
    scripted_provider, scripted_relay, staging,
};
use ryeos_runtime::callback::CallbackError;
use ryeos_runtime::callback_uds::UdsRuntimeClient;
use ryeos_runtime::scoped_relay_handoff::ScopedRelayHandoff;
use ryeos_state::external_content::products::producer_recipe::{
    ProductProducerRecipe, ProducerEnvironmentBinding, ProducerEnvironmentSource,
    prepared_directory_mount_destination,
};
use ryeos_state::external_content::products::qualification::ProductProducerRecipeSourceIdentity;
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, ffi::{OsStr, OsString}, io::Read as _, path::Path};

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
    let direct_stage = staging::stage_direct_target_probe(&selected, &project, &parameters)?;
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
    ensure!(
        !race_probe
            || parameters
                .configuration
                .expected_producer_recipe
                .loopback_ingress
                .is_some(),
        "direct scoped race requires the admitted Codex ingress"
    );
    let (start, mut running_relay) = if let Some(ingress) = parameters
        .configuration
        .expected_producer_recipe
        .loopback_ingress
        .as_ref()
    {
        direct_stage.recheck_preflight(&parameters)?;
        let relay_thread = thread_id.clone();
        let relay_source = expected_source.clone();
        let relay_ingress = ingress.clone();
        let relay_origin = parameters.configuration.responses_origin.clone();
        let relay_directory = challenge.directory().try_clone()?;
        let relay_name = socket_name.clone();
        // START cannot acknowledge until the held target's listener is
        // transferred and this verifier returns exact READY. A separate
        // blocking task owns the inherited descriptor while the async UDS
        // request waits; neither task may initiate a second START.
        let receiver = tokio::task::spawn_blocking(move || {
            scoped_relay::receive_and_ack(
                &relay_thread,
                PRODUCER_SCENARIO_ID,
                &relay_source,
                &relay_ingress,
                &relay_origin,
                relay_directory,
                relay_name,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(130)),
            )
        });
        // RESUME is a point read of the same durable attempt. The reserved
        // gate in the daemon test holds START until RESUME has observed that
        // exact attempt; it never authorizes another producer launch.
        let resume_client = if race_probe {
            Some(UdsRuntimeClient::from_env()?)
        } else {
            None
        };
        let (started, resumed, relay) = tokio::join!(
            client.start_scoped_child(&thread_id, PRODUCER_SCENARIO_ID),
            async {
                match resume_client {
                    Some(client) => Some(client.resume_scoped_child(&thread_id).await),
                    None => None,
                }
            },
            receiver,
        );
        let relay = match relay
            .context("scoped relay receiver did not settle")
            .and_then(|result| result)
        {
            Ok(relay) => relay,
            Err(receiver_error) => {
                // START may have released and acknowledged an exact child
                // before the receiver failed. Collect BOTH independently
                // returned locators. A disagreement is never permission to
                // retire just one and declare the whole attempt clean.
                let mut known = returned_scoped_locators(&started, resumed.as_ref());
                let recovery =
                    if known.is_empty() && matches!(started, Err(CallbackError::Transport(_))) {
                        Some(client.resume_scoped_child(&thread_id).await)
                    } else {
                        None
                    };
                if let Some(Ok(value)) = recovery.as_ref() {
                    known.push(value.clone());
                }
                let cleanup = abort_known_scoped_locators(&client, &thread_id, &known).await;
                return Err(receiver_error.context(format!(
                    "direct-target relay receiver failed; exact scoped cleanup={cleanup:?}; point-read recovery={recovery:?}"
                )));
            }
        };
        let started = if let Some(resumed) = resumed {
            let known = returned_scoped_locators(&started, Some(&resumed));
            let (accepted, failure) = reconcile_race_locators(started, resumed);
            if let Some(error) = failure {
                let abort = abort_known_scoped_locators(&client, &thread_id, &known).await;
                let relay_cancel = cancel_scoped_relay(relay);
                return Err(error.context(format!(
                    "direct scoped race failed; exact scoped abort={abort:?}; relay cancellation={relay_cancel}"
                )));
            }
            Ok(accepted.context("direct scoped race lost its accepted locator")?)
        } else {
            started
        };
        (started, Some(relay))
    } else {
        (
            client
                .start_scoped_child(&thread_id, PRODUCER_SCENARIO_ID)
                .await,
            None,
        )
    };
    // A transport error after release is ambiguous. Resume is an exact
    // owner-bound point read, never a second launch or a new scenario choice.
    let locator_result = async {
        let value = match start {
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
        let locator: ScopedAttemptLocator = serde_json::from_value(value)
            .context("scoped child returned a noncanonical locator")?;
        check_scoped_locator_source(&locator, &expected_source)?;
        check_scoped_prepared_immutable(
            &locator,
            &parameters.configuration.scripted_baseline_sha256,
            direct_stage.environment_configuration_sha256(),
        )?;
        if let Some(relay) = &running_relay {
            ensure!(
                relay.handoff().attempt_id == locator.attempt_id,
                "live scoped relay belongs to a different durable attempt"
            );
        }
        Ok::<_, anyhow::Error>(locator)
    }
    .await;
    let locator = match locator_result {
        Ok(locator) => locator,
        Err(error) => {
            if let Some(relay) = running_relay {
                let abort =
                    abort_exact_scoped_child(&client, &thread_id, &relay.handoff().attempt_id)
                        .await;
                let relay_cancel = cancel_scoped_relay(relay);
                return Err(error.context(format!(
                    "direct-target START/locator failed; exact scoped abort={abort:?}; relay cancellation={relay_cancel}"
                )));
            }
            return Err(error);
        }
    };
    let direct_conversation = if running_relay.is_some() {
        let transport =
            ryeos_independent_runtime_verifier::scoped_app_server::ScopedAppServerTransport::new(
                &client,
                &thread_id,
                &locator.attempt_id,
            );
        let mut conversation =
            ryeos_independent_runtime_verifier::scoped_app_server::ScopedAppServerConversation::new(
                transport,
            );
        let result = async {
            let ids = conversation.run_scripted_turn().await?;
            conversation.close_input().await?;
            Ok::<_, anyhow::Error>(ids)
        }
        .await;
        let (codex_thread, codex_turn) = match result {
            Ok(ids) => ids,
            Err(error) => {
                let abort =
                    abort_exact_scoped_child(&client, &thread_id, &locator.attempt_id).await;
                let relay_cancel = running_relay
                    .take()
                    .map(cancel_scoped_relay)
                    .context("direct-target conversation lost its owned relay")?;
                return Err(error.context(format!(
                    "direct-target conversation failed; exact scoped abort={abort:?}; relay cancellation={relay_cancel}"
                )));
            }
        };
        Some((
            codex_thread,
            codex_turn,
            conversation.notifications().to_vec(),
        ))
    } else {
        None
    };
    let observed = match client
        .observe_scoped_child(&thread_id, &locator.attempt_id)
        .await
    {
        Ok(observed) => observed,
        Err(error) if running_relay.is_some() => {
            // A lost observation response might already have committed a
            // natural result. The exact abort CAS refuses in that case; it
            // cannot turn the committed observation into cleanup evidence.
            let abort = abort_exact_scoped_child(&client, &thread_id, &locator.attempt_id).await;
            let relay_cancel = running_relay
                .take()
                .map(cancel_scoped_relay)
                .context("direct-target observation lost its owned relay")?;
            return Err(anyhow::Error::new(error).context(format!(
                "direct-target observation failed; exact scoped abort={abort:?}; relay cancellation={relay_cancel}"
            )));
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context("scoped producer observation refused"));
        }
    };
    let observation: ScopedObservationCut = match serde_json::from_value(observed) {
        Ok(observation) => observation,
        Err(error) => {
            if let Some(relay) = running_relay.take() {
                // The observation may already have committed a natural
                // result. Abort is an exact CAS, never proof of cleanliness.
                let abort =
                    abort_exact_scoped_child(&client, &thread_id, &locator.attempt_id).await;
                let relay_cancel = cancel_scoped_relay(relay);
                return Err(anyhow::Error::new(error).context(format!(
                    "direct-target observation envelope invalid; exact scoped abort={abort:?}; relay cancellation={relay_cancel}"
                )));
            }
            return Err(error).context("scoped producer returned an invalid observation envelope");
        }
    };
    let direct_evidence = if let Some(relay) = running_relay {
        if observation.relay_handoff.as_ref() != Some(relay.handoff()) {
            let relay_cancel = cancel_scoped_relay(relay);
            bail!(
                "live scoped relay handoff differs from settled daemon observation; relay cancellation={relay_cancel}"
            );
        }
        let contacts = match relay.finish_after_target_settlement(
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
        ) {
            Ok(result) => result?,
            Err(relay) => {
                // The first timeout retains the live task and authenticated
                // channel. Interrupt and join explicitly; even a successful
                // cancellation is not contact or qualification evidence.
                let cancelled = relay.cancel_until(lillux::time::MonotonicDeadline::after(
                    lillux::time::Duration::from_secs(5),
                ));
                match cancelled {
                    Ok(_) => bail!("direct-target relay needed cancellation after scope death"),
                    Err(unsettled) => {
                        // Drop remains a last-resort interrupting join, not a
                        // clean result. The verifier must not return while a
                        // provider relay task can still own its sockets.
                        drop(unsettled);
                        bail!("direct-target relay remained unsettled after cancellation")
                    }
                }
            }
        };
        ensure!(
            contacts == scripted_provider::REQUEST_COUNT,
            "direct-target relay contact count differs from scripted provider"
        );
        let (codex_thread, codex_turn, notifications) =
            direct_conversation.context("direct-target Codex conversation was not collected")?;
        ensure!(
            !codex_thread.is_empty() && !codex_turn.is_empty() && !notifications.is_empty(),
            "direct-target Codex conversation is incomplete"
        );
        Some((codex_thread, codex_turn, notifications, contacts))
    } else {
        None
    };
    check_observed_full_source(&expected_source, &observation.producer_source)?;
    check_observed_isolation_class(&expected_isolation_class, &observation.isolation_provenance)?;
    check_scoped_plan_coordinate(&locator, &observation.isolation_provenance)?;
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
    check_scoped_effective_environment(
        expected_recipe,
        &realizations,
        &observation.applied_launch,
    )?;
    ensure!(
        observation.schema == "ryeos.scoped_producer_observation.v6"
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
            && observation.prepared_immutable_sha256 == locator.prepared_immutable_sha256
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
    if let Some((codex_thread, codex_turn, notifications, contacts)) = direct_evidence {
        let requests = finish_scripted_provider(provider)?;
        ensure!(
            requests.len() == contacts,
            "direct target relay and provider observed different contact counts"
        );
        challenge.recheck()?;
        provider_endpoint.recheck()?;
        let expected_canary = format!("{}\n", challenge.value());
        scripted_provider::check_requests(
            &requests,
            expected_canary.as_bytes(),
            expected_canary.as_bytes(),
            staging::CONTROLLER_CANARY_DENIAL,
        )?;
        let commands = challenge.scripted_canary_commands()?;
        let guest_command = scripted_provider::GUEST_COMMAND_SCRIPT;
        let routing = RoutingScenario {
            thread_id: codex_thread,
            turn_id: codex_turn,
            guest_cwd: "/workspace".into(),
            local_refusal_command: commands.forbidden_local_write,
            guest_command_script: guest_command.into(),
            secret_read_script: commands.guest_read.clone(),
            patch_input: scripted_provider::PATCH_INPUT.into(),
            guest_command: format!("{} -c '{guest_command}'", scripted_provider::GUEST_SHELL),
            secret_read_command: format!(
                "{} -c '{}'",
                scripted_provider::GUEST_SHELL,
                commands.guest_read
            ),
            candidate_path: scripted_provider::CANDIDATE_PATH.into(),
            candidate_added_content: scripted_provider::CANDIDATE_CONTENT.into(),
            expected_command_output: parameters.configuration.expected_command_output.clone(),
            secret_read_denial: staging::CONTROLLER_CANARY_DENIAL.into(),
            controller_canary_value: challenge.value().to_owned(),
        };
        check_notifications(&routing, &notifications)?;
        ensure!(
            direct_stage.request_sha256() == expected_request_sha256,
            "direct prepared request differs from selected signed request"
        );
        let frozen = direct_stage.inspect_frozen_after_scope_empty(&parameters)?;
        ensure!(
            frozen.environment_configuration_sha256
                == direct_stage.environment_configuration_sha256()
                && frozen.candidate_sha256
                    == lillux::sha256_hex(scripted_provider::CANDIDATE_CONTENT.as_bytes()),
            "direct frozen configuration or candidate differs from prepared expectation"
        );
        native_guest::check_applied_receipt_against_signed_request(
            &frozen.guest_observation,
            &expected_request,
        )?;
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
                "direct scoped START/RESUME locators, applied environment and frozen candidate joined, but complete qualification evidence remains unproven"
            );
        }
        bail!(
            "direct-target launch, applied environment, relay, conversation and frozen candidate joined, but complete qualification evidence remains unproven"
        );
    }
    let transcript: serde_json::Value = serde_json::from_str(&observation.stdout)
        .context("scoped producer stdout is not a bounded scenario transcript")?;
    ensure!(
        transcript["schema"] == "test.independent_routed_scenario_driver.v1"
            && transcript["request_sha256"] == expected_request_sha256
            && transcript["relay_contacts"] == scripted_provider::REQUEST_COUNT,
        "scoped producer returned a different scenario transcript"
    );
    let requests = finish_scripted_provider(provider)?;
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
    bail!(
        "scoped producer and frozen files settled; Codex and guest records are internally checked, but collector identity, isolation, terminal and runtime provenance is incomplete; no qualification claims issued"
    )
}

async fn abort_exact_scoped_child(
    client: &UdsRuntimeClient,
    thread_id: &str,
    attempt_id: &str,
) -> Result<()> {
    let mut response = client.abort_scoped_child(thread_id, attempt_id).await;
    if matches!(response, Err(CallbackError::Transport(_))) {
        response = client.abort_scoped_child(thread_id, attempt_id).await;
    }
    let response = response?;
    ensure!(
        response["schema"] == "ryeos.scoped_child_abort.v1"
            && response["attempt_id"] == attempt_id
            && response["settlement"] == "retired_cleanup_only",
        "exact scoped abort returned an invalid settlement acknowledgment"
    );
    Ok(())
}

/// Cleanup every independently returned locator for this root. The daemon
/// remains the authority for whether each ID belongs to the admitted owner;
/// no malformed or unequal reply can be silently converted to clean success.
async fn abort_known_scoped_locators(
    client: &UdsRuntimeClient,
    thread_id: &str,
    values: &[serde_json::Value],
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut outcomes = Vec::with_capacity(values.len());
    for value in values.iter().take(2) {
        match serde_json::from_value::<ScopedAttemptLocator>(value.clone()) {
            Ok(locator) => {
                if let Err(error) = locator.validate() {
                    outcomes.push(format!("invalid locator: {error}"));
                } else if seen.insert(locator.attempt_id.clone()) {
                    let result =
                        abort_exact_scoped_child(client, thread_id, &locator.attempt_id).await;
                    outcomes.push(format!("{}: {result:?}", locator.attempt_id));
                }
            }
            Err(error) => outcomes.push(format!("noncanonical locator: {error}")),
        }
    }
    outcomes
}

fn cancel_scoped_relay(relay: scoped_relay::RunningScopedRelay) -> String {
    match relay.cancel_until(lillux::time::MonotonicDeadline::after(
        lillux::time::Duration::from_secs(5),
    )) {
        Ok(result) => format!("{result:?}"),
        Err(unsettled) => {
            // This is NOT a five-second wall-clock bound: Drop performs the
            // final interrupting join and may wait longer. Returning with a
            // live provider relay would be an unsafe false settlement.
            drop(unsettled);
            "relay remained unsettled after cancellation".to_owned()
        }
    }
}

fn finish_scripted_provider(provider: RunningScriptedPeer) -> Result<Vec<serde_json::Value>> {
    match provider.finish_after_producer_settlement(lillux::time::MonotonicDeadline::after(
        lillux::time::Duration::from_secs(10),
    )) {
        Ok(result) => result,
        Err(provider) => {
            match provider.cancel_until(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(5),
            )) {
                Ok(_) => bail!("scripted provider needed cancellation after producer settlement"),
                Err(unsettled) => {
                    // Never promote an unfinished provider task to a contact
                    // result. Its final interrupting Drop join retains the
                    // socket owner until it actually stops.
                    drop(unsettled);
                    bail!("scripted provider remained unsettled after cancellation")
                }
            }
        }
    }
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

fn check_scoped_prepared_immutable(
    locator: &ScopedAttemptLocator,
    baseline_sha256: &str,
    environment_sha256: &str,
) -> Result<()> {
    locator.validate()?;
    ensure!(
        lillux::valid_hash(baseline_sha256) && lillux::valid_hash(environment_sha256),
        "verifier immutable input hashes are invalid"
    );
    let home = ryeos_state::external_content::products::producer_recipe::prepared_directory_mount_destination(
        staging::DIRECT_HOME_ID,
    )?;
    let expected = BTreeMap::from([
        (home.join("config.toml").to_string_lossy().into_owned(), baseline_sha256.to_owned()),
        (home.join("environments.toml").to_string_lossy().into_owned(), environment_sha256.to_owned()),
    ]);
    ensure!(
        locator.prepared_immutable_sha256 == expected,
        "daemon-sealed producer config differs from verifier-derived exact bytes"
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
    ensure!(
        receipt.matches_post_release_mounts(&locator.expected_mount_preparation)
            && locator.held_mount_preparation.matches_commitments(&locator.expected_mount_preparation)
            && receipt.owned_child_pid == locator.held_mount_preparation.owned_child_pid,
        "scoped producer applied mounts differ from held final-root preparation"
    );
    Ok(())
}

/// Derive the complete direct Codex environment from the signed finite recipe
/// and the verifier's separately admitted realization set. The daemon's
/// prelaunch commitment alone is not an independent expectation of its env.
fn check_scoped_effective_environment(
    recipe: &ProductProducerRecipe,
    admitted_realizations: &str,
    receipt: &lillux::LinuxSandboxAppliedLaunchReceipt,
) -> Result<()> {
    let environment = signed_direct_environment(recipe, admitted_realizations)?;
    // Lillux exposes the canonical applied-env commitment through the same
    // target projection as its pre-exec receipt. Other target fields below are
    // inert placeholders: compare only environment_sha256.
    let executable = std::path::Path::new("/bin/true");
    let argv0 = OsString::from("/bin/true");
    let cwd = std::path::Path::new("/");
    let expected = lillux::LinuxSandboxAppliedLaunchCommitments::from_target(
        lillux::LinuxSandboxAppliedLaunchTarget {
            executable,
            argv0: &argv0,
            arguments: &[],
            cwd,
            environment: &environment,
        },
    ).map_err(anyhow::Error::msg)?;
    ensure!(
        receipt.environment_sha256 == expected.environment_sha256,
        "direct Codex applied environment differs from signed recipe and admitted realizations"
    );
    Ok(())
}

fn signed_direct_environment(
    recipe: &ProductProducerRecipe,
    admitted_realizations: &str,
) -> Result<BTreeMap<OsString, OsString>> {
    recipe.validate()?;
    ensure!(
        recipe.environment_sources == [ProducerEnvironmentSource::AdmittedRealizations],
        "direct Codex recipe has an unexpected environment source"
    );
    let mut environment = BTreeMap::<OsString, OsString>::new();
    environment.insert(
        OsString::from("RYEOS_EXTERNAL_REALIZATIONS"),
        OsString::from(admitted_realizations),
    );
    for (name, binding) in &recipe.environment_bindings {
        let value = match binding {
            ProducerEnvironmentBinding::Literal { value } => OsString::from(value),
            ProducerEnvironmentBinding::PreparedDirectory { id } => {
                prepared_directory_mount_destination(id)?.into_os_string()
            }
            ProducerEnvironmentBinding::VerifierPrivateWorkspace => {
                bail!("direct Codex recipe cannot inherit a verifier-private path")
            }
        };
        ensure!(
            environment.insert(OsString::from(name), value).is_none(),
            "direct Codex environment binding is duplicated"
        );
    }
    environment.insert(OsString::from("TMPDIR"), OsString::from("/tmp"));
    ensure!(
        environment.insert(
            OsString::from("RYEOS_PRODUCER_STDIN_FD"),
            OsString::from("0"),
        ).is_none(),
        "direct Codex recipe collides with its protected channel"
    );
    Ok(environment)
}

fn check_scoped_plan_coordinate(
    locator: &ScopedAttemptLocator,
    observed: &serde_json::Value,
) -> Result<()> {
    locator.validate()?;
    ensure!(
        observed["plan_digest"].as_str() == Some(locator.isolation_plan_digest.as_str()),
        "scoped isolation plan differs from exact prelaunch locator"
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

/// Preserve both independently returned exact locators for cleanup. A
/// divergent RESUME response must not disappear behind START precedence.
fn returned_scoped_locators(
    started: &std::result::Result<serde_json::Value, CallbackError>,
    resumed: Option<&std::result::Result<serde_json::Value, CallbackError>>,
) -> Vec<serde_json::Value> {
    let mut known = Vec::with_capacity(2);
    if let Ok(value) = started {
        known.push(value.clone());
    }
    if let Some(Ok(value)) = resumed {
        known.push(value.clone());
    }
    known
}

/// Reconcile only the exact point read with START. A lost START reply may use
/// the already-retained RESUME locator; a definite START refusal or unequal
/// locators cannot be converted into success.
fn reconcile_race_locators(
    started: std::result::Result<serde_json::Value, CallbackError>,
    resumed: std::result::Result<serde_json::Value, CallbackError>,
) -> (Option<serde_json::Value>, Option<anyhow::Error>) {
    match (started, resumed) {
        (Ok(started), Ok(resumed)) if started == resumed => (Some(started), None),
        (Err(CallbackError::Transport(_)), Ok(resumed)) => (Some(resumed), None),
        (Ok(started), Ok(_)) => (
            Some(started),
            Some(anyhow::anyhow!(
                "direct scoped START and RESUME locators differ"
            )),
        ),
        (Ok(started), Err(error)) => (
            Some(started),
            Some(
                anyhow::Error::new(error)
                    .context("direct scoped RESUME refused the reserved attempt"),
            ),
        ),
        (Err(error), Ok(resumed)) => (
            Some(resumed),
            Some(anyhow::anyhow!(
                "direct scoped START refused after exact RESUME: {error}"
            )),
        ),
        (Err(start_error), Err(resume_error)) => (
            None,
            Some(anyhow::anyhow!(
                "direct scoped START and RESUME both refused: start={start_error}; resume={resume_error}"
            )),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedAttemptLocator {
    schema: String,
    attempt_id: String,
    recipe_digest: String,
    recipe_generation: String,
    scenario_digest: String,
    isolation_plan_digest: String,
    expected_applied_launch: lillux::LinuxSandboxAppliedLaunchCommitments,
    expected_mount_preparation: lillux::LinuxSandboxMountPreparationCommitments,
    held_mount_preparation: lillux::LinuxSandboxMountPreparationReceipt,
    prepared_immutable_sha256: BTreeMap<String, String>,
}

impl ScopedAttemptLocator {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == "ryeos.scoped_producer_locator.v5"
                && self.attempt_id.starts_with("scoped-")
                && self.attempt_id.len() == 71
                && self.attempt_id[7..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                && self.scenario_digest == self.attempt_id[7..]
                && lillux::valid_hash(&self.recipe_digest)
                && self
                    .isolation_plan_digest
                    .strip_prefix("sha256:")
                    .is_some_and(lillux::valid_hash)
                && !self.recipe_generation.is_empty()
                && self.recipe_generation.len() <= 256
                && self.expected_mount_preparation.mount_count > 0
                && self.held_mount_preparation.matches_commitments(&self.expected_mount_preparation)
                && self.prepared_immutable_sha256.len()
                    <= ryeos_state::external_content::products::producer_recipe::MAX_PRODUCER_PREPARED_IMMUTABLE_FILES
                && self.prepared_immutable_sha256.values().all(|digest| lillux::valid_hash(digest)),
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
    prepared_immutable_sha256: BTreeMap<String, String>,
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
    fn signed_direct_environment_matches_only_the_complete_applied_environment() {
        let recipe = ProductProducerRecipe::from_value(json!({
            "schema":"ryeos.product_producer_recipe.v5",
            "executable_source":{
                "kind":"admitted_realization_member",
                "realization_id":"subject",
                "manifest_hash":"a".repeat(64),
                "relative_path":"bin/codex",
                "executable_sha256":"b".repeat(64)
            },
            "argv":["app-server"],
            "stdin_source":{
                "kind":"interactive_verifier_channel",
                "maximum_frame_bytes":1024,
                "maximum_total_bytes":4096,
                "maximum_frames":4
            },
            "cwd_source":{"kind":"prepared_directory","id":"codex-occurrence"},
            "environment_sources":["admitted_realizations"],
            "environment_bindings":{
                "CODEX_HOME":{"kind":"prepared_directory","id":"codex-home"},
                "HOME":{"kind":"prepared_directory","id":"codex-home"},
                "PATH":{"kind":"literal","value":""},
                "LANG":{"kind":"literal","value":"C"},
                "LC_ALL":{"kind":"literal","value":"C"}
            },
            "prepared_immutable_files":[],
            "loopback_ingress":null,
            "bounds":{
                "maximum_wall_time_ms":1000,
                "maximum_stdout_bytes":1024,
                "maximum_stderr_bytes":1024,
                "maximum_memory_bytes":1048576,
                "maximum_processes":4
            }
        })).unwrap();
        let admitted = "[{\"id\":\"signed\"}]";
        let environment = signed_direct_environment(&recipe, admitted).unwrap();
        assert_eq!(environment.len(), 8);
        assert_eq!(environment.get(OsStr::new("RYEOS_EXTERNAL_REALIZATIONS")),
            Some(&OsString::from(admitted)));
        assert_eq!(environment.get(OsStr::new("CODEX_HOME")),
            Some(&OsString::from("/ryeos/producer-prepared/codex-home")));
        assert_eq!(environment.get(OsStr::new("RYEOS_PRODUCER_STDIN_FD")),
            Some(&OsString::from("0")));
        let executable = Path::new("/bin/true");
        let argv0 = OsString::from("/bin/true");
        let cwd = Path::new("/");
        let commitment = lillux::LinuxSandboxAppliedLaunchCommitments::from_target(
            lillux::LinuxSandboxAppliedLaunchTarget {
                executable,
                argv0: &argv0,
                arguments: &[],
                cwd,
                environment: &environment,
            },
        ).unwrap();
        let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid:42, namespace_pid:1, effective_uid:1, effective_gid:1,
            no_new_privs:true, seccomp_mode:2,
            executable_sha256:[1;32], argv_sha256:[2;32],
            environment_sha256:commitment.environment_sha256, cwd_sha256:[3;32],
            post_release_mount_view:lillux::LinuxSandboxMountPreparationCommitments {
                schema:1, mount_count:1, destination_access_sha256:[0;32],
            },
        };
        check_scoped_effective_environment(&recipe, admitted, &receipt).unwrap();
        assert!(check_scoped_effective_environment(&recipe, "different", &receipt).is_err());
        let mut changed = receipt.clone();
        changed.environment_sha256[0] ^= 1;
        assert!(check_scoped_effective_environment(&recipe, admitted, &changed).is_err());
        let mut altered_recipe = recipe.clone();
        altered_recipe.environment_bindings.insert(
            "LANG".into(), ProducerEnvironmentBinding::Literal { value:"POSIX".into() }
        );
        assert!(check_scoped_effective_environment(&altered_recipe, admitted, &receipt).is_err());
        altered_recipe.environment_sources.clear();
        assert!(check_scoped_effective_environment(&altered_recipe, admitted, &receipt).is_err());
    }

    #[test]
    fn race_reconciliation_accepts_only_one_exact_attempt_or_lost_ack() {
        let locator = json!({"attempt_id":"scoped-exact"});
        let other = json!({"attempt_id":"scoped-other"});
        let (accepted, failure) = reconcile_race_locators(Ok(locator.clone()), Ok(locator.clone()));
        assert_eq!(accepted, Some(locator.clone()));
        assert!(failure.is_none());

        let (accepted, failure) = reconcile_race_locators(
            Err(CallbackError::Transport(anyhow::anyhow!("reply lost"))),
            Ok(locator.clone()),
        );
        assert_eq!(accepted, Some(locator.clone()));
        assert!(failure.is_none());

        let (accepted, failure) = reconcile_race_locators(Ok(locator.clone()), Ok(other));
        assert_eq!(accepted, Some(locator.clone()));
        assert!(failure.unwrap().to_string().contains("locators differ"));

        let (accepted, failure) = reconcile_race_locators(
            Err(CallbackError::ActionFailed {
                code: "refused".into(),
                message: "definite".into(),
                retryable: false,
            }),
            Ok(locator.clone()),
        );
        assert_eq!(accepted, Some(locator));
        assert!(failure.unwrap().to_string().contains("START refused"));

        let divergent = returned_scoped_locators(
            &Ok(json!({"attempt_id":"scoped-first"})),
            Some(&Ok(json!({"attempt_id":"scoped-second"}))),
        );
        assert_eq!(divergent.len(), 2);
        assert_ne!(divergent[0], divergent[1]);
    }

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
            "schema":"ryeos.product_producer_recipe.v5",
            "executable_source":{"kind":"admitted_verifier_executable"},
            "argv":["--scenario-driver"],
            "stdin_source":{"kind":"signed_verifier_parameters"},
            "cwd_source":{"kind":"verifier_private_workspace"},
            "environment_sources":["admitted_realizations"],
            "environment_bindings":{},
            "prepared_immutable_files":[],
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
            schema: "ryeos.scoped_producer_locator.v5".into(),
            attempt_id: format!("scoped-{}", "d".repeat(64)),
            recipe_digest: digest.clone(),
            recipe_generation: source.bundle_generation_identity.clone(),
            scenario_digest: "d".repeat(64),
            isolation_plan_digest: format!("sha256:{}", "e".repeat(64)),
            expected_applied_launch: lillux::LinuxSandboxAppliedLaunchCommitments {
                executable_sha256: [1; 32],
                argv_sha256: [2; 32],
                environment_sha256: [3; 32],
                cwd_sha256: [4; 32],
            },
            expected_mount_preparation: lillux::LinuxSandboxMountPreparationCommitments {
                schema: 1,
                mount_count: 1,
                destination_access_sha256: [0; 32],
            },
            held_mount_preparation: lillux::LinuxSandboxMountPreparationReceipt {
                schema: 1,
                owned_child_pid: 42,
                mount_count: 1,
                destination_access_sha256: [0; 32],
            },
            prepared_immutable_sha256: BTreeMap::new(),
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
            "schema": "ryeos.scoped_producer_locator.v5",
            "attempt_id": format!("scoped-{}", "a".repeat(64)),
            "recipe_digest": "b".repeat(64),
            "recipe_generation": "signed-generation-one",
            "scenario_digest": "a".repeat(64),
            "isolation_plan_digest": format!("sha256:{}", "c".repeat(64)),
            "expected_applied_launch": {
                "executable_sha256": vec![1; 32],
                "argv_sha256": vec![2; 32],
                "environment_sha256": vec![3; 32],
                "cwd_sha256": vec![4; 32],
            },
            "expected_mount_preparation": {
                "schema": 1,
                "mount_count": 1,
                "destination_access_sha256": vec![0; 32],
            },
            "held_mount_preparation": {
                "schema": 1,
                "owned_child_pid": 42,
                "mount_count": 1,
                "destination_access_sha256": vec![0; 32],
            },
            "prepared_immutable_sha256": {},
        });
        let locator: ScopedAttemptLocator = serde_json::from_value(valid.clone()).unwrap();
        locator.validate().unwrap();
        let home = ryeos_state::external_content::products::producer_recipe::prepared_directory_mount_destination(
            staging::DIRECT_HOME_ID,
        )
        .unwrap();
        let mut exact_files = valid.clone();
        exact_files["prepared_immutable_sha256"] = json!(BTreeMap::from([
            (home.join("config.toml").to_string_lossy().into_owned(), "1".repeat(64)),
            (home.join("environments.toml").to_string_lossy().into_owned(), "2".repeat(64)),
        ]));
        let exact: ScopedAttemptLocator = serde_json::from_value(exact_files.clone()).unwrap();
        check_scoped_prepared_immutable(&exact, &"1".repeat(64), &"2".repeat(64)).unwrap();
        let config_path = home.join("config.toml").to_string_lossy().into_owned();
        let environment_path = home.join("environments.toml").to_string_lossy().into_owned();
        for changed in [
            {
                let mut value = exact_files.clone();
                value["prepared_immutable_sha256"][&config_path] = json!("3".repeat(64));
                value
            },
            {
                let mut value = exact_files.clone();
                value["prepared_immutable_sha256"]
                    .as_object_mut().unwrap().remove(&environment_path);
                value
            },
            {
                let mut value = exact_files.clone();
                value["prepared_immutable_sha256"][home.join("extra.toml").to_string_lossy().as_ref()] =
                    json!("4".repeat(64));
                value
            },
            {
                let mut value = exact_files.clone();
                value["prepared_immutable_sha256"][&config_path] = json!("2".repeat(64));
                value["prepared_immutable_sha256"][&environment_path] = json!("1".repeat(64));
                value
            },
        ] {
            let changed: ScopedAttemptLocator = serde_json::from_value(changed).unwrap();
            assert!(check_scoped_prepared_immutable(&changed, &"1".repeat(64), &"2".repeat(64)).is_err());
        }
        exact_files["schema"] = json!("ryeos.scoped_producer_locator.v4");
        assert!(serde_json::from_value::<ScopedAttemptLocator>(exact_files)
            .unwrap().validate().is_err());
        assert!(
            check_scoped_plan_coordinate(
                &locator,
                &json!({"plan_digest": locator.isolation_plan_digest.clone()})
            )
            .is_ok()
        );
        assert!(
            check_scoped_plan_coordinate(
                &locator,
                &json!({"plan_digest": format!("sha256:{}", "d".repeat(64))})
            )
            .is_err()
        );
        assert!(check_scoped_plan_coordinate(&locator, &json!({})).is_err());
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
            post_release_mount_view: lillux::LinuxSandboxMountPreparationCommitments {
                schema: 1,
                mount_count: 1,
                destination_access_sha256: [0; 32],
            },
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
            {
                let mut value = receipt.clone();
                value.post_release_mount_view.destination_access_sha256[0] ^= 1;
                value
            },
        ] {
            assert!(check_scoped_applied_target(&locator, &altered).is_err());
            altered.owned_child_pid = 0;
            assert!(check_scoped_applied_target(&locator, &altered).is_err());
        }
        let mut wrong_held = valid.clone();
        wrong_held["held_mount_preparation"]["destination_access_sha256"][0] = json!(1);
        assert!(serde_json::from_value::<ScopedAttemptLocator>(wrong_held)
            .unwrap()
            .validate()
            .is_err());
        for (field, value) in [
            ("schema", json!(2)),
            ("mount_count", json!(2)),
            ("owned_child_pid", json!(43)),
        ] {
            let mut changed = valid.clone();
            changed["held_mount_preparation"][field] = value;
            let changed: ScopedAttemptLocator = serde_json::from_value(changed).unwrap();
            if field == "owned_child_pid" {
                assert!(check_scoped_applied_target(&changed, &receipt).is_err());
            } else {
                assert!(changed.validate().is_err());
            }
        }
        let mut missing_mounts = valid.clone();
        missing_mounts.as_object_mut().unwrap().remove("expected_mount_preparation");
        assert!(serde_json::from_value::<ScopedAttemptLocator>(missing_mounts).is_err());
        for field in ["recipe_digest", "scenario_digest", "isolation_plan_digest"] {
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
            "schema": "ryeos.scoped_producer_locator.v5",
            "attempt_id": format!("scoped-{}", "a".repeat(64)),
            "recipe_digest": "b".repeat(64),
            "recipe_generation": "signed-generation-one",
            "scenario_digest": "a".repeat(64),
            "isolation_plan_digest": format!("sha256:{}", "c".repeat(64)),
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
