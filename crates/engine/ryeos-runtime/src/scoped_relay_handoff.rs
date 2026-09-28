//! Exact, bounded handshake for one admitted scoped producer's loopback relay.
//!
//! The inherited channel identifies the live verifier root; these canonical
//! messages correlate its one listener transfer and readiness ACK to a signed
//! recipe source and durable attempt. They do not authorize a launch, prove a
//! network namespace, or replace applied-launch and whole-scope evidence.

use anyhow::{Result, bail, ensure};
use ryeos_state::external_content::products::ProducerLoopbackIngress;
use ryeos_state::external_content::products::qualification::ProductProducerRecipeSourceIdentity;
use serde::{Deserialize, Serialize};
use std::io::{Read as _, Write as _};

pub const HANDOFF_SCHEMA: &str = "ryeos.scoped_relay_handoff.v1";
pub const READY_SCHEMA: &str = "ryeos.scoped_relay_ready.v1";
pub const MAX_HANDOFF_MESSAGE_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedRelayHandoff {
    pub schema: String,
    pub root_thread_id: String,
    pub attempt_id: String,
    pub scenario_id: String,
    pub recipe_source: ProductProducerRecipeSourceIdentity,
    pub adapter_request_digest: String,
    pub ingress_address: String,
    pub expected_applied_launch_digest: String,
    pub held_process_identity_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedRelayReady {
    pub schema: String,
    pub attempt_id: String,
    pub handoff_digest: String,
}

impl ScopedRelayHandoff {
    /// The verifier calls this against its pre-launch signed source and
    /// scripted origin before starting a relay or returning readiness. The
    /// attempt and process coordinates remain subject to later daemon
    /// observation and locator joins.
    pub fn validate_for_verifier(
        &self,
        root_thread_id: &str,
        scenario_id: &str,
        expected_source: &ProductProducerRecipeSourceIdentity,
        expected_ingress: &ProducerLoopbackIngress,
    ) -> Result<()> {
        self.validate()?;
        expected_source.validate()?;
        expected_ingress.validate()?;
        ensure!(
            self.root_thread_id == root_thread_id
                && self.scenario_id == scenario_id
                && self.recipe_source == *expected_source
                && self.ingress_address == expected_ingress.address,
            "scoped relay handoff differs from verifier preflight"
        );
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == HANDOFF_SCHEMA,
            "unsupported scoped relay handoff schema"
        );
        crate::validate_runtime_thread_id(&self.root_thread_id).map_err(anyhow::Error::msg)?;
        validate_attempt(&self.attempt_id)?;
        ensure!(
            !self.scenario_id.is_empty()
                && self.scenario_id.len() <= 128
                && self.scenario_id.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                }),
            "scoped relay scenario is not canonical"
        );
        self.recipe_source.validate()?;
        for (name, digest) in [
            ("adapter request", &self.adapter_request_digest),
            (
                "applied launch expectation",
                &self.expected_applied_launch_digest,
            ),
            ("held process identity", &self.held_process_identity_digest),
        ] {
            ensure!(
                lillux::valid_hash(digest) && !digest.bytes().any(|byte| byte.is_ascii_uppercase()),
                "scoped relay {name} digest is invalid"
            );
        }
        ryeos_state::external_content::products::ProducerLoopbackIngress {
            address: self.ingress_address.clone(),
        }
        .validate()?;
        ensure!(
            self.canonical_bytes_unchecked()?.len() <= MAX_HANDOFF_MESSAGE_BYTES,
            "scoped relay handoff exceeds byte bound"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(&self.canonical_bytes_unchecked()?))
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        self.canonical_bytes_unchecked()
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_HANDOFF_MESSAGE_BYTES,
            "scoped relay handoff byte length is invalid"
        );
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        ensure!(
            value.canonical_bytes_unchecked()? == bytes,
            "scoped relay handoff is not canonical"
        );
        Ok(value)
    }

    fn canonical_bytes_unchecked(&self) -> Result<Vec<u8>> {
        Ok(lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes())
    }
}

impl ScopedRelayReady {
    pub fn for_handoff(handoff: &ScopedRelayHandoff) -> Result<Self> {
        Ok(Self {
            schema: READY_SCHEMA.to_owned(),
            attempt_id: handoff.attempt_id.clone(),
            handoff_digest: handoff.digest()?,
        })
    }

    pub fn validate_for(&self, handoff: &ScopedRelayHandoff) -> Result<()> {
        ensure!(
            self.schema == READY_SCHEMA,
            "unsupported scoped relay ready schema"
        );
        validate_attempt(&self.attempt_id)?;
        ensure!(
            self.attempt_id == handoff.attempt_id && self.handoff_digest == handoff.digest()?,
            "scoped relay readiness differs from exact handoff"
        );
        Ok(())
    }

    pub fn canonical_bytes_for(&self, handoff: &ScopedRelayHandoff) -> Result<Vec<u8>> {
        self.validate_for(handoff)?;
        Ok(lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes())
    }

