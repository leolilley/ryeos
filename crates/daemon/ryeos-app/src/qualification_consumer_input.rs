//! The exact accepted input a remote consumer attempt is journaled against.
//!
//! A signed policy with a consumer context drives a Worker lane whose input is
//! the Worker consumer record. A policy without one drives a probe lane whose
//! input is the daemon-authored probe record. Both lanes share one attempt
//! journal, so the journal accepts either typed input and never a loose pair
//! of nullable fields.

use anyhow::{Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeVerificationCoordinate, ConsumerRuntimeVerifierSelection,
    RemoteVerificationPurpose, RestoredVerifierAttemptIntent,
};
use ryeos_state::external_content::qualification_consumer_record::{
    ConsumerInputRecord, MAX_CONSUMER_INPUT_RECORD_BYTES, REMOTE_PROBE_INPUT_RECORD_SCHEMA,
    RemoteProbeInputRecord,
};
use ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose;

/// Borrowed typed input for journal reservation and contact.
#[derive(Clone, Copy)]
pub enum ConsumerInputRef<'a> {
    Worker(&'a ConsumerInputRecord),
    Probe(&'a RemoteProbeInputRecord),
}

impl<'a> ConsumerInputRef<'a> {
    pub fn purpose(&self) -> &'a QualificationLaunchPurpose {
        match self {
            Self::Worker(record) => &record.purpose,
            Self::Probe(record) => &record.purpose,
        }
    }

    pub fn coordinate(&self) -> &'a ConsumerRuntimeVerificationCoordinate {
        match self {
            Self::Worker(record) => &record.coordinate,
            Self::Probe(record) => &record.coordinate,
        }
    }

    pub fn selection(&self) -> &'a ConsumerRuntimeVerifierSelection {
        match self {
            Self::Worker(record) => &record.selection,
            Self::Probe(record) => &record.selection,
        }
    }

    /// Canonical retained bytes, validated first.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        match self {
            Self::Worker(record) => record.canonical_bytes(),
            Self::Probe(record) => record.canonical_bytes(),
        }
    }

    /// Agreement between this input and one exact run attempt.
    pub fn validate_attempt(&self, intent: &RestoredVerifierAttemptIntent) -> Result<()> {
        match self {
            Self::Worker(record) => record.validate_attempt(intent),
            Self::Probe(record) => {
                record.validate()?;
                record.purpose.require_consumer_invocation(
                    &intent.qualification_operation_id,
                    &record.coordinate,
                )?;
                let RemoteVerificationPurpose::ConsumerRuntime { coordinate, .. } = &intent.purpose
                else {
                    anyhow::bail!("owner measurement cannot consume a probe input archive");
                };
                ensure!(
                    coordinate == &record.coordinate
                        && intent.verifier_artifact_hash == record.selection.verifier_artifact_hash,
                    "probe input archive differs from exact run attempt"
                );
                ensure!(
                    intent.upload_bytes > 0
                        && intent.upload_bytes
                            <= record.selection.archive_budget.maximum_framed_bytes
                        && lillux::valid_hash(&intent.upload_sha256)
                        && intent.attempt_deadline_ms > 0,
                    "probe attempt exceeds protected archive budget or has invalid upload identity"
                );
                intent.consumer_challenge_digest()?;
                Ok(())
            }
        }
    }
}

impl<'a> From<&'a ConsumerInputRecord> for ConsumerInputRef<'a> {
    fn from(record: &'a ConsumerInputRecord) -> Self {
        Self::Worker(record)
    }
}

impl<'a> From<&'a RemoteProbeInputRecord> for ConsumerInputRef<'a> {
    fn from(record: &'a RemoteProbeInputRecord) -> Self {
        Self::Probe(record)
    }
}

/// Owned retained input read back from journal-owned CAS bytes.
pub enum RetainedConsumerInput {
    Worker(Box<ConsumerInputRecord>),
    Probe(Box<RemoteProbeInputRecord>),
}

impl RetainedConsumerInput {
    /// Decode canonical retained bytes. The record's own schema field selects
    /// the lane; each record type then validates its own lane's invariants,
    /// so bytes cannot be reinterpreted under the other lane.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
            "retained consumer input exceeds its bound"
        );
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        if value.get("schema").and_then(serde_json::Value::as_str)
            == Some(REMOTE_PROBE_INPUT_RECORD_SCHEMA)
        {
            ensure!(
                lillux::canonical_json(&value)?.as_bytes() == bytes,
                "remote probe input record is not canonical"
            );
            return Ok(Self::Probe(Box::new(RemoteProbeInputRecord::from_value(
                &value,
            )?)));
        }
        Ok(Self::Worker(Box::new(ConsumerInputRecord::parse(bytes)?)))
    }

    pub fn input_ref(&self) -> ConsumerInputRef<'_> {
        match self {
            Self::Worker(record) => ConsumerInputRef::Worker(record),
            Self::Probe(record) => ConsumerInputRef::Probe(record),
        }
    }
}

/// Parse a probe record submitted by the verifier. Equality with the
/// daemon-derived record is the caller's separate duty.
pub fn parse_probe_record(bytes: &[u8]) -> Result<RemoteProbeInputRecord> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
        "remote probe input record exceeds its bound"
    );
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    ensure!(
        lillux::canonical_json(&value)?.as_bytes() == bytes,
        "remote probe input record is not canonical"
    );
    RemoteProbeInputRecord::from_value(&value)
}
