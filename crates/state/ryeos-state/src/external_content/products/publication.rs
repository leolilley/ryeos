//! Durable exact publication and lookup for named-product capture testimony.
//!
//! This module persists testimony only. Its caller must already have verified
//! the successful terminal result and admitted recipe binding; neither can be
//! inferred from the witness's self-described historical coordinates.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ProductBounds, ProductCaptureEvidence, ProductShape};
use crate::object_closure::{ObjectClosureLimits, collect_object_closure_with_cas_and_limits};
use crate::objects::{
    Attestation, EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
    ExternalContentManifestEntryKind, ExternalContentManifestObject,
    ExternalLargeContentManifestObject, MAX_EXTERNAL_CONTENT_MANIFEST_BYTES,
    MAX_LARGE_CONTENT_MANIFEST_BYTES,
};
use crate::{CasMutationGuard, PinnedStateAuthority, Signer};

pub const PRODUCT_CAPTURE_HEAD_NAMESPACE: &str = "retained-product-captures";
const PRODUCT_CAPTURE_COORDINATE_DOMAIN: &str = "ryeos.retained_product_capture.coordinate";
pub const MAX_PRODUCT_ATTESTATION_BYTES: u64 = 128 * 1024;

/// The product protocol and current node policy both bound an untrusted
/// witness before its JSON body is allocated.
pub fn product_attestation_byte_limit(limits: ObjectClosureLimits) -> u64 {
    MAX_PRODUCT_ATTESTATION_BYTES.min(limits.max_object_bytes)
}

/// Open and authenticate one possible product-attestation object without
/// allocating beyond the product/node wire ceiling. Absence remains distinct
/// from malformed, oversized, non-canonical, or wrongly addressed content.
pub fn load_product_attestation_value(
    authority: &PinnedStateAuthority,
    attestation_hash: &str,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<Option<Value>> {
    super::validate_hash("product attestation", attestation_hash)?;
    authority.ensure_guard(guard)?;
    let cas = authority.cas_store()?;
    let Some((file, size)) = cas
        .open_object(attestation_hash)
        .with_context(|| format!("open product attestation {attestation_hash}"))?
    else {
        return Ok(None);
    };
    let max_bytes = product_attestation_byte_limit(limits);
    if size > max_bytes {
        bail!(
            "product capture attestation exceeds its bounded wire contract: {size} > {max_bytes}"
        );
    }
    let bytes = lillux::read_open_regular_file_exact_bounded(file, size, max_bytes)
        .with_context(|| format!("read product attestation {attestation_hash}"))?;
    if u64::try_from(bytes.len()).ok() != Some(size)
        || lillux::sha256_hex(&bytes) != attestation_hash
    {
        bail!("product attestation object does not match its requested CAS identity");
    }
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode product attestation {attestation_hash}"))?;
    if lillux::canonical_json(&value)?.as_bytes() != bytes {
        bail!("product attestation object is not canonical JSON");
    }
    Ok(Some(value))
}

/// Immutable request identity for exactly one named capture attempt.
///
/// Result bytes, node policy and timestamps are deliberately absent. The
/// attested result is the one immutable answer selected for this exact request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductCaptureCoordinate {
    pub owner_principal: String,
    pub chain_root_id: String,
    pub thread_id: String,
    pub recipe_binding: String,
    pub product_name: String,
}

impl ProductCaptureCoordinate {
    pub fn validate(&self) -> anyhow::Result<()> {
        let owner = self
            .owner_principal
            .strip_prefix("fp:")
            .context("product owner must be an exact fingerprint principal")?;
        super::validate_hash("product owner", owner)?;
        for (label, value) in [
            ("product root", &self.chain_root_id),
            ("product thread", &self.thread_id),
        ] {
            if value.is_empty()
                || value.len() > 2048
                || value.trim() != value
                || value.chars().any(char::is_control)
            {
                bail!("{label} is not a bounded exact coordinate");
            }
        }
        super::validate_binding_name(&self.recipe_binding)?;
        super::validate_name(&self.product_name)?;
        Ok(())
    }

    pub fn from_evidence(evidence: &ProductCaptureEvidence) -> anyhow::Result<Self> {
        evidence.validate()?;
        let coordinate = Self {
            owner_principal: evidence.owner_principal.clone(),
            chain_root_id: evidence.chain_root_id.clone(),
            thread_id: evidence.thread_id.clone(),
            recipe_binding: evidence.recipe_binding.clone(),
            product_name: evidence.declaration.name.clone(),
        };
        coordinate.validate()?;
        Ok(coordinate)
    }