    pub fn from_canonical_bytes_for(bytes: &[u8], handoff: &ScopedRelayHandoff) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_HANDOFF_MESSAGE_BYTES,
            "scoped relay ready byte length is invalid"
        );
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate_for(handoff)?;
        ensure!(
            value.canonical_bytes_for(handoff)? == bytes,
            "scoped relay readiness is not canonical"
        );
        Ok(value)
    }
}

/// One bounded frame on the already admitted inherited channel. A partial
/// send is terminal for this attempt; neither caller may retry the frame to
/// recover a lost acknowledgment or infer a clean handoff from byte count.
pub fn send_handoff(
    channel: &mut lillux::InheritedDuplexChannel,
    handoff: &ScopedRelayHandoff,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    write_frame(channel, &handoff.canonical_bytes()?, deadline)
}

pub fn receive_handoff(
    channel: &mut lillux::InheritedDuplexChannel,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<ScopedRelayHandoff> {
    ScopedRelayHandoff::from_canonical_bytes(&read_frame(channel, deadline)?)
}

pub fn send_ready(
    channel: &mut lillux::InheritedDuplexChannel,
    handoff: &ScopedRelayHandoff,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    let ready = ScopedRelayReady::for_handoff(handoff)?;
    write_frame(channel, &ready.canonical_bytes_for(handoff)?, deadline)
}

pub fn receive_ready(
    channel: &mut lillux::InheritedDuplexChannel,
    handoff: &ScopedRelayHandoff,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<ScopedRelayReady> {
    ScopedRelayReady::from_canonical_bytes_for(&read_frame(channel, deadline)?, handoff)
}

fn write_frame(
    channel: &mut lillux::InheritedDuplexChannel,
    bytes: &[u8],
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_HANDOFF_MESSAGE_BYTES,
        "scoped relay frame length is invalid"
    );
    let length = u32::try_from(bytes.len())?.to_be_bytes();
    let mut stream = channel.with_deadline(deadline);
    stream.write_all(&length)?;
    stream.write_all(bytes)?;
    Ok(())
}

fn read_frame(
    channel: &mut lillux::InheritedDuplexChannel,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<Vec<u8>> {
    let mut stream = channel.with_deadline(deadline);
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        (1..=MAX_HANDOFF_MESSAGE_BYTES).contains(&length),
        "scoped relay frame length is invalid"
    );
    let mut bytes = vec![0u8; length];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn validate_attempt(attempt_id: &str) -> Result<()> {
    if !attempt_id.starts_with("scoped-")
        || attempt_id.len() != 71
        || !attempt_id[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("scoped relay attempt is not canonical");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff() -> ScopedRelayHandoff {
        ScopedRelayHandoff {
            schema: HANDOFF_SCHEMA.into(),
            root_thread_id: "T-8e87d350-7719-5fdf-63ed-b3bb579a45ca".into(),
            attempt_id: format!("scoped-{}", "a".repeat(64)),
            scenario_id: "codex-direct".into(),
            recipe_source: ProductProducerRecipeSourceIdentity {
                bundle_generation_identity: "generation-1".into(),
                canonical_ref: "config:fixtures/direct-codex".into(),
                raw_content_digest: "b".repeat(64),
                effective_definition_digest: "c".repeat(64),
                publisher_fingerprint: "d".repeat(64),
                recipe_digest: "e".repeat(64),
            },
            adapter_request_digest: "f".repeat(64),
            ingress_address: "127.0.0.1:18765".into(),
            expected_applied_launch_digest: "1".repeat(64),
            held_process_identity_digest: "2".repeat(64),
        }
    }

    #[test]
    fn handoff_and_ready_require_exact_canonical_attempt() {
        let handoff = handoff();
        handoff
            .validate_for_verifier(
                &handoff.root_thread_id,
                &handoff.scenario_id,
                &handoff.recipe_source,
                &ProducerLoopbackIngress {
                    address: handoff.ingress_address.clone(),
                },
            )
            .unwrap();
        let encoded = handoff.canonical_bytes().unwrap();
        assert_eq!(
            ScopedRelayHandoff::from_canonical_bytes(&encoded).unwrap(),
            handoff
        );
        let ready = ScopedRelayReady::for_handoff(&handoff).unwrap();
        let ready_bytes = ready.canonical_bytes_for(&handoff).unwrap();
        assert_eq!(
            ScopedRelayReady::from_canonical_bytes_for(&ready_bytes, &handoff).unwrap(),
            ready
        );
        let mut moved = handoff.clone();
        moved.adapter_request_digest = "0".repeat(64);
        assert!(ScopedRelayReady::from_canonical_bytes_for(&ready_bytes, &moved).is_err());
        moved = handoff.clone();
        moved.recipe_source.recipe_digest = "0".repeat(64);
        assert!(
            moved
                .validate_for_verifier(
                    &handoff.root_thread_id,
                    &handoff.scenario_id,
                    &handoff.recipe_source,
                    &ProducerLoopbackIngress {
                        address: handoff.ingress_address.clone(),
                    },
                )
                .is_err()
        );
        let mut malformed = encoded;
        malformed.push(b' ');
        assert!(ScopedRelayHandoff::from_canonical_bytes(&malformed).is_err());
    }
}
