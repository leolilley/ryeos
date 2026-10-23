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
//!
//! One record kind carries both qualification lanes. The lane is explicit in
//! the testimony body and in the receiver's acceptance, and a receiver policy
//! lists the lanes it accepts. The product lane testifies to a guest-owner
//! runtime product; the content lane testifies to an activated content
//! manifest. The content lane carries no product, so what the receiver can
//! re-derive offline is recorded in the acceptance as data.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::qualification::{
    ProductQualificationEvidence, ProductQualificationPolicySource, ProductQualificationResult,
    ProductQualificationVerifier, bounded, exact_coordinate, validate_claims,
};
use super::qualification_publication::QualificationCoordinate;
use super::transfer::ProductWitnessSource;
use super::{validate_canonical_unsuffixed_ref, validate_hash};
use crate::Signer;
use crate::external_content::qualification_evidence::ContentQualificationEvidence;
use crate::objects::{Attestation, canonical_value_digest, deserialize_required_nullable};

pub const REMOTE_QUALIFICATION_TESTIMONY_CLAIM: &str = "qualification_testimony";
pub const REMOTE_QUALIFICATION_TESTIMONY_POLICY: &str = "ryeos.remote_qualification_testimony.v1";
pub const REMOTE_QUALIFICATION_TESTIMONY_BODY_SCHEMA: &str =
    "ryeos.remote_qualification_testimony_body.v2";
pub const REMOTE_QUALIFICATION_ACCEPTANCE_CLAIM: &str = "qualification_testimony_accepted";
pub const REMOTE_QUALIFICATION_ACCEPTANCE_POLICY: &str = "ryeos.remote_qualification_acceptance.v1";
pub const REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_SCHEMA: &str =
    "ryeos.remote_qualification_acceptance_evidence.v3";
pub const REMOTE_QUALIFICATION_ACCEPTANCE_POLICY_SCHEMA: &str =
    "ryeos.remote_qualification_acceptance_policy.v2";
pub const REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACE: &str = "remote-qualification-testimonies";
pub const MAX_REMOTE_QUALIFICATION_TESTIMONY_BODY_BYTES: usize = 128 * 1024;
pub const MAX_REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_BYTES: usize = 256 * 1024;
pub const MAX_REMOTE_QUALIFICATION_ACCEPTANCE_POLICY_BYTES: usize = 16 * 1024;
pub const MAX_REMOTE_QUALIFICATION_TRUSTED_ATTESTERS: usize = 16;
pub const MAX_REMOTE_QUALIFICATION_ACCEPTED_DIGESTS: usize = 32;
pub const MAX_REMOTE_QUALIFICATION_TESTIMONY_AGE_SECONDS: u64 = 31 * 24 * 60 * 60;
pub const MAX_REMOTE_QUALIFICATION_CLOCK_SKEW_SECONDS: u64 = 60 * 60;

/// Digests and facts a receiver re-derives offline from a product testimony,
/// recorded in the acceptance so it states what it did and nothing more.
pub const PRODUCT_LANE_REDERIVATIONS: &[&str] = &[
    "attestation_hash",
    "claims",
    "coordinate",
    "origin_policy",
    "received_product_join",
    "subject_manifest",
];
/// The content lane re-derives the same facts without a product join. Source
/// preparation against the receiver's own activation and consumer inputs is
/// not part of acceptance; it is repeated by every use of the acceptance.
pub const CONTENT_LANE_REDERIVATIONS: &[&str] = &[
    "attestation_hash",
    "claims",
    "coordinate",
    "origin_policy",
    "subject_manifest",
];

/// Facts the receiver takes on the attester's word, recorded in the acceptance
/// so it never reads as re-derived. The product lane asserts nothing.
pub const PRODUCT_LANE_ASSERTIONS: &[&str] = &[];
/// The content lane's activation name cannot be derived from the evidence; the
/// receiver joins it to its own binding and re-prepares against its own
/// activation at every use.
pub const CONTENT_LANE_ASSERTIONS: &[&str] = &["activation_ref"];

/// Which qualification a testimony speaks to. Selects the evidence type the
/// embedded attestation must decode as; it carries no policy of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationLane {
    Content,
    Product,
}

