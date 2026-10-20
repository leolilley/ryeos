//! Source-owned cancellation testimony on the existing seat chain.
//! Shape validation proves neither absence of Prepared nor writer authority.
//! Publication must serialize paired-head absence checks against late Prepare.
use serde::{Deserialize, Serialize};

use crate::transfer::{
    MoveFreezeEvidence, TRANSFER_SCHEMA, TransferEvidence, TransferIdentity, TransferPhase,
    TransferRecord,
};
use crate::transfer_lifecycle_v2::TransferLifecycleFenceV2;

pub const MOVE_FREEZE_CANCELLED_EVENT: &str = "contract.ryeos_ui.move_freeze_cancelled.v1";

/// Narrow commitment projection from an ordinary receipt-authenticated event.
/// NOT a valid-Core-freeze proof: renderer export semantics and exact event-time
/// composition admission remain separate owners. Cancellation publication and
/// Core release must additionally validate the original typed preparation.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveFreezePreparationCommitment {
    pub operation_id: String,
    pub transfer: TransferIdentity,
    pub prepared_engine_seq: u64,
    pub root_binding: serde_json::Value,
}

pub fn preparation_commitment(
    payload: &serde_json::Value,
) -> Result<Option<MoveFreezePreparationCommitment>, String> {
    use serde_json::Value;
    if payload.pointer("/body/kind").and_then(Value::as_str) != Some("move_freeze") {
        return Ok(None);
    }
    let operation_id = payload
        .get("operation_id")
        .and_then(Value::as_str)
        .ok_or("freeze operation identity absent")?;
    let request = payload
        .pointer("/body/request")
        .ok_or("freeze request absent")?;
    fn closed(value: &Value, keys: &[&str]) -> bool {
        value.as_object().is_some_and(|object| {
            object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key))
        })
    }
    if !closed(
        payload,
        &[
            "schema",
            "operation_id",
            "producer",
            "prepared_engine_seq",
            "root_binding",
            "body",
            "request_bounds",
        ],
    ) || !closed(
        payload.get("body").ok_or("freeze body absent")?,
        &["kind", "request"],
    ) || !closed(
        request,
        &[
            "schema",
            "operation_id",
            "transfer",
            "root_binding",
            "export",
            "instances",
            "view_sets",
        ],
    ) || !closed(
        payload.get("producer").ok_or("freeze producer absent")?,
        &["presentation_id", "thread_id", "producer_incarnation"],
    ) || !closed(
        payload
            .get("request_bounds")
            .ok_or("freeze bounds absent")?,
        &[
            "max_request_bytes",
            "max_input_bytes",
            "max_seat_append_bytes",
        ],
    ) || !closed(
        payload.get("root_binding").ok_or("freeze root absent")?,
        &[
            "binding_attachment_id",
            "binding_generation",
            "binding_digest",
        ],
    ) || !request.get("instances").is_some_and(Value::is_array)
        || !request.get("view_sets").is_some_and(Value::is_array)
    {
        return Err("freeze commitment has unknown or absent closed fields".into());
    }
    let transfer: TransferIdentity = serde_json::from_value(
        request
            .get("transfer")
            .cloned()
            .ok_or("freeze transfer absent")?,
    )
    .map_err(|error| error.to_string())?;
    TransferRecord {
        schema: TRANSFER_SCHEMA,
        transfer: transfer.clone(),
        phase: TransferPhase::InboundCommitment,
    }
    .validate_shape()?;
    let root_binding = payload
        .get("root_binding")
        .cloned()
        .ok_or("freeze root absent")?;
    let bounds = payload
        .get("request_bounds")
        .ok_or("freeze bounds absent")?;
    let request_bound = bounds
        .get("max_request_bytes")
        .and_then(Value::as_u64)
        .filter(|bound| *bound > 0)
        .ok_or("freeze request bound is not a positive u64")?;
    let append_bound = bounds
        .get("max_seat_append_bytes")
        .and_then(Value::as_u64)
        .filter(|bound| *bound > 0)
        .ok_or("freeze append bound is not a positive u64")?;
    bounds
        .get("max_input_bytes")
        .and_then(Value::as_u64)
        .ok_or("freeze input bound is not a u64")?;
    if root_binding
        .get("binding_attachment_id")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        || root_binding
            .get("binding_generation")
            .and_then(Value::as_u64)
            .is_none_or(|generation| generation == 0)
        || root_binding
            .get("binding_digest")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || serde_json::to_vec(request)
            .map_err(|error| error.to_string())?
            .len() as u64
            > request_bound
    {
        return Err("freeze commitment root or request bound is invalid".into());
    }
    let prepared_engine_seq = payload
        .get("prepared_engine_seq")
        .and_then(Value::as_u64)
        .ok_or("freeze original engine sequence absent")?;
    let export = request.get("export").ok_or("freeze export absent")?;
    if !closed(export, &["schema", "mode", "content"])
        || export.get("schema").and_then(Value::as_u64) != Some(1)
        || serde_json::to_vec(export)
            .map_err(|error| error.to_string())?
            .len() as u64
            > request_bound
    {
        return Err("freeze export envelope is not the bounded current closed shape".into());
    }
    let content = export
        .get("content")
        .ok_or("freeze export content absent")?;
    let mut instances = Vec::new();
    let mut sets = Vec::new();
    let mut mount = |value: &Value| -> Result<(), String> {
        if !closed(
            value,
            &[
                "source_instance",
                "view_ref",
                "binding",
                "local",
                "subject",
                "route",
                "current_label",
                "drafts",
                "lens_frames",
            ],
        ) || !value.get("drafts").is_some_and(Value::is_array)
            || !value.get("lens_frames").is_some_and(Value::is_array)
            || !value
                .get("current_label")
                .is_some_and(|label| label.is_null() || label.is_string())
        {
            return Err("freeze mount envelope is not closed".into());
        }
        instances.push(
            value
                .get("source_instance")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or("freeze instance is not a string")?
                .to_owned(),
        );
        Ok(())
    };
    match content.get("kind").and_then(Value::as_str) {
        Some("view_sets") if closed(content, &["kind", "active_set", "sets"]) => {
            let exports = content
                .get("sets")
                .and_then(Value::as_array)
                .filter(|sets| !sets.is_empty())
                .ok_or("freeze set collection empty or mistyped")?;
            for set in exports {
                if !closed(
                    set,
                    &[
                        "handle",
                        "source_view_set_id",
                        "source_owner",
                        "binding",
                        "seed",
                        "focused_view",
                        "mounts",
                    ],
                ) {
                    return Err("freeze set envelope is not closed".into());
                }
                sets.push(
                    set.get("source_view_set_id")
                        .and_then(Value::as_u64)
                        .filter(|id| *id > 0)
                        .ok_or("freeze set identity is not positive u64")?,
                );
                for value in set
                    .get("mounts")
                    .and_then(Value::as_array)
                    .ok_or("freeze mounts are not an array")?
                {
                    mount(value)?;
                }
            }
        }
        Some("surface_mount") if closed(content, &["kind", "source_view_set_id", "mount"]) => {
            sets.push(
                content
                    .get("source_view_set_id")
                    .and_then(Value::as_u64)
                    .filter(|id| *id > 0)
                    .ok_or("freeze set identity is not positive u64")?,
            );
            mount(content.get("mount").ok_or("freeze mount absent")?)?;
        }
        Some("shared_dock")
            if closed(content, &["kind", "edge", "visible", "size", "mount"])
                && content.get("visible").is_some_and(Value::is_boolean)
                && content
                    .get("size")
                    .and_then(Value::as_u64)
                    .is_some_and(|size| size <= u64::from(u16::MAX)) =>
        {
            mount(content.get("mount").ok_or("freeze dock mount absent")?)?;
        }
        _ => return Err("freeze content envelope is not the current closed shape".into()),
    }
    let original_instances = instances.len();
    let original_sets = sets.len();
    instances.sort();
    instances.dedup();
    sets.sort_unstable();
    sets.dedup();
    if original_instances != instances.len()
        || original_sets != sets.len()
        || request.get("instances")
            != Some(&serde_json::to_value(&instances).map_err(|error| error.to_string())?)
        || request.get("view_sets")
            != Some(&serde_json::to_value(&sets).map_err(|error| error.to_string())?)
    {
        return Err("freeze inventory differs from exact export envelope closure".into());
    }
    if payload.get("schema").and_then(Value::as_u64) != Some(2)
        || request.get("schema").and_then(Value::as_u64) != Some(1)
        || request.get("operation_id").and_then(Value::as_str) != Some(operation_id)
        || request.get("root_binding") != Some(&root_binding)
        || payload
            .pointer("/producer/presentation_id")
            .and_then(Value::as_str)
            != Some(transfer.source.presentation_id.as_str())
        || payload
            .pointer("/producer/thread_id")
            .and_then(Value::as_str)
            != Some(transfer.source.thread_id.as_str())
        || payload
            .pointer("/producer/producer_incarnation")
            .and_then(Value::as_str)
            != Some(transfer.source.producer_incarnation.as_str())
        || export.get("mode").and_then(Value::as_str) != Some("move")
        || crate::payload_digest::seat_payload_digest(export).map_err(|error| error.to_string())?
            != transfer.export_digest
    {
        return Err("freeze ordinary preparation differs from exact source commitment".into());
    }
    let mut body = payload.get("body").cloned().ok_or("freeze body absent")?;
    body.get_mut("request")
        .and_then(Value::as_object_mut)
        .ok_or("freeze request is not object")?
        .remove("operation_id");
    let identity = crate::freeze_witness::preparation_identity(
        2,
        payload.get("producer").ok_or("freeze producer absent")?,
        prepared_engine_seq,
        &root_binding,
        payload
            .get("request_bounds")
            .ok_or("freeze request bounds absent")?,
        &body,
    )
    .map_err(str::to_owned)?;
    if identity != operation_id {
        return Err("freeze ordinary preparation identity contradicts its body".into());
    }
    if !crate::append_bound::single_seat_append_fits(
        &transfer.source.presentation_id,
        &transfer.source.thread_id,
        &transfer.source.producer_incarnation,
        "seat.presentation_operation.prepared",
        payload,
        append_bound,
    ) {
        return Err("freeze preparation exceeds its retained append envelope".into());
    }
    Ok(Some(MoveFreezePreparationCommitment {
        operation_id: operation_id.into(),
        transfer,
        prepared_engine_seq,
        root_binding,
    }))
}

