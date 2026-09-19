//! Bundle-source catalog admission boundaries.
//!
//! This module does not publish a signed head itself. It produces an admitted
//! transition only after the publisher-authored catalog closure, current-head
//! fence, and current local policy have all been checked. The caller must keep
//! the closure protected until its compare-and-swap head commit is durable.

use anyhow::{Context as _, bail};
use ryeos_bundle_publication_contract::{
    BundleCatalogPublication, BundleCatalogSnapshot, BundleGeneration, BundleSet,
};
use ryeos_state::objects::Attestation;

use super::{
    PublicationObjectReader,
    attestation::{
        BUNDLE_CATALOG_RELEASE_CLAIM, BUNDLE_PUBLICATION_POLICY, ReleaseSubjectKind,
        verify_release_attestation,
    },
};
use crate::node_policy::sections::{
    bundle_publication::BundlePublicationPolicy, object_closure::NodeObjectClosurePolicy,
};

pub const BUNDLE_CATALOG_HEAD_NAMESPACE: &str = "bundle-catalog";
pub const BUNDLE_CATALOG_CHECKPOINT_NAMESPACE: &str = "bundle-catalog-checkpoint";
pub const BUNDLE_GENERATION_PUBLICATION_NAMESPACE: &str = "bundle-generation-publication";
pub const BUNDLE_SET_PUBLICATION_NAMESPACE: &str = "bundle-set-publication";

