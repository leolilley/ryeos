//! The lease readiness request a lifecycle adapter judges, and the verdict it
//! returns.
//!
//! Workload: a remote lease whose guest has dialed its controller and sent
//! two frames, Ready and one capability report. The controller holds both
//! frames un-journalled and asks the binding's lifecycle adapter whether the
//! guest is the one the lease was activated for and whether its report meets
//! the workload's guest entries. The adapter owns that judgement; this module
//! only composes its request from what the controller retained and reads back
//! the verdict's identity fields. Nothing in the request is taken from the
//! guest: the lease identity, the activation digests and the channel binding
//! come from the lease record and the retained binding, and the admitted
//! stage-one request is embedded exactly as it was retained.
//!
//! The adapter's strict reader re-serializes what it reads in its own field
//! order and refuses a request whose bytes differ, so the mirror structs here
//! keep that order exactly. A golden fixture shared with the adapter core pins
//! the bytes.

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_state::external_execution::ExecutionChannelBinding;
use serde::Serialize;
use serde_json::value::RawValue;

use crate::runtime_db::external_execution::{
    ExternalLeaseAdmission, ExternalLeaseRecord, ExternalLeaseRecordState,
};

/// The adapter's readiness request schema this controller writes.
pub(crate) const LEASE_READY_SCHEMA: u32 = 3;
/// The largest readiness request. It embeds the whole admitted stage-one
/// request, so it can exceed the stage-one bound on its own.
pub(crate) const MAX_LEASE_READY_REQUEST_BYTES: usize = 128 * 1024;
/// The longest refusal reason this controller stores from an adapter verdict.
const MAX_VERDICT_REASON_BYTES: usize = 900;

/// What the controller retained for one lease. None of it comes from the
/// guest.
#[derive(Serialize)]
struct LeaseReadyExpectation<'a> {
    lease_id: &'a str,
    channel: &'a ExecutionChannelBinding,
    activation_request_digest: &'a str,
    signed_import_sha256: &'a str,
    guest_package_sha256: &'a str,
    ready_deadline_ms: i64,
}

/// The adapter's readiness request shape, fields in the adapter's order. A
/// first admission carries no resume state, which serializes as null.
#[derive(Serialize)]
struct LeaseReadyRequest<'a> {
    schema: u32,
    adapter_id: &'a str,
    admission: &'a RawValue,
    admission_request_sha256: &'a str,
    expectation: LeaseReadyExpectation<'a>,
    resume: Option<&'a serde_json::Value>,
}

/// Canonical readiness request bytes for the first admission of the lease
/// `record` describes. `admitted_request` is the stage-one request retained
/// on the reservation, `binding` the channel binding retained for the lease.
pub(crate) fn canonical_request(
    adapter_id: &str,
    admitted_request: &[u8],
    record: &ExternalLeaseRecord,
    binding: &ExecutionChannelBinding,
) -> Result<Vec<u8>> {
    ensure!(
        record.state == ExternalLeaseRecordState::Activated,
        "lease {} is not activated, so it has no readiness request",
        record.placement_thread_id
    );
    let activation = record
        .activation
        .as_ref()
        .context("an activated lease record carries its activation")?;
    ensure!(
        lillux::sha256_hex(admitted_request) == record.admission_request_sha256,
        "the retained admitted request is not the one the lease record was reserved with"
    );
    ensure!(
        binding.placement_thread_id == record.placement_thread_id
            && binding.occurrence_id == activation.lease_id,
        "the retained channel binding is not the lease's"
    );
    let admission = std::str::from_utf8(admitted_request)
        .context("the retained admitted request is not UTF-8")?;
    let admission = RawValue::from_string(admission.to_owned())
        .context("the retained admitted request is not JSON")?;
    let request = LeaseReadyRequest {
        schema: LEASE_READY_SCHEMA,
        adapter_id,
        admission: &admission,
        admission_request_sha256: &record.admission_request_sha256,
        expectation: LeaseReadyExpectation {
            lease_id: &activation.lease_id,
            channel: binding,
            activation_request_digest: &activation.activation_request_digest,
            signed_import_sha256: &activation.signed_import_sha256,
            guest_package_sha256: &activation.guest_package_sha256,
            ready_deadline_ms: activation.ready_deadline_ms,
        },
        resume: None,
    };
    let bytes = ryeos_external_execution_contract::canonical_json(&request)?;
    ensure!(
        bytes.len() <= MAX_LEASE_READY_REQUEST_BYTES,
        "lease readiness request exceeds its bound"
    );
    Ok(bytes)
}

