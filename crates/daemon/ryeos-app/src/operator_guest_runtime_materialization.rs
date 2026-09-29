//! Controller-local preparation of one guest owner from signed Bundle data.
//!
//! This stage performs no provider contact and grants no runtime qualification.
//! The later durable publication owner must retain the exact tree and source
//! testimony in CAS before a snapshot operation may contact its provider.

use std::ffi::OsStr;

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use ryeos_engine::binary_resolver::{BundlePayloadSourceProof, capture_bundle_payload_for_target};
use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace, SubjectResolutionAuthority};
use ryeos_engine::engine::{CapturedSignedBundleItemSource, EffectiveItemRequest};
use ryeos_engine::plan_builder::CapturedSignedBundleManifest;
use ryeos_engine::resolution::TrustClass;
use ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime;
use ryeos_external_execution::guest_runtime_product::{
    GuestOwnerMaterializationRecipe, GuestOwnerRuntimeManifestIdentity, OWNER_NAME,
    derive_guest_owner_runtime_manifest_identity,
    produce_guest_owner_runtime_from_admitted_payload,
};
use ryeos_state::objects::{
    GUEST_RUNTIME_MATERIALIZATION_SCHEMA, GUEST_RUNTIME_MATERIALIZATION_SOURCE_KIND,
    GuestRuntimeMaterializationSourceEvidence, MaterializationExecutorSource,
    MaterializationSignatureEnvelope, MaterializationSignedBundleManifest,
    MaterializationSignedItem, MaterializationSignerKey,
};
use serde::{Deserialize, Serialize};

use crate::handler_context::HandlerContext;
use crate::operator_authority::AdmittedOperatorAuthority;
use crate::state::AppState;

mod publication;
mod retained;

pub use publication::{
    CurrentGuestOwnerMaterialization, PublishedGuestOwnerMaterialization,
    load_current_guest_owner_materialization, publish_prepared_guest_owner_runtime,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestOwnerMaterializationSource {
    pub schema: u32,
    pub materializer_protocol: String,
    pub materialization_binding_id: String,
    pub materialization_binding_digest: String,
    pub recipe_ref: String,
    pub recipe_content_digest: String,
    pub recipe_effective_digest: String,
    pub signed_recipe_source_set_digest: String,
    pub signed_bundle_manifest_set_digest: String,
    pub bundle_name: String,
    pub bundle_generation: String,
    pub recipe_publisher_fingerprint: String,
    pub executor_manifest_hash: String,
    pub executor_manifest_ref_signed_digest: String,
    pub executor_item_source_hash: String,
    pub executor_sidecar_signed_digest: String,
    pub owner_executable_sha256: String,
    pub guest_target_triple: String,
    pub maximum_owner_bytes: u64,
    pub profile_digest: String,
    pub operator_authority: AdmittedOperatorAuthority,
    pub node_site_id: String,
    pub controller_public_root: String,
    pub runtime_manifest_hash: String,
}

impl GuestOwnerMaterializationSource {
    /// Immutable request coordinate, excluding the output manifest. Two
    /// different admitted sources may produce identical bytes without being
    /// forced into one publication head. A changed output for the same exact
    /// request is a contradiction to resolve, never an overwrite.
    pub fn coordinate_digest(&self) -> Result<String> {
        ensure!(
            self.schema == 1
                && self.materializer_protocol == "ryeos.guest-owner-materialization.v1",
            "unsupported guest owner materialization source"
        );
        self.operator_authority.validate()?;
        crate::identity::validate_canonical_site_id(&self.node_site_id)?;
        if self.operator_authority.principal_class
            == crate::identity::AuthorizedKeyPrincipalClass::LocalClient
        {
            ensure!(
                self.operator_authority.origin_site_id == self.node_site_id,
                "local materialization owner has a foreign origin"
            );
        }
        let encoded_root = self
            .controller_public_root
            .strip_prefix("ed25519:")
            .context("materialization controller root is not Ed25519")?;
        let decoded_root = base64::engine::general_purpose::STANDARD.decode(encoded_root)?;
        let root_bytes: [u8; 32] = decoded_root
            .try_into()
            .map_err(|_| anyhow::anyhow!("materialization controller root changed length"))?;
        let verifying_key = lillux::crypto::VerifyingKey::from_bytes(&root_bytes)?;
        ensure!(
            !verifying_key.is_weak()
                && base64::engine::general_purpose::STANDARD.encode(root_bytes) == encoded_root,
            "materialization controller root is not canonical or strong"
        );
        for (label, value) in [
            ("binding digest", &self.materialization_binding_digest),
            ("recipe content", &self.recipe_content_digest),
            ("recipe definition", &self.recipe_effective_digest),
            (
                "signed recipe source set",
                &self.signed_recipe_source_set_digest,
            ),
            (
                "signed Bundle manifest set",
                &self.signed_bundle_manifest_set_digest,
            ),
            ("bundle generation", &self.bundle_generation),
            ("recipe publisher", &self.recipe_publisher_fingerprint),
            ("executor manifest", &self.executor_manifest_hash),
            (
                "signed executor manifest ref",
                &self.executor_manifest_ref_signed_digest,
            ),
            ("executor item source", &self.executor_item_source_hash),
            (
                "signed executor sidecar",
                &self.executor_sidecar_signed_digest,
            ),
            ("owner executable", &self.owner_executable_sha256),
            ("profile", &self.profile_digest),
            ("output manifest", &self.runtime_manifest_hash),
        ] {
            ensure!(
                lillux::valid_hash(value),
                "materialization {label} is invalid"
            );
        }
        ensure!(
            !self.materialization_binding_id.is_empty()
                && self.materialization_binding_id.len() <= 128
                && self
                    .materialization_binding_id
                    .bytes()
                    .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') })
                && !self.guest_target_triple.is_empty()
                && !self.guest_target_triple.starts_with('.')
                && !self.guest_target_triple.contains("..")
                && self.guest_target_triple.len() <= 96
                && self.guest_target_triple.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                })
                && !self.bundle_name.is_empty()
                && self.bundle_name.len() <= 64
                && self.bundle_name.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                })
                && (1..=32 * 1024 * 1024).contains(&self.maximum_owner_bytes),
            "materialization binding, bundle, target or byte bound is invalid"
        );
        let recipe_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(&self.recipe_ref)?;
        ensure!(
            recipe_ref.kind == "config"
                && recipe_ref.suffix.is_none()
                && recipe_ref.to_string() == self.recipe_ref,
            "materialization recipe is not a canonical Config ref"
        );
        ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.guest-owner-materialization-coordinate.v1",
            "materializer_protocol": self.materializer_protocol,
            "materialization_binding_id": self.materialization_binding_id,
            "materialization_binding_digest": self.materialization_binding_digest,
            "recipe_ref": self.recipe_ref,
            "recipe_content_digest": self.recipe_content_digest,
            "recipe_effective_digest": self.recipe_effective_digest,
            "signed_recipe_source_set_digest": self.signed_recipe_source_set_digest,
            "signed_bundle_manifest_set_digest": self.signed_bundle_manifest_set_digest,
            "bundle_name": self.bundle_name,
            "bundle_generation": self.bundle_generation,
            "recipe_publisher_fingerprint": self.recipe_publisher_fingerprint,
            "executor_manifest_hash": self.executor_manifest_hash,
            "executor_manifest_ref_signed_digest": self.executor_manifest_ref_signed_digest,
            "executor_item_source_hash": self.executor_item_source_hash,
            "executor_sidecar_signed_digest": self.executor_sidecar_signed_digest,
            "owner_executable_sha256": self.owner_executable_sha256,
            "guest_target_triple": self.guest_target_triple,
            "maximum_owner_bytes": self.maximum_owner_bytes,
            "profile_digest": self.profile_digest,
            "operator_authority": self.operator_authority,
            "node_site_id": self.node_site_id,
            "controller_public_root": self.controller_public_root,
        }))
    }
}

