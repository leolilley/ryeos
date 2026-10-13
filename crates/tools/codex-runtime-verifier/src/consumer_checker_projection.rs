//! Inert native identity projection for an interpreted subordinate checker.
//!
//! No caller is wired. Original chunks must come from the future protected
//! reader's retained bytes, never reserialization of its parsed stream. This
//! module creates product data only: no launch, admission or settlement proof.
use anyhow::{Context as _, Result, ensure};
use ryeos_codex_external_configuration::qualification_request::controller_canary_commands;
use ryeos_state::objects::{
    QualificationWorkerTranscriptReference, QualificationWorkerTranscriptStream,
    canonical_value_digest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::consumer_record::ConsumerInputRecord;
use crate::consumer_worker_semantics::CheckedQConversation;
use crate::consumer_worker_transcript::VerifiedConsumerWorkerTranscript;

const MAX_HEADER_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_NOTIFICATIONS: usize = 512;
const MAX_NOTIFICATION_BYTES: usize = 64 * 1024;
const MAX_NOTIFICATION_TOTAL_BYTES: usize = 1024 * 1024;

/// Raw original bytes, not a claim that the caller authenticated them. There
/// is deliberately no live producer yet; H must retain protected reader bytes.
pub(crate) struct OriginalTranscriptChunks {
    bytes: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckerIdentityEnvelope {
    original_record_sha256: String,
    transcript_manifest_hash: String,
    transcript_stream_digest: String,
    checker_identity_digest: String,
    invocation_id: String,
}

#[derive(Debug, Serialize)]
struct ArgumentProjection {
    source_sha256: String,
    value: Value,
}

#[derive(Debug, Serialize)]
struct NotificationProjection {
    batch_index: usize,
    event_index: usize,
    raw_sha256: String,
    value: Value,
    arguments: Option<ArgumentProjection>,
}

#[derive(Debug, Serialize)]
struct ObservationProjection {
    frame: String,
    index: usize,
    value: Value,
    digest: String,
}

#[derive(Debug, Serialize)]
struct ArgvPresentation {
    argv: [String; 3],
    rendered: String,
}

/// Exactly the Python v2 fields except stream, reconstructed only from the
/// separate original chunk bytes by a future bounded FD bootstrap.
#[derive(Serialize)]
struct CheckerInputMetadata<'a> {
    schema: &'static str,
    identities: &'a CheckerIdentityEnvelope,
    record: &'a ConsumerInputRecord,
    reference: &'a QualificationWorkerTranscriptReference,
    product_contract: &'a crate::finite_product_contract::FiniteProductContract,
    notifications: &'a [NotificationProjection],
    observations: &'a [ObservationProjection],
    argv_presentations: &'a [ArgvPresentation],
}

/// Retains originals, exact original chunks, derived identities and finite
/// transport header. Private data, never VerifiedQConversation or admission.
pub(crate) struct PreparedCheckerInput {
    original: ConsumerInputRecord,
    transcript: VerifiedConsumerWorkerTranscript,
    chunks: OriginalTranscriptChunks,
    identities: CheckerIdentityEnvelope,
    header: Vec<u8>,
}

impl OriginalTranscriptChunks {
    /// Retain original protected bytes WITHOUT asserting authentication. Only
    /// the future reader may supply live custody; prepare verifies exact bytes.
    pub(crate) fn from_original_bytes(bytes: Vec<Vec<u8>>) -> Self {
        Self { bytes }
    }
}

impl PreparedCheckerInput {
    pub(crate) fn metadata_header(&self) -> &[u8] { &self.header }
    pub(crate) fn original_chunks(&self) -> &[Vec<u8>] { &self.chunks.bytes }
    pub(crate) fn original_record(&self) -> &ConsumerInputRecord { &self.original }
    pub(crate) fn transcript(&self) -> &VerifiedConsumerWorkerTranscript { &self.transcript }
}

fn validate_hash(value: &str) -> Result<()> {
    ryeos_state::objects::thread_snapshot::validate_canonical_hash("checker identity", value)
}

fn derive_identities(
    original: &ConsumerInputRecord,
    reference: &QualificationWorkerTranscriptReference,
    checker_identity_digest: String,
) -> Result<CheckerIdentityEnvelope> {
    validate_hash(&checker_identity_digest)?;
    let original_record_sha256 = lillux::sha256_hex(&original.canonical_bytes()?);
    // verify_chunks proves these are exactly canonical original stream bytes.
    let transcript_stream_digest = reference.manifest.stream_sha256.clone();
    let transcript_manifest_hash = reference.manifest_hash.clone();
    let invocation = original.consumer_invocation()?;
    let invocation_id = canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.codex.semantic-invocation.v1",
        "coordinate": original.coordinate,
        "qualification_operation_id": invocation.qualification_operation_id,
        "original_record_sha256": original_record_sha256,
        "transcript_manifest_hash": transcript_manifest_hash,
        "transcript_stream_digest": transcript_stream_digest,
        "checker_identity_digest": checker_identity_digest,
    }))?;
    Ok(CheckerIdentityEnvelope {
        original_record_sha256,
        transcript_manifest_hash,
        transcript_stream_digest,
        checker_identity_digest,
        invocation_id,
    })
}

