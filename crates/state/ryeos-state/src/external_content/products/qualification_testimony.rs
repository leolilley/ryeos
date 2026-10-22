//! Signed cross-node qualification testimony and the receiver's acceptance
//! policy.
//!
//! A testimony is an origin operator's signed statement over the origin node's
//! own signed qualification attestation. It is never a local qualification:
//! it has its own claim, policy, evidence schema and head namespace. The
//! receiver re-derives every digest, coordinate and claim it can from the
//! embedded attestation and admits it only under signed policy data that names
//! the attester. The receiver never runs the origin's verifier and never reads
//! the origin's retained execution as its own.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::qualification::{
    ProductQualificationEvidence, bounded, exact_coordinate, validate_claims,
};
use super::qualification_publication::QualificationCoordinate;
use super::transfer::ProductWitnessSource;
use super::{validate_canonical_unsuffixed_ref, validate_hash};
use crate::Signer;
use crate::objects::{Attestation, canonical_value_digest};

pub const REMOTE_QUALIFICATION_TESTIMONY_CLAIM: &str = "qualification_testimony";
pub const REMOTE_QUALIFICATION_TESTIMONY_POLICY: &str = "ryeos.remote_qualification_testimony.v1";
pub const REMOTE_QUALIFICATION_TESTIMONY_BODY_SCHEMA: &str =
    "ryeos.remote_qualification_testimony_body.v1";
pub const REMOTE_QUALIFICATION_ACCEPTANCE_CLAIM: &str = "qualification_testimony_accepted";
pub const REMOTE_QUALIFICATION_ACCEPTANCE_POLICY: &str = "ryeos.remote_qualification_acceptance.v1";
pub const REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_SCHEMA: &str =
    "ryeos.remote_qualification_acceptance_evidence.v1";
pub const REMOTE_QUALIFICATION_ACCEPTANCE_POLICY_SCHEMA: &str =
    "ryeos.remote_qualification_acceptance_policy.v1";
pub const REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACE: &str =
    "remote-product-qualification-testimonies";
pub const MAX_REMOTE_QUALIFICATION_TESTIMONY_BODY_BYTES: usize = 128 * 1024;
pub const MAX_REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_BYTES: usize = 256 * 1024;
pub const MAX_REMOTE_QUALIFICATION_ACCEPTANCE_POLICY_BYTES: usize = 16 * 1024;
pub const MAX_REMOTE_QUALIFICATION_TRUSTED_ATTESTERS: usize = 16;
pub const MAX_REMOTE_QUALIFICATION_ACCEPTED_DIGESTS: usize = 32;
pub const MAX_REMOTE_QUALIFICATION_TESTIMONY_AGE_SECONDS: u64 = 31 * 24 * 60 * 60;

fn epoch_seconds(label: &str, value: &str) -> anyhow::Result<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| time.timestamp())
        .with_context(|| format!("{label} is not an RFC 3339 time"))
}

fn attestation_hash(attestation: &Attestation) -> anyhow::Result<String> {
    canonical_value_digest(&attestation.to_value())
}

fn validate_config_ref(label: &str, value: &str) -> anyhow::Result<()> {
    validate_canonical_unsuffixed_ref(label, value)?;
    if !value.starts_with("config:") {
        bail!("{label} must be an exact config ref");
    }
    Ok(())
}

fn validate_fingerprint_principal(label: &str, value: &str) -> anyhow::Result<()> {
    let fingerprint = value
        .strip_prefix("fp:")
        .with_context(|| format!("{label} must be an exact fingerprint principal"))?;
    validate_hash(label, fingerprint)
}

/// Origin-side body. The embedded qualification attestation is the only
/// source of truth; the asserted fields exist so any disagreement is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteQualificationTestimonyBody {
    pub schema: String,
    pub origin_site_id: String,
    pub qualification_attestation: Attestation,
    pub qualification_attestation_hash: String,
    pub asserted_coordinate_id: String,
    pub asserted_claims: Vec<String>,
    pub acceptance_policy_ref: String,
}