/// A live private stage. Its source statement is *not* a durable witness;
/// callers must publish its CAS closure before exposing it to a provider.
pub struct PreparedGuestOwnerMaterialization {
    root: lillux::PinnedDirectory,
    identity: GuestOwnerRuntimeManifestIdentity,
    source: GuestOwnerMaterializationSource,
    signed_recipe_sources: Vec<CapturedSignedBundleItemSource>,
    signed_bundle_manifests: Vec<CapturedSignedBundleManifest>,
    signer_keys: Vec<MaterializationSignerKey>,
    executor_source_proof: BundlePayloadSourceProof,
}

fn signed_bundle_manifest_set_digest(manifests: &[CapturedSignedBundleManifest]) -> Result<String> {
    ensure!(
        !manifests.is_empty() && manifests.len() <= 8,
        "materialization signed Bundle manifest count is invalid"
    );
    let mut entries = Vec::with_capacity(manifests.len());
    for manifest in manifests {
        entries.push(serde_json::json!({
            "name": manifest.identity.name,
            "body_digest": manifest.identity.body_digest,
            "signer_fingerprint": manifest.identity.signer_fingerprint,
            "signed_blob_hash": lillux::sha256_hex(&manifest.signed_bytes),
        }));
    }
    entries.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    ensure!(
        entries
            .windows(2)
            .all(|pair| pair[0]["name"] != pair[1]["name"]),
        "materialization signed Bundle manifest names are duplicated"
    );
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.guest-owner-signed-bundle-manifest-set.v1",
        "entries": entries,
    }))
}

fn signed_recipe_source_set_digest(sources: &[CapturedSignedBundleItemSource]) -> Result<String> {
    ensure!(
        !sources.is_empty() && sources.len() <= 64,
        "materialization signed recipe source count is invalid"
    );
    let mut entries = Vec::with_capacity(sources.len());
    for source in sources {
        ensure!(
            lillux::sha256_hex(&source.signed_bytes) == source.source_content_digest,
            "materialization signed recipe source bytes differ from verified identity"
        );
        let ItemSourceRoot::Bundle { name: bundle_name } = &source.source_root else {
            anyhow::bail!("materialization signed recipe source is not Bundle content");
        };
        entries.push((
            source.resolved_ref.clone(),
            bundle_name.clone(),
            source.source_content_digest.clone(),
            serde_json::json!({
            "resolved_ref": source.resolved_ref,
            "source_root": source.source_root,
            "signer_fingerprint": source.signer_fingerprint,
            "source_content_digest": source.source_content_digest,
            "raw_content_digest": source.raw_content_digest,
            "signature_envelope": source.signature_envelope,
            }),
        ));
    }
    entries.sort_by(|left, right| (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2)));
    ensure!(
        entries
            .windows(2)
            .all(|pair| (&pair[0].0, &pair[0].1) != (&pair[1].0, &pair[1].1)),
        "materialization signed recipe source set has duplicate identities"
    );
    let entries: Vec<_> = entries.into_iter().map(|(_, _, _, entry)| entry).collect();
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.guest-owner-signed-recipe-source-set.v1",
        "entries": entries,
    }))
}

fn build_source_evidence(
    source: &GuestOwnerMaterializationSource,
    recipe_sources: &[CapturedSignedBundleItemSource],
    bundle_manifests: &[CapturedSignedBundleManifest],
    signer_keys: &[MaterializationSignerKey],
    executor: &BundlePayloadSourceProof,
) -> Result<GuestRuntimeMaterializationSourceEvidence> {
    source.coordinate_digest()?;
    ensure!(
        signed_recipe_source_set_digest(recipe_sources)? == source.signed_recipe_source_set_digest
            && signed_bundle_manifest_set_digest(bundle_manifests)?
                == source.signed_bundle_manifest_set_digest,
        "retained materialization recipe source sets differ from coordinate"
    );
    ensure!(
        recipe_sources.iter().any(|item| {
            item.resolved_ref == source.recipe_ref
                && item.raw_content_digest == source.recipe_content_digest
                && item.signer_fingerprint == source.recipe_publisher_fingerprint
                && item.source_root
                    == ItemSourceRoot::Bundle {
                        name: source.bundle_name.clone(),
                    }
        }) && bundle_manifests.iter().any(|manifest| {
            manifest.identity.name == source.bundle_name
                && manifest.identity.signer_fingerprint == source.recipe_publisher_fingerprint
        }),
        "retained materialization root recipe or executor Bundle differs from coordinate"
    );
    let manifest_bytes = lillux::canonical_json(&executor.manifest_object)?;
    let item_source_bytes = lillux::canonical_json(&executor.item_source_object)?;
    ensure!(
        lillux::sha256_hex(manifest_bytes.as_bytes()) == source.executor_manifest_hash
            && lillux::sha256_hex(item_source_bytes.as_bytes()) == source.executor_item_source_hash
            && lillux::sha256_hex(&executor.signed_manifest_ref)
                == source.executor_manifest_ref_signed_digest
            && lillux::sha256_hex(&executor.signed_sidecar)
                == source.executor_sidecar_signed_digest,
        "retained executor proof differs from materialization coordinate"
    );
    let selected = ryeos_engine::executor_resolution::verify_executor_manifest_object(
        &executor.manifest_object,
        &source.executor_manifest_hash,
    )?;
    ensure!(
        executor.selected_item_ref == format!("bin/{}/{}", source.guest_target_triple, OWNER_NAME)
            && selected.get(&executor.selected_item_ref) == Some(&source.executor_item_source_hash),
        "retained executor manifest does not select the captured ItemSource"
    );
    let (payload_hash, _) = ryeos_engine::executor_resolution::verify_executor_item_source(
        &executor.item_source_object,
        &source.executor_item_source_hash,
        &executor.selected_item_ref,
    )?;
    ensure!(
        payload_hash == source.owner_executable_sha256,
        "retained executor ItemSource does not select the materialized payload"
    );

    let mut signed_recipe_items = Vec::with_capacity(recipe_sources.len());
    for item in recipe_sources {
        let ItemSourceRoot::Bundle { name } = &item.source_root else {
            anyhow::bail!("retained materialization recipe item is not Bundle content");
        };
        signed_recipe_items.push(MaterializationSignedItem {
            resolved_ref: item.resolved_ref.clone(),
            bundle_name: name.clone(),
            signer_fingerprint: item.signer_fingerprint.clone(),
            signed_blob_hash: lillux::sha256_hex(&item.signed_bytes),
            raw_content_digest: item.raw_content_digest.clone(),
            signature_envelope: MaterializationSignatureEnvelope {
                prefix: item.signature_envelope.prefix.clone(),
                suffix: item.signature_envelope.suffix.clone(),
                after_shebang: item.signature_envelope.after_shebang,
            },
        });
    }
    signed_recipe_items.sort_by(|left, right| {
        (&left.resolved_ref, &left.bundle_name).cmp(&(&right.resolved_ref, &right.bundle_name))
    });
    let mut signed_bundle_manifests = bundle_manifests
        .iter()
        .map(|item| MaterializationSignedBundleManifest {
            bundle_name: item.identity.name.clone(),
            signer_fingerprint: item.identity.signer_fingerprint.clone(),
            signed_blob_hash: lillux::sha256_hex(&item.signed_bytes),
            body_digest: item.identity.body_digest.clone(),
        })
        .collect::<Vec<_>>();
    signed_bundle_manifests.sort_by(|left, right| left.bundle_name.cmp(&right.bundle_name));
    let evidence = GuestRuntimeMaterializationSourceEvidence {
        schema: GUEST_RUNTIME_MATERIALIZATION_SCHEMA,
        kind: GUEST_RUNTIME_MATERIALIZATION_SOURCE_KIND.to_owned(),
        signed_recipe_items,
        signed_bundle_manifests,
        signer_keys: signer_keys.to_vec(),
        executor: MaterializationExecutorSource {
            bundle_name: source.bundle_name.clone(),
            item_ref: executor.selected_item_ref.clone(),
            target_triple: source.guest_target_triple.clone(),
            signer_fingerprint: source.recipe_publisher_fingerprint.clone(),
            signed_manifest_ref_blob_hash: source.executor_manifest_ref_signed_digest.clone(),
            manifest_object_blob_hash: source.executor_manifest_hash.clone(),
            item_source_object_hash: source.executor_item_source_hash.clone(),
            signed_sidecar_blob_hash: source.executor_sidecar_signed_digest.clone(),
            payload_blob_hash: source.owner_executable_sha256.clone(),
        },
    };
    evidence.validate()?;
    Ok(evidence)
}

