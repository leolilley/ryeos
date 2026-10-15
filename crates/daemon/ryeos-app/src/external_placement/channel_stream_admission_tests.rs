//! Stream admission from retained facts: only a live attached binding that
//! selects the stream admits one, and each refusal says which it lacked.

use super::*;
use ryeos_state::external_execution::ExecutionChannelBinding;
use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_STREAM_CONTRACT, ExternalControllerTransportSelection,
};

const OCCURRENCE: &str = "occ-stream";

fn binding(now_ms: i64) -> ExecutionChannelBinding {
    let owner = lillux::crypto::generate_signing_key();
    let supervisor = lillux::crypto::generate_signing_key();
    ExecutionChannelBinding {
        schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
        execution_mode: ryeos_state::external_execution::ExternalExecutionMode::StructuredSession {},
        placement_thread_id: "T-stream".into(),
        occurrence_id: OCCURRENCE.into(),
        allocation_request_digest: "a".repeat(64),
        admitted_capsule_hash: "b".repeat(64),
        base_snapshot_hash: "c".repeat(64),
        execution_binding_hash: "d".repeat(64),
        supervisor_runtime_hash: "e".repeat(64),
        candidate_program_digest: "0".repeat(64),
        channel_nonce: "f".repeat(64),
        owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
            &owner.verifying_key(),
        )
        .unwrap(),
        supervisor_public_key: ryeos_state::external_execution::encode_channel_public_key(
            &supervisor.verifying_key(),
        )
        .unwrap(),
        issued_at_ms: now_ms - 1_000,
        execution_deadline_ms: now_ms + 600_000,
        expires_at_ms: now_ms + 1_200_000,
        candidate_export_max_bytes: 512 * 1024,
        max_frames: 100,
        max_bytes: 1024 * 1024,
    }
}

fn controller(selects_stream: bool) -> ExternalControllerTransportSelection {
    let mut controller = RetainedExternalExecutionBinding::test_fixture()
        .backend_contract()
        .controller_transport;
    if selects_stream {
        controller.http_mut().unwrap().route_contract = EXTERNAL_CHANNEL_STREAM_CONTRACT.into();
    }
    controller
}

fn check(
    phase: ExternalAllocationPhase,
    bound: Option<&str>,
    binding: Option<&ExecutionChannelBinding>,
    controller: &ExternalControllerTransportSelection,
    now_ms: i64,
) -> std::result::Result<ExternalChannelStreamAdmission, ExternalChannelStreamRefusal> {
    check_channel_stream(
        ChannelStreamFacts {
            phase,
            bound_occurrence_id: bound,
            binding,
            controller,
        },
        OCCURRENCE,
        now_ms,
    )
}

fn no_live_channel(
    outcome: std::result::Result<ExternalChannelStreamAdmission, ExternalChannelStreamRefusal>,
    expected: &str,
) {
    match outcome {
        Err(ExternalChannelStreamRefusal::NoLiveChannel(error)) => {
            assert!(format!("{error:#}").contains(expected), "{error:#}")
        }
        other => panic!("expected no live channel ({expected}), got {other:?}"),
    }
}

#[test]
fn a_live_binding_that_selects_the_stream_is_admitted_with_its_bound() {
    let now = lillux::time::timestamp_millis();
    let binding = binding(now);
    let stream = controller(true);
    let admitted = check(
        ExternalAllocationPhase::Bound,
        Some(OCCURRENCE),
        Some(&binding),
        &stream,
        now,
    )
    .unwrap();
    assert_eq!(
        admitted.maximum_response_bytes(),
        stream.http().unwrap().maximum_response_bytes
    );
}

#[test]
fn a_binding_that_does_not_select_the_stream_is_refused_by_its_own_name() {
    let now = lillux::time::timestamp_millis();
    let binding = binding(now);
    let request_response = controller(false);
    assert!(!controller_exchanges_on_stream(&request_response));
    assert!(controller_exchanges_on_stream(&controller(true)));
    assert!(matches!(
        check(
            ExternalAllocationPhase::Bound,
            Some(OCCURRENCE),
            Some(&binding),
            &request_response,
            now,
        ),
        Err(ExternalChannelStreamRefusal::NotSelected)
    ));
}

#[test]
fn a_terminated_allocation_admits_no_stream() {
    let now = lillux::time::timestamp_millis();
    let binding = binding(now);
    no_live_channel(
        check(
            ExternalAllocationPhase::Terminated,
            Some(OCCURRENCE),
            Some(&binding),
            &controller(true),
            now,
        ),
        "terminated",
    );
}

#[test]
fn a_binding_outside_its_lifetime_admits_no_stream() {
    let now = lillux::time::timestamp_millis();
    let binding = binding(now);
    no_live_channel(
        check(
            ExternalAllocationPhase::Bound,
            Some(OCCURRENCE),
            Some(&binding),
            &controller(true),
            binding.expires_at_ms,
        ),
        "lifetime",
    );
    no_live_channel(
        check(
            ExternalAllocationPhase::Bound,
            Some(OCCURRENCE),
            Some(&binding),
            &controller(true),
            binding.issued_at_ms - 1,
        ),
        "lifetime",
    );
}

#[test]
fn a_different_or_missing_occurrence_admits_no_stream() {
    let now = lillux::time::timestamp_millis();
    let binding = binding(now);
    no_live_channel(
        check(
            ExternalAllocationPhase::Bound,
            Some("occ-other"),
            Some(&binding),
            &controller(true),
            now,
        ),
        "different occurrence",
    );
    no_live_channel(
        check(
            ExternalAllocationPhase::Bound,
            None,
            Some(&binding),
            &controller(true),
            now,
        ),
        "no bound occurrence",
    );
    let mut other = binding.clone();
    other.occurrence_id = "occ-other".into();
    no_live_channel(
        check(
            ExternalAllocationPhase::Bound,
            Some(OCCURRENCE),
            Some(&other),
            &controller(true),
            now,
        ),
        "attached channel names a different occurrence",
    );
    no_live_channel(
        check(
            ExternalAllocationPhase::Bound,
            Some(OCCURRENCE),
            None,
            &controller(true),
            now,
        ),
        "no attached channel",
    );
}