impl QualificationLane {
    pub fn rederivations(self) -> &'static [&'static str] {
        match self {
            Self::Product => PRODUCT_LANE_REDERIVATIONS,
            Self::Content => CONTENT_LANE_REDERIVATIONS,
        }
    }

    /// The owned form of [`Self::rederivations`], as recorded in an acceptance.
    pub fn rederived(self) -> Vec<String> {
        self.rederivations()
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    }

    pub fn assertions(self) -> &'static [&'static str] {
        match self {
            Self::Product => PRODUCT_LANE_ASSERTIONS,
            Self::Content => CONTENT_LANE_ASSERTIONS,
        }
    }

    /// The owned form of [`Self::assertions`], as recorded in an acceptance.
    pub fn asserted(self) -> Vec<String> {
        self.assertions()
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Product => "product",
        }
    }
}

/// The embedded qualification evidence, decoded under the declared lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteQualificationEvidence {
    Product(ProductQualificationEvidence),
    Content(ContentQualificationEvidence),
}

impl RemoteQualificationEvidence {
    fn decode(lane: QualificationLane, attestation: &Attestation) -> anyhow::Result<Self> {
        Ok(match lane {
            QualificationLane::Product => {
                Self::Product(ProductQualificationEvidence::from_attestation(attestation)?)
            }
            QualificationLane::Content => {
                Self::Content(ContentQualificationEvidence::from_attestation(attestation)?)
            }
        })
    }

    fn verify_for_owner(
        lane: QualificationLane,
        attestation: &Attestation,
        node_key: &lillux::crypto::VerifyingKey,
        owner_principal: &str,
    ) -> anyhow::Result<Self> {
        Ok(match lane {
            QualificationLane::Product => {
                Self::Product(ProductQualificationEvidence::verify_attestation_for_owner(
                    attestation,
                    node_key,
                    owner_principal,
                )?)
            }
            QualificationLane::Content => {
                Self::Content(ContentQualificationEvidence::verify_attestation_for_owner(
                    attestation,
                    node_key,
                    owner_principal,
                )?)
            }
        })
    }

    pub fn lane(&self) -> QualificationLane {
        match self {
            Self::Product(_) => QualificationLane::Product,
            Self::Content(_) => QualificationLane::Content,
        }
    }

    pub fn result(&self) -> &ProductQualificationResult {
        match self {
            Self::Product(evidence) => &evidence.result,
            Self::Content(evidence) => &evidence.result,
        }
    }

    pub fn verifier(&self) -> &ProductQualificationVerifier {
        match self {
            Self::Product(evidence) => &evidence.verifier,
            Self::Content(evidence) => &evidence.verifier,
        }
    }

    pub fn policy_source(&self) -> &ProductQualificationPolicySource {
        match self {
            Self::Product(evidence) => &evidence.policy_source,
            Self::Content(evidence) => &evidence.purpose.policy_source,
        }
    }

    /// The principal that owns the qualified subject.
    pub fn owner_principal(&self) -> &str {
        match self {
            Self::Product(evidence) => &evidence.product_coordinate.owner_principal,
            Self::Content(evidence) => &evidence.purpose.owner_fingerprint,
        }
    }

    pub fn coordinate_id(&self) -> anyhow::Result<String> {
        match self {
            Self::Product(evidence) => {
                QualificationCoordinate::from_evidence(evidence)?.coordinate_id()
            }
            Self::Content(evidence) => {
                crate::external_content::qualification_publication::coordinate_id(evidence)
            }
        }
    }

    pub fn product(&self) -> anyhow::Result<&ProductQualificationEvidence> {
        match self {
            Self::Product(evidence) => Ok(evidence),
            Self::Content(_) => bail!("remote qualification testimony is not a product testimony"),
        }
    }

    pub fn content(&self) -> anyhow::Result<&ContentQualificationEvidence> {
        match self {
            Self::Content(evidence) => Ok(evidence),
            Self::Product(_) => bail!("remote qualification testimony is not a content testimony"),
        }
    }
}

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
    pub lane: QualificationLane,
    pub qualification_attestation: Attestation,
    pub qualification_attestation_hash: String,
    pub asserted_coordinate_id: String,
    pub asserted_claims: Vec<String>,
    /// Content lane only: the origin's name for the activated content. The
    /// evidence cannot derive it, so it is a label the receiver joins to its
    /// own binding and re-prepares against its own activation at use.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub activation_ref: Option<String>,
    pub acceptance_policy_ref: String,
}