impl RemoteQualificationTestimonyBody {
    /// Derive the asserted fields from the origin's own attestation.
    pub fn new(
        qualification_attestation: Attestation,
        origin_site_id: String,
        acceptance_policy_ref: String,
    ) -> anyhow::Result<Self> {
        let evidence = ProductQualificationEvidence::from_attestation(&qualification_attestation)?;
        let body = Self {
            schema: REMOTE_QUALIFICATION_TESTIMONY_BODY_SCHEMA.into(),
            origin_site_id,
            qualification_attestation_hash: attestation_hash(&qualification_attestation)?,
            asserted_coordinate_id: QualificationCoordinate::from_evidence(&evidence)?
                .coordinate_id()?,
            asserted_claims: evidence.result.claims.clone(),
            acceptance_policy_ref,
            qualification_attestation,
        };
        body.validate()?;
        Ok(body)
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        bounded(
            value,
            MAX_REMOTE_QUALIFICATION_TESTIMONY_BODY_BYTES,
            "remote qualification testimony body",
        )?;
        let body: Self =
            serde_json::from_value(value.clone()).context("decode remote qualification body")?;
        body.validate()?;
        Ok(body)
    }

    /// Structure and self-consistency only; no signature is checked here.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != REMOTE_QUALIFICATION_TESTIMONY_BODY_SCHEMA {
            bail!("unsupported remote qualification testimony body schema");
        }
        exact_coordinate("remote qualification origin site", &self.origin_site_id)?;
        self.qualification_attestation.validate()?;
        validate_hash(
            "remote qualification attestation hash",
            &self.qualification_attestation_hash,
        )?;
        validate_hash(
            "remote qualification asserted coordinate",
            &self.asserted_coordinate_id,
        )?;
        validate_claims(&self.asserted_claims)?;
        validate_config_ref(
            "remote qualification acceptance policy",
            &self.acceptance_policy_ref,
        )?;
        if attestation_hash(&self.qualification_attestation)? != self.qualification_attestation_hash
        {
            bail!("remote qualification attestation hash differs from the embedded attestation");
        }
        bounded(
            self,
            MAX_REMOTE_QUALIFICATION_TESTIMONY_BODY_BYTES,
            "remote qualification testimony body",
        )
    }

    fn check_derived(&self, evidence: &ProductQualificationEvidence) -> anyhow::Result<()> {
        if QualificationCoordinate::from_evidence(evidence)?.coordinate_id()?
            != self.asserted_coordinate_id
        {
            bail!("remote qualification asserted coordinate differs from the embedded evidence");
        }
        if evidence.result.claims != self.asserted_claims {
            bail!("remote qualification asserted claims differ from the embedded evidence");
        }
        Ok(())
    }

    /// Sign the testimony with the origin operator's key. The operator
    /// fingerprint must own the qualified product, and the embedded
    /// attestation must verify under the origin node key.
    pub fn sign(
        &self,
        signer: &dyn Signer,
        origin_node_key: &lillux::crypto::VerifyingKey,
        issued_at: String,
        expires_at: String,
    ) -> anyhow::Result<Attestation> {
        self.validate()?;
        let testimony = Attestation::unsigned(
            self.qualification_attestation.subject_hash.clone(),
            REMOTE_QUALIFICATION_TESTIMONY_CLAIM.into(),
            REMOTE_QUALIFICATION_TESTIMONY_POLICY.into(),
            issued_at,
            Some(expires_at),
            serde_json::to_value(self)?,
        )
        .sign(signer)?;
        verify_remote_qualification_testimony(
            &testimony,
            &signer.verifying_key(),
            origin_node_key,
        )?;
        Ok(testimony)
    }
}

/// A testimony whose signatures and derivations have all been re-checked.
/// It is testimony only; it is not a local qualification witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRemoteQualificationTestimony {
    pub testimony_hash: String,
    pub testimony: Attestation,
    pub body: RemoteQualificationTestimonyBody,
    pub evidence: ProductQualificationEvidence,
    pub coordinate_id: String,
}