pub fn validate_completion_commitment(
    payload: &serde_json::Value,
    prepared: &MoveFreezePreparationCommitment,
) -> Result<(), String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Completion {
        operation_id: String,
        result: Receipt,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Receipt {
        schema: u32,
        operation_id: String,
        transfer_operation_id: String,
        export_digest: String,
        source_revision: u64,
    }
    let value: Completion =
        serde_json::from_value(payload.clone()).map_err(|error| error.to_string())?;
    if value.operation_id != prepared.operation_id
        || value.result.operation_id != prepared.operation_id
        || value.result.schema != 1
        || value.result.transfer_operation_id != prepared.transfer.operation_id
        || value.result.export_digest != prepared.transfer.export_digest
        || value.result.source_revision != prepared.transfer.source.revision
    {
        return Err("freeze receipt differs from exact preparation commitment".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MoveFreezeCancellationWriter {
    /// Current admitted producer, independent of immutable transfer provenance.
    CurrentProducer {
        producer_incarnation: String,
        next_engine_seq: u64,
    },
    /// The existing durable lifecycle fence, never a renderer's clock.
    NodeLifecycle { fence: TransferLifecycleFenceV2 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveFreezeCancellationEvidence {
    pub preparation: TransferEvidence,
    /// Explicit null means completed paired scans proved no completion at
    /// these exact cuts. It is never a caller assertion of absence.
    #[serde(deserialize_with = "required_completion")]
    pub completion: Option<TransferEvidence>,
}

fn required_completion<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TransferEvidence>, D::Error> {
    Option::<TransferEvidence>::deserialize(deserializer)
}

impl From<MoveFreezeEvidence> for MoveFreezeCancellationEvidence {
    fn from(value: MoveFreezeEvidence) -> Self {
        Self {
            preparation: value.preparation,
            completion: Some(value.completion),
        }
    }
}

impl MoveFreezeCancellationEvidence {
    pub fn last_witness(&self) -> &TransferEvidence {
        self.completion.as_ref().unwrap_or(&self.preparation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveFreezeCancellation {
    pub schema: u32,
    pub cancellation_id: String,
    pub transfer: TransferIdentity,
    pub freeze: MoveFreezeCancellationEvidence,
    pub writer: MoveFreezeCancellationWriter,
}

impl MoveFreezeCancellation {
    pub fn new(
        transfer: TransferIdentity,
        freeze: MoveFreezeCancellationEvidence,
        writer: MoveFreezeCancellationWriter,
    ) -> Result<Self, String> {
        let mut value = Self {
            schema: 1,
            cancellation_id: String::new(),
            transfer,
            freeze,
            writer,
        };
        value.cancellation_id = value.derive_identity()?;
        value.validate_shape()?;
        Ok(value)
    }

    pub fn validate_shape(&self) -> Result<(), String> {
        // Reuse the exact two-seat identity vocabulary without pretending this
        // data-only validation has observed an inbound commitment.
        TransferRecord {
            schema: TRANSFER_SCHEMA,
            transfer: self.transfer.clone(),
            phase: TransferPhase::InboundCommitment,
        }
        .validate_shape()?;
        if self.schema != 1
            || self.freeze.completion.as_ref().is_some_and(|completion| {
                self.freeze.preparation.chain_seq >= completion.chain_seq
                    || self.freeze.preparation.event_hash == completion.event_hash
            })
            || self
                .evidence_links()
                .iter()
                .any(|link| !canonical_hash(&link.event_hash))
            || self.cancellation_id != self.derive_identity()?
        {
            return Err(
                "MOVE freeze cancellation differs from its closed identity or ordered evidence"
                    .into(),
            );
        }
        match &self.writer {
            MoveFreezeCancellationWriter::CurrentProducer {
                producer_incarnation,
                ..
            } if canonical_hash(producer_incarnation) => {}
            MoveFreezeCancellationWriter::NodeLifecycle { fence }
                if fence.reaping_at_ms() >= 0 && fence.latest_control().chain_seq > 0 => {}
            _ => return Err("MOVE freeze cancellation writer witness is malformed".into()),
        }
        Ok(())
    }

    pub fn owning_seat(&self) -> &crate::transfer::TransferSeat {
        &self.transfer.source
    }

    pub fn evidence_links(&self) -> Vec<&TransferEvidence> {
        let mut links = vec![&self.freeze.preparation];
        if let Some(completion) = &self.freeze.completion {
            links.push(completion);
        }
        if let MoveFreezeCancellationWriter::NodeLifecycle { fence } = &self.writer {
            links.push(fence.latest_control());
        }
        links
    }

    pub fn validate_bounds(&self, max_bytes: u64) -> Result<(), String> {
        self.validate_shape()?;
        if max_bytes == 0
            || serde_json::to_vec(self)
                .map_err(|error| error.to_string())?
                .len() as u64
                > max_bytes
        {
            return Err("MOVE freeze cancellation exceeds existing seat payload bounds".into());
        }
        Ok(())
    }

    fn derive_identity(&self) -> Result<String, String> {
        crate::payload_digest::seat_payload_digest(&serde_json::json!({
            "domain": MOVE_FREEZE_CANCELLED_EVENT, "schema": self.schema,
            "transfer": self.transfer, "freeze": self.freeze, "writer": self.writer,
        }))
        .map_err(|error| error.to_string())
    }
}

fn canonical_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Decode an exact source-chain object. This does not authenticate its signer,
/// linked receipts, current producer, or the paired absence of Prepared.
pub fn from_event_value(value: &serde_json::Value) -> Result<MoveFreezeCancellation, String> {
    if value.get("event_type").and_then(serde_json::Value::as_str)
        != Some(MOVE_FREEZE_CANCELLED_EVENT)
    {
        return Err("not a MOVE freeze cancellation event".into());
    }
    let record: MoveFreezeCancellation = serde_json::from_value(
        value
            .get("payload")
            .cloned()
            .ok_or("MOVE freeze cancellation payload absent")?,
    )
    .map_err(|error| error.to_string())?;
    record.validate_shape()?;
    if value.get("thread_id").and_then(serde_json::Value::as_str)
        != Some(record.transfer.source.thread_id.as_str())
        || value
            .get("chain_root_id")
            .and_then(serde_json::Value::as_str)
            != Some(record.transfer.source.chain_root_id.as_str())
        || value
            .get("chain_seq")
            .and_then(serde_json::Value::as_u64)
            .is_none_or(|seq| seq <= record.freeze.last_witness().chain_seq)
    {
        return Err(
            "MOVE freeze cancellation is outside its exact source chain or freeze order".into(),
        );
    }
    if let MoveFreezeCancellationWriter::NodeLifecycle { fence } = &record.writer
        && value
            .get("chain_seq")
            .and_then(serde_json::Value::as_u64)
            .is_none_or(|seq| fence.latest_control().chain_seq >= seq)
    {
        return Err("MOVE freeze cancellation precedes its historical control fence".into());
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::TransferSeat;
    fn fixture() -> MoveFreezeCancellation {
        let seat = |id: &str, side: &str| TransferSeat {
            presentation_id: id.into(),
            thread_id: format!("thread-{side}"),
            chain_root_id: format!("root-{side}"),
            producer_incarnation: "ab".repeat(32),
            revision: 9,
        };
        MoveFreezeCancellation::new(
            TransferIdentity {
                operation_id: "33333333-3333-4333-8333-333333333333".into(),
                source: seat("11111111-1111-4111-8111-111111111111", "source"),
                destination: seat("22222222-2222-4222-8222-222222222222", "dest"),
                export_digest: "cd".repeat(32),
            },
            MoveFreezeEvidence {
                preparation: TransferEvidence {
                    event_hash: "01".repeat(32),
                    chain_seq: 7,
                },
                completion: TransferEvidence {
                    event_hash: "02".repeat(32),
                    chain_seq: 11,
                },
            }
            .into(),
            MoveFreezeCancellationWriter::CurrentProducer {
                producer_incarnation: "ef".repeat(32),
                next_engine_seq: 8192,
            },
        )
        .unwrap()
    }
    #[test]
    fn shared_receipt_projection_commits_closed_preparation_and_completion() {
        let mut transfer = fixture().transfer;
        let export = serde_json::json!({"schema":1,"mode":"move","content":{"kind":"view_sets","active_set":null,"sets":[{
            "handle":"empty","source_view_set_id":1,"source_owner":{"kind":"surface","presentation_id":transfer.source.presentation_id,"surface_instance_id":"surface"},
            "binding":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"ab".repeat(32)},
            "seed":{"schema":2,"id":"empty","title":"Empty","root":null},"focused_view":null,"mounts":[]
        }]}});
        transfer.export_digest = crate::payload_digest::seat_payload_digest(&export).unwrap();
        let producer = serde_json::json!({"presentation_id":transfer.source.presentation_id,"thread_id":transfer.source.thread_id,"producer_incarnation":transfer.source.producer_incarnation});
        let root = serde_json::json!({"binding_attachment_id":"root","binding_generation":1,"binding_digest":"ab".repeat(32)});
        let bounds = serde_json::json!({"max_request_bytes":65536,"max_input_bytes":65536,"max_seat_append_bytes":65536});
        let mut body = serde_json::json!({"kind":"move_freeze","request":{"schema":1,"transfer":transfer,"root_binding":root,"export":export,"instances":[],"view_sets":[1]}});
        let id =
            crate::freeze_witness::preparation_identity(2, &producer, 4096, &root, &bounds, &body)
                .unwrap();
        body["request"]["operation_id"] = serde_json::json!(id);
        let mut payload = serde_json::json!({"schema":2,"operation_id":id,"producer":producer,"prepared_engine_seq":4096,"root_binding":root,"request_bounds":bounds,"body":body});
        let commitment = preparation_commitment(&payload).unwrap().unwrap();
        let mut completion = serde_json::json!({"operation_id":id,"result":{"schema":1,"operation_id":id,"transfer_operation_id":commitment.transfer.operation_id,"export_digest":commitment.transfer.export_digest,"source_revision":commitment.transfer.source.revision}});
        validate_completion_commitment(&completion, &commitment).unwrap();
        // Full current mount envelope is committed, including an explicit
        // absent lens label and the shared dock's local presentation state.
        let mut mounted = payload.clone();
        let instance = "seat:source:shared:dock:left";
        mounted["body"]["request"]["export"]["content"] = serde_json::json!({
            "kind":"shared_dock", "edge":"left", "visible":true, "size":24,
            "mount":{
                "source_instance":instance, "view_ref":"view:test/list",
                "binding":root, "local":null, "subject":{"mode":"unresolved"},
                "route":{"mode":"authored"}, "current_label":null,
                "drafts":[], "lens_frames":[]
            }
        });
        mounted["body"]["request"]["instances"] = serde_json::json!([instance]);
        mounted["body"]["request"]["view_sets"] = serde_json::json!([]);
        mounted["body"]["request"]["transfer"]["export_digest"] = serde_json::json!(
            crate::payload_digest::seat_payload_digest(&mounted["body"]["request"]["export"])
                .unwrap()
        );
        mounted["body"]["request"]
            .as_object_mut()
            .unwrap()
            .remove("operation_id");
        let mounted_id = crate::freeze_witness::preparation_identity(
            2,
            &producer,
            4096,
            &root,
            &bounds,
            &mounted["body"],
        )
        .unwrap();
        mounted["operation_id"] = serde_json::json!(mounted_id);
        mounted["body"]["request"]["operation_id"] = serde_json::json!(mounted_id);
        assert!(preparation_commitment(&mounted).unwrap().is_some());
        // Recommit malformed payloads so rejection proves the closed shape,
        // rather than merely detecting an unrelated stale hash or identity.
        let recommit = |value: &mut serde_json::Value| {
            value["body"]["request"]["transfer"]["export_digest"] = serde_json::json!(
                crate::payload_digest::seat_payload_digest(&value["body"]["request"]["export"])
                    .unwrap()
            );
            value["body"]["request"]
                .as_object_mut()
                .unwrap()
                .remove("operation_id");
            let operation = crate::freeze_witness::preparation_identity(
                2,
                &producer,
                4096,
                &root,
                &bounds,
                &value["body"],
            )
            .unwrap();
            value["operation_id"] = serde_json::json!(operation);
            value["body"]["request"]["operation_id"] = serde_json::json!(operation);
        };
        for field in ["visible", "size"] {
            let mut missing = mounted.clone();
            missing["body"]["request"]["export"]["content"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            recommit(&mut missing);
            assert!(
                preparation_commitment(&missing)
                    .unwrap_err()
                    .contains("content envelope")
            );
        }
        let mut missing = mounted.clone();
        missing["body"]["request"]["export"]["content"]["mount"]
            .as_object_mut()
            .unwrap()
            .remove("current_label");
        recommit(&mut missing);
        assert!(
            preparation_commitment(&missing)
                .unwrap_err()
                .contains("mount envelope")
        );
        for malformed in [serde_json::json!(false), serde_json::json!({})] {
            let mut invalid = mounted.clone();
            invalid["body"]["request"]["export"]["content"]["mount"]["current_label"] = malformed;
            recommit(&mut invalid);
            assert!(
                preparation_commitment(&invalid)
                    .unwrap_err()
                    .contains("mount envelope")
            );
        }
        for (field, malformed) in [
            ("visible", serde_json::json!("true")),
            ("visible", serde_json::Value::Null),
            ("size", serde_json::json!(-1)),
            ("size", serde_json::json!(65536)),
        ] {
            let mut invalid = mounted.clone();
            invalid["body"]["request"]["export"]["content"][field] = malformed;
            recommit(&mut invalid);
            assert!(
                preparation_commitment(&invalid)
                    .unwrap_err()
                    .contains("content envelope")
            );
        }
        completion["result"]["source_revision"] = serde_json::json!(10);
        assert!(validate_completion_commitment(&completion, &commitment).is_err());
        payload["body"]["request"]["grant"] = serde_json::json!(true);
        assert!(preparation_commitment(&payload).is_err());
        payload["body"]["request"]
            .as_object_mut()
            .unwrap()
            .remove("grant");
        payload["request_bounds"]["max_request_bytes"] = serde_json::json!("65536");
        assert!(
            preparation_commitment(&payload)
                .unwrap_err()
                .contains("positive u64")
        );
        payload["request_bounds"]["max_request_bytes"] = serde_json::json!(65536);
        payload["body"]["request"]["export"] = serde_json::json!({"mode":"move","bogus":true});
        assert!(
            preparation_commitment(&payload)
                .unwrap_err()
                .contains("export envelope")
        );
    }

    #[test]
    fn cancellation_is_source_owned_and_commits_nonadjacent_exact_links() {
        let value = fixture();
        value.validate_bounds(65536).unwrap();
        assert_eq!(value.owning_seat(), &value.transfer.source);
        assert_eq!(value.evidence_links().len(), 2);
        let mut wrong = value.clone();
        wrong.freeze.completion.as_mut().unwrap().chain_seq = 7;
        assert!(wrong.validate_shape().is_err());
        let mut wrong = value.clone();
        wrong.transfer.source.revision += 1;
        assert!(wrong.validate_shape().is_err());
        assert!(value.validate_bounds(1).is_err());
        let mut unknown = serde_json::to_value(value).unwrap();
        unknown["grant"] = serde_json::json!(true);
        assert!(serde_json::from_value::<MoveFreezeCancellation>(unknown).is_err());
    }
    #[test]
    fn pending_cancellation_requires_explicit_null_and_links_only_preparation() {
        let original = fixture();
        let value = MoveFreezeCancellation::new(
            original.transfer,
            MoveFreezeCancellationEvidence {
                preparation: original.freeze.preparation,
                completion: None,
            },
            original.writer,
        )
        .unwrap();
        assert_eq!(value.evidence_links().len(), 1);
        let mut wire = serde_json::to_value(&value).unwrap();
        assert!(wire["freeze"]["completion"].is_null());
        assert_eq!(
            serde_json::from_value::<MoveFreezeCancellation>(wire.clone()).unwrap(),
            value
        );
        wire["freeze"].as_object_mut().unwrap().remove("completion");
        assert!(serde_json::from_value::<MoveFreezeCancellation>(wire).is_err());
        let event = serde_json::json!({"event_type":MOVE_FREEZE_CANCELLED_EVENT,"thread_id":value.transfer.source.thread_id,"chain_root_id":value.transfer.source.chain_root_id,"chain_seq":8,"payload":value});
        from_event_value(&event).unwrap();
    }

    #[test]
    fn expiry_cancellation_reuses_durable_lifecycle_fence() {
        let original = fixture();
        let value = MoveFreezeCancellation::new(
            original.transfer,
            original.freeze,
            MoveFreezeCancellationWriter::NodeLifecycle {
                fence: TransferLifecycleFenceV2::LeaseReaping {
                    latest_control: TransferEvidence {
                        event_hash: "03".repeat(32),
                        chain_seq: 15,
                    },
                    reaping_at_ms: 1234,
                },
            },
        )
        .unwrap();
        assert_eq!(value.evidence_links().len(), 3);
        value.validate_shape().unwrap();
    }
}