/// Complete request for the bundle source's only catalog-head mutation path.
/// The authenticated principal is retained independently from the publisher
/// fingerprint so an HTTP/service adapter cannot silently substitute policy
/// identity for transport authentication.
#[derive(Debug, Clone)]
pub struct CatalogPublicationRequest {
    pub authenticated_principal: String,
    pub catalog_namespace: String,
    pub candidate_attestation_hash: String,
    pub expected_current: Option<String>,
    /// Durable upload session that already protects the candidate closure.
    /// Publication never creates an unrelated replacement session.
    pub upload_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CatalogPublicationOutcome {
    pub catalog_namespace: String,
    pub catalog_publication_attestation_hash: String,
    pub sequence: u64,
    pub idempotent: bool,
}

#[derive(Debug, Clone)]
pub struct LocalCatalogClosureStageRequest {
    pub authenticated_principal: String,
    pub catalog_namespace: String,
    pub candidate_attestation_hash: String,
    pub expected_current: Option<String>,
}

/// Admit a complete catalog candidate that was produced in this node's CAS
/// into the same durable upload protocol used by remote publishers. This is
/// deliberately a separate authenticated step: object presence alone never
/// grants publication authority, and `publish_catalog` still re-verifies the
/// entire candidate and exact head fence.
pub fn stage_local_catalog_closure(
    state: &crate::state::AppState,
    request: LocalCatalogClosureStageRequest,
) -> anyhow::Result<String> {
    require_name(&request.catalog_namespace, "catalog namespace")?;
    require_hash(
        &request.candidate_attestation_hash,
        "candidate catalog attestation",
    )?;
    require_optional_hash(request.expected_current.as_deref(), "expected catalog head")?;
    let policy = state.node_policy.require::<BundlePublicationPolicy>()?;
    let catalog = policy.require_catalog(&request.catalog_namespace)?;
    if request.authenticated_principal != catalog.publisher_fingerprint {
        bail!("authenticated principal is not the current catalog publisher");
    }
    let publication_key = ryeos_state::DurableCasPublicationKey::bundle_catalog(
        &catalog.publisher_fingerprint,
        &request.catalog_namespace,
        request.expected_current.as_deref(),
        &policy.section_digest()?,
        state.node_policy.generation_digest(),
    )?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire catalog staging permit: {error}"))?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [request.candidate_attestation_hash.clone()],
        limits,
    )?;
    if !closure.is_complete() {
        bail!("local catalog candidate closure is incomplete");
    }
    let mut stage = authority
        .require_recovery()?
        .begin_durable_cas_upload_admitted(
            &guard,
            &request.authenticated_principal,
            "bundle-catalog-local-cas",
            &publication_key,
            request.expected_current.as_deref(),
        )?;
    stage.protect_cas_closure(
        &guard,
        closure.object_hashes.iter().map(String::as_str),
        closure.blob_hashes.iter().map(String::as_str),
    )?;
    for hash in &closure.large_object_hashes {
        stage.protect_large_object_hash(&guard, hash)?;
    }
    stage.ensure_protects_object(&request.candidate_attestation_hash)?;
    Ok(stage.staging_id().to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CatalogInspection {
    pub catalog_namespace: String,
    pub catalog_publication_attestation_hash: String,
    pub publication: BundleCatalogPublication,
    pub snapshot: BundleCatalogSnapshot,
    pub current: bool,
}

/// Read and fully re-verify one catalog publication against current policy,
/// trust, and bounded local closure availability. Supplying no hash resolves
/// the currently signed head; supplying a hash never treats it as current.
pub fn inspect_catalog(
    state: &crate::state::AppState,
    catalog_namespace: &str,
    attestation_hash: Option<&str>,
) -> anyhow::Result<CatalogInspection> {
    require_name(catalog_namespace, "catalog namespace")?;
    let publication_policy = state.node_policy.require::<BundlePublicationPolicy>()?;
    let catalog_policy = publication_policy.require_catalog(catalog_namespace)?;
    let authority = state.state_store.pinned_state_authority()?;
    let _guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let head = state.state_store.with_state_db(|db| {
        db.read_generic_head_ref(BUNDLE_CATALOG_HEAD_NAMESPACE, catalog_namespace)
    })?;
    let current_hash = head.as_ref().map(|value| value.target_hash.as_str());
    let candidate = attestation_hash
        .or(current_hash)
        .context("catalog head is absent")?;
    require_hash(candidate, "catalog publication attestation")?;
    let policy_section_digest = publication_policy.section_digest()?;
    let publisher = format!("fp:{}", catalog_policy.publisher_fingerprint);
    let binding = CatalogPolicyBinding {
        authenticated_principal: catalog_policy.publisher_fingerprint.clone(),
        publisher_fingerprint: publisher,
        catalog_namespace: catalog_namespace.to_owned(),
        policy_section_digest: policy_section_digest.clone(),
        node_policy_generation_digest: state.node_policy.generation_digest().to_owned(),
    };
    let policy = CurrentCatalogPolicyAuthority {
        policy: publication_policy,
        policy_section_digest,
        node_policy_generation_digest: state.node_policy.generation_digest(),
        trust_store: authority.trust_store(),
    };
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let availability = CasCatalogClosureAvailability { cas: &cas, limits };
    // An exact historical inspection uses the publication's own predecessor
    // fence. Current resolution uses the signed local head fence.
    let attestation = read_attestation(&cas, candidate)?;
    let publication = BundleCatalogPublication::from_current_value(&read_exact(
        &cas,
        &attestation.subject_hash,
    )?)?;
    let expected = publication.previous_publication_attestation_hash.as_deref();
    let verified = verify_catalog_publication(
        candidate,
        expected,
        Some(candidate),
        binding,
        &cas,
        &policy,
        &availability,
    )?;
    Ok(CatalogInspection {
        catalog_namespace: catalog_namespace.to_owned(),
        catalog_publication_attestation_hash: candidate.to_owned(),
        publication: verified.publication().clone(),
        snapshot: verified.snapshot().clone(),
        current: current_hash == Some(candidate),
    })
}

/// Current immutable node-policy and trust-store view used during one catalog
/// transaction. Holding the pinned state authority and policy snapshot around
/// this value prevents trust or policy from being re-read from ambient paths.
struct CurrentCatalogPolicyAuthority<'a> {
    policy: &'a BundlePublicationPolicy,
    policy_section_digest: String,
    node_policy_generation_digest: &'a str,
    trust_store: &'a ryeos_engine::trust::TrustStore,
}

struct CasCatalogClosureAvailability<'a> {
    cas: &'a lillux::CasStore,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
}

/// Local policy decision bound to the durable upload/publication session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPolicyBinding {
    pub authenticated_principal: String,
    pub publisher_fingerprint: String,
    pub catalog_namespace: String,
    pub policy_section_digest: String,
    pub node_policy_generation_digest: String,
}

/// Trust, signature, and current-policy authority. Implementations must reject
/// revoked publishers and stale policy digests; general trust-store membership
/// alone is not catalog authorization.
pub trait CatalogPolicyAuthority {
    fn verify_current_binding(&self, binding: &CatalogPolicyBinding) -> anyhow::Result<()>;

