//! Authenticated consumer-verifier delivery, observation and settlement.
//! Record interpretation remains at the product edge. Inputs and observations
//! are read-only; settlement uses the existing exact-occurrence termination
//! journal. No executable custody, credential or publication permission leaves
//! the daemon through these responses.

use anyhow::{Result, ensure};
use ryeos_app::callback_token::CallbackCapability;
use ryeos_app::state::AppState;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputsRequest {
    thread_id: String,
    selection: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerVerificationInputSelection,
}

fn parse_inputs(params: &Value, thread_id: &str) -> Result<InputsRequest> {
    ensure!(
        serde_json::to_vec(params)?.len() <= 16 * 1024,
        "consumer verification request exceeds callback ceiling"
    );
    let request: InputsRequest = serde_json::from_value(params.clone())?;
    ensure!(
        request.thread_id == thread_id,
        "consumer verification inputs name another root"
    );
    request.selection.validate()?;
    Ok(request)
}

pub(super) async fn inputs(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    let request = parse_inputs(params, &cap.thread_id)?;
    let state = state.clone();
    let token = cap.token.clone();
    // Cancellation of the RPC may abandon only an inert read. No provider
    // occurrence or executable permission can escape this blocking owner.
    tokio::task::spawn_blocking(move || {
        let limits = state.node_policy.require::<
            ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy
        >()?.closure_limits()?;
        serde_json::to_value(
            ryeos_app::operator_external_content::product_qualification::consumer_verification_inputs_for_callback(
                &state, &token, &request.thread_id, &request.selection, limits,
            )?
        ).map_err(Into::into)
    }).await.map_err(|error| anyhow::anyhow!("consumer input read did not settle: {error}"))?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveRequest {
    thread_id: String,
    qualification_operation_id: String,
    coordinate: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
}

/// Product-edge record bytes are opaque to the generic transport. The daemon
/// selects the executable/archive members independently from retained custody.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    thread_id: String,
    qualification_operation_id: String,
    coordinate: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    record: String,
}

fn parse_start(params: &Value, thread_id: &str) -> Result<StartRequest> {
    use ryeos_external_execution_contract::restored_runtime_measurement::MAX_CONSUMER_INPUT_RECORD_BYTES;
    // JSON string escaping can expand opaque UTF-8 bytes by six times. Bound
    // both framing and decoded bytes without accepting caller filenames.
    ensure!(
        serde_json::to_vec(params)?.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES * 6 + 16 * 1024,
        "consumer start request exceeds transport ceiling"
    );
    let request: StartRequest = serde_json::from_value(params.clone())?;
    ensure!(
        request.thread_id == thread_id && request.coordinate.accepted_root_id == thread_id,
        "consumer start names another root"
    );
    request.coordinate.validate()?;
    ensure!(
        lillux::valid_hash(&request.qualification_operation_id),
        "consumer start requires exact qualification digest"
    );
    ensure!(
        !request.record.is_empty() && request.record.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
        "consumer input record exceeds decoded allowance"
    );
    Ok(request)
}

pub(super) async fn start(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    let request = parse_start(params, &cap.thread_id)?;
    let state = state.clone();
    let token = cap.token.clone();
    // State retains the controller exclusion lifetime. This blocking owner
    // keeps archive/scratch custody until the durable one-shot attempt settles,
    // even if the RPC waiter disappears; ambiguous attempts are not relaunched.
    tokio::task::spawn_blocking(move || {
        let limits = state.node_policy.require::<
            ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy
        >()?.closure_limits()?;
        let prepared = ryeos_app::operator_external_content::product_qualification::prepare_retained_consumer_verifier_for_callback(
            &state, &token, &request.coordinate, &request.qualification_operation_id, limits,
        )?;
        let attempt = ryeos_app::operator_runtime_snapshot::prepare_consumer_verifier_attempt_for_callback(
            &state, &token, &prepared, &request.qualification_operation_id, limits,
        )?;
        let deadline = attempt.deadline();
        let scratch_name = format!("consumer-delivery-{}", lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>()));
        let (_, scratch) = ryeos_app::temp_dir_guard::create_projectless_workspace(
            &state.config.runtime_root().cache(), &scratch_name,
        )?;
        let archive = ryeos_executor::execution::external_consumer_delivery::prepare_retained_consumer_archive(
            &state, &prepared, request.record.as_bytes(), scratch.owned_scratch_root()?, deadline,
        )?;
        let intent = attempt.bind_archive(&archive)?;
        serde_json::to_value(ryeos_app::operator_runtime_snapshot::verify_consumer_qualification_occurrence_for_callback(
            &state, &token, &intent, &archive, limits, deadline,
        )?).map_err(Into::into)
    }).await.map_err(|error| anyhow::anyhow!("consumer start owner did not settle: {error}"))?
}

pub(super) async fn settle(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    let request = parse_observe(params, &cap.thread_id)?;
    let state = state.clone();
    let token = cap.token.clone();
    // The existing durable termination journal owns a claimed provider
    // operation even if the RPC waiter disappears. A retry reconciles it.
    tokio::task::spawn_blocking(move || {
        let limits = state.node_policy.require::<
            ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy
        >()?.closure_limits()?;
        serde_json::to_value(ryeos_app::operator_runtime_snapshot::terminate_consumer_qualification_occurrence_for_callback(
            &state, &token, &request.coordinate, &request.qualification_operation_id, limits,
        )?).map_err(Into::into)
    }).await.map_err(|error| anyhow::anyhow!("consumer termination owner did not settle: {error}"))?
}

