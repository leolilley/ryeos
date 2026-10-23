//! Cross-node qualification testimony: origin export, receiver acceptance and
//! launch-time re-verification.
//!
//! A testimony is the origin operator's signed statement over the origin
//! node's own signed qualification. The receiver never runs the origin's
//! verifier. It re-derives every digest, coordinate and claim it can, admits
//! the testimony only under signed acceptance policy data that names the
//! attester, and publishes its own node-signed acceptance in a namespace that
//! no local-qualification reader consults. A remote testimony is therefore
//! never returned as, or counted as, a local qualification witness.

use anyhow::{Context as _, bail, ensure};
use ryeos_node_runtime::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use ryeos_state::external_content::products::composition::{
    AdmittedProductQualification, ProductRelationshipQualification,
};
use ryeos_state::external_content::products::publication::{
    ProductCaptureCoordinate, VerifiedProductWitness,
};
use ryeos_state::external_content::products::qualification::{
    ProductQualificationEvidence, ProductQualificationPolicySource,
};
use ryeos_state::external_content::products::qualification_testimony::{
    MAX_REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_BYTES,
    MAX_REMOTE_QUALIFICATION_TESTIMONY_AGE_SECONDS, QualificationLane,
    REMOTE_QUALIFICATION_ACCEPTANCE_CLAIM, REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_SCHEMA,
    REMOTE_QUALIFICATION_ACCEPTANCE_POLICY, REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACE,
    RemoteQualificationAcceptanceEvidence, RemoteQualificationAcceptancePolicy,
    RemoteQualificationEvidence, RemoteQualificationTestimonyBody,
    VerifiedRemoteQualificationTestimony, admit, verify_remote_qualification_testimony,
};
use ryeos_state::external_content::products::transfer::{
    MAX_PRODUCT_ADMISSION_ATTESTATION_BYTES, ProductWitnessSource, RECEIVED_PRODUCT_POLICY,
    ReceivedProductEvidence,
};
use ryeos_state::external_content::qualification_evidence::ContentQualificationEvidence;
use ryeos_state::external_content::qualification_purpose::QualificationSubject;
use ryeos_state::object_closure::{ObjectClosureLimits, load_exact_cas_object_with_cas};
use ryeos_state::objects::{
    Attestation, RetainedExternalRuntimeContentQualification, RetainedRemoteQualificationTestimony,
    canonical_value_digest,
};
use ryeos_state::{CasMutationGuard, PinnedStateAuthority};

use super::content_qualification::{load_current_qualified_content, prepare_source};
use super::product_qualification::{
    load_current_qualification, require_unguarded_resolution,
    resolve_consumer_definition_in_generation, resolve_current_bundle_qualification_policy,
    verify_current_external_runtime_qualification, with_state_guard,
};
use super::product_receipt::{ProductSourceVerification, load_product_source};
use crate::handler_context::HandlerContext;
use crate::managed_external_content_operation::AcquisitionMode;
use crate::node_config::sections::external_execution::{
    ExternalRuntimeContentQualificationBinding, ExternalRuntimeQualificationBinding,
};
use crate::state::AppState;

const ACCEPTANCE_POLICY_FIELD: &str = "remote_qualification_acceptance_policy";

/// The origin's exported, operator-signed testimony.
#[derive(Debug, Clone)]
pub struct TestimonyExport {
    pub testimony: Attestation,
    pub testimony_hash: String,
    pub coordinate_id: String,
}

/// The receiver's recorded acceptance of one testimony.
#[derive(Debug, Clone)]
pub struct TestimonyAcceptance {
    pub testimony_hash: String,
    pub acceptance_hash: String,
    pub coordinate_id: String,
    pub reused_existing: bool,
}

/// A remote testimony that passed every live check at launch. It is a
/// distinct type from a local qualification witness on purpose: nothing that
/// accepts a witness accepts this.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedRemoteRuntimeTestimony {
    pub(crate) acceptance_hash: String,
    pub(crate) acceptance: RemoteQualificationAcceptanceEvidence,
    /// The origin's qualification attestation hash, unmodified.
    pub(crate) qualification_attestation_hash: String,
    /// The origin's evidence, unmodified.
    pub(crate) evidence: ProductQualificationEvidence,
}

/// A remote content testimony that passed every live check at launch. It is
/// a distinct type from a published content qualification on purpose: the
/// evidence and attestation hash are the origin's, and nothing that accepts a
/// local qualification accepts this.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedRemoteRuntimeContentTestimony {
    pub(crate) acceptance_hash: String,
    pub(crate) acceptance: RemoteQualificationAcceptanceEvidence,
    /// The origin's coordinate for the content qualification.
    pub(crate) coordinate_id: String,
    /// The origin's qualification attestation hash, unmodified.
    pub(crate) qualification_attestation_hash: String,
    /// The origin's evidence, unmodified.
    pub(crate) evidence: ContentQualificationEvidence,
}

/// Origin side, blocking. The origin signs only what it would itself launch
/// on now: the exact local qualification must pass the origin's full current
/// verification, so a stale proof is refused rather than exported.
pub fn export_testimony_blocking(
    state: &AppState,
    operator: &NodeIdentity,
    qualification_hash: &str,
    acceptance_policy_ref: &str,
    now: &str,
) -> anyhow::Result<TestimonyExport> {
    require_unguarded_resolution("remote qualification testimony export")?;
    ensure!(
        lillux::valid_hash(qualification_hash),
        "qualification hash must be a canonical hash"
    );
    let owner_principal = operator.principal_id();
    let limits = closure_limits(state)?;
    let authority = state.state_store.pinned_state_authority()?;
    let witness = with_state_guard(&authority, |guard| {
        load_current_qualification(
            state,
            &authority,
            guard,
            limits,
            &owner_principal,
            qualification_hash,
        )
    })?;
    let binding = ExternalRuntimeQualificationBinding {
        attestation_hash: qualification_hash.to_owned(),
        owner_principal: owner_principal.clone(),
        qualification: ProductRelationshipQualification {
            policy_ref: Some(witness.evidence.policy_source.canonical_ref.clone()),
            required_claims: witness.evidence.result.claims.clone(),
        },
        remote_testimony: None,
    };
    let current = verify_current_external_runtime_qualification(
        state,
        &binding,
        &witness.evidence.result.subject_manifest_hash,
    )?;
    ensure!(
        current.attestation_hash == witness.attestation_hash
            && current.evidence == witness.evidence,
        "qualification changed while the origin re-verified it"
    );
    let expires_at = witness
        .attestation
        .expires_at
        .clone()
        .context("a qualification without an expiry cannot be testified")?;
    let body = RemoteQualificationTestimonyBody::new_product(
        witness.attestation.clone(),
        state.threads.site_id().to_owned(),
        acceptance_policy_ref.to_owned(),
    )?;
    let signer = crate::state_store::NodeIdentitySigner::from_identity(operator);
    let testimony = body.sign(
        &signer,
        state.identity.verifying_key(),
        now.to_owned(),
        expires_at,
    )?;
    Ok(TestimonyExport {
        testimony_hash: canonical_value_digest(&testimony.to_value())?,
        testimony,
        coordinate_id: witness.coordinate_id,
    })
}

