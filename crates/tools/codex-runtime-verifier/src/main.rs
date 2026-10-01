//! Bundle Tool entrypoint for the exact daemon-owned direct-target diagnostic.
//! Its result proposes finite claims; RyeOS must independently join the
//! scoped attempt and verifier terminal before publishing qualification.

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_codex_runtime_verifier::{
    DIRECT_QUALIFICATION_CLAIMS, INPUT_LIMIT, Parameters,
    guest_observation::{GuestScenario, check_guest_observation},
    native_guest,
    routing_observation::{RoutingScenario, check_notifications},
    scoped_relay,
    scripted_peer::{RunningScriptedPeer, ScriptedPeer},
    scripted_provider, staging,
};
use ryeos_runtime::callback::CallbackError;
use ryeos_runtime::callback_uds::UdsRuntimeClient;
use ryeos_runtime::scoped_relay_handoff::ScopedRelayHandoff;
use ryeos_state::external_content::products::producer_recipe::{
    ProducerCwdSource, ProducerEnvironmentBinding, ProducerEnvironmentSource,
    ProducerExecutableSource, ProductProducerRecipe, prepared_directory_mount_destination,
};
use ryeos_state::external_content::products::qualification::{
    PRODUCT_QUALIFICATION_RESULT_SCHEMA, ProductProducerRecipeSourceIdentity,
    ProductQualificationResult,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io::Read as _,
    path::Path,
};

enum VerifierExit {
    Refused(anyhow::Error),
    ConsumerUncertain(ryeos_codex_runtime_verifier::consumer_observer::ConsumerObservationFailure),
}

impl From<anyhow::Error> for VerifierExit {
    fn from(error: anyhow::Error) -> Self {
        Self::Refused(error)
    }
}

impl std::fmt::Debug for VerifierExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(error) => f.debug_tuple("Refused").field(error).finish(),
            Self::ConsumerUncertain(failure) => {
                f.debug_tuple("ConsumerUncertain").field(failure).finish()
            }
        }
    }
}

fn main() -> std::result::Result<(), VerifierExit> {
    use ryeos_codex_runtime_verifier::consumer_observer::{
        CONSUMER_STARTUP_FLAG, decode_startup_arguments, prepare_startup,
    };
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(CONSUMER_STARTUP_FLAG)) {
        return ordinary_main().map_err(VerifierExit::Refused);
    }
    if std::env::var_os(
        ryeos_codex_runtime_verifier::consumer_record::CONSUMER_NATIVE_CHALLENGE_ENV,
    )
    .is_some()
        || std::env::var_os(ryeos_codex_runtime_verifier::consumer_outer_owner::OUTER_CHALLENGE_ENV)
            .is_some()
    {
        return Err(VerifierExit::Refused(anyhow::anyhow!(
            "consumer observer conflicts with another startup role"
        )));
    }
    let challenge = decode_startup_arguments(std::env::args_os().skip(1))?;
    let (deadline, cleanup_deadline) = native_guest::production_deadlines(
        challenge.intent.attempt_deadline_ms,
        lillux::time::timestamp_millis(),
    )?;
    // SAFETY: exclusive synchronous startup, before stdio, privilege transition,
    // async runtimes or threads. No Rust owner or other actor aliases stdout.
    let mut output = unsafe {
        lillux::inherited_pipes::InheritedPipeOutput::take_inherited_output(1, 4096, deadline)
    }
    .context("consumer evidence output is not an owned write pipe")?;
    let prepared = prepare_startup(&challenge, deadline, cleanup_deadline)?;
    let bytes = match prepared.observe(&challenge) {
        Ok(bytes) => bytes,
        Err(failure) => match failure.settle() {
            Ok(error) => return Err(VerifierExit::Refused(error)),
            // Preserve live ownership through terminal refusal. Last-resort
            // Drop is cleanup, never namespace/provider-death testimony.
            Err(failure) => return Err(VerifierExit::ConsumerUncertain(failure)),
        },
    };
    let deadline = deadline.min(prepared.active_deadline);
    for chunk in bytes.chunks(4096) {
        output
            .write_all(chunk, deadline)
            .context("consumer evidence delivery failed; no resend")?;
    }
    output
        .write_all(b"\n", deadline)
        .context("consumer evidence terminator delivery failed; no resend")?;
    Ok(())
}

fn ordinary_main() -> Result<()> {
    let native_challenge = std::env::var_os(
        ryeos_codex_runtime_verifier::consumer_record::CONSUMER_NATIVE_CHALLENGE_ENV,
    );
    let outer_challenge =
        std::env::var_os(ryeos_codex_runtime_verifier::consumer_outer_owner::OUTER_CHALLENGE_ENV);
    ensure!(
        native_challenge.is_none() || outer_challenge.is_none(),
        "consumer native and outer startup roles conflict"
    );
    if let Some(encoded) = outer_challenge {
        ensure!(
            std::env::args_os().skip(1).next().is_none(),
            "consumer outer environment conflicts with CLI arguments"
        );
        return ryeos_codex_runtime_verifier::consumer_outer_owner::run_startup(
            encoded.to_str().context("outer challenge is not UTF-8")?,
        );
    }
    if let Some(encoded) = native_challenge {
        ensure!(
            std::env::args_os().skip(1).next().is_none(),
            "consumer native environment conflicts with CLI arguments"
        );
        return native_guest::run_production_probe(
            encoded
                .to_str()
                .context("consumer challenge is not UTF-8")?,
        );
    }
    // Native namespace preparation must run before creating async runtime or
    // helper threads. Ordinary admitted verification retains its prior loop.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(admitted_main())
}

