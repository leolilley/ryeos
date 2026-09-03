//! One-successor chain-writer transition carried by the generic attestation
//! object. Trust in a node key is deliberately insufficient to sign an
//! existing chain; this proof grants one exact successor transition.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use super::Attestation;
use crate::signer::Signer;

pub const CHAIN_WRITER_TRANSITION_POLICY: &str = "chain-writer-transition-v1";
pub const CHAIN_WRITER_TRANSITION_CLAIM: &str = "granted";
pub const CHAIN_WRITER_TRANSITION_SCHEMA: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainWriterTransitionEvidence {
    pub schema: u32,
    pub operation_id: String,
    pub owner_principal: String,
    pub chain_root_id: String,
    pub origin_site_id: String,
    pub source_site_id: String,
    pub target_site_id: String,
    pub source_chain_head_hash: String,
    pub source_node_signer_fingerprint: String,
    pub source_placement_thread_id: String,
    pub source_last_event_hash: String,
    pub successor_placement_thread_id: String,
    pub placement_attestation_hash: String,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub source_accounting_transfer_hash: Option<String>,
    pub transition_subject_hash: String,
    pub target_node_signer_fingerprint: String,
}

impl ChainWriterTransitionEvidence {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != CHAIN_WRITER_TRANSITION_SCHEMA {
            bail!("chain writer transition is not the exact current contract");
        }
        for (label, value) in [
            ("chain writer operation", self.operation_id.as_str()),
            (
                "chain writer source head",
                self.source_chain_head_hash.as_str(),
            ),
            (
                "chain writer source event",
                self.source_last_event_hash.as_str(),
            ),
            (
                "chain writer placement attestation",
                self.placement_attestation_hash.as_str(),
            ),
            (
                "chain writer transition subject",
                self.transition_subject_hash.as_str(),
            ),
        ] {
            super::thread_snapshot::validate_canonical_hash(label, value)?;
        }
        if let Some(value) = &self.source_accounting_transfer_hash {
            super::thread_snapshot::validate_canonical_hash(
                "chain writer accounting allowance transfer",
                value,
            )?;
        }
        for (label, value) in [
            ("chain writer owner", self.owner_principal.as_str()),
            ("chain writer root", self.chain_root_id.as_str()),
            ("chain writer origin site", self.origin_site_id.as_str()),
            ("chain writer source site", self.source_site_id.as_str()),
            ("chain writer target site", self.target_site_id.as_str()),
            (
                "chain writer source placement",
                self.source_placement_thread_id.as_str(),
            ),
            (
                "chain writer successor placement",
                self.successor_placement_thread_id.as_str(),
            ),
        ] {
            validate_label(label, value)?;
        }
        validate_fingerprint(
            "chain writer source signer",
            &self.source_node_signer_fingerprint,
        )?;
        validate_fingerprint(
            "chain writer target signer",
            &self.target_node_signer_fingerprint,
        )?;
        if self.source_site_id == self.target_site_id
            || self.source_node_signer_fingerprint == self.target_node_signer_fingerprint
            || self.source_placement_thread_id == self.successor_placement_thread_id
        {
            bail!("chain writer transition does not change its exact writer placement");
        }
        Ok(())
    }

    pub fn sign_attestation(&self, signer: &dyn Signer) -> anyhow::Result<Attestation> {
        self.validate()?;
        if signer.fingerprint() != self.source_node_signer_fingerprint {
            bail!("chain writer grant must be signed by its exact source node");
        }
        Attestation::unsigned(
            self.transition_subject_hash.clone(),
            CHAIN_WRITER_TRANSITION_CLAIM.to_owned(),
            CHAIN_WRITER_TRANSITION_POLICY.to_owned(),
            lillux::time::iso8601_now(),
            None,
            serde_json::to_value(self).context("serialize chain writer transition")?,
        )
        .sign(signer)
    }

    pub fn from_attestation(attestation: &Attestation) -> anyhow::Result<Self> {
        if attestation.policy != CHAIN_WRITER_TRANSITION_POLICY
            || attestation.claim != CHAIN_WRITER_TRANSITION_CLAIM
        {
            bail!("attestation is not a chain writer transition");
        }
        let evidence: Self = serde_json::from_value(attestation.evidence.clone())
            .context("decode chain writer transition")?;
        evidence.validate()?;
        if attestation.subject_hash != evidence.transition_subject_hash
            || attestation.issuer_fingerprint()? != evidence.source_node_signer_fingerprint
        {
            bail!("chain writer attestation contradicts its scoped transition");
        }
        Ok(evidence)
    }
}

fn validate_fingerprint(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} is not a canonical fingerprint");
    }
    Ok(())
}

fn validate_label(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 4096
        || value.trim() != value
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        bail!("{label} is not a bounded canonical label");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;

    fn evidence(source: &TestSigner, target: &TestSigner) -> ChainWriterTransitionEvidence {
        ChainWriterTransitionEvidence {
            schema: CHAIN_WRITER_TRANSITION_SCHEMA,
            operation_id: "1".repeat(64),
            owner_principal: "owner".into(),
            chain_root_id: "T-root".into(),
            origin_site_id: "site:a".into(),
            source_site_id: "site:a".into(),
            target_site_id: "site:b".into(),
            source_chain_head_hash: "2".repeat(64),
            source_node_signer_fingerprint: source.fingerprint().into(),
            source_placement_thread_id: "T-source".into(),
            source_last_event_hash: "3".repeat(64),
            successor_placement_thread_id: "T-target".into(),
            placement_attestation_hash: "4".repeat(64),
            source_accounting_transfer_hash: None,
            transition_subject_hash: "5".repeat(64),
            target_node_signer_fingerprint: target.fingerprint().into(),
        }
    }

    #[test]
    fn grant_is_one_source_to_one_target_successor() {
        let source = TestSigner::new();
        let target = TestSigner::with_fingerprint("9".repeat(64));
        let evidence = evidence(&source, &target);
        let signed = evidence.sign_attestation(&source).unwrap();
        signed.verify_with_key(&source.verifying_key()).unwrap();
        assert_eq!(
            ChainWriterTransitionEvidence::from_attestation(&signed).unwrap(),
            evidence
        );

        let mut changed = evidence;
        changed.target_node_signer_fingerprint = source.fingerprint().into();
        assert!(changed.validate().is_err());
    }

    #[test]
    fn transition_requires_explicit_nullable_accounting_transfer() {
        let source = TestSigner::new();
        let target = TestSigner::with_fingerprint("9".repeat(64));
        let mut value = serde_json::to_value(evidence(&source, &target)).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("source_accounting_transfer_hash");
        let error = serde_json::from_value::<ChainWriterTransitionEvidence>(value).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("source_accounting_transfer_hash"),
            "unexpected error: {error}"
        );
    }
}