/// Names the activated content a content testimony speaks to, the way a
/// launch names it: the activation, the origin's coordinate and the manifest
/// of the runtime content.
#[derive(Debug, Clone)]
pub struct ContentExportSelection {
    pub activation_ref: String,
    pub coordinate_id: String,
    pub manifest_hash: String,
}

/// Origin side for the content lane, blocking. The exact published content
/// qualification must pass the origin's full current verification, as a launch
/// requires, so a stale proof is refused rather than exported. A content
/// qualification carries no expiry of its own, so the testimony lives for the
/// maximum testimony age, or less when the qualification expires sooner.
pub fn export_content_testimony_blocking(
    state: &AppState,
    context: &HandlerContext,
    operator: &NodeIdentity,
    selection: &ContentExportSelection,
    qualification_hash: &str,
    acceptance_policy_ref: &str,
    now: &str,
) -> anyhow::Result<TestimonyExport> {
    require_unguarded_resolution("remote content qualification testimony export")?;
    for (label, hash) in [
        ("qualification hash", qualification_hash),
        ("coordinate id", selection.coordinate_id.as_str()),
        ("manifest hash", selection.manifest_hash.as_str()),
    ] {
        ensure!(
            lillux::valid_hash(hash),
            "content {label} must be a canonical hash"
        );
    }
    let owner_principal = operator.principal_id();
    ensure!(
        context.fingerprint == owner_principal,
        "content testimony export requires the configured operator"
    );
    let published = load_current_qualified_content(
        state,
        context,
        &selection.activation_ref,
        &selection.coordinate_id,
        qualification_hash,
        &selection.manifest_hash,
    )?;
    ensure!(
        published.coordinate_id == selection.coordinate_id
            && published.attestation_hash == qualification_hash
            && published.evidence.purpose.owner_fingerprint == owner_principal,
        "published content qualification differs from the requested coordinate or operator"
    );
    let expires_at = content_testimony_expiry(&published.attestation, now)?;
    let body = RemoteQualificationTestimonyBody::new_content(
        published.attestation.clone(),
        state.threads.site_id().to_owned(),
        selection.activation_ref.clone(),
        acceptance_policy_ref.to_owned(),
    )?;
    let signer = crate::state_store::NodeIdentitySigner::from_identity(operator);
    let testimony = body.sign(
        &signer,
        state.identity.verifying_key(),
        now.to_owned(),
        expires_at,
    )?;
    Ok(TestimonyExport {
        testimony_hash: canonical_value_digest(&testimony.to_value())?,
        testimony,
        coordinate_id: published.coordinate_id,
    })
}

