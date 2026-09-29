//! Historical byte verification for a materialization source CAS closure.
//!
//! This is not an authority entrypoint. Its caller must first authenticate
//! the node-signed materialization attestation and exact source coordinate;
//! historical publisher keys here cannot grant fresh work.

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use ryeos_external_execution::guest_runtime_product::{
    GuestOwnerRuntimeManifestIdentity, derive_guest_owner_runtime_manifest_identity,
};
use ryeos_state::objects::GuestRuntimeMaterializationSourceEvidence;
use ryeos_state::objects::external_content_manifest::ExternalContentManifestObject;
use ryeos_state::objects::{
    Attestation, GuestRuntimeMaterializationSubject, MaterializationSignerKey,
};
use serde::{Deserialize, Serialize};

use super::{
    GuestOwnerMaterializationSource, ItemSourceRoot, OWNER_NAME, verify_retained_executor_source,
    verify_retained_signed_bundle_manifest, verify_retained_signed_recipe_item,
};

const MAX_SOURCE_OBJECT_BYTES: u64 = 256 * 1024;
const MAX_SIGNED_RECIPE_BYTES: u64 = 256 * 1024;
const MAX_SIGNED_BUNDLE_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_EXECUTOR_MANIFEST_OBJECT_BYTES: u64 = 1024 * 1024;
const MAX_EXECUTOR_ITEM_SOURCE_OBJECT_BYTES: u64 = 64 * 1024;
const MAX_EXECUTOR_SIDECAR_BYTES: u64 = 1024 * 1024;
const MAX_OUTPUT_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_PROFILE_BYTES: u64 = 4 * 1024;
const MAX_SUBJECT_OBJECT_BYTES: u64 = 16 * 1024;
pub(super) const CLAIM: &str = "guest_owner_runtime_materialized";
pub(super) const POLICY: &str = "ryeos.guest-owner-materialization.checked-bundle-generation.v1";

/// The node's checked-generation decision, not a grant created by retained
/// historical keys. The publishing path must construct this only after its
/// checked Bundle-generation and operator-admission checks succeed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MaterializationClaimEvidence {
    pub schema: u32,
    pub kind: String,
    pub source: GuestOwnerMaterializationSource,
    pub checked_trusted_publishers: Vec<String>,
}

impl MaterializationClaimEvidence {
    pub fn from_checked_source(
        source: &GuestOwnerMaterializationSource,
        signer_keys: &[MaterializationSignerKey],
    ) -> Result<Self> {
        source.coordinate_digest()?;
        let evidence = Self {
            schema: 1,
            kind: "guest_runtime_materialization_claim".to_owned(),
            source: source.clone(),
            checked_trusted_publishers: signer_keys
                .iter()
                .map(|key| key.signer_fingerprint.clone())
                .collect(),
        };
        evidence.validate()?;
        Ok(evidence)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.kind == "guest_runtime_materialization_claim",
            "materialization claim schema/kind is not current"
        );
        self.source.coordinate_digest()?;
        ensure!(
            !self.checked_trusted_publishers.is_empty()
                && self.checked_trusted_publishers.len() <= 16,
            "materialization checked publisher set is invalid"
        );
        let mut previous: Option<&str> = None;
        for signer in &self.checked_trusted_publishers {
            ensure!(
                lillux::valid_hash(signer)
                    && !previous.is_some_and(|value| value >= signer.as_str()),
                "materialization checked publishers are not canonical and sorted"
            );
            previous = Some(signer);
        }
        Ok(())
    }
}

pub(super) struct VerifiedMaterializationTestimony {
    pub subject: GuestRuntimeMaterializationSubject,
    pub source: GuestOwnerMaterializationSource,
    pub output: GuestOwnerRuntimeManifestIdentity,
}