/// Count canonical JSON bytes without serializing arbitrary values. Ordering
/// affects identity, not size. Numbers delegate only their small scalar token to
/// the sole Lillux encoder; strings are counted scalar-by-scalar with no clone.
fn preflight_value_size(value: &Value, maximum: usize) -> Result<usize> {
    fn charge(used: &mut usize, maximum: usize, count: usize) -> Result<()> {
        *used = used.checked_add(count).context("canonical size overflow")?;
        ensure!(*used <= maximum, "projection value exceeds remaining header budget");
        Ok(())
    }
    fn string(value: &str, used: &mut usize, maximum: usize) -> Result<()> {
        charge(used, maximum, 2)?;
        for ch in value.chars() {
            let count = match ch {
                '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
                c if c < '\u{20}' => 6,
                c if c.is_ascii() => 1,
                c if (c as u32) <= 0xffff => 6,
                _ => 12,
            };
            charge(used, maximum, count)?;
        }
        Ok(())
    }
    fn walk(value: &Value, used: &mut usize, maximum: usize) -> Result<()> {
        match value {
            Value::Null => charge(used, maximum, 4)?,
            Value::Bool(value) => charge(used, maximum, if *value { 4 } else { 5 })?,
            Value::Number(_) => charge(used, maximum, lillux::canonical_json(value)?.len())?,
            Value::String(value) => string(value, used, maximum)?,
            Value::Array(values) => {
                charge(used, maximum, 2)?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 { charge(used, maximum, 1)?; }
                    walk(value, used, maximum)?;
                }
            }
            Value::Object(values) => {
                charge(used, maximum, 2)?;
                for (index, (key, value)) in values.iter().enumerate() {
                    if index != 0 { charge(used, maximum, 1)?; }
                    string(key, used, maximum)?;
                    charge(used, maximum, 1)?;
                    walk(value, used, maximum)?;
                }
            }
        }
        Ok(())
    }
    // Values originate in the existing bounded native JSON parser. This pure
    // preflight creates no new accepted recursion/domain policy.
    let mut used = 0;
    walk(value, &mut used, maximum)?;
    Ok(used)
}

/// A metadata budget charged with exact canonical JSON bytes INCLUDING field,
/// array and object overhead. Oversized entries refuse before retained cloning.
struct MetadataBudget {
    used: usize,
}

impl MetadataBudget {
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.used = self.used.checked_add(bytes).context("checker header size overflow")?;
        ensure!(self.used <= MAX_HEADER_BYTES, "checker header exceeds finite bound");
        Ok(())
    }

    fn value<T: Serialize>(&mut self, value: &T) -> Result<()> {
        self.charge(lillux::canonical_json(&serde_json::to_value(value)?)?.len())
    }
}