async fn admitted_main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next();
    ensure!(args.next().is_none(), "unexpected verifier arguments");
    ensure!(
        mode.is_none()
            || mode.as_deref() == Some(OsStr::new("--scoped-resume-race-probe"))
            || mode.as_deref() == Some(OsStr::new("--consumer-controller")),
        "unsupported verifier mode"
    );
    let mut input = Vec::new();
    std::io::stdin()
        .take((INPUT_LIMIT + 1) as u64)
        .read_to_end(&mut input)?;
    ensure!(input.len() <= INPUT_LIMIT, "verifier input exceeds bound");
    if mode.as_deref() == Some(OsStr::new("--consumer-controller")) {
        return run_consumer_controller(&input).await;
    }
    let parameters = Parameters::parse(&input)?;
    ensure!(
        parameters
            .configuration
            .expected_producer_recipe
            .loopback_ingress
            .is_some(),
        "direct Codex verifier requires signed loopback ingress"
    );
    let realizations = std::env::var("RYEOS_EXTERNAL_REALIZATIONS")
        .context("daemon-protected realization set absent")?;
    let selected = parameters.select(&realizations)?;
    let project = lillux::PinnedDirectory::open(Path::new("."))?
        .context("admitted verifier project root absent")?;
    let thread_id = std::env::var("RYEOS_THREAD_ID")
        .context("admitted verifier root thread identity absent")?;
    let client = UdsRuntimeClient::from_env()?;
    let producer_scenario_id = &parameters.configuration.profile.producer_scenario_id;
    if project
        .open_child_directory(OsStr::new("prepared"))?
        .is_some()
    {
        // A new verifier process cannot inherit the original relay, pipe or
        // provider transcript. Reopening sealed files may aid diagnosis, but
        // never permits restaging or another START under this accepted root.
        let reopened = staging::reopen_direct_target_probe(&selected, &project, &parameters);
        let resumed = client.resume_scoped_child(&thread_id).await;
        let cleanup = match &resumed {
            Ok(locator) => {
                abort_known_scoped_locators(&client, &thread_id, std::slice::from_ref(locator))
                    .await
            }
            Err(_) => Vec::new(),
        };
        bail!(
            "direct verifier preparation already exists; sealed reopen={:?}; exact resume={:?}; cleanup={cleanup:?}",
            reopened.map(|_| ()),
            resumed.map(|_| ())
        );
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
        parameters.configuration.profile.command_fixture,
        parameters.configuration.profile.candidate.clone(),
        commands.forbidden_local_write,
        commands.guest_read,
        challenge.value().to_owned(),
    )?;
    let provider_endpoint = challenge.publish_provider_endpoint(&socket_name)?;
    let provider = provider.start()?;
    let expected_source: ProductProducerRecipeSourceIdentity = serde_json::from_value(
        client
            .scoped_child_expected_source(&thread_id, producer_scenario_id)
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
        let relay_scenario_id = producer_scenario_id.to_string();
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
                &relay_scenario_id,
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
            client.start_scoped_child(&thread_id, producer_scenario_id),
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
        bail!("direct Codex verifier requires signed loopback ingress")
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
        if running_relay.is_some() {
            require_direct_prepared_source_join(
                &locator.prepared_directory_sources,
                &direct_stage.prepared_directory_sources()?,
            )?;
        }
        if running_relay.is_some() {
            let signed_target = signed_direct_target_commitments(
                &parameters.configuration.expected_producer_recipe,
                &realizations,
                &project,
                selected.subject_mount_relative()?,
            )?;
            ensure!(
                locator.expected_applied_launch == signed_target,
                "direct Codex held target differs from independently selected signed member"
            );
            // Refuse a different daemon launch expectation before this
            // verifier sends its first Codex protocol frame. START may have
            // already released the target; this is not proof of zero startup
            // provider contact. The applied receipt must still match later.
            check_scoped_direct_expected_environment_and_cwd(
                &parameters.configuration.expected_producer_recipe,
                &realizations,
                &locator.expected_applied_launch,
            )?;
            check_scoped_applied_target(&locator, &locator.applied_launch)?;
            ensure!(
                locator.applied_launch.matches_commitments(&signed_target),
                "direct Codex applied target differs from independently selected signed member"
            );
            check_scoped_direct_environment_and_cwd(
                &parameters.configuration.expected_producer_recipe,
                &realizations,
                &locator.applied_launch,
            )?;
        }
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
    // The provider relay stays closed until the daemon's live child-origin
    // applied receipt agrees with the independently derived signed target,
    // mount, environment and CWD commitments. This is direct-probe evidence,
    // not yet hosted Worker lifecycle parity or a qualification claim.
    if let Some(relay) = running_relay.as_ref()
        && let Err(error) = relay.permit_provider_contact()
    {
        let abort = abort_exact_scoped_child(&client, &thread_id, &locator.attempt_id).await;
        let relay_cancel = running_relay
            .take()
            .map(cancel_scoped_relay)
            .context("direct-target provider gate lost its owned relay")?;
        return Err(error.context(format!(
            "direct-target provider gate refused; exact scoped abort={abort:?}; relay cancellation={relay_cancel}"
        )));
    }
    let direct_conversation = if running_relay.is_some() {
        let transport =
            ryeos_codex_runtime_verifier::scoped_app_server::ScopedAppServerTransport::new(
                &client,
                &thread_id,
                &locator.attempt_id,
            );
        let mut conversation =
            ryeos_codex_runtime_verifier::scoped_app_server::ScopedAppServerConversation::new(
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
    let first_observation = client
        .observe_scoped_child(&thread_id, &locator.attempt_id)
        .await;
    // A lost response may follow a committed natural result. This is a
    // point-read of the same attempt, not a second producer or model turn.
    // The daemon waits for its sole live observer and replays only retired
    // CAS-backed evidence while this callback remains authorized. A response
    // lost near the deadline may still be unrecoverable; another transport
    // loss remains uncertain.
    let (observation_result, first_transport_loss) = match first_observation {
        Err(first @ CallbackError::Transport(_)) if running_relay.is_some() => (
            client
                .observe_scoped_child(&thread_id, &locator.attempt_id)
                .await,
            Some(first.to_string()),
        ),
        other => (other, None),
    };
    let observed = match observation_result {
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
                "direct-target observation failed; first transport loss={first_transport_loss:?}; exact scoped abort={abort:?}; relay cancellation={relay_cancel}"
            )));
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context("scoped producer observation refused"));
        }
    };
    // The daemon stores this exact callback value as a canonical CAS object.
    // The terminal may only propose its coordinate; the qualification reader
    // independently loads the object and joins it to the exact attempt.
    let observation_object_hash = retained_observation_coordinate(&observed)?;
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
                producer_scenario_id,
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
        observation.applied_launch == locator.applied_launch,
        "settled scoped observation differs from pre-contact applied launch"
    );
    check_scoped_direct_environment_and_cwd(
        expected_recipe,
        &realizations,
        &observation.applied_launch,
    )?;
    if direct_evidence.is_some() {
        require_direct_prepared_observation_join(
            &locator.prepared_directory_sources,
            &observation.prepared_directory_sources,
            &direct_stage.prepared_directory_sources()?,
        )?;
    }
    ensure!(
        observation.schema == "ryeos.scoped_producer_observation.v7"
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
            && observation.prepared_directory_sources == locator.prepared_directory_sources
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
            &parameters.configuration.profile.candidate,
            expected_canary.as_bytes(),
            expected_canary.as_bytes(),
            staging::CONTROLLER_CANARY_DENIAL,
        )?;
        let commands = challenge.scripted_canary_commands()?;
        let guest_command = parameters.configuration.profile.command_script();
        let routing = RoutingScenario {
            thread_id: codex_thread,
            turn_id: codex_turn,
            guest_cwd: "/workspace".into(),
            local_refusal_command: commands.forbidden_local_write,
            guest_command_script: guest_command.clone(),
            secret_read_script: commands.guest_read.clone(),
            patch_input: parameters.configuration.profile.candidate.patch_input(),
            guest_command: format!("{} -c '{guest_command}'", scripted_provider::GUEST_SHELL),
            secret_read_command: format!(
                "{} -c '{}'",
                scripted_provider::GUEST_SHELL,
                commands.guest_read
            ),
            candidate_path: parameters.configuration.profile.candidate.path(),
            candidate_added_content: parameters.configuration.profile.candidate.content.clone(),
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
                    == lillux::sha256_hex(
                        parameters
                            .configuration
                            .profile
                            .candidate
                            .content
                            .as_bytes()
                    ),
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
            guest_command: parameters.configuration.profile.command_script(),
            secret_read_command: &routing.secret_read_script,
            expected_command_output: &routing.expected_command_output,
            controller_canary_value: challenge.value(),
            secret_read_denial: staging::CONTROLLER_CANARY_DENIAL,
            candidate_uri: parameters.configuration.profile.candidate.uri(),
            candidate_relative_path: &parameters.configuration.profile.candidate.relative_path,
            candidate_content: parameters
                .configuration
                .profile
                .candidate
                .content
                .as_bytes(),
        };
        check_guest_observation(&frozen.guest_observation, &guest)?;
        if race_probe {
            bail!(
                "direct scoped START/RESUME locators, applied environment and frozen candidate joined, but complete qualification evidence remains unproven"
            );
        }
        let result = ProductQualificationResult {
            schema: PRODUCT_QUALIFICATION_RESULT_SCHEMA.to_owned(),
            subject_manifest_hash: parameters.configuration.subject_manifest_hash.clone(),
            claims: DIRECT_QUALIFICATION_CLAIMS
                .into_iter()
                .map(str::to_owned)
                .collect(),
            probe_evidence: serde_json::json!({
                "scoped_attempt": {
                    "attempt_id": locator.attempt_id,
                    "scenario_id": producer_scenario_id,
                    "observation_object_hash": observation_object_hash,
                }
            }),
        };
        result.validate()?;
        let encoded = lillux::canonical_json(&serde_json::to_value(&result)?)?;
        std::io::stdout()
            .lock()
            .write_all(format!("{encoded}\n").as_bytes())?;
        return Ok(());
    }
    bail!("direct-target conversation evidence was not collected; no qualification claims issued")
}