/// Authenticate the node's historical checked-generation claim *before*
/// deriving the source coordinate and walking either retained CAS closure.
/// Current policy/admission must be checked separately for fresh operations.
pub(super) fn verify_testimony(
    cas: &lillux::CasStore,
    attestation: &Attestation,
    node_key: &lillux::crypto::VerifyingKey,
) -> Result<VerifiedMaterializationTestimony> {
    ensure!(
        lillux::canonical_json(&attestation.to_value())?.len() as u64 <= 64 * 1024,
        "materialization node testimony exceeds its bounded wire contract"
    );
    attestation.verify_with_key(node_key)?;
    ensure!(
        attestation.claim == CLAIM
            && attestation.policy == POLICY
            && attestation.expires_at.is_none(),
        "materialization node testimony has wrong claim or policy"
    );
    let evidence: MaterializationClaimEvidence =
        serde_json::from_value(attestation.evidence.clone())?;
    evidence.validate()?;
    let source = evidence.source;
    let node_fingerprint = lillux::crypto::fingerprint(node_key);
    ensure!(
        source.node_site_id == format!("site:{node_fingerprint}")
            && source.controller_public_root
                == format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD.encode(node_key.to_bytes())
                ),
        "materialization node testimony differs from controller identity"
    );
    let subject_value =
        load_object_bounded(cas, &attestation.subject_hash, MAX_SUBJECT_OBJECT_BYTES)?;
    let subject = GuestRuntimeMaterializationSubject::from_value(&subject_value)?;
    ensure!(
        subject.coordinate_digest == source.coordinate_digest()?
            && subject.runtime_manifest_hash == source.runtime_manifest_hash,
        "materialization subject differs from node-signed source"
    );
    let verified_source = verify_source_closure(cas, &source, &subject.source_evidence_hash)?;
    ensure!(
        evidence.checked_trusted_publishers
            == verified_source
                .evidence
                .signer_keys
                .iter()
                .map(|key| key.signer_fingerprint.clone())
                .collect::<Vec<_>>(),
        "materialization checked publishers differ from retained signed sources"
    );
    let output = verify_output_closure(cas, &source, &verified_source)?;
    Ok(VerifiedMaterializationTestimony {
        subject,
        source,
        output,
    })
}

pub(super) struct VerifiedGuestOwnerSourceClosure {
    pub evidence: GuestRuntimeMaterializationSourceEvidence,
    pub payload_size: u64,
}