/// The adapter's judgement of a lease's first two guest frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeaseReadyVerdict {
    /// Both frames are authentic and the report meets the workload's guest
    /// entries. Carries what the lease record stores.
    Admitted(ExternalLeaseAdmission),
    /// The lease cannot be used; the reason is the adapter's, made printable
    /// and bounded.
    Terminate { reason: String },
}

/// Accept the adapter's verdict on exactly the lease and frames `record`
/// holds, or refuse it as no verdict. A verdict about another lease or
/// another admitted request is no verdict, and an admitted verdict must name
/// the digests of exactly the two frames held, so the controller journals
/// what the adapter judged and nothing else.
pub(crate) fn read_verdict(
    response: &[u8],
    record: &ExternalLeaseRecord,
    adapter_id: &str,
) -> Result<LeaseReadyVerdict> {
    let verdict: serde_json::Value = ryeos_external_execution_contract::from_json_slice_strict(
        response,
        ryeos_external_execution_contract::MAX_LIFECYCLE_RESPONSE_BYTES,
    )
    .with_context(|| format!("lifecycle adapter `{adapter_id}` returned no readiness verdict"))?;
    let field = |name: &str| verdict.get(name).and_then(serde_json::Value::as_str);
    let activation = record
        .activation
        .as_ref()
        .context("a lease record without activation has no readiness verdict")?;
    ensure!(
        verdict.get("schema").and_then(serde_json::Value::as_u64)
            == Some(u64::from(LEASE_READY_SCHEMA))
            && field("lease_id") == Some(activation.lease_id.as_str()),
        "lifecycle adapter `{adapter_id}` answered lease readiness for a different lease"
    );
    match field("verdict") {
        Some("admitted") => {
            ensure!(
                field("admission_request_sha256") == Some(record.admission_request_sha256.as_str()),
                "lifecycle adapter `{adapter_id}` admitted the lease against a different request"
            );
            let held = |frame: &Option<Vec<u8>>, label: &str| -> Result<String> {
                Ok(lillux::sha256_hex(frame.as_deref().with_context(|| {
                    format!("the lease record holds no {label} frame")
                })?))
            };
            let ready_frame_digest = held(&record.held_ready_frame, "Ready")?;
            let report_frame_digest = held(&record.held_report_frame, "report")?;
            ensure!(
                field("ready_frame_digest") == Some(ready_frame_digest.as_str())
                    && field("report_frame_digest") == Some(report_frame_digest.as_str()),
                "lifecycle adapter `{adapter_id}` judged different frames than the controller holds"
            );
            ensure!(
                field("boot_id").is_some_and(|boot| !boot.is_empty()),
                "lifecycle adapter `{adapter_id}` admitted the lease without a boot id"
            );
            let channel = verdict
                .get("channel")
                .filter(|channel| channel.is_object())
                .with_context(|| {
                    format!(
                        "lifecycle adapter `{adapter_id}` admitted the lease without resume state"
                    )
                })?;
            Ok(LeaseReadyVerdict::Admitted(ExternalLeaseAdmission {
                ready_frame_digest,
                report_frame_digest,
                resume_state_json: serde_json::to_string(channel)?,
            }))
        }
        Some("terminate") => Ok(LeaseReadyVerdict::Terminate {
            reason: plain_reason(field("reason").unwrap_or("no reason given")),
        }),
        _ => bail!("lifecycle adapter `{adapter_id}` returned an unknown readiness verdict"),
    }
}