    fn verify_publisher_attestation(
        &self,
        attestation: &Attestation,
        expected_publisher: &str,
        expected_claim: &str,
        expected_policy: &str,
    ) -> anyhow::Result<()>;

    fn verify_generation_channel(
        &self,
        attestation_hash: &str,
        expected_publisher: &str,
        objects: &dyn PublicationObjectReader,
    ) -> anyhow::Result<()>;

    fn verify_set_channel(
        &self,
        attestation_hash: &str,
        expected_publisher: &str,
        objects: &dyn PublicationObjectReader,
    ) -> anyhow::Result<()>;
}

/// Availability proof owned by the bundle source. This must perform a bounded,
/// typed closure walk and prove that every referenced object/blob is locally
/// readable before the visible head can advance.
pub trait CatalogClosureAvailability {
    fn verify_complete_closure(&self, root_hash: &str) -> anyhow::Result<()>;
}

/// The only value accepted by catalog head publication orchestration.
#[derive(Debug)]
pub struct VerifiedCatalogPublication {
    attestation_hash: String,
    attestation: Attestation,
    publication: BundleCatalogPublication,
    snapshot: BundleCatalogSnapshot,
    binding: CatalogPolicyBinding,
    idempotent: bool,
}

impl VerifiedCatalogPublication {
    pub fn attestation_hash(&self) -> &str {
        &self.attestation_hash
    }

    pub fn publication(&self) -> &BundleCatalogPublication {
        &self.publication
    }

    pub fn snapshot(&self) -> &BundleCatalogSnapshot {
        &self.snapshot
    }

    pub fn binding(&self) -> &CatalogPolicyBinding {
        &self.binding
    }

    pub fn is_idempotent(&self) -> bool {
        self.idempotent
    }

    pub fn publisher_attestation(&self) -> &Attestation {
        &self.attestation
    }
}

/// Verify one fresh-genesis or exact-successor catalog transition.
///
/// `current_target` is the target of the already trust-verified local generic
/// head. `expected_current` is the publisher request's compare-and-swap fence.
/// A retry whose candidate is already current is accepted idempotently, but is
/// still rechecked for closure availability and current policy.
pub fn verify_catalog_publication(
    candidate_attestation_hash: &str,
    expected_current: Option<&str>,
    current_target: Option<&str>,
    binding: CatalogPolicyBinding,
    objects: &dyn PublicationObjectReader,
    policy: &dyn CatalogPolicyAuthority,
    availability: &dyn CatalogClosureAvailability,
) -> anyhow::Result<VerifiedCatalogPublication> {
    require_hash(candidate_attestation_hash, "candidate catalog attestation")?;
    require_optional_hash(expected_current, "expected catalog head")?;
    require_optional_hash(current_target, "current catalog head")?;
    validate_binding(&binding)?;
    policy
        .verify_current_binding(&binding)
        .context("catalog policy binding is stale or unauthorized")?;

    let attestation = read_attestation(objects, candidate_attestation_hash)
        .context("candidate catalog attestation is invalid")?;
    policy
        .verify_publisher_attestation(
            &attestation,
            &binding.publisher_fingerprint,
            BUNDLE_CATALOG_RELEASE_CLAIM,
            BUNDLE_PUBLICATION_POLICY,
        )
        .context("catalog publication attestation is unauthorized")?;
    let publication = BundleCatalogPublication::from_current_value(&read_exact(
        objects,
        &attestation.subject_hash,
    )?)
    .context("catalog publication subject is invalid")?;
    if publication.catalog_namespace != binding.catalog_namespace {
        bail!("catalog publication namespace disagrees with admitted policy binding");
    }
    let snapshot = BundleCatalogSnapshot::from_current_value(&read_exact(
        objects,
        &publication.snapshot_hash,
    )?)
    .context("catalog snapshot is invalid")?;
    if snapshot.publisher != binding.publisher_fingerprint {
        bail!("catalog snapshot publisher disagrees with admitted publisher");
    }

    for channel in &snapshot.bundle_channels {
        policy
            .verify_generation_channel(
                &channel.generation_attestation_hash,
                &binding.publisher_fingerprint,
                objects,
            )
            .with_context(|| {
                format!(
                    "catalog generation channel {}/{} is unauthorized",
                    channel.bundle_name, channel.channel
                )
            })?;
    }
    for channel in &snapshot.set_channels {
        policy
            .verify_set_channel(
                &channel.set_attestation_hash,
                &binding.publisher_fingerprint,
                objects,
            )
            .with_context(|| {
                format!(
                    "catalog set channel {}/{} is unauthorized",
                    channel.set_name, channel.channel
                )
            })?;
    }

    availability
        .verify_complete_closure(candidate_attestation_hash)
        .context("catalog publication closure is incomplete")?;

    let idempotent = current_target == Some(candidate_attestation_hash);
    if idempotent {
        if candidate_predecessor(&publication) != expected_current {
            bail!("idempotent catalog retry changed its original predecessor fence");
        }
    } else {
        if expected_current != current_target {
            bail!("catalog head changed since publication admission");
        }
        verify_transition_predecessor(
            &publication,
            current_target,
            objects,
            policy,
            availability,
            &binding,
        )?;
    }

    Ok(VerifiedCatalogPublication {
        attestation_hash: candidate_attestation_hash.to_owned(),
        attestation,
        publication,
        snapshot,
        binding,
        idempotent,
    })
}

