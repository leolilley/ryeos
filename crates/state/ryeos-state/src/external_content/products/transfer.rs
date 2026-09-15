//! Same-operator, CAS-only product transfer testimony.
//!
//! These contracts reuse generic admission publication. Their owning subjects
//! are acceptance -> generic origin admission -> untouched product witness. Neither
//! a signature nor these read-only checks establishes a current local head,
//! configured-remote permission, consumer binding, or local producer execution.
//! The application must establish those authorities separately. In particular,
//! a received product never authorizes replay as a locally executed build.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::publication::{
    ProductCaptureCoordinate, ProductWitnessLookup, VerifiedProductWitness,
    load_product_attestation_value, lookup_product_witness_hash_guarded,
};
use crate::admission::LOCAL_ADMISSION_POLICY;
use crate::object_closure::{
    ObjectClosureLimits, collect_object_closure_with_cas_and_limits, load_exact_cas_object_with_cas,
};
use crate::objects::{Attestation, canonical_value_digest};
use crate::{CasMutationGuard, PinnedStateAuthority, Signer};

pub const RECEIVED_PRODUCT_SCHEMA: &str = "ryeos.received_product.v1";
pub const RECEIVED_PRODUCT_POLICY: &str = "received-product-v1";
pub const RECEIVED_PRODUCT_CLAIM: &str = "accepted";
pub const MAX_PRODUCT_ADMISSION_ATTESTATION_BYTES: u64 = 8192;
const MAX_EVIDENCE_BYTES: usize = 2048;

/// One explicit source selector; never infer a received source from CAS presence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProductWitnessSource {
    LocalCapture {},
    Received { acceptance_hash: String },
}

impl ProductWitnessSource {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Self::Received { acceptance_hash } = self {
            super::validate_hash("received product acceptance", acceptance_hash)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivedProductEvidence {
    pub schema: String,
    pub owner_principal: String,
    /// The exact origin key admitted by the receiver, not a caller trust grant.
    /// Retaining it permits signature verification after remote configuration
    /// changes without consulting or authorizing a new remote head.
    pub origin_verifying_key: [u8; 32],
}

impl ReceivedProductEvidence {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_evidence(&self.schema, RECEIVED_PRODUCT_SCHEMA, &self.owner_principal)?;
        self.origin_key()?;
        Ok(())
    }

