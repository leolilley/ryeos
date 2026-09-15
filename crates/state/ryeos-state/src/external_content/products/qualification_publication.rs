//! Immutable node testimony for an independently completed product verifier.
//!
//! The app proves the actual terminal launch and current policy before calling
//! this owner. Historical verifier/policy coordinates are signed testimony,
//! not owning CAS edges. The existing attestation subject owns product bytes.
//! Lookup authenticates exact heads, but does not imply current policy or
//! expiry eligibility; consumers must check those explicitly.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

use super::publication::{
    ProductWitnessLookup, load_product_attestation_value, lookup_product_witness_guarded,
    product_attestation_byte_limit, publish_immutable_product_attestation,
};
use super::qualification::ProductQualificationEvidence;
use crate::object_closure::ObjectClosureLimits;
use crate::objects::{Attestation, canonical_value_digest};
use crate::{CasMutationGuard, PinnedStateAuthority, Signer};

pub const PRODUCT_QUALIFICATION_HEAD_NAMESPACE: &str = "retained-product-qualifications";
const QUALIFICATION_COORDINATE_DOMAIN: &str = "ryeos.retained_product_qualification.coordinate.v1";

/// Exact independently verified attempt; issue times never create new keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationCoordinate {
    pub owner_principal: String,
    pub product_witness_hash: String,
    pub policy_source_digest: String,
    pub verifier_chain_root_id: String,
    pub verifier_thread_id: String,
    pub verifier_capsule_hash: String,
    pub verifier_terminal_snapshot_hash: String,
}

impl QualificationCoordinate {
    pub fn validate(&self) -> anyhow::Result<()> {
        let owner = self
            .owner_principal
            .strip_prefix("fp:")
            .context("qualification owner must be an exact fingerprint principal")?;
        for (label, hash) in [
            ("qualification owner", owner),
            (
                "qualified product witness",
                self.product_witness_hash.as_str(),
            ),
            (
                "qualification policy source",
                self.policy_source_digest.as_str(),
            ),
            (
                "qualification verifier capsule",
                self.verifier_capsule_hash.as_str(),
            ),
            (
                "qualification verifier terminal",
                self.verifier_terminal_snapshot_hash.as_str(),
            ),
        ] {
            super::validate_hash(label, hash)?;
        }
        for value in [&self.verifier_chain_root_id, &self.verifier_thread_id] {
            if value.is_empty()
                || value.len() > 2048
                || value.trim() != value
                || value.chars().any(char::is_control)
            {
                bail!("qualification verifier coordinate is not exact and bounded");
            }
        }
        Ok(())
    }

    pub fn from_evidence(evidence: &ProductQualificationEvidence) -> anyhow::Result<Self> {
        evidence.validate()?;
        let coordinate = Self {
            owner_principal: evidence.product_coordinate.owner_principal.clone(),
            product_witness_hash: evidence.product_witness_hash.clone(),
            policy_source_digest: canonical_value_digest(&serde_json::to_value(
                &evidence.policy_source,
            )?)?,
            verifier_chain_root_id: evidence.verifier.chain_root_id.clone(),
            verifier_thread_id: evidence.verifier.thread_id.clone(),
            verifier_capsule_hash: evidence.verifier.admitted_launch_capsule_hash.clone(),
            verifier_terminal_snapshot_hash: evidence.verifier.terminal_snapshot_hash.clone(),
        };
        coordinate.validate()?;
        Ok(coordinate)
    }