fn verify_retained_signed_recipe_item(
    item: &MaterializationSignedItem,
    signer: &MaterializationSignerKey,
    signed_bytes: &[u8],
) -> Result<()> {
    ensure!(
        item.signer_fingerprint == signer.signer_fingerprint,
        "retained recipe item signer differs from its selected verifier"
    );
    verify_retained_signed_envelope(
        signed_bytes,
        &item.signed_blob_hash,
        &item.raw_content_digest,
        &item.signature_envelope,
        signer,
    )?;
    Ok(())
}

fn verify_retained_signed_envelope(
    signed_bytes: &[u8],
    signed_hash: &str,
    body_hash: &str,
    envelope: &MaterializationSignatureEnvelope,
    signer: &MaterializationSignerKey,
) -> Result<String> {
    ensure!(
        lillux::sha256_hex(signed_bytes) == signed_hash,
        "retained signed source bytes differ from their address"
    );
    let key = retained_verifier(signer)?;
    let signed = std::str::from_utf8(signed_bytes)?;
    let (raw, header) = lillux::signature::strip_canonical_signature_with_envelope(
        signed,
        &envelope.prefix,
        envelope.suffix.as_deref(),
        envelope.after_shebang,
    )?;
    let header = header.context("retained source has no canonical signature header")?;
    ensure!(
        lillux::sha256_hex(raw.as_bytes()) == body_hash
            && lillux::signature::is_valid_signature_for(
                &header.content_hash,
                &header.signature_b64,
                &header.signer_fingerprint,
                lillux::signature::content_to_sign(&raw, envelope.after_shebang),
                &key,
                &signer.signer_fingerprint,
            ),
        "retained source signature or parsed body changed"
    );
    Ok(raw)
}

fn verify_retained_signed_bundle_manifest(
    manifest: &MaterializationSignedBundleManifest,
    signer: &MaterializationSignerKey,
    signed_bytes: &[u8],
) -> Result<()> {
    ensure!(
        manifest.signer_fingerprint == signer.signer_fingerprint
            && lillux::sha256_hex(signed_bytes) == manifest.signed_blob_hash,
        "retained Bundle manifest signer differs from historical verifier"
    );
    let identity = ryeos_engine::plan_builder::verify_retained_signed_bundle_manifest_bytes(
        signed_bytes,
        &manifest.bundle_name,
        &manifest.signer_fingerprint,
        &retained_verifier(signer)?,
    )?;
    ensure!(
        identity.body_digest == manifest.body_digest
            && identity.name == manifest.bundle_name
            && identity.signer_fingerprint == manifest.signer_fingerprint,
        "retained signed Bundle manifest identity changed"
    );
    Ok(())
}

fn retained_verifier(signer: &MaterializationSignerKey) -> Result<lillux::crypto::VerifyingKey> {
    let encoded = signer
        .verifying_key
        .strip_prefix("ed25519:")
        .context("retained recipe verifier is not Ed25519")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    let key_bytes: [u8; 32] = decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("retained recipe verifier length changed"))?;
    let key = lillux::crypto::VerifyingKey::from_bytes(&key_bytes)?;
    ensure!(
        lillux::crypto::fingerprint(&key) == signer.signer_fingerprint,
        "retained recipe verifier differs from fingerprint"
    );
    Ok(key)
}

fn verify_retained_executor_source(
    evidence: &GuestRuntimeMaterializationSourceEvidence,
    signed_manifest_ref: &[u8],
    manifest_object: &serde_json::Value,
    item_source_object: &serde_json::Value,
    signed_sidecar: &[u8],
    payload: &[u8],
) -> Result<()> {
    evidence.validate()?;
    let executor = &evidence.executor;
    ensure!(
        executor.item_ref == format!("bin/{}/{}", executor.target_triple, OWNER_NAME),
        "retained executor is not the exact guest occurrence owner"
    );
    let signer = evidence
        .signer_keys
        .iter()
        .find(|key| key.signer_fingerprint == executor.signer_fingerprint)
        .context("retained executor signer has no historical verifier")?;
    let key = retained_verifier(signer)?;
    ensure!(
        lillux::sha256_hex(signed_manifest_ref) == executor.signed_manifest_ref_blob_hash,
        "retained executor manifest ref bytes changed"
    );
    let signed_ref = std::str::from_utf8(signed_manifest_ref)?;
    let verified_ref = ryeos_engine::executor_resolution::verify_signed_executor_manifest_ref(
        signed_ref,
        |fingerprint| (fingerprint == signer.signer_fingerprint).then(|| key.clone()),
        TrustClass::TrustedBundle,
    )?;
    ensure!(
        verified_ref.signer_fingerprint == executor.signer_fingerprint
            && verified_ref.manifest_hash == executor.manifest_object_blob_hash,
        "retained executor manifest ref differs from source evidence"
    );
    let manifest_bytes = lillux::canonical_json(manifest_object)?;
    ensure!(
        lillux::sha256_hex(manifest_bytes.as_bytes()) == executor.manifest_object_blob_hash,
        "retained executor manifest object bytes changed"
    );
    let selected = ryeos_engine::executor_resolution::verify_executor_manifest_object(
        manifest_object,
        &executor.manifest_object_blob_hash,
    )?;
    ensure!(
        selected.get(&executor.item_ref) == Some(&executor.item_source_object_hash),
        "retained executor manifest does not select ItemSource"
    );
    let item_source_bytes = lillux::canonical_json(item_source_object)?;
    ensure!(
        lillux::sha256_hex(item_source_bytes.as_bytes()) == executor.item_source_object_hash,
        "retained executor ItemSource object bytes changed"
    );
    let (content_hash, mode) = ryeos_engine::executor_resolution::verify_executor_item_source(
        item_source_object,
        &executor.item_source_object_hash,
        &executor.item_ref,
    )?;
    ensure!(
        content_hash == executor.payload_blob_hash && mode == 0o755,
        "retained executor ItemSource differs from the executable payload contract"
    );
    verify_retained_signed_envelope(
        signed_sidecar,
        &executor.signed_sidecar_blob_hash,
        &executor.item_source_object_hash,
        &MaterializationSignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: false,
        },
        signer,
    )?;
    ensure!(
        lillux::sha256_hex(payload) == executor.payload_blob_hash,
        "retained executor payload bytes changed"
    );
    Ok(())
}