    pub fn origin_key(&self) -> anyhow::Result<lillux::crypto::VerifyingKey> {
        lillux::crypto::VerifyingKey::from_bytes(&self.origin_verifying_key)
            .context("received product origin key is invalid")
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        bound_evidence(value)?;
        let evidence: Self = serde_json::from_value(value.clone())?;
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn from_attestation(attestation: &Attestation) -> anyhow::Result<Self> {
        validate_envelope(attestation, RECEIVED_PRODUCT_POLICY)?;
        Self::from_value(&attestation.evidence)
    }

    pub fn verify_attestation(
        attestation: &Attestation,
        receiver_key: &lillux::crypto::VerifyingKey,
        owner_principal: &str,
    ) -> anyhow::Result<Self> {
        let evidence = Self::from_attestation(attestation)?;
        attestation.verify_with_key(receiver_key)?;
        require_owner(&evidence.owner_principal, owner_principal)?;
        Ok(evidence)
    }

    /// Subject is the origin admission, not its witness or manifest. The
    /// existing generic subject edge therefore retains the whole proof chain.
    pub fn sign_attestation(
        &self,
        origin_admission_hash: &str,
        signer: &dyn Signer,
        issued_at: String,
    ) -> anyhow::Result<Attestation> {
        self.validate()?;
        sign(
            origin_admission_hash,
            RECEIVED_PRODUCT_POLICY,
            self,
            signer,
            issued_at,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAdmittedProduct {
    pub attestation_hash: String,
    pub attestation: Attestation,
    pub product: VerifiedProductWitness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedReceivedProduct {
    pub acceptance_hash: String,
    pub acceptance: Attestation,
    pub evidence: ReceivedProductEvidence,
    pub admitted_product: VerifiedAdmittedProduct,
}

/// Verify historical immutable testimony and actual CAS bytes. This does not
/// authenticate any current acceptance head or configured remote. The generic
/// origin evidence is diagnostic only: actual bytes and receiver bounds are
/// checked here, not inferred from the origin's counters or policy limits.
pub fn verify_admitted_product_guarded(
    authority: &PinnedStateAuthority,
    guard: &CasMutationGuard,
    limits: ObjectClosureLimits,
    admission: &Attestation,
    origin_key: &lillux::crypto::VerifyingKey,
    owner_principal: &str,
) -> anyhow::Result<VerifiedAdmittedProduct> {
    authority.ensure_guard(guard)?;
    let limits = reserve_attestation_limits(admission, limits)?;
    verify_origin_admission(admission, origin_key)?;
    validate_owner(owner_principal)?;
    let cas = authority.cas_store()?;
    let closure =
        collect_object_closure_with_cas_and_limits(&cas, [admission.subject_hash.clone()], limits)?;
    require_cas_only_closure(&closure)?;
    let value = load_product_attestation_value(authority, &admission.subject_hash, limits, guard)?
        .context("transferred product witness is absent")?;
    let product_attestation = Attestation::from_value(&value)?;
    let product_evidence = super::ProductCaptureEvidence::from_attestation(&product_attestation)?;
    require_owner(&product_evidence.owner_principal, owner_principal)?;
    let coordinate = ProductCaptureCoordinate::from_evidence(&product_evidence)?;
    let ProductWitnessLookup::Found(product) = lookup_product_witness_hash_guarded(
        authority,
        &coordinate,
        &admission.subject_hash,
        origin_key,
        limits,
        guard,
    )?
    else {
        bail!("transferred product witness is absent");
    };
    Ok(VerifiedAdmittedProduct {
        attestation_hash: canonical_value_digest(&admission.to_value())?,
        attestation: admission.clone(),
        product,
    })
}

/// Verify an exact local acceptance and its retained origin proof without
/// consulting current heads. Fresh use must additionally check the exact local
/// acceptance head, current receiving policy, and ordinary consumer authority.
pub fn verify_received_product_guarded(
    authority: &PinnedStateAuthority,
    guard: &CasMutationGuard,
    limits: ObjectClosureLimits,
    acceptance: &Attestation,
    receiver_key: &lillux::crypto::VerifyingKey,
    owner_principal: &str,
) -> anyhow::Result<VerifiedReceivedProduct> {
    authority.ensure_guard(guard)?;
    let limits = reserve_attestation_limits(acceptance, limits)?;
    let evidence =
        ReceivedProductEvidence::verify_attestation(acceptance, receiver_key, owner_principal)?;
    let value = load_exact_cas_object_with_cas(
        &authority.cas_store()?,
        &acceptance.subject_hash,
        limits
            .max_object_bytes
            .min(MAX_PRODUCT_ADMISSION_ATTESTATION_BYTES),
    )?;
    let admission = Attestation::from_value(&value)?;
    let admitted_product = verify_admitted_product_guarded(
        authority,
        guard,
        limits,
        &admission,
        &evidence.origin_key()?,
        owner_principal,
    )?;
    if admitted_product.attestation_hash != acceptance.subject_hash {
        bail!("received product acceptance names a different origin admission");
    }
    Ok(VerifiedReceivedProduct {
        acceptance_hash: canonical_value_digest(&acceptance.to_value())?,
        acceptance: acceptance.clone(),
        evidence,
        admitted_product,
    })
}

fn require_cas_only_closure(
    closure: &crate::object_closure::ObjectClosureReport,
) -> anyhow::Result<()> {
    if !closure.is_complete() {
        bail!("product transfer requires a complete bounded CAS closure");
    }
    if !closure.large_object_hashes.is_empty() {
        bail!("product transfer does not transport large-object sidecars");
    }
    Ok(())
}

fn validate_evidence(schema: &str, expected: &str, owner: &str) -> anyhow::Result<()> {
    if schema != expected {
        bail!("unsupported product transfer evidence schema");
    }
    validate_owner(owner)
}

fn validate_owner(owner: &str) -> anyhow::Result<()> {
    super::validate_hash(
        "product transfer operator",
        owner
            .strip_prefix("fp:")
            .context("product transfer operator must be a fingerprint principal")?,
    )
}

fn require_owner(actual: &str, expected: &str) -> anyhow::Result<()> {
    validate_owner(expected)?;
    if actual != expected {
        bail!("product transfer requires the same configured operator");
    }
    Ok(())
}

// Candidate admissions may not be stored yet. Reserve their exact contribution
// before collecting their owning subjects, including both envelopes on receipt.
fn reserve_attestation_limits(
    attestation: &Attestation,
    mut limits: ObjectClosureLimits,
) -> anyhow::Result<ObjectClosureLimits> {
    let bytes = lillux::canonical_json(&attestation.to_value())?.len() as u64;
    if bytes > limits.max_object_bytes || limits.max_links_per_object == 0 {
        bail!("product transfer attestation exceeds current closure object bounds");
    }
    limits.max_objects = limits
        .max_objects
        .checked_sub(1)
        .context("product transfer exceeds closure object count")?;
    limits.max_total_object_bytes = limits
        .max_total_object_bytes
        .checked_sub(bytes)
        .context("product transfer exceeds aggregate closure object bytes")?;
    Ok(limits)
}

fn bound_evidence(value: &Value) -> anyhow::Result<()> {
    if lillux::canonical_json(value)?.len() > MAX_EVIDENCE_BYTES {
        bail!("product transfer evidence exceeds its byte bound");
    }
    Ok(())
}

fn verify_origin_admission(
    admission: &Attestation,
    origin_key: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<()> {
    // Attestation is the existing generic admission envelope/decoder. Its
    // evidence has no additional product authority and is not reinterpreted
    // using a second transfer schema. In particular, origin closure counters
    // cannot substitute for the receiving node's actual closure verification.
    validate_envelope(admission, LOCAL_ADMISSION_POLICY)?;
    admission.verify_with_key(origin_key)
}

fn validate_envelope(attestation: &Attestation, policy: &str) -> anyhow::Result<()> {
    attestation.validate()?;
    if attestation.policy != policy
        || attestation.claim != RECEIVED_PRODUCT_CLAIM
        || attestation.expires_at.is_some()
    {
        bail!("unsupported product transfer admission policy, decision, or expiry");
    }
    if lillux::canonical_json(&attestation.to_value())?.len() as u64
        > MAX_PRODUCT_ADMISSION_ATTESTATION_BYTES
    {
        bail!("product transfer attestation exceeds its byte bound");
    }
    Ok(())
}

fn sign(
    subject: &str,
    policy: &str,
    evidence: &impl Serialize,
    signer: &dyn Signer,
    issued_at: String,
) -> anyhow::Result<Attestation> {
    super::validate_hash("product transfer subject", subject)?;
    let evidence = serde_json::to_value(evidence)?;
    bound_evidence(&evidence)?;
    let attestation = Attestation::unsigned(
        subject.to_owned(),
        RECEIVED_PRODUCT_CLAIM.into(),
        policy.into(),
        issued_at,
        None,
        evidence,
    )
    .sign(signer)?;
    // Signer::fingerprint and its actual key must agree, not just parse.
    attestation.verify_with_key(&signer.verifying_key())?;
    validate_envelope(&attestation, policy)?;
    Ok(attestation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;
    use serde_json::json;

    struct ReceiverSigner(lillux::crypto::SigningKey, String);

    impl ReceiverSigner {
        fn new() -> Self {
            let key = lillux::crypto::SigningKey::from_bytes(&[7; 32]);
            let fingerprint = lillux::crypto::fingerprint(&key.verifying_key());
            Self(key, fingerprint)
        }
    }

    impl Signer for ReceiverSigner {
        fn sign(&self, bytes: &[u8]) -> Vec<u8> {
            use lillux::crypto::Signer as _;
            self.0.sign(bytes).to_bytes().to_vec()
        }

        fn fingerprint(&self) -> &str {
            &self.1
        }

        fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
            self.0.verifying_key()
        }
    }

    fn evidence() -> ReceivedProductEvidence {
        ReceivedProductEvidence {
            schema: RECEIVED_PRODUCT_SCHEMA.into(),
            owner_principal: format!("fp:{}", "a".repeat(64)),
            origin_verifying_key: *TestSigner::new().verifying_key().as_bytes(),
        }
    }

    fn origin_admission(signer: &dyn Signer) -> Attestation {
        // The generic origin envelope is unchanged. These diagnostic counters
        // deliberately grant nothing to the product verifier.
        Attestation::unsigned(
            "b".repeat(64),
            "accepted".into(),
            LOCAL_ADMISSION_POLICY.into(),
            "2026-09-08T00:00:00Z".into(),
            None,
            json!({"closure":{"root_count":1,"object_count":2,"blob_count":1},
                "limits":{"max_objects":100,"max_total_blob_bytes":1}}),
        )
        .sign(signer)
        .unwrap()
    }

    #[test]
    fn source_is_closed_and_received_hash_is_exact() {
        for value in [
            json!({"kind":"local_capture"}),
            json!({
                "kind":"received", "acceptance_hash":"b".repeat(64)
            }),
        ] {
            serde_json::from_value::<ProductWitnessSource>(value)
                .unwrap()
                .validate()
                .unwrap();
        }
        for value in [
            json!({"kind":"local_capture","acceptance_hash":"b".repeat(64)}),
            json!({"kind":"received"}),
            json!({"kind":"remote"}),
        ] {
            assert!(serde_json::from_value::<ProductWitnessSource>(value).is_err());
        }
        assert!(
            ProductWitnessSource::Received {
                acceptance_hash: "B".repeat(64)
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn receiver_evidence_is_closed_versioned_bounded_and_not_a_qualification() {
        let valid = serde_json::to_value(evidence()).unwrap();
        ReceivedProductEvidence::from_value(&valid).unwrap();
        for (field, value) in [
            ("schema", json!("future")),
            ("owner_principal", json!("operator")),
            ("qualification_hash", json!("b".repeat(64))),
            ("padding", json!("x".repeat(2049))),
            ("origin_verifying_key", json!([1, 2, 3])),
        ] {
            let mut wire = valid.clone();
            wire[field] = value;
            assert!(ReceivedProductEvidence::from_value(&wire).is_err());
        }
    }

    #[test]
    fn generic_admission_chain_retains_exact_origin_and_receiver_authorities() {
        let origin = TestSigner::new();
        let receiver = ReceiverSigner::new();
        let admission = origin_admission(&origin);
        let admission_hash = canonical_value_digest(&admission.to_value()).unwrap();
        let evidence = evidence();
        let acceptance = evidence
            .sign_attestation(&admission_hash, &receiver, "2026-09-08T01:00:00Z".into())
            .unwrap();
        assert_eq!(acceptance.subject_hash, admission_hash);
        assert_eq!(admission.subject_hash, "b".repeat(64));
        verify_origin_admission(&admission, &evidence.origin_key().unwrap()).unwrap();
        ReceivedProductEvidence::verify_attestation(
            &acceptance,
            &receiver.verifying_key(),
            &evidence.owner_principal,
        )
        .unwrap();
        assert!(verify_origin_admission(&admission, &receiver.verifying_key()).is_err());
        assert!(
            ReceivedProductEvidence::verify_attestation(
                &acceptance,
                &origin.verifying_key(),
                &evidence.owner_principal,
            )
            .is_err()
        );
        assert!(
            ReceivedProductEvidence::verify_attestation(
                &acceptance,
                &receiver.verifying_key(),
                &format!("fp:{}", "c".repeat(64)),
            )
            .is_err()
        );
        assert!(ReceivedProductEvidence::from_attestation(&admission).is_err());
        let mut changed = admission.clone();
        changed.subject_hash = "c".repeat(64);
        assert!(verify_origin_admission(&changed, &origin.verifying_key()).is_err());
        // This is historical origin admission, not a newly invented product
        // transfer policy. Even a correctly signed alternate policy refuses.
        changed.policy = "retained-product-transfer-v1".into();
        changed = changed.sign(&origin).unwrap();
        assert!(verify_origin_admission(&changed, &origin.verifying_key()).is_err());

        let mut limits = ObjectClosureLimits::default();
        let remaining = reserve_attestation_limits(&admission, limits).unwrap();
        assert_eq!(remaining.max_objects, limits.max_objects - 1);
        assert_eq!(
            remaining.max_total_object_bytes,
            limits.max_total_object_bytes
                - lillux::canonical_json(&admission.to_value()).unwrap().len() as u64
        );
        limits.max_object_bytes = 1;
        assert!(reserve_attestation_limits(&admission, limits).is_err());
        limits = ObjectClosureLimits::default();
        limits.max_total_object_bytes = 1;
        assert!(reserve_attestation_limits(&admission, limits).is_err());
        limits = ObjectClosureLimits::default();
        limits.max_objects = 0;
        assert!(reserve_attestation_limits(&admission, limits).is_err());
    }

    #[test]
    fn admission_refuses_expiry_other_decisions_and_incoherent_signer() {
        let signer = TestSigner::new();
        let admission = origin_admission(&signer);
        let mut changed = admission.clone();
        changed.expires_at = Some("2026-09-09T00:00:00Z".into());
        changed = changed.sign(&signer).unwrap();
        assert!(verify_origin_admission(&changed, &signer.verifying_key()).is_err());
        changed = admission;
        changed.claim = "qualified".into();
        changed = changed.sign(&signer).unwrap();
        assert!(verify_origin_admission(&changed, &signer.verifying_key()).is_err());
        let inconsistent = TestSigner::with_fingerprint("c".repeat(64));
        assert!(
            evidence()
                .sign_attestation(
                    &"b".repeat(64),
                    &inconsistent,
                    "2026-09-08T00:00:00Z".into(),
                )
                .is_err()
        );
    }

    #[test]
    fn actual_sidecar_dependencies_are_refused_even_when_resident() {
        let mut closure = crate::object_closure::ObjectClosureReport::default();
        closure.object_hashes.insert("a".repeat(64));
        closure.blob_hashes.insert("b".repeat(64));
        require_cas_only_closure(&closure).unwrap();
        closure.large_object_hashes.insert("c".repeat(64));
        assert!(require_cas_only_closure(&closure).is_err());
        closure.large_object_hashes.clear();
        closure
            .missing_blobs
            .push(crate::object_closure::MissingDependency {
                hash: "b".repeat(64),
                referenced_by: Some("a".repeat(64)),
            });
        assert!(require_cas_only_closure(&closure).is_err());
    }
}