impl RemoteQualificationTestimonyBody {
    /// Derive the asserted fields from the origin's own product attestation.
    pub fn new_product(
        qualification_attestation: Attestation,
        origin_site_id: String,
        acceptance_policy_ref: String,
    ) -> anyhow::Result<Self> {
        Self::build(
            QualificationLane::Product,
            qualification_attestation,
            origin_site_id,
            None,
            acceptance_policy_ref,
        )
    }

    /// Derive the asserted fields from the origin's own content attestation.
    pub fn new_content(
        qualification_attestation: Attestation,
        origin_site_id: String,
        activation_ref: String,
        acceptance_policy_ref: String,
    ) -> anyhow::Result<Self> {
        Self::build(
            QualificationLane::Content,
            qualification_attestation,
            origin_site_id,
            Some(activation_ref),
            acceptance_policy_ref,
        )
    }

    fn build(
        lane: QualificationLane,
        qualification_attestation: Attestation,
        origin_site_id: String,
        activation_ref: Option<String>,
        acceptance_policy_ref: String,
    ) -> anyhow::Result<Self> {
        let evidence = RemoteQualificationEvidence::decode(lane, &qualification_attestation)?;
        let body = Self {
            schema: REMOTE_QUALIFICATION_TESTIMONY_BODY_SCHEMA.into(),
            origin_site_id,
            lane,
            qualification_attestation_hash: attestation_hash(&qualification_attestation)?,
            asserted_coordinate_id: evidence.coordinate_id()?,
            asserted_claims: evidence.result().claims.clone(),
            activation_ref,
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
        match (self.lane, self.activation_ref.as_deref()) {
            (QualificationLane::Product, None) => {}
            (QualificationLane::Content, Some(activation_ref)) => {
                validate_config_ref("remote qualification activation ref", activation_ref)?;
            }
            _ => {
                bail!("remote qualification activation ref is present exactly for the content lane")
            }
        }
        if attestation_hash(&self.qualification_attestation)? != self.qualification_attestation_hash
        {
            bail!("remote qualification attestation hash differs from the embedded attestation");
        }
        RemoteQualificationEvidence::decode(self.lane, &self.qualification_attestation)?;
        bounded(
            self,
            MAX_REMOTE_QUALIFICATION_TESTIMONY_BODY_BYTES,
            "remote qualification testimony body",
        )
    }

    fn check_derived(&self, evidence: &RemoteQualificationEvidence) -> anyhow::Result<()> {
        if evidence.lane() != self.lane {
            bail!("remote qualification lane differs from the embedded evidence");
        }
        if evidence.coordinate_id()? != self.asserted_coordinate_id {
            bail!("remote qualification asserted coordinate differs from the embedded evidence");
        }
        if evidence.result().claims != self.asserted_claims {
            bail!("remote qualification asserted claims differ from the embedded evidence");
        }
        Ok(())
    }

    /// Sign the testimony with the origin operator's key. The operator
    /// fingerprint must own the qualification, and the embedded
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
    pub evidence: RemoteQualificationEvidence,
    pub coordinate_id: String,
}

impl VerifiedRemoteQualificationTestimony {
    pub fn lane(&self) -> QualificationLane {
        self.body.lane
    }
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
    let evidence = RemoteQualificationEvidence::verify_for_owner(
        body.lane,
        &body.qualification_attestation,
        origin_node_key,
        &testimony.issuer,
    )?;
    body.check_derived(&evidence)?;
    if testimony.subject_hash != evidence.result().subject_manifest_hash
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
    pub lane: QualificationLane,
    pub testimony: Attestation,
    /// Product lane: the received product the qualification was joined to.
    /// Content lane: null, the content lane has no product.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub product_source: Option<ProductWitnessSource>,
    pub acceptance_policy_ref: String,
    pub acceptance_policy_digest: String,
    pub attester_verifying_key: [u8; 32],
    pub origin_node_verifying_key: [u8; 32],
    pub accepted_claims: Vec<String>,
    /// Exactly the offline re-derivations acceptance performed for the lane.
    pub rederived: Vec<String>,
    /// Exactly the facts the lane takes on the attester's word, not re-derived.
    pub asserted: Vec<String>,
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
        if body.lane != self.lane {
            bail!("remote qualification acceptance lane differs from the testimony");
        }
        match (&self.product_source, self.lane) {
            (Some(source @ ProductWitnessSource::Received { .. }), QualificationLane::Product) => {
                source.validate()?;
            }
            (None, QualificationLane::Content) => {}
            _ => bail!(
                "remote qualification acceptance requires a received product source exactly for the product lane"
            ),
        }
        if self.rederived != self.lane.rederivations() {
            bail!(
                "remote qualification acceptance records re-derivations the lane does not define"
            );
        }
        if self.asserted != self.lane.assertions() {
            bail!("remote qualification acceptance records assertions the lane does not define");
        }
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
    /// Lanes this receiver accepts testimony for. A lane absent here is
    /// refused however well the testimony verifies.
    pub allowed_lanes: Vec<QualificationLane>,
    pub max_testimony_age_seconds: u64,
    /// Tolerated distance of an issue time into this node's future.
    pub max_clock_skew_seconds: u64,
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
        if self.allowed_lanes.is_empty()
            || self.allowed_lanes.windows(2).any(|pair| pair[0] >= pair[1])
        {
            bail!("allowed lanes must be a non-empty sorted unique set");
        }
        if self.max_testimony_age_seconds == 0
            || self.max_testimony_age_seconds > MAX_REMOTE_QUALIFICATION_TESTIMONY_AGE_SECONDS
        {
            bail!("maximum testimony age must be positive and bounded");
        }
        if self.max_clock_skew_seconds > MAX_REMOTE_QUALIFICATION_CLOCK_SKEW_SECONDS {
            bail!("maximum clock skew must be bounded");
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

    /// A lane absent from the policy is refused however well the testimony
    /// verifies.
    pub fn require_lane(&self, lane: QualificationLane) -> anyhow::Result<()> {
        if self.allowed_lanes.binary_search(&lane).is_err() {
            bail!(
                "remote qualification acceptance policy does not allow the {} lane",
                lane.name()
            );
        }
        Ok(())
    }

    /// An acceptance was made under exactly one policy value. Any edit of the
    /// policy, including one that would still admit the testimony, voids it
    /// until the testimony is accepted again under the edited policy.
    pub fn require_unchanged_since_acceptance(&self, recorded_digest: &str) -> anyhow::Result<()> {
        let current = self.digest()?;
        if current != recorded_digest {
            bail!(
                "remote qualification acceptance policy changed since acceptance: recorded digest {recorded_digest}, current digest {current}; the testimony must be accepted again under the current policy"
            );
        }
        Ok(())
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
/// are taken as arguments; the caller owns those reads and any lane-specific
/// join (the received product, the receiver's own activation).
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

    // 0. The receiver names the lane it accepts.
    policy.require_lane(body.lane)?;

    // 1. Policy refs agree between the receiver policy, binding and evidence.
    if body.acceptance_policy_ref != acceptance_policy_ref
        || policy.qualification_policy_ref != binding_policy_ref
        || binding_policy_ref != evidence.policy_source().canonical_ref
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
    let claims = &evidence.result().claims;
    let origin_allowed = &evidence.policy_source().policy.allowed_claims;
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
    let verifier = evidence.verifier();
    let artifact_digest =
        canonical_value_digest(&serde_json::to_value(&verifier.artifact_identity)?)?;
    if policy
        .accepted_verifier_definition_digests
        .binary_search(&verifier.effective_definition_digest)
        .is_err()
        || policy
            .accepted_verifier_artifact_digests
            .binary_search(&artifact_digest)
            .is_err()
    {
        bail!("remote qualification carries a stale verifier identity");
    }

    // 5. Age, clock skew and expiry of both the testimony and the
    //    qualification it carries.
    let now_seconds = epoch_seconds("admission time", now)?;
    require_issue_time_within_policy(
        policy,
        "testimony",
        &verified.testimony.issued_at,
        now_seconds,
    )?;
    require_issue_time_within_policy(
        policy,
        "embedded qualification",
        &body.qualification_attestation.issued_at,
        now_seconds,
    )?;
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

/// An issue time may lead this node's clock by at most the policy's skew and
/// may trail it by at most the policy's maximum age.
fn require_issue_time_within_policy(
    policy: &RemoteQualificationAcceptancePolicy,
    what: &str,
    issued_at: &str,
    now_seconds: i64,
) -> anyhow::Result<()> {
    let issued = epoch_seconds(&format!("{what} issue time"), issued_at)?;
    let skew = i64::try_from(policy.max_clock_skew_seconds).unwrap_or(i64::MAX);
    if issued > now_seconds.saturating_add(skew) {
        bail!(
            "remote qualification {what} is issued in the future beyond the allowed clock skew of {} seconds",
            policy.max_clock_skew_seconds
        );
    }
    let age = now_seconds.saturating_sub(issued);
    if u64::try_from(age).is_ok_and(|age| age > policy.max_testimony_age_seconds) {
        bail!(
            "remote qualification {what} is older than the maximum testimony age of {} seconds",
            policy.max_testimony_age_seconds
        );
    }
    Ok(())
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
        let body = RemoteQualificationTestimonyBody::new_product(
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
            allowed_lanes: vec![QualificationLane::Product],
            max_testimony_age_seconds: 30 * 24 * 60 * 60,
            max_clock_skew_seconds: 300,
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
        assert_eq!(v.evidence.result().claims, f.body.asserted_claims);
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
            lane: QualificationLane::Product,
            testimony: f.testimony.clone(),
            product_source: Some(ProductWitnessSource::Received {
                acceptance_hash: "a".repeat(64),
            }),
            acceptance_policy_ref: ACCEPTANCE_REF.into(),
            acceptance_policy_digest: f.policy.digest().unwrap(),
            attester_verifying_key: f.attester.verifying_key().to_bytes(),
            origin_node_verifying_key: f.node.verifying_key().to_bytes(),
            accepted_claims: f.body.asserted_claims.clone(),
            rederived: QualificationLane::Product.rederived(),
            asserted: QualificationLane::Product.asserted(),
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
        local.product_source = Some(ProductWitnessSource::LocalCapture {});
        assert!(local.validate().is_err());
        let mut overclaiming = evidence.clone();
        overclaiming.rederived.push("source_preparation".into());
        assert!(overclaiming.validate().is_err());
        let mut over_asserting = evidence.clone();
        over_asserting.asserted.push("activation_ref".into());
        assert!(over_asserting.validate().is_err());
        let mut wrong_lane = evidence.clone();
        wrong_lane.lane = QualificationLane::Content;
        assert!(wrong_lane.validate().is_err());
        let mut wrong_key = evidence.clone();
        wrong_key.attester_verifying_key = f.node.verifying_key().to_bytes();
        assert!(wrong_key.verify().is_err());
        let mut wrong_claims = evidence;
        wrong_claims.accepted_claims = vec!["command_probe".into()];
        assert!(wrong_claims.validate().is_err());
    }

    #[test]
    fn issue_time_skew_and_embedded_qualification_age_are_bounded() {
        let f = fixture();
        // Testimony issued 2026-10-21T00:00:00Z.
        assert!(admit_default(&f, &f.policy, "2026-10-20T23:57:00Z").is_ok());
        let error = admit_default(&f, &f.policy, "2026-10-20T23:50:00Z").unwrap_err();
        assert!(error.to_string().contains("clock skew"));
        let mut no_skew = f.policy.clone();
        no_skew.max_clock_skew_seconds = 0;
        assert!(admit_default(&f, &no_skew, "2026-10-20T23:59:59Z").is_err());
        assert!(admit_default(&f, &no_skew, "2026-10-21T00:00:00Z").is_ok());
        // The embedded qualification was issued 2026-10-20T00:00:00Z, a day
        // before the testimony, so a two-day age bound refuses it while the
        // testimony alone would still pass.
        let mut tight = f.policy.clone();
        tight.max_testimony_age_seconds = 36 * 60 * 60;
        assert!(admit_default(&f, &tight, "2026-10-21T06:00:00Z").is_ok());
        let error = admit_default(&f, &tight, "2026-10-21T13:00:00Z").unwrap_err();
        assert!(error.to_string().contains("embedded qualification"));
        let mut huge = f.policy.clone();
        huge.max_clock_skew_seconds = MAX_REMOTE_QUALIFICATION_CLOCK_SKEW_SECONDS + 1;
        assert!(huge.validate().is_err());
    }

    #[test]
    fn a_receiver_policy_must_name_the_lane_and_names_both_digests_on_edit() {
        let f = fixture();
        let mut content_only = f.policy.clone();
        content_only.allowed_lanes = vec![QualificationLane::Content];
        let error = admit_default(&f, &content_only, NOW).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not allow the product lane")
        );
        let mut unsorted = f.policy.clone();
        unsorted.allowed_lanes = vec![QualificationLane::Product, QualificationLane::Content];
        assert!(unsorted.validate().is_err());
        let mut empty = f.policy.clone();
        empty.allowed_lanes = Vec::new();
        assert!(empty.validate().is_err());

        let recorded = f.policy.digest().unwrap();
        f.policy
            .require_unchanged_since_acceptance(&recorded)
            .unwrap();
        let mut edited = f.policy.clone();
        edited.max_testimony_age_seconds -= 1;
        let error = edited
            .require_unchanged_since_acceptance(&recorded)
            .unwrap_err()
            .to_string();
        assert!(error.contains(&recorded) && error.contains(&edited.digest().unwrap()));
    }

    struct ContentFixture {
        testimony: Attestation,
        body: RemoteQualificationTestimonyBody,
        node: KeySigner,
        attester: KeySigner,
        policy: RemoteQualificationAcceptancePolicy,
    }

    const ACTIVATION_REF: &str = "config:fixtures/activation";

    fn content_fixture() -> ContentFixture {
        let attester = KeySigner::new(7);
        let node = KeySigner::new(9);
        let capsule =
            crate::external_execution::admission::test_support::content_candidate_capsule(
                &"a".repeat(64),
            )
            .unwrap();
        let mut evidence = capsule
            .retained_external_runtime_content_qualification
            .unwrap()
            .evidence;
        evidence.purpose.owner_fingerprint = format!("fp:{}", attester.fingerprint());
        evidence.validate().unwrap();
        let artifact_digest = canonical_value_digest(
            &serde_json::to_value(&evidence.verifier.artifact_identity).unwrap(),
        )
        .unwrap();
        let qualification = evidence
            .sign_attestation(&node, "2026-10-20T00:00:00Z".into(), None)
            .unwrap();
        let body = RemoteQualificationTestimonyBody::new_content(
            qualification,
            SITE.into(),
            ACTIVATION_REF.into(),
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
            qualification_policy_ref: evidence.purpose.policy_source.canonical_ref.clone(),
            trusted_attesters: vec![RemoteQualificationTrustedAttester {
                principal: format!("fp:{}", attester.fingerprint()),
                origin_site_id: SITE.into(),
            }],
            required_claims: evidence.result.claims.clone(),
            allowed_claims: evidence.purpose.policy_source.policy.allowed_claims.clone(),
            accepted_verifier_definition_digests: vec![
                evidence.verifier.effective_definition_digest.clone(),
            ],
            accepted_verifier_artifact_digests: vec![artifact_digest],
            allowed_lanes: vec![QualificationLane::Content],
            max_testimony_age_seconds: 30 * 24 * 60 * 60,
            max_clock_skew_seconds: 300,
        };
        policy.validate().unwrap();
        ContentFixture {
            testimony,
            body,
            node,
            attester,
            policy,
        }
    }

    fn verified_content(f: &ContentFixture) -> VerifiedRemoteQualificationTestimony {
        verify_remote_qualification_testimony(
            &f.testimony,
            &f.attester.verifying_key(),
            &f.node.verifying_key(),
        )
        .unwrap()
    }

    #[test]
    fn the_content_lane_verifies_and_is_admitted_only_when_the_policy_allows_it() {
        let f = content_fixture();
        let verified = verified_content(&f);
        assert_eq!(verified.lane(), QualificationLane::Content);
        assert!(verified.evidence.content().is_ok());
        assert!(verified.evidence.product().is_err());
        assert_eq!(verified.coordinate_id, f.body.asserted_coordinate_id);
        let claims = f.policy.required_claims.clone();
        let policy_ref = f.policy.qualification_policy_ref.clone();
        let accepted = admit(
            &f.policy,
            ACCEPTANCE_REF,
            &verified,
            SITE,
            &policy_ref,
            &claims,
            NOW,
        )
        .unwrap();
        assert_eq!(accepted.claims, f.body.asserted_claims);

        let mut product_only = f.policy.clone();
        product_only.allowed_lanes = vec![QualificationLane::Product];
        let error = admit(
            &product_only,
            ACCEPTANCE_REF,
            &verified,
            SITE,
            &policy_ref,
            &claims,
            NOW,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not allow the content lane")
        );
    }

    #[test]
    fn the_lane_is_explicit_and_cannot_be_swapped() {
        let f = content_fixture();
        // A content attestation declared as the product lane does not decode.
        let mut swapped = serde_json::to_value(&f.body).unwrap();
        swapped["lane"] = json!("product");
        swapped["activation_ref"] = Value::Null;
        assert!(RemoteQualificationTestimonyBody::from_value(&swapped).is_err());
        // The activation ref is present exactly for the content lane.
        let mut missing = serde_json::to_value(&f.body).unwrap();
        missing["activation_ref"] = Value::Null;
        assert!(RemoteQualificationTestimonyBody::from_value(&missing).is_err());
        let mut absent = serde_json::to_value(&f.body).unwrap();
        absent.as_object_mut().unwrap().remove("activation_ref");
        assert!(RemoteQualificationTestimonyBody::from_value(&absent).is_err());
        let pf = fixture();
        let mut forced = serde_json::to_value(&pf.body).unwrap();
        forced["lane"] = json!("content");
        forced["activation_ref"] = json!(ACTIVATION_REF);
        assert!(RemoteQualificationTestimonyBody::from_value(&forced).is_err());
        let mut predecessor = serde_json::to_value(&f.body).unwrap();
        predecessor["schema"] = json!("ryeos.remote_qualification_testimony_body.v1");
        assert!(RemoteQualificationTestimonyBody::from_value(&predecessor).is_err());
    }

    #[test]
    fn content_acceptance_evidence_has_no_product_and_records_its_rederivations() {
        let f = content_fixture();
        let evidence = RemoteQualificationAcceptanceEvidence {
            schema: REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_SCHEMA.into(),
            lane: QualificationLane::Content,
            testimony: f.testimony.clone(),
            product_source: None,
            acceptance_policy_ref: ACCEPTANCE_REF.into(),
            acceptance_policy_digest: f.policy.digest().unwrap(),
            attester_verifying_key: f.attester.verifying_key().to_bytes(),
            origin_node_verifying_key: f.node.verifying_key().to_bytes(),
            accepted_claims: f.body.asserted_claims.clone(),
            rederived: QualificationLane::Content.rederived(),
            asserted: QualificationLane::Content.asserted(),
        };
        let round_trip = RemoteQualificationAcceptanceEvidence::from_value(
            &serde_json::to_value(&evidence).unwrap(),
        )
        .unwrap();
        assert_eq!(
            round_trip.verify().unwrap().coordinate_id,
            f.body.asserted_coordinate_id
        );
        assert!(
            !evidence
                .rederived
                .iter()
                .any(|name| name == "received_product_join")
        );
        let mut with_product = evidence.clone();
        with_product.product_source = Some(ProductWitnessSource::Received {
            acceptance_hash: "a".repeat(64),
        });
        assert!(with_product.validate().is_err());
        assert_eq!(evidence.asserted, vec!["activation_ref".to_owned()]);
        let mut unasserted = evidence.clone();
        unasserted.asserted.clear();
        assert!(unasserted.validate().is_err());
        let mut product_rederivations = evidence;
        product_rederivations.rederived = QualificationLane::Product.rederived();
        assert!(product_rederivations.validate().is_err());
    }
}