fn project_notifications(
    batches: &[Value],
    budget: &mut MetadataBudget,
) -> Result<Vec<NotificationProjection>> {
    let mut projections = Vec::new();
    let mut total = 0usize;
    for (batch_index, batch) in batches.iter().enumerate() {
        let events = batch.get("events").and_then(Value::as_array)
            .context("checker batch events absent")?;
        for (event_index, event) in events.iter().enumerate() {
            let Some(payload) = event.get("payload").filter(|v| v.is_object()) else { continue };
            let Some(raw) = payload.get("notification_json") else { continue };
            let raw = raw.as_str().context("raw notification is not text")?;
            total = total.checked_add(raw.len()).context("notification bytes overflow")?;
            ensure!(raw.len() <= MAX_NOTIFICATION_BYTES && total <= MAX_NOTIFICATION_TOTAL_BYTES
                && projections.len() < MAX_NOTIFICATIONS, "raw notification bounds exceeded");
            // Do not classify method, type, name, role or effects here.
            let value: Value = serde_json::from_str(raw).context("decode raw notification")?;
            ensure!(lillux::canonical_json(&value)? == raw, "raw notification is not canonical");
            let arguments = match value.pointer("/params/item/arguments").and_then(Value::as_str) {
                Some(source) => {
                    ensure!(source.len() <= MAX_NOTIFICATION_BYTES, "embedded argument source exceeds bound");
                    Some(ArgumentProjection {
                        source_sha256: lillux::sha256_hex(source.as_bytes()),
                        value: serde_json::from_str(source).context("decode generic embedded arguments")?,
                    })
                },
                None => None,
            };
            let projection = NotificationProjection {
                batch_index, event_index, raw_sha256: lillux::sha256_hex(raw.as_bytes()), value, arguments,
            };
            budget.value(&projection)?;
            if !projections.is_empty() { budget.charge(1)?; } // exact array comma
            projections.push(projection);
        }
    }
    Ok(projections)
}

/// Reserve the COMPLETE entry (including an optional array comma) before
/// touching the arbitrary source value. The probe contains metadata only;
/// its null placeholder is replaced by the exact source-size count.
fn preflight_observation_entry(
    value: &Value, frame: &str, index: usize, comma: bool, remaining: usize,
) -> Result<usize> {
    let probe = serde_json::json!({"frame":frame,"index":index,"digest":"0".repeat(64),"value":null});
    let overhead = lillux::canonical_json(&probe)?.len()
        .checked_sub(4).context("observation probe lacks null value")?
        .checked_add(usize::from(comma)).context("observation entry overhead overflow")?;
    let available = remaining.checked_sub(overhead)
        .context("observation metadata exceeds remaining header budget")?;
    let value_bytes = preflight_value_size(value, available)?;
    overhead.checked_add(value_bytes).context("observation entry size overflow")
}

fn project_observations(
    stream: &QualificationWorkerTranscriptStream,
    budget: &mut MetadataBudget,
) -> Result<Vec<ObservationProjection>> {
    let mut result = Vec::new();
    let mut append = |frame: String, body: &Value| -> Result<()> {
        let observations = body.get("session_observations").and_then(Value::as_array)
            .context("checker frame observations absent")?;
        for (index, value) in observations.iter().enumerate() {
            // Full metadata/punctuation/comma overhead is included BEFORE
            // arbitrary observation digest, clone or serialization.
            let comma = !result.is_empty();
            let count = preflight_observation_entry(value, &frame, index, comma,
                MAX_HEADER_BYTES.saturating_sub(budget.used))?;
            budget.charge(count)?;
            let digest = canonical_value_digest(value)?;
            let entry = serde_json::json!({"frame":frame,"index":index,"value":value,"digest":digest});
            // Independent actual encoding check: the preflight cannot silently
            // become a conservative rejection or underestimate writer output.
            let actual = lillux::canonical_json(&entry)?.len();
            ensure!(actual.checked_add(usize::from(comma)) == Some(count),
                "observation size preflight differs from native encoding");
            result.push(ObservationProjection { frame: frame.clone(), index, value: value.clone(), digest });
        }
        Ok(())
    };
    append("setup".to_owned(), &stream.setup_final_response)?;
    append("final".to_owned(), &stream.final_response)?;
    if let Some(delta) = &stream.delta_response { append("delta".to_owned(), delta)?; }
    for (index, batch) in stream.batches.iter().enumerate() {
        append(format!("batch/{index}"), batch)?;
    }
    Ok(result)
}

fn project_argv(original: &ConsumerInputRecord, reference: &QualificationWorkerTranscriptReference)
    -> Result<Vec<ArgvPresentation>>
{
    let configuration = original.scripted_configuration()?;
    let expected = &configuration.qualification_worker;
    let path = &reference.manifest.controller_canary.as_ref()
        .context("checker canary binding absent")?.binding.controller_path;
    let read = controller_canary_commands(path)?.read_command;
    [expected.command.clone(), read].into_iter().map(|command| {
        let argv = [expected.command_shell.clone(), "-c".to_owned(), command];
        let rendered = shlex::try_join(argv.iter().map(String::as_str))
            .context("checker selected argv cannot be rendered")?;
        Ok(ArgvPresentation { argv, rendered })
    }).collect()
}