impl CatalogPolicyAuthority for CurrentCatalogPolicyAuthority<'_> {
    fn verify_current_binding(&self, binding: &CatalogPolicyBinding) -> anyhow::Result<()> {
        self.policy.validate()?;
        let catalog = self.policy.require_catalog(&binding.catalog_namespace)?;
        if catalog.frozen {
            bail!("catalog namespace is frozen by current node policy");
        }
        if binding.policy_section_digest != self.policy_section_digest
            || binding.node_policy_generation_digest != self.node_policy_generation_digest
        {
            bail!("catalog binding does not name the exact live node-policy generation");
        }
        let expected_publisher = format!("fp:{}", catalog.publisher_fingerprint);
        if binding.publisher_fingerprint != expected_publisher
            || binding.authenticated_principal != catalog.publisher_fingerprint
        {
            bail!("catalog binding publisher is not authorized by current node policy");
        }
        self.publisher_key(&catalog.publisher_fingerprint)?;
        Ok(())
    }

    fn verify_publisher_attestation(
        &self,
        attestation: &Attestation,
        expected_publisher: &str,
        expected_claim: &str,
        expected_policy: &str,
    ) -> anyhow::Result<()> {
        let fingerprint = expected_publisher
            .strip_prefix("fp:")
            .context("expected catalog publisher is not a fingerprint principal")?;
        let key = self.publisher_key(fingerprint)?;
        attestation.verify_with_key(key)?;
        if attestation.issuer_fingerprint()? != fingerprint
            || attestation.claim != expected_claim
            || attestation.policy != expected_policy
            || attestation.expires_at.is_some()
        {
            bail!("publisher attestation does not satisfy the closed catalog policy");
        }
        Ok(())
    }

    fn verify_generation_channel(
        &self,
        attestation_hash: &str,
        expected_publisher: &str,
        objects: &dyn PublicationObjectReader,
    ) -> anyhow::Result<()> {
        self.verify_release_subject(
            attestation_hash,
            expected_publisher,
            ReleaseSubjectKind::BundleGeneration,
            objects,
            |value| BundleGeneration::from_current_value(value).map(|_| ()),
        )
    }

    fn verify_set_channel(
        &self,
        attestation_hash: &str,
        expected_publisher: &str,
        objects: &dyn PublicationObjectReader,
    ) -> anyhow::Result<()> {
        self.verify_release_subject(
            attestation_hash,
            expected_publisher,
            ReleaseSubjectKind::BundleSet,
            objects,
            |value| BundleSet::from_current_value(value).map(|_| ()),
        )
    }
}