fn content_testimony_expiry(qualification: &Attestation, now: &str) -> anyhow::Result<String> {
    let parse = |label: &str, value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|time| time.with_timezone(&chrono::Utc))
            .with_context(|| format!("{label} is not an RFC 3339 time"))
    };
    let limit = parse("export time", now)?
        + chrono::Duration::seconds(i64::try_from(
            MAX_REMOTE_QUALIFICATION_TESTIMONY_AGE_SECONDS,
        )?);
    let expiry = match qualification.expires_at.as_deref() {
        Some(own) => limit.min(parse("qualification expiry", own)?),
        None => limit,
    };
    Ok(expiry.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// Controller side, blocking. `owner_principal` is the authenticated
/// `remote_operator` and `origin_node_key` the pinned key of the configured
/// origin. The testimony is admitted only under the receiver's own signed
/// policy; the resulting node-signed acceptance is idempotent per coordinate
/// and a contradicting head is refused. The product lane joins the testimony
/// to the received product. The content lane has no product: it records the
/// offline re-derivations it performed and leaves source preparation to every
/// use of the acceptance.
pub fn accept_testimony_for_owner(
    state: &AppState,
    owner_principal: &str,
    origin_node_key: &lillux::crypto::VerifyingKey,
    testimony: Attestation,
    now: &str,
) -> anyhow::Result<TestimonyAcceptance> {
    require_unguarded_resolution("remote qualification testimony acceptance")?;
    ensure!(
        testimony.issuer == owner_principal,
        "remote qualification testimony was not issued by the authenticated operator"
    );
    let grant = current_attester_grant(state, owner_principal)?;
    let verified = verify_remote_qualification_testimony(&testimony, &grant.key, origin_node_key)?;
    ensure!(
        verified.evidence.owner_principal() == owner_principal,
        "remote qualification belongs to another owner"
    );
    let policy = resolve_acceptance_policy(state, &verified.body.acceptance_policy_ref)?;
    policy.require_lane(verified.lane())?;
    let origin_policy =
        resolve_current_bundle_qualification_policy(state, &policy.qualification_policy_ref)?;
    require_origin_policy(&verified.evidence, &origin_policy, &policy.required_claims)?;

    let limits = closure_limits(state)?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let product_source = match verified.lane() {
        QualificationLane::Product => {
            let product_evidence = verified.evidence.product()?;
            let received_acceptance_hash = find_received_acceptance(
                state,
                &authority,
                &guard,
                limits,
                owner_principal,
                &product_evidence.product_witness_hash,
                origin_node_key,
            )?;
            let source = ProductWitnessSource::Received {
                acceptance_hash: received_acceptance_hash,
            };
            let product = load_product_source(
                state,
                &authority,
                &guard,
                limits,
                owner_principal,
                &product_evidence.product_witness_hash,
                &source,
                ProductSourceVerification::Fresh,
            )?;
            require_received_product_join(&product, product_evidence)?;
            Some(source)
        }
        QualificationLane::Content => None,
    };
    let accepted = admit(
        &policy,
        &verified.body.acceptance_policy_ref,
        &verified,
        &grant.origin_site_id,
        &policy.qualification_policy_ref,
        &policy.required_claims,
        now,
    )?;

    let evidence = RemoteQualificationAcceptanceEvidence {
        schema: REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_SCHEMA.into(),
        lane: verified.lane(),
        testimony: testimony.clone(),
        product_source,
        acceptance_policy_ref: verified.body.acceptance_policy_ref.clone(),
        acceptance_policy_digest: policy.digest()?,
        attester_verifying_key: grant.key.to_bytes(),
        origin_node_verifying_key: origin_node_key.to_bytes(),
        accepted_claims: accepted.claims.clone(),
        rederived: verified.lane().rederived(),
        asserted: verified.lane().asserted(),
    };
    evidence.validate()?;
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let acceptance = Attestation::unsigned(
        testimony.subject_hash.clone(),
        REMOTE_QUALIFICATION_ACCEPTANCE_CLAIM.into(),
        REMOTE_QUALIFICATION_ACCEPTANCE_POLICY.into(),
        now.to_owned(),
        testimony.expires_at.clone(),
        serde_json::to_value(&evidence)?,
    )
    .sign(&signer)?;

    let node_key = *state.identity.verifying_key();
    // A coordinate is accepted once. A testimony signed again for it, or the
    // same testimony under an edited policy, is refused here with the
    // incumbent named; re-qualification is the remedy, not a supersede.
    if let Some(head) = ryeos_state::immutable_testimony::read_immutable_attestation_head(
        &authority,
        REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACE,
        &accepted.coordinate_id,
        &guard,
    )? {
        let incumbent = load_acceptance_attestation(&authority, limits, &head.target_hash)?;
        let incumbent = verify_acceptance_attestation(&incumbent, &node_key)?;
        ensure!(
            incumbent.evidence == evidence,
            "a different remote qualification testimony or acceptance policy is already accepted at coordinate {} as acceptance {}; accepted testimony is immutable at its coordinate, so re-qualification at the origin is the remedy, not a re-signed testimony",
            accepted.coordinate_id,
            incumbent.hash
        );
    }
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire remote qualification acceptance permit: {error}")
        })?;
    let (loaded, reused_existing) =
        ryeos_state::immutable_testimony::publish_immutable_attestation(
            &authority,
            REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACE,
            &accepted.coordinate_id,
            &acceptance,
            &signer,
            &guard,
            |candidate| verify_acceptance_attestation(candidate, &node_key),
            |hash| {
                let attestation = load_acceptance_attestation(&authority, limits, hash)?;
                let loaded = verify_acceptance_attestation(&attestation, &node_key)?;
                Ok((attestation, loaded))
            },
        )?;
    Ok(TestimonyAcceptance {
        testimony_hash: verified.testimony_hash,
        acceptance_hash: loaded.hash,
        coordinate_id: accepted.coordinate_id,
        reused_existing,
    })
}

/// Fresh launch check for a binding that selects a remote testimony. It
/// re-reads the attester's current grant, the published acceptance, the
/// receiver's acceptance policy and the origin's qualification policy, and
/// re-admits the testimony. It never runs a verifier.
pub(crate) fn verify_current_remote_runtime_testimony(
    state: &AppState,
    binding: &ExternalRuntimeQualificationBinding,
    expected_manifest_hash: &str,
) -> anyhow::Result<VerifiedRemoteRuntimeTestimony> {
    binding.validate()?;
    ensure!(
        lillux::valid_hash(expected_manifest_hash),
        "external runtime manifest must be a canonical hash"
    );
    let remote = binding
        .remote_testimony
        .as_ref()
        .context("runtime qualification selects no remote testimony")?;
    require_unguarded_resolution("remote qualification testimony check")?;
    let grant = current_attester_grant(state, &binding.owner_principal)?;
    let policy_ref = binding
        .qualification
        .policy_ref
        .as_deref()
        .context("external runtime qualification has no signed policy")?;
    let acceptance_policy = resolve_acceptance_policy(state, &remote.acceptance_policy_ref)?;
    let origin_policy = resolve_current_bundle_qualification_policy(state, policy_ref)?;
    let limits = closure_limits(state)?;
    let authority = state.state_store.pinned_state_authority()?;
    let (loaded, product) = with_state_guard(&authority, |guard| {
        read_published_acceptance(
            state,
            &authority,
            guard,
            limits,
            &binding.owner_principal,
            &remote.acceptance_hash,
        )
    })?;
    ensure!(
        loaded.evidence.acceptance_policy_ref == remote.acceptance_policy_ref,
        "remote qualification acceptance was made under a different policy"
    );
    ensure!(
        loaded.evidence.lane == QualificationLane::Product,
        "remote qualification acceptance is not a product acceptance"
    );
    acceptance_policy
        .require_unchanged_since_acceptance(&loaded.evidence.acceptance_policy_digest)?;
    let product = product.context("remote product acceptance has no received product")?;
    // The live grant keys re-verify the testimony: a rotated or revoked
    // attester or origin node key stops launches here even though the
    // retained keys still verify.
    let origin_node_key = loaded.evidence.origin_node_key()?;
    require_current_origin_node_grant(state, &origin_node_key, &grant.origin_site_id)?;
    let live = verify_remote_qualification_testimony(
        &loaded.evidence.testimony,
        &grant.key,
        &origin_node_key,
    )?;
    let live_evidence = live.evidence.product()?;
    ensure!(
        live.body.qualification_attestation_hash == binding.attestation_hash
            && live.testimony.issuer == binding.owner_principal
            && live_evidence.result.subject_manifest_hash == expected_manifest_hash,
        "remote qualification differs from the signed runtime binding"
    );
    require_received_product_join(&product, live_evidence)?;
    require_origin_policy(
        &live.evidence,
        &origin_policy,
        &binding.qualification.required_claims,
    )?;
    admit(
        &acceptance_policy,
        &remote.acceptance_policy_ref,
        &live,
        &grant.origin_site_id,
        policy_ref,
        &binding.qualification.required_claims,
        &lillux::time::iso8601_now(),
    )?;
    // The acceptance and its product must still be the same after the
    // unguarded policy resolution above.
    let (current, current_product) = with_state_guard(&authority, |guard| {
        read_published_acceptance(
            state,
            &authority,
            guard,
            limits,
            &binding.owner_principal,
            &remote.acceptance_hash,
        )
    })?;
    ensure!(
        current.hash == loaded.hash
            && current_product.map(|current| current.attestation_hash)
                == Some(product.attestation_hash.clone()),
        "remote qualification acceptance changed while its policy was re-resolved"
    );
    let live_evidence = live_evidence.clone();
    Ok(VerifiedRemoteRuntimeTestimony {
        acceptance_hash: loaded.hash,
        acceptance: loaded.evidence,
        qualification_attestation_hash: live.body.qualification_attestation_hash,
        evidence: live_evidence,
    })
}

