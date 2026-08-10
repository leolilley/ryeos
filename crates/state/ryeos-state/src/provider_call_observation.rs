//! Canonical daemon-authored provider-call observation contract.
//!
//! Provider transports and runtimes cannot author this event. The daemon emits
//! it only after replay-index publication is proven, or after an exact replay
//! record has been verified. The field consumes this same strict decoder.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const PROVIDER_CALL_OBSERVATION_SCHEMA: &str = "ryeos.provider_call_observation.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCallObservationSource {
    Executed,
    Replay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCallObservationPublication {
    Inserted,
    Folded,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallReplaySource {
    pub produced_by_thread: String,
    pub attempt_id: String,
}

impl ProviderCallReplaySource {
    fn validate(&self) -> Result<()> {
        validate_identifier(
            &self.produced_by_thread,
            256,
            "provider replay source thread",
        )?;
        validate_identifier(&self.attempt_id, 256, "provider replay source attempt")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallObservationRecordedPayload {
    pub schema_version: String,
    pub observation_id: String,
    pub turn: u32,
    pub attempt_number: u32,
    pub effect_coordinate_digest: String,
    pub source: ProviderCallObservationSource,
    pub answer_digest: String,
    pub record_hash: String,
    pub publication: ProviderCallObservationPublication,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed_from: Option<ProviderCallReplaySource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCallObservationDraft {
    pub turn: u32,
    pub attempt_number: u32,
    pub effect_coordinate_digest: String,
    pub source: ProviderCallObservationSource,
    pub answer_digest: String,
    pub record_hash: String,
    pub publication: ProviderCallObservationPublication,
    pub replayed_from: Option<ProviderCallReplaySource>,
}

impl ProviderCallObservationDraft {
    pub fn validate(&self) -> Result<()> {
        ProviderCallObservationRecordedPayload {
            schema_version: PROVIDER_CALL_OBSERVATION_SCHEMA.to_string(),
            observation_id: "provider-call-observation:draft".to_string(),
            turn: self.turn,
            attempt_number: self.attempt_number,
            effect_coordinate_digest: self.effect_coordinate_digest.clone(),
            source: self.source,
            answer_digest: self.answer_digest.clone(),
            record_hash: self.record_hash.clone(),
            publication: self.publication,
            replayed_from: self.replayed_from.clone(),
        }
        .validate()
    }
}

impl ProviderCallObservationRecordedPayload {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != PROVIDER_CALL_OBSERVATION_SCHEMA {
            bail!(
                "provider-call observation schema is {}, expected {PROVIDER_CALL_OBSERVATION_SCHEMA}",
                self.schema_version
            );
        }
        validate_identifier(&self.observation_id, 128, "provider-call observation ID")?;
        if self.turn == 0 || self.attempt_number == 0 {
            bail!("provider-call observation turn and attempt_number are one-based");
        }
        for (label, digest) in [
            ("provider effect coordinate", &self.effect_coordinate_digest),
            ("provider answer", &self.answer_digest),
            ("provider record", &self.record_hash),
        ] {
            if !lillux::valid_hash(digest) {
                bail!("{label} digest is invalid");
            }
        }
        match (self.source, self.publication, self.replayed_from.as_ref()) {
            (
                ProviderCallObservationSource::Executed,
                ProviderCallObservationPublication::Inserted
                | ProviderCallObservationPublication::Folded,
                None,
            ) => {}
            (
                ProviderCallObservationSource::Replay,
                ProviderCallObservationPublication::NotApplicable,
                Some(source),
            ) => source.validate()?,
            _ => bail!("provider-call observation source/publication provenance is incoherent"),
        }
        Ok(())
    }

    pub fn validate_for_subject(&self, chain_root_id: &str, thread_id: &str) -> Result<()> {
        self.validate()?;
        let expected = provider_call_observation_id(
            chain_root_id,
            thread_id,
            self.turn,
            self.attempt_number,
            &self.effect_coordinate_digest,
            self.source,
            &self.answer_digest,
            &self.record_hash,
            self.publication,
            self.replayed_from.as_ref(),
        )?;
        if self.observation_id != expected {
            bail!("provider-call observation ID contradicts its chain/thread coordinate");
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn provider_call_observation_id(
    chain_root_id: &str,
    thread_id: &str,
    turn: u32,
    attempt_number: u32,
    effect_coordinate_digest: &str,
    source: ProviderCallObservationSource,
    answer_digest: &str,
    record_hash: &str,
    publication: ProviderCallObservationPublication,
    replayed_from: Option<&ProviderCallReplaySource>,
) -> Result<String> {
    validate_identifier(chain_root_id, 256, "provider observation chain root")?;
    validate_identifier(thread_id, 256, "provider observation thread")?;
    if turn == 0 || attempt_number == 0 {
        bail!("provider observation turn and attempt_number are one-based");
    }
    for digest in [effect_coordinate_digest, answer_digest, record_hash] {
        if !lillux::valid_hash(digest) {
            bail!("provider observation identity contains an invalid digest");
        }
    }
    if let Some(source) = replayed_from {
        source.validate()?;
    }
    let seed = json!({
        "chain_root_id": chain_root_id,
        "thread_id": thread_id,
        "turn": turn,
        "attempt_number": attempt_number,
        "effect_coordinate_digest": effect_coordinate_digest,
        "source": source,
        "answer_digest": answer_digest,
        "record_hash": record_hash,
        "publication": publication,
        "replayed_from": replayed_from,
    });
    let canonical = lillux::canonical_json(&seed)
        .context("canonicalize provider-call observation identity seed")?;
    Ok(format!(
        "provider-call-observation:{}",
        lillux::sha256_hex(canonical.as_bytes())
    ))
}

fn validate_identifier(value: &str, max_bytes: usize, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        bail!("{label} is not a bounded canonical identifier");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executed_and_replay_provenance_are_disjoint() {
        let replay = ProviderCallReplaySource {
            produced_by_thread: "T-origin".to_string(),
            attempt_id: "attempt-origin".to_string(),
        };
        let executed = provider_call_observation_id(
            "T-root",
            "T-current",
            1,
            1,
            &"a".repeat(64),
            ProviderCallObservationSource::Executed,
            &"b".repeat(64),
            &"c".repeat(64),
            ProviderCallObservationPublication::Inserted,
            None,
        )
        .unwrap();
        let replayed = provider_call_observation_id(
            "T-root",
            "T-current",
            1,
            1,
            &"a".repeat(64),
            ProviderCallObservationSource::Replay,
            &"b".repeat(64),
            &"c".repeat(64),
            ProviderCallObservationPublication::NotApplicable,
            Some(&replay),
        )
        .unwrap();
        assert_ne!(executed, replayed);
    }
}