fn parse_observe(params: &Value, thread_id: &str) -> Result<ObserveRequest> {
    ensure!(
        serde_json::to_vec(params)?.len() <= 16 * 1024,
        "consumer observation exceeds callback ceiling"
    );
    let request: ObserveRequest = serde_json::from_value(params.clone())?;
    ensure!(
        request.thread_id == thread_id && request.coordinate.accepted_root_id == thread_id,
        "consumer observation names another root"
    );
    request.coordinate.validate()?;
    ensure!(
        lillux::valid_hash(&request.qualification_operation_id),
        "consumer observation requires exact operation digests"
    );
    Ok(request)
}

pub(super) async fn observe(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    let request = parse_observe(params, &cap.thread_id)?;
    let state = state.clone();
    let token = cap.token.clone();
    tokio::task::spawn_blocking(move || {
        let limits = state
            .node_policy
            .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
            .closure_limits()?;
        serde_json::to_value(
            ryeos_app::operator_runtime_snapshot::observe_consumer_verifier_attempt_for_callback(
                &state,
                &token,
                &request.coordinate,
                &request.qualification_operation_id,
                limits,
            )?,
        )
        .map_err(Into::into)
    })
    .await
    .map_err(|error| anyhow::anyhow!("consumer observation read did not settle: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> Value {
        let hash = "a".repeat(64);
        json!({"thread_id":"T-root", "qualification_operation_id":hash,
            "coordinate": {"schema":1, "accepted_root_id":"T-root",
                "accepted_capsule_hash":hash, "qualification_purpose_digest":hash,
                "scenario_id":"consumer", "scenario_source_digest":hash,
                "subject_digest":hash, "use_digest":hash,
                "prerequisite_measurement_attempt_id":hash,
                "prerequisite_measurement_observation_digest":hash}})
    }

    fn inputs_request() -> Value {
        json!({"thread_id":"T-root", "selection": {
            "scenario_id":"consumer", "qualification_operation_id":"a".repeat(64),
            "prerequisite_measurement_attempt_id":"b".repeat(64),
        }})
    }

    #[test]
    fn consumer_inputs_request_is_closed_bounded_and_exact_root() {
        parse_inputs(&inputs_request(), "T-root").unwrap();
        assert!(parse_inputs(&inputs_request(), "T-other").is_err());
        assert!(parse_inputs(&request(), "T-root").is_err());
        for (field, value) in [
            ("executable", json!("/ambient/codex")),
            ("coordinate", request()["coordinate"].clone()),
            ("requirement", json!({})),
            ("padding", json!("x".repeat(16 * 1024))),
        ] {
            let mut bad = inputs_request();
            bad[field] = value;
            assert!(parse_inputs(&bad, "T-root").is_err());
        }
        for field in [
            "qualification_operation_id",
            "prerequisite_measurement_attempt_id",
        ] {
            let mut bad = inputs_request();
            bad["selection"][field] = json!("not-an-operation");
            assert!(parse_inputs(&bad, "T-root").is_err());
        }
    }

    #[test]
    fn consumer_observation_request_is_closed_bounded_and_exact() {
        let mut valid = request();
        parse_observe(&valid, "T-root").unwrap();
        assert!(parse_observe(&valid, "T-other").is_err());
        for field in ["qualification_operation_id"] {
            let mut bad = valid.clone();
            bad[field] = json!("invalid");
            assert!(parse_observe(&bad, "T-root").is_err());
        }
        for value in [json!("/ambient/codex"), json!("x".repeat(16 * 1024))] {
            let mut bad = valid.clone();
            bad["executable"] = value;
            assert!(parse_observe(&bad, "T-root").is_err());
        }
        let mut bad = valid.clone();
        bad["coordinate"]["accepted_root_id"] = json!("T-other");
        assert!(parse_observe(&bad, "T-root").is_err());
        valid["operation_id"] = json!("b".repeat(64));
        assert!(parse_observe(&valid, "T-root").is_err());
    }

    #[test]
    fn consumer_start_request_cannot_choose_delivery_authority() {
        let mut valid = request();
        valid["record"] = json!("opaque product-owned record");
        parse_start(&valid, "T-root").unwrap();
        assert!(parse_start(&valid, "T-other").is_err());
        for field in [
            "executable",
            "archive",
            "path",
            "deadline_ms",
            "nonce",
            "supplementary_entries",
        ] {
            let mut bad = valid.clone();
            bad[field] = json!("caller-selected");
            assert!(parse_start(&bad, "T-root").is_err());
        }
        for record in [String::new(), "x".repeat(
            ryeos_external_execution_contract::restored_runtime_measurement::MAX_CONSUMER_INPUT_RECORD_BYTES + 1
        )] {
            let mut bad = valid.clone();
            bad["record"] = json!(record);
            assert!(parse_start(&bad, "T-root").is_err());
        }
    }
}