impl PreparedGuestOwnerMaterialization {
    pub fn root(&self) -> &lillux::PinnedDirectory {
        &self.root
    }

    pub fn identity(&self) -> &GuestOwnerRuntimeManifestIdentity {
        &self.identity
    }

    pub fn source(&self) -> &GuestOwnerMaterializationSource {
        &self.source
    }

    /// Exact verified signed envelopes for the recipe's effective Bundle
    /// definition. The durable publisher must retain these bytes in its CAS
    /// closure; the source digests alone do not preserve historical authority.
    pub fn signed_recipe_sources(&self) -> &[CapturedSignedBundleItemSource] {
        &self.signed_recipe_sources
    }

    pub fn signed_bundle_manifests(&self) -> &[CapturedSignedBundleManifest] {
        &self.signed_bundle_manifests
    }

    pub fn executor_source_proof(&self) -> &BundlePayloadSourceProof {
        &self.executor_source_proof
    }

    pub fn source_evidence(&self) -> Result<GuestRuntimeMaterializationSourceEvidence> {
        build_source_evidence(
            &self.source,
            &self.signed_recipe_sources,
            &self.signed_bundle_manifests,
            &self.signer_keys,
            &self.executor_source_proof,
        )
    }

    pub fn ensure_current(&self) -> Result<()> {
        let observed = ObservedGuestRuntime::observe(&self.root)?;
        ensure!(
            observed.manifest_hash() == self.identity.manifest_hash,
            "prepared guest owner runtime drifted after materialization"
        );
        Ok(())
    }
}