    pub fn coordinate_id(&self) -> anyhow::Result<String> {
        self.validate()?;
        let value = serde_json::json!({
            "domain": PRODUCT_CAPTURE_COORDINATE_DOMAIN,
            "owner_principal": self.owner_principal,
            "chain_root_id": self.chain_root_id,
            "thread_id": self.thread_id,
            "recipe_binding": self.recipe_binding,
            "product_name": self.product_name,
        });
        let canonical = lillux::canonical_json(&value)?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    fn ensure_matches(&self, evidence: &ProductCaptureEvidence) -> anyhow::Result<()> {
        if &Self::from_evidence(evidence)? != self {
            bail!("product capture testimony contradicts its exact request coordinate");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedProductWitness {
    pub coordinate_id: String,
    pub attestation_hash: String,
    pub attestation: Attestation,
    pub evidence: ProductCaptureEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductWitnessLookup {
    Missing,
    Found(VerifiedProductWitness),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductWitnessPublication {
    pub witness: VerifiedProductWitness,
    pub reused_existing: bool,
}

/// Publish the immutable answer for one exact product-capture request.
///
/// Closure verification is performed through descriptor-pinned authority and
/// does not require holding the application's global `StateDb` mutex. The
/// supplied shared mutation guard must already span this verification and the
/// short per-coordinate signed-head publication.
pub fn publish_product_witness(
    authority: &PinnedStateAuthority,
    coordinate: &ProductCaptureCoordinate,
    attestation: &Attestation,
    limits: ObjectClosureLimits,
    signer: &dyn Signer,
    guard: &CasMutationGuard,
) -> anyhow::Result<ProductWitnessPublication> {
    coordinate.validate()?;
    let (witness, reused_existing) = publish_immutable_product_attestation(
        authority,
        PRODUCT_CAPTURE_HEAD_NAMESPACE,
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
            )
        },
    )?;
    Ok(ProductWitnessPublication {
        witness,
        reused_existing,
    })
}

/// Shared storage transaction for immutable product testimony. Typed owners
/// supply their exact signature, coordinate and subject verification; this
/// helper owns only bounded CAS reads and the per-coordinate signed head.
#[allow(clippy::too_many_arguments)]
pub(super) fn publish_immutable_product_attestation<T>(
    authority: &PinnedStateAuthority,
    namespace: &str,
    coordinate_id: &str,
    attestation: &Attestation,
    limits: ObjectClosureLimits,
    signer: &dyn Signer,
    guard: &CasMutationGuard,
    verify: impl Fn(&Attestation) -> anyhow::Result<T>,
) -> anyhow::Result<(T, bool)> {
    authority.ensure_guard(guard)?;
    crate::signer::ensure_signer_trusted(signer, authority.trust_store())?;
    let candidate = verify(attestation)?;
    if attestation.issuer_fingerprint()? != signer.fingerprint() {
        bail!("product testimony was not signed by the publishing node");
    }
    let candidate_hash =
        lillux::sha256_hex(lillux::canonical_json(&attestation.to_value())?.as_bytes());
    let load = |hash: &str| -> anyhow::Result<(Attestation, T)> {
        let value = load_product_attestation_value(authority, hash, limits, guard)?
            .context("product testimony head target is missing")?;
        let retained = Attestation::from_value(&value)?;
        let verified = verify(&retained)?;
        Ok((retained, verified))
    };
    // Verify an incumbent before taking its per-coordinate writer lock. Heads
    // are immutable in this namespace, so the target comparison below makes
    // the normal retry path a short locked re-read rather than a closure walk.
    let observed_existing = match crate::refs::read_verified_generic_head_ref_in_directory(
        authority.refs_directory(),
        namespace,
        coordinate_id,
        authority.trust_store(),
    )? {
        Some(head) => {
            if head.signer != signer.fingerprint() {
                bail!("product capture head is signed by a different node");
            }
            Some((head.target_hash.clone(), load(&head.target_hash)?))
        }
        None => None,
    };
    let head_lock = crate::refs::GenericHeadLock::acquire_in_refs_directory(
        authority.refs_directory(),
        namespace,
        coordinate_id,
    )?;
    if let Some(head) = crate::refs::read_verified_generic_head_ref_in_directory(
        authority.refs_directory(),
        namespace,
        coordinate_id,
        authority.trust_store(),
    )? {
        if head.signer != signer.fingerprint() {
            bail!("product capture head is signed by a different node");
        }
        let (existing_attestation, existing) = match observed_existing {
            Some((observed_hash, existing)) if observed_hash == head.target_hash => existing,
            _ => load(&head.target_hash)?,
        };
        if !same_testimony_ignoring_issue_time(&existing_attestation, attestation) {
            bail!(
                "existing product testimony {} contradicts the requested signed evidence",
                head.target_hash
            );
        }
        return Ok((existing, true));
    }

    let cas = authority.cas_store()?;
    let value = attestation.to_value();
    let stored = cas
        .put_object(&value)
        .context("store product capture attestation")?;
    if stored.hash != candidate_hash {
        bail!("product capture attestation CAS digest changed during publication");
    }
    crate::refs::write_verified_generic_head_ref_in_directory(
        authority.refs_directory(),
        namespace,
        coordinate_id,
        &stored.hash,
        signer,
        authority.trust_store(),
        &head_lock,
    )?;
    Ok((candidate, false))
}

/// Exact lookup which acquires its own shared mutation guard.
pub fn lookup_product_witness(
    authority: &PinnedStateAuthority,
    coordinate: &ProductCaptureCoordinate,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
) -> anyhow::Result<ProductWitnessLookup> {
    let guard = authority.acquire_shared_guard()?;
    lookup_product_witness_guarded(authority, coordinate, node_key, limits, &guard)
}

/// Exact lookup for a caller that already holds the node's CAS mutation guard.
pub fn lookup_product_witness_guarded(
    authority: &PinnedStateAuthority,
    coordinate: &ProductCaptureCoordinate,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<ProductWitnessLookup> {
    coordinate.validate()?;
    authority.ensure_guard(guard)?;
    let coordinate_id = coordinate.coordinate_id()?;
    let Some(head) = crate::refs::read_verified_generic_head_ref_in_directory(
        authority.refs_directory(),
        PRODUCT_CAPTURE_HEAD_NAMESPACE,
        &coordinate_id,
        authority.trust_store(),
    )?
    else {
        return Ok(ProductWitnessLookup::Missing);
    };
    let expected_node = lillux::crypto::fingerprint(node_key);
    if head.signer != expected_node {
        bail!("product capture head is not signed by the requested node");
    }
    let witness = load_and_verify_attestation(
        authority,
        coordinate,
        &head.target_hash,
        node_key,
        limits,
        false,
        guard,
    )?
    .context("product capture head target is missing")?;
    Ok(ProductWitnessLookup::Found(witness))
}

/// Verify an exact immutable witness hash without consulting its current head.
pub fn lookup_product_witness_hash(
    authority: &PinnedStateAuthority,
    coordinate: &ProductCaptureCoordinate,
    attestation_hash: &str,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
) -> anyhow::Result<ProductWitnessLookup> {
    let guard = authority.acquire_shared_guard()?;
    lookup_product_witness_hash_guarded(
        authority,
        coordinate,
        attestation_hash,
        node_key,
        limits,
        &guard,
    )
}

pub fn lookup_product_witness_hash_guarded(
    authority: &PinnedStateAuthority,
    coordinate: &ProductCaptureCoordinate,
    attestation_hash: &str,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<ProductWitnessLookup> {
    coordinate.validate()?;
    super::validate_hash("product attestation", attestation_hash)?;
    authority.ensure_guard(guard)?;
    Ok(
        match load_and_verify_attestation(
            authority,
            coordinate,
            attestation_hash,
            node_key,
            limits,
            true,
            guard,
        )? {
            Some(witness) => ProductWitnessLookup::Found(witness),
            None => ProductWitnessLookup::Missing,
        },
    )
}

fn load_and_verify_attestation(
    authority: &PinnedStateAuthority,
    coordinate: &ProductCaptureCoordinate,
    attestation_hash: &str,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
    missing_is_absence: bool,
    guard: &CasMutationGuard,
) -> anyhow::Result<Option<VerifiedProductWitness>> {
    super::validate_hash("product attestation", attestation_hash)?;
    let Some(value) = load_product_attestation_value(authority, attestation_hash, limits, guard)?
    else {
        if missing_is_absence {
            return Ok(None);
        }
        bail!("product attestation {attestation_hash} is missing");
    };
    let attestation = Attestation::from_value(&value).context("decode product attestation")?;
    let witness = verify_attestation(authority, coordinate, &attestation, node_key, limits)?;
    if witness.attestation_hash != attestation_hash {
        bail!("product attestation object does not match its requested CAS identity");
    }
    Ok(Some(witness))
}

fn verify_attestation(
    authority: &PinnedStateAuthority,
    coordinate: &ProductCaptureCoordinate,
    attestation: &Attestation,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
) -> anyhow::Result<VerifiedProductWitness> {
    let canonical = lillux::canonical_json(&attestation.to_value())?;
    let max_bytes = product_attestation_byte_limit(limits);
    if u64::try_from(canonical.len()).unwrap_or(u64::MAX) > max_bytes {
        bail!(
            "product capture attestation exceeds its bounded wire contract: {} > {max_bytes}",
            canonical.len()
        );
    }
    attestation
        .verify_with_key(node_key)
        .context("verify product testimony node signature")?;
    let evidence = ProductCaptureEvidence::from_attestation(attestation)?;
    coordinate.ensure_matches(&evidence)?;
    verify_product_subject(authority, &evidence, limits)?;
    Ok(VerifiedProductWitness {
        coordinate_id: coordinate.coordinate_id()?,
        attestation_hash: lillux::sha256_hex(canonical.as_bytes()),
        attestation: attestation.clone(),
        evidence,
    })
}

fn verify_product_subject(
    authority: &PinnedStateAuthority,
    evidence: &ProductCaptureEvidence,
    limits: ObjectClosureLimits,
) -> anyhow::Result<()> {
    let cas = authority.cas_store()?;
    let closure =
        collect_object_closure_with_cas_and_limits(&cas, [evidence.manifest_hash.clone()], limits)
            .context("verify retained product closure")?;
    if !closure.is_complete() {
        bail!(
            "retained product closure is incomplete: missing_objects={}, missing_blobs={}, malformed_objects={}, unsupported_objects={}",
            closure.missing_objects.len(),
            closure.missing_blobs.len(),
            closure.malformed_objects.len(),
            closure.unsupported_objects.len()
        );
    }

    match evidence.manifest_kind.as_str() {
        EXTERNAL_CONTENT_MANIFEST_KIND => {
            let value = crate::object_closure::load_exact_cas_object_with_cas(
                &cas,
                &evidence.manifest_hash,
                (MAX_EXTERNAL_CONTENT_MANIFEST_BYTES as u64).min(limits.max_object_bytes),
            )?;
            let manifest = ExternalContentManifestObject::from_value(&value)?;
            ensure_manifest_metrics(
                evidence,
                manifest.entry_count,
                manifest.total_bytes,
                manifest.is_file_shaped(),
                manifest
                    .entries
                    .iter()
                    .map(|entry| (entry.path.as_str(), entry.kind, entry.size)),
                None,
            )?;
            // Closure traversal proves the blob identities are present. The
            // typed loader additionally proves every manifest size agrees with
            // the bytes stored at that identity.
            crate::VerifiedExternalContentClosure::load(&cas, &evidence.manifest_hash)?;
        }
        EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
            let value = crate::object_closure::load_exact_cas_object_with_cas(
                &cas,
                &evidence.manifest_hash,
                (MAX_LARGE_CONTENT_MANIFEST_BYTES as u64).min(limits.max_object_bytes),
            )?;
            let manifest = ExternalLargeContentManifestObject::from_value(&value)?;
            ensure_manifest_metrics(
                evidence,
                manifest.entry_count,
                manifest.total_bytes,
                manifest.is_file_shaped(),
                manifest
                    .entries
                    .iter()
                    .map(|entry| (entry.path.as_str(), entry.kind, entry.size)),
                None,
            )?;
            let large_store = authority.large_object_store()?;
            for entry in &manifest.entries {
                if let Some(file_sha256) = entry.file_sha256.as_deref() {
                    large_store
                        .verify_manifest_commitment(entry)
                        .with_context(|| {
                            format!("verify retained large product entry {}", entry.path)
                        })?;
                    let findings = large_store.scrub_object(file_sha256)?;
                    if !findings.is_empty() {
                        bail!(
                            "retained large product entry {} failed byte integrity: {findings:?}",
                            entry.path
                        );
                    }
                } else if let Some(blob_hash) = entry.blob_hash.as_deref() {
                    let bytes = crate::object_closure::load_exact_cas_blob_with_cas(
                        &cas,
                        blob_hash,
                        crate::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES.min(limits.max_blob_bytes),
                    )?;
                    if entry.size != Some(bytes.len() as u64) {
                        bail!(
                            "retained large product blob {} size contradicts its manifest",
                            entry.path
                        );
                    }
                }
            }
        }
        other => bail!("unsupported product manifest kind {other}"),
    }
    Ok(())
}

/// Recheck the already-authenticated manifest's per-entry metrics against a
/// relationship's narrower bounds. Witness verification has already proved the
/// payload bytes; this bounded metadata pass deliberately does not rescrub large
/// chunks merely to apply a stricter invocation-time allowance.
pub fn verify_product_manifest_against_bounds(
    authority: &PinnedStateAuthority,
    evidence: &ProductCaptureEvidence,
    required_bounds: &ProductBounds,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<()> {
    authority.ensure_guard(guard)?;
    evidence.validate()?;
    required_bounds.validate()?;
    if required_bounds.maximum_entries > evidence.declaration.bounds.maximum_entries
        || required_bounds.maximum_depth > evidence.declaration.bounds.maximum_depth
        || required_bounds.maximum_file_bytes > evidence.declaration.bounds.maximum_file_bytes
        || required_bounds.maximum_total_bytes > evidence.declaration.bounds.maximum_total_bytes
    {
        bail!("product relationship bounds widen the captured declaration");
    }
    let cas = authority.cas_store()?;
    match evidence.manifest_kind.as_str() {
        EXTERNAL_CONTENT_MANIFEST_KIND => {
            let value = crate::object_closure::load_exact_cas_object_with_cas(
                &cas,
                &evidence.manifest_hash,
                (MAX_EXTERNAL_CONTENT_MANIFEST_BYTES as u64).min(limits.max_object_bytes),
            )?;
            let manifest = ExternalContentManifestObject::from_value(&value)?;
            ensure_manifest_metrics(
                evidence,
                manifest.entry_count,
                manifest.total_bytes,
                manifest.is_file_shaped(),
                manifest
                    .entries
                    .iter()
                    .map(|entry| (entry.path.as_str(), entry.kind, entry.size)),
                Some(required_bounds),
            )
        }
        EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
            let value = crate::object_closure::load_exact_cas_object_with_cas(
                &cas,
                &evidence.manifest_hash,
                (MAX_LARGE_CONTENT_MANIFEST_BYTES as u64).min(limits.max_object_bytes),
            )?;
            let manifest = ExternalLargeContentManifestObject::from_value(&value)?;
            ensure_manifest_metrics(
                evidence,
                manifest.entry_count,
                manifest.total_bytes,
                manifest.is_file_shaped(),
                manifest
                    .entries
                    .iter()
                    .map(|entry| (entry.path.as_str(), entry.kind, entry.size)),
                Some(required_bounds),
            )
        }
        other => bail!("unsupported product manifest kind {other}"),
    }
}

fn ensure_manifest_metrics<'a>(
    evidence: &ProductCaptureEvidence,
    entry_count: usize,
    total_bytes: u64,
    file_shaped: bool,
    entries: impl Iterator<Item = (&'a str, ExternalContentManifestEntryKind, Option<u64>)>,
    required_bounds: Option<&ProductBounds>,
) -> anyhow::Result<()> {
    if entry_count != evidence.entry_count || total_bytes != evidence.total_bytes {
        bail!("retained product manifest metrics contradict capture testimony");
    }
    if evidence.declaration.shape == ProductShape::File && !file_shaped {
        bail!("retained product manifest shape contradicts capture testimony");
    }
    if required_bounds.is_some_and(|bounds| {
        entry_count > bounds.maximum_entries || total_bytes > bounds.maximum_total_bytes
    }) {
        bail!("retained product manifest exceeds the relationship aggregate bound");
    }
    for (path, kind, size) in entries {
        // Match capture's strict directory-depth convention: files and links
        // occupy their parent directory, while a directory itself must be
        // enterable at its complete component depth.
        let depth = path
            .split('/')
            .count()
            .saturating_sub(usize::from(kind != ExternalContentManifestEntryKind::Dir));
        if depth >= evidence.declaration.bounds.maximum_depth {
            bail!("retained product manifest exceeds its declared depth bound at {path}");
        }
        if kind == ExternalContentManifestEntryKind::File
            && size.is_none_or(|size| size > evidence.declaration.bounds.maximum_file_bytes)
        {
            bail!("retained product manifest file exceeds its declared bound at {path}");
        }
        if let Some(bounds) = required_bounds {
            if depth >= bounds.maximum_depth {
                bail!("retained product manifest exceeds the relationship depth bound at {path}");
            }
            if kind == ExternalContentManifestEntryKind::File
                && size.is_none_or(|size| size > bounds.maximum_file_bytes)
            {
                bail!("retained product manifest file exceeds the relationship bound at {path}");
            }
        }
    }
    Ok(())
}

fn same_testimony_ignoring_issue_time(left: &Attestation, right: &Attestation) -> bool {
    left.schema == right.schema
        && left.kind == right.kind
        && left.subject_hash == right.subject_hash
        && left.claim == right.claim
        && left.policy == right.policy
        && left.issuer == right.issuer
        && left.expires_at == right.expires_at
        && left.evidence == right.evidence
}

#[cfg(test)]
pub(super) mod tests;