/// The signed Tool selects this entrypoint and the exact finite policy input.
/// No caller supplies root/capsule/purpose hashes. This cut observes the
/// independent diagnostic, and cannot issue the still-unjoined Q claim.
async fn run_consumer_controller(input: &[u8]) -> Result<()> {
    use ryeos_codex_runtime_verifier::consumer_controller::{
        ConsumerControllerInvocation, ConsumerControllerOutcome, run_controller_beat,
    };
    let parameters: ryeos_codex_runtime_verifier::consumer_record::ConsumerVerifierConfiguration =
        serde_json::from_slice(input)?;
    parameters.validate()?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(
        parameters.controller.maximum_duration_ms,
    ));
    let thread_id = std::env::var("RYEOSD_THREAD_ID")
        .context("protected invoking verifier thread identity absent")?;
    ryeos_runtime::validate_runtime_thread_id(&thread_id).map_err(anyhow::Error::msg)?;
    let client = UdsRuntimeClient::from_env()?;
    let project = lillux::PinnedDirectory::open(Path::new("."))?
        .context("admitted verifier working directory absent")?;
    let name = format!("consumer-controller-{thread_id}");
    let (private, invocation) = match project.open_child_directory(OsStr::new(&name))? {
        Some(directory) => (directory, ConsumerControllerInvocation::Recover),
        None => (
            project.create_child(OsStr::new(&name), 0o700)?,
            ConsumerControllerInvocation::Fresh,
        ),
    };
    match run_controller_beat(
        &client,
        &private,
        &thread_id,
        &parameters.controller.inputs,
        invocation,
        deadline,
    )
    .await?
    {
        ConsumerControllerOutcome::Settled(observed) => {
            // Bounded exact references are diagnostic output. They are not a
            // ProductQualificationResult and cannot satisfy its claim schema.
            println!(
                "{}",
                json!({
                    "schema": "ryeos.codex.consumer_controller_diagnostic.v1",
                    "remote_consumer_attempt": {
                        "operation_id": observed.challenge.intent.operation_id,
                        "evidence_sha256": observed.evidence_sha256,
                        "termination_operation_id": observed.provider_terminal.operation_id,
                        "terminal_observation_sha256": observed.provider_terminal.observation_sha256,
                    },
                    "qualification_claims_enabled": false,
                })
            );
            bail!(
                "independent consumer diagnostic settled; actual Q transcript and qualification semantics remain unjoined"
            )
        }
        ConsumerControllerOutcome::Unresolved(_) => {
            bail!("consumer diagnostic unresolved; observation/recovery required, no relaunch")
        }
    }
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
        (
            home.join("config.toml").to_string_lossy().into_owned(),
            baseline_sha256.to_owned(),
        ),
        (
            home.join("environments.toml")
                .to_string_lossy()
                .into_owned(),
            environment_sha256.to_owned(),
        ),
    ]);
    ensure!(
        locator.prepared_immutable_sha256 == expected,
        "daemon-sealed producer config differs from verifier-derived exact bytes"
    );
    Ok(())
}

