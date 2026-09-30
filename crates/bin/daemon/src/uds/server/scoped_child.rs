//! Protected qualification-child RPC boundary.
//!
//! A verifier may select an admitted scenario, but may not supply an
//! executable, path, recipe, scope, process identity, or attempt nonce. Start
//! preflights the signed recipe and exact retained materials; observation
//! locates only a journaled attempt. Neither may acknowledge an unproven child.

use anyhow::{Result, bail};
use ryeos_app::callback_token::CallbackCapability;
use ryeos_app::state::AppState;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedChildStartRequest {
    thread_id: String,
    scenario_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedChildObserveRequest {
    thread_id: String,
    attempt_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedChildAbortRequest {
    thread_id: String,
    attempt_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedChildResumeRequest {
    thread_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedChildWriteRequest {
    thread_id: String,
    attempt_id: String,
    sequence: u32,
    bytes: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedChildReadRequest {
    thread_id: String,
    attempt_id: String,
    offset: u64,
    maximum_bytes: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedChildCloseRequest {
    thread_id: String,
    attempt_id: String,
    sequence: u32,
}

fn validate_attempt_id(attempt_id: &str) -> Result<()> {
    if !attempt_id.starts_with("scoped-")
        || attempt_id.len() != 71
        || !attempt_id[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("scoped child operation requires a canonical attempt id");
    }
    Ok(())
}

fn parse_start_request(params: &Value) -> Result<ScopedChildStartRequest> {
    let request: ScopedChildStartRequest = serde_json::from_value(params.clone())?;
    if request.scenario_id.is_empty()
        || request.scenario_id.len() > 128
        || !request
            .scenario_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("scoped child scenario identifier is not bounded and canonical");
    }
    Ok(request)
}

fn parse_observe_request(params: &Value) -> Result<ScopedChildObserveRequest> {
    let request: ScopedChildObserveRequest = serde_json::from_value(params.clone())?;
    validate_attempt_id(&request.attempt_id)?;
    Ok(request)
}

fn parse_abort_request(params: &Value) -> Result<ScopedChildAbortRequest> {
    let request: ScopedChildAbortRequest = serde_json::from_value(params.clone())?;
    validate_attempt_id(&request.attempt_id)?;
    Ok(request)
}

fn parse_resume_request(params: &Value) -> Result<ScopedChildResumeRequest> {
    Ok(serde_json::from_value(params.clone())?)
}

fn parse_write_request(params: &Value) -> Result<ScopedChildWriteRequest> {
    if params
        .get("bytes")
        .and_then(Value::as_array)
        .is_none_or(|bytes| bytes.is_empty() || bytes.len() > 64 * 1024)
    {
        bail!("scoped child write frame is not bounded");
    }
    let request: ScopedChildWriteRequest = serde_json::from_value(params.clone())?;
    validate_attempt_id(&request.attempt_id)?;
    Ok(request)
}

fn parse_read_request(params: &Value) -> Result<ScopedChildReadRequest> {
    let request: ScopedChildReadRequest = serde_json::from_value(params.clone())?;
    validate_attempt_id(&request.attempt_id)?;
    if request.maximum_bytes == 0 || request.maximum_bytes > 64 * 1024 {
        bail!("scoped child read length is not bounded");
    }
    Ok(request)
}

fn parse_close_request(params: &Value) -> Result<ScopedChildCloseRequest> {
    let request: ScopedChildCloseRequest = serde_json::from_value(params.clone())?;
    validate_attempt_id(&request.attempt_id)?;
    Ok(request)
}

pub(super) async fn handle(
    method: &str,
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    // A future held spawn and natural-empty observation block in Lillux.
    // Give the complete transaction owned state so a dropped UDS future
    // cannot detach work between process release and registry insertion.
    let method = method.to_owned();
    let params = params.clone();
    let state = state.clone();
    let cap = cap.clone();
    tokio::task::spawn_blocking(move || handle_blocking(&method, &params, &state, &cap))
        .await
        .map_err(|error| anyhow::anyhow!("scoped child worker did not settle: {error}"))?
}

fn handle_blocking(
    method: &str,
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    if cap.expires_at.has_elapsed() {
        bail!("scoped child callback expired before its blocking transaction began");
    }
    if !matches!(
        method,
        "runtime.scoped_child_expected_source"
            | "runtime.scoped_child_expected_isolation_class"
            | "runtime.scoped_child_start"
            | "runtime.scoped_child_resume"
            | "runtime.scoped_child_observe"
            | "runtime.scoped_child_abort"
            | "runtime.scoped_child_write"
            | "runtime.scoped_child_read"
            | "runtime.scoped_child_close_input"
    ) {
        bail!("unsupported scoped child operation");
    }
    let source = if method == "runtime.scoped_child_expected_source" {
        Some(parse_start_request(params)?)
    } else {
        None
    };
    let isolation_class = if method == "runtime.scoped_child_expected_isolation_class" {
        Some(parse_resume_request(params)?)
    } else {
        None
    };
    let start = if method == "runtime.scoped_child_start" {
        Some(parse_start_request(params)?)
    } else {
        None
    };
    let observe = if method == "runtime.scoped_child_observe" {
        Some(parse_observe_request(params)?)
    } else {
        None
    };
    let abort = if method == "runtime.scoped_child_abort" {
        Some(parse_abort_request(params)?)
    } else {
        None
    };
    let resume = if method == "runtime.scoped_child_resume" {
        Some(parse_resume_request(params)?)
    } else {
        None
    };
    let write = if method == "runtime.scoped_child_write" {
        Some(parse_write_request(params)?)
    } else {
        None
    };
    let read = if method == "runtime.scoped_child_read" {
        Some(parse_read_request(params)?)
    } else {
        None
    };
    let close = if method == "runtime.scoped_child_close_input" {
        Some(parse_close_request(params)?)
    } else {
        None
    };
    let thread_id = source
        .as_ref()
        .map(|request| request.thread_id.as_str())
        .or_else(|| {
            isolation_class
                .as_ref()
                .map(|request| request.thread_id.as_str())
        })
        .or_else(|| start.as_ref().map(|request| request.thread_id.as_str()))
        .or_else(|| observe.as_ref().map(|request| request.thread_id.as_str()))
        .or_else(|| abort.as_ref().map(|request| request.thread_id.as_str()))
        .or_else(|| resume.as_ref().map(|request| request.thread_id.as_str()))
        .or_else(|| write.as_ref().map(|request| request.thread_id.as_str()))
        .or_else(|| read.as_ref().map(|request| request.thread_id.as_str()))
        .or_else(|| close.as_ref().map(|request| request.thread_id.as_str()))
        .ok_or_else(|| anyhow::anyhow!("scoped child request has no thread id"))?;
    let grant = cap
        .scoped_producer_grant
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scoped child requires an admitted producer grant"))?;
    grant.validate()?;
    if thread_id != cap.thread_id || thread_id != grant.root_thread_id {
        bail!("scoped child requires its exact admitted root thread");
    }
    let owner = cap
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("scoped child callback has no launch owner"))?;
    if owner != grant.launch_owner {
        bail!("scoped child launch owner differs from protected admission");
    }
    state.state_store.assert_launch_owner(thread_id, owner)?;

    if let Some(request) = abort.as_ref() {
        let typed_owner: ryeos_app::runtime_db::LaunchOwner = serde_json::from_str(owner)?;
        let key = ryeos_app::scoped_producer_process::ScopedProducerProcessKey::new(
            request.attempt_id.clone(),
            typed_owner,
        )?;
        ryeos_app::scoped_producer_stop::abort_scoped_producer_for_attempt(
            state,
            &key,
            cap.expires_at.remaining().min(Duration::from_secs(60)),
        )?;
        return Ok(serde_json::json!({
            "schema": "ryeos.scoped_child_abort.v1",
            "attempt_id": request.attempt_id,
            "settlement": "retired_cleanup_only",
        }));
    }

    if let Some(request) = write.as_ref() {
        let typed_owner: ryeos_app::runtime_db::LaunchOwner = serde_json::from_str(owner)?;
        let delivered = ryeos_app::scoped_producer_io::write_scoped_producer(
            state,
            &request.attempt_id,
            &typed_owner,
            request.sequence,
            &request.bytes,
            cap.expires_at,
        )?;
        return Ok(serde_json::json!({
            "schema": "ryeos.scoped_input_delivery.v1",
            "attempt_id": request.attempt_id,
            "sequence": delivered.sequence,
            "kind": "write",
            "payload_digest": delivered.payload_digest,
            "byte_count": delivered.byte_count,
            "delivery": "delivered"
        }));
    }
    if let Some(request) = close.as_ref() {
        let typed_owner: ryeos_app::runtime_db::LaunchOwner = serde_json::from_str(owner)?;
        let delivered = ryeos_app::scoped_producer_io::close_scoped_producer_input(
            state,
            &request.attempt_id,
            &typed_owner,
            request.sequence,
            cap.expires_at,
        )?;
        return Ok(serde_json::json!({
            "schema": "ryeos.scoped_input_delivery.v1",
            "attempt_id": request.attempt_id,
            "sequence": delivered.sequence,
            "kind": "close",
            "payload_digest": delivered.payload_digest,
            "byte_count": delivered.byte_count,
            "delivery": "delivered"
        }));
    }
    if let Some(request) = read.as_ref() {
        let typed_owner: ryeos_app::runtime_db::LaunchOwner = serde_json::from_str(owner)?;
        let offset = usize::try_from(request.offset)?;
        let bytes = ryeos_app::scoped_producer_io::read_scoped_producer_stdout(
            state,
            &request.attempt_id,
            &typed_owner,
            offset,
            request.maximum_bytes as usize,
            cap.expires_at,
        )?;
        return Ok(serde_json::json!({
            "schema": "ryeos.scoped_stdout_read.v1",
            "attempt_id": request.attempt_id,
            "offset": request.offset,
            "bytes_sha256": lillux::sha256_hex(&bytes),
            "eof": bytes.is_empty(),
            "bytes": bytes,
        }));
    }

    if isolation_class.is_some() {
        // Compare the live registered generation with the class retained
        // before this root gained child-launch authority. The verifier never
        // derives its expectation from a child row or observation.
        let current = state.isolation.admission_class_provenance()?;
        if current != grant.isolation_class {
            bail!("isolation admission class changed after root grant");
        }
        return Ok(serde_json::to_value(&grant.isolation_class)?);
    }

    if let Some(request) = source.as_ref() {
        // This is the pre-launch expectation retained in the root's sealed
        // qualification purpose. Re-resolve current signed authority so a
        // moved Config fails before START; do not derive it from a child row.
        let selected = ryeos_app::operator_external_content::product_qualification::
            resolve_current_bundle_producer_recipe_for_purpose(
                state, &grant.purpose.execution_view()?, &request.scenario_id,
            )?;
        let source = selected.source_identity()?;
        let retained = grant
            .purpose
            .producer_recipe_sources
            .get(&request.scenario_id)
            .ok_or_else(|| anyhow::anyhow!("signed scenario has no retained producer source"))?;
        if &source != retained {
            bail!("current producer source differs from sealed qualification purpose");
        }
        return Ok(serde_json::to_value(source)?);
    }

    if let Some(request) = start.as_ref() {
        if state.isolation.admission_class_provenance()? != grant.isolation_class {
            bail!("isolation admission class changed before scoped child start");
        }
        // Resolve the selector from current signed Bundle authority, never
        // from callback-supplied executable, bytes, or command arguments.
        // Observation of an already-started attempt must instead use its
        // retained journal identity, even if current policy has since moved.
        let recipe = ryeos_app::operator_external_content::product_qualification::
            resolve_current_bundle_producer_recipe_for_purpose(
                state,
                &grant.purpose.execution_view()?,
                &request.scenario_id,
            )?;
        let admitted_stdin = ryeos_app::operator_external_content::product_qualification::
            admitted_root_producer_stdin(state, thread_id, &grant.purpose.execution_view()?)?;
        let typed_owner: ryeos_app::runtime_db::LaunchOwner = serde_json::from_str(owner)?;
        let key = ryeos_app::scoped_producer_authority::ScopedProducerAuthorityKey::new(
            thread_id.to_owned(),
            typed_owner,
        )?;
        let attempt =
            ryeos_app::scoped_producer_authority::ScopedProducerAttemptCoordinate::derive(
                &key,
                &request.scenario_id,
                &recipe.source_identity()?,
                &admitted_stdin,
            )?;
        let attempt_id = attempt.attempt_id();
        if let Some(retained) = state.state_store.scoped_child_attempt(attempt_id)? {
            if retained.initial.owner == key.launch_owner
                && state.scoped_producer_processes.contains_exact(&retained)?
            {
                // The previous ACK may have been lost. The exact process handle is
                // still owned here (the OS child may already have exited); return its locator without touching the
                // producer or minting another scope.
                return scoped_attempt_locator(state, &retained);
            }
            bail!(
                "scoped child attempt already exists but no exact live child can be acknowledged"
            );
        }
        let live = state.scoped_producer_authorities.get_exact(&key)?;
        // The eventual isolated launch must carry this descriptor as its
        // retained workspace view. A daemon-owned scratch pathname alone
        // cannot prove which verifier root owns the writable directory.
        let _workspace_view = live.workspace_view()?;
        let selected_command = live.admitted_command_for_recipe(
            &request.scenario_id,
            &recipe.source_identity()?,
            &recipe.recipe.executable_source,
        )?;
        let _process_request =
            live.request_for_recipe(&recipe.recipe, &admitted_stdin, &selected_command)?;
        // Planning is read-only. The later one-shot start must durably reserve
        // this exact allocation before asking Lillux to create it.
        let attempt_id = ryeos_app::scoped_producer_start::start_scoped_producer(
            state,
            &key,
            &grant.purpose.execution_view()?,
            &request.scenario_id,
            &recipe,
            &admitted_stdin,
            &attempt,
            &grant.isolation_class,
            cap.expires_at.clone(),
        )?;
        let retained = state
            .state_store
            .scoped_child_attempt(&attempt_id)?
            .ok_or_else(|| anyhow::anyhow!("started scoped child has no retained attempt"))?;
        return scoped_attempt_locator(state, &retained);
    } else if resume.is_some() {
        // A lost START acknowledgement may outlive a signed recipe update.
        // Resolve only the original unique root owner and require its exact
        // retained process handle. This operation cannot choose a scenario or launch.
        let typed_owner: ryeos_app::runtime_db::LaunchOwner = serde_json::from_str(owner)?;
        let wait_deadline = cap.expires_at;
        loop {
            if wait_deadline.has_elapsed() {
                bail!("scoped child start remained unsettled through exact resume deadline");
            }
            if let Some(retained) = state
                .state_store
                .scoped_child_attempt_for_owner(&typed_owner)?
            {
                if retained.initial.owner != typed_owner
                    || retained.initial.owner.thread_id != thread_id
                {
                    bail!("scoped child retained attempt differs from root owner");
                }
                if state.scoped_producer_processes.contains_exact(&retained)? {
                    return scoped_attempt_locator(state, &retained);
                }
                #[cfg(feature = "handoff-test-support")]
                if retained.phase
                    == ryeos_app::runtime_db::scoped_child_attempt::ScopedChildPhase::Reserved
                {
                    if let Some(gate) = state.extensions.get::<
                        ryeos_app::scoped_producer_start::test_support::ReservedAttemptGate,
                    >() {
                        let key = ryeos_app::scoped_producer_authority::ScopedProducerAuthorityKey::new(
                            thread_id.to_owned(),
                            typed_owner.clone(),
                        )?;
                        gate.note_resume_pending(&key, &retained.initial.attempt_id)?;
                    }
                }
                if !matches!(
                    retained.phase,
                    ryeos_app::runtime_db::scoped_child_attempt::ScopedChildPhase::Reserved
                        | ryeos_app::runtime_db::scoped_child_attempt::ScopedChildPhase::ScopeBound
                        | ryeos_app::runtime_db::scoped_child_attempt::ScopedChildPhase::ProcessAttached
                ) {
                    bail!("scoped child has no exact live process to resume");
                }
            }
            // START registers its exact in-flight key before scope allocation
            // and notifies this registry on insertion or failed settlement.
            // Re-read the owner-unique journal after every wake. The bounded
            // fallback interval covers a notification raced with this wait.
            state
                .scoped_producer_processes
                .wait_for_change(wait_deadline.remaining().min(Duration::from_secs(1)))?;
        }
    } else if let Some(request) = observe.as_ref() {
        // Observation locates the exact retained journal row; it does not
        // re-resolve a current recipe or mint a replacement attempt when the
        // signed Bundle generation moves after child release.
        let typed_owner: ryeos_app::runtime_db::LaunchOwner = serde_json::from_str(owner)?;
        let retained = state
            .state_store
            .scoped_child_attempt(&request.attempt_id)?
            .ok_or_else(|| anyhow::anyhow!("scoped child attempt is not retained"))?;
        if retained.initial.owner != typed_owner || retained.initial.owner.thread_id != thread_id {
            bail!("scoped child observation differs from its exact root launch owner");
        }
        let key = ryeos_app::scoped_producer_process::ScopedProducerProcessKey::new(
            request.attempt_id.clone(),
            typed_owner,
        )?;
        return ryeos_app::scoped_producer_observe::observe_scoped_producer(
            state,
            &key,
            cap.expires_at.clone(),
        );
    }

    bail!("scoped child {method} has no selected operation")
}

fn scoped_attempt_locator(
    state: &AppState,
    record: &ryeos_app::runtime_db::scoped_child_attempt::ScopedChildAttemptRecord,
) -> Result<Value> {
    let (expected, applied, isolation_plan_digest) = state
        .scoped_producer_processes
        .live_applied_evidence_exact(record)?
        .ok_or_else(|| anyhow::anyhow!("scoped locator has no exact live prelaunch target"))?;
    let held_mounts = record
        .mount_preparation_evidence
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scoped locator has no retained held mount preparation"))?;
    anyhow::ensure!(
        held_mounts.plan_digest == isolation_plan_digest
            && held_mounts
                .observed
                .matches_commitments(&held_mounts.expected)
            && applied.matches_post_release_mounts(&held_mounts.expected),
        "scoped locator held mount preparation differs from compiled plan"
    );
    Ok(serde_json::json!({
        "schema": "ryeos.scoped_producer_locator.v7",
        "attempt_id": record.initial.attempt_id,
        "recipe_digest": record.initial.recipe_digest,
        "recipe_generation": record.initial.recipe_generation,
        "scenario_digest": record.initial.scenario_digest,
        "isolation_plan_digest": isolation_plan_digest,
        "expected_applied_launch": expected,
        "applied_launch": applied,
        "expected_mount_preparation": held_mounts.expected,
        "held_mount_preparation": held_mounts.observed,
        "prepared_directory_sources": held_mounts.prepared_directory_sources,
        "prepared_immutable_sha256": held_mounts.prepared_immutable_sha256,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_excludes_caller_supplied_launch_authority() {
        for field in [
            "attempt_id",
            "executable",
            "path",
            "recipe",
            "recipe_digest",
            "scope_allocation",
            "process_identity",
            "stdout",
        ] {
            let mut request = json!({"thread_id":"T-1", "scenario_id":"smoke"});
            request[field] = json!("caller-controlled");
            assert!(parse_start_request(&request).is_err(), "accepted {field}");
        }
    }

    #[test]
    fn scenario_identifier_is_bounded_and_pathless() {
        assert!(
            parse_start_request(&json!({"thread_id":"T-1", "scenario_id":"native_codex"})).is_ok()
        );
        for scenario in ["", "../escape", "/absolute", "a b", "a\\b"] {
            assert!(
                parse_start_request(&json!({"thread_id":"T-1", "scenario_id":scenario})).is_err()
            );
        }
        assert!(
            parse_start_request(&json!({"thread_id":"T-1", "scenario_id":"a".repeat(129)}))
                .is_err()
        );
        assert!(parse_start_request(&json!({"thread_id":"T-1", "scenario_id":"smoke.v1"})).is_ok());
    }

    #[test]
    fn observation_accepts_only_exact_attempt_locator_not_a_new_scenario() {
        let attempt_id = format!("scoped-{}", "a".repeat(64));
        assert!(
            parse_observe_request(&json!({"thread_id":"T-1", "attempt_id":attempt_id})).is_ok()
        );
        for attempt_id in ["scoped-abc", "scoped-../other", "scoped-A"].iter() {
            assert!(
                parse_observe_request(&json!({"thread_id":"T-1", "attempt_id":attempt_id}))
                    .is_err()
            );
        }
        assert!(
            parse_observe_request(
                &json!({"thread_id":"T-1", "attempt_id":format!("scoped-{}", "A".repeat(64))})
            )
            .is_err()
        );
        assert!(parse_observe_request(&json!({"thread_id":"T-1", "attempt_id":format!("scoped-{}", "a".repeat(64)), "scenario_id":"smoke"})).is_err());
    }

    #[test]
    fn abort_accepts_only_exact_attempt_and_no_launch_authority() {
        let attempt_id = format!("scoped-{}", "a".repeat(64));
        assert!(parse_abort_request(&json!({"thread_id":"T-1", "attempt_id":attempt_id})).is_ok());
        for field in [
            "scenario_id",
            "recipe",
            "executable",
            "scope",
            "process_identity",
        ] {
            let mut request = json!({"thread_id":"T-1", "attempt_id":attempt_id});
            request[field] = json!("caller-controlled");
            assert!(parse_abort_request(&request).is_err(), "accepted {field}");
        }
        assert!(
            parse_abort_request(&json!({
                "thread_id":"T-1", "attempt_id":format!("scoped-{}", "A".repeat(64))
            }))
            .is_err()
        );
    }

    #[test]
    fn resume_carries_only_exact_root_selector() {
        assert!(parse_resume_request(&json!({"thread_id":"T-1"})).is_ok());
        for field in ["scenario_id", "attempt_id", "recipe", "executable"] {
            let mut request = json!({"thread_id":"T-1"});
            request[field] = json!("caller-controlled");
            assert!(parse_resume_request(&request).is_err(), "accepted {field}");
        }
    }
}