/// Verify the testimony signature against the attester key, the embedded
/// qualification attestation against the origin node key with the attester as
/// owner, and re-derive the hash, coordinate and claims. Expiry against a
/// clock is an admission duty, not checked here.
pub fn verify_remote_qualification_testimony(
    testimony: &Attestation,
    attester_key: &lillux::crypto::VerifyingKey,
    origin_node_key: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<VerifiedRemoteQualificationTestimony> {
    testimony.verify_with_key(attester_key)?;
    if testimony.claim != REMOTE_QUALIFICATION_TESTIMONY_CLAIM
        || testimony.policy != REMOTE_QUALIFICATION_TESTIMONY_POLICY
    {
        bail!("attestation is not remote qualification testimony");
    }
    let body = RemoteQualificationTestimonyBody::from_value(&testimony.evidence)?;
    let evidence = ProductQualificationEvidence::verify_attestation_for_owner(
        &body.qualification_attestation,
        origin_node_key,
        &testimony.issuer,
    )?;
    body.check_derived(&evidence)?;
    if testimony.subject_hash != evidence.result.subject_manifest_hash
        || testimony.subject_hash != body.qualification_attestation.subject_hash
    {
        bail!("remote qualification testimony subject differs from the qualified manifest");
    }
    let expires_at = testimony
        .expires_at
        .as_deref()
        .context("remote qualification testimony must expire")?;
    if let Some(origin_expiry) = body.qualification_attestation.expires_at.as_deref()
        && epoch_seconds("testimony expiry", expires_at)?
            > epoch_seconds("qualification expiry", origin_expiry)?
    {
        bail!("remote qualification testimony outlives the qualification it carries");
    }
    let coordinate_id = body.asserted_coordinate_id.clone();
    Ok(VerifiedRemoteQualificationTestimony {
        testimony_hash: attestation_hash(testimony)?,
        testimony: testimony.clone(),
        body,
        evidence,
        coordinate_id,
    })
}

/// Receiver-side retained acceptance. Keys are retained so recovery verifies
/// the same signatures after remote grants or configuration change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteQualificationAcceptanceEvidence {
    pub schema: String,
    pub testimony: Attestation,
    pub product_source: ProductWitnessSource,
    pub acceptance_policy_ref: String,
    pub acceptance_policy_digest: String,
    pub attester_verifying_key: [u8; 32],
    pub origin_node_verifying_key: [u8; 32],
    pub accepted_claims: Vec<String>,
}

impl RemoteQualificationAcceptanceEvidence {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        bounded(
            value,
            MAX_REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_BYTES,
            "remote qualification acceptance evidence",
        )?;
        let evidence: Self = serde_json::from_value(value.clone())
            .context("decode remote qualification acceptance evidence")?;
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_SCHEMA {
            bail!("unsupported remote qualification acceptance evidence schema");
        }
        self.testimony.validate()?;
        let body = RemoteQualificationTestimonyBody::from_value(&self.testimony.evidence)?;
        if !matches!(self.product_source, ProductWitnessSource::Received { .. }) {
            bail!("remote qualification acceptance requires a received product source");
        }
        self.product_source.validate()?;
        validate_config_ref(
            "remote qualification acceptance policy",
            &self.acceptance_policy_ref,
        )?;
        if self.acceptance_policy_ref != body.acceptance_policy_ref {
            bail!("remote qualification acceptance policy differs from the testimony");
        }
        validate_hash(
            "remote qualification acceptance policy digest",
            &self.acceptance_policy_digest,
        )?;
        validate_claims(&self.accepted_claims)?;
        if self.accepted_claims != body.asserted_claims {
            bail!("remote qualification accepted claims differ from the testimony");
        }
        self.attester_key()?;
        self.origin_node_key()?;
        bounded(
            self,
            MAX_REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_BYTES,
            "remote qualification acceptance evidence",
        )
    }

    pub fn attester_key(&self) -> anyhow::Result<lillux::crypto::VerifyingKey> {
        lillux::crypto::VerifyingKey::from_bytes(&self.attester_verifying_key)
            .context("remote qualification attester key is invalid")
    }

    pub fn origin_node_key(&self) -> anyhow::Result<lillux::crypto::VerifyingKey> {
        lillux::crypto::VerifyingKey::from_bytes(&self.origin_node_verifying_key)
            .context("remote qualification origin node key is invalid")
    }

    /// Re-verify the retained testimony with the retained keys. This consults
    /// no current grant and no current policy.
    pub fn verify(&self) -> anyhow::Result<VerifiedRemoteQualificationTestimony> {
        self.validate()?;
        verify_remote_qualification_testimony(
            &self.testimony,
            &self.attester_key()?,
            &self.origin_node_key()?,
        )
    }
}