fn require_direct_prepared_source_join(
    retained: &BTreeMap<String, lillux::PinnedDirectoryIdentity>,
    pinned: &BTreeMap<String, lillux::PinnedDirectoryIdentity>,
) -> Result<()> {
    ensure!(
        retained.len() == 2
            && retained.contains_key(staging::DIRECT_OCCURRENCE_ID)
            && retained.contains_key(staging::DIRECT_HOME_ID)
            && retained == pinned,
        "direct prepared mount sources differ from verifier-pinned occurrence and home"
    );
    Ok(())
}

fn require_direct_prepared_observation_join(
    retained: &BTreeMap<String, lillux::PinnedDirectoryIdentity>,
    observed: &BTreeMap<String, lillux::PinnedDirectoryIdentity>,
    pinned: &BTreeMap<String, lillux::PinnedDirectoryIdentity>,
) -> Result<()> {
    require_direct_prepared_source_join(retained, pinned)?;
    ensure!(
        observed == retained,
        "settled direct mount sources differ from retained pre-contact sources"
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
            && locator
                .held_mount_preparation
                .matches_commitments(&locator.expected_mount_preparation)
            && receipt.owned_child_pid == locator.held_mount_preparation.owned_child_pid,
        "scoped producer applied mounts differ from held final-root preparation"
    );
    Ok(())
}

/// Derive the complete direct Codex environment from the signed finite recipe
/// and the verifier's separately admitted realization set. The daemon's
/// prelaunch commitment alone is not an independent expectation of its env.
fn check_scoped_direct_environment_and_cwd(
    recipe: &ProductProducerRecipe,
    admitted_realizations: &str,
    receipt: &lillux::LinuxSandboxAppliedLaunchReceipt,
) -> Result<()> {
    let expected = signed_direct_environment_cwd_commitments(recipe, admitted_realizations)?;
    ensure!(
        receipt.environment_sha256 == expected.environment_sha256
            && receipt.cwd_sha256 == expected.cwd_sha256,
        "direct Codex applied environment or cwd differs from signed recipe and admitted realizations"
    );
    Ok(())
}

/// Independently derive all four applied target commitments from the signed
/// recipe, the sealed realization selection, and this verifier's pinned
/// admitted project root. The daemon's compiled target is not used as an
/// input. Descriptor-backed member content is checked by `SelectedInput`'s
/// rooted preflight before this comparison; this binds its namespace path.
fn signed_direct_target_commitments(
    recipe: &ProductProducerRecipe,
    admitted_realizations: &str,
    project: &lillux::PinnedDirectory,
    subject_mount: &str,
) -> Result<lillux::LinuxSandboxAppliedLaunchCommitments> {
    recipe.validate()?;
    project.ensure_path_binding()?;
    let project_path = lillux::canonicalize_existing_path(project.path())?;
    let rebound = lillux::PinnedDirectory::open(&project_path)?
        .context("canonical admitted verifier root disappeared")?;
    ensure!(
        project.is_same_directory(&rebound)?,
        "canonical verifier root differs from pinned admitted project"
    );
    let ProducerExecutableSource::AdmittedRealizationMember {
        realization_id,
        relative_path,
        ..
    } = &recipe.executable_source
    else {
        bail!("direct Codex target is not an admitted realization member");
    };
    ensure!(
        realization_id == "subject"
            && subject_mount == "qualification/subject"
            && relative_path == "bin/codex",
        "direct Codex target differs from selected subject member"
    );
    let executable = ryeos_state::objects::ExternalContentMountRoot::Project
        .destination(Some(&project_path), subject_mount)?
        .join(relative_path);
    let argv0 = executable.as_os_str().to_os_string();
    let arguments = recipe.argv.iter().map(OsString::from).collect::<Vec<_>>();
    let cwd = match &recipe.cwd_source {
        ProducerCwdSource::PreparedDirectory { id } => prepared_directory_mount_destination(id)?,
        ProducerCwdSource::VerifierPrivateWorkspace => {
            bail!("direct Codex cwd cannot be verifier-private")
        }
    };
    let environment = signed_direct_environment(recipe, admitted_realizations)?;
    lillux::LinuxSandboxAppliedLaunchCommitments::from_target(
        lillux::LinuxSandboxAppliedLaunchTarget {
            executable: &executable,
            argv0: &argv0,
            arguments: &arguments,
            cwd: &cwd,
            environment: &environment,
        },
    )
    .map_err(anyhow::Error::msg)
}