/// Fresh launch check for a content binding that selects a remote testimony.
/// It re-reads the attester's current grant, the published acceptance, the
/// receiver's acceptance policy and the origin's qualification policy, and
/// re-admits the testimony. It never runs a verifier. What acceptance left to
/// use is done here: the receiver's own activation is prepared against the
/// evidence's sealed purpose, so the content the controller will run is the
/// content that was qualified. `context` is the attester's admitted authority.
pub(crate) fn verify_current_remote_runtime_content_testimony(
    state: &AppState,
    context: &HandlerContext,
    binding: &ExternalRuntimeContentQualificationBinding,
    expected_manifest_hash: &str,
) -> anyhow::Result<VerifiedRemoteRuntimeContentTestimony> {
    binding.validate()?;
    ensure!(
        lillux::valid_hash(expected_manifest_hash),
        "external runtime content manifest must be a canonical hash"
    );
    let remote = binding
        .remote_testimony
        .as_ref()
        .context("runtime content qualification selects no remote testimony")?;
    require_unguarded_resolution("remote content qualification testimony check")?;
    let grant = current_attester_grant(state, &binding.owner_principal)?;
    let acceptance_policy = resolve_acceptance_policy(state, &remote.acceptance_policy_ref)?;
    let policy_ref = acceptance_policy.qualification_policy_ref.as_str();
    let origin_policy = resolve_current_bundle_qualification_policy(state, policy_ref)?;
    let limits = closure_limits(state)?;
    let authority = state.state_store.pinned_state_authority()?;
    let (loaded, product) = with_state_guard(&authority, |guard| {
        read_published_acceptance(
            state,
            &authority,
            guard,
            limits,
            &binding.owner_principal,
            &remote.acceptance_hash,
        )
    })?;
    ensure!(
        loaded.evidence.acceptance_policy_ref == remote.acceptance_policy_ref,
        "remote qualification acceptance was made under a different policy"
    );
    ensure!(
        loaded.evidence.lane == QualificationLane::Content && product.is_none(),
        "remote qualification acceptance is not a content acceptance"
    );
    acceptance_policy
        .require_unchanged_since_acceptance(&loaded.evidence.acceptance_policy_digest)?;
    let origin_node_key = loaded.evidence.origin_node_key()?;
    require_current_origin_node_grant(state, &origin_node_key, &grant.origin_site_id)?;
    let live = verify_remote_qualification_testimony(
        &loaded.evidence.testimony,
        &grant.key,
        &origin_node_key,
    )?;
    let live_evidence = live.evidence.content()?;
    let QualificationSubject::ActivatedContent { content } = &live_evidence.purpose.subject else {
        bail!("remote content qualification has no activated content subject");
    };
    ensure!(
        live.body.qualification_attestation_hash == binding.attestation_hash
            && live.coordinate_id == binding.coordinate_id
            && live.testimony.issuer == binding.owner_principal
            && live.body.activation_ref.as_deref() == Some(binding.activation_ref.as_str())
            && content.manifest_hash == expected_manifest_hash
            && live_evidence.result.subject_manifest_hash == expected_manifest_hash,
        "remote content qualification differs from the signed runtime binding"
    );
    require_origin_policy(&live.evidence, &origin_policy, &binding.required_claims)?;
    admit(
        &acceptance_policy,
        &remote.acceptance_policy_ref,
        &live,
        &grant.origin_site_id,
        policy_ref,
        &binding.required_claims,
        &lillux::time::iso8601_now(),
    )?;
    // Acceptance proved the origin's attestation and the receiver's policy.
    // Whether this node's own activation of the content is the qualified
    // content is established here, against the sealed purpose.
    prepare_source(
        state,
        context,
        &binding.activation_ref,
        AcquisitionMode::Offline,
        &content.realization,
    )?
    .require_current_consumer(
        state,
        context,
        AcquisitionMode::Offline,
        &live_evidence.purpose,
    )?;
    let (current, current_product) = with_state_guard(&authority, |guard| {
        read_published_acceptance(
            state,
            &authority,
            guard,
            limits,
            &binding.owner_principal,
            &remote.acceptance_hash,
        )
    })?;
    ensure!(
        current.hash == loaded.hash && current_product.is_none(),
        "remote content qualification acceptance changed while it was re-resolved"
    );
    let live_evidence = live_evidence.clone();
    Ok(VerifiedRemoteRuntimeContentTestimony {
        acceptance_hash: loaded.hash,
        acceptance: loaded.evidence,
        coordinate_id: live.coordinate_id,
        qualification_attestation_hash: live.body.qualification_attestation_hash,
        evidence: live_evidence,
    })
}