impl CurrentCatalogPolicyAuthority<'_> {
    fn publisher_key(&self, fingerprint: &str) -> anyhow::Result<&lillux::crypto::VerifyingKey> {
        self.trust_store
            .get(fingerprint)
            .map(|signer| &signer.verifying_key)
            .with_context(|| format!("catalog publisher {fingerprint} is not currently trusted"))
    }

    fn verify_release_subject(
        &self,
        attestation_hash: &str,
        expected_publisher: &str,
        kind: ReleaseSubjectKind,
        objects: &dyn PublicationObjectReader,
        decode_subject: impl FnOnce(&serde_json::Value) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let attestation = read_attestation(objects, attestation_hash)?;
        let fingerprint = expected_publisher
            .strip_prefix("fp:")
            .context("expected release publisher is not a fingerprint principal")?;
        verify_release_attestation(
            &attestation,
            &attestation.subject_hash,
            kind,
            fingerprint,
            self.publisher_key(fingerprint)?,
        )?;
        decode_subject(&read_exact(objects, &attestation.subject_hash)?)
            .context("release attestation subject has the wrong current object kind")
    }
}

impl CatalogClosureAvailability for CasCatalogClosureAvailability<'_> {
    fn verify_complete_closure(&self, root_hash: &str) -> anyhow::Result<()> {
        let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
            self.cas,
            [root_hash.to_owned()],
            self.limits,
        )?;
        if !closure.is_complete() {
            bail!("catalog closure traversal found unavailable content");
        }
        Ok(())
    }
}

/// Verify and publish one catalog transition while its complete closure is
/// protected by both the global CAS guard and a durable, principal-bound
/// publication session. Immutable generation/set attestations receive their
/// own roots before the visible catalog head advances, so replacing a channel
/// does not silently collect an older release coordinate.
pub fn publish_catalog(
    state: &crate::state::AppState,
    request: CatalogPublicationRequest,
) -> anyhow::Result<CatalogPublicationOutcome> {
    require_name(&request.catalog_namespace, "catalog namespace")?;
    require_hash(
        &request.candidate_attestation_hash,
        "candidate catalog attestation",
    )?;
    require_optional_hash(request.expected_current.as_deref(), "expected catalog head")?;

    let publication_policy = state.node_policy.require::<BundlePublicationPolicy>()?;
    let catalog_policy = publication_policy.require_catalog(&request.catalog_namespace)?;
    let publisher_principal = format!("fp:{}", catalog_policy.publisher_fingerprint);
    if request.authenticated_principal != catalog_policy.publisher_fingerprint {
        bail!("authenticated principal is not the current catalog publisher");
    }
    let policy_section_digest = publication_policy.section_digest()?;
    let binding = CatalogPolicyBinding {
        authenticated_principal: request.authenticated_principal.clone(),
        publisher_fingerprint: publisher_principal,
        catalog_namespace: request.catalog_namespace.clone(),
        policy_section_digest: policy_section_digest.clone(),
        node_policy_generation_digest: state.node_policy.generation_digest().to_owned(),
    };

    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire catalog publication permit: {error}"))?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let closure_limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let availability = CasCatalogClosureAvailability {
        cas: &cas,
        limits: closure_limits,
    };
    let current = state.state_store.with_state_db(|db| {
        db.read_generic_head_ref(BUNDLE_CATALOG_HEAD_NAMESPACE, &request.catalog_namespace)
    })?;
    let current_target = current.as_ref().map(|head| head.target_hash.as_str());
    let policy = CurrentCatalogPolicyAuthority {
        policy: publication_policy,
        policy_section_digest,
        node_policy_generation_digest: state.node_policy.generation_digest(),
        trust_store: authority.trust_store(),
    };
    let verified = verify_catalog_publication(
        &request.candidate_attestation_hash,
        request.expected_current.as_deref(),
        current_target,
        binding,
        &cas,
        &policy,
        &availability,
    )?;

    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [request.candidate_attestation_hash.clone()],
        closure_limits,
    )?;
    if !closure.is_complete() {
        bail!("catalog closure became unavailable before durable admission");
    }
    let publication_key = ryeos_state::DurableCasPublicationKey::bundle_catalog(
        &catalog_policy.publisher_fingerprint,
        &request.catalog_namespace,
        request.expected_current.as_deref(),
        &verified.binding().policy_section_digest,
        &verified.binding().node_policy_generation_digest,
    )?;
    let mut stage = authority
        .require_recovery()?
        .open_durable_cas_upload_admitted(
            &guard,
            &request.upload_session_id,
            &request.authenticated_principal,
        )?;
    stage.ensure_publication_contract(&publication_key, request.expected_current.as_deref())?;
    if let Some(target) = stage.admitted_target_hash() {
        if target != request.candidate_attestation_hash {
            bail!("catalog upload session is already admitted for another publication target");
        }
    }
    stage.protect_cas_closure(
        &guard,
        closure.object_hashes.iter().map(String::as_str),
        closure.blob_hashes.iter().map(String::as_str),
    )?;
    for hash in &closure.large_object_hashes {
        stage.protect_large_object_hash(&guard, hash)?;
    }
    stage.ensure_protects_object(&request.candidate_attestation_hash)?;

    let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    root_release_coordinates(state, &guard, &signer, verified.snapshot())?;
    if !verified.is_idempotent() {
        state.state_store.with_state_db(|db| {
            db.advance_generic_head_ref(
                BUNDLE_CATALOG_HEAD_NAMESPACE,
                &request.catalog_namespace,
                &request.candidate_attestation_hash,
                request.expected_current.as_deref(),
                &signer,
                &guard,
            )
        })?;
    }
    checkpoint_catalog_head(
        state,
        &guard,
        &signer,
        &request.catalog_namespace,
        &request.candidate_attestation_hash,
    );
    if let Err(error) = stage.finish_admitted(&guard, &request.candidate_attestation_hash) {
        tracing::warn!(
            %error,
            staging_id = %stage.staging_id(),
            "catalog head published while its durable publication receipt remained retryable"
        );
    }
    Ok(CatalogPublicationOutcome {
        catalog_namespace: request.catalog_namespace,
        catalog_publication_attestation_hash: request.candidate_attestation_hash,
        sequence: verified.publication().sequence,
        idempotent: verified.is_idempotent(),
    })
}

