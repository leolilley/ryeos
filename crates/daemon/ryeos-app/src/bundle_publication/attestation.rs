//! Closed release-attestation policy for native bundle publication.

use ryeos_state::objects::Attestation;
use ryeos_state::signer::Signer;

use super::VerifiedBundleGeneration;
use ryeos_bundle_publication_contract::SubstrateRelease;

pub const BUNDLE_GENERATION_RELEASE_CLAIM: &str = "ryeos.bundle-generation.release.v1";
pub const BUNDLE_SET_RELEASE_CLAIM: &str = "ryeos.bundle-set.release.v1";
pub const SUBSTRATE_RELEASE_CLAIM: &str = "ryeos.substrate-release.v1";
pub const BUNDLE_CATALOG_RELEASE_CLAIM: &str = "ryeos.bundle-catalog-publication.v1";
pub const BUNDLE_PUBLICATION_POLICY: &str = "ryeos.bundle-publication.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseSubjectKind {
    BundleGeneration,
    BundleSet,
    SubstrateRelease,
    BundleCatalogPublication,
}

impl ReleaseSubjectKind {
    fn claim(self) -> &'static str {
        match self {
            Self::BundleGeneration => BUNDLE_GENERATION_RELEASE_CLAIM,
            Self::BundleSet => BUNDLE_SET_RELEASE_CLAIM,
            Self::SubstrateRelease => SUBSTRATE_RELEASE_CLAIM,
            Self::BundleCatalogPublication => BUNDLE_CATALOG_RELEASE_CLAIM,
        }
    }
}