/// The adapter's reason reaches operators verbatim; keep it printable and
/// bounded.
fn plain_reason(reason: &str) -> String {
    let mut plain = String::new();
    for character in reason.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if plain.len() + character.len_utf8() > MAX_VERDICT_REASON_BYTES {
            plain.push('…');
            break;
        }
        plain.push(character);
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_db::external_execution::ExternalLeaseActivation;
    use ryeos_state::external_execution::{
        EXECUTION_CHANNEL_BINDING_SCHEMA, ExternalExecutionMode,
    };

    /// The golden bytes the adapter core composes for the same inputs.
    const GOLDEN: &[u8] = include_bytes!(
        "../../../../host-adapters/remote-lease-core/fixtures/lease-ready-request.json"
    );

    pub(crate) fn fixture_binding() -> ExecutionChannelBinding {
        ExecutionChannelBinding {
            schema: EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode: ExternalExecutionMode::StructuredSession {},
            placement_thread_id: "T-lease".into(),
            allocation_request_digest: "1".repeat(64),
            occurrence_id: "lease-1".into(),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            candidate_program_digest: "0".repeat(64),
            channel_nonce: "f".repeat(64),
            owner_public_key: "bnoc3Smwt4/ROvTFWY/v9O8qlxZuPKby5Pv8zYBQW/E=".into(),
            supervisor_public_key: "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=".into(),
            issued_at_ms: 100,
            execution_deadline_ms: 10_000,
            expires_at_ms: 20_000,
            candidate_export_max_bytes: 512 * 1024,
            max_frames: 100,
            max_bytes: 1024 * 1024,
        }
    }

    /// The admitted stage-one request the golden fixture embeds, verbatim.
    fn golden_admission() -> Vec<u8> {
        #[derive(serde::Deserialize)]
        struct Probe {
            admission: Box<RawValue>,
        }
        let probe: Probe = serde_json::from_slice(GOLDEN).unwrap();
        probe.admission.get().as_bytes().to_vec()
    }

    pub(crate) fn activated_record(admission: &[u8]) -> ExternalLeaseRecord {
        ExternalLeaseRecord {
            placement_thread_id: "T-lease".into(),
            admission_request_sha256: lillux::sha256_hex(admission),
            activation_span_ms: 1_900,
            state: ExternalLeaseRecordState::Activated,
            activation: Some(ExternalLeaseActivation {
                lease_id: "lease-1".into(),
                activation_request_digest: "2".repeat(64),
                signed_import_sha256: "3".repeat(64),
                guest_package_sha256: "4".repeat(64),
                ready_deadline_ms: 2_000,
            }),
            held_ready_frame: None,
            held_report_frame: None,
            admission: None,
            refusal_reason: None,
            created_at_ms: 100,
            updated_at_ms: 100,
        }
    }

    #[test]
    fn the_request_is_the_adapter_cores_golden_bytes() {
        let admission = golden_admission();
        let record = activated_record(&admission);
        let composed =
            canonical_request("remote-lease-test", &admission, &record, &fixture_binding())
                .unwrap();
        assert!(
            composed == GOLDEN,
            "composed readiness request differs from the golden fixture:\n{}",
            String::from_utf8_lossy(&composed)
        );
        assert!(
            String::from_utf8(composed)
                .unwrap()
                .ends_with(",\"resume\":null}")
        );
    }

    #[test]
    fn the_request_is_refused_for_a_different_admission_or_channel() {
        let admission = golden_admission();
        let record = activated_record(&admission);
        let mut other = admission.clone();
        other.push(b' ');
        let refusal = canonical_request("remote-lease-test", &other, &record, &fixture_binding())
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("not the one the lease record was reserved with"),
            "{refusal}"
        );

        let mut binding = fixture_binding();
        binding.occurrence_id = "lease-2".into();
        let refusal = canonical_request("remote-lease-test", &admission, &record, &binding)
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("not the lease's"), "{refusal}");

        let mut reserved = record.clone();
        reserved.state = ExternalLeaseRecordState::Reserved;
        reserved.activation = None;
        assert!(
            canonical_request(
                "remote-lease-test",
                &admission,
                &reserved,
                &fixture_binding()
            )
            .is_err()
        );
    }

    fn held_record() -> ExternalLeaseRecord {
        let mut record = activated_record(&golden_admission());
        record.held_ready_frame = Some(b"ready-wire".to_vec());
        record.held_report_frame = Some(b"report-wire".to_vec());
        record
    }

    fn admitted(record: &ExternalLeaseRecord) -> serde_json::Value {
        serde_json::json!({
            "verdict": "admitted",
            "schema": 3,
            "lease_id": "lease-1",
            "admission_request_sha256": record.admission_request_sha256,
            "boot_id": "997aad14-f3e6-4990-9d40-d1068e5a38d5",
            "ready_frame_digest": lillux::sha256_hex(b"ready-wire"),
            "report_frame_digest": lillux::sha256_hex(b"report-wire"),
            "met_by": {},
            "channel": {"chain": "state"},
        })
    }

    fn read(
        record: &ExternalLeaseRecord,
        verdict: &serde_json::Value,
    ) -> Result<LeaseReadyVerdict> {
        read_verdict(
            &serde_json::to_vec(verdict).unwrap(),
            record,
            "remote-lease-test",
        )
    }

    #[test]
    fn an_admitted_verdict_must_name_the_held_frames_digests() {
        let record = held_record();
        let LeaseReadyVerdict::Admitted(admission) = read(&record, &admitted(&record)).unwrap()
        else {
            panic!("an admitted verdict was read as terminate")
        };
        assert_eq!(
            admission.ready_frame_digest,
            lillux::sha256_hex(b"ready-wire")
        );
        assert_eq!(
            admission.report_frame_digest,
            lillux::sha256_hex(b"report-wire")
        );
        assert_eq!(admission.resume_state_json, "{\"chain\":\"state\"}");

        for (field, value) in [
            ("ready_frame_digest", lillux::sha256_hex(b"other-ready")),
            ("report_frame_digest", lillux::sha256_hex(b"other-report")),
            ("admission_request_sha256", "9".repeat(64)),
            ("lease_id", "lease-2".to_owned()),
        ] {
            let mut verdict = admitted(&record);
            verdict[field] = value.into();
            assert!(read(&record, &verdict).is_err(), "{field}");
        }
        let mut wrong_schema = admitted(&record);
        wrong_schema["schema"] = 2.into();
        assert!(read(&record, &wrong_schema).is_err());
        for missing in ["boot_id", "channel"] {
            let mut verdict = admitted(&record);
            verdict.as_object_mut().unwrap().remove(missing);
            assert!(read(&record, &verdict).is_err(), "{missing}");
        }
    }

    #[test]
    fn a_verdict_is_judged_against_frames_actually_held() {
        let mut record = held_record();
        let verdict = admitted(&record);
        record.held_report_frame = None;
        assert!(read(&record, &verdict).is_err());
    }

    #[test]
    fn a_terminate_verdict_carries_a_plain_bounded_reason() {
        let record = held_record();
        let reason = format!("bad\nguest {}", "x".repeat(2_000));
        let LeaseReadyVerdict::Terminate { reason } = read(
            &record,
            &serde_json::json!({
                "verdict": "terminate", "schema": 3, "lease_id": "lease-1", "reason": reason
            }),
        )
        .unwrap() else {
            panic!("a terminate verdict was read as admitted")
        };
        assert!(
            reason.starts_with("bad guest xxx") && reason.len() <= 1_000,
            "{reason}"
        );
        assert!(
            read(
                &record,
                &serde_json::json!({"verdict": "terminate", "schema": 3, "lease_id": "other"})
            )
            .is_err()
        );
        assert!(
            read(
                &record,
                &serde_json::json!({"verdict": "maybe", "schema": 3, "lease_id": "lease-1"})
            )
            .is_err()
        );
    }
}