/// Load only the node-attested historical source closure. No installed Bundle
/// path or current trust store participates in this read.
pub(super) fn verify_source_closure(
    cas: &lillux::CasStore,
    source: &GuestOwnerMaterializationSource,
    evidence_hash: &str,
) -> Result<VerifiedGuestOwnerSourceClosure> {
    source.coordinate_digest()?;
    let value = load_object_bounded(cas, evidence_hash, MAX_SOURCE_OBJECT_BYTES)?;
    let evidence = GuestRuntimeMaterializationSourceEvidence::from_value(&value)?;
    ensure!(
        evidence.signed_recipe_items.iter().any(|item| {
            item.resolved_ref == source.recipe_ref
                && item.bundle_name == source.bundle_name
                && item.signer_fingerprint == source.recipe_publisher_fingerprint
                && item.raw_content_digest == source.recipe_content_digest
        }) && evidence.executor.bundle_name == source.bundle_name
            && evidence.executor.signer_fingerprint == source.recipe_publisher_fingerprint
            && evidence.executor.target_triple == source.guest_target_triple
            && evidence.executor.item_ref
                == format!("bin/{}/{}", source.guest_target_triple, OWNER_NAME)
            && evidence.executor.signed_manifest_ref_blob_hash
                == source.executor_manifest_ref_signed_digest
            && evidence.executor.manifest_object_blob_hash == source.executor_manifest_hash
            && evidence.executor.item_source_object_hash == source.executor_item_source_hash
            && evidence.executor.signed_sidecar_blob_hash == source.executor_sidecar_signed_digest
            && evidence.executor.payload_blob_hash == source.owner_executable_sha256,
        "retained materialization source evidence differs from node coordinate"
    );
    ensure!(
        recipe_source_set_digest(&evidence)? == source.signed_recipe_source_set_digest
            && bundle_manifest_set_digest(&evidence)? == source.signed_bundle_manifest_set_digest,
        "retained materialization signed-source sets differ from node coordinate"
    );

    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        cas,
        [evidence_hash.to_owned()],
        ryeos_state::object_closure::ObjectClosureLimits::default(),
    )?;
    ensure!(
        closure.is_complete(),
        "retained materialization source CAS closure is incomplete"
    );

    for item in &evidence.signed_recipe_items {
        let signer = evidence
            .signer_keys
            .iter()
            .find(|key| key.signer_fingerprint == item.signer_fingerprint)
            .context("retained recipe signer has no historical verifier")?;
        let bytes = load_blob_bounded(cas, &item.signed_blob_hash, MAX_SIGNED_RECIPE_BYTES)?;
        verify_retained_signed_recipe_item(item, signer, &bytes)?;
    }
    for manifest in &evidence.signed_bundle_manifests {
        let signer = evidence
            .signer_keys
            .iter()
            .find(|key| key.signer_fingerprint == manifest.signer_fingerprint)
            .context("retained Bundle signer has no historical verifier")?;
        let bytes = load_blob_bounded(
            cas,
            &manifest.signed_blob_hash,
            MAX_SIGNED_BUNDLE_MANIFEST_BYTES,
        )?;
        verify_retained_signed_bundle_manifest(manifest, signer, &bytes)?;
    }
    let executor = &evidence.executor;
    let signed_ref = load_blob_bounded(
        cas,
        &executor.signed_manifest_ref_blob_hash,
        ryeos_engine::executor_resolution::MAX_EXECUTOR_MANIFEST_REF_BYTES,
    )?;
    let manifest_bytes = load_blob_bounded(
        cas,
        &executor.manifest_object_blob_hash,
        MAX_EXECUTOR_MANIFEST_OBJECT_BYTES,
    )?;
    let manifest_object: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    ensure!(
        lillux::canonical_json(&manifest_object)?.as_bytes() == manifest_bytes,
        "retained executor manifest is not canonical JSON"
    );
    let item_source = load_object_bounded(
        cas,
        &executor.item_source_object_hash,
        MAX_EXECUTOR_ITEM_SOURCE_OBJECT_BYTES,
    )?;
    let sidecar = load_blob_bounded(
        cas,
        &executor.signed_sidecar_blob_hash,
        MAX_EXECUTOR_SIDECAR_BYTES,
    )?;
    let payload = load_blob_bounded(cas, &executor.payload_blob_hash, source.maximum_owner_bytes)?;
    verify_retained_executor_source(
        &evidence,
        &signed_ref,
        &manifest_object,
        &item_source,
        &sidecar,
        &payload,
    )?;
    Ok(VerifiedGuestOwnerSourceClosure {
        evidence,
        payload_size: payload.len() as u64,
    })
}

