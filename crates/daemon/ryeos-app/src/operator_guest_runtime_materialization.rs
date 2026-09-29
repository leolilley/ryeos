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
    MaterializationSignedBundleManifest, MaterializationSignedItem, MaterializationSignerKey,
};
use serde::{Deserialize, Serialize};

use crate::handler_context::HandlerContext;
use crate::operator_authority::AdmittedOperatorAuthority;
use crate::state::AppState;

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
        let verifier = SigningKey::from_bytes(&[11u8; 32]).verifying_key();
        source.recipe_publisher_fingerprint = lillux::crypto::fingerprint(&verifier);
        let signer_keys = vec![MaterializationSignerKey {
            signer_fingerprint: source.recipe_publisher_fingerprint.clone(),
            verifying_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(verifier.to_bytes())
            ),
        }];
        let recipe = CapturedSignedBundleItemSource {
            resolved_ref: source.recipe_ref.clone(),
            source_root: ItemSourceRoot::Bundle {
                name: source.bundle_name.clone(),
            },
            signer_fingerprint: source.recipe_publisher_fingerprint.clone(),
            source_content_digest: lillux::sha256_hex(b"signed recipe"),
            raw_content_digest: source.recipe_content_digest.clone(),
            signed_bytes: b"signed recipe".to_vec(),
        };
        let bundle = CapturedSignedBundleManifest {
            identity: ryeos_engine::plan_builder::SignedBundleManifestIdentity {
                name: source.bundle_name.clone(),
                body_digest: hash('7'),
                signer_fingerprint: source.recipe_publisher_fingerprint.clone(),
                provides_kinds: vec![],
                requires_kinds: vec![],
                uses_kinds: vec![],
            },
            signed_bytes: b"signed Bundle manifest".to_vec(),
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
        let proof = BundlePayloadSourceProof {
            selected_item_ref: item_ref,
            signed_manifest_ref: b"signed manifest ref".to_vec(),
            manifest_object: manifest,
            item_source_object: item_source,
            signed_sidecar: b"signed sidecar".to_vec(),
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