/// Verify the cryptographic and closed policy identity of an official release.
/// Catalog authorization still decides whether this fingerprint is current for
/// the selected namespace and trust epoch.
pub fn verify_release_attestation(
    attestation: &Attestation,
    subject_hash: &str,
    subject_kind: ReleaseSubjectKind,
    publisher_fingerprint: &str,
    publisher_key: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<()> {
    attestation.verify_with_key(publisher_key)?;
    if attestation.subject_hash != subject_hash {
        anyhow::bail!("release attestation names a different subject");
    }
    if attestation.claim != subject_kind.claim() {
        anyhow::bail!("release attestation has the wrong closed claim");
    }
    if attestation.policy != BUNDLE_PUBLICATION_POLICY {
        anyhow::bail!("release attestation has the wrong publication policy");
    }
    if attestation.expires_at.is_some() {
        anyhow::bail!("v1 release attestations must be non-expiring");
    }
    if attestation.issuer_fingerprint()? != publisher_fingerprint {
        anyhow::bail!("release attestation belongs to a different publisher");
    }
    let key_fingerprint = lillux::crypto::fingerprint(publisher_key);
    if key_fingerprint != publisher_fingerprint {
        anyhow::bail!("publisher key disagrees with publication policy");
    }
    Ok(())
}

/// The narrow generation-release authorization surface. It cannot sign an
/// arbitrary caller-supplied hash: the subject is derived from a semantically
/// verified generation, and the signer must be the same publisher that
/// performed the admitted in-tree materialization.
pub fn authorize_generation_release(
    generation: &VerifiedBundleGeneration,
    signer: &dyn Signer,
    issued_at: &str,
) -> anyhow::Result<Attestation> {
    let materialization = generation.materialization().result();
    if materialization.publisher_fingerprint != signer.fingerprint() {
        anyhow::bail!("release signer differs from in-tree publisher");
    }
    let verifying_key = signer.verifying_key();
    if lillux::crypto::fingerprint(&verifying_key) != signer.fingerprint() {
        anyhow::bail!("release signer fingerprint disagrees with its key");
    }
    let subject_hash = generation.generation().content_hash()?;
    Attestation::unsigned(
        subject_hash,
        BUNDLE_GENERATION_RELEASE_CLAIM.to_owned(),
        BUNDLE_PUBLICATION_POLICY.to_owned(),
        issued_at.to_owned(),
        None,
        serde_json::json!({
            "publisher_materialization_result_hash": generation
                .generation()
                .publisher_materialization_result_hash,
        }),
    )
    .sign(signer)
}

/// Purpose-owned authorization for one immutable substrate/Core binding.
/// Callers must first verify the referenced Core generation and prove that it
/// is the exact generation carried by the measured substrate release.
pub fn authorize_substrate_release(
    release: &SubstrateRelease,
    signer: &dyn Signer,
    issued_at: &str,
) -> anyhow::Result<Attestation> {
    release.validate()?;
    let verifying_key = signer.verifying_key();
    if lillux::crypto::fingerprint(&verifying_key) != signer.fingerprint() {
        anyhow::bail!("substrate release signer fingerprint disagrees with its key");
    }
    Attestation::unsigned(
        release.content_hash()?,
        SUBSTRATE_RELEASE_CLAIM.to_owned(),
        BUNDLE_PUBLICATION_POLICY.to_owned(),
        issued_at.to_owned(),
        None,
        serde_json::json!({
            "core_generation_hash": release.core_generation_hash,
            "core_generation_attestation_hash": release.core_generation_attestation_hash,
            "substrate_image_digest": release.substrate_image_digest,
            "substrate_build_accepted_result_hash": release.substrate_build_accepted_result_hash,
            "substrate_build_receipt_hash": release.substrate_build_receipt_hash,
            "selected_substrate_product_witness": release.selected_substrate_product_witness,
            "qualification_evidence_hashes": release.qualification_evidence_hashes,
            "bundle_publication_policy_section_digest": release.bundle_publication_policy_section_digest,
            "trust_epoch": release.trust_epoch,
        }),
    )
    .sign(signer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_bundle_publication_contract::{
        BundleTarget, SUBSTRATE_RELEASE_KIND, SUBSTRATE_RELEASE_SCHEMA,
    };
    use serde_json::json;

    struct TestSigner {
        key: lillux::crypto::SigningKey,
        fingerprint: String,
    }

    impl TestSigner {
        fn new() -> Self {
            let key = lillux::crypto::SigningKey::from_bytes(&[29; 32]);
            let fingerprint = lillux::crypto::fingerprint(&key.verifying_key());
            Self { key, fingerprint }
        }
    }

    impl Signer for TestSigner {
        fn sign(&self, data: &[u8]) -> Vec<u8> {
            use lillux::crypto::Signer as _;
            self.key.sign(data).to_bytes().to_vec()
        }

        fn fingerprint(&self) -> &str {
            &self.fingerprint
        }

        fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
            self.key.verifying_key()
        }
    }

    fn signed(
        signer: &TestSigner,
        subject: &str,
        claim: &str,
        expires_at: Option<String>,
    ) -> Attestation {
        Attestation::unsigned(
            subject.to_owned(),
            claim.to_owned(),
            BUNDLE_PUBLICATION_POLICY.to_owned(),
            "2026-01-01T00:00:00Z".to_owned(),
            expires_at,
            json!({}),
        )
        .sign(signer)
        .unwrap()
    }

    #[test]
    fn accepts_only_exact_non_expiring_release_claim() {
        let signer = TestSigner::new();
        let subject = "a".repeat(64);
        let attestation = signed(&signer, &subject, BUNDLE_GENERATION_RELEASE_CLAIM, None);
        verify_release_attestation(
            &attestation,
            &subject,
            ReleaseSubjectKind::BundleGeneration,
            signer.fingerprint(),
            &signer.verifying_key(),
        )
        .unwrap();

        let expiring = signed(
            &signer,
            &subject,
            BUNDLE_GENERATION_RELEASE_CLAIM,
            Some("2027-01-01T00:00:00Z".to_owned()),
        );
        assert!(
            verify_release_attestation(
                &expiring,
                &subject,
                ReleaseSubjectKind::BundleGeneration,
                signer.fingerprint(),
                &signer.verifying_key(),
            )
            .is_err()
        );
    }

    #[test]
    fn substrate_release_authorization_uses_closed_claim_and_exact_subject() {
        let signer = TestSigner::new();
        let release = SubstrateRelease {
            schema: SUBSTRATE_RELEASE_SCHEMA.into(),
            kind: SUBSTRATE_RELEASE_KIND.into(),
            catalog_namespace: "official".into(),
            bundle_publication_policy_section_digest: "8".repeat(64),
            trust_epoch: 1,
            substrate_image_digest: format!("sha256:{}", "a".repeat(64)),
            substrate_protocol: 1,
            target: BundleTarget::Portable,
            substrate_build_accepted_result_hash: "d".repeat(64),
            substrate_build_receipt_hash: "1".repeat(64),
            selected_substrate_product_identity: "substrate".into(),
            selected_substrate_product_witness: "e".repeat(64),
            qualification_evidence_hashes: vec!["f".repeat(64)],
            core_generation_hash: "b".repeat(64),
            core_generation_attestation_hash: "c".repeat(64),
        };
        let attestation =
            authorize_substrate_release(&release, &signer, "2026-01-01T00:00:00Z").unwrap();
        verify_release_attestation(
            &attestation,
            &release.content_hash().unwrap(),
            ReleaseSubjectKind::SubstrateRelease,
            signer.fingerprint(),
            &signer.verifying_key(),
        )
        .unwrap();
    }
}