fn check_scoped_direct_expected_environment_and_cwd(
    recipe: &ProductProducerRecipe,
    admitted_realizations: &str,
    launch: &lillux::LinuxSandboxAppliedLaunchCommitments,
) -> Result<()> {
    let expected = signed_direct_environment_cwd_commitments(recipe, admitted_realizations)?;
    ensure!(
        launch.environment_sha256 == expected.environment_sha256
            && launch.cwd_sha256 == expected.cwd_sha256,
        "direct Codex pre-conversation launch environment or cwd differs from signed recipe and admitted realizations"
    );
    Ok(())
}

fn signed_direct_environment_cwd_commitments(
    recipe: &ProductProducerRecipe,
    admitted_realizations: &str,
) -> Result<lillux::LinuxSandboxAppliedLaunchCommitments> {
    let environment = signed_direct_environment(recipe, admitted_realizations)?;
    // Lillux exposes the canonical applied environment and cwd commitments
    // through the same target projection as its pre-exec receipt. Executable
    // and argv below are inert placeholders; they remain joined separately to
    // the daemon's held target while their independent source is established.
    let executable = std::path::Path::new("/bin/true");
    let argv0 = OsString::from("/bin/true");
    let cwd = match &recipe.cwd_source {
        ProducerCwdSource::PreparedDirectory { id } => prepared_directory_mount_destination(id)?,
        ProducerCwdSource::VerifierPrivateWorkspace => {
            bail!("direct Codex cwd cannot be verifier-private")
        }
    };
    lillux::LinuxSandboxAppliedLaunchCommitments::from_target(
        lillux::LinuxSandboxAppliedLaunchTarget {
            executable,
            argv0: &argv0,
            arguments: &[],
            cwd: &cwd,
            environment: &environment,
        },
    )
    .map_err(anyhow::Error::msg)
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
        environment
            .insert(
                OsString::from("RYEOS_PRODUCER_STDIN_FD"),
                OsString::from("0"),
            )
            .is_none(),
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
    applied_launch: lillux::LinuxSandboxAppliedLaunchReceipt,
    expected_mount_preparation: lillux::LinuxSandboxMountPreparationCommitments,
    held_mount_preparation: lillux::LinuxSandboxMountPreparationReceipt,
    prepared_directory_sources: BTreeMap<String, lillux::PinnedDirectoryIdentity>,
    prepared_immutable_sha256: BTreeMap<String, String>,
}

impl ScopedAttemptLocator {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == "ryeos.scoped_producer_locator.v7"
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
                && self.applied_launch.matches_commitments(&self.expected_applied_launch)
                && self.applied_launch.matches_post_release_mounts(&self.expected_mount_preparation)
                && self.applied_launch.owned_child_pid == self.held_mount_preparation.owned_child_pid
                && self.prepared_directory_sources.len()
                    <= ryeos_state::external_content::products::producer_recipe::MAX_PRODUCER_PREPARED_DIRECTORIES
                && self.prepared_directory_sources.keys().all(|id| {
                    prepared_directory_mount_destination(id).is_ok()
                })
                && self.applied_launch.namespace_pid == 1
                && self.applied_launch.effective_uid == 1
                && self.applied_launch.effective_gid == 1
                && self.applied_launch.no_new_privs
                && self.applied_launch.seccomp_mode == 2
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
    prepared_directory_sources: BTreeMap<String, lillux::PinnedDirectoryIdentity>,
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

fn retained_observation_coordinate(value: &serde_json::Value) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(value)?.as_bytes(),
    ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn consumer_cli_requires_one_exact_canonical_challenge() {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use ryeos_external_execution_contract::restored_runtime_measurement::{
            ConsumerArchiveBudget, ConsumerRuntimeChallenge, ConsumerRuntimeVerificationCoordinate,
            ConsumerRuntimeVerifierSelection, RemoteVerificationPurpose,
            RestoredVerifierAttemptIntent,
        };
        let decode = ryeos_codex_runtime_verifier::consumer_observer::decode_startup_arguments;

        let coordinate = ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-consumer-cli".into(),
            accepted_capsule_hash: "a".repeat(64),
            qualification_purpose_digest: "b".repeat(64),
            scenario_id: "consumer-cli".into(),
            scenario_source_digest: "c".repeat(64),
            subject_digest: "d".repeat(64),
            use_digest: "e".repeat(64),
            prerequisite_measurement_attempt_id: "f".repeat(64),
            prerequisite_measurement_observation_digest: "1".repeat(64),
        };
        let mut intent = RestoredVerifierAttemptIntent {
            schema: 2,
            operation_id: String::new(),
            qualification_operation_id: "2".repeat(64),
            restored_occurrence_id: "sbx-consumer-cli".into(),
            verifier_artifact_hash: "3".repeat(64),
            upload_sha256: "4".repeat(64),
            upload_bytes: 1024,
            purpose: RemoteVerificationPurpose::ConsumerRuntime {
                coordinate,
                nonce_hex: "5".repeat(64),
                guest_runtime_manifest_hash: "6".repeat(64),
            },
            attempt_deadline_ms: 42,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let challenge = ConsumerRuntimeChallenge {
            schema: 1,
            intent,
            selection: ConsumerRuntimeVerifierSelection {
                scenario_source_digest: "c".repeat(64),
                verifier_artifact_hash: "3".repeat(64),
                archive_budget: ConsumerArchiveBudget::new(16, 4096, 8192).unwrap(),
            },
        };
        let canonical = challenge.canonical_bytes().unwrap();
        let encoded = URL_SAFE_NO_PAD.encode(&canonical);
        let args = || ["--consumer-challenge-b64".into(), encoded.clone().into()];
        assert_eq!(decode(args()).unwrap(), challenge);
        assert!(decode(["--consumer-challenge-b64".into()]).is_err());
        assert!(decode(["--challenge-b64".into(), encoded.clone().into()]).is_err());
        assert!(
            decode([
                "--consumer-challenge-b64".into(),
                format!("{encoded}=").into()
            ])
            .is_err()
        );
        assert!(decode(args().into_iter().chain(["extra".into()])).is_err());
        let mut noncanonical = canonical;
        noncanonical.push(b'\n');
        assert!(
            decode([
                "--consumer-challenge-b64".into(),
                URL_SAFE_NO_PAD.encode(noncanonical).into()
            ])
            .is_err()
        );
    }