fn root_release_coordinates(
    state: &crate::state::AppState,
    guard: &ryeos_state::CasMutationGuard,
    signer: &dyn ryeos_state::signer::Signer,
    snapshot: &BundleCatalogSnapshot,
) -> anyhow::Result<()> {
    for channel in &snapshot.bundle_channels {
        ensure_immutable_root(
            state,
            guard,
            signer,
            BUNDLE_GENERATION_PUBLICATION_NAMESPACE,
            &channel.generation_attestation_hash,
        )?;
    }
    for channel in &snapshot.set_channels {
        ensure_immutable_root(
            state,
            guard,
            signer,
            BUNDLE_SET_PUBLICATION_NAMESPACE,
            &channel.set_attestation_hash,
        )?;
    }
    Ok(())
}

fn ensure_immutable_root(
    state: &crate::state::AppState,
    guard: &ryeos_state::CasMutationGuard,
    signer: &dyn ryeos_state::signer::Signer,
    namespace: &str,
    attestation_hash: &str,
) -> anyhow::Result<()> {
    state.state_store.with_state_db(|db| {
        let current = db.read_generic_head_ref(namespace, attestation_hash)?;
        match current {
            Some(current) if current.target_hash == attestation_hash => Ok(()),
            Some(_) => bail!("immutable release-coordinate root points to another object"),
            None => db.advance_generic_head_ref(
                namespace,
                attestation_hash,
                attestation_hash,
                None,
                signer,
                guard,
            ),
        }
    })
}

fn checkpoint_catalog_head(
    state: &crate::state::AppState,
    guard: &ryeos_state::CasMutationGuard,
    signer: &dyn ryeos_state::signer::Signer,
    catalog_namespace: &str,
    target: &str,
) {
    let result = state.state_store.with_state_db(|db| {
        let checkpoint =
            db.read_generic_head_ref(BUNDLE_CATALOG_CHECKPOINT_NAMESPACE, catalog_namespace)?;
        if checkpoint
            .as_ref()
            .is_some_and(|head| head.target_hash == target)
        {
            return Ok(());
        }
        db.advance_generic_head_ref(
            BUNDLE_CATALOG_CHECKPOINT_NAMESPACE,
            catalog_namespace,
            target,
            checkpoint.as_ref().map(|head| head.target_hash.as_str()),
            signer,
            guard,
        )
    });
    if let Err(error) = result {
        // The catalog head is already durable and authoritative at this point.
        // A failed local observation checkpoint must be repaired, but turning
        // this into a request failure would invite an ambiguous republish.
        tracing::warn!(
            %error,
            %catalog_namespace,
            %target,
            "catalog published but local observation checkpoint did not advance"
        );
    }
}

