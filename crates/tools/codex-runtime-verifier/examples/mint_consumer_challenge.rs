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
//! Usage: mint_consumer_challenge <intent.json> <selection.json> <seconds>

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeChallenge, ConsumerRuntimeVerifierSelection, RestoredVerifierAttemptIntent,
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
    ensure!(
        arguments.next().is_none() && (1..=86_400).contains(&seconds),
        "usage: mint_consumer_challenge <intent.json> <selection.json> <seconds in 1..=86400>"
    );
    let mut intent: RestoredVerifierAttemptIntent =
        serde_json::from_slice(&std::fs::read(&intent_path).context("read intent")?)
            .context("decode intent")?;
    let selection: ConsumerRuntimeVerifierSelection =
        serde_json::from_slice(&std::fs::read(&selection_path).context("read selection")?)
            .context("decode selection")?;
    intent.attempt_deadline_ms = lillux::time::timestamp_millis()
        .checked_add(seconds * 1000)
        .context("deadline overflow")?;
    ensure!(
        intent.operation_id == intent.derived_operation_id()?,
        "retained intent does not carry its derived operation identity"
    );
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