    #[test]
    fn scoped_observation_coordinate_matches_retained_cas_object() {
        let observed = json!({
            "schema": "ryeos.scoped_producer_observation.v7",
            "attempt_id": format!("scoped-{}", "a".repeat(64)),
            "nested": {"producer_exit_clean": true, "contacts": 4},
        });
        let temporary = tempfile::tempdir().unwrap();
        let cas = lillux::cas::CasStore::new(temporary.path().to_path_buf());
        let stored = cas.store_object(&observed).unwrap();
        assert_eq!(retained_observation_coordinate(&observed).unwrap(), stored);
        let mut changed = observed;
        changed["nested"]["contacts"] = json!(5);
        assert_ne!(retained_observation_coordinate(&changed).unwrap(), stored);
    }

    #[test]
    fn signed_direct_environment_and_cwd_match_applied_fields() {
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
        }))
        .unwrap();
        let admitted = "[{\"id\":\"signed\"}]";
        let environment = signed_direct_environment(&recipe, admitted).unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let project = lillux::PinnedDirectory::open(project_dir.path())
            .unwrap()
            .unwrap();
        let signed_target =
            signed_direct_target_commitments(&recipe, admitted, &project, "qualification/subject")
                .unwrap();
        let signed_executable = project_dir.path().join("qualification/subject/bin/codex");
        let signed_argv0 = signed_executable.as_os_str().to_os_string();
        let signed_arguments = vec![OsString::from("app-server")];
        let signed_cwd = Path::new("/ryeos/producer-prepared/codex-occurrence");
        let expected_signed_target = lillux::LinuxSandboxAppliedLaunchCommitments::from_target(
            lillux::LinuxSandboxAppliedLaunchTarget {
                executable: &signed_executable,
                argv0: &signed_argv0,
                arguments: &signed_arguments,
                cwd: signed_cwd,
                environment: &environment,
            },
        )
        .unwrap();
        assert_eq!(signed_target, expected_signed_target);
        assert!(signed_direct_target_commitments(&recipe, admitted, &project, "other").is_err());
        let mut wrong_subject = recipe.clone();
        if let ProducerExecutableSource::AdmittedRealizationMember { realization_id, .. } =
            &mut wrong_subject.executable_source
        {
            *realization_id = "other".into();
        }
        assert!(
            signed_direct_target_commitments(
                &wrong_subject,
                admitted,
                &project,
                "qualification/subject"
            )
            .is_err()
        );
        let mut wrong_member = recipe.clone();
        if let ProducerExecutableSource::AdmittedRealizationMember { relative_path, .. } =
            &mut wrong_member.executable_source
        {
            *relative_path = "bin/other".into();
        }
        assert!(
            signed_direct_target_commitments(
                &wrong_member,
                admitted,
                &project,
                "qualification/subject"
            )
            .is_err()
        );
        let mut wrong_executable_source = recipe.clone();
        wrong_executable_source.executable_source =
            ProducerExecutableSource::AdmittedVerifierExecutable;
        assert!(
            signed_direct_target_commitments(
                &wrong_executable_source,
                admitted,
                &project,
                "qualification/subject"
            )
            .is_err()
        );
        let mut changed_arguments = recipe.clone();
        changed_arguments.argv.push("--different".into());
        assert_ne!(
            signed_direct_target_commitments(
                &changed_arguments,
                admitted,
                &project,
                "qualification/subject"
            )
            .unwrap(),
            signed_target
        );
        let mut changed_target_cwd = recipe.clone();
        changed_target_cwd.cwd_source = ProducerCwdSource::PreparedDirectory {
            id: "other-occurrence".into(),
        };
        assert_ne!(
            signed_direct_target_commitments(
                &changed_target_cwd,
                admitted,
                &project,
                "qualification/subject"
            )
            .unwrap(),
            signed_target
        );
        changed_target_cwd.cwd_source = ProducerCwdSource::VerifierPrivateWorkspace;
        assert!(
            signed_direct_target_commitments(
                &changed_target_cwd,
                admitted,
                &project,
                "qualification/subject"
            )
            .is_err()
        );
        let mut changed_target_environment = recipe.clone();
        changed_target_environment.environment_bindings.insert(
            "LANG".into(),
            ProducerEnvironmentBinding::Literal {
                value: "POSIX".into(),
            },
        );
        assert_ne!(
            signed_direct_target_commitments(
                &changed_target_environment,
                admitted,
                &project,
                "qualification/subject"
            )
            .unwrap(),
            signed_target
        );
        let rebound_root = tempfile::tempdir().unwrap();
        let selected_path = rebound_root.path().join("selected");
        let displaced_path = rebound_root.path().join("displaced");
        std::fs::create_dir(&selected_path).unwrap();
        let pinned_selected = lillux::PinnedDirectory::open(&selected_path)
            .unwrap()
            .unwrap();
        std::fs::rename(&selected_path, &displaced_path).unwrap();
        std::fs::create_dir(&selected_path).unwrap();
        assert!(
            signed_direct_target_commitments(
                &recipe,
                admitted,
                &pinned_selected,
                "qualification/subject"
            )
            .is_err()
        );
        std::fs::remove_dir(&selected_path).unwrap();
        std::fs::rename(&displaced_path, &selected_path).unwrap();
        assert_eq!(environment.len(), 8);
        assert_eq!(
            environment.get(OsStr::new("RYEOS_EXTERNAL_REALIZATIONS")),
            Some(&OsString::from(admitted))
        );
        assert_eq!(
            environment.get(OsStr::new("CODEX_HOME")),
            Some(&OsString::from("/ryeos/producer-prepared/codex-home"))
        );
        assert_eq!(
            environment.get(OsStr::new("RYEOS_PRODUCER_STDIN_FD")),
            Some(&OsString::from("0"))
        );
        let executable = Path::new("/bin/true");
        let argv0 = OsString::from("/bin/true");
        let cwd = Path::new("/ryeos/producer-prepared/codex-occurrence");
        let commitment = lillux::LinuxSandboxAppliedLaunchCommitments::from_target(
            lillux::LinuxSandboxAppliedLaunchTarget {
                executable,
                argv0: &argv0,
                arguments: &[],
                cwd,
                environment: &environment,
            },
        )
        .unwrap();
        check_scoped_direct_expected_environment_and_cwd(&recipe, admitted, &commitment).unwrap();
        assert!(
            check_scoped_direct_expected_environment_and_cwd(&recipe, "different", &commitment)
                .is_err()
        );
        let mut changed_precontact = commitment.clone();
        changed_precontact.environment_sha256[0] ^= 1;
        assert!(
            check_scoped_direct_expected_environment_and_cwd(
                &recipe,
                admitted,
                &changed_precontact
            )
            .is_err()
        );
        let mut changed_precontact_cwd = commitment.clone();
        changed_precontact_cwd.cwd_sha256[0] ^= 1;
        assert!(
            check_scoped_direct_expected_environment_and_cwd(
                &recipe,
                admitted,
                &changed_precontact_cwd
            )
            .is_err()
        );
        let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 42,
            namespace_pid: 1,
            effective_uid: 1,
            effective_gid: 1,
            no_new_privs: true,
            seccomp_mode: 2,
            executable_sha256: [1; 32],
            argv_sha256: [2; 32],
            environment_sha256: commitment.environment_sha256,
            cwd_sha256: commitment.cwd_sha256,
            post_release_mount_view: lillux::LinuxSandboxMountPreparationCommitments {
                schema: 1,
                mount_count: 1,
                destination_access_sha256: [0; 32],
            },
        };
        check_scoped_direct_environment_and_cwd(&recipe, admitted, &receipt).unwrap();
        assert!(check_scoped_direct_environment_and_cwd(&recipe, "different", &receipt).is_err());
        let mut changed = receipt.clone();
        changed.environment_sha256[0] ^= 1;
        assert!(check_scoped_direct_environment_and_cwd(&recipe, admitted, &changed).is_err());
        let mut changed_cwd = receipt.clone();
        changed_cwd.cwd_sha256[0] ^= 1;
        assert!(check_scoped_direct_environment_and_cwd(&recipe, admitted, &changed_cwd).is_err());
        let mut altered_recipe = recipe.clone();
        altered_recipe.environment_bindings.insert(
            "LANG".into(),
            ProducerEnvironmentBinding::Literal {
                value: "POSIX".into(),
            },
        );
        assert!(
            check_scoped_direct_expected_environment_and_cwd(
                &altered_recipe,
                admitted,
                &commitment
            )
            .is_err()
        );
        assert!(
            check_scoped_direct_environment_and_cwd(&altered_recipe, admitted, &receipt).is_err()
        );
        let mut different_cwd = recipe.clone();
        different_cwd.cwd_source = ProducerCwdSource::PreparedDirectory {
            id: "different-occurrence".into(),
        };
        assert!(
            check_scoped_direct_expected_environment_and_cwd(&different_cwd, admitted, &commitment)
                .is_err()
        );
        assert!(
            check_scoped_direct_environment_and_cwd(&different_cwd, admitted, &receipt).is_err()
        );
        altered_recipe.environment_sources.clear();
        assert!(
            check_scoped_direct_environment_and_cwd(&altered_recipe, admitted, &receipt).is_err()
        );
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
            canonical_ref: "config:fixtures/codex-runtime/scenario-driver".into(),
            raw_content_digest: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            recipe_digest: digest.clone(),
        };
        let locator = ScopedAttemptLocator {
            schema: "ryeos.scoped_producer_locator.v7".into(),
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
            applied_launch: lillux::LinuxSandboxAppliedLaunchReceipt {
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
            prepared_directory_sources: BTreeMap::new(),
            prepared_immutable_sha256: BTreeMap::new(),
        };
        assert!(
            check_admitted_producer_source(
                &source,
                "config:fixtures/codex-runtime/scenario-driver",
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
            "schema": "ryeos.scoped_producer_locator.v7",
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
            "applied_launch": {
                "owned_child_pid": 42,
                "namespace_pid": 1,
                "effective_uid": 1,
                "effective_gid": 1,
                "no_new_privs": true,
                "seccomp_mode": 2,
                "executable_sha256": vec![1; 32],
                "argv_sha256": vec![2; 32],
                "environment_sha256": vec![3; 32],
                "cwd_sha256": vec![4; 32],
                "post_release_mount_view": {
                    "schema": 1,
                    "mount_count": 1,
                    "destination_access_sha256": vec![0; 32],
                },
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
            "prepared_directory_sources": {},
            "prepared_immutable_sha256": {},
        });
        let locator: ScopedAttemptLocator = serde_json::from_value(valid.clone()).unwrap();
        locator.validate().unwrap();
        let mut missing_prepared_sources = valid.clone();
        missing_prepared_sources
            .as_object_mut()
            .unwrap()
            .remove("prepared_directory_sources");
        assert!(serde_json::from_value::<ScopedAttemptLocator>(missing_prepared_sources).is_err());
        let mut invalid_prepared_source = valid.clone();
        invalid_prepared_source["prepared_directory_sources"]["../foreign"] =
            json!({"containing_device": 1, "inode": 2});
        assert!(
            serde_json::from_value::<ScopedAttemptLocator>(invalid_prepared_source)
                .unwrap()
                .validate()
                .is_err()
        );
        for field in [
            "executable_sha256",
            "argv_sha256",
            "environment_sha256",
            "cwd_sha256",
        ] {
            let mut changed = valid.clone();
            changed["applied_launch"][field][0] = json!(255);
            assert!(
                serde_json::from_value::<ScopedAttemptLocator>(changed)
                    .unwrap()
                    .validate()
                    .is_err(),
                "applied {field} drift reached provider permit"
            );
        }
        for (field, value) in [
            ("namespace_pid", json!(2)),
            ("effective_uid", json!(0)),
            ("effective_gid", json!(0)),
            ("no_new_privs", json!(false)),
            ("seccomp_mode", json!(0)),
        ] {
            let mut changed = valid.clone();
            changed["applied_launch"][field] = value;
            assert!(
                serde_json::from_value::<ScopedAttemptLocator>(changed)
                    .unwrap()
                    .validate()
                    .is_err(),
                "applied {field} control drift reached provider permit"
            );
        }
        let mut missing_applied = valid.clone();
        missing_applied
            .as_object_mut()
            .unwrap()
            .remove("applied_launch");
        assert!(serde_json::from_value::<ScopedAttemptLocator>(missing_applied).is_err());
        let mut changed_mounts = valid.clone();
        changed_mounts["expected_mount_preparation"]["destination_access_sha256"][0] = json!(255);
        changed_mounts["held_mount_preparation"]["destination_access_sha256"][0] = json!(255);
        assert!(
            serde_json::from_value::<ScopedAttemptLocator>(changed_mounts)
                .unwrap()
                .validate()
                .is_err(),
            "a consistently rewritten held mount view reached provider permit"
        );
        let home = ryeos_state::external_content::products::producer_recipe::prepared_directory_mount_destination(
            staging::DIRECT_HOME_ID,
        )
        .unwrap();
        let mut exact_files = valid.clone();
        exact_files["prepared_immutable_sha256"] = json!(BTreeMap::from([
            (
                home.join("config.toml").to_string_lossy().into_owned(),
                "1".repeat(64)
            ),
            (
                home.join("environments.toml")
                    .to_string_lossy()
                    .into_owned(),
                "2".repeat(64)
            ),
        ]));
        let exact: ScopedAttemptLocator = serde_json::from_value(exact_files.clone()).unwrap();
        check_scoped_prepared_immutable(&exact, &"1".repeat(64), &"2".repeat(64)).unwrap();
        let config_path = home.join("config.toml").to_string_lossy().into_owned();
        let environment_path = home
            .join("environments.toml")
            .to_string_lossy()
            .into_owned();
        for changed in [
            {
                let mut value = exact_files.clone();
                value["prepared_immutable_sha256"][&config_path] = json!("3".repeat(64));
                value
            },
            {
                let mut value = exact_files.clone();
                value["prepared_immutable_sha256"]
                    .as_object_mut()
                    .unwrap()
                    .remove(&environment_path);
                value
            },
            {
                let mut value = exact_files.clone();
                value["prepared_immutable_sha256"]
                    [home.join("extra.toml").to_string_lossy().as_ref()] = json!("4".repeat(64));
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
            assert!(
                check_scoped_prepared_immutable(&changed, &"1".repeat(64), &"2".repeat(64))
                    .is_err()
            );
        }
        exact_files["schema"] = json!("ryeos.scoped_producer_locator.v4");
        assert!(
            serde_json::from_value::<ScopedAttemptLocator>(exact_files)
                .unwrap()
                .validate()
                .is_err()
        );
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
        assert!(
            serde_json::from_value::<ScopedAttemptLocator>(wrong_held)
                .unwrap()
                .validate()
                .is_err()
        );
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
        missing_mounts
            .as_object_mut()
            .unwrap()
            .remove("expected_mount_preparation");
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
        let mut missing_recipe = valid;
        missing_recipe
            .as_object_mut()
            .unwrap()
            .remove("recipe_digest");
        assert!(serde_json::from_value::<ScopedAttemptLocator>(missing_recipe).is_err());
        let mut no_prelaunch_target = json!({
            "schema": "ryeos.scoped_producer_locator.v7",
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
    fn direct_prepared_source_join_rejects_wrong_swapped_and_missing_real_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let occurrence = root
            .create_child(std::ffi::OsStr::new("codex-occurrence"), 0o700)
            .unwrap();
        let home = root
            .create_child(std::ffi::OsStr::new("codex-home"), 0o700)
            .unwrap();
        let foreign = root
            .create_child(std::ffi::OsStr::new("foreign"), 0o700)
            .unwrap();
        let pinned = BTreeMap::from([
            (
                staging::DIRECT_OCCURRENCE_ID.to_owned(),
                occurrence.identity().unwrap(),
            ),
            (staging::DIRECT_HOME_ID.to_owned(), home.identity().unwrap()),
        ]);
        require_direct_prepared_source_join(&pinned, &pinned).unwrap();

        let mut wrong = pinned.clone();
        wrong.insert(
            staging::DIRECT_HOME_ID.to_owned(),
            foreign.identity().unwrap(),
        );
        assert!(require_direct_prepared_source_join(&wrong, &pinned).is_err());
        let mut swapped = pinned.clone();
        swapped.insert(
            staging::DIRECT_HOME_ID.to_owned(),
            occurrence.identity().unwrap(),
        );
        swapped.insert(
            staging::DIRECT_OCCURRENCE_ID.to_owned(),
            home.identity().unwrap(),
        );
        assert!(require_direct_prepared_source_join(&swapped, &pinned).is_err());
        let mut missing = pinned.clone();
        missing.remove(staging::DIRECT_HOME_ID);
        assert!(require_direct_prepared_source_join(&missing, &pinned).is_err());
        let mut extra = pinned.clone();
        extra.insert("foreign".into(), foreign.identity().unwrap());
        assert!(require_direct_prepared_source_join(&extra, &pinned).is_err());
        let mut substituted_observation = pinned.clone();
        substituted_observation.insert(
            staging::DIRECT_HOME_ID.to_owned(),
            foreign.identity().unwrap(),
        );
        assert!(
            require_direct_prepared_observation_join(&pinned, &substituted_observation, &pinned,)
                .is_err()
        );
    }
}