/// Recovery check for a retained remote testimony under the caller's CAS
/// guard. Only the retained keys and the signed acceptance are used: no
/// current grant, policy or wall-clock eligibility is reapplied. The product
/// is loaded with the acceptance's `Received` source, because the origin's
/// evidence names its own local capture.
pub(crate) fn verify_retained_remote_testimony_guarded(
    state: &AppState,
    authority: &PinnedStateAuthority,
    guard: &CasMutationGuard,
    limits: ObjectClosureLimits,
    retained: &RetainedRemoteQualificationTestimony,
    proof: &AdmittedProductQualification,
    owner_principal: &str,
) -> anyhow::Result<VerifiedProductWitness> {
    retained.validate()?;
    authority.ensure_guard(guard)?;
    let loaded =
        load_acceptance_guarded(state, authority, guard, limits, &retained.acceptance_hash)?;
    ensure!(
        loaded.evidence == retained.acceptance
            && loaded.verified.evidence.product()? == &proof.evidence
            && loaded.verified.body.qualification_attestation_hash == proof.attestation_hash
            && loaded.verified.testimony.issuer == owner_principal,
        "retained remote testimony differs from its authenticated acceptance"
    );
    let source = loaded
        .evidence
        .product_source
        .as_ref()
        .context("retained remote product testimony has no received product source")?;
    let product = load_product_source(
        state,
        authority,
        guard,
        limits,
        owner_principal,
        &proof.evidence.product_witness_hash,
        source,
        ProductSourceVerification::Retained,
    )?;
    require_received_product_join(&product, &proof.evidence)?;
    Ok(product)
}

/// Recovery check for a retained remote content testimony under the caller's
/// CAS guard, mirroring the product lane: only the retained keys and the
/// signed acceptance are used, with no current grant, policy, source or
/// wall-clock eligibility reapplied.
pub(crate) fn verify_retained_remote_content_testimony_guarded(
    state: &AppState,
    authority: &PinnedStateAuthority,
    guard: &CasMutationGuard,
    limits: ObjectClosureLimits,
    retained: &RetainedExternalRuntimeContentQualification,
) -> anyhow::Result<()> {
    retained.validate()?;
    let remote = retained
        .remote_testimony
        .as_ref()
        .context("retained runtime content selects no remote testimony")?;
    authority.ensure_guard(guard)?;
    let loaded = load_acceptance_guarded(state, authority, guard, limits, &remote.acceptance_hash)?;
    ensure!(
        loaded.evidence == remote.acceptance
            && loaded.evidence.lane == QualificationLane::Content
            && loaded.verified.evidence.content()? == &retained.evidence
            && loaded.verified.body.qualification_attestation_hash == retained.attestation_hash
            && loaded.verified.coordinate_id == retained.coordinate_id
            && loaded.verified.body.activation_ref.as_deref()
                == Some(retained.activation_ref.as_str())
            && loaded.verified.testimony.issuer == retained.evidence.purpose.owner_fingerprint,
        "retained remote content testimony differs from its authenticated acceptance"
    );
    Ok(())
}

struct AttesterGrant {
    key: lillux::crypto::VerifyingKey,
    origin_site_id: String,
}

/// The attester must hold a current node-signed `remote_operator` grant. Its
/// key and configured origin site come from that grant alone.
fn current_attester_grant(state: &AppState, principal: &str) -> anyhow::Result<AttesterGrant> {
    let authority =
        crate::operator_authority::admitted_operator_authority_for_principal(state, principal)?;
    ensure!(
        authority.principal_class == AuthorizedKeyPrincipalClass::RemoteOperator,
        "remote qualification attester must hold a remote_operator grant"
    );
    let fingerprint = principal
        .strip_prefix("fp:")
        .context("remote qualification attester is not a fingerprint principal")?;
    let grant = ryeos_node_runtime::identity::load_verified_authorized_key(
        fingerprint,
        &state.config.authorized_keys_dir,
        &state.identity,
    )?
    .context("remote qualification attester grant was revoked")?;
    ensure!(
        grant.source_file_hash == authority.grant_digest,
        "remote qualification attester grant changed while it was read"
    );
    Ok(AttesterGrant {
        key: grant.public_key,
        origin_site_id: authority.origin_site_id,
    })
}

/// The origin node key retained at acceptance must still hold a current
/// node-signed `remote_node` grant for the attester's site.
fn require_current_origin_node_grant(
    state: &AppState,
    origin_node_key: &lillux::crypto::VerifyingKey,
    origin_site_id: &str,
) -> anyhow::Result<()> {
    let grant = ryeos_node_runtime::identity::load_verified_authorized_key(
        &lillux::crypto::fingerprint(origin_node_key),
        &state.config.authorized_keys_dir,
        &state.identity,
    )?
    .context("remote qualification origin node grant was revoked")?;
    ensure!(
        grant.principal_class == AuthorizedKeyPrincipalClass::RemoteNode
            && grant.public_key == *origin_node_key
            && grant.configured_origin_site_id.as_deref() == Some(origin_site_id),
        "remote qualification origin node grant does not name the attester's site"
    );
    Ok(())
}

/// Resolve the receiver's signed acceptance policy from the trusted bundle
/// generation. Runs handler processes, so it must be called unguarded.
fn resolve_acceptance_policy(
    state: &AppState,
    policy_ref: &str,
) -> anyhow::Result<RemoteQualificationAcceptancePolicy> {
    state.engine.with_checked_bundle_generation(|generation| {
        let resolution =
            resolve_consumer_definition_in_generation(generation, policy_ref, "config")?;
        let value = resolution
            .composed
            .composed
            .get(ACCEPTANCE_POLICY_FIELD)
            .context("acceptance Config has no remote_qualification_acceptance_policy")?;
        RemoteQualificationAcceptancePolicy::from_value(value)
    })
}

/// The origin's recorded policy must be exactly what this node resolves for
/// the same ref now, and must still admit the claims the receiver requires.
/// The verifier identity is compared against itself because the receiver's
/// accepted-verifier digests, checked at admission, are what pin it.
fn require_origin_policy(
    evidence: &RemoteQualificationEvidence,
    current: &ProductQualificationPolicySource,
    required_claims: &[String],
) -> anyhow::Result<()> {
    match evidence {
        RemoteQualificationEvidence::Product(evidence) => evidence.validate_current_policy(
            current,
            &evidence.verifier.effective_definition_digest,
            required_claims,
        ),
        RemoteQualificationEvidence::Content(evidence) => {
            evidence.validate()?;
            current.validate()?;
            ensure!(
                &evidence.purpose.policy_source == current,
                "qualification no longer matches the exact current policy"
            );
            evidence
                .result
                .validate_claims_for(&current.policy, required_claims)
        }
    }
}

