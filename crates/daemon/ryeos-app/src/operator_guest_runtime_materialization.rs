//! Controller-local preparation of one guest owner from signed Bundle data.
//!
//! This stage performs no provider contact and grants no runtime qualification.
//! The later durable publication owner must retain the exact tree and source
//! testimony in CAS before a snapshot operation may contact its provider.

use std::ffi::OsStr;

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use ryeos_engine::binary_resolver::capture_bundle_payload_for_target;
use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace, SubjectResolutionAuthority};
use ryeos_engine::engine::EffectiveItemRequest;
use ryeos_engine::resolution::TrustClass;
use ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime;
use ryeos_external_execution::guest_runtime_product::{
    GuestOwnerMaterializationRecipe, GuestOwnerRuntimeManifestIdentity,
    derive_guest_owner_runtime_manifest_identity,
    produce_guest_owner_runtime_from_admitted_payload,
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
    pub bundle_name: String,
    pub bundle_generation: String,
    pub publisher_fingerprint: String,
    pub executor_manifest_hash: String,
    pub executor_item_source_hash: String,
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
            ("bundle generation", &self.bundle_generation),
            ("publisher", &self.publisher_fingerprint),
            ("executor manifest", &self.executor_manifest_hash),
            ("executor item source", &self.executor_item_source_hash),
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
            "bundle_name": self.bundle_name,
            "bundle_generation": self.bundle_generation,
            "publisher_fingerprint": self.publisher_fingerprint,
            "executor_manifest_hash": self.executor_manifest_hash,
            "executor_item_source_hash": self.executor_item_source_hash,
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
        let publisher = root
            .signer_fingerprint
            .as_deref()
            .context("guest owner recipe has no admitted publisher")?;
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
            bundle_name: bundle_name.to_owned(),
            bundle_generation: generation.request_engine_generation_identity().to_owned(),
            publisher_fingerprint: publisher.to_owned(),
            executor_manifest_hash: payload.identity.manifest_hash,
            executor_item_source_hash: payload.identity.item_source_hash,
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
    fn materialization_coordinate_distinguishes_source_not_result_bytes() {
        let source = GuestOwnerMaterializationSource {
            schema: 1,
            materializer_protocol: "ryeos.guest-owner-materialization.v1".into(),
            materialization_binding_id: "owner".into(),
            materialization_binding_digest: hash('a'),
            recipe_ref: "config:codex/guest-owner-materialization".into(),
            recipe_content_digest: hash('b'),
            recipe_effective_digest: hash('c'),
            bundle_name: "codex".into(),
            bundle_generation: hash('d'),
            publisher_fingerprint: hash('e'),
            executor_manifest_hash: hash('f'),
            executor_item_source_hash: hash('1'),
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
        };
        let coordinate = source.coordinate_digest().unwrap();
        let mut changed = source.clone();
        changed.runtime_manifest_hash = hash('7');
        assert_eq!(changed.coordinate_digest().unwrap(), coordinate);
        changed = source.clone();
        changed.recipe_effective_digest = hash('8');
        assert_ne!(changed.coordinate_digest().unwrap(), coordinate);
        changed = source;
        changed.operator_authority.grant_digest = hash('9');
        assert_ne!(changed.coordinate_digest().unwrap(), coordinate);
    }
}