fn candidate_predecessor(publication: &BundleCatalogPublication) -> Option<&str> {
    publication.previous_publication_attestation_hash.as_deref()
}

fn verify_transition_predecessor(
    candidate: &BundleCatalogPublication,
    current_target: Option<&str>,
    objects: &dyn PublicationObjectReader,
    policy: &dyn CatalogPolicyAuthority,
    availability: &dyn CatalogClosureAvailability,
    binding: &CatalogPolicyBinding,
) -> anyhow::Result<()> {
    match current_target {
        None => {
            if candidate.previous_publication_attestation_hash.is_some() || candidate.sequence != 0
            {
                bail!("an absent catalog accepts only an authorized sequence-zero genesis");
            }
        }
        Some(current_hash) => {
            if candidate.previous_publication_attestation_hash.as_deref() != Some(current_hash) {
                bail!("catalog successor does not name the exact current publication");
            }
            let current_attestation = read_attestation(objects, current_hash)
                .context("current catalog attestation is invalid")?;
            availability
                .verify_complete_closure(current_hash)
                .context("current catalog head closure is incomplete; repair is required")?;
            policy.verify_publisher_attestation(
                &current_attestation,
                &binding.publisher_fingerprint,
                BUNDLE_CATALOG_RELEASE_CLAIM,
                BUNDLE_PUBLICATION_POLICY,
            )?;
            let current = BundleCatalogPublication::from_current_value(&read_exact(
                objects,
                &current_attestation.subject_hash,
            )?)
            .context("current catalog publication subject is invalid")?;
            if current.catalog_namespace != candidate.catalog_namespace {
                bail!("catalog successor changed namespace");
            }
            let current_snapshot = BundleCatalogSnapshot::from_current_value(&read_exact(
                objects,
                &current.snapshot_hash,
            )?)
            .context("current catalog snapshot is invalid")?;
            if current_snapshot.publisher != binding.publisher_fingerprint {
                bail!("current catalog snapshot belongs to a different publisher");
            }
            let expected_sequence = current
                .sequence
                .checked_add(1)
                .context("catalog sequence is exhausted")?;
            if candidate.sequence != expected_sequence {
                bail!("catalog successor sequence is not exactly current plus one");
            }
        }
    }
    Ok(())
}

fn read_attestation(
    objects: &dyn PublicationObjectReader,
    hash: &str,
) -> anyhow::Result<Attestation> {
    Attestation::from_value(&read_exact(objects, hash)?)
}

fn read_exact(
    objects: &dyn PublicationObjectReader,
    hash: &str,
) -> anyhow::Result<serde_json::Value> {
    let value = objects
        .get_object(hash)?
        .with_context(|| format!("catalog object {hash} is absent"))?;
    let actual = lillux::cas::sha256_hex(lillux::canonical_json(&value)?.as_bytes());
    if actual != hash {
        bail!("catalog object content disagrees with requested CAS identity");
    }
    Ok(value)
}

fn validate_binding(binding: &CatalogPolicyBinding) -> anyhow::Result<()> {
    let publisher = binding
        .publisher_fingerprint
        .strip_prefix("fp:")
        .context("catalog publisher must be a fingerprint principal")?;
    require_hash(publisher, "catalog publisher fingerprint")?;
    if binding.authenticated_principal != publisher {
        bail!("authenticated catalog publisher disagrees with policy publisher");
    }
    require_name(&binding.catalog_namespace, "catalog namespace")?;
    require_hash(
        &binding.policy_section_digest,
        "catalog policy-section digest",
    )?;
    require_hash(
        &binding.node_policy_generation_digest,
        "catalog node-policy generation digest",
    )
}

fn require_optional_hash(value: Option<&str>, label: &str) -> anyhow::Result<()> {
    if let Some(value) = value {
        require_hash(value, label)?;
    }
    Ok(())
}

fn require_hash(value: &str, label: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be a lowercase 64-hex digest");
    }
    Ok(())
}

fn require_name(value: &str, label: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        bail!("{label} must be a bounded lowercase identifier");
    }
    Ok(())
}
