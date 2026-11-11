//! Re-mint a retained consumer verifier attempt for a local rehearsal.
//!
//! Reads the attempt intent and the protected consumer selection as the node
//! retained them, moves the attempt deadline into the future, and prints the
//! challenge exactly as the Render adapter hands it to the in-sandbox
//! verifier (`--consumer-challenge-b64`), followed by the upload directory the
//! verifier expects. The operation identity does not depend on the deadline,
//! so the directory name matches the live attempt's. This grants nothing: it
//! is the argument of a program run by hand on a developer machine.
//!
//! With a fourth argument, a locally rebuilt verifier stands in for the
//! delivered one: the intent and the selection name that file's hash and the
//! operation identity is derived again, so the upload directory changes. The
//! delivered input record is re-sealed to match: its two verifier references
//! change, the sealed purpose digest is recomputed, and the record and the
//! intent carry that coordinate. The re-sealed record is written to the last
//! argument for the caller to lay out in place of the delivered one.
//!
//! Usage: mint_consumer_challenge <intent.json> <selection.json> <seconds>
//!        [<verifier> <record-in.json> <record-out.json>]

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeChallenge, ConsumerRuntimeVerifierSelection, RemoteVerificationPurpose,
    RestoredVerifierAttemptIntent,
};

fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let intent_path = arguments.next().context("intent path argument absent")?;
    let selection_path = arguments.next().context("selection path argument absent")?;
    let seconds: i64 = arguments
        .next()
        .context("deadline seconds argument absent")?
        .parse()
        .context("deadline seconds is not an integer")?;
    let replacement = match arguments.next() {
        Some(verifier) => Some((
            verifier,
            arguments.next().context("record input path absent")?,
            arguments.next().context("record output path absent")?,
        )),
        None => None,
    };
    ensure!(
        arguments.next().is_none() && (1..=86_400).contains(&seconds),
        "usage: mint_consumer_challenge <intent.json> <selection.json> <seconds in 1..=86400> [<verifier> <record-in> <record-out>]"
    );
    let mut intent: RestoredVerifierAttemptIntent =
        serde_json::from_slice(&std::fs::read(&intent_path).context("read intent")?)
            .context("decode intent")?;
    let mut selection: ConsumerRuntimeVerifierSelection =
        serde_json::from_slice(&std::fs::read(&selection_path).context("read selection")?)
            .context("decode selection")?;
    intent.attempt_deadline_ms = lillux::time::timestamp_millis()
        .checked_add(seconds * 1000)
        .context("deadline overflow")?;
    ensure!(
        intent.operation_id == intent.derived_operation_id()?,
        "retained intent does not carry its derived operation identity"
    );
    if let Some((verifier, record_in, record_out)) = replacement {
        let verifier_bytes = std::fs::read(&verifier).context("read replacement verifier")?;
        let hash = lillux::sha256_hex(&verifier_bytes);
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record_in).context("read input record")?)
                .context("decode input record")?;
        record["selection"]["verifier_artifact_hash"] = hash.clone().into();
        let scenario = record["coordinate"]["scenario_id"]
            .as_str()
            .context("record coordinate has no scenario")?
            .to_owned();
        let source = &mut record["purpose"]["remote_verifier_sources"][&scenario];
        source["executor"]["payload_blob_hash"] = hash.clone().into();
        source["payload_bytes"] = (verifier_bytes.len() as u64).into();
        let purpose: ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose =
            serde_json::from_value(record["purpose"].clone()).context("decode sealed purpose")?;
        let purpose_digest =
            ryeos_state::objects::canonical_value_digest(&serde_json::to_value(&purpose)?)?;
        record["coordinate"]["qualification_purpose_digest"] = purpose_digest.clone().into();
        let RemoteVerificationPurpose::ConsumerRuntime { coordinate, .. } = &mut intent.purpose
        else {
            anyhow::bail!("retained intent is not a consumer runtime attempt");
        };
        coordinate.qualification_purpose_digest = purpose_digest;
        intent.verifier_artifact_hash = hash.clone();
        selection.verifier_artifact_hash = hash;
        intent.operation_id = intent.derived_operation_id()?;
        std::fs::write(&record_out, lillux::canonical_json(&record)?)
            .context("write re-sealed record")?;
    }
    let challenge = ConsumerRuntimeChallenge {
        schema: 1,
        intent,
        selection,
    };
    challenge.validate()?;
    let encoded =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge.canonical_bytes()?);
    println!("{encoded}");
    println!("{}", challenge.intent.remote_upload_directory()?);
    Ok(())
}