/// Verify the exact retained product, after source verification and an
/// authenticated node attestation have bound `source` to this CAS closure.
/// This does not establish a fresh policy grant or runtime qualification.
pub(super) fn verify_output_closure(
    cas: &lillux::CasStore,
    source: &GuestOwnerMaterializationSource,
    verified_source: &VerifiedGuestOwnerSourceClosure,
) -> Result<GuestOwnerRuntimeManifestIdentity> {
    source.coordinate_digest()?;
    ensure!(
        verified_source.evidence.executor.payload_blob_hash == source.owner_executable_sha256,
        "retained output owner differs from verified source"
    );
    let manifest_value = load_object_bounded(
        cas,
        &source.runtime_manifest_hash,
        MAX_OUTPUT_MANIFEST_BYTES,
    )?;
    let root_bytes = base64::engine::general_purpose::STANDARD.decode(
        source
            .controller_public_root
            .strip_prefix("ed25519:")
            .context("retained controller root is not Ed25519")?,
    )?;
    let root_bytes: [u8; 32] = root_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("retained controller root has wrong length"))?;
    let root_key = lillux::crypto::VerifyingKey::from_bytes(&root_bytes)?;
    let identity = derive_guest_owner_runtime_manifest_identity(&manifest_value, &root_key)?;
    ensure!(
        identity.manifest_hash == source.runtime_manifest_hash
            && identity.owner_executable_sha256 == source.owner_executable_sha256
            && identity.controller_public_root == source.controller_public_root,
        "retained product identity differs from materialization source"
    );
    let manifest = ExternalContentManifestObject::from_value(&manifest_value)?;
    let owner = manifest
        .entries
        .iter()
        .find(|entry| entry.path == "bin/ryeos-external-guest-occurrence-owner")
        .context("retained product has no owner entry")?;
    ensure!(
        owner.size == Some(verified_source.payload_size),
        "retained product owner size differs from source payload"
    );
    let profile = manifest
        .entries
        .iter()
        .find(|entry| entry.path == "guest-owner-profile.json")
        .context("retained product has no profile entry")?;
    ensure!(
        profile.blob_hash.as_deref() == Some(source.profile_digest.as_str()),
        "retained product profile differs from materialization source"
    );
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        cas,
        [source.runtime_manifest_hash.clone()],
        ryeos_state::object_closure::ObjectClosureLimits::default(),
    )?;
    ensure!(
        closure.is_complete(),
        "retained materialization output CAS closure is incomplete"
    );
    let owner_bytes = load_blob_bounded(
        cas,
        &source.owner_executable_sha256,
        source.maximum_owner_bytes,
    )?;
    ensure!(
        owner_bytes.len() as u64 == verified_source.payload_size,
        "retained product owner bytes differ from source payload"
    );
    load_blob_bounded(cas, &source.profile_digest, MAX_PROFILE_BYTES)?;
    load_blob_bounded(cas, &identity.controller_root_blob_sha256, 64)?;
    Ok(identity)
}

fn recipe_source_set_digest(
    evidence: &GuestRuntimeMaterializationSourceEvidence,
) -> Result<String> {
    let entries = evidence
        .signed_recipe_items
        .iter()
        .map(|item| {
            serde_json::json!({
                "resolved_ref": item.resolved_ref,
                "source_root": ItemSourceRoot::Bundle { name: item.bundle_name.clone() },
                "signer_fingerprint": item.signer_fingerprint,
                "source_content_digest": item.signed_blob_hash,
                "raw_content_digest": item.raw_content_digest,
                "signature_envelope": item.signature_envelope,
            })
        })
        .collect::<Vec<_>>();
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.guest-owner-signed-recipe-source-set.v1",
        "entries": entries,
    }))
}

fn bundle_manifest_set_digest(
    evidence: &GuestRuntimeMaterializationSourceEvidence,
) -> Result<String> {
    let entries = evidence
        .signed_bundle_manifests
        .iter()
        .map(|manifest| {
            serde_json::json!({
                "name": manifest.bundle_name,
                "body_digest": manifest.body_digest,
                "signer_fingerprint": manifest.signer_fingerprint,
                "signed_blob_hash": manifest.signed_blob_hash,
            })
        })
        .collect::<Vec<_>>();
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.guest-owner-signed-bundle-manifest-set.v1",
        "entries": entries,
    }))
}

fn load_blob_bounded(cas: &lillux::CasStore, hash: &str, maximum_bytes: u64) -> Result<Vec<u8>> {
    cas.get_blob_bounded(hash, maximum_bytes)?
        .with_context(|| format!("retained materialization CAS blob {hash} is missing"))
}

pub(super) fn load_object_bounded(
    cas: &lillux::CasStore,
    hash: &str,
    maximum_bytes: u64,
) -> Result<serde_json::Value> {
    let (file, size) = cas
        .open_object(hash)?
        .with_context(|| format!("retained materialization CAS object {hash} is missing"))?;
    let bytes = lillux::read_open_regular_file_exact_bounded(file, size, maximum_bytes)?;
    ensure!(
        lillux::sha256_hex(&bytes) == hash,
        "retained materialization CAS object bytes changed"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        lillux::canonical_json(&value)?.as_bytes() == bytes,
        "retained materialization CAS object is not canonical JSON"
    );
    Ok(value)
}