    pub fn coordinate_id(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::json!({
            "domain": QUALIFICATION_COORDINATE_DOMAIN,
            "coordinate": self,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedQualificationWitness {
    pub coordinate_id: String,
    pub attestation_hash: String,
    pub attestation: Attestation,
    pub evidence: ProductQualificationEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualificationWitnessLookup {
    Missing,
    Found(VerifiedQualificationWitness),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualificationWitnessPublication {
    pub witness: VerifiedQualificationWitness,
    pub reused_existing: bool,
}

pub fn publish_qualification_witness(
    authority: &PinnedStateAuthority,
    coordinate: &QualificationCoordinate,
    attestation: &Attestation,
    limits: ObjectClosureLimits,
    signer: &dyn Signer,
    guard: &CasMutationGuard,
) -> anyhow::Result<QualificationWitnessPublication> {
    coordinate.validate()?;
    let (witness, reused_existing) = publish_immutable_product_attestation(
        authority,
        PRODUCT_QUALIFICATION_HEAD_NAMESPACE,
        &coordinate.coordinate_id()?,
        attestation,
        limits,
        signer,
        guard,
        |attestation| {
            verify_attestation(
                authority,
                coordinate,
                attestation,
                &signer.verifying_key(),
                limits,
                guard,
            )
        },
    )?;
    Ok(QualificationWitnessPublication {
        witness,
        reused_existing,
    })
}

pub fn lookup_qualification_witness(
    authority: &PinnedStateAuthority,
    coordinate: &QualificationCoordinate,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
) -> anyhow::Result<QualificationWitnessLookup> {
    let guard = authority.acquire_shared_guard()?;
    lookup_qualification_witness_guarded(authority, coordinate, node_key, limits, &guard)
}

pub fn lookup_qualification_witness_guarded(
    authority: &PinnedStateAuthority,
    coordinate: &QualificationCoordinate,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<QualificationWitnessLookup> {
    coordinate.validate()?;
    authority.ensure_guard(guard)?;
    let Some(hash) = current_head(authority, coordinate, node_key)? else {
        return Ok(QualificationWitnessLookup::Missing);
    };
    let witness = load_witness(authority, coordinate, &hash, node_key, limits, guard)?
        .context("qualification head target is missing")?;
    Ok(QualificationWitnessLookup::Found(witness))
}

/// Exact-hash lookup still requires the owning immutable head. An uncommitted
/// CAS object is not a published qualification, even if its signature is valid.
pub fn lookup_qualification_witness_hash_guarded(
    authority: &PinnedStateAuthority,
    coordinate: &QualificationCoordinate,
    attestation_hash: &str,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<QualificationWitnessLookup> {
    coordinate.validate()?;
    super::validate_hash("qualification witness", attestation_hash)?;
    authority.ensure_guard(guard)?;
    let Some(witness) = load_witness(
        authority,
        coordinate,
        attestation_hash,
        node_key,
        limits,
        guard,
    )?
    else {
        return Ok(QualificationWitnessLookup::Missing);
    };
    if current_head(authority, coordinate, node_key)?.as_deref() != Some(attestation_hash) {
        bail!("qualification witness is not the exact published head");
    }
    Ok(QualificationWitnessLookup::Found(witness))
}

fn current_head(
    authority: &PinnedStateAuthority,
    coordinate: &QualificationCoordinate,
    node_key: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<Option<String>> {
    let Some(head) = crate::refs::read_verified_generic_head_ref_in_directory(
        authority.refs_directory(),
        PRODUCT_QUALIFICATION_HEAD_NAMESPACE,
        &coordinate.coordinate_id()?,
        authority.trust_store(),
    )?
    else {
        return Ok(None);
    };
    if head.signer != lillux::crypto::fingerprint(node_key) {
        bail!("qualification head is not signed by the requested node");
    }
    Ok(Some(head.target_hash))
}

fn load_witness(
    authority: &PinnedStateAuthority,
    coordinate: &QualificationCoordinate,
    hash: &str,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<Option<VerifiedQualificationWitness>> {
    let Some(value) = load_product_attestation_value(authority, hash, limits, guard)? else {
        return Ok(None);
    };
    let attestation = Attestation::from_value(&value)?;
    let witness = verify_attestation(authority, coordinate, &attestation, node_key, limits, guard)?;
    if witness.attestation_hash != hash {
        bail!("qualification CAS identity changed");
    }
    Ok(Some(witness))
}

fn verify_attestation(
    authority: &PinnedStateAuthority,
    coordinate: &QualificationCoordinate,
    attestation: &Attestation,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<VerifiedQualificationWitness> {
    let canonical = lillux::canonical_json(&attestation.to_value())?;
    if u64::try_from(canonical.len()).unwrap_or(u64::MAX) > product_attestation_byte_limit(limits) {
        bail!("qualification attestation exceeds its bounded wire contract");
    }
    let evidence = ProductQualificationEvidence::verify_attestation_for_owner(
        attestation,
        node_key,
        &coordinate.owner_principal,
    )?;
    verify_retained_verifier_realization(authority, &evidence, limits, guard)?;
    if QualificationCoordinate::from_evidence(&evidence)? != *coordinate {
        bail!("qualification testimony contradicts its exact request coordinate");
    }
    let product = match &evidence.witness_source {
        super::transfer::ProductWitnessSource::LocalCapture {} => {
            let ProductWitnessLookup::Found(product) = lookup_product_witness_guarded(
                authority,
                &evidence.product_coordinate,
                node_key,
                limits,
                guard,
            )?
            else {
                bail!("qualified product has no published capture witness");
            };
            product
        }
        super::transfer::ProductWitnessSource::Received { acceptance_hash } => {
            // Exact retained local acceptance authenticates the origin key and
            // same-operator witness. It is not a transferred qualification or
            // a current import-policy/head grant; the app owns fresh eligibility.
            let value = crate::object_closure::load_exact_cas_object_with_cas(
                &authority.cas_store()?,
                acceptance_hash,
                limits
                    .max_object_bytes
                    .min(super::transfer::MAX_PRODUCT_ADMISSION_ATTESTATION_BYTES),
            )?;
            let acceptance = Attestation::from_value(&value)?;
            let received = super::transfer::verify_received_product_guarded(
                authority,
                guard,
                limits,
                &acceptance,
                node_key,
                &coordinate.owner_principal,
            )?;
            if received.acceptance_hash != *acceptance_hash {
                bail!("qualification receipt contradicts its exact acceptance hash");
            }
            received.admitted_product.product
        }
    };
    if product.attestation_hash != evidence.product_witness_hash
        || super::publication::ProductCaptureCoordinate::from_evidence(&product.evidence)?
            != evidence.product_coordinate
        || product.evidence.manifest_hash != attestation.subject_hash
    {
        bail!("qualification subject contradicts the exact published product witness");
    }
    Ok(VerifiedQualificationWitness {
        coordinate_id: coordinate.coordinate_id()?,
        attestation_hash: lillux::sha256_hex(canonical.as_bytes()),
        attestation: attestation.clone(),
        evidence,
    })
}

/// Authenticate the immutable execution material named by signed verifier
/// testimony. This is also the recovery path: it requires no historical
/// capsule, current witness head, policy lookup, or wall-clock eligibility.
pub fn verify_retained_verifier_realization(
    authority: &PinnedStateAuthority,
    evidence: &ProductQualificationEvidence,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<()> {
    use crate::object_closure::{
        collect_object_closure_with_cas_and_limits, load_exact_cas_object_with_cas,
    };
    use crate::objects::{
        AdmittedExecutionRealization, EXECUTION_IDENTITY_ATTESTATION_CLAIM,
        EXECUTION_IDENTITY_ATTESTATION_POLICY, ExecutionIdentity, MAX_EXECUTION_IDENTITY_BYTES,
        MAX_EXECUTION_REALIZATION_BYTES,
    };
    authority.ensure_guard(guard)?;
    evidence.validate()?;
    let cas = authority.cas_store()?;
    // One aggregate closure budget covers root and probe together. Per-node
    // traversal would both repeat shared work and multiply the caller's bound.
    let closure = collect_object_closure_with_cas_and_limits(
        &cas,
        evidence
            .execution_verifiers()
            .map(|verifier| verifier.execution_realization_hash.clone()),
        limits,
    )?;
    if !closure.is_complete() {
        bail!("qualification verifier execution realization closure is incomplete");
    }
    for verifier in evidence.execution_verifiers() {
        let value = load_exact_cas_object_with_cas(
            &cas,
            &verifier.execution_realization_hash,
            limits
                .max_object_bytes
                .min(MAX_EXECUTION_REALIZATION_BYTES as u64),
        )?;
        let realization = AdmittedExecutionRealization::from_current_value(&value)?;
        if realization.content_hash()? != verifier.execution_realization_hash
            || realization.artifact_identity_digest
                != canonical_value_digest(&serde_json::to_value(&verifier.artifact_identity)?)?
            || realization.effective_definition_digest != verifier.effective_definition_digest
            || realization.launch_authority_digest != verifier.launch_authority_digest
            || realization.substrate_identity_hash != verifier.substrate_identity_hash
        {
            bail!(
                "qualification verifier testimony contradicts its retained execution realization"
            );
        }
        realization.verify_retained_components(&cas, &authority.large_object_store()?)?;
        let identity = ExecutionIdentity::from_current_value(&load_exact_cas_object_with_cas(
            &cas,
            &realization.substrate_identity_hash,
            limits
                .max_object_bytes
                .min(MAX_EXECUTION_IDENTITY_BYTES as u64),
        )?)?;
        let attestation = Attestation::from_value(&load_exact_cas_object_with_cas(
            &cas,
            &realization.substrate_attestation_hash,
            limits.max_object_bytes,
        )?)?;
        if attestation.subject_hash != realization.substrate_identity_hash
            || attestation.claim != EXECUTION_IDENTITY_ATTESTATION_CLAIM
            || attestation.policy != EXECUTION_IDENTITY_ATTESTATION_POLICY
            || attestation.issuer_fingerprint()? != identity.node_signer_fingerprint
        {
            bail!(
                "qualification verifier substrate attestation contradicts its execution identity"
            );
        }
        attestation.verify_with_trust_store(authority.trust_store())?;
    }
    authority.ensure_guard(guard)
}

#[cfg(test)]
mod tests;