fn require_received_product_join(
    product: &VerifiedProductWitness,
    evidence: &ProductQualificationEvidence,
) -> anyhow::Result<()> {
    if product.attestation_hash != evidence.product_witness_hash
        || product.evidence.manifest_hash != evidence.result.subject_manifest_hash
        || ProductCaptureCoordinate::from_evidence(&product.evidence)?
            != evidence.product_coordinate
    {
        bail!("remote qualification differs from its received product");
    }
    Ok(())
}

fn closure_limits(state: &AppState) -> anyhow::Result<ObjectClosureLimits> {
    state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()
}

/// Find this node's own acceptance of the received product that carries the
/// qualified witness, accepted from the same origin key for the same owner.
fn find_received_acceptance(
    state: &AppState,
    authority: &PinnedStateAuthority,
    guard: &CasMutationGuard,
    limits: ObjectClosureLimits,
    owner_principal: &str,
    witness_hash: &str,
    origin_key: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<String> {
    authority.ensure_guard(guard)?;
    let namespace = format!("admissions/{RECEIVED_PRODUCT_POLICY}");
    let heads = state
        .state_store
        .with_state_db(|db| db.list_generic_head_refs(&namespace))?;
    let cas = authority.cas_store()?;
    let byte_limit = limits
        .max_object_bytes
        .min(MAX_PRODUCT_ADMISSION_ATTESTATION_BYTES);
    let mut matching = Vec::new();
    for head in heads {
        if head.signer != state.identity.fingerprint() {
            continue;
        }
        // Another owner's, another origin's or an unrelated product's
        // acceptance is simply not a candidate; the exact candidate is fully
        // verified by the product load that follows.
        let Ok(value) = load_exact_cas_object_with_cas(&cas, &head.target_hash, byte_limit) else {
            continue;
        };
        let Ok(acceptance) = Attestation::from_value(&value) else {
            continue;
        };
        let Ok(received) = ReceivedProductEvidence::verify_attestation(
            &acceptance,
            state.identity.verifying_key(),
            owner_principal,
        ) else {
            continue;
        };
        if received.origin_verifying_key != origin_key.to_bytes() {
            continue;
        }
        let Ok(admission_value) =
            load_exact_cas_object_with_cas(&cas, &acceptance.subject_hash, byte_limit)
        else {
            continue;
        };
        let Ok(admission) = Attestation::from_value(&admission_value) else {
            continue;
        };
        if admission.subject_hash == witness_hash {
            matching.push(head.target_hash);
        }
    }
    ensure!(
        matching.len() <= 1,
        "the qualified product was received more than once from this origin by this operator"
    );
    matching
        .pop()
        .context("the qualified product was not received from this origin by this operator")
}

fn acceptance_byte_limit(limits: ObjectClosureLimits) -> u64 {
    limits.max_object_bytes.min(
        MAX_REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_BYTES as u64
            + MAX_PRODUCT_ADMISSION_ATTESTATION_BYTES,
    )
}

fn load_acceptance_attestation(
    authority: &PinnedStateAuthority,
    limits: ObjectClosureLimits,
    hash: &str,
) -> anyhow::Result<Attestation> {
    ensure!(
        lillux::valid_hash(hash),
        "remote qualification acceptance must be a canonical hash"
    );
    let value = load_exact_cas_object_with_cas(
        &authority.cas_store()?,
        hash,
        acceptance_byte_limit(limits),
    )?;
    Attestation::from_value(&value)
}

#[derive(Debug, Clone)]
struct LoadedAcceptance {
    hash: String,
    evidence: RemoteQualificationAcceptanceEvidence,
    verified: VerifiedRemoteQualificationTestimony,
}

/// Authenticate one acceptance attestation as this node's own signature and
/// re-verify the testimony it retains with its retained keys.
fn verify_acceptance_attestation(
    attestation: &Attestation,
    node_key: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<LoadedAcceptance> {
    attestation.verify_with_key(node_key)?;
    ensure!(
        attestation.claim == REMOTE_QUALIFICATION_ACCEPTANCE_CLAIM
            && attestation.policy == REMOTE_QUALIFICATION_ACCEPTANCE_POLICY,
        "attestation is not a remote qualification acceptance"
    );
    let evidence = RemoteQualificationAcceptanceEvidence::from_value(&attestation.evidence)?;
    let verified = evidence.verify()?;
    ensure!(
        attestation.subject_hash == verified.evidence.result().subject_manifest_hash
            && attestation.expires_at == evidence.testimony.expires_at,
        "remote qualification acceptance contradicts the testimony it retains"
    );
    Ok(LoadedAcceptance {
        hash: canonical_value_digest(&attestation.to_value())?,
        evidence,
        verified,
    })
}

fn load_acceptance_guarded(
    state: &AppState,
    authority: &PinnedStateAuthority,
    guard: &CasMutationGuard,
    limits: ObjectClosureLimits,
    acceptance_hash: &str,
) -> anyhow::Result<LoadedAcceptance> {
    authority.ensure_guard(guard)?;
    let attestation = load_acceptance_attestation(authority, limits, acceptance_hash)?;
    let loaded = verify_acceptance_attestation(&attestation, state.identity.verifying_key())?;
    ensure!(
        loaded.hash == acceptance_hash,
        "remote qualification acceptance changed identity"
    );
    Ok(loaded)
}

/// Load the acceptance named by a binding, require it to be this node's
/// current published head for its coordinate, and load its received product
/// freshly when the lane has one.
fn read_published_acceptance(
    state: &AppState,
    authority: &PinnedStateAuthority,
    guard: &CasMutationGuard,
    limits: ObjectClosureLimits,
    owner_principal: &str,
    acceptance_hash: &str,
) -> anyhow::Result<(LoadedAcceptance, Option<VerifiedProductWitness>)> {
    let loaded = load_acceptance_guarded(state, authority, guard, limits, acceptance_hash)?;
    let head = state
        .state_store
        .with_state_db(|db| {
            db.read_generic_head_ref(
                REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACE,
                &loaded.verified.coordinate_id,
            )
        })?
        .context("remote qualification acceptance has no published head")?;
    ensure!(
        head.signer == state.identity.fingerprint() && head.target_hash == acceptance_hash,
        "remote qualification acceptance is not the exact published head"
    );
    let product = match &loaded.evidence.product_source {
        Some(source) => Some(load_product_source(
            state,
            authority,
            guard,
            limits,
            owner_principal,
            &loaded.verified.evidence.product()?.product_witness_hash,
            source,
            ProductSourceVerification::Fresh,
        )?),
        None => None,
    };
    Ok((loaded, product))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_config::sections::external_execution::ExternalRuntimeContentQualificationBinding;
    use ryeos_state::Signer;
    use ryeos_state::external_content::products::qualification_testimony::{
        REMOTE_QUALIFICATION_TESTIMONY_CLAIM, REMOTE_QUALIFICATION_TESTIMONY_POLICY,
    };
    use std::sync::Arc;

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

    const ACCEPTANCE_REF: &str = "config:fixtures/acceptance";

    struct Fixture {
        attester: KeySigner,
        origin_node: KeySigner,
        receiver: KeySigner,
        testimony: Attestation,
        evidence: RemoteQualificationAcceptanceEvidence,
    }

    fn fixture() -> Fixture {
        let attester = KeySigner::new(7);
        let origin_node = KeySigner::new(9);
        let receiver = KeySigner::new(11);
        let selections = ryeos_state::external_content::products::qualification::test_support::qualified_runtime_selections(
            &"4".repeat(64),
        )
        .unwrap();
        let mut evidence = selections
            .get("auxiliary")
            .unwrap()
            .qualification
            .clone()
            .unwrap()
            .evidence;
        evidence.product_coordinate.owner_principal = format!("fp:{}", attester.fingerprint());
        evidence.verifier.result_digest = evidence.result.digest().unwrap();
        evidence.validate().unwrap();
        let qualification = evidence
            .sign_attestation(
                &origin_node,
                "2026-10-20T00:00:00Z".into(),
                Some("2026-10-30T00:00:00Z".into()),
            )
            .unwrap();
        let body = RemoteQualificationTestimonyBody::new_product(
            qualification,
            "origin-site".into(),
            ACCEPTANCE_REF.into(),
        )
        .unwrap();
        let testimony = body
            .sign(
                &attester,
                &origin_node.verifying_key(),
                "2026-10-21T00:00:00Z".into(),
                "2026-10-25T00:00:00Z".into(),
            )
            .unwrap();
        let acceptance_evidence = RemoteQualificationAcceptanceEvidence {
            schema: REMOTE_QUALIFICATION_ACCEPTANCE_EVIDENCE_SCHEMA.into(),
            lane: QualificationLane::Product,
            testimony: testimony.clone(),
            product_source: Some(ProductWitnessSource::Received {
                acceptance_hash: "a".repeat(64),
            }),
            acceptance_policy_ref: ACCEPTANCE_REF.into(),
            acceptance_policy_digest: "b".repeat(64),
            attester_verifying_key: attester.verifying_key().to_bytes(),
            origin_node_verifying_key: origin_node.verifying_key().to_bytes(),
            accepted_claims: body.asserted_claims.clone(),
            rederived: QualificationLane::Product.rederived(),
            asserted: QualificationLane::Product.asserted(),
        };
        Fixture {
            attester,
            origin_node,
            receiver,
            testimony,
            evidence: acceptance_evidence,
        }
    }

    fn acceptance(f: &Fixture) -> Attestation {
        Attestation::unsigned(
            f.testimony.subject_hash.clone(),
            REMOTE_QUALIFICATION_ACCEPTANCE_CLAIM.into(),
            REMOTE_QUALIFICATION_ACCEPTANCE_POLICY.into(),
            "2026-10-22T00:00:00Z".into(),
            f.testimony.expires_at.clone(),
            serde_json::to_value(&f.evidence).unwrap(),
        )
        .sign(&f.receiver)
        .unwrap()
    }

    #[test]
    fn acceptance_attestation_round_trips_under_the_receiver_key_only() {
        let f = fixture();
        let attestation = acceptance(&f);
        let loaded =
            verify_acceptance_attestation(&attestation, &f.receiver.verifying_key()).unwrap();
        assert_eq!(loaded.verified.testimony, f.testimony);
        assert_eq!(
            loaded.hash,
            canonical_value_digest(&attestation.to_value()).unwrap()
        );
        assert!(
            verify_acceptance_attestation(&attestation, &f.origin_node.verifying_key()).is_err()
        );
        assert!(verify_acceptance_attestation(&attestation, &f.attester.verifying_key()).is_err());
    }

    #[test]
    fn acceptance_attestation_refuses_contradicting_envelopes() {
        let f = fixture();
        let sign = |subject: String, claim: &str, policy: &str, expires: Option<String>| {
            Attestation::unsigned(
                subject,
                claim.into(),
                policy.into(),
                "2026-10-22T00:00:00Z".into(),
                expires,
                serde_json::to_value(&f.evidence).unwrap(),
            )
            .sign(&f.receiver)
            .unwrap()
        };
        let key = f.receiver.verifying_key();
        let good_subject = f.testimony.subject_hash.clone();
        let good_expiry = f.testimony.expires_at.clone();
        assert!(
            verify_acceptance_attestation(
                &sign(
                    good_subject.clone(),
                    REMOTE_QUALIFICATION_ACCEPTANCE_CLAIM,
                    REMOTE_QUALIFICATION_ACCEPTANCE_POLICY,
                    good_expiry.clone()
                ),
                &key
            )
            .is_ok()
        );
        // A testimony envelope is never an acceptance, and an acceptance with
        // a longer life or another subject contradicts its retained testimony.
        assert!(
            verify_acceptance_attestation(
                &sign(
                    good_subject.clone(),
                    REMOTE_QUALIFICATION_TESTIMONY_CLAIM,
                    REMOTE_QUALIFICATION_TESTIMONY_POLICY,
                    good_expiry.clone()
                ),
                &key
            )
            .is_err()
        );
        assert!(
            verify_acceptance_attestation(
                &sign(
                    good_subject.clone(),
                    REMOTE_QUALIFICATION_ACCEPTANCE_CLAIM,
                    REMOTE_QUALIFICATION_ACCEPTANCE_POLICY,
                    Some("2026-10-31T00:00:00Z".into())
                ),
                &key
            )
            .is_err()
        );
        assert!(
            verify_acceptance_attestation(
                &sign(
                    "0".repeat(64),
                    REMOTE_QUALIFICATION_ACCEPTANCE_CLAIM,
                    REMOTE_QUALIFICATION_ACCEPTANCE_POLICY,
                    good_expiry
                ),
                &key
            )
            .is_err()
        );
    }

    #[test]
    fn a_remote_acceptance_is_never_a_local_qualification_witness() {
        let f = fixture();
        let attestation = acceptance(&f);
        let root = tempfile::tempdir().unwrap();
        let (_owner, mut state) = crate::state::test_support::build(root.path()).unwrap();
        state.node_policy = Arc::new(
            ryeos_node_runtime::node_policy::NodePolicySnapshot::from_test_records(vec![
                Arc::new(NodeObjectClosurePolicy {
                    schema: 1,
                    max_roots: 256,
                    max_objects: 32_768,
                    max_blobs: 32_768,
                    max_object_bytes: 32 * 1024 * 1024,
                    max_total_object_bytes: 64 * 1024 * 1024,
                    max_blob_bytes: 128 * 1024 * 1024,
                    max_total_blob_bytes: 128 * 1024 * 1024,
                    max_response_bytes: 256 * 1024 * 1024,
                    max_links_per_object: 100_000,
                    local_verification: None,
                }),
                Arc::new(
                    ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::test_policy(),
                ),
            ]),
        );
        let authority = state.state_store.pinned_state_authority().unwrap();
        let limits = closure_limits(&state).unwrap();
        let stored = authority
            .cas_store()
            .unwrap()
            .put_object(&attestation.to_value())
            .unwrap();
        let acceptance_hash = stored.hash;
        let owner = format!("fp:{}", f.attester.fingerprint());
        let error = with_state_guard(&authority, |guard| {
            load_current_qualification(&state, &authority, guard, limits, &owner, &acceptance_hash)
        })
        .unwrap_err();
        assert!(!error.to_string().is_empty());
        // The same hash is not an acceptance until the receiver's own key
        // signed it and its head exists.
        let wrong_node = KeySigner::new(13);
        assert!(verify_acceptance_attestation(&attestation, &wrong_node.verifying_key()).is_err());
    }

    #[test]
    fn acceptance_refuses_an_attester_without_a_current_grant() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let (_owner, state) = crate::state::test_support::build(root.path()).unwrap();
        let owner = format!("fp:{}", f.attester.fingerprint());
        let error = accept_testimony_for_owner(
            &state,
            &owner,
            &f.origin_node.verifying_key(),
            f.testimony.clone(),
            "2026-10-22T00:00:00Z",
        )
        .unwrap_err();
        assert!(error.to_string().contains("grant was revoked"));
        // The authenticated submitter must be the issuer.
        let stranger = format!("fp:{}", "1".repeat(64));
        let error = accept_testimony_for_owner(
            &state,
            &stranger,
            &f.origin_node.verifying_key(),
            f.testimony,
            "2026-10-22T00:00:00Z",
        )
        .unwrap_err();
        assert!(error.to_string().contains("authenticated operator"));
    }

    #[test]
    fn content_launch_check_refuses_without_a_binding_selection_or_grant() {
        let root = tempfile::tempdir().unwrap();
        let (_owner, state) = crate::state::test_support::build(root.path()).unwrap();
        let owner = format!("fp:{}", "9".repeat(64));
        let context = HandlerContext::new(owner.clone(), vec![], true);
        let mut binding = ExternalRuntimeContentQualificationBinding {
            activation_ref: "config:fixtures/activation".into(),
            coordinate_id: "1".repeat(64),
            attestation_hash: "2".repeat(64),
            owner_principal: owner,
            required_claims: vec!["runtime_compatible".into()],
            remote_testimony: None,
        };
        let error = verify_current_remote_runtime_content_testimony(
            &state,
            &context,
            &binding,
            &"4".repeat(64),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no remote testimony"));
        binding.remote_testimony = Some(
            crate::node_config::sections::external_execution::ExternalRemoteQualificationTestimonyBinding {
                acceptance_hash: "3".repeat(64),
                acceptance_policy_ref: ACCEPTANCE_REF.into(),
            },
        );
        let error = verify_current_remote_runtime_content_testimony(
            &state,
            &context,
            &binding,
            &"4".repeat(64),
        )
        .unwrap_err();
        assert!(error.to_string().contains("grant was revoked"));
    }

    #[test]
    fn a_content_testimony_expires_with_the_qualification_or_the_maximum_age() {
        let fixture = fixture();
        let mut unbounded = fixture.testimony.clone();
        unbounded.expires_at = None;
        let expiry = content_testimony_expiry(&unbounded, "2026-10-21T00:00:00Z").unwrap();
        assert_eq!(expiry, "2026-11-21T00:00:00Z");
        let mut sooner = fixture.testimony;
        sooner.expires_at = Some("2026-10-25T00:00:00Z".into());
        let expiry = content_testimony_expiry(&sooner, "2026-10-21T00:00:00Z").unwrap();
        assert_eq!(expiry, "2026-10-25T00:00:00Z");
    }

    #[test]
    fn launch_check_refuses_without_a_binding_selection_or_grant() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let (_owner, state) = crate::state::test_support::build(root.path()).unwrap();
        let mut binding = ExternalRuntimeQualificationBinding {
            attestation_hash: "1".repeat(64),
            owner_principal: format!("fp:{}", f.attester.fingerprint()),
            qualification: ProductRelationshipQualification {
                policy_ref: Some("config:fixtures/qualification".into()),
                required_claims: vec!["render_snapshot_v1".into()],
            },
            remote_testimony: None,
        };
        let error =
            verify_current_remote_runtime_testimony(&state, &binding, &"4".repeat(64)).unwrap_err();
        assert!(error.to_string().contains("no remote testimony"));
        binding.remote_testimony = Some(
            crate::node_config::sections::external_execution::ExternalRemoteQualificationTestimonyBinding {
                acceptance_hash: "2".repeat(64),
                acceptance_policy_ref: ACCEPTANCE_REF.into(),
            },
        );
        let error =
            verify_current_remote_runtime_testimony(&state, &binding, &"4".repeat(64)).unwrap_err();
        assert!(error.to_string().contains("grant was revoked"));
    }
}