pub fn prepare_current_guest_owner_runtime(
    state: &AppState,
    context: &HandlerContext,
    materialization_binding_id: &str,
    private_parent: &lillux::PinnedDirectory,
    child_name: &OsStr,
) -> Result<PreparedGuestOwnerMaterialization> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    private_parent.require_owner_private_directory()?;
    let operator_authority = crate::operator_authority::admitted_operator_authority_for_principal(
        state,
        &context.fingerprint,
    )?;
    ensure!(
        operator_authority.principal_class
            == context
                .authorized_key_class
                .context("operator class is missing")?
            && (operator_authority.principal_class
                == crate::identity::AuthorizedKeyPrincipalClass::LocalClient
                || context.authenticated_origin_site_id.as_deref()
                    == Some(operator_authority.origin_site_id.as_str())),
        "materialization owner differs from the authenticated operator grant"
    );
    let binding = state
        .node_config
        .guest_runtime_materialization
        .iter()
        .find(|binding| binding.id() == materialization_binding_id)
        .context("guest runtime materialization binding is not installed")?;
    let recipe_ref = binding.recipe_ref();
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(recipe_ref)?;
    ensure!(
        canonical.to_string() == recipe_ref
            && canonical.suffix.is_none()
            && canonical.kind == "config",
        "guest owner recipe must be an exact Config ref"
    );

    state.engine.with_checked_bundle_generation(|generation| {
        let resolution = generation.effective_resolution_output(EffectiveItemRequest {
            item_ref: canonical,
            expected_kind: Some("config".into()),
            project_root: None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
        })?;
        let root = &resolution.root;
        let ItemSourceRoot::Bundle { name: bundle_name } = &root.source_root else {
            anyhow::bail!("guest owner recipe has no registered Bundle provenance");
        };
        ensure!(
            root.resolved_ref == recipe_ref
                && root.source_space == ItemSpace::Bundle
                && resolution.effective_trust_class == TrustClass::TrustedBundle,
            "guest owner recipe is not an exact trusted Bundle Config"
        );
        binding.require_recipe_identity(
            &root.resolved_ref,
            &root.raw_content_digest,
            resolution.effective_definition_digest()?.as_str(),
        )?;
        let signed_recipe_sources =
            generation.capture_verified_signed_bundle_sources(&resolution)?;
        let source_set_digest = signed_recipe_source_set_digest(&signed_recipe_sources)?;
        let signed_bundle_manifests =
            generation.capture_verified_source_bundle_manifests(&signed_recipe_sources)?;
        let bundle_manifest_set_digest =
            signed_bundle_manifest_set_digest(&signed_bundle_manifests)?;
        let publisher = root
            .signer_fingerprint
            .as_deref()
            .context("guest owner recipe has no admitted publisher")?;
        let mut signer_fingerprints = signed_recipe_sources
            .iter()
            .map(|item| item.signer_fingerprint.clone())
            .chain(
                signed_bundle_manifests
                    .iter()
                    .map(|item| item.identity.signer_fingerprint.clone()),
            )
            .collect::<std::collections::BTreeSet<_>>();
        signer_fingerprints.insert(publisher.to_owned());
        let signer_keys = signer_fingerprints
            .into_iter()
            .map(|fingerprint| {
                let key = state
                    .engine
                    .node_trust_store
                    .get(&fingerprint)
                    .context("checked materialization signer has no retained verifier")?;
                ensure!(
                    key.fingerprint == fingerprint
                        && lillux::crypto::fingerprint(&key.verifying_key) == fingerprint,
                    "checked materialization signer verifier differs from fingerprint"
                );
                Ok(MaterializationSignerKey {
                    signer_fingerprint: fingerprint,
                    verifying_key: format!(
                        "ed25519:{}",
                        base64::engine::general_purpose::STANDARD
                            .encode(key.verifying_key.to_bytes())
                    ),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let recipe: GuestOwnerMaterializationRecipe = serde_json::from_value(
            resolution
                .composed
                .composed
                .get("guest_owner_materialization")
                .cloned()
                .context("guest owner Config has no materialization recipe")?,
        )?;
        recipe.validate()?;
        let profile_digest = lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(&recipe.profile)?)?.as_bytes(),
        );
        let bundle_root = state
            .engine
            .registered_bundle_root(bundle_name)
            .context("recipe Bundle has no registered content root")?;
        let payload = capture_bundle_payload_for_target(
            &recipe.owner_binary_ref()?,
            &recipe.guest_target_triple,
            bundle_root,
            &state.engine.node_trust_store,
            recipe.maximum_owner_bytes,
        )?;
        ensure!(
            payload.identity.signer_fingerprint == publisher
                && payload.identity.target_triple == recipe.guest_target_triple,
            "guest owner payload differs from the signed recipe's Bundle authority"
        );
        let product = produce_guest_owner_runtime_from_admitted_payload(
            private_parent,
            child_name,
            payload.authority(),
            payload.bytes,
            &payload.identity.content_hash,
            state.identity.verifying_key(),
            &recipe.profile,
        )?;
        let observed = ObservedGuestRuntime::observe(product.root())?;
        let manifest = ryeos_state::observe_external_content_tree_exact(product.root())?;
        let identity = derive_guest_owner_runtime_manifest_identity(
            &serde_json::to_value(&manifest)?,
            state.identity.verifying_key(),
        )?;
        ensure!(
            identity.manifest_hash == product.manifest_hash()
                && identity.manifest_hash == observed.manifest_hash()
                && identity.owner_executable_sha256 == payload.identity.content_hash,
            "materialized guest owner differs from its admitted source"
        );
        let source = GuestOwnerMaterializationSource {
            schema: 1,
            materializer_protocol: "ryeos.guest-owner-materialization.v1".into(),
            materialization_binding_id: binding.id().to_owned(),
            materialization_binding_digest: binding.digest().to_owned(),
            recipe_ref: root.resolved_ref.clone(),
            recipe_content_digest: root.raw_content_digest.clone(),
            recipe_effective_digest: resolution
                .effective_definition_digest()?
                .as_str()
                .to_owned(),
            signed_recipe_source_set_digest: source_set_digest,
            signed_bundle_manifest_set_digest: bundle_manifest_set_digest,
            bundle_name: bundle_name.to_owned(),
            bundle_generation: generation.request_engine_generation_identity().to_owned(),
            recipe_publisher_fingerprint: publisher.to_owned(),
            executor_manifest_hash: payload.identity.manifest_hash,
            executor_manifest_ref_signed_digest: lillux::sha256_hex(
                &payload.source_proof.signed_manifest_ref,
            ),
            executor_item_source_hash: payload.identity.item_source_hash,
            executor_sidecar_signed_digest: lillux::sha256_hex(
                &payload.source_proof.signed_sidecar,
            ),
            owner_executable_sha256: payload.identity.content_hash,
            guest_target_triple: recipe.guest_target_triple,
            maximum_owner_bytes: recipe.maximum_owner_bytes,
            profile_digest,
            operator_authority,
            node_site_id: state.identity.site_id(),
            controller_public_root: identity.controller_public_root.clone(),
            runtime_manifest_hash: identity.manifest_hash.clone(),
        };
        source.coordinate_digest()?;
        Ok(PreparedGuestOwnerMaterialization {
            root: product.root().try_clone()?,
            identity,
            source,
            signed_recipe_sources,
            signed_bundle_manifests,
            signer_keys,
            executor_source_proof: payload.source_proof,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use ryeos_state::Signer as _;

    struct FixtureNodeSigner {
        key: SigningKey,
        fingerprint: String,
    }

    impl ryeos_state::Signer for FixtureNodeSigner {
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

    fn hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    #[test]
    fn signed_recipe_source_set_is_order_independent_and_byte_bound() {
        let source = |resolved_ref: &str, bytes: &[u8]| CapturedSignedBundleItemSource {
            resolved_ref: resolved_ref.to_owned(),
            source_root: ItemSourceRoot::Bundle {
                name: "codex".to_owned(),
            },
            signer_fingerprint: hash('a'),
            source_content_digest: lillux::sha256_hex(bytes),
            raw_content_digest: hash('b'),
            signature_envelope: ryeos_engine::contracts::SignatureEnvelope {
                prefix: "#".to_owned(),
                suffix: None,
                after_shebang: false,
            },
            signed_bytes: bytes.to_vec(),
        };
        let first = source("config:codex/first", b"signed first");
        let second = source("config:codex/second", b"signed second");
        let expected = signed_recipe_source_set_digest(&[first.clone(), second.clone()]).unwrap();
        assert_eq!(
            signed_recipe_source_set_digest(&[second.clone(), first.clone()]).unwrap(),
            expected
        );
        assert!(signed_recipe_source_set_digest(&[first.clone(), first.clone()]).is_err());
        let replaced = source("config:codex/first", b"different signed first");
        assert!(signed_recipe_source_set_digest(&[first.clone(), replaced.clone()]).is_err());
        assert_ne!(
            signed_recipe_source_set_digest(&[replaced, second.clone()]).unwrap(),
            expected
        );
        let mut alternate_envelope = second.clone();
        alternate_envelope.signature_envelope.prefix = "//".to_owned();
        assert_ne!(
            signed_recipe_source_set_digest(&[first.clone(), alternate_envelope]).unwrap(),
            expected
        );
        let mut changed = first;
        changed.signed_bytes.push(b'!');
        assert!(signed_recipe_source_set_digest(&[changed, second]).is_err());
    }

    fn fixture_source() -> GuestOwnerMaterializationSource {
        GuestOwnerMaterializationSource {
            schema: 1,
            materializer_protocol: "ryeos.guest-owner-materialization.v1".into(),
            materialization_binding_id: "owner".into(),
            materialization_binding_digest: hash('a'),
            recipe_ref: "config:codex/guest-owner-materialization".into(),
            recipe_content_digest: hash('b'),
            recipe_effective_digest: hash('c'),
            signed_recipe_source_set_digest: hash('0'),
            signed_bundle_manifest_set_digest: hash('a'),
            bundle_name: "codex".into(),
            bundle_generation: hash('d'),
            recipe_publisher_fingerprint: hash('e'),
            executor_manifest_hash: hash('f'),
            executor_manifest_ref_signed_digest: hash('a'),
            executor_item_source_hash: hash('1'),
            executor_sidecar_signed_digest: hash('b'),
            owner_executable_sha256: hash('2'),
            guest_target_triple: "x86_64-unknown-linux-gnu".into(),
            maximum_owner_bytes: 32 * 1024 * 1024,
            profile_digest: hash('3'),
            operator_authority: AdmittedOperatorAuthority {
                owner_principal: format!("fp:{}", hash('4')),
                origin_site_id: "site:controller".into(),
                principal_class: crate::identity::AuthorizedKeyPrincipalClass::LocalClient,
                grant_digest: hash('5'),
                scopes: vec!["ryeos.execute.service.guest-runtime/materialize".into()],
            },
            node_site_id: "site:controller".into(),
            controller_public_root: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(
                    SigningKey::from_bytes(&[7u8; 32])
                        .verifying_key()
                        .to_bytes()
                )
            ),
            runtime_manifest_hash: hash('6'),
        }
    }

    #[test]
    fn source_evidence_joins_selected_executor_and_root_recipe() {
        let mut source = fixture_source();
        let node_signer = FixtureNodeSigner {
            key: SigningKey::from_bytes(&[7u8; 32]),
            fingerprint: lillux::crypto::fingerprint(
                &SigningKey::from_bytes(&[7u8; 32]).verifying_key(),
            ),
        };
        source.node_site_id = format!("site:{}", node_signer.fingerprint);
        source.operator_authority.origin_site_id = source.node_site_id.clone();
        let signing_key = SigningKey::from_bytes(&[11u8; 32]);
        let verifier = signing_key.verifying_key();
        let payload = b"exact owner bytes";
        source.owner_executable_sha256 = lillux::sha256_hex(payload);
        source.recipe_publisher_fingerprint = lillux::crypto::fingerprint(&verifier);
        let signer_keys = vec![MaterializationSignerKey {
            signer_fingerprint: source.recipe_publisher_fingerprint.clone(),
            verifying_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(verifier.to_bytes())
            ),
        }];
        let recipe_body = "kind: config\nguest_owner_materialization: exact\n";
        let signed_recipe = lillux::signature::sign_content_at(
            recipe_body,
            &signing_key,
            "#",
            None,
            "2026-09-29T00:00:00Z",
        );
        source.recipe_content_digest = lillux::sha256_hex(recipe_body.as_bytes());
        let recipe = CapturedSignedBundleItemSource {
            resolved_ref: source.recipe_ref.clone(),
            source_root: ItemSourceRoot::Bundle {
                name: source.bundle_name.clone(),
            },
            signer_fingerprint: source.recipe_publisher_fingerprint.clone(),
            source_content_digest: lillux::sha256_hex(signed_recipe.as_bytes()),
            raw_content_digest: source.recipe_content_digest.clone(),
            signature_envelope: ryeos_engine::contracts::SignatureEnvelope {
                prefix: "#".to_owned(),
                suffix: None,
                after_shebang: false,
            },
            signed_bytes: signed_recipe.into_bytes(),
        };
        let bundle_body = "name: codex\nversion: 1.0.0\nprovides_kinds: []\nrequires_kinds: []\n";
        let signed_bundle = lillux::signature::sign_content_at(
            bundle_body,
            &signing_key,
            "#",
            None,
            "2026-09-29T00:00:00Z",
        );
        let bundle = CapturedSignedBundleManifest {
            identity: ryeos_engine::plan_builder::SignedBundleManifestIdentity {
                name: source.bundle_name.clone(),
                body_digest: lillux::sha256_hex(bundle_body.as_bytes()),
                signer_fingerprint: source.recipe_publisher_fingerprint.clone(),
                provides_kinds: vec![],
                requires_kinds: vec![],
                uses_kinds: vec![],
            },
            signed_bytes: signed_bundle.into_bytes(),
        };
        source.signed_recipe_source_set_digest =
            signed_recipe_source_set_digest(std::slice::from_ref(&recipe)).unwrap();
        source.signed_bundle_manifest_set_digest =
            signed_bundle_manifest_set_digest(std::slice::from_ref(&bundle)).unwrap();
        let item_ref = format!("bin/{}/{}", source.guest_target_triple, OWNER_NAME);
        let item_source = serde_json::json!({
            "kind": "item_source",
            "item_ref": item_ref,
            "content_blob_hash": source.owner_executable_sha256,
            "integrity": format!("sha256:{}", source.owner_executable_sha256),
            "signature_info": null,
            "mode": 0o755,
        });
        source.executor_item_source_hash =
            ryeos_state::objects::canonical_value_digest(&item_source).unwrap();
        let manifest = serde_json::json!({
            "kind": "source_manifest",
            "item_source_hashes": {item_ref.clone(): source.executor_item_source_hash},
        });
        source.executor_manifest_hash =
            ryeos_state::objects::canonical_value_digest(&manifest).unwrap();
        let signed_ref = lillux::signature::sign_content_at(
            &format!(
                "{}\n{}\n",
                ryeos_engine::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN,
                source.executor_manifest_hash
            ),
            &signing_key,
            "#",
            None,
            "2026-09-29T00:00:00Z",
        );
        let signed_sidecar = lillux::signature::sign_content_at(
            &lillux::canonical_json(&item_source).unwrap(),
            &signing_key,
            "#",
            None,
            "2026-09-29T00:00:00Z",
        );
        let proof = BundlePayloadSourceProof {
            selected_item_ref: item_ref,
            signed_manifest_ref: signed_ref.into_bytes(),
            manifest_object: manifest,
            item_source_object: item_source,
            signed_sidecar: signed_sidecar.into_bytes(),
        };
        source.executor_manifest_ref_signed_digest = lillux::sha256_hex(&proof.signed_manifest_ref);
        source.executor_sidecar_signed_digest = lillux::sha256_hex(&proof.signed_sidecar);
        let evidence = build_source_evidence(
            &source,
            std::slice::from_ref(&recipe),
            std::slice::from_ref(&bundle),
            &signer_keys,
            &proof,
        )
        .unwrap();
        assert_eq!(
            evidence.executor.item_source_object_hash,
            source.executor_item_source_hash
        );
        assert_eq!(
            evidence.signed_recipe_items[0].raw_content_digest,
            source.recipe_content_digest
        );
        verify_retained_executor_source(
            &evidence,
            &proof.signed_manifest_ref,
            &proof.manifest_object,
            &proof.item_source_object,
            &proof.signed_sidecar,
            payload,
        )
        .unwrap();
        assert!(
            verify_retained_executor_source(
                &evidence,
                &proof.signed_manifest_ref,
                &proof.manifest_object,
                &proof.item_source_object,
                &proof.signed_sidecar,
                b"changed owner bytes",
            )
            .is_err()
        );

        let tmp = tempfile::tempdir().unwrap();
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(node_signer.fingerprint.clone(), node_signer.verifying_key());
        let db = ryeos_state::StateDb::open(tmp.path(), std::sync::Arc::new(trust)).unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let cas = authority.cas_store().unwrap();
        assert_eq!(
            cas.store_blob(&recipe.signed_bytes).unwrap(),
            evidence.signed_recipe_items[0].signed_blob_hash
        );
        assert_eq!(
            cas.store_blob(&bundle.signed_bytes).unwrap(),
            evidence.signed_bundle_manifests[0].signed_blob_hash
        );
        assert_eq!(
            cas.store_blob(&proof.signed_manifest_ref).unwrap(),
            evidence.executor.signed_manifest_ref_blob_hash
        );
        assert_eq!(
            cas.store_blob(
                lillux::canonical_json(&proof.manifest_object)
                    .unwrap()
                    .as_bytes()
            )
            .unwrap(),
            evidence.executor.manifest_object_blob_hash
        );
        assert_eq!(
            cas.store_object(&proof.item_source_object).unwrap(),
            evidence.executor.item_source_object_hash
        );
        assert_eq!(
            cas.store_blob(payload).unwrap(),
            evidence.executor.payload_blob_hash
        );
        let evidence_hash = cas.store_object(&evidence.to_value().unwrap()).unwrap();
        assert!(retained::verify_source_closure(&cas, &source, &evidence_hash).is_err());
        assert_eq!(
            cas.store_blob(&proof.signed_sidecar).unwrap(),
            evidence.executor.signed_sidecar_blob_hash
        );
        let verified = retained::verify_source_closure(&cas, &source, &evidence_hash).unwrap();
        assert_eq!(verified.payload_size, payload.len() as u64);
        assert_eq!(verified.evidence, evidence);
        let profile = br#"{"kind":"guest-owner-profile"}"#;
        source.profile_digest = lillux::sha256_hex(profile);
        let controller_root = hex::encode(
            SigningKey::from_bytes(&[7u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        let root_hash = lillux::sha256_hex(controller_root.as_bytes());
        let output = ryeos_state::objects::external_content_manifest::ExternalContentManifestObject {
            schema: ryeos_state::objects::external_content_manifest::EXTERNAL_CONTENT_TREE_SCHEMA.to_owned(),
            kind: ryeos_state::objects::external_content_manifest::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            entries: vec![
                ryeos_state::objects::external_content_manifest::ExternalContentManifestEntry {
                    path: "bin".to_owned(), kind: ryeos_state::objects::external_content_manifest::ExternalContentManifestEntryKind::Dir,
                    mode: None, blob_hash: None, size: None, target: None,
                },
                ryeos_state::objects::external_content_manifest::ExternalContentManifestEntry {
                    path: format!("bin/{OWNER_NAME}"), kind: ryeos_state::objects::external_content_manifest::ExternalContentManifestEntryKind::File,
                    mode: Some(0o755), blob_hash: Some(source.owner_executable_sha256.clone()), size: Some(payload.len() as u64), target: None,
                },
                ryeos_state::objects::external_content_manifest::ExternalContentManifestEntry {
                    path: "controller-root.hex".to_owned(), kind: ryeos_state::objects::external_content_manifest::ExternalContentManifestEntryKind::File,
                    mode: Some(0o644), blob_hash: Some(root_hash.clone()), size: Some(64), target: None,
                },
                ryeos_state::objects::external_content_manifest::ExternalContentManifestEntry {
                    path: "guest-owner-profile.json".to_owned(), kind: ryeos_state::objects::external_content_manifest::ExternalContentManifestEntryKind::File,
                    mode: Some(0o644), blob_hash: Some(source.profile_digest.clone()), size: Some(profile.len() as u64), target: None,
                },
            ],
            entry_count: 4,
            total_bytes: payload.len() as u64 + 64 + profile.len() as u64,
        };
        output.validate().unwrap();
        source.runtime_manifest_hash = cas
            .store_object(&serde_json::to_value(&output).unwrap())
            .unwrap();
        assert!(retained::verify_output_closure(&cas, &source, &verified).is_err());
        assert_eq!(cas.store_blob(profile).unwrap(), source.profile_digest);
        assert_eq!(
            cas.store_blob(controller_root.as_bytes()).unwrap(),
            root_hash
        );
        let output_identity = retained::verify_output_closure(&cas, &source, &verified).unwrap();
        assert_eq!(output_identity.manifest_hash, source.runtime_manifest_hash);
        let subject = ryeos_state::objects::GuestRuntimeMaterializationSubject {
            schema: ryeos_state::objects::GUEST_RUNTIME_MATERIALIZATION_SCHEMA,
            kind: ryeos_state::objects::GUEST_RUNTIME_MATERIALIZATION_SUBJECT_KIND.to_owned(),
            coordinate_digest: source.coordinate_digest().unwrap(),
            runtime_manifest_hash: source.runtime_manifest_hash.clone(),
            source_evidence_hash: evidence_hash.clone(),
        };
        let subject_hash = cas.store_object(&subject.to_value().unwrap()).unwrap();
        let claim =
            retained::MaterializationClaimEvidence::from_checked_source(&source, &signer_keys)
                .unwrap();
        let attestation = ryeos_state::objects::Attestation::unsigned(
            subject_hash.clone(),
            retained::CLAIM.to_owned(),
            retained::POLICY.to_owned(),
            "2026-09-29T00:00:00Z".to_owned(),
            None,
            serde_json::to_value(claim).unwrap(),
        )
        .sign(&node_signer)
        .unwrap();
        let testimony =
            retained::verify_testimony(&cas, &attestation, &node_signer.verifying_key()).unwrap();
        assert_eq!(testimony.subject, subject);
        assert_eq!(testimony.source, source);
        assert_eq!(testimony.output, output_identity);
        let coordinate = source.coordinate_digest().unwrap();
        let publication_key = ryeos_state::DurableCasPublicationKey::guest_runtime_materialization(
            &source.materialization_binding_digest,
            &coordinate,
        )
        .unwrap();
        let mut stage = authority
            .require_recovery()
            .unwrap()
            .begin_durable_cas_upload_admitted(
                &guard,
                &source.operator_authority.owner_principal,
                "guest-runtime-materialization",
                &publication_key,
                None,
            )
            .unwrap();
        stage
            .protect_cas_closure(&guard, [subject_hash.as_str()], std::iter::empty())
            .unwrap();
        let candidate_hash = stage
            .store_object(&guard, &cas, &attestation.to_value())
            .unwrap();
        let publish = |candidate: &ryeos_state::objects::Attestation| {
            ryeos_state::immutable_testimony::publish_immutable_attestation(
                &authority,
                "guest-runtime-materialization",
                &coordinate,
                candidate,
                &node_signer,
                &guard,
                |value| retained::verify_testimony(&cas, value, &node_signer.verifying_key()),
                |hash| {
                    let value = retained::load_object_bounded(&cas, hash, 64 * 1024)?;
                    let incumbent = ryeos_state::objects::Attestation::from_value(&value)?;
                    let verified =
                        retained::verify_testimony(&cas, &incumbent, &node_signer.verifying_key())?;
                    Ok((incumbent, verified))
                },
            )
        };
        let (_, reused) = publish(&attestation).unwrap();
        assert!(!reused);
        let head = ryeos_state::immutable_testimony::read_immutable_attestation_head(
            &authority,
            "guest-runtime-materialization",
            &coordinate,
            &guard,
        )
        .unwrap()
        .unwrap();
        assert_eq!(head.target_hash, candidate_hash);
        stage
            .protect_cas_closure(&guard, [head.target_hash.as_str()], std::iter::empty())
            .unwrap();
        stage.finish_admitted(&guard, &head.target_hash).unwrap();
        let replay = ryeos_state::objects::Attestation::unsigned(
            subject_hash,
            retained::CLAIM.to_owned(),
            retained::POLICY.to_owned(),
            "2026-09-29T00:00:01Z".to_owned(),
            None,
            attestation.evidence.clone(),
        )
        .sign(&node_signer)
        .unwrap();
        let (_, reused) = publish(&replay).unwrap();
        assert!(reused);
        let head_after = ryeos_state::immutable_testimony::read_immutable_attestation_head(
            &authority,
            "guest-runtime-materialization",
            &coordinate,
            &guard,
        )
        .unwrap()
        .unwrap();
        assert_eq!(head_after.target_hash, candidate_hash);
        assert!(
            retained::verify_testimony(
                &cas,
                &attestation,
                &SigningKey::from_bytes(&[9u8; 32]).verifying_key(),
            )
            .is_err()
        );
        let mut wrong_claim = attestation.clone();
        wrong_claim.claim = "execution_captured".to_owned();
        assert!(
            retained::verify_testimony(&cas, &wrong_claim, &node_signer.verifying_key()).is_err()
        );
        let mut wrong_profile = source.clone();
        wrong_profile.profile_digest = hash('8');
        assert!(retained::verify_output_closure(&cas, &wrong_profile, &verified).is_err());
        let mut wrong_coordinate = source.clone();
        wrong_coordinate.signed_recipe_source_set_digest = hash('8');
        assert!(retained::verify_source_closure(&cas, &wrong_coordinate, &evidence_hash).is_err());

        let mut changed = source.clone();
        changed.executor_sidecar_signed_digest = hash('8');
        assert!(
            build_source_evidence(
                &changed,
                &[recipe.clone()],
                &[bundle.clone()],
                &signer_keys,
                &proof
            )
            .is_err()
        );
        changed = source.clone();
        changed.recipe_content_digest = hash('8');
        assert!(
            build_source_evidence(&changed, &[recipe], &[bundle], &signer_keys, &proof).is_err()
        );
        drop(stage);
        drop(cas);
        drop(guard);
        drop(authority);
        drop(db);
        let mut reopened_trust = ryeos_state::refs::TrustStore::new();
        reopened_trust.insert(node_signer.fingerprint.clone(), node_signer.verifying_key());
        let reopened =
            ryeos_state::StateDb::open(tmp.path(), std::sync::Arc::new(reopened_trust)).unwrap();
        let reopened_authority = reopened.pinned_authority().unwrap();
        let reopened_guard = reopened_authority.acquire_shared_guard().unwrap();
        let reopened_head = ryeos_state::immutable_testimony::read_immutable_attestation_head(
            &reopened_authority,
            "guest-runtime-materialization",
            &coordinate,
            &reopened_guard,
        )
        .unwrap()
        .unwrap();
        assert_eq!(reopened_head.target_hash, candidate_hash);
        let reopened_cas = reopened_authority.cas_store().unwrap();
        let reopened_value =
            retained::load_object_bounded(&reopened_cas, &reopened_head.target_hash, 64 * 1024)
                .unwrap();
        let reopened_attestation =
            ryeos_state::objects::Attestation::from_value(&reopened_value).unwrap();
        let recovered = retained::verify_testimony(
            &reopened_cas,
            &reopened_attestation,
            &node_signer.verifying_key(),
        )
        .unwrap();
        assert_eq!(recovered.subject, subject);
    }

    #[test]
    fn retained_recipe_rechecks_exact_signature_without_live_bundle() {
        let key = SigningKey::from_bytes(&[21u8; 32]);
        let verifier = key.verifying_key();
        let body = "kind: config\nvalue: exact\n";
        let signed =
            lillux::signature::sign_content_at(body, &key, "#", None, "2026-09-29T00:00:00Z");
        let signer = MaterializationSignerKey {
            signer_fingerprint: lillux::crypto::fingerprint(&verifier),
            verifying_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(verifier.to_bytes())
            ),
        };
        let item = MaterializationSignedItem {
            resolved_ref: "config:codex/guest-owner-materialization".to_owned(),
            bundle_name: "codex".to_owned(),
            signer_fingerprint: signer.signer_fingerprint.clone(),
            signed_blob_hash: lillux::sha256_hex(signed.as_bytes()),
            raw_content_digest: lillux::sha256_hex(body.as_bytes()),
            signature_envelope: MaterializationSignatureEnvelope {
                prefix: "#".to_owned(),
                suffix: None,
                after_shebang: false,
            },
        };
        verify_retained_signed_recipe_item(&item, &signer, signed.as_bytes()).unwrap();
        let mut substituted = item.clone();
        substituted.signature_envelope.prefix = "//".to_owned();
        assert!(
            verify_retained_signed_recipe_item(&substituted, &signer, signed.as_bytes()).is_err()
        );
        substituted = item;
        substituted.raw_content_digest = hash('a');
        assert!(
            verify_retained_signed_recipe_item(&substituted, &signer, signed.as_bytes()).is_err()
        );

        let bundle_body = "name: codex\nversion: 1.0.0\nprovides_kinds: []\nrequires_kinds: []\n";
        let signed_bundle = lillux::signature::sign_content_at(
            bundle_body,
            &key,
            "#",
            None,
            "2026-09-29T00:00:00Z",
        );
        let bundle = MaterializationSignedBundleManifest {
            bundle_name: "codex".to_owned(),
            signer_fingerprint: signer.signer_fingerprint.clone(),
            signed_blob_hash: lillux::sha256_hex(signed_bundle.as_bytes()),
            body_digest: lillux::sha256_hex(bundle_body.as_bytes()),
        };
        verify_retained_signed_bundle_manifest(&bundle, &signer, signed_bundle.as_bytes()).unwrap();
        let mut changed_bundle = bundle;
        changed_bundle.bundle_name = "other".to_owned();
        assert!(
            verify_retained_signed_bundle_manifest(
                &changed_bundle,
                &signer,
                signed_bundle.as_bytes()
            )
            .is_err()
        );
        let malformed = lillux::signature::sign_content_at(
            "name: codex\nversion: 1.0.0\n",
            &key,
            "#",
            None,
            "2026-09-29T00:00:00Z",
        );
        changed_bundle.bundle_name = "codex".to_owned();
        changed_bundle.signed_blob_hash = lillux::sha256_hex(malformed.as_bytes());
        changed_bundle.body_digest = lillux::sha256_hex(b"name: codex\nversion: 1.0.0\n");
        assert!(
            verify_retained_signed_bundle_manifest(&changed_bundle, &signer, malformed.as_bytes())
                .is_err()
        );
    }

    #[test]
    fn retained_referenced_source_verifies_after_shebang_envelope() {
        let key = SigningKey::from_bytes(&[22u8; 32]);
        let body = "#!/usr/bin/env python3\nprint('bounded')\n";
        let signed = lillux::signature::sign_content_at_with_options(
            body,
            &key,
            "#",
            None,
            "2026-09-29T00:00:00Z",
            true,
        );
        let signer = MaterializationSignerKey {
            signer_fingerprint: lillux::crypto::fingerprint(&key.verifying_key()),
            verifying_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes())
            ),
        };
        let item = MaterializationSignedItem {
            resolved_ref: "tool:codex/example".to_owned(),
            bundle_name: "codex".to_owned(),
            signer_fingerprint: signer.signer_fingerprint.clone(),
            signed_blob_hash: lillux::sha256_hex(signed.as_bytes()),
            raw_content_digest: lillux::sha256_hex(body.as_bytes()),
            signature_envelope: MaterializationSignatureEnvelope {
                prefix: "#".to_owned(),
                suffix: None,
                after_shebang: true,
            },
        };
        verify_retained_signed_recipe_item(&item, &signer, signed.as_bytes()).unwrap();
        let mut changed = item;
        changed.signature_envelope.after_shebang = false;
        assert!(verify_retained_signed_recipe_item(&changed, &signer, signed.as_bytes()).is_err());
    }

    #[test]
    fn materialization_coordinate_distinguishes_source_not_result_bytes() {
        let source = fixture_source();
        let coordinate = source.coordinate_digest().unwrap();
        let mut changed = source.clone();
        changed.runtime_manifest_hash = hash('7');
        assert_eq!(changed.coordinate_digest().unwrap(), coordinate);
        changed = source.clone();
        changed.recipe_effective_digest = hash('8');
        assert_ne!(changed.coordinate_digest().unwrap(), coordinate);
        changed = source.clone();
        changed.signed_recipe_source_set_digest = hash('8');
        assert_ne!(changed.coordinate_digest().unwrap(), coordinate);
        changed = source.clone();
        changed.signed_bundle_manifest_set_digest = hash('8');
        assert_ne!(changed.coordinate_digest().unwrap(), coordinate);
        changed = source.clone();
        changed.executor_manifest_ref_signed_digest = hash('8');
        assert_ne!(changed.coordinate_digest().unwrap(), coordinate);
        changed = source.clone();
        changed.executor_sidecar_signed_digest = hash('8');
        assert_ne!(changed.coordinate_digest().unwrap(), coordinate);
        changed = source;
        changed.operator_authority.grant_digest = hash('9');
        assert_ne!(changed.coordinate_digest().unwrap(), coordinate);
    }
}