/// One attester the receiver trusts, named by operator principal and the
/// origin site its grant must be configured for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteQualificationTrustedAttester {
    pub principal: String,
    pub origin_site_id: String,
}

/// Signed receiver policy as data. Lists are sorted and unique so equality is
/// canonical; nothing here is an enum or a code-selected tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteQualificationAcceptancePolicy {
    pub schema: String,
    pub qualification_policy_ref: String,
    pub trusted_attesters: Vec<RemoteQualificationTrustedAttester>,
    pub required_claims: Vec<String>,
    pub allowed_claims: Vec<String>,
    pub accepted_verifier_definition_digests: Vec<String>,
    pub accepted_verifier_artifact_digests: Vec<String>,
    pub max_testimony_age_seconds: u64,
}

fn validate_digest_list(label: &str, digests: &[String]) -> anyhow::Result<()> {
    if digests.is_empty()
        || digests.len() > MAX_REMOTE_QUALIFICATION_ACCEPTED_DIGESTS
        || digests.windows(2).any(|pair| pair[0] >= pair[1])
    {
        bail!("{label} must be a non-empty bounded sorted unique set");
    }
    for digest in digests {
        validate_hash(label, digest)?;
    }
    Ok(())
}

impl RemoteQualificationAcceptancePolicy {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        bounded(
            value,
            MAX_REMOTE_QUALIFICATION_ACCEPTANCE_POLICY_BYTES,
            "remote qualification acceptance policy",
        )?;
        let policy: Self = serde_json::from_value(value.clone())
            .context("decode remote qualification acceptance policy")?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != REMOTE_QUALIFICATION_ACCEPTANCE_POLICY_SCHEMA {
            bail!("unsupported remote qualification acceptance policy schema");
        }
        validate_config_ref(
            "remote qualification policy",
            &self.qualification_policy_ref,
        )?;
        if self.trusted_attesters.is_empty()
            || self.trusted_attesters.len() > MAX_REMOTE_QUALIFICATION_TRUSTED_ATTESTERS
            || self
                .trusted_attesters
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            bail!("trusted attesters must be a non-empty bounded sorted unique set");
        }
        for attester in &self.trusted_attesters {
            validate_fingerprint_principal("trusted attester", &attester.principal)?;
            exact_coordinate("trusted attester origin site", &attester.origin_site_id)?;
        }
        validate_claims(&self.required_claims)?;
        validate_claims(&self.allowed_claims)?;
        if self
            .required_claims
            .iter()
            .any(|claim| self.allowed_claims.binary_search(claim).is_err())
        {
            bail!("required remote qualification claims must be allowed");
        }
        validate_digest_list(
            "accepted verifier definition digest",
            &self.accepted_verifier_definition_digests,
        )?;
        validate_digest_list(
            "accepted verifier artifact digest",
            &self.accepted_verifier_artifact_digests,
        )?;
        if self.max_testimony_age_seconds == 0
            || self.max_testimony_age_seconds > MAX_REMOTE_QUALIFICATION_TESTIMONY_AGE_SECONDS
        {
            bail!("maximum testimony age must be positive and bounded");
        }
        bounded(
            self,
            MAX_REMOTE_QUALIFICATION_ACCEPTANCE_POLICY_BYTES,
            "remote qualification acceptance policy",
        )
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::to_value(self)?)
    }
}

/// Claims admitted by policy for one verified testimony.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedClaims {
    pub coordinate_id: String,
    pub claims: Vec<String>,
}