pub(crate) fn prepare_checker_projection(
    original: ConsumerInputRecord,
    reference: QualificationWorkerTranscriptReference,
    chunks: OriginalTranscriptChunks,
    checker_identity_digest: String,
) -> Result<PreparedCheckerInput> {
    crate::consumer_worker_transcript::validate_reference_for_record(&reference, &original)?;
    let product_contract = original.product_contract()?;
    // Exactly ONE baseline verification. H's protected reader must hand off
    // retained reference + original chunks BEFORE parsing the stream itself.
    // Existing native verification uses bounded full-stream scratch; no claim
    // of zero-copy, header-sized process memory, or a second parse is made.
    let stream = reference.manifest.verify_chunks(&chunks.bytes)?;
    // Native structural capture identity only, not expected final-byte meaning.
    if let (Some(fixture), Some(content)) = (
        original.purpose.policy_source.policy.producer_scenarios
            .get(&original.coordinate.scenario_id)
            .and_then(|s| s.consumer_worker_request.as_ref()).and_then(|r| r.fixture.as_ref()),
        stream.fixture_content.as_ref(),
    ) {
        ensure!(content.metadata.fixture_contract_digest
            == ryeos_state::objects::qualification_fixture::q_fixture_contract_digest(fixture)?
            && content.metadata.size_bytes <= fixture.max_bytes,
            "capture differs from retained signed fixture contract");
    }
    let identities = derive_identities(&original, &reference, checker_identity_digest)?;
    let argv_presentations = project_argv(&original, &reference)?;
    let empty: &[NotificationProjection] = &[];
    let empty_observations: &[ObservationProjection] = &[];
    let base = CheckerInputMetadata {
        schema: "ryeos.codex.semantic_input.v2", identities: &identities,
        record: &original, reference: &reference, product_contract: &product_contract,
        notifications: empty, observations: empty_observations, argv_presentations: &argv_presentations,
    };
    let mut budget = MetadataBudget { used: 0 };
    budget.value(&base)?;
    let notifications = project_notifications(&stream.batches, &mut budget)?;
    let observations = project_observations(&stream, &mut budget)?;
    let header = CheckerInputMetadata { notifications: &notifications, observations: &observations, ..base };
    let header = lillux::canonical_json(&serde_json::to_value(header)?)?.into_bytes();
    ensure!(header.len() <= MAX_HEADER_BYTES, "complete checker metadata header exceeds bound");
    Ok(PreparedCheckerInput {
        original, transcript: VerifiedConsumerWorkerTranscript { reference, stream }, chunks, identities, header,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversationResponse {
    thread_id: String,
    turn_id: String,
    fixture_write_call_id: String,
    canary_read_call_id: String,
    local_refusal_call_id: String,
    notification_count: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckerResponse {
    schema: String,
    conversation: ConversationResponse,
    original_record_sha256: String,
    transcript_manifest_hash: String,
    transcript_stream_digest: String,
    checker_identity_digest: String,
    invocation_id: String,
}

pub(crate) fn decode_checker_response(bytes: &[u8], prepared: &PreparedCheckerInput) -> Result<CheckedQConversation> {
    decode_response_for_identities(bytes, &prepared.identities)
}

fn decode_response_for_identities(bytes: &[u8], expected: &CheckerIdentityEnvelope)
    -> Result<CheckedQConversation>
{
    ensure!(bytes.len() <= MAX_RESPONSE_BYTES, "checker response exceeds bound");
    // Total frame cap includes optional LF. Only ONE exact LF may follow JSON.
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let value: Value = ryeos_external_execution_contract::from_json_slice_strict(bytes, MAX_RESPONSE_BYTES)?;
    ensure!(lillux::canonical_json(&value)?.as_bytes() == bytes, "checker response is not canonical");
    let response: CheckerResponse = serde_json::from_value(value).context("decode closed checker response")?;
    ensure!(response.schema == "ryeos.codex.checked_conversation.v1", "checker response schema mismatch");
    let observed = CheckerIdentityEnvelope {
        original_record_sha256: response.original_record_sha256,
        transcript_manifest_hash: response.transcript_manifest_hash,
        transcript_stream_digest: response.transcript_stream_digest,
        checker_identity_digest: response.checker_identity_digest,
        invocation_id: response.invocation_id,
    };
    ensure!(&observed == expected, "checker response changed locally retained identities");
    let conversation = response.conversation;
    for id in [&conversation.thread_id, &conversation.turn_id, &conversation.fixture_write_call_id,
        &conversation.canary_read_call_id, &conversation.local_refusal_call_id]
    {
        ensure!(!id.is_empty() && id.len() <= 256 && id.trim() == id
            && !id.chars().any(char::is_control), "checker response identifier invalid");
    }
    ensure!((1..=MAX_NOTIFICATIONS).contains(&conversation.notification_count), "checker count invalid");
    ensure!(conversation.fixture_write_call_id != conversation.canary_read_call_id
        && conversation.fixture_write_call_id != conversation.local_refusal_call_id
        && conversation.canary_read_call_id != conversation.local_refusal_call_id, "checker role IDs alias");
    Ok(CheckedQConversation {
        thread_id: conversation.thread_id, turn_id: conversation.turn_id,
        fixture_write_call_id: conversation.fixture_write_call_id,
        canary_read_call_id: conversation.canary_read_call_id,
        local_refusal_call_id: conversation.local_refusal_call_id,
        notification_count: conversation.notification_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_notifications_and_arguments_survive_without_role_selection() {
        let raw = lillux::canonical_json(&json!({"method":"unknown","params":{"item":{"arguments":"{\"opaque\":1.5}"}}})).unwrap();
        let batches = [json!({"events":[{"payload":{"notification_json":raw}}]})];
        let projections = project_notifications(&batches, &mut MetadataBudget { used: 0 }).unwrap();
        assert_eq!(projections[0].value["method"], "unknown");
        assert_eq!(projections[0].arguments.as_ref().unwrap().value, json!({"opaque":1.5}));
        let bad = [json!({"events":[{"payload":{"notification_json":"{ \"method\":\"unknown\" }"}}]})];
        assert!(project_notifications(&bad, &mut MetadataBudget { used: 0 }).is_err());
    }

    #[test]
    fn all_observations_include_unknown_values_and_optional_delta() {
        let frame = |v| json!({"session_observations":[v]});
        let stream = QualificationWorkerTranscriptStream {
            schema: "synthetic".to_owned(), fixture_content: None,
            setup_final_response: frame(json!({"unknown":1.5})),
            final_response: frame(json!({"kind":"not_selected"})),
            delta_response: Some(frame(json!({"kind":"also_unknown"}))),
            batches: vec![frame(json!({"other":true}))],
        };
        let entries = project_observations(&stream, &mut MetadataBudget { used: 0 }).unwrap();
        assert_eq!(entries.iter().map(|e| e.frame.as_str()).collect::<Vec<_>>(), vec!["setup","final","delta","batch/0"]);
        assert_eq!(entries[0].value, json!({"unknown":1.5}));
        assert_eq!(entries[0].digest, canonical_value_digest(&entries[0].value).unwrap());
        assert!(project_observations(&stream, &mut MetadataBudget { used: MAX_HEADER_BYTES }).is_err());
    }

    #[test]
    fn response_identity_and_closed_shape_are_local_not_child_authority() {
        let identities = CheckerIdentityEnvelope {
            original_record_sha256: "1".repeat(64), transcript_manifest_hash: "2".repeat(64),
            transcript_stream_digest: "3".repeat(64), checker_identity_digest: "4".repeat(64), invocation_id: "5".repeat(64),
        };
        let mut response = serde_json::to_value(&identities).unwrap();
        response["schema"] = json!("ryeos.codex.checked_conversation.v1");
        response["conversation"] = json!({"thread_id":"thread","turn_id":"turn","fixture_write_call_id":"a",
            "canary_read_call_id":"b","local_refusal_call_id":"c","notification_count":3});
        let bytes = lillux::canonical_json(&response).unwrap();
        assert!(decode_response_for_identities(bytes.as_bytes(), &identities).is_ok());
        let single_lf = format!("{bytes}\n");
        assert!(decode_response_for_identities(single_lf.as_bytes(), &identities).is_ok());
        for suffix in ["\n\n", "\r\n", " ", "\n{}"] {
            assert!(decode_response_for_identities(format!("{bytes}{suffix}").as_bytes(), &identities).is_err());
        }
        let mut over_frame = vec![b' '; MAX_RESPONSE_BYTES];
        over_frame.push(b'\n');
        assert!(decode_response_for_identities(&over_frame, &identities).is_err());
        for key in ["original_record_sha256","transcript_manifest_hash","transcript_stream_digest","checker_identity_digest","invocation_id"] {
            let mut changed = response.clone(); changed[key] = json!("0".repeat(64));
            assert!(decode_response_for_identities(lillux::canonical_json(&changed).unwrap().as_bytes(), &identities).is_err());
        }
        response["extra"] = json!(true);
        assert!(decode_response_for_identities(lillux::canonical_json(&response).unwrap().as_bytes(), &identities).is_err());
    }

    #[test]
    fn size_preflight_matches_native_scalar_encoding_and_exact_boundary() {
        let value = json!({"unicode":"é🙂\n\u{1}", "numbers":[1,1.5,-0.0], "nested":{"b":true,"n":null}});
        let encoded = lillux::canonical_json(&value).unwrap();
        assert_eq!(preflight_value_size(&value, encoded.len()).unwrap(), encoded.len());
        assert!(preflight_value_size(&value, encoded.len() - 1).is_err());
        let huge = Value::String("x".repeat(MAX_HEADER_BYTES));
        assert!(preflight_value_size(&huge, MAX_HEADER_BYTES).is_err());
        // A genuine arbitrary observation, not a selected lifecycle value,
        // must refuse before its digest/value clone/serialization branch.
        let stream = QualificationWorkerTranscriptStream {
            schema: "synthetic".to_owned(), fixture_content: None,
            setup_final_response: json!({"session_observations":[huge]}),
            final_response: json!({"session_observations":[]}),
            delta_response: None, batches: Vec::new(),
        };
        assert!(project_observations(&stream, &mut MetadataBudget { used: 0 }).is_err());
    }

    #[test]
    fn original_chunk_owner_retains_bytes_without_reconstruction_or_authority() {
        let bytes = vec![vec![1,2,3], vec![4,5]];
        let first_allocation = bytes[0].as_ptr();
        let owner = OriginalTranscriptChunks::from_original_bytes(bytes);
        assert_eq!(owner.bytes[0].as_ptr(), first_allocation);
        assert_eq!(owner.bytes, vec![vec![1,2,3], vec![4,5]]);
        // This constructor does not authenticate these deliberately invalid
        // synthetic bytes. Only the single manifest verifier can join them.
    }

    #[test]
    fn full_observation_entry_boundary_includes_metadata_and_array_comma() {
        let value = json!({"opaque":"é🙂\n"});
        let digest = canonical_value_digest(&value).unwrap();
        let actual = lillux::canonical_json(&json!({"frame":"setup","index":0,"value":value,"digest":digest})).unwrap();
        let value_bytes = lillux::canonical_json(&value).unwrap().len();
        // Value alone fits; metadata makes this a predictable BEFORE-digest
        // refusal. Probe never contains or clones the arbitrary source value.
        assert!(preflight_value_size(&value, value_bytes).is_ok());
        assert!(preflight_observation_entry(&value, "setup", 0, false, value_bytes).is_err());
        assert_eq!(preflight_observation_entry(&value, "setup", 0, false, actual.len()).unwrap(), actual.len());
        assert!(preflight_observation_entry(&value, "setup", 0, false, actual.len() - 1).is_err());
        assert_eq!(preflight_observation_entry(&value, "setup", 0, true, actual.len() + 1).unwrap(), actual.len() + 1);
        assert!(preflight_observation_entry(&value, "setup", 0, true, actual.len()).is_err());

        let stream = QualificationWorkerTranscriptStream {
            schema: "synthetic".to_owned(), fixture_content: None,
            setup_final_response: json!({"session_observations":[value]}),
            final_response: json!({"session_observations":[]}),
            delta_response: None, batches: Vec::new(),
        };
        let mut exact = MetadataBudget { used: MAX_HEADER_BYTES - actual.len() };
        assert_eq!(project_observations(&stream, &mut exact).unwrap().len(), 1);
        assert_eq!(exact.used, MAX_HEADER_BYTES);
        let mut one_short = MetadataBudget { used: MAX_HEADER_BYTES - actual.len() + 1 };
        let before = one_short.used;
        assert!(project_observations(&stream, &mut one_short).is_err());
        assert_eq!(one_short.used, before); // refused before digest/encode charge
    }
}