/// Pure admission under receiver policy. The live grant lookup (its
/// configured origin site) and the binding's policy ref and required claims
/// are taken as arguments; the caller owns those reads and the product load.
pub fn admit(
    policy: &RemoteQualificationAcceptancePolicy,
    acceptance_policy_ref: &str,
    verified: &VerifiedRemoteQualificationTestimony,
    issuer_grant_site: &str,
    binding_policy_ref: &str,
    binding_required_claims: &[String],
    now: &str,
) -> anyhow::Result<AcceptedClaims> {
    policy.validate()?;
    let evidence = &verified.evidence;
    let body = &verified.body;

    // 1. Policy refs agree between the receiver policy, binding and evidence.
    if body.acceptance_policy_ref != acceptance_policy_ref
        || policy.qualification_policy_ref != binding_policy_ref
        || binding_policy_ref != evidence.policy_source.canonical_ref
    {
        bail!("remote qualification policy ref differs from the binding or the evidence");
    }

    // 2. The attester and its origin site are named by policy and by the grant.
    let named = policy.trusted_attesters.iter().any(|attester| {
        attester.principal == verified.testimony.issuer
            && attester.origin_site_id == body.origin_site_id
    });
    if !named || issuer_grant_site != body.origin_site_id {
        bail!("remote qualification attester is not trusted for this origin site");
    }

    // 3. required policy claims <= binding claims <= evidence claims
    //    <= allowed policy claims and the origin policy's allowance.
    let claims = &evidence.result.claims;
    let origin_allowed = &evidence.policy_source.policy.allowed_claims;
    if policy
        .required_claims
        .iter()
        .any(|claim| !binding_required_claims.contains(claim))
        || binding_required_claims
            .iter()
            .any(|claim| claims.binary_search(claim).is_err())
        || claims.iter().any(|claim| {
            policy.allowed_claims.binary_search(claim).is_err()
                || origin_allowed.binary_search(claim).is_err()
        })
    {
        bail!("remote qualification claims differ from the required and allowed claims");
    }

    // 4. Verifier identity digests are pinned as policy data.
    let artifact_digest =
        canonical_value_digest(&serde_json::to_value(&evidence.verifier.artifact_identity)?)?;
    if policy
        .accepted_verifier_definition_digests
        .binary_search(&evidence.verifier.effective_definition_digest)
        .is_err()
        || policy
            .accepted_verifier_artifact_digests
            .binary_search(&artifact_digest)
            .is_err()
    {
        bail!("remote qualification carries a stale verifier identity");
    }

    // 5. Age and expiry of both the testimony and the qualification it carries.
    let issued_at = epoch_seconds("testimony issue time", &verified.testimony.issued_at)?;
    let now_seconds = epoch_seconds("admission time", now)?;
    if issued_at > now_seconds
        || u64::try_from(now_seconds - issued_at)
            .map_or(true, |age| age > policy.max_testimony_age_seconds)
    {
        bail!("remote qualification testimony is too old or issued in the future");
    }
    if verified.testimony.is_expired_at(now)?
        || body.qualification_attestation.is_expired_at(now)?
    {
        bail!("remote qualification testimony or its qualification is expired");
    }

    Ok(AcceptedClaims {
        coordinate_id: verified.coordinate_id.clone(),
        claims: claims.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct KeySigner(lillux::crypto::SigningKey, String);

    impl KeySigner {
        fn new(seed: u8) -> Self {
            let key = lillux::crypto::SigningKey::from_bytes(&[seed; 32]);
            let fingerprint = lillux::crypto::fingerprint(&key.verifying_key());
            Self(key, fingerprint)
        }
    }

    impl Signer for KeySigner {
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

    const SITE: &str = "origin-site";
    const NOW: &str = "2026-10-22T00:00:00Z";
    const ACCEPTANCE_REF: &str = "config:fixtures/acceptance";
    const QUALIFICATION_REF: &str = "config:fixtures/qualification";

    struct Fixture {
        attester: KeySigner,
        node: KeySigner,
        body: RemoteQualificationTestimonyBody,
        testimony: Attestation,
        policy: RemoteQualificationAcceptancePolicy,
    }

    fn fixture() -> Fixture {
        let attester = KeySigner::new(7);
        let node = KeySigner::new(9);
        let mut evidence = super::super::qualification::tests::evidence();
        evidence.product_coordinate.owner_principal = format!("fp:{}", attester.fingerprint());
        evidence.result.claims = vec!["command_probe".into(), "extension_probe".into()];
        evidence.verifier.result_digest = evidence.result.digest().unwrap();
        evidence.validate().unwrap();
        let artifact_digest = canonical_value_digest(
            &serde_json::to_value(&evidence.verifier.artifact_identity).unwrap(),
        )
        .unwrap();
        let qualification = evidence
            .sign_attestation(
                &node,
                "2026-10-20T00:00:00Z".into(),
                Some("2026-10-30T00:00:00Z".into()),
            )
            .unwrap();
        let body = RemoteQualificationTestimonyBody::new(
            qualification,
            SITE.into(),
            ACCEPTANCE_REF.into(),
        )
        .unwrap();
        let testimony = body
            .sign(
                &attester,
                &node.verifying_key(),
                "2026-10-21T00:00:00Z".into(),
                "2026-10-25T00:00:00Z".into(),
            )
            .unwrap();
        let policy = RemoteQualificationAcceptancePolicy {
            schema: REMOTE_QUALIFICATION_ACCEPTANCE_POLICY_SCHEMA.into(),
            qualification_policy_ref: QUALIFICATION_REF.into(),
            trusted_attesters: vec![RemoteQualificationTrustedAttester {
                principal: format!("fp:{}", attester.fingerprint()),
                origin_site_id: SITE.into(),
            }],
            required_claims: vec!["command_probe".into()],
            allowed_claims: vec!["command_probe".into(), "extension_probe".into()],
            accepted_verifier_definition_digests: vec![
                evidence.verifier.effective_definition_digest.clone(),
            ],
            accepted_verifier_artifact_digests: vec![artifact_digest],
            max_testimony_age_seconds: 30 * 24 * 60 * 60,
        };
        policy.validate().unwrap();
        Fixture {
            attester,
            node,
            body,
            testimony,
            policy,
        }
    }

    fn verified(f: &Fixture) -> VerifiedRemoteQualificationTestimony {
        verify_remote_qualification_testimony(
            &f.testimony,
            &f.attester.verifying_key(),
            &f.node.verifying_key(),
        )
        .unwrap()
    }

    fn admit_default(
        f: &Fixture,
        policy: &RemoteQualificationAcceptancePolicy,
        now: &str,
    ) -> anyhow::Result<AcceptedClaims> {
        admit(
            policy,
            ACCEPTANCE_REF,
            &verified(f),
            SITE,
            QUALIFICATION_REF,
            &["command_probe".to_string()],
            now,
        )
    }

    #[test]
    fn body_round_trips_and_refuses_unknown_fields() {
        let f = fixture();
        let value = serde_json::to_value(&f.body).unwrap();
        assert_eq!(
            RemoteQualificationTestimonyBody::from_value(&value).unwrap(),
            f.body
        );
        let mut extra = value;
        extra["extra"] = json!(1);
        assert!(RemoteQualificationTestimonyBody::from_value(&extra).is_err());
        let v = verified(&f);
        assert_eq!(v.evidence.result.claims, f.body.asserted_claims);
    }

    #[test]
    fn mutated_testimony_signature_is_refused() {
        let f = fixture();
        let mut tampered = f.testimony.clone();
        tampered.expires_at = Some("2026-10-26T00:00:00Z".into());
        assert!(
            verify_remote_qualification_testimony(
                &tampered,
                &f.attester.verifying_key(),
                &f.node.verifying_key()
            )
            .is_err()
        );
        assert!(
            verify_remote_qualification_testimony(
                &f.testimony,
                &f.node.verifying_key(),
                &f.node.verifying_key()
            )
            .is_err()
        );
    }

    #[test]
    fn mutated_embedded_attestation_hash_is_refused() {
        let f = fixture();
        let mut body = f.body.clone();
        body.qualification_attestation_hash = "0".repeat(64);
        assert!(body.validate().is_err());
        assert!(
            body.sign(
                &f.attester,
                &f.node.verifying_key(),
                "2026-10-21T00:00:00Z".into(),
                "2026-10-25T00:00:00Z".into()
            )
            .is_err()
        );
        let mut body = f.body.clone();
        body.qualification_attestation.expires_at = Some("2026-10-31T00:00:00Z".into());
        assert!(body.validate().is_err());
    }

    #[test]
    fn asserted_coordinate_and_claims_must_match_the_embedded_evidence() {
        let f = fixture();
        let mut body = f.body.clone();
        body.asserted_coordinate_id = "0".repeat(64);
        assert!(
            body.sign(
                &f.attester,
                &f.node.verifying_key(),
                "2026-10-21T00:00:00Z".into(),
                "2026-10-25T00:00:00Z".into()
            )
            .is_err()
        );
        let mut body = f.body.clone();
        body.asserted_claims = vec!["command_probe".into()];
        assert!(
            body.sign(
                &f.attester,
                &f.node.verifying_key(),
                "2026-10-21T00:00:00Z".into(),
                "2026-10-25T00:00:00Z".into()
            )
            .is_err()
        );
    }

    #[test]
    fn issuer_must_own_the_qualified_product() {
        let f = fixture();
        let stranger = KeySigner::new(11);
        assert!(
            f.body
                .sign(
                    &stranger,
                    &f.node.verifying_key(),
                    "2026-10-21T00:00:00Z".into(),
                    "2026-10-25T00:00:00Z".into()
                )
                .is_err()
        );
        let other_node = KeySigner::new(13);
        assert!(
            verify_remote_qualification_testimony(
                &f.testimony,
                &f.attester.verifying_key(),
                &other_node.verifying_key()
            )
            .is_err()
        );
    }

    #[test]
    fn testimony_may_not_outlive_its_qualification() {
        let f = fixture();
        assert!(
            f.body
                .sign(
                    &f.attester,
                    &f.node.verifying_key(),
                    "2026-10-21T00:00:00Z".into(),
                    "2026-10-31T00:00:00Z".into()
                )
                .is_err()
        );
    }

    #[test]
    fn acceptance_policy_parser_is_exact_sorted_and_non_empty() {
        let f = fixture();
        let value = serde_json::to_value(&f.policy).unwrap();
        assert_eq!(
            RemoteQualificationAcceptancePolicy::from_value(&value).unwrap(),
            f.policy
        );
        let mut extra = value.clone();
        extra["extra"] = json!(true);
        assert!(RemoteQualificationAcceptancePolicy::from_value(&extra).is_err());
        let mut empty = value.clone();
        empty["required_claims"] = json!([]);
        assert!(RemoteQualificationAcceptancePolicy::from_value(&empty).is_err());
        let mut unsorted = value.clone();
        unsorted["allowed_claims"] = json!(["extension_probe", "command_probe"]);
        assert!(RemoteQualificationAcceptancePolicy::from_value(&unsorted).is_err());
        let mut unsorted_attesters = f.policy.clone();
        let mut second = unsorted_attesters.trusted_attesters[0].clone();
        second.principal = format!("fp:{}", "0".repeat(64));
        unsorted_attesters
            .trusted_attesters
            .insert(0, second.clone());
        assert!(unsorted_attesters.validate().is_ok());
        unsorted_attesters.trusted_attesters.reverse();
        assert!(unsorted_attesters.validate().is_err());
        let mut not_config = value;
        not_config["qualification_policy_ref"] = json!("tool:fixtures/qualification");
        assert!(RemoteQualificationAcceptancePolicy::from_value(&not_config).is_err());
    }

    #[test]
    fn admission_accepts_the_listed_attester_and_refuses_the_rest() {
        let f = fixture();
        let accepted = admit_default(&f, &f.policy, NOW).unwrap();
        assert_eq!(accepted.claims, f.body.asserted_claims);
        assert_eq!(accepted.coordinate_id, f.body.asserted_coordinate_id);

        let mut unlisted = f.policy.clone();
        unlisted.trusted_attesters[0].principal = format!("fp:{}", "0".repeat(64));
        assert!(admit_default(&f, &unlisted, NOW).is_err());

        let mut other_site = f.policy.clone();
        other_site.trusted_attesters[0].origin_site_id = "elsewhere".into();
        assert!(admit_default(&f, &other_site, NOW).is_err());

        let v = verified(&f);
        let admit_with =
            |grant_site: &str, binding_ref: &str, required: &[String], accept_ref: &str| {
                admit(
                    &f.policy,
                    accept_ref,
                    &v,
                    grant_site,
                    binding_ref,
                    required,
                    NOW,
                )
            };
        let required = vec!["command_probe".to_string()];
        assert!(admit_with(SITE, QUALIFICATION_REF, &required, ACCEPTANCE_REF).is_ok());
        assert!(admit_with("elsewhere", QUALIFICATION_REF, &required, ACCEPTANCE_REF).is_err());
        assert!(admit_with(SITE, "config:fixtures/other", &required, ACCEPTANCE_REF).is_err());
        assert!(admit_with(SITE, QUALIFICATION_REF, &required, "config:fixtures/other").is_err());
        assert!(admit_with(SITE, QUALIFICATION_REF, &[], ACCEPTANCE_REF).is_err());
        let unproven = vec!["command_probe".to_string(), "never_claimed".to_string()];
        assert!(admit_with(SITE, QUALIFICATION_REF, &unproven, ACCEPTANCE_REF).is_err());
    }

    #[test]
    fn admission_refuses_claims_outside_the_receiver_allowance() {
        let f = fixture();
        let mut narrow = f.policy.clone();
        narrow.allowed_claims = vec!["command_probe".into()];
        assert!(admit_default(&f, &narrow, NOW).is_err());
    }

    #[test]
    fn admission_refuses_a_stale_verifier_identity() {
        let f = fixture();
        let mut stale_definition = f.policy.clone();
        stale_definition.accepted_verifier_definition_digests = vec!["0".repeat(64)];
        let error = admit_default(&f, &stale_definition, NOW).unwrap_err();
        assert!(error.to_string().contains("stale verifier identity"));
        let mut stale_artifact = f.policy.clone();
        stale_artifact.accepted_verifier_artifact_digests = vec!["0".repeat(64)];
        assert!(admit_default(&f, &stale_artifact, NOW).is_err());
    }

    #[test]
    fn admission_refuses_old_future_and_expired_testimony() {
        let f = fixture();
        let mut strict = f.policy.clone();
        strict.max_testimony_age_seconds = 3600;
        assert!(admit_default(&f, &strict, NOW).is_err());
        assert!(admit_default(&f, &f.policy, "2026-10-20T12:00:00Z").is_err());
        assert!(admit_default(&f, &f.policy, "2026-10-26T00:00:00Z").is_err());
    }

    #[test]
    fn acceptance_evidence_retains_keys_and_reverifies() {
        let f = fixture();
        let evidence = RemoteQualificationAcceptanceEvidence {
            schema: REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_SCHEMA.into(),
            testimony: f.testimony.clone(),
            product_source: ProductWitnessSource::Received {
                acceptance_hash: "a".repeat(64),
            },
            acceptance_policy_ref: ACCEPTANCE_REF.into(),
            acceptance_policy_digest: f.policy.digest().unwrap(),
            attester_verifying_key: f.attester.verifying_key().to_bytes(),
            origin_node_verifying_key: f.node.verifying_key().to_bytes(),
            accepted_claims: f.body.asserted_claims.clone(),
        };
        let round_trip = RemoteQualificationAcceptanceEvidence::from_value(
            &serde_json::to_value(&evidence).unwrap(),
        )
        .unwrap();
        assert_eq!(
            round_trip.verify().unwrap().coordinate_id,
            f.body.asserted_coordinate_id
        );

        let mut local = evidence.clone();
        local.product_source = ProductWitnessSource::LocalCapture {};
        assert!(local.validate().is_err());
        let mut wrong_key = evidence.clone();
        wrong_key.attester_verifying_key = f.node.verifying_key().to_bytes();
        assert!(wrong_key.verify().is_err());
        let mut wrong_claims = evidence;
        wrong_claims.accepted_claims = vec!["command_probe".into()];
        assert!(wrong_claims.validate().is_err());
    }
}
